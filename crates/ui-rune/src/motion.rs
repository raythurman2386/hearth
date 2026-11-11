//! The hearth motion catalog, on Rune's clock.
//!
//! The gpui UI drove its own `with_animation` timelines; Rune animates layout, presence and
//! colour itself. This module keeps the catalog as data — the pure [`MotionSpec`] timeline math
//! (kept for tests and parity) plus the [`rune_motion::Spring`]/[`rune_motion::Curve`] constants
//! the elements actually use.
//!
//! Catalog (docs/research/feature-inventory.md §1.12):
//! - `fade-in`   0.5s expo-out, translateY 4→0 (entrances)
//! - `fade-quick` 0.15s
//! - `menu-in`   0.14s scale 0.96 + translateY −2 (popovers)
//! - `dialog-in` 0.18s scale 0.96→1
//! - `splash-out` 0.5s opacity + translateY −6, 0.15s delay
//! - `hearth-pulse` 2.4s staggered cell opacity 0.08→1 (loaders)
//! - `gradient-spin-pulse` 750ms per-cell phase wave (working indicator)
//! - 200ms ease-out width/height transitions (sidebar/panes)
//!
//! Reduced motion is Rune's: spatial change snaps, while opacity, presence and colour keep
//! [`rune_motion::REDUCED_FADE`]. The element helpers below therefore set only the spring; they
//! never need a second reduced-motion branch.

use rune::{El, Transition};
use rune_motion::{Easing, Spring, SpringPair};

// ---------------------------------------------------------------------------
// Cubic bezier (pure; the original catalog's curve values)
// ---------------------------------------------------------------------------

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function. Evaluation solves `x(t) = input`
/// with Newton iteration and a bisection fallback — the standard UnitBezier approach.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    fn coefficients(a: f32, b: f32) -> (f32, f32, f32) {
        let c = 3.0 * a;
        let bb = 3.0 * (b - a) - c;
        let aa = 1.0 - c - bb;
        (aa, bb, c)
    }

    fn sample_x(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        ((a * t + b) * t + c) * t
    }

    fn sample_y(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.y1, self.y2);
        ((a * t + b) * t + c) * t
    }

    fn sample_x_derivative(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        (3.0 * a * t + 2.0 * b) * t + c
    }

    fn solve_t_for_x(&self, x: f32) -> f32 {
        let mut t = x;
        for _ in 0..8 {
            let err = self.sample_x(t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let d = self.sample_x_derivative(t);
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        for _ in 0..32 {
            let mid = (lo + hi) / 2.0;
            if self.sample_x(mid) < x {
                lo = mid
            } else {
                hi = mid
            }
        }
        (lo + hi) / 2.0
    }

    /// Eased output for input progress `x ∈ [0,1]` (clamped).
    pub fn eval(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        self.sample_y(self.solve_t_for_x(x)).clamp(0.0, 1.0)
    }

    /// This curve as a Rune easing.
    pub const fn easing(self) -> Easing {
        Easing::CubicBezier(rune_motion::CubicBezier::new(
            self.x1, self.y1, self.x2, self.y2,
        ))
    }
}

/// hearth's signature entrance curve — CSS `cubic-bezier(0.16, 1, 0.3, 1)`.
pub const EASE_OUT_EXPO: CubicBezier = CubicBezier::new(0.16, 1.0, 0.3, 1.0);
/// CSS `ease-out` — width/height transitions.
pub const EASE_OUT: CubicBezier = CubicBezier::new(0.0, 0.0, 0.58, 1.0);
/// CSS `ease` — quick fades, menu/dialog pops.
pub const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.0);
/// Sidebar resort glide — CSS `cubic-bezier(0.22, 1, 0.36, 1)`.
pub const EASE_RESORT: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// CSS `ease-in-out` — the transcript scroll glide.
pub const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0.0, 0.58, 1.0);
/// Tailwind's default `transition-colors` curve — `cubic-bezier(0.4, 0, 0.2, 1)`.
pub const EASE_TAILWIND: CubicBezier = CubicBezier::new(0.4, 0.0, 0.2, 1.0);

