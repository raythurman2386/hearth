//! The Rune `AppSpec`: window geometry, fonts, the setup/poll/on_key hooks and
//! the headless scenes used by `--snap` / `--tree` verification.

use rune::{AppSpec, KeyEvent, Tx};

use crate::state::App;
use crate::theme::Theme;

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
static SCENES: &[(&str, fn(&mut App))] = &[
    ("ready", |app| {
        app.connection = crate::state::ConnectionStatus::Ready;
    }),
    ("failure", |app| {
        app.connection =
            crate::state::ConnectionStatus::Failed("engine unreachable".to_string());
    }),
    ("empty", |app| {
        app.connection = crate::state::ConnectionStatus::Ready;
    }),
    ("demo", demo_scene),
    ("settings", |app| {
        demo_scene(app);
        app.shell.route = crate::shell::Route::Settings(crate::settings::SettingsSection::Devices);
    }),
];

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
    changed |= app.pickers.poll();
    changed |= crate::pickers::pump(app);
    changed |= crate::composer::poll(app);
    if let Some(sent) = crate::composer::take_sent(app) {
        app.transcript_state.on_own_send(sent.chat_id, sent.message_id);
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
