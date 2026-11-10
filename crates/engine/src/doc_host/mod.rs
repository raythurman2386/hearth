//! DocHost — per-chat `SessionDoc` handles: snapshot persistence (debounced), edge room
//! sync (offline-tolerant), and the HOST-ONLY durable command executor.
//!
//! Pragmatic port of hearth's `session-docs.ts` + the `main.ts` executor (spec:
//! feature-inventory §3.3, ARCHITECTURE §2 "command plane"):
//! - the doc IS the outbox: commands and user entries commit locally and sync whenever a
//!   room connection exists; the engine is fully functional with sync disabled;
//! - on every doc change (local commit or remote import) the handle re-emits the joined
//!   transcript to watchers, drains pending commands, and schedules a snapshot save;
//! - command drain: evaluate via `evaluate_command` (with the DocsStore processed
//!   ledger), mark processed BEFORE execute, execute through the sessions engine, then
//!   write the outcome status back into the doc as the sole outcome writer.
//!
//! Chat ownership is gated on the workspace doc (`chats[chat_id].deviceId`), with
//! claim-on-first-command for unknown chats. Queueing a command for a chat hosted on
//! another device POSTs a durable nudge to that device's room (§7 cold-chat delivery);
//! the host's relay receives it and warm-opens the doc, which drains the queue.
//!
//! Split by concern: [`open`] builds and registers a handle, [`chat2_join`] owns the
//! chat2 room join/seed/checkpoint duties, and [`command_delivery`] owns the durable
//! command plane (queue, escort, drain, execute, retry). This module holds the shared
//! state, config, LRU/connectivity machinery and the snapshot persistence.

mod chat2_join;
mod command_delivery;
mod open;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use hearth_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, SessionCommandStatus, SessionDoc,
    SessionMessageEntry, join_continuation_entries,
};
use hearth_proto::HarnessId;
use hearth_sync::DocsStore;

use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, now_ms};

/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;

/// Warm-doc LRU: how many unwatched, run-less docs stay fully open. Everything
/// beyond this (and beyond [`hearth_doc::DOC_LRU_BYTE_BUDGET`]) is evicted
/// oldest-access-first — reopening from the SQLite snapshot measured within
/// ~11ms of a warm doc, so the cap trades no perceptible open latency.
const WARM_DOC_CAP: usize = 12;

/// Resident-memory estimate per compressed snapshot byte. Loro snapshots are
/// columnar+compressed; the in-memory doc plus mirror runs well above the blob
/// size. A rough multiplier is enough here — the budget is a safety ceiling,
/// the count cap does the day-to-day work.
const RESIDENT_BYTES_PER_SNAPSHOT_BYTE: usize = 6;

/// Floor per open doc (room socket buffers, tasks) regardless of content size.
const DOC_RESIDENT_FLOOR_BYTES: usize = 512 * 1024;

/// Docs touched this recently are never evicted. Closes the open→attach race:
/// `open()` returns a handle, and until the caller's `watch_messages` lands
/// the doc is unwatched and unpinned — a concurrent eviction would orphan the
/// watcher on a roomless doc that renders once and never updates again.
const EVICT_MIN_IDLE_MS: i64 = 30_000;

/// Edge connection config. The bearer is a **provider**, never a snapshot:
/// every room (re)connect and HTTP request re-reads it, so a rotated bearer
/// takes effect without an engine restart. The device identity is static and
/// rides the same seam as a [`hearth_rpc::StaticToken`].
#[derive(Clone)]
pub struct EdgeConfig {
    /// Edge base URL (`http(s)://…`); rewritten to `ws(s)` for the room socket.
    pub url: String,
    /// Fresh-bearer provider (the relay's `TokenSource`), consulted per
    /// connect/request. `None` from the provider = signed out.
    pub token: Arc<dyn hearth_rpc::TokenSource>,
    /// This engine's device id, carried on room dials (`&device=`) so the
    /// edge can attribute sockets in logs. Debugging the 2026-08-04 deaf
    /// socket meant reverse-engineering devices from rotating IPv6 privacy
    /// addresses; never again. Empty = omitted (tests).
    pub device_id: String,
}

impl std::fmt::Debug for EdgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeConfig")
            .field("url", &self.url)
            .field("token", &"<provider>")
            .finish()
    }
}

impl EdgeConfig {
    pub fn new(url: impl Into<String>, token: Arc<dyn hearth_rpc::TokenSource>) -> Self {
        Self {
            url: url.into(),
            token,
            device_id: String::new(),
        }
    }

    /// Attribute this engine's room sockets in edge logs.
    pub fn with_device(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = device_id.into();
        self
    }

    /// Fixed bearer — dev mode and tests, where tokens never expire.
    pub fn with_static_token(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::new(url, Arc::new(hearth_rpc::StaticToken(token.into())))
    }

    /// The current bearer, refreshed by the provider if stale. `None` = signed out.
    pub async fn bearer(&self) -> Option<String> {
        self.token.token().await
    }

    pub fn token_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.token.subscribe()
    }

    /// A per-dial room URL provider for `path` (e.g. `/session/{chatId}/ws`):
    /// the bearer is re-fetched before every connect, so reconnects after a
    /// token expiry present a fresh `?token=` instead of the boot-time one.
    pub fn room_url(&self, path: impl Into<String>) -> Arc<dyn hearth_sync::UrlProvider> {
        let ws_base = self.url.replacen("http", "ws", 1);
        Arc::new(EdgeRoomUrl {
            base: format!("{}{}", ws_base.trim_end_matches('/'), path.into()),
            token: self.token.clone(),
            device_id: self.device_id.clone(),
        })
    }
}

struct EdgeRoomUrl {
    base: String,
    token: Arc<dyn hearth_rpc::TokenSource>,
    device_id: String,
}

impl hearth_sync::UrlProvider for EdgeRoomUrl {
    fn url(&self) -> futures::future::BoxFuture<'static, Result<String, hearth_sync::SyncError>> {
        let token = self.token.clone();
        let base = self.base.clone();
        let device = self.device_id.clone();
        Box::pin(async move {
            let token = token.token().await.ok_or_else(|| {
                hearth_sync::SyncError::Auth("no access token (signed out)".into())
            })?;
            let mut url = format!("{base}?token={token}");
            if !device.is_empty() {
                url.push_str(&format!("&device={device}"));
            }
            Ok(url)
        })
    }
}

#[derive(Debug, Clone)]
pub struct DocHostConfig {
    pub device_id: String,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// When present, each opened chat joins its edge session room. `None` = fully
    /// offline operation (local snapshots only).
    pub edge: Option<EdgeConfig>,
}

