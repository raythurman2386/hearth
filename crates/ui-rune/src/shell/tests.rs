//! Pure shell logic: per-session panels, navigation, pane math and the tab catalog.
//!
//! The module-specific tests live next to their functions (`nav`, `panels`, `sidebar`, `tabs`,
//! `titlebar`, `spaces`); this file keeps the cross-cutting parity checks ported from the gpui
//! `shell/tests.rs`.

use crate::icons;
use crate::settings::{CHAT_PANEL_MIN, RIGHT_PANE_MIN, SettingsSection, platform_combo};
use crate::theme::Theme;

use super::panels::{
    PANE_RESIZE_HITBOX_TOP, conversation_width, right_pane_max_width, right_pane_takeover_width,
    right_panel_content_width, stable_panel_content_width,
};
use super::tabs::right_pane_expand_icon;
use super::titlebar::{
    CLUSTER_BUTTONS_WIDTH, TITLEBAR_ACTION_EDGE_INSET, TITLEBAR_ACTION_SLOT_WIDTH,
    TITLEBAR_CONTROL_GAP, TITLEBAR_GROUP_GAP, TITLEBAR_IDENTITY_GAP, caption_buttons_width,
    cluster_buttons_start, cluster_clearance, titlebar_cluster_start, titlebar_new_session_alpha,
    titlebar_right_padding, titlebar_spacer_width,
};
use super::{ChatPanels, RightSurface, SessionPanels, tabs};

#[test]
fn every_default_shortcut_parses() {
    for id in crate::settings::ShortcutId::ALL {
        let combo = platform_combo(id.default_combo());
        let chord = super::rune_chord(&combo);
        if let Err(err) = chord.parse::<rune::Shortcut>() {
            panic!(
                "{} default {combo:?} (chord {chord:?}) does not parse: {err:?}",
                id.label()
            );
        }
    }
}

#[test]
fn right_pane_ceiling_preserves_the_chat_floor() {
    assert_eq!(right_pane_max_width(1200.0, 256.0), 644.0);
    assert_eq!(1200.0 - 256.0 - 644.0, CHAT_PANEL_MIN);
    assert_eq!(right_pane_max_width(800.0, 256.0), 244.0);
    assert_eq!(800.0 - 256.0 - 244.0, CHAT_PANEL_MIN);
    assert!(right_pane_max_width(800.0, 256.0) < RIGHT_PANE_MIN);
}

#[test]
fn right_pane_takeover_consumes_the_chat_column() {
    assert_eq!(right_pane_takeover_width(1200.0, 256.0), 944.0);
    assert_eq!(1200.0 - 256.0 - 944.0, 0.0);
}

#[test]
fn right_pane_takeover_control_reverses_direction() {
    assert_eq!(
        right_pane_expand_icon(false).name,
        icons::EXPAND_ARROWS.name
    );
    assert_eq!(
        right_pane_expand_icon(true).name,
        icons::COLLAPSE_ARROWS.name
    );
}

#[test]
fn pane_resize_hitboxes_yield_the_titlebar_chrome() {
    assert_eq!(PANE_RESIZE_HITBOX_TOP, Theme::TITLEBAR_HEIGHT);
}

#[test]
fn new_session_action_lives_in_the_titlebar_only_when_useful() {
    assert_eq!(titlebar_new_session_alpha(true, true), 1.0);
    assert_eq!(titlebar_new_session_alpha(true, false), 0.0);
    assert_eq!(titlebar_new_session_alpha(false, true), 0.0);
    assert_eq!(titlebar_new_session_alpha(false, false), 0.0);
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
fn titlebar_cluster_matches_hearth_window_controls() {
    assert_eq!(titlebar_cluster_start(false), 88.0);
    assert_eq!(titlebar_cluster_start(true), 12.0);
    assert_eq!(TITLEBAR_CONTROL_GAP, 2.0);
    assert_eq!(TITLEBAR_GROUP_GAP, Theme::SPACE_SM);
    assert_eq!(TITLEBAR_IDENTITY_GAP, Theme::SPACE_MD);
    assert_eq!(CLUSTER_BUTTONS_WIDTH, 82.0);
    assert_eq!(TITLEBAR_ACTION_SLOT_WIDTH, 32.0);
    assert_eq!(TITLEBAR_ACTION_EDGE_INSET, 6.0);
    assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
    assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
    assert_eq!(caption_buttons_width(3), 76.0);
    assert_eq!(titlebar_right_padding(false, 3, 16.0), 102.0);
    assert_eq!(cluster_buttons_start(false, false, 1), 36.0);
    assert!(cluster_clearance(true, false, 0, 16.0) > 0.0);
}

// ---- per-session panel flags ----

#[test]
fn session_panels_default_closed_per_chat() {
    let panels = SessionPanels::default();
    assert_eq!(panels.get("a"), ChatPanels::default());
    assert!(!panels.get("a").terminal_open);
    assert!(!panels.get("a").changes_open);
    assert_eq!(panels.get("a").right_active, RightSurface::Shared);
    assert!(!panels.get("").terminal_open);
}

#[test]
fn session_panels_flags_are_chat_scoped() {
    let mut panels = SessionPanels::default();
    assert!(panels.toggle_terminal("a"));
    assert!(panels.get("a").terminal_open);
    assert!(!panels.get("b").terminal_open);
    assert!(!panels.get("").terminal_open);
    assert!(panels.toggle_changes("b"));
    assert!(panels.get("b").changes_open);
    assert!(!panels.get("b").terminal_open);
    assert!(!panels.get("a").changes_open);
    assert!(panels.get("a").terminal_open);
    assert!(!panels.toggle_terminal("a"));
    assert!(!panels.get("a").terminal_open);
}

#[test]
fn session_panels_both_flags_coexist_per_chat() {
    let mut panels = SessionPanels::default();
    panels.toggle_terminal("a");
    panels.toggle_changes("a");
    assert_eq!(
        panels.get("a"),
        ChatPanels {
            terminal_open: true,
            changes_open: true,
            ..Default::default()
        }
    );
    assert_eq!(panels.get("b"), ChatPanels::default());
    assert!(!panels.toggle_changes("a"));
    assert!(!panels.get("a").changes_open);
}

#[test]
fn session_panels_update_tracks_right_surfaces() {
    let mut panels = SessionPanels::default();
    panels.update("a", |p| p.right_active = RightSurface::Diff(3));
    assert_eq!(panels.get("a").right_active, RightSurface::Diff(3));
    assert_eq!(panels.get("b").right_active, RightSurface::Shared);
    panels.update("a", |p| p.right_active = RightSurface::Terminal(7));
    assert_eq!(panels.get("a").right_active, RightSurface::Terminal(7));
    panels.remove("a");
    assert_eq!(panels.get("a"), ChatPanels::default());
}

#[test]
fn sections_are_distinct_and_labelled() {
    let mut seen = std::collections::HashSet::new();
    for section in SettingsSection::ALL {
        assert!(!section.label().is_empty());
        assert!(seen.insert(section.label()));
    }
}

#[test]
fn tabs_cycle_module_is_wired() {
    // A light contract check that the re-exported helpers exist and agree.
    let order = vec!["a".to_string(), "b".to_string()];
    assert_eq!(
        tabs::cycle_target(&order, Some("a"), true),
        Some("b".into())
    );
}
