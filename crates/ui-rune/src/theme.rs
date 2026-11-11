//! The app theme — two concrete appearances, one token set, plus the
//! [`rune_kit::Kit`] the component library reads.
//!
//! Colors are Rune [`Color`] (linear sRGB, straight alpha). The palette is the
//! same Ravenwood set the gpui UI paints: an oklch-derived neutral scale, warm
//! beige ink on an olive-tinged forest floor in dark, cream paper in light.
//! **Numbers drive layout, colors are paint**: layout constants live here as
//! plain numbers and never depend on which color is painted.
//!
//! Light is designed, not inverted: surface order, elevation, fill alphas and
//! accent steps are all their own tokens (see the module docs in the gpui
//! `theme.rs` this was ported from).

use rune::color::{oklch, srgb_decode, Color};
use rune_kit::{Kit, PaletteSeed};
use hearth_syntax::HighlightKind;

/// Which appearance the app is painting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl Appearance {
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    /// The opposite appearance.
    pub fn inverted(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

/// Shared paint-only syntax palette.
#[derive(Debug, Clone)]
pub struct SyntaxPalette {
    pub comment: Color,
    pub keyword: Color,
    pub string: Color,
    pub string_special: Color,
    pub escape: Color,
    pub number: Color,
    pub boolean: Color,
    pub type_name: Color,
    pub type_builtin: Color,
    pub constructor: Color,
    pub function: Color,
    pub function_builtin: Color,
    pub macro_name: Color,
    pub property: Color,
    pub constant: Color,
    pub variable: Color,
    pub variable_special: Color,
    pub parameter: Color,
    pub operator: Color,
    pub punctuation: Color,
    pub tag: Color,
    pub attribute: Color,
    pub label: Color,
    pub invalid: Color,
}

impl SyntaxPalette {
    pub fn color(&self, kind: HighlightKind) -> Color {
        match kind {
            HighlightKind::Comment => self.comment,
            HighlightKind::Keyword => self.keyword,
            HighlightKind::String => self.string,
            HighlightKind::StringSpecial => self.string_special,
            HighlightKind::Escape => self.escape,
            HighlightKind::Number => self.number,
            HighlightKind::Boolean => self.boolean,
            HighlightKind::Type => self.type_name,
            HighlightKind::TypeBuiltin => self.type_builtin,
            HighlightKind::Constructor => self.constructor,
            HighlightKind::Function => self.function,
            HighlightKind::FunctionBuiltin => self.function_builtin,
            HighlightKind::Macro => self.macro_name,
            HighlightKind::Property => self.property,
            HighlightKind::Constant => self.constant,
            HighlightKind::Variable => self.variable,
            HighlightKind::VariableSpecial => self.variable_special,
            HighlightKind::Parameter => self.parameter,
            HighlightKind::Operator => self.operator,
            HighlightKind::Punctuation | HighlightKind::Embedded => self.punctuation,
            HighlightKind::Tag => self.tag,
            HighlightKind::Attribute => self.attribute,
            HighlightKind::Label => self.label,
            HighlightKind::Invalid => self.invalid,
        }
    }

    fn dark(text: Color, comment: Color, danger: Color) -> Self {
        // Ravenwood dark accents. Green is the hero; String/Constant/Number are
        // yellow, Function orange, Operator blue, Special/Macro purple, Type aqua,
        // comment grey. `text` stays the warm beige foreground.
        let yellow = hex("fbbf24");
        let orange = hex("e69875");
        let aqua = hex("34d399");
        let blue = hex("22d3ee");
        let purple = hex("f472b6");
        Self {
            comment,
            keyword: danger,
            string: yellow,
            string_special: purple,
            escape: orange,
            number: yellow,
            boolean: purple,
            type_name: aqua,
            type_builtin: aqua,
            constructor: aqua,
            function: orange,
            function_builtin: orange,
            macro_name: purple,
            property: blue,
            constant: yellow,
            variable: blue,
            variable_special: purple,
            parameter: text,
            operator: blue,
            punctuation: comment,
            tag: purple,
            attribute: yellow,
            label: yellow,
            invalid: danger,
        }
    }

    fn light(text: Color, comment: Color, danger: Color) -> Self {
        // Ravenwood light syntax: the same hue families as dark, moved down the
        // tonal scale (the 400/300 steps fail on cream).
        let yellow = hex("7a5a00");
        let orange = hex("8f4c10");
        let aqua = hex("16624a");
        let blue = hex("1a6d9e");
        let purple = hex("8f3d74");
        Self {
            comment,
            keyword: danger,
            string: yellow,
            string_special: purple,
            escape: orange,
            number: yellow,
            boolean: yellow,
            type_name: aqua,
            type_builtin: aqua,
            constructor: aqua,
            function: orange,
            function_builtin: orange,
            macro_name: purple,
            property: blue,
            constant: yellow,
            variable: blue,
            variable_special: purple,
            parameter: text,
            operator: blue,
            punctuation: comment,
            tag: purple,
            attribute: yellow,
            label: yellow,
            invalid: danger,
        }
    }
}

/// The app theme. Two concrete instances — [`Theme::dark`] and [`Theme::light`].
#[derive(Debug, Clone)]
pub struct Theme {
    /// Which appearance these tokens were built for.
    pub appearance: Appearance,

    // ---- paint: neutral surfaces ----
    /// Main content panel.
    pub bg: Color,
    /// Shell / sidebar surface.
    pub surface: Color,
    /// Raised surface: opaque pills and chips.
    pub surface_raised: Color,

    // ---- paint: elevation ladder ----
    /// Inline card resting on the main panel.
    pub surface_card: Color,
    /// Modal dialog, floating over a scrim.
    pub surface_dialog: Color,
    /// Popover, menu and command-palette surface — the highest plane.
    pub surface_overlay: Color,
    /// Hover wash for interactive rows/buttons.
    pub element_hover: Color,
    /// Hairline border.
    pub border: Color,
    /// Stronger border for focused/raised edges.
    pub border_strong: Color,

    // ---- paint: text ----
    pub text: Color,
    pub text_muted: Color,
    pub text_faint: Color,
    pub text_dim: Color,

    // ---- paint: high-contrast solid (primary buttons) ----
    pub solid: Color,
    pub on_solid: Color,

    // ---- paint: accents ----
    pub accent: Color,
    pub accent_strong: Color,
    pub on_accent: Color,
    pub danger: Color,
    pub danger_muted: Color,
    pub warning: Color,
    pub warning_muted: Color,
    pub success: Color,
    pub busy: Color,
    pub success_muted: Color,

    // ---- paint: components ----
    pub surface_raised_hover: Color,
    pub band: Color,
    pub input_bg: Color,
    pub selection: Color,
    pub cursor: Color,
    pub caret: Color,
    pub danger_strong: Color,

    // ---- paint: code & diff ----
    pub code_text: Color,
    pub code_wash: Color,
    pub syntax: SyntaxPalette,
    pub diff_add: Color,
    pub diff_del: Color,
    pub diff_hunk_bg: Color,

    // ---- fonts ----
    pub font_sans: &'static str,
    pub font_mono: &'static str,

    /// The rune-kit semantic token set derived from this palette. Built once per
    /// appearance and rebuilt only when the appearance changes.
    pub kit: Kit,
}

impl Theme {
    // ---- numbers drive layout (px) ----
    /// Frost translucency over the blurred window background (macOS vibrancy).
    pub const GLASS_ALPHA: f32 = if cfg!(target_os = "macos") { 0.80 } else { 1.0 };
    /// Light-mode frost alpha.
    pub const GLASS_ALPHA_LIGHT: f32 = if cfg!(target_os = "macos") { 0.80 } else { 1.0 };
    /// Main-panel header height (hearth `h-11`).
    pub const HEADER_HEIGHT: f32 = 44.0;
    /// The unified window titlebar (traffic lights + cluster + tabs).
    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    /// Downward shift of titlebar content within the bar.
    pub const TITLEBAR_TOP_PAD: f32 = 2.0;
    /// Reserved status strip under the content outlet.
    pub const STATUS_STRIP_HEIGHT: f32 = 24.0;
    /// Height of the gradient that fades the transcript into the panel.
    pub const TRANSCRIPT_FADE_BAND: f32 = 24.0;
    /// Message bubble corner radius.
    pub const BUBBLE_RADIUS: f32 = 16.0;
    /// Small control radius (buttons, chips).
    pub const CONTROL_RADIUS: f32 = 6.0;
    /// Base spacing steps.
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;
    /// Optical separation for a tightly coupled title/description stack.
    pub const TEXT_STACK_GAP: f32 = 1.0;

    /// The frost tint painted over the blurred window background (macOS glass).
    pub fn glass(&self) -> Color {
        match self.appearance {
            Appearance::Dark => hsla(0.0, 0.0, 0.0, 0.76),
            Appearance::Light => hsla(0.0, 0.0, 1.0, 0.70),
        }
    }

    /// Whether the shell paints a translucent frost surface (macOS only).
    pub fn is_glass(&self) -> bool {
        cfg!(target_os = "macos") && self.glass().a < 1.0
    }

    /// Whether the shell paints the frosted sidebar treatment.
    pub fn is_frost(&self) -> bool {
        cfg!(target_os = "macos")
    }

    /// Hover tone over the glass surface.
    pub fn glass_hover(&self) -> Color {
        wash_for(self.appearance, 0.08)
    }

    /// Floating overlay plane over glass.
    pub fn glass_overlay(&self) -> Color {
        match self.appearance {
            Appearance::Dark => hsla(0.0, 0.0, 1.0, 0.06),
            Appearance::Light => hsla(0.0, 0.0, 1.0, 0.10),
        }
    }

    /// The composer pill and other input plates.
    pub fn input_glass_bg(&self) -> Color {
        match self.appearance {
            Appearance::Dark => hsla(0.0, 0.0, 1.0, 0.05),
            Appearance::Light => Color::WHITE,
        }
    }

    /// Inline card surface on the panel.
    pub fn card_glass_bg(&self) -> Color {
        self.surface_card
    }

    /// Modal backdrop.
    pub fn scrim(&self) -> Color {
        scrim_for(self.appearance, SCRIM_ALPHA_DARK)
    }

    pub fn dark() -> Self {
        let syntax = SyntaxPalette::dark(hex("e8d5b7"), hex("7f897d"), hex("e67e80"));
        Self {
            appearance: Appearance::Dark,
            bg: hex("141814"),
            surface: hex("1f241f"),
            surface_raised: hex("3d4a40"),
            surface_card: hex("1f241f"),
            surface_dialog: hex("222822"),
            surface_overlay: hex("2d3830"),
            element_hover: hsla(0.13, 0.25, 0.55, 0.30),
            border: hsla(0.14, 0.15, 0.70, 0.12),
            border_strong: hsla(0.14, 0.15, 0.70, 0.18),
            text: hex("e8d5b7"),
            text_muted: hex("859289"),
            text_faint: hex("7f897d"),
            text_dim: hex("8a9489"),
            solid: hex("e8d5b7"),
            on_solid: hex("141814"),
            accent: hex("4ade80"),
            accent_strong: hex("2ebd6b"),
            on_accent: hex("0f1410"),
            danger: hex("e67e80"),
            danger_muted: hex("da6362"),
            warning: hex("e69875"),
            warning_muted: hex("d77f48"),
            success: hex("4ade80"),
            busy: hex("f472b6"),
            success_muted: hex("5e8d5e"),
            surface_raised_hover: hex("4a5a4d"),
            band: band_for(Appearance::Dark),
            input_bg: hsla(0.10, 0.20, 0.85, 0.06),
            selection: hsla(140.0 / 360.0, 0.50, 0.55, 0.32),
            cursor: hsla(45.0 / 360.0, 0.30, 0.85, 0.40),
            caret: hex("22d3ee"),
            danger_strong: hex("c0392b"),
            code_text: hex("f472b6"),
            code_wash: hsla(320.0 / 360.0, 0.70, 0.62, 0.14),
            syntax,
            diff_add: hex("4ade80"),
            diff_del: hex("e67e80"),
            diff_hunk_bg: hsla(345.0 / 360.0, 0.08, 0.50, 0.10),
            font_sans: "Geist",
            font_mono: "Geist Mono",
            kit: Kit::neutral(true),
        }
        .with_kit()
    }

    pub fn light() -> Self {
        let syntax = SyntaxPalette::light(hex("3d4c53"), hex("5c6656"), hex("c03c39"));
        Self {
            appearance: Appearance::Light,
            bg: hex("fdf6e3"),
            surface: hex("efebd4"),
            surface_raised: hex("efebd4"),
            surface_card: hex("fdf6e3"),
            surface_dialog: hex("fdf6e3"),
            surface_overlay: hex("fdf6e3"),
            element_hover: hsla(0.0, 0.0, 0.10, 0.05),
            border: hsla(0.0, 0.0, 0.0, 0.10),
            border_strong: hsla(0.0, 0.0, 0.0, 0.17),
            text: hex("3d4c53"),
            text_muted: hex("5c6658"),
            text_faint: hex("5c6658"),
            text_dim: hex("5c6658"),
            solid: hex("3d4c53"),
            on_solid: hex("fdf6e3"),
            accent: hex("5c7a0c"),
            accent_strong: hex("5c7a0c"),
            on_accent: hex("fdf6e3"),
            danger: hex("c03c39"),
            danger_muted: hex("9e2b29"),
            warning: hex("c55e15"),
            warning_muted: hex("9e5410"),
            success: hex("5c7a0c"),
            busy: hex("b84d94"),
            success_muted: hex("4a6210"),
            surface_raised_hover: hex("e6e2cc"),
            band: band_for(Appearance::Light),
            input_bg: hex("fdf6e3"),
            selection: hsla(140.0 / 360.0, 0.40, 0.40, 0.25),
            cursor: hsla(0.0, 0.0, 0.0, 0.50),
            caret: hex("1a6d9e"),
            danger_strong: hex("a53930"),
            code_text: hex("8f3d74"),
            code_wash: hsla(320.0 / 360.0, 0.40, 0.45, 0.10),
            syntax,
            diff_add: hex("5c7a0c"),
            diff_del: hex("c03c39"),
            diff_hunk_bg: hsla(345.0 / 360.0, 0.08, 0.50, 0.07),
            font_sans: "Geist",
            font_mono: "Geist Mono",
            kit: Kit::neutral(false),
        }
        .with_kit()
    }

    /// Build the theme for an appearance.
    pub fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

    /// Derive the rune-kit token set from this palette.
    fn with_kit(mut self) -> Self {
        self.kit = Kit::from_palette(PaletteSeed {
            bg: self.bg,
            fg: self.text,
            accent: self.accent,
        });
        self
    }

    /// Translucent fill ink at `alpha`.
    pub fn ink(&self, alpha: f32) -> Color {
        ink_for(self.appearance, alpha)
    }

    /// Translucent hairline ink at `alpha`.
    pub fn hairline(&self, alpha: f32) -> Color {
        hairline_for(self.appearance, alpha)
    }

    /// Interactive wash at `alpha`.
    pub fn wash(&self, alpha: f32) -> Color {
        wash_for(self.appearance, alpha)
    }
}

/// Light-mode alpha multiplier for **fills** (hover/active washes, chip and pill
/// backgrounds).
pub const INK_FILL_SCALE: f32 = 1.0;
/// Light-mode alpha multiplier for **hairlines** (borders, dividers, rings).
pub const INK_HAIRLINE_SCALE: f32 = 1.35;

/// Translucent **fill** ink for interactive states and chip plates.
pub fn ink_for(appearance: Appearance, alpha: f32) -> Color {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 1.0, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.0, alpha * INK_FILL_SCALE),
    }
}

