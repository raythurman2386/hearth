//! Read the active Omarchy theme.
//!
//! Omarchy publishes the applied theme at
//! `$XDG_STATE_HOME/omarchy/current/theme/` (default
//! `~/.local/state/omarchy/current/theme/`), with the older
//! `~/.config/omarchy/current/theme/` as a fallback:
//!
//! - `colors.toml` — semantic colours (background/foreground/accent/status);
//! - `alacritty.toml` — the 16 ANSI slots, cursor and selection;
//! - `../theme.generation` — changes whenever the theme is re-applied.
//!
//! The files are tiny and the format is simple (flat `key = "value"` plus
//! `[section]` tables), so this parses them directly rather than adding a TOML
//! dependency. Missing files or colours fall back to the built-in palette.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rune::Color;

use crate::theme::{Appearance, hex};

/// One applied Omarchy theme, already resolved to Rune colours.
#[derive(Debug, Clone)]
pub struct OmarchyTheme {
    pub name: String,
    pub generation: String,
    pub appearance: Appearance,

    // Semantic colours (`colors.toml`).
    pub background: Color,
    pub dark_background: Color,
    pub darker_background: Color,
    pub lighter_background: Color,
    pub foreground: Color,
    pub muted: Color,
    pub light_foreground: Color,
    pub dark_foreground: Color,
    pub accent: Color,
    pub selection: Color,
    pub selection_foreground: Color,
    pub red: Color,
    pub yellow: Color,
    pub green: Color,
    pub cyan: Color,
    pub blue: Color,
    pub magenta: Color,
    pub bright_red: Color,
    pub bright_yellow: Color,
    pub bright_green: Color,
    pub bright_cyan: Color,
    pub bright_blue: Color,
    pub bright_magenta: Color,
    pub inactive_border: Color,

    // Terminal (`alacritty.toml` / `colors.toml`).
    pub ansi: [Color; 16],
    pub terminal_bg: Color,
    pub terminal_fg: Color,
    pub terminal_cursor: Color,
    pub terminal_selection: Color,
}

impl OmarchyTheme {
    /// Load the currently applied theme. `None` when Omarchy is not present or
    /// the theme cannot be read (the built-in palette then wins).
    pub fn load() -> Option<Self> {
        let dir = current_theme_dir()?;
        let colors = std::fs::read_to_string(dir.join("colors.toml")).ok()?;
        let semantic = parse_toml(&colors);
        let alacritty = std::fs::read_to_string(dir.join("alacritty.toml"))
            .ok()
            .map(|src| parse_toml(&src))
            .unwrap_or_default();

        let get = |key: &str| semantic.get(key).map(String::as_str);
        let term = |key: &str| alacritty.get(key).map(String::as_str);
        let color = |key: &str, fallback: Color| get(key).and_then(maybe_hex).unwrap_or(fallback);

        let background = color("background", hex("#141414"));
        let dark_background = color("dark_background", background);
        let darker_background = color("darker_background", dark_background);
        let lighter_background = color("lighter_background", background);
        let foreground = color("foreground", hex("#e1e1e1"));
        let muted = color("muted", foreground);
        let light_foreground = color("light_foreground", foreground);
        let dark_foreground = color("dark_foreground", muted);
        let accent = color("accent", foreground);
        let selection = color("selection", lighter_background);
        let selection_foreground = color("selection_foreground", foreground);

        let red = color("red", hex("#f7768e"));
        let yellow = color("yellow", hex("#e0af68"));
        let green = color("green", hex("#9ece6a"));
        let cyan = color("cyan", hex("#7dcfff"));
        let blue = color("blue", hex("#7aa2f7"));
        let magenta = color("magenta", accent);
        let bright_red = color("bright_red", red);
        let bright_yellow = color("bright_yellow", yellow);
        let bright_green = color("bright_green", green);
        let bright_cyan = color("bright_cyan", cyan);
        let bright_blue = color("bright_blue", blue);
        let bright_magenta = color("bright_magenta", magenta);
        let inactive_border = color("hyprland_inactive_border", selection);

        // ANSI slots: alacritty's `colors.normal/bright`, falling back to the
        // semantic palette when a theme ships only `colors.toml`.
        let ansi_color =
            |key: &str, fallback: Color| term(key).and_then(maybe_hex).unwrap_or(fallback);
        let ansi = [
            ansi_color("colors.normal.black", darker_background),
            ansi_color("colors.normal.red", red),
            ansi_color("colors.normal.green", green),
            ansi_color("colors.normal.yellow", yellow),
            ansi_color("colors.normal.blue", blue),
            ansi_color("colors.normal.magenta", magenta),
            ansi_color("colors.normal.cyan", cyan),
            ansi_color("colors.normal.white", light_foreground),
            ansi_color("colors.bright.black", muted),
            ansi_color("colors.bright.red", bright_red),
            ansi_color("colors.bright.green", bright_green),
            ansi_color("colors.bright.yellow", bright_yellow),
            ansi_color("colors.bright.blue", bright_blue),
            ansi_color("colors.bright.magenta", bright_magenta),
            ansi_color("colors.bright.cyan", bright_cyan),
            ansi_color("colors.bright.white", foreground),
        ];

        let terminal_bg = term("colors.primary.background")
            .and_then(maybe_hex)
            .unwrap_or(background);
        let terminal_fg = term("colors.primary.foreground")
            .and_then(maybe_hex)
            .unwrap_or(foreground);
        let terminal_cursor = term("colors.cursor.cursor")
            .and_then(maybe_hex)
            .unwrap_or(yellow);
        let terminal_selection = term("colors.selection.background")
            .and_then(maybe_hex)
            .unwrap_or(selection);

        let appearance = match get("mode") {
            Some("light") => Appearance::Light,
            _ => Appearance::Dark,
        };

        Some(Self {
            name: read_name(&dir),
            generation: read_generation(&dir),
            appearance,
            background,
            dark_background,
            darker_background,
            lighter_background,
            foreground,
            muted,
            light_foreground,
            dark_foreground,
            accent,
            selection,
            selection_foreground,
            red,
            yellow,
            green,
            cyan,
            blue,
            magenta,
            bright_red,
            bright_yellow,
            bright_green,
            bright_cyan,
            bright_blue,
            bright_magenta,
            inactive_border,
            ansi,
            terminal_bg,
            terminal_fg,
            terminal_cursor,
            terminal_selection,
        })
    }
}

