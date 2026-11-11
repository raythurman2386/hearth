//! Streaming fade cadence (ported from the gpui paint veil).
//!
//! Rune's renderer applies the fade itself: `rune_kit::Markdown::reveal(word_fade)`
//! fades newly appended words in, staggered from 20 ms down to 5.5 ms as a
//! backlog builds (`rune::growth::edge::WORD_FADE`). The constants and curve
//! below are the gpui/mugen `FadePainter` math kept for parity tests and for
//! callers that want the same cadence in their own reveal timing.

/// EMA seed for the inter-append gap (mugen `EMA_SEED_MS`).
pub const VEIL_EMA_SEED_MS: f32 = 160.0;
/// Duration clamp (mugen `MIN_FADE_MS` / `MAX_FADE_MS`).
pub const VEIL_MIN_FADE_MS: f32 = 120.0;
pub const VEIL_MAX_FADE_MS: f32 = 400.0;
/// Dissolve exponent (mugen: `alpha = (1 - p) ** 1.6`).
pub const VEIL_CURVE_POW: f32 = 1.6;
/// Gap clamp feeding the EMA (mugen: `min(gap, 1000)`).
pub const VEIL_GAP_CLAMP_MS: f32 = 1000.0;

/// Text alpha for a fade progress `p` (0..1): the veil dissolves as
/// `(1 − p)^1.6`, so the text shows through at `1 − veil`. Pure.
pub fn veil_opacity(p: f32) -> f32 {
    1.0 - (1.0 - p.clamp(0.0, 1.0)).powf(VEIL_CURVE_POW)
}

/// Chunk fade duration for the current inter-append EMA (mugen:
/// `min(MAX, max(MIN, ema * 3))`).
pub fn veil_duration_ms(ema_ms: f32) -> f32 {
    (ema_ms * 3.0).clamp(VEIL_MIN_FADE_MS, VEIL_MAX_FADE_MS)
}

/// Fast-stream boost: 3+ chunks fading concurrently speed up by 30% each
/// (mugen: `1 + 0.3 * max(0, veils.length - 2)`).
pub fn veil_boost(active_chunks: usize) -> f32 {
    1.0 + 0.3 * active_chunks.saturating_sub(2) as f32
}

/// EMA update on a new append gap (mugen: `ema*0.7 + min(gap,1000)*0.3`).
pub fn veil_ema_next(ema_ms: f32, gap_ms: f32) -> f32 {
    ema_ms * 0.7 + gap_ms.min(VEIL_GAP_CLAMP_MS) * 0.3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_constants_match_mugen_fade_painter() {
        assert_eq!(VEIL_EMA_SEED_MS, 160.0);
        assert_eq!(VEIL_MIN_FADE_MS, 120.0);
        assert_eq!(VEIL_MAX_FADE_MS, 400.0);
        assert_eq!(VEIL_CURVE_POW, 1.6);
        assert_eq!(veil_duration_ms(160.0), 400.0);
        assert_eq!(veil_duration_ms(30.0), 120.0);
        assert_eq!(veil_duration_ms(60.0), 180.0);
        assert_eq!(veil_ema_next(160.0, 100.0), 160.0 * 0.7 + 100.0 * 0.3);
        assert_eq!(veil_ema_next(160.0, 5000.0), 160.0 * 0.7 + 1000.0 * 0.3);
        assert_eq!(veil_boost(0), 1.0);
        assert_eq!(veil_boost(2), 1.0);
        assert!((veil_boost(3) - 1.3).abs() < 1e-6);
        assert!((veil_boost(5) - 1.9).abs() < 1e-6);
    }

    #[test]
    fn veil_opacity_curve_endpoints() {
        assert_eq!(veil_opacity(0.0), 0.0);
        assert_eq!(veil_opacity(1.0), 1.0);
        let mid = veil_opacity(0.5);
        assert!(mid > 0.0 && mid < 1.0);
        assert!(mid > 0.5);
        assert!((mid - (1.0 - 0.5f32.powf(1.6))).abs() < 1e-6);
        assert!(veil_opacity(0.2) <= veil_opacity(0.4));
        assert_eq!(veil_opacity(-1.0), 0.0);
        assert_eq!(veil_opacity(2.0), 1.0);
    }
}
