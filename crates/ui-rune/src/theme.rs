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

use hearth_syntax::HighlightKind;
use rune::color::{Color, oklch, srgb_decode};
use rune_kit::{Kit, PaletteSeed, StateColors};

use crate::omarchy::OmarchyTheme;

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

    /// Map an Omarchy theme's ANSI/status colours onto the syntax roles.
    fn from_omarchy(om: &OmarchyTheme) -> Self {
        Self {
            comment: om.muted,
            keyword: om.red,
            string: om.green,
            string_special: om.magenta,
            escape: om.yellow,
            number: om.yellow,
            boolean: om.magenta,
            type_name: om.cyan,
            type_builtin: om.cyan,
            constructor: om.cyan,
            function: om.blue,
            function_builtin: om.blue,
            macro_name: om.magenta,
            property: om.cyan,
            constant: om.yellow,
            variable: om.blue,
            variable_special: om.magenta,
            parameter: om.foreground,
            operator: om.blue,
            punctuation: om.muted,
            tag: om.magenta,
            attribute: om.yellow,
            label: om.yellow,
            invalid: om.red,
        }
    }
}

/// Built-in Ravenwood ANSI slots for the dark appearance (normal 0–7, bright 8–15).
pub const ANSI16_DARK: [[u8; 3]; 16] = [
    [0x2a, 0x31, 0x2c],
    [0xda, 0x63, 0x62],
    [0x5e, 0x8d, 0x5e],
    [0xbf, 0x98, 0x3d],
    [0x5a, 0x93, 0xa2],
    [0xb8, 0x7b, 0x9d],
    [0x44, 0x7d, 0x6b],
    [0x85, 0x92, 0x89],
    [0x2d, 0x38, 0x32],
    [0xe6, 0x7e, 0x80],
    [0x4a, 0xde, 0x80],
    [0xfb, 0xbf, 0x24],
    [0x22, 0xd3, 0xee],
    [0xf4, 0x72, 0xb6],
    [0x34, 0xd3, 0x99],
    [0xe8, 0xd5, 0xb7],
];

/// Built-in Ravenwood ANSI slots for the light appearance.
pub const ANSI16_LIGHT: [[u8; 3]; 16] = [
    [0x14, 0x18, 0x14],
    [0xc0, 0x3c, 0x39],
    [0x5c, 0x7a, 0x0c],
    [0xb0, 0x85, 0x00],
    [0x1a, 0x6d, 0x9e],
    [0xb8, 0x4d, 0x94],
    [0x1e, 0x7d, 0x5a],
    [0x3d, 0x4c, 0x53],
    [0x2d, 0x38, 0x32],
    [0x9e, 0x2b, 0x29],
    [0x4a, 0x62, 0x10],
    [0x8f, 0x6b, 0x00],
    [0x14, 0x5a, 0x7a],
    [0x8f, 0x3d, 0x74],
    [0x1a, 0x63, 0x47],
    [0x18, 0x1c, 0x1e],
];

fn rgb8(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: srgb_decode(r as f32 / 255.0),
        g: srgb_decode(g as f32 / 255.0),
        b: srgb_decode(b as f32 / 255.0),
        a: 1.0,
    }
}

fn ansi_from(table: [[u8; 3]; 16]) -> [Color; 16] {
    table.map(|[r, g, b]| rgb8(r, g, b))
}

