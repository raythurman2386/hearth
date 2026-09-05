//! hearth-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
pub use hearth_proto::{EngineInfo, HarnessId, WorkspaceScope};
use hearth_rpc::{RpcError, RpcReply, RpcService, methods};

use hearth_sync::DocsStore;

pub mod agent_accounts;
pub mod auth;
pub mod change_requests;
pub mod chat2_host;
pub mod diff_sync;
pub mod doc_host;
pub mod instance_lock;
pub mod local_import;
pub mod profile;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod source_control;
pub mod spaces;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{Auth, AuthConfig, AuthState, AuthUser, OrgMembership};
pub use change_requests::{ChangeRequestCacheKey, CheckoutChangeRequests};
pub use diff_sync::{
    CheckoutDiffSync, DiffFileTextPair, DiffSidecar, DiffSnapshot, TurnSnapshot,
    capture_commit_diff, capture_diff, capture_diff_against, capture_turn_diff, merge_base,
    read_diff_file_text, snapshot_tree, working_diff_base,
};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use profile::EngineProfile;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use source_control::{
    BranchHeadContext, ChangeRequestError, ChangeRequestProvider, ChangeRequestResolution,
    ChangeRequestResolver, CheckoutChangeRequestLookup, CheckoutSourceContext, GitHubCli,
    GitRemote, parse_git_remote,
};
pub use spaces::SpacesSync;
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use uploads::{AttachmentChunk, Uploads};
pub use workspace_host::{
    DEFAULT_ORG_ID, DEFAULT_USER_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig,
};

pub(crate) const LEGACY_UNKNOWN_DEVICE_NAME: &str = "unknown-device";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("doc: {0}")]
    Doc(#[from] hearth_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] hearth_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] hearth_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Whole-shutdown budget: every graceful-teardown stage (session settle,
/// updater stop, sync-graph joins) races this clock. 0.2.4's stop hung 90s
/// waiting on sync actors whose reconnect backoff ignored the shutdown flag;
/// the packaged unit gives 15s (TimeoutStopSec) and this budget must land
/// comfortably inside it.
const SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
/// The snapshot flush runs after the stage budget with its own bound — a
/// stuck store costs the budget, never the stop itself.
const FLUSH_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.hearth`, dev `~/.hearth-dev`).
    pub data_dir: PathBuf,
    /// Edge base URL.
    pub edge_url: String,
    /// Explicit development bearer for edge room joins. Synced WorkOS runtimes
    /// obtain their bearer from [`Auth`]; development stays offline when this is absent.
    pub edge_token: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Workspace-doc org (`ws/{orgId}` room). `None` = `$HEARTH_ORG_ID` or the dev default.
    /// In WorkOS mode the signed-in session's org wins.
    pub org_id: Option<String>,
    /// WorkOS client id — enables real auth; `None` = dev mode (bearer = `edge_token`).
    pub workos_client_id: Option<String>,
    /// MagicDNS hostname (or Tailscale hostname) of the always-on hub.
    /// Set `HEARTH_TAILNET_HOST` to opt into tailnet sync. Empty = local only.
    pub tailnet_host: Option<String>,
    /// Hub listen/dial port (default 27655).
    pub tailnet_port: u16,
    /// When true this process hosts chat2/registry rooms. When false, detect
    /// from `tailscale status` matching [`Self::tailnet_host`].
    pub tailnet_hub: bool,
}

impl EngineConfig {
    pub fn tailnet_enabled(&self) -> bool {
        self.tailnet_host
            .as_deref()
            .is_some_and(|h| !h.trim().is_empty())
    }

    /// `http://{tailnet_host}:{tailnet_port}` when tailnet is configured.
    pub fn tailnet_http_url(&self) -> Option<String> {
        let host = self.tailnet_host.as_deref()?.trim();
        (!host.is_empty()).then(|| format!("http://{host}:{}", self.tailnet_port))
    }
}

