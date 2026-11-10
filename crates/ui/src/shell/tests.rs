use super::panels::{
    PANE_RESIZE_HITBOX_TOP, conversation_width, right_pane_max_width, right_pane_takeover_width,
    right_panel_content_width, stable_panel_content_width,
};
use super::sidebar::{RESORT, resort_offsets};
use super::titlebar::{
    CLUSTER_BUTTONS_WIDTH, TITLEBAR_ACTION_EDGE_INSET, TITLEBAR_ACTION_SLOT_WIDTH,
    TITLEBAR_CLUSTER_PAD, TITLEBAR_CONTROL_GAP, TITLEBAR_GROUP_GAP, TITLEBAR_IDENTITY_GAP,
    caption_buttons_width, cluster_buttons_start, cluster_clearance, titlebar_cluster_start,
    titlebar_new_session_alpha, titlebar_right_padding, titlebar_spacer_width,
};
use super::{ChatPanels, NavEntry, NavHistory, RightSurface, SessionPanels, SettingsSection, tabs};
use crate::icons;
use crate::motion;
use crate::settings::{CHAT_PANEL_MIN, platform_combo};
use crate::theme::Theme;
use gpui::Keystroke;

#[test]
fn every_default_shortcut_binds_on_this_platform() {
    // `apply_keymap` silently falls back on an unparseable combo, so a
    // default gpui cannot parse would ship as a dead shortcut.
    for id in crate::settings::ShortcutId::ALL {
        let combo = platform_combo(id.default_combo());
        assert!(
            Keystroke::parse(&combo).is_ok(),
            "{} default {combo:?} does not parse",
            id.label()
        );
    }
}

#[test]
fn right_pane_ceiling_preserves_the_chat_floor() {
    assert_eq!(right_pane_max_width(1200.0, 256.0), 644.0);
    assert_eq!(1200.0 - 256.0 - 644.0, CHAT_PANEL_MIN);
    // The chat floor wins over the right pane's preferred 360px minimum
    // when the whole window is unusually narrow.
    assert_eq!(right_pane_max_width(800.0, 256.0), 244.0);
    assert_eq!(800.0 - 256.0 - 244.0, CHAT_PANEL_MIN);
}

#[test]
fn right_pane_takeover_consumes_the_chat_column() {
    assert_eq!(right_pane_takeover_width(1200.0, 256.0), 944.0);
    assert_eq!(1200.0 - 256.0 - 944.0, 0.0);
}

#[test]
fn right_pane_takeover_control_reverses_direction() {
    assert_eq!(tabs::right_pane_expand_icon(false), icons::EXPAND_ARROWS);
    assert_eq!(tabs::right_pane_expand_icon(true), icons::COLLAPSE_ARROWS);
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
    assert_eq!(
        stable_panel_content_width(conversation, Some((takeover, conversation))),
        conversation
    );
}

#[test]
fn titlebar_cluster_matches_hearth_window_controls() {
    // hearth window-controls.tsx: `left: fullscreen ? 12 : 88` — the
    // cluster clears the {14,15} traffic lights, and reclaims the inset
    // when fullscreen hides them.
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
    // macOS, lights visible: spacer fills up to the 88px cluster start.
    assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
    assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
    assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
    // macOS fullscreen: the inset animates away (clamped at zero when the
    // strip's own padding already exceeds the 12px cluster start).
    assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
    assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
    // Linux / Windows: never any inset.
    assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
    assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
    assert_eq!(
        TITLEBAR_CLUSTER_PAD + titlebar_spacer_width(true, false, TITLEBAR_CLUSTER_PAD),
        titlebar_cluster_start(false),
        "the rendered row padding and spacer must land on the declared cluster start"
    );
}

#[test]
fn windows_caption_controls_reserve_titlebar_space() {
    assert_eq!(titlebar_right_padding(true, 0, 16.0), 124.0);
    assert_eq!(titlebar_right_padding(false, 0, 16.0), 16.0);
}

