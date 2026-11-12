//! The Rune `AppSpec`: window geometry, fonts, the setup/poll/on_key hooks and
//! the headless scenes used by `--snap` / `--tree` verification.

use rune::{AppSpec, KeyEvent, Tx};

use crate::shell::Route;
use crate::state::App;
use crate::theme::{Appearance, Theme};

/// Embedded UI fonts — Geist and Geist Mono (variable), © Vercel Inc.,
/// licensed under the SIL Open Font License 1.1.
pub static FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Geist.ttf"),
    include_bytes!("../assets/fonts/GeistMono.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Geist-Bold.ttf"),
];

/// The spec. `init` is a dummy fn pointer; `App::init` supplies the real one
/// with the [`crate::UiConfig`] captured.
pub fn spec() -> AppSpec<App> {
    AppSpec {
        title: "Hearth",
        size: (1320.0, 880.0),
        min_size: Some((900.0, 600.0)),
        init: empty,
        view: App::view,
        scenes: SCENES,
        fonts: FONTS,
        families: ("Geist", "Geist Mono"),
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
    }
}

/// Named states the headless snapshot/tree tooling can render.
type Scene = (&'static str, fn(&mut App));
static SCENES: &[Scene] = &[
    ("ready", |app| {
        app.connection = crate::state::ConnectionStatus::Ready;
    }),
    ("loading", |app| {
        app.connection = crate::state::ConnectionStatus::Connecting;
        app.shell.splash = crate::shell::SplashPhase::Visible;
    }),
    ("failure", |app| {
        app.connection = crate::state::ConnectionStatus::Failed("engine unreachable".to_string());
    }),
    ("empty", |app| {
        app.connection = crate::state::ConnectionStatus::Ready;
    }),
    ("demo", demo_scene),
    ("new-chat", |app| {
        demo_scene(app);
        app.selected_chat = None;
        crate::pickers::seed_demo_catalog(app);
    }),
    ("settings", |app| {
        demo_scene(app);
        app.shell.route = crate::shell::Route::Settings(crate::settings::SettingsSection::Devices);
    }),
    ("appearance", |app| {
        demo_scene(app);
        app.shell.route =
            crate::shell::Route::Settings(crate::settings::SettingsSection::Appearance);
    }),
    ("models", |app| {
        demo_scene(app);
        crate::pickers::seed_demo_catalog(app);
        app.pickers.model_rail = crate::pickers::ModelRail::Favorites;
        app.pickers.open = Some(crate::pickers::PickerKind::HarnessModel);
    }),
    ("narrow", |app| {
        demo_scene(app);
        let key = app.shell.panel_key(app);
        app.shell
            .panels
            .update(&key, |panels| panels.changes_open = true);
    }),
    ("diff", diff_scene),
    ("scopes", |app| {
        diff_scene(app);
        app.changes.scope_menu_open = true;
    }),
    ("refs", |app| {
        diff_scene(app);
        app.changes.scope = crate::changes::DiffScope::Branch;
        app.changes.base_ref = Some("main".into());
        app.changes.branches = vec!["main".into(), "port/rune".into(), "feat/rune-scopes".into()];
        app.changes.ref_menu_open = true;
    }),
    ("history", |app| {
        demo_scene(app);
        let key = app.shell.panel_key(app);
        app.shell
            .panels
            .update(&key, |panels| panels.changes_open = true);
        crate::shell::add_diff_surface(app);
        app.changes.scope = crate::changes::DiffScope::History;
        let now = chrono::Utc::now();
        // A deterministic fake hash so demo rows look like real commits.
        let fake_sha = |n: usize| -> String {
            let mut x = (n as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            (0..40)
                .map(|_| {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    char::from_digit((x >> 60) as u32, 16).unwrap_or('0')
                })
                .collect()
        };
        let commits: Vec<hearth_proto::GitHistoryCommit> = (0..24)
            .map(|i| hearth_proto::GitHistoryCommit {
                sha: fake_sha(i),
                parent_shas: if i + 1 < 24 {
                    vec![fake_sha(i + 1)]
                } else {
                    Vec::new()
                },
                subject: format!(
                    "Port the {} surface to Rune",
                    [
                        "shell",
                        "transcript",
                        "composer",
                        "terminal",
                        "diff",
                        "settings"
                    ][i % 6]
                ),
                author_name: "ret".into(),
                author_email: "ret@example.com".into(),
                authored_at: now.to_rfc3339(),
                refs: if i == 0 {
                    vec![
                        hearth_proto::GitHistoryRef {
                            kind: hearth_proto::GitHistoryRefKind::Branch,
                            label: "main".into(),
                        },
                        hearth_proto::GitHistoryRef {
                            kind: hearth_proto::GitHistoryRefKind::Tag,
                            label: "v0.3.0".into(),
                        },
                    ]
                } else {
                    Vec::new()
                },
            })
            .collect();
        app.changes
            .seed_history_demo("/home/ret/Development/hearth", commits, Some(fake_sha(0)));
    }),
    ("stress", stress_scene),
];

/// Stress the transcript/composer with long unbroken tokens at a narrow width.
fn stress_scene(app: &mut App) {
    demo_scene(app);
    use chrono::Utc;
    use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
    let now = Utc::now();
    let long = format!("https://example.com/{}", "a".repeat(180));
    app.transcript.push(SessionMessageEntry {
        id: "msg-user-stress".into(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: "part-user-stress".into(),
            text: format!(
                "Here is a long unbroken token: {long} and another: {} plus a code-ish blob {}",
                "b".repeat(160),
                "c".repeat(120)
            ),
        }],
        created_at: now.timestamp_millis() + 2000,
        device_id: "dev-local".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    });
    app.transcript.push(SessionMessageEntry {
        id: "msg-assistant-stress".into(),
        role: MessageRole::Assistant,
        parts: vec![MessagePart::Text {
            id: "part-assistant-stress".into(),
            text: format!(
                "A long code line follows:\n\n```rust\nlet token = \"{}\";\n```\n\nAnd a wide table:\n\n| Key | Value |\n| --- | --- |\n| token | {long} |",
                "d".repeat(160)
            ),
        }],
        created_at: now.timestamp_millis() + 3000,
        device_id: "dev-local".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    });
    app.composer
        .drafts
        .insert("chat-1".into(), format!("Draft with a long token: {long}"));
}

/// The right pane showing the demo patch (the `diff` and `scopes` scenes).
fn diff_scene(app: &mut App) {
    demo_scene(app);
    let key = app.shell.panel_key(app);
    app.shell
        .panels
        .update(&key, |panels| panels.changes_open = true);
    crate::shell::add_diff_surface(app);
    app.changes
        .seed_demo(DEMO_PATCH, "chat-1", "/home/ret/Development/hearth");
}

const DEMO_PATCH: &str = r#"diff --git a/crates/ui-rune/src/shell.rs b/crates/ui-rune/src/shell.rs
index 1111111..2222222 100644
--- a/crates/ui-rune/src/shell.rs
+++ b/crates/ui-rune/src/shell.rs
@@ -1,7 +1,9 @@
 fn main() {
-    let shell = Shell::new();
-    shell.run();
+    let shell = Shell::new();
+    shell.install_theme();
+    shell.run();
 }
 
 // trailing context
diff --git a/README.md b/README.md
index 3333333..4444444 100644
--- a/README.md
+++ b/README.md
@@ -1,3 +1,4 @@
 # Hearth
 
-Old copy
+New copy with a very long line that should scroll horizontally instead of widening the pane: https://example.com/a/really/long/path/that/keeps/going/and/going
"#;

/// A populated chat for layout snapshots: one project, one session, a user
/// prompt and an assistant reply with markdown + a code fence.
fn demo_scene(app: &mut App) {
    use chrono::Utc;
    use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
    use hearth_proto::{Chat, Device, Space, WorkspaceScope};

    app.connection = crate::state::ConnectionStatus::Ready;
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
                text: "## Port underway\n\nThe shell, sidebar, transcript and composer are wired.\n\n- **Rows** keep their block ids\n- `Markdown` renders GFM tables and code\n\n```rust\nfn view(s: &State, cx: &mut Cx<State>) -> El {\n    col().child(text(\"hello\"))\n}\n```\n\n| Layer | Status |\n| --- | --- |\n| Shell | done |\n| Transcript | done |\n| Composer | done |".into(),
            }],
            created_at: now.timestamp_millis() + 1000,
            device_id: "dev-local".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        },
    ];
    app.transcript_replayed = true;
}

