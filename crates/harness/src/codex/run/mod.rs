//! The live session loop, split into its phases around a shared [`RunState`]:
//!
//! - [`setup`] runs the handshake, opens (or resumes) the app-server thread,
//!   emits `SessionStarted` and starts the first turn. It returns the state the
//!   remaining phases mutate, or `None` when the run is already over (its Done
//!   emitted, child reaped).
//! - [`r#loop`] drives the select over app-server messages, the steering
//!   mailbox, the interrupt token and consumer liveness.
//! - [`steering`] holds the steer delivery paths (`turn/steer`, and the
//!   follow-up `turn/start` fallback).
//! - [`teardown`] emits the terminal Done and reaps the child.

mod r#loop;
mod setup;
mod steering;
mod teardown;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use hearth_proto::{AgentEvent, RunRequest};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{HarnessError, StderrTail, SteerMessage};

use super::session::Session;
use super::turn::{RequestInputFn, TurnRouter};

/// Everything the phases after setup share. Fields are `pub(super)` so
/// [`r#loop`], [`steering`] and [`teardown`] can reach the same state.
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
    /// Wire approval policy for thread start and every turn ("never" — yolo
    /// mode, parity with the Claude adapter's auto-approve).
    pub(super) approval_policy: &'static str,
    pub(super) effort: Option<&'static str>,
    pub(super) service_tier: Option<String>,
    pub(super) interrupt_grace: Duration,
    pub(super) kill_grace: Duration,
    pub(super) stderr_tail: StderrTail,
    // ---- session ----------------------------------------------------------
    pub(super) thread_id: String,
    pub(super) assistant_message_id: String,
    pub(super) router: TurnRouter,
    /// Child app-server threads (multi-agent v2): child thread id → the
    /// parent-feed spawn call id its traffic is attributed to.
    pub(super) children: HashMap<String, String>,
    // ---- turn bookkeeping -------------------------------------------------
    /// Deltas seen per agent-message item, so a model that never streams
    /// (item/completed only) still emits its text exactly once.
    pub(super) streamed_text: HashSet<String>,
    /// Token usage is held until the turn ends, emitted just before Done.
    pub(super) pending_usage: Option<AgentEvent>,
    /// Steers whose `turn/steer` lost the turn-completed race; delivered as the
    /// next `turn/start` when the expected turn's end notification arrives.
    pub(super) queued_steers: VecDeque<String>,
    pub(super) steering_open: bool,
    pub(super) interrupted: bool,
    pub(super) interrupt_sent: bool,
    /// A Done has been emitted for the turn currently/last in flight.
    pub(super) done_current: bool,
    pub(super) done_after_interrupt: bool,
    pub(super) escalation: Option<tokio::task::JoinHandle<()>>,
}

impl RunState {
    /// The `turn/start` params for one prompt: the thread, the text, the
    /// sandbox/approval policy and the run's model/effort/service tier.
    pub(super) fn turn_params(&self, text: &str) -> Value {
        let mut p = serde_json::Map::new();
        p.insert("threadId".into(), Value::String(self.thread_id.clone()));
        p.insert(
            "input".into(),
            serde_json::json!([{ "type": "text", "text": text }]),
        );
        p.insert("approvalPolicy".into(), self.approval_policy.into());
        p.insert(
            "sandboxPolicy".into(),
            super::catalog::sandbox_policy_value(self.request.sandbox),
        );
        // Reasoning summaries stream (`item/reasoning/summaryTextDelta`) only
        // when asked for — without this codex "thinks" in silence for minutes:
        // nothing renders and the UI's 45s staleness gate flips Working off
        // (user report: "not streaming, doesn't say it's working").
        p.insert("summary".into(), "auto".into());
        if let Some(model) = &self.request.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(effort) = self.effort {
            p.insert("effort".into(), effort.into());
        }
        if let Some(tier) = &self.service_tier {
            p.insert("serviceTier".into(), Value::String(tier.clone()));
        }
        Value::Object(p)
    }
}

/// The per-run event loop: setup, then the select loop, then teardown.
pub(crate) async fn run_session(session: Session) {
    let Some(mut state) = setup::setup(session).await else {
        return;
    };
    state.drive().await;
    state.teardown().await;
}
