//! Settings → Archived: archived chats across devices, with Unarchive
//! (`Mutate` `setChatArchived` false).
//!
//! Ported from `crates/ui/src/settings/archived.rs`; the gpui entity/task pair
//! becomes page state plus a bridge job drained by [`ArchivedPage::poll`].

use std::sync::{Arc, Mutex};

use hearth_proto::Chat;
use hearth_rpc::methods;
use rune::{El, TextStyle, Tx, col, el, row, text};

use crate::icons;
use crate::state::{App, chat_location, format_time_ago};
use crate::theme::Theme;

use super::widgets;

/// Archived rows in sidebar (recency) order. Pure.
pub fn archived_chats(chats: &[Chat]) -> Vec<&Chat> {
    chats.iter().filter(|c| c.archived).collect()
}

type UnarchiveSlot = Arc<Mutex<Option<(u64, Result<(), String>)>>>;

/// View state for the Archived page.
#[derive(Debug, Default)]
pub struct ArchivedPage {
    pub error: Option<String>,
    /// Chat with an in-flight unarchive (the button shows its working state).
    pub busy: Option<String>,
    /// Row index under the pointer — drives the Unarchive reveal.
    pub hovered: Option<usize>,
    result: UnarchiveSlot,
    generation: u64,
}

impl ArchivedPage {
    /// Drain a finished unarchive.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some((generation, result)) = self.result.lock().unwrap().take()
            && generation == self.generation
        {
            self.busy = None;
            if let Err(message) = result {
                self.error = Some(format!("Unarchive failed: {message}"));
            }
            changed = true;
        }
        changed
    }
}

/// Unarchive one chat.
pub fn unarchive(app: &mut App, chat_id: String) {
    let page = &mut app.settings_ui.archived;
    page.busy = Some(chat_id.clone());
    page.error = None;
    page.generation = page.generation.wrapping_add(1);
    let generation = page.generation;
    let slot = page.result.clone();
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        page.busy = None;
        page.error = Some("Unarchive failed: Not connected to the engine".to_string());
        return;
    };
    let bridge = app.bridge.clone();
    let params = serde_json::json!({
        "op": "setChatArchived",
        "chatId": chat_id,
        "archived": false,
    });
    app.bridge.spawn(async move {
        let result = client
            .call(methods::MUTATE, params)
            .await
            .map(|_| ())
            .map_err(|err| err.to_string());
        *slot.lock().unwrap() = Some((generation, result));
        bridge.wake();
    });
}

fn set_hovered(app: &mut App, ix: usize, hovered: bool) {
    let page = &mut app.settings_ui.archived;
    if hovered {
        page.hovered = Some(ix);
    } else if page.hovered == Some(ix) {
        page.hovered = None;
    }
}

fn dismiss_error(app: &mut App) {
    app.settings_ui.archived.error = None;
}

