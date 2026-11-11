//! Settings → Appearance: pick between following the system and pinning light or
//! dark.
//!
//! Uses a preview-card picker: the choice is a look, and a miniature of the
//! result says more than a sentence about it. Stateless — the choice lives in
//! [`crate::appearance::AppearanceState`], and switching repaints every window
//! through [`crate::state::App::refresh_theme`].

use rune::{Color, El, TextStyle, col, el, row, text};

use crate::appearance::AppearanceMode;
use crate::state::App;
use crate::theme::{Appearance, Theme};

use super::widgets;

/// Corner radius of the preview frame (`OPTION_CARD_RADIUS`).
pub const OPTION_CARD_RADIUS: f32 = 10.0;
/// Default height of the preview frame.
pub const OPTION_CARD_HEIGHT: f32 = 148.0;
/// Clear space between the frame and the selection ring.
pub const RING_GAP: f32 = 2.0;
/// Thickness of the selection ring.
pub const RING_WIDTH: f32 = 2.0;

/// The page has no view state of its own.
#[derive(Debug, Default)]
pub struct AppearancePage;

impl AppearancePage {
    /// The Appearance page has no async work.
    pub fn poll(&mut self) -> bool {
        false
    }
}

/// One placeholder bar in the miniature, width given as a fraction of its
/// container. Relative rather than fixed px: the System card renders this same
/// miniature into half a card.
fn bar(fraction: f32, tone: Color) -> El {
    el().h(5.0).w_frac(fraction).radius(3.0).fill(tone)
}

/// A miniature of the app in `theme`: sidebar strip, inset content card, a few
/// placeholder lines. Built from the theme's own tokens.
fn miniature(theme: &Theme) -> El {
    let line = theme.text.alpha(0.22);
    let strong = theme.text.alpha(0.34);
    row()
        .w_full()
        .h_full()
        .fill(theme.surface)
        .child(
            col()
                .w(44.0)
                .h_full()
                .gap(7.0)
                .px(8.0)
                .pt(14.0)
                .child(bar(0.70, strong))
                .child(bar(1.0, line))
                .child(bar(0.85, line))
                .child(bar(1.0, line)),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .my(8.0)
                .mr(8.0)
                .radius(6.0)
                .border(1.0, theme.border)
                .fill(theme.bg)
                .clip_rounded(6.0)
                .gap(7.0)
                .padding(10.0)
                .child(bar(0.62, strong))
                .child(bar(0.88, line))
                .child(bar(0.76, line))
                .child(bar(0.52, line)),
        )
}

/// The System card: light on the left, dark on the right.
fn miniature_split() -> El {
    row()
        .w_full()
        .h_full()
        .clip_rounded(OPTION_CARD_RADIUS)
        .child(
            el().w_frac(0.5)
                .h_full()
                .clip_rounded(OPTION_CARD_RADIUS)
                .child(miniature(&Theme::light())),
        )
        .child(
            el().w_frac(0.5)
                .h_full()
                .clip_rounded(OPTION_CARD_RADIUS)
                .child(miniature(&Theme::dark())),
        )
}

/// The preview graphic for a mode.
fn preview(mode: AppearanceMode) -> El {
    match mode {
        AppearanceMode::System => miniature_split(),
        AppearanceMode::Light => miniature(&Theme::light()),
        AppearanceMode::Dark => miniature(&Theme::dark()),
    }
}

/// Helper copy under the picker. Pure.
pub fn helper(mode: AppearanceMode, system: Appearance) -> String {
    match mode {
        AppearanceMode::System => {
            let resolved = if system.is_dark() { "dark" } else { "light" };
            format!(
                "Following the system appearance — currently {resolved}. Hearth switches with \
                 macOS, including scheduled changes."
            )
        }
        AppearanceMode::Light => "Always light, whatever the system is set to.".to_string(),
        AppearanceMode::Dark => "Always dark, whatever the system is set to.".to_string(),
    }
}

/// One card in the option row: a fixed-height preview frame carrying the
/// selection ring, with a caption underneath.
fn option_card(theme: &Theme, mode: AppearanceMode, selected: bool) -> El {
    let frame = col()
        .h(OPTION_CARD_HEIGHT)
        .w_full()
        .radius(OPTION_CARD_RADIUS)
        .border(1.0, theme.border)
        .clip_rounded(OPTION_CARD_RADIUS)
        .child(preview(mode));
    col()
        .grow(1.0)
        .min_w(0.0)
        .items_center()
        .gap(8.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |app: &mut App| set_appearance(app, mode))
        .child(
            el().w_full()
                .radius(OPTION_CARD_RADIUS + RING_GAP + RING_WIDTH)
                .padding(RING_GAP)
                .border(
                    RING_WIDTH,
                    if selected {
                        theme.accent
                    } else {
                        Color::TRANSPARENT
                    },
                )
                .child(frame),
        )
        .child(
            text(mode.label())
                .font(TextStyle::new(13.0, 400.0))
                .color(if selected {
                    theme.text
                } else {
                    theme.text_muted
                }),
        )
}

