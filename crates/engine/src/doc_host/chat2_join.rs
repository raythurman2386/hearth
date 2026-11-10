//! chat2 room join, quiet seed and checkpoint duties.
//!
//! Join resolves only after full catch-up (checkpoint + rows); the seed flips a
//! straggler gen-1 chat onto chat2 once it is verifiably quiet; the checkpoint
//! posts keep cold readers loading one compact blob.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use hearth_doc::SessionDoc;

use super::*;

/// chat2 relay join (docs/chat2-sync.md C3): deadline on every dial,
/// capped jittered backoff, wake redial — and the client resolves only
/// after full catch-up (checkpoint + rows), so "joined" here means
/// "transcript converged".
impl DocHost {
    pub(super) fn spawn_chat2_join(
        &self,
        edge: EdgeConfig,
        handle: &Arc<ChatDocHandle>,
        cursor: u64,
    ) {
        let chat = handle.chat_id.clone();
        let doc = handle.doc.clone();
        let store = self.inner.store.clone();
        let http = self.inner.http.clone();
        let device = self.inner.config.device_id.clone();
        let weak = Arc::downgrade(handle);
        let host = self.clone();
        let mut token_changes = edge.token_changes();
        self.spawn_worker(async move {
            let sink = Arc::new(crate::chat2_host::EngineChatSink::new(&doc, store, chat.clone()));
            // The sink holds only a Weak doc ref (a strong one made every
            // chat2 handle read as perma-pinned — LRU eviction dead); this
            // task's own strong ref dies when the join resolves.
            drop(doc);
            let fetcher = Arc::new(crate::chat2_host::EdgeCheckpointFetcher::new(
                http,
                edge.clone(),
                chat.clone(),
            ));
            let url = edge.room_url(format!("/chat2/{chat}/ws"));
            let mut wake = hearth_sync::wake::subscribe();
            // Sibling-dial successes end a backoff wait immediately, exactly
            // like the joined clients' own reconnect loops (chat_client.rs).
            // Without this, a NEW chat whose first joins hit a network blip
            // waited out the full accumulated backoff (→30s) while every
            // established room redialed instantly on recovery — fresh sends
            // to new sessions stalled while other chats hummed (2026-08-19
            // user report, reproduced on two networks).
            let mut online = hearth_sync::wake::subscribe_online();
            let mut backoff = crate::workspace_host::JOIN_RETRY_BASE;
            loop {
                if weak.upgrade().is_none() {
                    return; // evicted or purged while dialing
                }
                // Dual transport: WS dial + a plain-HTTPS pull/push seam
                // (rows GET / POST on the same bearer auth as the checkpoint
                // fetch) — bootstraps in ~1 RTT and keeps syncing at backoff
                // cadence on networks that never pass the WS upgrade. With
                // the transport, connect resolves immediately (local-first).
                let transport = Arc::new(crate::chat2_host::EdgeChatTransport::new(
                    host.inner.http.clone(),
                    edge.clone(),
                    chat.clone(),
                    device.clone(),
                ));
                let dial = tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    hearth_sync::ChatClient::connect_via_transport(
                        url.clone(),
                        sink.clone(),
                        fetcher.clone(),
                        &device,
                        cursor,
                        transport,
                    ),
                )
                .await;
                match dial {
                    Ok(Ok(client)) => {
                        if edge.bearer().await.is_none() {
                            return;
                        }
                        let Some(handle) = weak.upgrade() else {
                            return; // evicted mid-dial: drop leaves the room
                        };
                        let mut events = client.events();
                        let mut lifecycle_events = client.events();
                        {
                            // Store + drain under ONE client-lock critical
                            // section: the subscription pushes to the buffer
                            // while holding this same lock, so every commit
                            // is either drained here or enqueued directly
                            // after — never dropped between (verify pass).
                            let mut client_slot = lock(&handle.chat2);
                            let pending: Vec<Vec<u8>> =
                                std::mem::take(&mut *lock(&handle.chat2_pending_local));
                            for update in pending {
                                client.enqueue_update(update);
                            }
                            *client_slot = Some(client);
                        }
                        tracing::info!(chat = %chat, "chat2 room joined (converged)");
                        // Bootstrap heal: a room with NO checkpoint can't
                        // cover its rows' causal deps for cold readers — a
                        // pre-0.1.34 first contact whose init batch never
                        // went up (every reader parks every row on missing
                        // deps, transcript invisible forever), or a host
                        // whose WS pushes strand. The checkpoint is the
                        // universal patch: full doc over plain HTTP. Checked
                        // once, shortly after join (an idle chat never hits
                        // the quiesce tick, so the tick can't be the only
                        // trigger).
                        if host.is_host(&chat) {
                            let host = host.clone();
                            let weak = weak.clone();
                            host.clone().spawn_worker(async move {
                                // With the pull-first transport the client
                                // constructs before any state answer — wait
                                // until the server's view is KNOWN (bounded)
                                // or the all-zero placeholder stats would
                                // misread as "no checkpoint" and upload a
                                // spurious full-doc heal on a thin link.
                                for _ in 0..40u32 {
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    let Some(handle) = weak.upgrade() else { return };
                                    let known = lock(&handle.chat2)
                                        .as_ref()
                                        .is_some_and(|c| c.stats().server_known);
                                    if known {
                                        break;
                                    }
                                }
                                let Some(handle) = weak.upgrade() else { return };
                                let no_checkpoint = lock(&handle.chat2)
                                    .as_ref()
                                    .is_some_and(|c| c.stats().server_known && c.stats().checkpoint_size == 0);
                                let has_content = handle
                                    .doc
                                    .read_entries()
                                    .map(|e| !e.is_empty())
                                    .unwrap_or(false);
                                if no_checkpoint && has_content {
                                    tracing::info!(chat = %handle.chat_id,
                                        "chat2 room has rows but no checkpoint; posting bootstrap checkpoint");
                                    host.spawn_chat2_checkpoint(&handle, "bootstrap");
                                }
                            });
                        }
                        // Host recovery duties (C3): a wiped room needs a
                        // seed checkpoint or fresh readers see only
                        // post-reset rows; rejected pushes reach peers only
                        // through a checkpoint. Watcher dies with the handle.
                        if host.is_host(&chat) {
                            let host = host.clone();
                            let weak = weak.clone();
                            let chat = chat.clone();
                            host.clone().spawn_worker(async move {
                                use hearth_sync::chat_client::ChatEvent;
                                loop {
                                    match events.recv().await {
                                        Ok(ChatEvent::ServerReset) => {
                                            let Some(handle) = weak.upgrade() else { return };
                                            tracing::warn!(chat = %chat, "chat2 room reset; posting seed checkpoint");
                                            host.spawn_chat2_checkpoint(&handle, "server-reset");
                                        }
                                        Ok(ChatEvent::PushRejected) => {
                                            let Some(handle) = weak.upgrade() else { return };
                                            tracing::warn!(chat = %chat, "chat2 push rejected; compensating via checkpoint");
                                            host.spawn_chat2_checkpoint(&handle, "push-rejected");
                                        }
                                        Ok(ChatEvent::GapUnrepairable { cursor }) => {
                                            let Some(handle) = weak.upgrade() else { return };
                                            tracing::error!(
                                                chat = %chat,
                                                cursor,
                                                "chat2 rows parked on deps the room \
                                                 cannot deliver; rebuilding the doc \
                                                 from the checkpoint"
                                            );
                                            host.spawn_chat2_doc_rebuild(&handle, cursor);
                                        }
                                        Ok(_) => {}
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                                    }
                                }
                            });
                        }
                        drop(handle);
                        if token_changes.is_none() {
                            return;
                        }
                        loop {
                            tokio::select! {
                                event = lifecycle_events.recv() => match event {
                                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                                },
                                _ = crate::workspace_host::token_changed(&mut token_changes) => {
                                    if edge.bearer().await.is_none() {
                                        if let Some(handle) = weak.upgrade() {
                                            lock(&handle.chat2).take();
                                            lock(&handle.chat2_local_sub).take();
                                        }
                                        tracing::info!(chat = %chat,
                                            "chat2 credentials removed; leaving room");
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(chat = %chat, error = %err,
                            backoff_ms = backoff.as_millis() as u64,
                            "chat2 join failed; retrying");
                    }
                    Err(_) => {
                        tracing::warn!(chat = %chat,
                            backoff_ms = backoff.as_millis() as u64,
                            "chat2 join timed out; retrying");
                    }
                }
                // Drain stale online events first: only successes DURING this
                // wait count, or our own last dial would cut every wait to
                // zero (same discipline as chat_client's wait_backoff).
                while online.try_recv().is_ok() {}
                tokio::select! {
                    _ = tokio::time::sleep(backoff + crate::workspace_host::join_retry_jitter()) => {
                        backoff = (backoff * 2).min(crate::workspace_host::JOIN_RETRY_CAP);
                    }
                    _ = wake.recv() => {
                        backoff = crate::workspace_host::JOIN_RETRY_BASE;
                    }
                    _ = online.recv() => {
                        backoff = crate::workspace_host::JOIN_RETRY_BASE;
                    }
                    _ = crate::workspace_host::token_changed(&mut token_changes) => {
                        backoff = crate::workspace_host::JOIN_RETRY_BASE;
                    }
                }
            }
        });
    }

    /// Host-side chat2 seed (docs/chat2-sync.md M1/M2): rebuild thin, POST
    /// the seed checkpoint, persist the thin lineage locally, THEN flip the
    /// registry — each step idempotent, a crash before the flip leaves the
    /// chat on s2 and the next open retries (M5).
    /// Defer a chat2 seed until the chat is verifiably quiet: the s2 room has
    /// joined (the rebuild must include every row the room holds — seeding
    /// from the pre-backfill local doc forked other devices' rows into the
    /// retired lineage) and the doc frontier has stopped moving (a
    /// nudge-burst in flight — an incoming queued command — must land and
    /// execute first). Holds only a weak handle: eviction ends the wait.
    pub(super) fn spawn_chat2_seed_when_quiet(
        &self,
        edge: EdgeConfig,
        chat_id: &str,
        handle: &Arc<ChatDocHandle>,
    ) {
        const TICK: std::time::Duration = std::time::Duration::from_millis(500);
        const QUIET_TICKS: u32 = 4; // 2s of frontier silence
        if !lock(&self.inner.seed_waiting).insert(chat_id.to_string()) {
            return; // a quiet-waiter is already armed for this chat
        }
        let host = self.clone();
        let chat = chat_id.to_string();
        let weak = Arc::downgrade(handle);
        self.spawn_worker(async move {
            async {
                // The old join-wait gate (seed only after the s2 room had
                // backfilled) is gone with the s2 client: no device writes
                // an s2 room anymore, so the host's local doc IS the
                // authority for a straggler gen-1 chat. Only the quiet gate
                // remains (no seed under a moving frontier).
                let mut quiet = 0u32;
                let mut last_vv: Option<Vec<u8>> = None;
                loop {
                    tokio::time::sleep(TICK).await;
                    let Some(handle) = weak.upgrade() else { return };
                    if handle.retired.load(Ordering::Relaxed) {
                        return;
                    }
                    let vv = handle.doc.doc().oplog_vv().encode();
                    if last_vv.as_ref() == Some(&vv) {
                        quiet += 1;
                        if quiet >= QUIET_TICKS {
                            let doc = handle.doc.clone();
                            drop(handle);
                            host.spawn_chat2_seed(edge, &chat, doc);
                            return;
                        }
                    } else {
                        quiet = 0;
                        last_vv = Some(vv);
                    }
                }
            }
            .await;
            // Cleared on EVERY exit path so an aborted wait (evicted handle,
            // room never joined, seed handed off) can re-arm later.
            lock(&host.inner.seed_waiting).remove(&chat);
        });
    }

    fn spawn_chat2_seed(&self, edge: EdgeConfig, chat_id: &str, doc: Arc<SessionDoc>) {
        {
            let mut seeding = lock(&self.inner.seeding);
            if !seeding.insert(chat_id.to_string()) {
                return; // seed already in flight
            }
        }
        let host = self.clone();
        let chat = chat_id.to_string();
        self.spawn_worker(async move {
            let outcome = host.seed_chat2(&edge, &chat, doc).await;
            lock(&host.inner.seeding).remove(&chat);
            match outcome {
                Ok(()) => {
                    tracing::info!(chat = %chat, "chat2 seeded; registry flipped to roomGen 2");
                }
                Err(err) => {
                    tracing::warn!(chat = %chat, error = %err,
                        "chat2 seed failed; chat stays on s2 (retries next open)");
                }
            }
        });
    }

    async fn seed_chat2(
        &self,
        edge: &EdgeConfig,
        chat_id: &str,
        doc: Arc<SessionDoc>,
    ) -> Result<(), String> {
        use base64::Engine as _;
        let vv_at_rebuild = doc.doc().oplog_vv().encode();
        let rebuilt = hearth_doc::rebuild::rebuild_thin_doc(&doc).map_err(|e| e.to_string())?;
        // The seed's own doc ref must be gone before the pinned re-check
        // below: `pinned` reads `Arc::strong_count(&handle.doc) > 1`, and
        // holding this clone made that true unconditionally — every seed
        // aborted "became active mid-seed" and the cutover never flipped a
        // single chat (v0.1.32 DOA).
        drop(doc);
        if !rebuilt.sidecar.is_empty() {
            // Sidecar PARKED (docs/chat2-sync.md A2): full outputs are not
            // uploaded; they survive in the rollback snapshot + run journal.
            tracing::info!(chat = %chat_id, payloads = rebuilt.sidecar.len(),
                "chat2 seed: sidecar parked; outputs stay local");
        }
        let snapshot = rebuilt.doc.export_snapshot().map_err(|e| e.to_string())?;
        let frontier = rebuilt.doc.doc().oplog_vv().encode();
        let bearer = edge.bearer().await.ok_or("signed out")?;
        let url = format!(
            "{}/chat2/{}/checkpoint?seqCovered=0",
            edge.url.trim_end_matches('/'),
            chat_id
        );
        let res = self
            .inner
            .http
            .post(&url)
            .bearer_auth(&bearer)
            .header(
                "x-chat2-frontier",
                base64::engine::general_purpose::STANDARD.encode(&frontier),
            )
            .body(snapshot.clone())
            .send()
            .await
            .map_err(|e| format!("seed checkpoint POST: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("seed checkpoint HTTP {}", res.status()));
        }
        // PINNED RE-CHECK before anything irreversible (review B1): if a
        // run/watcher attached during the rebuild+POST, abort — everything
        // they write would fork away from the thin lineage. The orphan
        // checkpoint in the chat2 room is harmless (wholly replaced by the
        // next seed's seqCovered=0 POST); the chat stays on s2 and the next
        // quiet open retries.
        {
            let handles = lock(&self.inner.handles);
            if let Some(handle) = handles.get(chat_id) {
                if self.seed_blocked(handle) {
                    return Err("chat became active mid-seed; aborted before flip".into());
                }
                // Frontier seal (review B1's TOCTOU): ANY doc movement since
                // the rebuild — a synced remote row, a local write that has
                // already released its doc ref — means the thin lineage is
                // missing it. Borrowed read (no Arc clone: that would trip
                // the pinned check we just passed).
                if handle.doc.doc().oplog_vv().encode() != vv_at_rebuild {
                    return Err("doc advanced mid-seed; aborted before flip".into());
                }
            }
        }
        // Rollback copy of the fat lineage BEFORE the thin one replaces it —
        // never overwriting an existing copy (review B5).
        let rollback_id = format!("{chat_id}.pre-chat2");
        if matches!(self.inner.store.load_snapshot(&rollback_id), Ok(None))
            && let Ok(Some(old)) = self.inner.store.load_snapshot(chat_id)
        {
            let _ = self.inner.store.save_snapshot(&rollback_id, &old);
        }
        self.inner
            .store
            .save_snapshot_with_cursor(chat_id, &snapshot, 0, crate::chat2_host::CHAT2_DOC_EPOCH)
            .map_err(|e| e.to_string())?;
        // Registry flip LAST — the cutover signal every device dials by.
        let flipped = self
            .workspace()
            .ok_or("no workspace host")?
            .set_chat_room_gen(chat_id, 2)
            .map_err(|e| e.to_string())?;
        if !flipped {
            return Err("chat row vanished during seed".into());
        }
        // The live handle still holds the FAT doc on the s2 room. Retire it:
        // it must never persist again (it would clobber the thin lineage);
        // drop it entirely when unpinned so the next open converges onto
        // chat2. A pinned (watched/running) handle keeps working against s2
        // until it closes — the flip is registry-side, readers already moved.
        let dropped = {
            let mut handles = lock(&self.inner.handles);
            if let Some(handle) = handles.get(chat_id) {
                handle.retired.store(true, Ordering::Relaxed);
                // Drop unless a live WRITER holds the doc. Watchers do not
                // keep the fat handle alive: their streams end with it and
                // they resubscribe onto the chat2 adopt path (the same
                // contract the cutover watcher enforces for remote flips).
                if Arc::strong_count(&handle.doc) == 1 {
                    handles.remove(chat_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        tracing::info!(chat = %chat_id, handle_dropped = dropped, "chat2 seed complete");
        Ok(())
    }
}

impl DocHost {
    /// chat2 host duties on the doc-quiesce tick (docs/chat2-sync.md C3):
    /// - threshold checkpoint: when the room's row log passes 512KB or 200
    ///   rows, post a full checkpoint so cold readers load one compact blob
    ///   instead of replaying the log (the alert-shaped growth bound);
    /// - tail sidecar: publish the last-64 transcript JSON for thin/instant
    ///   readers (the iOS fallback path).
    pub(super) async fn chat2_maintenance(&self, handle: &Arc<ChatDocHandle>) {
        if handle.retired.load(Ordering::Relaxed) {
            return;
        }
        let stats = match &*lock(&handle.chat2) {
            Some(client) => client.stats(),
            None => return,
        };
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let chat_id = handle.chat_id.clone();
        // Tail publish: cheap, every quiesce tick.
        if let Ok(tail) =
            hearth_doc::materialize_tail(&handle.doc, now_ms(), hearth_doc::TAIL_MESSAGE_COUNT)
            && let Ok(body) = serde_json::to_vec(&tail)
        {
            let http = self.inner.http.clone();
            let edge_tail = edge.clone();
            let chat = chat_id.clone();
            self.spawn_worker(async move {
                let Some(bearer) = edge_tail.bearer().await else {
                    return;
                };
                let url = format!(
                    "{}/chat2/{}/tail",
                    edge_tail.url.trim_end_matches('/'),
                    chat
                );
                let _ = http
                    .put(&url)
                    .bearer_auth(&bearer)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await;
            });
        }
        // Threshold checkpoint (rowBytes > 512KB || rows > 200), one in
        // flight at a time (review H1).
        if stats.row_bytes <= 512 * 1024 && stats.row_count <= 200 {
            return;
        }
        self.spawn_chat2_checkpoint(handle, "threshold");
    }

    /// POST a full checkpoint for a chat2 room (one in flight per handle).
    /// Callers: the quiesce-tick threshold above, and the client recovery
    /// events (`ServerReset` — a wiped room needs a seed checkpoint or every
    /// fresh reader sees only post-reset rows; `PushRejected` — the rejected
    /// ops reach peers only through a checkpoint).
    fn spawn_chat2_checkpoint(&self, handle: &Arc<ChatDocHandle>, reason: &'static str) {
        use base64::Engine as _;
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let stats = match &*lock(&handle.chat2) {
            Some(client) => client.stats(),
            None => return,
        };
        let chat_id = handle.chat_id.clone();
        if handle
            .checkpointing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let in_flight = handle.checkpointing.clone();
        let Ok(snapshot) = handle.doc.export_snapshot() else {
            in_flight.store(false, Ordering::Release);
            return;
        };
        let frontier = handle.doc.doc().oplog_vv().encode();
        let seq_covered = stats.cursor;
        let http = self.inner.http.clone();
        let weak_note = Arc::downgrade(handle);
        self.spawn_worker(async move {
            let Some(bearer) = edge.bearer().await else {
                in_flight.store(false, Ordering::Release);
                return;
            };
            let url = format!(
                "{}/chat2/{}/checkpoint?seqCovered={}",
                edge.url.trim_end_matches('/'),
                chat_id,
                seq_covered
            );
            let size = snapshot.len() as u64;
            match http
                .post(&url)
                .bearer_auth(&bearer)
                .header(
                    "x-chat2-frontier",
                    base64::engine::general_purpose::STANDARD.encode(&frontier),
                )
                .body(snapshot)
                .send()
                .await
            {
                Ok(res) if res.status().is_success() => {
                    tracing::info!(chat = %chat_id, seq_covered, reason, "chat2 checkpoint posted");
                    if let Some(handle) = weak_note.upgrade()
                        && let Some(client) = &*lock(&handle.chat2)
                    {
                        client.note_checkpoint(seq_covered, size);
                    }
                }
                Ok(res) => {
                    tracing::warn!(chat = %chat_id, status = res.status().as_u16(),
                        "chat2 checkpoint rejected");
                }
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "chat2 checkpoint POST failed");
                }
            }
            in_flight.store(false, Ordering::Release);
        });
    }

    /// Rebuild a chat's doc after [`ChatEvent::GapUnrepairable`]: the doc's
    /// pending buffer holds ops whose causal deps the room can no longer
    /// deliver (a founding op the checkpoint never covered — the 2026-08-27
    /// macpro wedge, chat 2bcb8778). Backfilling loops forever; the only heal
    /// is a FRESH doc re-imported from the room's checkpoint + rows. Drop the
    /// live handle (retiring it so it never persists its parked doc again) —
    /// the next open rebuilds from the stored snapshot + room catch-up, and
    /// its chat2 join refetches the checkpoint into a clean oplog.
    ///
    /// Bounded: at most one rebuild per [`REBUILD_COOLDOWN`] per chat, so a
    /// genuinely-broken room cannot hot-loop retire/reopen.
    fn spawn_chat2_doc_rebuild(&self, handle: &Arc<ChatDocHandle>, cursor: u64) {
        const REBUILD_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);
        let chat_id = handle.chat_id.clone();
        {
            let mut last = lock(&self.inner.rebuilds);
            if let Some(at) = last.get(&chat_id)
                && at.elapsed() < REBUILD_COOLDOWN
            {
                tracing::warn!(
                    chat = %chat_id,
                    "chat2 doc rebuild skipped (cooldown); transcript stays degraded"
                );
                return;
            }
            last.insert(chat_id.clone(), std::time::Instant::now());
        }
        // Retire + drop unless a live writer holds the doc (a run streaming
        // into it keeps its doc; the rebuild lands when the handle closes —
        // same contract as the chat2 seed's retire path).
        let dropped = {
            let mut handles = lock(&self.inner.handles);
            if let Some(existing) = handles.get(&chat_id) {
                existing.retired.store(true, Ordering::Relaxed);
                if Arc::strong_count(&existing.doc) == 1 {
                    handles.remove(&chat_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        tracing::info!(
            chat = %chat_id,
            cursor,
            handle_dropped = dropped,
            "chat2 doc rebuild: handle retired; the next open re-catches-up \
             from the checkpoint"
        );
        // Nudge any watchers: dropping the handle ends their transcript
        // streams; a UI resubscribe (or the next user action) reopens and
        // repopulates. Nothing else to drive from here.
    }
}
