//! The app shell: window chrome, sidebar, main pane, right pane and overlays.
//!
//! This is the Rune port of the gpui `shell` module. Layout numbers are the gpui constants;
//! colours come from [`crate::theme::Theme`] and every floating surface is a rune-kit
//! `Menu`/`Dialog`/`Popover` in the view's overlay layer.
//!
//! Ownership: the shell owns route/nav/panels/popups ([`ShellState`]) and the pure layout math
//! ([`panels`], [`tabs`], [`routes`]). The transcript, composer, terminal and changes modules
//! plug into the marked outlet hooks in [`render`] as they land.

pub mod keymap;
pub mod nav;
pub mod overlays;
pub mod panels;
pub mod render;
pub mod routes;
pub mod sidebar;
pub mod spaces;
pub mod state_sync;
pub mod tabs;
#[cfg(test)]
mod tests;
pub mod titlebar;

use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};

use rune::{Cx, El, Tx};

use crate::settings::SettingsSection;
use crate::state::{App, GatePhase};
use crate::theme::Theme;

pub use keymap::on_key;
pub use nav::{NavEntry, NavHistory};
pub use panels::{
    CHAT_ROW_HEIGHT, conversation_width, right_pane_max_width, right_pane_takeover_width,
    right_panel_content_width, stable_panel_content_width,
};
pub use routes::Route;
pub use tabs::{cycle_target, in_space_filter};

/// One right-pane surface tab (t3code RightPanelSurface, narrowed to our kinds): a git-diff
/// page, an embedded terminal tab, or a subagent transcript. `Shared` is the empty chooser.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RightSurface {
    #[default]
    Shared,
    Diff(u64),
    Terminal(u64),
    Subagent(u64),
}

/// Per-chat panel open flags (hearth parity: `sessionPanels`). Everything defaults CLOSED; a
/// fresh open with no surface tabs lands on the picker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    pub terminal_open: bool,
    /// Right pane visible (the surface host).
    pub changes_open: bool,
    /// Which surface tab renders; validated against the live tab list each frame.
    pub right_active: RightSurface,
}

/// The session-scoped panel map. Keys are chat ids; the new-chat canvas keys per space.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: HashMap<String, ChatPanels>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Flip the changes pane flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }

    /// Mutate `key`'s flags in place.
    pub fn update(&mut self, key: &str, f: impl FnOnce(&mut ChatPanels)) {
        f(self.map.entry(key.to_string()).or_default());
    }

    /// Drop a chat's state when the chat is deleted.
    pub fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }
}

/// Boot splash lifecycle. `FadingOut` carries the wall-clock instant the fade ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

/// The in-app update strip's state (hearth `UpdateFlow`): the manifest fetch + payload
/// staging run on [`crate::bridge::Bridge`], this is only what the strip shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateFlow {
    #[default]
    Idle,
    Downloading,
    /// Staged payload ready — one click restarts into it (macOS bundle path, or
    /// managed `current/hearth` exe path).
    Ready(String),
    Failed(String),
}

/// A right-pane subagent tab: the doc and the strip title.
#[derive(Debug, Clone)]
pub struct SubagentTab {
    pub doc_id: String,
    pub title: String,
}

/// The session-row context menu anchor: `(chat id, window point)`.
#[derive(Debug, Clone)]
pub struct ChatMenu {
    pub chat_id: String,
    pub x: f32,
    pub y: f32,
}

/// The space-row context menu anchor: `(space id, window point)`.
#[derive(Debug, Clone)]
pub struct SpaceMenu {
    pub space_id: String,
    pub x: f32,
    pub y: f32,
}

/// The chat-row Rename dialog.
#[derive(Debug)]
pub struct RenameChatDialog {
    pub chat_id: String,
    pub input: rune::TextEdit,
    pub focus_pending: bool,
}

/// The space-row Rename dialog.
#[derive(Debug)]
pub struct RenameSpaceDialog {
    pub space_id: String,
    pub input: rune::TextEdit,
    pub focus_pending: bool,
}

/// A background result from a shell task (RPC calls, list loads). Drained by [`poll`].
#[derive(Debug)]
pub enum ShellTask {
    Notice(String),
    FolderListing {
        generation: u64,
        path: Option<String>,
        result: Result<hearth_proto::FolderListing, String>,
    },
    Drives {
        generation: u64,
        result: Result<Vec<hearth_proto::DriveEntry>, String>,
    },
    SpaceCreated {
        generation: u64,
        space_id: String,
        result: Result<(), String>,
    },
    RepoReady {
        generation: u64,
        result: Result<String, String>,
    },
    /// An update payload finished staging (`Ok(exe/bundle path)`) or failed.
    UpdateStaged(Result<String, String>),
}

