//! Terminal paint + input encoding, ported from the gpui UI.
//!
//! - the ANSI palette on the terminal background — `#141814` dark, `#fdf6e3`
//!   light (Ravenwood) — and the 256-color cube/grayscale resolution;
//! - keystroke → PTY byte encoding (printables, control keys, arrows/nav
//!   escape sequences, Ctrl- combos, Alt prefixing);
//! - the 12 ms input coalescer and the 80 ms resize debounce constants (the
//!   panel drives the timers; the buffer logic here is pure);
//! - pointer → cell mapping and the scrollbar geometry used by the panel.
//!
//! Rune has no custom-paint element, so the grid itself is built from Rows of
//! [`rune::rich`] text plus absolutely positioned background/selection/cursor
//! quads in [`super::panel`]. Everything here is paint-agnostic and testable
//! without a window.

use rune::color::srgb_decode;
use rune::{Color, Key, KeyEvent, Mods};

use crate::theme::{ANSI16_DARK, ANSI16_LIGHT, Appearance, Theme};

use super::emulator::{CellColor, Side};

/// Terminal font metrics (mono), matching the gpui constants.
pub const TERM_FONT_SIZE: f32 = 13.0;
pub const TERM_LINE_HEIGHT: f32 = 18.0;
/// Inner padding of the grid area.
pub const TERM_PADDING: f32 = 12.0;

/// Keyboard input coalescing window before a `WriteTerminal` flush.
pub const COALESCE_MS: u64 = 12;
/// Debounce for `ResizeTerminal` after viewport-driven size changes.
pub const RESIZE_DEBOUNCE_MS: u64 = 80;

/// The mono style the grid is set in: the kit's mono face at the terminal's
/// own 13/18 metrics (the kit default is 12/18).
pub fn term_style(theme: &Theme) -> rune::TextStyle {
    rune::TextStyle {
        size: TERM_FONT_SIZE,
        line_height: TERM_LINE_HEIGHT,
        ..theme.kit.ty.mono
    }
}

/// The advance of one grid column in points, from the real mono face.
///
/// Falls back to `0.6 × size` when the thread has no text system yet (a pure
/// test), so layout math never divides by zero.
pub fn cell_width(style: rune::TextStyle) -> f32 {
    let advance = rune::text::advance("M", style);
    if advance.is_finite() && advance > 0.0 {
        advance
    } else {
        TERM_FONT_SIZE * 0.6
    }
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Terminal background for an appearance: Ravenwood's dark bg0 `#141814` and
/// its cream light bg `#fdf6e3`.
pub fn terminal_bg_for(appearance: Appearance) -> Color {
    // Ravenwood dark bg0 and light cream, parsed from static literals (no
    // allocation on the per-cell path).
    let code = match appearance {
        Appearance::Dark => "#141814",
        Appearance::Light => "#fdf6e3",
    };
    rune::color::hex(code).expect("terminal background literal")
}

/// The panel fill behind the grid. On glass the opaque tone thins to a
/// translucent wash so the blurred desktop reads through like the rest of the
/// chrome; opaque platforms keep the true tone.
pub fn terminal_panel_bg(theme: &Theme) -> Color {
    if theme.is_glass() {
        theme.terminal_bg.alpha(0.4)
    } else {
        theme.terminal_bg
    }
}

/// Selection wash over the grid.
pub fn terminal_selection(theme: &Theme) -> Color {
    theme.terminal_selection
}

/// Selection wash over the grid: achromatic on purpose, so selected output
/// keeps the ANSI colors the program asked for instead of being tinted toward
/// a saturated accent (see the gpui module docs).
pub fn terminal_selection_for(appearance: Appearance) -> Color {
    match appearance {
        Appearance::Dark => Color {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 0.22,
        },
        Appearance::Light => Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.16,
        },
    }
}

/// An opaque linear-sRGB color from 8-bit sRGB components.
pub fn rgb8(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: srgb_decode(r as f32 / 255.0),
        g: srgb_decode(g as f32 / 255.0),
        b: srgb_decode(b as f32 / 255.0),
        a: 1.0,
    }
}

