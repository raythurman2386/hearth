//! The shell's root layout and its pane outlets.
//!
//! The gpui shell floated an inset card over a full-height frost column and let the titlebar
//! overlay the content row, so the transcript scrolled under the chrome. Rune keeps the same
//! shape: a full-height content row (sidebar, main, right pane), a full-height sidebar tone,
//! and an absolute titlebar band. Columns that must not underlap pad themselves down by
//! [`Theme::TITLEBAR_HEIGHT`].
//!
//! Outlet hooks for wave 2 are marked `WAVE 2 HOOK` and kept as small functions:
//! [`main_outlet`], [`transcript_slot`], [`composer_slot`], [`terminal_slot`],
//! [`right_surface_outlet`].

use rune::{Cursor, El, TextStyle, col, el, icon, row, text};

use crate::icons;
use crate::state::App;
use crate::theme::Theme;

use super::panels::{self, conversation_width, resize_handle, right_target, sidebar_target};
use super::tabs::right_pane_expand_icon;
use super::titlebar::platform_cluster_inset;
use super::{
    RightSurface, Route, add_diff_surface, add_terminal_surface,
    open_settings, resolved_right_active, right_pane_open, terminal_open, toggle_right_pane,
    toggle_right_pane_expand,
};

/// The boot splash's bare page under the overlay.
pub fn splash_root(app: &App, _cx: &mut rune::Cx<App>, _layer: &mut Vec<El>) -> El {
    let theme = &app.theme;
    el().key("shell-root").w_full().h_full().fill(theme.bg)
}

/// The boot-failure card.
pub fn gate_failed(app: &App, theme: &Theme, err: &str) -> El {
    let card = col()
        .key("gate-card")
        .max_w(440.0)
        .gap(Theme::SPACE_MD)
        .padding(Theme::SPACE_LG)
        .radius(12.0)
        .fill(theme.surface_dialog)
        .border(1.0, theme.border)
        .child(
            text("Could not reach the engine")
                .font(TextStyle::new(15.0, 600.0))
                .color(theme.text),
        )
        .child(
            text(err.to_string())
                .font(TextStyle::new(13.0, 400.0))
                .color(theme.text_muted)
                .wrap(),
        )
        .child(
            el().on_press(|app: &mut App| app.bootstrap_engine())
                .px(Theme::SPACE_MD)
                .py(6.0)
                .radius(Theme::CONTROL_RADIUS)
                .fill(theme.solid)
                .center()
                .child(
                    text("Retry")
                        .font(TextStyle::new(13.0, 500.0))
                        .color(theme.on_solid),
                ),
        );

    el().key("shell-root")
        .w_full()
        .h_full()
        .fill(theme.bg)
        .center()
        .child(card)
        .child(super::titlebar::gate_drag_region(theme))
        .children(super::titlebar::render_captions(app, theme))
}

