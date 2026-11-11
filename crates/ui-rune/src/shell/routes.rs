//! Routing: chat selection, settings sections and the settings-page outlet.

use crate::settings::SettingsSection;
use crate::state::App;
use crate::theme::Theme;
use rune::{El, Icon, col};

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Chat,
    Settings(SettingsSection),
}

impl Route {
    pub fn is_chat(self) -> bool {
        matches!(self, Self::Chat)
    }

    pub fn section(self) -> Option<SettingsSection> {
        match self {
            Self::Settings(section) => Some(section),
            Self::Chat => None,
        }
    }
}

/// `HEARTH_OPEN_ROUTE=settings[/<section>]` boots straight into a settings section; `new` pins
/// the new-chat canvas. Unknown values fall back to Chat.
pub fn route_from_env() -> Route {
    let Ok(value) = std::env::var("HEARTH_OPEN_ROUTE") else {
        return Route::Chat;
    };
    match value.trim() {
        "settings" | "settings/devices" | "settings/agents-harnesses" => {
            Route::Settings(SettingsSection::Devices)
        }
        "settings/harnesses" => Route::Settings(SettingsSection::Harnesses),
        "settings/accounts" | "settings/agents" => Route::Settings(SettingsSection::Agents),
        "settings/appearance" => Route::Settings(SettingsSection::Appearance),
        "settings/notifications" => Route::Settings(SettingsSection::Notifications),
        "settings/shortcuts" => Route::Settings(SettingsSection::Shortcuts),
        "settings/archived" => Route::Settings(SettingsSection::Archived),
        _ => Route::Chat,
    }
}

/// The sidebar glyph for a settings section (hearth settings-sidebar.tsx).
pub fn section_icon(section: SettingsSection) -> &'static Icon {
    match section {
        SettingsSection::Devices => &crate::icons::MONITOR,
        SettingsSection::Harnesses => &crate::icons::WIDGET,
        SettingsSection::Agents => &crate::icons::KEY_MINIMALISTIC,
        SettingsSection::Appearance => &crate::icons::TUNING,
        SettingsSection::Notifications => &crate::icons::BELL,
        SettingsSection::Shortcuts => &crate::icons::KEYBOARD,
        SettingsSection::Archived => &crate::icons::ARCHIVE_MINIMALISTIC,
    }
}

/// Render the selected settings page. The pages own their state under [`App::settings_ui`];
/// this is the shell's outlet.
pub fn settings_outlet(app: &App, _section: SettingsSection, theme: &Theme) -> El {
    col()
        .key("settings-outlet")
        .w_full()
        .h_full()
        .child(crate::settings::render(app, theme))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_is_the_default_route() {
        assert_eq!(Route::default(), Route::Chat);
        assert!(Route::Chat.is_chat());
        assert_eq!(Route::Chat.section(), None);
    }

    #[test]
    fn every_settings_section_has_an_icon_and_a_label() {
        for section in SettingsSection::ALL {
            assert!(!section.label().is_empty());
            assert!(!section_icon(section).name.is_empty());
        }
    }

    #[test]
    fn section_routes_are_distinct() {
        assert_ne!(
            Route::Settings(SettingsSection::Devices),
            Route::Settings(SettingsSection::Shortcuts)
        );
        assert_ne!(Route::Chat, Route::Settings(SettingsSection::Devices));
    }
}
