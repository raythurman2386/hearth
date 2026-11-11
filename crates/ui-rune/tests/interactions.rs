//! Click/keyboard interaction tests for the Rune shell.
//!
//! `Runtime` drives the exact input path a window does: a press picks through
//! the drawn tree, a release runs the handler, and the next frame re-runs the
//! view. The fixture is the same populated `demo` state the headless snapshots
//! use (`app.rs::demo_scene`), rebuilt here so these tests don't depend on that
//! function staying public.

use chrono::Utc;
use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use hearth_proto::{Chat, Device, Space, WorkspaceScope};
use hearth_ui_rune::UiConfig;
use hearth_ui_rune::engine::{EngineBootConfig, EngineHandle};
use hearth_ui_rune::shell::{self, Route, SplashPhase};
use hearth_ui_rune::state::App;
use rune::{Env, Key, KeyEvent, Mods, Runtime};

struct Harness {
    app: App,
    dir: tempfile::TempDir,
}

/// The `demo` scene: one local project, one chat, a user prompt and an
/// assistant reply — plus the shell poked past its boot splash so overlay
/// chrome can't intercept clicks.
fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("temp data dir");
    let mut app = App::new(UiConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    });

    app.connection = hearth_ui_rune::state::ConnectionStatus::Ready;
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
                text: "## Port underway\n\nThe shell, sidebar, transcript and composer are wired."
                    .into(),
            }],
            created_at: now.timestamp_millis() + 1000,
            device_id: "dev-local".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        },
    ];
    app.transcript_replayed = true;

    // The headed shell fades its splash out through `poll`; a test drives
    // frames directly, so park it gone and pin the route rather than inherit
    // whatever `HEARTH_OPEN_ROUTE`/`HEARTH_OPEN_DIALOG` a developer set.
    app.shell.splash = SplashPhase::Gone;
    app.shell.route = Route::Chat;
    app.shell.debug_dialog = None;

    Harness { app, dir }
}

fn runtime(app: App) -> Runtime<App> {
    let mut rt = Runtime::new(app, App::view, (1320.0, 880.0), Env::new(true, 1.0));
    // The shell's shortcuts are `AppSpec::on_key` in the real binary; tests
    // wire the same public dispatcher so keymap chords work.
    rt.on_key = Some(shell::on_key);
    rt.frame(0.0);
    rt
}

