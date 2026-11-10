//! `DocHost::open` — the registry-gen/epoch decision block, doc construction,
//! handle registration and the edge-room join kick.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use tokio::sync::watch;

use hearth_doc::{SessionCommandEntry, SessionCommandStatus, SessionDoc};

use super::*;

impl DocHost {
    /// Open (or return) the chat's doc handle: load the local snapshot (or init fresh),
    /// start the change-driven task, and join the edge room when configured.
    pub fn open(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        // The registry names the sync room generation (docs/chat2-sync.md
        // M2): absent row / absent field = legacy s2. Read it BEFORE the
        // cached-handle check — a cached s2-mode handle for a chat another
        // device has since cut over to chat2 would otherwise serve its frozen
        // fat lineage forever (the host writes only to the chat2 room now;
        // this device's s2 room has gone permanently silent).
        let chat_row = self
            .workspace()
            .and_then(|w| w.chat(chat_id).ok().flatten());
        // A row that EXISTS without `roomGen` is a pre-cutover legacy chat
        // (gen 1). A MISSING row is a chat being born right now: its
        // CreateChat mint (which stamps roomGen 2) is racing this open —
        // the composer attaches the transcript watch before its own mutate
        // lands, and a nudge beats registry sync by design. Defaulting the
        // absent row to 1 minted brand-new s2 rooms post-cutover: the host
        // ran the whole session against a room no other device reads (they
        // follow the row's gen 2 to an empty chat2 room), the run's live doc
        // ref blocked every heal, and the transcript never synced anywhere
        // (2026-08-11).
        let registry_gen = match chat_row.as_ref() {
            Some(row) => row.room_gen.unwrap_or(1),
            None => 2,
        };
        {
            let mut handles = lock(&self.inner.handles);
            if let Some(handle) = handles.get(chat_id) {
                let stale = registry_gen >= 2 && handle.room_gen < 2;
                if (stale || handle.retired.load(Ordering::Relaxed)) && !self.pinned(handle) {
                    // A seed flipped this chat under a cached fat handle
                    // (review B1): drop it so this open converges onto the
                    // thin lineage + chat2 room. Retire only at the drop:
                    // marking a PINNED stale handle retired while it kept
                    // serving suppressed the only persistence a stuck-live
                    // doc had and aborted its quiet-seed (born-gen2 race,
                    // 2026-08-11) — the cutover watcher and the seed itself
                    // own converging pinned handles.
                    handle.retired.store(true, Ordering::Relaxed);
                    handles.remove(chat_id);
                } else {
                    handle.touch();
                    return Ok(handle.clone());
                }
            }
        }
        // B2/M5 guard: the LOCAL epoch is the second cutover signal. A crash
        // between the thin save and the registry flip (or a not-yet-synced
        // registry) must NOT route an epoch-2 doc back onto s2 — the s2
        // room's fat doc would merge into the unrelated thin lineage and
        // duplicate every message. Local epoch >= 2 forces the chat2 branch
        // and best-effort completes the flip.
        let stored = self.inner.store.load_snapshot_with_cursor(chat_id)?;
        let stored_epoch = stored.as_ref().map(|(_, _, e)| *e).unwrap_or(0);
        let room_gen = if stored_epoch >= crate::chat2_host::CHAT2_DOC_EPOCH {
            if registry_gen < 2
                && let Some(ws) = self.workspace()
            {
                let _ = ws.set_chat_room_gen(chat_id, 2);
                tracing::info!(chat = %chat_id,
                    "completed interrupted chat2 flip (local epoch 2, registry said s2)");
            }
            2
        } else {
            registry_gen
        };
        let mut snapshot_len = 0usize;
        let mut chat2_cursor = 0u64;
        let mut requeue_commands: Vec<SessionCommandEntry> = Vec::new();
        let doc = if room_gen >= 2 {
            match stored {
                Some((bytes, cursor, epoch)) if epoch >= crate::chat2_host::CHAT2_DOC_EPOCH => {
                    snapshot_len = bytes.len();
                    chat2_cursor = cursor;
                    let raw = loro::LoroDoc::new();
                    raw.import(&bytes)
                        .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                    SessionDoc::from_doc(raw)
                }
                Some((bytes, _cursor, epoch)) if self.inner.config.edge.is_none() => {
                    // Offline/edge-less: adopting would blank a readable
                    // transcript with no way to catch up (review B4). Keep
                    // the old doc read-only-ish; the adopt runs on the next
                    // online open.
                    tracing::info!(chat = %chat_id, old_epoch = epoch,
                        "chat2 adopt deferred (no edge configured)");
                    snapshot_len = bytes.len();
                    let raw = loro::LoroDoc::new();
                    raw.import(&bytes)
                        .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                    SessionDoc::from_doc(raw)
                }
                Some((bytes, _cursor, epoch)) => {
                    // M3 discard-and-adopt: this device's doc predates the
                    // chat2 lineage. Keep the old snapshot under a suffixed
                    // id for rollback, carry over OUR OWN unresolved
                    // commands, and start fresh — the chat2 catch-up
                    // (checkpoint + rows) repopulates the transcript. This
                    // is the self-repair path: no user action, ever.
                    tracing::info!(chat = %chat_id, old_epoch = epoch,
                        "chat2 adopt: discarding pre-chat2 local doc (rollback copy kept)");
                    let rollback_id = format!("{chat_id}.pre-chat2");
                    // A re-adopt after a mid-catch-up crash reruns this path
                    // with a near-empty doc under `chat_id` — the FIRST
                    // rollback copy is the real transcript; never overwrite
                    // it (review B5).
                    if matches!(self.inner.store.load_snapshot(&rollback_id), Ok(None)) {
                        let _ = self.inner.store.save_snapshot(&rollback_id, &bytes);
                    }
                    if let Ok(raw) = {
                        let old = loro::LoroDoc::new();
                        old.import(&bytes).map(|_| old)
                    } {
                        let old_doc = SessionDoc::from_doc(raw);
                        if let Ok(commands) = old_doc.read_commands() {
                            requeue_commands = commands
                                .into_iter()
                                .filter(|c| {
                                    c.status == SessionCommandStatus::Pending
                                        && c.issued_by == self.inner.config.device_id
                                })
                                .collect();
                        }
                    }
                    SessionDoc::init(chat_id)?
                }
                None => {
                    // Born on chat2 (or a cold reader's first open): stamp
                    // the epoch-2 lineage NOW. Plain snapshot saves preserve
                    // an existing row's epoch but default a NEW row to 0 —
                    // without this stamp, the next open reads "pre-chat2
                    // doc" and the M3 adopt DISCARDS everything written
                    // since (caught by the restart_resume suite: first-turn
                    // transcripts vanished on reopen).
                    let doc = SessionDoc::init(chat_id)?;
                    if let Ok(snapshot) = doc.export_snapshot() {
                        let _ = self.inner.store.save_snapshot_with_cursor(
                            chat_id,
                            &snapshot,
                            0,
                            crate::chat2_host::CHAT2_DOC_EPOCH,
                        );
                    }
                    doc
                }
            }
        } else {
            match stored {
                Some((bytes, _, _)) => {
                    snapshot_len = bytes.len();
                    let raw = loro::LoroDoc::new();
                    raw.import(&bytes)
                        .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                    SessionDoc::from_doc(raw)
                }
                None => SessionDoc::init(chat_id)?,
            }
        };
        let doc = Arc::new(doc);

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        // The mirror starts dirty and empty: many opens (command queueing,
        // drains, nudges) never watch the transcript, and the first
        // watch_messages attach materializes it on demand.
        let (messages_tx, _) = watch::channel(Vec::new());

        let handle = Arc::new(ChatDocHandle {
            chat_id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            doc: doc.clone(),
            messages_tx,
            mirror_dirty: AtomicBool::new(true),
            last_access: AtomicI64::new(now_ms()),
            snapshot_bytes: AtomicUsize::new(snapshot_len),
            room_gen,
            retired: AtomicBool::new(false),
            checkpointing: Arc::new(AtomicBool::new(false)),
            chat2: Mutex::new(None),
            chat2_pending_local: Mutex::new(Vec::new()),
            chat2_local_sub: Mutex::new(None),
            _sub: sub,
        });
        {
            let mut handles = lock(&self.inner.handles);
            if let Some(existing) = handles.get(chat_id) {
                return Ok(existing.clone()); // racing open — keep the first
            }
            handles.insert(chat_id.to_string(), handle.clone());
        }

        // Edge room join — offline-tolerant AND supervised. `ChatClient` only
        // self-reconnects AFTER a first successful join; a one-shot attempt
        // here (the pre-LRU design) left the doc silently local-only until
        // app restart whenever the dial hit a transient gap — a post-wake
        // network, `Auth::token()` momentarily `None` around a refresh, an
        // edge deploy. The LRU made that dice-roll constant (every reopen),
        // and a watched doc is pinned against eviction, so nothing ever
        // retried: the exact "transcript frozen until restart" report.
        // Retry on the workspace host's capped, jittered backoff; a system
        // wake redials immediately; eviction/purge ends the loop via `weak`.
        if let Some(edge) = &self.inner.config.edge {
            if room_gen >= 2 {
                // Subscription BEFORE the dial (review B3): every local
                // commit lands in the client when connected, else in the
                // pending buffer the join drains — nothing composed during
                // (or before) the dial is lost to the room.
                let weak_push = Arc::downgrade(&handle);
                let sub = doc
                    .doc()
                    .subscribe_local_update(Box::new(move |bytes: &Vec<u8>| {
                        if let Some(handle) = weak_push.upgrade() {
                            // The buffer push happens WHILE HOLDING the client
                            // lock (verify pass: releasing it between the None
                            // check and the push let the join's store+drain
                            // slip between, orphaning the update forever).
                            let client_guard = lock(&handle.chat2);
                            match &*client_guard {
                                Some(client) => client.enqueue_update(bytes.clone()),
                                None => lock(&handle.chat2_pending_local).push(bytes.clone()),
                            }
                        }
                        true
                    }));
                *lock(&handle.chat2_local_sub) = Some(sub);
                // Re-queue survives the adopt: our own pending commands
                // become fresh entries in the new lineage (the
                // processed_commands ledger still guards double execution).
                // Committed AFTER the local-update subscription above — a
                // commit before it never enters the pending buffer or the
                // client, so the requeued command would sit in the local doc
                // and never reach the room (the host would never see it).
                for command in &requeue_commands {
                    let _ = doc.queue_command(command);
                }
                // First contact with the room (cursor 0): everything
                // committed BEFORE the subscription above — SessionDoc::
                // init's container/meta ops, an adopt's fresh doc — is
                // invisible to the push path, yet every later commit
                // causally DEPENDS on it. Rows built on unpushed deps import
                // into peers' loro pending-buffers and never materialize:
                // born-chat2 cross-device runs sat invisible on every other
                // device (host never saw the command, viewers never saw the
                // transcript). Push the doc's full update log as the join's
                // first batch; once acked the cursor moves and this never
                // re-arms.
                if chat2_cursor == 0 {
                    match doc
                        .doc()
                        .export(loro::ExportMode::updates(&loro::VersionVector::default()))
                    {
                        Ok(bytes) if !bytes.is_empty() => {
                            lock(&handle.chat2_pending_local).push(bytes);
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(chat = %chat_id, error = %err,
                                "chat2 first-contact export failed; peers may stall on missing deps");
                        }
                    }
                }
                self.spawn_chat2_join(edge.clone(), &handle, chat2_cursor);
            } else {
                // Straggler gen-1 chat (the s2 client is gone — post-cutover,
                // no device reads or writes an s2 room). The local fat doc
                // serves reads as-is; if we host the chat, seed it onto chat2
                // in the background and the flip converges every device.
                let is_host = chat_row
                    .as_ref()
                    .is_some_and(|c| c.device_id == self.inner.config.device_id);
                if is_host && chat_row.is_some() {
                    // Quiescent-only (review B1): a seed under a live run or
                    // watched transcript would strand everything written
                    // after the rebuild instant in a retired fat lineage.
                    self.spawn_chat2_seed_when_quiet(edge.clone(), chat_id, &handle);
                }
            }
        }
        self.spawn_worker(chat_task(self.clone(), Arc::downgrade(&handle), changed_rx));
        self.evict_over_budget();
        Ok(handle)
    }
}
