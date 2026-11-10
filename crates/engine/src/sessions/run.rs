//! The run task: one chat's harness stream, folded into the chat's doc.
//!
//! `drive_run` owns the select loop (stream events, the engine's input bridge,
//! interrupts, the idle reaper, the coalesced commit tick, the turn-quiesce
//! watchdog). [`RunFold`] owns the mutable streaming state that loop folds into;
//! [`SubagentRouter`] owns the tagged subagent transcripts; [`QuiesceWindows`]
//! owns the watchdog's environment-scoped windows.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use hearth_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, STREAM_COMMIT_MS, SegmentWriter, SessionDoc,
    SessionMessageEntry, fold_event_into_parts, sanitize_tool_call,
};
use hearth_harness::{Harness, RunControls};
use hearth_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SessionStatus};

use super::*;
use crate::{new_id, now_ms};

/// A live subagent transcript sink: its own doc (opened by id — the room
/// `chat2/{docId}/ws` dials automatically, so viewers sync it like a chat),
/// one streaming assistant entry folded from the tagged events. The held
/// doc Arc pins the doc warm for the LRU while the subagent runs.
struct SubagentSink {
    doc_id: String,
    doc: Arc<SessionDoc>,
    entry_id: String,
    started_at: i64,
    entry_index: Option<usize>,
    written: Vec<MessagePart>,
    folded: Vec<MessagePart>,
    dirty: bool,
}

impl SubagentSink {
    fn flush(&mut self, device_id: &str) {
        if !self.dirty || self.folded.is_empty() {
            return;
        }
        let rendered = render_parts(&self.folded);
        let result = match self.entry_index {
            Some(ix) => {
                let mut w = SegmentWriter::resume(&self.doc, ix, std::mem::take(&mut self.written));
                let r = w.sync(&rendered);
                let (ix, written) = w.into_state();
                self.entry_index = Some(ix);
                self.written = written;
                r
            }
            None => {
                match SegmentWriter::begin(&self.doc, &self.entry_id, device_id, self.started_at) {
                    Ok(mut w) => {
                        let r = w.sync(&rendered);
                        let (ix, written) = w.into_state();
                        self.entry_index = Some(ix);
                        self.written = written;
                        r
                    }
                    Err(e) => Err(e),
                }
            }
        };
        if let Err(err) = result {
            // Fail soft: a broken subagent doc degrades to chip-only, never
            // errors the chat.
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent sink flush failed");
        }
        self.dirty = false;
    }

    /// A parent→subagent steer ([`AgentEvent::UserMessage`], tagged): close
    /// the open assistant segment (it reads Complete above the steer), write
    /// the steer as its own USER entry, and reset so the next tagged delta
    /// opens a fresh assistant entry below it — the subagent transcript then
    /// reads like any steered chat.
    fn push_user(&mut self, device_id: &str, text: &str) {
        let rendered = render_parts(&self.folded);
        let closed = match self.entry_index.take() {
            Some(ix) => SegmentWriter::resume(&self.doc, ix, std::mem::take(&mut self.written))
                .finish(&rendered, MessageStatus::Complete),
            None if !self.folded.is_empty() => {
                match SegmentWriter::begin(&self.doc, &self.entry_id, device_id, self.started_at) {
                    Ok(w) => w.finish(&rendered, MessageStatus::Complete),
                    Err(e) => Err(e),
                }
            }
            None => Ok(()),
        };
        if let Err(err) = closed {
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent segment close failed");
        }
        let entry = SessionMessageEntry {
            id: new_id(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_owned(),
            }],
            created_at: now_ms(),
            device_id: device_id.to_owned(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        };
        if let Err(err) = self.doc.push_message(&entry) {
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent steer write failed");
        }
        self.entry_id = new_id();
        self.started_at = now_ms();
        self.written = Vec::new();
        self.folded = Vec::new();
        self.dirty = false;
    }

    /// Final flush + entry finalize; returns the transcript JSON for the
    /// frozen blob (entries as the client renders them).
    fn finish(mut self, device_id: &str, status: MessageStatus) -> Option<String> {
        let rendered = render_parts(&self.folded);
        let finished = match self.entry_index {
            Some(ix) => SegmentWriter::resume(&self.doc, ix, std::mem::take(&mut self.written))
                .finish(&rendered, status),
            None if !self.folded.is_empty() => {
                match SegmentWriter::begin(&self.doc, &self.entry_id, device_id, self.started_at) {
                    Ok(w) => w.finish(&rendered, status),
                    Err(e) => Err(e),
                }
            }
            None => Ok(()),
        };
        if let Err(err) = finished {
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent sink finish failed");
        }
        let entries = hearth_doc::join_continuation_entries(self.doc.read_entries().ok()?);
        serde_json::to_string(&entries).ok()
    }
}

/// The parent-chip refresh a tagged event implies, for the in-place (parked)
/// path — LIFECYCLE ONLY, mirroring the fold (live tails were rejected:
/// per-delta chip rewrites grew the parent doc for the whole run).
fn subagent_chip_update(event: &AgentEvent) -> Option<&'static str> {
    match event {
        AgentEvent::Done { status, .. } => Some(match status {
            DoneStatus::Errored => "failed",
            _ => "done",
        }),
        _ => Some("running"),
    }
}

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Full inputs live only in the local run journal.
fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
    parts
        .iter()
        .map(|part| match part {
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
                output,
                diff,
                output_ref,
                output_bytes,
                diff_ref,
                diff_stats,
                subagent_ref,
                subagent_status,
                subagent_tail,
            } => MessagePart::Tool {
                id: id.clone(),
                call: sanitize_tool_call(call),
                is_error: *is_error,
                resolved: *resolved,
                // Output summaries, diff stats, and sidecar refs are
                // deliberately kept: unlike raw tool inputs they are the
                // transcript's record of what happened, and the strip already
                // bounded them (docs/chat2-sync.md A1).
                output: output.clone(),
                diff: diff.clone(),
                output_ref: output_ref.clone(),
                output_bytes: *output_bytes,
                diff_ref: diff_ref.clone(),
                diff_stats: diff_stats.clone(),
                subagent_ref: subagent_ref.clone(),
                subagent_status: *subagent_status,
                subagent_tail: subagent_tail.clone(),
            },
            other => other.clone(),
        })
        .collect()
}

