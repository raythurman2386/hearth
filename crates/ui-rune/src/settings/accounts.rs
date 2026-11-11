//! Settings page: ported from crates/ui/src/settings/accounts.rs.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub struct AccountsPage;

pub fn render(_state: &AccountsPage, _app: &App, theme: &Theme) -> El {
    col().child(text("accounts").color(theme.text))
}
