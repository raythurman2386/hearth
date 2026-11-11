//! Pure decision logic: flip/auto-grow math, the Send/Steer/Stop morph,
//! pending-input detection and the question wizard reducer.
//!
//! Everything here is framework-free so the port's layout decisions are unit
//! tested without a window. Rune drives its own layout springs, so the morph
//! timeline below is the reference geometry ([`FlipMorph`] and the anchoring
//! helpers); the renderer uses the resting values and lets the pill's layout
//! spring carry the height change.

use crate::motion;
use crate::theme::Theme;
use hearth_doc::{MessagePart, MessageRole, SessionMessageEntry};
use hearth_proto::{UserInputAnswer, UserInputQuestion};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Expanded-mode textarea vertical padding: `pt-4 pb-1` (hearth composer.tsx
/// line 578) = 16 + 4.
pub const TEXTAREA_PAD_V: f32 = 20.0;
/// The expanded textarea BOX (content + padding) is clamped by the original's
/// auto-grow effect: `ta.style.height = Math.min(Math.max(scrollHeight, 76),
/// 260)` (hearth composer.tsx line 235). The 76px floor applies even when
/// empty — it's what makes the always-expanded new-chat composer tall.
pub const TEXTAREA_MIN: f32 = 76.0;
pub const TEXTAREA_MAX: f32 = 260.0;
/// Expanded actions row: `pt-1` (4) + h-8 picker chips (32 — the tallest
/// children; composer/styles.tsx pickerChip) + `pb-2.5` (10) — hearth
/// composer-actions.tsx line 60.
pub const ACTIONS_ROW_HEIGHT: f32 = 46.0;
/// The pill's 1px hairline, top + bottom (`rounded-[26px] border`).
pub const PILL_BORDER_V: f32 = 2.0;
/// Expanded composer bounds, border-box: 76 + 46 + 2 = 124 when empty (the
/// new-chat canvas), 260 + 46 + 2 = 308 at the content cap.
pub const COMPOSER_MIN_HEIGHT: f32 = TEXTAREA_MIN + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
pub const COMPOSER_MAX_HEIGHT: f32 = TEXTAREA_MAX + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
/// Compact pill, border-box: one-line textarea `py-3` (24) + one 22.75px line
/// (scrollHeight rounds to 47 in the original) + the 2px hairline = 49. The
/// compact cluster (`py-1.5` + h-8 = 44) is shorter, so the textarea wins.
pub const COMPACT_TOTAL_HEIGHT: f32 = 49.0;
/// `max-w-3xl`: stable outer width of the centered composer column.
pub const COMPOSER_MAX_WIDTH: f32 = 768.0;
/// Ignore subpixel noise when the shell reports the conversation width.
pub const COMPOSER_WIDTH_EPSILON: f32 = 0.5;
/// Below this pill input width the composer always expands.
pub const MIN_COMPACT_INPUT_WIDTH: f32 = 200.0;
/// Input text metrics: `text-[14px] leading-relaxed` = 14 × 1.625 = 22.75.
pub const INPUT_LINE_HEIGHT: f32 = 22.75;
pub const INPUT_TEXT_SIZE: f32 = 14.0;
/// Single-select questions auto-advance after this long.
pub const AUTO_ADVANCE_MS: u64 = 220;
/// The `/` command discovery debounce / retry cadence, in milliseconds.
pub const SLASH_DEBOUNCE_MS: u64 = 80;
/// Caret blink half-period (standard textarea cadence: ~500ms on / 500ms off).
/// Rune's `TextEdit` draws its own caret; kept as the reference cadence and for
/// tests (the gpui input drove its own blink loop).
pub const CARET_BLINK_MS: u64 = 500;
/// Drag-selection autoscroll runs at the display-friendly 60fps cadence.
/// Rune's `TextEdit` owns drag selection; kept for parity.
pub const DRAG_SCROLL_FRAME_MS: u64 = 16;

/// Hysteresis slack for the expanded→compact flip: once expanded, the composer
/// only collapses when the text is comfortably narrower than the compact
/// capacity — expanding and collapsing share no boundary, so a width right at
/// the flip threshold can't oscillate between the two layouts.
pub const COLLAPSE_HYSTERESIS: f32 = 32.0;
/// During an interactive resize, collapsing back to the compact mode waits
/// until the measured widths have been stable this long. Expansion remains
/// immediate so a narrowing panel never traps the controls in a compact row.
pub const RESIZE_SETTLE_MS: u64 = 150;

/// Compact↔expanded flip with hysteresis. `capacity` is the compact input
/// capacity (a layout-stable width: the pill's content box, measured from the
/// container, never the post-flip measured width — which differs per mode and
/// would feed back into the decision):
/// - a newline always expands;
/// - while `resizing`, an expanded composer stays expanded until sizes settle;
/// - a too-narrow pill (`capacity < MIN_COMPACT_INPUT_WIDTH`) always expands;
/// - compact expands only when `text_width > capacity`; expanded collapses
///   only when `text_width < capacity - COLLAPSE_HYSTERESIS`.
pub fn composer_flip(
    expanded: bool,
    text_width: f32,
    capacity: f32,
    has_newline: bool,
    resizing: bool,
) -> bool {
    if has_newline {
        return true;
    }
    if capacity < MIN_COMPACT_INPUT_WIDTH {
        return true;
    }
    if expanded {
        resizing || text_width >= capacity - COLLAPSE_HYSTERESIS
    } else {
        text_width > capacity
    }
}

pub fn composer_width_changed(previous: Option<f32>, current: f32) -> bool {
    previous.is_none_or(|previous| (current - previous).abs() > COMPOSER_WIDTH_EPSILON)
}

/// Auto-grow: content height for a wrapped-line count.
pub fn input_content_height(wrapped_lines: usize) -> f32 {
    wrapped_lines.max(1) as f32 * INPUT_LINE_HEIGHT
}

/// Total expanded composer height (border-box) for a content height: the
/// textarea BOX (content + `pt-4 pb-1`) clamps to 76–260 exactly like the
/// original's auto-grow effect, then the 46px actions row and the hairline
/// ride on top. Range 124–308.
pub fn composer_total_height(content_height: f32) -> f32 {
    (content_height + TEXTAREA_PAD_V).clamp(TEXTAREA_MIN, TEXTAREA_MAX)
        + ACTIONS_ROW_HEIGHT
        + PILL_BORDER_V
}

/// The compact pill's capacity: the pill width less `pl-4` (16) + `pr-2` (8)
/// and the 2px hairline grows. Rune's `TextEdit` reports a wrapped content
/// height, so the flip's `text_width` is derived from whether the field fits
/// on one line inside this box.
pub fn compact_input_capacity(pill_width: f32) -> f32 {
    (pill_width - 24.0 - PILL_BORDER_V).max(0.0)
}

/// The flip's `text_width` input, from the measured field content height: a
/// one-line field fits (reported comfortably under the collapse threshold); a
/// wrapped one overflows. Binary by construction — `TextEdit` measures the
/// wrap, not the raw advance width.
pub fn measured_flip_width(field_height: f32, capacity: f32) -> f32 {
    let lines = (field_height / INPUT_LINE_HEIGHT).round().max(1.0);
    if lines > 1.0 {
        capacity + 1.0
    } else {
        (capacity - COLLAPSE_HYSTERESIS - 1.0).max(0.0)
    }
}

/// The number of line boxes a measured field height covers (at least one).
pub fn measured_lines(field_height: f32) -> usize {
    ((field_height / INPUT_LINE_HEIGHT).round().max(1.0)) as usize
}

/// Caret blink phase for a time since the last keystroke/caret move: solid
/// through the first half-period (typing bursts never blink — each keystroke
/// resets the phase), then alternating.
pub fn caret_visible(ms_since_activity: u64) -> bool {
    (ms_since_activity / CARET_BLINK_MS).is_multiple_of(2)
}

/// The input's internal scroll range for a content/viewport height.
pub fn input_max_scroll(content_height: f32, viewport_height: f32) -> f32 {
    (content_height - viewport_height).max(0.0)
}

/// Apply a wheel delta to a top-origin input offset. Positive deltas mean
/// scrolling toward the start, matching Rune's scroll views.
pub fn input_scroll_offset(
    current: f32,
    delta_y: f32,
    content_height: f32,
    viewport_height: f32,
) -> f32 {
    (current - delta_y).clamp(0.0, input_max_scroll(content_height, viewport_height))
}

/// Minimally adjust the viewport so the caret row is fully visible.
pub fn input_scroll_offset_for_cursor(
    current: f32,
    cursor_top: f32,
    cursor_height: f32,
    content_height: f32,
    viewport_height: f32,
) -> f32 {
    let mut next = current;
    if cursor_top < next {
        next = cursor_top;
    } else if cursor_top + cursor_height > next + viewport_height {
        next = cursor_top + cursor_height - viewport_height;
    }
    next.clamp(0.0, input_max_scroll(content_height, viewport_height))
}

/// What a mouse press in a text field asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressIntent {
    /// Take the whole field.
    SelectAll,
    /// Grow the current selection to the pressed position.
    ExtendSelection,
    /// Put the caret at the pressed position.
    PlaceCaret,
}

