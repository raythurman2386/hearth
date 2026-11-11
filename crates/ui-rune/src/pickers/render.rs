//! Rendering for the picker chrome: trigger chips, the composer footer, the
//! harness/model rail + list, the traits/mode/checkout/ref menus and the
//! device/project selectors.
//!
//! Every surface is `rune_kit::popover::anchored` to the chip that opened it
//! and pushed into the layer passed to `rune_kit::root(body, layer)`, so the
//! composer just calls [`popovers`] after building its body. Colors come from
//! [`crate::theme::Theme`]; layout numbers match the gpui constants.
//!
//! Harness brands use the shared icon catalogue in the rail and model rows; the
//! compact chip keeps a two-letter mark where a full icon would crowd the label.
//! harness brands draw as a compact two-letter monogram (`harness_mark`) rather
//! than the gpui SVG marks. Everything else keeps the gpui geometry.

use rune::{
    Color, El, GrowthSpec, Key, KeyEvent, ScrollSpec, TextStyle, Tx, col, el, icon, row, text,
};
use rune_kit::{Placement, TextInput, popover};

use hearth_proto::SessionMode;

use crate::state::App;
use crate::theme::Theme;

use super::loads::{Loadable, ensure_harnesses, ensure_refs, prefetch_models};
use super::model::{
    CheckoutKind, MAX_REF_ROWS, NO_ACTIVE_ROW, default_reasoning, harness_mark, mode_label,
    reasoning_label,
};
use super::selection::{self, selected_chat_row, traits_active, traits_label};
use super::{ModelRail, ModelRowData, PickerKind, PickersState, checkout, menu_step, space_picker};

// ---- anchor keys (one namespace; prefix app keys) -------------------------
pub const ANCHOR_MODEL: &str = "pickers/anchor-model";
pub const ANCHOR_TRAITS: &str = "pickers/anchor-traits";
pub const ANCHOR_MODE: &str = "pickers/anchor-mode";
pub const ANCHOR_BRANCH: &str = "pickers/anchor-branch";
pub const ANCHOR_CHECKOUT: &str = "pickers/anchor-checkout";
pub const ANCHOR_SPACE: &str = "pickers/anchor-space";
pub const ANCHOR_DEVICE: &str = "pickers/anchor-device";

const CHIP_HEIGHT: f32 = 32.0;
const CHIP_RADIUS: f32 = 8.0;
const CHIP_PAD_X: f32 = 10.0;
const FOOTER_CHIP_HEIGHT: f32 = 20.0;
const FOOTER_CHIP_RADIUS: f32 = 6.0;
const FOOTER_CHIP_PAD_X: f32 = 8.0;
const MENU_ROW_HEIGHT: f32 = 30.0;
const MENU_ROW_RADIUS: f32 = 6.0;
const LIST_MAX_HEIGHT: f32 = 224.0;
const MODEL_PICKER_HEIGHT: f32 = 346.0;
const MODEL_RAIL_WIDTH: f32 = 44.0;
const MODEL_LIST_ESTIMATE: f32 = 52.0;