/// Apply a mode: persist it and repaint the theme when the resolved appearance
/// changed. Omarchy owns the palette when it is active.
pub fn set_appearance(app: &mut App, mode: AppearanceMode) {
    if app.omarchy.is_some() {
        return;
    }
    app.settings.appearance = mode;
    if app.appearance.set_mode(mode) {
        app.refresh_theme();
    }
}

/// The Appearance page body.
pub fn render(page: &AppearancePage, app: &App, theme: &Theme) -> El {
    let _ = page;
    if let Some(om) = &app.omarchy {
        let content = widgets::page_column()
            .child(widgets::page_header(theme, "Appearance", None))
            .child(
                widgets::page_subtitle(
                    theme,
                    "Hearth follows the active Omarchy theme. Change it system-wide; Hearth \
                     updates live.",
                )
                .max_w(512.0),
            )
            .child(
                col()
                    .mt(32.0)
                    .gap(8.0)
                    .child(widgets::field_label(theme, "Omarchy theme"))
                    .child(
                        text(om.name.clone())
                            .font(TextStyle::new(13.0, 500.0))
                            .color(theme.text),
                    )
                    .child(
                        text(format!(
                            "Set it with `omarchy theme set <name>`. The light/dark picker is \
                             disabled while an Omarchy theme is active (currently {}).",
                            if om.appearance.is_dark() {
                                "dark"
                            } else {
                                "light"
                            }
                        ))
                        .font(TextStyle::new(12.0, 400.0).lh(18.0))
                        .color(theme.text_muted)
                        .wrap(),
                    ),
            );
        return widgets::page_root("appearance-page").child(content);
    }

    let current = app.appearance.mode;
    let system = app.appearance.system;
    let cards: Vec<El> = AppearanceMode::ALL
        .into_iter()
        .map(|mode| option_card(theme, mode, mode == current))
        .collect();

    let content = widgets::page_column()
        .child(widgets::page_header(theme, "Appearance", None))
        .child(
            widgets::page_subtitle(
                theme,
                "How hearth picks between light and dark. This setting stays on this device.",
            )
            .max_w(512.0),
        )
        .child(
            col()
                .mt(32.0)
                .gap(12.0)
                .child(widgets::field_label(theme, "Theme"))
                .child(row().w_full().items_start().gap(16.0).children(cards)),
        )
        .child(
            text(helper(current, system))
                .mt(16.0)
                .font(TextStyle::new(12.0, 400.0).lh(18.0))
                .color(theme.text_muted)
                .wrap(),
        );

    widgets::page_root("appearance-page").child(content)
}

/// No page-level key handling.
pub fn on_key(_app: &mut App, _ev: &rune::KeyEvent) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_gets_a_label() {
        assert_eq!(AppearanceMode::ALL.len(), 3);
        for mode in AppearanceMode::ALL {
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn system_helper_names_the_resolved_appearance() {
        let dark = helper(AppearanceMode::System, Appearance::Dark);
        let light = helper(AppearanceMode::System, Appearance::Light);
        assert!(dark.contains("currently dark"), "got {dark}");
        assert!(light.contains("currently light"), "got {light}");
    }

    /// The pinned modes must not claim to follow anything — that copy is the
    /// only thing telling the user the system setting is being ignored.
    #[test]
    fn pinned_helpers_do_not_mention_following() {
        for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
            for system in [Appearance::Light, Appearance::Dark] {
                let copy = helper(mode, system).to_lowercase();
                assert!(!copy.contains("following"), "{mode:?}: {copy}");
                assert!(copy.contains("whatever the system"), "{mode:?}: {copy}");
            }
        }
    }

    #[test]
    fn light_and_dark_previews_draw_from_different_palettes() {
        let (l, d) = (Theme::light(), Theme::dark());
        assert_ne!(l.surface, d.surface);
        assert_ne!(l.bg, d.bg);
    }

    #[test]
    fn appearance_state_import_is_used() {
        assert_eq!(AppearanceMode::System.label(), "System");
    }
}
