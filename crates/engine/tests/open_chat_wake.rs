//! Tailscale cold-host wake: a sender wakes a remote host via peer
//! [`hearth_rpc::methods::OPEN_CHAT`] (direct `/rpc`), with no DeviceRoom and
//! no `{edge}/device/{id}/nudge` HTTP route — matching production Tailscale
//! assembly where `HostRelay` is never started.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use async_trait::async_trait;

use hearth_doc::{MessageRole, MessageStatus, SessionCommandPayload};
use hearth_engine::{EdgeConfig, EngineCore, HarnessId, HarnessRegistry};
use hearth_harness::{Harness, HarnessError, RunControls};
use hearth_proto::{
    AgentEvent, Device, DoneStatus, Model, ReasoningLevel, RunRequest, SandboxLevel, SteeringMode,
};
use hearth_rpc::{LinkCache, LinkCacheConfig, StaticToken, methods};
use hearth_sync::hub::{Hub, HubConfig};

const CHAT: &str = "chat-openchat-wake";

struct InstantHarness;

#[async_trait]
impl Harness for InstantHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Instant"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "instant-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-openchat".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "opened via OpenChat".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-openchat".into()),
            }),
        ])
        .boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(InstantHarness));
    Arc::new(registry)
}

fn assemble(dir: &std::path::Path, device_id: &str, hub: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let edge = Some(EdgeConfig::with_static_token(hub, "tailnet").with_device(device_id));
    EngineCore::assemble_with_identity(
        dir,
        registry(),
        HarnessId::Mock,
        edge,
        "wake-org",
        "alice",
    )
    .expect("engine assembles")
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_host_wakes_via_peer_open_chat_without_device_room() {
    let rooms = tempfile::tempdir().unwrap();
    // Shared hub: chat2 + registry only (no DeviceRoom /device nudge).
    let hub = Hub::bind(
        "127.0.0.1:0",
        HubConfig {
            data_dir: rooms.path().to_path_buf(),
            serve_rooms: true,
            on_rpc: None,
            skip_whois: true,
        },
    )
    .await
    .unwrap();
    let hub_url = format!("http://127.0.0.1:{}", hub.local_addr().port());
    let _hub_task = hub.spawn();

    let dirs = tempfile::tempdir().unwrap();
    let core_b = assemble(&dirs.path().join("b"), "device-b", &hub_url);
    let core_a = assemble(&dirs.path().join("a"), "device-a", &hub_url);

    // B serves direct /rpc (spoke-style bind: rooms off, RPC on).
    let service_b: Arc<dyn hearth_rpc::RpcService> = core_b.rpc_service();
    let on_rpc = Arc::new(move |ws| {
        let service = service_b.clone();
        tokio::spawn(hearth_rpc::serve_websocket(ws, service));
    });
    let rpc_hub = Hub::bind(
        "127.0.0.1:0",
        HubConfig {
            data_dir: dirs.path().join("b-rpc-rooms"),
            serve_rooms: false,
            on_rpc: Some(on_rpc),
            skip_whois: true,
        },
    )
    .await
    .unwrap();
    let b_rpc_port = rpc_hub.local_addr().port();
    let _b_rpc_task = rpc_hub.spawn();

    // A dials B over plain peer /rpc — never DeviceRoom.
    let mut link_config =
        LinkCacheConfig::new(hub_url.clone(), Arc::new(StaticToken("tailnet".into())));
    link_config.probe_timeout = Duration::from_secs(5);
    link_config.peer_url = Some(Arc::new(move |device_id: &str| {
        let device_id = device_id.to_string();
        let port = b_rpc_port;
        Box::pin(async move {
            if device_id != "device-b" {
                return Err(hearth_rpc::RpcError::Transport(format!(
                    "unknown peer {device_id}"
                )));
            }
            Ok(format!("ws://127.0.0.1:{port}/rpc"))
        })
    }));
    core_a.set_links(LinkCache::new(link_config));

    core_a.workspace.upsert_device_row(&Device {
        id: "device-b".into(),
        name: "b".into(),
        platform: "linux".into(),
        last_seen_at: Some(chrono::Utc::now()),
        created_at: None,
        version: Some("0.1.0".into()),
    });

    let client_a = hearth_rpc::memory_client(core_a.rpc_service());
    client_a
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": CHAT, "deviceId": "device-b" }),
        )
        .await
        .expect("createChat on A");

    // Ensure B has the hosted chat row locally (registry may lag in the test).
    core_b.workspace.upsert_device_row(&Device {
        id: "device-b".into(),
        name: "b".into(),
        platform: "linux".into(),
        last_seen_at: Some(chrono::Utc::now()),
        created_at: None,
        version: Some("0.1.0".into()),
    });
    if core_b.workspace.chat(CHAT).ok().flatten().is_none() {
        let client_b = hearth_rpc::memory_client(core_b.rpc_service());
        client_b
            .call(
                methods::MUTATE,
                serde_json::json!({ "op": "createChat", "chatId": CHAT, "deviceId": "device-b" }),
            )
            .await
            .expect("createChat on B");
    }

    let command = serde_json::to_value(SessionCommandPayload::Run {
        request: RunRequest {
            mode: None,
            prompt: "wake the host".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "~".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            worktree: None,
            resume: None,
        },
        message_id: "msg-openchat-1".into(),
    })
    .expect("command json");
    client_a
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({ "chatId": CHAT, "command": command }),
        )
        .await
        .expect("queue on A");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if complete_assistant_count(&core_b) == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "OpenChat wake never executed the command on B"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let entries = core_b
        .doc_host
        .open(CHAT)
        .expect("open on B")
        .doc()
        .read_entries()
        .expect("read entries");
    assert!(
        entries
            .iter()
            .any(|e| e.id == "msg-openchat-1" && e.role == MessageRole::User),
        "B persisted the user message under the client-minted id"
    );

    core_a.shutdown().await;
    core_b.shutdown().await;
}