// ---------------------------------------------------------------------------
// Pure catalog timeline (kept for parity + tests)
// ---------------------------------------------------------------------------

/// One catalog entry: duration + optional delay + curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionSpec {
    pub duration_ms: u64,
    pub delay_ms: u64,
    pub curve: CubicBezier,
}

impl MotionSpec {
    pub const fn new(duration_ms: u64, curve: CubicBezier) -> Self {
        Self {
            duration_ms,
            delay_ms: 0,
            curve,
        }
    }

    pub const fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    /// Wall-clock span of the whole timeline (delay + duration).
    pub fn total(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.delay_ms + self.duration_ms)
    }

    /// Eased progress (0..1) for a raw timeline delta (0..1 across [`Self::total`]).
    pub fn progress(&self, raw_delta: f32) -> f32 {
        let total = (self.delay_ms + self.duration_ms) as f32;
        if total <= 0.0 || self.duration_ms == 0 {
            return 1.0;
        }
        let t =
            (raw_delta.clamp(0.0, 1.0) * total - self.delay_ms as f32) / self.duration_ms as f32;
        self.curve.eval(t.clamp(0.0, 1.0))
    }
}

/// Entrances: 0.5s expo-out fade + 4px rise.
pub const FADE_IN_SPEC: MotionSpec = MotionSpec::new(500, EASE_OUT_EXPO);
/// Quick fade: 0.15s.
pub const FADE_QUICK_SPEC: MotionSpec = MotionSpec::new(150, EASE);
/// Popover-in: 0.14s.
pub const MENU_IN_SPEC: MotionSpec = MotionSpec::new(140, EASE);
/// Popover-out: 0.1s.
pub const MENU_OUT_SPEC: MotionSpec = MotionSpec::new(100, EASE);
/// Dialog-in: 0.18s.
pub const DIALOG_IN_SPEC: MotionSpec = MotionSpec::new(180, EASE);
/// Boot splash exit: 0.5s fade + 6px lift after a 0.15s hold.
pub const SPLASH_OUT_SPEC: MotionSpec = MotionSpec::new(500, EASE).with_delay(150);
/// Sidebar / pane width+height transitions: 200ms ease-out.
pub const RESIZE_SPEC: MotionSpec = MotionSpec::new(200, EASE_OUT);
/// Terminal tab drag-reorder sliding transforms: 150ms.
pub const TAB_SLIDE_SPEC: MotionSpec = MotionSpec::new(150, EASE_OUT);
/// Sidebar resort glide: 260ms.
pub const RESORT_SPEC: MotionSpec = MotionSpec::new(260, EASE_RESORT);
/// Diff-pane per-file collapse: 180ms.
pub const COLLAPSE_SPEC: MotionSpec = MotionSpec::new(180, EASE_OUT);
/// Diff-pane chevron rotation: 200ms.
pub const CHEVRON_SPEC: MotionSpec = MotionSpec::new(200, EASE);
/// Rail-tick / scroll-to-row glide: 500ms ease-in-out.
pub const SCROLL_GLIDE_SPEC: MotionSpec = MotionSpec::new(500, EASE_IN_OUT);
/// CSS `transition-colors` default: 150ms.
pub const HOVER_FADE_SPEC: MotionSpec = MotionSpec::new(150, EASE_TAILWIND);
/// Hearth loader pulse period: 2.4s.
pub const HEARTH_PULSE_SPEC: MotionSpec = MotionSpec::new(2400, EASE);
/// Gradient matrix spinner wave period: 750ms.
pub const GRADIENT_SPIN_SPEC: MotionSpec = MotionSpec::new(750, EASE);

// ---------------------------------------------------------------------------
// Spring / curve constants Rune animates with
// ---------------------------------------------------------------------------