/// The persisted assistant text of a folded segment (workspace preview source).
fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync_segment<'a>(
    doc: &'a SessionDoc,
    writer: &mut Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
) -> Result<(), DocError> {
    if folded.is_empty() {
        return Ok(());
    }
    let rendered = render_parts(folded);
    if writer.is_none() {
        *writer = Some(SegmentWriter::begin(doc, entry_id, device_id, started_at)?);
    }
    if let Some(w) = writer.as_mut() {
        w.sync(&rendered)?;
    }
    Ok(())
}

fn finish_segment<'a>(
    doc: &'a SessionDoc,
    writer: Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
    status: MessageStatus,
) -> Result<(), DocError> {
    let rendered = render_parts(folded);
    match writer {
        Some(w) => w.finish(&rendered, status),
        None if !folded.is_empty() => {
            SegmentWriter::begin(doc, entry_id, device_id, started_at)?.finish(&rendered, status)
        }
        None => Ok(()),
    }
}

/// Resume bookkeeping for one run task: which user entry the run answers (so
/// the startup-crash retry re-dispatches idempotently against the same doc
/// entry), whether `dispatch` injected the resume id itself (only
/// engine-injected resumes retry — a caller-specified resume fails loudly),
/// and whether this run already IS the retry (one attempt only).
pub(super) struct RunResumeState {
    pub(super) user_message_id: String,
    pub(super) resume_injected: bool,
    pub(super) startup_retry: bool,
}

/// How one folded event leaves the run loop.
enum FoldStep {
    /// Handled — keep looping.
    Continue,
    /// A terminal Done: the run ends with this status (the epilogue runs).
    End(SessionStatus),
    /// The run task hands off (the startup-crash retry owns the chat now).
    Abandon,
}

/// Read-only per-run values the fold needs; everything mutable lives on
/// [`RunFold`] or the loop.
struct RunEnv<'a> {
    chat_id: &'a str,
    run_id: &'a str,
    harness_id: HarnessId,
    user_prompt: &'a str,
    run_cwd: &'a str,
    steerable: bool,
}

/// Per-run streaming state: the fold, its segment writer, and the quiesce clock.
struct RunFold<'a> {
    doc: &'a SessionDoc,
    device_id: &'a str,
    /// Every tool id this run has folded, across segment resets. Adapters
    /// re-emit shape-bearing `tool_call_update`s (title/rawInput refreshes,
    /// long-running completions) as full ToolCall events; once the fold has
    /// reset at a steer/park boundary those ids are gone from `folded`, and
    /// folding the echo would mint an orphan chip mid-text in the NEXT
    /// segment — the mid-word transcript splits.
    seen_tools: HashSet<String>,
    folded: Vec<MessagePart>,
    writer: Option<SegmentWriter<'a>>,
    entry_id: String,
    segment_started: i64,
    dirty: bool,
    flush_at: tokio::time::Instant,
    /// PERSISTENT SESSION (hearth runsBySession): a completed turn on a
    /// steerable harness parks here instead of ending the run — the child and
    /// its steering mailbox stay warm, and the next user message (dispatch
    /// routes into a live run) starts the next turn with zero respawn/resume
    /// latency. `Some(when)` = idle since then; the 30-min reaper below ends
    /// a session nobody comes back to (hearth SESSION_IDLE_MS).
    idle_since: Option<tokio::time::Instant>,
    /// True while the parked turn's next output is a self-continued wake (see
    /// [`QuiesceWindows::self_after`]).
    self_continued_turn: bool,
    saw_session_started: bool,
    last_stream_activity: tokio::time::Instant,
    quiesce: QuiesceWindows,
}

impl<'a> RunFold<'a> {
    fn new(doc: &'a SessionDoc, device_id: &'a str) -> Self {
        Self {
            doc,
            device_id,
            seen_tools: HashSet::new(),
            folded: Vec::new(),
            writer: None,
            entry_id: new_id(),
            segment_started: now_ms(),
            dirty: false,
            flush_at: tokio::time::Instant::now(),
            idle_since: None,
            self_continued_turn: false,
            saw_session_started: false,
            last_stream_activity: tokio::time::Instant::now(),
            quiesce: QuiesceWindows::from_env(),
        }
    }

    /// Armed only when the fold says nothing is in flight: an unresolved tool
    /// part is a command still running, an unresolved input part is a question
    /// awaiting the user. The live-plan chip is exempt: it is a singleton that
    /// never resolves.
    fn has_in_flight(&self) -> bool {
        self.folded.iter().any(|p| match p {
            MessagePart::Tool {
                id,
                resolved: false,
                ..
            } => id != hearth_proto::LIVE_PLAN_TOOL_ID,
            MessagePart::Input {
                resolved: false, ..
            } => true,
            _ => false,
        })
    }

    /// Coalesced STREAM_COMMIT_MS tick: one doc commit per window.
    fn sync(&mut self, chat_id: &str) {
        if !self.dirty {
            return;
        }
        if let Err(err) = sync_segment(
            self.doc,
            &mut self.writer,
            &self.entry_id,
            self.device_id,
            self.segment_started,
            &self.folded,
        ) {
            tracing::warn!(chat = %chat_id, error = %err, "segment sync failed");
        }
        self.dirty = false;
    }

    /// Park the turn: finalize the open segment (when anything streamed) and
    /// reset per-turn state, keeping the child and mailbox warm.
    fn park(&mut self, inner: &Arc<Inner>, chat_id: &str) {
        if !self.folded.is_empty() || self.writer.is_some() {
            if let Err(err) = finish_segment(
                self.doc,
                self.writer.take(),
                &self.entry_id,
                self.device_id,
                self.segment_started,
                &self.folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment finish failed");
            }
            inner.note_message(chat_id, &folded_text(&self.folded));
        }
        self.folded.clear();
        self.dirty = false;
        self.entry_id = new_id();
        self.segment_started = now_ms();
        self.idle_since = Some(tokio::time::Instant::now());
        self.self_continued_turn = false;
        inner.set_status(chat_id, SessionStatus::Idle, false);
    }

