//! A resume a few seconds after park is the SAME turn continuing after a
//! false settle (ACP quiet-settle / engine quiesce mid-`cargo build`), not
//! a genuine background-wake. The short self-continued window must NOT
//! apply — that path oscillated Idle↔Working every ~20s on the live hub
//! (2026-08-24) and follow-up sends cancelled the still-running tool.
//!
//! Own test binary: the env knobs are process-global. The sibling
//! `self_continued_quiesce.rs` pins `HEARTH_SELF_CONTINUED_SHORT_AFTER_MS=0`
//! so its 1.2s park still takes the short path.

mod common;
use common::*;

use std::sync::Once;
use std::time::Duration;

use hearth_engine::EngineCore;
use hearth_proto::{DoneStatus, HarnessId, SessionStatus};

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

fn assemble(main_prompt: &str) -> Rig {
    init_env();
    Rig::assemble(main_prompt, "hs-str")
}

fn status(core: &EngineCore) -> Option<SessionStatus> {
    common::session_status(core, CHAT)
}

async fn wait_for<F>(predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    common::wait_for_with(
        predicate,
        what,
        Duration::from_secs(10),
        Duration::from_millis(10),
    )
    .await;
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

    rig.feed.send(session_started("hs-str", "a-str")).unwrap();
    rig.feed.send(text("Compiling…")).unwrap();
    rig.feed
        .send(done_with_session(DoneStatus::Completed, "hs-str"))
        .unwrap();
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
