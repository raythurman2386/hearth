//! Headless layout assertions for the `demo` scene.
//!
//! The smoke `--tree` export is the human regression net; this is its headless
//! equivalent, so `cargo test` catches a collapsed transcript viewport, a
//! sidebar that drifts off 256, chrome that overflows the window, or a
//! duplicate sibling key before anyone looks at a screenshot.

use chrono::Utc;
use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use hearth_proto::{Chat, Device, Space, WorkspaceScope};
use hearth_ui_rune::UiConfig;
use hearth_ui_rune::shell::{Route, SplashPhase};
use hearth_ui_rune::state::{App, ConnectionStatus};
use rune::{Env, Runtime};
use std::collections::HashMap;

const W: f32 = 1320.0;
const H: f32 = 880.0;
const SIDEBAR_W: f32 = 256.0;
const EPS: f32 = 0.5;

/// `app.rs::demo_scene`, rebuilt locally (the snapshot fixture is private).
fn demo_app() -> (App, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp data dir");
    let mut app = App::new(UiConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    });

    app.connection = ConnectionStatus::Ready;
    app.workspace_scope = Some(WorkspaceScope::Local);
    app.local_device_id = Some("dev-local".into());
    let now = Utc::now();
    app.devices = vec![Device {
        id: "dev-local".into(),
        name: "Studio".into(),
        platform: "linux".into(),
        last_seen_at: Some(now),
        created_at: Some(now),
        version: Some("0.3.0".into()),
    }];
    app.spaces = vec![Space {
        id: "space-1".into(),
        device_id: "dev-local".into(),
        path: "/home/ret/Development/hearth".into(),
        name: Some("hearth".into()),
        git_detected: true,
        git_checked_at: Some(now),
        checkout_id: Some("checkout-1".into()),
        created_at: now,
    }];
    app.chats = vec![Chat {
        id: "chat-1".into(),
        device_id: "dev-local".into(),
        title: Some("Port the UI to Rune".into()),
        archived: false,
        cwd: Some("/home/ret/Development/hearth".into()),
        branch: Some("port/rune".into()),
        checkout_id: Some("checkout-1".into()),
        config: None,
        last_message_preview: Some("The shell renders.".into()),
        last_message_at: Some(now),
        created_at: now,
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: Some("space-1".into()),
        last_seen_at: Some(now),
        room_gen: None,
    }];
    app.selected_space = Some("space-1".into());
    app.selected_chat = Some("chat-1".into());
    app.transcript = vec![
        SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "part-user-1".into(),
                text: "Port the GUI to Rune without losing layout or functionality.".into(),
            }],
            created_at: now.timestamp_millis(),
            device_id: "dev-local".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        },
        SessionMessageEntry {
            id: "msg-assistant-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "part-assistant-1".into(),
                text: "## Port underway\n\nThe shell, sidebar, transcript and composer are wired.\n\n```rust\nfn view(s: &State, cx: &mut Cx<State>) -> El {\n    col().child(text(\"hello\"))\n}\n```\n\n| Layer | Status |\n| --- | --- |\n| Shell | done |\n| Transcript | done |\n| Composer | done |".into(),
            }],
            created_at: now.timestamp_millis() + 1000,
            device_id: "dev-local".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        },
    ];
    app.transcript_replayed = true;

    app.shell.splash = SplashPhase::Gone;
    app.shell.route = Route::Chat;
    app.shell.debug_dialog = None;

    (app, dir)
}

struct Rendered {
    rt: Runtime<App>,
    /// Owns the data dir for as long as the runtime; dropped (and deleted) at
    /// the end of the test.
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Rendered {
    type Target = Runtime<App>;
    fn deref(&self) -> &Self::Target {
        &self.rt
    }
}

impl std::ops::DerefMut for Rendered {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rt
    }
}

fn rendered() -> Rendered {
    let (app, dir) = demo_app();
    let mut rt = Runtime::new(app, App::view, (W, H), Env::new(true, 1.0));
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    Rendered { rt, _dir: dir }
}

fn new_chat_rendered() -> Rendered {
    let (mut app, dir) = demo_app();
    app.selected_chat = None;
    let mut rt = Runtime::new(app, App::view, (W, H), Env::new(true, 1.0));
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    Rendered { rt, _dir: dir }
}

#[test]
fn the_window_splits_into_sidebar_main_and_chrome() {
    let rt = rendered();

    let (root, _) = rt.presented("shell-root").expect("the shell root drew");
    assert!((root[0]).abs() < EPS, "root x {}", root[0]);
    assert!((root[1]).abs() < EPS, "root y {}", root[1]);
    assert!((root[2] - W).abs() < EPS, "root width {}", root[2]);
    assert!((root[3] - H).abs() < EPS, "root height {}", root[3]);

    let (sidebar, _) = rt.presented("sidebar-pane").expect("the sidebar drew");
    assert!(
        (sidebar[2] - SIDEBAR_W).abs() < EPS,
        "sidebar width {} (expected {SIDEBAR_W})",
        sidebar[2]
    );
    assert!(
        (sidebar[3] - H).abs() < EPS,
        "sidebar should be full height, got {}",
        sidebar[3]
    );

    let (main, _) = rt.presented("main-pane").expect("the main pane drew");
    assert!(main[2] > 0.0, "main pane width {}", main[2]);
    assert!(
        (main[2] - (W - SIDEBAR_W)).abs() < EPS,
        "main pane should take the rest of the row, got {}",
        main[2]
    );
    assert!(main[3] > 0.0, "main pane height {}", main[3]);

    let (titlebar, _) = rt.presented("titlebar-drag").expect("the titlebar drew");
    assert!(
        (titlebar[3] - 38.0).abs() < EPS,
        "titlebar height {}",
        titlebar[3]
    );
}