/// Translucent **hairline** ink for borders, dividers and rings.
pub fn hairline_for(appearance: Appearance, alpha: f32) -> Color {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 1.0, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.0, (alpha * INK_HAIRLINE_SCALE).min(0.5)),
    }
}

/// Interactive-state wash: a softened [`ink_for`] that stops short of pure black
/// or white.
pub fn wash_for(appearance: Appearance, alpha: f32) -> Color {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.92, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.10, alpha * INK_FILL_SCALE),
    }
}

/// Alpha of the standard modal backdrop in dark mode.
pub const SCRIM_ALPHA_DARK: f32 = 0.60;

/// Modal backdrop at `alpha_dark` (quoted in dark-mode terms).
pub fn scrim_for(appearance: Appearance, alpha_dark: f32) -> Color {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.0, alpha_dark),
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.32 * (alpha_dark / SCRIM_ALPHA_DARK)),
    }
}

/// Recessed band behind a palette/picker header or footer strip.
pub fn band_for(appearance: Appearance) -> Color {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.0, 0.16),
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.045),
    }
}

/// Selected-state glass treatment (tabs, session rows, space rows).
pub fn glass_selected_bg(appearance: Appearance) -> Color {
    match appearance {
        Appearance::Dark => wash_for(appearance, 0.11),
        Appearance::Light => wash_for(appearance, 0.06),
    }
}