    /// Fold one non-subagent event into the doc: freshness, the parked gate,
    /// stale echoes, the startup-crash retry, the steer boundary, status
    /// stamping, the fold itself, and the Done finalization/park.
    #[allow(clippy::too_many_arguments)]
    fn fold_event(
        &mut self,
        inner: &Arc<Inner>,
        env: &RunEnv<'_>,
        resume_state: &RunResumeState,
        retry_request: &mut Option<RunRequest>,
        event: &AgentEvent,
        interrupted: bool,
    ) -> FoldStep {
        // Any stream activity proves the run is alive — keep the session's
        // freshness inside the UI's 45s staleness window (throttled), and
        // push the quiesce watchdog's window out.
        inner.touch_session(env.chat_id);
        self.last_stream_activity = tokio::time::Instant::now();
        // The engine's input bridge is the sole authority on input requests:
        // it mints the id and parks the resolver BEFORE emitting the event,
        // so a legitimate id is always pending here. A harness emitting its
        // own copy (a different id no resolver knows) would fold an
        // unanswerable twin chip into the doc — and answering the twin would
        // never resume the run. Dropped BEFORE the parked gate below: letting
        // it un-park the session and then dropping it would disarm the idle
        // reaper with no turn running (a leaked child no reap ever ends).
        if let AgentEvent::InputRequested { request_id, .. } = event {
            let pending = lock(&inner.runs)
                .get(env.chat_id)
                .map(|h| h.pending_inputs.clone());
            let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
            if !known {
                tracing::warn!(
                    chat = %env.chat_id,
                    request = %request_id,
                    "dropping harness-emitted InputRequested (unknown id; \
                     the engine input bridge owns this lifecycle)"
                );
                return FoldStep::Continue;
            }
        }
        // PARKED: a steer boundary, a terminal Done, or SELF-CONTINUED OUTPUT
        // re-opens the session; everything else stays gated. The ACP child
        // keeps forwarding `session/update` frames after a turn completes,
        // and they split two ways:
        //
        // - Post-turn NOISE — late tool_call_updates for commands folded in a
        //   prior segment, command refreshes, reasoning heartbeats. Treating
        //   those as "the next turn" re-armed Working with no Done ever
        //   coming (the eternally-running session bug) and folded orphan
        //   parts into a phantom segment. Still dropped.
        // - SELF-CONTINUED WORK — Claude Code re-invokes itself when a
        //   background task finishes (turns no prompt started) and streams
        //   real output for them. Dropping those LOST transcript content
        //   (2026-08-12: "Build finished successfully…" streamed by the
        //   agent, absent from the doc). Fresh text or a genuinely new tool
        //   call resumes the session: new segment, Working, and the turn
        //   settles again via Done — or via the quiesce watchdog, which is
        //   what makes this resume safe where the naive version was not.
        //
        // The RESUME_GATE separates the two by arrival time: a finished
        // turn's tail flush lands within milliseconds of its Done, while a
        // self-continued turn starts a whole new agent round trip (seconds
        // at minimum, minutes in the incident). Inside the gate everything
        // non-boundary stays inert, exactly as before.
        const RESUME_GATE: std::time::Duration = std::time::Duration::from_secs(1);
        if self.idle_since.is_some() {
            let self_continued = self
                .idle_since
                .is_some_and(|parked_at| parked_at.elapsed() >= RESUME_GATE)
                && (matches!(
                    event,
                    AgentEvent::TextDelta { text } if !text.is_empty()
                ) || matches!(
                    event,
                    AgentEvent::ToolCall { id, .. }
                        if id == hearth_proto::LIVE_PLAN_TOOL_ID || !self.seen_tools.contains(id)
                ));
            if self_continued {
                let parked_for = self.idle_since.map(|at| at.elapsed()).unwrap_or_default();
                // Brief parks are a false settle (same turn still running);
                // only a long-gap resume is a genuine background wake that
                // should use the short quiesce window.
                self.self_continued_turn = parked_for >= self.quiesce.self_short_after;
                tracing::info!(
                    chat = %env.chat_id,
                    parked_ms = parked_for.as_millis() as u64,
                    short_window = self.self_continued_turn,
                    "parked session resumed by self-continued agent output"
                );
                self.idle_since = None;
                // The park cleared the fold; rotate to a fresh entry and
                // fall through — this event is the new segment's first part.
                self.entry_id = new_id();
                self.segment_started = now_ms();
                inner.set_status(env.chat_id, SessionStatus::Working, true);
            } else {
                match event {
                    AgentEvent::Steered { .. } => {
                        self.idle_since = None;
                        inner.set_status(env.chat_id, SessionStatus::Working, true);
                    }
                    AgentEvent::Done { .. } => {
                        self.idle_since = None;
                    }
                    // A question with NO turn behind it (post-turn permission
                    // noise): answer it empty right here. Un-parking into
                    // AwaitingInput would disarm the idle reaper with no Done
                    // ever coming — stranded status, leaked warm child.
                    AgentEvent::InputRequested { request_id, .. } => {
                        let resolver = lock(&inner.runs)
                            .get(env.chat_id)
                            .and_then(|h| lock(&h.pending_inputs).remove(request_id));
                        if let Some(tx) = resolver {
                            let _ = tx.send(Vec::new());
                        }
                        tracing::debug!(chat = %env.chat_id, "parked session: post-turn input request auto-declined");
                        return FoldStep::Continue;
                    }
                    // A stale answer settling after its turn already closed:
                    // nothing is running — stay parked.
                    AgentEvent::InputResolved { .. } => return FoldStep::Continue,
                    _ => return FoldStep::Continue,
                }
            }
        }
        // Empty reasoning deltas are PURE heartbeats: redacted thinking and
        // tool-input-generation windows stream them with no text. They fold
        // to nothing, so journaling/publishing them is only noise (hundreds
        // per long turn observed) — the touch above already did their job.
        if matches!(event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
            return FoldStep::Continue;
        }

        // Stale tool echoes: a ToolCall/ToolResult naming an id folded in a
        // PRIOR segment (the fold reset at a steer or park since) belongs to
        // a chip that already rendered in its own entry. Folding the echo
        // would splice a phantom chip into the middle of the current
        // segment's streaming text (the mid-word transcript splits). Dropped;
        // same-segment refreshes still land in place.
        let in_segment = |folded: &[MessagePart], id: &str| {
            folded
                .iter()
                .any(|p| matches!(p, MessagePart::Tool { id: pid, .. } if pid == id))
        };
        match event {
            // The live plan chip is a deliberate singleton (every update
            // reuses `LIVE_PLAN_TOOL_ID` so the fold refreshes in place):
            // treating its reappearance after a park/steer reset as a stale
            // echo dropped the todo list for the rest of the run — from the
            // first boundary on, plans never rendered again.
            AgentEvent::ToolCall { id, .. } if id == hearth_proto::LIVE_PLAN_TOOL_ID => {}
            AgentEvent::ToolResult { id, .. } if id == hearth_proto::LIVE_PLAN_TOOL_ID => {}
            AgentEvent::ToolCall { id, .. } => {
                if !in_segment(&self.folded, id) && self.seen_tools.contains(id) {
                    return FoldStep::Continue;
                }
                self.seen_tools.insert(id.clone());
            }
            AgentEvent::ToolResult { id, .. }
                if !in_segment(&self.folded, id) && self.seen_tools.contains(id) =>
            {
                return FoldStep::Continue;
            }
            _ => {}
        }

        // Startup-crash retry: a run that dies before ever starting (errored
        // Done, no SessionStarted, nothing streamed) means the AGENT CHILD
        // failed to come up — not that the injected resume id was bad. Since
        // the ACP conversion (2026-08-08) a stale id is handled inside the
        // harness (`session/load` falls back to `session/new`), so the old
        // guess here — tombstone the id, retry fresh — fired only on child
        // startup failures and permanently severed GOOD conversations (user
        // incident 2026-08-13). The id stays; retry ONCE against the same
        // user entry, resume and all, in case the crash was transient. A
        // helper that is down hard fails the retry too and surfaces its
        // crash text (the harness now appends exit status + stderr).
        if resume_state.resume_injected
            && !resume_state.startup_retry
            && !self.saw_session_started
            && self.folded.is_empty()
            && !interrupted
            && matches!(
                event,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    ..
                }
            )
            && let Some(retry) = retry_request.take()
        {
            tracing::warn!(
                chat = %env.chat_id,
                "run died before session start; retrying once (resume kept)"
            );
            inner.remove_run(env.chat_id, env.run_id);
            let engine = SessionsEngine {
                inner: inner.clone(),
            };
            let chat = env.chat_id.to_string();
            let harness_id = env.harness_id;
            let message_id = resume_state.user_message_id.clone();
            tokio::spawn(async move {
                // The user entry write inside dispatch is idempotent by
                // message id; `startup_retry` makes this attempt final.
                if let Err(err) = engine
                    .dispatch_with(&chat, harness_id, retry, Some(message_id), true)
                    .await
                {
                    tracing::error!(chat = %chat, error = %err, "startup-crash retry dispatch failed");
                    // No run is coming: leaving the row Working would spin
                    // the session forever with nothing behind it.
                    engine
                        .inner
                        .set_status(&chat, SessionStatus::Errored, false);
                }
            });
            return FoldStep::Abandon;
        }