/// View-local shell state: route, history, panes, popups, splash and per-chat panels.
pub struct ShellState {
    pub route: Route,
    pub nav: NavHistory,
    pub panels: SessionPanels,
    /// The chat currently shown ("" = the new-chat canvas), mirrored each poll.
    pub active_chat: String,
    /// Ordered surface tabs per panel key.
    pub right_tabs: HashMap<String, Vec<RightSurface>>,
    pub diff_titles: HashMap<u64, String>,
    pub terminal_titles: HashMap<u64, String>,
    pub subagent_tabs: HashMap<u64, SubagentTab>,
    pub diff_seq: u64,
    pub terminal_seq: u64,
    pub subagent_seq: u64,
    /// Monotonic generation for add-space listing responses (fences stale tasks across opens).
    pub space_generation: u64,
    /// Follows the viewport width so takeover can fill the pane.
    pub viewport_width: f32,
    /// Changes-panel takeover (the header's expand button).
    pub right_pane_expanded: bool,
    pub archived_open: bool,
    pub archived_shown: usize,
    pub chat_menu: Option<ChatMenu>,
    pub rename_chat: Option<RenameChatDialog>,
    pub delete_confirm: Option<String>,
    pub space_menu: Option<SpaceMenu>,
    pub rename_space: Option<RenameSpaceDialog>,
    pub delete_space_confirm: Option<String>,
    pub add_space: Option<spaces::AddSpaceFlow>,
    pub spaces_menu: Option<spaces::SpacesMenuState>,
    pub user_menu_open: bool,
    pub sidebar_notice: Option<String>,
    pub update_dismissed: Option<String>,
    pub splash: SplashPhase,
    /// Wall-clock instant the splash fade ends (`FadingOut` only).
    pub splash_deadline: Option<f64>,
    /// `settings.last_space_id` applied once after the first spaces frame.
    pub space_boot_applied: bool,
    /// `HEARTH_OPEN_ROUTE=new` suppresses boot auto-select.
    pub pinned_new_canvas: bool,
    /// Last observed fullscreen state (macOS traffic-light clearance).
    pub fullscreen: bool,
    /// `HEARTH_OPEN_DIALOG` debug knob (rename/delete/model/add-space).
    pub debug_dialog: Option<String>,
    /// Focus hand-offs resolved in `App::poll` (handlers have no `Tx`).
    pub focus_terminal: bool,
    pub focus_composer: bool,
    pub composer_focused_once: bool,
    /// Measured bottom chrome (composer stack) for the jump pill.
    pub bottom_stack: f32,
    /// Last effective indicator per chat: the transition chime/banner baseline.
    pub sound_prev: HashMap<String, hearth_proto::SessionStatus>,
    /// The window holds keyboard focus (wired from Rune's `App::on_focus`). The
    /// background-only banner setting reads it.
    pub window_focused: bool,
    /// An OS file drag is over the chat column: draw the "Drop images to attach" veil.
    pub drop_hover: bool,
    /// The tab index a titlebar-strip drag is currently over (live reorder preview).
    pub right_tab_drag: Option<usize>,
    /// How this binary was installed; decides what the update strip's click does.
    pub install: hearth_update::InstallKind,
    /// The update strip's visible phase.
    pub update_flow: UpdateFlow,
    /// `side` channel results from `Bridge` jobs.
    pub(crate) task_tx: Sender<ShellTask>,
    pub(crate) task_rx: Receiver<ShellTask>,
}