impl PressIntent {
    /// Whether the press starts a drag selection. A select-all must not, or
    /// the next mouse move shrinks it back to a drag from the press position.
    pub fn arms_drag(self) -> bool {
        !matches!(self, Self::SelectAll)
    }
}

/// Read the intent from the press. Two clicks or more take the whole field,
/// and every further click keeps it.
pub fn press_intent(click_count: usize, shift: bool) -> PressIntent {
    if click_count >= 2 {
        PressIntent::SelectAll
    } else if shift {
        PressIntent::ExtendSelection
    } else {
        PressIntent::PlaceCaret
    }
}

/// Per-frame drag-selection scroll. Distance increases speed, capped at one
/// text row per frame so crossing the input boundary never causes a jump.
pub fn input_drag_scroll_delta(
    pointer_y: f32,
    viewport_top: f32,
    viewport_bottom: f32,
    line_height: f32,
) -> f32 {
    let distance = if pointer_y < viewport_top {
        pointer_y - viewport_top
    } else if pointer_y > viewport_bottom {
        pointer_y - viewport_bottom
    } else {
        return 0.0;
    };
    distance.signum() * (distance.abs() * 0.2).clamp(1.0, line_height)
}

// ---------------------------------------------------------------------------
// Flip morph (reference timeline; Rune animates the pill's layout spring)
// ---------------------------------------------------------------------------

