//! Shell overlays: the user menu, the session context menu, the rename/delete dialogs and the
//! jump-to-bottom pill.
//!
//! Every floating surface goes into the view's overlay layer (`rune_kit::root(body, layer)`), so
//! focus, Escape and outside presses are Rune's modal stack and the cards paint above every
//! pane. The app owns only `open` and the selected ids.

use rune::{Cursor, El, TextStyle, col, el, icon, row, text};
use rune_kit::Placement;

use crate::icons;
use crate::state::App;
use crate::theme::Theme;

use super::{
    RightSurface, delete_chat, open_rename_chat, open_settings, resolved_right_active,
    set_chat_archived, submit_rename_chat,
};

/// Sidebar identity and account menu: the runtime's fixed data boundary, then Settings.
pub fn render_user_menu(app: &App, theme: &Theme, layer: &mut Vec<El>) -> El {
    let (user_line, subline, identity) = match app.workspace_scope {
        Some(hearth_proto::WorkspaceScope::Local) => (
            "Local only".to_string(),
            None,
            "Stored on this device".to_string(),
        ),
        Some(hearth_proto::WorkspaceScope::Development) | None => (
            "Development".to_string(),
            Some("Local development runtime"),
            "Authentication disabled".to_string(),
        ),
    };
    let initial = user_line
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    let open = app.shell.user_menu_open;

    let trigger = row()
        .key("user-menu")
        .w_full()
        .radius(8.0)
        .px(Theme::SPACE_SM)
        .py(Theme::SPACE_SM)
        .gap(10.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .fill(if open {
            theme.glass_hover()
        } else {
            theme.ink(0.0)
        })
        .hover(theme.glass_hover())
        .on_press(|app: &mut App| {
            app.shell.user_menu_open = !app.shell.user_menu_open;
            app.shell.spaces_menu = None;
        })
        .child(
            el().size(28.0, 28.0)
                .radius(14.0)
                .center()
                .fill(theme.text)
                .child(
                    text(initial)
                        .font(TextStyle::new(12.0, 600.0))
                        .color(theme.bg),
                ),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(
                    text(user_line)
                        .font(TextStyle::new(13.0, 500.0).lh(17.0))
                        .color(theme.text)
                        .truncate(),
                )
                .children(subline.map(|line| {
                    text(line.to_string())
                        .font(TextStyle::new(11.0, 400.0).lh(15.0))
                        .color(theme.text_muted)
                })),
        );

    let body = col()
        .w((app.settings.sidebar_width - 2.0 * Theme::SPACE_SM).max(160.0))
        .gap(2.0)
        .child(
            text(identity)
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted.alpha(0.7))
                .truncate()
                .px(8.0)
                .pt(6.0)
                .pb(4.0),
        )
        .child(
            row()
                .key("user-menu-settings")
                .w_full()
                .h(30.0)
                .px(8.0)
                .radius(6.0)
                .gap(8.0)
                .center_y()
                .cursor(Cursor::Pointer)
                .hover(theme.element_hover)
                .on_press(|app: &mut App| {
                    app.shell.user_menu_open = false;
                    open_settings(app, crate::settings::SettingsSection::Devices);
                })
                .child(icon(&icons::SETTINGS_MINIMALISTIC, 16.0).color(theme.text_muted))
                .child(
                    text("Settings")
                        .font(TextStyle::new(13.0, 400.0))
                        .color(theme.text),
                ),
        );

    rune_kit::Popover::new("user-menu")
        .trigger(trigger)
        .open(open)
        .padding(6.0)
        .placement(Placement::TopStart)
        .gutter(4.0)
        .on_close(|app: &mut App| app.shell.user_menu_open = false)
        .child(body)
        .build(&super::shell_kit(theme), layer)
}

