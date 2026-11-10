//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, and streaming markdown.
//!
//! Row model (docs/research/mugen-pretext.md §3):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups
//!   (agent/spawn chips split out so they never collapse) and input/error chips;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}` — LIVE
//!   (streaming) entries split per block exactly like completed ones (the list
//!   virtualizes them, so a fading live reply re-renders only its visible tail
//!   each frame — flat cost in the reply length); on completion each block row
//!   keeps its id, so row identity is continuous and nothing flickers;
//! - rows are cached per entry keyed by a content fingerprint — only changed
//!   messages rebuild (the anti-"streaming stutter" trick);
//! - row-set changes diff by (id, version) into one minimal `splice`.
//!
//! Stick-to-bottom is a velocity spring (mugen §1e, the same shape as
//! stackblitz's use-stick-to-bottom): while pinned, a per-frame stepper glides
//! the viewport toward the list end with a feed-forward term tracking the
//! smoothed target growth, so 120ms doc commits read as a continuous glide
//! instead of per-commit snaps. The pin breaks only on user input (the list's
//! scroll handler fires exclusively from its wheel/touch path) and re-engages
//! inside the 70px band; the first send in an empty chat anchors the prompt at
//! the viewport top and hands off to the same glide when the reply overflows.
//! While that anchor holds, wheel/touch is clamped rather than obeyed — the
//! whole turn is already visible, so there is nothing to scroll to.

mod attachments;
mod chips;
mod highlights;
mod model;
mod render;
mod rows;
mod spring;
#[cfg(test)]
mod tests;
mod viewport;

use self::chips::{FoldState, blob_detail};
use self::highlights::HighlightStore;
use self::rows::CachedRows;
use self::spring::should_anchor_live_stream;
use self::viewport::{OwnTurnAnchor, SavedViewport, SavedViewportCache, TranscriptReplayState};
use crate::markdown::parser::{BlockTree, IncrementalParser};
use crate::markdown::render::RenderCache;
use crate::markdown::veil::RowVeil;
use crate::motion;
use crate::state::AppState;
use gpui::{
    Context, Entity, ListAlignment, ListScrollEvent, ListState, Pixels, Point, SharedString,
    Subscription, Task, px,
};
use hearth_doc::MessageStatus;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use attachments::{ATT_STRIP_H, ATT_THUMB_H, ATT_THUMB_W};
pub use chips::{BLOB_AFFORDANCE_HEIGHT, CHIP_CARD_HEIGHT, CHIP_GAP, CHIP_HEIGHT};
pub use hearth_proto::view::{single_line, tool_chip_content};
pub use model::{
    CALL_WRAP_COLS, DIFF_DETAIL_MAX_LINES, FLAVOUR_ROTATE_SECS, FLAVOUR_WORDS,
    OUTPUT_DETAIL_MAX_LINES, OUTPUT_LINE_HEIGHT, Row, RowKind, ToolDetail, ToolItem, call_block,
    chips_height, detail_height, diff_to_file, flavour_seed, flavour_word, format_elapsed,
    format_timestamp, sending_bridge, tool_detail, tool_group_summary,
};
pub use render::{MAX_CONTENT_WIDTH, OVERDRAW_PX};
pub use rows::{ParseOutcome, diff_rows, parse_for_row, rows_for_entry, top_gap_for};
pub use spring::{
    AT_BOTTOM_PX, GLIDE_MAX_VIEWPORTS, SCROLL_BUTTON_THRESHOLD_PX, SPRING_CHASE_MAX_LEAD,
    SPRING_DAMPING, SPRING_FRAME_MS, SPRING_GROWTH_EMA, SPRING_MASS, SPRING_MAX_CATCHUP_FRAMES,
    SPRING_SETTLE_GRACE_MS, SPRING_STIFFNESS, StickSpring,
};
pub(crate) use viewport::OWN_SEND_TOP_INSET_PX;
pub use viewport::STICK_THRESHOLD_PX;

