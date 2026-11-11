//! Theme token gates: contrast floors, the elevation ladder, full syntax
//! coverage and the layout numbers the gpui UI pinned.
//!
//! These are integration tests on purpose: `theme.rs`'s own unit tests only
//! guard its colour math, while these assert the *design* claims the shell and
//! components rely on (legible text, ordered planes, every `HighlightKind`
//! painted, constants unchanged).

use hearth_syntax::HighlightKind;
use hearth_ui_rune::theme::{self, Appearance, Theme, contrast_ratio, flatten, relative_luminance};

fn both() -> [Theme; 2] {
    [Theme::dark(), Theme::light()]
}

fn label(theme: &Theme) -> &'static str {
    match theme.appearance {
        Appearance::Dark => "dark",
        Appearance::Light => "light",
    }
}

// ---------------------------------------------------------------------------
// Contrast
// ---------------------------------------------------------------------------

#[test]
fn body_and_secondary_text_clear_their_floors_on_every_plane() {
    for t in both() {
        let text_bg = contrast_ratio(t.text, t.bg);
        let text_surface = contrast_ratio(t.text, t.surface);
        assert!(
            text_bg >= 7.0,
            "{} text on bg is {text_bg:.2}:1, below 7:1",
            label(&t)
        );
        assert!(
            text_surface >= 7.0,
            "{} text on the shell surface is {text_surface:.2}:1",
            label(&t)
        );

        for (name, fg) in [("text_muted", t.text_muted), ("text_dim", t.text_dim)] {
            for (plane, bg) in [("bg", t.bg), ("surface", t.surface)] {
                let ratio = contrast_ratio(fg, bg);
                assert!(
                    ratio >= 4.5,
                    "{} {name} on {plane} is {ratio:.2}:1, below AA",
                    label(&t)
                );
            }
        }

        // Placeholder/disabled copy is the one deliberately lower tone; it must
        // still read against the content plane.
        let faint = contrast_ratio(t.text_faint, t.bg);
        assert!(
            faint >= 4.5,
            "{} text_faint on bg is {faint:.2}:1",
            label(&t)
        );
    }
}

#[test]
fn solid_and_accent_plates_carry_their_labels() {
    for t in both() {
        let solid = contrast_ratio(t.on_solid, t.solid);
        assert!(
            solid >= 7.0,
            "{} primary button label is {solid:.2}:1",
            label(&t)
        );
        let accent = contrast_ratio(t.on_accent, t.accent_strong);
        assert!(
            accent >= 4.0,
            "{} accent button label is {accent:.2}:1",
            label(&t)
        );
    }
}

#[test]
fn contrast_ratio_matches_known_anchors() {
    let white = theme::grey(0xff);
    let black = theme::grey(0x00);
    assert!((contrast_ratio(white, black) - 21.0).abs() < 0.01);
    assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
    assert!((relative_luminance(white) - 1.0).abs() < 0.001);
    assert!(relative_luminance(black).abs() < 0.001);
}

// ---------------------------------------------------------------------------
// Elevation
// ---------------------------------------------------------------------------

#[test]
fn dark_planes_climb_a_strict_ladder() {
    let d = Theme::dark();
    let ladder = [
        ("bg", d.bg),
        ("surface_card", d.surface_card),
        ("surface_dialog", d.surface_dialog),
        ("surface_overlay", d.surface_overlay),
        ("surface_raised", d.surface_raised),
    ];
    for pair in ladder.windows(2) {
        let ((lower, lo), (upper, hi)) = (pair[0], pair[1]);
        assert!(
            relative_luminance(lo) < relative_luminance(hi),
            "dark {upper} must sit above {lower}: {:.4} vs {:.4}",
            relative_luminance(hi),
            relative_luminance(lo)
        );
    }
    assert!(
        relative_luminance(d.bg) < relative_luminance(d.surface),
        "dark chrome sits above the content plane"
    );
    assert!(
        relative_luminance(d.surface) < relative_luminance(d.surface_raised),
        "dark raised plates are lighter"
    );
}

