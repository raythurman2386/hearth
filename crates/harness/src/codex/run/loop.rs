//! The select loop: one task multiplexing app-server messages, the steering
//! mailbox, the interrupt token, and consumer liveness.
//!
//! Each arm carries its payload out of the `select!` as an [`Arm`] so no borrow
//! of [`RunState`] outlives the selection; the handler methods then run with
//! the whole state free.

use serde_json::Value;

use hearth_proto::{AgentEvent, DoneStatus};

use crate::SteerMessage;
use crate::jsonrpc::Incoming;

use super::super::normalize::{
    ChildRoute, Phase, delta_text, item_id, item_type, map_item, notification_thread_id,
    route_child_notification, turn_error_message, turn_id, usage_event, user_message_text,
};
use super::super::turn::{handle_server_request, rotate, send};
use super::RunState;

/// Which branch of the loop select fired, with that branch's result.
enum Arm {
    Incoming(Option<Incoming>),
    Steer(Option<SteerMessage>),
    Interrupted,
    ConsumerClosed,
}

impl RunState {
    pub(super) async fn drive(&mut self) {
        'main: loop {
            let steer_armed = self.steering_open && !self.interrupted;
            let interrupt_armed = !self.interrupt_sent;

            let arm = tokio::select! {
                inc = self.incoming.recv() => Arm::Incoming(inc),
                steer = self.steering.recv(), if steer_armed => Arm::Steer(steer),
                _ = self.interrupt.cancelled(), if interrupt_armed => Arm::Interrupted,
                _ = self.event_tx.closed() => Arm::ConsumerClosed,
            };

            let keep_going = match arm {
                Arm::Incoming(inc) => self.on_incoming(inc).await,
                Arm::Steer(steer) => self.on_steer(steer).await,
                Arm::Interrupted => self.on_interrupt().await,
                Arm::ConsumerClosed => false,
            };
            if !keep_going {
                break 'main;
            }
        }
    }

    /// One incoming wire message (or the reader's EOF).
    async fn on_incoming(&mut self, inc: Option<Incoming>) -> bool {
        match inc {
            Some(Incoming::Notification { method, params }) => {
                self.on_notification(method, params).await
            }
            Some(Incoming::Request { id, method, params }) => {
                handle_server_request(
                    &self.client,
                    id,
                    &method,
                    &params,
                    self.request.auto_approve,
                    &self.request_input,
                );
                true
            }
            // stdout EOF or reader gone: the app server exited.
            Some(Incoming::Eof) | None => false,
        }
    }

    /// One app-server notification: child-thread traffic first, then the
    /// parent turn's own routing.
    async fn on_notification(&mut self, method: String, params: Value) -> bool {
        // Foreign-thread traffic FIRST: a child thread's turn/thread
        // bookkeeping must never reach the parent turn router below
        // (a child's turn/completed would settle the PARENT turn).
        if let Some(nthread) = notification_thread_id(&method, &params)
            && !nthread.is_empty()
            && nthread != self.thread_id
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
                self.children
                    .entry(nthread.clone())
                    .or_insert_with(|| nthread.clone());
            }
            match route_child_notification(&method) {
                ChildRoute::Parent => {
                    // Unknown/parent-owned: fall through so a codex
                    // update degrades to "the parent sees it",
                    // never silent loss.
                }
                ChildRoute::Consumed => return true,
                ChildRoute::Subagent => {
                    // Attributed child traffic — but only for a
                    // REGISTERED child; pre-registration lifecycle
                    // (captured ordering: a child's status change
                    // can precede its registration) is dropped, not
                    // passed to the parent path.
                    if let Some(parent_call) = self.children.get(&nthread).cloned() {
                        let events = child_events(&method, &params, &nthread);
                        for ev in events {
                            let wrapped = AgentEvent::Subagent {
                                parent_tool_use_id: parent_call.clone(),
                                event: Box::new(ev),
                            };
                            if !send(&self.event_tx, wrapped).await {
                                return false;
                            }
                        }
                    }
                    return true;
                }
            }
        }
        match method.as_str() {
            "turn/started" => self.router.note_started(turn_id(&params)),

            "item/agentMessage/delta" => {
                self.streamed_text.insert(item_id(&params));
                if let Some(text) = delta_text(&params)
                    && !send(&self.event_tx, AgentEvent::TextDelta { text }).await
                {
                    return false;
                }
            }

            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                if let Some(text) = delta_text(&params)
                    && !send(&self.event_tx, AgentEvent::ReasoningDelta { text }).await
                {
                    return false;
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
                if matches!(item_type(&item), "subAgentActivity" | "sub_agent_activity") {
                    let child = item
                        .get("agentThreadId")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let path = item.get("agentPath").and_then(Value::as_str).unwrap_or("");
                    let call = item.get("id").and_then(Value::as_str).unwrap_or("");
                    if !child.is_empty()
                        && child != self.thread_id
                        && path != "/root"
                        && path != "/"
                        && !call.is_empty()
                    {
                        self.children.insert(child.to_owned(), call.to_owned());
                    } else if child == self.thread_id || path == "/root" || path == "/" {
                        // The root's own activity marker: no chip, no
                        // registration — it is not a subagent.
                        return true;
                    }
                }
                if matches!(item_type(&item), "agentMessage" | "agent_message") {
                    if phase == Phase::Completed {
                        // Fallback for non-streamed messages only.
                        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                        if !self.streamed_text.contains(id)
                            && !text.is_empty()
                            && !send(&self.event_tx, AgentEvent::TextDelta { text: text.into() })
                                .await
                        {
                            return false;
                        }
                        // Codex emits several assistant messages per
                        // turn (commentary, final answer); their
                        // deltas carry no separator, so consecutive
                        // messages rendered concatenated
                        // ("…waiting.Beta's 90-second…" — live
                        // finding). Close each message as a
                        // paragraph.
                        if !send(
                            &self.event_tx,
                            AgentEvent::TextDelta {
                                text: "\n\n".into(),
                            },
                        )
                        .await
                        {
                            return false;
                        }
                        // Deltas are token chunks, not steering
                        // boundaries: the completed item is the
                        // provider-authoritative end of the text part.
                        let (prev, _next) = rotate(&mut self.assistant_message_id);
                        if !send(
                            &self.event_tx,
                            AgentEvent::AssistantMessageCompleted {
                                assistant_message_id: prev,
                            },
                        )
                        .await
                        {
                            return false;
                        }
                    }
                } else {
                    for ev in map_item(phase, &item) {
                        if !send(&self.event_tx, ev).await {
                            return false;
                        }
                    }
                }
            }

            "thread/tokenUsage/updated" => {
                if let Some(usage) = usage_event(&params) {
                    self.pending_usage = Some(usage);
                }
            }

            "turn/completed" => {
                let id = turn_id(&params);
                self.router.note_completed(&id);
                // Item ids never span turns; without this the set grew
                // one entry per message for a persistent session's life.
                self.streamed_text.clear();
                if let Some(usage) = self.pending_usage.take()
                    && !send(&self.event_tx, usage).await
                {
                    return false;
                }
                let error = turn_error_message(&params).or_else(|| {
                    (params.pointer("/turn/status").and_then(Value::as_str) == Some("failed"))
                        .then(|| "Codex turn failed".to_owned())
                });
                let status = if self.interrupted {
                    DoneStatus::Interrupted
                } else if error.is_some() {
                    DoneStatus::Errored
                } else {
                    DoneStatus::Completed
                };
                self.done_current = true;
                if !send(
                    &self.event_tx,
                    AgentEvent::Done {
                        status,
                        result: None,
                        error,
                        session_id: Some(self.thread_id.clone()),
                    },
                )
                .await
                {
                    return false;
                }
                if self.interrupted {
                    self.done_after_interrupt = true;
                    return false;
                }
                // Persistent session: a steer that lost the race with
                // this turn's end becomes the next turn now; otherwise
                // stay alive for the mailbox — the caller owns teardown.
                if let Some(text) = self.queued_steers.pop_front() {
                    if !self.start_steer_turn(text).await {
                        return false;
                    }
                } else if !self.steering_open {
                    return false;
                }
            }

            "turn/failed" => {
                self.router.note_completed(&turn_id(&params));
                if let Some(usage) = self.pending_usage.take()
                    && !send(&self.event_tx, usage).await
                {
                    return false;
                }
                self.done_current = true;
                if self.interrupted {
                    self.done_after_interrupt = true;
                }
                let _ = send(
                    &self.event_tx,
                    AgentEvent::Done {
                        status: if self.interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Errored
                        },
                        result: None,
                        error: Some(
                            turn_error_message(&params)
                                .unwrap_or_else(|| "Codex turn failed".into()),
                        ),
                        session_id: Some(self.thread_id.clone()),
                    },
                )
                .await;
                return false;
            }

            "turn/aborted" => {
                self.router.note_completed(&turn_id(&params));
                self.done_current = true;
                if self.interrupted {
                    self.done_after_interrupt = true;
                }
                let _ = send(
                    &self.event_tx,
                    AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(self.thread_id.clone()),
                    },
                )
                .await;
                return false;
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
                if !send(&self.event_tx, AgentEvent::Error { message }).await {
                    return false;
                }
            }

            // thread/status, mcpServer startup, account noise, … —
            // unknown notification methods are tolerated by design.
            _ => {}
        }
        true
    }

    /// Cancelling [`RunControls::interrupt`](crate::RunControls::interrupt):
    /// `turn/interrupt` plus a SIGTERM → SIGKILL escalation the teardown
    /// disarms.
    async fn on_interrupt(&mut self) -> bool {
        self.interrupt_sent = true;
        self.interrupted = true;
        if let Some(turn) = self.router.active.clone() {
            let client = self.client.clone();
            let thread = self.thread_id.clone();
            tokio::spawn(async move {
                if let Err(e) = client
                    .request(
                        "turn/interrupt",
                        serde_json::json!({ "threadId": thread, "turnId": turn }),
                    )
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
            if let Some(pid) = self.child.id() {
                let (interrupt_grace, kill_grace) = (self.interrupt_grace, self.kill_grace);
                self.escalation = Some(tokio::spawn(async move {
                    tokio::time::sleep(interrupt_grace).await;
                    crate::send_signal(pid, crate::Signal::Term);
                    tokio::time::sleep(kill_grace).await;
                    crate::send_signal(pid, crate::Signal::Kill);
                }));
            }
        } else {
            // Idle between turns: nothing to interrupt — the terminal
            // bookkeeping below still guarantees Done { Interrupted }.
            return false;
        }
        true
    }
}

/// The events a CHILD app-server thread's notification contributes, tagged by
/// the parent spawn-call id at the call site.
fn child_events(method: &str, params: &Value, nthread: &str) -> Vec<AgentEvent> {
    match method {
        // The child settling its turn IS the
        // subagent finishing its assignment —
        // the chip's terminal state.
        "turn/completed" => vec![AgentEvent::Done {
            status: if turn_error_message(params).is_some()
                || params.pointer("/turn/status").and_then(Value::as_str) == Some("failed")
            {
                DoneStatus::Errored
            } else {
                DoneStatus::Completed
            },
            result: None,
            error: None,
            session_id: Some(nthread.to_owned()),
        }],
        "turn/failed" => vec![AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: turn_error_message(params),
            session_id: Some(nthread.to_owned()),
        }],
        "turn/aborted" => vec![AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some(nthread.to_owned()),
        }],
        "item/agentMessage/delta" => delta_text(params)
            .map(|text| AgentEvent::TextDelta { text })
            .into_iter()
            .collect(),
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => delta_text(params)
            .map(|text| AgentEvent::ReasoningDelta { text })
            .into_iter()
            .collect(),
        "item/started" | "item/completed" => {
            let phase = if method == "item/started" {
                Phase::Started
            } else {
                Phase::Completed
            };
            let item = params.get("item").cloned().unwrap_or(Value::Null);
            // Same paragraphing as the parent:
            // a child's completed message ends
            // a paragraph in its transcript.
            if phase == Phase::Completed
                && matches!(item_type(&item), "agentMessage" | "agent_message")
            {
                vec![AgentEvent::TextDelta {
                    text: "\n\n".into(),
                }]
            } else if matches!(item_type(&item), "userMessage" | "user_message") {
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
                .or_else(|| params.get("message").and_then(Value::as_str))
                .unwrap_or("Codex subagent error")
                .to_owned(),
        }],
        "thread/closed" => vec![AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some(nthread.to_owned()),
        }],
        _ => Vec::new(),
    }
}
