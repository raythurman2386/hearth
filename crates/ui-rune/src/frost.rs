//! Frosted glass floats and window materials.
//!
//! The gpui UI painted a backdrop blur into a scene layer so a popover's quads could not be
//! sorted under the blur. Rune's [`El::backdrop_blur`] already blurs what was drawn behind the
//! element, inside its rounded clip, with the element's fill as the tint — one element, one
//! draw order. These helpers name the two recipes the shell uses.

use rune::{El, SystemMaterial};

use crate::theme::Theme;

/// Backdrop-blur sigma for floating menu/dialog glass. The reference hearth `.glass-surface`
/// runs `blur(44px)`; CSS blur is usually taken as roughly two standard deviations, so this is
/// the sigma Rune's Gaussian path expects.
pub const MENU_BLUR: f32 = 22.0;

/// Backdrop-blur sigma for the composer pill and small floats (the reference blurs 12–16).
pub const PILL_BLUR: f32 = 8.0;

/// Frost `child`: a backdrop blur clipped to `corner_radius`, with the child's own fill as the
/// tint. On opaque places Rune draws the fill with no blur, which is the intended fallback.
pub fn frosted(corner_radius: f32, blur_sigma: f32, child: El) -> El {
    child.backdrop_blur(blur_sigma).clip_rounded(corner_radius)
}

/// Frost a floating card at [`MENU_BLUR`].
pub fn menu_glass(corner_radius: f32, child: El) -> El {
    frosted(corner_radius, MENU_BLUR, child)
}

/// The window's own material: real vibrancy on macOS, a tint elsewhere. Used by the titlebar
/// band and the sidebar when the theme asks for glass.
pub fn window_material(theme: &Theme, band: El) -> El {
    if theme.is_glass() {
        band.system_material(SystemMaterial::Popover)
            .fill(theme.glass())
    } else if theme.is_frost() {
        band.system_material(SystemMaterial::Popover)
            .fill(theme.surface)
    } else {
        band.fill(theme.surface)
    }
}

/// The sidebar / titlebar band treatment: the theme's surface, with a system material on macOS.
/// Keeps the shell's chrome in one place so light and dark agree.
pub fn chrome_band(theme: &Theme, band: El) -> El {
    if theme.is_frost() {
        band.system_material(SystemMaterial::Popover)
            .fill(theme.surface)
    } else {
        band.fill(theme.surface)
    }
}

/// Rune already draws a frosted card's contents in the card's own scene order; this used to
/// start a nested gpui layer. It is now an identity helper so ported call sites keep reading
/// the same.
pub fn layered(child: El) -> El {
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_blur_is_the_css_radius_halved() {
        assert_eq!(MENU_BLUR, 22.0);
        assert_eq!(PILL_BLUR, 8.0);
    }
}