/// The full shell for a connected engine.
pub fn shell(app: &App, cx: &mut rune::Cx<App>, layer: &mut Vec<El>) -> El {
    let theme = &app.theme;
    let viewport = cx
        .layout("shell-root")
        .map(|rect| rect[2])
        .filter(|width| *width > 0.0)
        .unwrap_or(crate::app::WINDOW_SIZE.0);
    let viewport_h = cx
        .layout("shell-root")
        .map(|rect| rect[3])
        .filter(|height| *height > 0.0)
        .unwrap_or(crate::app::WINDOW_SIZE.1);

    let sidebar_w = sidebar_target(app.settings.sidebar_collapsed, app.settings.sidebar_width);
    let right_open = right_pane_open(app);
    let right_w = right_target(
        right_open,
        app.shell.right_pane_expanded,
        app.settings.right_pane_width,
        viewport,
        sidebar_w,
    );
    let _conversation = conversation_width(viewport, sidebar_w, right_w);
    let terminal_height = if terminal_open(app) {
        panels::clamp_terminal_height(app.settings.terminal_height, viewport_h)
    } else {
        0.0
    };

    let sidebar = el()
        .key("sidebar-pane")
        .w(sidebar_w)
        .h_full()
        .clip_soft(0.0)
        .motion(crate::motion::RESIZE)
        .child(if sidebar_w > 0.5 {
            super::sidebar::render(app, theme, layer)
        } else {
            el()
        });

    let scale = app.scale.get().max(1.0);
    let sidebar_seam = if sidebar_w > 0.5 {
        seam(
            panels::ResizeKind::Sidebar,
            viewport,
            viewport_h,
            theme,
            scale,
        )
    } else {
        el()
    };

    let main = col()
        .key("main-pane")
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .fill(theme.bg)
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .pt(Theme::TITLEBAR_HEIGHT)
                .child(match app.shell.route {
                    Route::Settings(section) => super::routes::settings_outlet(app, section, theme),
                    Route::Chat => main_outlet(app, theme),
                })
                .children(drop_veil(app, theme)),
        )
        .children(
            (terminal_height > 0.0).then(|| terminal_slot(app, theme, terminal_height, viewport_h)),
        );

    let right = el()
        .key("right-pane")
        .w(right_w)
        .h_full()
        .clip_soft(0.0)
        .motion(crate::motion::RESIZE)
        .child(if right_open && right_w > 0.5 {
            right_pane(app, theme)
        } else {
            el()
        });
    let right_seam = if right_open && right_w > 0.5 {
        seam(
            panels::ResizeKind::RightPane,
            viewport,
            viewport_h,
            theme,
            scale,
        )
    } else {
        el()
    };

    let tone = el()
        .absolute(0.0, 0.0)
        .w(sidebar_w)
        .h_full()
        .fill(theme.wash(0.05))
        .border_sides([0.0, 1.0 / cx.env().scale, 0.0, 0.0], theme.border);

    let content = row()
        .w_full()
        .h_full()
        .child(sidebar)
        .child(sidebar_seam)
        .child(main)
        .child(right_seam)
        .child(right);

    // Overlay layers owned by the shell. rune-kit menus/dialogs push themselves into `layer`.
    super::overlays::render_chat_overlays(app, theme, layer);
    super::spaces::render_space_overlays(app, theme, layer);
    super::spaces::render_add_space(app, theme, layer);

    let mut root = el()
        .key("shell-root")
        .w_full()
        .h_full()
        .fill(if theme.is_glass() {
            theme.glass()
        } else {
            theme.bg
        })
        .on_drop_files(|app: &mut App, files: &[std::path::PathBuf], _tx| {
            crate::composer::add_paths(app, files.to_vec());
        })
        .on_drop_hover(|app: &mut App, over: bool, _tx| {
            app.shell.drop_hover = over;
        })
        .child(tone)
        .child(content)
        .child(super::titlebar::render(app, theme, cx))
        .child(super::titlebar::render_cluster(app, theme, cx));

    if let Some(captions) = super::titlebar::render_captions(app, theme) {
        root = root.child(captions);
    }
    if let Some(pill) = super::overlays::render_jump_to_bottom(app, theme, app.shell.bottom_stack) {
        root = root.child(pill);
    }
    root
}

fn seam(kind: panels::ResizeKind, viewport: f32, viewport_h: f32, theme: &Theme, scale: f32) -> El {
    el().w(0.0)
        .h_full()
        .child(resize_handle(kind, theme, viewport, viewport_h, scale))
}

/// The chat route's content: transcript + composer, or the new-session canvas.
///
/// WAVE 2 HOOK: [`transcript_slot`] becomes `crate::transcript::render(app, ..)` and
/// [`composer_slot`] becomes `crate::composer::render(app, ..)`.
pub fn main_outlet(app: &App, theme: &Theme) -> El {
    let has_selection = app.selected_chat.is_some();
    let has_spaces = !app.spaces.is_empty();
    let no_project = app.no_project;

    if has_selection {
        col()
            .key("chat-outlet")
            .w_full()
            .h_full()
            .child(transcript_slot(app, theme))
            .child(composer_slot(app, theme))
    } else if !has_spaces && !no_project {
        onboarding_canvas(app, theme)
    } else {
        new_chat_canvas(app, theme)
    }
}