fn empty() -> App {
    App::new(crate::UiConfig::default())
}

/// Store the waker and kick off the engine bootstrap. Headless runs skip the
/// engine: their scenes set the state the snapshot should show.
fn setup(app: &mut App, waker: rune::runtime::Waker) {
    app.bridge.set_waker(waker.clone());
    crate::composer::set_waker(waker);
    if rune::tooling::headless_run().is_none() {
        app.bootstrap_engine();
    }
}

/// Drain engine events and run the per-frame pumps; true when visible state
/// changed.
fn poll(app: &mut App, _now: f64, tx: &mut Tx) -> bool {
    let mut changed = app.drain_events();
    changed |= app.refresh_omarchy_if_changed();

    // Follow the OS when the user chose "System" and no Omarchy theme wins.
    if app.omarchy.is_none() {
        let system = if app.system_dark.get() {
            Appearance::Dark
        } else {
            Appearance::Light
        };
        if app.appearance.system != system {
            app.appearance.system = system;
            changed |= app.refresh_theme();
        }
    }

    changed |= crate::shell::poll(app);
    changed |= app.settings_ui.poll();
    app.settings_ui.sync(&app.devices);
    if let Some(url) = app.settings_ui.take_open_url() {
        let _ = tx.open_url(&url);
        changed = true;
    }
    changed |= app.transcript_state.poll();
    changed |= crate::changes::poll(app);
    changed |= app.terminal.poll();
    if let Some(wait) = app.terminal.next_wake() {
        tx.wake_after(wait);
    }
    changed |= app.pickers.poll();
    changed |= crate::pickers::pump(app);
    changed |= crate::composer::poll(app);
    if let Some(sent) = crate::composer::take_sent(app) {
        app.transcript_state
            .on_own_send(sent.chat_id, sent.message_id);
        changed = true;
    }

    // Focus hand-offs requested by shell handlers (they have no `Tx`).
    if app.shell.focus_terminal {
        app.shell.focus_terminal = false;
        tx.focus("terminal-body", false);
        changed = true;
    }
    if app.shell.focus_composer {
        app.shell.focus_composer = false;
        crate::composer::focus(app, tx);
        changed = true;
    }
    // First usable frame: put the caret in the prompt so typing works without
    // a click (the gpui shell focused the composer on Ready).
    if !app.shell.composer_focused_once
        && app.connection == crate::state::ConnectionStatus::Ready
        && app.shell.route == Route::Chat
        && !crate::shell::terminal_open(app)
    {
        app.shell.composer_focused_once = true;
        crate::composer::focus(app, tx);
        changed = true;
    }
    changed
}

/// Window-level keys not handled by a focused control.
fn on_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    crate::shell::on_key(app, ev, tx)
}

/// Window geometry shared with the shell.
pub const WINDOW_SIZE: (f32, f32) = (1320.0, 880.0);
pub const WINDOW_MIN_SIZE: (f32, f32) = (900.0, 600.0);

/// Titlebar height (matches [`Theme::TITLEBAR_HEIGHT`]).
pub fn titlebar_height() -> f32 {
    Theme::TITLEBAR_HEIGHT
}
