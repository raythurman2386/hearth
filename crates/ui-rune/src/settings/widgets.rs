//! Shared settings page widgets: page column, header, section cards, rows,
//! toggle switches, badges, ghost actions, error strips.
//!
//! The layout numbers are the gpui settings constants (`mx-auto w-full max-w-3xl
//! px-6 pb-16 pt-8`, section cards `rounded-xl border bg-card`, rows
//! `px-5 py-3.5`); colours come from [`Theme`].

use rune::{Color, El, ScrollSpec, TextStyle, col, el, row, text};

use crate::icons;
use crate::theme::Theme;

/// Shared typography for a settings component's title and description.
pub const ROW_TITLE_SIZE: f32 = 13.0;
pub const ROW_DESCRIPTION_SIZE: f32 = 12.0;

/// The centered page column: `mx-auto w-full max-w-3xl px-6 pb-16 pt-8`.
pub const PAGE_MAX_WIDTH: f32 = 768.0;
pub const PAGE_PAD_X: f32 = 24.0;
pub const PAGE_PAD_TOP: f32 = 32.0;
pub const PAGE_PAD_BOTTOM: f32 = 64.0;

/// One page's scroll viewport: the full outlet, with the centered page column
/// as its only child. The page itself scrolls, like the gpui pages'
/// `size_full().overflow_y_scroll()`.
pub fn page_root(key: &'static str) -> El {
    col()
        .key(key)
        .w_full()
        .h_full()
        .min_h(0.0)
        .items_center()
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
}

/// The centered page column (`max-w-3xl`).
pub fn page_column() -> El {
    col()
        .w_full()
        .max_w(PAGE_MAX_WIDTH)
        .px(PAGE_PAD_X)
        .pt(PAGE_PAD_TOP)
        .pb(PAGE_PAD_BOTTOM)
}

