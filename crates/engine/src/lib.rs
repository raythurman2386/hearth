//! hearth-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use hearth_proto::{EngineInfo, HarnessId, WorkspaceScope};
use hearth_rpc::RpcService;

use hearth_sync::DocsStore;

pub mod agent_accounts;
pub mod auth;
pub mod change_requests;
pub mod chat2_host;
mod daemon;
mod device_identity;
mod device_name;
pub mod diff_sync;
pub mod doc_host;
pub mod instance_lock;
pub mod profile;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod source_control;
pub mod spaces;
mod tailnet;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{Auth, AuthConfig, AuthState, AuthUser};
pub use change_requests::CheckoutChangeRequests;
pub use daemon::serve_ipc;
use daemon::{HeadlessRpc, shutdown_signal};
use device_identity::load_or_create_device_id;
use device_name::local_device_name;
pub use diff_sync::{
    CheckoutDiffSync, capture_commit_diff, capture_diff, capture_diff_against, capture_turn_diff,
    merge_base, read_diff_file_text, snapshot_tree, working_diff_base,
};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use profile::EngineProfile;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{SessionsEngine, SteerOutcome};
pub use source_control::{
    BranchHeadContext, ChangeRequestError, ChangeRequestProvider, ChangeRequestResolution,
    ChangeRequestResolver, CheckoutChangeRequestLookup, CheckoutSourceContext, GitHubCli,
    GitRemote, parse_git_remote,
};
pub use spaces::SpacesSync;
use tailnet::{resolve_tailnet_host, this_device_is_hub};
pub use terminals::Terminals;
pub(crate) use titles::TitleGenerator;
pub use uploads::Uploads;
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
    /// Explicit bearer for hub room joins (the tailnet dummy bearer). This
    /// runtime stays offline when it is absent and no tailnet host is set.
    pub edge_token: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Workspace-doc org (`ws/{orgId}` room). `None` = `$HEARTH_ORG_ID` or the dev default.
    pub org_id: Option<String>,
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
        let profile = EngineProfile::development(data_dir, org_id, user_id);
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
        // Queued-attachment support: the doc host resolves `pending://` refs
        // against this store and pushes staged bytes to remote hosts.
        doc_host.set_uploads(uploads.clone());
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

    /// The attached auth service, or a lazily-created one (in-process embeds
    /// that never wired auth still answer AuthStatus honestly).
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
            let mut config = AuthConfig::new();
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
        // stopped runtime must not keep polling releases or draining the
        // attachment outbox after shutdown returns.
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
        // that window after shutdown begins.
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

    /// Resolve the fixed device identity for headed and headless modes. There is
    /// no account service to probe: a runtime never reaches the network during
    /// construction, so offline installs start exactly like tailnet ones.
    pub fn build_auth(config: &EngineConfig) -> Auth {
        let mut auth_config = AuthConfig::new();
        if let Some(token) = &config.edge_token {
            auth_config.dev_user_id = token.clone();
        }
        Auth::new(auth_config)
    }

    /// The fixed workspace boundary every runtime uses. The device is its own
    /// identity; the profile never changes while a runtime is alive.
    pub fn initial_workspace_scope(_auth: &Auth) -> WorkspaceScope {
        WorkspaceScope::Development
    }

    /// Resolve the profile for the captured scope.
    pub fn resolve_profile(
        config: &EngineConfig,
        auth: &Auth,
        scope: WorkspaceScope,
    ) -> Result<EngineProfile, EngineError> {
        match scope {
            WorkspaceScope::Local => EngineProfile::local(&config.data_dir),
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
                Ok(EngineProfile::development(
                    &config.data_dir,
                    &org_id,
                    &user_id,
                ))
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

    /// Open one already-resolved profile. Online runtimes always keep their
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
            // The device bearer is always present; only an explicitly
            // configured, non-empty bearer or a tailnet host opts this runtime
            // into hub rooms and relays. A bare offline install stays offline.
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

    /// Run until ctrl-c: sessions engine + doc host + command executor, IPC
    /// server, and — when the runtime is online — the device-room host relay
    /// plus peer link cache (targetDeviceId routing).
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        let auth = Self::build_auth(&config);
        let workspace_scope = Self::initial_workspace_scope(&auth);
        let profile = Self::resolve_profile(&config, &auth, workspace_scope)?;

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
        }
        tracing::info!("shutting down");
        server.abort();
        runtime.shutdown().await;
        Ok(())
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}
