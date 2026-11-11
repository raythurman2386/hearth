//! Settings page: ported from crates/ui/src/settings/appearance.rs.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub struct AppearancePage;

pub fn render(_state: &AppearancePage, _app: &App, theme: &Theme) -> El {
    col().child(text("appearance").color(theme.text))
}