        // A steer boundary splits the assistant entry exactly where the fold resets.
        if let AgentEvent::Steered {
            next_assistant_message_id,
            ..
        } = event
        {
            inner.publish(env.chat_id, event);
            // A steer boundary means a real prompt owns the turn again — its
            // Done will come; the short self-continued window stands down.
            self.self_continued_turn = false;
            if let Err(err) = finish_segment(
                self.doc,
                self.writer.take(),
                &self.entry_id,
                self.device_id,
                self.segment_started,
                &self.folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %env.chat_id, error = %err, "segment finish failed");
            }
            inner.note_message(env.chat_id, &folded_text(&self.folded));
            self.folded.clear();
            self.dirty = false;
            self.entry_id = next_assistant_message_id.clone().unwrap_or_else(new_id);
            self.segment_started = now_ms();
            // The elapsed timer is per user message, not per child process: a
            // steer boundary restarts it (matches the parked-resume path and
            // the composer's optimistic overlay, which already reads 0:00).
            inner.set_status(env.chat_id, SessionStatus::Working, true);
            // The boundary confirms delivery of the oldest accepted steer —
            // retire its at-least-once ledger entry.
            if let Some(h) = lock(&inner.runs)
                .get(env.chat_id)
                .filter(|h| h.run_id == env.run_id)
            {
                lock(&h.routed_steers).pop_front();
            }
            return FoldStep::Continue;
        }

        match event {
            AgentEvent::SessionStarted {
                session_id, cwd, ..
            } => {
                self.saw_session_started = true;
                // The event's own cwd (where the harness actually created the
                // session) scopes the stored id, not the request's.
                inner.remember_harness_session(env.chat_id, session_id, cwd);
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                ..
            } => {
                inner.remember_harness_session(env.chat_id, session_id, env.run_cwd);
            }
            AgentEvent::InputRequested { .. } => {
                // Known-id guaranteed: the unknown-id twin was dropped above,
                // before the parked gate.
                inner.set_status(env.chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::InputResolved { .. } => {
                inner.set_status(env.chat_id, SessionStatus::Working, false);
            }
            _ => {}
        }

        inner.publish(env.chat_id, event);

        // Defensive rule from hearth: a mid-run SessionStarted re-emission (Claude SDK
        // background re-invocations) must not wipe the segment being written.
        let skip_fold =
            matches!(event, AgentEvent::SessionStarted { .. }) && !self.folded.is_empty();
        if !skip_fold {
            fold_event_into_parts(&mut self.folded, event);
            // R2 sidecar PARKED (2026-08-10, product call): the fold's
            // summary/stats ARE the doc's whole record — no refs stamped, no
            // uploads. Full outputs survive only in the host's local run
            // journal. To reintroduce: `hearth_doc::sidecar_payload(&event)`
            // → `apply_sidecar_refs` → `doc_host.upload_tool_sidecar`, all
            // still in place and tested.
        }

        if let AgentEvent::Done { status, .. } = event {
            return self.finish_done(inner, env, status, interrupted);
        }

        if !self.folded.is_empty() && !self.dirty {
            self.dirty = true;
            self.flush_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(STREAM_COMMIT_MS);
        }

        FoldStep::Continue
    }

    /// A terminal Done: drain the run's pending input resolvers, resolve
    /// the fold's input parts, finalize the segment, run the post-turn duties and
    /// either PARK (persistent session) or end the run.
    fn finish_done(
        &mut self,
        inner: &Arc<Inner>,
        env: &RunEnv<'_>,
        status: &DoneStatus,
        interrupted: bool,
    ) -> FoldStep {
        // answered (its turn is over): drain the resolvers NOW, or a late
        // `respond_input` finds one, emits InputResolved, and un-parks
        // the session into Working with no turn behind it — stranded
        // Working, timer forever, reaper disarmed. Empty answers unblock
        // the harness-side bridge like an interrupt does.
        let pending = lock(&inner.runs)
            .get(env.chat_id)
            .filter(|h| h.run_id == env.run_id)
            .map(|h| h.pending_inputs.clone());
        if let Some(pending) = pending {
            for (_, tx) in lock(&pending).drain() {
                let _ = tx.send(Vec::new());
            }
        }
        let message_status = match status {
            DoneStatus::Interrupted => MessageStatus::Aborted,
            DoneStatus::Completed | DoneStatus::Errored => MessageStatus::Complete,
        };
        // No dangling chips: a run that ends for ANY reason (completed,
        // errored, interrupted) terminally resolves its input parts — an
        // unresolved question must not outlive the run that asked it
        // (its resolver died with the run; an answer could never land).
        for part in self.folded.iter_mut() {
            if let MessagePart::Input { resolved, .. } = part {
                *resolved = true;
            }
        }
        // A Done landing on a PARKED session with nothing streamed (the
        // idle reaper's or an interrupt's own teardown) has no entry to
        // finalize — writing one would leave an empty aborted stub.
        let nothing_streamed = self.writer.is_none() && self.folded.is_empty();
        if !nothing_streamed {
            if let Err(err) = finish_segment(
                self.doc,
                self.writer.take(),
                &self.entry_id,
                self.device_id,
                self.segment_started,
                &self.folded,
                message_status,
            ) {
                tracing::warn!(chat = %env.chat_id, error = %err, "final segment finish failed");
            }
            inner.note_message(env.chat_id, &folded_text(&self.folded));
        }
        if *status == DoneStatus::Completed {
            // A cleanly completed turn resets the auto-resume revival
            // budget: only consecutive crash-revive-crash cycles spend it.
            inner.journal.clear_resume_attempts(env.chat_id);
        }
        // Exchange completed on an untitled chat → name it (fire-and-forget;
        // interrupted/errored turns never trigger naming).
        if *status == DoneStatus::Completed
            && let Some(titles) = inner.titles.get()
        {
            titles.maybe_generate(env.chat_id, env.harness_id, env.user_prompt, env.run_cwd);
        }
        // PERSISTENT SESSION: a cleanly completed turn on a steerable
        // harness PARKS instead of ending — child + mailbox stay warm for
        // the next routed dispatch; per-turn state resets for it.
        if *status == DoneStatus::Completed && env.steerable && !interrupted {
            self.folded.clear();
            self.dirty = false;
            self.entry_id = new_id();
            self.segment_started = now_ms();
            // Resume-retry is strictly a first-turn concern.
            self.saw_session_started = true;
            self.idle_since = Some(tokio::time::Instant::now());
            self.self_continued_turn = false;
            inner.set_status(env.chat_id, SessionStatus::Idle, false);
            return FoldStep::Continue;
        }
        FoldStep::End(match status {
            DoneStatus::Errored => SessionStatus::Errored,
            _ => SessionStatus::Idle,
        })
    }
}

