//! Settings page: ported from crates/ui/src/settings/harnesses.rs.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub struct HarnessesPage;

pub fn render(_state: &HarnessesPage, _app: &App, theme: &Theme) -> El {
    col().child(text("harnesses").color(theme.text))
}
