use super::Transcript;
use super::model::Row;
use super::spring::{
    AT_BOTTOM_PX, GLIDE_MAX_VIEWPORTS, SCROLL_BUTTON_THRESHOLD_PX, SPRING_FRAME_MS,
    SPRING_MAX_CATCHUP_FRAMES,
};
use crate::markdown::render;
use crate::motion;
use crate::theme::Theme;
use gpui::{
    Bounds, ClipboardItem, Context, ListOffset, ListScrollEvent, ListState, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, SharedString, Task, Window, px,
};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// Re-engage the bottom pin when the user returns within this many px of the end.
pub const STICK_THRESHOLD_PX: f32 = 70.0;

/// Bound session-local viewport memory independently of total chat history.
pub(super) const MAX_SAVED_VIEWPORTS: usize = 256;

/// Text-selection edge scrolling runs only during a drag. A 24 ms cadence is
/// smooth enough to track text while avoiding a permanent animation-frame loop
/// on low-end devices.
const SELECTION_SCROLL_TICK_MS: u64 = 24;

const SELECTION_SCROLL_EDGE_PX: f32 = 36.0;

const SELECTION_SCROLL_MAX_STEP_PX: f32 = 24.0;

/// Signed list scroll step for a pointer near a viewport edge.
///
/// GPUI list offsets increase toward the document bottom. The quadratic ramp
/// keeps entry into the edge zone gentle and reaches full speed at the edge.
pub(super) fn selection_scroll_step(bounds: Bounds<Pixels>, position: Point<Pixels>) -> f32 {
    let height = f32::from(bounds.size.height);
    if height <= 0.0 {
        return 0.0;
    }
    let edge = SELECTION_SCROLL_EDGE_PX.min(height / 3.0);
    if edge <= 0.0 {
        return 0.0;
    }
    let y = f32::from(position.y);
    let top = f32::from(bounds.top());
    let bottom = f32::from(bounds.bottom());
    let scaled = |penetration: f32| {
        let t = (penetration / edge).clamp(0.0, 1.0);
        SELECTION_SCROLL_MAX_STEP_PX * t * t
    };
    if y < top + edge {
        -scaled(top + edge - y)
    } else if y > bottom - edge {
        scaled(y - (bottom - edge))
    } else {
        0.0
    }
}

/// A freshly-sent prompt rests this far below the transcript viewport's top.
/// The titlebar overlays the full-height list, so its height is part of the
/// inset; the extra 10px matches the first row's breathing room.
pub(crate) const OWN_SEND_TOP_INSET_PX: f32 = Theme::TITLEBAR_HEIGHT + 10.0;

/// Epsilon of extra height under the reservation. The runway ends AT the
/// app's bottom — this is not scroll room (24px of it read as a janky
/// overshoot-and-fight zone, user report) — it exists only to keep the held
/// layout out of gpui's shorter-than-viewport regime, where a bottom-aligned
/// list reports no item bounds (sizing goes blind) and position becomes a
/// function of content height instead of the hold. Two pixels of travel is
/// below perception.
const OWN_SEND_SCROLL_SLACK_PX: f32 = 2.0;

/// Per-60fps-frame fraction of the remaining entry glide retained (~90%
/// covered in ~230ms, ease-out).
const OWN_SEND_GLIDE_RETAIN: f32 = 0.85;

/// The entry glide snaps to the absolute hold within this error.
const OWN_SEND_GLIDE_SNAP_PX: f32 = 1.0;

/// The reservation a held turn still needs: the room under the prompt's
/// top-inset position (`usable` = viewport minus inset and bottom chrome)
/// not yet consumed by the turn's own content. Zero once the reply has
/// filled the reserved space — the notes-app `minHeight` analogue.
pub(super) fn own_turn_reservation(usable: f32, turn_height: f32) -> f32 {
    (usable - turn_height).max(0.0)
}