#[test]
fn linux_caption_controls_reserve_titlebar_space() {
    // 24px buttons on the cluster's 2px rhythm.
    assert_eq!(caption_buttons_width(0), 0.0);
    assert_eq!(caption_buttons_width(1), 24.0);
    assert_eq!(caption_buttons_width(3), 76.0);
    // Right-side captions (the Linux default: minimize,maximize,close):
    // content pads past the 10px edge inset + the button row.
    assert_eq!(titlebar_right_padding(false, 3, 16.0), 16.0 + 10.0 + 76.0);
    // GNOME-vanilla ":close" — a single right button.
    assert_eq!(titlebar_right_padding(false, 1, 16.0), 16.0 + 10.0 + 24.0);
    // Left-side captions ("close:…" layouts) shift the app cluster right
    // by the button row + one 2px gap.
    assert_eq!(cluster_buttons_start(false, false, 0), 10.0);
    assert_eq!(cluster_buttons_start(false, false, 1), 10.0 + 24.0 + 2.0);
    assert_eq!(cluster_buttons_start(false, false, 3), 10.0 + 76.0 + 2.0);
    // macOS ignores the Linux caption count entirely.
    assert_eq!(cluster_buttons_start(true, false, 3), 88.0);
}

#[test]
fn cluster_clearance_clears_the_overlay_buttons() {
    // Linux: buttons at 10..92; a 16px-padded header needs 84 more px to
    // put content at 92 + 8 breathing room.
    assert_eq!(cluster_clearance(false, false, 0, 16.0), 84.0);
    assert_eq!(cluster_clearance(false, false, 0, 10.0), 90.0);
    // Linux with a left-side close caption: everything shifts one slot.
    assert_eq!(cluster_clearance(false, false, 1, 16.0), 84.0 + 26.0);
    // macOS: buttons start at the 88px traffic-light cluster start.
    assert_eq!(
        cluster_clearance(true, false, 0, 16.0),
        88.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
    );
    // macOS fullscreen: cluster reclaims the inset (starts at 12).
    assert_eq!(
        cluster_clearance(true, true, 0, 16.0),
        12.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
    );
}

// ---- per-session panel flags (§1.10/1.11 parity: hearth sessionPanels) ----

#[test]
fn session_panels_default_closed_per_chat() {
    let panels = SessionPanels::default();
    assert_eq!(panels.get("a"), ChatPanels::default());
    // Everything closed until explicitly opened (user request — the
    // brief default-open popped the pane on every visited session).
    assert!(!panels.get("a").terminal_open);
    assert!(!panels.get("a").changes_open);
    assert_eq!(panels.get("a").right_active, RightSurface::Picker);
    // The new-chat canvas ("" key) is its own session, also closed.
    assert!(!panels.get("").terminal_open);
}

#[test]
fn session_panels_flags_are_chat_scoped() {
    let mut panels = SessionPanels::default();
    // Opening the terminal in chat A opens it ONLY in chat A.
    assert!(panels.toggle_terminal("a"));
    assert!(panels.get("a").terminal_open);
    assert!(!panels.get("b").terminal_open);
    assert!(!panels.get("").terminal_open);
    // Changes pane in B is independent of A's terminal.
    assert!(panels.toggle_changes("b"));
    assert!(panels.get("b").changes_open);
    assert!(!panels.get("b").terminal_open);
    assert!(!panels.get("a").changes_open);
    // Switching back to A restores A's state untouched.
    assert!(panels.get("a").terminal_open);
    // Toggling off round-trips.
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
    // The right pane round-trips back closed.
    assert!(!panels.toggle_changes("a"));
    assert!(!panels.get("a").changes_open);
}

#[test]
fn session_panels_update_tracks_right_surfaces() {
    let mut panels = SessionPanels::default();
    panels.update("a", |p| p.right_active = RightSurface::Diff(3));
    assert_eq!(panels.get("a").right_active, RightSurface::Diff(3));
    // Other chats keep the picker default.
    assert_eq!(panels.get("b").right_active, RightSurface::Picker);
    panels.update("a", |p| p.right_active = RightSurface::Terminal(7));
    assert_eq!(panels.get("a").right_active, RightSurface::Terminal(7));
}

// ---- sidebar resort FLIP diff (§1.6) ----

fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
    list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
}

#[test]
fn resort_offsets_empty_when_order_unchanged() {
    let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
    assert!(resort_offsets(&order, &order, 2.0).is_empty());
}

#[test]
fn resort_offsets_activity_moves_row_to_top() {
    // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
    // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
    // painted at +62 decaying to 0 (a glide UP into place). a and b shift
    // down by c's height + gap (31).
    let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
    let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
    let offsets = resort_offsets(&old, &new, 2.0);
    assert_eq!(offsets.get("c"), Some(&62.0));
    assert_eq!(offsets.get("a"), Some(&-31.0));
    assert_eq!(offsets.get("b"), Some(&-31.0));
}