struct DocHostInner {
    store: Arc<DocsStore>,
    config: DocHostConfig,
    /// Set-once (first wins), cleared by `shutdown_workers`: sessions and
    /// doc-host reference each other through Arcs, so a retired runtime's
    /// graph only drops once this back-edge is severed.
    sessions: Mutex<Option<SessionsEngine>>,
    workspace: OnceLock<WorkspaceHost>,
    /// Worktree materialization for Run commands (see `set_repos`).
    repos: OnceLock<crate::repos::Repos>,
    /// Cancels every worker spawned through `spawn_worker` — the loops'
    /// own exit conditions (weak handle death, closed channels) don't cover
    /// runtime replacement, where Edge-capable tasks must stop doing
    /// network work even while something still pins the graph.
    shutdown: CancellationToken,
    /// Tracks every spawned worker so `shutdown_workers` can await them.
    tasks: TaskTracker,
    handles: Mutex<HashMap<String, Arc<ChatDocHandle>>>,
    /// chat2 seeds in flight (one per chat — reopen storms must not race
    /// duplicate rebuild+checkpoint POSTs; benign server-side, wasteful).
    seeding: Mutex<HashSet<String>>,
    /// chat2 quiet-waiters armed (one per chat): the cutover watcher re-arms
    /// on every registry change, and a long run would stack a tick loop per
    /// change without this.
    seed_waiting: Mutex<HashSet<String>>,
    /// Attachment-wait re-drain timers armed (one per chat): a command
    /// deferred on in-transit attachment bytes re-checks on a cadence, and
    /// each deferral must not stack another timer.
    drain_waiting: Mutex<HashSet<String>>,
    /// chat id → last doc-rebuild instant (GapUnrepairable heal cooldown,
    /// see `spawn_chat2_doc_rebuild`).
    rebuilds: Mutex<HashMap<String, std::time::Instant>>,
    /// Uploads store (engine assembly) — resolves `pending://` attachment
    /// refs and jails transfer reads to the uploads dir.
    uploads: OnceLock<crate::uploads::Uploads>,
    /// Connectivity watch (`WatchConnectivity`): lazily-started monitor
    /// publishes the edge posture on change (see `watch_connectivity`).
    connectivity: OnceLock<watch::Sender<hearth_proto::Connectivity>>,
    connectivity_started: AtomicBool,
    /// In-flight queued-attachment transfers, published per landed chunk
    /// (see `watch_transfers`). Entries live exactly as long as bytes are
    /// moving: added when a file's push starts, removed on commit or failure
    /// (a retry re-adds), so consumers can render a real percent while the
    /// relay leg runs and fall back to indeterminate otherwise.
    transfers: watch::Sender<Vec<hearth_proto::TransferProgress>>,
    connectivity_grace: Mutex<DegradeGrace>,
    /// Command ids currently BETWEEN mark-processed and their resolution in a
    /// drain. Distinguishes "executing right now" from "consumed by the
    /// ledger but dead" (a crash between mark and resolve): the drain
    /// terminalizes the latter as Rejected instead of leaving a forever-
    /// Pending entry no retry could ever reach (2026-08-19 swallowed-send).
    executing: Mutex<HashSet<String>>,
    /// Peer links (engine assembly, edge runtimes only) — the transport that
    /// pushes queued attachment bytes to a remote host.
    links: OnceLock<Arc<hearth_rpc::LinkCache>>,
    /// Shared client for sidecar blob PUT/GET (30s timeout, uploads.rs
    /// discipline — diff_sync's untimed client hung on dead links).
    http: reqwest::Client,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long raw degradation must persist before connectivity reports it.
/// Room joins, idle-link wakes, and navigation dials resolve well under a
/// second on healthy networks; real outages outlive this comfortably. Recovery
/// is never delayed.
const DEGRADE_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

/// One tracked degradation source in [`DegradeGrace`].
enum GraceKey<'a> {
    OsPath,
    Registry,
    Chat(&'a str),
}

/// Show-slow / hide-fast hysteresis over raw connectivity signals. Pure over
/// injected `Instant`s so the grace window is unit-testable.
#[derive(Default)]
struct DegradeGrace {
    os_path: Option<std::time::Instant>,
    registry: Option<std::time::Instant>,
    chats: HashMap<String, std::time::Instant>,
}

impl DegradeGrace {
    /// Feed one raw sample; returns whether to REPORT the source as degraded.
    /// Healthy clears the timer instantly; degraded reports only once it has
    /// persisted for [`DEGRADE_GRACE`].
    fn degraded(&mut self, key: GraceKey, raw: bool, now: std::time::Instant) -> bool {
        let slot: &mut Option<std::time::Instant> = match key {
            GraceKey::OsPath => &mut self.os_path,
            GraceKey::Registry => &mut self.registry,
            GraceKey::Chat(id) => {
                if raw && !self.chats.contains_key(id) {
                    self.chats.insert(id.to_string(), now);
                }
                match self.chats.get_mut(id) {
                    Some(_) if !raw => {
                        self.chats.remove(id);
                        return false;
                    }
                    Some(since) => return now.duration_since(*since) >= DEGRADE_GRACE,
                    None => return false,
                }
            }
        };
        if !raw {
            *slot = None;
            return false;
        }
        let since = *slot.get_or_insert(now);
        now.duration_since(since) >= DEGRADE_GRACE
    }

    /// Drop timers for chats that no longer have open docs.
    fn retain_chats(&mut self, keep: impl Fn(&str) -> bool) {
        self.chats.retain(|id, _| keep(id));
    }
}

#[derive(Clone)]
pub struct DocHost {
    inner: Arc<DocHostInner>,
}

/// One open chat doc: the `SessionDoc`, its change plumbing, and the room client.
pub struct ChatDocHandle {
    chat_id: String,
    device_id: String,
    doc: Arc<SessionDoc>,
    messages_tx: watch::Sender<Vec<SessionMessageEntry>>,
    /// True when the doc changed while nobody watched: the mirror rebuild is
    /// deferred to the next `watch_messages` attach instead of paid per commit.
    mirror_dirty: AtomicBool,
    /// Epoch ms of the last open/watch touch — the LRU eviction key.
    last_access: AtomicI64,
    /// Last known snapshot blob size — the eviction budget estimate's input.
    snapshot_bytes: AtomicUsize,
    /// The sync generation this handle was BUILT for (1 = legacy s2,
    /// 2 = chat2). Gen-1 handles no longer join any room (the s2 client is
    /// gone); they serve the local fat doc read-only until the host's seed
    /// flips the chat to chat2. The staleness checks compare this against
    /// the registry — inferring mode from `chat2_local_sub` misread
    /// edge-less chat2 handles (no subscription is ever installed offline)
    /// as stale s2 and retired them on every open, dropping the doc out
    /// from under live runs.
    room_gen: u32,
    /// A threshold checkpoint POST is in flight (review H1: the quiesce
    /// tick must not stack concurrent full-snapshot uploads).
    checkpointing: Arc<AtomicBool>,
    /// Set when a chat2 seed replaced this handle's lineage on disk: every
    /// further snapshot save from this handle is a stale FAT doc that would
    /// clobber the thin one — retired handles never persist again (unless no
    /// thin lineage exists on disk at all; `save_snapshot` double-checks, so
    /// a doc that was never seeded can't lose its only copy).
    retired: AtomicBool,
    /// chat2 relay client (docs/chat2-sync.md C3) — populated once the
    /// registry names roomGen 2 for this chat and the join resolves.
    chat2: Mutex<Option<hearth_sync::ChatClient>>,
    /// Local commits made before the relay connects (the dial can take up
    /// to a minute; offline, forever): buffered here by the subscription
    /// below and drained into the client on join (review B3 — a user
    /// message typed during the dial must not silently never sync).
    chat2_pending_local: Mutex<Vec<Vec<u8>>>,
    /// Local-update feed into the chat2 client (drop = unsubscribe).
    chat2_local_sub: Mutex<Option<loro::Subscription>>,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
}

impl ChatDocHandle {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn doc(&self) -> &SessionDoc {
        &self.doc
    }