/// Default tailnet listen/dial port (IPC is 27654).
pub const DEFAULT_TAILNET_PORT: u16 = 27655;

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub terminals: Terminals,
    pub change_requests: CheckoutChangeRequests,
    pub diff_sync: CheckoutDiffSync,
    pub spaces_sync: SpacesSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub device_id: String,
    /// Local→synced profile import (account-scoped runtimes only).
    pub local_import: Option<local_import::LocalImporter>,
    workspace_scope: WorkspaceScope,
    /// Auth service (attached by [`Engine::run`]; a lazy dev-mode instance otherwise).
    auth: std::sync::Mutex<Option<Auth>>,
    /// Peer link cache for `targetDeviceId` routing (attached when edge+auth are ready).
    links: std::sync::Mutex<Option<Arc<hearth_rpc::LinkCache>>>,
    /// Release checker (attached by [`Engine::assemble_runtime`]) — the
    /// UpdateStatus stream + ApplyUpdate.
    updater: std::sync::Mutex<Option<hearth_update::Updater>>,
    /// The updater's token-change wake forwarder — owned so shutdown can end it.
    updater_wake: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash. Identity comes from
    /// `$HEARTH_ORG_ID` / `$HEARTH_USER_ID` (dev defaults `dev-org` / `dev-user`);
    /// use [`Self::assemble_with_identity`] to pass one explicitly.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let org_id = env_or("HEARTH_ORG_ID", DEFAULT_ORG_ID);
        let user_id = env_or("HEARTH_USER_ID", DEFAULT_USER_ID);
        let profile = EngineProfile::development(data_dir, &org_id, &user_id);
        Self::assemble_with_profile(profile, registry, default_harness, edge)
    }

    pub fn assemble_with_identity(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        org_id: &str,
        user_id: &str,
    ) -> Result<Self, EngineError> {
        let profile = EngineProfile::synced(data_dir, org_id, user_id);
        Self::assemble_with_profile(profile, registry, default_harness, edge)
    }

    /// Assemble the engine against one resolved, immutable workspace profile.
    pub fn assemble_with_profile(
        profile: EngineProfile,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let data_dir = profile.device_root();
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        Self::assemble_with_profile_locked(profile, registry, default_harness, edge, lock)
    }

    /// Assemble against a pre-acquired [`InstanceLock`]. The headed app takes
    /// the lock before binding the IPC port so the listener owner and the
    /// data-dir owner cannot diverge when several viewports bootstrap at once.
    pub fn assemble_with_profile_locked(
        profile: EngineProfile,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        lock: InstanceLock,
    ) -> Result<Self, EngineError> {
        let data_dir = profile.device_root();
        std::fs::create_dir_all(data_dir)?;
        let legacy_uploads_root = profile.claim_legacy_uploads_root()?;
        let device_id = load_or_create_device_id(data_dir)?;
        // This device's harness enablement (Settings → Agents) rides the
        // engine data dir — per-device, like the CLI installs it gates.
        registry.load_prefs(data_dir);
        let store = Arc::new(DocsStore::open(profile.store_root())?);
        let store_for_import = store.clone();
        let journal = Arc::new(RunJournal::open(profile.store_root().join("journals"))?);
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone());
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
                edge: edge.clone(),
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(&device_id),
                platform: std::env::consts::OS.to_string(),
                org_id: profile.org_id().to_string(),
                user_id: profile.user_id().to_string(),
                edge: edge.clone(),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        doc_host.spawn_transcript_salvage(profile.store_root().join("journals"));
        let repos = Repos::new(data_dir, &device_id);
        doc_host.set_repos(repos.clone());
        let change_requests = CheckoutChangeRequests::start(repos.clone(), &device_id);
        let terminals = Terminals::new();
        let uploads = Uploads::from_root_with_fallback(
            profile.uploads_root(),
            legacy_uploads_root.as_deref(),
        );
        // A recorded local→synced import grants this account the local
        // profile's uploads root read-only — transcripts imported earlier
        // embed absolute paths under it (same shape as the legacy adoption).
        if profile.scope() != WorkspaceScope::Local
            && let Some(root) =
                local_import::marker_grants_read_root(data_dir, profile.org_id(), profile.user_id())
        {
            uploads.add_read_only_root(&root);
        }
        // Queued-attachment support: the doc host resolves `pending://` refs
        // against this store and pushes staged bytes to remote hosts.
        doc_host.set_uploads(uploads.clone());
        let local_import = (profile.scope() == WorkspaceScope::Synced).then(|| {
            local_import::LocalImporter::new(
                data_dir,
                &device_id,
                profile.org_id(),
                profile.user_id(),
                store_for_import.clone(),
                profile.store_root().join("journals"),
                workspace.clone(),
                uploads.clone(),
            )
        });
        let agent_accounts = AgentAccounts::new(AgentAccountsConfig::detect(data_dir));
        sessions.set_titles(TitleGenerator::new(
            workspace.clone(),
            registry.clone(),
            repos.clone(),
        ));
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id, edge);
        // Turn starts snapshot the checkout tree — the "Latest turn" diff base.
        let turn_diff = diff_sync.clone();
        sessions.set_turn_listener(Arc::new(move |chat_id, cwd| {
            turn_diff.note_turn_start(chat_id, cwd);
        }));
        let spaces_sync = SpacesSync::start(repos.clone(), workspace.clone(), &device_id);
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            change_requests,
            diff_sync,
            spaces_sync,
            uploads,
            agent_accounts,
            device_id,
            local_import,
            workspace_scope: profile.scope(),
            auth: std::sync::Mutex::new(None),
            links: std::sync::Mutex::new(None),
            updater: std::sync::Mutex::new(None),
            updater_wake: std::sync::Mutex::new(None),
            _instance_lock: lock,
        })
    }

    pub fn workspace_scope(&self) -> WorkspaceScope {
        self.workspace_scope
    }

    /// Attach the auth service (before building the RPC service / relays).
    pub fn set_auth(&self, auth: Auth) {
        *self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(auth);
    }

    /// The attached auth service, or a lazily-created dev-mode one (in-process embeds
    /// that never wired WorkOS still answer AuthStatus honestly).
    pub fn auth(&self) -> Auth {
        let mut slot = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            let dev_user = std::env::var("HEARTH_EDGE_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "dev-user".into());
            let mut config = AuthConfig::new("http://localhost:27640", std::env::temp_dir());
            config.dev_user_id = dev_user;
            Auth::new(config)
        })
        .clone()
    }

    /// Attach the peer link cache — enables `targetDeviceId` routing,
    /// [`Self::dial_device`], and the doc host's queued-attachment transfers.
    pub fn set_links(&self, links: Arc<hearth_rpc::LinkCache>) {
        self.doc_host.set_links(links.clone());
        *self
            .links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(links);
    }

    pub fn links(&self) -> Option<Arc<hearth_rpc::LinkCache>> {
        self.links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Attach the release checker (before building the RPC service).
    pub fn set_updater_wake(&self, handle: tokio::task::JoinHandle<()>) {
        *self
            .updater_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    pub fn set_updater(&self, updater: hearth_update::Updater) {
        *self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(updater);
    }

    pub fn updater(&self) -> Option<hearth_update::Updater> {
        self.updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A live RPC client to another device's engine through its relay DO (the router's
    /// dial seam). Cached per device; invalidated + re-dialed on failure.
    pub async fn dial_device(
        &self,
        device_id: &str,
    ) -> Result<Arc<hearth_rpc::RpcClient>, EngineError> {
        let links = self
            .links()
            .ok_or_else(|| EngineError::Other("peer links unavailable (offline)".into()))?;
        links
            .client(device_id)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))
    }

    /// Start hosting our device room: serve the full RPC surface to relay clients and
    /// warm-open chat docs on nudges (§7 cold-chat command delivery). The token source
    /// re-reads auth on every (re)dial, so token refreshes take effect at reconnect.
    pub fn start_host_relay(&self, edge_url: &str) -> hearth_rpc::HostRelay {
        let auth = self.auth();
        let config =
            hearth_rpc::HostRelayConfig::new(edge_url, self.device_id.clone(), Arc::new(auth));
        let doc_host = self.doc_host.clone();
        let on_nudge: hearth_rpc::NudgeHandler = Arc::new(move |chat_id: String| {
            // Opening the doc joins its room + syncs; drain fires on the change
            // subscription — the command executes with no standing per-chat socket.
            match doc_host.open(&chat_id) {
                Ok(_) => tracing::info!(chat = %chat_id, "nudge: chat doc opened"),
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "nudge: open failed")
                }
            }
        });
        hearth_rpc::HostRelay::spawn(config, self.rpc_service(), on_nudge)
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        let mut rpc = EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
            self.repos.clone(),
            self.terminals.clone(),
            self.change_requests.clone(),
            self.diff_sync.clone(),
            self.uploads.clone(),
            self.agent_accounts.clone(),
            self.workspace_scope,
        )
        .with_auth(self.auth());
        if let Some(links) = self.links() {
            rpc = rpc.with_links(links);
        }
        if let Some(updater) = self.updater() {
            rpc = rpc.with_updater(updater);
        }
        if let Some(importer) = self.local_import.clone() {
            rpc = rpc.with_local_import(importer);
        }
        Arc::new(rpc)
    }

    /// Revoke every account-scoped transport before any slower graceful
    /// draining. Connected sockets remain authorized by their handshake, so
    /// clearing credentials alone is not a security boundary.
    pub fn disconnect_edge(&self) {
        if let Some(links) = self.links() {
            links.disconnect_all();
        }
        self.doc_host.disconnect_edge();
        self.workspace.disconnect_edge();
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// kill live PTYs, stamp our workspace `lastSeenAt`, and flush every open doc
    /// snapshot.
    ///
    /// The whole path is bounded by [`SHUTDOWN_BUDGET`]: each stage waits for
    /// its workers only within the remaining budget, and the snapshot flush —
    /// the one step whose work must not be half-abandoned — runs last, off the
    /// budget, but strictly bounded. 0.2.4's stop hung 90s here (the sync
    /// actors' reconnect backoff outran the join), and systemd escalated to
    /// SIGKILL, which is the crash-recovery path for every open store.
    pub async fn shutdown(&self) {
        use tokio_util::sync::CancellationToken;
        let deadline = CancellationToken::new();
        let deadline_for_stage = deadline.clone();
        let budget = tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_BUDGET).await;
            deadline_for_stage.cancel();
        });
        tokio::select! {
            _ = self.shutdown_within(&deadline) => {}
            _ = deadline.cancelled() => {
                tracing::warn!(
                    budget_secs = SHUTDOWN_BUDGET.as_secs(),
                    "graceful shutdown exceeded its budget; proceeding with the flush"
                );
            }
        }
        budget.abort();
        // Persist every open doc — this is why we shut down gracefully at all.
        // Bounded on its own: a stuck store must cost the budget, not the stop.
        tokio::time::timeout(FLUSH_BUDGET, async {
            self.doc_host.flush_all_bounded(FLUSH_BUDGET).await
        })
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("snapshot flush exceeded its budget");
        });
        self.workspace.shutdown();
        // Break the sessions ⇄ doc-host retain cycle so the replaced graph can
        // actually be freed once the last handle drops.
        self.sessions.clear_doc_host();
    }

    /// The stages of graceful teardown, all racing the shared deadline.
    async fn shutdown_within(&self, deadline: &tokio_util::sync::CancellationToken) {
        tokio::select! {
            _ = deadline.cancelled() => return,
            _ = self.sessions.shutdown() => {}
        }
        self.terminals.shutdown();
        self.agent_accounts.shutdown();
        self.change_requests.shutdown();
        // Cancel + await every worker that can reach Edge before flushing: a
        // replaced synced runtime must not keep polling releases or draining
        // the attachment outbox under the old identity after Local boots.
        let wake = self
            .updater_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(wake) = wake {
            wake.abort();
        }
        let updater = self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(updater) = updater {
            tokio::select! {
                _ = deadline.cancelled() => {}
                _ = updater.shutdown() => {}
            }
        }
        tokio::select! {
            _ = deadline.cancelled() => {}
            _ = self.diff_sync.shutdown() => {}
        }
        tokio::select! {
            _ = deadline.cancelled() => {}
            _ = self.spaces_sync.shutdown() => {}
        }
        tokio::select! {
            _ = deadline.cancelled() => {}
            _ = self.doc_host.shutdown_workers() => {}
        }
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

/// A fully assembled identity-scoped engine plus the relay handle whose lifetime
/// keeps this device reachable. Used by both the headless server and the headed
/// in-process engine so their production authentication paths cannot diverge.
pub struct EngineRuntime {
    core: EngineCore,
    host_relay: std::sync::Mutex<Option<hearth_rpc::HostRelay>>,
    hub: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// IPC-only lifecycle control owned by `hearth headless`. The regular
/// [`EngineRpc`] deliberately does not expose this method, so a viewport
/// attached to another headed process cannot shut down that process's engine.
struct HeadlessRpc {
    inner: Arc<dyn RpcService>,
    stop_tx: tokio::sync::mpsc::UnboundedSender<()>,
}

#[async_trait]
impl RpcService for HeadlessRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if method != methods::STOP_ENGINE {
            return self.inner.handle(method, params).await;
        }

        let stop_tx = self.stop_tx.clone();
        // Let the unary success frame reach the client before `Engine::run`
        // aborts the IPC server and drains the runtime.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = stop_tx.send(());
        });
        RpcReply::value(&serde_json::json!({ "ok": true }))
    }
}