#[test]
fn resort_offsets_respect_heights_and_gap() {
    // Tall row (45px) swaps with a short one (29px).
    let old = keys(&[("tall", 45.0), ("short", 29.0)]);
    let new = keys(&[("short", 29.0), ("tall", 45.0)]);
    let offsets = resort_offsets(&old, &new, 2.0);
    // short: old y 47 → new y 0; tall: old y 0 → new y 31.
    assert_eq!(offsets.get("short"), Some(&47.0));
    assert_eq!(offsets.get("tall"), Some(&-31.0));
}

#[test]
fn resort_offsets_ignore_added_and_removed_keys() {
    let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
    let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
    let offsets = resort_offsets(&old, &new, 2.0);
    // "new" has no old position (fades in instead); "gone" just goes.
    assert!(!offsets.contains_key("new"));
    assert!(!offsets.contains_key("gone"));
    // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
    // slot replaced by "new" of equal height — no move, no entry).
    assert_eq!(offsets.get("a"), Some(&-31.0));
    assert_eq!(offsets.get("b"), None);
}

#[test]
fn resort_glide_spec_matches_original() {
    // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
    assert_eq!(RESORT.duration_ms, 260);
    assert_eq!(RESORT.curve, motion::EASE_RESORT);
}

// ---- navigation history (titlebar back/forward) ----

fn chat(id: &str) -> NavEntry {
    NavEntry::Chat(id.to_string())
}

#[test]
fn nav_history_starts_with_nothing_to_walk() {
    let nav = NavHistory::new(chat(""));
    assert!(!nav.can_back());
    assert!(!nav.can_forward());
    assert_eq!(*nav.current(), chat(""));
}

#[test]
fn nav_push_then_back_and_forward() {
    let mut nav = NavHistory::new(chat("a"));
    nav.push(chat("b"));
    nav.push(NavEntry::Settings(SettingsSection::Devices));
    assert!(nav.can_back());
    assert!(!nav.can_forward());

    // Back walks toward the oldest entry without dropping anything.
    assert_eq!(
        nav.back(),
        Some(chat("b")),
        "back lands on the previous route"
    );
    assert_eq!(nav.back(), Some(chat("a")));
    assert!(!nav.can_back());
    assert!(nav.can_forward());
    assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

    // Forward retraces the same path.
    assert_eq!(nav.forward(), Some(chat("b")));
    assert_eq!(
        nav.forward(),
        Some(NavEntry::Settings(SettingsSection::Devices))
    );
    assert!(!nav.can_forward());
    assert_eq!(nav.forward(), None);
}

#[test]
fn nav_push_dedups_the_current_route() {
    let mut nav = NavHistory::new(chat("a"));
    nav.push(chat("a"));
    nav.push(chat("a"));
    assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
    nav.push(NavEntry::Settings(SettingsSection::Agents));
    nav.push(NavEntry::Settings(SettingsSection::Agents));
    assert_eq!(nav.len(), 2);
}

#[test]
fn nav_push_truncates_the_forward_branch() {
    // a → b → c, back to a, then push d: the b/c branch is gone (browser
    // semantics — hearth's memory history PUSH truncates entries ahead).
    let mut nav = NavHistory::new(chat("a"));
    nav.push(chat("b"));
    nav.push(chat("c"));
    nav.back();
    nav.back();
    assert_eq!(*nav.current(), chat("a"));
    assert!(nav.can_forward());
    nav.push(chat("d"));
    assert!(!nav.can_forward(), "the old branch is unreachable");
    assert_eq!(nav.len(), 2);
    assert_eq!(nav.back(), Some(chat("a")));
    assert_eq!(nav.forward(), Some(chat("d")));
}

#[test]
fn nav_replace_swaps_in_place() {
    // The boot auto-select replaces the untouched canvas entry, so Back
    // stays disabled after landing in the last-used chat.
    let mut nav = NavHistory::new(chat(""));
    nav.replace(chat("boot"));
    assert_eq!(nav.len(), 1);
    assert_eq!(*nav.current(), chat("boot"));
    assert!(!nav.can_back());
}

#[test]
fn nav_settings_sections_are_distinct_entries() {
    let mut nav = NavHistory::new(chat("a"));
    nav.push(NavEntry::Settings(SettingsSection::Devices));
    nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
    assert_eq!(nav.len(), 3, "section changes are navigations");
    assert_eq!(
        nav.back(),
        Some(NavEntry::Settings(SettingsSection::Devices))
    );
    assert_eq!(nav.back(), Some(chat("a")));
}