impl Default for ShellState {
    fn default() -> Self {
        let route = routes::route_from_env();
        let mut pinned_new_canvas = false;
        let debug_dialog = std::env::var("HEARTH_OPEN_DIALOG").ok();
        if std::env::var("HEARTH_OPEN_ROUTE").ok().as_deref() == Some("new") {
            pinned_new_canvas = true;
        }
        let initial = match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        };
        let (task_tx, task_rx) = mpsc::channel();
        Self {
            route,
            nav: NavHistory::new(initial),
            panels: SessionPanels::default(),
            active_chat: String::new(),
            right_tabs: HashMap::new(),
            diff_titles: HashMap::new(),
            terminal_titles: HashMap::new(),
            subagent_tabs: HashMap::new(),
            diff_seq: 0,
            terminal_seq: 0,
            subagent_seq: 0,
            space_generation: 0,
            viewport_width: crate::app::WINDOW_SIZE.0,
            right_pane_expanded: false,
            archived_open: true,
            archived_shown: 0,
            chat_menu: None,
            rename_chat: None,
            delete_confirm: None,
            space_menu: None,
            rename_space: None,
            delete_space_confirm: None,
            add_space: None,
            spaces_menu: None,
            user_menu_open: false,
            sidebar_notice: None,
            update_dismissed: None,
            splash: SplashPhase::Visible,
            splash_deadline: None,
            space_boot_applied: false,
            pinned_new_canvas,
            fullscreen: false,
            debug_dialog,
            focus_terminal: false,
            focus_composer: false,
            composer_focused_once: false,
            bottom_stack: 0.0,
            sound_prev: HashMap::new(),
            // The app opens focused; a native focus transition corrects this
            // before the first banner (Rune's `App::on_focus`).
            window_focused: true,
            drop_hover: false,
            right_tab_drag: None,
            install: hearth_update::detect_install(),
            update_flow: UpdateFlow::Idle,
            task_tx,
            task_rx,
        }
    }
}

impl fmt::Debug for ShellState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellState")
            .field("route", &self.route)
            .field("active_chat", &self.active_chat)
            .field("splash", &self.splash)
            .field("right_pane_expanded", &self.right_pane_expanded)
            .finish_non_exhaustive()
    }
}

impl ShellState {
    /// The per-session panel key. The new-chat canvas keys per SPACE so one shared "" key
    /// doesn't make a canvas toggle read as global state.
    pub fn panel_key(&self, app: &App) -> String {
        match app.selected_chat.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                let space = app.selected_space.clone().unwrap_or_default();
                format!("space-canvas:{space}")
            }
        }
    }

    /// The chat the outlets should show: the selected chat, or "" for the canvas.
    pub fn active_chat(&self) -> &str {
        &self.active_chat
    }
}

/// The shell root: the gate decides between splash, failure card and the shell.
pub fn view(app: &App, cx: &mut Cx<App>) -> El {
    // The view is the only place `Cx` exists; record the environment for
    // `App::poll` (appearance reconciliation) and HIDPI hairlines.
    app.system_dark.set(cx.system_dark());
    app.scale.set(cx.env().scale);
    let theme = &app.theme;
    let mut layer: Vec<El> = Vec::new();
    let (body, splash) = match app.gate() {
        GatePhase::Loading => (render::splash_root(app, cx, &mut layer), true),
        GatePhase::Failed(err) => (render::gate_failed(app, theme, &err), false),
        GatePhase::Ready => {
            let body = render::shell(app, cx, &mut layer);
            let splash =
                app.shell.splash != SplashPhase::Gone && !rune::tooling::headless_run().is_some();
            (body, splash)
        }
    };
    let body = if splash {
        let fading = app.shell.splash == SplashPhase::FadingOut || app.gate() == GatePhase::Ready;
        body.child(crate::loaders::splash_overlay(theme, fading))
    } else {
        body
    };
    // Composer popups (pickers, @ mentions, / commands, lightbox) mount above
    // the shell's own overlays.
    crate::composer::popups(app, theme, &mut layer);
    if let Some(overlay) = crate::transcript::lightbox_overlay(app, theme) {
        layer.push(overlay);
    }
    rune_kit::root(body, layer)
}

// ---------------------------------------------------------------------------
// Handlers (free functions: `ShellState` lives inside `App`)
// ---------------------------------------------------------------------------

/// The rune-kit token set tuned to Hearth's own palette. `Theme::kit` is derived from three
/// opaque seeds; its warm layers don't match the green-grey elevation ladder. Shell overlays
/// read this instead so menus, dialogs and popovers paint the same surfaces as the shell.
pub fn shell_kit(theme: &Theme) -> rune_kit::Kit {
    theme.kit.clone()
}

/// Convert a persisted gpui-style combo (`mod-s`, `ctrl-shift-tab`) into Rune's `Chord`/`Shortcut`
/// spelling (`ctrl+s`, `ctrl+shift+tab`). Keymap values stay in the gpui spelling in
/// `ui-settings.json`; this is the one place that crosses the boundary.
pub fn rune_chord(combo: &str) -> String {
    crate::settings::platform_combo(combo).replace('-', "+")
}

/// Flip the sidebar and persist the choice.
pub fn toggle_sidebar(app: &mut App) {
    app.settings.sidebar_collapsed = !app.settings.sidebar_collapsed;
    if app.settings.sidebar_collapsed {
        app.shell.user_menu_open = false;
        app.shell.spaces_menu = None;
    }
    app.shell.focus_composer = true;
    app.save_settings();
}