/// Layout state for the most recent locally-sent turn (notes-app parity):
/// EVERY send reserves the space below the prompt for the reply — a trailing
/// runway pad sized `usable − turn height`, i.e. a min-height for the turn,
/// shrinking 1:1 as the reply streams so the held layout never moves. The
/// entry is an eased glide onto the prompt; landed, the hold re-asserts the
/// prompt's position absolutely after every layout (the bottom spring can't
/// hold here: parking at exact distance 0 re-glues gpui's list, which then
/// hard-tracks the pad's stale bottom on every commit — rig-traced). Wheel
/// input releases the hold, leaving the reservation as plain scrollable
/// space. The anchor retires once the reply overflows the reservation (pad
/// ~0, height-neutral). Chat switches snapshot its runway with the viewport
/// and restore it released, so revisiting never resumes hidden auto-follow.
#[derive(Clone, Debug)]
pub(super) struct OwnTurnAnchor {
    pub(super) chat_id: String,
    pub(super) message_id: SharedString,
    /// Current reservation pad on the last row (`usable − turn_height`).
    pub(super) runway: f32,
    /// The step still owns the viewport (glide → hold). Any wheel/touch
    /// input releases it — the reservation stays behind as plain scrollable
    /// space, and the ordinary escape/restick rules apply from then on.
    pub(super) held: bool,
    /// The entry glide has landed; the hold now re-asserts the prompt's
    /// position absolutely after every layout (glue- and lag-proof — the
    /// exact mechanism the shipped first-send anchor used).
    pub(super) positioned: bool,
    /// A fresh send may install the anchor one notification before its echo.
    /// Once the prompt has appeared, its later disappearance is terminal
    /// (failed echo or removed entry) and the runway must retire.
    pub(super) seen_prompt: bool,
}

impl OwnTurnAnchor {
    fn released_for_restore(mut self) -> Self {
        self.held = false;
        self.positioned = false;
        self.seen_prompt = true;
        self
    }

    pub(super) fn observe_prompt(&mut self, exists: bool) -> bool {
        if exists {
            self.seen_prompt = true;
        }
        exists || !self.seen_prompt
    }
}

/// A stable per-chat viewport anchor. Row identity is preferred over its old
/// index because async replay can insert or remove rows while a chat is away.
#[derive(Clone, Debug)]
pub(super) struct ViewportAnchor {
    row_id: SharedString,
    pub(super) entry_id: SharedString,
    fallback_ix: usize,
    offset_in_row: Pixels,
}

impl ViewportAnchor {
    pub(super) fn capture(rows: &[Row], scroll_top: ListOffset) -> Option<Self> {
        let fallback_ix = scroll_top.item_ix.min(rows.len().checked_sub(1)?);
        let row = &rows[fallback_ix];
        Some(Self {
            row_id: row.id.clone(),
            entry_id: row.entry_id.clone(),
            fallback_ix,
            offset_in_row: scroll_top.offset_in_item,
        })
    }

    fn resolve_exact(&self, rows: &[Row]) -> Option<ListOffset> {
        let item_ix = rows.iter().position(|row| row.id == self.row_id)?;
        Some(ListOffset {
            item_ix,
            offset_in_item: self.offset_in_row,
        })
    }

    pub(super) fn resolve(&self, rows: &[Row]) -> Option<ListOffset> {
        if let Some(offset) = self.resolve_exact(rows) {
            return Some(offset);
        }

        // A row can disappear when a streaming block is reshaped. Stay in the
        // same message entry, choosing the surviving row nearest the old
        // location; the intra-row offset is no longer meaningful in that case.
        let item_ix = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.entry_id == self.entry_id)
            .min_by_key(|(ix, _)| ix.abs_diff(self.fallback_ix))
            .map(|(ix, _)| ix)
            .unwrap_or_else(|| self.fallback_ix.min(rows.len().saturating_sub(1)));
        (!rows.is_empty()).then_some(ListOffset {
            item_ix,
            offset_in_item: px(0.0),
        })
    }
}

/// Session-local viewport state. Chats that were following their tail keep
/// following it; only user-owned viewports restore a concrete row anchor.
#[derive(Clone, Debug)]
pub(super) enum SavedViewport {
    FollowTail,
    Anchored {
        anchor: ViewportAnchor,
        distance_from_bottom: f32,
        /// Preserve the runway that made a short active turn scrollable.
        /// Navigation releases its automatic hold, so revisiting restores the
        /// viewport without immediately following new output to the bottom.
        own_turn: Option<OwnTurnAnchor>,
    },
}