/// Compact↔expanded flip morph: ONE committed flip starts exactly one 180ms
/// ease-out morph ([`motion::COLLAPSE_SPEC`]). The morph animates the pill's
/// COMMITTED height: the flip commits its final layout immediately (the input
/// never remounts — the caret survives) while the pill clips toward the live
/// target. The pill's bottom edge is stationary on screen, so the controls stay
/// pinned to it (constant screen-y; see the anchoring helpers below) and only
/// the text glides with the sweeping top edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlipMorph {
    /// Rendered height when the flip committed — the animation's start point.
    pub from: f32,
    /// Commit time in ms on the caller's monotonic clock.
    pub start_ms: f32,
}

impl FlipMorph {
    /// Raw timeline position 0..1 over [`motion::COLLAPSE_SPEC`]'s 180ms.
    fn raw(&self, now_ms: f32) -> f32 {
        let total = motion::COLLAPSE_SPEC.total().as_secs_f32() * 1000.0;
        ((now_ms - self.start_ms) / total).clamp(0.0, 1.0)
    }

    /// Eased progress 0..1 (ease-out).
    pub fn progress(&self, now_ms: f32) -> f32 {
        motion::COLLAPSE_SPEC.progress(self.raw(now_ms))
    }

    pub fn done(&self, now_ms: f32) -> bool {
        self.raw(now_ms) >= 1.0
    }

    /// Committed-height evaluation: eased lerp from the flip-time height to
    /// the LIVE target (auto-grow may move the target mid-morph — the morph
    /// tracks it instead of finishing on a stale height).
    pub fn height(&self, target: f32, now_ms: f32) -> f32 {
        motion::lerp(self.from, target, self.progress(now_ms))
    }
}

// -- morph anchoring --------------------------------------------------------
// The pill sits at the BOTTOM of the shell column: growing it moves its TOP
// edge; the bottom edge is stationary on screen. The controls are therefore
// pinned to the stationary bottom edge (absolute bottom row when expanded, a
// bottom-justified row when compact) and only the TEXT glides with the
// sweeping top edge.

/// Send/attach center sits 27px above the pill's outer bottom in expanded
/// mode (`pb-2.5` 10 + half the 32px content zone + 1px hairline) but 24.5px
/// in compact (centered in the 47px row) — an inherent 2.5px delta between
/// the two SOURCE geometries. The morph glides it instead of snapping.
pub const CLUSTER_Y_DELTA: f32 = 2.5;

/// The cluster's INTERNAL geometry is mode-independent. Reasoning/service
/// tier and attachment form one utility group; Send is a distinct primary
/// action. Only the wrapper's right inset differs: `pr-2` (8) compact vs
/// `px-3` (12) expanded — a whole-cluster 4px shift that glides with the morph.
pub const CLUSTER_X_DELTA: f32 = 4.0;
/// Optical join between the picker group and the paperclip.
pub const ACTION_UTILITY_GAP: f32 = 2.0;
/// Structural separation between utility actions and the primary Send action.
pub const ACTION_PRIMARY_GAP: f32 = Theme::SPACE_SM;

/// The right inset for the in-flight morph: eases from the OLD mode's resting
/// inset to the committed mode's (compact 8 ↔ expanded 12).
pub fn morph_cluster_inset(expanded: bool, progress: f32) -> f32 {
    let (from, to) = if expanded {
        (8.0, 8.0 + CLUSTER_X_DELTA)
    } else {
        (8.0 + CLUSTER_X_DELTA, 8.0)
    };
    motion::lerp(from, to, progress)
}

/// Expanded text top padding across the morph: starts at the compact resting
/// inset (12 ≈ `py-3`) and eases to `pt-4` (16).
pub fn morph_text_pad(progress: f32) -> f32 {
    motion::lerp(12.0, 16.0, progress)
}

/// Collapse-morph text glide: the committed compact row is bottom-anchored
/// (text resting top = 36px above the pill's outer bottom), while at the
/// commit instant the text sat 17px below the expanded pill's top.
pub fn collapse_text_glide(from: f32, progress: f32) -> f32 {
    (from - 53.0).max(0.0) * (1.0 - progress)
}

/// The decaying [`CLUSTER_Y_DELTA`] offset for the in-flight morph.
pub fn morph_cluster_dy(progress: f32) -> f32 {
    CLUSTER_Y_DELTA * (1.0 - progress)
}

