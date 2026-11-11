//! Engine connection: probe the localhost IPC port and connect to a daemon, or
//! embed an engine in-process. Mirrors the gpui UI's `EngineHandle`, but hands
//! back an [`Arc<RpcClient>`] so the Rune bridge can share one client across
//! every watch task (gpui's `Entity`-scoped pumps each held their own handle).

use std::sync::Arc;

use async_trait::async_trait;
use hearth_engine::{
    Engine, EngineConfig, EngineRuntime, InstanceLock, serve_ipc, DEFAULT_TAILNET_PORT,
};
use hearth_proto::{EngineInfo, WorkspaceScope};
use hearth_rpc::{RpcClient, RpcError, RpcService, connect_ws, memory_client, methods};
use serde::de::DeserializeOwned;

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.hearth`).
    pub data_dir: std::path::PathBuf,
    /// Localhost IPC port to probe / serve.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs offline.
    pub edge_token: Option<String>,
    /// Workspace org override for explicit dev-mode runs.
    pub org_id: Option<String>,
    /// Harness for doc-command runs until per-chat config lands.
    pub default_harness: hearth_proto::HarnessId,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// One of the two ways to own an engine connection.
#[async_trait]
pub trait EngineBackend: Send + Sync {
    fn mode(&self) -> EngineMode;
    /// Graceful teardown (drains runs / flushes docs for the in-process engine).
    async fn shutdown(&self);
}

/// Embedded engine: owns the [`EngineRuntime`] and an in-memory RPC loop.
struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    /// Serves this engine to other viewports over the IPC port. `None` when the
    /// port was already taken.
    ipc_task: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }

    async fn shutdown(&self) {
        // Stop accepting first: a viewport must not connect midway through the
        // drain and queue work against stores that are closing.
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
    }
}

/// External daemon over `ws://127.0.0.1:{port}`.
struct RemoteEngine {
    url: String,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }

    async fn shutdown(&self) {
        // The daemon outlives this viewport.
    }
}

/// Cheaply clonable handle to whichever backend won the probe.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
    /// Shared client for every watch pump.
    pub client: Arc<RpcClient>,
    pub info: EngineInfo,
}

impl EngineHandle {
    /// Probe the IPC port and connect (daemon listening) or embed (nothing there).
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        // Invariant: at most one bootstrap in this process runs probe+embed at
        // a time. The winner binds its IPC listener before releasing the gate.
        static BOOTSTRAP_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _gate = BOOTSTRAP_GATE.lock().await;

        if let Some(handle) = Self::attach_to_daemon(config.ipc_port).await {
            return Ok(handle);
        }

        tracing::info!(data_dir = %config.data_dir.display(), "no daemon on port; embedding engine");
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            edge_url: config.edge_url,
            edge_token: config.edge_token,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
            org_id: config.org_id,
            tailnet_host: std::env::var("HEARTH_TAILNET_HOST")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            tailnet_port: std::env::var("HEARTH_TAILNET_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(DEFAULT_TAILNET_PORT),
            tailnet_hub: matches!(
                std::env::var("HEARTH_TAILNET_HUB").as_deref(),
                Ok("1") | Ok("true") | Ok("yes")
            ),
        };