pub(super) struct RestoredViewport {
    pub(super) offset: ListOffset,
    pub(super) distance_from_bottom: f32,
    pub(super) own_turn: Option<OwnTurnAnchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ViewportFinalizeToken {
    pub(super) generation: u64,
    pub(super) layout_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TranscriptReplayState {
    Pending,
    Empty,
    Populated,
}

impl TranscriptReplayState {
    pub(super) fn authoritative_empty(self) -> bool {
        self == Self::Empty
    }

    pub(super) fn allows_fallback(self) -> bool {
        self == Self::Populated
    }
}

impl ViewportFinalizeToken {
    pub(super) fn still_current(self, generation: u64) -> bool {
        self.generation == generation
    }

    pub(super) fn layout_settled(self, layout_revision: u64) -> bool {
        self.layout_revision == layout_revision
    }
}

impl SavedViewport {
    pub(super) fn capture(
        rows: &[Row],
        scroll_top: ListOffset,
        pinned: bool,
        distance_from_bottom: f32,
        own_turn: Option<&OwnTurnAnchor>,
    ) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        if pinned {
            return Some(Self::FollowTail);
        }
        Some(Self::Anchored {
            anchor: ViewportAnchor::capture(rows, scroll_top)?,
            distance_from_bottom,
            own_turn: own_turn.cloned(),
        })
    }

    /// Before the opening reset arrives, rows may contain only optimistic
    /// echoes. In that gap an exact row is safe, but entry/index fallbacks
    /// would mistake an unrelated echo for the authoritative transcript.
    pub(super) fn resolve(&self, rows: &[Row], allow_fallback: bool) -> Option<RestoredViewport> {
        let Self::Anchored {
            anchor,
            distance_from_bottom,
            own_turn,
        } = self
        else {
            return None;
        };
        let offset = if allow_fallback {
            anchor.resolve(rows)?
        } else {
            anchor.resolve_exact(rows)?
        };
        let own_turn = own_turn
            .clone()
            .filter(|turn| {
                rows.iter()
                    .any(|row| row.turn_start && row.entry_id == turn.message_id)
            })
            .map(OwnTurnAnchor::released_for_restore);
        Some(RestoredViewport {
            offset,
            distance_from_bottom: *distance_from_bottom,
            own_turn,
        })
    }
}

#[derive(Default)]
pub(super) struct SavedViewportCache {
    pub(super) by_chat: HashMap<String, SavedViewport>,
    recency: VecDeque<String>,
}

impl SavedViewportCache {
    pub(super) fn insert(&mut self, chat_id: String, viewport: SavedViewport) {
        if self.by_chat.contains_key(&chat_id) {
            self.recency.retain(|candidate| candidate != &chat_id);
        }
        self.recency.push_back(chat_id.clone());
        self.by_chat.insert(chat_id, viewport);
        while self.by_chat.len() > MAX_SAVED_VIEWPORTS {
            let Some(evicted) = self.recency.pop_front() else {
                break;
            };
            self.by_chat.remove(&evicted);
        }
    }

    pub(super) fn get_cloned_and_touch(&mut self, chat_id: &str) -> Option<SavedViewport> {
        let viewport = self.by_chat.get(chat_id).cloned()?;
        self.recency.retain(|candidate| candidate != chat_id);
        self.recency.push_back(chat_id.to_string());
        Some(viewport)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.by_chat.len()
    }
}
impl Transcript {
    /// Snapshot the outgoing primary chat before its rows and ListState are
    /// reset. Empty rows never overwrite an older snapshot: during a rapid
    /// A→B→A switch, B's replay may not have arrived before leaving it again.
    pub(super) fn remember_current_viewport(&mut self) {
        // Rows can already contain optimistic echoes while an older snapshot
        // is still waiting for the authoritative replay. Leaving again in
        // that window must preserve the older snapshot, not replace it with
        // the partial echo-only viewport.
        if self.pending_viewport.is_some() {
            return;
        }
        let Some(chat_id) = self.chat_id.clone() else {
            return;
        };
        let distance_from_bottom = if self.pinned {
            0.0
        } else {
            self.distance_from_bottom()
        };
        let Some(viewport) = SavedViewport::capture(
            &self.rows,
            self.list.logical_scroll_top(),
            self.pinned,
            distance_from_bottom,
            self.own_turn.as_ref(),
        ) else {
            return;
        };
        self.saved_viewports.insert(chat_id, viewport);
    }

    /// Restore an exact optimistic row while replay is pending, enable stable
    /// fallbacks only after a populated reset, and retire snapshots proven
    /// absent by an empty reset. `scroll_to` remains valid while the virtual
    /// list measures restored rows on the following layout pass.
    pub(super) fn restore_pending_viewport(&mut self, replay: TranscriptReplayState) -> bool {
        if self.pending_viewport.is_none() {
            return false;
        }
        if !self.rows.is_empty()
            && let Some(restored) = self
                .pending_viewport
                .as_ref()
                .and_then(|saved| saved.resolve(&self.rows, replay.allows_fallback()))
        {
            self.pending_viewport = None;
            self.list.scroll_to(restored.offset);
            self.own_turn = restored.own_turn;
            self.own_turn_kick = self.own_turn.is_some();
            self.own_turn_last_tick = None;
            if self.own_turn.is_some() {
                // Replay readiness can change while echo rows stay identical,
                // so the no-diff path may install a runway without splicing.
                self.remeasure_last_row();
            }
            self.last_scroll_distance = restored.distance_from_bottom;
            self.show_jump_button = restored.distance_from_bottom > SCROLL_BUTTON_THRESHOLD_PX;
            self.viewport_finalize_pending = true;
            return true;
        }

        if !replay.authoritative_empty() {
            return false;
        }
        // The reset's document rows, not the combined rows, define
        // authoritative emptiness. A matching optimistic row above remains
        // valid, but an unrelated echo must never become an index fallback
        // for old history.
        self.discard_pending_viewport();
        if self.own_turn.is_none() {
            self.pinned = true;
            self.last_scroll_distance = 0.0;
            self.show_jump_button = false;
            self.list.scroll_to_end();
        }
        true
    }

