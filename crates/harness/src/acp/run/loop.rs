//! The select loop: one task multiplexing agent messages, the pending turn,
//! the steering mailbox, the interrupt token, and consumer liveness.
//!
//! Each arm carries its payload out of the `select!` as an [`Arm`] so no borrow
//! of [`RunState`] outlives the selection; the handler methods then run with
//! the whole state free.

use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use tokio::time::Instant;

use hearth_proto::{AgentEvent, DoneStatus};

use crate::HarnessError;
use crate::jsonrpc::Incoming;

use super::super::turn::{
    handle_server_request_live, session_update_events, stop_outcome, track_turn_signals,
    usage_from_response,
};

use super::RunState;
use super::rotate;
use super::send;

/// Which branch of the loop select fired, with that branch's result.
enum Arm {
    Turn(Result<Value, HarnessError>),
    Incoming(Option<Incoming>),
    SteeringResolved(Result<Value, HarnessError>),
    CancelFlushed,
    QuietSettled,
    Starved,
    Steer(Option<crate::SteerMessage>),
    Interrupted,
    PromptStalled,
    ConsumerClosed,
}

impl RunState {
    pub(super) async fn drive(&mut self) {
        'main: loop {
            // Preconditions are hoisted out of `select!`: the branches hold
            // `&mut` borrows of disjoint fields, so they cannot also read the
            // state they guard on.
            let turn_armed = self.turn.is_some();
            let steering_resolved_armed = self.steering_call.is_some();
            let cancel_armed = self.cancel_flush_deadline.is_some() && !self.interrupted;
            let quiet_armed = self.quiet_settle.is_some()
                && self.starve_deadline.is_none()
                && self.turn.is_some()
                && !self.interrupted
                && self.turn_content_seen
                && self.open_tools.is_empty()
                && self.open_questions.load(Ordering::SeqCst) == 0;
            let starve_armed =
                self.starve_deadline.is_some() && self.turn.is_some() && !self.interrupted;
            let steer_armed = self.steering_open && !self.interrupted;
            let interrupt_armed = !self.interrupt_sent;
            let stall_armed =
                self.prompt_stall_deadline.is_some() && self.turn.is_some() && !self.interrupted;
            let cancel_at = self.cancel_flush_deadline.unwrap_or_else(Instant::now);
            let starve_at = self.starve_deadline.unwrap_or_else(Instant::now);
            let stall_at = self.prompt_stall_deadline.unwrap_or_else(Instant::now);
            let quiet_at = self.last_update_at + self.quiet_window();

            let arm = {
                let turn = &mut self.turn;
                let steering_call = &mut self.steering_call;
                tokio::select! {
                    res = async { turn.as_mut().expect("guarded by if").await },
                        if turn_armed =>
                    {
                        Arm::Turn(res)
                    }

                    inc = self.incoming.recv() => Arm::Incoming(inc),

                    res = async { steering_call.as_mut().expect("guarded by if").1.as_mut().await },
                        if steering_resolved_armed =>
                    {
                        Arm::SteeringResolved(res)
                    }

                    // Busy-session cancel flushed (see BUSY_RECENT/CANCEL_FLUSH
                    // in `steering`): the unowned self-continued turn had its
                    // cancel and a drain window; the queued steer becomes a
                    // fresh prompt on a now-idle agent.
                    _ = tokio::time::sleep_until(cancel_at), if cancel_armed => {
                        Arm::CancelFlushed
                    }

                    // BLANKET quiet settle (see `quiet_settle`): feeds the
                    // recovery arm below by expiring its deadline — one settle
                    // path for all three evidence sources.
                    _ = tokio::time::sleep_until(quiet_at), if quiet_armed => {
                        Arm::QuietSettled
                    }

                    // Starved-turn recovery: the grace elapsed with the prompt
                    // still unsettled after turn-end evidence.
                    _ = tokio::time::sleep_until(starve_at), if starve_armed => Arm::Starved,

                    steer = self.steering.recv(), if steer_armed => Arm::Steer(steer),

                    _ = self.interrupt.cancelled(), if interrupt_armed => Arm::Interrupted,

                    // Prompt-stall watchdog: a prompt was sent and NOTHING has
                    // come back on the wire at all.
                    _ = tokio::time::sleep_until(stall_at), if stall_armed => {
                        Arm::PromptStalled
                    }

                    _ = self.event_tx.closed() => Arm::ConsumerClosed,
                }
            };

            let keep_going = match arm {
                Arm::Turn(res) => self.on_turn_settled(res).await,
                Arm::Incoming(inc) => self.on_incoming(inc).await,
                Arm::SteeringResolved(res) => self.on_steering_resolved(res).await,
                Arm::CancelFlushed => self.on_cancel_flush().await,
                Arm::QuietSettled => {
                    self.on_quiet_settle();
                    true
                }
                Arm::Starved => self.on_starve().await,
                Arm::Steer(steer) => self.on_steer(steer).await,
                Arm::Interrupted => self.on_interrupt().await,
                Arm::PromptStalled => self.on_prompt_stall().await,
                Arm::ConsumerClosed => false,
            };
            if !keep_going {
                break 'main;
            }
        }
    }

    /// The pending `session/prompt` settled: fold the tail, close the
    /// assistant message, emit usage + the turn's Done, then either promote a
    /// queued steer or stay parked for the mailbox.
    async fn on_turn_settled(&mut self, res: Result<Value, HarnessError>) -> bool {
        self.turn = None;
        self.starve_deadline = None;
        self.prompt_stall_deadline = None;
        if let Some(id) = self.current_prompt_id.take() {
            self.completed_prompts.push_back(id);
            while self.completed_prompts.len() > 32 {
                self.completed_prompts.pop_front();
            }
        }
        // Settle an in-flight `_session/steering` call BEFORE closing the
        // turn: its response rides the same stdout as the prompt response.
        if !self.settle_steering_call().await {
            return false;
        }
        // Updates streamed before the prompt response are already queued in
        // stdout order — fold them into the turn before closing it (responses
        // bypass the incoming queue).
        if !self.drain_incoming().await {
            return false;
        }
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
        // Per-turn token usage, when the adapter settles the prompt with it
        // (claude-agent-acp and codex-acp both do).
        if let Some(usage) = usage_from_response(&res)
            && !send(&self.event_tx, usage).await
        {
            return false;
        }
        let (status, error) = stop_outcome(&res, self.interrupted);
        self.done_current = true;
        if self.interrupted {
            self.done_after_interrupt = true;
        }
        if !send(
            &self.event_tx,
            AgentEvent::Done {
                status,
                result: None,
                error,
                session_id: Some(self.session_id.clone()),
            },
        )
        .await
        {
            return false;
        }
        if self.interrupted || res.is_err() {
            return false;
        }
        // Persistent session: a queued steer becomes the next turn; otherwise
        // stay alive for the mailbox — the caller owns teardown (mirrors the
        // codex harness).
        if let Some(text) = self.queued_steers.pop_front() {
            if !self.begin_turn(text).await {
                return false;
            }
        } else if !self.steering_open {
            return false;
        }
        true
    }

    /// One incoming wire message (or the reader's EOF).
    async fn on_incoming(&mut self, inc: Option<Incoming>) -> bool {
        match inc {
            Some(Incoming::Notification { method, params }) => {
                self.last_update_at = Instant::now();
                // Wire traffic is a sign of life for the prompt-stall
                // watchdog — EXCEPT session boilerplate: opencode emits
                // available_commands_update right after session/new on
                // every session, including ones whose provider is down
                // (where it then retries the provider stream forever
                // with nothing further on the wire — verified live,
                // 1.18.18). One such frame must not disarm the watchdog
                // for the whole turn; only turn progress counts.
                let boilerplate = method == "session/update"
                    && matches!(
                        params
                            .get("update")
                            .and_then(|u| u.get("sessionUpdate"))
                            .and_then(Value::as_str),
                        Some("available_commands_update")
                            | Some("config_option_update")
                            | Some("current_mode_update")
                    );
                if !boilerplate {
                    self.prompt_stall_deadline = None;
                }
                // `_x.ai/session/prompt_complete` — the AUTHORITATIVE
                // turn end for agents advertising it (grok): the
                // `session/prompt` RPC can hang after the turn really
                // finished. Settle through the SAME response arm by
                // swapping the in-flight future for the synthesized
                // response; the abandoned RPC future drains in the
                // background and its late result is discarded. Guards:
                // session match, an outstanding prompt, and an exact
                // prompt-id match (a stale replay of an already-settled
                // prompt must never settle a newer turn; grok echoes the
                // `_meta.promptId` we mint — verified live, 1.0.4).
                if self.prompt_complete_extension
                    && method == "_x.ai/session/prompt_complete"
                    && !self.interrupted
                    && self.turn.is_some()
                    && params.get("sessionId").and_then(Value::as_str)
                        == Some(self.session_id.as_str())
                {
                    let pid = params
                        .get("promptId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let stale = pid
                        .as_deref()
                        .is_some_and(|p| self.completed_prompts.iter().any(|c| c == p))
                        || (pid.is_some() && pid != self.current_prompt_id);
                    if !stale {
                        let stop = params
                            .get("stopReason")
                            .and_then(Value::as_str)
                            .unwrap_or("end_turn")
                            .to_owned();
                        if let Some(old) = self.turn.take() {
                            tokio::spawn(async move {
                                let _ = old.await;
                            });
                        }
                        self.turn =
                            Some(Box::pin(async move { Ok(json!({ "stopReason": stop })) }));
                    }
                }
                // Other notifications (other sessions, agent noise) are
                // tolerated by design.
                let events =
                    session_update_events(&method, &params, &self.session_id, &mut self.subagents);
                for ev in events {
                    track_turn_signals(
                        &ev,
                        &mut self.turn_content_seen,
                        &mut self.open_tools,
                        &mut self.had_exec,
                    );
                    if !send(&self.event_tx, ev).await {
                        return false;
                    }
                }
                true
            }
            Some(Incoming::Request { id, method, params }) => {
                self.prompt_stall_deadline = None;
                let events = handle_server_request_live(
                    &self.client,
                    id,
                    &method,
                    &params,
                    &self.request_input,
                    &self.open_questions,
                );
                for ev in events {
                    if !send(&self.event_tx, ev).await {
                        return false;
                    }
                }
                true
            }
            Some(Incoming::Eof) | None => {
                // The turn ends via a request RESPONSE, which races EOF
                // through a different channel than notifications: an agent
                // exiting right after its final response must read as a
                // clean finish, not a crash. The response (if any) is
                // already resolved by the reader before it sends Eof.
                // Only a RESOLVED response is a clean finish; a request
                // failed by the reader's EOF cleanup falls through to the
                // crash-message bookkeeping below (stderr tail intact).
                if let Some(mut fut) = self.turn.take() {
                    match tokio::time::timeout(std::time::Duration::from_millis(50), &mut fut).await
                    {
                        Ok(Ok(res)) => {
                            let (prev, _next) = rotate(&mut self.assistant_message_id);
                            let _ = send(
                                &self.event_tx,
                                AgentEvent::AssistantMessageCompleted {
                                    assistant_message_id: prev,
                                },
                            )
                            .await;
                            if let Some(usage) = usage_from_response(&Ok(res.clone())) {
                                let _ = send(&self.event_tx, usage).await;
                            }
                            let (status, error) = stop_outcome(&Ok(res), self.interrupted);
                            self.done_current = true;
                            if self.interrupted {
                                self.done_after_interrupt = true;
                            }
                            let _ = send(
                                &self.event_tx,
                                AgentEvent::Done {
                                    status,
                                    result: None,
                                    error,
                                    session_id: Some(self.session_id.clone()),
                                },
                            )
                            .await;
                        }
                        // EOF failed the pending prompt: the RPC-side
                        // message ("app-server exited before responding")
                        // hides the real evidence already in hand — the
                        // child's exit status and stderr tail. Re-read the
                        // child here so the surfaced Done carries both.
                        // An in-flight interrupt is excluded: the child
                        // died because we killed it, and the terminal
                        // bookkeeping below already ends that as
                        // Done{Interrupted}.
                        Ok(Err(rpc_err)) if !self.interrupted => {
                            self.done_current = true;
                            let status = self.child.try_wait().ok().flatten();
                            let detail =
                                crate::crash_message(self.agent_name, status, &self.stderr_tail);
                            let _ = send(
                                &self.event_tx,
                                AgentEvent::Done {
                                    status: DoneStatus::Errored,
                                    result: None,
                                    error: Some(format!("{rpc_err}; {detail}")),
                                    session_id: Some(self.session_id.clone()),
                                },
                            )
                            .await;
                        }
                        Err(_) if !self.interrupted => {
                            self.done_current = true;
                            let status = self.child.try_wait().ok().flatten();
                            let detail =
                                crate::crash_message(self.agent_name, status, &self.stderr_tail);
                            let _ = send(
                                &self.event_tx,
                                AgentEvent::Done {
                                    status: DoneStatus::Errored,
                                    result: None,
                                    error: Some(format!(
                                        "{} exited mid-turn; {detail}",
                                        self.agent_name
                                    )),
                                    session_id: Some(self.session_id.clone()),
                                },
                            )
                            .await;
                        }
                        _ => {}
                    }
                }
                false
            }
        }
    }

    /// The busy-session cancel flushed: the queued steer becomes a fresh
    /// prompt on a now-idle agent.
    async fn on_cancel_flush(&mut self) -> bool {
        self.cancel_flush_deadline = None;
        if self.turn.is_none()
            && let Some(text) = self.queued_steers.pop_front()
        {
            if !self.begin_turn(text).await {
                return false;
            }
        } else if self.turn.is_none() && !self.steering_open {
            // Mailbox closed while the flush waited: nothing left.
            return false;
        }
        true
    }

    /// The blanket quiet settle: settled output, no open tool, no pending
    /// question, stream quiet past the window with the prompt still unsettled.
    fn on_quiet_settle(&mut self) {
        let window = self.quiet_window();
        tracing::warn!(
            target: "hearth_harness::acp",
            quiet_ms = window.as_millis() as u64,
            had_exec = self.had_exec,
            "turn quiet past the settle window with completed output; \
             treating the prompt response as dropped"
        );
        self.starve_deadline = Some(Instant::now());
    }

    /// The grace elapsed with the prompt still unsettled after turn-end
    /// evidence: close the dead turn out with a Done — its output already
    /// streamed as session/updates and its text was delivered via the CLI's
    /// own queue — then promote any queued steer to a fresh prompt, which
    /// settles normally on a now-idle agent (verified against the real
    /// adapter).
    async fn on_starve(&mut self) -> bool {
        self.starve_deadline = None;
        tracing::warn!(
            target: "hearth_harness::acp",
            "prompt response missing past turn-end evidence; settling \
             the dead turn (and promoting any queued steer)"
        );
        // Drop the dead future: a response that somehow arrives later
        // resolves a closed channel harmlessly.
        self.turn = None;
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
        self.done_current = true;
        if !send(
            &self.event_tx,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some(self.session_id.clone()),
            },
        )
        .await
        {
            return false;
        }
        if let Some(text) = self.queued_steers.pop_front() {
            if !self.begin_turn(text).await {
                return false;
            }
        } else if !self.steering_open {
            // Mirror the normal turn-settled exit: mailbox closed and nothing
            // left to run — the session is over.
            return false;
        }
        true
    }

    /// Cancelling [`RunControls::interrupt`](crate::RunControls::interrupt):
    /// `session/cancel` plus a SIGTERM → SIGKILL escalation the teardown
    /// disarms.
    async fn on_interrupt(&mut self) -> bool {
        self.interrupt_sent = true;
        self.interrupted = true;
        if self.turn.is_some() {
            self.client.notify(
                "session/cancel",
                Some(json!({ "sessionId": self.session_id })),
            );
            // Escalate if the agent doesn't wind down (stopReason
            // "cancelled") within the grace periods.
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
            // Idle between turns: nothing to cancel — the terminal
            // bookkeeping below still guarantees Done { Interrupted }.
            return false;
        }
        true
    }

    /// A prompt was sent and NOTHING came back on the wire at all — no queue
    /// bookkeeping, no updates, no requests. Healthy grok acknowledges within
    /// milliseconds; total silence past the bound is a wedged agent (stale
    /// shared leader, launch-time update check). Surface a visible error
    /// instead of indefinite Working.
    async fn on_prompt_stall(&mut self) -> bool {
        let _ = send(
            &self.event_tx,
            AgentEvent::Error {
                message: format!(
                    "{} did not respond to the prompt at all \
                     (no wire activity for {}s). {}",
                    self.agent_name,
                    self.prompt_stall.map(|d| d.as_secs()).unwrap_or(0),
                    self.stall_hint,
                ),
            },
        )
        .await;
        self.done_current = true;
        let _ = send(
            &self.event_tx,
            AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some(format!(
                    "{} is unresponsive — the run was closed.",
                    self.agent_name
                )),
                session_id: Some(self.session_id.clone()),
            },
        )
        .await;
        false
    }

    /// Drain the messages the reader already queued into the CURRENT segment
    /// (responses bypass the incoming queue, so a settled request can leave
    /// updates behind it). Returns false when the consumer hung up.
    pub(super) async fn drain_incoming(&mut self) -> bool {
        while let Ok(inc) = self.incoming.try_recv() {
            match inc {
                Incoming::Notification { method, params } => {
                    let events = session_update_events(
                        &method,
                        &params,
                        &self.session_id,
                        &mut self.subagents,
                    );
                    for ev in events {
                        if !send(&self.event_tx, ev).await {
                            return false;
                        }
                    }
                }
                Incoming::Request { id, method, params } => {
                    let events = handle_server_request_live(
                        &self.client,
                        id,
                        &method,
                        &params,
                        &self.request_input,
                        &self.open_questions,
                    );
                    for ev in events {
                        if !send(&self.event_tx, ev).await {
                            return false;
                        }
                    }
                }
                _ => {}
            }
        }
        true
    }
}
