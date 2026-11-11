//! Settings → Shortcuts: a table of the rebindable bindings — click a combo to
//! record (Esc cancels), live conflict detection, per-row Reset and Restore
//! defaults. Changes persist through [`UiSettings::save`](crate::settings::UiSettings::save).
//!
//! The gpui page emitted a `Changed` event and let the shell persist; in Rune
//! the handler writes `app.settings.keymap` and calls `App::save_settings`
//! directly.

use rune::{El, Key, KeyEvent, TextStyle, Tx, col, row, text};

use crate::icons;
use crate::settings::{KeymapConfig, ShortcutId, combo_from_keystroke, display_combo};
use crate::state::App;
use crate::theme::Theme;

use super::widgets;

/// Outcome of one keystroke while recording. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Esc — abandon recording, keep the old combo.
    Cancelled,
    /// A bare modifier (or unusable key) — stay recording.
    Ignored,
    /// A full combo landed.
    Set(String),
}

pub fn record_key(key: &str, ctrl: bool, alt: bool, shift: bool, cmd: bool) -> RecordOutcome {
    if key.eq_ignore_ascii_case("escape") {
        return RecordOutcome::Cancelled;
    }
    match combo_from_keystroke(ctrl, alt, shift, cmd, key) {
        Some(combo) => RecordOutcome::Set(combo),
        None => RecordOutcome::Ignored,
    }
}

/// The shortcut (other than `id`) already bound to `combo`, if any. Pure.
pub fn conflict_owner(keymap: &KeymapConfig, id: ShortcutId, combo: &str) -> Option<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .find(|&other| other != id && keymap.get(other) == combo)
}

/// The key name the recorder stores, using the gpui spelling. `None` for a
/// bare modifier press (which records nothing).
pub fn key_name(key: &Key) -> Option<String> {
    Some(match key {
        Key::Text(value) => value.clone(),
        Key::Command(c) => c.to_string(),
        Key::Enter => "enter".to_string(),
        Key::Backspace => "backspace".to_string(),
        Key::Delete => "delete".to_string(),
        Key::Left => "left".to_string(),
        Key::Right => "right".to_string(),
        Key::Up => "up".to_string(),
        Key::Down => "down".to_string(),
        Key::Home => "home".to_string(),
        Key::End => "end".to_string(),
        Key::PageUp => "pageup".to_string(),
        Key::PageDown => "pagedown".to_string(),
        Key::Insert => "insert".to_string(),
        Key::Escape => "escape".to_string(),
        Key::Tab => "tab".to_string(),
        Key::Function(n) => format!("f{n}"),
        Key::Modifier { .. } => return None,
        _ => return None,
    })
}

/// One-line purpose copy per shortcut (hearth lib/shortcuts.ts
/// `SHORTCUT_DEFINITIONS` descriptions, verbatim).
pub fn description(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::ToggleSidebar => "Show or hide sessions and settings navigation.",
        ShortcutId::ToggleChanges => "Show or hide changes for the current session.",
        ShortcutId::ToggleTerminal => "Show or hide the terminal for the current session.",
        ShortcutId::NewSession => "Open a blank session canvas to start a new session.",
        ShortcutId::NextSession => "Select the next session in the sidebar, wrapping at the end.",
        ShortcutId::PrevSession => {
            "Select the previous session in the sidebar, wrapping at the start."
        }
    }
}

/// View state for the Shortcuts page. The keymap itself lives in
/// `app.settings.keymap`.
#[derive(Debug, Default)]
pub struct ShortcutsPage {
    pub recording: Option<ShortcutId>,
    /// A rejected record attempt ("{Combo} is already assigned to {label}.") —
    /// conflicts never persist; they're refused at record time.
    pub conflict_notice: Option<String>,
}

impl ShortcutsPage {
    /// The Shortcuts page has no async work.
    pub fn poll(&mut self) -> bool {
        false
    }
}

