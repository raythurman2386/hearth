//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, streaming markdown, and the message rail.
//!
//! Row model (docs/research/mugen-pretext.md §3):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups
//!   (agent/spawn chips split out so they never collapse) and input/error chips;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}`;
//! - rows are cached per entry keyed by a content fingerprint;
//! - row-set changes diff by (id, version) into one minimal splice.
//!
//! Rune owns the scroll physics: the viewport is `ScrollSpec { stick_bottom:
//! true }`, so it follows the growing tail, releases the end on wheel input,
//! and re-pins inside the end band. [`viewport`] keeps the pure 70 px policy
//! and own-turn reservation math for callers that need it.

mod attachments;
mod chips;
mod highlights;
mod model;
mod render;
mod rows;
mod viewport;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender};

use crate::attachments::PreviewImage;
use crate::markdown::parser::{BlockTree, IncrementalParser};
use crate::state::App;

use chips::{FoldState, blob_detail};
use highlights::HighlightStore;
use viewport::{
    OwnTurnAnchor, OWN_SEND_SCROLL_SLACK_PX, OWN_SEND_TOP_INSET_PX, SavedViewport,
    SavedViewportCache, TranscriptReplayState, own_turn_reservation,
};

pub use attachments::{ATT_STRIP_H, ATT_THUMB_H, ATT_THUMB_W};
pub use chips::{
    BLOB_AFFORDANCE_HEIGHT, CHIP_CARD_HEIGHT, CHIP_GAP, CHIP_HEIGHT, ChipAffordance, format_kb,
};
pub use hearth_proto::view::{single_line, tool_chip_content};
pub use model::{
    CALL_WRAP_COLS, DIFF_DETAIL_MAX_LINES, FLAVOUR_ROTATE_SECS, FLAVOUR_WORDS,
    OUTPUT_DETAIL_MAX_LINES, OUTPUT_LINE_HEIGHT, Row, RowKind, ToolDetail, ToolItem, call_block,
    chips_height, detail_height, diff_to_file, flavour_seed, flavour_word, format_elapsed,
    format_timestamp, sending_bridge, tool_detail, tool_group_summary,
};
pub use render::{
    MAX_CONTENT_WIDTH, OVERDRAW_PX, VIEWPORT_KEY, lightbox_overlay, render, render_subagent,
};
pub use rows::{ParseOutcome, diff_rows, parse_for_row, rows_for_entry, top_gap_for};
pub use viewport::{
    AT_BOTTOM_PX, MAX_SAVED_VIEWPORTS, RestoredViewport, SCROLL_BUTTON_THRESHOLD_PX,
    STICK_THRESHOLD_PX, SavedViewportCache as ViewportCache, should_restick,
};

/// Cap on cached sidecar blob details (full diff/output bodies).
pub const MAX_BLOB_DETAILS: usize = 256;
/// Cap on retained tool-detail fold states.
pub const MAX_TOOL_DETAILS: usize = 512;
/// Cap on cached subagent entry row builds (across every watched doc).
pub const MAX_SUBAGENT_ROWS: usize = 2_048;

/// A scroll the viewport owes after a replay/own-turn change. Consumed by the
/// layout handler on the transcript's rows (the only place a `Tx` exists).
#[derive(Clone, Debug, PartialEq)]
enum ScrollRequestKind {
    /// Jump to the end without a glide (a tail-following chat reopened).
    Tail,
    /// Place `key`'s top `at` points below the viewport top.
    Row { key: String, at: f32 },
}

/// The own-turn hold step owed to the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
enum OwnHold {
    None,
    /// The entry glide has not landed: reveal the prompt, then place it `at`
    /// points down on the following commit (a virtual row that isn't mounted
    /// has no layout for `scroll_key_to` to use yet).
    Position(f32),
    /// Hold the prompt where it shows across the next content change.
    Keep,
    /// The reservation is spent (the reply outgrew it): hand back to the pin.
    Release,
}

/// One sidecar blob fetch's lifecycle.
pub enum BlobFetch {
    Loading,
    /// Failed with the affordance re-armed as a retry.
    Failed,
    Ready(std::sync::Arc<ToolDetail>),
}

type BlobResult = (String, Option<ToolDetail>);