/// Native focus change. Lead wiring (Rune's `App` builder in `lib.rs`):
///
/// ```ignore
/// .on_focus(|app: &mut App, focused: bool, _tx: &mut Tx| {
///     hearth_ui_rune::shell::on_focus(app, focused)
/// })
/// ```
///
/// `AppSpec` has no `on_focus` field; the callback belongs on the `rune::App`
/// builder next to `.on_close(..)` in [`crate::run_app`]. The notification gate
/// reads [`ShellState::window_focused`].
pub fn on_focus(app: &mut App, focused: bool) {
    app.shell.window_focused = focused;
}

/// Flip the right pane for the current session.
pub fn toggle_right_pane(app: &mut App) {
    if app.selected_chat.is_none() {
        return;
    }
    let key = app.shell.panel_key(app);
    let open = app.shell.panels.toggle_changes(&key);
    if !open {
        app.shell.right_pane_expanded = false;
    }
    app.shell.focus_composer = true;
}

/// Flip the bottom terminal drawer for the current session.
pub fn toggle_terminal(app: &mut App) {
    let key = app.shell.panel_key(app);
    let open = app.shell.panels.toggle_terminal(&key);
    if open {
        crate::terminal::ensure_tab(app);
        app.shell.focus_terminal = true;
    } else {
        app.shell.focus_composer = true;
    }
}

/// Expand/restore right-pane takeover.
pub fn toggle_right_pane_expand(app: &mut App) {
    app.shell.right_pane_expanded = !app.shell.right_pane_expanded;
    app.shell.focus_composer = true;
}

/// Whether the right pane shows: chat-scoped, never on the new-session canvas.
pub fn right_pane_open(app: &App) -> bool {
    if app.selected_chat.is_none() {
        return false;
    }
    app.shell.panels.get(&app.shell.panel_key(app)).changes_open
}

/// Whether the bottom terminal shows for the current session.
pub fn terminal_open(app: &App) -> bool {
    app.shell
        .panels
        .get(&app.shell.panel_key(app))
        .terminal_open
}

/// Open a session from the sidebar; the main area follows.
pub fn open_chat(app: &mut App, chat_id: String) {
    let entry = chat_id.clone();
    app.shell.route = Route::Chat;
    app.select_chat(Some(chat_id));
    app.shell.nav.push(NavEntry::Chat(entry));
    app.shell.focus_composer = true;
}

/// `+` in the titlebar: open the new-session canvas, honouring the sidebar filter.
pub fn open_new_session(app: &mut App) {
    app.shell.route = Route::Chat;
    let target = app
        .settings
        .space_filter
        .clone()
        .filter(|id| app.space_row(id).is_some());
    if target.is_some() {
        app.select_space(target);
    }
    app.select_chat(None);
    app.shell.nav.push(NavEntry::Chat(String::new()));
    app.shell.focus_composer = true;
}

/// Alias used by the native menu actions.
pub fn new_session(app: &mut App) {
    open_new_session(app);
}

/// Open a settings section; the titlebar names it.
pub fn open_settings(app: &mut App, section: SettingsSection) {
    app.shell.route = Route::Settings(section);
    app.settings_ui.section = section;
    app.shell.nav.push(NavEntry::Settings(section));
    app.shell.user_menu_open = false;
    app.shell.spaces_menu = None;
    app.shell.chat_menu = None;
}

/// Leave settings and return to the current chat.
pub fn close_settings(app: &mut App) {
    app.shell.route = Route::Chat;
    app.shell
        .nav
        .push(NavEntry::Chat(app.shell.active_chat.clone()));
    app.shell.focus_composer = true;
}

/// Browser Back.
pub fn navigate_back(app: &mut App) {
    if let Some(entry) = app.shell.nav.back() {
        apply_nav(app, entry);
    }
}

/// Browser Forward.
pub fn navigate_forward(app: &mut App) {
    if let Some(entry) = app.shell.nav.forward() {
        apply_nav(app, entry);
    }
}

/// Land on a history entry without recording a new one.
pub fn apply_nav(app: &mut App, entry: NavEntry) {
    match entry {
        NavEntry::Chat(chat_id) => {
            app.shell.route = Route::Chat;
            let target = (!chat_id.is_empty()).then_some(chat_id);
            if app.selected_chat != target {
                app.select_chat(target);
            }
            app.shell.focus_composer = true;
        }
        NavEntry::Settings(section) => {
            app.shell.route = Route::Settings(section);
            app.settings_ui.section = section;
        }
    }
    app.shell.user_menu_open = false;
    app.shell.chat_menu = None;
}