impl EngineRuntime {
    pub fn core(&self) -> &EngineCore {
        &self.core
    }

    pub fn workspace_scope(&self) -> WorkspaceScope {
        self.core.workspace_scope()
    }

    pub fn disconnect_edge(&self) {
        // Revoke remote reachability before graceful draining. Sessions may
        // need time to settle; no authenticated relay RPC may enter during
        // that window after sign-out.
        self.host_relay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hub) = self
            .hub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            hub.abort();
        }
        self.core.disconnect_edge();
    }

    pub async fn shutdown(&self) {
        self.disconnect_edge();
        self.core.shutdown().await;
    }
}

impl Drop for EngineRuntime {
    fn drop(&mut self) {
        self.host_relay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hub) = self
            .hub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            hub.abort();
        }
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Resolve the shared dev/WorkOS auth configuration for headed and headless
    /// modes. A clean WorkOS boot deliberately avoids probing Edge: signed-out
    /// installations must be able to start locally without network access.
    pub async fn build_auth(config: &EngineConfig) -> Auth {
        let mut auth_config = AuthConfig::new(config.edge_url.clone(), config.data_dir.clone());
        auth_config.workos_client_id = config.workos_client_id.clone();
        if let Ok(base) = std::env::var("HEARTH_WORKOS_API_BASE")
            && !base.trim().is_empty()
        {
            auth_config.workos_api_base = base;
        }
        auth_config.callback_port = Some(
            std::env::var("HEARTH_CALLBACK_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(27641),
        );
        if let Some(token) = &config.edge_token {
            auth_config.dev_user_id = token.clone();
        }
        Auth::new(auth_config)
    }

    /// Capture the workspace boundary once, before refresh or sign-in can mutate auth.
    pub fn initial_workspace_scope(auth: &Auth) -> WorkspaceScope {
        if !auth.workos_enabled() {
            WorkspaceScope::Development
        } else if auth.loaded_workos_session() {
            WorkspaceScope::Synced
        } else {
            WorkspaceScope::Local
        }
    }

    /// Resolve a profile for the captured scope. A synced session without an
    /// organization returns `None` until onboarding selects one; it never falls
    /// through to the local or development profile.
    pub fn resolve_profile(
        config: &EngineConfig,
        auth: &Auth,
        scope: WorkspaceScope,
    ) -> Result<Option<EngineProfile>, EngineError> {
        match scope {
            WorkspaceScope::Local => EngineProfile::local(&config.data_dir).map(Some),
            WorkspaceScope::Development => {
                let dev_token_org = config
                    .edge_token
                    .as_deref()
                    .and_then(|token| token.split_once('@'))
                    .map(|(_, org)| org.to_string())
                    .filter(|org| !org.is_empty());
                let org_id = dev_token_org
                    .or(config.org_id.clone())
                    .unwrap_or_else(|| env_or("HEARTH_ORG_ID", DEFAULT_ORG_ID));
                let user_id = auth
                    .user_id()
                    .unwrap_or_else(|| env_or("HEARTH_USER_ID", DEFAULT_USER_ID));
                Ok(Some(EngineProfile::development(
                    &config.data_dir,
                    &org_id,
                    &user_id,
                )))
            }
            WorkspaceScope::Synced => {
                let state = auth.state();
                let Some(user) = state.user() else {
                    return Err(EngineError::Other(
                        "captured synced session no longer exposes its user identity".into(),
                    ));
                };
                let Some(org_id) = state.org_id() else {
                    return Ok(None);
                };
                Ok(Some(EngineProfile::synced(
                    &config.data_dir,
                    org_id,
                    &user.id,
                )))
            }
        }
    }

    /// Resolve the one-shot identity served before profile stores are available.
    pub fn engine_info(
        config: &EngineConfig,
        workspace_scope: WorkspaceScope,
    ) -> Result<EngineInfo, EngineError> {
        std::fs::create_dir_all(&config.data_dir)?;
        Ok(EngineInfo {
            device_id: load_or_create_device_id(&config.data_dir)?,
            workspace_scope,
        })
    }

    /// Open one already-resolved profile. Synced profiles always keep their
    /// Edge supervisors alive; temporary token or network failures are runtime
    /// states, not a reason to permanently assemble an offline engine.
    pub async fn assemble_runtime(
        config: &EngineConfig,
        auth: Auth,
        profile: EngineProfile,
    ) -> anyhow::Result<EngineRuntime> {
        Self::assemble_runtime_inner(config, auth, profile, None).await
    }

    /// Like [`Self::assemble_runtime`], but against an [`InstanceLock`] the
    /// caller already holds on the profile's device root (headed bootstrap
    /// acquires it before binding IPC).
    pub async fn assemble_runtime_with_lock(
        config: &EngineConfig,
        auth: Auth,
        profile: EngineProfile,
        lock: InstanceLock,
    ) -> anyhow::Result<EngineRuntime> {
        Self::assemble_runtime_inner(config, auth, profile, Some(lock)).await
    }

    async fn assemble_runtime_inner(
        config: &EngineConfig,
        auth: Auth,
        profile: EngineProfile,
        lock: Option<InstanceLock>,
    ) -> anyhow::Result<EngineRuntime> {
        let tailnet = config.tailnet_enabled();
        let edge_url = config
            .tailnet_http_url()
            .unwrap_or_else(|| config.edge_url.clone());
        let edge_enabled = match profile.scope() {
            WorkspaceScope::Local => tailnet,
            WorkspaceScope::Synced => {
                // Validate the persisted session in the BACKGROUND: the probe
                // still transitions auth to SignedOut on definitive revocation
                // (and warms the single-flight refresh every first dial waits
                // on), but assembly — and the viewport blocked on it — no
                // longer stalls on a WorkOS round trip that can take seconds
                // on a bad link. Everything shown at boot is local anyway.
                let auth_probe = auth.clone();
                tokio::spawn(async move {
                    let _ = auth_probe.access_token().await;
                });
                true
            }
            // Dev Auth always exposes `dev_user_id` as its synthetic access
            // token, including when WorkOS was merely disabled with
            // HEARTH_WORKOS_CLIENT_ID="". Only an explicitly configured,
            // non-empty bearer opts this runtime into Edge rooms and relays.
            WorkspaceScope::Development => {
                tailnet
                    || config
                        .edge_token
                        .as_deref()
                        .is_some_and(|token| !token.trim().is_empty())
            }
        };
        if edge_enabled {
            // OS network-path events (macOS NWPathMonitor): the instant the
            // path returns every parked reconnect backoff redials, and while
            // the OS says there is no path the dial loops park instead of
            // burning attempts. No-op on platforms without a monitor.
            hearth_sync::net_path::spawn_path_monitor();
        }
        let device_id = load_or_create_device_id(profile.device_root())?;
        let rooms_dir = profile.store_root().to_path_buf();
        let edge = edge_enabled.then(|| {
            EdgeConfig::new(edge_url.clone(), Arc::new(auth.clone())).with_device(device_id)
        });

        let core = match lock {
            Some(lock) => EngineCore::assemble_with_profile_locked(
                profile,
                Arc::new(default_registry()),
                config.default_harness,
                edge.clone(),
                lock,
            )?,
            None => EngineCore::assemble_with_profile(
                profile,
                Arc::new(default_registry()),
                config.default_harness,
                edge.clone(),
            )?,
        };
        core.set_auth(auth.clone());
        if edge_enabled {
            // Release checker: polls {edge}/releases on a 6h cadence; headless
            // installs with HEARTH_AUTO_UPDATE=1 apply + restart themselves — gated
            // on quiescence so a restart never lands under a live run or open PTY.
            let quiescent: hearth_update::QuiescentCheck = {
                let sessions = core.sessions.clone();
                let terminals = core.terminals.clone();
                Arc::new(move || !sessions.any_active() && !terminals.any_open())
            };
            let updater = hearth_update::Updater::spawn(edge_url.clone(), Some(quiescent));
            if let Some(mut token_changes) = edge.as_ref().and_then(EdgeConfig::token_changes) {
                let updater_for_tokens = updater.clone();
                let wake = tokio::spawn(async move {
                    while token_changes.changed().await.is_ok() {
                        updater_for_tokens.check_now();
                    }
                });
                core.set_updater_wake(wake);
            }
            core.set_updater(updater);
        }
        tracing::info!(device_id = %core.device_id, "engine core assembled");
        // Managed ACP adapters install in the background at boot (agents
        // whose CLI is present but whose adapter isn't yet), so a first chat
        // never waits on — or dies inside — an npm run.
        hearth_harness::acp::prewarm_managed_adapters();

        let mut host_relay = None;
        let mut hub = None;
        if tailnet {
            let port = config.tailnet_port;
            let workspace_for_url = core.workspace.clone();
            let mut link_config =
                hearth_rpc::LinkCacheConfig::new(edge_url.clone(), Arc::new(auth.clone()));
            let workspace_for_liveness = core.workspace.clone();
            link_config.liveness = Some(Arc::new(move |device_id: &str| {
                workspace_for_liveness.peer_liveness(device_id)
            }));
            link_config.peer_url = Some(Arc::new(move |device_id: &str| {
                let workspace = workspace_for_url.clone();
                let device_id = device_id.to_string();
                Box::pin(async move {
                    let Some(host) = resolve_tailnet_host(&workspace, &device_id).await else {
                        return Err(hearth_rpc::RpcError::Transport(format!(
                            "no Tailscale peer matches device {device_id} — rename the device \
                             to its Tailscale hostname (or MagicDNS first label) and retry"
                        )));
                    };
                    Ok(format!("ws://{host}:{port}/rpc"))
                })
            }));
            let links = hearth_rpc::LinkCache::new(link_config);
            let links_for_presence = links.clone();
            core.workspace
                .set_peer_alive_hook(Arc::new(move |device_id: &str| {
                    links_for_presence.reset_cooldown(device_id);
                }));
            core.set_links(links);

            let serve_rooms = config.tailnet_hub || this_device_is_hub(config).await;
            let service: Arc<dyn hearth_rpc::RpcService> = core.rpc_service();
            let on_rpc: hearth_sync::RpcHandler = Arc::new(move |ws| {
                let service = service.clone();
                tokio::spawn(hearth_rpc::serve_websocket(ws, service));
            });
            match hearth_sync::Hub::bind_port(
                port,
                hearth_sync::HubConfig {
                    data_dir: rooms_dir,
                    // Device-level releases dir: `hearth release publish`
                    // writes {data_dir}/releases, not the per-profile root.
                    releases_dir: config.data_dir.join("releases"),
                    serve_rooms,
                    on_rpc: Some(on_rpc),
                    skip_whois: false,
                },
            )
            .await
            {
                Ok(server) => {
                    tracing::info!(
                        addr = %server.local_addr(),
                        serve_rooms,
                        "tailnet hub listening"
                    );
                    hub = Some(server.spawn());
                }
                Err(err) if serve_rooms => {
                    return Err(anyhow::anyhow!(
                        "tailnet hub failed to bind (this process hosts rooms): {err}"
                    ));
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "tailnet /rpc listen failed — this device can still dial the hub"
                    );
                }
            }
        } else if let Some(edge) = edge.as_ref() {
            let mut link_config =
                hearth_rpc::LinkCacheConfig::new(edge.url.clone(), Arc::new(auth.clone()));
            // Registry-dark dial gate: devices with no recent presence fail
            // fast with zero dials; presence returning un-parks them (the
            // peer-alive hook below clears any cooldown at the same moment).
            let workspace_for_liveness = core.workspace.clone();
            link_config.liveness = Some(Arc::new(move |device_id: &str| {
                workspace_for_liveness.peer_liveness(device_id)
            }));
            let links = hearth_rpc::LinkCache::new(link_config);
            let links_for_presence = links.clone();
            core.workspace
                .set_peer_alive_hook(Arc::new(move |device_id: &str| {
                    links_for_presence.reset_cooldown(device_id);
                }));
            core.set_links(links);
            host_relay = Some(core.start_host_relay(&edge.url));
        }

        Ok(EngineRuntime {
            core,
            host_relay: std::sync::Mutex::new(host_relay),
            hub: std::sync::Mutex::new(hub),
        })
    }

    /// Run until ctrl-c: auth (dev or WorkOS), sessions engine + doc host + command
    /// executor, IPC server, and — when edge+auth are ready — the device-room host
    /// relay + peer link cache (targetDeviceId routing).
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        let auth = Self::build_auth(&config).await;
        let mut auth_state = auth.watch_state();
        let workspace_scope = Self::initial_workspace_scope(&auth);
        let mut profile = Self::resolve_profile(&config, &auth, workspace_scope)?;
        let _refresh_loop = auth.spawn_refresh_loop();

        // A captured cloud session without an organization must finish onboarding
        // before its profile can open. A clean signed-out install is local and never
        // enters the terminal sign-in flow.
        if workspace_scope == WorkspaceScope::Synced && profile.is_none() {
            terminal_sign_in(&auth).await?;
            profile = Self::resolve_profile(&config, &auth, workspace_scope)?;
        }
        let profile = profile
            .ok_or_else(|| EngineError::Other("synced workspace profile is not ready".into()))?;

        let runtime = Self::assemble_runtime(&config, auth, profile).await?;

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel();
        let service: Arc<dyn RpcService> = Arc::new(HeadlessRpc {
            inner: runtime.core().rpc_service(),
            stop_tx,
        });
        let server = serve_ipc(config.ipc_port, service).await?;

        tokio::select! {
            result = shutdown_signal() => result?,
            requested = stop_rx.recv() => {
                if requested.is_some() {
                    tracing::info!("headless shutdown requested over IPC");
                }
            }
            _ = wait_for_signed_out(&mut auth_state), if workspace_scope == WorkspaceScope::Synced => {
                // Edge transports observe the same auth signal and close at
                // once. Leave a brief reply window for a SignOut RPC before
                // the localhost server itself is aborted.
                runtime.disconnect_edge();
                tracing::info!("headless authentication revoked; stopping synced runtime");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        tracing::info!("shutting down");
        server.abort();
        runtime.shutdown().await;
        Ok(())
    }
}