/// Handle a key while a combo is being recorded. Returns true when consumed.
pub fn handle_record_key(app: &mut App, ev: &KeyEvent) -> bool {
    let Some(recording) = app.settings_ui.shortcuts.recording else {
        return false;
    };
    let Some(key) = key_name(&ev.key) else {
        // A bare modifier press: stay recording.
        return true;
    };
    match record_key(&key, ev.mods.ctrl, ev.mods.alt, ev.mods.shift, ev.mods.cmd) {
        RecordOutcome::Cancelled => {
            app.settings_ui.shortcuts.recording = None;
        }
        RecordOutcome::Ignored => {}
        RecordOutcome::Set(combo) => {
            if let Some(owner) = conflict_owner(&app.settings.keymap, recording, &combo) {
                app.settings_ui.shortcuts.conflict_notice = Some(format!(
                    "{} is already assigned to {}.",
                    display_combo(&combo),
                    owner.label()
                ));
                app.settings_ui.shortcuts.recording = None;
            } else {
                app.settings.keymap.set(recording, combo);
                app.settings_ui.shortcuts.recording = None;
                app.settings_ui.shortcuts.conflict_notice = None;
                app.save_settings();
            }
        }
    }
    true
}

fn record_event(app: &mut App, ev: &KeyEvent, _tx: &mut Tx) -> bool {
    handle_record_key(app, ev)
}

fn begin_recording(app: &mut App, tx: &mut Tx, id: ShortcutId, focus_key: String) {
    app.settings_ui.shortcuts.recording = Some(id);
    app.settings_ui.shortcuts.conflict_notice = None;
    tx.focus(focus_key, true);
}

fn reset_shortcut(app: &mut App, id: ShortcutId) {
    app.settings.keymap.reset(id);
    app.settings_ui.shortcuts.recording = None;
    app.save_settings();
}

fn restore_defaults(app: &mut App) {
    app.settings.keymap = KeymapConfig::default();
    app.settings_ui.shortcuts.recording = None;
    app.settings_ui.shortcuts.conflict_notice = None;
    app.save_settings();
}

fn shortcut_row(page: &ShortcutsPage, app: &App, ix: usize, id: ShortcutId, theme: &Theme) -> El {
    let combo = app.settings.keymap.get(id).to_string();
    let is_recording = page.recording == Some(id);
    let non_default = combo != id.default_combo();
    let chip_text = if is_recording {
        "Press keys…".to_string()
    } else {
        display_combo(&combo)
    };
    let chip_key = format!("settings/shortcut-combo-{ix}");
    let page_key = "shortcuts-page".to_string();

    let mut line = row()
        .key(format!("shortcut-row-{ix}"))
        .min_h(72.0)
        .px(20.0)
        .w_full()
        .items_center()
        .gap(20.0)
        .border_sides(
            if ix > 0 {
                [1.0, 0.0, 0.0, 0.0]
            } else {
                [0.0; 4]
            },
            theme.border,
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(
                    text(id.label())
                        .font(TextStyle::new(13.0, 500.0))
                        .color(theme.text),
                )
                .child(
                    text(description(id))
                        .mt(2.0)
                        .font(TextStyle::new(12.0, 400.0))
                        .color(theme.text_muted)
                        .wrap(),
                ),
        );
    if non_default && !is_recording {
        line = line.child(
            text("Reset")
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted.alpha(0.7))
                .cursor(rune::Cursor::Pointer)
                .on_press(move |app: &mut App| reset_shortcut(app, id)),
        );
    }
    line.child(
        row()
            .key(chip_key.clone())
            .min_w(96.0)
            .px(12.0)
            .py(6.0)
            .radius(8.0)
            .border(
                1.0,
                if is_recording {
                    theme.text.alpha(0.3)
                } else {
                    theme.border
                },
            )
            .fill(if is_recording { theme.text } else { theme.bg })
            .justify_center()
            .cursor(rune::Cursor::Pointer)
            .on_press_tx(move |app: &mut App, tx: &mut Tx| {
                begin_recording(app, tx, id, page_key.clone())
            })
            .child(
                text(chip_text)
                    .font(TextStyle::new(12.0, 400.0).mono())
                    .color(if is_recording {
                        theme.on_solid
                    } else {
                        theme.text
                    }),
            ),
    )
}