/// Transcript state: the row set, render-local folds, async sidecar fetches and
/// the thin scroll/rail state.
///
/// Fields read or refreshed from `render(&App, ..)` are interior-mutable
/// (`RefCell`/`Cell`) because Rune views take `&App`; handlers take `&mut App`
/// and mutate the plain fields directly.
pub struct TranscriptState {
    /// Current rows, rebuilt by [`TranscriptState::sync`] at the start of a
    /// render pass.
    pub rows: RefCell<Vec<Row>>,
    /// Hash of `(id, version)` across the row set; feeds
    /// `virtual_rows_version` so a same-width splice is detected.
    pub row_version: Cell<u64>,
    /// The chat these rows belong to.
    pub chat_id: RefCell<Option<String>>,
    /// `Some(doc_id)` pins this state to a SUBAGENT doc (read-only).
    pub doc_override: Option<String>,
    /// Whether an override instance watches a LIVE doc.
    pub doc_live: bool,

    row_cache: RefCell<HashMap<String, rows::CachedRows>>,
    live_parsers: RefCell<HashMap<String, IncrementalParser>>,
    tree_cache: RefCell<HashMap<String, (usize, std::sync::Arc<BlockTree>)>>,
    /// Per-entry row builds for subagent docs, keyed `"{doc}\u{1}{entry}"`.
    sub_cache: RefCell<HashMap<String, rows::CachedRows>>,
    sub_recency: RefCell<VecDeque<String>>,
    pub highlights: RefCell<HighlightStore>,

    /// Group folds keyed by row id.
    pub folds: HashMap<String, FoldState>,
    /// Detail folds per chip, keyed `"{row_id}#d{ix}"`.
    pub tool_details: HashMap<String, FoldState>,
    tool_touch_counter: u64,

    /// Sidecar blob fetches keyed by doc ref (`chatId/partId[.diff]`).
    pub blob_details: HashMap<String, BlobFetch>,
    /// Monotonic fetch order per blob ref.
    pub blob_fetch_order: HashMap<String, u64>,
    blob_fetch_counter: u64,
    blob_tx: Sender<BlobResult>,
    blob_rx: Receiver<BlobResult>,

    /// Session-local viewport snapshots (LRU 256).
    pub saved_viewports: RefCell<SavedViewportCache>,
    /// A snapshot waiting for the incoming chat's replay to resolve it.
    pending_restore: RefCell<Option<SavedViewport>>,
    /// Scroll the viewport owes on the next layout (consumed by
    /// [`TranscriptState::apply_layout_requests`]).
    scroll_request: RefCell<Option<ScrollRequestKind>>,
    /// The end-follow pin (mirrors the Rune viewport's `on_at_bottom`).
    pub pinned: Cell<bool>,
    /// Far enough from the end that the shell's jump pill should float.
    pub show_jump_button: Cell<bool>,
    /// Distance from the bottom at the last observation.
    pub last_scroll_distance: Cell<f32>,
    /// The most recent locally-sent turn and the reservation its hold needs.
    pub own_turn: RefCell<Option<OwnTurnAnchor>>,
    own_hold: Cell<OwnHold>,
    /// First visible row index (from `on_visible_rows`).
    pub visible_start: Cell<usize>,
    /// The transcript viewport's last measured size (`on_layout`/`Cx::layout`).
    pub viewport_size: Cell<[f32; 2]>,
    /// The rows stack's last measured height: the layout handler uses a height
    /// change (content arrived, a runway resized) to distinguish growth from a
    /// plain scroll, so the own-turn hold is not re-applied on every scroll
    /// frame.
    rows_height: Cell<f32>,
    /// Concrete content offset that lands the held prompt at its inset, when
    /// both rects were measurable in the view build.
    own_scroll_target: Cell<Option<f32>>,
    /// The transcript viewport's live scroll offset (`on_scroll_offset`): it
    /// cannot change state, so capture reads it from here.
    pub scroll_offset: rune::Live<[f32; 2]>,

    /// Width gate for [`crate::rail`] (the shell may drive it).
    pub rail_enabled: bool,
    /// Hovered rail bucket.
    pub rail_hover: Option<usize>,
    /// Height of the shell's composer/status/terminal stack overlaying the
    /// transcript's bottom.
    pub bottom_clearance: f32,