fn archived_row(
    page: &ArchivedPage,
    ix: usize,
    chat: &Chat,
    device_names: &std::collections::HashMap<String, String>,
    now: chrono::DateTime<chrono::Utc>,
    theme: &Theme,
) -> El {
    let title = chat
        .title
        .clone()
        .unwrap_or_else(|| "Untitled session".to_string());
    let device = device_names.get(&chat.device_id).cloned();
    let time_ago = format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now);
    let location = chat_location(chat);
    let is_busy = page.busy.as_deref() == Some(chat.id.as_str());
    let row_hovered = page.hovered == Some(ix);
    let chat_id = chat.id.clone();

    let mut meta = row().mt(2.0).gap(6.0).items_center();
    let both = device.is_some() && location.is_some();
    if let Some(device) = device {
        meta = meta.child(
            text(device)
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted.alpha(0.55)),
        );
    }
    if both {
        meta = meta.child(
            text("·")
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted.alpha(0.55)),
        );
    }
    if let Some(location) = location {
        meta = meta.child(
            text(location)
                .min_w(0.0)
                .truncate()
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted.alpha(0.55)),
        );
    }

    row()
        .key(format!("archived-row-{ix}"))
        .gap(12.0)
        .items_center()
        .radius(8.0)
        .px(12.0)
        .py(8.0)
        .hover(theme.ink(0.03))
        .on_hover_change(move |app: &mut App, hovered, _tx| set_hovered(app, ix, hovered))
        .child(
            el().w(32.0)
                .h(32.0)
                .radius(6.0)
                .border(1.0, theme.border)
                .center()
                .child(
                    icons::icon(&icons::ARCHIVE_MINIMALISTIC, 16.0)
                        .color(theme.text_muted.alpha(0.6)),
                ),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(
                    row()
                        .gap(8.0)
                        .items_center()
                        .child(
                            text(title)
                                .min_w(0.0)
                                .truncate()
                                .font(TextStyle::new(13.0, 500.0))
                                .color(theme.text),
                        )
                        .child(
                            text(time_ago)
                                .font(TextStyle::new(11.0, 400.0))
                                .color(theme.text_muted.alpha(0.5)),
                        ),
                )
                .child(meta),
        )
        .child(
            row()
                .gap(6.0)
                .items_center()
                .px(10.0)
                .py(4.0)
                .radius(6.0)
                .border(1.0, theme.border)
                .fill(theme.ink(0.0))
                .hover(theme.surface_raised)
                .cursor(rune::Cursor::Pointer)
                .opacity(if row_hovered || is_busy { 1.0 } else { 0.0 })
                .child(icons::icon(&icons::ARCHIVE_UP_MINIMALISTIC, 14.0).color(theme.text_muted))
                .child(
                    text(if is_busy {
                        "Unarchiving…"
                    } else {
                        "Unarchive"
                    })
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_muted),
                )
                .on_press(move |app: &mut App| unarchive(app, chat_id.clone())),
        )
}

/// The Archived page body.
pub fn render(page: &ArchivedPage, app: &App, theme: &Theme) -> El {
    let now = chrono::Utc::now();
    let rows: Vec<Chat> = archived_chats(&app.chats).into_iter().cloned().collect();
    let device_names: std::collections::HashMap<String, String> = app
        .devices
        .iter()
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect();
    let count = rows.len();

    let body = if rows.is_empty() {
        col()
            .mt(96.0)
            .items_center()
            .child(
                icons::icon(&icons::ARCHIVE_MINIMALISTIC, 28.0).color(theme.text_muted.alpha(0.2)),
            )
            .child(
                text("Nothing archived")
                    .mt(12.0)
                    .font(TextStyle::new(14.0, 400.0))
                    .color(theme.text_muted.alpha(0.5)),
            )
            .child(
                text("Right-click a session in the sidebar to archive it.")
                    .mt(4.0)
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_muted.alpha(0.4)),
            )
    } else {
        let mut list = col().mt(24.0).gap(2.0);
        for (ix, chat) in rows.iter().enumerate() {
            list = list.child(archived_row(page, ix, chat, &device_names, now, theme));
        }
        list
    };

    let mut content = widgets::page_column()
        .child(widgets::page_header(
            theme,
            "Archived sessions",
            (count > 0).then_some(count),
        ))
        .child(widgets::page_subtitle(
            theme,
            "Hidden from the sidebar, never deleted. Unarchiving puts a session back on its device.",
        ));
    if let Some(message) = page.error.clone() {
        content = content.child(
            widgets::error_strip(theme, message)
                .cursor(rune::Cursor::Pointer)
                .on_press(|app: &mut App| dismiss_error(app)),
        );
    }
    content = content.child(body);

    widgets::page_root("archived-page").child(content)
}

/// No page-level key handling.
pub fn on_key(_app: &mut App, _ev: &rune::KeyEvent, _tx: &mut Tx) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn chat(id: &str, archived: bool) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            title: None,
            archived,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    #[test]
    fn only_archived_rows_show() {
        let chats = vec![chat("a", false), chat("b", true), chat("c", true)];
        let rows = archived_chats(&chats);
        let ids: Vec<&str> = rows.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }
}