    pub fn doc_arc(&self) -> Arc<SessionDoc> {
        self.doc.clone()
    }

    /// Joined transcript watch — re-sent on every doc change (WatchDocMessages).
    ///
    /// Attach-time refresh: the mirror is only maintained while watched, so a
    /// doc that changed unwatched materializes here, once, instead of on every
    /// commit it sat through in the background.
    pub fn watch_messages(&self) -> watch::Receiver<Vec<SessionMessageEntry>> {
        self.touch();
        // Attach is a user signal: verify a quiet room is actually alive
        // (a doc-wedged DO keeps answering pings while delivering nothing,
        // and the background probe cadence can be hours out). Coalescing
        // no-op on a healthy or recently-active room.
        if let Some(chat2) = lock(&self.chat2).as_ref() {
            chat2.probe();
        }
        // Subscribe BEFORE the dirty check: a commit racing this attach then
        // sees a live receiver and publishes, instead of re-marking dirty
        // after our refresh and leaving the new watcher a cleared mirror.
        let rx = self.messages_tx.subscribe();
        if self.mirror_dirty.load(Ordering::Acquire) {
            self.publish_messages();
        }
        rx
    }

    fn touch(&self) {
        self.last_access.store(now_ms(), Ordering::Relaxed);
    }

    pub fn connected(&self) -> bool {
        lock(&self.chat2).is_some()
    }

    /// Write a complete user message entry, idempotent by id (the client-minted message
    /// id — a re-executed command or optimistic echo never duplicates the entry).
    pub fn write_user_message(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        if self.doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        self.doc.push_message(&SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_string(),
            }],
            created_at,
            device_id: self.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
    }

    /// Recovery sweep: stamp this device's abandoned `streaming` entries `aborted`, appending
    /// `note` as a visible error part so the transcript says WHY the turn
    /// ended (hearth folded "Run interrupted by backend restart" the same
    /// way). Returns the stamped entries' `(id, created_at)` — recovery uses
    /// them for the resume-freshness check.
    pub fn mark_abandoned_streams(&self, note: &str) -> Result<Vec<(String, i64)>, DocError> {
        let mut stamped = Vec::new();
        for entry in self.doc.read_entries()? {
            if entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Streaming)
                && entry.device_id == self.device_id
                && self
                    .doc
                    .set_message_status(&entry.id, MessageStatus::Aborted)?
            {
                let part_id = format!("{}-recovery", entry.id);
                if let Err(err) = self.doc.append_error_part(&entry.id, &part_id, note) {
                    tracing::warn!(chat = %self.chat_id, error = %err, "recovery note append failed");
                }
                stamped.push((entry.id.clone(), entry.created_at));
            }
        }
        if !stamped.is_empty() {
            self.publish_messages();
        }
        Ok(stamped)
    }

    fn publish_messages(&self) {
        self.mirror_dirty.store(false, Ordering::Release);
        match self.doc.read_entries() {
            Ok(entries) => {
                let joined = join_continuation_entries(entries);
                // send_replace: update the watch even with no subscribers yet, so a
                // late subscriber's first borrow sees the current transcript.
                self.messages_tx.send_replace(joined);
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "transcript read failed");
            }
        }
    }

    /// Per-commit publish path: unwatched docs just mark the mirror dirty —
    /// rebuilding a full transcript nobody reads was a per-tick cost on every
    /// open doc (and kept a second transcript copy hot).
    fn publish_messages_if_watched(&self) {
        if self.messages_tx.receiver_count() == 0 {
            self.mirror_dirty.store(true, Ordering::Release);
            // Shrink the stale mirror: watch_messages rebuilds on attach.
            self.messages_tx.send_replace(Vec::new());
        } else {
            self.publish_messages();
        }
    }

    /// Rough resident cost for the LRU budget.
    fn resident_estimate(&self) -> usize {
        (self.snapshot_bytes.load(Ordering::Relaxed) * RESIDENT_BYTES_PER_SNAPSHOT_BYTE)
            .max(DOC_RESIDENT_FLOOR_BYTES)
    }
}

impl DocHost {
    pub fn new(store: Arc<DocsStore>, config: DocHostConfig) -> Self {
        Self {
            inner: Arc::new(DocHostInner {
                store,
                config,
                sessions: Mutex::new(None),
                workspace: OnceLock::new(),
                repos: OnceLock::new(),
                shutdown: CancellationToken::new(),
                tasks: TaskTracker::new(),
                handles: Mutex::new(HashMap::new()),
                seeding: Mutex::new(HashSet::new()),
                seed_waiting: Mutex::new(HashSet::new()),
                drain_waiting: Mutex::new(HashSet::new()),
                // GapUnrepairable heal cooldown (see spawn_chat2_doc_rebuild).
                rebuilds: Mutex::new(HashMap::new()),
                uploads: OnceLock::new(),
                connectivity: OnceLock::new(),
                connectivity_started: AtomicBool::new(false),
                transfers: watch::channel(Vec::new()).0,
                connectivity_grace: Mutex::new(DegradeGrace::default()),
                executing: Mutex::new(HashSet::new()),
                links: OnceLock::new(),
                http: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
            }),
        }
    }

