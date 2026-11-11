//! The terminal panel: per-chat tabs over engine PTYs, ported from the gpui UI.
//!
//! Data path per tab: `OpenTerminal` → `SubscribeTerminal` stream. Data frames
//! (base64) feed the [`Emulator`]; query responses write back; the stream
//! reconnects with exponential backoff resuming from `afterSeq`; Exit appends
//! the `[process exited N]` line and stops. Keyboard bytes coalesce for 12 ms
//! before `WriteTerminal`; viewport-driven resizes debounce 80 ms before
//! `ResizeTerminal` (the emulator resizes immediately).
//!
//! Rune has no `cx.spawn` that can touch app state, so background tasks post
//! [`TerminalMsg`]s over an `mpsc` channel; [`TerminalState::poll`] applies
//! them on the UI thread. The lead calls `poll` from `App::poll`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use base64::Engine as _;
use hearth_proto::{TerminalEvent, TerminalSession};
use hearth_rpc::methods;
use rune::{
    Color, Cursor, Drag, El, KeyEvent, Phase, PointerButton, Span, Spring, TextStyle, Tx,
    WheelEvent, el, icon, row, text,
};

use crate::bridge::Bridge;
use crate::engine::EngineHandle;
use crate::icons;
use crate::settings::{TERMINAL_MAX_VH, TERMINAL_MIN_HEIGHT};
use crate::state::App;
use crate::theme::Theme;

use super::emulator::{
    CellColor, CellSnapshot, CursorSnapshot, Emulator, GridPoint, SelectionType, Side,
};
use super::view::{
    COALESCE_MS, InputCoalescer, RESIZE_DEBOUNCE_MS, SELECTION_DRAG_THRESHOLD, TERM_LINE_HEIGHT,
    TERM_PADDING, cell_at, cell_width, key_event_bytes, paste_bytes, resolve_color, term_style,
    terminal_panel_bg, terminal_selection_for,
};

/// Fixed tab width — drag-reorder math stays analytic.
pub const TAB_WIDTH: f32 = 118.0;
pub const TAB_BAR_HEIGHT: f32 = 40.0;
const TAB_BAR_PAD: f32 = 8.0;
const TAB_HEIGHT: f32 = 28.0;
const SCROLLBAR_TRACK_INSET: f32 = 4.0;
const SCROLLBAR_HIT_WIDTH: f32 = 10.0;
const SCROLLBAR_THUMB_WIDTH: f32 = 3.0;
const SCROLLBAR_HOVER_THUMB_WIDTH: f32 = 4.5;
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
/// Tab slide spring while a sibling is dragged over (150 ms).
pub const TAB_SLIDE: Spring = Spring::new(0.15, 0.0);

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Panel height clamp: 160 px … 55 % of the viewport.
pub fn clamp_terminal_height(height: f32, viewport_h: f32) -> f32 {
    let max = (viewport_h * TERMINAL_MAX_VH).max(TERMINAL_MIN_HEIGHT);
    if height.is_finite() {
        height.clamp(TERMINAL_MIN_HEIGHT, max)
    } else {
        TERMINAL_MIN_HEIGHT
    }
}

/// Reconnect backoff: 500 ms doubling to an 8 s ceiling.
pub fn backoff_ms(attempt: u32) -> u64 {
    (500u64 << attempt.min(4)).min(8_000)
}

/// Move a tab from `from` to `to` (indices into the same vec).
pub fn reorder_tabs<T>(tabs: &mut Vec<T>, from: usize, to: usize) {
    if from >= tabs.len() || to >= tabs.len() || from == to {
        return;
    }
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
}

/// Where a drag hovering at `rel_x` inside the tab strip would land.
pub fn drop_index(rel_x: f32, tab_w: f32, count: usize) -> usize {
    if count == 0 || tab_w <= 0.0 {
        return 0;
    }
    ((rel_x / tab_w).floor().max(0.0) as usize).min(count - 1)
}

/// Sliding offset (in tab-width units) for tab `ix` while `from` is dragged
/// over `over`: tabs between the two shift one slot toward the vacated gap.
pub fn slide_offset(ix: usize, from: usize, over: usize) -> f32 {
    if from < over && ix > from && ix <= over {
        -1.0
    } else if over < from && ix >= over && ix < from {
        1.0
    } else {
        0.0
    }
}

/// Active index after a reorder commit.
pub fn active_after_reorder(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// Active index after closing `closed` (given the new, shorter length).
pub fn active_after_close(active: usize, closed: usize, len_after: usize) -> usize {
    let shifted = if closed < active { active - 1 } else { active };
    if len_after == 0 {
        0
    } else {
        shifted.min(len_after - 1)
    }
}

/// The `[process exited N]` trailer, dimmed.
pub fn exit_message(code: i32) -> Vec<u8> {
    format!("\r\n\x1b[90m[process exited {code}]\x1b[0m\r\n").into_bytes()
}

/// Tab title from the session's shell path ("/bin/zsh" → "zsh").
pub fn shell_title(shell: &str) -> String {
    let name = shell.rsplit(['/', '\\']).next().unwrap_or(shell).trim();
    if name.is_empty() {
        "terminal".to_string()
    } else {
        name.to_string()
    }
}

/// Merge the `targetDeviceId` passthrough into RPC params (a no-op for chats on
/// the connected engine's own device).
pub fn with_target(mut params: serde_json::Value, target: &Option<String>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

fn decode_base64(data: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "terminal: dropping undecodable data frame");
            Vec::new()
        })
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Geometry / scrollbar
// ---------------------------------------------------------------------------

/// Where the grid landed this frame, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeometry {
    /// Full terminal body bounds `[x, y, w, h]`.
    pub bounds: [f32; 4],
    /// Top-left of the first glyph (bounds origin plus padding).
    pub origin: [f32; 2],
    pub cell_w: f32,
    pub line_h: f32,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarMetrics {
    pub track_top: f32,
    pub track_height: f32,
    pub thumb_top: f32,
    pub thumb_height: f32,
    pub history_lines: usize,
}

impl ScrollbarMetrics {
    pub fn travel(self) -> f32 {
        (self.track_height - self.thumb_height).max(0.0)
    }
}

/// Thumb/track metrics for a grid body of `bounds`, in body-local points.
fn scrollbar_metrics(
    bounds: [f32; 4],
    rows: usize,
    history_lines: usize,
    display_offset: usize,
) -> Option<ScrollbarMetrics> {
    if history_lines == 0 {
        return None;
    }
    let track_height = (bounds[3] - SCROLLBAR_TRACK_INSET * 2.0).max(0.0);
    if track_height <= 0.0 {
        return None;
    }
    let total_lines = history_lines.saturating_add(rows).max(1);
    let thumb_height = (track_height * rows as f32 / total_lines as f32)
        .max(SCROLLBAR_MIN_THUMB)
        .min(track_height);
    let travel = (track_height - thumb_height).max(0.0);
    let offset = display_offset.min(history_lines);
    let progress_from_top = 1.0 - offset as f32 / history_lines as f32;
    Some(ScrollbarMetrics {
        track_top: SCROLLBAR_TRACK_INSET,
        track_height,
        thumb_top: travel * progress_from_top,
        thumb_height,
        history_lines,
    })
}