    /// `(row id, entry id)` under the pointer.
    pub hovered_entry: Option<(String, String)>,
    /// Code block showing "Copied" feedback: `(row id, block ix)`.
    pub copied_code: Option<(String, usize)>,
    /// Entry whose hover action is showing copied feedback.
    pub copied_message: Option<String>,
    /// Transcript attachment being viewed full-size.
    pub attachment_preview: Option<PreviewImage>,
}

impl Default for TranscriptState {
    fn default() -> Self {
        let (blob_tx, blob_rx) = std::sync::mpsc::channel();
        Self {
            rows: RefCell::new(Vec::new()),
            row_version: Cell::new(0),
            chat_id: RefCell::new(None),
            doc_override: None,
            doc_live: false,
            row_cache: RefCell::new(HashMap::new()),
            live_parsers: RefCell::new(HashMap::new()),
            tree_cache: RefCell::new(HashMap::new()),
            sub_cache: RefCell::new(HashMap::new()),
            sub_recency: RefCell::new(VecDeque::new()),
            highlights: RefCell::new(HighlightStore::default()),
            folds: HashMap::new(),
            tool_details: HashMap::new(),
            tool_touch_counter: 0,
            blob_details: HashMap::new(),
            blob_fetch_order: HashMap::new(),
            blob_fetch_counter: 0,
            blob_tx,
            blob_rx,
            saved_viewports: RefCell::new(SavedViewportCache::default()),
            pending_restore: RefCell::new(None),
            scroll_request: RefCell::new(None),
            pinned: Cell::new(true),
            show_jump_button: Cell::new(false),
            last_scroll_distance: Cell::new(0.0),
            own_turn: RefCell::new(None),
            own_hold: Cell::new(OwnHold::None),
            visible_start: Cell::new(0),
            viewport_size: Cell::new([0.0, 0.0]),
            rows_height: Cell::new(-1.0),
            own_scroll_target: Cell::new(None),
            scroll_offset: rune::Live::new([0.0, 0.0]),
            rail_enabled: true,
            rail_hover: None,
            bottom_clearance: 0.0,
            hovered_entry: None,
            copied_code: None,
            copied_message: None,
            attachment_preview: None,
        }
    }
}

impl TranscriptState {
    /// A read-only state pinned to a subagent doc.
    pub fn for_doc(doc_id: impl Into<String>, live: bool) -> Self {
        Self {
            doc_override: Some(doc_id.into()),
            doc_live: live,
            rail_enabled: false,
            ..Self::default()
        }
    }

    /// Rebuild rows from app state. Cheap when nothing changed (the per-entry
    /// fingerprint cache short-circuits every untouched completed entry).
    ///
    /// On a chat switch this first snapshots the OUTGOING chat's viewport
    /// (row anchor + distance, and its own-turn runway), then resolves the
    /// incoming chat's snapshot once its replay has arrived — the Rune port of
    /// the gpui `remember_current_viewport` / `restore_pending_viewport` pair.
    pub fn sync(&self, app: &App) {
        let selected = match &self.doc_override {
            Some(doc_id) => Some(doc_id.clone()),
            None => app.selected_chat.clone(),
        };
        let attached = *self.chat_id.borrow() != selected;
        if attached {
            self.remember_current_viewport();
            // Read the incoming snapshot BEFORE inserting the outgoing one: a
            // full cache may evict its oldest entry, which can be exactly the
            // chat being reopened.
            let incoming = selected
                .as_ref()
                .and_then(|chat_id| self.saved_viewports.borrow_mut().get_cloned_and_touch(chat_id));
            let keep_own_turn = self
                .own_turn
                .borrow()
                .as_ref()
                .is_some_and(|anchor| selected.as_deref() == Some(anchor.chat_id.as_str()));
            if !keep_own_turn {
                *self.own_turn.borrow_mut() = None;
                self.own_hold.set(OwnHold::None);
                self.own_scroll_target.set(None);
            }
            self.row_cache.borrow_mut().clear();
            self.live_parsers.borrow_mut().clear();
            self.tree_cache.borrow_mut().clear();
            *self.chat_id.borrow_mut() = selected;
            *self.pending_restore.borrow_mut() = incoming;
            *self.scroll_request.borrow_mut() = None;
            self.visible_start.set(0);
            // New chats and tail-following ones open at the bottom; an
            // anchored snapshot overrides this once replay resolves it.
            self.pinned.set(self.own_turn.borrow().is_none());
            self.last_scroll_distance.set(0.0);
            self.show_jump_button.set(false);
        }

        let (entries, pending_count) = match &self.doc_override {
            Some(doc_id) => (app.sub_transcript(doc_id).to_vec(), 0),
            None => {
                let mut entries = app.transcript.clone();
                let pending = app.pending_echoes().len();
                entries.extend_from_slice(app.pending_echoes());
                (entries, pending)
            }
        };

        let mut new_rows: Vec<Row> = Vec::new();
        let split = entries.len().saturating_sub(pending_count);
        for (ix, entry) in entries.iter().enumerate() {
            new_rows.extend(self.rows_for(entry, ix >= split));
        }

        if attached || self.pending_restore.borrow().is_some() {
            self.resolve_pending_restore(app, &new_rows);
        }

        let version = row_set_version(&new_rows);
        *self.rows.borrow_mut() = new_rows;
        self.row_version.set(version);
    }