/// Turn-quiesce watchdog knobs, resolved once per run from the environment.
///
/// TURN-QUIESCE WATCHDOG (2026-08-12 stuck-Working incident): a harness that
/// loses a turn's Done — the adapter never settles `session/prompt` even
/// though the agent finished — strands Working forever: the live heartbeat
/// keeps the row fresh, and there is no per-turn timeout by design. This is
/// NOT that stall timeout: it never ends the run or errors anything. When the
/// stream has been silent past the window AND the fold shows completed output
/// with nothing in flight, the turn parks exactly like a Done would. A false
/// trip (the agent was quietly waiting on something invisible) costs a status
/// dip: the parked-resume path re-arms Working the moment output flows again.
struct QuiesceWindows {
    /// `HEARTH_TURN_QUIESCE_MS`: `None` = the built-in default (unless the
    /// harness is deterministic); `Some(None)` = disabled.
    ///
    /// RETIRED for native drivers: a harness whose every turn shape ends with
    /// a deterministic wire Done (claude/codex/cursor native) needs no quiesce
    /// backstop — arming one only risks false parks on long silent work. The
    /// env knob still forces a window on for diagnostics. The predicate is
    /// re-read every loop turn rather than cached: a harness can be
    /// deterministic in its steady state yet NOT while the app server owes it
    /// a JSON-RPC reply (Codex reports false then), and that is exactly the
    /// window where the watchdog must stay armed.
    default: Option<Option<std::time::Duration>>,
    /// SELF-CONTINUED turns get a much SHORTER quiesce window. A turn the
    /// agent starts on its own (background-task wake) can never receive a
    /// harness Done: the adapter has no `session/prompt` outstanding to settle
    /// — verified in claude-agent-acp's autonomous-result lane, which consumes
    /// the SDK's turn-end without emitting anything; codex shows the same
    /// shape. The watchdog is that turn shape's ONLY settle path, so the
    /// default 120s window read as 2min of stuck-Working after every
    /// background notification (user report 2026-08-13).
    /// `HEARTH_SELF_TURN_QUIESCE_MS` overrides; 0 falls back to the normal
    /// window. An explicit `HEARTH_TURN_QUIESCE_MS=0` still disables the
    /// watchdog entirely.
    self_after: Option<std::time::Duration>,
    /// A resume sooner than this after park is the SAME turn continuing after
    /// a false settle (ACP quiet-settle, engine quiesce) — cargo / lint silence
    /// of a few seconds, then more output. Applying the short self-continued
    /// window there oscillated Idle↔Working every ~20s on the live hub
    /// (2026-08-24) and made follow-up sends cancel the still-running tool. A
    /// genuine background wake is a whole agent round-trip later. `0` = always
    /// short (tests that pin the 20s path).
    self_short_after: std::time::Duration,
}