/// The user message bubble's plate.
pub fn user_bubble_bg(appearance: Appearance) -> Color {
    match appearance {
        Appearance::Dark => wash_for(appearance, 0.08),
        Appearance::Light => wash_for(appearance, 0.04),
    }
}

/// Selected/keyboard-active treatment for rows and chips INSIDE a floating card.
pub fn card_selected_bg(appearance: Appearance) -> Color {
    match appearance {
        Appearance::Dark => wash_for(appearance, 0.11),
        Appearance::Light => wash_for(appearance, 0.06),
    }
}

/// The selected chip's bright outline colour.
pub fn card_selected_ring(appearance: Appearance) -> Color {
    match appearance {
        Appearance::Dark => hairline_for(appearance, 0.09),
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.07),
    }
}

/// An exact achromatic tone from an 8-bit channel value.
pub fn grey(value: u8) -> Color {
    hsla(0.0, 0.0, value as f32 / 255.0, 1.0)
}

/// An opaque color from a `#RRGGBB` literal (the leading `#` is optional).
pub fn hex(code: &str) -> Color {
    if code.starts_with('#') {
        rune::color::hex(code).expect("theme hex literal")
    } else {
        rune::color::hex(&format!("#{code}")).expect("theme hex literal")
    }
}

/// An opaque color from a `#RRGGBB` literal (test alias).
pub fn hex_color(code: &str) -> Color {
    hex(code)
}

