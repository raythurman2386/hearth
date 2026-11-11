//! MessageRail: a left vertical minimap of the user's prompts. The active tick
//! brightens, hover grows the tick and shows a preview card (prompt + reply
//! opening), click glides the transcript to that row. Hidden below two ticks.
//!
//! Pure logic (tick extraction, active detection, bucketing, previews) lives in
//! free functions with unit tests; rendering reads [`crate::state::App`] and
//! uses `Tx::scroll_into_view`, so no per-frame spring state is needed.

use std::time::Duration;

use rune::{El, TextStyle, col, el, row, text};

use hearth_doc::{MessagePart, MessageRole, SessionMessageEntry};

use crate::state::App;
use crate::theme::Theme;

/// 48rem — the container width below which the rail (and wide gutters) collapse.
pub const RAIL_MIN_CONTAINER_WIDTH: f32 = 768.0;

pub fn rail_visible(container_width: f32) -> bool {
    container_width >= RAIL_MIN_CONTAINER_WIDTH
}

/// Preview text caps.
pub const PREVIEW_PROMPT_CHARS: usize = 160;
pub const PREVIEW_REPLY_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct RailTick {
    /// Message id — equals the user row's id in the transcript row model.
    pub message_id: String,
    pub prompt: String,
    pub reply: Option<String>,
}

fn user_text(entry: &SessionMessageEntry) -> String {
    let raw = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    crate::attachments::user_message_rail_text(&raw)
}

fn first_reply_text(entries: &[SessionMessageEntry]) -> Option<String> {
    entries
        .iter()
        .find(|entry| entry.role == MessageRole::Assistant)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Text { text, .. } if !text.trim().is_empty() => {
                    Some(text.trim().to_string())
                }
                _ => None,
            })
        })
}

/// Extract rail ticks from the transcript: one per user entry (doc entries
/// first, then unconfirmed echoes — matching transcript row order).
pub fn rail_ticks(
    entries: &[SessionMessageEntry],
    echoes: &[SessionMessageEntry],
) -> Vec<RailTick> {
    let mut ticks: Vec<RailTick> = Vec::new();
    for (ix, entry) in entries.iter().enumerate() {
        if entry.role != MessageRole::User {
            continue;
        }
        ticks.push(RailTick {
            message_id: entry.id.clone(),
            prompt: user_text(entry),
            reply: first_reply_text(&entries[ix + 1..]),
        });
    }
    for echo in echoes {
        if echo.role == MessageRole::User && !ticks.iter().any(|tick| tick.message_id == echo.id) {
            ticks.push(RailTick {
                message_id: echo.id.clone(),
                prompt: user_text(echo),
                reply: None,
            });
        }
    }
    ticks
}

/// The active tick for a scroll position: the last tick whose transcript row is
/// at or above the viewport-top row.
pub fn active_tick(tick_rows: &[usize], top_row: usize) -> Option<usize> {
    if tick_rows.is_empty() {
        return None;
    }
    match tick_rows.iter().rposition(|&row| row <= top_row) {
        Some(ix) => Some(ix),
        None => Some(0),
    }
}

// ---------------------------------------------------------------------------
// Fixed-footprint outline
// ---------------------------------------------------------------------------

/// One tick's hit-row height and the gap between ticks.
pub const TICK_SLOT: f32 = 10.0;
pub const TICK_GAP: f32 = 3.0;
/// Vertical breathing room kept clear above/below the tick stack.
pub const RAIL_V_MARGIN: f32 = 24.0;
/// Hard cap on visible ticks.
pub const MAX_RAIL_TICKS: usize = 12;

/// How many tick slots fit in a rail of `height` px (always ≥ 1).
pub fn rail_capacity(height: f32) -> usize {
    let usable = (height - 2.0 * RAIL_V_MARGIN).max(TICK_SLOT);
    (((usable + TICK_GAP) / (TICK_SLOT + TICK_GAP)).floor() as usize).max(1)
}

/// Slots the rail actually uses, hard-capped at [`MAX_RAIL_TICKS`].
pub fn rail_slots(height: f32) -> usize {
    rail_capacity(height).min(MAX_RAIL_TICKS)
}

/// Evenly-sized buckets over the conversation when prompts outnumber slots.
pub fn tick_buckets(n: usize, capacity: usize) -> Vec<(usize, usize)> {
    if n == 0 {
        return Vec::new();
    }
    let cap = capacity.clamp(1, n);
    (0..cap).map(|k| (k * n / cap, (k + 1) * n / cap)).collect()
}

/// The bucket containing tick `ix`.
pub fn bucket_of(buckets: &[(usize, usize)], ix: usize) -> Option<usize> {
    buckets
        .iter()
        .position(|&(start, end)| ix >= start && ix < end)
}