    /// Snapshot the outgoing primary chat before its rows and caches reset.
    ///
    /// Empty rows never overwrite an older snapshot: during a rapid A→B→A
    /// switch, B's replay may not have arrived before leaving it again. A
    /// snapshot still waiting for replay also protects the older one.
    fn remember_current_viewport(&self) {
        if self.pending_restore.borrow().is_some() {
            return;
        }
        let Some(chat_id) = self.chat_id.borrow().clone() else {
            return;
        };
        let distance_from_bottom = if self.pinned.get() {
            0.0
        } else {
            // The live offset is two frames old at worst; the pin flag is
            // authoritative for a tail-following chat.
            let live = self.scroll_offset.get()[1];
            if live > 0.0 {
                live
            } else {
                self.last_scroll_distance.get()
            }
        };
        let saved = SavedViewport::capture(
            &self.rows.borrow(),
            self.visible_start.get(),
            self.pinned.get(),
            distance_from_bottom,
            self.own_turn.borrow().as_ref(),
        );
        if let Some(saved) = saved {
            self.saved_viewports
                .borrow_mut()
                .insert(chat_id, saved);
        }
    }

    /// Resolve the incoming chat's snapshot against its replay.
    ///
    /// An exact optimistic row is safe while replay is pending; entry/index
    /// fallbacks only once the authoritative doc rows are populated. An
    /// authoritative empty reset retires the snapshot, and a chat with no
    /// snapshot opens at the tail.
    fn resolve_pending_restore(&self, app: &App, rows: &[Row]) {
        let replay = match &self.doc_override {
            Some(_) => {
                if rows.is_empty() {
                    TranscriptReplayState::Pending
                } else {
                    TranscriptReplayState::Populated
                }
            }
            None if !app.transcript_replayed => TranscriptReplayState::Pending,
            None if app.transcript.is_empty() => TranscriptReplayState::Empty,
            None => TranscriptReplayState::Populated,
        };
        let Some(pending) = self.pending_restore.borrow().clone() else {
            // A kept own-turn hold (send-created chat) owns the viewport.
            return;
        };
        if !rows.is_empty() {
            let restored = match &pending {
                SavedViewport::FollowTail => Some(RestoredViewport {
                    row_id: rows[rows.len() - 1].id.clone(),
                    offset_in_row: 0.0,
                    distance_from_bottom: 0.0,
                    own_turn: None,
                }),
                SavedViewport::Anchored { .. } => {
                    pending.resolve(rows, replay.allows_fallback())
                }
            };
            let Some(restored) = restored else {
                return;
            };
            *self.pending_restore.borrow_mut() = None;
            *self.scroll_request.borrow_mut() = Some(match &pending {
                SavedViewport::FollowTail => ScrollRequestKind::Tail,
                SavedViewport::Anchored { .. } => ScrollRequestKind::Row {
                    key: restored.row_id.clone(),
                    at: -restored.offset_in_row,
                },
            });
            let follows_tail = matches!(&pending, SavedViewport::FollowTail);
            *self.own_turn.borrow_mut() = restored.own_turn.clone();
            self.own_hold.set(if restored.own_turn.is_some() {
                OwnHold::Keep
            } else {
                OwnHold::None
            });
            self.pinned.set(follows_tail);
            let distance = if follows_tail {
                0.0
            } else {
                restored.distance_from_bottom
            };
            self.last_scroll_distance.set(distance);
            self.show_jump_button
                .set(distance > SCROLL_BUTTON_THRESHOLD_PX);
            return;
        }
        if !replay.authoritative_empty() {
            return;
        }
        // The reset's document rows, not the combined rows, define
        // authoritative emptiness: a matching optimistic row above remains
        // valid, but an unrelated echo must never become an index fallback.
        *self.pending_restore.borrow_mut() = None;
        if self.own_turn.borrow().is_none() {
            self.pinned.set(true);
            self.last_scroll_distance.set(0.0);
            self.show_jump_button.set(false);
            *self.scroll_request.borrow_mut() = Some(ScrollRequestKind::Tail);
        }
    }

