//! Settings page: ported from crates/ui/src/settings/archived.rs.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub struct ArchivedPage;

pub fn render(_state: &ArchivedPage, _app: &App, theme: &Theme) -> El {
    col().child(text("archived").color(theme.text))
}
