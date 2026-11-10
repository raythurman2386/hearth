//! Building the harness and resolving the CLI it spawns.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::HarnessError;

use super::CodexHarness;
use super::spec::resolve_codex_executable;

impl Default for CodexHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            commands: tokio::sync::OnceCell::new(),
            request_timeout: None,
            in_flight: Arc::default(),
        }
    }
}

impl CodexHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Override the per-request JSON-RPC bound (the default is a generous five
    /// minutes, `HEARTH_HARNESS_RPC_TIMEOUT_MS`). Tests shrink it so an
    /// unanswered request surfaces in milliseconds.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    pub(crate) fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_codex_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "codex (searched PATH, the login shell's PATH, ~/.local/bin, \
                 ~/.codex/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, \
                 and fnm/nvm/volta/pnpm/bun install dirs; set CODEX_EXECUTABLE to \
                 override)"
                    .into(),
            )
        })
    }
}