impl QuiesceWindows {
    const DEFAULT: std::time::Duration = std::time::Duration::from_secs(120);
    const SELF_DEFAULT: std::time::Duration = std::time::Duration::from_secs(20);
    const SELF_SHORT_AFTER_DEFAULT: std::time::Duration = std::time::Duration::from_secs(30);

    fn from_env() -> Self {
        let default = match std::env::var("HEARTH_TURN_QUIESCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(0) => Some(None),
            Some(ms) => Some(Some(std::time::Duration::from_millis(ms))),
            None => None,
        };
        let self_after = match std::env::var("HEARTH_SELF_TURN_QUIESCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(0) => None,
            Some(ms) => Some(std::time::Duration::from_millis(ms)),
            None => Some(Self::SELF_DEFAULT),
        };
        let self_short_after = match std::env::var("HEARTH_SELF_CONTINUED_SHORT_AFTER_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(ms) => std::time::Duration::from_millis(ms),
            None => Self::SELF_SHORT_AFTER_DEFAULT,
        };
        Self {
            default,
            self_after,
            self_short_after,
        }
    }

    /// The watchdog's window for this harness: an explicit override always
    /// wins; otherwise only a non-deterministic harness gets one.
    fn after(&self, deterministic_turn_end: bool) -> Option<std::time::Duration> {
        self.default
            .unwrap_or_else(|| (!deterministic_turn_end).then_some(Self::DEFAULT))
    }

    /// The window measured from the last stream activity.
    fn window(
        &self,
        quiesce_after: Option<std::time::Duration>,
        self_continued: bool,
    ) -> std::time::Duration {
        let mut window = quiesce_after.unwrap_or_default();
        if self_continued && let Some(short) = self.self_after {
            window = window.min(short);
        }
        window
    }
}

/// The run's subagent transcripts, keyed by parent tool-use id.
#[derive(Default)]
struct SubagentRouter {
    /// Spawn chips that have SETTLED (tagged Done seen). Content events for a
    /// settled chip with no live sink are dropped — a straggler frame after
    /// the freeze must not mint a new doc entry or wedge the transcript back
    /// into Streaming. Only a steer (UserMessage) legitimately REOPENS a
    /// settled subagent: it announces more work is coming.
    settled: HashSet<String>,
    /// Live subagent sinks, parent tool-use id → transcript doc state.
    sinks: HashMap<String, SubagentSink>,
}

impl SubagentRouter {
    fn any_dirty(&self) -> bool {
        self.sinks.values().any(|s| s.dirty)
    }

    fn flush(&mut self, device_id: &str) {
        for sink in self.sinks.values_mut() {
            sink.flush(device_id);
        }
    }