    /// Every background task rides the tracker, raced against the shutdown
    /// token: the loops' own exits stay authoritative in normal operation;
    /// the token is the retirement override.
    fn spawn_worker(&self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        let cancel = self.inner.shutdown.clone();
        self.inner.tasks.spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = fut => {}
            }
        });
    }

    /// `spawn_worker` for sites that pre-resolve a runtime handle (callers
    /// reachable from bare sync contexts, where `tasks.spawn` would panic).
    fn spawn_worker_on(
        &self,
        runtime: &tokio::runtime::Handle,
        fut: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let cancel = self.inner.shutdown.clone();
        self.inner.tasks.spawn_on(
            async move {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = fut => {}
                }
            },
            runtime,
        );
    }

    /// The sessions engine, once wired. `None` before assembly or after
    /// `shutdown_workers` — callers treat both as "executor unavailable".
    fn sessions(&self) -> Option<SessionsEngine> {
        lock(&self.inner.sessions).clone()
    }

    /// Wire the sessions engine (engine assembly; see `SessionsEngine::set_doc_host`).
    pub fn set_sessions(&self, sessions: SessionsEngine) {
        {
            // First set wins (the OnceLock contract this slot replaced).
            let mut slot = lock(&self.inner.sessions);
            if slot.is_none() {
                *slot = Some(sessions);
            }
        }
        // Commands may already be pending in warm-opened docs.
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            self.spawn_worker(async move { host.drain_commands(&handle).await });
        }
    }

    /// Retire this host's workers (runtime shutdown): cancel and await every
    /// spawned task, drop every open chat handle (ending the weak-keyed
    /// room/join loops and watcher streams), and sever the sessions back-edge
    /// so the engine graph can actually drop. Idempotent.
    pub async fn shutdown_workers(&self) {
        self.inner.shutdown.cancel();
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
        // Snapshot open docs BEFORE releasing their handles: the handles map
        // holds the only strong doc refs, and an unflushed doc dies with it.
        self.flush_all();
        // Take the map under the lock, drop the handles outside it.
        let handles = std::mem::take(&mut *lock(&self.inner.handles));
        drop(handles);
        lock(&self.inner.seeding).clear();
        lock(&self.inner.seed_waiting).clear();
        lock(&self.inner.sessions).take();
    }

    /// Test-only retirement sentinel: reports true once the doc-host graph
    /// has actually been freed.
    #[doc(hidden)]
    pub fn retirement_probe(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
        let weak = Arc::downgrade(&self.inner);
        Box::new(move || weak.upgrade().is_none())
    }

    /// Wire the repos engine (engine assembly) — worktree materialization for
    /// Run commands carrying a [`hearth_proto::WorktreeSpec`].
    pub fn set_repos(&self, repos: crate::repos::Repos) {
        let _ = self.inner.repos.set(repos);
    }

    /// Wire the uploads store (engine assembly) — `pending://` ref resolution
    /// and the transfer-read jail.
    pub fn set_uploads(&self, uploads: crate::uploads::Uploads) {
        let _ = self.inner.uploads.set(uploads);
    }

    /// Wire the peer-link cache (engine assembly, edge runtimes only) — the
    /// transport for queued attachment transfers to a remote host.
    pub fn set_links(&self, links: Arc<hearth_rpc::LinkCache>) {
        let _ = self.inner.links.set(links);
    }

    /// Re-evaluate every open chat's command queue NOW. Called after an
    /// upload commit lands bytes on this device: a Run deferred on those
    /// bytes (`pending://` refs not yet on disk) becomes executable the
    /// moment its transfer completes — event-driven, not timer luck.
    pub fn kick_drains(&self) {
        let handles: Vec<Arc<ChatDocHandle>> =
            lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            self.spawn_worker(async move { host.drain_commands(&handle).await });
        }
    }

    /// Wire the workspace host (engine assembly) — the source of chat-ownership rows.
    pub fn set_workspace(&self, workspace: WorkspaceHost) {
        let chats = workspace.watch_chats();
        if self.inner.workspace.set(workspace).is_ok() {
            self.spawn_cutover_watcher(chats);
            self.spawn_migration_sweep();
        }
    }

    /// Host migration sweep: proactively seed this device's own s2 chats
    /// onto chat2, one per tick. The lazy open()-triggered seed could never
    /// fire in real usage — idle chats are never opened, a headless host's
    /// only opens are nudges (which arrive exactly when a run starts), and
    /// viewing a chat used to pin it. Every safety gate stays (s2 room
    /// joined, 2s frontier quiet, no live writer, no pending commands,
    /// frontier seal at the flip) — only the trigger changes: the host owns
    /// migrating its chats. Aborted seeds retry after a gap; a migrated
    /// fleet makes the tick a cheap no-op scan.
    fn spawn_migration_sweep(&self) {
        const TICK: std::time::Duration = std::time::Duration::from_secs(30);
        const RETRY_GAP_MS: i64 = 10 * 60 * 1000;
        let host = self.clone();
        self.spawn_worker(async move {
            let mut attempted: HashMap<String, i64> = HashMap::new();
            loop {
                tokio::time::sleep(TICK).await;
                let Some(edge) = host.inner.config.edge.clone() else {
                    return; // edge-less engine: nothing to migrate onto
                };
                let Some(ws) = host.workspace() else { continue };
                let chats: Vec<hearth_proto::Chat> = ws.watch_chats().borrow().clone();
                let device = host.inner.config.device_id.clone();
                let now = now_ms();
                let candidate = chats.into_iter().find(|c| {
                    c.device_id == device
                        && c.room_gen.unwrap_or(1) < 2
                        && !attempted
                            .get(&c.id)
                            .is_some_and(|t| now - *t <= RETRY_GAP_MS)
                });
                let Some(chat) = candidate else { continue };
                attempted.insert(chat.id.clone(), now);
                // A cached (warm) handle never re-enters open()'s build path,
                // so arm the quiet-waiter directly; a cold chat opens, which
                // arms it on the way up.
                let cached = lock(&host.inner.handles).get(&chat.id).cloned();
                match cached {
                    Some(handle) => {
                        if handle.room_gen < 2 && !handle.retired.load(Ordering::Relaxed) {
                            tracing::debug!(chat = %chat.id, "migration sweep: arming seed on warm s2 chat");
                            host.spawn_chat2_seed_when_quiet(edge, &chat.id, &handle);
                        }
                    }
                    None => {
                        tracing::debug!(chat = %chat.id, "migration sweep: opening cold s2 chat to seed");
                        if let Err(err) = host.open(&chat.id) {
                            tracing::warn!(chat = %chat.id, error = %err, "migration sweep open failed");
                        }
                    }
                }
            }
        });
    }

    /// Live cutover convergence: when a registry chat row flips to roomGen 2
    /// while this device holds an s2-mode handle, retire it NOW — a viewer
    /// watching the chat at flip time otherwise stays frozen on the dead s2
    /// room until they happen to reopen (the host writes only to chat2 from
    /// the flip on). Dropping the handle ends the watch streams cleanly; the
    /// UI's transcript/standing watches resubscribe and the fresh open takes
    /// the chat2 adopt path. A handle with a LIVE local writer (a running
    /// turn's doc ref) is left alone — the host never flips mid-run, and a
    /// racing writer must never lose its doc out from under it.
    fn spawn_cutover_watcher(&self, mut chats: watch::Receiver<Vec<hearth_proto::Chat>>) {
        let host = self.clone();
        self.spawn_worker(async move {
            loop {
                if chats.changed().await.is_err() {
                    return; // workspace host gone (shutdown)
                }
                let flipped: Vec<String> = chats
                    .borrow_and_update()
                    .iter()
                    .filter(|c| c.room_gen.unwrap_or(1) >= 2)
                    .map(|c| c.id.clone())
                    .collect();
                if flipped.is_empty() {
                    continue;
                }
                let mut dropped: Vec<String> = Vec::new();
                let mut stuck_live: Vec<(String, Arc<ChatDocHandle>)> = Vec::new();
                {
                    let mut handles = lock(&host.inner.handles);
                    for chat_id in flipped {
                        let Some(handle) = handles.get(&chat_id) else {
                            continue;
                        };
                        if handle.room_gen >= 2 {
                            continue; // already chat2-mode
                        }
                        let live_writer = Arc::strong_count(&handle.doc) > 1;
                        if live_writer {
                            // A run is writing into this s2 doc while the
                            // registry already says chat2 — the born-gen2
                            // race (this open beat its own CreateChat mint,
                            // 2026-08-11: transcript never reached any other
                            // device). No thin lineage exists on disk yet,
                            // so retiring here would suppress the doc's only
                            // persistence and abort the quiet-seed. Leave it
                            // live; seed once the run quiesces — the seed
                            // posts the chat2 checkpoint, persists the thin
                            // lineage, and retires the handle itself.
                            stuck_live.push((chat_id, handle.clone()));
                            continue;
                        }
                        handle.retired.store(true, Ordering::Relaxed);
                        handles.remove(&chat_id);
                        tracing::info!(chat = %chat_id,
                            "s2 handle dropped on chat2 cutover; watchers resubscribe onto the new room");
                        dropped.push(chat_id);
                    }
                }
                for (chat_id, handle) in stuck_live {
                    if host.is_host(&chat_id)
                        && let Some(edge) = host.inner.config.edge.clone()
                    {
                        host.spawn_chat2_seed_when_quiet(edge, &chat_id, &handle);
                    }
                }
                // Watchers resubscribe on their own — but a NUDGE-opened
                // handle has none, and its s2 room never carried the queued
                // command anyway (the sender pushed to chat2). On a born-
                // chat2 chat the nudge beats the registry row by design
                // (direct HTTP vs room sync), so the first open lands here
                // and dying silently strands the first message until the
                // next nudge — every new remote session's first send sat
                // ~30s+ until the user re-sent (user report). If we host
                // the chat, reopen NOW: the fresh open dials the chat2
                // room and the change-driven drain executes the command.
                for chat_id in dropped {
                    if !host.is_host(&chat_id) {
                        continue;
                    }
                    match host.open(&chat_id) {
                        Ok(_) => tracing::info!(chat = %chat_id,
                            "reopened as chat2 after cutover drop (host, pending work possible)"),
                        Err(err) => tracing::warn!(chat = %chat_id, error = %err,
                            "chat2 reopen after cutover drop failed"),
                    }
                }
            }
        });
    }

    /// The workspace host, once wired (tests may assemble a DocHost without one).
    pub fn workspace(&self) -> Option<&WorkspaceHost> {
        self.inner.workspace.get()
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    /// Boot-time transcript salvage (born-gen2 aftermath, 2026-08-11): a chat
    /// we host whose chat2 doc has NO message entries while its run journal
    /// has events lost its transcript to a stuck s2 handle (the retired flag
    /// suppressed every snapshot save; the post-restart reopen born a blank
    /// lineage). The full fat doc still exists in the legacy s2 room — the
    /// stuck engine pushed every op into it until it died — and sometimes in
    /// a `.pre-chat2` rollback on disk. Re-append its entries (thinned) into
    /// the LIVE chat2 lineage as ordinary incremental updates: no lineage
    /// replacement, no checkpoint surgery, every device converges through
    /// the normal room flow. Idempotent: a doc with any message entry is
    /// never touched, and the salvage only runs on the hosting device.
    pub fn spawn_transcript_salvage(&self, journals_dir: std::path::PathBuf) {
        let host = self.clone();
        self.spawn_worker(async move {
            // Let boot settle (registry load, room joins) before sweeping.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let Some(ws) = host.workspace() else { return };
            let chats: Vec<hearth_proto::Chat> = ws.watch_chats().borrow().clone();
            for chat in chats {
                if chat.device_id != host.inner.config.device_id {
                    continue; // only the host owns its chats' history
                }
                if chat.room_gen.unwrap_or(1) < 2 {
                    continue; // still s2-mode: transcript lives in its room
                }
                let journal = journals_dir.join(format!("{}.jsonl", chat.id));
                let journaled = std::fs::metadata(&journal)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
                if !journaled {
                    continue; // never ran here — an empty doc is just new
                }
                if let Err(err) = host.salvage_chat_transcript(&chat.id).await {
                    tracing::warn!(chat = %chat.id, error = %err, "transcript salvage failed");
                }
            }
        });
    }

    async fn salvage_chat_transcript(&self, chat_id: &str) -> Result<(), String> {
        let handle = self.open(chat_id).map_err(|e| e.to_string())?;
        if !handle
            .doc()
            .read_entries()
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Ok(()); // transcript present — nothing lost
        }
        // Fat source: the M3 adopt's rollback copy on disk. (The other
        // source — the legacy s2 room — went away with the s2 client; any
        // transcript that existed only there was salvaged by earlier
        // releases or is reachable in the room's storage server-side.)
        let rollback_id = format!("{chat_id}.pre-chat2");
        let fat_bytes = self.inner.store.load_snapshot(&rollback_id).ok().flatten();
        let Some(bytes) = fat_bytes else {
            return Ok(()); // no fat lineage anywhere — genuinely empty chat
        };
        let raw = loro::LoroDoc::new();
        raw.import(&bytes).map_err(|e| e.to_string())?;
        let fat = SessionDoc::from_doc(raw);
        // Thin before appending (docs/chat2-sync.md A2): full outputs are
        // parked, exactly like a seed — they survive in the rollback copy
        // saved below and the run journal.
        let rebuilt = hearth_doc::rebuild::rebuild_thin_doc(&fat).map_err(|e| e.to_string())?;
        let entries = rebuilt.doc.read_entries().map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok(());
        }
        if matches!(self.inner.store.load_snapshot(&rollback_id), Ok(None)) {
            let _ = self.inner.store.save_snapshot(&rollback_id, &bytes);
        }
        // Re-check emptiness at the last instant: a run that started during
        // the room fetch must not get history interleaved under it.
        if !handle
            .doc()
            .read_entries()
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Err("doc gained entries mid-salvage; aborted".into());
        }
        for entry in &entries {
            handle
                .doc()
                .push_message(entry)
                .map_err(|e| e.to_string())?;
        }
        tracing::info!(chat = %chat_id, entries = entries.len(),
            "transcript salvaged into chat2 lineage");
        Ok(())
    }

    /// LRU eviction: while the warm set exceeds [`WARM_DOC_CAP`] or the
    /// resident estimate exceeds `DOC_LRU_BYTE_BUDGET`, close the
    /// least-recently-touched unpinned docs. Pinned (never evicted):
    /// - watched docs (`messages_tx` has receivers — a UI transcript);
    /// - docs with a live writer (`Arc<SessionDoc>` held outside the handle —
    ///   a run streaming into it);
    /// - host-side docs with pending commands (the executor owes them work).
    ///
    /// Eviction flushes a final snapshot, so reopen loses nothing; missed
    /// remote updates re-arrive through the room join's VV backfill.
    fn evict_over_budget(&self) {
        let mut by_age: Vec<(i64, String)> = {
            let handles = lock(&self.inner.handles);
            handles
                .values()
                .map(|h| (h.last_access.load(Ordering::Relaxed), h.chat_id.clone()))
                .collect()
        };
        by_age.sort_unstable();
        for (last_access, chat_id) in by_age {
            if now_ms() - last_access < EVICT_MIN_IDLE_MS {
                // Sorted oldest-first: everything after this is younger.
                return;
            }
            let (count, estimate) = {
                let handles = lock(&self.inner.handles);
                (
                    handles.len(),
                    handles
                        .values()
                        .map(|h| h.resident_estimate())
                        .sum::<usize>(),
                )
            };
            if count <= WARM_DOC_CAP && estimate <= hearth_doc::DOC_LRU_BYTE_BUDGET {
                return;
            }
            let evicted = {
                let mut handles = lock(&self.inner.handles);
                match handles.get(&chat_id) {
                    Some(handle) if !self.pinned(handle) => handles.remove(&chat_id),
                    _ => None,
                }
            };
            if let Some(handle) = evicted {
                // Final flush outside the map lock; ≤1s of changes could be
                // pending in the snapshot debounce.
                self.save_snapshot(&handle);
                tracing::debug!(chat = %handle.chat_id, "doc evicted (LRU)");
            }
        }
    }

    /// Seed-specific activity gate: a live WRITER (a run's doc ref) or
    /// pending host commands block a seed — watchers do NOT. Pure readers
    /// cannot lose writes, and the flip drops the handle so their streams
    /// end and resubscribe onto the chat2 adopt path. `pinned()` below keeps
    /// counting watchers for EVICTION, where a watched doc must stay
    /// resident. (Watcher-pinned seeds made "open a chat to look at it"
    /// self-defeating: the act of viewing blocked its own migration.)
    fn seed_blocked(&self, handle: &Arc<ChatDocHandle>) -> bool {
        if Arc::strong_count(&handle.doc) > 1 {
            return true;
        }
        if self.is_host(&handle.chat_id) {
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            match handle.doc.read_commands() {
                Ok(commands) => commands
                    .iter()
                    .any(|c| c.status == SessionCommandStatus::Pending && !is_processed(&c.id)),
                // Unreadable ledger: never flip blind.
                Err(_) => true,
            }
        } else {
            false
        }
    }

    fn pinned(&self, handle: &Arc<ChatDocHandle>) -> bool {
        if handle.messages_tx.receiver_count() > 0 {
            return true;
        }
        // The handle itself holds one doc ref; more means a live writer.
        if Arc::strong_count(&handle.doc) > 1 {
            return true;
        }
        if self.is_host(&handle.chat_id) {
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            match handle.doc.read_commands() {
                Ok(commands) => commands
                    .iter()
                    .any(|c| c.status == SessionCommandStatus::Pending && !is_processed(&c.id)),
                // Unreadable ledger: keep the doc, never evict blind.
                Err(_) => true,
            }
        } else {
            false
        }
    }

    /// Probe every open chat's room (window-focus liveness sweep). Each
    /// room ignores the hint unless it has been broadcast-quiet ≥30s.
    pub fn probe_open_chats(&self) {
        let handles: Vec<Arc<ChatDocHandle>> =
            lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            // chat2 rooms verify liveness on user signals — a
            // deaf-but-ponging DO otherwise freezes a watched transcript
            // for the whole background probe quiet window.
            if let Some(chat2) = lock(&handle.chat2).as_ref() {
                chat2.probe();
            }
        }
    }

    /// Window-focus fast path: one cheap HTTP probe of the edge decides
    /// whether to un-park every reconnect backoff NOW (success → online
    /// event → immediate redials with fresh backoff) or to leave them
    /// backing off (failure — a dial can't succeed either, so don't burn
    /// the attempt). Recovery rides the "user looked at the app" event
    /// instead of timer luck.
    pub fn probe_edge_reachability(&self) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let url = format!("{}/health", edge.url.trim_end_matches('/'));
        let http = self.inner.http.clone();
        self.spawn_worker_on(&runtime, async move {
            let res = http
                .get(&url)
                .timeout(std::time::Duration::from_secs(3))
                .send()
                .await;
            if let Ok(res) = res
                && res.status().is_success()
            {
                hearth_sync::wake::notify_online();
            }
        });
    }

    /// The in-flight queued-attachment transfer set: current entries first,
    /// then a fresh snapshot per landed chunk (see `push_attachments`).
    pub fn watch_transfers(&self) -> watch::Receiver<Vec<hearth_proto::TransferProgress>> {
        self.inner.transfers.subscribe()
    }

    /// Publish one transfer's progress (upserted by uploadId).
    fn transfer_progress_set(&self, upload_id: &str, file_name: &str, done: u64, total: u64) {
        self.inner.transfers.send_modify(|list| {
            match list.iter_mut().find(|t| t.upload_id == upload_id) {
                Some(t) => {
                    t.done = done;
                    t.total = total;
                }
                None => list.push(hearth_proto::TransferProgress {
                    upload_id: upload_id.to_string(),
                    file_name: file_name.to_string(),
                    done,
                    total,
                }),
            }
        });
    }

    /// Retire a transfer's progress entry (commit landed, or the attempt
    /// failed and the retry will re-publish).
    fn transfer_progress_clear(&self, upload_id: &str) {
        self.inner.transfers.send_modify(|list| {
            list.retain(|t| t.upload_id != upload_id);
        });
    }

    /// The connectivity stream: current posture first, then every change.
    /// Lazily starts a monitor — a 1s recompute over in-memory stats
    /// (atomics + small locks), published only when the value changes. The
    /// retry countdown renders client-side from `retry_at_ms`, so quiet
    /// periods emit nothing at all.
    pub fn watch_connectivity(&self) -> watch::Receiver<hearth_proto::Connectivity> {
        let tx = self
            .inner
            .connectivity
            .get_or_init(|| watch::channel(self.compute_connectivity()).0);
        let rx = tx.subscribe();
        if !self.inner.connectivity_started.swap(true, Ordering::SeqCst)
            && tokio::runtime::Handle::try_current().is_ok()
        {
            let host = self.clone();
            self.spawn_worker(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let next = host.compute_connectivity();
                    if let Some(tx) = host.inner.connectivity.get() {
                        tx.send_if_modified(|cur| {
                            if *cur == next {
                                false
                            } else {
                                *cur = next;
                                true
                            }
                        });
                    }
                }
            });
        }
        rx
    }

    /// One snapshot of the edge posture: OS path status beats registry-room
    /// state beats per-chat rooms. `Disabled` (the default) = local profile.
    ///
    /// Degradation is HYSTERETIC (v0.2.12 feedback): a room mid-join, an
    /// idle link waking for a send, or a navigation-triggered dial all read
    /// "disconnected" for a few hundred ms on a healthy network — surfacing
    /// those flashed amber warnings and "Queued" badges at every chat
    /// switch. Raw degradation must persist [`DEGRADE_GRACE`] before it is
    /// reported; recovery reports instantly.
    fn compute_connectivity(&self) -> hearth_proto::Connectivity {
        use hearth_proto::{ChatConnectivity, Connectivity, ConnectivityState};
        let workspace = self.workspace();
        let edge_expected = self.inner.config.edge.is_some()
            || workspace.as_ref().is_some_and(|w| w.edge_expected());
        if !edge_expected {
            return Connectivity::default();
        }
        let now = std::time::Instant::now();
        let mut grace = lock(&self.inner.connectivity_grace);
        let statuses = self.sync_statuses();
        grace.retain_chats(|id| statuses.iter().any(|(chat_id, _)| chat_id == id));
        let chats = statuses
            .into_iter()
            .map(|(chat_id, stats)| {
                let stats = stats.unwrap_or_default();
                let connected = !grace.degraded(GraceKey::Chat(&chat_id), !stats.connected, now);
                ChatConnectivity {
                    chat_id,
                    connected,
                    pending_pushes: stats.pending_pushes,
                }
            })
            .collect();
        let reconnect = workspace.as_ref().and_then(|w| w.reconnect_state());
        let registry_connected = workspace
            .as_ref()
            .and_then(|w| w.sync_status())
            .is_some_and(|s| s.connected);
        let path_offline =
            grace.degraded(GraceKey::OsPath, hearth_sync::wake::path_is_offline(), now);
        let registry_down = grace.degraded(GraceKey::Registry, !registry_connected, now);
        let (state, retry_at_ms, last_failure) = if path_offline {
            (
                ConnectivityState::Offline,
                0,
                reconnect.and_then(|r| r.last_failure),
            )
        } else if !registry_down {
            (ConnectivityState::Connected, 0, None)
        } else {
            let reconnect = reconnect.unwrap_or_default();
            (
                ConnectivityState::Reconnecting,
                reconnect.retry_at_ms,
                reconnect.last_failure,
            )
        };
        Connectivity {
            state,
            retry_at_ms,
            last_failure,
            chats,
        }
    }

    /// Per-open-chat room introspection for SyncStatus / `hearth sync`.
    /// `None` room = still dialing (join retry loop) or edge-less.
    pub fn sync_statuses(&self) -> Vec<(String, Option<hearth_sync::ChatStatsSnapshot>)> {
        let handles: Vec<Arc<ChatDocHandle>> =
            lock(&self.inner.handles).values().cloned().collect();
        let mut rows: Vec<(String, Option<hearth_sync::ChatStatsSnapshot>)> = handles
            .iter()
            .map(|h| {
                (
                    h.chat_id.clone(),
                    lock(&h.chat2).as_ref().map(|client| client.stats()),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// Drop a chat's doc unconditionally and delete its local snapshot — the
    /// chat is gone (DeleteChat / DeleteSpace cascade). Watchers see the
    /// stream end; a racing writer keeps its orphaned doc until the run ends.
    pub fn purge_chat(&self, chat_id: &str) {
        let removed = lock(&self.inner.handles).remove(chat_id);
        drop(removed);
        if let Err(err) = self.inner.store.delete_snapshot(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "snapshot delete failed");
        }
    }

    /// Upload a tool result's full output/diff to the R2 sidecar
    /// (`PUT {edge}/blob/{chatId}/{partId}[.diff]`, docs/chat2-sync.md A2).
    /// Fire-and-forget: the doc already carries the summary, so a lost upload
    /// degrades to "full output unavailable" — it must never block or fail
    /// the run. Offline/edge-less engines skip silently.
    pub fn upload_tool_sidecar(&self, chat_id: &str, payload: hearth_doc::SidecarPayload) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return; // bare sync callers (unit tests) skip rather than panic
        };
        let http = self.inner.http.clone();
        let base = format!(
            "{}/blob/{}/{}",
            edge.url.trim_end_matches('/'),
            chat_id,
            encode_part_segment(&payload.part_id)
        );
        self.spawn_worker_on(&runtime, async move {
            let Some(bearer) = edge.bearer().await else {
                return; // signed out; summary-only until the next session
            };
            let mut puts: Vec<(String, &'static str, Vec<u8>)> = Vec::new();
            if let Some(output) = &payload.output {
                puts.push((
                    base.clone(),
                    "text/plain; charset=utf-8",
                    output.clone().into_bytes(),
                ));
            }
            if let Some(diff) = &payload.diff
                && let Ok(json) = serde_json::to_vec(diff)
            {
                puts.push((format!("{base}.diff"), "application/json", json));
            }
            for (url, content_type, body) in puts {
                let sent = http
                    .put(&url)
                    .bearer_auth(&bearer)
                    .header("content-type", content_type)
                    .body(body)
                    .send()
                    .await;
                match sent {
                    Ok(res) if res.status().is_success() => {}
                    Ok(res) => tracing::warn!(url, status = res.status().as_u16(),
                        "tool sidecar upload rejected"),
                    Err(err) => {
                        tracing::warn!(url, error = %err, "tool sidecar upload failed (best-effort)")
                    }
                }
            }
        });
    }

    /// Fetch a sidecar blob by its doc-resident ref (`{chatId}/{partId}` or
    /// `…​.diff`) — the UI's lazy "Show full output" path, served over RPC
    /// because the UI crate has no HTTP client or edge bearer.
    pub async fn fetch_tool_blob(&self, blob_ref: &str) -> Result<String, EngineError> {
        // Same shape `apply_sidecar_refs` writes; anything else is a forged ref.
        let valid = blob_ref.split_once('/').is_some_and(|(chat, part)| {
            !chat.is_empty()
                && chat.len() <= 128
                && chat
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                && !part.is_empty()
                && part.len() <= 200
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:#~-".contains(&b))
        });
        if !valid {
            return Err(EngineError::Other(format!("bad blob ref: {blob_ref}")));
        }
        let Some(edge) = self.inner.config.edge.clone() else {
            return Err(EngineError::Other("offline: no edge configured".into()));
        };
        let Some(bearer) = edge.bearer().await else {
            return Err(EngineError::Other("signed out".into()));
        };
        // `valid` above guarantees the split; re-split to encode the part
        // segment for transport (PART_RE allows `#`, which a raw URL would
        // truncate as a fragment — the 2026-08-10 silent-collision bug).
        let (chat, part) = blob_ref.split_once('/').expect("validated above");
        let url = format!(
            "{}/blob/{}/{}",
            edge.url.trim_end_matches('/'),
            chat,
            encode_part_segment(part)
        );
        let res = self
            .inner
            .http
            .get(&url)
            .bearer_auth(&bearer)
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("sidecar fetch failed: {e}")))?;
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "sidecar fetch: HTTP {}",
                res.status().as_u16()
            )));
        }
        res.text()
            .await
            .map_err(|e| EngineError::Other(format!("sidecar body read failed: {e}")))
    }

    /// §2.2 writer discipline: we host a chat iff its workspace row's `deviceId` is
    /// ours; a chat with no row is claimable (claim-on-first-command). Without a
    /// wired workspace host (bare-DocHost tests) every open chat is ours — M2's
    /// behavior, now the degenerate case.
    fn is_host(&self, chat_id: &str) -> bool {
        self.workspace().is_none_or(|ws| ws.is_host(chat_id))
    }

    /// Chat-config harness when the workspace row carries one, else the default.
    fn harness_for(&self, chat_id: &str) -> HarnessId {
        self.workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .map(|config| config.harness)
            .unwrap_or(self.inner.config.default_harness)
    }

    /// The harness a request dispatches on: the request's own pick when it
    /// carries one (rides the command plane, immune to registry-row races),
    /// else [`Self::harness_for`].
    pub(crate) fn harness_for_request(
        &self,
        chat_id: &str,
        request: &hearth_proto::RunRequest,
    ) -> HarnessId {
        request.harness.unwrap_or_else(|| self.harness_for(chat_id))
    }

    /// A steer-turned-run with no in-process `last_request` (engine restarted
    /// since the last turn): rebuild the run config from the chat's workspace
    /// row — cwd from the row, model/reasoning/options/sandbox from its config
    /// (composer defaults otherwise). `None` without a workspace host or row.
    // (Also the RespondInput dead-run fallback's config source.)
    pub(crate) fn request_from_chat_row(
        &self,
        chat_id: &str,
        prompt: &str,
    ) -> Option<hearth_proto::RunRequest> {
        let workspace = self.workspace()?;
        let chat = match workspace.chat(chat_id) {
            Ok(chat) => chat?,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                return None;
            }
        };
        let config = chat.config;
        Some(hearth_proto::RunRequest {
            mode: config.as_ref().and_then(|c| c.mode),
            prompt: prompt.to_string(),
            harness: config.as_ref().map(|c| c.harness),
            model: config.as_ref().and_then(|c| c.model.clone()),
            reasoning: config.as_ref().and_then(|c| c.reasoning),
            model_options: config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default(),
            cwd: chat.cwd.unwrap_or_default(),
            sandbox: config
                .as_ref()
                .map(|c| c.sandbox)
                .unwrap_or(hearth_proto::SandboxLevel::WorkspaceWrite),
            auto_approve: false,
            attachments: Vec::new(),
            resume: None,
            worktree: None,
        })
    }

    fn save_snapshot(&self, handle: &ChatDocHandle) {
        if handle.retired.load(Ordering::Relaxed) {
            // A chat2 seed replaced this lineage on disk; persisting this
            // handle's fat doc would clobber the thin one. But retired with
            // NO thin lineage on disk (a stuck handle from the born-gen2
            // race) means this doc is its transcript's only copy — skipping
            // the save turned an app quit into total loss (2026-08-11);
            // persist it, and let the adopt path convert it on reopen.
            let thin_on_disk = matches!(
                self.inner.store.load_snapshot_with_cursor(&handle.chat_id),
                Ok(Some((_, _, epoch))) if epoch >= crate::chat2_host::CHAT2_DOC_EPOCH
            );
            if thin_on_disk {
                return;
            }
        }
        match handle.doc.export_snapshot() {
            Ok(bytes) => {
                handle.snapshot_bytes.store(bytes.len(), Ordering::Relaxed);
                if let Err(err) = self.inner.store.save_snapshot(&handle.chat_id, &bytes) {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot export failed");
            }
        }
    }

    /// Persist every open doc now (shutdown path; bypasses the debounce).
    pub fn flush_all(&self) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            self.save_snapshot(&handle);
        }
    }

    /// [`Self::flush_all`] bounded: every snapshot save runs off the async
    /// path and the whole loop races `budget`, so a wedged export/store costs
    /// its slice of the budget and never the stop itself. Docs whose save
    /// lost the race are logged and skipped — the next start's crash-recovery
    /// handles the gap.
    pub async fn flush_all_bounded(&self, budget: std::time::Duration) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        let deadline = std::time::Instant::now() + budget;
        for handle in handles {
            let chat_id = handle.chat_id.clone();
            let this = self.clone();
            let save = tokio::task::spawn_blocking(move || this.save_snapshot(&handle));
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if tokio::time::timeout(remaining.max(std::time::Duration::from_millis(100)), save)
                .await
                .is_err()
            {
                tracing::warn!(chat = %chat_id, "snapshot flush skipped (over budget)");
                break;
            }
        }
    }

    /// Close all account-scoped room memberships before graceful engine
    /// draining. Auth-aware join supervisors will not install a late client.
    pub fn disconnect_edge(&self) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            lock(&handle.chat2).take();
            lock(&handle.chat2_local_sub).take();
        }
    }
}