#[test]
fn light_planes_recede_then_float_at_cream_level() {
    let l = Theme::light();
    // Light is designed, not inverted: chrome is darker than the content plane
    // and floating surfaces stay at cream level, separated by borders.
    assert!(
        relative_luminance(l.surface) < relative_luminance(l.bg),
        "light sidebar recedes below the panel"
    );
    assert!(
        relative_luminance(l.surface_raised) < relative_luminance(l.bg),
        "light raised plates are darker than the paper"
    );
    assert!(
        (relative_luminance(l.bg) - relative_luminance(l.surface_raised)).abs() > 0.03,
        "a borderless raised plate needs a visible lightness step"
    );
    for (name, plane) in [
        ("surface_card", l.surface_card),
        ("surface_dialog", l.surface_dialog),
        ("surface_overlay", l.surface_overlay),
    ] {
        assert!(
            relative_luminance(plane) >= relative_luminance(l.bg) - 1e-4,
            "light {name} dropped below the panel"
        );
    }
    // With no strong lightness step, the stronger border is the separator.
    let border = contrast_ratio(flatten(l.border_strong, l.bg), l.bg);
    assert!(
        border > 1.15,
        "light border_strong only registers {border:.3}"
    );
}

#[test]
fn raised_plate_hover_goes_somewhere_visible() {
    for t in both() {
        let step = (relative_luminance(t.surface_raised_hover)
            - relative_luminance(t.surface_raised))
        .abs();
        assert!(
            step > 0.02,
            "{} raised hover moves only {step:.3}",
            label(&t)
        );
    }
}

// ---------------------------------------------------------------------------
// Syntax palette
// ---------------------------------------------------------------------------

const ALL_KINDS: [HighlightKind; 25] = [
    HighlightKind::Comment,
    HighlightKind::Keyword,
    HighlightKind::String,
    HighlightKind::StringSpecial,
    HighlightKind::Escape,
    HighlightKind::Number,
    HighlightKind::Boolean,
    HighlightKind::Type,
    HighlightKind::TypeBuiltin,
    HighlightKind::Constructor,
    HighlightKind::Function,
    HighlightKind::FunctionBuiltin,
    HighlightKind::Macro,
    HighlightKind::Property,
    HighlightKind::Constant,
    HighlightKind::Variable,
    HighlightKind::VariableSpecial,
    HighlightKind::Parameter,
    HighlightKind::Operator,
    HighlightKind::Punctuation,
    HighlightKind::Embedded,
    HighlightKind::Tag,
    HighlightKind::Attribute,
    HighlightKind::Label,
    HighlightKind::Invalid,
];

#[test]
fn every_highlight_kind_resolves_and_reads_on_its_backgrounds() {
    for t in both() {
        let add_bg = flatten(t.diff_add.alpha(0.055), t.bg);
        let del_bg = flatten(t.diff_del.alpha(0.055), t.bg);
        for kind in ALL_KINDS {
            let color = t.syntax.color(kind);
            assert!(
                color.a > 0.99,
                "{} {kind:?} resolved translucent ({})",
                label(&t),
                color.a
            );
            let on_code = contrast_ratio(color, t.bg);
            assert!(
                on_code >= 4.5,
                "{} {kind:?} is {on_code:.2}:1 on the code plane",
                label(&t)
            );
            // Diff tints mark rows; the 3:1 non-text floor is the right bar for
            // tones painted over a wash.
            for (name, background) in [("add", add_bg), ("del", del_bg)] {
                let ratio = contrast_ratio(color, background);
                assert!(
                    ratio >= 3.0,
                    "{} {kind:?} is {ratio:.2}:1 on the {name} tint",
                    label(&t)
                );
            }
        }
    }
}

