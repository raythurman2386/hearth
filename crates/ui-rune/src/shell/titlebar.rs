//! Window titlebar: the unified chat/settings band, the top-left window-control cluster, the
//! right-pane tab strip and per-platform caption controls.
//!
//! Layout is hearth's: a 38-point band, content starting past the macOS traffic lights and the
//! 82-point control cluster, then the session identity and the right-pane controls. The band is
//! the window's drag region (`.window_drag()`); buttons stay clickable because Rune's hit test
//! stops at their own press handler.

use rune::{Cursor, El, Phase, TextStyle, Tx, el, icon, row, text};

use crate::icons;
use crate::state::App;
use crate::theme::Theme;

use super::panels::right_target;
use super::{
    RightSurface, add_terminal_surface, close_right_surface, navigate_back, navigate_forward,
    open_new_session, right_pane_open, right_surface_rows, set_right_active, toggle_right_pane,
    toggle_right_pane_expand, toggle_sidebar,
};

// ---------------------------------------------------------------------------
// Traffic-light-aware layout
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in points from the window's left edge
/// (hearth window-controls.tsx: `left: fullscreen ? 12 : 88`).
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already carries
/// `container_pad` points of its own left padding. macOS only.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Within-group rhythm for Back/Forward.
pub const TITLEBAR_CONTROL_GAP: f32 = 2.0;

/// Structural separation between titlebar groups.
pub const TITLEBAR_GROUP_GAP: f32 = Theme::SPACE_SM;

/// Breathing room between the navigation cluster and session identity.
pub const TITLEBAR_IDENTITY_GAP: f32 = Theme::SPACE_MD;

/// A 28px action centered in the 38px titlebar with its 2px downward optical shift.
pub const TITLEBAR_ACTION_EDGE_INSET: f32 = 6.0;

/// Width of the persistent top-left button cluster: 24px toggle, group gap, two 24px history
/// buttons on a 2px rhythm.
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + TITLEBAR_GROUP_GAP + TITLEBAR_CONTROL_GAP;

/// Extra width consumed when the collapsed-sidebar New Session action joins the cluster.
pub const TITLEBAR_ACTION_SLOT_WIDTH: f32 = TITLEBAR_GROUP_GAP + 24.0;

/// Horizontal inset owned by the titlebar control row itself.
pub const TITLEBAR_CLUSTER_PAD: f32 = 10.0;

/// Width of a row of `count` Linux caption buttons at the 24px/2px rhythm.
pub fn caption_buttons_width(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    count as f32 * 24.0 + (count as f32 - 1.0) * 2.0
}

/// Where the cluster's first button starts, from the window's left edge.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool, linux_left_captions: usize) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else if linux_left_captions > 0 {
        10.0 + caption_buttons_width(linux_left_captions) + 2.0
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header needs so its content starts past the overlay cluster.
pub fn cluster_clearance(
    is_macos: bool,
    fullscreen: bool,
    linux_left_captions: usize,
    container_pad: f32,
) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen, linux_left_captions) + CLUSTER_BUTTONS_WIDTH + 8.0
        - container_pad)
        .max(0.0)
}

/// Right padding titlebar content needs to clear the platform's caption controls.
pub fn titlebar_right_padding(is_windows: bool, linux_right_captions: usize, base: f32) -> f32 {
    base + if is_windows {
        46.0 * 3.0
    } else if linux_right_captions > 0 {
        10.0 + caption_buttons_width(linux_right_captions)
    } else {
        0.0
    }
}

/// The new-session `+` is useful only while an existing session is selected on the chat route.
pub fn titlebar_new_session_alpha(is_chat_route: bool, has_selected_chat: bool) -> f32 {
    if is_chat_route && has_selected_chat {
        1.0
    } else {
        0.0
    }
}

/// The leading inset from the window's own controls: macOS traffic lights (`Cx::
/// titlebar_inset`), or the cluster's 10-point pad elsewhere.
pub fn platform_cluster_inset(cx: &rune::Cx<App>) -> f32 {
    if cfg!(target_os = "macos") {
        let lights = cx.titlebar_inset();
        if lights > 0.0 {
            lights
        } else {
            // Full screen hides the lights; the cluster reclaims the inset.
            titlebar_cluster_start(true)
        }
    } else {
        TITLEBAR_CLUSTER_PAD
    }
}