async fn wait_for_signed_out(state: &mut tokio::sync::watch::Receiver<AuthState>) {
    loop {
        if matches!(&*state.borrow(), AuthState::SignedOut) {
            return;
        }
        if state.changed().await.is_err() {
            return;
        }
    }
}

/// Ctrl-C or SIGTERM. systemd/launchd stop (and the auto-updater's service
/// restart) deliver SIGTERM — without catching it the daemon dies mid-write
/// and every stop takes the crash-recovery path instead of the graceful drain.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Serve the typed RPC on the localhost IPC port.
///
/// Both engines call this: the headless daemon, and the headed app's embedded
/// engine. That second case is the point — an embedded engine that keeps the
/// port to itself forces anyone wanting a second viewport (the terminal app) to
/// stop the desktop app, start a daemon, and start it again in the right order.
/// Serving here means any viewport can just attach.
///
/// Localhost only, exactly as before: this widens *which process* can serve the
/// port, not who can reach it.
pub async fn serve_ipc(
    port: u16,
    service: std::sync::Arc<dyn hearth_rpc::RpcService>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "IPC server listening");
    Ok(tokio::spawn(hearth_rpc::serve_ws_listener(
        listener, service,
    )))
}

/// Block until the WorkOS session is signed in AND org-scoped. On a TTY, print the
/// headless (paste-code) sign-in URL, read the pasted `state.code` from stdin, and
/// run workspace onboarding (create / auto-join / numbered picker). Off a TTY this
/// errors immediately — a daemon under systemd/launchd must load the session that
/// `hearth login` persisted, never wait on a prompt nobody can see.
pub async fn terminal_sign_in(auth: &Auth) -> Result<(), EngineError> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    let mut state_rx = auth.watch_state();
    let mut stdin_reader: Option<tokio::task::JoinHandle<()>> = None;
    let mut org_reader: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let state = state_rx.borrow().clone();
        match state {
            AuthState::SignedIn { user, org_id } => {
                tracing::info!(email = %user.email, org = org_id.as_deref().unwrap_or("<none>"),
                    "auth: session ready");
                break;
            }
            AuthState::NeedsOrganization { user } => {
                if !interactive {
                    // No reader tasks have been spawned on this path (both spawns
                    // are TTY-gated), so an early return leaks nothing.
                    return Err(EngineError::Other(format!(
                        "signed in as {} but no workspace is selected — run `hearth login` on this machine to pick one",
                        user.email
                    )));
                }
                if org_reader.is_none() {
                    // Workspace onboarding on the TTY (old hearth's
                    // `backend login` flow): create if none, auto-join a
                    // single membership, numbered picker otherwise.
                    println!("Signed in as {}.", user.email);
                    org_reader = Some(tokio::spawn(run_org_onboarding(auth.clone())));
                }
            }
            AuthState::SignedOut => {
                if !interactive {
                    return Err(EngineError::Other(
                        "not signed in — run `hearth login` on this machine first".into(),
                    ));
                }
                if stdin_reader.is_none() {
                    let url = auth.start_headless_sign_in();
                    println!("Sign in to Hearth:\n\n  {url}\n");
                    println!("Then paste the code shown in the browser here and press enter.");
                    let auth = auth.clone();
                    stdin_reader = Some(tokio::spawn(async move {
                        loop {
                            let Some(line) = read_stdin_line().await else {
                                return;
                            };
                            let pasted = line.trim();
                            if pasted.is_empty() {
                                continue;
                            }
                            match auth.complete_sign_in(pasted).await {
                                Ok(()) => return,
                                Err(err) => println!("Sign-in failed: {err}"),
                            }
                        }
                    }));
                }
            }
        }
        if state_rx.changed().await.is_err() {
            break;
        }
    }
    if let Some(reader) = stdin_reader {
        reader.abort();
    }
    if let Some(reader) = org_reader {
        reader.abort();
    }
    Ok(())
}

