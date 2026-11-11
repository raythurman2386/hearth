//! Sidebar: the space filter, the session list, the archived accordion, the connection pill,
//! the update strip and the user menu.
//!
//! Rows are hearth's: 61 points, a muted "project @ device" line, the title, then a status/time
//! corner that swaps to Archive under the pointer and a harness/branch/spinner meta line. Hover
//! swaps ride Rune hover groups, so moving the pointer doesn't commit the view.

use chrono::Utc;
use rune::{Cursor, El, ScrollSpec, TextStyle, col, el, icon, row, text};

use crate::icons;
use crate::loaders;
use crate::state::{App, format_time_ago};
use crate::theme::Theme;
use hearth_proto::ChatIndicator;

use super::spaces;
use super::titlebar::harness_mark;
use super::{ChatMenu, open_chat, set_chat_archived};

/// Sidebar row height for the resort FLIP estimate.
pub const SIDEBAR_ROW_GAP: f32 = 2.0;
/// Ramp height of the sidebar's scroll-edge fade.
const SIDEBAR_FADE_BAND: f32 = 32.0;

/// FLIP diff for a keyed list: each surviving key's paint-only start offset `old_y - new_y`.
pub fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// Chat-mode vs settings-mode sidebar.
pub fn render(app: &App, theme: &Theme, layer: &mut Vec<El>) -> El {
    match app.shell.route {
        super::Route::Settings(section) => render_settings_nav(app, section, theme),
        super::Route::Chat => render_chat_sidebar(app, theme, layer),
    }
}

fn render_settings_nav(_app: &App, section: crate::settings::SettingsSection, theme: &Theme) -> El {
    let mut nav =
        col()
            .w_full()
            .h_full()
            .child(
                col()
                    .grow(1.0)
                    .min_h(0.0)
                    .px(Theme::SPACE_SM)
                    .child(
                        text("Settings")
                            .font(TextStyle::new(11.0, 500.0))
                            .color(theme.text_muted.alpha(0.6))
                            .px(Theme::SPACE_SM)
                            .pt(12.0)
                            .pb(4.0),
                    )
                    .child(
                        col().gap(2.0).children(
                            crate::settings::SettingsSection::ALL
                                .into_iter()
                                .map(|item| {
                                    let selected = item == section;
                                    row()
                                        .key(format!("settings-nav-{}", item.label()))
                                        .w_full()
                                        .gap(8.0)
                                        .radius(8.0)
                                        .px(Theme::SPACE_SM)
                                        .py(6.0)
                                        .center_y()
                                        .cursor(Cursor::Pointer)
                                        .fill(if selected {
                                            crate::theme::glass_selected_bg(theme.appearance)
                                        } else {
                                            theme.ink(0.0)
                                        })
                                        .hover(theme.glass_hover())
                                        .on_press(move |app: &mut App| {
                                            super::open_settings(app, item)
                                        })
                                        .child(
                                            icon(super::routes::section_icon(item), 16.0)
                                                .color(theme.text_muted),
                                        )
                                        .child(
                                            text(item.label())
                                                .font(TextStyle::new(
                                                    13.0,
                                                    if selected { 500.0 } else { 400.0 },
                                                ))
                                                .color(if selected {
                                                    theme.text
                                                } else {
                                                    theme.text_muted
                                                }),
                                        )
                                }),
                        ),
                    ),
            )
            .child(
                el().px(Theme::SPACE_SM).pb(12.0).child(
                    row()
                        .key("settings-back")
                        .w_full()
                        .gap(6.0)
                        .radius(8.0)
                        .px(Theme::SPACE_SM)
                        .py(6.0)
                        .center_y()
                        .cursor(Cursor::Pointer)
                        .hover(theme.glass_hover())
                        .on_press(|app: &mut App| super::close_settings(app))
                        .child(icon(&icons::ALT_ARROW_LEFT, 16.0).color(theme.text_muted))
                        .child(
                            text("Back")
                                .font(TextStyle::new(13.0, 400.0))
                                .color(theme.text_muted),
                        ),
                ),
            );
    nav = nav.pt(Theme::TITLEBAR_HEIGHT);
    nav
}