fn text_style(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The composer action cluster: the mode, model and traits chips on the right
/// (the composer appends attach/send after this element), plus the footer row
/// (checkout + ref) underneath.
///
/// The caller pairs this with [`popovers`] in the same render pass and mounts
/// the layer with `rune_kit::root(body, layer)`.
pub fn render_cluster(state: &PickersState, app: &App, theme: &Theme) -> El {
    let no_agents = state.no_agents_available() && state.effective_harness(app).is_none();
    let model_label = model_chip_label(state, app, no_agents);
    let harness = state.effective_harness(app);
    let mark = if no_agents {
        "!"
    } else {
        harness.map(harness_mark).unwrap_or("AI")
    };
    let traits_set = traits_label(app);
    let model_chip = chip(theme, ANCHOR_MODEL, &model_label, true, Some(mark), false)
        .on_press(|app: &mut App| toggle_picker(app, PickerKind::HarnessModel));

    let has_traits = selection::has_traits(app);
    let traits_chip = has_traits.then(|| {
        let label = traits_set.clone().unwrap_or_else(|| "Traits".to_string());
        chip(
            theme,
            ANCHOR_TRAITS,
            &label,
            traits_active(app),
            None,
            state.compact_traits,
        )
        .on_press(|app: &mut App| toggle_picker(app, PickerKind::Traits))
    });

    let mode = state.effective_mode(app);
    let mode_chip = chip(
        theme,
        ANCHOR_MODE,
        mode.map(mode_label).unwrap_or("Mode"),
        mode.is_some(),
        None,
        false,
    )
    .on_press(|app: &mut App| toggle_picker(app, PickerKind::Mode));

    let right = row()
        .grow(0.0)
        .gap(4.0)
        .center_y()
        .child(open_state(
            mode_chip,
            theme,
            state.open == Some(PickerKind::Mode),
        ))
        .child(open_state(
            model_chip,
            theme,
            state.open == Some(PickerKind::HarnessModel),
        ))
        .children(
            traits_chip.map(|chip| open_state(chip, theme, state.open == Some(PickerKind::Traits))),
        );

    let mut cluster = col().w_full().min_w(0.0).gap(Theme::SPACE_SM);
    cluster = cluster.child(
        row()
            .w_full()
            .min_w(0.0)
            .justify_between()
            .gap(Theme::SPACE_SM)
            .center_y()
            .child(row().grow(1.0).min_w(0.0))
            .child(right),
    );
    if let Some(footer) = render_footer(state, app, theme) {
        cluster = cluster.child(footer);
    }
    cluster
}

/// The new-session canvas's picker block: the device + project selectors under
/// the canvas logo, then the action cluster. Their popovers open downward
/// ([`popovers`] anchors them below the chips).
pub fn render_new_session(state: &PickersState, app: &App, theme: &Theme) -> El {
    let device_id = app.effective_device_id();
    let device_label = device_id
        .as_deref()
        .and_then(|id| app.device_name(id))
        .unwrap_or("This device");
    let offline = device_id
        .as_deref()
        .is_some_and(|id| !app.device_online(id, chrono::Utc::now()));
    let project_label = app
        .selected_space_row()
        .map(|s| s.display_name().to_string())
        .unwrap_or_else(|| "No project".to_string());

    let device_chip = footer_chip(theme, PickerKind::Device, ANCHOR_DEVICE, device_label, "⌘")
        .on_press(|app: &mut App| toggle_picker(app, PickerKind::Device));
    let device_chip = if offline {
        device_chip.fill(theme.warning.alpha(0.10))
    } else {
        device_chip
    };
    let project_chip = footer_chip(theme, PickerKind::Space, ANCHOR_SPACE, &project_label, "▤")
        .on_press(|app: &mut App| toggle_picker(app, PickerKind::Space));

    col()
        .w_full()
        .gap(Theme::SPACE_SM)
        .child(
            row()
                .w_full()
                .gap(4.0)
                .center_y()
                .child(open_state(
                    device_chip,
                    theme,
                    state.open == Some(PickerKind::Device),
                ))
                .child(open_state(
                    project_chip,
                    theme,
                    state.open == Some(PickerKind::Space),
                )),
        )
        .child(render_cluster(state, app, theme))
}

/// The composer footer: the checkout-kind + ref chips on a new-session canvas,
/// read-only labels on an existing chat (a session never moves), `None` when
/// the selected space has no git.
pub fn render_footer(state: &PickersState, app: &App, theme: &Theme) -> Option<El> {
    let space = app.selected_space_row().cloned();
    if let Some(chat) = selected_chat_row(app) {
        let space = space.as_ref().filter(|s| s.git_detected)?;
        let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
        let (mark, label) = if is_worktree {
            ("⑂", "Worktree")
        } else {
            ("⌂", "Local checkout")
        };
        let left = row()
            .gap(4.0)
            .center_y()
            .min_w(0.0)
            .child(footer_label(theme, mark, label));
        let right = row().gap(4.0).center_y().min_w(0.0).child(footer_label(
            theme,
            "⑂",
            chat.branch.as_deref().unwrap_or("No ref"),
        ));
        return Some(
            row()
                .w_full()
                .justify_between()
                .gap(Theme::SPACE_SM)
                .px(10.0)
                .child(left)
                .child(right),
        );
    }

    let git = space.as_ref().is_some_and(|s| s.git_detected);
    if !git {
        return None;
    }

    let ref_chip = footer_chip(
        theme,
        PickerKind::Branch,
        ANCHOR_BRANCH,
        &state.ref_label(),
        "⑂",
    )
    .on_press(|app: &mut App| toggle_picker(app, PickerKind::Branch));
    let kind_mark = match (
        state.config.checkout,
        state.selected_ref_worktree().is_some(),
    ) {
        (CheckoutKind::Local, false) => "⌂",
        _ => "⑂",
    };
    let kind_chip = footer_chip(
        theme,
        PickerKind::Checkout,
        ANCHOR_CHECKOUT,
        state.checkout_label(),
        kind_mark,
    )
    .on_press(|app: &mut App| toggle_picker(app, PickerKind::Checkout));

    Some(
        row()
            .w_full()
            .justify_between()
            .gap(Theme::SPACE_SM)
            .px(10.0)
            .child(row().gap(4.0).center_y().min_w(0.0).child(open_state(
                kind_chip,
                theme,
                state.open == Some(PickerKind::Checkout),
            )))
            .child(row().gap(4.0).center_y().min_w(0.0).child(open_state(
                ref_chip,
                theme,
                state.open == Some(PickerKind::Branch),
            ))),
    )
}

/// Mount the open picker's surface (if any) into the window layer. Call after
/// the cluster/new-session body in the same view, then `rune_kit::root(body,
/// layer)`.
pub fn popovers(state: &PickersState, app: &App, theme: &Theme, layer: &mut Vec<El>) {
    let Some(kind) = state.open else {
        return;
    };
    // Guard against a stale open kind whose trigger isn't on screen: mounting
    // an anchored surface without its anchor would pin it to the window origin.
    match kind {
        PickerKind::Space | PickerKind::Device if app.selected_chat.is_some() => return,
        PickerKind::Branch | PickerKind::Checkout
            if !app.selected_space_row().is_some_and(|s| s.git_detected) =>
        {
            return;
        }
        _ => {}
    }
    let (anchor, placement) = match kind {
        PickerKind::HarnessModel => (ANCHOR_MODEL, Placement::TopEnd),
        PickerKind::Traits => (ANCHOR_TRAITS, Placement::TopEnd),
        PickerKind::Mode => (ANCHOR_MODE, Placement::TopEnd),
        PickerKind::Branch => (ANCHOR_BRANCH, Placement::TopEnd),
        PickerKind::Checkout => (ANCHOR_CHECKOUT, Placement::TopStart),
        PickerKind::Space => (ANCHOR_SPACE, Placement::BottomStart),
        PickerKind::Device => (ANCHOR_DEVICE, Placement::BottomStart),
    };
    let (width, body) = match kind {
        PickerKind::HarnessModel => (360.0, model_popover_body(state, app, theme)),
        PickerKind::Traits => (240.0, traits_popover_body(state, app, theme)),
        PickerKind::Mode => (200.0, mode_popover_body(state, app, theme)),
        PickerKind::Branch => (320.0, branch_popover_body(state, app, theme)),
        PickerKind::Checkout => (224.0, checkout_popover_body(state, app, theme)),
        PickerKind::Space => (280.0, space_popover_body(state, app, theme)),
        PickerKind::Device => (224.0, device_popover_body(state, app, theme)),
    };
    let flush = matches!(
        kind,
        PickerKind::HarnessModel | PickerKind::Traits | PickerKind::Mode
    );
    let pad = if flush { 0.0 } else { 4.0 };
    let card = col()
        .key(format!("pickers/popover-{}", kind_key(kind)))
        .w(width)
        .max_h(640.0)
        .fill(theme.surface_overlay)
        .border(1.0, theme.border)
        .radius(10.0)
        .clip_rounded(10.0)
        .shadow(10.0, 30.0, theme.ink(0.22))
        .padding(pad)
        .modal(true, |app: &mut App| close_picker(app))
        .focusable(true)
        .on_key(|app: &mut App, ev, _tx| handle_picker_key(app, ev))
        .child(body);
    // Searchable popovers focus their text field (the modal's default focus);
    // the rest focus the card itself for pure keyboard nav.
    let card = if has_search(kind) {
        card
    } else {
        card.autofocus()
    };
    layer.push(popover::anchored(&theme.kit, card, anchor, placement, 6.0));
}

// ---------------------------------------------------------------------------
// Open / close / keyboard
// ---------------------------------------------------------------------------

fn kind_key(kind: PickerKind) -> &'static str {
    match kind {
        PickerKind::HarnessModel => "model",
        PickerKind::Traits => "traits",
        PickerKind::Mode => "mode",
        PickerKind::Branch => "branch",
        PickerKind::Checkout => "checkout",
        PickerKind::Space => "space",
        PickerKind::Device => "device",
    }
}