/// The Shortcuts page body.
pub fn render(page: &ShortcutsPage, app: &App, theme: &Theme) -> El {
    let recording = page.recording;
    let customized = app.settings.keymap != KeymapConfig::default();

    let mut card = widgets::section_card(theme).mt(32.0);
    for (ix, id) in ShortcutId::ALL.into_iter().enumerate() {
        card = card.child(shortcut_row(page, app, ix, id, theme));
    }

    let disabled = !customized || recording.is_some();
    let mut restore = widgets::ghost_action(theme)
        .child(icons::icon(&icons::RESTART, 14.0).color(theme.text_muted))
        .child(
            text("Restore defaults")
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.text_muted),
        );
    if disabled {
        restore = restore.opacity(0.35);
    } else {
        restore = restore
            .hover(theme.ink(0.04))
            .on_press(|app: &mut App| restore_defaults(app));
    }

    let helper = if recording.is_some() {
        "Press Escape to cancel.".to_string()
    } else if let Some(notice) = page.conflict_notice.clone() {
        notice
    } else {
        "Shortcuts must be unique.".to_string()
    };

    let content = widgets::page_column()
        .child(
            row()
                .w_full()
                .items_start()
                .justify_between()
                .gap(24.0)
                .child(
                    col()
                        .grow(1.0)
                        .child(widgets::page_header(theme, "Keyboard shortcuts", None))
                        .child(
                            widgets::page_subtitle(
                                theme,
                                "Click a binding, then press the key combination you want to \
                                 use. Changes apply immediately and stay on this device.",
                            )
                            .max_w(512.0),
                        ),
                )
                .child(restore),
        )
        .child(card)
        .child(
            text(helper)
                .mt(12.0)
                .px(4.0)
                .min_h(20.0)
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.text_muted),
        );

    widgets::page_root("shortcuts-page")
        .focusable(true)
        .on_key(record_event)
        .child(content)
}

/// No window-level key handling: recording is handled by the page root while
/// it holds focus.
pub fn on_key(_app: &mut App, _ev: &KeyEvent, _tx: &mut Tx) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_outcomes() {
        assert_eq!(
            record_key("escape", false, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("Escape", true, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("s", false, false, false, true),
            RecordOutcome::Set("mod-s".into())
        );
        assert_eq!(
            record_key("k", false, true, true, true),
            RecordOutcome::Set("mod-alt-shift-k".into())
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            record_key("tab", true, false, true, false),
            RecordOutcome::Set("ctrl-shift-tab".into())
        );
        assert_eq!(
            record_key("shift", false, false, true, false),
            RecordOutcome::Ignored
        );
        assert_eq!(
            record_key("ctrl", true, false, false, false),
            RecordOutcome::Ignored
        );
    }

    #[test]
    fn conflicting_records_are_refused() {
        let keymap = KeymapConfig::default();
        let RecordOutcome::Set(combo) = record_key("b", false, false, false, true) else {
            panic!("expected Set");
        };
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, &combo),
            Some(ShortcutId::ToggleChanges)
        );
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleChanges, &combo),
            None
        );
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, "mod-shift-x"),
            None
        );
    }

    #[test]
    fn key_names_use_the_gpui_spelling() {
        assert_eq!(key_name(&Key::Text("K".into())), Some("K".into()));
        assert_eq!(key_name(&Key::Function(5)), Some("f5".into()));
        assert_eq!(key_name(&Key::Tab), Some("tab".into()));
        assert_eq!(key_name(&Key::Escape), Some("escape".into()));
        assert_eq!(
            key_name(&Key::Modifier {
                modifier: rune::Modifier::ShiftLeft,
                pressed: true
            }),
            None
        );
    }
}