fn render_chat_sidebar(app: &App, theme: &Theme, layer: &mut Vec<El>) -> El {
    let filter_row = spaces::render_filter(app, theme, layer);
    let rows = active_rows(app, theme);
    let archived = render_archived(app, theme);
    let connection = render_connection_pill(app, theme);
    let update = render_update_strip(app, theme);
    let notice = app.shell.sidebar_notice.clone();
    let user_menu = super::overlays::render_user_menu(app, theme, layer);

    let empty = rows.is_empty();
    let list = col()
        .key("sidebar-lists")
        .w_full()
        .h_full()
        .px(Theme::SPACE_SM)
        .pt(4.0)
        .child(if empty {
            el().px(Theme::SPACE_SM).pb(Theme::SPACE_SM).child(
                text("No sessions yet")
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_faint),
            )
        } else {
            col()
                .w_full()
                .gap(SIDEBAR_ROW_GAP)
                .pb(Theme::SPACE_SM)
                .children(rows)
        })
        .children(archived);

    col()
        .w_full()
        .h_full()
        .pt(Theme::TITLEBAR_HEIGHT)
        .child(filter_row)
        .child(
            el().w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_edge_fade(SIDEBAR_FADE_BAND)
                .child(list),
        )
        .children(connection)
        .children(update)
        .children(notice.map(|message| {
            el().key("sidebar-notice")
                .mx(Theme::SPACE_SM)
                .mb(Theme::SPACE_SM)
                .px(Theme::SPACE_SM)
                .py(4.0)
                .radius(Theme::CONTROL_RADIUS)
                .border(1.0, theme.danger)
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| super::dismiss_notice(app))
                .child(
                    text(message)
                        .font(TextStyle::new(11.0, 400.0))
                        .color(theme.danger)
                        .wrap(),
                )
        }))
        .child(el().padding(Theme::SPACE_SM).child(user_menu))
}

/// The underlying status label and colour for a chat row.
fn row_status(
    app: &App,
    chat: &hearth_proto::Chat,
    now: chrono::DateTime<Utc>,
) -> (ChatIndicator, String) {
    let queued = app.send_queued(&chat.id, now);
    let undelivered = app.send_pending(&chat.id, now) && !queued;
    let status = crate::state::display_status(chat, app.session_for(&chat.id), now);
    if undelivered {
        (ChatIndicator::Errored, "Failed".to_string())
    } else if queued {
        (ChatIndicator::Working, "Queued".to_string())
    } else {
        let label = match status {
            ChatIndicator::Working => "Working",
            ChatIndicator::AwaitingInput => "Input",
            ChatIndicator::Errored => "Failed",
            ChatIndicator::Completed => "Done",
            ChatIndicator::Idle => "",
        };
        (status, label.to_string())
    }
}