/// Terminal scroll direction for a selection near the grid edge: positive
/// scrolls up into history (alacritty's sign convention).
fn selection_scroll_lines(geometry: GridGeometry, y: f32) -> i32 {
    let grid_height = geometry.line_h * geometry.rows as f32;
    if grid_height <= 0.0 {
        return 0;
    }
    let edge = geometry.line_h.min(grid_height / 3.0);
    let top = geometry.origin[1];
    let bottom = top + grid_height;
    let speed = |penetration: f32| {
        let t = (penetration / edge).clamp(0.0, 1.0);
        (1.0 + 2.0 * t * t).round() as i32
    };
    if y < top + edge {
        speed(top + edge - y)
    } else if y > bottom - edge {
        -speed(y - (bottom - edge))
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One background task's message to the UI thread.
#[derive(Debug)]
pub enum TerminalMsg {
    Opened {
        chat: String,
        key: u64,
        terminal_id: String,
        shell: String,
    },
    OpenFailed {
        chat: String,
        key: u64,
        message: String,
    },
    Data {
        chat: String,
        key: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    Exit {
        chat: String,
        key: u64,
        seq: u64,
        exit_code: i32,
    },
}

/// One terminal tab.
pub struct TerminalTab {
    pub key: u64,
    pub title: String,
    pub terminal_id: Option<String>,
    pub emulator: Emulator,
    pub exited: Option<i32>,
    pub last_seq: u64,

    /// Shared with the stream task: the resume point after each applied frame.
    seq_shared: Arc<AtomicU64>,
    /// Set when the tab is closed so the task stops reconnecting.
    alive: Arc<AtomicBool>,
    /// Set when the Exit frame arrived; the task stops after the stream ends.
    exited_shared: Arc<AtomicBool>,
    /// Host device override for remote chats.
    target: Option<String>,
    /// The session's shell basename (tab fallback title).
    shell: Option<String>,

    coalescer: InputCoalescer,
    /// When the first byte of the current burst was queued.
    pending_input_at: Option<Instant>,
    /// When the deferred `ResizeTerminal` first became due.
    resize_due: Option<Instant>,
}

impl std::fmt::Debug for TerminalTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalTab")
            .field("key", &self.key)
            .field("title", &self.title)
            .field("terminal_id", &self.terminal_id)
            .field("exited", &self.exited)
            .field("last_seq", &self.last_seq)
            .finish()
    }
}

/// The tabs of one chat. `active` indexes `tabs`.
#[derive(Debug, Default)]
pub struct ChatTabs {
    pub tabs: Vec<TerminalTab>,
    pub active: usize,
}

/// In-flight tab drag-reorder state.
#[derive(Debug, Clone, Copy, Default)]
pub struct DragState {
    pub from: usize,
    pub over: usize,
    /// Latest pointer travel, for the lifted tab's offset.
    pub dx: f32,
}

/// An in-flight left-button selection gesture.
#[derive(Debug, Clone, Copy)]
struct SelectionDrag {
    position: (f32, f32),
    armed: bool,
}

/// A pointer move during a grid selection drag: where the pointer is, where the
/// press landed, and the drag's total travel.
#[derive(Debug, Clone, Copy)]
pub struct PointerMotion {
    pub x: f32,
    pub y: f32,
    pub start_x: f32,
    pub start_y: f32,
    pub dx: f32,
    pub dy: f32,
}

/// The terminal sub-state.
pub struct TerminalState {
    tx: Sender<TerminalMsg>,
    rx: Receiver<TerminalMsg>,
    pub chats: HashMap<String, ChatTabs>,
    /// Shell-driven visibility gate: no RPC happens while closed (lazy).
    pub open: bool,
    /// Right-pane surface host mode: the shell owns the tab strip, so the
    /// internal bar hides and tabs are only created explicitly.
    pub embedded: bool,
    /// The right pane is mid-tween: keep painting the retained grid but do not
    /// feed transient widths into the emulator.
    pub resize_suspended: bool,
    pub tab_seq: u64,
    pub drag: Option<DragState>,
    /// Last reported grid placement; `None` until the first layout pass.
    pub geometry: Option<GridGeometry>,
    pub terminal_hovered: bool,
    pub scrollbar_hovered: bool,
    selection_drag: Option<SelectionDrag>,
    scrollbar_grab: Option<f32>,
    /// The engine/bridge captured when a tab was opened, used by `poll` to
    /// flush coalesced input and debounced resizes without an `&App`.
    engine: Option<EngineHandle>,
    bridge: Option<Bridge>,
}

impl Default for TerminalState {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            chats: HashMap::new(),
            open: false,
            embedded: false,
            resize_suspended: false,
            tab_seq: 0,
            drag: None,
            geometry: None,
            terminal_hovered: false,
            scrollbar_hovered: false,
            selection_drag: None,
            scrollbar_grab: None,
            engine: None,
            bridge: None,
        }
    }
}

impl std::fmt::Debug for TerminalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalState")
            .field("chats", &self.chats.len())
            .field("open", &self.open)
            .field("embedded", &self.embedded)
            .finish()
    }
}

impl TerminalState {
    pub fn sender(&self) -> Sender<TerminalMsg> {
        self.tx.clone()
    }

    /// Install the engine and bridge that `poll` needs for background writes.
    /// Called automatically by [`Self::open_tab`].
    pub fn attach(&mut self, engine: EngineHandle, bridge: Bridge) {
        self.engine = Some(engine);
        self.bridge = Some(bridge);
    }

    /// Shell toggle hook. Opening lazily creates the first tab for the selected
    /// chat (drawer mode; embedded tabs are explicit); closing keeps every
    /// session alive (detach ≠ close).
    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    pub fn set_embedded(&mut self, embedded: bool) {
        self.embedded = embedded;
    }

    pub fn set_resize_suspended(&mut self, suspended: bool) {
        self.resize_suspended = suspended;
    }

    // ---- derived accessors ----

    pub fn tabs(&self, chat: &str) -> Option<&ChatTabs> {
        self.chats.get(chat)
    }

    pub fn active_tab(&self, chat: &str) -> Option<&TerminalTab> {
        let tabs = self.chats.get(chat)?;
        tabs.tabs.get(tabs.active)
    }

    fn active_tab_mut(&mut self, chat: &str) -> Option<&mut TerminalTab> {
        let tabs = self.chats.get_mut(chat)?;
        tabs.tabs.get_mut(tabs.active)
    }

    fn tab_mut(&mut self, chat: &str, key: u64) -> Option<&mut TerminalTab> {
        self.chats
            .get_mut(chat)?
            .tabs
            .iter_mut()
            .find(|t| t.key == key)
    }

    /// A tab's display label: the live OSC 0/2 title when the running program
    /// set one, else the fixed "Terminal N" (or the shell's basename).
    pub fn display_title(tab: &TerminalTab) -> String {
        match tab.emulator.title().map(str::trim) {
            Some(title) if !title.is_empty() => title.to_string(),
            _ => tab.title.clone(),
        }
    }

    /// `(key, title, exited)` for a chat's tabs, in tab order — the right-pane
    /// surface strip reads this.
    pub fn tab_summaries(&self, chat: &str) -> Vec<(u64, String, bool)> {
        self.chats
            .get(chat)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .map(|t| (t.key, Self::display_title(t), t.exited.is_some()))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---- open / stream lifecycle ----

    /// Open a fresh tab for `chat`.
    pub fn open_tab(
        &mut self,
        chat: String,
        engine: EngineHandle,
        bridge: Bridge,
        target: Option<String>,
    ) {
        self.tab_seq += 1;
        let key = self.tab_seq;
        let (cols, rows) = self.geometry.map(|g| (g.cols, g.rows)).unwrap_or((80, 24));

        let seq_shared = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let exited_shared = Arc::new(AtomicBool::new(false));

        let entry = self.chats.entry(chat.clone()).or_default();
        let tab_no = entry.tabs.len() + 1;
        entry.tabs.push(TerminalTab {
            key,
            title: format!("Terminal {tab_no}"),
            terminal_id: None,
            emulator: Emulator::new(cols, rows),
            exited: None,
            last_seq: 0,
            seq_shared: seq_shared.clone(),
            alive: alive.clone(),
            exited_shared: exited_shared.clone(),
            target: target.clone(),
            shell: None,
            coalescer: InputCoalescer::default(),
            pending_input_at: None,
            resize_due: None,
        });
        entry.active = entry.tabs.len() - 1;

        self.attach(engine.clone(), bridge.clone());
        spawn_session(
            chat,
            key,
            engine,
            bridge,
            self.tx.clone(),
            target,
            seq_shared,
            alive,
            exited_shared,
            cols,
            rows,
        );
    }

    /// Close a tab. Returns true when the chat has no tabs left.
    pub fn close_tab(
        &mut self,
        chat: &str,
        key: u64,
        engine: Option<EngineHandle>,
        bridge: Option<Bridge>,
        target: Option<String>,
    ) -> bool {
        let Some(tabs) = self.chats.get_mut(chat) else {
            return false;
        };
        let Some(ix) = tabs.tabs.iter().position(|t| t.key == key) else {
            return false;
        };
        let tab = tabs.tabs.remove(ix);
        tab.alive.store(false, Ordering::Relaxed);
        tabs.active = active_after_close(tabs.active, ix, tabs.tabs.len());
        let now_empty = tabs.tabs.is_empty();
        self.drag = None;
        if let (Some(engine), Some(id)) = (engine, tab.terminal_id.clone()) {
            let bridge = bridge.or_else(|| self.bridge.clone());
            if let Some(bridge) = bridge {
                let client = engine.client.clone();
                bridge.spawn(async move {
                    let _ = client
                        .call(
                            methods::CLOSE_TERMINAL,
                            with_target(serde_json::json!({ "terminalId": id }), &target),
                        )
                        .await;
                });
            }
        }
        if now_empty {
            self.chats.remove(chat);
        }
        now_empty
    }

    pub fn select_tab(&mut self, chat: &str, ix: usize) {
        if let Some(tabs) = self.chats.get_mut(chat)
            && ix < tabs.tabs.len()
        {
            tabs.active = ix;
        }
    }

    pub fn select_tab_by_key(&mut self, chat: &str, key: u64) {
        let Some(ix) = self
            .chats
            .get(chat)
            .and_then(|tabs| tabs.tabs.iter().position(|t| t.key == key))
        else {
            return;
        };
        self.select_tab(chat, ix);
    }

    // ---- tab drag-reorder ----

    pub fn begin_tab_drag(&mut self, from: usize) {
        self.drag = Some(DragState {
            from,
            over: from,
            dx: 0.0,
        });
    }

    pub fn update_tab_drag(&mut self, chat: &str, ix: usize, local_x: f32, dx: f32) {
        let Some(count) = self.chats.get(chat).map(|t| t.tabs.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        // Pointer position in strip coordinates: the dragged tab carries its own
        // travel through `drag.dx`, and the bar's leading padding is part of the
        // quantized range (as in the gpui listener).
        let rel_x = TAB_BAR_PAD + ix as f32 * TAB_WIDTH + dx + local_x;
        let over = drop_index(rel_x, TAB_WIDTH, count);
        let drag = self.drag.get_or_insert(DragState { from: ix, over, dx });
        drag.over = over;
        drag.dx = dx;
    }

    pub fn commit_tab_drag(&mut self, chat: &str) -> bool {
        let Some(drag) = self.drag.take() else {
            return false;
        };
        let Some(tabs) = self.chats.get_mut(chat) else {
            return false;
        };
        let active = tabs.active;
        reorder_tabs(&mut tabs.tabs, drag.from, drag.over);
        tabs.active = active_after_reorder(active, drag.from, drag.over);
        drag.from != drag.over
    }

    // ---- grid metrics ----

    /// Called from the grid's `on_layout`. Resizes the emulator immediately; the
    /// `ResizeTerminal` RPC debounces [`RESIZE_DEBOUNCE_MS`]. Returns true when
    /// this call newly armed the debounce (the caller should `wake_after` it).
    pub fn on_grid_metrics(
        &mut self,
        bounds: [f32; 4],
        cell_w: f32,
        line_h: f32,
        chat: Option<&str>,
    ) -> bool {
        let usable = cell_w.is_finite() && cell_w > 0.0 && line_h.is_finite() && line_h > 0.0;
        let (cols, rows) = if usable {
            let inner_w = bounds[2] - 2.0 * TERM_PADDING;
            let inner_h = bounds[3] - 2.0 * TERM_PADDING;
            (
                ((inner_w / cell_w).floor() as i64).clamp(2, 500) as u16,
                ((inner_h / line_h).floor() as i64).clamp(1, 500) as u16,
            )
        } else {
            (80, 24)
        };
        self.geometry = Some(GridGeometry {
            bounds,
            origin: [bounds[0] + TERM_PADDING, bounds[1] + TERM_PADDING],
            cell_w,
            line_h,
            cols,
            rows,
        });
        if self.resize_suspended {
            return false;
        }
        let Some(chat) = chat else { return false };
        let Some(tab) = self.active_tab_mut(chat) else {
            return false;
        };
        if tab.emulator.cols() == cols as usize && tab.emulator.rows() == rows as usize {
            return false;
        }
        tab.emulator.resize(cols, rows);
        let armed = tab.resize_due.is_none();
        if armed {
            tab.resize_due = Some(Instant::now());
        }
        armed
    }

    // ---- frames from the background task ----

    /// Apply queued stream frames and fire due timers. Returns true when the
    /// visible grid changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed |= self.apply_msg(msg);
        }
        self.flush_due(Instant::now());
        changed
    }

