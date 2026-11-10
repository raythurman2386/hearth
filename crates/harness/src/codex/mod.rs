//! Codex harness: spawns the installed `codex` CLI as `codex app-server` and
//! speaks JSON-RPC 2.0 over stdio — the same interface the Codex IDE extension
//! uses. Resurrected from the pre-ACP driver and modernized.
//!
//! VERSION PIN: the app-server API is EXPERIMENTAL (`capabilities.
//! experimentalApi`); this driver is validated against codex-cli 0.146.1 —
//! revalidate the method/notification surface when bumping past it.
//!
//! - `initialize` handshake (clientInfo + `capabilities.experimentalApi`) then
//!   the `initialized` notification; unknown notification methods tolerated.
//! - `thread/start` (or `thread/resume` with a fresh-start fallback) →
//!   `SessionStarted`; `turn/start` carries the prompt, model, effort,
//!   `sandboxPolicy`, and approval policy.
//! - Notifications map to [`AgentEvent`]s: agentMessage/reasoning deltas (both
//!   `delta`/`textDelta` spellings), item lifecycles → typed ToolCall/ToolResult,
//!   `thread/tokenUsage/updated` → Usage, turn/completed|failed|aborted → Done.
//! - Approvals + sandbox: yolo mode. The wire policy is always `"never"` and
//!   the sandbox is forced to `danger-full-access` — parity with the Claude
//!   adapter's auto-approve-everything (unattended runs). Stray
//!   `item/commandExecution/requestApproval` +
//!   `item/fileChange/requestApproval` still round-trip through
//!   [`RunControls::request_input`] as a synthesized yes/no question.
//! - Subagents are full child app-server threads (`thread/started` with
//!   `source.subAgent.thread_spawn`, `subAgentActivity` items on the parent).
//!   A registered child's notifications route through an EXPLICIT table
//!   ([`normalize::route_child_notification`]) — item lifecycles/errors become
//!   tagged [`AgentEvent::Subagent`] events, child turn bookkeeping is
//!   consumed so it can never settle the parent turn, and unknown methods
//!   fall through to the parent path (fail open, never silent loss).
//! - Steering: `turn/steer { expectedTurnId }` into the live turn; a rejected
//!   steer (the turn-completed race) is queued and delivered as the next
//!   `turn/start` on the same thread. The session is persistent across turns
//!   while the steering mailbox lives.
//! - Interrupt: cancelling [`RunControls::interrupt`] sends `turn/interrupt`,
//!   escalating to SIGTERM → SIGKILL if the child is unresponsive; the stream
//!   always ends with `Done { status: Interrupted }`.

pub(crate) mod catalog;
mod discovery;
mod launch;
mod normalize;
mod run;
mod session;
mod spec;
mod turn;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hearth_proto::SlashCommand;

/// The Codex harness. Construct with [`CodexHarness::new`]; tests point it at a
/// fake app server with [`CodexHarness::with_executable`].
///
/// Each phase lives in its own module with an `impl` block here: [`spec`]
/// locates the installed CLI, [`launch`] builds the harness, [`discovery`]
/// runs the short-lived probes, [`session`] carries the
/// [`Harness`](crate::Harness) impl, [`turn`] the per-turn helpers, and
/// [`run`] the live session loop.
pub struct CodexHarness {
    pub(crate) executable: Option<PathBuf>,
    /// Grace between `turn/interrupt` and SIGTERM.
    pub(crate) interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    pub(crate) kill_grace: Duration,
    /// Command discovery cache: only a successful probe is cached, so a
    /// broken CLI retries on the next picker open (ACP-harness parity).
    pub(crate) commands: tokio::sync::OnceCell<Vec<SlashCommand>>,
    /// Per-request JSON-RPC bound override; `None` uses the crate default
    /// (`HEARTH_HARNESS_RPC_TIMEOUT_MS`). Tests shrink it.
    pub(crate) request_timeout: Option<Duration>,
    /// Requests this harness's live session(s) have sent and not yet seen
    /// answered. Shared into the [`RpcClient`](crate::jsonrpc::RpcClient) so
    /// [`Harness::deterministic_turn_end`](crate::Harness::deterministic_turn_end)
    /// can keep the engine's quiesce watchdog armed while the app server owes
    /// us a reply (one counter per harness instance; concurrent chats only
    /// ever ARM the watchdog more).
    pub(crate) in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

/// A fresh id for an assistant message or an input question.
fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