pub fn transcript_slot(app: &App, theme: &Theme) -> El {
    crate::transcript::render(app, theme)
}

pub fn composer_slot(app: &App, theme: &Theme) -> El {
    el()
        .w_full()
        .on_layout(|app: &mut App, [_, _, _, h]| {
            app.shell.bottom_stack = h;
        })
        .child(crate::composer::render(app, theme))
}

fn onboarding_canvas(app: &App, theme: &Theme) -> El {
    col()
        .key("onboarding-canvas")
        .w_full()
        .h_full()
        .center()
        .gap(6.0)
        .child(icon(&icons::HEARTH_LOGO, 48.0).color(theme.text.alpha(0.12)))
        .child(
            text("Add a project to get started")
                .font(TextStyle::new(16.0, 500.0))
                .color(theme.text)
                .mt(18.0),
        )
        .child(
            text("A project is a folder on one of your devices.")
                .font(TextStyle::new(13.0, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        )
        .child(
            el().mt(14.0)
                .px(Theme::SPACE_MD)
                .py(6.0)
                .radius(Theme::CONTROL_RADIUS)
                .fill(theme.solid)
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| super::spaces::open_add_space(app))
                .child(
                    text("Add a project")
                        .font(TextStyle::new(13.0, 500.0))
                        .color(theme.on_solid),
                ),
        )
        .child(
            text(format!(
                "{} chats across {} spaces · {} sessions",
                app.chats.len(),
                app.spaces.len(),
                app.sessions.len()
            ))
            .font(TextStyle::new(11.0, 400.0))
            .color(theme.text_faint)
            .mt(8.0),
        )
}

fn new_chat_canvas(app: &App, theme: &Theme) -> El {
    let space_name = app
        .selected_space_row()
        .map(|s| s.display_name().to_string())
        .unwrap_or_default();
    let helper = if space_name.is_empty() {
        "Send a message to start a new session.".to_string()
    } else {
        format!("Send a message to start a session in {space_name}.")
    };
    col()
        .key("new-chat-canvas")
        .w_full()
        .h_full()
        .child(
            col()
                .grow(1.0)
                .min_h(0.0)
                .center()
                .gap(Theme::SPACE_SM)
                .child(icon(&icons::HEARTH_LOGO, 48.0).color(theme.text.alpha(0.2)))
                // The TARGET selectors (device chip + project chip, then the
                // mode/model/traits cluster and the checkout/ref footer) under
                // the mark, over the helper line — hearth's `main_pane.rs`
                // new-chat canvas. Their popovers mount from
                // `composer::popups` like the composer's own pickers.
                .child(
                    el().w_full().max_w(460.0).mt(16.0).child(
                        crate::pickers::render_new_session(&app.pickers, app, theme),
                    ),
                )
                .child(
                    text(helper)
                        .font(TextStyle::new(14.0, 400.0))
                        .color(theme.text_muted.alpha(0.6))
                        .mt(12.0),
                ),
        )
        .child(composer_slot(app, theme))
}

/// The file-drop veil over the conversation column (hearth's `chat-dropzone`
/// overlay): the root's `on_drop_hover` drives it, a drop stages the paths in
/// the composer through the composer's own `on_drop_files`.
fn drop_veil(app: &App, theme: &Theme) -> Option<El> {
    if !app.shell.drop_hover || !app.shell.route.is_chat() {
        return None;
    }
    Some(
        el().key("drop-veil")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .center()
            // hearth: `theme.scrim().opacity(0.4 / 0.6)`.
            .fill(crate::theme::scrim_for(theme.appearance, 0.4))
            .child(
                text("Drop images to attach")
                    .font(TextStyle::new(13.0, 400.0))
                    .color(theme.text),
            ),
    )
}

pub fn terminal_slot(app: &App, theme: &Theme, height: f32, viewport_h: f32) -> El {
    col()
        .key("terminal-drawer")
        .w_full()
        .h(height)
        .clip_soft(0.0)
        .motion(crate::motion::RESIZE)
        .border_sides([1.0 / app.scale.get().max(1.0), 0.0, 0.0, 0.0], theme.border)
        .child(panels::terminal_handle(
            theme,
            viewport_h,
            app.scale.get().max(1.0),
        ))
        .child(
            el().w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(crate::terminal::render(&app.terminal, app, theme)),
        )
}

/// The right pane: a slim header carrying the ACTIVE surface's identity (tabs live in the
/// titlebar strip), then the active surface.
fn right_pane(app: &App, theme: &Theme) -> El {
    let active = resolved_right_active(app);
    let title = active_surface_title(app, active);
    let mut header = row()
        .w_full()
        .h(34.0)
        .px(Theme::SPACE_SM)
        .gap(6.0)
        .center_y()
        .clip_soft(0.0)
        .border_sides([0.0, 0.0, 1.0 / app.scale.get().max(1.0), 0.0], theme.border)
        .child(icon(surface_icon(active), 12.0).color(theme.text_muted));
    if matches!(active, RightSurface::Diff(_)) {
        // Scope / base ref / split / wrap / fold-all live in the pane header.
        header = header.child(
            el().h_full()
                .grow(1.0)
                .min_w(0.0)
                .child(crate::changes::header_controls(&app.changes, app, theme)),
        );
    } else {
        header = header.child(
            text(title)
                .font(TextStyle::new(11.5, 500.0))
                .color(theme.text_muted)
                .truncate()
                .grow(1.0),
        );
    }
    header = header
        .child(
            icon(right_pane_expand_icon(app.shell.right_pane_expanded), 14.0)
                .color(theme.text_muted)
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| toggle_right_pane_expand(app)),
        )
        .child(
            icon(&icons::CLOSE, 12.0)
                .color(theme.text_muted)
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| toggle_right_pane(app)),
        );

    col()
        .key("right-pane-body")
        .w_full()
        .h_full()
        .pt(Theme::TITLEBAR_HEIGHT)
        .fill(theme.surface)
        .child(header)
        .child(
            el().w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(right_surface_outlet(app, theme, active)),
        )
}

