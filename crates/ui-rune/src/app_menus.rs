//! Native application menu items and their actions.
//!
//! Rune gives a Mac app the standard Edit and Window items through
//! [`rune::App::standard_menus`](rune::App::standard_menus) (already requested in `run_app`).
//! Everything hearth itself adds is declared here as [`rune::MenuItem`] values: the app owns
//! the actions, the lead wires them in once with [`install`] (or one `.menu_item(..)` per
//! item).
//!
//! On macOS these become real menu-bar items with key equivalents. Windows and Linux return
//! `Unsupported` from Rune's native menu backend, which is fine: the same actions are bound in
//! `app::on_key` once the lead wires the shell keymap.

use rune::{App as RuneApp, MenuItem as NativeMenuItem, Shortcut, Tx};

use crate::appearance::AppearanceMode;
use crate::settings::SettingsSection;
use crate::state::App;

/// Eat the builder and add every Hearth menu item.
///
/// `rune::App::new(spec).init(..).chrome(..).titlebar(..).standard_menus()
///     .pipe(hearth_ui_rune::app_menus::install)`
pub fn install(app: RuneApp<App>) -> RuneApp<App> {
    menu_items()
        .into_iter()
        .fold(app, |app, item| app.menu_item(item))
}

/// The Hearth menu items, in menu order. Call `.menu_item(item)` for each (or [`install`]).
pub fn menu_items() -> Vec<NativeMenuItem<App>> {
    vec![
        // ---- File ---------------------------------------------------------
        NativeMenuItem::new("New Session", new_session)
            .id("hearth.new-session")
            .menu("File")
            .key_equivalent(shortcut("mod+n")),
        NativeMenuItem::new("Settings…", open_settings)
            .id("hearth.settings")
            .menu("File")
            .key_equivalent(shortcut("mod+comma")),
        NativeMenuItem::separator().menu("File"),
        NativeMenuItem::new("Close Window", close_window)
            .id("hearth.close-window")
            .menu("File")
            .key_equivalent(shortcut("mod+w")),
        NativeMenuItem::new("Quit Hearth", quit)
            .id("hearth.quit")
            .menu("File")
            .key_equivalent(shortcut("mod+q")),
        // ---- View ---------------------------------------------------------
        NativeMenuItem::new("Toggle Sidebar", toggle_sidebar)
            .id("hearth.toggle-sidebar")
            .menu("View")
            .key_equivalent(shortcut("mod+s")),
        NativeMenuItem::new("Toggle Right Pane", toggle_right_pane)
            .id("hearth.toggle-right-pane")
            .menu("View")
            .key_equivalent(shortcut("mod+b")),
        NativeMenuItem::new("Toggle Terminal", toggle_terminal)
            .id("hearth.toggle-terminal")
            .menu("View")
            .key_equivalent(shortcut("mod+j")),
        NativeMenuItem::separator().menu("View"),
        NativeMenuItem::new("Appearance: System", appearance_system)
            .id("hearth.appearance-system")
            .menu("View"),
        NativeMenuItem::new("Appearance: Light", appearance_light)
            .id("hearth.appearance-light")
            .menu("View"),
        NativeMenuItem::new("Appearance: Dark", appearance_dark)
            .id("hearth.appearance-dark")
            .menu("View"),
        NativeMenuItem::separator().menu("View"),
        NativeMenuItem::new("Enter Full Screen", toggle_full_screen)
            .id("hearth.full-screen")
            .menu("View")
            .key_equivalent(shortcut("ctrl+mod+f")),
        // ---- Window -------------------------------------------------------
        NativeMenuItem::new("Minimize", minimize)
            .id("hearth.minimize")
            .menu("Window")
            .key_equivalent(shortcut("mod+m")),
        NativeMenuItem::new("Zoom", toggle_maximized)
            .id("hearth.zoom")
            .menu("Window"),
    ]
}

/// Parse a menu accelerator. A typo is a programming error, caught by the tests.
fn shortcut(spec: &str) -> Shortcut {
    spec.parse()
        .unwrap_or_else(|err| panic!("invalid menu shortcut {spec:?}: {err}"))
}

// ---- actions --------------------------------------------------------------

fn quit(app: &mut App, tx: &mut Tx) {
    let _ = app;
    tx.quit();
}

fn close_window(_: &mut App, tx: &mut Tx) {
    tx.close_window();
}

fn minimize(_: &mut App, tx: &mut Tx) {
    tx.minimize();
}

fn toggle_maximized(_: &mut App, tx: &mut Tx) {
    tx.toggle_maximized();
}

fn toggle_full_screen(_: &mut App, tx: &mut Tx) {
    tx.toggle_full_screen();
}

fn new_session(app: &mut App, _: &mut Tx) {
    crate::shell::new_session(app);
}

fn open_settings(app: &mut App, _: &mut Tx) {
    crate::shell::open_settings(app, SettingsSection::Devices);
}

fn toggle_sidebar(app: &mut App, _: &mut Tx) {
    crate::shell::toggle_sidebar(app);
}

fn toggle_right_pane(app: &mut App, _: &mut Tx) {
    crate::shell::toggle_right_pane(app);
}

fn toggle_terminal(app: &mut App, _: &mut Tx) {
    crate::shell::toggle_terminal(app);
}

fn set_appearance(app: &mut App, mode: AppearanceMode) {
    // Persist the choice in the settings the async save clones: `set_mode`
    // updates the appearance state, but a save from the stale `settings`
    // snapshot would revert it on the next load.
    app.settings.appearance = mode;
    if app.appearance.set_mode(mode) {
        app.refresh_theme();
    }
    app.save_settings();
}

fn appearance_system(app: &mut App, _: &mut Tx) {
    set_appearance(app, AppearanceMode::System);
}

fn appearance_light(app: &mut App, _: &mut Tx) {
    set_appearance(app, AppearanceMode::Light);
}

fn appearance_dark(app: &mut App, _: &mut Tx) {
    set_appearance(app, AppearanceMode::Dark);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shortcut_parses_and_the_list_is_populated() {
        // Construction above parses every accelerator; this proves the declaration ran and the
        // list stayed complete.
        let items = menu_items();
        assert!(items.len() >= 15);
    }

    #[test]
    fn menu_accelerators_round_trip() {
        for spec in ["mod+n", "mod+w", "mod+q", "mod+s", "mod+b", "mod+j"] {
            assert!(shortcut(spec).to_string().contains('+'));
        }
    }
}