/// Percent-encode one URL path segment of a sidecar part id. PART_RE's
/// alphabet includes `#` and `:` — legal in R2 keys and doc refs, but a raw
/// `#` in a URL is a fragment delimiter (the request would silently hit the
/// truncated key, colliding parts). The Worker decodes before validating.
fn encode_part_segment(part_id: &str) -> String {
    let mut out = String::with_capacity(part_id.len());
    for byte in part_id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod degrade_grace_tests {
    use super::{DEGRADE_GRACE, DegradeGrace, GraceKey};
    use std::time::{Duration, Instant};

    #[test]
    fn blips_shorter_than_the_grace_never_report() {
        let mut g = DegradeGrace::default();
        let t0 = Instant::now();
        // A 300ms room join: degraded at t0, healthy again shortly after.
        assert!(!g.degraded(GraceKey::Chat("c1"), true, t0));
        assert!(!g.degraded(GraceKey::Chat("c1"), true, t0 + Duration::from_millis(300)));
        assert!(!g.degraded(GraceKey::Chat("c1"), false, t0 + Duration::from_millis(600)));
        // The recovery cleared the timer — a fresh blip starts from zero.
        assert!(!g.degraded(GraceKey::Chat("c1"), true, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn persistent_degradation_reports_after_the_grace_and_clears_instantly() {
        let mut g = DegradeGrace::default();
        let t0 = Instant::now();
        assert!(!g.degraded(GraceKey::Registry, true, t0));
        assert!(!g.degraded(GraceKey::Registry, true, t0 + DEGRADE_GRACE / 2));
        assert!(g.degraded(GraceKey::Registry, true, t0 + DEGRADE_GRACE));
        assert!(g.degraded(GraceKey::Registry, true, t0 + DEGRADE_GRACE * 3));
        // Hide-fast: one healthy sample reports Connected immediately.
        assert!(!g.degraded(GraceKey::Registry, false, t0 + DEGRADE_GRACE * 4));
    }

    #[test]
    fn sources_are_independent_and_closed_chats_are_dropped() {
        let mut g = DegradeGrace::default();
        let t0 = Instant::now();
        assert!(!g.degraded(GraceKey::Chat("gone"), true, t0));
        assert!(!g.degraded(GraceKey::OsPath, true, t0));
        // The chat's doc closes; its timer must not leak.
        g.retain_chats(|id| id != "gone");
        assert!(g.chats.is_empty());
        // OsPath kept its own timer through the retain.
        assert!(g.degraded(GraceKey::OsPath, true, t0 + DEGRADE_GRACE));
    }
}

#[cfg(test)]
mod part_segment_tests {
    use super::encode_part_segment;

    #[test]
    fn hash_and_colon_are_escaped_unreserved_pass_through() {
        assert_eq!(encode_part_segment("m1#c1"), "m1%23c1");
        assert_eq!(encode_part_segment("tool:call_9"), "tool%3Acall_9");
        assert_eq!(encode_part_segment("plain-id_0.diff~"), "plain-id_0.diff~");
    }
}

/// Per-chat background task: reacts to doc changes (local commits and remote imports)
/// by re-publishing the transcript watch, draining commands, and debouncing snapshots.
/// Holds only a weak handle so a dropped host tears the task down.
async fn chat_task(host: DocHost, weak: Weak<ChatDocHandle>, mut changed_rx: watch::Receiver<u64>) {
    // Initial pass: the snapshot may already carry pending commands. The
    // mirror stays lazy — it materializes on the first watch attach.
    {
        let Some(handle) = weak.upgrade() else { return };
        host.drain_commands(&handle).await;
    }
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // doc handle (and its change sender) is gone
                }
                let Some(handle) = weak.upgrade() else { break };
                handle.publish_messages_if_watched();
                host.drain_commands(&handle).await;
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(handle) = weak.upgrade() else { break };
                host.save_snapshot(&handle);
                // chat2 host duties ride the same quiesce tick (C3):
                // threshold checkpoints + the tail sidecar publish.
                host.chat2_maintenance(&handle).await;
                // Post-quiesce eviction pass: sizes just refreshed.
                host.evict_over_budget();
            }
        }
    }
}