/// Session/route changes SNAP the composer (route swaps remount in the
/// original — zero motion). Every flip committed within this wall-clock window
/// of a navigation snaps.
pub const ROUTE_SNAP_MS: u64 = 250;

/// Advance the flip morph across one render pass. While the committed mode
/// holds, the morph is kept (a finished one clears) — same-mode renders can
/// NEVER restart the animation. A committed mode change starts one morph from
/// the last rendered height, which mid-flight is the CURRENT animated height,
/// so a reverse flip hands off seamlessly instead of popping to an endpoint.
pub fn flip_morph_step(
    morph: Option<FlipMorph>,
    mode_changed: bool,
    last_height: f32,
    now_ms: f32,
    reduced_motion: bool,
    route_snap: bool,
) -> Option<FlipMorph> {
    if route_snap {
        return None;
    }
    if !mode_changed {
        return morph.filter(|m| !m.done(now_ms));
    }
    if reduced_motion || last_height <= 0.0 {
        return None;
    }
    Some(FlipMorph {
        from: last_height,
        start_ms: now_ms,
    })
}

/// Engines at or above this version understand `pending://` attachment refs
/// and QueueCommand `transfers` (send-is-a-local-write attachments). Gated on
/// BOTH the local engine and, for remotely-hosted chats, the host device's
/// stamped registry version.
pub const QUEUED_ATTACHMENTS_MIN: (u64, u64, u64) = (0, 2, 12);

/// What the send button is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendButtonMode {
    /// No live run: plain send.
    Send,
    /// Live steerable run with text typed: "Send (steers the current run)".
    Steer,
    /// Live run, nothing typed: red stop square.
    Stop,
}

/// What the composer holds that a send could carry. A staged image or diff
/// comment counts: both synthesize their own prompt body, so either alone is
/// a legal send — and during a live run has to read as Steer, not Stop.
pub fn composer_has_content(text: &str, attachments: usize, comments: usize) -> bool {
    !text.trim().is_empty() || attachments > 0 || comments > 0
}

pub fn send_button_mode(run_live: bool, has_text: bool) -> SendButtonMode {
    match (run_live, has_text) {
        (false, _) => SendButtonMode::Send,
        (true, true) => SendButtonMode::Steer,
        (true, false) => SendButtonMode::Stop,
    }
}

/// Find the unresolved input request the panel should serve, if any: an
/// unresolved input part on the LAST assistant entry — regardless of the
/// entry's run status. The question stays answerable until the user actually
/// answers it: a run that died under its question leaves an aborted entry
/// whose answer the engine delivers as a resumed turn. A newer assistant entry
/// supersedes an unanswered question. Assistant-entry-scoped, not
/// last-entry: a steer prompt sent while the agent waits appends a USER entry
/// after the streaming assistant entry.
pub fn pending_input_request(
    transcript: &[SessionMessageEntry],
) -> Option<(String, Vec<UserInputQuestion>)> {
    transcript
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::Assistant)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Input {
                    request_id,
                    questions,
                    resolved: false,
                    ..
                } => Some((request_id.clone(), questions.clone())),
                _ => None,
            })
        })
}

/// Whether the transcript shows `request_id` explicitly resolved (here or on
/// another device) — the wizard latch's release condition.
pub fn input_request_resolved(transcript: &[SessionMessageEntry], request_id: &str) -> bool {
    transcript.iter().any(|entry| {
        entry.parts.iter().any(|part| {
            matches!(
                part,
                MessagePart::Input {
                    request_id: rid,
                    resolved: true,
                    ..
                } if rid == request_id
            )
        })
    })
}

// ---------------------------------------------------------------------------
// Question wizard (pure reducer)
// ---------------------------------------------------------------------------

/// Reducer outcome of a wizard interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    Stay,
    /// Single-select landed — advance after [`AUTO_ADVANCE_MS`].
    AutoAdvance,
    /// All pages answered — submit these answers.
    Done(Vec<UserInputAnswer>),
}

/// Paged question state ("1/3"): single-select auto-advances, multi-select and
/// typed answers advance explicitly, number keys 1-9 select, Back pages back.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    pub page: usize,
    picked: Vec<Vec<usize>>,
    typed: Vec<String>,
}

impl Wizard {
    pub fn new(request_id: String, questions: Vec<UserInputQuestion>) -> Self {
        let n = questions.len();
        Self {
            request_id,
            questions,
            page: 0,
            picked: vec![Vec::new(); n],
            typed: vec![String::new(); n],
        }
    }

    pub fn counter(&self) -> String {
        format!("{}/{}", self.page + 1, self.questions.len().max(1))
    }

    pub fn current(&self) -> Option<&UserInputQuestion> {
        self.questions.get(self.page)
    }

    pub fn is_picked(&self, option_ix: usize) -> bool {
        self.picked
            .get(self.page)
            .is_some_and(|p| p.contains(&option_ix))
    }