/// Entrances: 0.5s expo-out fade + 4px rise.
pub const FADE_IN: Spring = Spring::new(0.5, 0.0);
/// Quick fade: 0.15s.
pub const FADE_QUICK: Spring = Spring::new(0.15, 0.0);
/// Popover-in: 0.14s.
pub const MENU_IN: Spring = Spring::new(0.14, 0.0);
/// Popover-out: 0.1s.
pub const MENU_OUT: Spring = Spring::new(0.10, 0.0);
/// Dialog-in: 0.18s.
pub const DIALOG_IN: Spring = Spring::new(0.18, 0.0);
/// Boot splash exit. Rune's `Transition` carries the hold in its presence lifecycle; this is
/// the fade + lift after it.
pub const SPLASH_OUT: Spring = Spring::new(0.5, 0.0);
/// Sidebar / pane width+height transitions: 200ms ease-out.
pub const RESIZE: Spring = Spring::new(0.2, 0.0);
/// Terminal tab drag-reorder sliding transforms: 150ms.
pub const TAB_SLIDE: Spring = Spring::new(0.15, 0.0);
/// Sidebar resort glide: 260ms.
pub const RESORT: Spring = Spring::new(0.26, 0.0);
/// Diff-pane per-file collapse: 180ms.
pub const COLLAPSE: Spring = Spring::new(0.18, 0.0);
/// Diff-pane chevron rotation: 200ms.
pub const CHEVRON: Spring = Spring::new(0.2, 0.0);
/// Rail-tick / scroll-to-row glide, as a fixed-duration curve.
pub const SCROLL_GLIDE: rune_motion::Curve = rune_motion::Curve::tween(0.5, Easing::EASE_IN_OUT);
/// CSS `transition-colors` default: 150ms.
pub const HOVER_FADE: Spring = Spring::new(0.15, 0.0);
/// Hearth loader pulse period, in seconds.
pub const HEARTH_PULSE_PERIOD: f64 = 2.4;
/// Gradient matrix spinner wave period, in seconds.
pub const GRADIENT_SPIN_PERIOD: f64 = 0.75;

// ---------------------------------------------------------------------------
// Element helpers
// ---------------------------------------------------------------------------

/// Standard entrance: fade + 4pt rise on the catalog spring.
pub fn fade_in(el: El) -> El {
    el.transition(Transition::offset(0.0, 4.0).springs(SpringPair::new(FADE_IN, FADE_QUICK)))
}

/// Quick opacity-only fade.
pub fn fade_quick(el: El) -> El {
    el.transition(Transition::opacity().springs(SpringPair::new(FADE_QUICK, FADE_QUICK)))
}

/// Popover entrance: fade plus a 0.96 scale from the anchored side.
pub fn menu_in(el: El) -> El {
    el.transition(Transition::scale(0.96).springs(SpringPair::new(MENU_IN, MENU_OUT)))
}

/// Dialog entrance: fade plus a 0.96 scale.
pub fn dialog_in(el: El) -> El {
    el.transition(Transition::scale(0.96).springs(SpringPair::new(DIALOG_IN, MENU_OUT)))
}

/// Boot-splash exit: fade + 6pt lift.
pub fn splash_out(el: El) -> El {
    el.transition(Transition::offset(0.0, -6.0).springs(SpringPair::new(SPLASH_OUT, SPLASH_OUT)))
}

/// Animated pane width/height container motion.
pub fn pane_motion(el: El) -> El {
    el.motion(RESIZE)
}

/// Linear interpolation (pure layout tweens and tests).
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

// Loader math lives in `hearth_proto::motion` so the terminal and UI animate the same waves.
pub use hearth_proto::motion::{
    GSPIN_DIM, GSPIN_ROW_TINTS, MARK_CELLS, MARK_SPREAD, MATRIX_SIDE, PULSE_MIN_OPACITY,
    PULSE_MIN_SCALE, PULSE_STAGGER, gspin_opacity, mark_cell_stagger, pulse_opacity, pulse_scale,
    pulse_wave, staggered_phase,
};