    /// Route one tagged subagent event into its own doc; `true` means the event
    /// was a subagent event and the run loop must not fold it further.
    fn route(
        &mut self,
        inner: &Arc<Inner>,
        chat_id: &str,
        device_id: &str,
        doc: &SessionDoc,
        fold: &mut RunFold<'_>,
        event: &AgentEvent,
    ) -> bool {
        // ── subagent routing ───────────────────────────────────────────
        // Tagged events NEVER fold into the parent transcript: they stream
        // into the subagent's own doc, and the parent keeps only the spawn
        // chip (ref + status + one-line tail) — refreshed through the live
        // fold while its segment still streams, else stamped in place (the
        // eager-done norm: the chip's entry finished before the background
        // subagent spoke). Handled BEFORE the parked gate so a parked
        // session's background traffic still reaches the subagent doc
        // without un-parking the chat.
        if let AgentEvent::Subagent {
            parent_tool_use_id,
            event: sub_event,
        } = event
        {
            inner.publish(chat_id, event);
            let is_steer = matches!(sub_event.as_ref(), AgentEvent::UserMessage { .. });
            if is_steer {
                self.settled.remove(parent_tool_use_id);
            } else if self.settled.contains(parent_tool_use_id)
                && !self.sinks.contains_key(parent_tool_use_id)
            {
                // Straggler after the freeze: chip-only silence (the old
                // pre-viz behavior), never a reopened doc.
                return true;
            }
            let sub_id = subagent_doc_id(chat_id, parent_tool_use_id);
            let chip_streaming = fold
                .folded
                .iter()
                .any(|p| matches!(p, MessagePart::Tool { id, .. } if id == parent_tool_use_id));
            let sink_known = self.sinks.contains_key(parent_tool_use_id);
            // A Done with NO sink (a subagent that never streamed — codex
            // turn ends can beat registration) is chip-only: minting a doc
            // just to freeze it empty helps no one, and stamping the ref
            // would link the chip to that never-created doc (an empty tab
            // on click).
            let done_only = !sink_known && matches!(sub_event.as_ref(), AgentEvent::Done { .. });
            if chip_streaming {
                if !sink_known && !done_only {
                    for p in fold.folded.iter_mut() {
                        if let MessagePart::Tool {
                            id,
                            call,
                            subagent_ref,
                            ..
                        } = p
                            && id == parent_tool_use_id
                            // Genus gate: a subagent ref may only ever bind
                            // to a SPAWN call — mis-keyed tagged traffic
                            // must not turn an ordinary chip into a spawn
                            // link (empty tab on click).
                            && call.is_subagent_spawn()
                        {
                            *subagent_ref = Some(sub_id.clone());
                        }
                    }
                }
                hearth_doc::fold_event_into_parts(&mut fold.folded, event);
                if !fold.dirty {
                    fold.dirty = true;
                    fold.flush_at = tokio::time::Instant::now()
                        + std::time::Duration::from_millis(STREAM_COMMIT_MS);
                }
            }
            // Open the sink lazily; an open failure degrades to chip-only.
            if !sink_known && !done_only {
                let opened = inner.doc_host().and_then(|host| match host.open(&sub_id) {
                    Ok(handle) => Some(handle.doc_arc()),
                    Err(err) => {
                        tracing::warn!(doc = %sub_id, error = %err, "subagent doc open failed (chip-only)");
                        None
                    }
                });
                if let Some(sub_doc) = opened {
                    self.sinks.insert(
                        parent_tool_use_id.clone(),
                        SubagentSink {
                            doc_id: sub_id.clone(),
                            doc: sub_doc,
                            entry_id: new_id(),
                            started_at: now_ms(),
                            entry_index: None,
                            written: Vec::new(),
                            folded: Vec::new(),
                            dirty: false,
                        },
                    );
                    if !chip_streaming {
                        let _ = doc.update_subagent_chip(
                            parent_tool_use_id,
                            Some(&sub_id),
                            Some("running"),
                            None,
                        );
                    }
                }
            }
            let done = matches!(sub_event.as_ref(), AgentEvent::Done { .. });
            if done {
                self.settled.insert(parent_tool_use_id.clone());
            }
            if let Some(sink) = self.sinks.get_mut(parent_tool_use_id) {
                if let AgentEvent::UserMessage { text } = sub_event.as_ref() {
                    // A steer splits ENTRIES, not parts — handled at the
                    // sink level (the fold ignores UserMessage).
                    sink.push_user(device_id, text);
                    return true;
                }
                hearth_doc::fold_event_into_parts(&mut sink.folded, sub_event);
                sink.dirty = true;
                if !chip_streaming && done {
                    // In-place chip refresh on lifecycle transitions only —
                    // content never rewrites the parent doc.
                    let _ = doc.update_subagent_chip(
                        parent_tool_use_id,
                        None,
                        subagent_chip_update(sub_event),
                        None,
                    );
                }
                if done {
                    let status = match sub_event.as_ref() {
                        AgentEvent::Done {
                            status: DoneStatus::Errored,
                            ..
                        } => MessageStatus::Complete,
                        AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            ..
                        } => MessageStatus::Aborted,
                        _ => MessageStatus::Complete,
                    };
                    let sink = self.sinks.remove(parent_tool_use_id).expect("checked");
                    let doc_id = sink.doc_id.clone();
                    // FREEZE: the finished transcript uploads as a static R2
                    // blob (`blob/{chatId}/{subDocId}`) so viewers of
                    // finished self.sinks never wake the doc's room; dropping
                    // the sink unpins the doc for the LRU and the room
                    // idles. The live doc remains the fallback.
                    if let Some(json) = sink.finish(device_id, status)
                        && let Some(host) = inner.doc_host()
                    {
                        host.upload_tool_sidecar(
                            chat_id,
                            hearth_doc::SidecarPayload {
                                part_id: doc_id,
                                output: Some(json),
                                diff: None,
                            },
                        );
                    }
                }
            }
            return true;
        }
        false
    }

    /// Any subagent still streaming when the run ends freezes as-is: the
    /// parent process is gone, so nothing more can arrive on this stream.
    fn freeze_remaining(
        &mut self,
        inner: &Arc<Inner>,
        chat_id: &str,
        device_id: &str,
        doc: &SessionDoc,
    ) {
        for (parent_id, sink) in self.sinks.drain() {
            let doc_id = sink.doc_id.clone();
            let _ = doc.update_subagent_chip(&parent_id, None, Some("failed"), None);
            if let Some(json) = sink.finish(device_id, MessageStatus::Aborted)
                && let Some(host) = inner.doc_host()
            {
                host.upload_tool_sidecar(
                    chat_id,
                    hearth_doc::SidecarPayload {
                        part_id: doc_id,
                        output: Some(json),
                        diff: None,
                    },
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_run(
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
    harness: Arc<dyn Harness>,
    request: RunRequest,
    doc: Arc<SessionDoc>,
    controls: RunControls,
    mut engine_rx: mpsc::Receiver<AgentEvent>,
    mut cancel_rx: watch::Receiver<bool>,
    resume_state: RunResumeState,
) {
    let device_id = inner.device_id.clone();
    // Captured for post-run auto-titling (the request moves into the harness).
    let harness_id = harness.id();
    let user_prompt = request.prompt.clone();
    let run_cwd = request.cwd.clone();
    // Kept whole for the startup-crash retry (same user entry; dispatch
    // re-injects the stored resume id). Option so the retry branch (inside
    // the event loop) can take ownership.
    let mut retry_request = Some(RunRequest {
        resume: None,
        ..request.clone()
    });
    let mut stream = match harness.run(request, controls).await {
        Ok(stream) => stream,
        Err(err) => {
            let message = err.to_string();
            inner.publish(
                &chat_id,
                &AgentEvent::Error {
                    message: message.clone(),
                },
            );
            inner.publish(
                &chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                },
            );
            inner.remove_run(&chat_id, &run_id);
            inner.set_status(&chat_id, SessionStatus::Errored, false);
            return;
        }
    };

    let doc_ref: &SessionDoc = &doc;
    let mut fold = RunFold::new(doc_ref, &device_id);
    let mut subagents = SubagentRouter::default();
    // Set when the engine interrupts the run: the harness gets this long to end its own
    // stream (its token was cancelled); past it, a terminal Done is synthesized.
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupted = false;
    // Liveness heartbeat: this loop RUNNING is proof the harness stream is
    // open, so freshness must not depend on events arriving. Silent stretches
    // are normal and UNBOUNDED — a long tool call, redacted thinking, an
    // agent waiting on an external process, a question parked for an hour —
    // and each starved the UI's 45s staleness gate in turn (working strip /
    // AwaitingInput dot vanishing mid-run, both user-reported). No stall
    // timeout here by design (a first port was rejected — agents may
    // legitimately be quiet for >10min): a live child means Working, dying
    // paths each carry their own error, and engine death stops these ticks
    // so the gate still catches real crashes. touch_session throttles at 10s.
    let mut live_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    live_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    let steerable = harness.supports_steering();
    let env = RunEnv {
        chat_id: &chat_id,
        run_id: &run_id,
        harness_id,
        user_prompt: &user_prompt,
        run_cwd: &run_cwd,
        steerable,
    };

    let final_status = loop {
        let quiesce_after: Option<std::time::Duration> =
            fold.quiesce.after(harness.deterministic_turn_end());
        let event: AgentEvent = tokio::select! {
            biased;
            changed = cancel_rx.changed(), if !interrupted => {
                let _ = changed;
                interrupted = true;
                interrupt_deadline = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                );
                continue;
            }
            _ = tokio::time::sleep_until(
                interrupt_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if interrupt_deadline.is_some() => AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
            },
            _ = live_heartbeat.tick() => {
                inner.touch_session(&chat_id);
                continue;
            }
            // Idle reaper (hearth SESSION_IDLE_MS): a parked persistent session
            // nobody returned to in 30 minutes releases its child. The turn
            // was finalized at Done, so this end is clean — no aborted stamp.
            _ = tokio::time::sleep_until(
                fold.idle_since.map(|at| at + SESSION_IDLE).unwrap_or_else(tokio::time::Instant::now)
            ), if fold.idle_since.is_some() => {
                tracing::info!(chat = %chat_id, "reaping idle persistent session");
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                break SessionStatus::Idle;
            }
            Some(event) = engine_rx.recv() => event,
            next = stream.next() => match next {
                Some(Ok(event)) => event,
                // A stream error while PARKED is a post-turn child death —
                // the turn was already finalized, so the run ends clean
                // instead of stamping a completed session Errored.
                Some(Err(err)) if fold.idle_since.is_some() => {
                    tracing::warn!(chat = %chat_id, error = %err, "parked session child died; ending clean");
                    break SessionStatus::Idle;
                }
                Some(Err(err)) => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(err.to_string()),
                    session_id: None,
                },
                None if interrupted => AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                },
                // Stream end while PARKED idle: a per-turn adapter closing
                // after its final Done — a clean end, not a crash (the turn
                // was already finalized). Persistent adapters keep the
                // stream open and never hit this.
                None if fold.idle_since.is_some() => break SessionStatus::Idle,
                None => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("harness stream ended without Done".into()),
                    session_id: None,
                },
            },
            _ = tokio::time::sleep_until(fold.flush_at), if fold.dirty || subagents.any_dirty() => {
                // Coalesced STREAM_COMMIT_MS tick: one doc commit per window
                // (parent + any dirty subagent docs).
                fold.sync(&chat_id);
                subagents.flush(&device_id);
                continue;
            }
            // Turn-quiesce watchdog (see [`QuiesceWindows`]). Armed only when
            // the fold says nothing is in flight. An EMPTY fold still arms — a
            // Steered boundary that no output ever follows is one of the wedge
            // shapes — it just parks without writing a segment (an empty
            // finalize would leave a stub entry).
            _ = tokio::time::sleep_until(
                fold.last_stream_activity
                    + fold.quiesce.window(quiesce_after, fold.self_continued_turn)
            ), if quiesce_after.is_some()
                && fold.idle_since.is_none()
                && !interrupted
                && steerable
                && !fold.has_in_flight() =>
            {
                let window = fold.quiesce.window(quiesce_after, fold.self_continued_turn);
                tracing::warn!(
                    chat = %chat_id,
                    quiet_ms = window.as_millis() as u64,
                    self_continued = fold.self_continued_turn,
                    "turn quiesced: stream silent after completed output with no \
                     turn-end; parking (suspected missing harness Done)"
                );
                fold.park(&inner, &chat_id);
                continue;
            }
        };

        if subagents.route(&inner, &chat_id, &device_id, doc_ref, &mut fold, &event) {
            continue;
        }
        match fold.fold_event(
            &inner,
            &env,
            &resume_state,
            &mut retry_request,
            &event,
            interrupted,
        ) {
            FoldStep::Continue => continue,
            FoldStep::End(status) => break status,
            FoldStep::Abandon => return,
        }
    };

    subagents.freeze_remaining(&inner, &chat_id, &device_id, doc_ref);
    finish_run_tail(
        &inner,
        &chat_id,
        &run_id,
        harness_id,
        final_status,
        interrupted,
    );
}