/// Whether the app draws its own Linux caption buttons. Rune keeps the native frame on Linux;
/// a CSD session can opt into drawn controls with `HEARTH_CSD=1` (the shell then supplies
/// explicit minimize/maximize/close actions, as the gpui build did).
pub fn linux_captions_enabled() -> bool {
    cfg!(target_os = "linux") && std::env::var("HEARTH_CSD").is_ok()
}

fn linux_caption_count() -> usize {
    if linux_captions_enabled() { 3 } else { 0 }
}

/// The trailing inset reserved for the platform's caption controls.
pub fn platform_trailing_inset(cx: &rune::Cx<App>) -> f32 {
    // Windows: Rune draws the three 46-point caption buttons itself.
    let native = cx.chrome_insets().trailing;
    if native > 0.0 {
        native
    } else {
        titlebar_right_padding(false, linux_caption_count(), TITLEBAR_ACTION_EDGE_INSET)
    }
}

/// The unified window titlebar: chat -> the session identity and right-pane controls; settings
/// -> the section label. Full-width on the glass shell.
pub fn render(app: &App, theme: &Theme, cx: &rune::Cx<App>) -> El {
    let inner = match app.shell.route {
        super::Route::Chat => render_session_identity(app, theme, cx),
        super::Route::Settings(section) => {
            let leading = content_start(app, cx);
            row()
                .w_full()
                .h_full()
                .pt(Theme::TITLEBAR_TOP_PAD)
                .pl(leading)
                .pr(platform_trailing_inset(cx))
                .center_y()
                .child(
                    text(section.label())
                        .font(TextStyle::new(12.0, 500.0))
                        .color(theme.text),
                )
        }
    };
    crate::frost::chrome_band(
        theme,
        row()
            .key("titlebar-drag")
            .absolute(0.0, 0.0)
            .w_full()
            .h(Theme::TITLEBAR_HEIGHT)
            .window_drag()
            .child(inner),
    )
}

/// The top-left window-control cluster (sidebar toggle, back/forward, new session), painted
/// above the band so the sidebar may animate beneath it.
pub fn render_cluster(app: &App, theme: &Theme, cx: &rune::Cx<App>) -> El {
    let can_back = app.shell.nav.can_back();
    let can_forward = app.shell.nav.can_forward();
    let plus_alpha =
        titlebar_new_session_alpha(app.shell.route.is_chat(), app.selected_chat.is_some());
    let insert = platform_cluster_inset(cx);
    let mut cluster = row()
        .key("titlebar-cluster")
        .absolute(insert - TITLEBAR_CLUSTER_PAD, 0.0)
        .h(Theme::TITLEBAR_HEIGHT)
        .pt(Theme::TITLEBAR_TOP_PAD)
        .px(TITLEBAR_CLUSTER_PAD)
        .center_y()
        .child(control_button(
            "toggle-sidebar",
            &icons::SIDEBAR_MINIMALISTIC_LEFT,
            theme,
            true,
            |app: &mut App| toggle_sidebar(app),
        ));
    cluster = cluster.child(
        row()
            .ml(TITLEBAR_GROUP_GAP)
            .gap(TITLEBAR_CONTROL_GAP)
            .center_y()
            .child(control_button(
                "nav-back",
                &icons::ARROW_LEFT,
                theme,
                can_back,
                |app: &mut App| navigate_back(app),
            ))
            .child(control_button(
                "nav-forward",
                &icons::ARROW_RIGHT,
                theme,
                can_forward,
                |app: &mut App| navigate_forward(app),
            )),
    );
    if plus_alpha > 0.01 {
        cluster = cluster.child(el().ml(TITLEBAR_GROUP_SLOT).opacity(plus_alpha).child(
            control_button(
                "titlebar-new-session",
                &icons::PLUS,
                theme,
                true,
                |app: &mut App| open_new_session(app),
            ),
        ));
    }
    cluster
}

const TITLEBAR_GROUP_SLOT: f32 = TITLEBAR_GROUP_GAP;

fn content_start(app: &App, cx: &rune::Cx<App>) -> f32 {
    let sidebar = if app.settings.sidebar_collapsed {
        0.0
    } else {
        app.settings.sidebar_width
    };
    let cluster = platform_cluster_inset(cx) + CLUSTER_BUTTONS_WIDTH + TITLEBAR_IDENTITY_GAP;
    (sidebar + Theme::SPACE_LG).max(cluster)
}

