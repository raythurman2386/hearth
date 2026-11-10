//! Steering: the mid-turn `_session/steering` call (for agents advertising the
//! extension), the turn-boundary delivery path (for agents without it, or a
//! steer that lost the turn-end race), and the turn-start primitive all the
//! promotion paths share.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use hearth_proto::AgentEvent;

use crate::{HarnessError, SteerMessage};

use super::super::turn::{prompt_turn, steering_call_future};
use super::RunState;
use super::rotate;
use super::send;

/// Grace after a steering call answered `noRunningTurn` with our prompt still
/// outstanding before the dead turn is closed out. A short grace covers the
/// true turn-end race (its response lands within milliseconds).
const STARVE_GRACE: Duration = Duration::from_secs(2);
/// Visible-busy window for the self-continued-turn prevention below: no prompt
/// of ours outstanding but the stream moved this recently.
const BUSY_RECENT: Duration = Duration::from_secs(3);
/// How long a `session/cancel` of an unowned turn gets to die and drain before
/// the queued steer prompts into an idle agent.
const CANCEL_FLUSH: Duration = Duration::from_secs(2);

impl RunState {
    /// Promote `text` to a fresh `session/prompt` on the same session: emit the
    /// `Steered` boundary that opens the next assistant message, reset the
    /// per-turn signals, and arm the new turn's watchdogs. Returns false when
    /// the consumer hung up.
    pub(super) async fn begin_turn(&mut self, text: String) -> bool {
        let (prev, next) = rotate(&mut self.assistant_message_id);
        if !send(
            &self.event_tx,
            AgentEvent::Steered {
                assistant_message_id: Some(prev),
                next_assistant_message_id: Some(next),
            },
        )
        .await
        {
            return false;
        }
        self.done_current = false;
        self.turn_content_seen = false;
        self.open_tools.clear();
        self.had_exec = false;
        self.last_update_at = Instant::now();
        self.prompt_seq += 1;
        self.current_prompt_id = self
            .prompt_complete_extension
            .then(|| format!("hearth-p{}", self.prompt_seq));
        self.prompt_stall_deadline = self.prompt_stall.map(|d| tokio::time::Instant::now() + d);
        self.turn = Some(prompt_turn(
            self.client.clone(),
            self.session_id.clone(),
            text,
            self.current_prompt_id.clone(),
        ));
        true
    }

    /// Settle an in-flight `_session/steering` call BEFORE closing the turn:
    /// its response rides the same stdout as the prompt response, so by now it
    /// is (nearly always) already parsed — the select just hadn't polled it
    /// yet. Deciding it here keeps the ordering deterministic: an injection
    /// that landed in this turn emits its Steered boundary now, ahead of the
    /// drained tail and the Done (a Steered AFTER Done re-armed the consumer
    /// with no next turn — the stranded-Working bug); a rejected/unsettled call
    /// redelivers as the next turn. The timeout guards the flooded-incoming
    /// edge (reader blocked on a full channel never parses the response): past
    /// it the call is abandoned and the steer redelivered.
    pub(super) async fn settle_steering_call(&mut self) -> bool {
        let Some((text, mut fut)) = self.steering_call.take() else {
            return true;
        };
        let outcome = match tokio::time::timeout(Duration::from_millis(1000), &mut fut).await {
            Ok(Ok(resp)) => resp
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("injected")
                .to_owned(),
            Ok(Err(_)) | Err(_) => "promptRequired".to_owned(),
        };
        if self.interrupted {
            // Winding down; abandoned like any queued steer.
        } else if outcome != "promptRequired" {
            let (prev, next) = rotate(&mut self.assistant_message_id);
            if !send(
                &self.event_tx,
                AgentEvent::Steered {
                    assistant_message_id: Some(prev),
                    next_assistant_message_id: Some(next),
                },
            )
            .await
            {
                return false;
            }
        } else {
            self.queued_steers.push_back(text);
        }
        // Followers waiting on the settled call have no live turn to inject
        // into anymore: boundary delivery.
        while let Some(next_text) = self.steer_backlog.pop_front() {
            self.queued_steers.push_back(next_text);
        }
        true
    }