/// xterm 256-color cube component levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Resolve an indexed color (0-255) to RGB components for an appearance.
///
/// - **0-15** are named slots, re-tinted per theme (the two tables above).
/// - **16-231** is the 6×6×6 cube; left alone in both appearances.
/// - **232-255** is the grayscale ramp; light mode mirrors it so index 232
///   stays the faintest and 255 the strongest against the paper.
pub fn indexed_rgb(appearance: Appearance, index: u8) -> [u8; 3] {
    match index {
        0..=15 => match appearance {
            Appearance::Dark => ANSI16_DARK[index as usize],
            Appearance::Light => ANSI16_LIGHT[index as usize],
        },
        16..=231 => {
            let n = index as usize - 16;
            [
                CUBE_LEVELS[n / 36],
                CUBE_LEVELS[(n / 6) % 6],
                CUBE_LEVELS[n % 6],
            ]
        }
        _ => {
            let step = index - 232;
            let step = match appearance {
                Appearance::Dark => step,
                Appearance::Light => 23 - step,
            };
            let v = 8 + 10 * step;
            [v, v, v]
        }
    }
}

/// Resolve a cell color to paint against the theme.
pub fn resolve_color(color: CellColor, theme: &Theme) -> Color {
    match color {
        CellColor::Foreground => theme.terminal_fg,
        CellColor::Background => theme.terminal_bg,
        CellColor::Indexed(ix) => indexed_color(theme, ix),
        CellColor::Rgb(r, g, b) => rgb8(r, g, b),
    }
}

/// Resolve an indexed colour against the theme: 0–15 are the theme's ANSI
/// slots, 16–255 the fixed cube/grayscale ramp.
pub fn indexed_color(theme: &Theme, index: u8) -> Color {
    if index <= 15 {
        theme.ansi[index as usize]
    } else {
        let [r, g, b] = indexed_rgb(theme.appearance, index);
        rgb8(r, g, b)
    }
}

// ---------------------------------------------------------------------------
// Pointer → cell
// ---------------------------------------------------------------------------

/// Minimum pointer travel before a press turns into a selection.
pub const SELECTION_DRAG_THRESHOLD: f32 = 2.0;

/// Which cell a pointer landed on, and which edge of it a selection anchors to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellHit {
    pub row: usize,
    pub col: usize,
    /// Selections anchor to a cell *edge*, not a cell: pressing on the left
    /// half of a glyph includes it, the right half excludes it.
    pub side: Side,
}

/// Map a position *relative to the grid's top-left glyph* onto a cell.
///
/// Positions outside the grid clamp to the nearest cell rather than returning
/// `None`, because that is what a drag needs: the pointer routinely leaves the
/// panel mid-gesture, and the selection should extend to the edge it left
/// through instead of freezing at the last sample taken inside.
///
/// Overshoot also *forces the side*, so dragging past the bottom takes the
/// last line whole even when the pointer drifted left.
pub fn cell_at(x: f32, y: f32, cell_w: f32, line_h: f32, cols: usize, rows: usize) -> CellHit {
    let usable = |v: f32| v.is_finite() && v > 0.0;
    if cols == 0 || rows == 0 || !usable(cell_w) || !usable(line_h) {
        return CellHit {
            row: 0,
            col: 0,
            side: Side::Left,
        };
    }
    let x = if x.is_finite() { x } else { 0.0 };
    let y = if y.is_finite() { y } else { 0.0 };
    let last_col = cols - 1;
    let last_row = rows - 1;

    let raw_col = (x / cell_w).floor();
    let mut side = if x.max(0.0) % cell_w > cell_w / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    let col = if raw_col > last_col as f32 {
        side = Side::Right;
        last_col
    } else {
        raw_col.max(0.0) as usize
    };

    let raw_row = (y / line_h).floor();
    let row = if raw_row > last_row as f32 {
        side = Side::Right;
        last_row
    } else if raw_row < 0.0 {
        side = Side::Left;
        0
    } else {
        raw_row as usize
    };

    CellHit { row, col, side }
}