/// Char-cap a preview with an ellipsis; whitespace runs collapse to one space.
pub fn truncate_preview(text: &str, max_chars: usize) -> String {
    let flat = hearth_proto::view::single_line(text);
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

// ---------------------------------------------------------------------------
// Duration-based glide timeline (pure)
// ---------------------------------------------------------------------------

/// Fraction of the CURRENT remaining distance to consume for eased progress
/// `eased`. The Rune runtime owns the actual glide; this is kept because the
/// bucket/preview tests and callers use the same easing contract.
#[derive(Debug, Clone)]
pub struct GlideTimeline {
    eased_prev: f32,
}

impl Default for GlideTimeline {
    fn default() -> Self {
        Self::new()
    }
}

impl GlideTimeline {
    pub fn new() -> Self {
        Self { eased_prev: 0.0 }
    }

    pub fn step(&mut self, eased: f32) -> f32 {
        let eased = eased.clamp(self.eased_prev, 1.0);
        let denom = 1.0 - self.eased_prev;
        let frac = if denom <= 1e-6 {
            1.0
        } else {
            (eased - self.eased_prev) / denom
        };
        self.eased_prev = eased;
        frac.clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the rail overlay. `pairs` maps each tick to its transcript row key;
/// `active` is the active tick index; `hover` comes from transcript state.
pub fn render(
    app: &App,
    theme: &Theme,
    pairs: &[(RailTick, String)],
    active: Option<usize>,
    viewport_height: f32,
) -> El {
    if pairs.len() < 2 {
        return el();
    }
    let capacity = rail_slots(if viewport_height > 0.0 {
        viewport_height
    } else {
        600.0
    });
    let buckets = tick_buckets(pairs.len(), capacity);
    let active_bucket = active.and_then(|ix| bucket_of(&buckets, ix));
    let hover = app.transcript_state.rail_hover;

    let mut rail = col()
        .key("app/transcript/rail")
        .pin(Some(16.0), Some(0.0), None, Some(0.0))
        .w(26.0)
        .gap(TICK_GAP)
        .items_start()
        .justify_center();

    for (ix, &(start, end)) in buckets.iter().enumerate() {
        let rep = active.filter(|&a| a >= start && a < end).unwrap_or(start);
        let Some((tick, row_key)) = pairs.get(rep) else {
            continue;
        };
        let bucket_len = end - start;
        let is_active = active_bucket == Some(ix);
        let is_hovered = hover == Some(ix);
        let bar_width = if is_hovered { 20.0 } else { 12.0 };
        let bar_color = if is_active || is_hovered {
            theme.text.alpha(0.8)
        } else {
            theme.ink(0.16)
        };
        let prompt = truncate_preview(&tick.prompt, PREVIEW_PROMPT_CHARS);
        let reply = tick
            .reply
            .as_deref()
            .map(|reply| truncate_preview(reply, PREVIEW_REPLY_CHARS));
        let key = row_key.clone();
        let mut slot = row()
            .h(TICK_SLOT)
            .w_full()
            .items_center()
            .cursor(rune::Cursor::Pointer)
            .on_hover_change(move |app: &mut App, inside, _tx| {
                let next = if inside {
                    Some(ix)
                } else if app.transcript_state.rail_hover == Some(ix) {
                    None
                } else {
                    app.transcript_state.rail_hover
                };
                app.transcript_state.rail_hover = next;
            })
            .on_press_tx(move |app: &mut App, tx| {
                // Explicit navigation releases a held own-turn prompt first.
                app.transcript_state.release_own_turn();
                tx.scroll_into_view(key.clone());
            })
            .child(el().h(2.0).w(bar_width).radius(1.0).fill(bar_color));
        if is_hovered {
            let mut card = col()
                .pin(Some(26.0), Some(-6.0), None, None)
                .w(280.0)
                .padding(Theme::SPACE_SM)
                .gap(6.0)
                .radius(12.0)
                .fill(theme.surface_overlay)
                .border(1.0, theme.border)
                .shadow(8.0, 24.0, theme.ink(0.24))
                .child(
                    text(prompt)
                        .font(TextStyle::new(12.0, 400.0))
                        .color(theme.text)
                        .wrap(),
                );
            if let Some(reply) = reply {
                card = card.child(
                    text(reply)
                        .font(TextStyle::new(11.0, 400.0))
                        .color(theme.text_muted)
                        .wrap(),
                );
            }
            if bucket_len > 1 {
                card = card.child(
                    text(format!("{bucket_len} prompts"))
                        .font(TextStyle::new(10.0, 400.0))
                        .color(theme.text_muted.alpha(0.7)),
                );
            }
            slot = slot.child(card);
        }
        rail = rail.child(slot);
    }
    rail
}

/// The rail's scroll glide duration, matching the gpui `SCROLL_GLIDE`.
pub const SCROLL_GLIDE: Duration = Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_doc::MessageStatus;

    fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn capacity_counts_slots_that_fit() {
        assert_eq!(rail_capacity(880.0), 64);
        assert_eq!(rail_capacity(0.0), 1);
        assert!(rail_capacity(200.0) >= 10);
        assert_eq!(rail_slots(880.0), MAX_RAIL_TICKS);
        assert_eq!(rail_slots(2000.0), MAX_RAIL_TICKS);
        assert!(rail_slots(100.0) < MAX_RAIL_TICKS);
    }

    #[test]
    fn buckets_are_identity_under_capacity() {
        let buckets = tick_buckets(5, 64);
        assert_eq!(buckets.len(), 5);
        assert!(
            buckets
                .iter()
                .enumerate()
                .all(|(k, &(start, end))| start == k && end == k + 1)
        );
    }

    #[test]
    fn buckets_partition_evenly_over_capacity() {
        let n = 100;
        let buckets = tick_buckets(n, 8);
        assert_eq!(buckets.len(), 8);
        assert_eq!(buckets[0].0, 0);
        assert_eq!(buckets.last().unwrap().1, n);
        for window in buckets.windows(2) {
            assert_eq!(window[0].1, window[1].0, "contiguous");
        }
        for &(start, end) in &buckets {
            assert!((end - start) == 12 || (end - start) == 13);
        }
    }

    #[test]
    fn bucket_of_maps_ticks_to_their_bucket() {
        let buckets = tick_buckets(10, 3);
        assert_eq!(bucket_of(&buckets, 0), Some(0));
        assert_eq!(bucket_of(&buckets, 3), Some(1));
        assert_eq!(bucket_of(&buckets, 9), Some(2));
        assert_eq!(bucket_of(&buckets, 10), None);
        assert!(tick_buckets(0, 8).is_empty());
        assert_eq!(tick_buckets(3, 0), vec![(0, 3)]);
    }

    #[test]
    fn ticks_map_user_prompts_with_reply_openings() {
        let entries = vec![
            entry("u1", MessageRole::User, "first question"),
            entry("a1", MessageRole::Assistant, "first answer"),
            entry("u2", MessageRole::User, "second question"),
            entry("a2", MessageRole::Assistant, "second answer"),
        ];
        let ticks = rail_ticks(&entries, &[]);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].message_id, "u1");
        assert_eq!(ticks[0].prompt, "first question");
        assert_eq!(ticks[0].reply.as_deref(), Some("first answer"));
        assert_eq!(ticks[1].reply.as_deref(), Some("second answer"));
    }

    #[test]
    fn ticks_include_echoes_deduped() {
        let entries = vec![entry("u1", MessageRole::User, "sent")];
        let echoes = vec![
            entry("u1", MessageRole::User, "sent"),
            entry("u2", MessageRole::User, "pending"),
        ];
        let ticks = rail_ticks(&entries, &echoes);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[1].message_id, "u2");
        assert_eq!(ticks[1].reply, None);
    }

    #[test]
    fn tick_without_reply_yet() {
        let entries = vec![
            entry("u1", MessageRole::User, "q"),
            entry("a1", MessageRole::Assistant, "reply to first"),
            entry("u2", MessageRole::User, "latest"),
        ];
        let ticks = rail_ticks(&entries, &[]);
        assert_eq!(ticks[1].reply, None);
        assert!(rail_ticks(&[], &[]).is_empty());
    }

    #[test]
    fn active_tick_tracks_viewport_top() {
        let tick_rows = [0, 5, 9];
        assert_eq!(active_tick(&tick_rows, 0), Some(0));
        assert_eq!(active_tick(&tick_rows, 4), Some(0));
        assert_eq!(active_tick(&tick_rows, 5), Some(1));
        assert_eq!(active_tick(&tick_rows, 8), Some(1));
        assert_eq!(active_tick(&tick_rows, 100), Some(2));
        assert_eq!(active_tick(&[3, 7], 1), Some(0));
        assert_eq!(active_tick(&[], 4), None);
    }

    #[test]
    fn rail_width_gate() {
        assert!(rail_visible(768.0));
        assert!(rail_visible(1200.0));
        assert!(!rail_visible(767.9));
        assert!(!rail_visible(0.0));
    }

    #[test]
    fn glide_timeline_matches_absolute_eased_interpolation() {
        let mut timeline = GlideTimeline::new();
        let (start, target) = (1000.0f32, 0.0f32);
        let mut pos = start;
        for i in 1..=60 {
            let t = i as f32 / 60.0;
            let eased = t * t * (3.0 - 2.0 * t); // smoothstep, the eased shape
            let frac = timeline.step(eased);
            pos -= frac * (pos - target);
            let absolute = start + eased * (target - start);
            assert!(
                (pos - absolute).abs() < 0.05,
                "frame {i}: pos {pos} != absolute {absolute}"
            );
        }
        assert_eq!(pos, target);
    }

    #[test]
    fn glide_timeline_step_clamps() {
        let mut timeline = GlideTimeline::new();
        assert_eq!(timeline.step(0.4), 0.4);
        assert_eq!(timeline.step(0.3), 0.0);
        assert_eq!(timeline.step(1.0), 1.0);
        assert_eq!(timeline.step(1.0), 1.0);
    }

    #[test]
    fn preview_truncation() {
        assert_eq!(truncate_preview("short", 10), "short");
        assert_eq!(truncate_preview("  padded  ", 10), "padded");
        let long = "x".repeat(50);
        let cut = truncate_preview(&long, 10);
        assert!(cut.chars().count() <= 10);
        assert!(cut.ends_with('…'));
        let uni = "héllo wörld attaché case overflowing";
        let cut = truncate_preview(uni, 12);
        assert!(cut.ends_with('…'));
    }
}
