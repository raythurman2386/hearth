//! Shared support for the engine integration tests.
//!
//! Each integration-test file compiles this module into its own test binary, so
//! helpers that a given binary does not use are intentionally dead there.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{Mutex, mpsc};

use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use hearth_engine::{EdgeConfig, EngineCore, EngineProfile, HarnessRegistry, default_registry};
use hearth_harness::{Harness, HarnessError, RunControls};
use hearth_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode,
};

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

/// Poll `predicate` every 15ms for up to 10 seconds.
pub async fn wait_for<F>(predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    wait_for_with(
        predicate,
        what,
        Duration::from_secs(10),
        Duration::from_millis(15),
    )
    .await;
}

/// Poll `predicate` every 15ms for up to `deadline`.
pub async fn wait_for_within<F>(predicate: F, what: &str, deadline: Duration)
where
    F: FnMut() -> bool,
{
    wait_for_with(predicate, what, deadline, Duration::from_millis(15)).await;
}

/// Poll `predicate` every 15ms for up to `secs`.
pub async fn wait_for_secs<F>(predicate: F, what: &str, secs: u64)
where
    F: FnMut() -> bool,
{
    wait_for_within(predicate, what, Duration::from_secs(secs)).await;
}

/// Poll `predicate` every 50ms for up to `secs`.
pub async fn wait_for_within_secs<F>(predicate: F, what: &str, secs: u64)
where
    F: FnMut() -> bool,
{
    wait_for_with(
        predicate,
        what,
        Duration::from_secs(secs),
        Duration::from_millis(50),
    )
    .await;
}

/// Poll `predicate` until true, with an explicit deadline and poll interval.
pub async fn wait_for_with<F>(mut predicate: F, what: &str, deadline: Duration, interval: Duration)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + deadline;
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(interval).await;
    }
}

/// Poll `probe` until it yields `Some`, or panic at the deadline.
pub async fn wait_for_value<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Engine assembly
// ---------------------------------------------------------------------------

pub fn registry_with(harness: Arc<dyn Harness>) -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    Arc::new(registry)
}

pub fn assemble_with_registry(dir: &Path, registry: Arc<HarnessRegistry>) -> EngineCore {
    EngineCore::assemble(dir, registry, HarnessId::Mock, None).expect("engine core assembles")
}

pub fn assemble(dir: &Path) -> EngineCore {
    assemble_with_registry(dir, Arc::new(HarnessRegistry::new()))
}

pub fn assemble_with_harness(dir: &Path, harness: Arc<dyn Harness>) -> EngineCore {
    assemble_with_registry(dir, registry_with(harness))
}

/// Write `device-id` into `dir`, then assemble with the supplied registry.
pub fn assemble_with_device(
    dir: &Path,
    registry: Arc<HarnessRegistry>,
    device_id: &str,
) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    assemble_with_registry(dir, registry)
}

/// Assemble a device that syncs to a loopback hub with an org identity.
pub fn assemble_with_identity(
    dir: &Path,
    registry: Arc<HarnessRegistry>,
    device_id: &str,
    hub: &str,
    org: &str,
) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let edge = Some(EdgeConfig::with_static_token(hub, "tailnet").with_device(device_id));
    EngineCore::assemble_with_identity(dir, registry, HarnessId::Mock, edge, org, "alice")
        .expect("engine core assembles")
}

pub fn assemble_profile(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

// ---------------------------------------------------------------------------
// Doc reads
// ---------------------------------------------------------------------------

/// Tolerant read: a snapshot mid-segment-write deserializes with fields
/// missing — treat that instant as "not yet".
pub fn entries(core: &EngineCore, chat: &str) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(chat)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

pub fn complete_assistant_count(core: &EngineCore, chat: &str) -> usize {
    entries(core, chat)
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

pub fn assistant_texts(core: &EngineCore, chat: &str) -> Vec<(String, Option<MessageStatus>)> {
    entries(core, chat)
        .into_iter()
        .filter(|e| e.role == MessageRole::Assistant)
        .map(|e| {
            let text = e
                .parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            (text, e.status)
        })
        .collect()
}

pub fn session_status(core: &EngineCore, chat: &str) -> Option<SessionStatus> {
    core.sessions.session_status(chat).map(|s| s.status)
}

// ---------------------------------------------------------------------------
// Run / event builders
// ---------------------------------------------------------------------------

pub fn run_request(prompt: &str) -> RunRequest {
    run_request_in(prompt, "/tmp")
}

pub fn run_request_in(prompt: &str, cwd: &str) -> RunRequest {
    RunRequest {
        mode: None,
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    }
}

pub fn done(status: DoneStatus) -> AgentEvent {
    done_with_session(status, "hs-1")
}

pub fn done_with_session(status: DoneStatus, session_id: &str) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some(session_id.into()),
    }
}

pub fn session_started(session_id: &str, assistant_message_id: &str) -> AgentEvent {
    AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: session_id.into(),
        assistant_message_id: assistant_message_id.into(),
    }
}

pub fn text(t: &str) -> AgentEvent {
    AgentEvent::TextDelta { text: t.into() }
}

// ---------------------------------------------------------------------------
// Feed-by-hand quiesce rig
// ---------------------------------------------------------------------------

/// Feed-by-hand harness: the test pushes events through a channel, so it can
/// model turn boundaries, self-continuation, and a LOST turn-end exactly.
/// Accepted steers are confirmed with a `Steered` boundary, like the ACP
/// adapters do. The feed is served only to the test's own dispatch (matched
/// by prompt) — the engine's auto-titler also runs this harness, and gets an
/// immediately-completed empty stream instead.
pub struct FeedHarness {
    main_prompt: String,
    session_id: String,
    feed: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
}

#[async_trait]
impl Harness for FeedHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Feed"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
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
        mut controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if request.prompt != self.main_prompt {
            // The auto-titler's side run: complete instantly with nothing.
            let events = vec![Ok(done_with_session(
                DoneStatus::Completed,
                &self.session_id,
            ))];
            return Ok(futures::stream::iter(events).boxed());
        }
        let mut feed = self
            .feed
            .lock()
            .await
            .take()
            .expect("FeedHarness serves the main dispatch once per test");
        let (tx, rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(64);
        tokio::spawn(async move {
            let mut steering_open = true;
            loop {
                tokio::select! {
                    // Steers first: a Steered boundary always precedes feed
                    // events the test sends after steering (determinism).
                    biased;
                    steer = controls.steering.recv(), if steering_open => match steer {
                        Some(_) => {
                            let boundary = AgentEvent::Steered {
                                assistant_message_id: None,
                                next_assistant_message_id: None,
                            };
                            if tx.send(Ok(boundary)).await.is_err() {
                                return;
                            }
                        }
                        None => steering_open = false,
                    },
                    event = feed.recv() => match event {
                        Some(event) => {
                            if tx.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    },
                }
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

pub struct Rig {
    pub core: EngineCore,
    pub feed: mpsc::UnboundedSender<AgentEvent>,
    pub _dir: tempfile::TempDir,
}

impl Rig {
    pub fn assemble(main_prompt: &str, session_id: &str) -> Rig {
        let (feed, rx) = mpsc::unbounded_channel();
        let registry = HarnessRegistry::new();
        registry.register(Arc::new(FeedHarness {
            main_prompt: main_prompt.into(),
            session_id: session_id.into(),
            feed: Mutex::new(Some(rx)),
        }));
        let dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
            .expect("engine core assembles");
        Rig {
            core,
            feed,
            _dir: dir,
        }
    }
}
