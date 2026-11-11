//! Terminal panel (feature-inventory §1.10): an `alacritty_terminal`-backed
//! emulator fed by the engine's PTY stream over the generic RPC client.
//!
//! - [`emulator`] — pure Term + vte state machine (bytes in, grid out);
//! - [`view`] — cell palette, keystroke→bytes encoding, input coalescing and
//!   the pointer/scrollbar math;
//! - [`panel`] — session-scoped tabs, subscriptions with reconnect backoff,
//!   drag-reorder, Rune rendering, and [`panel::TerminalState::poll`].
//!
//! Method names come from `hearth_rpc::methods` and wire types from
//! `hearth_proto` (`TerminalSession`, `TerminalEvent`) — the same contract the
//! engine serves.
//!
//! Wiring: `App` owns a `TerminalState`; the lead calls
//! `app.terminal.poll()` from `App::poll` (its `bool` is "the grid changed")
//! and mounts [`panel::render`] in the drawer or [`panel::render_embedded`] in
//! the right pane.

pub mod emulator;
pub mod panel;
pub mod view;

pub use panel::{
    close_tab_by_key, ensure_tab, new_tab, render, render_embedded, select_tab_by_key, ChatTabs,
    GridGeometry, TerminalState, TerminalTab, TAB_BAR_HEIGHT, TAB_WIDTH,
};
