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
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

use crate::attachments::PreviewImage;
use crate::markdown::parser::{BlockTree, IncrementalParser};
use crate::state::App;

use chips::{FoldState, blob_detail};
use highlights::HighlightStore;
use viewport::{OwnTurnAnchor, SavedViewportCache};

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
pub use render::{MAX_CONTENT_WIDTH, OVERDRAW_PX, VIEWPORT_KEY, render, render_subagent};
pub use rows::{ParseOutcome, diff_rows, parse_for_row, rows_for_entry, top_gap_for};
pub use viewport::{
    AT_BOTTOM_PX, MAX_SAVED_VIEWPORTS, SCROLL_BUTTON_THRESHOLD_PX, STICK_THRESHOLD_PX,
    SavedViewport, SavedViewportCache as ViewportCache, should_restick,
};

/// Cap on cached sidecar blob details (full diff/output bodies).
pub const MAX_BLOB_DETAILS: usize = 256;
/// Cap on retained tool-detail fold states.
pub const MAX_TOOL_DETAILS: usize = 512;

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
    pub saved_viewports: SavedViewportCache,
    /// The end-follow pin (mirrors the Rune viewport's `on_at_bottom`).
    pub pinned: bool,
    /// Far enough from the end that the shell's jump pill should float.
    pub show_jump_button: bool,
    /// Distance from the bottom at the last observation.
    pub last_scroll_distance: f32,
    /// The most recent locally-sent turn.
    pub own_turn: Option<OwnTurnAnchor>,
    /// First visible row index (from `on_visible_rows`).
    pub visible_start: Cell<usize>,

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
            highlights: RefCell::new(HighlightStore::default()),
            folds: HashMap::new(),
            tool_details: HashMap::new(),
            tool_touch_counter: 0,
            blob_details: HashMap::new(),
            blob_fetch_order: HashMap::new(),
            blob_fetch_counter: 0,
            blob_tx,
            blob_rx,
            saved_viewports: SavedViewportCache::default(),
            pinned: true,
            show_jump_button: false,
            last_scroll_distance: 0.0,
            own_turn: None,
            visible_start: Cell::new(0),
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
    pub fn sync(&self, app: &App) {
        let selected = match &self.doc_override {
            Some(doc_id) => Some(doc_id.clone()),
            None => app.selected_chat.clone(),
        };
        let attached = *self.chat_id.borrow() != selected;
        if attached {
            self.row_cache.borrow_mut().clear();
            self.live_parsers.borrow_mut().clear();
            self.tree_cache.borrow_mut().clear();
            *self.chat_id.borrow_mut() = selected.clone();
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

        let version = row_set_version(&new_rows);
        *self.rows.borrow_mut() = new_rows;
        self.row_version.set(version);
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

    /// Hand viewport ownership to a send: Rune's stick_bottom follows the tail;
    /// the anchor records the turn so callers can reserve its runway.
    pub fn on_own_send(&mut self, chat_id: String, message_id: String) {
        let seen_prompt = self
            .rows
            .borrow()
            .iter()
            .any(|row| row.turn_start && row.entry_id == message_id);
        self.own_turn = Some(OwnTurnAnchor {
            chat_id,
            message_id,
            runway: 0.0,
            held: true,
            positioned: false,
            seen_prompt,
        });
        self.pinned = false;
        self.show_jump_button = false;
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
    app.transcript_state.show_jump_button && !app.transcript_state.pinned
}

/// The scroll-to-bottom pill's click: glide to the end and re-pin.
pub fn scroll_to_bottom(_app: &mut App, tx: &mut rune::Tx) {
    tx.scroll_to_bottom(render::VIEWPORT_KEY);
}

#[cfg(test)]
mod tests;