/// Set the sidebar's session filter (`None` = All spaces).
pub fn set_space_filter(app: &mut App, filter: Option<String>) {
    app.settings.space_filter = filter.clone();
    if let Some(space_id) = filter.clone()
        && app.selected_chat.is_none()
    {
        app.select_space(Some(space_id));
    }
    app.shell.spaces_menu = None;
    app.save_settings();
}

/// Land in a just-added space: filter to it and open the canvas there.
pub fn land_in_space(app: &mut App, space_id: String) {
    app.shell.route = Route::Chat;
    app.settings.space_filter = Some(space_id.clone());
    app.settings.last_space_id = Some(space_id.clone());
    app.select_space(Some(space_id));
    app.select_chat(None);
    app.save_settings();
}

/// Fire a `Mutate` op; failures surface in the sidebar notice strip.
pub fn mutate(app: &mut App, params: serde_json::Value) {
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        app.shell.sidebar_notice = Some("Engine not connected".to_string());
        return;
    };
    let bridge = app.bridge.clone();
    let tx = app.shell.task_tx.clone();
    app.bridge.spawn(async move {
        if let Err(err) = client.call(hearth_rpc::methods::MUTATE, params).await {
            let _ = tx.send(ShellTask::Notice(format!("{err}")));
            bridge.wake();
        }
    });
}

/// Drain shell tasks and advance boot/splash state. Call from `AppSpec::poll`; returns true
/// when visible state changed (so Rune commits a frame).
pub fn poll(app: &mut App) -> bool {
    let mut tasks = Vec::new();
    while let Ok(task) = app.shell.task_rx.try_recv() {
        tasks.push(task);
    }
    let mut changed = !tasks.is_empty();
    for task in tasks {
        match task {
            ShellTask::Notice(message) => app.shell.sidebar_notice = Some(message),
            ShellTask::UpdateStaged(Ok(path)) => app.shell.update_flow = UpdateFlow::Ready(path),
            ShellTask::UpdateStaged(Err(message)) => {
                tracing::warn!(%message, "update download failed");
                app.shell.update_flow = UpdateFlow::Failed(message);
            }
            other => changed |= spaces::apply_task(app, other),
        }
    }
    changed |= state_sync::poll(app);
    // Keep the embedded terminal surface on the tab it names.
    if let RightSurface::Terminal(key) = resolved_right_active(app) {
        crate::terminal::select_tab_by_key(app, key);
    }
    changed
}

// ---------------------------------------------------------------------------
// Surface tabs
// ---------------------------------------------------------------------------

/// The right pane's surface tabs in stored (drag-reorderable) order, skipping dead entries.
pub fn right_surface_rows(app: &App) -> Vec<(RightSurface, String)> {
    let key = app.shell.panel_key(app);
    let stored = app.shell.right_tabs.get(&key).cloned().unwrap_or_default();
    stored
        .into_iter()
        .filter_map(|surface| match surface {
            RightSurface::Diff(id) => app
                .shell
                .diff_titles
                .get(&id)
                .map(|title| (surface, title.clone())),
            RightSurface::Terminal(tab) => app
                .shell
                .terminal_titles
                .get(&tab)
                .map(|title| (surface, title.clone())),
            RightSurface::Subagent(id) => app
                .shell
                .subagent_tabs
                .get(&id)
                .map(|tab| (surface, tab.title.clone())),
            RightSurface::Shared => None,
        })
        .collect()
}

/// The surface that actually renders: the stored pick when it still exists, else the first
/// remaining tab, else the picker.
pub fn resolved_right_active(app: &App) -> RightSurface {
    let picked = app.shell.panels.get(&app.shell.panel_key(app)).right_active;
    let rows = right_surface_rows(app);
    if picked != RightSurface::Shared && rows.iter().any(|(surface, _)| *surface == picked) {
        picked
    } else {
        rows.first()
            .map(|(surface, _)| *surface)
            .unwrap_or(RightSurface::Shared)
    }
}

/// Select a right-pane surface tab.
pub fn set_right_active(app: &mut App, surface: RightSurface) {
    let key = app.shell.panel_key(app);
    app.shell.panels.update(&key, |p| p.right_active = surface);
    if matches!(surface, RightSurface::Terminal(_)) {
        app.shell.focus_terminal = true;
    }
}