/// One line from stdin (blocking read off the runtime). `None` = stdin closed.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None, // EOF / error
            Ok(_) => Some(line),
        }
    })
    .await
    .ok()
    .flatten()
}

/// TTY workspace onboarding for an org-less session (ports old hearth's
/// `backend login` flow): no memberships → prompt a name and create; exactly
/// one → auto-join; several → numbered picker. Success flips the auth state to
/// `SignedIn`, which ends [`wait_for_sign_in`]'s wait (and aborts this task).
async fn run_org_onboarding(auth: Auth) {
    let orgs = match auth.list_orgs().await {
        Ok(orgs) => orgs,
        Err(err) => {
            println!(
                "Could not list workspaces ({err}) — create or select one from the Hearth UI to continue."
            );
            return;
        }
    };
    match orgs.len() {
        0 => {
            println!("No workspaces yet — name your new workspace and press enter:");
            loop {
                let Some(line) = read_stdin_line().await else {
                    return;
                };
                let name = line.trim();
                if name.is_empty() {
                    continue;
                }
                match auth.create_org(name).await {
                    Ok(()) => return,
                    Err(err) => println!("Creating workspace failed: {err}"),
                }
            }
        }
        1 => {
            let only = &orgs[0];
            println!("Joining workspace \"{}\"…", only.name);
            if let Err(err) = auth.select_org(&only.organization_id).await {
                println!("Joining workspace failed: {err}");
            }
        }
        _ => {
            println!("\nYour workspaces:");
            for (index, org) in orgs.iter().enumerate() {
                println!("  {}. {}", index + 1, org.name);
            }
            println!("Pick a workspace [1-{}]:", orgs.len());
            loop {
                let Some(line) = read_stdin_line().await else {
                    return;
                };
                let choice = line
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|index| orgs.get(index));
                let Some(org) = choice else {
                    println!("Pick a workspace [1-{}]:", orgs.len());
                    continue;
                };
                match auth.select_org(&org.organization_id).await {
                    Ok(()) => return,
                    Err(err) => println!("Joining workspace failed: {err}"),
                }
            }
        }
    }
}

