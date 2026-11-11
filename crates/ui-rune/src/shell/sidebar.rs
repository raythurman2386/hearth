//! Sidebar: space filter, session list, archived accordion, user menu.
//! Filled in by the shell sidebar port.

use rune::{col, text, El};

use crate::state::App;
use crate::theme::Theme;

pub fn render(app: &App, theme: &Theme) -> El {
    col()
        .w_full()
        .h_full()
        .padding(crate::theme::Theme::SPACE_MD)
        .gap(crate::theme::Theme::SPACE_SM)
        .child(text("Sessions").font(rune::tok::type_::SECTION).color(theme.text))
        .child(
            text(format!("{} visible", app.visible_chats().count())).color(theme.text_muted),
        )
}