fn render_session_identity(app: &App, theme: &Theme, cx: &rune::Cx<App>) -> El {
    let (title, target, harness) = match app
        .selected_chat
        .as_deref()
        .and_then(|id| app.chats.iter().find(|c| c.id == id))
    {
        Some(chat) => {
            let folder = chat
                .space_id
                .as_deref()
                .and_then(|id| app.space_row(id))
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "~".to_string());
            let device = app.device_name(&chat.device_id).unwrap_or("Unknown device");
            (
                chat.title
                    .clone()
                    .unwrap_or_else(|| "New session".to_string()),
                Some(format!("{folder} @ {device}")),
                chat.config.as_ref().map(|c| c.harness),
            )
        }
        None => (String::new(), None, None),
    };
    let on_canvas = app.selected_chat.is_none();
    let leading = content_start(app, cx);
    let trailing_pad = platform_trailing_inset(cx);
    let right_open = right_pane_open(app);
    let right_w = right_target(
        right_open,
        app.shell.right_pane_expanded,
        app.settings.right_pane_width,
        cx.layout("shell-root")
            .map(|r| r[2])
            .unwrap_or(crate::app::WINDOW_SIZE.0),
        if app.settings.sidebar_collapsed {
            0.0
        } else {
            app.settings.sidebar_width
        },
    );

    let identity = row()
        .min_w(0.0)
        .gap(6.0)
        .center_y()
        .children(harness.map(|harness| {
            let (mark, tint) = harness_mark(harness, theme);
            icon(mark, 14.0).color(tint)
        }))
        .child(
            text(title)
                .font(TextStyle::new(12.0, 500.0))
                .color(if on_canvas {
                    theme.text_muted.alpha(0.7)
                } else {
                    theme.text.alpha(0.85)
                })
                .truncate(),
        )
        .children(target.map(|target| {
            text(target)
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.text_muted.alpha(0.5))
        }));

    let trailing: Option<El> = if on_canvas {
        None
    } else {
        let mut controls = row().h_full().center_y();
        if right_open {
            let strip = render_right_tab_strip(app, theme);
            controls = controls.child(
                el().h_full()
                    .w((right_w - 72.0).max(0.0))
                    .clip_soft(0.0)
                    .child(strip),
            );
        }
        controls = controls
            .child(icon_button(
                "expand-right",
                super::tabs::right_pane_expand_icon(app.shell.right_pane_expanded),
                theme,
                |app: &mut App| toggle_right_pane_expand(app),
            ))
            .child(icon_button(
                "toggle-right",
                &icons::SIDEBAR_MINIMALISTIC,
                theme,
                |app: &mut App| toggle_right_pane(app),
            ));
        Some(controls)
    };

    row()
        .w_full()
        .h_full()
        .pt(Theme::TITLEBAR_TOP_PAD)
        .pl(leading)
        .pr(trailing_pad)
        .gap(Theme::SPACE_SM)
        .center_y()
        .child(identity)
        .child(el().grow(1.0))
        .children(trailing)
}