pub struct Transcript {
    state: Entity<AppState>,
    list: ListState,
    rows: Vec<Row>,
    chat_id: Option<String>,
    /// `Some(doc_id)` pins this instance to a SUBAGENT doc: rows come from
    /// `AppState::sub_transcript(doc_id)` instead of the selected chat, and
    /// the instance is READ-ONLY — no echoes, no own-turn hold, and no global
    /// attachment protection (that set is shared with the primary transcript
    /// and overwritten wholesale).
    doc_override: Option<String>,
    /// Whether an override instance watches a LIVE doc (`for_doc(follow)`):
    /// only then may the working trailer render — a frozen snapshot must
    /// never spin, whatever its entries claim.
    doc_live: bool,
    /// Memory-only viewport state for primary chats visited in this window.
    /// A transcript instance is shared across tabs, so the active ListState is
    /// reset on every attach and cannot retain these positions by itself.
    saved_viewports: SavedViewportCache,
    /// An anchored viewport waiting for the selected chat's async replay.
    pending_viewport: Option<SavedViewport>,
    /// Generation of the selected chat, guarding post-layout restoration
    /// callbacks across rapid A→B→A navigation.
    viewport_generation: u64,
    /// A restored item anchor needs one post-layout refresh of distance-based
    /// UI state; programmatic list scrolling never invokes `handle_scroll`.
    viewport_finalize_pending: bool,
    viewport_finalize_scheduled: bool,
    /// Bumped whenever sync or own-turn logic invalidates measured rows. The
    /// post-restore finalizer waits until one layout completes without another
    /// invalidation, avoiding a stale jump-button decision.
    viewport_layout_revision: u64,
    /// One-shot "open at the latest content" for UNPINNED (frozen) override
    /// instances: rows land ASYNC after the tab opens (watch replay / blob
    /// fetch), so the end-scroll fires on the first non-empty sync, then
    /// never again — landing at the end and FOLLOWING it are different
    /// states, and the user owns the viewport from there. Pinned instances
    /// don't need it (the pin branch already opens at the end).
    land_end_pending: bool,
    row_cache: HashMap<String, CachedRows>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    folds: HashMap<SharedString, FoldState>,
    /// Detail folds (output/diff) per chip, keyed `"{row_id}#d{ix}"` — full
    /// [`FoldState`]s so detail bodies tween open/closed exactly like the
    /// group fold. Render-local like `folds` — never part of the row
    /// fingerprint. Retained across chat switches (a revisited chat keeps its
    /// open detail cards) and therefore bounded at [`MAX_TOOL_DETAILS`] by
    /// [`prune_tool_details`] on every new key, least recently toggled first.
    tool_details: HashMap<SharedString, FoldState>,
    /// Monotonic recency stamp shared by [`Transcript::tool_details`]; only
    /// the relative order inside the cap-sized map matters (wrapping is
    /// harmless).
    tool_touch_counter: u64,
    /// Streaming fade veils, one per live markdown row (dropped on completion).
    veils: HashMap<SharedString, Rc<RefCell<RowVeil>>>,
    /// Live rows present in the transcript's REPLAY after (re)attaching to a
    /// chat: their veils are created pre-seeded, so text that was already
    /// streamed before the switch never fades in — only appends after it do
    /// (mugen's `FadePainter.attach` baseline; user report: switching back to
    /// a streaming session dissolved the entire reply).
    veil_baseline: std::collections::HashSet<SharedString>,
    /// Armed at attach, disarmed on the first sync whose transcript is
    /// non-empty: the baseline must be captured from the doc REPLAY frame,
    /// not the attach-time sync — selection clears the transcript and the
    /// replay lands async, so capturing at attach seeded nothing and the
    /// still-streaming reply faded in whole on every session switch (user
    /// report, round 2).
    veil_attach_pending: bool,
    /// Cross-frame flatten/shape-input cache (see [`RenderCache`]): fade
    /// frames reuse settled blocks' text+runs; the incremental parser's stable
    /// boundary invalidates only the live tail per commit.
    render_cache: Rc<RefCell<RenderCache>>,
    highlights: HighlightStore,
    show_jump_button: bool,
    /// Distance from the bottom at the last observation (wheel event or spring
    /// tick) — restick and escape are direction-aware
    /// (see [`Transcript::should_restick`]).
    last_scroll_distance: f32,
    /// The stick-to-bottom pin. Broken only by user input (wheel/touch up);
    /// re-engaged inside the 70px band, after an own-send first overflows, and
    /// on the jump button.
    pinned: bool,
    /// A locally-sent prompt currently held near the viewport top while its
    /// reply grows into the empty space below it.
    own_turn: Option<OwnTurnAnchor>,
    /// A layout-affecting change needs one post-layout own-turn measurement.
    own_turn_kick: bool,
    /// One own-turn `on_next_frame` callback in flight at most.
    own_turn_scheduled: bool,
    /// Wall-clock of the previous entry-glide tick (`None` = not gliding).
    own_turn_last_tick: Option<Instant>,
    spring: StickSpring,
    /// Wall-clock of the previous spring tick (`None` = parked).
    spring_last_tick: Option<Instant>,
    /// When the spring last landed on the bottom (settle-grace bookkeeping).
    spring_settled_at: Option<Instant>,
    /// A doc commit / wake happened before layout measured it — run at least
    /// one spring tick even though the pre-layout distance still reads 0.
    spring_kick: bool,
    /// One `on_next_frame` callback in flight at most.
    spring_scheduled: bool,
    scroll_anim: Option<Task<()>>,
    /// Last pointer sample while markdown selection owns a left-button drag.
    selection_drag_position: Option<Point<Pixels>>,
    /// One-shot timer rescheduled only while the pointer remains in an edge
    /// zone. Dropping it on mouse-up stops all selection scroll work.
    selection_scroll_task: Option<Task<()>>,
    /// MessageRail width gate (set by the shell from the container width).
    rail_enabled: bool,
    /// Height of the shell's composer/status/terminal stack overlaying the
    /// transcript's bottom (measured last frame): the last row pads past it
    /// so pinned content rests above the glass chrome it scrolls under.
    bottom_clearance: f32,
    /// Hovered rail tick (grows + shows the preview card).
    rail_hover: Option<usize>,
    /// `(row id, entry id)` under the pointer — reveals the entry's timestamp
    /// strip (hearth chat-view.tsx `group-hover`; the rows report hover
    /// themselves). Keyed by ROW so a row→row move within one entry can't
    /// clear the reveal when the old row's leave event arrives after the new
    /// row's enter (enter/leave order across rows is not guaranteed).
    hovered_entry: Option<(SharedString, SharedString)>,
    /// Code block showing "Copied" feedback: `(row id, block ix)`, cleared by
    /// the companion task after ~1.2s.
    copied_code: Option<(SharedString, usize)>,
    copied_clear: Option<Task<()>>,
    /// Entry whose hover action is showing transient copied-check feedback.
    copied_message: Option<SharedString>,
    copied_message_clear: Option<Task<()>>,
    /// Transcript attachment being viewed full-size (click a user thumbnail).
    attachment_preview: Option<crate::attachments::PreviewImage>,
    /// Focused while the lightbox is open so Escape reaches it.
    attachment_preview_focus: gpui::FocusHandle,
    /// In-flight ReadAttachmentChunk loads, keyed `(deviceId, path)` — one per
    /// source; results land in the global attachment cache.
    attachment_loads: HashMap<(String, String), Task<()>>,
    /// Scheduled retry wake-ups for errored sources (the 2s→15s ladder).
    attachment_retries: HashMap<(String, String), Task<()>>,
    /// Sidecar blob fetches keyed by doc ref (`chatId/partId[.diff]`,
    /// chat2-sync A3). `Ready` holds the UPGRADED detail, built once on
    /// arrival — render swaps it in per chip; rows never rebuild for it.
    /// Deliberately NOT cleared on chat switch: refs are chat-qualified and a
    /// fetched blob stays valid. Bounded at [`MAX_BLOB_DETAILS`] by
    /// [`prune_blob_cache`] on every insertion (see the eviction policy
    /// there); escaping the cap would retain every sidecar body for the whole
    /// process.
    blob_details: HashMap<SharedString, BlobFetch>,
    /// Monotonic fetch order per blob ref: when a tool has BOTH a diff and
    /// an output blob fetched, the chip shows the one requested most
    /// recently (click "Show full output" after a diff → see the output).
    blob_fetch_order: HashMap<SharedString, u64>,
    blob_fetch_counter: u64,
    _observe: Subscription,
}