fn has_search(kind: PickerKind) -> bool {
    matches!(
        kind,
        PickerKind::HarnessModel | PickerKind::Branch | PickerKind::Space | PickerKind::Device
    )
}

/// Open a picker, or close it when it is already open. Forced loads refresh
/// stale rows in place (stale-while-revalidate).
pub fn toggle_picker(app: &mut App, kind: PickerKind) {
    if app.pickers.open == Some(kind) {
        close_picker(app);
        return;
    }
    app.pickers.open = Some(kind);
    app.pickers.switch_error = None;
    app.pickers.search.clear();

    // Prime the model picker's rail BEFORE anchoring the highlight (the visible
    // rows depend on it): the favorites view when stars exist, else the
    // effective harness. Locked chats stay on their own harness.
    if kind == PickerKind::HarnessModel {
        app.pickers.model_rail =
            if app.selected_chat.is_none() && !app.pickers.defaults.favorites.is_empty() {
                ModelRail::Favorites
            } else {
                ModelRail::Harness
            };
    }
    app.pickers.active = match kind {
        PickerKind::Checkout => match app.pickers.config.checkout {
            CheckoutKind::Local => 0,
            CheckoutKind::NewWorktree => 1,
        },
        PickerKind::Branch => app.pickers.selected_ref_index(app),
        PickerKind::HarnessModel | PickerKind::Traits => app.pickers.selected_model_index(app),
        PickerKind::Mode => app.pickers.effective_mode_index(app),
        PickerKind::Space => app.pickers.selected_space_index(app),
        PickerKind::Device => app.pickers.selected_device_index(app),
    };

    match kind {
        // Force: the checkout state moves under us (a send mints a
        // worktree+branch, terminals switch refs) — every open revalidates.
        PickerKind::Branch | PickerKind::Checkout => {
            ensure_refs(app, true);
        }
        PickerKind::HarnessModel | PickerKind::Traits => {
            // Force: the enabled set moves under us (Settings → Agents) and
            // model discovery can recover after a slow ACP cold start.
            ensure_harnesses(app, true);
            prefetch_models(app, true);
        }
        PickerKind::Mode | PickerKind::Space | PickerKind::Device => {}
    }
}

/// Close the open picker.
pub fn close_picker(app: &mut App) {
    app.pickers.open = None;
}

/// The shared key handler for every picker surface and search field.
pub fn handle_picker_key(app: &mut App, ev: &KeyEvent) -> bool {
    let Some(kind) = app.pickers.open else {
        return false;
    };
    // ⌘1…⌘9 jump-picks the Nth visible model row (t3 modelPickerKeys; the
    // chips on the rows advertise these).
    if kind == PickerKind::HarnessModel
        && let Key::Command(c) = ev.key
        && let Some(n) = c.to_digit(10)
        && (1..=9).contains(&n)
    {
        selection::activate_model_index(app, n as usize - 1);
        return true;
    }
    match ev.key {
        Key::Escape => {
            close_picker(app);
            true
        }
        Key::Up | Key::Down => {
            let delta = if matches!(ev.key, Key::Up) { -1 } else { 1 };
            let count = row_count(app, kind);
            let current = (app.pickers.active != NO_ACTIVE_ROW).then_some(app.pickers.active);
            app.pickers.active = menu_step(current, count, delta).unwrap_or(0);
            true
        }
        Key::Enter => {
            match kind {
                PickerKind::HarnessModel => selection::activate_model_row(app),
                PickerKind::Checkout => {
                    let kind = if app.pickers.active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                    checkout::pick_checkout(app, kind);
                }
                PickerKind::Mode => {
                    const MODES: [SessionMode; 3] =
                        [SessionMode::Plan, SessionMode::Agent, SessionMode::Chat];
                    if let Some(mode) = MODES.get(app.pickers.active) {
                        selection::pick_mode(app, *mode);
                        close_picker(app);
                    }
                }
                PickerKind::Branch => {
                    if let Some(row) = app
                        .pickers
                        .filtered_ref_rows(app)
                        .into_iter()
                        .nth(app.pickers.active)
                    {
                        selection::pick_ref(app, row);
                    }
                }
                PickerKind::Space => {
                    if let Some(space) = app
                        .pickers
                        .filtered_space_rows(app)
                        .into_iter()
                        .nth(app.pickers.active)
                    {
                        space_picker::pick_space(app, space.id);
                    }
                }
                PickerKind::Device => {
                    if let Some(device) = app
                        .pickers
                        .filtered_device_rows(app)
                        .into_iter()
                        .nth(app.pickers.active)
                    {
                        space_picker::pick_device(app, device.id);
                    }
                }
                PickerKind::Traits => {}
            }
            true
        }
        _ => false,
    }
}

fn row_count(app: &App, kind: PickerKind) -> usize {
    match kind {
        PickerKind::Branch => app.pickers.filtered_ref_rows(app).len().min(MAX_REF_ROWS),
        PickerKind::Checkout => 2,
        PickerKind::HarnessModel => app.pickers.model_rows_len(app),
        PickerKind::Traits => 0,
        PickerKind::Mode => 3,
        PickerKind::Space => app.pickers.filtered_space_rows(app).len(),
        PickerKind::Device => app.pickers.filtered_device_rows(app).len(),
    }
}

fn on_search_changed(app: &mut App) {
    match app.pickers.open {
        Some(PickerKind::Branch)
        | Some(PickerKind::HarnessModel)
        | Some(PickerKind::Space)
        | Some(PickerKind::Device) => app.pickers.active = 0,
        _ => {}
    }
}

/// The measured-width hook for the traits chip's compact presentation.
pub fn set_composer_width(app: &mut App, width: f32) {
    app.pickers.set_composer_width(width);
}

// ---------------------------------------------------------------------------
// Shared chrome
// ---------------------------------------------------------------------------

fn chip(
    theme: &Theme,
    anchor: &'static str,
    label: &str,
    set: bool,
    mark: Option<&str>,
    compact: bool,
) -> El {
    let mut e = row()
        .key(anchor)
        .h(CHIP_HEIGHT)
        .max_w(208.0)
        .min_w(0.0)
        .gap(6.0)
        .px(CHIP_PAD_X)
        .radius(CHIP_RADIUS)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .fill(Color::TRANSPARENT)
        .hover(theme.element_hover);
    if compact {
        e = e.w(CHIP_HEIGHT).px(0.0).justify_center().child(
            text("•••")
                .font(text_style(11.0, 500.0))
                .color(theme.text_muted),
        );
        return e;
    }
    if let Some(mark) = mark {
        e = e.child(mark_badge(theme, mark, 16.0));
    }
    let ink = if set {
        theme.text.alpha(0.9)
    } else {
        theme.text_muted
    };
    e.child(
        text(label.to_string())
            .font(text_style(12.0, 500.0))
            .color(ink)
            .truncate()
            .grow(1.0)
            .min_w(0.0),
    )
}