/// The active surface's strip/title text.
fn active_surface_title(app: &App, active: RightSurface) -> String {
    match active {
        RightSurface::Shared => "Surfaces".to_string(),
        RightSurface::Diff(id) => app
            .shell
            .diff_titles
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "Changes".to_string()),
        RightSurface::Terminal(id) => app
            .shell
            .terminal_titles
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "Terminal".to_string()),
        RightSurface::Subagent(id) => app
            .shell
            .subagent_tabs
            .get(&id)
            .map(|tab| tab.title.clone())
            .unwrap_or_else(|| "Subagent".to_string()),
    }
}

fn surface_icon(surface: RightSurface) -> &'static rune::Icon {
    match surface {
        RightSurface::Shared => &icons::ADD_CIRCLE,
        RightSurface::Diff(_) => &icons::SPLIT_COLUMNS,
        RightSurface::Terminal(_) => &icons::TERMINAL,
        RightSurface::Subagent(_) => &icons::BOT,
    }
}

/// WAVE 2 HOOK: diff/terminal/subagent bodies. Each arm becomes its module's render call.
pub fn right_surface_outlet(app: &App, theme: &Theme, active: RightSurface) -> El {
    match active {
        RightSurface::Shared => right_picker(app, theme),
        RightSurface::Diff(_id) => crate::changes::render(&app.changes, app, theme),
        RightSurface::Terminal(id) => {
            crate::terminal::render_embedded_key(&app.terminal, app, theme, id)
        }
        RightSurface::Subagent(id) => {
            let doc_id = app
                .shell
                .subagent_tabs
                .get(&id)
                .map(|tab| tab.doc_id.clone())
                .unwrap_or_default();
            if doc_id.is_empty() {
                el().w_full().h_full()
            } else {
                crate::transcript::render_subagent(app, theme, &doc_id)
            }
        }
    }
}