    /// Fold state for one chip's detail card, stamped as the most recently
    /// toggled and pruned to [`MAX_TOOL_DETAILS`] when it is a new key.
    pub fn tool_detail_fold_mut(&mut self, key: String) -> &mut FoldState {
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

    /// The effective detail a chip shows: the doc-resident one, upgraded in
    /// place by the most recently requested sidecar blob.
    pub fn effective_detail(&self, tool: &ToolItem) -> Option<std::sync::Arc<ToolDetail>> {
        let mut best: Option<(u64, std::sync::Arc<ToolDetail>)> = None;
        for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
            if let Some(BlobFetch::Ready(detail)) = self.blob_details.get(blob_ref) {
                let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                if best
                    .as_ref()
                    .is_none_or(|(best_order, _)| order > *best_order)
                {
                    best = Some((order, detail.clone()));
                }
            }
        }
        best.map(|(_, detail)| detail)
            .or_else(|| tool.detail.clone())
    }

    /// The fetch affordance under an open detail, if its full payload is still
    /// sidecar-only.
    pub fn affordance_for(&self, tool: &ToolItem) -> Option<ChipAffordance> {
        let shown: Option<&String> = {
            let mut best: Option<(u64, &String)> = None;
            for blob_ref in [&tool.diff_ref, &tool.output_ref].into_iter().flatten() {
                if matches!(self.blob_details.get(blob_ref), Some(BlobFetch::Ready(_))) {
                    let order = self.blob_fetch_order.get(blob_ref).copied().unwrap_or(0);
                    if best.is_none_or(|(best_order, _)| order > best_order) {
                        best = Some((order, blob_ref));
                    }
                }
            }
            best.map(|(_, blob_ref)| blob_ref)
        };
        let candidates = [
            (tool.diff_ref.as_ref(), "diff", None),
            (tool.output_ref.as_ref(), "output", tool.output_bytes),
        ];
        for (blob_ref, what, bytes) in candidates {
            let Some(blob_ref) = blob_ref else { continue };
            let label = match self.blob_details.get(blob_ref) {
                Some(BlobFetch::Ready(_)) => {
                    if shown == Some(blob_ref) {
                        continue;
                    }
                    format!("Show full {what}")
                }
                Some(BlobFetch::Loading) => format!("Loading full {what}…"),
                Some(BlobFetch::Failed) => {
                    format!("Couldn't load full {what} — tap to retry")
                }
                None => match bytes {
                    Some(bytes) => format!("Show full {what} ({})", format_kb(bytes)),
                    None => format!("Show full {what}"),
                },
            };
            return Some(ChipAffordance {
                blob_ref: blob_ref.clone(),
                label,
            });
        }
        None
    }