/// Page headline row: `flex items-baseline gap-2.5`.
pub fn page_header(theme: &Theme, title: &str, count: Option<usize>) -> El {
    let mut header = row().gap(10.0).center_y().child(
        text(title.to_string())
            .font(TextStyle::new(16.0, 600.0))
            .color(theme.text),
    );
    if let Some(count) = count {
        header = header.child(
            text(count.to_string())
                .font(TextStyle::new(13.0, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        );
    }
    header
}

/// Subtitle under the headline: `mt-1 text-[13px] text-muted-foreground`.
pub fn page_subtitle(theme: &Theme, copy: impl Into<String>) -> El {
    text(copy.into())
        .font(TextStyle::new(13.0, 400.0))
        .color(theme.text_muted)
        .wrap()
        .mt(4.0)
}

/// Small label above a group of controls.
pub fn field_label(theme: &Theme, label: impl Into<String>) -> El {
    text(label.into())
        .font(TextStyle::new(13.0, 500.0))
        .color(theme.text)
}

/// Section card: `mt-6 overflow-hidden rounded-xl border border-border bg-card`.
pub fn section_card(theme: &Theme) -> El {
    col()
        .mt(24.0)
        .radius(12.0)
        .border(1.0, theme.border)
        .fill(theme.card_glass_bg())
        .clip_rounded(12.0)
}

/// One card row: `border-t border-border px-5 py-3.5 first:border-t-0` with the
/// quiet hover wash.
pub fn card_row(theme: &Theme, first: bool) -> El {
    row()
        .px(20.0)
        .py(14.0)
        .gap(14.0)
        .center_y()
        .hover(theme.ink(0.015))
        .border_sides(
            if first {
                [0.0; 4]
            } else {
                [1.0, 0.0, 0.0, 0.0]
            },
            theme.border,
        )
}

/// The identity tile on a row: `size-9 rounded-[10px] border bg-white/[0.03]`
/// around a 16px icon.
pub fn row_tile(theme: &Theme, icon: &'static rune::Icon) -> El {
    el().w(36.0)
        .h(36.0)
        .radius(10.0)
        .border(1.0, theme.border)
        .fill(theme.ink(0.03))
        .center()
        .child(icons::icon(icon, 16.0).color(theme.text_muted))
}

/// A monogram tile used where the Rune port has no brand icon.
pub fn row_tile_text(theme: &Theme, mark: &str) -> El {
    el().w(36.0)
        .h(36.0)
        .radius(10.0)
        .border(1.0, theme.border)
        .fill(theme.ink(0.03))
        .center()
        .child(
            text(mark.to_string())
                .font(TextStyle::new(12.0, 600.0))
                .color(theme.text_muted),
        )
}

/// Row title (`text-[13px] font-medium`).
pub fn row_title(theme: &Theme, title: impl Into<String>) -> El {
    text(title.into())
        .min_w(0.0)
        .truncate()
        .font(TextStyle::new(ROW_TITLE_SIZE, 500.0))
        .color(theme.text)
}

/// One quiet meta fragment (`text-[12px] text-muted-foreground/65`).
pub fn meta_text(theme: &Theme, copy: impl Into<String>) -> El {
    text(copy.into())
        .font(TextStyle::new(ROW_DESCRIPTION_SIZE, 400.0))
        .color(theme.text_muted.alpha(0.65))
}

/// The quiet meta line: fragments joined by a faint dot.
pub fn meta_line(theme: &Theme, fragments: Vec<El>) -> El {
    let mut line = row()
        .mt(Theme::TEXT_STACK_GAP)
        .flex_wrap()
        .items_center()
        .gaps(2.0, 8.0);
    let mut first = true;
    for fragment in fragments {
        if !first {
            line = line.child(
                text("·")
                    .font(TextStyle::new(ROW_DESCRIPTION_SIZE, 400.0))
                    .color(theme.text_muted.alpha(0.3)),
            );
        }
        line = line.child(fragment);
        first = false;
    }
    line
}

/// Right-anchored badge pill: `rounded-full border px-2 py-0.5 text-[10.5px]`.
pub fn badge(theme: &Theme, label: impl Into<String>) -> El {
    row()
        .h(18.0)
        .px(8.0)
        .radius(9.0)
        .border(1.0, theme.border)
        .center_y()
        .child(
            text(label.into())
                .font(TextStyle::new(10.5, 400.0))
                .color(theme.text_muted),
        )
}

/// Emerald status pill (the Accounts "Active" badge).
pub fn badge_active(theme: &Theme, label: impl Into<String>) -> El {
    row()
        .h(18.0)
        .px(8.0)
        .radius(9.0)
        .fill(theme.success.alpha(0.12))
        .center_y()
        .child(
            text(label.into())
                .font(TextStyle::new(10.5, 400.0))
                .color(theme.success_muted.alpha(0.9)),
        )
}

/// Display-only toggle switch (18×32 pill); the caller adds the press handler.
pub fn toggle_switch(theme: &Theme, on: bool) -> El {
    el().w(32.0)
        .h(18.0)
        .radius(9.0)
        .fill(if on { theme.text } else { theme.ink(0.15) })
        .child(
            el().absolute(if on { 16.0 } else { 2.0 }, 2.0)
                .w(14.0)
                .h(14.0)
                .radius(7.0)
                .fill(if on { theme.on_solid } else { theme.ink(0.7) }),
        )
}

/// A small quiet ghost action (`rounded-lg px-2.5 py-1.5 text-[12px]`).
/// The caller adds the children, press handler and may add a hover wash.
pub fn ghost_action(_theme: &Theme) -> El {
    row()
        .px(10.0)
        .py(6.0)
        .gap(6.0)
        .radius(8.0)
        .center_y()
        .cursor(rune::Cursor::Pointer)
}

/// A ghost action with its label already in place.
pub fn ghost_label(theme: &Theme, label: impl Into<String>) -> El {
    ghost_action(theme).hover(theme.ink(0.06)).child(
        text(label.into())
            .font(TextStyle::new(12.0, 400.0))
            .color(theme.text_muted),
    )
}

/// The standard ghost hover wash.
pub fn ghost_hover(theme: &Theme) -> Color {
    theme.ink(0.06)
}

/// The dismissible red error strip.
pub fn error_strip(theme: &Theme, message: impl Into<String>) -> El {
    let red = theme.danger;
    let red_text = theme.danger_muted;
    row()
        .mt(16.0)
        .px(16.0)
        .py(12.0)
        .radius(12.0)
        .border(1.0, red.alpha(0.2))
        .fill(red.alpha(0.06))
        .items_start()
        .gap(8.0)
        .child(
            el().mt(2.0)
                .child(icons::icon(&icons::DANGER_TRIANGLE, 16.0).color(red_text.alpha(0.9))),
        )
        .child(
            text(message.into())
                .min_w(0.0)
                .font(TextStyle::new(12.5, 400.0))
                .color(red_text.alpha(0.9))
                .wrap(),
        )
}

/// The amber warning strip.
pub fn warning_strip(theme: &Theme, message: impl Into<String>) -> El {
    let amber = theme.warning;
    let amber_text = theme.warning_muted;
    row()
        .mt(8.0)
        .px(16.0)
        .py(10.0)
        .radius(12.0)
        .border(1.0, amber.alpha(0.2))
        .fill(amber.alpha(0.06))
        .items_start()
        .gap(8.0)
        .child(
            el().mt(2.0)
                .child(icons::icon(&icons::DANGER_TRIANGLE, 14.0).color(amber_text.alpha(0.9))),
        )
        .child(
            text(message.into())
                .min_w(0.0)
                .font(TextStyle::new(12.0, 400.0))
                .color(amber_text.alpha(0.9))
                .wrap(),
        )
}

/// A ghost row shaped like a loaded card row, for load skeletons. `dim` fades
/// the second/alternate row as the gpui accounts skeleton did.
pub fn skeleton_row(theme: &Theme, first: bool, dim: bool) -> El {
    let ghost = |w: f32, h: f32, round: f32| el().w(w).h(h).radius(round).fill(theme.ink(0.05));
    row()
        .px(20.0)
        .py(14.0)
        .gap(12.0)
        .items_center()
        .border_sides(
            if first {
                [0.0; 4]
            } else {
                [1.0, 0.0, 0.0, 0.0]
            },
            theme.border,
        )
        .opacity(if dim { 0.6 } else { 1.0 })
        .child(ghost(32.0, 32.0, 16.0))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(7.0)
                .child(ghost(176.0, 13.0, 4.0))
                .child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(ghost(48.0, 9.0, 4.0))
                        .child(
                            el().grow(1.0)
                                .min_w(56.0)
                                .max_w(230.0)
                                .h(5.0)
                                .radius(3.0)
                                .fill(theme.ink(0.04)),
                        ),
                ),
        )
        .child(ghost(64.0, 21.0, 10.0))
}

/// A card of `rows` skeleton rows.
pub fn skeleton_card(theme: &Theme, rows: usize) -> El {
    let mut card = section_card(theme);
    for ix in 0..rows {
        card = card.child(skeleton_row(theme, ix == 0, ix % 2 == 1));
    }
    card
}

/// A full-width separator line for card bodies that are not `card_row` rows.
pub fn hairline(theme: &Theme) -> El {
    el().w_full().h(1.0).fill(theme.ink(0.08))
}