/// Press and release at the centre of `key`, then settle the frame the handler
/// dirties. Two frames because a handler may mount new chrome (a popover, a
/// dialog, the right pane) that only exists after the state change commits.
fn click(rt: &mut Runtime<App>, key: &str) {
    let (x, y) = rt
        .find(key)
        .unwrap_or_else(|| panic!("no element keyed {key:?} in the drawn tree"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
    rt.frame(1.0 / 60.0);
}

fn press(rt: &mut Runtime<App>, key: Key, mods: Mods) {
    rt.key(KeyEvent { key, mods });
    rt.frame(1.0 / 60.0);
    rt.frame(1.0 / 60.0);
}

#[test]
fn titlebar_plus_starts_a_new_session_on_the_canvas() {
    let mut rt = runtime(harness().app);
    assert_eq!(rt.state.selected_chat.as_deref(), Some("chat-1"));
    assert!(rt.find("chat-outlet").is_some());

    click(&mut rt, "titlebar-new-session");

    assert_eq!(
        rt.state.selected_chat, None,
        "the titlebar + clears the selection"
    );
    assert_eq!(rt.state.shell.route, Route::Chat);
    assert!(rt.find("chat-outlet").is_none());
    assert!(
        rt.find("new-chat-canvas").is_some(),
        "the canvas replaces the transcript"
    );
}

#[test]
fn a_sidebar_chat_row_selects_that_chat() {
    let mut rt = runtime(harness().app);
    click(&mut rt, "titlebar-new-session");
    assert!(rt.state.selected_chat.is_none());

    click(&mut rt, "chat-chat-1");

    assert_eq!(rt.state.selected_chat.as_deref(), Some("chat-1"));
    assert_eq!(rt.state.selected_space.as_deref(), Some("space-1"));
    assert!(rt.find("chat-outlet").is_some());
    assert!(
        rt.find("app/transcript/viewport").is_some(),
        "the selected chat renders its transcript"
    );
}

#[test]
fn the_sidebar_toggle_flips_the_persisted_flag() {
    let mut rt = runtime(harness().app);
    assert!(!rt.state.settings.sidebar_collapsed);

    click(&mut rt, "toggle-sidebar");
    assert!(rt.state.settings.sidebar_collapsed);

    click(&mut rt, "toggle-sidebar");
    assert!(!rt.state.settings.sidebar_collapsed);
}

#[test]
fn the_right_pane_toggle_is_scoped_to_the_selected_chat() {
    let mut rt = runtime(harness().app);
    assert!(
        !rt.state.shell.panels.get("chat-1").changes_open,
        "panels start closed"
    );

    click(&mut rt, "toggle-right");

    assert!(rt.state.shell.panels.get("chat-1").changes_open);
    assert!(
        rt.find("right-pane-body").is_some(),
        "the pane body mounts with the flag"
    );

    click(&mut rt, "toggle-right");
    assert!(!rt.state.shell.panels.get("chat-1").changes_open);
}

#[test]
fn the_user_menu_opens_settings() {
    let mut rt = runtime(harness().app);
    assert_eq!(rt.state.shell.route, Route::Chat);

    click(&mut rt, "user-menu");
    assert!(
        rt.find("user-menu-settings").is_some(),
        "the account popover lists Settings"
    );

    click(&mut rt, "user-menu-settings");
    assert!(
        matches!(rt.state.shell.route, Route::Settings(_)),
        "route is {:?}",
        rt.state.shell.route
    );
    assert!(rt.find("settings-outlet").is_some());
    assert!(
        !rt.state.shell.user_menu_open,
        "the popover closes behind it"
    );
}

#[test]
fn escape_dismisses_the_rename_dialog() {
    let mut rt = runtime(harness().app);
    // The session-row menu's Rename… action opens this same dialog; setting it
    // directly keeps this test about the modal's Escape contract.
    rt.state.shell.rename_chat = Some(hearth_ui_rune::shell::RenameChatDialog {
        chat_id: "chat-1".into(),
        input: rune::TextEdit::new("Port the UI to Rune"),
        focus_pending: true,
    });
    rt.frame(0.0);
    assert!(
        rt.find("rename-chat-dialog-dialog").is_some(),
        "the dialog is mounted"
    );

    press(&mut rt, Key::Escape, Mods::default());

    assert!(
        rt.state.shell.rename_chat.is_none(),
        "Escape reached the modal's dismiss"
    );
    assert!(rt.find("rename-chat-dialog-dialog").is_none());
}

#[test]
fn typing_reaches_the_composer_text_model() {
    let mut rt = runtime(harness().app);
    let (x, y) = rt
        .find("composer/input")
        .expect("the composer field is drawn");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
    assert!(
        rt.state.composer.edit.focused,
        "a press focuses the composer field"
    );

    rt.paste("hello from rune");
    rt.frame(1.0 / 60.0);
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.composer.edit.text, "hello from rune");
}

#[test]
fn the_scope_menu_mounts_above_the_header_and_switches_scope() {
    let Harness { mut app, dir: _dir } = harness();
    shell::add_diff_surface(&mut app);
    let key = app.shell.panel_key(&app);
    app.shell
        .panels
        .update(&key, |panels| panels.changes_open = true);
    let mut rt = runtime(app);

    click(&mut rt, "app/changes/scope-trigger");
    assert!(rt.state.changes.scope_menu_open);
    let (rect, _) = rt
        .presented("app/changes/scope-menu")
        .expect("the scope menu mounts in the overlay layer");
    assert!(
        rect[3] > 100.0,
        "the menu keeps its five rows instead of clipping to the 34pt header: {rect:?}"
    );

    click(&mut rt, "app/changes/scope-row/history");
    assert_eq!(
        rt.state.changes.scope,
        hearth_ui_rune::changes::DiffScope::History
    );
    assert!(!rt.state.changes.scope_menu_open);
}

#[test]
fn a_model_star_toggles_without_picking_the_row() {
    use hearth_ui_rune::pickers::{ModelRail, PickerKind};

    let Harness { mut app, dir: _dir } = harness();
    hearth_ui_rune::pickers::seed_demo_catalog(&mut app);
    app.pickers.defaults.favorites.clear();
    app.pickers.model_rail = ModelRail::Harness;
    app.pickers.open = Some(PickerKind::HarnessModel);
    let mut rt = runtime(app);

    assert!(
        rt.state.pickers.defaults.favorites.is_empty(),
        "no stars to begin with"
    );
    click(&mut rt, "pickers/model-star/0");

    assert_eq!(
        rt.state.pickers.defaults.favorites.len(),
        1,
        "the star press reaches the favorite toggle"
    );
    assert_eq!(
        rt.state.pickers.open,
        Some(PickerKind::HarnessModel),
        "starring reorders rows but must not pick a model or close the picker"
    );
}

#[tokio::test]
async fn the_terminal_shortcut_opens_the_drawer_with_a_tab() {
    let Harness { mut app, dir } = harness();

    // `open_tab` needs a live engine handle (it spawns the PTY session stream).
    // Boot the same in-process mock engine the app would embed, with the
    // test's own data dir and a port that was just free.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("probe a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        default_harness: hearth_proto::HarnessId::Mock,
    })
    .await
    .expect("embedded engine");
    app.engine = Some(handle.clone());
    let bridge = app.bridge.clone();
    app.terminal
        .open_tab("chat-1".into(), handle.clone(), bridge, None);

    let mut rt = runtime(app);
    assert!(!rt.state.shell.panels.get("chat-1").terminal_open);

    // Ctrl+J (the `mod-j` default off macOS); the chord is resolved through
    // the shell's public keymap dispatcher set in `runtime`.
    press(
        &mut rt,
        Key::Text("j".into()),
        Mods {
            ctrl: true,
            ..Mods::default()
        },
    );

    assert!(
        rt.state.shell.panels.get("chat-1").terminal_open,
        "the shortcut flips the per-chat drawer flag"
    );
    assert!(
        rt.state.terminal.active_tab("chat-1").is_some(),
        "opening the drawer ensures a terminal tab"
    );
    assert!(rt.find("terminal-drawer").is_some(), "the drawer is drawn");

    drop(rt);
    handle.shutdown().await;
}