/// One sidecar blob fetch's lifecycle.
enum BlobFetch {
    Loading(#[allow(dead_code)] Task<()>),
    /// Failed with the affordance re-armed as a retry.
    Failed,
    Ready(Arc<ToolDetail>),
}

/// Cap on cached sidecar blob details (full diff/output bodies). The cache is
/// deliberately NOT cleared on chat switch — refs are chat-qualified and a
/// fetched blob stays valid — so a `Ready` entry would otherwise live for the
/// whole process. One entry per fetched ref; the least recently requested one
/// is evicted when a new fetch overflows the cap.
pub(super) const MAX_BLOB_DETAILS: usize = 256;

/// Oldest eviction candidate in the blob cache, or `None` when every key is
/// the protected one (or only `Loading` slots are left and `settled_only`).
fn oldest_blob_candidate(
    details: &HashMap<SharedString, BlobFetch>,
    order: &HashMap<SharedString, u64>,
    keep: &SharedString,
    settled_only: bool,
) -> Option<SharedString> {
    details
        .iter()
        .filter(|(blob_ref, fetch)| {
            *blob_ref != keep && !(settled_only && matches!(fetch, BlobFetch::Loading(_)))
        })
        .min_by_key(|(blob_ref, _)| order.get(*blob_ref).copied().unwrap_or(0))
        .map(|(blob_ref, _)| (*blob_ref).clone())
}

/// Evict the least-recently-fetched refs until the blob cache is within
/// [`MAX_BLOB_DETAILS`]. Both maps are pruned together so a detail and its
/// fetch-order stamp can never drift apart.
///
/// Policy:
/// - the victim is the lowest `blob_fetch_order` (the same recency the chip
///   uses for "most recently requested wins", bumped on every click);
/// - `keep` — the ref being inserted — is never a candidate;
/// - settled `Ready`/`Failed` entries are preferred. A `Loading` entry owns
///   the in-flight fetch `Task`, so evicting it CANCELS that request; that is
///   acceptable for the oldest slot (the chip's affordance re-arms as a
///   retry), but it only happens when no settled entry is left to evict.
fn prune_blob_cache(
    details: &mut HashMap<SharedString, BlobFetch>,
    order: &mut HashMap<SharedString, u64>,
    keep: &SharedString,
) {
    while details.len() > MAX_BLOB_DETAILS {
        let victim = oldest_blob_candidate(details, order, keep, true)
            .or_else(|| oldest_blob_candidate(details, order, keep, false));
        let Some(victim) = victim else {
            break;
        };
        details.remove(&victim);
        order.remove(&victim);
    }
}

/// Cap on retained tool-detail fold states (one per detail card the user has
/// opened). The map survives chat switches so a revisited chat still shows
/// its open cards, which means it would otherwise keep one small entry for
/// every chip ever expanded, for the whole process. The least recently
/// toggled entries are evicted first; an evicted card simply falls back to
/// the default (closed) fold — its content is re-foldable, not lost.
pub(super) const MAX_TOOL_DETAILS: usize = 512;

/// Evict the least recently toggled tool-detail folds until the map is
/// within [`MAX_TOOL_DETAILS`]. `keep` — the key just inserted — is never the
/// victim.
fn prune_tool_details(folds: &mut HashMap<SharedString, FoldState>, keep: &SharedString) {
    while folds.len() > MAX_TOOL_DETAILS {
        let victim = folds
            .iter()
            .filter(|(key, _)| *key != keep)
            .min_by_key(|(_, fold)| fold.touched)
            .map(|(key, _)| key.clone());
        let Some(victim) = victim else {
            break;
        };
        folds.remove(&victim);
    }
}

/// Shell-facing events (the transcript itself hosts no surfaces).
#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    /// A spawn chip's "Open subagent" affordance: open the subagent's
    /// transcript as a right-pane tab. `chat_id` is the doc the chip lives
    /// in (the frozen blob is keyed `{chat_id}/{doc_id}`); `frozen` means
    /// the subagent finished — try the blob before watching the doc.
    OpenSubagent {
        chat_id: String,
        doc_id: String,
        title: String,
        frozen: bool,
    },
}

