//! The live session loop: one task multiplexing app-server messages, the
//! steering mailbox, the interrupt token, and consumer liveness.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde_json::{Value, json};

use hearth_proto::{AgentEvent, DoneStatus, HarnessId};

use crate::jsonrpc::Incoming;
use crate::{HarnessError, RunControls, Signal, send_signal, shutdown_child};

use super::catalog::{sandbox_mode, sandbox_policy_value, to_effort};
use super::new_message_id;
use super::normalize::{
    ChildRoute, Phase, delta_text, item_id, item_type, map_item, notification_thread_id,
    route_child_notification, turn_error_message, turn_id, usage_event, user_message_text,
};
use super::session::Session;
use super::turn::{TurnRouter, handle_server_request, rotate, send, start_turn, steer_as_new_turn};

/// The per-run event loop: one task multiplexing app-server messages, the
/// steering mailbox, the interrupt token, and consumer liveness.
pub(crate) async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
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
    let setup = async {
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
    };
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
                return;
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
            return;
        }
    };

    let turn_params = |text: &str| -> Value {
        let mut p = serde_json::Map::new();
        p.insert("threadId".into(), Value::String(thread_id.clone()));
        p.insert("input".into(), json!([{ "type": "text", "text": text }]));
        p.insert("approvalPolicy".into(), approval_policy.into());
        p.insert(
            "sandboxPolicy".into(),
            sandbox_policy_value(request.sandbox),
        );
        // Reasoning summaries stream (`item/reasoning/summaryTextDelta`) only
        // when asked for — without this codex "thinks" in silence for minutes:
        // nothing renders and the UI's 45s staleness gate flips Working off
        // (user report: "not streaming, doesn't say it's working").
        p.insert("summary".into(), "auto".into());
        if let Some(model) = &request.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(effort) = effort {
            p.insert("effort".into(), effort.into());
        }
        if let Some(tier) = &service_tier {
            p.insert("serviceTier".into(), Value::String(tier.clone()));
        }
        Value::Object(p)
    };

    let mut assistant_message_id = new_message_id();
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
        return;
    }

    let mut router = TurnRouter::default();
    // Child app-server threads (multi-agent v2): child thread id → the
    // parent-feed spawn call id its traffic is attributed to. Registered
    // from `subAgentActivity` items on the parent thread and from a child
    // `thread/started` carrying a spawn source.
    let mut children: HashMap<String, String> = HashMap::new();
    match start_turn(&client, turn_params(&request.prompt)).await {
        Ok(id) => router.adopt_started(id),
        Err(e) => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(e.to_string()),
                    session_id: Some(thread_id.clone()),
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    }

    // ---- main loop --------------------------------------------------------
    // Deltas seen per agent-message item, so a model that never streams
    // (item/completed only) still emits its text exactly once.
    let mut streamed_text: HashSet<String> = HashSet::new();
    // Token usage is held until the turn ends, emitted just before Done.
    let mut pending_usage: Option<AgentEvent> = None;
    // Steers whose `turn/steer` lost the turn-completed race; delivered as the
    // next `turn/start` when the expected turn's end notification arrives.
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    // A Done has been emitted for the turn currently/last in flight.
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                // Foreign-thread traffic FIRST: a child thread's turn/thread
                // bookkeeping must never reach the parent turn router below
                // (a child's turn/completed would settle the PARENT turn).
                if let Some(nthread) = notification_thread_id(&method, &params)
                    && !nthread.is_empty()
                    && nthread != thread_id
                {
                    // Registration path: a child thread/started with an
                    // explicit spawn source. The spawn-call mapping from a
                    // subAgentActivity item wins (that id IS the parent
                    // chip); this only seeds a fallback.
                    if method == "thread/started"
                        && params
                            .pointer("/thread/source/subAgent/thread_spawn")
                            .is_some()
                    {
                        children
                            .entry(nthread.clone())
                            .or_insert_with(|| nthread.clone());
                    }
                    match route_child_notification(&method) {
                        ChildRoute::Parent => {
                            // Unknown/parent-owned: fall through so a codex
                            // update degrades to "the parent sees it",
                            // never silent loss.
                        }
                        ChildRoute::Consumed => continue,
                        ChildRoute::Subagent => {
                            // Attributed child traffic — but only for a
                            // REGISTERED child; pre-registration lifecycle
                            // (captured ordering: a child's status change
                            // can precede its registration) is dropped, not
                            // passed to the parent path.
                            if let Some(parent_call) = children.get(&nthread).cloned() {
                                let events: Vec<AgentEvent> = match method.as_str() {
                                    // The child settling its turn IS the
                                    // subagent finishing its assignment —
                                    // the chip's terminal state.
                                    "turn/completed" => vec![AgentEvent::Done {
                                        status: if turn_error_message(&params).is_some()
                                            || params
                                                .pointer("/turn/status")
                                                .and_then(Value::as_str)
                                                == Some("failed")
                                        {
                                            DoneStatus::Errored
                                        } else {
                                            DoneStatus::Completed
                                        },
                                        result: None,
                                        error: None,
                                        session_id: Some(nthread.clone()),
                                    }],
                                    "turn/failed" => vec![AgentEvent::Done {
                                        status: DoneStatus::Errored,
                                        result: None,
                                        error: turn_error_message(&params),
                                        session_id: Some(nthread.clone()),
                                    }],
                                    "turn/aborted" => vec![AgentEvent::Done {
                                        status: DoneStatus::Interrupted,
                                        result: None,
                                        error: None,
                                        session_id: Some(nthread.clone()),
                                    }],
                                    "item/agentMessage/delta" => delta_text(&params)
                                        .map(|text| AgentEvent::TextDelta { text })
                                        .into_iter()
                                        .collect(),
                                    "item/reasoning/textDelta"
                                    | "item/reasoning/summaryTextDelta" => delta_text(&params)
                                        .map(|text| AgentEvent::ReasoningDelta { text })
                                        .into_iter()
                                        .collect(),
                                    "item/started" | "item/completed" => {
                                        let phase = if method == "item/started" {
                                            Phase::Started
                                        } else {
                                            Phase::Completed
                                        };
                                        let item =
                                            params.get("item").cloned().unwrap_or(Value::Null);
                                        // Same paragraphing as the parent:
                                        // a child's completed message ends
                                        // a paragraph in its transcript.
                                        if phase == Phase::Completed
                                            && matches!(
                                                item_type(&item),
                                                "agentMessage" | "agent_message"
                                            )
                                        {
                                            vec![AgentEvent::TextDelta {
                                                text: "\n\n".into(),
                                            }]
                                        } else if matches!(
                                            item_type(&item),
                                            "userMessage" | "user_message"
                                        ) {
                                            // A CHILD thread's user message
                                            // is the parent steering it (the
                                            // collab send_message path) —
                                            // its own entry in the subagent
                                            // doc. Completed only: both
                                            // lifecycle events carry the
                                            // full item.
                                            if phase == Phase::Completed {
                                                user_message_text(&item)
                                                    .map(|text| AgentEvent::UserMessage { text })
                                                    .into_iter()
                                                    .collect()
                                            } else {
                                                Vec::new()
                                            }
                                        } else {
                                            map_item(phase, &item)
                                        }
                                    }
                                    "error" => vec![AgentEvent::Error {
                                        message: params
                                            .pointer("/error/message")
                                            .and_then(Value::as_str)
                                            .or_else(|| {
                                                params.get("message").and_then(Value::as_str)
                                            })
                                            .unwrap_or("Codex subagent error")
                                            .to_owned(),
                                    }],
                                    "thread/closed" => vec![AgentEvent::Done {
                                        status: DoneStatus::Completed,
                                        result: None,
                                        error: None,
                                        session_id: Some(nthread.clone()),
                                    }],
                                    _ => Vec::new(),
                                };
                                for ev in events {
                                    let wrapped = AgentEvent::Subagent {
                                        parent_tool_use_id: parent_call.clone(),
                                        event: Box::new(ev),
                                    };
                                    if !send(&event_tx, wrapped).await {
                                        break 'main;
                                    }
                                }
                            }
                            continue;
                        }
                    }
                }
                match method.as_str() {
                    "turn/started" => router.note_started(turn_id(&params)),

                    "item/agentMessage/delta" => {
                        streamed_text.insert(item_id(&params));
                        if let Some(text) = delta_text(&params)
                            && !send(&event_tx, AgentEvent::TextDelta { text }).await
                        {
                            break 'main;
                        }
                    }

                    "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                        if let Some(text) = delta_text(&params)
                            && !send(&event_tx, AgentEvent::ReasoningDelta { text }).await
                        {
                            break 'main;
                        }
                    }

                    "item/started" | "item/completed" => {
                        let phase = if method == "item/started" {
                            Phase::Started
                        } else {
                            Phase::Completed
                        };
                        let item = params.get("item").cloned().unwrap_or(Value::Null);
                        // A subAgentActivity item on the parent thread names a
                        // child: register it (its call id = the parent chip
                        // its traffic is attributed to). NEVER the root
                        // thread itself — the wire emits subAgentActivity
                        // about the root during collab runs, and registering
                        // it would intercept every subsequent root
                        // notification including turn/completed (the thread
                        // would hang Working after the fleet finished).
                        if matches!(
                            item_type(&item),
                            "subAgentActivity" | "sub_agent_activity"
                        ) {
                            let child = item
                                .get("agentThreadId")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let path = item
                                .get("agentPath")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let call = item.get("id").and_then(Value::as_str).unwrap_or("");
                            if !child.is_empty()
                                && child != thread_id
                                && path != "/root"
                                && path != "/"
                                && !call.is_empty()
                            {
                                children.insert(child.to_owned(), call.to_owned());
                            } else if child == thread_id || path == "/root" || path == "/" {
                                // The root's own activity marker: no chip, no
                                // registration — it is not a subagent.
                                continue;
                            }
                        }
                        if matches!(item_type(&item), "agentMessage" | "agent_message") {
                            if phase == Phase::Completed {
                                // Fallback for non-streamed messages only.
                                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                                if !streamed_text.contains(id)
                                    && !text.is_empty()
                                    && !send(&event_tx, AgentEvent::TextDelta { text: text.into() }).await
                                {
                                    break 'main;
                                }
                                // Codex emits several assistant messages per
                                // turn (commentary, final answer); their
                                // deltas carry no separator, so consecutive
                                // messages rendered concatenated
                                // ("…waiting.Beta's 90-second…" — live
                                // finding). Close each message as a
                                // paragraph.
                                if !send(
                                    &event_tx,
                                    AgentEvent::TextDelta {
                                        text: "\n\n".into(),
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                                // Deltas are token chunks, not steering
                                // boundaries: the completed item is the
                                // provider-authoritative end of the text part.
                                let (prev, _next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::AssistantMessageCompleted {
                                        assistant_message_id: prev,
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                        } else {
                            for ev in map_item(phase, &item) {
                                if !send(&event_tx, ev).await {
                                    break 'main;
                                }
                            }
                        }
                    }

                    "thread/tokenUsage/updated" => {
                        if let Some(usage) = usage_event(&params) {
                            pending_usage = Some(usage);
                        }
                    }

                    "turn/completed" => {
                        let id = turn_id(&params);
                        router.note_completed(&id);
                        // Item ids never span turns; without this the set grew
                        // one entry per message for a persistent session's life.
                        streamed_text.clear();
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        let error = turn_error_message(&params).or_else(|| {
                            (params
                                .pointer("/turn/status")
                                .and_then(Value::as_str)
                                == Some("failed"))
                            .then(|| "Codex turn failed".to_owned())
                        });
                        let status = if interrupted {
                            DoneStatus::Interrupted
                        } else if error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        done_current = true;
                        if !send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        if interrupted {
                            done_after_interrupt = true;
                            break 'main;
                        }
                        // Persistent session: a steer that lost the race with
                        // this turn's end becomes the next turn now; otherwise
                        // stay alive for the mailbox — the caller owns teardown.
                        if let Some(text) = queued_steers.pop_front() {
                            if !steer_as_new_turn(
                                &client,
                                turn_params(&text),
                                &mut router,
                                &event_tx,
                                &mut assistant_message_id,
                                &mut done_current,
                            )
                            .await
                            {
                                break 'main;
                            }
                        } else if !steering_open {
                            break 'main;
                        }
                    }

                    "turn/failed" => {
                        router.note_completed(&turn_id(&params));
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: if interrupted {
                                    DoneStatus::Interrupted
                                } else {
                                    DoneStatus::Errored
                                },
                                result: None,
                                error: Some(
                                    turn_error_message(&params)
                                        .unwrap_or_else(|| "Codex turn failed".into()),
                                ),
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "turn/aborted" => {
                        router.note_completed(&turn_id(&params));
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: DoneStatus::Interrupted,
                                result: None,
                                error: None,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "error" => {
                        // 0.146.x nests it (`params.error.message`); older
                        // builds were flat (`params.message`) — accept both.
                        let message = params
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .or_else(|| params.get("message").and_then(Value::as_str))
                            .unwrap_or("Codex error")
                            .to_owned();
                        if !send(&event_tx, AgentEvent::Error { message }).await {
                            break 'main;
                        }
                    }

                    // thread/status, mcpServer startup, account noise, … —
                    // unknown notification methods are tolerated by design.
                    _ => {}
                }
                }

                Some(Incoming::Request { id, method, params }) => {
                    handle_server_request(
                        &client,
                        id,
                        &method,
                        &params,
                        request.auto_approve,
                        &request_input,
                    );
                }

                // stdout EOF or reader gone: the app server exited.
                Some(Incoming::Eof) | None => break 'main,
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let text = msg.prompt;
                    if let Some(expected) = router.active.clone() {
                        let steer_params = json!({
                            "threadId": thread_id,
                            "expectedTurnId": expected,
                            "input": [{ "type": "text", "text": text }],
                        });
                        match client.request("turn/steer", steer_params).await {
                            Ok(_) => {
                                let (prev, next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::Steered {
                                        assistant_message_id: Some(prev),
                                        next_assistant_message_id: Some(next),
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                            // A failed `turn/steer` does NOT mean the text is
                            // bad: most commonly the active turn finished
                            // between the UI send and this request. Queue it
                            // for redelivery as the next `turn/start` when the
                            // expected turn's end arrives (also the safe
                            // fallback for older Codex without steering).
                            Err(e) => {
                                tracing::debug!(
                                    target: "hearth_harness::codex",
                                    "turn/steer rejected (queued as next turn): {e}"
                                );
                                if router.active.as_deref() == Some(expected.as_str())
                                    && !router.is_completed(&expected)
                                {
                                    queued_steers.push_back(text);
                                } else if !steer_as_new_turn(
                                    &client,
                                    turn_params(&text),
                                    &mut router,
                                    &event_tx,
                                    &mut assistant_message_id,
                                    &mut done_current,
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                        }
                    } else if !steer_as_new_turn(
                        &client,
                        turn_params(&text),
                        &mut router,
                        &event_tx,
                        &mut assistant_message_id,
                        &mut done_current,
                    )
                    .await
                    {
                        break 'main;
                    }
                }
                None => {
                    // Mailbox closed (the caller's graceful idle-reap): finish
                    // once nothing is in flight — mirrors codex.ts's steer loop
                    // `finish()` on a null take.
                    steering_open = false;
                    if router.active.is_none() && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if let Some(turn) = router.active.clone() {
                    let client = client.clone();
                    let thread = thread_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client
                            .request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }))
                            .await
                        {
                            tracing::debug!(
                                target: "hearth_harness::codex",
                                "turn/interrupt failed (escalation will reap): {e}"
                            );
                        }
                    });
                    // Escalate if the app server doesn't wind down (turn/aborted)
                    // within the grace periods: SIGTERM, then SIGKILL.
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to interrupt — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child KILLED mid-turn (OS memory pressure, `killall codex`)
            // must not read as a silent success — codex.ts's signal-death
            // handling, reduced to the turn-in-flight case.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message(
                        "codex app-server",
                        status,
                        &stderr_tail,
                    )),
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        }
    }

    // Escalation dies BEFORE the child is reaped: after `shutdown_child`
    // waits the pid, a still-armed SIGTERM/SIGKILL timer would fire at a
    // freed (reusable) pid — worse now that the signal targets the whole
    // process group. Same ordering as the ACP session loop.
    if let Some(handle) = escalation {
        handle.abort();
    }
    shutdown_child(&mut child, kill_grace).await;
}