/// Move a stored surface tab (`from` → `to`); the stored order is the titlebar strip's
/// order. Indices are clamped and an unchanged move is a no-op.
pub fn reorder_right_surface(app: &mut App, from: usize, to: usize) {
    let key = app.shell.panel_key(app);
    let Some(tabs) = app.shell.right_tabs.get_mut(&key) else {
        return;
    };
    if from >= tabs.len() {
        return;
    }
    let to = to.min(tabs.len() - 1);
    if from == to {
        return;
    }
    let surface = tabs.remove(from);
    tabs.insert(to, surface);
}

/// The drop index for a titlebar-strip drag starting at `from`: uniform chip slots of
/// `slot` points, the same quantisation the terminal tab strip uses.
pub fn surface_drop_index(from: usize, dx: f32, slot: f32, count: usize) -> usize {
    if count == 0 || slot <= 0.0 {
        return from;
    }
    let moved = from as f32 + (dx / slot).round();
    moved.clamp(0.0, (count - 1) as f32) as usize
}

/// Open a fresh diff surface.
pub fn add_diff_surface(app: &mut App) {
    app.shell.diff_seq += 1;
    let id = app.shell.diff_seq;
    app.shell.diff_titles.insert(id, format!("Changes {id}"));
    let key = app.shell.panel_key(app);
    app.shell
        .right_tabs
        .entry(key)
        .or_default()
        .push(RightSurface::Diff(id));
    set_right_active(app, RightSurface::Diff(id));
}

/// Open a fresh embedded terminal surface backed by a real PTY tab.
pub fn add_terminal_surface(app: &mut App) {
    let Some(tab_key) = crate::terminal::new_tab(app) else {
        return;
    };
    app.shell
        .terminal_titles
        .insert(tab_key, format!("Terminal {tab_key}"));
    let key = app.shell.panel_key(app);
    app.shell
        .right_tabs
        .entry(key)
        .or_default()
        .push(RightSurface::Terminal(tab_key));
    set_right_active(app, RightSurface::Terminal(tab_key));
}

/// A spawn chip's "Open subagent": focus the existing tab or open one.
pub fn add_subagent_surface(app: &mut App, doc_id: String, title: String) {
    if !right_pane_open(app) {
        toggle_right_pane(app);
    }
    if let Some((&id, _)) = app
        .shell
        .subagent_tabs
        .iter()
        .find(|(_, tab)| tab.doc_id == doc_id)
    {
        set_right_active(app, RightSurface::Subagent(id));
        return;
    }
    app.shell.subagent_seq += 1;
    let id = app.shell.subagent_seq;
    app.shell
        .subagent_tabs
        .insert(id, SubagentTab { doc_id, title });
    let key = app.shell.panel_key(app);
    app.shell
        .right_tabs
        .entry(key)
        .or_default()
        .push(RightSurface::Subagent(id));
    set_right_active(app, RightSurface::Subagent(id));
}

/// Close a surface tab. The active fallback happens through [`resolved_right_active`].
pub fn close_right_surface(app: &mut App, surface: RightSurface) {
    let key = app.shell.panel_key(app);
    if let Some(tabs) = app.shell.right_tabs.get_mut(&key) {
        tabs.retain(|s| *s != surface);
    }
    match surface {
        RightSurface::Diff(id) => {
            app.shell.diff_titles.remove(&id);
        }
        RightSurface::Terminal(id) => {
            crate::terminal::close_tab_by_key(app, id);
            app.shell.terminal_titles.remove(&id);
        }
        RightSurface::Subagent(id) => {
            if let Some(tab) = app.shell.subagent_tabs.remove(&id) {
                app.unwatch_subagent_doc(&tab.doc_id);
            }
        }
        RightSurface::Shared => {}
    }
    app.shell.panels.update(&key, |p| {
        if p.right_active == surface {
            p.right_active = RightSurface::Shared;
        }
    });
}

// ---------------------------------------------------------------------------
// Sidebar mutations
// ---------------------------------------------------------------------------

pub fn open_rename_chat(app: &mut App, chat_id: String) {
    app.shell.chat_menu = None;
    let current = app
        .chats
        .iter()
        .find(|c| c.id == chat_id)
        .and_then(|c| c.title.clone())
        .unwrap_or_default();
    let mut input = rune::TextEdit::new(current);
    input.focused = true;
    let len = input.text.len();
    input.set_selection(0, len);
    app.shell.rename_chat = Some(RenameChatDialog {
        chat_id,
        input,
        focus_pending: true,
    });
}

pub fn submit_rename_chat(app: &mut App) {
    let Some(dialog) = app.shell.rename_chat.take() else {
        return;
    };
    let title = dialog.input.text.trim().to_string();
    if !title.is_empty() {
        mutate(
            app,
            serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
        );
    }
}

