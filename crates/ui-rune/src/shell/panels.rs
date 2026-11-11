//! Pane width math, collapse/resize behavior and the drag handles.
//!
//! Numbers drive layout: sidebar 208–400 (default 256), right pane 360–520 (runtime ceiling
//! leaves the conversation [`CHAT_PANEL_MIN`]), terminal 160–55% of the viewport, and every
//! width/height change springs on the 200ms [`RESIZE`](crate::motion::RESIZE) catalog entry.
//! Rune animates the layout itself, so this module owns only the math and the pointer gestures.

use rune::{Cursor, El, Phase, el};

use crate::settings::{
    CHAT_PANEL_MIN, RIGHT_PANE_DEFAULT, RIGHT_PANE_MIN, SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN,
    TERMINAL_MIN_HEIGHT,
};
use crate::state::App;
use crate::theme::Theme;

/// Sidebar row height for the resort FLIP estimate: space line + title + meta line.
pub const CHAT_ROW_HEIGHT: f32 = 61.0;

/// Vertical pane-resize hitboxes yield the global titlebar.
pub const PANE_RESIZE_HITBOX_TOP: f32 = Theme::TITLEBAR_HEIGHT;

/// Width of a pane resize hit target (6 points each side of the seam).
pub const PANE_RESIZE_HITBOX_WIDTH: f32 = 12.0;

pub fn stable_panel_content_width(target: f32, transition: Option<(f32, f32)>) -> f32 {
    transition.map(|(from, to)| from.max(to)).unwrap_or(target)
}

pub fn right_panel_content_width(
    target: f32,
    transition: Option<(f32, f32)>,
    takeover_width: Option<f32>,
) -> f32 {
    takeover_width.unwrap_or_else(|| stable_panel_content_width(target, transition))
}

pub fn conversation_width(viewport: f32, sidebar: f32, right: f32) -> f32 {
    (viewport - sidebar - right).max(0.0)
}

/// Maximum width the right pane may occupy while retaining the conversation floor. On unusually
/// small windows this falls below the pane's preferred minimum: the chat stays usable and the
/// side surface yields the scarce space.
pub fn right_pane_max_width(viewport: f32, sidebar: f32) -> f32 {
    (viewport - sidebar - CHAT_PANEL_MIN).max(0.0)
}

/// Width used by right-pane takeover: takeover may consume the conversation completely.
pub fn right_pane_takeover_width(viewport: f32, sidebar: f32) -> f32 {
    (viewport - sidebar).max(0.0)
}

/// The sidebar's visible width.
pub fn sidebar_target(collapsed: bool, width: f32) -> f32 {
    if collapsed { 0.0 } else { width }
}

/// The right pane's visible width for the current viewport.
pub fn right_target(open: bool, expanded: bool, width: f32, viewport: f32, sidebar: f32) -> f32 {
    if !open {
        return 0.0;
    }
    if expanded {
        right_pane_takeover_width(viewport, sidebar)
    } else {
        width.min(right_pane_max_width(viewport, sidebar))
    }
}

/// Clamp a sidebar width into its persisted range.
pub fn clamp_sidebar_width(width: f32) -> f32 {
    width.clamp(SIDEBAR_MIN, SIDEBAR_MAX)
}

/// Clamp a right-pane width: at least [`RIGHT_PANE_MIN`] when the window has room, never past
/// the conversation floor.
pub fn clamp_right_pane_width(width: f32, viewport: f32, sidebar: f32) -> f32 {
    let max = right_pane_max_width(viewport, sidebar);
    if max >= RIGHT_PANE_MIN {
        width.clamp(RIGHT_PANE_MIN, max)
    } else {
        max
    }
}

/// Clamp the terminal drawer height: 160 points minimum, 55% of the viewport maximum.
pub fn clamp_terminal_height(height: f32, viewport_height: f32) -> f32 {
    let max = (viewport_height * crate::settings::TERMINAL_MAX_VH).max(TERMINAL_MIN_HEIGHT);
    height.clamp(TERMINAL_MIN_HEIGHT.min(max), max)
}

/// The terminal's visible height.
pub fn terminal_target(open: bool, height: f32) -> f32 {
    if open { height } else { 0.0 }
}

