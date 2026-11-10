//! The live session loop, split into its phases around a shared [`RunState`]:
//!
//! - [`setup`] runs the interruptible handshake, opens (or resumes) the ACP
//!   session, applies the run's mode/model/effort switches and emits the events
//!   that open the run. It returns the state the remaining phases mutate, or
//!   `None` when the run is already over (its Done emitted, child reaped).
//! - [`r#loop`] drives the select over agent messages, the pending turn, the
//!   steering mailbox, the interrupt token and the settle watchdogs.
//! - [`steering`] holds the mid-turn `_session/steering` call, the steer
//!   delivery paths and the turn-start primitive they share.
//! - [`teardown`] emits the terminal Done and reaps the child.

mod r#loop;
mod setup;
mod steering;
mod teardown;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use hearth_proto::{AgentEvent, ReasoningLevel, RunRequest};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{HarnessError, StderrTail, SteerMessage};

use super::session::Session;
use super::turn::{RequestInputFn, SubagentObserver};

/// Everything the phases after setup share: the wire handles, the run's
/// configuration, and the per-turn bookkeeping the select arms mutate. Fields
/// are `pub(super)` so [`r#loop`], [`steering`] and [`teardown`] can drive it.
pub(super) struct RunState {
    // ---- wire + host ------------------------------------------------------
    pub(super) child: Child,
    pub(super) client: RpcClient,
    pub(super) incoming: mpsc::Receiver<Incoming>,
    pub(super) event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    pub(super) steering: mpsc::Receiver<SteerMessage>,
    pub(super) interrupt: CancellationToken,
    pub(super) request_input: Arc<RequestInputFn>,
    // ---- run configuration ------------------------------------------------
    pub(super) request: RunRequest,
    pub(super) agent_name: &'static str,
    pub(super) prompt_transform: fn(Option<ReasoningLevel>, &str) -> String,
    pub(super) prompt_complete_extension: bool,
    pub(super) prompt_stall: Option<Duration>,
    pub(super) stall_hint: &'static str,
    pub(super) interrupt_grace: Duration,
    pub(super) kill_grace: Duration,
    pub(super) stderr_tail: StderrTail,
    // ---- session ----------------------------------------------------------
    pub(super) session_id: String,
    pub(super) steer_ext: bool,
    pub(super) subagents: SubagentObserver,
    pub(super) assistant_message_id: String,
    pub(super) open_questions: Arc<AtomicUsize>,
    // ---- turn bookkeeping -------------------------------------------------
    pub(super) prompt_seq: u64,
    pub(super) current_prompt_id: Option<String>,
    pub(super) completed_prompts: VecDeque<String>,
    pub(super) prompt_stall_deadline: Option<Instant>,
    pub(super) turn: Option<BoxFuture<'static, Result<Value, HarnessError>>>,
    pub(super) queued_steers: VecDeque<String>,
    pub(super) steering_call: Option<(String, BoxFuture<'static, Result<Value, HarnessError>>)>,
    pub(super) steer_backlog: VecDeque<String>,
    pub(super) steering_open: bool,
    pub(super) interrupted: bool,
    pub(super) interrupt_sent: bool,
    pub(super) done_current: bool,
    pub(super) done_after_interrupt: bool,
    pub(super) escalation: Option<tokio::task::JoinHandle<()>>,
    pub(super) starve_deadline: Option<Instant>,
    pub(super) quiet_settle: Option<Duration>,
    pub(super) exec_quiet: Option<Duration>,
    pub(super) last_update_at: Instant,
    pub(super) turn_content_seen: bool,
    pub(super) open_tools: HashSet<String>,
    pub(super) had_exec: bool,
    pub(super) cancel_flush_deadline: Option<Instant>,
}

impl RunState {
    /// The window a quiet stream must stay quiet for before the blanket settle
    /// fires: an execute-kind tool this turn widens it (a compile/link can be
    /// silent for minutes without being dead).
    fn quiet_window(&self) -> Duration {
        let base = self.quiet_settle.unwrap_or_default();
        if self.had_exec {
            self.exec_quiet.map(|d| d.max(base)).unwrap_or(base)
        } else {
            base
        }
    }
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, super::new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// The per-run event loop: setup, then the select loop, then teardown.
pub(crate) async fn run_session(session: Session) {
    let Some(mut state) = setup::setup(session).await else {
        return;
    };
    state.drive().await;
    state.teardown().await;
}