    /// Whether the current page has any picked option.
    pub fn page_has_pick(&self) -> bool {
        self.picked.get(self.page).is_some_and(|p| !p.is_empty())
    }

    /// Click/tap an option.
    pub fn select(&mut self, option_ix: usize) -> WizardStep {
        let Some(question) = self.questions.get(self.page) else {
            return WizardStep::Stay;
        };
        if option_ix >= question.options.len() {
            return WizardStep::Stay;
        }
        let multi = question.multi_select;
        let Some(picked) = self.picked.get_mut(self.page) else {
            return WizardStep::Stay;
        };
        if multi {
            match picked.iter().position(|&p| p == option_ix) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(option_ix),
            }
            WizardStep::Stay
        } else {
            *picked = vec![option_ix];
            WizardStep::AutoAdvance
        }
    }

    /// Number key 1-9.
    pub fn press_number(&mut self, number: usize) -> WizardStep {
        if number == 0 {
            return WizardStep::Stay;
        }
        self.select(number - 1)
    }

    pub fn set_typed(&mut self, text: String) {
        if let Some(slot) = self.typed.get_mut(self.page) {
            *slot = text;
        }
    }

    /// Explicit submit / auto-advance landing.
    pub fn advance(&mut self) -> WizardStep {
        if self.page + 1 < self.questions.len() {
            self.page += 1;
            WizardStep::Stay
        } else {
            WizardStep::Done(self.answers())
        }
    }

    /// Page back; false when already on the first page.
    pub fn back(&mut self) -> bool {
        if self.page > 0 {
            self.page -= 1;
            true
        } else {
            false
        }
    }

    /// Answers per question: free text overrides picked labels.
    pub fn answers(&self) -> Vec<UserInputAnswer> {
        self.questions
            .iter()
            .enumerate()
            .map(|(ix, q)| {
                let typed = self.typed.get(ix).map(|s| s.trim()).unwrap_or("");
                let labels = if !typed.is_empty() {
                    vec![typed.to_string()]
                } else {
                    self.picked
                        .get(ix)
                        .map(|picked| {
                            picked
                                .iter()
                                .filter_map(|&p| q.options.get(p).cloned())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                UserInputAnswer {
                    question_id: q.id.clone(),
                    labels,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_outer_width_only_schedules_reflow_on_real_changes() {
        assert!(composer_width_changed(None, 400.0));
        assert!(!composer_width_changed(Some(400.0), 400.0));
        assert!(!composer_width_changed(Some(400.0), 400.5));
        assert!(composer_width_changed(Some(400.0), 400.51));
    }

    fn question(id: &str, options: &[&str], multi: bool) -> UserInputQuestion {
        UserInputQuestion {
            id: id.into(),
            header: "Header".into(),
            question: format!("Question {id}"),
            options: options.iter().map(|s| s.to_string()).collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn flip_decision() {
        // Fits in the pill → compact stays compact.
        assert!(!composer_flip(false, 150.0, 300.0, false, false));
        // Overflow → expand.
        assert!(composer_flip(false, 320.0, 300.0, false, false));
        // Newline always expands (either mode, even mid-resize).
        assert!(composer_flip(false, 10.0, 300.0, true, false));
        assert!(composer_flip(true, 10.0, 300.0, true, true));
        // Narrow column (< MIN_COMPACT_INPUT_WIDTH) always expands.
        assert!(composer_flip(false, 10.0, 199.0, false, false));
        assert!(!composer_flip(false, 10.0, 200.0, false, false));
    }

    #[test]
    fn flip_hysteresis_band_prevents_oscillation() {
        let cap = 300.0;
        // Text just over capacity expands…
        assert!(composer_flip(false, cap + 1.0, cap, false, false));
        // …and the SAME width, now expanded, does NOT collapse back.
        assert!(composer_flip(true, cap + 1.0, cap, false, false));
        // Anywhere inside the band the two modes are both stable.
        let in_band = cap - COLLAPSE_HYSTERESIS + 1.0;
        assert!(!composer_flip(false, in_band, cap, false, false));
        assert!(composer_flip(true, in_band, cap, false, false));
        // Comfortably under the band → collapses.
        assert!(!composer_flip(
            true,
            cap - COLLAPSE_HYSTERESIS - 1.0,
            cap,
            false,
            false
        ));
    }

    #[test]
    fn resize_expands_live_but_defers_collapse() {
        // A compact composer expands immediately as its text stops fitting.
        assert!(composer_flip(false, 500.0, 300.0, false, true));
        assert!(composer_flip(false, 10.0, 150.0, false, true));
        // An expanded composer waits for the drag to settle before collapsing.
        assert!(composer_flip(true, 0.0, 300.0, false, true));
        // Once settled, the same wide layout may collapse.
        assert!(composer_flip(false, 500.0, 300.0, false, false));
        assert!(!composer_flip(true, 0.0, 300.0, false, false));
        assert!(composer_flip(false, 10.0, 150.0, false, false));
    }

    #[test]
    fn caret_blink_phase() {
        // Solid through the first half-period (typing burst never blinks).
        assert!(caret_visible(0));
        assert!(caret_visible(CARET_BLINK_MS - 1));
        // Off for the second half-period, back on for the third.
        assert!(!caret_visible(CARET_BLINK_MS));
        assert!(!caret_visible(2 * CARET_BLINK_MS - 1));
        assert!(caret_visible(2 * CARET_BLINK_MS));
    }

    #[test]
    fn input_wheel_scroll_uses_scroll_view_direction_and_clamps() {
        // Positive wheel delta moves toward the start; negative moves down.
        assert_eq!(input_scroll_offset(40.0, 20.0, 200.0, 100.0), 20.0);
        assert_eq!(input_scroll_offset(40.0, -30.0, 200.0, 100.0), 70.0);
        // Neither edge can be overscrolled.
        assert_eq!(input_scroll_offset(10.0, 50.0, 200.0, 100.0), 0.0);
        assert_eq!(input_scroll_offset(90.0, -50.0, 200.0, 100.0), 100.0);
        // Short content has no internal scroll range.
        assert_eq!(input_scroll_offset(20.0, -50.0, 80.0, 100.0), 0.0);
    }

    #[test]
    fn input_scroll_reveals_only_when_caret_leaves_viewport() {
        // A visible caret preserves the user's viewport.
        assert_eq!(
            input_scroll_offset_for_cursor(40.0, 60.0, 20.0, 300.0, 100.0),
            40.0
        );
        // Moving above or below reveals the row with the smallest adjustment.
        assert_eq!(
            input_scroll_offset_for_cursor(80.0, 30.0, 20.0, 300.0, 100.0),
            30.0
        );
        assert_eq!(
            input_scroll_offset_for_cursor(20.0, 130.0, 20.0, 300.0, 100.0),
            50.0
        );
        // Revealing the final row clamps exactly to the content end.
        assert_eq!(
            input_scroll_offset_for_cursor(0.0, 290.0, 20.0, 300.0, 100.0),
            200.0
        );
    }

    #[test]
    fn input_drag_autoscroll_is_edge_proportional_and_capped() {
        let top = 100.0;
        let bottom = 300.0;
        let line = INPUT_LINE_HEIGHT;
        assert_eq!(input_drag_scroll_delta(200.0, top, bottom, line), 0.0);
        assert_eq!(input_drag_scroll_delta(90.0, top, bottom, line), -2.0);
        assert_eq!(input_drag_scroll_delta(315.0, top, bottom, line), 3.0);
        assert_eq!(input_drag_scroll_delta(-100.0, top, bottom, line), -line);
        assert_eq!(input_drag_scroll_delta(500.0, top, bottom, line), line);
    }

    #[test]
    fn press_intent_selects_the_whole_field_on_double_click() {
        assert_eq!(press_intent(1, false), PressIntent::PlaceCaret);
        assert_eq!(press_intent(1, true), PressIntent::ExtendSelection);
        assert_eq!(press_intent(2, false), PressIntent::SelectAll);
        assert_eq!(press_intent(2, true), PressIntent::SelectAll);
        assert_eq!(press_intent(3, false), PressIntent::SelectAll);
        assert!(press_intent(1, true).arms_drag());
        assert!(!press_intent(2, false).arms_drag());
    }

    #[test]
    fn auto_grow_math() {
        assert_eq!(COMPOSER_MIN_HEIGHT, 124.0);
        assert_eq!(COMPOSER_MAX_HEIGHT, 308.0);
        assert_eq!(
            composer_total_height(input_content_height(1)),
            COMPOSER_MIN_HEIGHT
        );
        // Growth is linear once the textarea box exceeds its 76px floor.
        let h4 = composer_total_height(input_content_height(4));
        assert_eq!(
            h4,
            4.0 * INPUT_LINE_HEIGHT + TEXTAREA_PAD_V + ACTIONS_ROW_HEIGHT + PILL_BORDER_V
        );
        assert_eq!(
            composer_total_height(input_content_height(100)),
            COMPOSER_MAX_HEIGHT
        );
        assert_eq!(input_content_height(0), INPUT_LINE_HEIGHT);
    }

    #[test]
    fn measured_flip_width_is_binary_across_the_wrap() {
        let cap = 300.0;
        // One line → comfortably under the collapse threshold.
        assert!(!composer_flip(
            false,
            measured_flip_width(INPUT_LINE_HEIGHT, cap),
            cap,
            false,
            false
        ));
        assert!(!composer_flip(
            true,
            measured_flip_width(INPUT_LINE_HEIGHT, cap),
            cap,
            false,
            false
        ));
        // Two wrapped lines → over capacity, sticks expanded.
        assert!(composer_flip(
            false,
            measured_flip_width(2.0 * INPUT_LINE_HEIGHT, cap),
            cap,
            false,
            false
        ));
        assert!(composer_flip(
            true,
            measured_flip_width(2.0 * INPUT_LINE_HEIGHT, cap),
            cap,
            false,
            false
        ));
        assert_eq!(measured_lines(0.0), 1);
        assert_eq!(measured_lines(2.0 * INPUT_LINE_HEIGHT + 1.0), 2);
    }

    #[test]
    fn compact_capacity_subtracts_the_horizontal_padding() {
        assert_eq!(compact_input_capacity(768.0), 768.0 - 26.0);
        assert_eq!(compact_input_capacity(10.0), 0.0);
    }

    /// One frame short of the full morph timeline (never rounds up to done).
    const ALMOST: f32 = 179.0;

    #[test]
    fn flip_morph_starts_once_per_committed_flip() {
        // No committed flip → no morph.
        assert_eq!(flip_morph_step(None, false, 49.0, 0.0, false, false), None);
        // A committed flip starts one, from the last rendered height…
        let m = flip_morph_step(None, true, 49.0, 100.0, false, false).unwrap();
        assert_eq!(m.from, 49.0);
        assert_eq!(m.start_ms, 100.0);
        // …and same-mode renders keep it UNCHANGED.
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 150.0, false, false),
            Some(m)
        );
        // A finished morph clears on the next same-mode render.
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 100.0 + ALMOST, false, false),
            Some(m)
        );
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 300.0, false, false),
            None
        );
    }

    #[test]
    fn flip_morph_height_ramps_monotonically_to_target() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        let mut prev = m.height(124.0, 0.0);
        assert_eq!(prev, 49.0);
        for step in 1..=18 {
            let h = m.height(124.0, step as f32 * 10.0);
            assert!(h >= prev, "height regressed at {step}: {h} < {prev}");
            prev = h;
        }
        assert_eq!(m.height(124.0, 180.0), 124.0);
        assert!(m.done(180.0));
        assert_eq!(m.height(124.0, 500.0), 124.0);
        let down = FlipMorph {
            from: 124.0,
            start_ms: 0.0,
        };
        assert!(down.height(49.0, 90.0) < 124.0);
        assert!(down.height(49.0, 90.0) > 49.0);
    }

    #[test]
    fn flip_morph_reverse_hands_off_from_current_height() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        let mid = m.height(124.0, 90.0);
        assert!(mid > 49.0 && mid < 124.0);
        let rev = flip_morph_step(Some(m), true, mid, 90.0, false, false).unwrap();
        assert_eq!(rev.from, mid);
        assert_eq!(rev.height(49.0, 90.0), mid);
    }

    #[test]
    fn flip_morph_snaps_for_reduced_motion_and_first_paint() {
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, true, false), None);
        assert_eq!(flip_morph_step(None, true, 0.0, 0.0, false, false), None);
    }

    #[test]
    fn route_change_never_arms_the_morph() {
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, false, true), None);
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 50.0, false, true),
            None
        );
        assert_eq!(
            flip_morph_step(Some(m), true, 80.0, 50.0, false, true),
            None
        );
        let armed = flip_morph_step(None, true, 49.0, 300.0, false, false).unwrap();
        assert_eq!(armed.from, 49.0);
    }

    #[test]
    fn morph_anchoring_holds_controls_and_glides_text() {
        assert_eq!(morph_cluster_dy(1.0), 0.0);
        assert_eq!(morph_text_pad(1.0), 16.0);
        assert_eq!(collapse_text_glide(124.0, 1.0), 0.0);
        assert_eq!(morph_text_pad(0.0), 12.0);
        assert_eq!(morph_cluster_dy(0.0), CLUSTER_Y_DELTA);
        assert_eq!(collapse_text_glide(124.0, 0.0), 71.0);
        let mut prev = collapse_text_glide(124.0, 0.0);
        for step in 1..=10 {
            let g = collapse_text_glide(124.0, step as f32 / 10.0);
            assert!(g <= prev, "glide regressed at {step}");
            prev = g;
        }
        assert_eq!(collapse_text_glide(50.0, 0.0), 0.0);
    }

    #[test]
    fn cluster_inset_glides_between_the_source_endpoints() {
        assert_eq!(ACTION_UTILITY_GAP, 2.0);
        assert_eq!(ACTION_PRIMARY_GAP, Theme::SPACE_SM);
        const { assert!(ACTION_UTILITY_GAP < ACTION_PRIMARY_GAP) };
        assert_eq!(morph_cluster_inset(true, 0.0), 8.0);
        assert_eq!(morph_cluster_inset(true, 1.0), 12.0);
        assert_eq!(morph_cluster_inset(false, 0.0), 12.0);
        assert_eq!(morph_cluster_inset(false, 1.0), 8.0);
        let mut prev = morph_cluster_inset(true, 0.0);
        for step in 1..=10 {
            let v = morph_cluster_inset(true, step as f32 / 10.0);
            assert!(v >= prev && v <= 8.0 + CLUSTER_X_DELTA);
            prev = v;
        }
    }

    #[test]
    fn flip_morph_tracks_live_target_and_drives_fade() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        assert!(m.height(159.0, 90.0) > m.height(124.0, 90.0));
        assert_eq!(m.progress(0.0), 0.0);
        assert_eq!(m.progress(180.0), 1.0);
        let mid = m.progress(90.0);
        assert!(mid > 0.0 && mid < 1.0);
    }

    #[test]
    fn staged_comments_alone_are_content() {
        assert!(!composer_has_content("   ", 0, 0));
        assert!(composer_has_content("hi", 0, 0));
        assert!(composer_has_content("", 1, 0));
        assert!(composer_has_content("", 0, 1));
    }

    #[test]
    fn send_button_morph() {
        assert_eq!(send_button_mode(false, false), SendButtonMode::Send);
        assert_eq!(send_button_mode(false, true), SendButtonMode::Send);
        assert_eq!(send_button_mode(true, true), SendButtonMode::Steer);
        assert_eq!(send_button_mode(true, false), SendButtonMode::Stop);
    }

    #[test]
    fn wizard_single_select_auto_advances_and_completes() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a", "b"], false),
                question("q2", &["x"], false),
            ],
        );
        assert_eq!(w.counter(), "1/2");
        assert_eq!(w.select(1), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.advance(), WizardStep::Stay);
        assert_eq!(w.counter(), "2/2");
        assert_eq!(w.select(0), WizardStep::AutoAdvance);
        let WizardStep::Done(answers) = w.advance() else {
            panic!("expected Done")
        };
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].labels, vec!["b"]);
        assert_eq!(answers[1].labels, vec!["x"]);
    }

    #[test]
    fn wizard_multi_select_toggles_and_stays() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b", "c"], true)]);
        assert_eq!(w.select(0), WizardStep::Stay);
        assert_eq!(w.select(2), WizardStep::Stay);
        assert!(w.is_picked(0) && w.is_picked(2));
        assert_eq!(w.select(0), WizardStep::Stay);
        assert!(!w.is_picked(0));
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["c"]);
    }

    #[test]
    fn wizard_number_keys_and_bounds() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b"], false)]);
        assert_eq!(w.press_number(9), WizardStep::Stay, "out of range ignored");
        assert_eq!(w.press_number(0), WizardStep::Stay);
        assert_eq!(w.press_number(2), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.select(5), WizardStep::Stay, "bad option ix ignored");
    }

    #[test]
    fn wizard_typed_answer_overrides_and_back_pages() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a"], false),
                question("q2", &["x", "y"], false),
            ],
        );
        w.select(0);
        w.advance();
        assert_eq!(w.page, 1);
        assert!(w.back());
        assert_eq!(w.page, 0);
        assert!(!w.back(), "already at first page");
        w.advance();
        w.set_typed("  custom answer  ".into());
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["a"]);
        assert_eq!(
            answers[1].labels,
            vec!["custom answer"],
            "typed overrides picked, trimmed"
        );
    }

    #[test]
    fn pending_input_detection() {
        use hearth_doc::MessageStatus;
        let input_part = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![question("q", &["a"], false)],
            resolved: false,
        };
        let entry = |status: Option<MessageStatus>, parts: Vec<MessagePart>| SessionMessageEntry {
            id: "m".into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status,
            continuation_of: None,
        };
        // Streaming entry with unresolved input → panel.
        let t = vec![entry(
            Some(MessageStatus::Streaming),
            vec![input_part.clone()],
        )];
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into())
        );
        // DEAD entry with an unresolved input STILL gets the panel.
        let t = vec![entry(
            Some(MessageStatus::Aborted),
            vec![input_part.clone()],
        )];
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into())
        );
        // A NEWER assistant entry supersedes an unanswered question.
        let t = vec![
            entry(Some(MessageStatus::Aborted), vec![input_part.clone()]),
            SessionMessageEntry {
                id: "m2".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "t2".into(),
                    text: "moved on".into(),
                }],
                created_at: 2,
                device_id: "d".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            },
        ];
        assert!(pending_input_request(&t).is_none());
        // Resolved part → no panel.
        let resolved = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![],
            resolved: true,
        };
        let t = vec![entry(
            Some(MessageStatus::Streaming),
            vec![resolved.clone()],
        )];
        assert!(pending_input_request(&t).is_none());
        assert!(pending_input_request(&[]).is_none());

        // A steer prompt appends a USER entry AFTER the streaming assistant
        // entry — the question must still be found.
        let user_echo = SessionMessageEntry {
            id: "u2".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t".into(),
                text: "I answered".into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        };
        let t = vec![
            entry(Some(MessageStatus::Streaming), vec![input_part.clone()]),
            user_echo,
        ];
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into()),
            "question survives entries appended behind the streaming entry"
        );

        assert!(!input_request_resolved(&t, "r1"));
        let t = vec![entry(Some(MessageStatus::Streaming), vec![resolved])];
        assert!(input_request_resolved(&t, "r1"));
        assert!(!input_request_resolved(&t, "other"));
    }
}
