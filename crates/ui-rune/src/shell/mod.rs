//! The shell: window chrome, sidebar, main pane and right pane.
//!
//! This is the Rune port of the gpui `shell` module. The foundation renders the
//! gate/splash and the pane scaffold; the sidebar, spaces, transcript, composer,
//! terminal and diff panes fill it in as their modules land.

pub mod overlays;
pub mod panels;
pub mod routes;
pub mod sidebar;
pub mod spaces;
pub mod titlebar;

use rune::{col, el, row, text, Cx, El};

use crate::state::{App, GatePhase};
use crate::theme::Theme;

/// View-local shell state: route, panes, popups and per-chat panels.
#[derive(Debug, Default)]
pub struct ShellState {
    pub sidebar_collapsed: bool,
    pub right_open: bool,
    pub terminal_open: bool,
    pub route: Route,
}

/// Which top-level surface the main pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Chat,
    Settings,
}

/// The shell root: the gate decides between splash, failure card and the shell.
pub fn view(app: &App, _cx: &mut Cx<App>) -> El {
    let theme = &app.theme;
    match app.gate() {
        GatePhase::Loading => splash(app, theme),
        GatePhase::Failed(err) => gate_failed(theme, &err),
        GatePhase::Ready => shell(app, theme),
    }
}

fn splash(app: &App, theme: &Theme) -> El {
    let _ = app;
    col()
        .w_full()
        .h_full()
        .center()
        .gap(Theme::SPACE_MD)
        .fill(theme.bg)
        .child(text("Hearth").font(rune::tok::type_::TITLE).color(theme.text))
        .child(text("Connecting…").color(theme.text_muted))
}

fn gate_failed(theme: &Theme, err: &str) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .gap(Theme::SPACE_MD)
        .fill(theme.bg)
        .child(text("Could not reach the engine").color(theme.danger))
        .child(text(err).color(theme.text_muted))
        .child(
            el()
                .on_press(|app: &mut App| app.bootstrap_engine())
                .px(Theme::SPACE_MD)
                .py(Theme::SPACE_SM)
                .radius(Theme::CONTROL_RADIUS)
                .fill(theme.solid)
                .child(text("Retry").color(theme.on_solid)),
        )
}

fn shell(app: &App, theme: &Theme) -> El {
    let sidebar_width = if app.shell.sidebar_collapsed {
        0.0
    } else {
        app.settings.sidebar_width
    };
    let right_width = if app.shell.right_open {
        app.settings.right_pane_width
    } else {
        0.0
    };

    col()
        .w_full()
        .h_full()
        .fill(theme.bg)
        .child(
            row()
                .w_full()
                .h(Theme::TITLEBAR_HEIGHT)
                .fill(theme.surface)
                .child(titlebar::render(app, theme)),
        )
        .child(
            row()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(
                    col()
                        .w(sidebar_width)
                        .h_full()
                        .fill(theme.surface)
                        .child(sidebar::render(app, theme)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .h_full()
                        .fill(theme.bg)
                        .child(main_pane(app, theme)),
                )
                .child(
                    col()
                        .w(right_width)
                        .h_full()
                        .fill(theme.surface)
                        .child(if app.shell.right_open {
                            right_pane(app, theme)
                        } else {
                            el().w_full().h_full()
                        }),
                ),
        )
}

fn main_pane(app: &App, theme: &Theme) -> El {
    col()
        .w_full()
        .h_full()
        .padding(Theme::SPACE_LG)
        .gap(Theme::SPACE_SM)
        .fill(theme.bg)
        .child(
            text(match app.selected_chat.as_deref() {
                Some(id) => format!("Chat: {id}"),
                None => "New session".to_string(),
            })
            .font(rune::tok::type_::TITLE)
            .color(theme.text),
        )
        .child(
            text(format!(
                "{} spaces · {} chats · {} sessions",
                app.spaces.len(),
                app.chats.len(),
                app.sessions.len()
            ))
            .color(theme.text_muted),
        )
}

fn right_pane(_app: &App, theme: &Theme) -> El {
    col()
        .w_full()
        .h_full()
        .padding(Theme::SPACE_MD)
        .fill(theme.surface)
        .child(text("Changes").color(theme.text))
}