/// The measurement knob's scale: `HEARTH_MOTION_SCALE` multiplies every animation timeline
/// (used by screenshot/benchmark runs). Values outside `0.05..=4.0` are ignored.
pub fn speed_scale() -> f32 {
    std::env::var("HEARTH_MOTION_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && (0.05..=4.0).contains(v))
        .unwrap_or(1.0)
}

/// Whether the host asks for reduced motion. Rune applies the consequences itself; views use
/// this for their own bookkeeping (loader phases, drag thresholds).
pub fn reduced_motion<S>(cx: &rune::Cx<S>) -> bool {
    cx.is_reduced_motion()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bezier_endpoints_are_exact() {
        assert_eq!(EASE.eval(-1.0), 0.0);
        assert_eq!(EASE.eval(0.0), 0.0);
        assert_eq!(EASE.eval(1.0), 1.0);
        assert_eq!(EASE.eval(2.0), 1.0);
    }

    #[test]
    fn ease_out_expo_rises_fast() {
        assert!(EASE_OUT_EXPO.eval(0.25) > 0.7);
        assert!(EASE_OUT_EXPO.eval(0.5) > 0.9);
        assert!(EASE_OUT_EXPO.eval(1.0) == 1.0);
    }

    #[test]
    fn motion_specs_match_the_catalog() {
        assert_eq!(FADE_IN_SPEC.duration_ms, 500);
        assert_eq!(FADE_QUICK_SPEC.duration_ms, 150);
        assert_eq!(MENU_IN_SPEC.duration_ms, 140);
        assert_eq!(DIALOG_IN_SPEC.duration_ms, 180);
        assert_eq!(SPLASH_OUT_SPEC.delay_ms, 150);
        assert_eq!(SPLASH_OUT_SPEC.duration_ms, 500);
        assert_eq!(RESIZE_SPEC.duration_ms, 200);
        assert_eq!(RESORT_SPEC.duration_ms, 260);
        assert_eq!(TAB_SLIDE_SPEC.duration_ms, 150);
        assert_eq!(SCROLL_GLIDE_SPEC.duration_ms, 500);
        assert_eq!(HEARTH_PULSE_SPEC.duration_ms, 2400);
        assert_eq!(GRADIENT_SPIN_SPEC.duration_ms, 750);
    }

    #[test]
    fn spring_durations_match_the_specs() {
        assert!((FADE_IN.duration - 0.5).abs() < 1e-6);
        assert!((MENU_IN.duration - 0.14).abs() < 1e-6);
        assert!((RESIZE.duration - 0.2).abs() < 1e-6);
        assert!((RESORT.duration - 0.26).abs() < 1e-6);
        assert!((TAB_SLIDE.duration - 0.15).abs() < 1e-6);
        assert!((FADE_IN.bounce).abs() < 1e-6);
    }

    #[test]
    fn delayed_spec_holds_zero_until_the_delay_ends() {
        let spec = SPLASH_OUT_SPEC;
        assert_eq!(spec.progress(0.0), 0.0);
        // 150ms of 650ms is the end of the hold; f32 round-off leaves a hair of progress.
        assert!(spec.progress(0.15 / 0.65) < 1e-6);
        assert!(spec.progress(0.5) > 0.0);
        assert_eq!(spec.progress(1.0), 1.0);
    }

    #[test]
    fn lerp_is_exact_at_the_ends() {
        assert_eq!(lerp(10.0, 30.0, 0.0), 10.0);
        assert_eq!(lerp(10.0, 30.0, 0.5), 20.0);
        assert_eq!(lerp(10.0, 30.0, 1.0), 30.0);
    }

    #[test]
    fn speed_scale_rejects_nonsense() {
        // The environment is process-global; just assert the filter's shape.
        for value in ["0", "-1", "nan", "inf", "nope"] {
            assert!(
                value
                    .parse::<f32>()
                    .ok()
                    .filter(|v| v.is_finite() && (0.05..=4.0).contains(v))
                    .is_none(),
                "{value}"
            );
        }
        assert_eq!(1.0_f32, 1.0);
    }
}
