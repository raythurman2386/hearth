//! Loaders: the hearth pulse, the gradient matrix spinner and the boot splash.
//!
//! Rune owns the repeating clock: each cell is an ordinary element with a
//! [`El::looping`](rune::El::looping) opacity track, phase-shifted by the shared
//! [`Cycle`](rune_motion::Cycle). Reduced motion holds the first pose; the window goes back to
//! sleep when no loader is mounted.

use rune::{Color, El, LoopSpec, Shape, col, el, icon, row, text};
use rune_motion::Cycle;

use crate::icons;
use crate::motion;
use crate::theme::Theme;

// Shared with the terminal viewport so both animate the same loaders from the same numbers.
pub use hearth_proto::motion::{
    GSPIN_DIM, GSPIN_ROW_TINTS, MARK_CELLS, MARK_SPREAD, MATRIX_SIDE, mark_cell_stagger,
};

/// The gradient-spin cycle: a cell rests dim, brightens toward the front of the wave, then dims.
/// The four stops mirror `gspin_opacity`'s 0.45 / 0.92 ramp.
static GSPIN_STOPS: [(f32, f32); 4] =
    [(0.0, 1.0), (0.45, GSPIN_DIM), (0.88, GSPIN_DIM), (1.0, 1.0)];

fn gspin_cycle(period: f32, phase: f32) -> Cycle {
    Cycle {
        period,
        phase,
        stops: &GSPIN_STOPS,
        still: GSPIN_DIM,
        fps: 30.0,
    }
}

/// One spinner cell: a round tinted dot whose opacity rides the shared wave.
fn spinner_cell(tint: Color, cell_px: f32, phase: f32) -> El {
    el().size(cell_px, cell_px)
        .radius(cell_px / 2.0)
        .fill(tint)
        .looping(LoopSpec::cycle(gspin_cycle(0.75, phase)))
}

/// The gradient matrix spinner (WorkingIndicator), ported from hearth's
/// `gradient-spin.tsx`: a 3x3 grid of round cells tinted per row from the sunrise gradient.
/// The pulse enters at the bottom edge and converges toward the top-centre cell.
pub fn gradient_spinner(theme: &Theme, cell_px: f32) -> El {
    let _ = theme;
    let center = (MATRIX_SIDE as f32 - 1.0) / 2.0;
    let max = MATRIX_SIDE as f32 - 1.0 + center;
    col()
        .gap(cell_px / 2.0)
        .children((0..MATRIX_SIDE).map(move |r| {
            let tint = crate::theme::hex(&format!("{:06x}", GSPIN_ROW_TINTS[r]));
            row()
                .gap(cell_px / 2.0)
                .children((0..MATRIX_SIDE).map(move |c| {
                    let d = MATRIX_SIDE as f32 - 1.0 - r as f32 + (c as f32 - center).abs();
                    let phase = if max == 0.0 { 0.0 } else { d / (max + 1.0) };
                    spinner_cell(tint, cell_px, phase)
                }))
        }))
}

/// A 2x3 miniature of [`gradient_spinner`] for a status-dot slot. The brightness snakes around
/// the grid's perimeter — a tiny radial chase (~6x10 px at 2.5px cells).
pub fn mini_gradient_spinner(_theme: &Theme, cell_px: f32) -> El {
    let tints = GSPIN_ROW_TINTS.map(|t| crate::theme::hex(&format!("{t:06x}")));
    mini_spinner_tinted(&tints, cell_px)
}

/// [`mini_gradient_spinner`] in a single flat tint — the grayscale take for surfaces where the
/// brand gradient would pull focus (the sidebar connection pill).
pub fn mini_mono_spinner(tint: Color, cell_px: f32) -> El {
    mini_spinner_tinted(&[tint; 3], cell_px)
}

fn mini_spinner_tinted(row_tints: &[Color; 3], cell_px: f32) -> El {
    const COLS: usize = 2;
    const ROWS: usize = 3;
    /// Clockwise ring position of each `(row, col)` cell, top-left first.
    const RING: [[usize; COLS]; ROWS] = [[0, 1], [5, 2], [4, 3]];
    const RING_LEN: f32 = (COLS * ROWS) as f32;
    col().gap(cell_px / 2.0).children((0..ROWS).map(move |r| {
        let tint = row_tints[r];
        row().gap(cell_px / 2.0).children(
            (0..COLS).map(move |c| spinner_cell(tint, cell_px, RING[r][c] as f32 / RING_LEN)),
        )
    }))
}

/// The hearth mark, breathing on the 2.4s pulse.
pub fn hearth_pulse(theme: &Theme, size: f32) -> El {
    icon(&icons::HEARTH_LOGO, size)
        .color(theme.text)
        .looping(LoopSpec::opacity(2.4, 0.0, 0.72, 1.0))
}

/// Stroke width of [`upload_progress_ring`].
const RING_STROKE: f32 = 2.5;
/// Polyline segments for a full circle.
const RING_SEGMENTS: f32 = 64.0;