/// Reset widths to the catalog defaults (double-click on a handle).
pub fn reset_sidebar(app: &mut App) {
    app.settings.sidebar_width = SIDEBAR_DEFAULT;
    app.settings.sidebar_collapsed = false;
    app.save_settings();
}

pub fn reset_right_pane(app: &mut App) {
    app.settings.right_pane_width = RIGHT_PANE_DEFAULT;
    app.save_settings();
}

pub fn reset_terminal(app: &mut App) {
    app.settings.terminal_height = crate::settings::TERMINAL_DEFAULT_HEIGHT;
    app.save_settings();
}

/// Which seam a handle resizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeKind {
    Sidebar,
    RightPane,
    Terminal,
}

/// A drag handle for a pane seam. `onside` places it over the seam: the caller gives it a
/// zero-width relative parent, so this is an absolute 12-point target centered on the edge.
///
/// - Sidebar: `width = start + dx`.
/// - Right pane: `width = start - dx`, clamped to the conversation floor.
/// - Terminal: `height = start - dy`, clamped to 160–55vh.
pub fn resize_handle(
    kind: ResizeKind,
    theme: &Theme,
    viewport_width: f32,
    viewport_height: f32,
    scale: f32,
) -> El {
    let hover = theme.element_hover;
    let hairline = theme.hairline(0.18);
    let inch = 1.0 / scale.max(1.0);
    let width = PANE_RESIZE_HITBOX_WIDTH;
    let left = -width / 2.0;
    let mut handle = el()
        .absolute(left, PANE_RESIZE_HITBOX_TOP)
        .w(width)
        .h((viewport_height - PANE_RESIZE_HITBOX_TOP).max(0.0))
        .cursor(if kind == ResizeKind::Terminal {
            Cursor::ResizeRow
        } else {
            Cursor::ResizeColumn
        })
        .hover(hover)
        .on_drag(
            move |app: &mut App, drag: &mut rune::Drag, _tx| match drag.phase {
                Phase::Down => {
                    drag.data = match kind {
                        ResizeKind::Sidebar => app.settings.sidebar_width,
                        ResizeKind::RightPane => app.settings.right_pane_width,
                        ResizeKind::Terminal => app.settings.terminal_height,
                    };
                }
                Phase::Move => match kind {
                    ResizeKind::Sidebar => {
                        app.settings.sidebar_width = clamp_sidebar_width(drag.data + drag.dx);
                        app.settings.sidebar_collapsed = false;
                    }
                    ResizeKind::RightPane => {
                        let sidebar = sidebar_target(
                            app.settings.sidebar_collapsed,
                            app.settings.sidebar_width,
                        );
                        app.settings.right_pane_width =
                            clamp_right_pane_width(drag.data - drag.dx, viewport_width, sidebar);
                        app.shell.right_pane_expanded = false;
                    }
                    ResizeKind::Terminal => {
                        app.settings.terminal_height =
                            clamp_terminal_height(drag.data - drag.dy, viewport_height);
                    }
                },
                Phase::Up => app.save_settings(),
            },
        )
        .on_double_press(move |app: &mut App| match kind {
            ResizeKind::Sidebar => reset_sidebar(app),
            ResizeKind::RightPane => reset_right_pane(app),
            ResizeKind::Terminal => reset_terminal(app),
        })
        .child(
            // The seam's own hairline is the pane border; hover adds a stronger center
            // highlight that fades toward both ends.
            el().absolute(0.0, 0.0).w(inch).h_full().fill(hairline),
        );

    if kind == ResizeKind::Terminal {
        handle = handle.h(8.0).w_full().absolute(0.0, -4.0);
    }
    handle
}