/// Best-effort human name for this device's registry row.
fn local_device_name(device_id: &str) -> String {
    select_local_device_name(
        [
            std::env::var("HEARTH_DEVICE_NAME").ok(),
            native_friendly_device_name(),
            std::env::var("HOSTNAME").ok(),
            gethostname::gethostname().into_string().ok(),
            std::fs::read_to_string("/etc/hostname").ok(),
        ],
        device_id,
        std::env::consts::OS,
    )
}

fn select_local_device_name(
    candidates: impl IntoIterator<Item = Option<String>>,
    device_id: &str,
    platform: &str,
) -> String {
    candidates
        .into_iter()
        .flatten()
        .map(|name| name.trim().to_string())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| {
            let platform = match platform {
                "macos" => "macOS",
                "windows" => "Windows",
                "linux" => "Linux",
                _ => "Local",
            };
            let short_id: String = device_id.chars().take(8).collect();
            format!("{platform} device {short_id}")
        })
}

#[cfg(target_os = "macos")]
fn native_friendly_device_name() -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn native_friendly_device_name() -> Option<String> {
    #[cfg(target_os = "windows")]
    return std::env::var("COMPUTERNAME").ok();

    #[cfg(not(target_os = "windows"))]
    None
}

/// Trimmed env var or the given default.
async fn this_device_is_hub(config: &EngineConfig) -> bool {
    if config.tailnet_hub {
        return true;
    }
    let Some(want) = config.tailnet_host.as_deref() else {
        return false;
    };
    match hearth_sync::tailnet::discover_self().await {
        Ok(me) => host_matches(&me, want),
        Err(err) => {
            tracing::warn!(error = %err, "tailnet: could not read Self; not hosting rooms");
            false
        }
    }
}

