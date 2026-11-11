//! App-level keyboard shortcuts the shell owns.
//!
//! `AppSpec::on_key` is the lead's; this module is the shell's half of it. The gpui build bound
//! these through `shell::apply_keymap` from the persisted [`crate::settings::KeymapConfig`],
//! with the same defaults. Rune has no runtime keymap registry, so the two are composed here:
//! the customizable catalogue first, then the fixed app chords.
//!
//! Ship with:
//! ```ignore
//! fn on_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
//!     hearth_ui_rune::shell::on_key(app, ev, tx)
//! }
//! ```

use rune::{Key, KeyEvent, Tx};

use crate::settings::ShortcutId;
use crate::state::App;

use super::{open_new_session, spaces, toggle_right_pane, toggle_sidebar, toggle_terminal};

/// Handle a window key the focused control passed on. True when the shell took it.
pub fn on_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // Overlays own Escape while their modal is mounted; if focus is elsewhere, close the
    // shell's own popups rather than leaving one stranded.
    if matches!(ev.key, Key::Escape) && close_popups(app) {
        return true;
    }

    // Session cycling, honoring the user's persisted bindings.
    if chord_matches(app, ShortcutId::NextSession, ev, tx) {
        super::tabs::cycle_session(app, true);
        return true;
    }
    if chord_matches(app, ShortcutId::PrevSession, ev, tx) {
        super::tabs::cycle_session(app, false);
        return true;
    }

    // The customizable command shortcuts.
    let bindings: [(ShortcutId, fn(&mut App)); 4] = [
        (ShortcutId::NewSession, open_new_session),
        (ShortcutId::ToggleSidebar, toggle_sidebar),
        (ShortcutId::ToggleChanges, toggle_right_pane),
        (ShortcutId::ToggleTerminal, toggle_terminal),
    ];
    for (id, action) in bindings {
        if chord_matches(app, id, ev, tx) {
            action(app);
            return true;
        }
    }

    // ⌘K / Ctrl+K toggles the add-space palette (not user-bindable).
    if ev.is_command('k') {
        if app.shell.add_space.is_some() {
            spaces::close_add_space(app);
        } else {
            spaces::open_add_space(app);
        }
        return true;
    }
    false
}

/// Does `ev` match the combo persisted for `id`?
fn chord_matches(app: &App, id: ShortcutId, ev: &KeyEvent, tx: &Tx) -> bool {
    let combo = app.settings.keymap.get(id);
    rune::Chord::parse(&super::rune_chord(combo)).is_ok_and(|chord| chord.matches_tx(ev, tx))
}

/// Close any open shell popup. Returns true when one was open.
pub fn close_popups(app: &mut App) -> bool {
    let mut closed = false;
    if app.shell.user_menu_open {
        app.shell.user_menu_open = false;
        closed = true;
    }
    if app.shell.spaces_menu.is_some() {
        app.shell.spaces_menu = None;
        closed = true;
    }
    if app.shell.chat_menu.is_some() {
        app.shell.chat_menu = None;
        closed = true;
    }
    if app.shell.space_menu.is_some() {
        app.shell.space_menu = None;
        closed = true;
    }
    if app.shell.add_space.is_some() {
        app.shell.add_space = None;
        closed = true;
    }
    if app.shell.rename_chat.is_some() {
        app.shell.rename_chat = None;
        closed = true;
    }
    if app.shell.rename_space.is_some() {
        app.shell.rename_space = None;
        closed = true;
    }
    if app.shell.delete_confirm.is_some() {
        app.shell.delete_confirm = None;
        closed = true;
    }
    if app.shell.delete_space_confirm.is_some() {
        app.shell.delete_space_confirm = None;
        closed = true;
    }
    closed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key, mods: rune::Mods) -> KeyEvent {
        KeyEvent { key: k, mods }
    }

    #[test]
    fn command_letters_map_to_the_primary_modifier() {
        let ev = key(
            Key::Command('b'),
            rune::Mods {
                cmd: true,
                ..Default::default()
            },
        );
        assert!(ev.is_command('b'));
        assert!(ev.is_command('B'));
        assert!(!ev.is_command('s'));
    }

    #[test]
    fn ctrl_tab_is_the_cycle_chord() {
        let ev = key(
            Key::Tab,
            rune::Mods {
                ctrl: true,
                shift: true,
                ..Default::default()
            },
        );
        assert!(matches!(ev.key, Key::Tab));
        assert!(ev.mods.ctrl && ev.mods.shift);
    }
}