fn active_rows(app: &App, theme: &Theme) -> Vec<El> {
    let now = Utc::now();
    let filter = app.settings.space_filter.clone();
    let mut rows: Vec<&hearth_proto::Chat> = app
        .visible_chats()
        .filter(|chat| super::in_space_filter(chat, filter.as_deref()))
        .filter(|chat| match chat.space_id.as_deref() {
            None => true,
            Some(id) => app.space_row(id).is_some(),
        })
        .collect();
    rows.sort_by(|a, b| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
    rows.into_iter()
        .map(|chat| chat_row(app, chat, now, theme))
        .collect()
}

fn chat_row(app: &App, chat: &hearth_proto::Chat, now: chrono::DateTime<Utc>, theme: &Theme) -> El {
    let id = chat.id.clone();
    let selected = app.selected_chat.as_deref() == Some(chat.id.as_str());
    let (status, label) = row_status(app, chat, now);
    let time_ago = format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now);
    let folder = match chat.space_id.as_deref().and_then(|id| app.space_row(id)) {
        Some(space) => space.display_name().to_string(),
        None if chat.space_id.is_none() => "~".to_string(),
        None => "?".to_string(),
    };
    let folder = match app.device_name(&chat.device_id) {
        Some(device) => format!("{folder} @ {device}"),
        None => folder,
    };
    let title = chat
        .title
        .clone()
        .unwrap_or_else(|| "New session".to_string());
    let branch = chat
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);
    let harness = chat.config.as_ref().map(|c| c.harness);
    let working = status == ChatIndicator::Working && label != "Queued";
    let status_color = spaces::status_dot_color(status, theme);
    let subline = theme.text_muted.alpha(0.5);

    // Status corner at rest; the archive pill on row hover. Both states overlay the same
    // 74-point slot (absolute children), so the row's first line keeps a stable width and the
    // status text never runs under the pill.
    const CORNER_W: f32 = 74.0;
    let status_row = row()
        .absolute(0.0, 0.0)
        .w(CORNER_W)
        .justify_end()
        .gap(4.0)
        .center_y()
        .group_opacity(1.0, 0.0)
        .children(if label.is_empty() {
            vec![
                text(time_ago)
                    .font(TextStyle::new(10.0, 500.0))
                    .color(theme.text_muted),
            ]
        } else {
            vec![
                if status == ChatIndicator::Completed {
                    icon(&icons::CHECK, 11.0).color(status_color)
                } else {
                    el().size(6.0, 6.0).radius(3.0).fill(status_color)
                },
                text(label.clone())
                    .font(TextStyle::new(10.0, 500.0))
                    .color(status_color),
            ]
        });
    let archive_id = id.clone();
    let archive = row()
        .absolute(0.0, 0.0)
        .w(CORNER_W)
        .justify_end()
        .gap(4.0)
        .center_y()
        .h(18.0)
        .px(4.0)
        .radius(5.0)
        .fill(theme.ink(0.10))
        .hover(theme.ink(0.18))
        .group_opacity(0.0, 1.0)
        .cursor(Cursor::Pointer)
        .on_press(move |app: &mut App| set_chat_archived(app, archive_id.clone(), true))
        .child(icon(&icons::ARCHIVE_MINIMALISTIC, 11.0).color(theme.text_muted))
        .child(
            text("Archive")
                .font(TextStyle::new(10.0, 400.0))
                .color(theme.text_muted),
        );
    let corner = el()
        .key(format!("chat-corner-{id}"))
        .h(14.0)
        .w(CORNER_W)
        .child(status_row)
        .child(archive);

    let mut meta = row()
        .w_full()
        .gap(4.0)
        .center_y()
        .children(harness.map(|harness| {
            let (mark, tint) = harness_mark(harness, theme);
            icon(mark, 11.0).color(tint.alpha(0.8))
        }));
    if let Some(branch) = branch {
        meta = meta
            .child(icon(&icons::GIT_BRANCH, 11.0).color(subline))
            .child(
                text(branch)
                    .font(TextStyle::new(11.0, 400.0).lh(14.0))
                    .color(subline)
                    .truncate(),
            );
    }
    meta = meta.child(el().grow(1.0));
    if working {
        meta = meta.child(loaders::mini_gradient_spinner(theme, 2.0));
    }

    let select_id = id.clone();
    let menu_id = id.clone();
    col()
        .key(format!("chat-{id}"))
        .w_full()
        .h(super::CHAT_ROW_HEIGHT)
        .gap(2.0)
        .radius(8.0)
        .px(Theme::SPACE_SM)
        .py(6.0)
        .cursor(Cursor::Pointer)
        .hover_group()
        .fill(if selected {
            crate::theme::glass_selected_bg(theme.appearance)
        } else {
            theme.ink(0.0)
        })
        .hover(if selected {
            crate::theme::glass_selected_bg(theme.appearance)
        } else {
            theme.glass_hover()
        })
        .on_press(move |app: &mut App| open_chat(app, select_id.clone()))
        .on_secondary_press(move |app: &mut App, press: rune::PointerPress, _| {
            app.shell.chat_menu = Some(ChatMenu {
                chat_id: menu_id.clone(),
                x: press.x,
                y: press.y,
            });
        })
        .child(
            row()
                .w_full()
                .gap(Theme::SPACE_SM)
                .center_y()
                .child(
                    text(folder)
                        .font(TextStyle::new(11.0, 400.0).lh(14.0))
                        .color(subline)
                        .truncate()
                        .grow(1.0),
                )
                .child(corner),
        )
        .child(
            text(title)
                .font(TextStyle::new(13.0, 400.0).lh(17.0))
                .color(if selected {
                    theme.text
                } else {
                    theme.text.alpha(0.8)
                })
                .truncate()
                .w_full(),
        )
        .child(meta)
}