fn dns_first_label(host: &str) -> &str {
    host.trim_end_matches('.').split('.').next().unwrap_or(host)
}

/// True when `configured` names this Tailscale peer: exact MagicDNS / hostname,
/// or first-label equality (`minis` ↔ `minis.tailnet.ts.net`). Prefix matches
/// like `mac` ↔ `macbook…` are rejected.
fn host_matches(peer: &hearth_sync::tailnet::Peer, configured: &str) -> bool {
    let want = configured.trim().trim_end_matches('.').to_ascii_lowercase();
    if want.is_empty() {
        return false;
    }
    let dns = peer.dns_host().to_ascii_lowercase();
    let host = peer.host_name.to_ascii_lowercase();
    dns == want || host == want || dns_first_label(&dns) == want || dns_first_label(&want) == host
}

/// Resolve a registry device id to a Tailscale dial host. Returns `None` when
/// the device's friendly name does not match any peer — callers must not fall
/// back to the hub (that would run peer RPC on the wrong machine).
async fn resolve_tailnet_host(workspace: &WorkspaceHost, device_id: &str) -> Option<String> {
    let name = workspace
        .read_devices()
        .ok()
        .and_then(|devs| devs.into_iter().find(|d| d.id == device_id).map(|d| d.name))?;
    let peers = hearth_sync::tailnet::discover_peers().await.ok()?;
    peers
        .iter()
        .find(|p| host_matches(p, &name))
        .map(|p| p.dns_host().to_string())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    // EngineInfo is resolved before the lifetime InstanceLock is acquired, so
    // identity creation and legacy repair need their own short critical section.
    // The OS releases this lock after a crash; unlike a create_new lockfile it
    // cannot strand an installation permanently.
    let _identity_lock = DeviceIdentityLock::acquire(data_dir)?;
    let path = data_dir.join("device-id");
    let recovering_empty = match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_string()),
        // Older releases used truncate+write. A crash between those operations
        // left a zero-byte file that is safe to replace with a fresh identity.
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };

    let id = new_id();
    let temp_path = data_dir.join(format!(
        ".device-id.tmp-{}-{}",
        std::process::id(),
        new_id()
    ));
    let write_result = (|| -> Result<(), EngineError> {
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp.write_all(id.as_bytes())?;
        temp.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }

    // Fresh installs use create-if-absent. Legacy empty files need an atomic
    // same-directory replacement on Unix; the Windows fallback runs under the
    // identity lock and remains recoverable if interrupted.
    let publish_result = if recovering_empty {
        match std::fs::read_to_string(&path) {
            Ok(id) if !id.trim().is_empty() => {
                let _ = std::fs::remove_file(&temp_path);
                return Ok(id.trim().to_string());
            }
            Ok(_) => replace_empty_device_id(&temp_path, &path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::hard_link(&temp_path, &path)
            }
            Err(err) => Err(err),
        }
    } else {
        std::fs::hard_link(&temp_path, &path)
    };
    let _ = std::fs::remove_file(&temp_path);
    match publish_result {
        Ok(()) => Ok(id),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let winner = std::fs::read_to_string(&path)?;
            if winner.trim().is_empty() {
                Err(EngineError::Other(format!(
                    "invalid device identity {}: file is empty",
                    path.display()
                )))
            } else {
                Ok(winner.trim().to_string())
            }
        }
        Err(err) => Err(err.into()),
    }
}

