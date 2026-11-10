//! The harness itself: construction seams, the [`Harness`] impl, and the
//! per-run [`Session`] handed to the live loop.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::process::Child;
use tokio::sync::mpsc;

use hearth_proto::{
    AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand, SteeringMode,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls};

use super::AcpHarness;
use super::launch::find_on_paths;
use super::run::run_session;

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> HarnessId {
        self.spec.id
    }
    fn display_name(&self) -> &str {
        self.spec.display_name
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        self.spec.steering_mode
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        self.spec.reasoning_levels
    }

    /// The agent's own CLI, not the adapter: `claude` counts as installed even
    /// when `claude-agent-acp` would arrive via npx, and an npx-reachable
    /// adapter does NOT count when the CLI itself is missing. Explicit
    /// executables (tests, `*_EXECUTABLE` overrides) always count.
    fn installed(&self) -> bool {
        if self.executable.is_some() {
            return true;
        }
        if std::env::var_os(self.spec.env_override).is_some_and(|v| !v.is_empty()) {
            return true;
        }
        find_on_paths(self.spec.cli_executable, (self.spec.cli_extra_paths)()).is_some()
    }

    /// ACP is the source of truth: a short-lived probe reads the agent's
    /// advertised model list (cached on success). The spec's static catalog
    /// answers when the agent advertises nothing. Most legacy ACP adapters also
    /// use it when probing fails; OpenCode does not, because presenting two
    /// static Zen models as a successful load permanently hides a slow or
    /// failed plugin-backed catalog from the picker.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_launch()?;
        if let Some(models) = self.models_cache.get() {
            return Ok(models.clone());
        }
        let _probe = self.models_probe.lock().await;
        if let Some(models) = self.models_cache.get() {
            return Ok(models.clone());
        }
        match self.discover_models().await {
            Ok(models) if !models.is_empty() => {
                let _ = self.models_cache.set(models.clone());
                Ok(self.models_cache.get().cloned().unwrap_or(models))
            }
            Ok(_) => Ok((self.spec.models)()),
            Err(_) => Ok((self.spec.models)()),
        }
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.commands
            .get_or_try_init(|| self.discover_commands())
            .await
            .cloned()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (mut child, stderr_tail) = self.spawn_agent(Some(&request.cwd), true, &[]).await?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("agent child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("agent child has no stdout".into()))?;
        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            harness: self.spec.id,
            agent_name: self.spec.display_name,
            prompt_transform: self.spec.prompt_transform,
            effort_values: self.spec.effort_values,
            prompt_complete_extension: self.spec.prompt_complete_extension,
            prompt_stall: self.spec.prompt_stall,
            stall_hint: self.spec.stall_hint,
            sessions_root: self.sessions_root.clone(),
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            handshake_timeout: self.handshake_timeout,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

pub(crate) struct Session {
    pub(crate) child: Child,
    pub(crate) client: RpcClient,
    pub(crate) incoming: mpsc::Receiver<Incoming>,
    pub(crate) event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    pub(crate) controls: RunControls,
    pub(crate) request: RunRequest,
    pub(crate) harness: HarnessId,
    pub(crate) agent_name: &'static str,
    pub(crate) prompt_complete_extension: bool,
    pub(crate) prompt_stall: Option<Duration>,
    pub(crate) stall_hint: &'static str,
    /// Sessions-root override for the subagent transcript tail (tests).
    pub(crate) sessions_root: Option<PathBuf>,
    pub(crate) prompt_transform: fn(Option<ReasoningLevel>, &str) -> String,
    pub(crate) effort_values: fn(Option<ReasoningLevel>, Option<&str>) -> Vec<&'static str>,
    pub(crate) interrupt_grace: Duration,
    pub(crate) kill_grace: Duration,
    pub(crate) handshake_timeout: Duration,
    pub(crate) stderr_tail: crate::StderrTail,
}

pub(crate) fn initialize_params(_harness: HarnessId) -> Value {
    let capabilities = json!({
        "fs": { "readTextFile": false, "writeTextFile": false },
        "terminal": false,
    });
    json!({
        "protocolVersion": 1,
        "clientInfo": {
            "name": "hearth",
            "title": "Hearth",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // Declined: agents fall back to their own fs/terminal access, which
        // is what hearth wants — the working tree is the source of truth for
        // the diff pane, and commands belong to the agent's own sandbox.
        "clientCapabilities": capabilities,
    })
}

/// `initialize._meta.steering.supported` — the `_session/steering` extension
/// both org-maintained adapters advertise (not part of the v1 spec).
pub(crate) fn steering_supported(init: &Value) -> bool {
    init.get("_meta")
        .and_then(|m| m.get("steering"))
        .and_then(|s| s.get("supported"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steering_capability_reads_initialize_meta() {
        assert!(steering_supported(&json!({
            "protocolVersion": 1,
            "_meta": { "steering": { "supported": true } },
        })));
        assert!(!steering_supported(&json!({ "protocolVersion": 1 })));
        assert!(!steering_supported(&json!({
            "_meta": { "steering": { "supported": false } },
        })));
    }
}
