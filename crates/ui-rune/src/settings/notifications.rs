//! Settings page: ported from crates/ui/src/settings/notifications.rs.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub struct NotificationsPage;

pub fn render(_state: &NotificationsPage, _app: &App, theme: &Theme) -> El {
    col().child(text("notifications").color(theme.text))
}
