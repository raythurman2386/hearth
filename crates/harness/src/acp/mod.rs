//! ACP harness: spawns an Agent Client Protocol agent (JSON-RPC 2.0 over
//! stdio, protocol v1) and maps its session updates onto [`AgentEvent`]s.
//!
//! Used for Huginn (`huginn --acp`) and Grok (`grok agent stdio`). Codex has a
//! native app-server driver ([`crate::CodexHarness`]).
//!
//! - `initialize` (protocolVersion 1, fs/terminal capabilities declined) →
//!   `session/new`, or `session/load` with a fresh-session fallback when
//!   resuming; replayed history during a load is dropped (the doc already
//!   holds it).
//! - `session/prompt` owns the turn: its response's `stopReason` ends the
//!   turn (`cancelled` → Interrupted, `refusal` → Errored, else Completed).
//! - `session/update` notifications normalize per [`normalize::map_update`].
//! - Permission requests auto-accept with the agent's preferred allow option
//!   (hearth sessions run unattended); question-shaped requests block on the
//!   engine's input bridge.
//! - Steering: agents advertising `_session/steering` get mid-turn injection;
//!   others queue steers and deliver them as the next `session/prompt` at the
//!   turn boundary. The session stays parked between turns while the
//!   steering mailbox lives.
//! - Interrupt: `session/cancel`, escalating SIGTERM → SIGKILL; the stream
//!   always ends with `Done { status: Interrupted }`.

mod config;
mod discovery;
mod launch;
mod models;
mod normalize;
mod run;
mod session;
mod spec;
mod subagent;
mod turn;

use std::path::PathBuf;
use std::time::Duration;

use hearth_proto::{Model, SlashCommand};

pub(crate) use launch::find_on_paths;
use spec::AcpAgentSpec;
pub use spec::prewarm_managed_adapters;

/// The ACP harness. Construct with [`AcpHarness::grok`]; tests point it at a
/// fake agent with [`AcpHarness::with_executable`].
///
/// Each phase lives in its own module with an `impl` block here: [`launch`]
/// resolves and spawns the agent, [`discovery`] runs the short-lived probes,
/// [`session`] carries the [`Harness`](crate::Harness) impl, [`turn`] the
/// per-turn helpers, and [`run`] the live session loop.
pub struct AcpHarness {
    pub(crate) spec: AcpAgentSpec,
    pub(crate) executable: Option<PathBuf>,
    /// Override of the agent's on-disk sessions root (grok's
    /// `~/.grok/sessions`), where subagent transcripts are tailed from.
    pub(crate) sessions_root: Option<PathBuf>,
    /// Grace between `session/cancel` and SIGTERM.
    pub(crate) interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    pub(crate) kill_grace: Duration,
    /// Bound on the initialize → session handshake; a hang past it errors the
    /// run instead of spinning "Working" forever.
    pub(crate) handshake_timeout: Duration,
    /// Bound on the initialize → session/new probe used to populate the model
    /// picker. OpenCode shares this with its real startup budget because both
    /// paths wait for the same plugin-heavy boot.
    pub(crate) model_discovery_timeout: Duration,
    /// Discovery result cache: the advertised commands survive across calls.
    pub(crate) commands: tokio::sync::OnceCell<Vec<SlashCommand>>,
    /// Model discovery cache: only a successful, non-empty probe is cached,
    /// so a mis-authed agent retries on the next picker open.
    pub(crate) models_cache: tokio::sync::OnceCell<Vec<Model>>,
    /// Coalesce concurrent picker/title probes. Starting several OpenCode
    /// processes at once makes cold plugin loading slower and wastes memory.
    pub(crate) models_probe: tokio::sync::Mutex<()>,
}

/// A fresh id for an assistant message or an input question.
fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