    /// Drain async sidecar results. Called by `App::poll`; returns whether
    /// visible state changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok((blob_ref, detail)) = self.blob_rx.try_recv() {
            if !self.blob_details.contains_key(&blob_ref) {
                continue;
            }
            match detail {
                Some(detail) => {
                    self.blob_details.insert(
                        blob_ref.clone(),
                        BlobFetch::Ready(std::sync::Arc::new(detail)),
                    );
                }
                None => {
                    self.blob_details
                        .insert(blob_ref.clone(), BlobFetch::Failed);
                }
            }
            prune_blob_cache(
                &mut self.blob_details,
                &mut self.blob_fetch_order,
                &blob_ref,
            );
            changed = true;
        }
        changed
    }

    /// Hand viewport ownership to a send: reserve the turn's runway so the
    /// prompt can be held near the top while the reply grows below it.
    pub fn on_own_send(&mut self, chat_id: String, message_id: String) {
        let seen_prompt = self
            .rows
            .borrow()
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id);
        *self.own_turn.borrow_mut() = Some(OwnTurnAnchor {
            chat_id,
            message_id,
            runway: 0.0,
            held: true,
            positioned: false,
            seen_prompt,
        });
        self.own_hold.set(OwnHold::None);
        self.own_scroll_target.set(None);
        self.pinned.set(false);
        self.show_jump_button.set(false);
    }

    /// Explicit user navigation takes the viewport: drop a replay-delayed
    /// restore (its snapshot becomes tail-follow) and release the own-turn
    /// hold so the jump/rail move is not pulled back.
    pub fn begin_scroll_navigation(&mut self) {
        self.release_own_turn();
        *self.scroll_request.borrow_mut() = None;
        if self.pending_restore.borrow_mut().take().is_some()
            && let Some(chat_id) = self.chat_id.borrow().clone()
        {
            self.saved_viewports
                .borrow_mut()
                .insert(chat_id, SavedViewport::FollowTail);
        }
    }

    /// Drop the own-turn hold (a wheel, rail jump or jump-to-bottom pill).
    pub fn release_own_turn(&mut self) {
        *self.own_turn.borrow_mut() = None;
        self.own_hold.set(OwnHold::None);
        self.own_scroll_target.set(None);
    }

    /// The measured height of the transcript viewport (0 until first layout).
    pub fn viewport_height(&self) -> f32 {
        self.viewport_size.get()[1]
    }

    /// Size the own-turn reservation for this frame and record the hold step
    /// the next layout commit should take.
    ///
    /// gpui stepped this after layout with the anchor's and last row's real
    /// bounds; Rune reads the same rects through `memo::layout` during the
    /// view build (one frame behind, which is exactly the post-layout cadence
    /// the gpui step ran at). A fresh anchor takes the full usable height as a
    /// provisional pad — the prompt sits below the fold unmeasured, and
    /// without the pad there is no scroll room to bring it to its inset.
    pub fn update_own_turn(&self, rows: &[Row]) {
        let mut slot = self.own_turn.borrow_mut();
        let Some(anchor) = slot.as_mut() else {
            return;
        };
        if self.chat_id.borrow().as_deref() != Some(anchor.chat_id.as_str()) {
            return;
        }
        let prompt_ix = rows
            .iter()
            .position(|row| row.turn_start && row.entry_id == anchor.message_id);
        let keep = anchor.observe_prompt(prompt_ix.is_some());
        if !keep {
            *slot = None;
            self.own_hold.set(OwnHold::Release);
            return;
        }
        let Some(prompt_ix) = prompt_ix else {
            return;
        };
        let viewport_h = self.viewport_height();
        if viewport_h <= 0.0 {
            return;
        }
        let inset = if prompt_ix == 0 {
            0.0
        } else {
            OWN_SEND_TOP_INSET_PX
        };
        let base_pad = self.bottom_clearance + crate::theme::Theme::TRANSCRIPT_FADE_BAND + 8.0;
        let usable = (viewport_h - inset - base_pad + OWN_SEND_SCROLL_SLACK_PX).max(0.0);
        let last_ix = rows.len().saturating_sub(1);
        let measured = rune::memo::layout(&rows[prompt_ix].id)
            .zip(rune::memo::layout(&rows[last_ix].id));
        let target = match measured {
            Some((prompt, last)) => {
                // The runway and bottom clearance are a spacer after the list,
                // not part of the last row, so the turn's height is just the
                // span from its prompt's top to the last row's bottom.
                let turn_height = last[1] + last[3] - prompt[1];
                own_turn_reservation(usable, turn_height)
            }
            None => usable,
        };
        if target <= 0.5 {
            *slot = None;
            self.own_hold.set(OwnHold::Release);
            return;
        }
        if (target - anchor.runway).abs() > 0.5 {
            anchor.runway = target;
        }
        // The glide has landed once the prompt's top sits at its inset. Read
        // the last frame's rects (a view build can read layout; a handler
        // cannot), so the hold switches from placement to `keep_on_screen`
        // only after the scroll actually took.
        if let (Some(prompt), Some(viewport)) = (
            rune::memo::layout(&rows[prompt_ix].id),
            rune::memo::layout(render::VIEWPORT_KEY),
        ) {
            let offset = prompt[1] - viewport[1];
            if (offset - inset).abs() <= 8.0 {
                anchor.positioned = true;
                self.own_scroll_target.set(None);
            } else if !anchor.positioned {
                // Concrete content offset for `Tx::scroll_to`: the window
                // delta plus where the viewport is scrolled now. This avoids
                // depending on a reveal in the same commit.
                let current = self.scroll_offset.get()[0];
                self.own_scroll_target
                    .set(Some((current + offset - inset).max(0.0)));
            }
        }
        self.own_hold.set(if anchor.held && !anchor.positioned {
            OwnHold::Position(inset)
        } else {
            OwnHold::Keep
        });
    }

    /// Whether a layout commit has scroll work owed to it (so the render can
    /// mount the handler that owns the `Tx` only when needed).
    pub fn layout_request_pending(&self) -> bool {
        self.scroll_request.borrow().is_some() || self.own_hold.get() != OwnHold::None
    }

    /// Note a rows-stack layout and apply the restore/own-turn scroll the
    /// viewport owes. `content_changed` is true only when the stack's height
    /// moved (a replay splice, a runway resize, streamed growth), which is the
    /// commit `keep_on_screen` belongs to — not every scroll frame.
    pub fn apply_rows_layout(&self, height: f32, tx: &mut rune::Tx) {
        let changed = self.rows_height.replace(height) != height;
        self.apply_layout_requests(changed, tx);
    }

    /// Apply the restore/own-turn scroll the viewport owes. Called from the
    /// rows' (and viewport's) `on_layout_tx`, the only place a `Tx` exists.
    ///
    /// Rune applies reveals before offset requests in the same commit, so a
    /// row that isn't mounted is revealed first and then placed at its offset
    /// (`scroll_key_to`) by the same handler.
    pub fn apply_layout_requests(&self, content_changed: bool, tx: &mut rune::Tx) {
        let mut queued = false;
        if let Some(request) = self.scroll_request.borrow_mut().take() {
            queued = true;
            match request {
                ScrollRequestKind::Tail => tx.scroll_to_bottom_instant(render::VIEWPORT_KEY),
                ScrollRequestKind::Row { key, at } => {
                    tx.scroll_into_view(key.clone());
                    tx.scroll_key_to(key, at);
                }
            }
        }
        let hold = self.own_hold.replace(OwnHold::None);
        if hold == OwnHold::None {
            return;
        }
        if hold == OwnHold::Release {
            self.pinned.set(true);
            tx.scroll_to_bottom(render::VIEWPORT_KEY);
            return;
        }
        let prompt_key = self
            .own_turn
            .borrow()
            .as_ref()
            .map(|anchor| anchor.message_id.clone());
        let Some(prompt_key) = prompt_key else {
            return;
        };
        match hold {
            OwnHold::Position(inset) => {
                if let Some(target) = self.own_scroll_target.take() {
                    tx.scroll_to(render::VIEWPORT_KEY, 0.0, target);
                } else {
                    // The prompt's rect is not known yet: reveal it, then let
                    // the next measurable view build place it.
                    tx.scroll_into_view(prompt_key.clone());
                    tx.scroll_key_to(prompt_key, inset);
                }
            }
            OwnHold::Keep => {
                // Hold the prompt's top across the content change that just
                // laid out: the Rune-native form of the gpui absolute hold.
                if content_changed {
                    tx.keep_on_screen(prompt_key);
                } else if !queued {
                    // A scroll frame must not re-run the view for bookkeeping.
                    tx.live_only();
                }
            }
            OwnHold::None | OwnHold::Release => {}
        }
    }
}