pub fn archive_chat(app: &mut App, chat_id: String) {
    set_chat_archived(app, chat_id, true);
}

pub fn set_chat_archived(app: &mut App, chat_id: String, archived: bool) {
    app.shell.chat_menu = None;
    mutate(
        app,
        serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": archived }),
    );
}

pub fn delete_chat(app: &mut App, chat_id: String) {
    app.shell.delete_confirm = None;
    if app.selected_chat.as_deref() == Some(chat_id.as_str()) {
        app.select_chat(None);
    }
    if let Some(tabs) = app.shell.right_tabs.remove(&chat_id) {
        for surface in tabs {
            close_right_surface(app, surface);
        }
    }
    app.shell.panels.remove(&chat_id);
    app.shell.sound_prev.remove(&chat_id);
    // The chat's PTY tabs outlive its surface tabs: `close_right_surface` only
    // closes the terminal tab a surface names. This drops every terminal tab of
    // the chat (CloseTerminal per tab) whether or not it is selected.
    crate::terminal::close_chat_tabs(app, &chat_id);
    mutate(
        app,
        serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
    );
}

pub fn open_rename_space(app: &mut App, space_id: String) {
    app.shell.space_menu = None;
    let current = app
        .space_row(&space_id)
        .map(|s| s.display_name().to_string())
        .unwrap_or_default();
    let mut input = rune::TextEdit::new(current);
    input.focused = true;
    let len = input.text.len();
    input.set_selection(0, len);
    app.shell.rename_space = Some(RenameSpaceDialog {
        space_id,
        input,
        focus_pending: true,
    });
}

pub fn submit_rename_space(app: &mut App) {
    let Some(dialog) = app.shell.rename_space.take() else {
        return;
    };
    let name = dialog.input.text.trim().to_string();
    if !name.is_empty() {
        mutate(
            app,
            serde_json::json!({ "op": "renameSpace", "spaceId": dialog.space_id, "name": name }),
        );
    }
}

pub fn delete_space(app: &mut App, space_id: String) {
    app.shell.delete_space_confirm = None;
    mutate(
        app,
        serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
    );
}

/// Dismiss the inline sidebar error strip.
pub fn dismiss_notice(app: &mut App) {
    app.shell.sidebar_notice = None;
}

/// Dismiss the update strip for a version.
pub fn dismiss_update(app: &mut App, version: String) {
    app.shell.update_dismissed = Some(version);
}

// ---------------------------------------------------------------------------
// In-app update
// ---------------------------------------------------------------------------

/// What clicking the update strip does, from the install kind and the current phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateClick {
    /// Unmanaged (source) install: hide the strip for this version.
    Dismiss,
    /// Fetch the manifest and stage the payload.
    Download,
    /// A download is in flight: the strip is not clickable.
    Wait,
    /// A payload is staged: swap it in and relaunch.
    Apply,
}

/// Hearth's `on_update_strip_click` decision, without side effects.
pub fn update_click_action(install: &hearth_update::InstallKind, flow: &UpdateFlow) -> UpdateClick {
    use hearth_update::InstallKind;
    match install {
        InstallKind::Unmanaged => UpdateClick::Dismiss,
        InstallKind::MacApp { .. } | InstallKind::Managed { .. } => match flow {
            UpdateFlow::Idle | UpdateFlow::Failed(_) => UpdateClick::Download,
            UpdateFlow::Downloading => UpdateClick::Wait,
            UpdateFlow::Ready(_) => UpdateClick::Apply,
        },
    }
}

/// The update strip's press handler (takes `Tx` so a staged payload can quit+relaunch).
pub fn on_update_strip(app: &mut App, tx: &mut Tx) {
    match update_click_action(&app.shell.install, &app.shell.update_flow) {
        UpdateClick::Dismiss => {
            if let Some(version) = app
                .update
                .as_ref()
                .and_then(|status| status.latest_version.clone())
            {
                dismiss_update(app, version);
            }
        }
        UpdateClick::Download => begin_update_download(app),
        UpdateClick::Wait => {}
        UpdateClick::Apply => {
            if let UpdateFlow::Ready(staged) = app.shell.update_flow.clone() {
                apply_staged_update(app, &staged, tx);
            }
        }
    }
}