fn arc_points(diameter: f32, sweep: f32) -> Vec<[f32; 2]> {
    let radius = diameter / 2.0 - RING_STROKE;
    let center = diameter / 2.0;
    let steps = ((RING_SEGMENTS * sweep).ceil() as usize).max(2);
    (0..=steps)
        .map(|i| {
            let theta = -std::f32::consts::FRAC_PI_2
                + std::f32::consts::TAU * sweep * (i as f32 / steps as f32);
            [center + radius * theta.cos(), center + radius * theta.sin()]
        })
        .collect()
}

/// Radial upload-progress ring with the percent centred, over an image thumbnail while its
/// bytes cross the relay. A faint full track plus a bright arc growing clockwise from 12.
pub fn upload_progress_ring(percent: u8, diameter: f32) -> El {
    let frac = f32::from(percent.min(100)) / 100.0;
    let mut shape = Shape::new()
        .stroke(
            arc_points(diameter, 1.0),
            RING_STROKE,
            Color::WHITE.alpha(0.22),
        )
        .view_box(diameter, diameter);
    if frac > 0.0 {
        shape = shape.stroke(
            arc_points(diameter, frac),
            RING_STROKE,
            Color::WHITE.alpha(0.95),
        );
    }
    el().size(diameter, diameter)
        .center()
        .child(
            rune::canvas(shape)
                .size(diameter, diameter)
                .absolute(0.0, 0.0),
        )
        .child(
            text(format!("{percent}%"))
                .font(rune::tok::type_::MONO_CAPTION)
                .color(Color::WHITE.alpha(0.95)),
        )
}

/// Full-window boot splash (hearth `App.tsx` `Splash`): the ASCII hero over the glass, with an
/// uppercase tracked "Loading" line. `fading` plays the splash-out sequence; the shell removes
/// it once the phase reaches `Gone`.
pub fn splash_overlay(theme: &Theme, fading: bool) -> El {
    let content = col()
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .fill(theme.glass())
        .center()
        .gap(28.0)
        .child(hero_ascii(theme))
        .child(loading_word(theme));
    if fading {
        motion::splash_out(content)
    } else {
        content
    }
}

/// The landing page's hero comet, monochrome: one text line per row. The rectangle is softened
/// on all four edges so the block dissolves into the frost.
fn hero_ascii(theme: &Theme) -> El {
    /// Glyph cell: the site runs 7.6px/1.25; a hair smaller keeps the 110-column art inside a
    /// narrow window.
    const FONT: f32 = 7.0;
    const LINE: f32 = 8.75;
    const FADE_BAND: f32 = 72.0;
    let art = col()
        .font(
            rune::TextStyle::new(FONT, 400.0)
                .mono()
                .lh(LINE)
                .features("'liga' 0, 'calt' 0, 'dlig' 0"),
        )
        .color(theme.text.alpha(0.55))
        .children(HERO_ASCII.lines().map(|line| text(line.to_string())));
    crate::edge_fade::fade_all(art, FADE_BAND)
}

/// The landing page's hero comet (`apps/landing/public/index.html` `.hero-ascii`).
const HERO_ASCII: &str = include_str!("../assets/hero.txt");

/// "L O A D I N G" — `text-[11px] uppercase tracking-[0.32em] text-muted-foreground/70`.
pub fn loading_word(theme: &Theme) -> El {
    text("L\u{2009}O\u{2009}A\u{2009}D\u{2009}I\u{2009}N\u{2009}G")
        .font(rune::TextStyle::new(11.0, 500.0).tracking(1.2))
        .color(theme.text_muted.alpha(0.7))
}

/// A tiny inline "thinking" loader for buttons and inline status: Rune-kit's Spinner when the
/// caller wants a uniform ring, the hearth matrix when they want the brand wave.
pub fn thinking(theme: &Theme) -> El {
    rune_kit::Spinner::new()
        .size(14.0)
        .color(theme.text_muted)
        .build(&theme.kit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_arc_starts_at_twelve_and_ends_clockwise() {
        let points = arc_points(20.0, 0.25);
        let first = points.first().unwrap();
        let last = points.last().unwrap();
        // 12 o'clock is (center, top).
        assert!((first[0] - 10.0).abs() < 1e-4);
        assert!((first[1] - (10.0 - 7.5)).abs() < 1e-4);
        // A quarter sweep lands at 3 o'clock.
        assert!((last[0] - (10.0 + 7.5)).abs() < 1e-3);
        assert!((last[1] - 10.0).abs() < 1e-3);
    }

    #[test]
    fn a_full_ring_ends_where_it_began() {
        let points = arc_points(20.0, 1.0);
        let first = points.first().unwrap();
        let last = points.last().unwrap();
        assert!((first[0] - last[0]).abs() < 1e-3);
        assert!((first[1] - last[1]).abs() < 1e-3);
    }

    #[test]
    fn mark_stagger_follows_flight_axis() {
        let tail = mark_cell_stagger(720.0, 0.0);
        let head = mark_cell_stagger(0.0, 840.0);
        assert!(tail > head);
        assert!((head - 0.0).abs() < 1e-6);
        assert!(tail <= MARK_SPREAD + 1e-6);
        for &(x, y) in &MARK_CELLS {
            let s = mark_cell_stagger(x, y);
            assert!((0.0..=MARK_SPREAD + 1e-6).contains(&s));
        }
    }

    #[test]
    fn splash_art_is_present() {
        assert!(HERO_ASCII.lines().count() > 20);
    }
}