pub(crate) fn row_set_version(rows: &[Row]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for row in rows {
        for byte in row.id.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1_0000_01b3);
        }
        for byte in row.version.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x1_0000_01b3);
        }
    }
    hash
}

/// Oldest eviction candidate in the blob cache.
fn oldest_blob_candidate(
    details: &HashMap<String, BlobFetch>,
    order: &HashMap<String, u64>,
    keep: &str,
    settled_only: bool,
) -> Option<String> {
    details
        .iter()
        .filter(|(blob_ref, fetch)| {
            blob_ref.as_str() != keep && !(settled_only && matches!(fetch, BlobFetch::Loading))
        })
        .min_by_key(|(blob_ref, _)| order.get(*blob_ref).copied().unwrap_or(0))
        .map(|(blob_ref, _)| blob_ref.clone())
}

/// Evict the least-recently-fetched refs until the blob cache is within
/// [`MAX_BLOB_DETAILS`].
fn prune_blob_cache(
    details: &mut HashMap<String, BlobFetch>,
    order: &mut HashMap<String, u64>,
    keep: &str,
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

/// Evict the least recently toggled tool-detail folds.
fn prune_tool_details(folds: &mut HashMap<String, FoldState>, keep: &str) {
    while folds.len() > MAX_TOOL_DETAILS {
        let victim = folds
            .iter()
            .filter(|(key, _)| key.as_str() != keep)
            .min_by_key(|(_, fold)| fold.touched)
            .map(|(key, _)| key.clone());
        let Some(victim) = victim else {
            break;
        };
        folds.remove(&victim);
    }
}

/// Request a full tool output/diff blob from the engine.
///
/// Clicking a Ready ref is the "show me this one again" toggle (recency bump +
/// repaint, no re-fetch); Failed re-arms as a retry.
pub fn request_blob(app: &mut App, blob_ref: String) {
    app.transcript_state.blob_fetch_counter += 1;
    let counter = app.transcript_state.blob_fetch_counter;
    app.transcript_state
        .blob_fetch_order
        .insert(blob_ref.clone(), counter);
    match app.transcript_state.blob_details.get(&blob_ref) {
        Some(BlobFetch::Ready(_)) => return,
        Some(BlobFetch::Loading) => return,
        Some(BlobFetch::Failed) | None => {}
    }
    let Some(engine) = app.engine.clone() else {
        app.transcript_state.blob_fetch_order.remove(&blob_ref);
        return;
    };
    let bridge = app.bridge.clone();
    let spawn_bridge = bridge.clone();
    let sender = app.transcript_state.blob_tx.clone();
    let is_diff = blob_ref.ends_with(".diff");
    let ref_key = blob_ref.clone();
    app.transcript_state
        .blob_details
        .insert(blob_ref.clone(), BlobFetch::Loading);
    prune_blob_cache(
        &mut app.transcript_state.blob_details,
        &mut app.transcript_state.blob_fetch_order,
        &blob_ref,
    );
    spawn_bridge.spawn(async move {
        let reply = crate::attachments::call_with_timeout(
            &engine,
            hearth_rpc::methods::FETCH_TOOL_BLOB,
            serde_json::json!({ "blobRef": ref_key.as_str() }),
            std::time::Duration::from_secs(20),
        )
        .await;
        let fetched = match reply {
            Ok(value) => {
                let text = value
                    .get("text")
                    .and_then(|text| text.as_str())
                    .unwrap_or_default();
                blob_detail(text, is_diff)
            }
            Err(_) => None,
        };
        let _ = sender.send((ref_key, fetched));
        bridge.wake();
    });
}

/// Whether the shell should float its "Scroll to bottom" pill.
pub fn transcript_jump_shown(app: &App) -> bool {
    app.transcript_state.show_jump_button.get() && !app.transcript_state.pinned.get()
}

/// The scroll-to-bottom pill's click: glide to the end and re-pin.
pub fn scroll_to_bottom(app: &mut App, tx: &mut rune::Tx) {
    app.transcript_state.begin_scroll_navigation();
    app.transcript_state.pinned.set(true);
    tx.scroll_to_bottom(render::VIEWPORT_KEY);
}

#[cfg(test)]
mod tests;
