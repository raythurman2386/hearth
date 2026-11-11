//! Settings page: ported from crates/ui/src/settings/devices.rs.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

#[derive(Debug, Default)]
pub struct DevicesPage;

pub fn render(_state: &DevicesPage, _app: &App, theme: &Theme) -> El {
    col().child(text("devices").color(theme.text))
}