    /// Explicit user/navigation intent supersedes a replay-delayed restore.
    /// Replace its cache entry with tail-follow until current rows can be
    /// snapshotted normally on the next chat switch.
    pub(crate) fn discard_pending_viewport(&mut self) {
        if self.pending_viewport.take().is_some()
            && let Some(chat_id) = self.chat_id.clone()
        {
            self.saved_viewports
                .insert(chat_id, SavedViewport::FollowTail);
        }
    }

    /// Hand viewport ownership to explicit rail/navigation input before its
    /// reduced-motion or animated branch moves the list.
    pub(crate) fn begin_scroll_navigation(&mut self) {
        self.discard_pending_viewport();
        // Rail navigation within the session RELEASES the hold but keeps the
        // runway (user spec: only leaving and revisiting the session clears
        // it) — scrolling back down re-arms the hold like any restick.
        self.release_own_turn_hold();
        self.pinned = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.spring_settled_at = None;
        self.spring_kick = false;
        self.scroll_anim = None;
    }

    /// Store the animation after [`Self::begin_scroll_navigation`].
    pub(crate) fn set_scroll_task(&mut self, task: Task<()>) {
        self.scroll_anim = Some(task);
    }

    /// Give the viewport to the user/navigation without dropping the
    /// reservation: the pad stays, the hold stands down until a restick.
    fn release_own_turn_hold(&mut self) {
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.held = false;
        }
        self.own_turn_last_tick = None;
    }