/// The session-row context menu + rename/delete dialogs.
pub fn render_chat_overlays(app: &App, theme: &Theme, layer: &mut Vec<El>) {
    if let Some(menu) = app.shell.chat_menu.clone() {
        let chat_id = menu.chat_id.clone();
        let archived = app
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .is_some_and(|c| c.archived);
        let archive_id = chat_id.clone();
        let delete_id = chat_id.clone();
        let rename_id = chat_id.clone();
        let menu = rune_kit::Menu::new("chat-context")
            .trigger(el())
            .open(true)
            .at(menu.x, menu.y)
            .min_width(170.0)
            .on_close(|app: &mut App| app.shell.chat_menu = None)
            .item(
                rune_kit::MenuItem::new("Rename…")
                    .icon(&icons::PEN)
                    .on_select(move |app: &mut App| open_rename_chat(app, rename_id.clone())),
            )
            .item(
                rune_kit::MenuItem::new(if archived { "Unarchive" } else { "Archive" })
                    .icon(&icons::ARCHIVE_MINIMALISTIC)
                    .on_select(move |app: &mut App| {
                        app.shell.chat_menu = None;
                        set_chat_archived(app, archive_id.clone(), !archived);
                    }),
            )
            .separator()
            .item(
                rune_kit::MenuItem::new("Delete…")
                    .icon(&icons::TRASH_BIN_MINIMALISTIC)
                    .danger(true)
                    .on_select(move |app: &mut App| {
                        app.shell.chat_menu = None;
                        app.shell.delete_confirm = Some(delete_id.clone());
                    }),
            );
        let _ = menu.build(&super::shell_kit(theme), layer);
    }

    if let Some(dialog) = app.shell.rename_chat.as_ref() {
        let body = col()
            .gap(12.0)
            .child(
                rune::text_input(&dialog.input, TextStyle::new(14.0, 400.0))
                    .w_full()
                    .h(34.0)
                    .px(8.0)
                    .radius(6.0)
                    .fill(theme.input_bg)
                    .border(1.0, theme.border)
                    .autofocus()
                    .on_edit(|app: &mut App| {
                        &mut app
                            .shell
                            .rename_chat
                            .as_mut()
                            .expect("rename chat open")
                            .input
                    })
                    .on_submit(|app: &mut App, _| submit_rename_chat(app)),
            )
            .child(
                row()
                    .w_full()
                    .justify_end()
                    .gap(8.0)
                    .child(
                        dialog_button(theme, "Cancel", false).on_press(|app: &mut App| {
                            app.shell.rename_chat = None;
                        }),
                    )
                    .child(
                        dialog_button(theme, "Rename", true)
                            .on_press(|app: &mut App| submit_rename_chat(app)),
                    ),
            );
        rune_kit::Dialog::new("rename-chat-dialog", "Rename session")
            .open(true)
            .width(420.0)
            .fit()
            .on_close(|app: &mut App| app.shell.rename_chat = None)
            .child(body)
            .build(&super::shell_kit(theme), layer);
    }

    if let Some(chat_id) = app.shell.delete_confirm.clone() {
        let title = app
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_else(|| "New session".to_string());
        let copy = format!("“{title}” will be permanently deleted. This can’t be undone.");
        let id = chat_id.clone();
        let body = row()
            .w_full()
            .justify_end()
            .gap(8.0)
            .child(
                dialog_button(theme, "Cancel", false).on_press(|app: &mut App| {
                    app.shell.delete_confirm = None;
                }),
            )
            .child(
                dialog_button(theme, "Delete", true)
                    .on_press(move |app: &mut App| delete_chat(app, id.clone())),
            );
        rune_kit::Dialog::new("delete-chat-dialog", "Delete session?")
            .open(true)
            .width(440.0)
            .fit()
            .description(copy)
            .on_close(|app: &mut App| app.shell.delete_confirm = None)
            .child(body)
            .build(&super::shell_kit(theme), layer);
    }
}

fn dialog_button(theme: &Theme, label: &str, primary: bool) -> El {
    el().h(30.0)
        .px(12.0)
        .radius(Theme::CONTROL_RADIUS)
        .center()
        .cursor(Cursor::Pointer)
        .fill(if primary { theme.solid } else { theme.ink(0.0) })
        .hover(if primary {
            theme.surface_raised_hover
        } else {
            theme.element_hover
        })
        .child(
            text(label.to_string())
                .font(TextStyle::new(13.0, 500.0))
                .color(if primary { theme.on_solid } else { theme.text }),
        )
}

/// Whether the transcript wants the "Scroll to bottom" pill.
pub fn transcript_jump_shown(app: &App) -> bool {
    crate::transcript::transcript_jump_shown(app)
}

/// The "↓ Scroll to bottom" pill: a labelled rounded-full chip floating above the composer.
/// `stack_h` is the measured bottom chrome stack.
pub fn render_jump_to_bottom(app: &App, theme: &Theme, stack_h: f32) -> Option<El> {
    if !transcript_jump_shown(app) {
        return None;
    }
    let pill = row()
        .key("jump-to-bottom")
        .h(30.0)
        .radius(15.0)
        .border(1.0, theme.border)
        .shadow(2.0, 8.0, theme.ink(0.22))
        .cursor(Cursor::Pointer)
        .center_y()
        .gap(6.0)
        .pl(11.0)
        .pr(13.0)
        .fill(if theme.is_glass() {
            theme.glass_overlay()
        } else {
            theme.surface_raised
        })
        .hover(theme.surface_raised_hover)
        .on_press_tx(|app: &mut App, tx: &mut rune::Tx| {
            crate::transcript::scroll_to_bottom(app, tx)
        })
        .child(
            text("↓")
                .font(TextStyle::new(13.0, 400.0))
                .color(theme.text_muted),
        )
        .child(
            text("Scroll to bottom")
                .font(TextStyle::new(13.0, 400.0))
                .color(theme.text),
        );
    Some(
        el().key("jump-to-bottom-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .child(
                el().pin(None, None, None, Some(stack_h + 8.0))
                    .w_full()
                    .h(30.0)
                    .center()
                    .child(pill),
            ),
    )
}

/// The right-pane header's surface label, for the titlebar strip.
pub fn active_surface_label(app: &App) -> Option<String> {
    match resolved_right_active(app) {
        RightSurface::Shared => None,
        surface => super::right_surface_rows(app)
            .into_iter()
            .find(|(row, _)| *row == surface)
            .map(|(_, title)| title),
    }
}