#[test]
fn the_conversation_column_keeps_a_viewport_and_a_composer() {
    let rt = rendered();

    let (outlet, _) = rt.presented("chat-outlet").expect("the chat outlet drew");
    assert!(outlet[3] > 0.0, "chat outlet height {}", outlet[3]);

    let (transcript, _) = rt
        .presented("app/transcript/viewport")
        .expect("the transcript viewport drew");
    assert!(
        transcript[3] > 0.0,
        "the transcript viewport collapsed to {}",
        transcript[3]
    );

    let (composer, _) = rt.presented("composer-root").expect("the composer drew");
    assert!(
        composer[3] > 0.0,
        "the composer collapsed to {}",
        composer[3]
    );
    assert!(
        transcript[3] + composer[3] <= outlet[3] + EPS,
        "transcript {} + composer {} overflow the outlet {}",
        transcript[3],
        composer[3],
        outlet[3]
    );
}

#[test]
fn the_new_chat_canvas_centers_its_targets_and_leaves_the_cluster_to_the_composer() {
    let rt = new_chat_rendered();
    assert!(
        rt.find("new-chat-canvas").is_some(),
        "the new-session canvas drew"
    );

    let (main, _) = rt.presented("main-pane").expect("the main pane drew");
    let center = main[0] + main[2] * 0.5;
    let (device, _) = rt
        .presented("pickers/anchor-device")
        .expect("the device selector drew");
    let (project, _) = rt
        .presented("pickers/anchor-space")
        .expect("the project selector drew");
    let left = device[0].min(project[0]);
    let right = (device[0] + device[2]).max(project[0] + project[2]);
    assert!(
        ((left + right) * 0.5 - center).abs() < 1.0,
        "the target selectors should sit on the canvas centre ({center}), got {left}…{right}"
    );

    let model_chips = rt
        .elements()
        .into_iter()
        .filter(|element| element.key.as_deref() == Some("pickers/anchor-model"))
        .count();
    assert_eq!(
        model_chips, 1,
        "only the composer carries the mode/model cluster; the canvas must not repeat it"
    );
}

#[test]
fn nothing_draws_outside_the_window() {
    let rt = rendered();
    let mut offenders = Vec::new();
    for element in rt.elements() {
        let [x, y, w, h] = element.visible;
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        let key = element.key.as_deref().unwrap_or(&element.path);
        if x < -EPS || y < -EPS || x + w > W + EPS || y + h > H + EPS {
            offenders.push(format!("{key}: visible [{x:.1}, {y:.1}, {w:.1}, {h:.1}]"));
        }
    }
    assert!(
        offenders.is_empty(),
        "{} elements draw outside the {W}x{H} window:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

#[test]
fn sibling_keys_are_unique() {
    let rt = rendered();
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let mut repaired = Vec::new();
    for element in rt.elements() {
        let Some(key) = element.key.as_deref() else {
            continue;
        };
        if key.contains("#dup") || element.path.contains("#dup") {
            // Rune's key check renames later duplicates `key#dup1` rather than
            // refusing to lay out. Those names are the visible warning here:
            // `Runtime::key_warnings` (which the `--tree` export prints) is
            // crate-private, so the repair marker is the public equivalent.
            repaired.push(format!("{key} at {}", element.path));
            continue;
        }
        let parent = element
            .path
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_string();
        *seen.entry((parent, key.to_string())).or_default() += 1;
    }
    let duplicates: Vec<_> = seen
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|((parent, key), n)| format!("{key:?} x{n} under {parent:?}"))
        .collect();
    assert!(
        duplicates.is_empty(),
        "duplicate sibling keys:\n{}",
        duplicates.join("\n")
    );
    assert!(
        repaired.is_empty(),
        "rune repaired duplicate keys (its key warnings):\n{}",
        repaired.join("\n")
    );
}

#[test]
fn drawn_text_stays_inside_its_row() {
    // A cheap overflow guard for the transcript: no row's text box may start or
    // end outside the window horizontally. Scroll clipping handles the rest.
    let rt = rendered();
    let mut offenders = Vec::new();
    for element in rt.elements() {
        if element.role != "text" {
            continue;
        }
        let [x, _, w, _] = element.visible;
        if w <= 0.0 {
            continue;
        }
        if x < -EPS || x + w > W + EPS {
            offenders.push(format!(
                "{:?}: [{x:.1} … {:.1}]",
                element.key.as_deref().unwrap_or(&element.path),
                x + w
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "text outside the window:\n{}",
        offenders.join("\n")
    );
}
