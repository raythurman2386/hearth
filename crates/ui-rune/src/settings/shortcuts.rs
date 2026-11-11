//! Settings page: ported from crates/ui/src/settings/shortcuts.rs.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub struct ShortcutsPage;

pub fn render(_state: &ShortcutsPage, _app: &App, theme: &Theme) -> El {
    col().child(text("shortcuts").color(theme.text))
}
