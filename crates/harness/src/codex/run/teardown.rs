//! Terminal bookkeeping: the run never ends its stream without a Done unless
//! the consumer already hung up, and the child is reaped only after the
//! escalation timer is disarmed.

use hearth_proto::{AgentEvent, DoneStatus};

use crate::shutdown_child;

use super::RunState;

impl RunState {
    pub(super) async fn teardown(&mut self) {
        // Terminal bookkeeping: never end the stream without a Done unless the
        // consumer already hung up.
        if !self.event_tx.is_closed() {
            if self.interrupted && !self.done_after_interrupt {
                let _ = self
                    .event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: Some(self.thread_id.clone()),
                    }))
                    .await;
            } else if !self.interrupted && !self.done_current {
                // A child KILLED mid-turn (OS memory pressure, `killall codex`)
                // must not read as a silent success — codex.ts's signal-death
                // handling, reduced to the turn-in-flight case.
                let status = self.child.try_wait().ok().flatten();
                let _ = self
                    .event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(crate::crash_message(
                            "codex app-server",
                            status,
                            &self.stderr_tail,
                        )),
                        session_id: Some(self.thread_id.clone()),
                    }))
                    .await;
            }
        }

        // Escalation dies BEFORE the child is reaped: after `shutdown_child`
        // waits the pid, a still-armed SIGTERM/SIGKILL timer would fire at a
        // freed (reusable) pid — worse now that the signal targets the whole
        // process group. Same ordering as the ACP session loop.
        if let Some(handle) = self.escalation.take() {
            handle.abort();
        }
        shutdown_child(&mut self.child, self.kill_grace).await;
    }
}