    /// The in-flight `_session/steering` call resolved.
    pub(super) async fn on_steering_resolved(&mut self, res: Result<Value, HarnessError>) -> bool {
        let (text, _) = self.steering_call.take().expect("guarded by if");
        let outcome = match &res {
            Ok(resp) => resp
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("injected")
                .to_owned(),
            Err(e) => {
                tracing::debug!(
                    target: "hearth_harness::acp",
                    "_session/steering failed (redelivering): {e}"
                );
                // Failed calls redeliver like a lost turn-end race.
                "promptRequired".to_owned()
            }
        };
        if self.interrupted {
            // The run is winding down; the steer is abandoned like any queued
            // steer at interrupt.
        } else if outcome != "promptRequired" {
            // Injected into a live turn → a Steered boundary. But if the turn
            // ended while the call was in flight, the injection was consumed
            // by THAT turn — its output already streamed and the turn's Done
            // already closed the segment. Emitting Steered after that Done
            // re-armed the consumer (parked session → Working) with no next
            // turn and no Done ever coming — the stranded-Working /
            // eternal-timer bug. Post-turn: nothing left to do.
            if self.turn.is_some() {
                // The injection proves the turn is LIVE: any settle deadline
                // armed off a cost frame that raced this response is invalid
                // evidence.
                self.starve_deadline = None;
                // Pre-injection updates can still sit in `incoming` (responses
                // bypass that queue): drain them into the CURRENT segment
                // first, or text the agent streamed before the injection
                // landed folds after the split — the transcript attributes it
                // to the reply-to-steer.
                if !self.drain_incoming().await {
                    return false;
                }
                let (prev, next) = rotate(&mut self.assistant_message_id);
                if !send(
                    &self.event_tx,
                    AgentEvent::Steered {
                        assistant_message_id: Some(prev),
                        next_assistant_message_id: Some(next),
                    },
                )
                .await
                {
                    return false;
                }
            }
        } else if self.turn.is_some() {
            // Raced the turn end: redeliver at the boundary the loop is about
            // to hit. `noRunningTurn` is stronger — the adapter says nothing is
            // running while our prompt is still outstanding: the starved-turn
            // signature. Arm the grace deadline; if the prompt's response does
            // not land first, the recovery arm below settles the dead turn and
            // promotes this steer.
            if res
                .as_ref()
                .ok()
                .and_then(|r| r.get("reason"))
                .and_then(Value::as_str)
                == Some("noRunningTurn")
            {
                tracing::warn!(
                    target: "hearth_harness::acp",
                    "steering answered noRunningTurn with a prompt \
                     outstanding; arming starved-turn recovery"
                );
                self.starve_deadline = Some(Instant::now() + STARVE_GRACE);
            }
            self.queued_steers.push_back(text);
        } else {
            // The turn ended while the call was in flight and its boundary
            // already passed — the steer becomes the next turn directly.
            if !self.begin_turn(text).await {
                return false;
            }
        }
        while let Some(next_text) = self.steer_backlog.pop_front() {
            if self.turn.is_some() && !self.interrupted {
                let fut = steering_call_future(&self.client, &self.session_id, &next_text);
                self.steering_call = Some((next_text, fut));
                break;
            }
            // No live turn to inject into: boundary delivery.
            self.queued_steers.push_back(next_text);
        }
        true
    }

    /// A steer arrived from the host.
    pub(super) async fn on_steer(&mut self, steer: Option<SteerMessage>) -> bool {
        match steer {
            Some(msg) => {
                // Same transform as the initial prompt: Claude's
                // Ultrathink prefix rides every steer too.
                let text = (self.prompt_transform)(self.request.reasoning, &msg.prompt);
                if self.turn.is_none() && self.cancel_flush_deadline.is_some() {
                    // A busy-session cancel is already in flight: this
                    // steer lines up behind it and dispatches at flush.
                    self.queued_steers.push_back(text);
                } else if self.turn.is_none()
                    && (!self.open_tools.is_empty() || self.last_update_at.elapsed() < BUSY_RECENT)
                {
                    // Mid self-continued turn (see BUSY_RECENT above):
                    // cancel it rather than prompt into the starve.
                    //
                    // Claude skips this branch ON PURPOSE and prompts
                    // straight in — its NATIVE semantics: the CLI queues
                    // the message and folds it into the running turn (no
                    // work lost, verified from live session data). The
                    // adapter drops that prompt's reply, and the
                    // cost-frame settle reconstructs it ~1s after the
                    // merged turn really ends. Only adapters with no
                    // verified turn-end frame pay the cancel.
                    tracing::info!(
                        target: "hearth_harness::acp",
                        "steer into a self-continuing session; cancelling \
                         the unowned turn before prompting"
                    );
                    self.client.notify(
                        "session/cancel",
                        Some(serde_json::json!({ "sessionId": self.session_id })),
                    );
                    self.queued_steers.push_back(text);
                    self.cancel_flush_deadline = Some(Instant::now() + CANCEL_FLUSH);
                } else if self.turn.is_none() {
                    // Idle between turns: a steer is simply the next turn.
                    if !self.begin_turn(text).await {
                        return false;
                    }
                } else if self.steer_ext {
                    // Mid-turn injection via the `_session/steering`
                    // extension: start the call, resolved by its own
                    // select branch. One call in flight at a time;
                    // followers wait in the backlog.
                    if self.steering_call.is_some() {
                        self.steer_backlog.push_back(text);
                    } else {
                        let fut = steering_call_future(&self.client, &self.session_id, &text);
                        self.steering_call = Some((text, fut));
                    }
                } else {
                    // No extension (Grok today): turn-boundary delivery.
                    self.queued_steers.push_back(text);
                }
                true
            }
            None => {
                self.steering_open = false;
                if self.turn.is_none() && self.queued_steers.is_empty() {
                    return false;
                }
                true
            }
        }
    }
}