/// Mark the chip that owns the open popover.
fn open_state(e: El, theme: &Theme, open: bool) -> El {
    if open { e.fill(theme.element_hover) } else { e }
}

fn footer_chip(
    theme: &Theme,
    _kind: PickerKind,
    anchor: &'static str,
    label: &str,
    mark: &str,
) -> El {
    row()
        .key(anchor)
        .h(FOOTER_CHIP_HEIGHT)
        .max_w(280.0)
        .min_w(0.0)
        .gap(6.0)
        .px(FOOTER_CHIP_PAD_X)
        .radius(FOOTER_CHIP_RADIUS)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .fill(Color::TRANSPARENT)
        .hover(theme.element_hover)
        .child(
            text(mark.to_string())
                .font(text_style(11.0, 500.0))
                .color(theme.text_muted.alpha(0.7)),
        )
        .child(
            text(label.to_string())
                .font(text_style(12.0, 500.0))
                .color(theme.text_muted.alpha(0.7))
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .child(
            text("⌄")
                .font(text_style(11.0, 500.0))
                .color(theme.text_muted.alpha(0.5)),
        )
}

fn footer_label(theme: &Theme, mark: &str, label: &str) -> El {
    row()
        .h(FOOTER_CHIP_HEIGHT)
        .max_w(160.0)
        .min_w(0.0)
        .gap(6.0)
        .px(FOOTER_CHIP_PAD_X)
        .center_y()
        .child(
            text(mark.to_string())
                .font(text_style(11.0, 500.0))
                .color(theme.text_muted.alpha(0.6)),
        )
        .child(
            text(label.to_string())
                .font(text_style(12.0, 500.0))
                .color(theme.text_muted.alpha(0.6))
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
}

fn mark_badge(theme: &Theme, mark: &str, size: f32) -> El {
    el().size(size, size)
        .min_w(size)
        .radius(4.0)
        .center()
        .fill(theme.ink(0.07))
        .child(
            text(mark.to_string())
                .font(text_style(if size <= 16.0 { 8.0 } else { 10.0 }, 600.0))
                .color(theme.text_muted),
        )
}

fn section_heading(theme: &Theme, label: &str) -> El {
    row().w_full().px(8.0).py(4.0).child(
        text(label.to_string())
            .font(text_style(10.0, 600.0))
            .color(theme.text_muted.alpha(0.6)),
    )
}

fn separator(theme: &Theme) -> El {
    el().w_full().h(1.0).my(4.0).fill(theme.ink(0.08))
}

fn empty_note(theme: &Theme, copy: &str) -> El {
    row().w_full().px(8.0).py(24.0).center().child(
        text(copy.to_string())
            .font(text_style(12.0, 400.0))
            .color(theme.text_muted.alpha(0.6))
            .wrap()
            .align(rune::Align::Center),
    )
}

fn skeleton_rows(theme: &Theme, rows: usize, height: f32) -> El {
    col()
        .w_full()
        .gap(4.0)
        .children((0..rows).map(|_| el().w_full().h(height).radius(6.0).fill(theme.ink(0.04))))
}

/// A menu row: hover moves the shared highlight (one wash at a time), selection
/// is the stronger wash, the active keyboard/hover cursor a light one.
fn menu_row(
    theme: &Theme,
    ix: usize,
    key_prefix: &'static str,
    selected: bool,
    active: bool,
    label: &str,
    trailing: Option<El>,
) -> El {
    let mut e = row()
        .key(format!("{key_prefix}/{ix}"))
        .w_full()
        .h(MENU_ROW_HEIGHT)
        .px(8.0)
        .radius(MENU_ROW_RADIUS)
        .gap(8.0)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .fill(if selected {
            theme.wash(0.11)
        } else if active {
            theme.ink(0.05)
        } else {
            Color::TRANSPARENT
        })
        .hover(theme.ink(0.06))
        .on_hover_change(move |app: &mut App, inside: bool, _tx: &mut Tx| {
            if inside && app.pickers.active != ix {
                app.pickers.active = ix;
            }
        })
        .child(
            text(label.to_string())
                .font(text_style(12.0, 500.0))
                .color(if selected {
                    theme.text
                } else {
                    theme.text.alpha(0.9)
                })
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        );
    if let Some(trailing) = trailing {
        e = e.child(trailing);
    }
    e
}

fn check_mark(theme: &Theme, on: bool) -> El {
    text(if on { "✓" } else { "" })
        .font(text_style(11.0, 600.0))
        .color(theme.accent)
}

fn default_badge(theme: &Theme) -> El {
    text("Default")
        .font(text_style(10.0, 600.0))
        .color(theme.text_muted.alpha(0.6))
}

fn kbd_hint(theme: &Theme, keys: &str) -> El {
    row()
        .h(18.0)
        .min_w(28.0)
        .px(4.0)
        .radius(4.0)
        .border(1.0, theme.border)
        .center()
        .child(
            text(keys.to_string())
                .font(text_style(10.0, 500.0))
                .color(theme.text_muted.alpha(0.6)),
        )
}

fn retry_row(theme: &Theme, message: &str, kind: PickerKind) -> El {
    col()
        .w_full()
        .gap(6.0)
        .padding(8.0)
        .child(
            text(message.to_string())
                .font(text_style(11.0, 400.0))
                .color(theme.danger.alpha(0.9))
                .wrap(),
        )
        .child(
            row()
                .px(Theme::SPACE_SM)
                .py(3.0)
                .radius(Theme::CONTROL_RADIUS)
                .border(1.0, theme.border)
                .cursor(rune::Cursor::Pointer)
                .hover(theme.element_hover)
                .on_press(move |app: &mut App| retry_load(app, kind))
                .child(
                    text("Retry")
                        .font(text_style(12.0, 500.0))
                        .color(theme.text),
                ),
        )
}

fn retry_load(app: &mut App, kind: PickerKind) {
    match kind {
        PickerKind::Branch | PickerKind::Checkout => {
            ensure_refs(app, true);
        }
        PickerKind::HarnessModel | PickerKind::Traits => {
            app.pickers.harnesses = Loadable::Idle;
            app.pickers.models.clear();
            app.pickers.catalog_rev += 1;
            ensure_harnesses(app, false);
        }
        PickerKind::Space | PickerKind::Device | PickerKind::Mode => {}
    }
}

fn search_field(state: &PickersState, theme: &Theme, placeholder: &str) -> El {
    TextInput::new(&state.search)
        .plain()
        .width(None)
        .placeholder(placeholder.to_string())
        .label(placeholder.to_string())
        .text_style(text_style(13.0, 400.0).lh(18.0))
        .height(28.0)
        .on_edit(|app: &mut App| &mut app.pickers.search)
        .on_change(|app: &mut App, _tx: &mut Tx| on_search_changed(app))
        .on_key(|app: &mut App, ev, _tx| handle_picker_key(app, ev))
        .build(&theme.kit)
}

fn search_box(state: &PickersState, theme: &Theme, placeholder: &str) -> El {
    row()
        .w_full()
        .h(40.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], theme.ink(0.08))
        .child(
            text("⌕")
                .font(text_style(14.0, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        )
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .child(search_field(state, theme, placeholder)),
        )
}

// ---------------------------------------------------------------------------
// Harness / model popover
// ---------------------------------------------------------------------------

fn model_chip_label(state: &PickersState, app: &App, no_agents: bool) -> String {
    if no_agents {
        return "No agents available".to_string();
    }
    if let Some(model) = state.selected_model(app) {
        return model.label.clone();
    }
    let remembered = state
        .effective_harness(app)
        .and_then(|h| state.defaults.model_for(h));
    match state.effective_model_id(app) {
        Some(id) => remembered
            .filter(|m| m.id == id)
            .map(|m| m.label.clone())
            .or_else(|| state.defaults.label_for(id).map(str::to_string))
            .unwrap_or_else(|| id.to_string()),
        None => remembered.map(|m| m.label.clone()).unwrap_or_default(),
    }
}

fn model_popover_body(state: &PickersState, app: &App, theme: &Theme) -> El {
    match &state.harnesses {
        Loadable::Loading | Loadable::Idle => {
            return col()
                .h(MODEL_PICKER_HEIGHT)
                .padding(8.0)
                .child(skeleton_rows(theme, 5, 44.0));
        }
        Loadable::Error(message) => {
            return col().h(MODEL_PICKER_HEIGHT).padding(8.0).child(retry_row(
                theme,
                message,
                PickerKind::HarnessModel,
            ));
        }
        Loadable::Ready(_) => {}
    }

    let effective = state.effective_harness(app);
    let locked = state.harness_locked(app);
    let descriptors = state.rail_descriptors(app);
    if descriptors.is_empty() {
        return col()
            .h(MODEL_PICKER_HEIGHT)
            .padding(16.0)
            .center()
            .gap(8.0)
            .child(
                text("No agents available")
                    .font(text_style(13.0, 500.0))
                    .color(theme.text),
            )
            .child(
                text("Enable an installed agent in Settings → Agents, or install an agent CLI.")
                    .font(text_style(12.0, 400.0))
                    .color(theme.text_muted)
                    .wrap()
                    .align(rune::Align::Center),
            );
    }

    let searching = !state.search.text.trim().is_empty();
    let favorites_view = !searching && state.model_rail == ModelRail::Favorites;
    let rows = state.model_rows(app);

    let rail = (!searching).then(|| {
        let mut column = col()
            .w(MODEL_RAIL_WIDTH)
            .min_w(MODEL_RAIL_WIDTH)
            .padding(4.0)
            .gap(4.0);
        let favorites_active = favorites_view;
        column = column.child(
            el().key("pickers/rail-favorites")
                .size(36.0, 36.0)
                .radius(8.0)
                .center()
                .cursor(rune::Cursor::Pointer)
                .hover(theme.ink(0.06))
                .on_press(|app: &mut App| {
                    app.pickers.model_rail = ModelRail::Favorites;
                    let active = app.pickers.selected_model_index(app);
                    app.pickers.active = active;
                })
                .child(
                    text("★")
                        .font(text_style(17.0, 400.0))
                        .color(if favorites_active {
                            theme.text
                        } else {
                            theme.text_muted.alpha(0.75)
                        }),
                )
                .children(favorites_active.then(|| rail_indicator(theme))),
        );
        column = column.child(el().w_full().h(1.0).mx(-4.0).my(1.0).fill(theme.ink(0.08)));
        for (ix, descriptor) in descriptors.iter().enumerate() {
            let harness = descriptor.id;
            let is_viewed = !favorites_view && effective == Some(harness);
            let is_disabled = locked && effective != Some(harness);
            let mut tab = el()
                .key(format!("pickers/harness-tab/{ix}"))
                .size(36.0, 36.0)
                .radius(8.0)
                .center()
                .child(
                    icon(crate::shell::titlebar::harness_mark(harness, theme).0, 18.0).color(
                        if is_viewed {
                            theme.text
                        } else {
                            theme.text_muted
                        },
                    ),
                );
            if is_disabled {
                tab = tab.opacity(0.35);
            } else {
                tab = tab
                    .cursor(rune::Cursor::Pointer)
                    .hover(theme.ink(0.06))
                    .on_press(move |app: &mut App| {
                        app.pickers.model_rail = ModelRail::Harness;
                        selection::pick_harness(app, harness);
                    });
            }
            if is_viewed {
                tab = tab.child(rail_indicator(theme));
            }
            column = column.child(tab);
        }
        column
    });

    let list = if rows.is_empty() {
        let note = if searching {
            "No models found"
        } else if favorites_view {
            "No starred models yet — hit a row's star"
        } else if let Some(Loadable::Error(message)) = effective.and_then(|h| state.models.get(&h))
        {
            let mut retry = col();
            retry = retry.child(retry_row(theme, message, PickerKind::HarnessModel));
            return col()
                .h(MODEL_PICKER_HEIGHT)
                .child(
                    row()
                        .h(46.0)
                        .px(10.0)
                        .border_sides([0.0, 0.0, 1.0, 0.0], theme.ink(0.08))
                        .child(search_field(state, theme, "Search models…")),
                )
                .child(retry);
        } else {
            ""
        };
        if note.is_empty() {
            skeleton_rows(theme, 4, 44.0)
        } else {
            empty_note(theme, note)
        }
    } else {
        let rows_el = col()
            .key("pickers/model-rows")
            .w_full()
            .growth(GrowthSpec::default())
            .virtualize(MODEL_LIST_ESTIMATE)
            .children(
                rows.iter()
                    .enumerate()
                    .map(|(ix, row_data)| model_row(state, app, theme, ix, row_data)),
            );
        let viewport = col()
            .key("pickers/model-scroll")
            .grow(1.0)
            .min_h(0.0)
            .padding(6.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(rows_el);
        rune_kit::scroll_bar(
            &theme.kit,
            "pickers/model-scroll-area",
            "pickers/model-scroll",
            viewport,
        )
        .grow(1.0)
        .min_h(0.0)
    };

    let pane = col()
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .fill(theme.ink(0.02))
        .border_sides([0.0, 0.0, 0.0, 1.0], theme.ink(0.07))
        .child(search_box(state, theme, "Search models…"))
        .child(
            col()
                .key("pickers/model-list")
                .grow(1.0)
                .min_h(0.0)
                .child(list),
        );

    row()
        .h(MODEL_PICKER_HEIGHT)
        .items_stretch()
        .child(
            col()
                .grow(0.0)
                .child(rail.unwrap_or_else(|| el().w(0.0).h_full())),
        )
        .child(pane)
}

fn rail_indicator(theme: &Theme) -> El {
    el().w(3.0)
        .h(20.0)
        .pin(None, Some(8.0), Some(-4.0), None)
        .radius(3.0)
        .fill(theme.accent)
}

fn model_row(
    state: &PickersState,
    app: &App,
    theme: &Theme,
    ix: usize,
    row_data: &ModelRowData,
) -> El {
    let effective = state.effective_harness(app);
    let selected = Some(row_data.harness) == effective
        && state.selected_model(app).map(|m| m.id.as_str()) == Some(row_data.model.id.as_str());
    let active = ix == state.active;
    let favorite = state
        .defaults
        .is_favorite(row_data.harness, &row_data.model.id);
    let harness = row_data.harness;
    let model_id = row_data.model.id.clone();
    let attribution = row_data
        .model
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(&row_data.harness_name))
        .map(str::to_string);

    let mut subline = row()
        .gap(6.0)
        .center_y()
        .child(
            icon(crate::shell::titlebar::harness_mark(harness, theme).0, 11.0)
                .color(theme.text_muted),
        )
        .child(
            text(row_data.harness_name.clone())
                .font(text_style(11.0, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        );
    if let Some(attribution) = attribution {
        subline = subline
            .child(
                text("·")
                    .font(text_style(11.0, 400.0))
                    .color(theme.text_muted.alpha(0.45)),
            )
            .child(
                text(attribution)
                    .font(text_style(11.0, 400.0))
                    .color(theme.text_muted.alpha(0.7))
                    .truncate()
                    .min_w(0.0),
            );
    }

    let mut body = row()
        .w_full()
        .px(8.0)
        .py(6.0)
        .radius(8.0)
        .gap(10.0)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .fill(if selected {
            theme.wash(0.11)
        } else if active {
            theme.ink(0.05)
        } else {
            Color::TRANSPARENT
        })
        .hover(theme.ink(0.06))
        .on_hover_change(move |app: &mut App, inside: bool, _tx: &mut Tx| {
            if inside && app.pickers.active != ix {
                app.pickers.active = ix;
            }
        })
        .on_press(move |app: &mut App| selection::activate_model_index(app, ix))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text(row_data.model.label.clone())
                        .font(text_style(12.5, 500.0))
                        .color(theme.text)
                        .truncate()
                        .w_full(),
                )
                .child(subline),
        );
    if ix < 9 {
        body = body.child(kbd_hint(theme, &format!("⌘{}", ix + 1)));
    }
    body = body.child(
        row()
            .key(format!("pickers/model-star/{ix}"))
            .size(22.0, 22.0)
            .min_w(22.0)
            .radius(6.0)
            .center()
            .cursor(rune::Cursor::Pointer)
            .hover(theme.ink(0.08))
            .on_press(move |app: &mut App| {
                selection::toggle_model_favorite(app, harness, &model_id);
            })
            .child(
                text(if favorite { "★" } else { "☆" })
                    .font(text_style(13.0, 400.0))
                    .color(if favorite {
                        theme.warning
                    } else {
                        theme.text_muted.alpha(0.45)
                    }),
            ),
    );
    col()
        .key(format!("pickers/model-row/{ix}"))
        .w_full()
        .pb(2.0)
        .child(body)
}

// ---------------------------------------------------------------------------
// Traits / mode
// ---------------------------------------------------------------------------

fn traits_popover_body(state: &PickersState, app: &App, theme: &Theme) -> El {
    let Some(model) = state.selected_model(app) else {
        return col().padding(8.0).child(skeleton_rows(theme, 3, 24.0));
    };
    let levels = state.trait_ladder(app);
    let current = state.effective_reasoning(app);
    let mut sections: Vec<El> = Vec::new();
    if !levels.is_empty() {
        let default_level = default_reasoning(&levels);
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(section_heading(theme, "Reasoning"));
        for (ix, level) in levels.iter().copied().enumerate() {
            let is_active = current == Some(level);
            let row = menu_row(
                theme,
                ix,
                "pickers/reasoning-row",
                is_active,
                false,
                reasoning_label(level),
                None,
            )
            .on_press(move |app: &mut App| selection::pick_reasoning(app, level));
            let row = if default_level == Some(level) {
                row.child(default_badge(theme))
            } else {
                row
            };
            section = section.child(row);
        }
        sections.push(section);
    }

    let selections = state.explicit_options(app);
    for (opt_ix, option) in model.options.iter().enumerate() {
        if !sections.is_empty() {
            sections.push(separator(theme));
        }
        let selected_choice = selections
            .get(&option.id)
            .and_then(|v| v.as_str())
            .unwrap_or(&option.default_choice)
            .to_string();
        let option_id = option.id.clone();
        let default_choice = option.default_choice.clone();
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(section_heading(theme, &option.label));
        for (choice_ix, choice) in option.choices.iter().enumerate() {
            let choice_id = choice.id.clone();
            let option_id = option_id.clone();
            let is_default = choice.id == default_choice;
            let is_active = selected_choice == choice.id;
            let row = menu_row(
                theme,
                opt_ix * 32 + choice_ix,
                "pickers/trait-choice",
                is_active,
                false,
                &choice.label,
                None,
            )
            .on_press(move |app: &mut App| {
                selection::pick_option(app, option_id.clone(), choice_id.clone(), is_default);
            });
            let row = if is_default {
                row.child(default_badge(theme))
            } else {
                row.child(check_mark(theme, is_active))
            };
            section = section.child(row);
        }
        sections.push(section);
    }

    col().w_full().pb(2.0).children(sections)
}

fn mode_popover_body(state: &PickersState, app: &App, theme: &Theme) -> El {
    let current = state.effective_mode(app);
    const MODES: [SessionMode; 3] = [SessionMode::Plan, SessionMode::Agent, SessionMode::Chat];
    let mut body = col()
        .w_full()
        .gap(2.0)
        .child(section_heading(theme, "Mode"));
    for (ix, mode) in MODES.iter().copied().enumerate() {
        let is_active = current == Some(mode);
        let row = menu_row(
            theme,
            ix,
            "pickers/mode-row",
            is_active,
            false,
            mode_label(mode),
            Some(check_mark(theme, is_active)),
        )
        .on_press(move |app: &mut App| {
            selection::pick_mode(app, mode);
            close_picker(app);
        });
        body = body.child(row);
    }
    body
}

// ---------------------------------------------------------------------------
// Branch / checkout
// ---------------------------------------------------------------------------

fn branch_popover_body(state: &PickersState, app: &App, theme: &Theme) -> El {
    if app.selected_space_row().is_none() {
        return col().padding(8.0).child(
            text("No project selected")
                .font(text_style(12.0, 400.0))
                .color(theme.text_faint),
        );
    }
    let rows = state.filtered_ref_rows(app);
    let total = rows.len();
    let shown = total.min(MAX_REF_ROWS);
    let session_branch = selected_chat_row(app).and_then(|c| c.branch.clone());
    let selected = session_branch.or_else(|| state.config.branch.clone());
    let switching = state.switching.clone();

    let body = match &state.refs {
        Loadable::Loading | Loadable::Idle => skeleton_rows(theme, 4, 26.0),
        Loadable::Error(message) => retry_row(theme, message, PickerKind::Branch),
        Loadable::Ready(_) if rows.is_empty() => empty_note(theme, "No refs found."),
        Loadable::Ready(_) => col()
            .w_full()
            .gap(2.0)
            .max_h(LIST_MAX_HEIGHT)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .children(
                rows.into_iter()
                    .take(MAX_REF_ROWS)
                    .enumerate()
                    .map(|(ix, row_data)| {
                        let is_selected = selected.as_deref() == Some(row_data.name.as_str());
                        let tag = if row_data.current {
                            Some("current")
                        } else if row_data.worktree_path.is_some() {
                            Some("worktree")
                        } else {
                            None
                        };
                        let is_switching = switching.as_deref() == Some(row_data.name.as_str());
                        let mut trailing = row().gap(6.0).center_y();
                        if is_switching {
                            trailing = trailing.child(
                                text("switching…")
                                    .font(text_style(10.0, 400.0))
                                    .color(theme.text_muted.alpha(0.6)),
                            );
                        }
                        if let Some(tag) = tag {
                            trailing = trailing.child(
                                text(tag.to_string())
                                    .font(text_style(10.0, 400.0))
                                    .color(theme.text_muted.alpha(0.45)),
                            );
                        }
                        let row = menu_row(
                            theme,
                            ix,
                            "pickers/branch-row",
                            is_selected,
                            ix == state.active,
                            &row_data.name,
                            Some(trailing),
                        )
                        .on_press(move |app: &mut App| selection::pick_ref(app, row_data.clone()));
                        if switching.is_some() {
                            row.opacity(0.55)
                        } else {
                            row
                        }
                    }),
            ),
    };

    let mut panel = col()
        .w_full()
        .gap(2.0)
        .child(search_box(state, theme, "Search refs…"))
        .child(body);
    if let Some(error) = &state.switch_error {
        panel = panel.child(separator(theme)).child(
            text(error.clone())
                .font(text_style(11.0, 400.0))
                .color(theme.danger.alpha(0.9))
                .wrap()
                .px(8.0)
                .py(4.0),
        );
    }
    if total > shown {
        panel = panel.child(separator(theme)).child(
            text(format!("Showing {shown} of {total} refs"))
                .font(text_style(11.0, 400.0))
                .color(theme.text_faint)
                .px(8.0)
                .py(4.0),
        );
    }
    panel
}

fn checkout_popover_body(state: &PickersState, app: &App, theme: &Theme) -> El {
    let _ = app;
    let has_worktree = state.selected_ref_worktree().is_some();
    let local_label = if has_worktree {
        "Current worktree"
    } else {
        "Current checkout"
    };
    let options: [(CheckoutKind, &str, &str); 2] = [
        (
            CheckoutKind::Local,
            local_label,
            if has_worktree { "⑂" } else { "⌂" },
        ),
        (CheckoutKind::NewWorktree, "New worktree", "⑂"),
    ];
    let mut body = col().w_full().gap(2.0);
    for (ix, (kind, label, mark)) in options.into_iter().enumerate() {
        let is_selected = state.config.checkout == kind;
        let row = menu_row(
            theme,
            ix,
            "pickers/checkout-row",
            is_selected,
            ix == state.active,
            label,
            Some(check_mark(theme, is_selected)),
        )
        .child(
            text(mark.to_string())
                .font(text_style(13.0, 400.0))
                .color(theme.text_muted),
        )
        .on_press(move |app: &mut App| checkout::pick_checkout(app, kind));
        body = body.child(row);
    }
    body
}

// ---------------------------------------------------------------------------
// Space / device
// ---------------------------------------------------------------------------

fn space_popover_body(state: &PickersState, app: &App, theme: &Theme) -> El {
    let rows = state.filtered_space_rows(app);
    let selected = app.selected_space_row().map(|s| s.id.clone());
    let body = if rows.is_empty() {
        let copy = if state.search.text.is_empty() {
            "No projects on this device."
        } else {
            "No projects match."
        };
        empty_note(theme, copy)
    } else {
        col()
            .w_full()
            .gap(2.0)
            .max_h(LIST_MAX_HEIGHT)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .children(rows.into_iter().enumerate().map(|(ix, space)| {
                let is_selected = selected.as_deref() == Some(space.id.as_str());
                let id = space.id.clone();
                menu_row(
                    theme,
                    ix,
                    "pickers/space-row",
                    is_selected,
                    ix == state.active,
                    space.display_name(),
                    Some(check_mark(theme, is_selected)),
                )
                .on_press(move |app: &mut App| space_picker::pick_space(app, id.clone()))
            }))
    };

    let new_project = row()
        .key("pickers/project-new")
        .w_full()
        .h(MENU_ROW_HEIGHT)
        .px(8.0)
        .radius(MENU_ROW_RADIUS)
        .gap(8.0)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .hover(theme.ink(0.06))
        .on_press(|app: &mut App| space_picker::request_new_project(app))
        .child(
            text("＋")
                .font(text_style(12.0, 500.0))
                .color(theme.text_muted.alpha(0.7)),
        )
        .child(
            text("New project…")
                .font(text_style(12.0, 500.0))
                .color(theme.text.alpha(0.9)),
        );

    col()
        .w_full()
        .gap(2.0)
        .child(search_box(state, theme, "Search projects…"))
        .child(body)
        .child(separator(theme))
        .child(new_project)
}

fn device_popover_body(state: &PickersState, app: &App, theme: &Theme) -> El {
    let rows = state.filtered_device_rows(app);
    let effective = app.effective_device_id();
    let local = app.local_device_id.clone();
    let now = chrono::Utc::now();
    let body = if rows.is_empty() {
        empty_note(theme, "No devices match.")
    } else {
        col()
            .w_full()
            .gap(2.0)
            .max_h(LIST_MAX_HEIGHT)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .children(rows.into_iter().enumerate().map(|(ix, device)| {
                let is_local = local.as_deref() == Some(device.id.as_str());
                let is_selected = effective.as_deref() == Some(device.id.as_str());
                let online = app.device_online(&device.id, now);
                let id = device.id.clone();
                let mut trailing = row().gap(6.0).center_y();
                if is_local {
                    trailing = trailing.child(
                        text("You")
                            .font(text_style(10.0, 500.0))
                            .color(theme.text_muted.alpha(0.45)),
                    );
                }
                if !online {
                    trailing = trailing.child(
                        text("●")
                            .font(text_style(10.0, 400.0))
                            .color(theme.warning.alpha(0.8)),
                    );
                }
                menu_row(
                    theme,
                    ix,
                    "pickers/device-row",
                    is_selected,
                    ix == state.active,
                    &device.name,
                    Some(trailing),
                )
                .on_press(move |app: &mut App| space_picker::pick_device(app, id.clone()))
            }))
    };

    col()
        .w_full()
        .gap(2.0)
        .child(search_box(state, theme, "Search devices…"))
        .child(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UiConfig;
    use crate::state::App;
    use hearth_engine::registry::HarnessDescriptor;
    use hearth_proto::{
        Device, HarnessId, Model, ModelOption, ModelOptionChoice, ReasoningLevel, RepoRef,
        SandboxLevel, Space, SteeringMode,
    };

    fn descriptor() -> HarnessDescriptor {
        HarnessDescriptor {
            id: HarnessId::Huginn,
            name: "Claude Code".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            installed: true,
            enabled: Some(true),
        }
    }

    fn model() -> Model {
        Model {
            id: "opus".into(),
            label: "Opus".into(),
            description: Some("Anthropic".into()),
            reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            options: vec![ModelOption {
                id: "context".into(),
                label: "Context Window".into(),
                choices: vec![ModelOptionChoice {
                    id: "1m".into(),
                    label: "1M".into(),
                }],
                default_choice: "1m".into(),
            }],
        }
    }

    fn seeded_app() -> App {
        let mut app = App::new(UiConfig::default());
        app.pickers.install(None);
        app.local_device_id = Some("local".into());
        app.selected_device = Some("local".into());
        app.devices = vec![Device {
            id: "local".into(),
            name: "This Mac".into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }];
        let space = Space {
            id: "s1".into(),
            device_id: "local".into(),
            path: "/home/w/hearth".into(),
            name: Some("hearth".into()),
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        };
        app.selected_space = Some(space.id.clone());
        app.spaces = vec![space];
        app.pickers.harnesses = Loadable::Ready(vec![descriptor()]);
        app.pickers
            .models
            .insert(HarnessId::Huginn, Loadable::Ready(vec![model()]));
        app.pickers.refs = Loadable::Ready(vec![
            RepoRef {
                name: "main".into(),
                current: true,
                worktree_path: None,
            },
            RepoRef {
                name: "feature".into(),
                current: false,
                worktree_path: Some("/home/w/wt".into()),
            },
        ]);
        app.pickers.refs_space = Some("s1".into());
        app
    }

    #[test]
    fn every_picker_renders_its_chip_and_one_anchored_surface() {
        let app = seeded_app();
        let theme = Theme::dark();
        let state = &app.pickers;

        // The cluster + footer build without a window, and the footer exists
        // for a git space.
        let cluster = render_cluster(state, &app, &theme);
        let _ = cluster;
        assert!(render_footer(state, &app, &theme).is_some());
        let _ = render_new_session(state, &app, &theme);

        for kind in [
            PickerKind::HarnessModel,
            PickerKind::Traits,
            PickerKind::Mode,
            PickerKind::Branch,
            PickerKind::Checkout,
            PickerKind::Space,
            PickerKind::Device,
        ] {
            let mut state = seeded_app();
            state.pickers.open = Some(kind);
            let mut layer = Vec::new();
            popovers(&state.pickers, &state, &theme, &mut layer);
            assert_eq!(layer.len(), 1, "one surface for {kind:?}");
        }
    }

    #[test]
    fn popovers_handle_loading_and_error_slots() {
        let mut app = seeded_app();
        app.pickers.refs = Loadable::Loading;
        app.pickers.harnesses = Loadable::Error("boom".into());
        let theme = Theme::light();
        for kind in [
            PickerKind::HarnessModel,
            PickerKind::Traits,
            PickerKind::Branch,
            PickerKind::Checkout,
            PickerKind::Mode,
            PickerKind::Space,
            PickerKind::Device,
        ] {
            app.pickers.open = Some(kind);
            let mut layer = Vec::new();
            popovers(&app.pickers, &app, &theme, &mut layer);
            assert_eq!(layer.len(), 1, "one surface for {kind:?}");
        }
    }

    #[test]
    fn chat_config_round_trips_through_the_resolved_run_config() {
        // A concrete catalog resolves to concrete picks (no "default"
        // passthrough), and `chat_config` carries them all.
        let app = seeded_app();
        let resolved = app.pickers.resolved(&app);
        assert_eq!(resolved.harness, Some(HarnessId::Huginn));
        assert_eq!(resolved.model.as_deref(), Some("opus"));
        assert_eq!(resolved.reasoning, Some(ReasoningLevel::High));
        let config = resolved.chat_config().expect("harness");
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }
}