/// The run-task epilogue: claim any accepted-but-unconfirmed steers BEFORE the
/// handle goes away, retire the run handle, publish the final status, and
/// re-dispatch the claimed steers as fresh turns.
fn finish_run_tail(
    inner: &Arc<Inner>,
    chat_id: &str,
    run_id: &str,
    harness_id: HarnessId,
    final_status: SessionStatus,
    interrupted: bool,
) {
    // Claim any accepted-but-unconfirmed steers BEFORE the handle goes away:
    // a routed send that raced this exit either finds its entry gone (we own
    // it — re-dispatched below) or reclaims it and starts a fresh run itself.
    let orphans: Vec<RoutedSteer> = lock(&inner.runs)
        .get(chat_id)
        .filter(|h| h.run_id == run_id)
        .map(|h| std::mem::take(&mut *lock(&h.routed_steers)).into())
        .unwrap_or_default();
    inner.remove_run(chat_id, run_id);
    inner.set_status(chat_id, final_status, false);
    if !interrupted && !orphans.is_empty() {
        // The dying run accepted these into its mailbox but never confirmed a
        // Steered boundary (idle-reaper race, a mid-turn error discarding
        // queued boundary steers, a parked child death). Their user entries
        // are already in the transcript — a message that shows as sent must
        // never silently not run. Re-dispatch each as a fresh turn
        // (write_user_message dedupes by id; resume is engine-injected).
        let engine = SessionsEngine {
            inner: inner.clone(),
        };
        let chat = chat_id.to_string();
        tokio::spawn(async move {
            for steer in orphans {
                let Some(mut request) = engine.last_request(&chat) else {
                    tracing::warn!(chat = %chat, "orphaned steer lost: no run config to re-dispatch");
                    break;
                };
                request.prompt = steer.prompt.clone();
                request.resume = None;
                request.attachments = Vec::new();
                tracing::info!(chat = %chat, "re-dispatching steer orphaned by a dying run");
                if let Err(err) = engine
                    .dispatch(&chat, harness_id, request, Some(steer.message_id.clone()))
                    .await
                {
                    tracing::warn!(chat = %chat, error = %err, "orphaned steer re-dispatch failed");
                    engine
                        .inner
                        .set_status(&chat, SessionStatus::Errored, false);
                    break;
                }
            }
        });
    }
}
