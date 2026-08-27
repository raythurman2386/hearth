//! The reported failure shape: a viewer engine is "closed and reopened" while
//! a session is running on the host. The viewer must converge on the chat2
//! transcript after reopen — a stale cursor, a missed roomGen flip, or a
//! checkpoint that a reconnected cursor skips would leave the chat frozen
//! until an app restart (the exact "sync fails and the chat never updates"
//! report).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use hearth_doc::{MessageRole, MessageStatus, SessionCommandPayload};
use hearth_engine::{EdgeConfig, EngineCore, HarnessRegistry};
use hearth_harness::{Harness, HarnessError, RunControls};
use hearth_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};
use hearth_sync::hub::{Hub, HubConfig};

const CHAT: &str = "chat-reopen-converges";

struct OneLinerHarness;

#[async_trait]
impl Harness for OneLinerHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "OneLiner"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let events: Vec<Result<AgentEvent, HarnessError>> = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "sess-reopen".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "the codeword is PINEAPPLE".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-reopen".into()),
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(OneLinerHarness));
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
        "reopen-org",
        "alice",
    )
    .expect("engine core assembles")
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until the engine instance lock on `dir` is free (the prior engine's
/// graph has dropped). Tests run multiple engines over one process, and the
/// reassemble races the teardown's lock release otherwise.
fn wait_lock_released(dir: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while hearth_engine::InstanceLock::holder(dir).is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "engine instance lock on {} never released",
            dir.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn complete_assistant(core: &EngineCore) -> usize {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

fn user_entries(core: &EngineCore) -> Vec<String> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.role == MessageRole::User)
        .map(|e| e.id)
        .collect()
}

/// Viewer B (the "client") is closed and reopened (fresh `EngineCore` over the
/// same data dir) while the host A runs a session. B must converge on the
/// full transcript after reopen and keep receiving new messages written
/// after the reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_reopen_converges_on_host_written_transcript() {
    let rooms = tempfile::tempdir().unwrap();
    let hub = Hub::bind(
        "127.0.0.1:0",
        HubConfig {
            data_dir: rooms.path().to_path_buf(),
            releases_dir: rooms.path().join("releases"),
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

    // Phase 1: host A creates + hosts the chat, viewer B opens it (so a chat2
    // snapshot exists on B), then A runs one full turn.
    let a = assemble(&dirs.path().join("a"), "device-a", &hub_url);
    let b = assemble(&dirs.path().join("b"), "device-b", &hub_url);

    let client_a = hearth_rpc::memory_client(a.rpc_service());
    client_a
        .call(
            hearth_rpc::methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": CHAT,
                "deviceId": "device-a",
            }),
        )
        .await
        .expect("createChat on A");
    a.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("pre-title");

    // B opens the chat before the turn so it holds a chat2 handle + snapshot.
    let _b_open = b.doc_host.open(CHAT).expect("B opens chat");

    // A queues a run and completes it; B observes the assistant entry.
    let run_req = RunRequest {
        mode: None,
        prompt: "first turn".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    a.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: run_req,
                message_id: "msg-1".into(),
            },
        )
        .expect("queue first run");
    wait_for(|| complete_assistant(&a) == 1, "host first turn completes").await;
    // The viewer's snapshot converges (the room carries the row).
    wait_for(
        || complete_assistant(&b) == 1,
        "viewer converged on first turn before reopen",
    )
    .await;

    // Phase 2: the viewer is closed (dropped) and reopened over the SAME
    // data dir, while the host keeps running.
    a.shutdown().await;
    drop(a);
    b.shutdown().await;
    drop(b);
    // Both engines share one process here; the instance lock on each data dir
    // releases when the engine graph drops. Wait for it so the reassemble
    // below doesn't race the teardown threads.
    wait_lock_released(&dirs.path().join("a"));
    wait_lock_released(&dirs.path().join("b"));

    let b2 = assemble(&dirs.path().join("b"), "device-b", &hub_url);
    let a2 = assemble(&dirs.path().join("a"), "device-a", &hub_url);
    // Give the reopened engines a beat to join the rooms.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // The reopened viewer must converge on the transcript written BEFORE
    // the reopen (its stale snapshot + cursor path).
    wait_for(
        || complete_assistant(&b2) >= 1,
        "reopened viewer converges on pre-reopen transcript",
    )
    .await;
    let pre = user_entries(&b2);
    assert!(
        pre.iter().any(|id| id == "msg-1"),
        "reopened viewer must show msg-1: {pre:?}"
    );

    // Phase 3: a NEW message sent after reopen still lands on the reopened
    // viewer — the room, not a stale cursor, must carry it.
    a2.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: RunRequest {
                    mode: None,
                    prompt: "second turn".into(),
                    harness: None,
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "/tmp".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: true,
                    attachments: Vec::new(),
                    worktree: None,
                    resume: None,
                },
                message_id: "msg-2".into(),
            },
        )
        .expect("queue second run");
    wait_for(
        || user_entries(&b2).iter().any(|id| id == "msg-2"),
        "reopened viewer receives post-reopen message",
    )
    .await;

    a2.shutdown().await;
    b2.shutdown().await;
}
