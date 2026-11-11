//! Settings → Notifications: the session ping toggles — the completion /
//! question chime and the desktop banner ride the same status transitions; this
//! page flips their three `UiSettings` flags.
//!
//! Every flip persists through the shell's [`crate::settings::UiSettings`]
//! snapshot; nothing here talks RPC — all three flags are device-local UI
//! settings.

use rune::{El, col};

use crate::icons;
use crate::state::App;
use crate::theme::Theme;

use super::widgets;

/// View state for the Notifications page: the flags live in `App::settings`,
/// so the page only needs to exist.
#[derive(Debug, Default)]
pub struct NotificationsPage;

impl NotificationsPage {
    /// The Notifications page has no async work.
    pub fn poll(&mut self) -> bool {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn toggle_row(
    theme: &Theme,
    first: bool,
    icon: &'static rune::Icon,
    title: &'static str,
    description: &'static str,
    on: bool,
    interactive: bool,
    flip: impl Fn(&mut App) + 'static,
) -> El {
    let mut switch = widgets::toggle_switch(theme, on);
    if interactive {
        switch = switch
            .cursor(rune::Cursor::Pointer)
            .on_press(move |app: &mut App| flip(app));
    }
    let mut row = widgets::card_row(theme, first)
        .child(widgets::row_tile(theme, icon))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(widgets::row_title(theme, title))
                .child(widgets::meta_line(
                    theme,
                    vec![widgets::meta_text(theme, description)],
                )),
        )
        .child(switch);
    if !interactive {
        row = row.opacity(0.55);
    }
    row
}

fn toggle_sound(app: &mut App) {
    app.settings.sound_enabled = !app.settings.sound_enabled;
    app.save_settings();
}

fn toggle_desktop(app: &mut App) {
    app.settings.notifications_enabled = !app.settings.notifications_enabled;
    app.save_settings();
}

fn toggle_background(app: &mut App) {
    app.settings.notifications_background_only = !app.settings.notifications_background_only;
    app.save_settings();
}

/// The Notifications page body.
pub fn render(_page: &NotificationsPage, app: &App, theme: &Theme) -> El {
    let sound = app.settings.sound_enabled;
    let desktop = app.settings.notifications_enabled;
    let background_only = app.settings.notifications_background_only;

    let card = widgets::section_card(theme)
        .child(toggle_row(
            theme,
            true,
            &icons::VOLUME_LOUD,
            "Sounds",
            "Chime when a run finishes or an agent asks a question.",
            sound,
            true,
            toggle_sound,
        ))
        .child(toggle_row(
            theme,
            false,
            &icons::BELL,
            "Desktop notifications",
            "Show a system banner on the same events, so pings reach you while Hearth is in the background.",
            desktop,
            true,
            toggle_desktop,
        ))
        .child(toggle_row(
            theme,
            false,
            &icons::MONITOR,
            "Only when in the background",
            "Skip the banner while a Hearth window is focused — the chime already covers it.",
            background_only,
            desktop,
            toggle_background,
        ));

    let content = widgets::page_column()
        .child(widgets::page_header(theme, "Notifications", None))
        .child(
            widgets::page_subtitle(
                theme,
                "How session pings reach you when a run finishes or an agent is waiting on your input.",
            )
            .max_w(512.0),
        )
        .child(card);

    widgets::page_root("notifications-page").child(content)
}

/// No page-level key handling.
pub fn on_key(_app: &mut App, _ev: &rune::KeyEvent) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_stateless() {
        let page = NotificationsPage;
        let _ = format!("{page:?}");
    }
}