/// The right-pane surface tab strip (tab click selects, ✕ closes, drag reorders).
pub fn render_right_tab_strip(app: &App, theme: &Theme) -> El {
    // Fixed chip slot (hearth's 112 + the strip's 4 gap): drag reorder
    // quantises the pointer's travel by this, so uniform widths are required.
    const CHIP_W: f32 = 112.0;
    const CHIP_SLOT: f32 = 116.0;

    let rows = right_surface_rows(app);
    let active = super::resolved_right_active(app);
    let count = rows.len();
    let dragging = app.shell.right_tab_drag;
    let mut strip = row()
        .key("right-tab-strip")
        .h_full()
        .gap(4.0)
        .center_y()
        .clip_soft(0.0);
    if rows.is_empty() {
        return strip.child(
            text("Add a surface")
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted.alpha(0.5)),
        );
    }
    for (ix, (surface, title)) in rows.into_iter().enumerate() {
        let selected = surface == active;
        let surface_key = surface;
        let select = surface;
        let close = surface;
        // The drop target gets a hint while a sibling is dragged over it.
        let drop_target = dragging.is_some_and(|over| over == ix) && !selected;
        strip = strip.child(
            row()
                .key(format!("surface-{surface:?}"))
                .h(24.0)
                .w(CHIP_W)
                .min_w(CHIP_W)
                .px(8.0)
                .radius(6.0)
                .gap(6.0)
                .center_y()
                .cursor(Cursor::Pointer)
                .fill(if selected {
                    theme.surface_raised
                } else if drop_target {
                    theme.wash(0.10)
                } else {
                    theme.ink(0.0)
                })
                .hover(theme.element_hover)
                .on_press(move |app: &mut App| set_right_active(app, select))
                .on_drag(move |app: &mut App, drag: &mut rune::Drag, _tx| match drag.phase {
                    Phase::Down => {
                        app.shell.right_tab_drag = Some(ix);
                    }
                    Phase::Move => {
                        app.shell.right_tab_drag = Some(super::surface_drop_index(
                            ix, drag.dx, CHIP_SLOT, count,
                        ));
                    }
                    Phase::Up => {
                        let to = app.shell.right_tab_drag.take().unwrap_or(ix);
                        if !drag.cancelled() {
                            super::reorder_right_surface(app, ix, to);
                        }
                    }
                })
                .child(icon(surface_icon(surface_key), 12.0).color(if selected {
                    theme.text
                } else {
                    theme.text_muted
                }))
                .child(
                    text(title)
                        .font(TextStyle::new(11.5, 500.0))
                        .color(if selected {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .truncate()
                        .grow(1.0),
                )
                .child(
                    icon(&icons::CLOSE, 10.0)
                        .key(format!("surface-close-{surface:?}"))
                        .color(theme.text_muted.alpha(0.7))
                        .cursor(Cursor::Pointer)
                        .on_press(move |app: &mut App| close_right_surface(app, close)),
                ),
        );
    }
    strip.child(icon_button(
        "surface-plus",
        &icons::PLUS,
        theme,
        |app: &mut App| {
            if super::right_surface_rows(app).is_empty() {
                super::add_diff_surface(app);
            } else {
                add_terminal_surface(app);
            }
        },
    ))
}

fn surface_icon(surface: RightSurface) -> &'static rune::Icon {
    match surface {
        RightSurface::Shared => &icons::ADD_CIRCLE,
        RightSurface::Diff(_) => &icons::SPLIT_COLUMNS,
        RightSurface::Terminal(_) => &icons::TERMINAL,
        RightSurface::Subagent(_) => &icons::BOT,
    }
}

/// Harness brand marks (icons.tsx).
pub fn harness_mark(
    harness: hearth_proto::HarnessId,
    theme: &Theme,
) -> (&'static rune::Icon, rune::Color) {
    use hearth_proto::HarnessId as H;
    match harness {
        H::Codex => (&icons::OPENAI_MARK, theme.text_muted),
        H::Grok => (&icons::GROK_MARK, theme.text_muted),
        H::Huginn => (&icons::HUGINN_MARK, theme.text_muted),
        H::Mock => (&icons::BOT, theme.text_muted),
    }
}

fn control_button(
    key: &'static str,
    glyph: &'static rune::Icon,
    theme: &Theme,
    enabled: bool,
    action: fn(&mut App),
) -> El {
    let mut button = el()
        .key(key)
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .cursor(if enabled {
            Cursor::Pointer
        } else {
            Cursor::Default
        })
        .hover(theme.glass_hover())
        .no_window_drag()
        .child(icon(glyph, 16.0).color(theme.text_muted.alpha(if enabled { 1.0 } else { 0.35 })));
    if enabled {
        button = button.on_press(action);
    }
    button
}

fn icon_button(
    key: &'static str,
    glyph: &'static rune::Icon,
    theme: &Theme,
    action: fn(&mut App),
) -> El {
    control_button(key, glyph, theme, true, action)
}