    /// How long until the next coalesced input flush or debounced resize is
    /// due. `App::poll` schedules it with `tx.wake_after` so Rune wakes from
    /// rest and [`Self::poll`] can fire the timer; `None` when at rest.
    pub fn next_wake(&self) -> Option<Duration> {
        let now = Instant::now();
        let mut next: Option<Duration> = None;
        let mut consider = |at: Instant, window_ms: u64| {
            let due =
                Duration::from_millis(window_ms).saturating_sub(now.saturating_duration_since(at));
            next = Some(next.map_or(due, |current: Duration| current.min(due)));
        };
        for tabs in self.chats.values() {
            for tab in &tabs.tabs {
                if let Some(at) = tab.pending_input_at {
                    consider(at, COALESCE_MS);
                }
                if let Some(at) = tab.resize_due {
                    consider(at, RESIZE_DEBOUNCE_MS);
                }
            }
        }
        next
    }

    fn apply_msg(&mut self, msg: TerminalMsg) -> bool {
        match msg {
            TerminalMsg::Opened {
                chat,
                key,
                terminal_id,
                shell,
            } => {
                let Some(tab) = self.tab_mut(&chat, key) else {
                    return false;
                };
                tab.terminal_id = Some(terminal_id);
                tab.shell = Some(shell.clone());
                // A grid resize may have landed while the PTY was opening; send
                // the current size once the session id exists.
                if tab.resize_due.is_none() {
                    tab.resize_due = Some(Instant::now());
                }
                if tab.title.starts_with("Terminal ") {
                    // The shell name is a nicer fallback than "Terminal N".
                    let name = shell_title(&shell);
                    if name != "terminal" {
                        tab.title = name;
                    }
                }
                true
            }
            TerminalMsg::OpenFailed { chat, key, message } => {
                let Some(tab) = self.tab_mut(&chat, key) else {
                    return false;
                };
                tab.exited = Some(-1);
                let line = format!("\x1b[31mfailed to open terminal: {message}\x1b[0m\r\n");
                tab.emulator.feed(line.as_bytes());
                true
            }
            TerminalMsg::Data {
                chat,
                key,
                seq,
                bytes,
            } => {
                let mut responses = Vec::new();
                let mut write_to = None;
                let mut changed = false;
                if let Some(tab) = self.tab_mut(&chat, key) {
                    tab.last_seq = seq.max(tab.last_seq);
                    tab.seq_shared.store(tab.last_seq, Ordering::Relaxed);
                    responses = tab.emulator.feed(&bytes);
                    write_to = tab.terminal_id.clone().map(|id| (id, tab.target.clone()));
                    changed = true;
                }
                if let (Some((id, target)), Some(engine), Some(bridge)) =
                    (write_to, self.engine.clone(), self.bridge.clone())
                    && !responses.is_empty()
                {
                    write_bytes(engine, bridge, target, id, responses);
                }
                changed
            }
            TerminalMsg::Exit {
                chat,
                key,
                seq,
                exit_code,
            } => {
                let Some(tab) = self.tab_mut(&chat, key) else {
                    return false;
                };
                tab.last_seq = seq.max(tab.last_seq);
                tab.seq_shared.store(tab.last_seq, Ordering::Relaxed);
                tab.exited = Some(exit_code);
                tab.exited_shared.store(true, Ordering::Relaxed);
                tab.emulator.feed(&exit_message(exit_code));
                tab.coalescer.take();
                tab.pending_input_at = None;
                true
            }
        }
    }