/// A horizontal terminal handle sits above the drawer inside the main column.
pub fn terminal_handle(theme: &Theme, viewport_height: f32, scale: f32) -> El {
    let hairline = theme.hairline(0.18);
    let inch = 1.0 / scale.max(1.0);
    el().w_full()
        .h(8.0)
        .cursor(Cursor::ResizeRow)
        .on_drag(
            move |app: &mut App, drag: &mut rune::Drag, _tx| match drag.phase {
                Phase::Down => drag.data = app.settings.terminal_height,
                Phase::Move => {
                    app.settings.terminal_height =
                        clamp_terminal_height(drag.data - drag.dy, viewport_height);
                }
                Phase::Up => app.save_settings(),
            },
        )
        .on_double_press(|app: &mut App| reset_terminal(app))
        .child(el().w_full().h(inch).fill(hairline))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_pane_ceiling_preserves_the_chat_floor() {
        assert_eq!(right_pane_max_width(1200.0, 256.0), 644.0);
        assert_eq!(1200.0 - 256.0 - 644.0, CHAT_PANEL_MIN);
        assert_eq!(right_pane_max_width(800.0, 256.0), 244.0);
        assert_eq!(800.0 - 256.0 - 244.0, CHAT_PANEL_MIN);
    }

    #[test]
    fn right_pane_takeover_consumes_the_chat_column() {
        assert_eq!(right_pane_takeover_width(1200.0, 256.0), 944.0);
        assert_eq!(1200.0 - 256.0 - 944.0, 0.0);
    }

    #[test]
    fn pane_resize_hitboxes_yield_the_titlebar_chrome() {
        assert_eq!(PANE_RESIZE_HITBOX_TOP, Theme::TITLEBAR_HEIGHT);
    }

    #[test]
    fn right_panel_content_keeps_the_larger_width_only_during_transition() {
        assert_eq!(right_panel_content_width(520.0, None, None), 520.0);
        assert_eq!(
            right_panel_content_width(0.0, Some((520.0, 0.0)), None),
            520.0
        );
        assert_eq!(
            right_panel_content_width(760.0, Some((520.0, 760.0)), None),
            760.0
        );
        assert_eq!(
            right_panel_content_width(1064.0, Some((520.0, 1064.0)), Some(760.0)),
            760.0
        );

        let conversation = conversation_width(1320.0, 256.0, 520.0);
        let takeover = conversation_width(1320.0, 256.0, 1064.0);
        assert_eq!(conversation, 544.0);
        assert_eq!(takeover, 0.0);
        assert_eq!(
            stable_panel_content_width(takeover, Some((conversation, takeover))),
            conversation
        );
    }

    #[test]
    fn sidebar_bounds_and_collapse() {
        assert_eq!(clamp_sidebar_width(10.0), SIDEBAR_MIN);
        assert_eq!(clamp_sidebar_width(999.0), SIDEBAR_MAX);
        assert_eq!(clamp_sidebar_width(300.0), 300.0);
        assert_eq!(sidebar_target(true, 256.0), 0.0);
        assert_eq!(sidebar_target(false, 256.0), 256.0);
    }

    #[test]
    fn right_target_respects_takeover_and_the_chat_floor() {
        assert_eq!(right_target(false, false, 520.0, 1320.0, 256.0), 0.0);
        assert_eq!(right_target(true, false, 520.0, 1320.0, 256.0), 520.0);
        assert_eq!(right_target(true, true, 520.0, 1320.0, 256.0), 1064.0);
        // An over-wide request is capped so the conversation keeps its 300 points.
        assert_eq!(right_target(true, false, 2000.0, 1320.0, 256.0), 764.0);
    }

    #[test]
    fn right_pane_clamp_keeps_the_floor_when_room_is_scarce() {
        assert_eq!(clamp_right_pane_width(100.0, 1320.0, 256.0), RIGHT_PANE_MIN);
        assert_eq!(clamp_right_pane_width(900.0, 1320.0, 256.0), 764.0);
        assert_eq!(clamp_right_pane_width(400.0, 800.0, 256.0), 244.0);
    }

    #[test]
    fn terminal_height_bounds_are_160_to_55_percent() {
        assert_eq!(clamp_terminal_height(10.0, 880.0), 160.0);
        assert_eq!(clamp_terminal_height(2000.0, 880.0), 484.0);
        assert_eq!(clamp_terminal_height(300.0, 880.0), 300.0);
        assert_eq!(terminal_target(false, 300.0), 0.0);
        assert_eq!(terminal_target(true, 300.0), 300.0);
    }
}
