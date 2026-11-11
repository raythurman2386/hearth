//! Light/dark switching: what the user asked for, what the OS reports, and the
//! plumbing that turns a change in either into a repaint.
//!
//! Rune has no global registry, so the persisted choice and the last OS report
//! live in [`crate::state::App`] and the resolved [`Appearance`] is rebuilt into
//! the [`crate::theme::Theme`] whenever either changes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::theme::Appearance;

/// The user's appearance preference. Persisted in `ui-settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppearanceMode {
    /// Follow the OS. The default.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// Menu/label text.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];
}

/// What the user chose, and what the OS last said. Kept separate from
/// [`crate::theme::Theme`] so flipping the OS appearance while the user has
/// pinned Light still records the new system value.
#[derive(Debug, Clone)]
pub struct AppearanceState {
    pub mode: AppearanceMode,
    pub system: Appearance,
    /// Where `ui-settings.json` lives, so a menu action can persist the choice.
    pub data_dir: PathBuf,
}

impl AppearanceState {
    pub fn new(mode: AppearanceMode, system: Appearance, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            system,
            data_dir: data_dir.into(),
        }
    }

    /// The appearance currently in effect.
    pub fn resolved(&self) -> Appearance {
        resolve(self.mode, self.system)
    }

    /// Change the user's preference and persist just that key. Returns true
    /// when the resolved appearance changed (the caller rebuilds the theme).
    pub fn set_mode(&mut self, mode: AppearanceMode) -> bool {
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        self.persist(mode);
        true
    }

    /// Read-modify-write `ui-settings.json` for just the appearance key.
    ///
    /// Deliberately a fresh load rather than a write of some cached struct: the
    /// shell holds its own [`crate::settings::UiSettings`] and saves it
    /// debounced, so writing a stale snapshot from here would silently roll
    /// back a pane resize the user made seconds earlier.
    fn persist(&self, mode: AppearanceMode) {
        let mut settings = crate::settings::UiSettings::load(&self.data_dir);
        settings.appearance = mode;
        if let Err(err) = settings.save(&self.data_dir) {
            tracing::warn!(error = %err, "could not persist appearance");
        }
    }
}

/// Combine the user's choice with the OS state.
pub fn resolve(mode: AppearanceMode, system: Appearance) -> Appearance {
    match mode {
        AppearanceMode::System => system,
        AppearanceMode::Light => Appearance::Light,
        AppearanceMode::Dark => Appearance::Dark,
    }
}