struct DeviceIdentityLock {
    _file: std::fs::File,
}

impl DeviceIdentityLock {
    fn acquire(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join("device-id.lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
            let mut retries = 200;
            let file = loop {
                match options.open(&path) {
                    Ok(file) => break file,
                    Err(err)
                        if err.kind() == std::io::ErrorKind::PermissionDenied && retries > 0 =>
                    {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(err) => return Err(err.into()),
                }
            };
            return Ok(Self { _file: file });
        }

        #[cfg(not(windows))]
        let file = options.open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINTR) {
                    return Err(err.into());
                }
            }
        }

        #[cfg(not(windows))]
        Ok(Self { _file: file })
    }
}

fn replace_empty_device_id(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(temp_path, path)
    }
    #[cfg(not(unix))]
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        std::fs::hard_link(temp_path, path)
    }
}

#[cfg(test)]
mod host_match_tests {
    use super::{dns_first_label, host_matches};
    use hearth_sync::tailnet::Peer;

    fn peer(host_name: &str, dns_name: &str) -> Peer {
        Peer {
            id: "p".into(),
            host_name: host_name.into(),
            dns_name: dns_name.into(),
            tailscale_ips: vec![],
            os: "linux".into(),
            online: true,
            user_id: 0,
            login_name: String::new(),
        }
    }

    #[test]
    fn hub_autodetect_matches_short_and_magicdns() {
        let me = peer("minis", "minis.tailnet.ts.net.");
        assert!(host_matches(&me, "minis"));
        assert!(host_matches(&me, "minis.tailnet.ts.net"));
        assert!(host_matches(&me, "minis.tailnet.ts.net."));
    }

    #[test]
    fn hub_autodetect_rejects_hostname_prefix_collisions() {
        let spoke = peer("mac", "mac.tailnet.ts.net.");
        assert!(!host_matches(&spoke, "macbook.tailnet.ts.net"));
        assert!(!host_matches(&spoke, "macbook"));
        let book = peer("macbook", "macbook.tailnet.ts.net.");
        assert!(!host_matches(&book, "mac"));
        assert!(!host_matches(&book, "mac.tailnet.ts.net"));
    }

    #[test]
    fn dns_first_label_strips_suffix() {
        assert_eq!(dns_first_label("minis.tailnet.ts.net"), "minis");
        assert_eq!(dns_first_label("minis."), "minis");
        assert_eq!(dns_first_label("minis"), "minis");
    }
}

#[cfg(test)]
mod device_name_tests {
    use super::select_local_device_name;

    fn name(candidates: &[Option<&str>], device_id: &str, platform: &str) -> String {
        select_local_device_name(
            candidates
                .iter()
                .map(|candidate| candidate.map(str::to_string)),
            device_id,
            platform,
        )
    }

    #[test]
    fn explicit_override_wins_and_is_trimmed() {
        assert_eq!(
            name(
                &[Some("  Studio Mac  "), Some("system-host")],
                "17bc0aa2-rest",
                "macos"
            ),
            "Studio Mac"
        );
    }

    #[test]
    fn native_friendly_name_wins_over_hostnames() {
        assert_eq!(
            name(
                &[
                    None,
                    Some("MacBook Pro de Jose"),
                    None,
                    Some("MacBook-Pro.local"),
                ],
                "17bc0aa2-rest",
                "macos"
            ),
            "MacBook Pro de Jose"
        );
    }

    #[test]
    fn windows_computer_name_is_used_when_present() {
        assert_eq!(
            name(
                &[None, Some("DESKTOP-123"), Some("shell-host")],
                "17bc0aa2-rest",
                "windows"
            ),
            "DESKTOP-123"
        );
    }

    #[test]
    fn blank_candidates_are_ignored() {
        assert_eq!(
            name(
                &[Some("  "), None, Some("\n"), Some("linux-box")],
                "17bc0aa2-rest",
                "linux"
            ),
            "linux-box"
        );
    }

    #[test]
    fn final_fallback_is_platform_specific_and_distinct() {
        assert_eq!(
            name(&[None, Some(" ")], "17bc0aa2-rest", "linux"),
            "Linux device 17bc0aa2"
        );
    }
}