impl gpui::EventEmitter<TranscriptEvent> for Transcript {}
impl Transcript {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        Self::build(state, None, true, cx)
    }

    /// A read-only transcript over one SUBAGENT doc (right-pane tab). The
    /// caller starts the feed (`watch_subagent_doc` or the frozen snapshot);
    /// this instance only renders whatever lands under `doc_id`. `follow` =
    /// the doc is live: engage the end-follow pin from the start. Either
    /// way the tab OPENS at the latest content — a frozen transcript lands
    /// at the end once, unpinned, and free-scrolls from there.
    pub fn for_doc(
        state: Entity<AppState>,
        doc_id: String,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(state, Some(doc_id), follow, cx)
    }

    fn build(
        state: Entity<AppState>,
        doc_override: Option<String>,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        // FollowMode stays Normal: the tail pin is ours (a per-frame spring),
        // not the list's per-layout hard snap.
        //
        // Override instances align TOP: a subagent transcript reads like a
        // fresh notes page — entries anchored at the top, streaming growing
        // into the empty space below, never rising from the pane's bottom.
        // Top alignment gets that structurally (a short list rests at the
        // top with no reservation pad), and the PIN machinery still runs on
        // top of it for end-follow: the spring is purely distance-based, and
        // the glue trap it was built around is Bottom-only — layout
        // materializes a Top list's past-end offset to a CONCRETE position
        // every frame (gpui list.rs: only `Bottom` re-glues to the `None`
        // sentinel), so a parked spring can't re-glue and hard-track growth.
        let alignment = if doc_override.is_some() {
            ListAlignment::Top
        } else {
            ListAlignment::Bottom
        };
        let list = ListState::new(0, alignment, px(OVERDRAW_PX));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event: &ListScrollEvent, _window, cx| {
            weak.update(cx, |this: &mut Transcript, cx| {
                this.handle_scroll(event, cx)
            })
            .ok();
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        // The rail is sized for the conversation column; a narrow right-pane
        // tab has no width gate driving it, so override instances skip it.
        let rail_enabled = doc_override.is_none();
        // `follow` is the initial pin: the primary transcript always opens
        // pinned; an override instance pins only while its doc is LIVE (a
        // frozen transcript reads top-down, free-scrolling). Short content
        // is at-end by definition (distance 0), so the pin is invisible
        // until streaming overflows the pane — then it follows, releases on
        // wheel-up, and resticks/jumps exactly like the main transcript.
        let pinned = follow;
        let mut this = Self {
            state,
            list,
            rows: Vec::new(),
            // Pre-set so `sync` never sees an attach edge — an override
            // instance must not reset (or re-pin) on selection changes.
            chat_id: doc_override.clone(),
            land_end_pending: doc_override.is_some() && !follow,
            doc_live: doc_override.is_some() && follow,
            doc_override,
            saved_viewports: SavedViewportCache::default(),
            pending_viewport: None,
            viewport_generation: 0,
            viewport_finalize_pending: false,
            viewport_finalize_scheduled: false,
            viewport_layout_revision: 0,
            row_cache: HashMap::new(),
            live_parsers: HashMap::new(),
            tree_cache: HashMap::new(),
            folds: HashMap::new(),
            tool_details: HashMap::new(),
            tool_touch_counter: 0,
            veils: HashMap::new(),
            veil_baseline: std::collections::HashSet::new(),
            veil_attach_pending: true,
            render_cache: Rc::new(RefCell::new(RenderCache::default())),
            highlights: HighlightStore::default(),
            show_jump_button: false,
            last_scroll_distance: 0.0,
            pinned,
            own_turn: None,
            own_turn_kick: false,
            own_turn_scheduled: false,
            own_turn_last_tick: None,
            spring: StickSpring::new(),
            spring_last_tick: None,
            spring_settled_at: None,
            spring_kick: false,
            spring_scheduled: false,
            scroll_anim: None,
            selection_drag_position: None,
            selection_scroll_task: None,
            rail_enabled,
            bottom_clearance: 0.0,
            rail_hover: None,
            hovered_entry: None,
            copied_code: None,
            copied_clear: None,
            copied_message: None,
            copied_message_clear: None,
            attachment_preview: None,
            attachment_preview_focus: cx.focus_handle(),
            attachment_loads: HashMap::new(),
            attachment_retries: HashMap::new(),
            blob_details: HashMap::new(),
            blob_fetch_order: HashMap::new(),
            blob_fetch_counter: 0,
            _observe: observe,
        };
        this.sync(cx);
        this
    }

    // ---- rail plumbing (rendering lives in crate::rail) ----

    /// Shell-driven width gate: the rail hides below 48rem of container width.
    pub fn set_rail_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.rail_enabled != enabled {
            self.rail_enabled = enabled;
            cx.notify();
        }
    }

    pub(crate) fn rail_enabled(&self) -> bool {
        self.rail_enabled
    }

    /// Shell-driven: the measured height of the bottom chrome stack the
    /// transcript scrolls under. Sub-pixel jitter is ignored so steady-state
    /// frames don't re-notify.
    pub fn set_bottom_clearance(&mut self, height: f32, cx: &mut Context<Self>) {
        if (self.bottom_clearance - height).abs() > 0.5 {
            self.bottom_clearance = height;
            if self.own_turn.is_some() {
                self.remeasure_last_row();
                self.own_turn_kick = true;
            }
            cx.notify();
        }
    }

    pub(crate) fn rail_hover(&self) -> Option<usize> {
        self.rail_hover
    }

    pub(crate) fn set_rail_hover(&mut self, hover: Option<usize>) {
        self.rail_hover = hover;
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn list_state(&self) -> &ListState {
        &self.list
    }

    pub(crate) fn state_entity(&self) -> &Entity<AppState> {
        &self.state
    }

    /// Rebuild rows from app state; splice minimal ranges into the list.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (selected, entries, echoes, replay) = {
            let s = self.state.read(cx);
            match &self.doc_override {
                // Pinned to a subagent doc: `selected` equals `chat_id` by
                // construction, so the attach/reset branch below never fires,
                // and echoes stay empty (nothing is ever sent from here).
                Some(doc_id) => (
                    Some(doc_id.clone()),
                    s.sub_transcript(doc_id).to_vec(),
                    Vec::new(),
                    TranscriptReplayState::Populated,
                ),
                None => {
                    let replay = if !s.transcript_replayed {
                        TranscriptReplayState::Pending
                    } else if s.transcript.is_empty() {
                        TranscriptReplayState::Empty
                    } else {
                        TranscriptReplayState::Populated
                    };
                    (
                        s.selected_chat.clone(),
                        s.transcript.clone(),
                        s.pending_echoes().to_vec(),
                        replay,
                    )
                }
            }
        };

        let attached = selected != self.chat_id;
        if attached {
            // Read the incoming snapshot before inserting the outgoing one:
            // a full bounded cache may evict its oldest entry, which can be
            // exactly the chat the user is reopening.
            let saved_viewport = selected
                .as_ref()
                .and_then(|chat_id| self.saved_viewports.get_cloned_and_touch(chat_id));
            self.remember_current_viewport();
            let keep_own_turn = self
                .own_turn
                .as_ref()
                .is_some_and(|anchor| selected.as_deref() == Some(anchor.chat_id.as_str()));
            if !keep_own_turn {
                self.own_turn = None;
                self.own_turn_kick = false;
                self.own_turn_last_tick = None;
            }
            self.chat_id = selected;
            self.rows.clear();
            self.row_cache.clear();
            self.live_parsers.clear();
            self.tree_cache.clear();
            self.folds.clear();
            self.veils.clear();
            self.render_cache.borrow_mut().clear();
            self.highlights.entries.clear();
            self.copied_message = None;
            self.copied_message_clear = None;
            self.list.reset(0);
            self.pending_viewport = None;
            self.viewport_generation = self.viewport_generation.wrapping_add(1);
            self.viewport_finalize_pending = false;
            if self.own_turn.is_some() {
                // A kept own-turn hold (send-created chat) owns the viewport.
                self.pinned = false;
                self.last_scroll_distance = 0.0;
                self.show_jump_button = false;
            } else if let Some(SavedViewport::Anchored {
                anchor,
                distance_from_bottom,
                own_turn,
            }) = saved_viewport
            {
                // Keep a possible runway pending until replay confirms that
                // its optimistic prompt still exists. Installing it on this
                // empty attach frame can leave a failed send's stale anchor
                // intercepting scroll-to-bottom forever.
                self.pinned = false;
                self.last_scroll_distance = distance_from_bottom;
                self.show_jump_button = distance_from_bottom > SCROLL_BUTTON_THRESHOLD_PX;
                self.pending_viewport = Some(SavedViewport::Anchored {
                    anchor,
                    distance_from_bottom,
                    own_turn,
                });
            } else {
                // New chats and chats that were following their tail retain
                // the existing open-at-bottom behavior.
                self.pinned = true;
                self.last_scroll_distance = 0.0;
                self.show_jump_button = false;
            }
            self.spring.reset();
            self.spring_last_tick = None;
            self.spring_settled_at = None;
            self.spring_kick = false;
            self.scroll_anim = None;
            self.stop_selection_scroll();
        }

        let mut new_rows: Vec<Row> = Vec::new();
        for entry in &entries {
            new_rows.extend(self.rows_for(entry, false));
        }
        for echo in &echoes {
            new_rows.extend(self.rows_for(echo, true));
        }

        // Text already streamed before this (re)attach is the veil BASELINE:
        // its rows' veils seed instead of fading (render creates them from
        // this set), so only post-switch appends animate. Captured from the
        // first NON-EMPTY transcript after attach — the replay frame — never
        // the attach-time sync, whose transcript is still empty (selection
        // clears it; the doc watch refills it async).
        if attached {
            self.veil_baseline.clear();
            self.veil_attach_pending = true;
        }
        if self.veil_attach_pending && !entries.is_empty() {
            self.veil_attach_pending = false;
            self.veil_baseline = new_rows
                .iter()
                .filter(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
                .map(|r| r.id.clone())
                .collect();
        }

        // Veils live exactly as long as their live row — drop them on the
        // live→complete flip (any mid-fade chunk snaps to full, matching the
        // row's version splice).
        self.veils.retain(|id, _| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });
        self.veil_baseline.retain(|id| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });

        // Capture this before the row splice changes the list's measured end.
        // When the user is truly live-following, retaining the end anchor
        // keeps the in-flow working trailer at the same viewport position as
        // transcript lines grow above it. Nothing about the trailer's layout
        // or coordinates changes.
        let live_following = should_anchor_live_stream(
            self.pinned,
            self.distance_from_bottom(),
            entries
                .last()
                .is_some_and(|entry| entry.status == Some(MessageStatus::Streaming)),
        );
        let was_empty = self.rows.is_empty();
        let old_last = self.rows.len().checked_sub(1);
        match diff_rows(&self.rows, &new_rows) {
            None => {
                self.rows = new_rows;
                self.refresh_protected_attachments(cx);
                self.reconcile_own_turn_prompt();
                // Replay readiness is independent of row content: an empty
                // reset (or one identical to optimistic rows) still resolves
                // or retires the pending viewport.
                if self.restore_pending_viewport(replay) {
                    cx.notify();
                }
                return;
            }
            Some((old_range, count)) => {
                // Any replaced row's cached flatten results are stale — and
                // because live replies splice only the rows whose content hash
                // changed (the tail), this is O(changed rows) per commit, never
                // O(reply).
                for row in &self.rows[old_range.clone()] {
                    self.render_cache.borrow_mut().invalidate_row(&row.id);
                }
                if old_range.len() == count {
                    // In-place content change, same row count — notably the
                    // live→complete flip, where EVERY row of the streamed
                    // message changes version (streaming bit, tool auto_open,
                    // timestamp bit) with identical ids. `splice` would reset
                    // those items to hint-less Unmeasured (heights read 0
                    // until the next paint) and, when the viewport-top item is
                    // inside the range, clobber the scroll anchor to the range
                    // start — the end-of-turn up/down jump the spring then has
                    // to walk back. `remeasure_items` keeps old sizes as hints
                    // and holds the anchor across the remeasure.
                    self.list.remeasure_items(old_range);
                } else {
                    self.list.splice(old_range, count);
                }
                self.viewport_layout_revision = self.viewport_layout_revision.wrapping_add(1);
            }
        }
        self.rows = new_rows;
        self.refresh_protected_attachments(cx);
        self.reconcile_own_turn_prompt();
        self.restore_pending_viewport(replay);
        if self.land_end_pending && !self.rows.is_empty() {
            // First content for an unpinned override tab: land at the end.
            // `scroll_to_end` is ITEM-anchored (past-the-end offset that the
            // next layout materializes) — a pixel scroll off `max_offset`
            // would land short here, since the freshly-spliced rows are
            // still unmeasured. Short content clamps back to the top under
            // Top alignment, so "end" and "top" coincide there.
            self.land_end_pending = false;
            self.list.scroll_to_end();
        }
        if self.own_turn.is_some() {
            // Appending a reply moves the runway from the previous last row to
            // the new one. Both measurements must be invalidated because the
            // row diff itself only knows that rows were appended at the tail.
            if let Some(old_last) = old_last.filter(|&ix| ix < self.rows.len()) {
                self.list.remeasure_items(old_last..old_last + 1);
            }
            self.remeasure_last_row();
            self.own_turn_kick = true;
        }
        if self.pinned {
            if live_following {
                self.list.scroll_to_end();
                self.spring.reset();
                self.spring_last_tick = None;
                self.spring_settled_at = None;
                self.spring_kick = false;
                self.last_scroll_distance = 0.0;
            } else {
                if motion::reduced_motion(cx) || was_empty {
                    // First fill (chat open) lands at the bottom instantly
                    // (mugen initialScroll:'bottom'); reduced motion snaps.
                    self.list.scroll_to_end();
                } else if self.is_glued() {
                    // A glued offset (`None` / anchored past the end) makes
                    // the upcoming layout hard-snap to the new end — the
                    // per-commit stutter. Materialize a pixel anchor a hair
                    // above the bottom so layout holds position and the
                    // spring glides the growth.
                    self.list.scroll_by(px(-0.75));
                }
                self.spring_kick = true;
            }
        }
        cx.notify();
    }

    /// Fold state for one chip's detail card, stamped as the most recently
    /// toggled and pruned to [`MAX_TOOL_DETAILS`] when it is a new key. The
    /// click handler keeps writing the returned entry exactly as before.
    fn tool_detail_fold_mut(&mut self, key: SharedString) -> &mut FoldState {
        self.tool_touch_counter = self.tool_touch_counter.wrapping_add(1);
        let touched = self.tool_touch_counter;
        match self.tool_details.get_mut(&key) {
            Some(entry) => entry.touched = touched,
            None => {
                self.tool_details.insert(
                    key.clone(),
                    FoldState {
                        touched,
                        ..FoldState::default()
                    },
                );
                prune_tool_details(&mut self.tool_details, &key);
            }
        }
        self.tool_details
            .get_mut(&key)
            .expect("tool detail fold was just inserted")
    }

    /// Fetch a sidecar blob (full tool output or diff) and build its upgraded
    /// [`ToolDetail`] once, off the render path. Re-entry while Loading/Ready
    /// is a no-op; Failed re-arms as a retry (the affordance label says so).
    fn spawn_blob_fetch(&mut self, blob_ref: SharedString, cx: &mut Context<Self>) {
        // Rank BEFORE the already-fetched guard: clicking a Ready ref is the
        // "show me this one again" toggle (recency bump + repaint, no
        // re-fetch) — with both a diff and an output fetched, the two
        // affordances must be able to trade places forever.
        self.blob_fetch_counter += 1;
        self.blob_fetch_order
            .insert(blob_ref.clone(), self.blob_fetch_counter);
        match self.blob_details.get(&blob_ref) {
            Some(BlobFetch::Ready(_)) => {
                cx.notify();
                return;
            }
            Some(BlobFetch::Loading(_)) => return,
            Some(BlobFetch::Failed) | None => {}
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            // The recency bump above has no matching cache entry — drop it so
            // `blob_fetch_order` cannot accumulate keys that no eviction can
            // ever reach (the maps stay in lockstep).
            self.blob_fetch_order.remove(&blob_ref);
            return;
        };
        let is_diff = blob_ref.ends_with(".diff");
        let ref_key = blob_ref.clone();
        let task = cx.spawn(async move |this, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                hearth_rpc::methods::FETCH_TOOL_BLOB,
                serde_json::json!({ "blobRef": ref_key.as_ref() }),
                Duration::from_secs(20),
            )
            .await;
            let fetched = match reply {
                Ok(value) => {
                    let text = value
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default();
                    blob_detail(text, is_diff)
                        .map(|d| BlobFetch::Ready(Arc::new(d)))
                        .unwrap_or(BlobFetch::Failed)
                }
                Err(_) => BlobFetch::Failed,
            };
            this.update(cx, |this, cx| {
                // The slot may have been evicted while this fetch was in
                // flight. Eviction drops the `Task`, which normally cancels
                // this very callback — guard anyway so a late result can
                // never resurrect a key with no bounded slot.
                if !this.blob_details.contains_key(&ref_key) {
                    return;
                }
                this.blob_details.insert(ref_key.clone(), fetched);
                prune_blob_cache(&mut this.blob_details, &mut this.blob_fetch_order, &ref_key);
                cx.notify();
            })
            .ok();
        });
        self.blob_details
            .insert(blob_ref.clone(), BlobFetch::Loading(task));
        prune_blob_cache(
            &mut self.blob_details,
            &mut self.blob_fetch_order,
            &blob_ref,
        );
    }
}