#[test]
fn the_palette_keeps_its_family_relationships() {
    for t in both() {
        let p = &t.syntax;
        assert_eq!(p.color(HighlightKind::Punctuation), p.comment);
        assert_eq!(p.color(HighlightKind::Embedded), p.comment);
        assert_eq!(p.color(HighlightKind::Parameter), t.text);
        assert_eq!(
            p.color(HighlightKind::String),
            p.color(HighlightKind::Number)
        );
        assert_eq!(
            p.color(HighlightKind::Type),
            p.color(HighlightKind::Constructor)
        );
        assert_eq!(
            p.color(HighlightKind::Function),
            p.color(HighlightKind::FunctionBuiltin)
        );
        // Keyword and invalid share the danger tone; both are error/emphasis.
        assert_eq!(p.color(HighlightKind::Keyword), t.danger);
        assert_eq!(p.color(HighlightKind::Invalid), t.danger);
    }
}

#[test]
fn code_and_diff_tones_are_readable() {
    for t in both() {
        for (name, fg) in [
            ("code_text", t.code_text),
            ("diff_add", t.diff_add),
            ("diff_del", t.diff_del),
        ] {
            let floor = if name == "code_text" { 4.5 } else { 3.0 };
            let ratio = contrast_ratio(fg, t.bg);
            assert!(ratio >= floor, "{} {name} is {ratio:.2}:1 on bg", label(&t));
        }
        let caret = contrast_ratio(t.caret, t.bg);
        assert!(caret >= 3.0, "{} caret is {caret:.2}:1", label(&t));
    }
}

// ---------------------------------------------------------------------------
// Layout numbers and the kit
// ---------------------------------------------------------------------------

#[test]
fn layout_constants_match_the_gpui_port() {
    assert_eq!(Theme::TITLEBAR_HEIGHT, 38.0);
    assert_eq!(Theme::TITLEBAR_TOP_PAD, 2.0);
    assert_eq!(Theme::STATUS_STRIP_HEIGHT, 24.0);
    assert_eq!(Theme::HEADER_HEIGHT, 44.0);
    assert_eq!(Theme::TRANSCRIPT_FADE_BAND, 24.0);
    assert_eq!(Theme::BUBBLE_RADIUS, 16.0);
    assert_eq!(Theme::CONTROL_RADIUS, 6.0);
    assert_eq!(Theme::SPACE_SM, 8.0);
    assert_eq!(Theme::SPACE_MD, 12.0);
    assert_eq!(Theme::SPACE_LG, 16.0);
    assert_eq!(Theme::TEXT_STACK_GAP, 1.0);
}

#[test]
fn the_kit_page_is_the_panel_background() {
    for t in both() {
        assert_eq!(t.kit.c.page, t.bg, "{} kit page", label(&t));
        assert_eq!(t.kit.c.bg_base, t.bg, "{} kit base", label(&t));
    }
}

#[test]
fn appearance_selection_builds_the_matching_theme() {
    assert!(Theme::for_appearance(Appearance::Dark).appearance.is_dark());
    assert!(
        !Theme::for_appearance(Appearance::Light)
            .appearance
            .is_dark()
    );
    assert_eq!(Appearance::Dark.inverted(), Appearance::Light);
    assert_eq!(Appearance::Light.inverted(), Appearance::Dark);
}

#[test]
fn glass_alpha_follows_the_platform() {
    let expected = if cfg!(target_os = "macos") { 0.80 } else { 1.0 };
    assert_eq!(Theme::GLASS_ALPHA, expected);
    assert_eq!(Theme::GLASS_ALPHA_LIGHT, expected);
    for t in both() {
        assert_eq!(
            t.is_glass(),
            cfg!(target_os = "macos"),
            "{} glass is a macOS-only material",
            label(&t)
        );
        assert_eq!(t.is_frost(), cfg!(target_os = "macos"));
        if cfg!(target_os = "macos") {
            assert!(t.glass().a < 1.0, "frost must be translucent");
        }
    }
    // The two glass tints are appearance-specific, not one inverted colour.
    assert_ne!(Theme::dark().glass(), Theme::light().glass());
}