// ---------------------------------------------------------------------------
// Keyboard → bytes
// ---------------------------------------------------------------------------

/// Keys with the platform's primary shortcut modifier alone belong to the app
/// keymap, never the PTY. On macOS that is Command; elsewhere Rune reports
/// `cmd` together with `ctrl` for the Control key, so a plain Ctrl combo is
/// still the terminal's (Ctrl+C must reach the PTY as 0x03).
fn platform_primary(mods: &Mods) -> bool {
    if cfg!(target_os = "macos") {
        mods.cmd
    } else {
        mods.cmd && !mods.ctrl
    }
}

/// Encode a keystroke as PTY bytes. `None` means "not ours".
///
/// `app_cursor` switches arrows/home/end from CSI to SS3 per DECCKM.
pub fn keystroke_bytes(
    key: &str,
    key_char: Option<&str>,
    mods: &Mods,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    // Platform-primary combos belong to the app keymap.
    if platform_primary(mods) {
        return None;
    }
    if mods.alt {
        // ESC-prefix the same keystroke without alt.
        let inner = keystroke_bytes(
            key,
            key_char,
            &Mods {
                alt: false,
                ..*mods
            },
            app_cursor,
        )?;
        let mut out = vec![0x1b];
        out.extend(inner);
        return Some(out);
    }
    if mods.ctrl {
        return control_bytes(key);
    }

    let seq = |csi: &[u8], ss3: &[u8]| {
        Some(if app_cursor {
            ss3.to_vec()
        } else {
            csi.to_vec()
        })
    };
    match key {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "tab" => Some(if mods.shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        "escape" => Some(vec![0x1b]),
        "space" => Some(b" ".to_vec()),
        "up" => seq(b"\x1b[A", b"\x1bOA"),
        "down" => seq(b"\x1b[B", b"\x1bOB"),
        "right" => seq(b"\x1b[C", b"\x1bOC"),
        "left" => seq(b"\x1b[D", b"\x1bOD"),
        "home" => seq(b"\x1b[H", b"\x1bOH"),
        "end" => seq(b"\x1b[F", b"\x1bOF"),
        "insert" => Some(b"\x1b[2~".to_vec()),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "f1" => Some(b"\x1bOP".to_vec()),
        "f2" => Some(b"\x1bOQ".to_vec()),
        "f3" => Some(b"\x1bOR".to_vec()),
        "f4" => Some(b"\x1bOS".to_vec()),
        "f5" => Some(b"\x1b[15~".to_vec()),
        "f6" => Some(b"\x1b[17~".to_vec()),
        "f7" => Some(b"\x1b[18~".to_vec()),
        "f8" => Some(b"\x1b[19~".to_vec()),
        "f9" => Some(b"\x1b[20~".to_vec()),
        "f10" => Some(b"\x1b[21~".to_vec()),
        "f11" => Some(b"\x1b[23~".to_vec()),
        "f12" => Some(b"\x1b[24~".to_vec()),
        _ => {
            // Printable: prefer the typed character (IME/shift-aware).
            let text = key_char.filter(|c| !c.is_empty()).or({
                if key.chars().count() == 1 {
                    Some(key)
                } else {
                    None
                }
            })?;
            Some(text.as_bytes().to_vec())
        }
    }
}

/// Ctrl-key encoding (caret notation).
fn control_bytes(key: &str) -> Option<Vec<u8>> {
    let mut chars = key.chars();
    let (c, rest) = (chars.next()?, chars.next());
    if rest.is_some() {
        return match key {
            "space" => Some(vec![0x00]),
            "backspace" => Some(vec![0x08]),
            "enter" => Some(b"\r".to_vec()),
            _ => None,
        };
    }
    match c {
        'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
        '@' => Some(vec![0x00]),
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' | '/' => Some(vec![0x1f]),
        '?' => Some(vec![0x7f]),
        _ => None,
    }
}

/// The character a Rune key carries (`Key::Text`), if it types one.
pub fn event_char(ev: &KeyEvent) -> Option<char> {
    match &ev.key {
        Key::Text(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        }
        Key::Command(c) => Some(*c),
        _ => None,
    }
}

/// Map a Rune [`KeyEvent`] onto the PTY encoding. `None` falls through.
pub fn key_event_bytes(ev: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    // The platform primary modifier belongs to the app keymap.
    if platform_primary(&ev.mods) {
        return None;
    }
    let (key, key_char): (&str, Option<&str>) = match &ev.key {
        Key::Text(s) => match s.as_str() {
            " " => ("space", Some(" ")),
            _ if s.chars().count() == 1 => (s.as_str(), Some(s.as_str())),
            _ => return None,
        },
        Key::Enter => ("enter", None),
        Key::Backspace => ("backspace", None),
        Key::Delete => ("delete", None),
        Key::Left => ("left", None),
        Key::Right => ("right", None),
        Key::Up => ("up", None),
        Key::Down => ("down", None),
        Key::Home => ("home", None),
        Key::End => ("end", None),
        Key::PageUp => ("pageup", None),
        Key::PageDown => ("pagedown", None),
        Key::Insert => ("insert", None),
        Key::Escape => ("escape", None),
        Key::Tab => ("tab", None),
        Key::Function(n) => (function_name(*n)?, None),
        _ => return None,
    };
    keystroke_bytes(key, key_char, &ev.mods, app_cursor)
}

fn function_name(n: u8) -> Option<&'static str> {
    Some(match n {
        1 => "f1",
        2 => "f2",
        3 => "f3",
        4 => "f4",
        5 => "f5",
        6 => "f6",
        7 => "f7",
        8 => "f8",
        9 => "f9",
        10 => "f10",
        11 => "f11",
        12 => "f12",
        _ => return None,
    })
}

