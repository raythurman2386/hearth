//! Steering delivery: the native `turn/steer` into the live turn, and the
//! follow-up `turn/start` on the same thread when that turn already ended.

use serde_json::json;

use hearth_proto::AgentEvent;

use crate::SteerMessage;

use super::super::turn::{rotate, send, steer_as_new_turn};
use super::RunState;

impl RunState {
    /// A steer arrived from the host.
    pub(super) async fn on_steer(&mut self, steer: Option<SteerMessage>) -> bool {
        match steer {
            Some(msg) => {
                let text = msg.prompt;
                if let Some(expected) = self.router.active.clone() {
                    let steer_params = json!({
                        "threadId": self.thread_id,
                        "expectedTurnId": expected,
                        "input": [{ "type": "text", "text": text }],
                    });
                    match self.client.request("turn/steer", steer_params).await {
                        Ok(_) => {
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
                            if self.router.active.as_deref() == Some(expected.as_str())
                                && !self.router.is_completed(&expected)
                            {
                                self.queued_steers.push_back(text);
                            } else if !self.start_steer_turn(text).await {
                                return false;
                            }
                        }
                    }
                } else if !self.start_steer_turn(text).await {
                    return false;
                }
                true
            }
            None => {
                // Mailbox closed (the caller's graceful idle-reap): finish
                // once nothing is in flight — mirrors codex.ts's steer loop
                // `finish()` on a null take.
                self.steering_open = false;
                if self.router.active.is_none() && self.queued_steers.is_empty() {
                    return false;
                }
                true
            }
        }
    }

    /// Deliver a steer as a fresh `turn/start` on the same thread (the fallback
    /// leg of the steer race, and the between-turns delivery path). Returns
    /// false when the loop should end (turn/start failed or the consumer hung
    /// up).
    pub(super) async fn start_steer_turn(&mut self, text: String) -> bool {
        let params = self.turn_params(&text);
        steer_as_new_turn(
            &self.client,
            params,
            &mut self.router,
            &self.event_tx,
            &mut self.assistant_message_id,
            &mut self.done_current,
        )
        .await
    }
}