    /// Fire the coalesced input flush and the debounced resize when due.
    fn flush_due(&mut self, now: Instant) {
        let mut writes: Vec<(String, Option<String>, Vec<u8>)> = Vec::new();
        let mut resizes: Vec<(String, Option<String>, u16, u16)> = Vec::new();
        for tabs in self.chats.values_mut() {
            for tab in &mut tabs.tabs {
                if let Some(since) = tab.pending_input_at
                    && now.duration_since(since) >= Duration::from_millis(COALESCE_MS)
                {
                    tab.pending_input_at = None;
                    let bytes = tab.coalescer.take();
                    if !bytes.is_empty() {
                        match &tab.terminal_id {
                            Some(id) => {
                                writes.push((id.clone(), tab.target.clone(), bytes));
                            }
                            None if tab.exited.is_none() => {
                                // OpenTerminal still in flight — keep the
                                // buffer and retry on the next tick.
                                tab.coalescer.push(&bytes);
                                tab.pending_input_at = Some(now);
                            }
                            None => {}
                        }
                    }
                }
                if let Some(due) = tab.resize_due
                    && now.duration_since(due) >= Duration::from_millis(RESIZE_DEBOUNCE_MS)
                {
                    tab.resize_due = None;
                    if let Some(id) = &tab.terminal_id
                        && tab.exited.is_none()
                    {
                        resizes.push((
                            id.clone(),
                            tab.target.clone(),
                            tab.emulator.cols() as u16,
                            tab.emulator.rows() as u16,
                        ));
                    }
                }
            }
        }
        let (Some(engine), Some(bridge)) = (self.engine.clone(), self.bridge.clone()) else {
            return;
        };
        for (id, target, bytes) in writes {
            write_bytes(engine.clone(), bridge.clone(), target, id, bytes);
        }
        for (id, target, cols, rows) in resizes {
            let client = engine.client.clone();
            let bridge = bridge.clone();
            bridge.spawn(async move {
                let _ = client
                    .call(
                        methods::RESIZE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": id, "cols": cols, "rows": rows }),
                            &target,
                        ),
                    )
                    .await;
            });
        }
    }

    // ---- input ----

    /// Queue keyboard bytes on the active tab (12 ms coalescing window).
    /// Returns true when this push armed the flush timer; the caller then asks
    /// Rune to wake after [`COALESCE_MS`].
    pub fn queue_input(&mut self, chat: &str, bytes: &[u8]) -> bool {
        let Some(tab) = self.active_tab_mut(chat) else {
            return false;
        };
        if tab.exited.is_some() {
            return false;
        }
        // A keypress while scrolled back snaps to the live bottom (xterm).
        if tab.emulator.display_offset() > 0 {
            tab.emulator.scroll_to_bottom();
        }
        if tab.coalescer.push(bytes) {
            tab.pending_input_at = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// Paste into the active tab, bracketed per the running program's mode.
    /// Returns true when this paste armed the coalescer flush timer.
    pub fn paste(&mut self, chat: &str, text: &str) -> bool {
        let bracketed = self
            .active_tab(chat)
            .map(|tab| tab.emulator.bracketed_paste_mode())
            .unwrap_or(false);
        let bytes = paste_bytes(text, bracketed);
        self.queue_input(chat, &bytes)
    }

    /// Copy the active tab's selection. Returns whether anything was copied.
    pub fn copy_active(&mut self, chat: &str, tx: &mut Tx) -> bool {
        let Some(text) = self
            .active_tab(chat)
            .and_then(|tab| tab.emulator.selection_text())
        else {
            return false;
        };
        tx.copy(text);
        true
    }

    // ---- selection ----

    fn with_emulator<R>(&mut self, chat: &str, f: impl FnOnce(&mut Emulator) -> R) -> Option<R> {
        let tab = self.active_tab_mut(chat)?;
        Some(f(&mut tab.emulator))
    }

    fn grid_point_at(&self, chat: &str, x: f32, y: f32) -> Option<(GridPoint, Side)> {
        let geometry = self.geometry?;
        let tab = self.active_tab(chat)?;
        let hit = cell_at(
            x - TERM_PADDING,
            y - TERM_PADDING,
            geometry.cell_w,
            geometry.line_h,
            tab.emulator.cols(),
            tab.emulator.rows(),
        );
        let point = tab.emulator.grid_point(hit.row, hit.col);
        Some((point, hit.side))
    }

    pub fn pointer_down(&mut self, chat: &str, x: f32, y: f32, clicks: u32) {
        let ty = match clicks {
            0 | 1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        let Some((point, side)) = self.grid_point_at(chat, x, y) else {
            return;
        };
        if ty == SelectionType::Simple {
            // A plain press clears and arms; the selection itself only begins
            // once the pointer travels far enough to mean it.
            self.with_emulator(chat, |emu| emu.clear_selection());
            self.selection_drag = Some(SelectionDrag {
                position: (x, y),
                armed: false,
            });
        } else {
            self.with_emulator(chat, |emu| emu.start_selection(ty, point, side));
            self.selection_drag = Some(SelectionDrag {
                position: (x, y),
                armed: true,
            });
        }
    }

    pub fn pointer_move(&mut self, chat: &str, motion: PointerMotion) {
        let Some(mut drag) = self.selection_drag else {
            return;
        };
        drag.position = (motion.x, motion.y);
        self.selection_drag = Some(drag);
        if !drag.armed {
            if motion.dx.hypot(motion.dy) < SELECTION_DRAG_THRESHOLD {
                return;
            }
            // Threshold tripped: anchor at the *press*, not here.
            let Some((anchor, side)) = self.grid_point_at(chat, motion.start_x, motion.start_y)
            else {
                return;
            };
            self.with_emulator(chat, |emu| {
                emu.start_selection(SelectionType::Simple, anchor, side)
            });
            if let Some(drag) = self.selection_drag.as_mut() {
                drag.armed = true;
            }
        }
        let Some((point, side)) = self.grid_point_at(chat, motion.x, motion.y) else {
            return;
        };
        self.with_emulator(chat, |emu| emu.update_selection(point, side));
    }

    pub fn pointer_up(&mut self) {
        self.selection_drag = None;
        self.scrollbar_grab = None;
    }

    /// One step of edge auto-scroll for a live selection; the shell can drive
    /// this from a `wake_after` timer if it wants animated edge scrolling.
    pub fn step_selection_scroll(&mut self, chat: &str) -> bool {
        let (Some(drag), Some(geometry)) = (self.selection_drag, self.geometry) else {
            return false;
        };
        if !drag.armed {
            return false;
        }
        let lines = selection_scroll_lines(geometry, drag.position.1);
        if lines == 0 {
            return false;
        }
        self.scroll_active(chat, lines);
        if let Some((point, side)) = self.grid_point_at(chat, drag.position.0, drag.position.1) {
            self.with_emulator(chat, |emu| emu.update_selection(point, side));
        }
        true
    }

    // ---- scrolling ----

    pub fn scroll_active(&mut self, chat: &str, delta_lines: i32) {
        if delta_lines == 0 {
            return;
        }
        if let Some(tab) = self.active_tab_mut(chat) {
            tab.emulator.scroll(delta_lines);
        }
    }

    /// Scroll by a wheel delta in points (positive = content down).
    pub fn wheel(&mut self, chat: &str, dy: f32) {
        self.scroll_active(chat, wheel_lines(dy, TERM_LINE_HEIGHT));
    }

    fn scrollbar_metrics_for(&self, chat: &str) -> Option<ScrollbarMetrics> {
        let geometry = self.geometry?;
        let tab = self.active_tab(chat)?;
        scrollbar_metrics(
            [0.0, 0.0, geometry.bounds[2], geometry.bounds[3]],
            tab.emulator.rows(),
            tab.emulator.history_lines(),
            tab.emulator.display_offset(),
        )
    }

    /// Scrollbar thumb drag. `y` is local to the body (the rail is full height).
    pub fn scrollbar_drag(&mut self, chat: &str, phase: Phase, y: f32) {
        match phase {
            Phase::Down => {
                let Some(metrics) = self.scrollbar_metrics_for(chat) else {
                    return;
                };
                let on_track = y - SCROLLBAR_TRACK_INSET;
                let grab = if (metrics.thumb_top..=metrics.thumb_top + metrics.thumb_height)
                    .contains(&on_track)
                {
                    on_track - metrics.thumb_top
                } else {
                    metrics.thumb_height / 2.0
                };
                self.scrollbar_grab = Some(grab);
                self.scrollbar_to(chat, y, grab);
            }
            Phase::Move => {
                if let Some(grab) = self.scrollbar_grab {
                    self.scrollbar_to(chat, y, grab);
                }
            }
            Phase::Up => self.scrollbar_grab = None,
        }
    }

    fn scrollbar_to(&mut self, chat: &str, y: f32, grab: f32) {
        let Some(metrics) = self.scrollbar_metrics_for(chat) else {
            return;
        };
        let thumb_top = (y - SCROLLBAR_TRACK_INSET - grab).clamp(0.0, metrics.travel());
        let offset = if metrics.travel() <= 0.0 {
            0
        } else {
            ((1.0 - thumb_top / metrics.travel()) * metrics.history_lines as f32).round() as usize
        };
        if let Some(tab) = self.active_tab_mut(chat) {
            tab.emulator.scroll_to_offset(offset);
        }
    }
}

/// Emit a message and wake the UI loop (Rune polls once per frame, and the
/// waker schedules one even when nothing else changed).
fn emit(tx: &Sender<TerminalMsg>, bridge: &Bridge, msg: TerminalMsg) {
    let _ = tx.send(msg);
    bridge.wake();
}

fn write_bytes(
    engine: EngineHandle,
    bridge: Bridge,
    target: Option<String>,
    id: String,
    bytes: Vec<u8>,
) {
    let data = encode_base64(&bytes);
    let client = engine.client.clone();
    bridge.spawn(async move {
        let _ = client
            .call(
                methods::WRITE_TERMINAL,
                with_target(
                    serde_json::json!({ "terminalId": id, "data": data }),
                    &target,
                ),
            )
            .await;
    });
}

/// Wheel delta in points → whole terminal lines, with alacritty's sign
/// (positive scrolls up into history). One notch always moves at least a line.
fn wheel_lines(dy: f32, line_h: f32) -> i32 {
    if !dy.is_finite() || !line_h.is_finite() || line_h <= 0.0 {
        return 0;
    }
    if dy < 0.0 {
        ((-dy / line_h).round() as i32).max(1)
    } else if dy > 0.0 {
        -((dy / line_h).round() as i32).max(1)
    } else {
        0
    }
}

/// OpenTerminal, then pump SubscribeTerminal with reconnect backoff.
#[allow(clippy::too_many_arguments)]
fn spawn_session(
    chat: String,
    key: u64,
    engine: EngineHandle,
    bridge: Bridge,
    tx: Sender<TerminalMsg>,
    target: Option<String>,
    seq: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    cols: u16,
    rows: u16,
) {
    let task_bridge = bridge.clone();
    bridge.spawn(async move {
        let client = engine.client.clone();
        let opened = client
            .call_as::<TerminalSession>(
                methods::OPEN_TERMINAL,
                with_target(
                    serde_json::json!({
                        "chatId": chat,
                        "cols": cols,
                        "rows": rows,
                    }),
                    &target,
                ),
            )
            .await;
        let session = match opened {
            Ok(session) => session,
            Err(err) => {
                if alive.load(Ordering::Relaxed) {
                    tracing::warn!(error = %err, "OpenTerminal failed");
                    emit(
                        &tx,
                        &task_bridge,
                        TerminalMsg::OpenFailed {
                            chat,
                            key,
                            message: err.to_string(),
                        },
                    );
                }
                alive.store(false, Ordering::Relaxed);
                return;
            }
        };
        let terminal_id = session.id.clone();
        let shell = session.shell.clone();

        if !alive.load(Ordering::Relaxed) {
            // Tab was closed before the open completed — release the PTY.
            let _ = client
                .call(
                    methods::CLOSE_TERMINAL,
                    with_target(serde_json::json!({ "terminalId": terminal_id }), &target),
                )
                .await;
            return;
        }
        emit(
            &tx,
            &task_bridge,
            TerminalMsg::Opened {
                chat: chat.clone(),
                key,
                terminal_id: terminal_id.clone(),
                shell,
            },
        );

        let mut attempt: u32 = 0;
        loop {
            if !alive.load(Ordering::Relaxed) || exited.load(Ordering::Relaxed) {
                return;
            }
            let after_seq = seq.load(Ordering::Relaxed);
            let subscribed = client
                .subscribe(
                    methods::SUBSCRIBE_TERMINAL,
                    with_target(
                        serde_json::json!({
                            "terminalId": terminal_id,
                            "afterSeq": after_seq,
                        }),
                        &target,
                    ),
                )
                .await;
            let mut rx = match subscribed {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(error = %err, attempt, "SubscribeTerminal failed; backing off");
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            };

            while let Some(value) = rx.recv().await {
                let event: TerminalEvent = match serde_json::from_value(value) {
                    Ok(event) => event,
                    Err(err) => {
                        tracing::warn!(error = %err, "terminal: malformed stream frame");
                        continue;
                    }
                };
                attempt = 0;
                match event {
                    TerminalEvent::Data { seq: s, data } => {
                        seq.store(s, Ordering::Relaxed);
                        emit(
                            &tx,
                            &task_bridge,
                            TerminalMsg::Data {
                                chat: chat.clone(),
                                key,
                                seq: s,
                                bytes: decode_base64(&data),
                            },
                        );
                    }
                    TerminalEvent::Exit {
                        seq: s, exit_code, ..
                    } => {
                        seq.store(s, Ordering::Relaxed);
                        exited.store(true, Ordering::Relaxed);
                        emit(
                            &tx,
                            &task_bridge,
                            TerminalMsg::Exit {
                                chat: chat.clone(),
                                key,
                                seq: s,
                                exit_code,
                            },
                        );
                        return;
                    }
                }
            }

            // Stream dropped without an exit — reconnect from afterSeq.
            if exited.load(Ordering::Relaxed) || !alive.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
            attempt = attempt.saturating_add(1);
        }
    });
}

// ---------------------------------------------------------------------------
// App-level actions (handlers call these with `&mut App`)
// ---------------------------------------------------------------------------

pub fn chat_target(app: &App, chat: &str) -> Option<String> {
    let device = app.chats.iter().find(|c| c.id == chat)?.device_id.clone();
    (app.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
}

/// Create a tab for `chat` on the selected available engine.
fn open_tab_for(app: &mut App, chat: &str) {
    let Some(engine) = app.engine.clone() else {
        return;
    };
    let bridge = app.bridge.clone();
    let target = chat_target(app, chat);
    app.terminal
        .open_tab(chat.to_string(), engine, bridge, target);
}

/// Hide the bottom drawer: clear our mirror of the shell's per-chat flag. The
/// shell owns the flag itself (`SessionPanels`), so both are updated.
fn hide_drawer(app: &mut App, chat: &str) {
    app.terminal.set_open(false);
    app.shell
        .panels
        .update(chat, |panels| panels.terminal_open = false);
}

/// Close a tab and, in drawer mode, collapse the panel when it was the last
/// one (an empty dock is dead space). Embedded mode leaves emptiness to the
/// shell, which falls back to its surface picker.
fn close_tab_action(app: &mut App, chat: &str, key: u64) {
    let engine = app.engine.clone();
    let bridge = Some(app.bridge.clone());
    let target = chat_target(app, chat);
    let now_empty = app.terminal.close_tab(chat, key, engine, bridge, target);
    let drawer_open = app.terminal.open || app.shell.panels.get(chat).terminal_open;
    if now_empty && !app.terminal.embedded && drawer_open {
        hide_drawer(app, chat);
    }
}

/// Open a fresh tab for the selected chat; returns its key.
pub fn new_tab(app: &mut App) -> Option<u64> {
    let chat = app.selected_chat.clone()?;
    open_tab_for(app, &chat);
    Some(app.terminal.tab_seq)
}

/// Ensure the selected chat has at least one tab (drawer open path).
pub fn ensure_tab(app: &mut App) {
    let Some(chat) = app.selected_chat.clone() else {
        return;
    };
    let empty = app
        .terminal
        .chats
        .get(&chat)
        .is_none_or(|tabs| tabs.tabs.is_empty());
    if empty {
        open_tab_for(app, &chat);
    }
}

/// Surface-tab hook: make `key` the rendered tab of the selected chat.
pub fn select_tab_by_key(app: &mut App, key: u64) {
    let Some(chat) = app.selected_chat.clone() else {
        return;
    };
    app.terminal.select_tab_by_key(&chat, key);
}

/// Surface-tab hook: close `key`, hiding the drawer when the last tab goes.
pub fn close_tab_by_key(app: &mut App, key: u64) {
    let Some(chat) = app.selected_chat.clone() else {
        return;
    };
    close_tab_action(app, &chat, key);
}

/// Close every terminal tab of `chat`, whether or not it is the selected chat
/// (chat deletion must not leak PTYs). Each tab sends `CloseTerminal` with the
/// device target it was opened against, then drops its emulator and stream.
pub fn close_chat_tabs(app: &mut App, chat: &str) {
    let Some(tabs) = app.terminal.chats.get(chat) else {
        return;
    };
    let entries: Vec<(u64, Option<String>)> = tabs
        .tabs
        .iter()
        .map(|tab| (tab.key, tab.target.clone()))
        .collect();
    let engine = app.engine.clone();
    let bridge = Some(app.bridge.clone());
    for (key, target) in entries {
        app.terminal
            .close_tab(chat, key, engine.clone(), bridge.clone(), target);
    }
    // close_tab drops the entry with its last tab; also clear a tab-less entry.
    app.terminal.chats.remove(chat);
    app.terminal.drag = None;
    let selected = app.selected_chat.as_deref() == Some(chat);
    if selected && !app.terminal.embedded {
        let drawer_open = app.terminal.open || app.shell.panels.get(chat).terminal_open;
        if drawer_open {
            hide_drawer(app, chat);
        }
    }
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

/// Keys for a grid showing `chat` (embedded surfaces may show a chat that is
/// not the selected one). Returns true when the PTY took the key.
fn handle_key_for(app: &mut App, chat: &str, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // The app-level terminal toggle (Ctrl+J off macOS, Cmd+J on macOS) belongs
    // to the shell's keymap; Ctrl+J must not be swallowed as a raw LF here.
    if !cfg!(target_os = "macos")
        && ev.mods.ctrl
        && !ev.mods.shift
        && super::view::event_char(ev).is_some_and(|c| c.eq_ignore_ascii_case(&'j'))
    {
        return false;
    }
    let primary = cfg!(target_os = "macos") && ev.mods.cmd;
    let chord = primary || (ev.mods.ctrl && ev.mods.shift);
    if let Some(c) = super::view::event_char(ev)
        && chord
    {
        match c.to_ascii_lowercase() {
            'v' => {
                let chat = chat.to_string();
                tx.read_clipboard(move |app: &mut App, content, tx: &mut Tx| {
                    let text = content.text();
                    if !text.is_empty() && app.terminal.paste(&chat, text) {
                        tx.wake_after(Duration::from_millis(COALESCE_MS));
                    }
                });
                return true;
            }
            'c' => {
                // Only swallowed when it actually copied, so Ctrl+Shift+C with
                // nothing selected still falls through to the interrupt.
                return app.terminal.copy_active(chat, tx);
            }
            _ => {}
        }
    }
    let app_cursor = app
        .terminal
        .active_tab(chat)
        .map(|tab| tab.emulator.app_cursor_mode())
        .unwrap_or(false);
    if let Some(bytes) = key_event_bytes(ev, app_cursor) {
        if app.terminal.queue_input(chat, &bytes) {
            tx.wake_after(Duration::from_millis(COALESCE_MS));
        }
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// The bottom drawer: internal tab strip over the active tab's grid.
pub fn render(state: &TerminalState, app: &App, theme: &Theme) -> El {
    let mut root = rune::col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .fill(terminal_panel_bg(theme));

    let Some(chat) = app.selected_chat.clone() else {
        return root.center().child(
            text("Select a chat to open a terminal")
                .font(rune::TextStyle::new(12.0, 400.0))
                .color(theme.text_faint),
        );
    };

    root = root.child(tab_bar(state, theme, &chat));
    root.child(grid_body(state, theme, &chat))
}

/// The right-pane surface: just the grid (the shell owns the surface tabs).
pub fn render_embedded(state: &TerminalState, app: &App, theme: &Theme) -> El {
    let root = rune::col().w_full().h_full().min_h(0.0);
    match app.selected_chat.as_deref() {
        Some(chat) => root.child(grid_body(state, theme, chat)),
        None => root.center().child(
            text("Select a chat to open a terminal")
                .font(rune::TextStyle::new(12.0, 400.0))
                .color(theme.text_faint),
        ),
    }
}

/// The right-pane surface for an explicit surface tab: renders that tab's grid
/// even when the active index has not caught up yet. `key` is the tab key the
/// shell stores in `RightSurface::Terminal(key)`.
pub fn render_embedded_key(state: &TerminalState, app: &App, theme: &Theme, key: u64) -> El {
    let root = rune::col().w_full().h_full().min_h(0.0);
    let chat = chat_for_tab(state, key)
        .map(str::to_owned)
        .or_else(|| app.selected_chat.clone());
    match chat {
        Some(chat) => root.child(grid_body_key(state, theme, &chat, Some(key))),
        None => root.center().child(
            text("Select a chat to open a terminal")
                .font(rune::TextStyle::new(12.0, 400.0))
                .color(theme.text_faint),
        ),
    }
}

/// The chat owning tab `key` (tab keys are unique across chats).
fn chat_for_tab(state: &TerminalState, key: u64) -> Option<&str> {
    state
        .chats
        .iter()
        .find(|(_, tabs)| tabs.tabs.iter().any(|tab| tab.key == key))
        .map(|(chat, _)| chat.as_str())
}

/// Point the state's active index at the grid's tab before a pointer or key
/// action, so a surface that renders a keyed tab not yet selected still types
/// into the tab it shows.
fn focus_tab(app: &mut App, chat: &str, key: Option<u64>) {
    if let Some(key) = key {
        app.terminal.select_tab_by_key(chat, key);
    }
}

fn tab_bar(state: &TerminalState, theme: &Theme, chat: &str) -> El {
    let tabs = state.chats.get(chat);
    let (active, count) = tabs.map(|t| (t.active, t.tabs.len())).unwrap_or((0, 0));
    let entries: Vec<(usize, u64, String, bool, bool)> = tabs
        .map(|tabs| {
            tabs.tabs
                .iter()
                .enumerate()
                .map(|(ix, tab)| {
                    (
                        ix,
                        tab.key,
                        TerminalState::display_title(tab),
                        ix == active,
                        tab.exited.is_some(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let mut bar = row()
        .w_full()
        .h(TAB_BAR_HEIGHT)
        .items_center()
        .gap(4.0)
        .pl(TAB_BAR_PAD)
        .pr(6.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], theme.hairline(0.07));

    for (ix, key, title, selected, exited) in entries {
        bar = bar.child(tab_button(
            state, theme, chat, ix, key, title, selected, exited, count,
        ));
    }

    // "+" new tab.
    bar = bar.child(
        el().key("terminal-new-tab")
            .size(28.0, 28.0)
            .center()
            .radius(8.0)
            .hover(theme.ink(0.05))
            .cursor(Cursor::Pointer)
            .on_press(|app: &mut App| {
                new_tab(app);
            })
            .child(icon(&icons::PLUS, 16.0).color(theme.text_muted.alpha(0.6))),
    );

    bar.child(el().grow(1.0))
        .child(
            el().key("terminal-collapse")
                .size(28.0, 28.0)
                .center()
                .radius(8.0)
                .hover(theme.ink(0.05))
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| {
                    if let Some(chat) = app.selected_chat.clone() {
                        hide_drawer(app, &chat);
                    }
                })
                .child(icon(&icons::ALT_ARROW_DOWN, 13.0).color(theme.text_muted.alpha(0.55))),
        )
        // Unused but keeps the signature symmetrical with the strip math.
        .key("terminal-tab-bar")
}
#[allow(clippy::too_many_arguments)]
fn tab_button(
    state: &TerminalState,
    theme: &Theme,
    chat: &str,
    ix: usize,
    key: u64,
    title: String,
    selected: bool,
    exited: bool,
    count: usize,
) -> El {
    let _ = count;
    let (text_color, bg, glyph_alpha) = if selected {
        (theme.text, theme.ink(0.08), 0.8)
    } else {
        (theme.text_muted.alpha(0.6), Color::TRANSPARENT, 0.6)
    };
    let chat_select = chat.to_string();
    let chat_close = chat.to_string();
    let chat_drag = chat.to_string();

    let mut tab = row()
        .key(format!("terminal-tab-{key}").as_str())
        .w(TAB_WIDTH)
        .h(TAB_HEIGHT)
        .items_center()
        .gap(6.0)
        .pl(8.0)
        .pr(4.0)
        .radius(8.0)
        .fill(bg)
        .hover(theme.ink(0.05))
        .cursor(Cursor::Pointer)
        .pointer_buttons([PointerButton::Middle])
        .on_press_tx(move |app: &mut App, tx: &mut Tx| {
            if tx.button() == PointerButton::Middle {
                close_tab_action(app, &chat_close, key);
            } else {
                app.terminal.select_tab(&chat_select, ix);
            }
        })
        .on_drag(
            move |app: &mut App, drag: &mut Drag, _tx: &mut Tx| match drag.phase {
                Phase::Down => app.terminal.begin_tab_drag(ix),
                Phase::Move => app
                    .terminal
                    .update_tab_drag(&chat_drag, ix, drag.x, drag.dx),
                Phase::Up => {
                    app.terminal.commit_tab_drag(&chat_drag);
                }
            },
        );

    // Contextual label: the OSC title wins over the fixed fallback.
    tab = tab
        .child(icon(&icons::TERMINAL, 16.0).color(text_color.alpha(glyph_alpha)))
        .child(
            row().grow(1.0).min_w(0.0).child(
                text(title)
                    .font(rune::TextStyle::new(12.0, 400.0))
                    .color(text_color)
                    .truncate(),
            ),
        );
    // The close slot keeps its 20 pt whether or not the tab is active (gpui's
    // `.invisible()`), so activating a tab does not shift its label.
    let chat_close2 = chat.to_string();
    let mut close = el()
        .size(20.0, 20.0)
        .center()
        .radius(6.0)
        .child(icon(&icons::CLOSE, 12.0).color(theme.text_muted.alpha(0.8)));
    if selected {
        close = close
            .hover(theme.ink(0.09))
            .cursor(Cursor::Pointer)
            .on_press(move |app: &mut App| close_tab_action(app, &chat_close2, key));
    } else {
        close = close.opacity(0.0);
    }
    tab = tab.child(close);
    if exited {
        tab = tab.opacity(0.55);
    }

    // Sliding transform while a sibling is dragged over (150 ms).
    if let Some(drag) = state.drag
        && ix != drag.from
    {
        let offset = slide_offset(ix, drag.from, drag.over) * TAB_WIDTH;
        tab = tab.offset(offset, 0.0).motion(TAB_SLIDE);
    } else if let Some(drag) = state.drag
        && ix == drag.from
    {
        tab = tab.offset(drag.dx, 0.0).motion(TAB_SLIDE);
    }
    tab
}

fn grid_body(state: &TerminalState, theme: &Theme, chat: &str) -> El {
    grid_body_key(state, theme, chat, None)
}

fn grid_body_key(state: &TerminalState, theme: &Theme, chat: &str, key: Option<u64>) -> El {
    let style = term_style(theme);
    let cell_w = cell_width(style);
    let line_h = TERM_LINE_HEIGHT;

    let tab = match key {
        Some(key) => state
            .chats
            .get(chat)
            .and_then(|tabs| tabs.tabs.iter().find(|tab| tab.key == key)),
        None => state.active_tab(chat),
    };
    let rows: Vec<Vec<CellSnapshot>> = tab.map(|t| t.emulator.lines()).unwrap_or_default();
    let cursor = tab.and_then(|t| t.emulator.cursor());
    let history = tab.map(|t| t.emulator.history_lines()).unwrap_or(0);
    let offset = tab.map(|t| t.emulator.display_offset()).unwrap_or(0);
    let emu_rows = tab.map(|t| t.emulator.rows()).unwrap_or(0);

    let chat_owned = chat.to_string();
    let chat_key = chat.to_string();
    let chat_wheel = chat.to_string();
    let chat_scroll = chat.to_string();
    let chat_layout = chat.to_string();
    let chat_focus = chat.to_string();

    let mut body = rune::col()
        .key("terminal-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .padding(TERM_PADDING)
        .clip_rounded(0.0)
        .focusable(true)
        .tab_index(-1)
        .on_layout_tx(move |app: &mut App, rect: [f32; 4], tx: &mut Tx| {
            focus_tab(app, &chat_layout, key);
            if app
                .terminal
                .on_grid_metrics(rect, cell_w, line_h, Some(&chat_layout))
            {
                tx.wake_after(Duration::from_millis(RESIZE_DEBOUNCE_MS));
            }
        })
        .on_key(move |app: &mut App, ev: &KeyEvent, tx: &mut Tx| {
            focus_tab(app, &chat_key, key);
            handle_key_for(app, &chat_key, ev, tx)
        })
        .on_hover_change(|app: &mut App, inside: bool, _tx: &mut Tx| {
            app.terminal.terminal_hovered = inside;
            if !inside {
                app.terminal.scrollbar_hovered = false;
            }
        })
        .on_wheel(move |app: &mut App, ev: WheelEvent, _tx: &mut Tx| -> bool {
            focus_tab(app, &chat_wheel, key);
            app.terminal.wheel(&chat_wheel, ev.dy);
            true
        })
        .on_drag(move |app: &mut App, drag: &mut Drag, _tx: &mut Tx| {
            focus_tab(app, &chat_focus, key);
            match drag.phase {
                Phase::Down => app
                    .terminal
                    .pointer_down(&chat_owned, drag.x, drag.y, drag.clicks),
                Phase::Move => app.terminal.pointer_move(
                    &chat_focus,
                    PointerMotion {
                        x: drag.x,
                        y: drag.y,
                        start_x: drag.start_x,
                        start_y: drag.start_y,
                        dx: drag.dx,
                        dy: drag.dy,
                    },
                ),
                Phase::Up => app.terminal.pointer_up(),
            }
        });

    if rows.is_empty() {
        return body
            .center()
            .child(label("No terminal", 12.0, theme.text_faint));
    }

    for (row_ix, cells) in rows.iter().enumerate() {
        body = body.child(render_row(
            cells, row_ix, cursor, theme, style, cell_w, line_h,
        ));
    }

    // Scrollbar: an on-demand affordance under the pointer, only when there is
    // history above the viewport.
    if state.terminal_hovered
        && let Some(metrics) = scrollbar_metrics(
            [
                0.0,
                0.0,
                state.geometry.map(|g| g.bounds[2]).unwrap_or(0.0),
                state.geometry.map(|g| g.bounds[3]).unwrap_or(0.0),
            ],
            emu_rows,
            history,
            offset,
        )
    {
        let thumb_width = if state.scrollbar_hovered {
            SCROLLBAR_HOVER_THUMB_WIDTH
        } else {
            SCROLLBAR_THUMB_WIDTH
        };
        body = body.child(
            el().key("terminal-scrollbar")
                .pin(None, Some(0.0), Some(0.0), Some(0.0))
                .w(SCROLLBAR_HIT_WIDTH)
                .cursor(Cursor::Pointer)
                .on_hover_change(|app: &mut App, inside: bool, _tx: &mut Tx| {
                    app.terminal.scrollbar_hovered = inside;
                })
                .on_drag(move |app: &mut App, drag: &mut Drag, _tx: &mut Tx| {
                    focus_tab(app, &chat_scroll, key);
                    app.terminal
                        .scrollbar_drag(&chat_scroll, drag.phase, drag.y);
                })
                .child(
                    el().pin(
                        None,
                        Some(SCROLLBAR_TRACK_INSET + metrics.thumb_top),
                        Some(2.0),
                        None,
                    )
                    .w(thumb_width)
                    .h(metrics.thumb_height)
                    .radius(thumb_width / 2.0)
                    .fill(theme.text_faint.alpha(0.52)),
                ),
        );
    }
    body
}

fn render_row(
    cells: &[CellSnapshot],
    row_ix: usize,
    cursor: Option<CursorSnapshot>,
    theme: &Theme,
    style: TextStyle,
    cell_w: f32,
    line_h: f32,
) -> El {
    let mut row_el = rune::col().w_full().h(line_h);

    // Background runs, merged into one quad per contiguous span.
    let mut run_start: Option<(usize, Color)> = None;
    for col in 0..=cells.len() {
        let paint = cells.get(col).and_then(|cell| {
            let (_, bg) = cell.display_colors();
            match bg {
                CellColor::Background => None,
                other => Some(resolve_color(other, theme)),
            }
        });
        match (&run_start, paint) {
            (None, Some(color)) => run_start = Some((col, color)),
            (Some((start, current)), next) if next != Some(*current) => {
                row_el = row_el.child(quad(*start, col, cell_w, line_h, *current));
                run_start = next.map(|color| (col, color));
            }
            _ => {}
        }
    }

    // Selection wash, painted over the cell backgrounds and under the glyphs.
    let wash = terminal_selection_for(theme.appearance);
    let mut sel_start: Option<usize> = None;
    for col in 0..=cells.len() {
        let selected = cells.get(col).is_some_and(|cell| cell.selected);
        match (sel_start, selected) {
            (None, true) => sel_start = Some(col),
            (Some(start), false) => {
                row_el = row_el.child(quad(start, col, cell_w, line_h, wash));
                sel_start = None;
            }
            _ => {}
        }
    }

    // One rich-text run per row, column advances coming from the mono face.
    let (content, spans) = row_spans(cells, theme);
    let mut text_el = text(content).font(style);
    if !spans.is_empty() {
        text_el = text_el.spans(spans);
    }
    row_el = row_el.child(text_el);

    // The cursor block paints last, over the glyph underneath (translucent, so
    // the character stays legible), as the gpui element did.
    if let Some(c) = cursor
        && c.row == row_ix
    {
        row_el = row_el.child(quad(c.col, c.col + 1, cell_w, line_h, theme.cursor));
    }
    row_el
}

fn label(content: impl Into<String>, size: f32, color: Color) -> El {
    text(content)
        .font(rune::TextStyle::new(size, 400.0))
        .color(color)
}

fn quad(start_col: usize, end_col: usize, cell_w: f32, line_h: f32, color: Color) -> El {
    el().absolute(start_col as f32 * cell_w, 0.0)
        .w(((end_col - start_col) as f32 * cell_w).max(0.0))
        .h(line_h)
        .fill(color)
}

/// Build the row's text and per-run spans (color, weight, italic, underline).
fn row_spans(cells: &[CellSnapshot], theme: &Theme) -> (String, Vec<Span>) {
    let mut content = String::with_capacity(cells.len());
    let mut spans: Vec<Span> = Vec::new();
    let mut run: Option<(usize, Color, bool, bool, bool)> = None;

    for cell in cells {
        if cell.wide_spacer {
            continue;
        }
        let ch = if cell.hidden { ' ' } else { cell.ch };
        let (fg, _) = cell.display_colors();
        let mut color = resolve_color(fg, theme);
        if cell.dim {
            color.a *= 0.6;
        }
        let bold = cell.bold;
        let italic = cell.italic;
        let underline = cell.underline;

        let start = content.len();
        content.push(ch);

        let same = run.is_some_and(|(_, c, b, i, u)| {
            c == color && b == bold && i == italic && u == underline
        });
        if !same {
            if let Some((from, c, b, i, u)) = run.take() {
                spans.push(span(from, start, c, b, i, u));
            }
            run = Some((start, color, bold, italic, underline));
        }
    }
    if let Some((from, c, b, i, u)) = run {
        spans.push(span(from, content.len(), c, b, i, u));
    }
    (content, spans)
}

fn span(start: usize, end: usize, color: Color, bold: bool, italic: bool, underline: bool) -> Span {
    let mut s = Span::new(start, end).color(color).mono();
    if bold {
        // gpui's FontWeight::BOLD.
        s = s.weight(700.0);
    }
    if italic {
        s = s.italic();
    }
    if underline {
        s = s.underline(color);
    }
    s
}

// Keep the span path honest: rows use the plain text element with per-run
// spans (never `.wrap()`, so a terminal row cannot reflow), which is what
// `rich` builds internally before it turns wrapping on.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_clamps_between_160_and_55vh() {
        assert_eq!(clamp_terminal_height(300.0, 900.0), 300.0);
        assert_eq!(clamp_terminal_height(10.0, 900.0), 160.0);
        assert_eq!(clamp_terminal_height(4000.0, 900.0), 900.0 * 0.55);
        // Tiny windows: min wins over the 55vh cap.
        assert_eq!(clamp_terminal_height(200.0, 100.0), 160.0);
        assert_eq!(clamp_terminal_height(f32::NAN, 900.0), 160.0);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_ms(0), 500);
        assert_eq!(backoff_ms(1), 1000);
        assert_eq!(backoff_ms(2), 2000);
        assert_eq!(backoff_ms(3), 4000);
        assert_eq!(backoff_ms(4), 8000);
        assert_eq!(backoff_ms(10), 8000);
        assert_eq!(backoff_ms(u32::MAX), 8000);
    }

    fn test_geometry() -> GridGeometry {
        GridGeometry {
            bounds: [10.0, 20.0, 300.0, 200.0],
            origin: [18.0, 28.0],
            cell_w: 8.0,
            line_h: 20.0,
            cols: 35,
            rows: 9,
        }
    }

    #[test]
    fn selection_edge_scroll_uses_terminal_direction() {
        let geometry = test_geometry();
        assert!(selection_scroll_lines(geometry, 28.0) > 0);
        assert_eq!(selection_scroll_lines(geometry, 100.0), 0);
        assert!(selection_scroll_lines(geometry, 208.0) < 0);
    }

    #[test]
    fn scrollbar_thumb_maps_history_top_and_bottom() {
        let bounds = test_geometry().bounds;
        assert!(scrollbar_metrics(bounds, 20, 0, 0).is_none());

        let bottom = scrollbar_metrics(bounds, 20, 80, 0).unwrap();
        let top = scrollbar_metrics(bounds, 20, 80, 80).unwrap();
        assert!((bottom.thumb_height - 38.4).abs() < 0.01);
        assert!((bottom.thumb_top - bottom.travel()).abs() < 0.01);
        assert_eq!(top.thumb_top, 0.0);
        assert_eq!(top.thumb_height, bottom.thumb_height);
    }

    #[test]
    fn reorder_moves_forward_and_backward() {
        let mut v = vec!["a", "b", "c", "d"];
        reorder_tabs(&mut v, 0, 2);
        assert_eq!(v, ["b", "c", "a", "d"]);
        reorder_tabs(&mut v, 3, 0);
        assert_eq!(v, ["d", "b", "c", "a"]);
        // Out-of-range / no-op moves leave the vec untouched.
        reorder_tabs(&mut v, 9, 0);
        reorder_tabs(&mut v, 1, 1);
        assert_eq!(v, ["d", "b", "c", "a"]);
    }

    #[test]
    fn drop_index_quantizes_and_clamps() {
        assert_eq!(drop_index(-10.0, 150.0, 3), 0);
        assert_eq!(drop_index(0.0, 150.0, 3), 0);
        assert_eq!(drop_index(149.0, 150.0, 3), 0);
        assert_eq!(drop_index(150.0, 150.0, 3), 1);
        assert_eq!(drop_index(700.0, 150.0, 3), 2);
        assert_eq!(drop_index(50.0, 150.0, 0), 0);
    }

    #[test]
    fn slide_offsets_shift_toward_the_gap() {
        // Dragging 0 over 2: tabs 1 and 2 slide left one slot.
        assert_eq!(slide_offset(0, 0, 2), 0.0);
        assert_eq!(slide_offset(1, 0, 2), -1.0);
        assert_eq!(slide_offset(2, 0, 2), -1.0);
        assert_eq!(slide_offset(3, 0, 2), 0.0);
        // Dragging 3 over 1: tabs 1 and 2 slide right.
        assert_eq!(slide_offset(0, 3, 1), 0.0);
        assert_eq!(slide_offset(1, 3, 1), 1.0);
        assert_eq!(slide_offset(2, 3, 1), 1.0);
        assert_eq!(slide_offset(3, 3, 1), 0.0);
        // Hovering the origin: nothing moves.
        for ix in 0..4 {
            assert_eq!(slide_offset(ix, 2, 2), 0.0);
        }
    }

    #[test]
    fn active_index_tracks_reorders() {
        assert_eq!(active_after_reorder(1, 1, 3), 3);
        assert_eq!(active_after_reorder(2, 0, 3), 1);
        assert_eq!(active_after_reorder(1, 3, 0), 2);
        assert_eq!(active_after_reorder(0, 2, 3), 0);
    }

    #[test]
    fn active_index_tracks_closes() {
        assert_eq!(active_after_close(2, 0, 3), 1); // close left of active
        assert_eq!(active_after_close(1, 1, 2), 1); // close active mid-list
        assert_eq!(active_after_close(2, 2, 2), 1); // close active at tail
        assert_eq!(active_after_close(0, 0, 0), 0); // last tab closed
    }

    #[test]
    fn exit_message_format() {
        let text = String::from_utf8(exit_message(0)).unwrap();
        assert!(text.contains("[process exited 0]"));
        let text = String::from_utf8(exit_message(137)).unwrap();
        assert!(text.contains("[process exited 137]"));
        assert!(text.starts_with("\r\n"));
        assert!(text.ends_with("\r\n"));
    }

    #[test]
    fn shell_titles() {
        assert_eq!(shell_title("/bin/zsh"), "zsh");
        assert_eq!(shell_title("/usr/local/bin/fish"), "fish");
        assert_eq!(shell_title("C:\\Windows\\System32\\cmd.exe"), "cmd.exe");
        assert_eq!(shell_title("bash"), "bash");
        assert_eq!(shell_title(""), "terminal");
    }

    #[test]
    fn stream_events_deserialize_per_contract() {
        let data: TerminalEvent =
            serde_json::from_str(r#"{"type":"data","seq":7,"data":"aGk="}"#).unwrap();
        assert_eq!(
            data,
            TerminalEvent::Data {
                seq: 7,
                data: "aGk=".into()
            }
        );
        let exit: TerminalEvent =
            serde_json::from_str(r#"{"type":"exit","seq":8,"exitCode":130}"#).unwrap();
        assert_eq!(
            exit,
            TerminalEvent::Exit {
                seq: 8,
                exit_code: 130,
                signal: None
            }
        );
        let session: TerminalSession =
            serde_json::from_str(r#"{"id":"t1","cwd":"/w","shell":"/bin/zsh"}"#).unwrap();
        assert_eq!(session.id, "t1");
        assert_eq!(session.shell, "/bin/zsh");
    }

    #[test]
    fn base64_round_trip_and_tolerance() {
        assert_eq!(decode_base64("aGk="), b"hi".to_vec());
        assert_eq!(
            decode_base64("aGk"),
            b"hi".to_vec(),
            "unpadded input tolerated"
        );
        assert_eq!(
            decode_base64("!!!"),
            Vec::<u8>::new(),
            "garbage decodes to nothing"
        );
        assert_eq!(encode_base64(b"hi"), "aGk=");
    }

    #[test]
    fn target_passthrough_only_for_remote_chats() {
        let params = serde_json::json!({ "chatId": "c1" });
        assert_eq!(with_target(params.clone(), &None), params);
        let tagged = with_target(params, &Some("dev-2".into()));
        assert_eq!(tagged["targetDeviceId"], "dev-2");
    }

    #[test]
    fn exit_message_feeds_cleanly_through_the_emulator() {
        let mut emulator = Emulator::new(40, 4);
        emulator.feed(b"$ done");
        emulator.feed(&exit_message(1));
        assert_eq!(emulator.row_text(1), "[process exited 1]");
    }

    #[test]
    fn grid_metrics_quantize_to_whole_cells() {
        let mut state = TerminalState::default();
        state.on_grid_metrics([10.0, 20.0, 300.0, 200.0], 8.0, 20.0, None);
        let geometry = state.geometry.unwrap();
        // (300 - 24) / 8 = 34.5 → 34 cols; (200 - 24) / 20 = 8.8 → 8 rows.
        assert_eq!(geometry.cols, 34);
        assert_eq!(geometry.rows, 8);
        assert_eq!(geometry.origin, [22.0, 32.0]);
    }

    #[test]
    fn wheel_delta_is_line_quantized() {
        // Negative dy is a wheel-up: positive lines into history.
        assert_eq!(wheel_lines(-18.0, TERM_LINE_HEIGHT), 1);
        assert_eq!(wheel_lines(-36.0, TERM_LINE_HEIGHT), 2);
        // Down toward the live bottom.
        assert_eq!(wheel_lines(18.0, TERM_LINE_HEIGHT), -1);
        // One notch always moves at least a line.
        assert_eq!(wheel_lines(-1.0, TERM_LINE_HEIGHT), 1);
        assert_eq!(wheel_lines(0.0, TERM_LINE_HEIGHT), 0);
        assert_eq!(wheel_lines(f32::NAN, TERM_LINE_HEIGHT), 0);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    fn test_view(app: &App, _cx: &mut rune::Cx<App>) -> El {
        render(&app.terminal, app, &app.theme)
    }

    fn embedded_view(app: &App, _cx: &mut rune::Cx<App>) -> El {
        render_embedded(&app.terminal, app, &app.theme)
    }

    fn keyed_view(app: &App, _cx: &mut rune::Cx<App>) -> El {
        render_embedded_key(&app.terminal, app, &app.theme, 2)
    }

    fn test_tab() -> TerminalTab {
        test_tab_with_key(1)
    }

    fn test_tab_with_key(key: u64) -> TerminalTab {
        let mut emulator = Emulator::new(20, 5);
        emulator.feed(format!("tab-{key} $ ok").as_bytes());
        TerminalTab {
            key,
            title: format!("Terminal {key}"),
            terminal_id: None,
            emulator,
            exited: None,
            last_seq: 0,
            seq_shared: Arc::new(AtomicU64::new(0)),
            alive: Arc::new(AtomicBool::new(true)),
            exited_shared: Arc::new(AtomicBool::new(false)),
            target: None,
            shell: Some("zsh".into()),
            coalescer: InputCoalescer::default(),
            pending_input_at: None,
            resize_due: None,
        }
    }

    fn app_with_tab() -> App {
        let mut app = App::new(crate::UiConfig::default());
        app.selected_chat = Some("chat-1".into());
        app.terminal.chats.insert(
            "chat-1".into(),
            ChatTabs {
                tabs: vec![test_tab()],
                active: 0,
            },
        );
        app
    }

    #[test]
    fn the_drawer_renders_and_reports_grid_geometry() {
        let mut rt = rune::Runtime::new(
            app_with_tab(),
            test_view,
            (800.0, 400.0),
            rune::Env::new(true, 2.0),
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        let geometry = rt
            .state
            .terminal
            .geometry
            .expect("on_layout reported the grid");
        assert!(geometry.cols >= 2 && geometry.rows >= 1, "{geometry:?}");
        assert_eq!(
            rt.state
                .terminal
                .active_tab("chat-1")
                .unwrap()
                .emulator
                .cols(),
            geometry.cols as usize
        );
    }

    #[test]
    fn the_embedded_surface_renders_without_a_tab_strip() {
        let mut rt = rune::Runtime::new(
            app_with_tab(),
            embedded_view,
            (600.0, 320.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.terminal.geometry.is_some());
        // The shell's surface strip owns the tabs, so the internal bar must be absent.
        assert!(rt.find("terminal-tab-bar").is_none());
        assert!(rt.find("terminal-body").is_some());
    }

    fn app_with_two_tabs() -> App {
        let mut app = App::new(crate::UiConfig::default());
        app.selected_chat = Some("chat-1".into());
        let mut second = test_tab_with_key(2);
        second.emulator.feed(b"\r\nsecond-tab-marker");
        app.terminal.chats.insert(
            "chat-1".into(),
            ChatTabs {
                tabs: vec![test_tab(), second],
                active: 0,
            },
        );
        app
    }

    #[test]
    fn a_key_in_the_focused_grid_queues_coalesced_bytes() {
        let mut rt = rune::Runtime::new(
            app_with_tab(),
            embedded_view,
            (600.0, 320.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        rt.update(|_app, tx| tx.focus("terminal-body", false));
        rt.frame(1.0 / 60.0);
        let focused = rt
            .elements()
            .into_iter()
            .find(|el| el.key.as_deref() == Some("terminal-body"))
            .is_some_and(|el| el.focused);
        assert!(focused, "the grid takes programmatic focus");

        rt.key(rune::KeyEvent {
            key: rune::Key::Text("a".into()),
            mods: rune::Mods::default(),
        });
        rt.frame(1.0 / 60.0);

        let tab = rt.state.terminal.active_tab("chat-1").expect("active tab");
        assert!(!tab.coalescer.is_empty(), "the key reached the coalescer");
        assert!(tab.pending_input_at.is_some(), "the flush timer is armed");
        let wait = rt
            .state
            .terminal
            .next_wake()
            .expect("a coalescer wake is scheduled");
        assert!(wait <= Duration::from_millis(COALESCE_MS), "{wait:?}");
    }

    #[test]
    fn the_embedded_surface_renders_the_surface_tab_key() {
        let mut rt = rune::Runtime::new(
            app_with_two_tabs(),
            keyed_view,
            (600.0, 320.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        let text: String = rt
            .elements()
            .into_iter()
            .filter_map(|el| el.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("second-tab-marker"), "{text}");
        assert!(!text.contains("tab-1"), "{text}");
        // Laying out the keyed grid selects the tab it shows.
        assert_eq!(
            rt.state
                .terminal
                .active_tab("chat-1")
                .expect("active tab")
                .key,
            2
        );
    }

    #[test]
    fn close_chat_tabs_closes_an_unselected_chats_tabs() {
        let mut app = App::new(crate::UiConfig::default());
        app.selected_chat = Some("chat-2".into());
        app.terminal.chats.insert(
            "chat-1".into(),
            ChatTabs {
                tabs: vec![test_tab_with_key(1), test_tab_with_key(3)],
                active: 0,
            },
        );
        app.terminal.chats.insert(
            "chat-2".into(),
            ChatTabs {
                tabs: vec![test_tab_with_key(2)],
                active: 0,
            },
        );

        close_chat_tabs(&mut app, "chat-1");

        assert!(!app.terminal.chats.contains_key("chat-1"));
        assert!(app.terminal.chats.contains_key("chat-2"));
        assert_eq!(app.selected_chat.as_deref(), Some("chat-2"));
        // A chat with no tabs is a no-op.
        close_chat_tabs(&mut app, "chat-1");
    }

    #[test]
    fn close_chat_tabs_hides_the_selected_chats_drawer() {
        let mut app = app_with_tab();
        app.terminal.open = true;
        app.shell
            .panels
            .update("chat-1", |panels| panels.terminal_open = true);

        close_chat_tabs(&mut app, "chat-1");

        assert!(!app.terminal.chats.contains_key("chat-1"));
        assert!(!app.terminal.open);
        assert!(!app.shell.panels.get("chat-1").terminal_open);
    }
}