fn right_picker(app: &App, theme: &Theme) -> El {
    col()
        .w_full()
        .h_full()
        .padding(Theme::SPACE_MD)
        .gap(Theme::SPACE_SM)
        .child(
            text("Open a surface")
                .font(TextStyle::new(13.0, 500.0))
                .color(theme.text),
        )
        .child(picker_card(
            theme,
            &icons::SPLIT_COLUMNS,
            "Git diff",
            "Review the working tree or a commit.",
            |app: &mut App| add_diff_surface(app),
        ))
        .child(picker_card(
            theme,
            &icons::TERMINAL,
            "Terminal",
            "An embedded shell in this workspace.",
            |app: &mut App| add_terminal_surface(app),
        ))
        .child(
            text(format!(
                "{} tabs stored for this session",
                app.shell.right_tabs.len()
            ))
            .font(TextStyle::new(11.0, 400.0))
            .color(theme.text_faint),
        )
}

fn picker_card(
    theme: &Theme,
    glyph: &'static rune::Icon,
    title: &str,
    description: &str,
    action: fn(&mut App),
) -> El {
    row()
        .w_full()
        .gap(10.0)
        .padding(Theme::SPACE_MD)
        .radius(10.0)
        .fill(theme.surface_card)
        .border(1.0, theme.border)
        .cursor(Cursor::Pointer)
        .hover(theme.element_hover)
        .on_press(action)
        .child(
            el().size(30.0, 30.0)
                .radius(8.0)
                .center()
                .fill(theme.ink(0.06))
                .child(icon(glyph, 16.0).color(theme.text_muted)),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(
                    text(title.to_string())
                        .font(TextStyle::new(13.0, 500.0))
                        .color(theme.text),
                )
                .child(
                    text(description.to_string())
                        .font(TextStyle::new(11.5, 400.0))
                        .color(theme.text_muted),
                ),
        )
}

#[allow(dead_code)]
fn surface_placeholder(
    theme: &Theme,
    glyph: &'static rune::Icon,
    title: &str,
    hint: &str,
    close: impl Fn(&mut App) + 'static,
) -> El {
    col()
        .w_full()
        .h_full()
        .padding(Theme::SPACE_MD)
        .gap(8.0)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(icon(glyph, 15.0).color(theme.text_muted))
                .child(
                    text(title.to_string())
                        .font(TextStyle::new(13.0, 500.0))
                        .color(theme.text)
                        .grow(1.0),
                )
                .child(
                    icon(&icons::CLOSE, 12.0)
                        .color(theme.text_muted)
                        .cursor(Cursor::Pointer)
                        .on_press(close),
                ),
        )
        .child(
            text(hint.to_string())
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.text_faint)
                .wrap(),
        )
}

/// Settings route helper used by menu/keyboard handlers that need the current section.
pub fn current_settings_section(app: &App) -> crate::settings::SettingsSection {
    app.shell.route.section().unwrap_or(app.settings_ui.section)
}

/// Open the settings section currently remembered when no section is given.
pub fn open_last_settings(app: &mut App) {
    open_settings(app, current_settings_section(app));
}

/// The main-pane header start (kept for the wave-2 transcript header).
pub fn content_left(app: &App, cx: &rune::Cx<App>) -> f32 {
    let sidebar = sidebar_target(app.settings.sidebar_collapsed, app.settings.sidebar_width);
    sidebar.max(platform_cluster_inset(cx))
}