    pub(super) fn remeasure_last_row(&mut self) {
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.list.remeasure_items(last..last + 1);
            self.viewport_layout_revision = self.viewport_layout_revision.wrapping_add(1);
        }
    }

    pub(crate) fn distance_from_bottom(&self) -> f32 {
        let max = f32::from(self.list.max_offset_for_scrollbar().y);
        let cur = f32::from(self.list.scroll_px_offset_for_scrollbar().y);
        (max + cur).max(0.0)
    }

    pub(super) fn handle_scroll(&mut self, _event: &ListScrollEvent, cx: &mut Context<Self>) {
        // The list invokes this handler ONLY from its wheel/touch input path
        // (programmatic scroll_by/scroll_to never re-enter it), while holding
        // its internal RefCell borrow — reading the ListState back
        // synchronously panics with "already mutably borrowed". Defer to the
        // end of the effect cycle, after the list has released its borrow.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this: &mut Transcript, cx| {
                this.discard_pending_viewport();
                // Wheel/touch while a runway lives: input owns the viewport,
                // and the BOTTOM PIN must stay out of it entirely. Escaping
                // releases the hold (the reservation stays behind as plain
                // scrollable space); returning toward the bottom re-arms the
                // HOLD, never `pinned` — a restick pin glued the view to the
                // bottom of the reservation pad, where streaming reads as
                // text stuck at the viewport top with the runway never
                // filling (user report; the pad can't resize there either,
                // its anchor being off-screen). macOS trackpad momentum can
                // even release-and-restick within one gesture right after a
                // send, so under the old rules the prompt never landed at
                // the top at all.
                if this.own_turn.is_some() {
                    let distance = this.distance_from_bottom();
                    let previous = this.last_scroll_distance;
                    this.last_scroll_distance = distance;
                    let held = this.own_turn.as_ref().is_some_and(|a| a.held);
                    if distance > previous + 1.0 && distance > AT_BOTTOM_PX {
                        // Input moving away from the bottom breaks the hold.
                        if let Some(anchor) = this.own_turn.as_mut() {
                            anchor.held = false;
                        }
                        this.own_turn_last_tick = None;
                        this.pinned = false;
                        this.spring.reset();
                        this.spring_last_tick = None;
                    } else if !held
                        && (distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous))
                    {
                        // Returning to the bottom returns to the RUNWAY: the
                        // glide re-lands the prompt at its inset.
                        if let Some(anchor) = this.own_turn.as_mut() {
                            anchor.held = true;
                            anchor.positioned = false;
                        }
                        this.own_turn_last_tick = None;
                        this.own_turn_kick = true;
                    } else if held {
                        // Wheel-down while held: the bottom is a HARD STOP.
                        // The pad runs one frame behind a streaming commit,
                        // so the list's own end-clamp can briefly admit
                        // travel into the transient surplus — re-assert the
                        // hold in the same effect cycle, before anything
                        // paints, and the sink never reaches the screen.
                        // (scroll_to is bounds-free, so this also covers the
                        // wheel gluing the offset at the end.)
                        if let Some(ix) = this.own_turn_anchor_ix() {
                            this.list.scroll_to(ListOffset {
                                item_ix: ix,
                                offset_in_item: px(0.0),
                            });
                            this.list.scroll_by(px(-Self::own_send_inset(ix)));
                        }
                        this.last_scroll_distance = this.distance_from_bottom();
                    }
                    let show = distance > SCROLL_BUTTON_THRESHOLD_PX
                        && !this.own_turn.as_ref().is_some_and(|a| a.held);
                    if show != this.show_jump_button {
                        this.show_jump_button = show;
                    }
                    cx.notify();
                    return;
                }
                let distance = this.distance_from_bottom();
                let previous = this.last_scroll_distance;
                this.last_scroll_distance = distance;
                if distance > previous + 1.0 && distance > AT_BOTTOM_PX {
                    // User input moving away from the bottom breaks the pin.
                    // Content growth never lands here — it doesn't fire the
                    // scroll handler (mugen §1e: interrupt from input, not
                    // scrollbar position).
                    this.pinned = false;
                    this.spring.reset();
                    this.spring_last_tick = None;
                } else if distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous) {
                    // Returning toward the bottom inside the 70px band (or
                    // arriving at it) re-engages the pin with a glide.
                    if !this.pinned {
                        this.pinned = true;
                        this.wake_spring();
                    }
                }
                let show = distance > SCROLL_BUTTON_THRESHOLD_PX && !this.pinned;
                if show != this.show_jump_button {
                    this.show_jump_button = show;
                }
                cx.notify();
            })
            .ok();
        });
    }

    pub(super) fn on_selection_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() || !crate::markdown::selection::is_dragging() {
            self.stop_selection_scroll();
            return;
        }
        self.selection_drag_position = Some(event.position);
        if render::update_drag_at(event.position) {
            cx.notify();
        }
        self.schedule_selection_scroll(cx);
    }

    pub(super) fn on_selection_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.stop_selection_scroll();
        if let Some(_text) = crate::markdown::selection::end_active_drag() {
            // X11 middle-click paste parity, including the case where the
            // anchor row has virtualized away and cannot receive mouse-up.
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            _cx.write_to_primary(ClipboardItem::new_string(_text));
        }
    }

    pub(super) fn stop_selection_scroll(&mut self) {
        self.selection_drag_position = None;
        self.selection_scroll_task = None;
    }

    fn schedule_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if self.selection_scroll_task.is_some() || !crate::markdown::selection::is_dragging() {
            return;
        }
        let Some(position) = self.selection_drag_position else {
            return;
        };
        if selection_scroll_step(self.list.viewport_bounds(), position) == 0.0 {
            return;
        }
        self.selection_scroll_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SELECTION_SCROLL_TICK_MS))
                .await;
            let _ = this.update(cx, |transcript, cx| {
                transcript.selection_scroll_task = None;
                transcript.step_selection_scroll(cx);
            });
        }));
    }

    fn step_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if !crate::markdown::selection::is_dragging() {
            self.stop_selection_scroll();
            return;
        }
        let Some(position) = self.selection_drag_position else {
            return;
        };
        let step = selection_scroll_step(self.list.viewport_bounds(), position);
        if step == 0.0 {
            return;
        }

        // Resolve against the registry painted after the previous step before
        // moving it again. This is what lets a stationary edge pointer consume
        // successive virtualized rows.
        render::update_drag_at(position);
        self.scroll_anim = None;
        self.discard_pending_viewport();
        self.release_own_turn_hold();
        self.pinned = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.list.scroll_by(px(step));
        self.last_scroll_distance = self.distance_from_bottom();
        self.show_jump_button = self.last_scroll_distance > SCROLL_BUTTON_THRESHOLD_PX;
        cx.notify();
        self.schedule_selection_scroll(cx);
    }

    /// Reserve the reply's space below a locally-sent prompt — EVERY send,
    /// not just the first (a steer or a post-turn send used to collapse the
    /// previous reservation and drop the messages back down — user report).
    /// [`Self::step_own_turn`] sizes the reservation; the motion is just the
    /// bottom pin: with the pad installed, the spring's glide to the new
    /// bottom lands the prompt at the top. Replacing a still-held previous
    /// anchor collapses its pad into the same glide — one continuous motion.
    pub fn on_own_send(&mut self, chat_id: String, message_id: String, cx: &mut Context<Self>) {
        self.discard_pending_viewport();
        self.pinned = false;
        self.show_jump_button = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.spring_settled_at = None;
        self.spring_kick = false;
        self.scroll_anim = None;
        // A glued offset re-snaps to the end on EVERY layout — the pad would
        // land and the viewport hard-track its bottom in the same frame,
        // skipping the glide entirely (rig-traced). Pin the offset to a
        // CONCRETE visible item first; the pad then reads as scrollable
        // distance for the glide to cover.
        self.materialize_scroll_anchor();
        let seen_prompt = self
            .rows
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id.as_str());
        self.own_turn = Some(OwnTurnAnchor {
            chat_id,
            message_id: SharedString::from(message_id),
            runway: 0.0,
            held: true,
            positioned: false,
            seen_prompt,
        });
        self.own_turn_last_tick = None;
        self.own_turn_kick = true;
        self.remeasure_last_row();
        cx.notify();
    }

    /// Convert a glued scroll offset (`None`/past-the-end — layout re-snaps
    /// it to the end each frame) into a concrete `{item, offset}` anchored at
    /// the first visible row, which layout holds still.
    fn materialize_scroll_anchor(&mut self) {
        if !self.is_glued() {
            return;
        }
        let vp_top = f32::from(self.list.viewport_bounds().top());
        for ix in 0..self.rows.len() {
            if let Some(bounds) = self.list.bounds_for_item(ix)
                && f32::from(bounds.bottom()) > vp_top + 0.5
            {
                self.list.scroll_to(ListOffset {
                    item_ix: ix,
                    offset_in_item: px(vp_top - f32::from(bounds.top())),
                });
                return;
            }
        }
    }

    /// The held prompt's top offset from the viewport top. Row 0 already
    /// carries the titlebar chrome inside its own box (the first row's
    /// top gap), so the hold adds nothing — adding the inset on top parked
    /// a new chat's first prompt a double-chrome ~66px low (user report).
    fn own_send_inset(anchor_ix: usize) -> f32 {
        if anchor_ix == 0 {
            0.0
        } else {
            OWN_SEND_TOP_INSET_PX
        }
    }

    fn own_turn_anchor_ix(&self) -> Option<usize> {
        let anchor = self.own_turn.as_ref()?;
        self.rows
            .iter()
            .position(|row| row.turn_start && row.entry_id == anchor.message_id)
    }

    pub(super) fn reconcile_own_turn_prompt(&mut self) {
        let Some(message_id) = self
            .own_turn
            .as_ref()
            .map(|anchor| anchor.message_id.clone())
        else {
            return;
        };
        let exists = self
            .rows
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id);
        let keep = self
            .own_turn
            .as_mut()
            .is_some_and(|anchor| anchor.observe_prompt(exists));
        if keep {
            return;
        }

        self.own_turn = None;
        self.own_turn_kick = false;
        self.own_turn_last_tick = None;
        self.remeasure_last_row();
        self.last_scroll_distance = self.distance_from_bottom();
        self.show_jump_button = self.last_scroll_distance > SCROLL_BUTTON_THRESHOLD_PX;
        self.viewport_finalize_pending = true;
    }

    /// One post-layout own-turn step: size the reservation pad. Pure layout —
    /// all motion is the ordinary bottom pin (see [`OwnTurnAnchor`]).
    pub(super) fn step_own_turn(&mut self, cx: &mut Context<Self>) {
        self.own_turn_kick = false;
        // Layout moves the bottom too (pad refinement, streaming growth):
        // refresh the wheel handler's escape baseline every frame so only a
        // WHEEL's own delta registers as user intent. Without this, the pad
        // growing at turn-completion between two wheel events read as
        // "scrolled away" and silently released the hold — the next wheels
        // then sank unopposed deep into the runway blank (rig-traced).
        self.last_scroll_distance = self.distance_from_bottom();
        let Some(anchor_ix) = self.own_turn_anchor_ix() else {
            // The optimistic echo may arrive on the next state notification.
            return;
        };
        if let Some(anchor) = self.own_turn.as_mut() {
            anchor.seen_prompt = true;
        }
        let viewport = self.list.viewport_bounds();
        let viewport_height = f32::from(viewport.size.height);
        if viewport_height <= 0.0 {
            self.own_turn_kick = true;
            cx.notify();
            return;
        }
        let Some(last_ix) = self.rows.len().checked_sub(1) else {
            return;
        };
        let base_pad = self.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND + 8.0;
        let inset = Self::own_send_inset(anchor_ix);
        // A glued offset hard-tracks a GROWING end — streamed text visually
        // pushes everything above it up while the runway blank persists
        // below (user report; the glued representation also hides every
        // item's bounds, so the sizing that would consume the runway goes
        // blind). Dissolve it for HELD and RELEASED views alike. The glued
        // sentinel resolves NUMERICALLY to the total content height (a
        // viewport top past the last item), so a small nudge lands in an
        // absurd overscroll that layout's under-fill normalizer re-glues on
        // the very next frame — an invisible wedge loop (rig-traced).
        // Stepping back a FULL viewport from the sentinel is exactly "end
        // at the screen bottom": the same visual position, concrete.
        if self.is_glued() {
            self.list.scroll_by(px(-viewport_height));
        }
        // The slack keeps the held layout scrollable (see the constant) —
        // the reservation deliberately over-fills by this much.
        let usable = viewport_height - inset - base_pad + OWN_SEND_SCROLL_SLACK_PX;
        let current = self.own_turn.as_ref().map_or(0.0, |a| a.runway);

        // A fresh anchor installs a provisional pad BEFORE anything needs
        // bounds: the just-sent rows sit below the fold, unmeasured, and
        // without the pad there is no scroll room to bring them into the
        // measured window (gating the pad on their bounds deadlocked — the
        // clamped scroll kept them unmeasured forever). Sized at FULL
        // `usable` — a deliberate overshoot by the turn's own height, safe
        // under the absolute hold (scroll_to pins the prompt regardless) and
        // REQUIRED for short chats: gpui's bottom-aligned list reports no
        // item bounds while its content is shorter than the viewport
        // (rig-traced: a new session's first send sat ~150px below the
        // inset forever — the old undershot pad left the content short, the
        // bounds-free scroll_to clamped, and the bounds-gated refinement
        // could never rescue it). Overshooting guarantees the scroll room;
        // the surplus sits below the fold until the refinement trues it.
        if current <= 0.0 {
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.runway = usable.max(0.0);
            }
            self.remeasure_last_row();
            cx.notify();
            return;
        }

        // ---- reservation sizing (skipped while unmeasured: the provisional
        // pad stands; the render gate re-runs this every live frame) --------
        if let (Some(anchor_bounds), Some(last_bounds)) = (
            self.list.bounds_for_item(anchor_ix),
            self.list.bounds_for_item(last_ix),
        ) {
            // Content height of the turn, excluding the pads on the last row.
            let turn_height = f32::from(last_bounds.bottom())
                - f32::from(anchor_bounds.top())
                - current
                - base_pad;
            let target = own_turn_reservation(usable, turn_height);
            // FLOOR: never shrink the pad faster than the viewport allows.
            // The step runs a frame behind content growth, so a wheel that
            // lands inside that window can sink the view toward the stale
            // end; snapping the pad straight to `target` then pulls the end
            // UP THROUGH the viewport (the list clamps instantly — a visible
            // yank, user report "stutter push back"). Shrinking is capped so
            // the end never rises above the current view; deferred surplus
            // burns off as the view moves away from the stop.
            let dist = self.distance_from_bottom();
            let floor = current - (dist - OWN_SEND_SCROLL_SLACK_PX).max(0.0);
            let target = target.max(floor.min(current));
            if target <= 0.5 {
                // The reply has outgrown the reserved space (or the prompt
                // alone overfills it): the pad is ~0, so dropping it is
                // height-neutral. A still-held view hands off to the bottom
                // pin; a released one doesn't move at all.
                let held = self.own_turn.take().is_some_and(|a| a.held);
                self.remeasure_last_row();
                if held {
                    self.engage_pin(cx);
                } else {
                    cx.notify();
                }
                return;
            }
            if (target - current).abs() > 0.5 {
                if let Some(anchor) = self.own_turn.as_mut() {
                    anchor.runway = target;
                }
                // Growth into the reservation shrinks the pad 1:1 — the held
                // layout never moves.
                self.remeasure_last_row();
                cx.notify();
            }
        }

        // ---- entry glide, then absolute hold -------------------------------
        let (held, positioned) = self
            .own_turn
            .as_ref()
            .map_or((false, false), |a| (a.held, a.positioned));
        if !held {
            return;
        }
        if positioned {
            // Landed: re-assert the prompt's position after every layout.
            // scroll_to is absolute and bounds-independent, so neither glue
            // re-snaps, pad-sizing lag, nor a splice's unmeasured flicker can
            // carry the view off the prompt (each broke the spring-held
            // variants of this — rig-traced). ONE-SIDED: only upward drift
            // (view above the hold) is corrected. The scroll slack under the
            // reservation is legal resting space — wheel-down sinks into it
            // and stops hard at the list's own clamp; snapping back up from
            // there made the bottom bounce/stutter on every scroll event
            // (user report). Way-below-slack (impossible short of a bug)
            // still re-asserts.
            let moved = match self.list.bounds_for_item(anchor_ix) {
                Some(b) => {
                    let err = f32::from(b.top()) - (f32::from(viewport.top()) + inset);
                    // The legal rest zone below the hold is the epsilon plus
                    // rounding; anything deeper is a transient-collision sink
                    // and rubber-bands back.
                    !(-(OWN_SEND_SCROLL_SLACK_PX + 2.0)..=0.5).contains(&err)
                }
                // Bounds vanish in the glued representation (dissolved
                // above, so at most for this one frame) and through splice
                // flicker. Near the stop that is dead-band space — no
                // assert (asserting on None here was the bottom bounce);
                // far from it the position is unknowable flicker: re-assert.
                None => self.distance_from_bottom() > OWN_SEND_SCROLL_SLACK_PX + 8.0,
            };
            if moved {
                // Correct with the entry glide's ease, not a snap: the only
                // in-band escapes are one-frame commit transients and splice
                // flicker, and an eased ~200ms return reads as native
                // rubber-banding where an instant re-assert read as stutter
                // (user report). Bounds-less flicker still snaps — there is
                // nothing to ease against.
                match self.list.bounds_for_item(anchor_ix) {
                    Some(b) => {
                        let err = f32::from(b.top()) - (f32::from(viewport.top()) + inset);
                        let now = Instant::now();
                        let frames = match self.own_turn_last_tick {
                            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0
                                / SPRING_FRAME_MS)
                                .min(SPRING_MAX_CATCHUP_FRAMES),
                            None => 1.0,
                        };
                        self.own_turn_last_tick = Some(now);
                        let ease = 1.0 - OWN_SEND_GLIDE_RETAIN.powf(frames);
                        if err.abs() <= OWN_SEND_GLIDE_SNAP_PX {
                            self.list.scroll_by(px(err));
                            self.own_turn_last_tick = None;
                        } else {
                            self.list.scroll_by(px(err * ease));
                        }
                        self.own_turn_kick = true;
                    }
                    None => {
                        self.list.scroll_to(ListOffset {
                            item_ix: anchor_ix,
                            offset_in_item: px(0.0),
                        });
                        self.list.scroll_by(px(-inset));
                        self.own_turn_last_tick = None;
                    }
                }
                cx.notify();
            } else {
                self.own_turn_last_tick = None;
            }
            return;
        }
        let now = Instant::now();
        let frames = match self.own_turn_last_tick {
            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0 / SPRING_FRAME_MS)
                .min(SPRING_MAX_CATCHUP_FRAMES),
            None => 1.0,
        };
        self.own_turn_last_tick = Some(now);
        let ease = 1.0 - OWN_SEND_GLIDE_RETAIN.powf(frames);
        // Remaining travel: the anchor's own error once it measures; the
        // bottom distance while it is still below the measured window (the
        // undershot provisional pad guarantees the bottom stops short of the
        // prompt, so this leg can never overshoot it).
        // The two error legs mean DIFFERENT things at zero: on the bounds
        // leg, err 0 is AT the hold (no correction needed); on the bounds-
        // less leg, err is the distance to the pad's bottom — arrival there
        // still needs the absolute snap onto the anchor (the short-chat/
        // glued landing, where bounds never appear). Conflating them once
        // marked entries "positioned" at the pad bottom without ever
        // landing (rig-caught: sends parked deep in blank runway).
        let (err, anchored) = match self.list.bounds_for_item(anchor_ix) {
            Some(bounds) => (
                f32::from(bounds.top()) - (f32::from(viewport.top()) + inset),
                true,
            ),
            None => (self.distance_from_bottom(), false),
        };
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport_height;
        let err = if err > glide_max {
            self.list.scroll_by(px(err - glide_max));
            glide_max
        } else {
            err
        };
        let land = |list: &ListState| {
            list.scroll_to(ListOffset {
                item_ix: anchor_ix,
                offset_in_item: px(0.0),
            });
            list.scroll_by(px(-inset));
        };
        if motion::reduced_motion(cx) {
            land(&self.list);
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else if anchored
            && (-(OWN_SEND_SCROLL_SLACK_PX + 2.0)..=OWN_SEND_GLIDE_SNAP_PX).contains(&err)
        {
            // At the hold — or resting inside the slack under it (a restick
            // that fired at the true bottom): land WITHOUT pulling the view
            // up. Only a still-above position gets the snap.
            if err > 0.5 {
                land(&self.list);
            }
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else if !anchored && err <= OWN_SEND_GLIDE_SNAP_PX {
            // Arrived at the bottom with the anchor still unmeasured: the
            // absolute, bounds-free snap IS the landing.
            land(&self.list);
            if let Some(anchor) = self.own_turn.as_mut() {
                anchor.positioned = true;
            }
            self.own_turn_last_tick = None;
        } else {
            self.list.scroll_by(px(err * ease));
        }
        self.own_turn_kick = true;
        cx.notify();
    }
}