/// Fetch the manifest and stage the update payload (macOS app tarball or managed
/// headless tarball). The result comes back as [`ShellTask::UpdateStaged`].
fn begin_update_download(app: &mut App) {
    app.shell.update_flow = UpdateFlow::Downloading;
    let edge_url = app.config.edge_url.clone();
    let data_dir = app.config.data_dir.clone();
    let install = app.shell.install.clone();
    let bridge = app.bridge.clone();
    let tx = app.shell.task_tx.clone();
    app.bridge.spawn(async move {
        let outcome = stage_update(&edge_url, &data_dir, &install).await;
        let _ = tx.send(ShellTask::UpdateStaged(outcome));
        bridge.wake();
    });
}

/// The download half of the flow (hearth's `begin_update_download` body).
async fn stage_update(
    edge_url: &str,
    data_dir: &std::path::Path,
    install: &hearth_update::InstallKind,
) -> Result<String, String> {
    use hearth_update::InstallKind;
    let manifest = hearth_update::fetch_latest(edge_url)
        .await
        .map_err(|err| format!("{err:#}"))?;
    match install {
        InstallKind::MacApp { .. } => hearth_update::stage_mac_app(edge_url, &manifest, data_dir)
            .await
            .map(|path| path.display().to_string())
            .map_err(|err| format!("{err:#}")),
        InstallKind::Managed { app_root } => {
            hearth_update::stage_headless(edge_url, &manifest, app_root)
                .await
                .map_err(|err| format!("{err:#}"))?;
            hearth_update::apply_headless(app_root, &manifest.version)
                .map_err(|err| format!("{err:#}"))?;
            let _ = hearth_update::restart_service();
            Ok(app_root
                .join("current")
                .join("hearth")
                .display()
                .to_string())
        }
        InstallKind::Unmanaged => Err("this install is not update-managed".to_string()),
    }
}

/// Swap a staged payload in and relaunch (hearth's `apply_staged_update`). macOS
/// replaces the `.app` bundle; a managed install already flipped `current` during
/// the download and only relaunches the new exe.
fn apply_staged_update(app: &mut App, staged: &str, tx: &mut Tx) {
    use hearth_update::InstallKind;
    match app.shell.install.clone() {
        InstallKind::MacApp { bundle } => {
            match hearth_update::apply_mac_app(std::path::Path::new(staged), &bundle) {
                Ok(()) => {
                    hearth_update::relaunch_app_after_exit(&bundle);
                    tx.quit();
                }
                Err(err) => {
                    tracing::error!(error = %err, "update apply failed");
                    app.shell.update_flow = UpdateFlow::Failed(format!("{err:#}"));
                }
            }
        }
        InstallKind::Managed { .. } => {
            hearth_update::relaunch_exe_after_exit(std::path::Path::new(staged));
            tx.quit();
        }
        InstallKind::Unmanaged => {}
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;
    use hearth_update::InstallKind;

    fn managed() -> InstallKind {
        InstallKind::Managed {
            app_root: std::path::PathBuf::from("/home/u/.hearth/app"),
        }
    }

    #[test]
    fn update_click_follows_the_install_kind_and_phase() {
        assert_eq!(
            update_click_action(&InstallKind::Unmanaged, &UpdateFlow::Idle),
            UpdateClick::Dismiss
        );
        assert_eq!(
            update_click_action(&managed(), &UpdateFlow::Idle),
            UpdateClick::Download
        );
        assert_eq!(
            update_click_action(&managed(), &UpdateFlow::Failed("x".into())),
            UpdateClick::Download
        );
        assert_eq!(
            update_click_action(&managed(), &UpdateFlow::Downloading),
            UpdateClick::Wait
        );
        assert_eq!(
            update_click_action(&managed(), &UpdateFlow::Ready("/tmp/x".into())),
            UpdateClick::Apply
        );
        assert_eq!(
            update_click_action(
                &InstallKind::MacApp {
                    bundle: std::path::PathBuf::from("/Applications/Hearth.app")
                },
                &UpdateFlow::Ready("/tmp/x".into())
            ),
            UpdateClick::Apply
        );
    }

    #[test]
    fn surface_drop_index_quantizes_uniform_slots() {
        assert_eq!(surface_drop_index(0, 0.0, 116.0, 3), 0);
        assert_eq!(surface_drop_index(0, 60.0, 116.0, 3), 1);
        assert_eq!(surface_drop_index(0, -400.0, 116.0, 3), 0);
        assert_eq!(surface_drop_index(2, 400.0, 116.0, 3), 2);
        assert_eq!(surface_drop_index(1, 116.0, 116.0, 3), 2);
        // Empty strip / nonsense slot: stay put.
        assert_eq!(surface_drop_index(0, 50.0, 116.0, 0), 0);
        assert_eq!(surface_drop_index(2, 50.0, 0.0, 3), 2);
    }
}