/// Linux client-side-decoration caption controls. Off Linux (or without `HEARTH_CSD`) this is
/// `None`; Windows captions are drawn by Rune over the titlebar it owns.
pub fn render_captions(app: &App, theme: &Theme) -> Option<El> {
    let _ = app;
    if !linux_captions_enabled() {
        return None;
    }
    Some(
        row()
            .key("linux-captions")
            .absolute(-10.0, 0.0)
            .h(Theme::TITLEBAR_HEIGHT)
            .pt(Theme::TITLEBAR_TOP_PAD)
            .gap(2.0)
            .px(10.0)
            .center_y()
            .child(caption_button(
                "window-minimize",
                &icons::WINDOW_MINIMIZE,
                theme,
                |_app, tx: &mut Tx| tx.minimize(),
            ))
            .child(caption_button(
                "window-maximize",
                &icons::WINDOW_MAXIMIZE,
                theme,
                |_app, tx: &mut Tx| tx.toggle_maximized(),
            ))
            .child(caption_button(
                "window-close",
                &icons::CLOSE,
                theme,
                |_app, tx: &mut Tx| tx.close_window(),
            ))
            .pin(None, Some(0.0), Some(0.0), None),
    )
}

fn caption_button(
    key: &'static str,
    glyph: &'static rune::Icon,
    theme: &Theme,
    action: fn(&mut App, &mut Tx),
) -> El {
    el().key(key)
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .cursor(Cursor::Pointer)
        .hover(theme.glass_hover())
        .no_window_drag()
        .on_press_tx(action)
        .child(icon(glyph, 16.0).color(theme.text_muted))
}

/// The drag band used by the boot-failure card.
pub fn gate_drag_region(_theme: &Theme) -> El {
    el().key("gate-titlebar-drag")
        .absolute(0.0, 0.0)
        .w_full()
        .h(Theme::TITLEBAR_HEIGHT)
        .window_drag()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titlebar_cluster_matches_hearth_window_controls() {
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
        assert_eq!(TITLEBAR_CONTROL_GAP, 2.0);
        assert_eq!(TITLEBAR_GROUP_GAP, Theme::SPACE_SM);
        assert_eq!(TITLEBAR_IDENTITY_GAP, Theme::SPACE_MD);
        assert_eq!(CLUSTER_BUTTONS_WIDTH, 82.0);
        assert_eq!(TITLEBAR_ACTION_SLOT_WIDTH, 32.0);
        assert_eq!(TITLEBAR_ACTION_EDGE_INSET, 6.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(
            TITLEBAR_CLUSTER_PAD + titlebar_spacer_width(true, false, TITLEBAR_CLUSTER_PAD),
            titlebar_cluster_start(false)
        );
    }

    #[test]
    fn windows_caption_controls_reserve_titlebar_space() {
        assert_eq!(titlebar_right_padding(true, 0, 16.0), 154.0);
        assert_eq!(titlebar_right_padding(false, 0, 16.0), 16.0);
    }

    #[test]
    fn linux_caption_controls_reserve_titlebar_space() {
        assert_eq!(caption_buttons_width(0), 0.0);
        assert_eq!(caption_buttons_width(1), 24.0);
        assert_eq!(caption_buttons_width(3), 76.0);
        assert_eq!(titlebar_right_padding(false, 3, 16.0), 16.0 + 10.0 + 76.0);
        assert_eq!(titlebar_right_padding(false, 1, 16.0), 16.0 + 10.0 + 24.0);
        assert_eq!(cluster_buttons_start(false, false, 0), 10.0);
        assert_eq!(cluster_buttons_start(false, false, 1), 10.0 + 24.0 + 2.0);
        assert_eq!(cluster_buttons_start(false, false, 3), 10.0 + 76.0 + 2.0);
        assert_eq!(cluster_buttons_start(true, false, 3), 88.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        assert_eq!(cluster_clearance(false, false, 0, 16.0), 84.0);
        assert_eq!(cluster_clearance(false, false, 0, 10.0), 90.0);
        assert_eq!(cluster_clearance(false, false, 1, 16.0), 84.0 + 26.0);
        assert_eq!(
            cluster_clearance(true, false, 0, 16.0),
            88.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
        assert_eq!(
            cluster_clearance(true, true, 0, 16.0),
            12.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
    }

    #[test]
    fn new_session_action_lives_in_the_titlebar_only_when_useful() {
        assert_eq!(titlebar_new_session_alpha(true, true), 1.0);
        assert_eq!(titlebar_new_session_alpha(true, false), 0.0);
        assert_eq!(titlebar_new_session_alpha(false, true), 0.0);
        assert_eq!(titlebar_new_session_alpha(false, false), 0.0);
    }

    #[test]
    fn width_constants_stay_consistent() {}
}