fn state_colors(fg: Color) -> StateColors {
    StateColors {
        bg: fg.alpha(0.12),
        fg,
        border: fg.alpha(0.35),
        icon: fg,
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

    // ---- terminal ----
    /// The 16 ANSI slots (normal 0–7, bright 8–15).
    pub ansi: [Color; 16],
    pub terminal_bg: Color,
    pub terminal_fg: Color,
    pub terminal_cursor: Color,
    pub terminal_selection: Color,
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
            ansi: ansi_from(ANSI16_DARK),
            terminal_bg: hex("141814"),
            terminal_fg: hex("e8d5b7"),
            terminal_cursor: hex("22d3ee"),
            terminal_selection: Color::WHITE.alpha(0.22),
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
            ansi: ansi_from(ANSI16_LIGHT),
            terminal_bg: hex("fdf6e3"),
            terminal_fg: hex("3d4c53"),
            terminal_cursor: hex("1a6d9e"),
            terminal_selection: Color::BLACK.alpha(0.16),
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

    /// Build the theme from the active Omarchy theme.
    pub fn from_omarchy(om: &OmarchyTheme) -> Self {
        let syntax = SyntaxPalette::from_omarchy(om);
        let bg = om.background;
        let raised = om.lighter_background;
        Self {
            appearance: om.appearance,
            bg,
            // Omarchy is flat: the page, sidebar and pane share the background;
            // hairline borders and raised cards carry the separation.
            surface: bg,
            surface_raised: raised,
            surface_card: raised,
            surface_dialog: raised,
            surface_overlay: raised,
            element_hover: om.selection,
            border: om.inactive_border,
            border_strong: om.light_foreground.alpha(0.25),
            text: om.foreground,
            text_muted: om.light_foreground,
            text_faint: om.muted,
            text_dim: om.dark_foreground,
            solid: om.foreground,
            on_solid: bg,
            accent: om.accent,
            accent_strong: om.accent,
            on_accent: bg,
            danger: om.red,
            danger_muted: om.bright_red,
            warning: om.yellow,
            warning_muted: om.bright_yellow,
            success: om.green,
            busy: om.blue,
            success_muted: om.bright_green,
            surface_raised_hover: raised.mix(om.foreground, 0.07),
            band: om.darker_background.alpha(0.6),
            input_bg: raised,
            selection: om.selection,
            cursor: om.terminal_cursor,
            caret: om.terminal_cursor,
            danger_strong: om.red,
            code_text: om.cyan,
            code_wash: om.selection.alpha(0.6),
            syntax,
            diff_add: om.green,
            diff_del: om.red,
            diff_hunk_bg: om.selection.alpha(0.5),
            font_sans: "Geist",
            font_mono: "Geist Mono",
            kit: Kit::neutral(om.appearance.is_dark()),
            ansi: om.ansi,
            terminal_bg: om.terminal_bg,
            terminal_fg: om.terminal_fg,
            terminal_cursor: om.terminal_cursor,
            terminal_selection: om.terminal_selection.alpha(0.6),
        }
        .with_kit()
    }

    /// Derive the rune-kit semantic token set from this palette, so every
    /// component (not just the shell) paints the active theme.
    fn with_kit(mut self) -> Self {
        let mut kit = Kit::from_palette(PaletteSeed {
            bg: self.bg,
            fg: self.text,
            accent: self.accent,
        });
        kit.blend_over = Some(self.bg);
        kit.c.page = self.bg;
        kit.c.bg_base = self.bg;
        kit.c.bg_deep = self.bg;
        kit.c.layer_01 = self.surface_overlay;
        kit.c.layer_02 = self.surface_dialog;
        kit.c.layer_03 = self.surface_raised;
        kit.c.layer_04 = self.surface_raised_hover;
        kit.c.surface_base = self.surface;
        kit.c.surface_weak = self.surface_card;
        kit.c.surface_hover = self.element_hover;
        kit.c.surface_raised = self.surface_raised;
        kit.c.surface_raised_base = self.surface_raised;
        kit.c.surface_raised_hover = self.surface_raised_hover;
        kit.c.surface_float = self.surface_overlay;
        kit.c.surface_disabled = self.surface_card;
        kit.c.text_base = self.text;
        kit.c.text_muted = self.text_muted;
        kit.c.text_faint = self.text_faint;
        kit.c.text_weak = self.text_dim;
        kit.c.text_strong = self.text;
        kit.c.text_accent = self.accent;
        kit.c.icon_base = self.text;
        kit.c.icon_muted = self.text_muted;
        kit.c.icon_faint = self.text_faint;
        kit.c.icon_accent = self.accent;
        kit.c.border_muted = self.border;
        kit.c.border_base = self.border;
        kit.c.border_strong = self.border_strong;
        kit.c.border_weak = self.border;
        kit.c.border_focus = self.accent;
        kit.c.hover = self.element_hover;
        kit.c.pressed = self.wash(0.10);
        kit.c.scrim = self.scrim();
        kit.c.submit_fg = self.on_solid;
        kit.c.submit_icon = self.on_solid;
        kit.c.submit_highlight = Color::WHITE.alpha(0.18);
        kit.c.submit_hover = self.bg.alpha(0.12);
        kit.c.bg_accent = self.accent;
        kit.c.bg_button_neutral = self.surface_raised;
        kit.c.bg_icon_button_contrast = self.solid;
        kit.c.bg_inverse = self.solid;
        kit.c.bg_contrast = self.solid;
        kit.c.text_inverse = self.on_solid;
        kit.c.text_contrast = self.on_solid;
        kit.c.icon_inverse = self.on_solid;
        kit.c.icon_contrast = self.on_solid;
        kit.c.knob = self.surface_raised;
        kit.c.knob_on = self.accent;
        kit.c.white = Color::WHITE;
        kit.c.black = Color::BLACK;
        kit.c.depth_top = Color::WHITE.alpha(0.10);
        kit.c.depth_bottom = Color::BLACK.alpha(0.12);
        kit.c.std_border = self.border;
        kit.c.std_icon = self.text;
        kit.c.std_icon_invert = self.on_solid;
        kit.c.std_icon_strong = self.text;
        kit.c.std_text = self.text;
        kit.c.success = state_colors(self.success);
        kit.c.warning = state_colors(self.warning);
        kit.c.danger = state_colors(self.danger);
        kit.c.info = state_colors(self.busy);
        self.kit = kit;
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
    fn text_contrast_is_legible_in_both_appearances() {
        let dark = Theme::dark();
        let light = Theme::light();
        let dark_ratio = contrast_ratio(dark.text, dark.bg);
        let light_ratio = contrast_ratio(light.text, light.bg);
        assert!(
            dark_ratio >= 7.0 && light_ratio >= 7.0,
            "dark {dark_ratio}, light {light_ratio}"
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