        std::fs::create_dir_all(&engine_config.data_dir)?;
        let lock_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut dial_timed_out = false;
        let lock = loop {
            match InstanceLock::acquire(&engine_config.data_dir) {
                Ok(lock) => break lock,
                Err(err) => {
                    if std::time::Instant::now() >= lock_deadline {
                        if dial_timed_out
                            && let Some(pid) = InstanceLock::holder(&engine_config.data_dir)
                        {
                            return Err(anyhow::anyhow!(
                                "engine (pid {pid}) appears wedged — it holds the data dir \
                                 and is not completing IPC handshakes. Run: \
                                 systemctl --user restart hearth"
                            ));
                        }
                        return Err(err.into());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    match Self::attach_to_daemon(engine_config.ipc_port).await {
                        Some(handle) => return Ok(handle),
                        None if listener_refused(engine_config.ipc_port) => {}
                        None => dial_timed_out = true,
                    }
                }
            }
        };

        let auth = Engine::build_auth(&engine_config);
        let workspace_scope = Engine::initial_workspace_scope(&auth);
        let profile = Engine::resolve_profile(&engine_config, &auth, workspace_scope)?;
        let engine_info = Engine::engine_info(&engine_config, workspace_scope)?;
        let runtime =
            Engine::assemble_runtime_with_lock(&engine_config, auth, profile, lock).await?;
        let service: Arc<dyn RpcService> = runtime.core().rpc_service();
        let client = Arc::new(memory_client(service.clone()));

        let ipc_task = match serve_ipc(engine_config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        Ok(EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime: Arc::new(tokio::sync::Mutex::new(Some(runtime))),
                ipc_task,
            }),
            client,
            info: engine_info,
        })
    }

    /// Probe the IPC port and, if a live engine answers, attach as a remote
    /// viewport. `None` means embed.
    async fn attach_to_daemon(ipc_port: u16) -> Option<EngineHandle> {
        let url = format!("ws://127.0.0.1:{ipc_port}");
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)),
        )
        .await;
        if !matches!(probe, Ok(Ok(_))) {
            return None;
        }
        tracing::info!(%url, "engine daemon detected; connecting");
        let handshake =
            tokio::time::timeout(std::time::Duration::from_secs(8), connect_ws(&url)).await;
        let client = match handshake {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => {
                tracing::warn!(%url, error = %err, "not an engine; embedding instead");
                return None;
            }
            Err(_) => {
                tracing::warn!(%url, "engine handshake timed out; treating the listener as wedged");
                return None;
            }
        };
        let engine_info = match query_engine_info(&client).await {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!(%url, error = %err, "listener did not provide engine identity; embedding instead");
                return None;
            }
        };
        match client
            .call(methods::ENGINE_READY, serde_json::json!({}))
            .await
        {
            Ok(_) => {}
            Err(RpcError::UnknownMethod(method)) if method == methods::ENGINE_READY => {}
            Err(err) => {
                tracing::warn!(%url, error = %err, "engine is not ready; embedding instead");
                return None;
            }
        }
        Some(EngineHandle {
            inner: Arc::new(RemoteEngine { url }),
            client: Arc::new(client),
            info: engine_info,
        })
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

/// Distinguish "the engine refused to be attached" from "a listener is there
/// but the engine is not completing handshakes."
fn listener_refused(ipc_port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], ipc_port)),
        std::time::Duration::from_millis(300),
    )
    .is_err()
}

/// Query the current protocol first, with a conservative fallback for daemons
/// from before `EngineInfo` existed.
async fn query_engine_info(client: &RpcClient) -> Result<EngineInfo, RpcError> {
    match client
        .call_as::<EngineInfo>(methods::ENGINE_INFO, serde_json::json!({}))
        .await
    {
        Ok(info) => Ok(info),
        Err(RpcError::UnknownMethod(method)) if method == methods::ENGINE_INFO => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct LocalDevice {
                device_id: String,
            }
            let legacy: LocalDevice = client
                .call_as(methods::LOCAL_DEVICE, serde_json::json!({}))
                .await?;
            Ok(EngineInfo {
                device_id: legacy.device_id,
                workspace_scope: WorkspaceScope::Development,
            })
        }
        Err(err) => Err(err),
    }
}

/// Deserialize a typed RPC response, logging and returning `None` on a shape
/// mismatch (the engine may be older than this UI).
pub async fn call_as<T: DeserializeOwned>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
) -> Option<T> {
    match client.call_as::<T>(method, params).await {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::warn!(%method, error = %err, "rpc call failed");
            None
        }
    }
}