/// Wrap pasted text for the PTY (bracketed-paste aware; strips the one control
/// sequence a paste could inject).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let sanitized = text.replace("\x1b[201~", "");
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}

// ---------------------------------------------------------------------------
// Input coalescer (pure buffer; the panel owns the 12 ms timer)
// ---------------------------------------------------------------------------

/// Buffers keyboard bytes between flushes. `push` returns `true` exactly when
/// a flush timer should be scheduled (the buffer was empty), so at most one
/// flush is in flight per burst.
#[derive(Debug, Default)]
pub struct InputCoalescer {
    pending: Vec<u8>,
}

impl InputCoalescer {
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        let was_empty = self.pending.is_empty();
        self.pending.extend_from_slice(bytes);
        was_empty && !self.pending.is_empty()
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods() -> Mods {
        Mods::default()
    }

    #[test]
    fn printables_prefer_key_char() {
        assert_eq!(
            keystroke_bytes("a", Some("a"), &mods(), false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            keystroke_bytes(
                "a",
                Some("A"),
                &Mods {
                    shift: true,
                    ..mods()
                },
                false
            ),
            Some(b"A".to_vec())
        );
        // Multi-byte characters pass through as UTF-8.
        assert_eq!(
            keystroke_bytes("e", Some("é"), &mods(), false),
            Some("é".as_bytes().to_vec())
        );
        // Named single-char keys fall back to the key name.
        assert_eq!(
            keystroke_bytes("/", None, &mods(), false),
            Some(b"/".to_vec())
        );
        // Unknown multi-char keys are not ours.
        assert_eq!(keystroke_bytes("capslock", None, &mods(), false), None);
    }

    #[test]
    fn control_keys_and_sequences() {
        assert_eq!(
            keystroke_bytes("enter", None, &mods(), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_bytes("backspace", None, &mods(), false),
            Some(vec![0x7f])
        );
        assert_eq!(
            keystroke_bytes("tab", None, &mods(), false),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            keystroke_bytes(
                "tab",
                None,
                &Mods {
                    shift: true,
                    ..mods()
                },
                false
            ),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            keystroke_bytes("escape", None, &mods(), false),
            Some(vec![0x1b])
        );
        assert_eq!(
            keystroke_bytes("delete", None, &mods(), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            keystroke_bytes("pageup", None, &mods(), false),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            keystroke_bytes("f5", None, &mods(), false),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn arrows_respect_app_cursor_mode() {
        assert_eq!(
            keystroke_bytes("up", None, &mods(), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_bytes("up", None, &mods(), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            keystroke_bytes("home", None, &mods(), false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            keystroke_bytes("end", None, &mods(), true),
            Some(b"\x1bOF".to_vec())
        );
    }

    #[test]
    fn ctrl_combos_map_to_control_bytes() {
        let ctrl = Mods {
            ctrl: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("c", Some("c"), &ctrl, false),
            Some(vec![0x03])
        );
        assert_eq!(keystroke_bytes("z", None, &ctrl, false), Some(vec![0x1a]));
        assert_eq!(
            keystroke_bytes("space", None, &ctrl, false),
            Some(vec![0x00])
        );
        assert_eq!(keystroke_bytes("[", None, &ctrl, false), Some(vec![0x1b]));
        assert_eq!(keystroke_bytes("_", None, &ctrl, false), Some(vec![0x1f]));
        // Ctrl+1 has no caret encoding — not ours.
        assert_eq!(keystroke_bytes("1", Some("1"), &ctrl, false), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Mods {
            alt: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("b", Some("b"), &alt, false),
            Some(vec![0x1b, b'b'])
        );
        let alt_ctrl = Mods {
            alt: true,
            ctrl: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("c", None, &alt_ctrl, false),
            Some(vec![0x1b, 0x03])
        );
    }

    #[test]
    fn platform_primary_combos_fall_through() {
        let cmd = Mods {
            cmd: true,
            ..mods()
        };
        assert_eq!(keystroke_bytes("j", Some("j"), &cmd, false), None);
        assert_eq!(keystroke_bytes("enter", None, &cmd, false), None);
    }

    /// Off macOS Rune reports the Control key as `cmd + ctrl`; that is still a
    /// terminal combo (Ctrl+C must reach the PTY), not an app shortcut.
    #[test]
    fn plain_control_still_reaches_the_pty_off_macos() {
        if cfg!(target_os = "macos") {
            return;
        }
        let ctrl = Mods {
            cmd: true,
            ctrl: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("c", Some("c"), &ctrl, false),
            Some(vec![0x03])
        );
    }

    #[test]
    fn rune_key_events_map_onto_the_same_bytes() {
        use rune::Key;
        let ev = |key: Key| KeyEvent {
            key,
            mods: Mods::default(),
        };
        assert_eq!(
            key_event_bytes(&ev(Key::Text("a".into())), false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            key_event_bytes(&ev(Key::Enter), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(key_event_bytes(&ev(Key::Tab), false), Some(b"\t".to_vec()));
        assert_eq!(
            key_event_bytes(&ev(Key::Up), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_event_bytes(&ev(Key::Function(5)), false),
            Some(b"\x1b[15~".to_vec())
        );
        // Platform shortcuts and unknown keys are the app's, not the PTY's.
        assert_eq!(key_event_bytes(&ev(Key::Command('j')), false), None);
        assert_eq!(key_event_bytes(&ev(Key::Function(13)), false), None);
        let alt_b = KeyEvent {
            key: Key::Text("b".into()),
            mods: Mods {
                alt: true,
                ..Mods::default()
            },
        };
        assert_eq!(key_event_bytes(&alt_b, false), Some(vec![0x1b, b'b']));
    }

    #[test]
    fn paste_wraps_when_bracketed() {
        assert_eq!(paste_bytes("hi", false), b"hi".to_vec());
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
        // Close-bracket injection is stripped.
        assert_eq!(
            paste_bytes("a\x1b[201~rm -rf", true),
            b"\x1b[200~arm -rf\x1b[201~".to_vec()
        );
    }

    #[test]
    fn coalescer_schedules_once_per_burst() {
        let mut c = InputCoalescer::default();
        assert!(c.is_empty());
        assert!(c.push(b"a"), "first push schedules the flush");
        assert!(!c.push(b"b"), "subsequent pushes ride the pending flush");
        assert!(!c.push(b"c"));
        assert_eq!(c.take(), b"abc".to_vec());
        assert!(c.is_empty());
        // Next burst schedules again.
        assert!(c.push(b"d"));
        // Empty pushes never schedule.
        let mut c = InputCoalescer::default();
        assert!(!c.push(b""));
    }

    /// Relative luminance, for contrast assertions (WCAG 2.x definition).
    fn luminance(r: u8, g: u8, b: u8) -> f32 {
        let f = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }

    fn contrast(a: [u8; 3], b: [u8; 3]) -> f32 {
        let (la, lb) = (luminance(a[0], a[1], a[2]), luminance(b[0], b[1], b[2]));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn cube_is_appearance_independent() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            assert_eq!(indexed_rgb(appearance, 16), [0, 0, 0]);
            assert_eq!(indexed_rgb(appearance, 231), [255, 255, 255]);
            assert_eq!(indexed_rgb(appearance, 196), [255, 0, 0]);
            assert_eq!(indexed_rgb(appearance, 21), [0, 0, 255]);
        }
    }

    #[test]
    fn grayscale_ramp_mirrors_in_light() {
        assert_eq!(indexed_rgb(Appearance::Dark, 232), [8, 8, 8]);
        assert_eq!(indexed_rgb(Appearance::Dark, 255), [238, 238, 238]);
        assert_eq!(indexed_rgb(Appearance::Light, 232), [238, 238, 238]);
        assert_eq!(indexed_rgb(Appearance::Light, 255), [8, 8, 8]);
    }

    #[test]
    fn ansi_range_hits_the_appearance_table() {
        assert_eq!(indexed_rgb(Appearance::Dark, 1), ANSI16_DARK[1]);
        assert_eq!(indexed_rgb(Appearance::Light, 1), ANSI16_LIGHT[1]);
        assert_ne!(ANSI16_DARK, ANSI16_LIGHT);
    }

    #[test]
    fn terminal_bg_tracks_the_appearance() {
        let dark = terminal_bg_for(Appearance::Dark);
        assert_eq!(dark, crate::theme::hex("141814"));
        // Ravenwood dark bg0 is a muted olive, not pure grey.
        assert!(dark.g > dark.r && dark.r == dark.b);

        let light = terminal_bg_for(Appearance::Light);
        assert_eq!(light, crate::theme::hex("fdf6e3"));
        // Warm paper, not white, and not pure grey.
        assert!(light.r > light.g && light.g > light.b);
        let lum = light.luminance();
        assert!(lum > 0.80 && lum < 1.0, "light terminal bg is cream: {lum}");
    }

    const ANSI_BLACK: usize = 0;

    /// The regression this module's light table exists to prevent: every ANSI
    /// slot has to be readable on its *own* background.
    #[test]
    fn every_ansi_slot_is_legible_on_its_background() {
        const MIN_TEXT: f32 = 3.0;
        const MIN_GREY: f32 = 1.25;
        let cases = [
            (Appearance::Dark, ANSI16_DARK, [0x14, 0x18, 0x14]),
            (Appearance::Light, ANSI16_LIGHT, [0xfd, 0xf6, 0xe3]),
        ];
        for (appearance, table, bg) in cases {
            for (ix, color) in table.iter().enumerate() {
                let grey = ix % 8 == ANSI_BLACK;
                let min = if grey { MIN_GREY } else { MIN_TEXT };
                let ratio = contrast(*color, bg);
                assert!(
                    ratio >= min,
                    "{appearance:?} ANSI {ix} {color:?} is {ratio:.2}:1 on {bg:?}, want {min}:1"
                );
            }
        }
    }

    /// "Bright" means *more* prominent in both appearances — which is darker on
    /// a light field, not lighter.
    #[test]
    fn bright_slots_gain_emphasis_in_both_appearances() {
        let lum = |c: [u8; 3]| luminance(c[0], c[1], c[2]);
        for ix in 0..8 {
            if ix == ANSI_BLACK {
                continue;
            }
            assert!(
                lum(ANSI16_DARK[ix + 8]) > lum(ANSI16_DARK[ix]),
                "dark ANSI {ix}: bright should be lighter on near-black"
            );
            assert!(
                lum(ANSI16_LIGHT[ix + 8]) < lum(ANSI16_LIGHT[ix]),
                "light ANSI {ix}: bright should be darker on near-white"
            );
        }
        // The exception, asserted rather than merely skipped.
        assert!(lum(ANSI16_DARK[8]) > lum(ANSI16_DARK[ANSI_BLACK]));
        assert!(lum(ANSI16_LIGHT[8]) > lum(ANSI16_LIGHT[ANSI_BLACK]));
    }

    #[test]
    fn timing_constants_match_spec() {
        assert_eq!(COALESCE_MS, 12);
        assert_eq!(RESIZE_DEBOUNCE_MS, 80);
    }

    // ---- pointer → cell ----

    fn hit(x: f32, y: f32) -> CellHit {
        cell_at(x, y, 10.0, 20.0, 8, 4)
    }

    #[test]
    fn pointer_maps_to_the_cell_it_is_over() {
        assert_eq!(
            hit(0.0, 0.0),
            CellHit {
                row: 0,
                col: 0,
                side: Side::Left
            }
        );
        assert_eq!(
            hit(25.0, 45.0),
            CellHit {
                row: 2,
                col: 2,
                side: Side::Left
            }
        );
        assert_eq!(
            hit(70.0, 60.0),
            CellHit {
                row: 3,
                col: 7,
                side: Side::Left
            }
        );
    }

    #[test]
    fn side_splits_the_cell_at_its_midpoint() {
        assert_eq!(hit(21.0, 0.0).side, Side::Left);
        assert_eq!(
            hit(25.0, 0.0).side,
            Side::Left,
            "the midpoint itself is left"
        );
        assert_eq!(hit(26.0, 0.0).side, Side::Right);
        assert_eq!(hit(21.0, 0.0).col, 2);
        assert_eq!(hit(29.0, 0.0).col, 2);
    }

    #[test]
    fn overshoot_clamps_into_the_grid() {
        assert_eq!(hit(9_999.0, 0.0).col, 7);
        assert_eq!(hit(0.0, 9_999.0).row, 3);
        assert_eq!(hit(-50.0, 0.0).col, 0);
        assert_eq!(hit(0.0, -50.0).row, 0);
    }

    #[test]
    fn overshoot_forces_the_side_to_the_edge() {
        assert_eq!(hit(9_999.0, 10.0).side, Side::Right);
        assert_eq!(hit(1.0, 9_999.0).side, Side::Right);
        assert_eq!(hit(1.0, 9_999.0).col, 0);
        assert_eq!(hit(75.0, -50.0).side, Side::Left);
    }

    #[test]
    fn degenerate_metrics_do_not_panic() {
        assert_eq!(
            cell_at(5.0, 5.0, 0.0, 20.0, 8, 4),
            CellHit {
                row: 0,
                col: 0,
                side: Side::Left
            }
        );
        assert_eq!(
            cell_at(5.0, 5.0, 10.0, 20.0, 0, 0),
            CellHit {
                row: 0,
                col: 0,
                side: Side::Left
            }
        );
        assert_eq!(cell_at(f32::NAN, f32::INFINITY, 10.0, 20.0, 8, 4).col, 0);
    }

    #[test]
    fn drag_threshold_matches_the_gpui_default() {
        assert_eq!(SELECTION_DRAG_THRESHOLD, 2.0);
    }

    /// The selection veil must stay achromatic, or it tints the ANSI text it
    /// covers instead of just lifting it.
    #[test]
    fn selection_wash_is_neutral_and_translucent() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let wash = terminal_selection_for(appearance);
            assert_eq!(wash.r, wash.g, "{appearance:?} selection must have no hue");
            assert_eq!(wash.g, wash.b, "{appearance:?} selection must have no hue");
            assert!(
                wash.a > 0.0 && wash.a < 0.5,
                "{appearance:?} selection must veil the cell, not replace it"
            );
        }
        // Opposite directions: lighten the dark grid, darken the light one.
        assert_eq!(terminal_selection_for(Appearance::Dark).r, 1.0);
        assert_eq!(terminal_selection_for(Appearance::Light).r, 0.0);
    }
}
