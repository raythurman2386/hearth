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
];

fn empty() -> App {
    App::new(crate::UiConfig::default())
}

/// Store the waker and kick off the engine bootstrap. Headless runs skip the
/// engine: their scenes set the state the snapshot should show.
fn setup(app: &mut App, waker: rune::runtime::Waker) {
    app.bridge.set_waker(waker);
    if rune::tooling::headless_run().is_none() {
        app.bootstrap_engine();
    }
}

/// Drain engine events once per frame; true when visible state changed.
fn poll(app: &mut App, _now: f64, _tx: &mut Tx) -> bool {
    app.drain_events()
}

/// Window-level keys not handled by a focused control.
fn on_key(_app: &mut App, _ev: &KeyEvent, _tx: &mut Tx) -> bool {
    false
}

/// Window geometry shared with the shell.
pub const WINDOW_SIZE: (f32, f32) = (1320.0, 880.0);
pub const WINDOW_MIN_SIZE: (f32, f32) = (900.0, 600.0);

/// Titlebar height (matches [`Theme::TITLEBAR_HEIGHT`]).
pub fn titlebar_height() -> f32 {
    Theme::TITLEBAR_HEIGHT
}
