//! Session navigation: sidebar filtering, Ctrl+Tab cycling and right-pane tab icons.
//!
//! The horizontal session tab strip is gone: the activity sidebar IS the session list, and the
//! titlebar names the selected session. `UiSettings.open_tabs` remains legacy.

use chrono::Utc;

use crate::icons;
use crate::state::App;

/// Whether a chat belongs in the sidebar's Sessions list under `filter` (`None` = All spaces).
pub fn in_space_filter(chat: &hearth_proto::Chat, filter: Option<&str>) -> bool {
    filter.is_none_or(|space_id| chat.space_id.as_deref() == Some(space_id))
}

/// The chat one step from `selected` in `order`, wrapping at both ends. With nothing selected
/// the new-session canvas enters at the end it would have wrapped to; a selection that has left
/// the list is treated the same way rather than dead-ending.
pub fn cycle_target(order: &[String], selected: Option<&str>, forward: bool) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let at = selected.and_then(|id| order.iter().position(|c| c == id));
    let next = match (at, forward) {
        (Some(at), true) => (at + 1) % order.len(),
        (Some(at), false) => (at + order.len() - 1) % order.len(),
        (None, true) => 0,
        (None, false) => order.len() - 1,
    };
    Some(order[next].clone())
}

/// The sidebar order cycling walks: every visible chat of a live space, filtered, most recent
/// first.
pub fn cycle_order(app: &App) -> Vec<String> {
    let filter = app.settings.space_filter.clone();
    let mut rows: Vec<&hearth_proto::Chat> = app
        .visible_chats()
        .filter(|chat| in_space_filter(chat, filter.as_deref()))
        .filter(|chat| match chat.space_id.as_deref() {
            None => true,
            Some(id) => app.space_row(id).is_some(),
        })
        .collect();
    let now = Utc::now();
    let _ = now;
    rows.sort_by(|a, b| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
    rows.into_iter().map(|chat| chat.id.clone()).collect()
}

/// Ctrl+Tab / Ctrl+Shift+Tab: step through the sidebar's Sessions list in the order it is
/// drawn. Chat-scoped: no cycling under Settings or the add-space palette.
pub fn cycle_session(app: &mut App, forward: bool) {
    if !app.shell.route.is_chat() || app.shell.add_space.is_some() {
        return;
    }
    let order = cycle_order(app);
    if let Some(target) = cycle_target(&order, app.selected_chat.as_deref(), forward) {
        super::open_chat(app, target);
    }
}

/// The right pane's expand/restore glyph.
pub fn right_pane_expand_icon(expanded: bool) -> &'static rune::Icon {
    if expanded {
        &icons::COLLAPSE_ARROWS
    } else {
        &icons::EXPAND_ARROWS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn chat(id: &str, space_id: Option<&str>) -> hearth_proto::Chat {
        hearth_proto::Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: space_id.map(Into::into),
            last_seen_at: None,
            room_gen: None,
        }
    }

    #[test]
    fn steps_forward_and_back_through_the_list() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, Some("a"), true).as_deref(), Some("b"));
        assert_eq!(cycle_target(&list, Some("b"), true).as_deref(), Some("c"));
        assert_eq!(cycle_target(&list, Some("c"), false).as_deref(), Some("b"));
        assert_eq!(cycle_target(&list, Some("b"), false).as_deref(), Some("a"));
    }

    #[test]
    fn wraps_at_both_ends() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, Some("c"), true).as_deref(), Some("a"));
        assert_eq!(cycle_target(&list, Some("a"), false).as_deref(), Some("c"));
    }

    #[test]
    fn a_single_session_cycles_to_itself() {
        let list = order(&["only"]);
        assert_eq!(
            cycle_target(&list, Some("only"), true).as_deref(),
            Some("only")
        );
        assert_eq!(
            cycle_target(&list, Some("only"), false).as_deref(),
            Some("only")
        );
    }

    #[test]
    fn no_selection_enters_the_list_from_the_matching_end() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, None, true).as_deref(), Some("a"));
        assert_eq!(cycle_target(&list, None, false).as_deref(), Some("c"));
        assert_eq!(
            cycle_target(&list, Some("gone"), true).as_deref(),
            Some("a")
        );
        assert_eq!(
            cycle_target(&list, Some("gone"), false).as_deref(),
            Some("c")
        );
    }

    #[test]
    fn an_empty_list_has_nothing_to_select() {
        assert_eq!(cycle_target(&[], None, true), None);
        assert_eq!(cycle_target(&[], Some("a"), true), None);
    }

    #[test]
    fn cycling_honors_the_sidebar_space_filter() {
        let mine = chat("mine", Some("space-1"));
        let theirs = chat("theirs", Some("space-2"));
        let loose = chat("loose", None);
        assert!(in_space_filter(&mine, Some("space-1")));
        assert!(!in_space_filter(&theirs, Some("space-1")));
        assert!(!in_space_filter(&loose, Some("space-1")));
        for chat in [&mine, &theirs, &loose] {
            assert!(in_space_filter(chat, None));
        }
    }

    #[test]
    fn expand_icon_reverses_direction() {
        assert_eq!(
            right_pane_expand_icon(false).name,
            icons::EXPAND_ARROWS.name
        );
        assert_eq!(
            right_pane_expand_icon(true).name,
            icons::COLLAPSE_ARROWS.name
        );
    }
}
