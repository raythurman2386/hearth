//! A resume a few seconds after park is the SAME turn continuing after a
//! false settle (ACP quiet-settle / engine quiesce mid-`cargo build`), not
//! a genuine background-wake. The short self-continued window must NOT
//! apply — that path oscillated Idle↔Working every ~20s on the live hub
//! (2026-08-24) and follow-up sends cancelled the still-running tool.
//!
//! Own test binary: the env knobs are process-global. The sibling
//! `self_continued_quiesce.rs` pins `HEARTH_SELF_CONTINUED_SHORT_AFTER_MS=0`
//! so its 1.2s park still takes the short path.

use std::sync::{Arc, Once};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::{Mutex, mpsc};

use hearth_engine::{EngineCore, HarnessRegistry};
use hearth_harness::{Harness, HarnessError, RunControls};
use hearth_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode,
};

const CHAT: &str = "chat-same-turn-resume";
const QUIESCE_MS: u64 = 600_000;
const SELF_QUIESCE_MS: u64 = 400;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        unsafe {
            std::env::set_var("HEARTH_TURN_QUIESCE_MS", QUIESCE_MS.to_string());
            std::env::set_var("HEARTH_SELF_TURN_QUIESCE_MS", SELF_QUIESCE_MS.to_string());
            // Production default is 30s; leave it so a 1.2s park is "brief".
        }
    });
}

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        mode: None,
        prompt: prompt.into(),
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
    }
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some("hs-str".into()),
    }
}

fn session_started() -> AgentEvent {
    AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: "hs-str".into(),
        assistant_message_id: "a-str".into(),
    }
}

fn text(t: &str) -> AgentEvent {
    AgentEvent::TextDelta { text: t.into() }
}

struct FeedHarness {
    main_prompt: String,
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
            let events = vec![Ok(done(DoneStatus::Completed))];
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

struct Rig {
    core: EngineCore,
    feed: mpsc::UnboundedSender<AgentEvent>,
    _dir: tempfile::TempDir,
}

fn assemble(main_prompt: &str) -> Rig {
    init_env();
    let (feed, rx) = mpsc::unbounded_channel();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(FeedHarness {
        main_prompt: main_prompt.into(),
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

fn status(core: &EngineCore) -> Option<SessionStatus> {
    core.sessions.session_status(CHAT).map(|s| s.status)
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Done → 1.2s gap (past the 1s resume gate, well under the 30s short-after
/// threshold) → more output. The short 400ms window must NOT park it.
#[tokio::test]
async fn brief_park_resume_keeps_the_normal_window() {
    let rig = assemble("lint and rebuild");
    rig.core
        .sessions
        .dispatch(CHAT, HarnessId::Mock, run_request("lint and rebuild"), None)
        .await
        .expect("dispatch");

    rig.feed.send(session_started()).unwrap();
    rig.feed.send(text("Compiling…")).unwrap();
    rig.feed.send(done(DoneStatus::Completed)).unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Idle),
        "park after Done",
    )
    .await;

    tokio::time::sleep(Duration::from_millis(1200)).await;
    rig.feed.send(text("   Compiling hearth-engine")).unwrap();
    wait_for(
        || status(&rig.core) == Some(SessionStatus::Working),
        "brief-gap resume re-arms Working",
    )
    .await;

    // Several short-window lengths of silence: must stay Working (the 10min
    // normal window is the only armed one).
    tokio::time::sleep(Duration::from_millis(SELF_QUIESCE_MS * 8)).await;
    assert_eq!(
        status(&rig.core),
        Some(SessionStatus::Working),
        "a same-turn resume must not park on the 20s/short self-continued window"
    );

    rig.core.sessions.shutdown().await;
}
