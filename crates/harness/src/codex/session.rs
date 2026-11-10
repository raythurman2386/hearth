//! The harness itself: the [`Harness`] impl and the per-run [`Session`]
//! handed to the live loop.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use hearth_proto::{
    AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand, SteeringMode,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls};

use super::CodexHarness;
use super::catalog::{REASONING_LEVELS, static_models};
use super::run::run_session;
use super::spec::resolve_codex_executable;

#[async_trait]
impl Harness for CodexHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }
    fn display_name(&self) -> &str {
        // "Codex" (not "Codex CLI") — comet composer/defaults.ts
        // HARNESS_LABEL; must also match the registry's lazy descriptor so
        // the catalog entry doesn't change after the first resolve.
        "Codex"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// Native `turn/steer` injects into the active turn; a steer that misses
    /// the turn falls back to a follow-up `turn/start` on the same thread.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }
    fn installed(&self) -> bool {
        self.executable.is_some() || resolve_codex_executable().is_some()
    }
    /// Done is the CLI's own terminal frame, for wake turns too — UNLESS the
    /// app server currently owes us a JSON-RPC reply. A request with no
    /// response is exactly the hang that retires the engine's quiesce
    /// watchdog too early: while one is outstanding, report false so the
    /// watchdog stays armed (it parks the turn if the per-request timeout has
    /// not fired yet). Once nothing is outstanding, the native Done is again
    /// authoritative and the watchdog retires as before.
    fn deterministic_turn_end(&self) -> bool {
        self.in_flight.load(std::sync::atomic::Ordering::SeqCst) == 0
    }

    /// The curated static catalog (see [`catalog`]); requires an installed CLI
    /// so an absent binary surfaces as [`HarnessError::NotInstalled`] here.
    /// This is the seam for live discovery: a short-lived `codex app-server`
    /// paging `model/list` (experimentalApi) exactly as codex.ts does.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        Ok(static_models())
    }

    /// Skills from a short-lived `skills/list` probe (see
    /// [`Self::discover_commands`]); cached on success.
    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.commands
            .get_or_try_init(|| self.discover_commands())
            .await
            .cloned()
    }

    async fn run(
        &self,
        mut request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        // Yolo mode: danger-full-access + approvalPolicy "never" (set below) —
        // codex's --dangerously-bypass-approvals-and-sandbox equivalent.
        // Parity with the Claude adapter, which auto-approves every
        // can_use_tool and so effectively grants full access. This also
        // sidesteps codex ≤0.144.x's workspace-write bug where a linked
        // worktree on a slash-named branch derives a malformed mount that
        // kills every command.
        request.sandbox = hearth_proto::SandboxLevel::DangerFullAccess;
        let mut cmd = Command::new(&exe);
        cmd.arg("app-server");
        crate::compose_child_path(&mut cmd, &exe);
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        crate::isolate_process_group(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "hearth_harness::codex", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let timeout = self
            .request_timeout
            .or_else(crate::jsonrpc::default_request_timeout);
        let (client, incoming) =
            RpcClient::new_shared(stdin, stdout, timeout, Arc::clone(&self.in_flight));
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

pub(crate) struct Session {
    pub(crate) child: Child,
    pub(crate) client: RpcClient,
    pub(crate) incoming: mpsc::Receiver<Incoming>,
    pub(crate) event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    pub(crate) controls: RunControls,
    pub(crate) request: RunRequest,
    pub(crate) interrupt_grace: Duration,
    pub(crate) kill_grace: Duration,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    pub(crate) stderr_tail: crate::StderrTail,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine retires its quiesce watchdog when a turn end is
    /// deterministic. An outstanding JSON-RPC request is exactly when it must
    /// stay armed, so the predicate must flip while one is in flight.
    #[test]
    fn watchdog_stays_armed_while_a_request_is_in_flight() {
        let harness = CodexHarness::new();
        assert!(harness.deterministic_turn_end());

        harness
            .in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(!harness.deterministic_turn_end());

        harness
            .in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        assert!(harness.deterministic_turn_end());
    }
}
