//! Handshake + thread setup: `initialize`/`initialized`, `thread/start` (or
//! `thread/resume` with a fresh-thread fallback), the `SessionStarted` event
//! and the run's first turn. On failure the terminal Done is emitted and the
//! child reaped here, so the caller has nothing left to do.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde_json::{Value, json};

use hearth_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest};

use crate::jsonrpc::RpcClient;
use crate::{HarnessError, RunControls, shutdown_child};

use super::super::catalog::{sandbox_mode, to_effort};
use super::super::new_message_id;
use super::super::session::Session;
use super::super::turn::{TurnRouter, send, start_turn};
use super::RunState;

/// The handshake + thread open: `initialize` / `initialized` then
/// `thread/start` (or `thread/resume` with a fresh-thread fallback). Returns
/// the thread id.
async fn handshake(
    client: &RpcClient,
    request: &RunRequest,
    start_params: &serde_json::Map<String, Value>,
) -> Result<String, HarnessError> {
    client
        .request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "hearth-native",
                    "title": "Hearth",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            }),
        )
        .await?;
    client.notify("initialized", None);

    let thread = if let Some(resume) = &request.resume {
        let mut p = start_params.clone();
        p.insert("threadId".into(), Value::String(resume.clone()));
        match client.request("thread/resume", Value::Object(p)).await {
            Ok(thread) => thread,
            // A missing/foreign rollout falls back to a fresh thread.
            Err(e) => {
                tracing::debug!(
                    target: "hearth_harness::codex",
                    "thread/resume failed (starting fresh): {e}"
                );
                client
                    .request("thread/start", Value::Object(start_params.clone()))
                    .await?
            }
        }
    } else {
        client
            .request("thread/start", Value::Object(start_params.clone()))
            .await?
    };
    let thread_id = thread["thread"]["id"].as_str().unwrap_or("").to_owned();
    Ok::<String, HarnessError>(thread_id)
}

/// Run the interruptible handshake and open the thread. `None` means the run
/// is already over: the terminal Done was emitted (a setup failure, an
/// interrupt before the handshake finished, or a failed first `turn/start`)
/// and the child reaped.
pub(super) async fn setup(session: Session) -> Option<RunState> {
    let Session {
        mut child,
        client,
        incoming,
        event_tx,
        controls,
        request,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        steering,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);

    // ---- wire params ------------------------------------------------------
    // Parity with the Claude adapter, which auto-approves every `can_use_tool`
    // regardless of `auto_approve` (hearth sessions run unattended; combined
    // with the danger-full-access override above this is codex's yolo mode):
    // never surface wire approvals. "on-request" turned
    // every command into a yes/no question (user report: "asking me for
    // approval at every step"). The approval-as-input plumbing below stays for
    // stray requests and a future explicit permission-mode setting.
    let approval_policy = "never";
    let effort = to_effort(request.reasoning);
    // Service tier rides thread-start and every turn (mirrors the Codex IDE
    // client). "default" means Standard — omit it entirely.
    let service_tier = request
        .model_options
        .get("serviceTier")
        .and_then(Value::as_str)
        .filter(|t| *t != "default")
        .map(str::to_owned);

    let start_params = {
        let mut p = serde_json::Map::new();
        p.insert("cwd".into(), Value::String(request.cwd.clone()));
        p.insert("approvalPolicy".into(), approval_policy.into());
        p.insert("sandbox".into(), sandbox_mode(request.sandbox).into());
        if let Some(model) = &request.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(tier) = &service_tier {
            p.insert("serviceTier".into(), Value::String(tier.clone()));
        }
        p
    };

    // ---- handshake + thread + first turn (interruptible) ------------------
    let setup = handshake(&client, &request, &start_params);
    let thread_id = tokio::select! {
        res = setup => match res {
            Ok(thread_id) => thread_id,
            Err(e) => {
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return None;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return None;
        }
    };

    let assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Codex,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: thread_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return None;
    }

    let router = TurnRouter::default();
    // Child app-server threads (multi-agent v2): child thread id → the
    // parent-feed spawn call id its traffic is attributed to. Registered
    // from `subAgentActivity` items on the parent thread and from a child
    // `thread/started` carrying a spawn source.
    let children: HashMap<String, String> = HashMap::new();
    let mut state = RunState {
        child,
        client,
        incoming,
        event_tx,
        steering,
        interrupt,
        request_input,
        request,
        approval_policy,
        effort,
        service_tier,
        interrupt_grace,
        kill_grace,
        stderr_tail,
        thread_id,
        assistant_message_id,
        router,
        children,
        streamed_text: HashSet::new(),
        pending_usage: None,
        queued_steers: VecDeque::new(),
        steering_open: true,
        interrupted: false,
        interrupt_sent: false,
        done_current: false,
        done_after_interrupt: false,
        escalation: None,
    };
    if !start_first_turn(&mut state).await {
        return None;
    }
    Some(state)
}

/// Start the run's first `turn/start`. A failure surfaces as the terminal
/// Errored Done and reaps the child, exactly like a handshake failure.
async fn start_first_turn(state: &mut RunState) -> bool {
    let params = state.turn_params(&state.request.prompt.clone());
    match start_turn(&state.client, params).await {
        Ok(id) => {
            state.router.adopt_started(id);
            true
        }
        Err(e) => {
            let _ = state
                .event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(e.to_string()),
                    session_id: Some(state.thread_id.clone()),
                }))
                .await;
            shutdown_child(&mut state.child, state.kill_grace).await;
            false
        }
    }
}