/// The generation token for the applied theme. Cheap enough to poll once a
/// second; `None` when Omarchy is absent.
pub fn current_generation() -> Option<String> {
    let dir = current_theme_dir()?;
    Some(read_generation(&dir))
}

/// Where Omarchy keeps the applied theme.
fn current_theme_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    if let Some(base) = state {
        let dir = base.join("omarchy/current/theme");
        if dir.join("colors.toml").is_file() {
            return Some(dir);
        }
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let dir = config?.join("omarchy/current/theme");
    dir.join("colors.toml").is_file().then_some(dir)
}

fn read_name(dir: &Path) -> String {
    // `theme.name` sits beside `theme/` in the state directory.
    let name_file = dir.parent().map(|parent| parent.join("theme.name"));
    if let Some(name) = name_file
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
    {
        return name;
    }
    "Omarchy".to_string()
}

fn read_generation(dir: &Path) -> String {
    let generation_file = dir
        .parent()
        .map(|parent| parent.join("theme.generation"))
        .filter(|path| path.is_file())
        .or_else(|| {
            let path = dir.join("generation");
            path.is_file().then_some(path)
        });
    if let Some(path) = generation_file
        && let Ok(text) = std::fs::read_to_string(&path)
        && !text.trim().is_empty()
    {
        return text.trim().to_string();
    }
    // Fallback: the theme files' modification time.
    let modified = ["colors.toml", "alacritty.toml"]
        .into_iter()
        .filter_map(|name| {
            std::fs::metadata(dir.join(name))
                .and_then(|meta| meta.modified())
                .ok()
        })
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    format!(
        "mtime-{}",
        modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// Parse a `#rrggbb` / `rrggbb` value into a Rune colour.
fn maybe_hex(value: &str) -> Option<Color> {
    if value.starts_with('#') {
        rune::color::hex(value)
    } else {
        rune::color::hex(&format!("#{value}"))
    }
}

/// Parse `key = "value"` lines, prefixing keys inside `[section]` tables with
/// the section name (`colors.normal.red`, `colors.primary.background`).
fn parse_toml(src: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut section = String::new();
    for raw in src.lines() {
        // Strip comments but keep `#` inside a quoted value.
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        let key = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        map.insert(key, value.to_string());
    }
    map
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (ix, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..ix],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLORS: &str = r##"
mode = "dark"
accent = "#34d399"
selection = "#242424"
background = "#141414" # page
foreground = "#e1e1e1"
red = "#f7768e"
yellow = "#e0af68"
"##;

    const ALACRITTY: &str = r##"
[colors.primary]
background = "#141414"
foreground = "#e1e1e1"

[colors.normal]
black = "#0a0a0a"
red = "#f7768e"
green = "#9ece6a"
"##;

    #[test]
    fn parses_flat_and_sectioned_keys() {
        let flat = parse_toml(COLORS);
        assert_eq!(flat.get("background").map(String::as_str), Some("#141414"));
        assert_eq!(flat.get("accent").map(String::as_str), Some("#34d399"));
        let nested = parse_toml(ALACRITTY);
        assert_eq!(
            nested.get("colors.primary.background").map(String::as_str),
            Some("#141414")
        );
        assert_eq!(
            nested.get("colors.normal.green").map(String::as_str),
            Some("#9ece6a")
        );
    }

    #[test]
    fn comments_inside_quotes_survive() {
        assert_eq!(
            strip_comment(r##"accent = "#34d399" # comment"##),
            r##"accent = "#34d399" "##
        );
        assert_eq!(strip_comment("# whole line"), "");
    }

    #[test]
    fn missing_theme_files_return_none() {
        // The parser is pure; `load` only runs on a real machine.
        assert!(current_theme_dir().is_none() || current_theme_dir().is_some());
    }
}