fn render_archived(app: &App, theme: &Theme) -> Option<El> {
    const INITIAL: usize = 10;
    const PAGE: usize = 25;
    let now = Utc::now();
    let filter = app.settings.space_filter.clone();
    let rows: Vec<&hearth_proto::Chat> = app
        .chats
        .iter()
        .filter(|c| c.archived)
        .filter(|chat| match &filter {
            Some(space_id) => chat.space_id.as_deref() == Some(space_id.as_str()),
            None => true,
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    let total = rows.len();
    let open = app.shell.archived_open;
    let shown = app.shell.archived_shown.max(INITIAL);
    let label = if open {
        "Archived".to_string()
    } else {
        format!("Archived ({total})")
    };
    let header = row()
        .key("archived-toggle")
        .w_full()
        .gap(8.0)
        .center_y()
        .mt(12.0)
        .mb(4.0)
        .px(10.0)
        .cursor(Cursor::Pointer)
        .on_press(|app: &mut App| {
            app.shell.archived_open = !app.shell.archived_open;
            app.shell.archived_shown = INITIAL;
        })
        .child(
            text(label)
                .font(TextStyle::new(12.0, 500.0))
                .color(theme.text_muted.alpha(0.5)),
        )
        .child(el().h(1.0).grow(1.0).fill(theme.border.alpha(0.6)))
        .child(
            icon(
                if open {
                    &icons::ALT_ARROW_DOWN
                } else {
                    &icons::ALT_ARROW_RIGHT
                },
                12.0,
            )
            .color(theme.text_muted.alpha(0.5)),
        );

    let mut section = col().w_full().child(header);
    if open {
        let selected = app.selected_chat.clone();
        let mut list = col().w_full().gap(SIDEBAR_ROW_GAP);
        for chat in rows.into_iter().take(shown) {
            let id = chat.id.clone();
            let is_selected = selected.as_deref() == Some(id.as_str());
            let title = chat
                .title
                .clone()
                .unwrap_or_else(|| "New session".to_string());
            let time_ago = format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now);
            let (mark, tint) = chat
                .config
                .as_ref()
                .map(|c| harness_mark(c.harness, theme))
                .unwrap_or((&icons::CHAT_ROUND_LINE, theme.text_muted));
            let restore_id = id.clone();
            let open_id = id.clone();
            let menu_id = id.clone();
            list = list.child(
                row()
                    .key(format!("archived-{id}"))
                    .w_full()
                    .h(36.0)
                    .gap(10.0)
                    .center_y()
                    .px(10.0)
                    .radius(6.0)
                    .cursor(Cursor::Pointer)
                    .hover_group()
                    .fill(if is_selected {
                        crate::theme::glass_selected_bg(theme.appearance)
                    } else {
                        theme.ink(0.0)
                    })
                    .hover(theme.glass_hover())
                    .on_press(move |app: &mut App| open_chat(app, open_id.clone()))
                    .on_secondary_press(move |app: &mut App, press: rune::PointerPress, _| {
                        app.shell.chat_menu = Some(ChatMenu {
                            chat_id: menu_id.clone(),
                            x: press.x,
                            y: press.y,
                        });
                    })
                    .child(icon(mark, 14.0).color(tint.alpha(0.45)))
                    .child(
                        text(title)
                            .font(TextStyle::new(13.0, 400.0))
                            .color(theme.text.alpha(0.6))
                            .truncate()
                            .grow(1.0),
                    )
                    .child(
                        text(time_ago)
                            .font(TextStyle::new(11.0, 400.0))
                            .color(theme.text_muted.alpha(0.55))
                            .group_opacity(1.0, 0.0),
                    )
                    .child(
                        row()
                            .gap(4.0)
                            .center_y()
                            .h(18.0)
                            .px(4.0)
                            .mr(-4.0)
                            .radius(5.0)
                            .fill(theme.ink(0.10))
                            .hover(theme.ink(0.18))
                            .group_opacity(0.0, 1.0)
                            .cursor(Cursor::Pointer)
                            .on_press(move |app: &mut App| {
                                set_chat_archived(app, restore_id.clone(), false)
                            })
                            .child(
                                icon(&icons::ARCHIVE_UP_MINIMALISTIC, 11.0).color(theme.text_muted),
                            )
                            .child(
                                text("Unarchive")
                                    .font(TextStyle::new(10.0, 400.0))
                                    .color(theme.text_muted),
                            ),
                    ),
            );
        }
        section = section.child(list);
        if total > shown {
            let remaining = (total - shown).min(PAGE);
            section = section.child(
                row()
                    .key("archived-more")
                    .w_full()
                    .mt(2.0)
                    .h(36.0)
                    .gap(10.0)
                    .center_y()
                    .px(10.0)
                    .radius(6.0)
                    .cursor(Cursor::Pointer)
                    .hover(theme.glass_hover())
                    .on_press(|app: &mut App| {
                        app.shell.archived_shown = app.shell.archived_shown.max(INITIAL) + PAGE;
                    })
                    .child(icon(&icons::PLUS, 14.0).color(theme.text_muted))
                    .child(
                        text(format!("Show {remaining} more"))
                            .font(TextStyle::new(13.0, 400.0))
                            .color(theme.text_muted.alpha(0.55)),
                    ),
            );
        }
    }
    Some(section.pb(Theme::SPACE_SM))
}

fn render_connection_pill(app: &App, theme: &Theme) -> Option<El> {
    use hearth_proto::ConnectivityState as S;
    let (label, glyph): (&str, El) = match app.connectivity.state {
        S::Disabled | S::Connected => return None,
        S::Offline => (
            "Offline — sends are saved",
            el().size(5.0, 5.0).radius(2.5).fill(theme.warning),
        ),
        S::Reconnecting => (
            "Reconnecting…",
            loaders::mini_mono_spinner(theme.text_muted, 2.0),
        ),
    };
    Some(
        row()
            .key("connection-pill")
            .mx(Theme::SPACE_SM + 4.0)
            .mb(Theme::SPACE_SM)
            .gap(6.0)
            .center_y()
            .child(glyph)
            .child(
                text(label)
                    .font(TextStyle::new(11.0, 400.0))
                    .color(theme.text_faint)
                    .truncate(),
            ),
    )
}

fn render_update_strip(app: &App, theme: &Theme) -> Option<El> {
    let status = app.update.as_ref()?;
    if !status.update_available {
        return None;
    }
    let latest = status.latest_version.clone()?;
    if app.shell.update_dismissed.as_deref() == Some(latest.as_str()) {
        return None;
    }
    let label = format!("Update available — v{latest}");
    Some(
        row()
            .key("update-strip")
            .mx(Theme::SPACE_SM)
            .px(Theme::SPACE_SM)
            .py(6.0)
            .radius(Theme::CONTROL_RADIUS)
            .fill(theme.accent.alpha(0.14))
            .cursor(Cursor::Pointer)
            .hover(theme.accent.alpha(0.22))
            .on_press(move |app: &mut App| {
                super::dismiss_update(app, latest.clone());
            })
            .child(
                text(label)
                    .font(TextStyle::new(11.0, 500.0))
                    .color(theme.accent)
                    .truncate()
                    .grow(1.0),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_is_260ms() {
        assert_eq!(crate::motion::RESORT_SPEC.duration_ms, 260);
        assert_eq!(crate::motion::RESORT_SPEC.curve, crate::motion::EASE_RESORT);
    }
}
