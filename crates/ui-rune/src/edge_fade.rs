//! Edge fades.
//!
//! The gpui UI wrapped a subtree in an `EdgeFade` scope so content faded per primitive at the
//! panel's edges. Rune has the same effect built in:
//! - a scroll viewport fades its own overflow edges with [`El::scroll_edge_fade`];
//! - any element can fade a subtree with [`El::clip_fade`] (a soft mask, four widths).
//!
//! The helpers stay thin so the shell can name the intent rather than the mechanism.

use rune::El;

/// Fade a scroll viewport's top and bottom only while content is hidden under them.
/// `band` is the ramp height in points (the gpui `SIDEBAR_GLASS_FADE_BAND`, 32).
pub fn scroll_edges(viewport: El, band: f32) -> El {
    viewport.scroll_edge_fade(band)
}

/// Wrap `child` in a soft vertical mask with `band` points of ramp at the top and bottom.
/// The caller owns the bounds (a relative column with a bounded height).
pub fn fade_vertical(child: El, band: f32) -> El {
    child.clip_fade(0.0, band, 0.0, band)
}

/// Fade every edge: the splash hero's dissolve into the frost.
pub fn fade_all(child: El, band: f32) -> El {
    child.clip_fade(band, band, band, band)
}

/// The old `EdgeFaded` builder, kept for call sites that toggled sides. A disabled side is 0.
#[derive(Clone)]
pub struct EdgeFaded {
    pub band: f32,
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
    pub child: El,
}

/// Start an edge-fade scope. Chain [`EdgeFaded::fade_left`] / [`EdgeFaded::fade_right`] and
/// finish with [`EdgeFaded::build`].
pub fn edge_faded(band: f32, top: bool, bottom: bool, child: El) -> EdgeFaded {
    EdgeFaded {
        band,
        top,
        bottom,
        left: false,
        right: false,
        child,
    }
}

impl EdgeFaded {
    pub fn fade_left(mut self, on: bool) -> Self {
        self.left = on;
        self
    }

    pub fn fade_right(mut self, on: bool) -> Self {
        self.right = on;
        self
    }

    pub fn build(self) -> El {
        let b = self.band;
        self.child.clip_fade(
            if self.left { b } else { 0.0 },
            if self.top { b } else { 0.0 },
            if self.right { b } else { 0.0 },
            if self.bottom { b } else { 0.0 },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_edge_has_no_ramp() {
        let faded = edge_faded(32.0, true, false, rune::el());
        assert_eq!(faded.band, 32.0);
        assert!(faded.top);
        assert!(!faded.bottom);
        assert!(!faded.left);
        assert!(!faded.right);
    }
}
