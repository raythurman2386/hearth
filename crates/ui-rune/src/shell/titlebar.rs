//! Window titlebar: traffic-light clearance, sidebar toggle, back/forward,
//! session identity and right-pane controls. Filled in by the titlebar port.

use rune::El;

use crate::state::App;
use crate::theme::Theme;

pub fn render(_app: &App, _theme: &Theme) -> El {
    rune::el().w_full().h_full()
}