/// HSL (all components 0..1) → Rune [`Color`].
pub fn hsla(h: f32, s: f32, l: f32, a: f32) -> Color {
    let [r, g, b] = hsl_to_rgb(h, s, l);
    Color {
        r: srgb_decode(r),
        g: srgb_decode(g),
        b: srgb_decode(b),
        a,
    }
}

/// Convert an oklch color (CSS notation: L 0..1, C, H in degrees) to [`Color`].
pub fn oklch_color(l: f32, c: f32, h_deg: f32) -> Color {
    oklch(l, c, h_deg)
}

/// HSL → sRGB components 0..1.
pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    if s <= f32::EPSILON {
        return [l, l, l];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0)]
}

/// WCAG 2.1 relative luminance of an opaque color.
pub fn relative_luminance(color: Color) -> f32 {
    color.luminance()
}

/// WCAG 2.1 contrast ratio between two opaque colors (1.0 … 21.0).
pub fn contrast_ratio(a: Color, b: Color) -> f32 {
    let (la, lb) = (a.luminance(), b.luminance());
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Composite `fg` (which may be translucent) over an opaque `bg`, returning the
/// opaque result — the color the eye actually receives.
pub fn flatten(fg: Color, bg: Color) -> Color {
    let a = fg.a.clamp(0.0, 1.0);
    Color {
        r: fg.r * a + bg.r * (1.0 - a),
        g: fg.g * a + bg.g * (1.0 - a),
        b: fg.b * a + bg.b * (1.0 - a),
        a: 1.0,
    }
}

/// Linear per-component mix of two colors.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    a.mix(b, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_literals_resolve() {
        assert_eq!(hex_color("000000"), Color::BLACK);
        assert_eq!(hex_color("ffffff"), Color::WHITE);
    }

    #[test]
    fn text_contrast_is_paired_across_appearances() {
        let dark = Theme::dark();
        let light = Theme::light();
        let dark_ratio = contrast_ratio(dark.text, dark.bg);
        let light_ratio = contrast_ratio(light.text, light.bg);
        assert!(
            (dark_ratio - light_ratio).abs() < 0.5,
            "dark {dark_ratio} vs light {light_ratio}"
        );
    }

    #[test]
    fn kit_is_built_for_both_appearances() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_eq!(dark.kit.c.page, dark.bg);
        assert_eq!(light.kit.c.page, light.bg);
    }
}
