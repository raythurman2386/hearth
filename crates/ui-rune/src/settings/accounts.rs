//! Settings → Accounts: provider cards (Codex) with account rows — email, plan
//! badge, Active, usage meters (indigo → amber ≥80% → red ≥95%, reset time),
//! Switch / Forget — plus the add-account dialogs (paste-code and browser-poll
//! flows) and account-shaped loading skeletons.
//!
//! Ported from `crates/ui/src/settings/accounts.rs`. The gpui entity and tasks
//! are replaced by page state plus one-shot bridge jobs drained by
//! [`AccountsPage::poll`].

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hearth_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, HarnessId,
};
use hearth_rpc::methods;
use rune::{Align, Color, El, TextStyle, Tx, col, el, row, text};
use rune_kit::Placement;

use crate::icons;
use crate::pickers::Loadable;
use crate::state::App;
use crate::theme::Theme;

use super::widgets;

// ---------------------------------------------------------------------------
// Pure: usage meters + labels
// ---------------------------------------------------------------------------

pub const USAGE_WARN_FRACTION: f32 = 0.80;
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

/// How long the browser login poll waits between calls.
pub const LOGIN_POLL_MS: u64 = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// < 80% — indigo.
    Normal,
    /// ≥ 80% — amber.
    Warn,
    /// ≥ 95% — red.
    Critical,
}

/// Threshold classification of a usage fraction. Pure.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}

pub fn usage_color(level: UsageLevel, theme: &Theme) -> Color {
    match level {
        UsageLevel::Normal => theme.accent,
        UsageLevel::Warn => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// Why a `ListAgentAccounts` load is happening. Pure input to
/// [`force_usage_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadTrigger {
    /// Page construction — the visit's first list.
    Mount,
    /// "Click to retry" after a failed load — still the visit's first
    /// successful list.
    Retry,
    /// The explicit Refresh button.
    Refresh,
    /// After a completed add-account login flow.
    PostLogin,
    /// After Switch/Forget succeeds.
    PostAction,
}

/// Whether a load should ask the engine to probe usage (`forceUsage`). The
/// engine only hits the provider when forced; non-forced lists serve its 60s
/// usage cache or nothing. The visit's first list must force, or every first
/// open renders "Usage unavailable" until a manual Refresh. Post-Switch/Forget
/// lists ride the still-warm cache. Pure.
pub fn force_usage_for(trigger: LoadTrigger) -> bool {
    match trigger {
        LoadTrigger::Mount | LoadTrigger::Retry | LoadTrigger::Refresh | LoadTrigger::PostLogin => {
            true
        }
        LoadTrigger::PostAction => false,
    }
}

/// Compact absolute reset moment: a local clock time ("resets 3:45 PM") when it
/// lands within ~22h, a short weekday ("Mon") within a week, else month + day
/// ("Sep 14"). Pure given `now`.
pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    use chrono::Local;
    let at = resets_at?;
    let local = at.with_timezone(&Local);
    Some(if at.signed_duration_since(now).num_hours() < 22 {
        format!("resets {}", local.format("%-I:%M %p"))
    } else if at.signed_duration_since(now).num_hours() < 24 * 7 {
        format!("resets {}", local.format("%a"))
    } else {
        format!("resets {}", local.format("%b %-d"))
    })
}

/// The provider cards, in display order: (harness, name, CLI command).
pub const PROVIDERS: [(HarnessId, &str, &str); 1] = [(HarnessId::Codex, "Codex", "codex")];

/// Accounts of one provider, in the engine's order (slot creation). No
/// active-first re-sort: switching accounts must not move the switched-to card.
/// Pure.
pub fn provider_accounts(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Vec<&AgentAccount> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .collect()
}

/// Merge the `targetDeviceId` passthrough into a params object. Pure.
pub fn with_target(mut value: serde_json::Value, target: Option<&str>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, value.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(target));
    }
    value
}

// ---------------------------------------------------------------------------
// Page state
// ---------------------------------------------------------------------------

/// The add-account flow's dialog state.
#[derive(Debug, Clone)]
pub enum LoginFlow {
    /// StartAgentLogin in flight.
    Starting { harness: HarnessId },
    /// Claude-style: open the URL, paste the code back.
    PasteCode {
        harness: HarnessId,
        start: AgentLoginStart,
        submitting: bool,
        error: Option<String>,
    },
    /// Codex-style: open the URL, poll until the browser flow lands.
    Browser {
        harness: HarnessId,
        start: AgentLoginStart,
        message: Option<String>,
        error: Option<String>,
    },
}

impl LoginFlow {
    fn harness(&self) -> HarnessId {
        match self {
            LoginFlow::Starting { harness }
            | LoginFlow::PasteCode { harness, .. }
            | LoginFlow::Browser { harness, .. } => *harness,
        }
    }

    /// Dialog title.
    fn title(&self) -> &'static str {
        match self.harness() {
            HarnessId::Codex => "Add Codex account",
            _ => "Add account",
        }
    }
}

type LoadSlot = Arc<Mutex<Option<(u64, Result<AgentAccountsSnapshot, String>)>>>;
type StartSlot = Arc<Mutex<Option<(u64, Result<AgentLoginStart, String>)>>>;
type PollSlot = Arc<Mutex<Option<(u64, Result<AgentLoginPoll, String>)>>>;
type CompleteSlot = Arc<Mutex<Option<(u64, Result<(), String>)>>>;

#[derive(Debug)]
enum ActionOutcome {
    Succeeded,
    Failed(String),
}

type ActionSlot = Arc<Mutex<Option<(u64, ActionOutcome)>>>;

/// View state for the Accounts page.
pub struct AccountsPage {
    /// Which device's logins are shown; `None` = this device.
    pub target_device: Option<String>,
    pub device_menu_open: bool,
    pub snapshot: Loadable<AgentAccountsSnapshot>,
    /// Account id with an in-flight Switch/Forget.
    pub busy_account: Option<String>,
    pub login: Option<LoginFlow>,
    pub error: Option<String>,
    /// The paste-code field.
    pub code: rune::TextEdit,
    /// A URL the next frame should open in the browser (the login start's
    /// authorization page). `poll` cannot reach a `Tx`, so this is handed to
    /// the lead's `App::poll`, which has one.
    pending_open_url: Option<String>,

    harness: Option<HarnessId>,
    load_rx: LoadSlot,
    start_rx: StartSlot,
    poll_rx: PollSlot,
    complete_rx: CompleteSlot,
    action_rx: ActionSlot,
    load_gen: Arc<AtomicU64>,
    flow_gen: Arc<AtomicU64>,
    action_gen: Arc<AtomicU64>,
    last_load: Cell<u64>,
    active_flow: Cell<u64>,
    active_action: Cell<u64>,
    poll_cancel: Arc<AtomicBool>,
    started: Cell<bool>,
}

impl std::fmt::Debug for AccountsPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountsPage")
            .field("target_device", &self.target_device)
            .field("snapshot", &self.snapshot)
            .field("busy_account", &self.busy_account)
            .field("login", &self.login)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl Default for AccountsPage {
    fn default() -> Self {
        Self {
            target_device: None,
            device_menu_open: false,
            snapshot: Loadable::Idle,
            busy_account: None,
            login: None,
            error: None,
            code: rune::TextEdit::new(""),
            pending_open_url: None,
            harness: None,
            load_rx: Arc::new(Mutex::new(None)),
            start_rx: Arc::new(Mutex::new(None)),
            poll_rx: Arc::new(Mutex::new(None)),
            complete_rx: Arc::new(Mutex::new(None)),
            action_rx: Arc::new(Mutex::new(None)),
            load_gen: Arc::new(AtomicU64::new(0)),
            flow_gen: Arc::new(AtomicU64::new(0)),
            action_gen: Arc::new(AtomicU64::new(0)),
            last_load: Cell::new(0),
            active_flow: Cell::new(0),
            active_action: Cell::new(0),
            poll_cancel: Arc::new(AtomicBool::new(false)),
            started: Cell::new(false),
        }
    }
}

fn alloc(counter: &AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::Relaxed) + 1
}

fn snapshot_slot(result: Result<AgentAccountsSnapshot, String>) -> Loadable<AgentAccountsSnapshot> {
    match result {
        Ok(snapshot) => Loadable::Ready(snapshot),
        Err(message) => Loadable::Error(message),
    }
}

fn begin_load(app: &App, slot: LoadSlot, target: Option<String>, force_usage: bool, id: u64) {
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        *slot.lock().unwrap() = Some((id, Err("Engine not connected".to_string())));
        app.bridge.wake();
        return;
    };
    let bridge = app.bridge.clone();
    let params = with_target(
        serde_json::json!({ "forceUsage": force_usage }),
        target.as_deref(),
    );
    app.bridge.spawn(async move {
        let result = client
            .call(methods::LIST_AGENT_ACCOUNTS, params)
            .await
            .map_err(|err| err.to_string())
            .and_then(|value| {
                serde_json::from_value::<AgentAccountsSnapshot>(value)
                    .map_err(|err| err.to_string())
            });
        *slot.lock().unwrap() = Some((id, result));
        bridge.wake();
    });
}

impl AccountsPage {
    /// Take the login authorization URL `poll` received, for the lead to open
    /// with `Tx::open_url` (the gpui page called `cx.open_url` there).
    pub fn take_open_url(&mut self) -> Option<String> {
        self.pending_open_url.take()
    }

    /// First-render load (forced usage probe).
    pub fn ensure_loaded(&self, app: &App) {
        if self.started.get() || app.engine.is_none() {
            return;
        }
        self.started.set(true);
        let id = alloc(&self.load_gen);
        self.last_load.set(id);
        begin_load(
            app,
            self.load_rx.clone(),
            self.target_device.clone(),
            force_usage_for(LoadTrigger::Mount),
            id,
        );
    }

    /// Drain finished loads, login steps and account actions.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;

        if let Some((id, result)) = self.load_rx.lock().unwrap().take()
            && id >= self.last_load.get()
        {
            self.last_load.set(id);
            self.snapshot = snapshot_slot(result);
            changed = true;
        }

        if let Some((id, result)) = self.start_rx.lock().unwrap().take()
            && id == self.active_flow.get()
        {
            changed = true;
            match result {
                Ok(start) => {
                    let harness = self.harness.unwrap_or(HarnessId::Codex);
                    self.pending_open_url = Some(start.url.clone());
                    self.code = rune::TextEdit::new("");
                    self.login = Some(match start.mode {
                        AgentLoginMode::PasteCode => LoginFlow::PasteCode {
                            harness,
                            start,
                            submitting: false,
                            error: None,
                        },
                        AgentLoginMode::Browser => LoginFlow::Browser {
                            harness,
                            start,
                            message: None,
                            error: None,
                        },
                    });
                }
                Err(err) => {
                    self.login = None;
                    self.harness = None;
                    self.error = Some(format!("Login failed to start: {err}"));
                }
            }
        }

        if let Some((id, result)) = self.poll_rx.lock().unwrap().take()
            && id == self.active_flow.get()
        {
            changed = true;
            if let Some(LoginFlow::Browser { message, error, .. }) = &mut self.login {
                match result {
                    Ok(poll) => match poll.status {
                        AgentLoginStatus::Done => {
                            self.login = None;
                            self.harness = None;
                        }
                        AgentLoginStatus::Error => {
                            *error =
                                Some(poll.message.unwrap_or_else(|| "Login failed".to_string()));
                        }
                        AgentLoginStatus::Pending => {
                            if let Some(text) = poll.message {
                                *message = Some(text);
                            }
                        }
                    },
                    Err(err) => *error = Some(format!("Poll failed: {err}")),
                }
            }
        }

        if let Some((id, result)) = self.complete_rx.lock().unwrap().take()
            && id == self.active_flow.get()
        {
            changed = true;
            match result {
                Ok(()) => {
                    self.login = None;
                    self.harness = None;
                }
                Err(err) => {
                    if let Some(LoginFlow::PasteCode {
                        submitting, error, ..
                    }) = &mut self.login
                    {
                        *submitting = false;
                        *error = Some(err);
                    }
                }
            }
        }

        if let Some((id, outcome)) = self.action_rx.lock().unwrap().take()
            && id == self.active_action.get()
        {
            changed = true;
            self.busy_account = None;
            if let ActionOutcome::Failed(err) = outcome {
                self.error = Some(err);
            }
        }

        changed
    }
}

/// Reload the account list.
pub fn reload(app: &mut App, trigger: LoadTrigger) {
    let page = &mut app.settings_ui.accounts;
    let id = alloc(&page.load_gen);
    page.last_load.set(id);
    page.snapshot = Loadable::Loading;
    page.error = None;
    page.started.set(true);
    let slot = page.load_rx.clone();
    let target = page.target_device.clone();
    begin_load(app, slot, target, force_usage_for(trigger), id);
}

/// Retarget the page at another device's logins and reload.
pub fn set_target_device(app: &mut App, target: Option<String>) {
    let page = &mut app.settings_ui.accounts;
    page.device_menu_open = false;
    if page.target_device == target {
        return;
    }
    page.target_device = target;
    page.login = None;
    page.busy_account = None;
    page.error = None;
    page.active_flow.set(0);
    page.active_action.set(0);
    page.poll_cancel.store(true, Ordering::Relaxed);
    load_now(app, LoadTrigger::Mount);
}

fn load_now(app: &mut App, trigger: LoadTrigger) {
    let page = &mut app.settings_ui.accounts;
    let id = alloc(&page.load_gen);
    page.last_load.set(id);
    page.snapshot = Loadable::Loading;
    page.error = None;
    page.started.set(true);
    let slot = page.load_rx.clone();
    let target = page.target_device.clone();
    begin_load(app, slot, target, force_usage_for(trigger), id);
}

// ---- account actions ----

/// Switch / Forget an account.
pub fn account_action(app: &mut App, method: &'static str, account_id: String, harness: HarnessId) {
    let page = &mut app.settings_ui.accounts;
    page.busy_account = Some(account_id.clone());
    page.error = None;
    let action_id = alloc(&page.action_gen);
    page.active_action.set(action_id);
    let load_id = alloc(&page.load_gen);
    page.last_load.set(load_id);
    let slot = page.action_rx.clone();
    let load_slot = page.load_rx.clone();
    let target = page.target_device.clone();
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        page.busy_account = None;
        page.error = Some("Engine not connected".to_string());
        return;
    };
    let bridge = app.bridge.clone();
    let params = with_target(
        serde_json::json!({
            "id": account_id,
            "accountId": account_id,
            "harness": harness,
        }),
        target.as_deref(),
    );
    app.bridge.spawn(async move {
        match client.call(method, params).await {
            Ok(_) => {
                *slot.lock().unwrap() = Some((action_id, ActionOutcome::Succeeded));
                // Ride the still-warm 60s usage cache.
                let list = client
                    .call(
                        methods::LIST_AGENT_ACCOUNTS,
                        with_target(
                            serde_json::json!({
                                "forceUsage": force_usage_for(LoadTrigger::PostAction)
                            }),
                            target.as_deref(),
                        ),
                    )
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|value| {
                        serde_json::from_value::<AgentAccountsSnapshot>(value)
                            .map_err(|err| err.to_string())
                    });
                *load_slot.lock().unwrap() = Some((load_id, list));
            }
            Err(err) => {
                *slot.lock().unwrap() = Some((action_id, ActionOutcome::Failed(err.to_string())));
            }
        }
        bridge.wake();
    });
}

// ---- add-account flows ----

/// Start `StartAgentLogin` for a harness.
pub fn start_login(app: &mut App, harness: HarnessId) {
    let page = &mut app.settings_ui.accounts;
    page.login = Some(LoginFlow::Starting { harness });
    page.harness = Some(harness);
    page.error = None;
    page.poll_cancel.store(false, Ordering::Relaxed);
    let flow_id = alloc(&page.flow_gen);
    page.active_flow.set(flow_id);
    let start_slot = page.start_rx.clone();
    let poll_slot = page.poll_rx.clone();
    let load_slot = page.load_rx.clone();
    let load_gen = page.load_gen.clone();
    let cancel = page.poll_cancel.clone();
    let target = page.target_device.clone();
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        page.login = None;
        page.harness = None;
        page.error = Some("Engine not connected".to_string());
        return;
    };
    let bridge = app.bridge.clone();
    let params = with_target(serde_json::json!({ "harness": harness }), target.as_deref());
    app.bridge.spawn(async move {
        let start = match client
            .call(methods::START_AGENT_LOGIN, params)
            .await
            .map_err(|err| err.to_string())
            .and_then(|value| {
                serde_json::from_value::<AgentLoginStart>(value).map_err(|err| err.to_string())
            }) {
            Ok(start) => start,
            Err(err) => {
                *start_slot.lock().unwrap() = Some((flow_id, Err(err)));
                bridge.wake();
                return;
            }
        };
        *start_slot.lock().unwrap() = Some((flow_id, Ok(start.clone())));
        bridge.wake();
        if start.mode != AgentLoginMode::Browser {
            return;
        }
        let poll_params = with_target(
            serde_json::json!({ "loginId": start.login_id }),
            target.as_deref(),
        );
        loop {
            tokio::time::sleep(Duration::from_millis(LOGIN_POLL_MS)).await;
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let result = client
                .call(methods::POLL_AGENT_LOGIN, poll_params.clone())
                .await
                .map_err(|err| err.to_string())
                .and_then(|value| {
                    serde_json::from_value::<AgentLoginPoll>(value).map_err(|err| err.to_string())
                });
            let done = match &result {
                Ok(poll) => poll.status != AgentLoginStatus::Pending,
                Err(_) => true,
            };
            let completed = matches!(
                &result,
                Ok(AgentLoginPoll {
                    status: AgentLoginStatus::Done,
                    ..
                })
            );
            *poll_slot.lock().unwrap() = Some((flow_id, result));
            if done {
                if completed {
                    let list_id = alloc(&load_gen);
                    let list = client
                        .call(
                            methods::LIST_AGENT_ACCOUNTS,
                            with_target(
                                serde_json::json!({
                                    "forceUsage": force_usage_for(LoadTrigger::PostLogin)
                                }),
                                target.as_deref(),
                            ),
                        )
                        .await
                        .map_err(|err| err.to_string())
                        .and_then(|value| {
                            serde_json::from_value::<AgentAccountsSnapshot>(value)
                                .map_err(|err| err.to_string())
                        });
                    *load_slot.lock().unwrap() = Some((list_id, list));
                }
                bridge.wake();
                break;
            }
            bridge.wake();
        }
    });
}

/// Submit the pasted authorization code.
pub fn submit_code(app: &mut App) {
    let Some(LoginFlow::PasteCode {
        start, submitting, ..
    }) = &mut app.settings_ui.accounts.login
    else {
        return;
    };
    if *submitting {
        return;
    }
    let code = app.settings_ui.accounts.code.text.trim().to_string();
    if code.is_empty() {
        return;
    }
    let login_id = start.login_id.clone();
    *submitting = true;
    let page = &mut app.settings_ui.accounts;
    let flow_id = page.active_flow.get();
    let slot = page.complete_rx.clone();
    let load_slot = page.load_rx.clone();
    let load_gen = page.load_gen.clone();
    let target = page.target_device.clone();
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        return;
    };
    let bridge = app.bridge.clone();
    let params = with_target(
        serde_json::json!({ "loginId": login_id, "code": code }),
        target.as_deref(),
    );
    app.bridge.spawn(async move {
        match client.call(methods::COMPLETE_AGENT_LOGIN, params).await {
            Ok(_) => {
                *slot.lock().unwrap() = Some((flow_id, Ok(())));
                let list_id = alloc(&load_gen);
                let list = client
                    .call(
                        methods::LIST_AGENT_ACCOUNTS,
                        with_target(
                            serde_json::json!({
                                "forceUsage": force_usage_for(LoadTrigger::PostLogin)
                            }),
                            target.as_deref(),
                        ),
                    )
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|value| {
                        serde_json::from_value::<AgentAccountsSnapshot>(value)
                            .map_err(|err| err.to_string())
                    });
                *load_slot.lock().unwrap() = Some((list_id, list));
            }
            Err(err) => {
                *slot.lock().unwrap() = Some((flow_id, Err(err.to_string())));
            }
        }
        bridge.wake();
    });
}

/// Dismiss the login dialog and cancel the engine-side flow (best effort).
pub fn cancel_login(app: &mut App) {
    let page = &mut app.settings_ui.accounts;
    let login_id = match &page.login {
        Some(LoginFlow::PasteCode { start, .. }) | Some(LoginFlow::Browser { start, .. }) => {
            Some(start.login_id.clone())
        }
        _ => None,
    };
    page.login = None;
    page.harness = None;
    page.active_flow.set(0);
    page.poll_cancel.store(true, Ordering::Relaxed);
    let target = page.target_device.clone();
    if let (Some(login_id), Some(client)) = (
        login_id,
        app.engine.as_ref().map(|engine| engine.client.clone()),
    ) {
        let params = with_target(
            serde_json::json!({ "loginId": login_id }),
            target.as_deref(),
        );
        app.bridge.spawn(async move {
            if let Err(err) = client.call(methods::CANCEL_AGENT_LOGIN, params).await {
                tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
            }
        });
    }
}

// ---- render pieces ----

fn open_device_menu(app: &mut App) {
    let page = &mut app.settings_ui.accounts;
    page.device_menu_open = !page.device_menu_open;
}

fn close_device_menu(app: &mut App) {
    app.settings_ui.accounts.device_menu_open = false;
}

fn platform_glyph(platform: &str) -> &'static rune::Icon {
    match platform {
        "macos" | "darwin" => &icons::LAPTOP,
        "ios" | "android" => &icons::SMARTPHONE,
        _ => &icons::MONITOR,
    }
}

fn provider_mark(harness: HarnessId, theme: &Theme) -> El {
    let mark = match harness {
        HarnessId::Codex => &icons::OPENAI_MARK,
        HarnessId::Grok => &icons::GROK_MARK,
        HarnessId::Huginn | HarnessId::Mock => &icons::HUGINN_MARK,
    };
    el().w(24.0)
        .h(24.0)
        .center()
        .child(icons::icon(mark, 16.0).color(theme.text_muted))
}

fn device_switcher(page: &AccountsPage, app: &App, theme: &Theme, layer: &mut Vec<El>) -> El {
    let mut devices = app.devices.clone();
    devices.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    let local_id = app.local_device_id.clone();
    let effective = page.target_device.clone().or_else(|| local_id.clone());
    let selected = devices
        .iter()
        .find(|d| Some(d.id.as_str()) == effective.as_deref());
    let trigger_glyph = platform_glyph(selected.map(|d| d.platform.as_str()).unwrap_or("macos"));
    let trigger_label = selected
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "This device".to_string());
    let open = page.device_menu_open;

    let trigger = row()
        .h(28.0)
        .px(8.0)
        .radius(6.0)
        .gap(6.0)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .fill(if open {
            theme.ink(0.06)
        } else {
            theme.ink(0.0)
        })
        .hover(theme.ink(0.04))
        .on_press(|app: &mut App| open_device_menu(app))
        .child(icons::icon(trigger_glyph, 16.0).color(theme.text_muted))
        .child(
            text(trigger_label)
                .min_w(0.0)
                .truncate()
                .font(TextStyle::new(12.5, 500.0))
                .color(theme.text),
        )
        .child(
            el().w(6.0)
                .h(6.0)
                .radius(3.0)
                .fill(if effective == local_id {
                    theme.success
                } else {
                    theme.ink(0.2)
                }),
        )
        .child(
            icons::icon(&icons::SORT_VERTICAL, 14.0).color(theme.text_muted.alpha(if open {
                0.9
            } else {
                0.4
            })),
        );

    let mut body = col().w(220.0).gap(2.0).child(
        text("Devices")
            .font(TextStyle::new(11.0, 500.0))
            .color(theme.text_muted.alpha(0.6))
            .px(8.0)
            .py(4.0),
    );
    for (ix, device) in devices.iter().enumerate() {
        let is_active = Some(device.id.as_str()) == effective.as_deref();
        let is_local = local_id.as_deref() == Some(device.id.as_str());
        let glyph = platform_glyph(&device.platform);
        let pick_id = device.id.clone();
        let pick_local = is_local;
        let mut item = row()
            .key(format!("settings/accounts-device-row-{ix}"))
            .w_full()
            .h(30.0)
            .px(8.0)
            .radius(6.0)
            .gap(8.0)
            .center_y()
            .cursor(rune::Cursor::Pointer)
            .fill(if is_active {
                theme.ink(0.06)
            } else {
                theme.ink(0.0)
            })
            .hover(theme.ink(0.06))
            .on_press(move |app: &mut App| {
                set_target_device(app, (!pick_local).then(|| pick_id.clone()));
            })
            .child(icons::icon(glyph, 16.0).color(theme.text_muted))
            .child(
                text(device.name.clone())
                    .grow(1.0)
                    .min_w(0.0)
                    .truncate()
                    .font(TextStyle::new(13.0, 400.0))
                    .color(theme.text),
            );
        if is_local {
            item = item.child(
                text("You")
                    .font(TextStyle::new(10.5, 400.0))
                    .color(theme.text_muted.alpha(0.35)),
            );
        }
        item = item.child(el().w(6.0).h(6.0).radius(3.0).fill(if is_local {
            theme.success
        } else {
            theme.ink(0.2)
        }));
        body = body.child(item);
    }

    rune_kit::Popover::new("settings/accounts-device")
        .trigger(trigger)
        .open(open)
        .placement(Placement::BottomEnd)
        .padding(4.0)
        .on_close(|app: &mut App| close_device_menu(app))
        .child(body)
        .build(&crate::shell::shell_kit(theme), layer)
}

fn usage_meter(window: &hearth_proto::AgentUsageWindow, theme: &Theme, now: DateTime<Utc>) -> El {
    let fraction = window.used_fraction.clamp(0.0, 1.0);
    let level = usage_level(fraction);
    let fill = usage_color(level, theme).alpha(match level {
        UsageLevel::Normal => 0.8,
        _ => 0.85,
    });
    let reset = format_reset(window.resets_at, now);
    let mut meter = row()
        .gap(8.0)
        .center_y()
        .child(
            text(window.label.clone())
                .w(48.0)
                .truncate()
                .font(TextStyle::new(11.5, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        )
        .child({
            let mut bar = el()
                .grow(1.0)
                .min_w(56.0)
                .max_w(230.0)
                .h(5.0)
                .radius(3.0)
                .fill(theme.ink(0.07));
            if fraction > 0.0 {
                // A 1.5% floor keeps tiny non-zero usage visible.
                bar = bar.child(
                    el().h_full()
                        .w_frac(fraction.max(0.015))
                        .radius(3.0)
                        .fill(fill),
                );
            }
            bar
        })
        .child(
            text(format!("{}% used", (fraction * 100.0).round() as u32))
                .w(64.0)
                .align(Align::End)
                .font(TextStyle::new(11.5, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        );
    if let Some(reset) = reset {
        meter = meter.child(
            text(reset)
                .truncate()
                .font(TextStyle::new(11.5, 400.0))
                .color(theme.text_muted.alpha(0.45)),
        );
    }
    meter
}

fn account_row(
    page: &AccountsPage,
    account: &AgentAccount,
    ix: usize,
    first: bool,
    theme: &Theme,
    now: DateTime<Utc>,
) -> El {
    let is_busy = page.busy_account.as_deref() == Some(account.id.as_str());
    let email = account
        .email
        .clone()
        .or_else(|| account.display_name.clone())
        .unwrap_or_else(|| "Unknown account".to_string());
    let initial = email
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());

    let mut badges = row().gap(6.0).items_center();
    if account.active {
        badges = badges.child(widgets::badge_active(theme, "Active"));
    }
    if let Some(plan) = account.plan_label.clone() {
        badges = badges.child(widgets::badge(theme, plan));
    }

    let mut usage = col().mt(6.0).gap(4.0);
    if account.usage_windows.is_empty() {
        usage = usage.child(
            text(if account.switchable {
                "Usage unavailable"
            } else {
                "Credentials unavailable"
            })
            .truncate()
            .font(TextStyle::new(11.5, 400.0))
            .color(theme.text_muted.alpha(0.6)),
        );
    } else {
        for window in &account.usage_windows {
            usage = usage.child(usage_meter(window, theme, now));
        }
    }

    let mut right = col().items_end().justify_between().gap(8.0).child(badges);
    if !account.active {
        let forget_id = account.id.clone();
        let switch_id = account.id.clone();
        let switch_harness = account.harness;
        right = right.child(
            row()
                .gap(4.0)
                .items_center()
                .child(
                    widgets::ghost_action(theme)
                        .opacity(if is_busy { 0.5 } else { 1.0 })
                        .hover(theme.ink(0.06))
                        .on_press(move |app: &mut App| {
                            account_action(
                                app,
                                methods::FORGET_AGENT_ACCOUNT,
                                forget_id.clone(),
                                switch_harness,
                            )
                        })
                        .child(
                            icons::icon(&icons::TRASH_BIN_MINIMALISTIC, 14.0)
                                .color(theme.text_muted),
                        ),
                )
                .child(if account.switchable {
                    rune_kit::Button::new(if is_busy { "Switching…" } else { "Switch" })
                        .variant(rune_kit::ButtonVariant::Contrast)
                        .on_press(move |app: &mut App| {
                            account_action(
                                app,
                                methods::ACTIVATE_AGENT_ACCOUNT,
                                switch_id.clone(),
                                switch_harness,
                            )
                        })
                        .build(&crate::shell::shell_kit(theme))
                        .opacity(if is_busy { 0.5 } else { 1.0 })
                } else {
                    el()
                }),
        );
    }

    row()
        .key(format!("account-row-{ix}"))
        .px(20.0)
        .py(14.0)
        .items_center()
        .gap(12.0)
        .border_sides(
            if first {
                [0.0; 4]
            } else {
                [1.0, 0.0, 0.0, 0.0]
            },
            theme.border,
        )
        .child(
            el().w(32.0)
                .h(32.0)
                .radius(16.0)
                .border(1.0, theme.border)
                .fill(theme.ink(0.03))
                .center()
                .child(
                    text(initial)
                        .font(TextStyle::new(12.0, 600.0))
                        .color(theme.text_muted),
                ),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(widgets::row_title(theme, email))
                .child(usage),
        )
        .child(right)
}

fn provider_section(
    page: &AccountsPage,
    harness: HarnessId,
    name: &str,
    cli: &str,
    snapshot: &AgentAccountsSnapshot,
    theme: &Theme,
    now: DateTime<Utc>,
) -> El {
    let accounts = provider_accounts(snapshot, harness);
    let warnings: Vec<String> = snapshot
        .warnings
        .iter()
        .filter(|w| w.harness == harness)
        .map(|w| w.message.clone())
        .collect();
    let mut card = widgets::section_card(theme).mt(8.0);
    if accounts.is_empty() {
        card = card.child(
            col().w_full().py(32.0).items_center().child(
                text(format!(
                    "No {name} login detected on this device — sign in with \u{201C}{cli}\u{201D} or add an account."
                ))
                .font(TextStyle::new(14.0, 400.0))
                .color(theme.text_muted.alpha(0.6))
                .wrap(),
            ),
        );
    } else {
        for (ix, account) in accounts.iter().enumerate() {
            card = card.child(account_row(page, account, ix, ix == 0, theme, now));
        }
    }

    let add = widgets::ghost_action(theme)
        .hover(theme.ink(0.06))
        .on_press(move |app: &mut App| start_login(app, harness))
        .child(icons::icon(&icons::ADD_CIRCLE, 16.0).color(theme.text_muted))
        .child(
            text("Add account")
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.text_muted),
        );

    let mut section = col().mt(24.0).child(
        row()
            .gap(8.0)
            .items_center()
            .child(provider_mark(harness, theme))
            .child(
                text(name.to_string())
                    .font(TextStyle::new(14.0, 500.0))
                    .color(theme.text),
            )
            .child(el().grow(1.0))
            .child(add),
    );
    for warning in warnings {
        section = section.child(widgets::warning_strip(theme, warning));
    }
    section.child(card)
}

fn skeleton_section(harness: HarnessId, name: &str, theme: &Theme) -> El {
    col()
        .mt(24.0)
        .child(
            row()
                .gap(8.0)
                .items_center()
                .child(provider_mark(harness, theme))
                .child(
                    text(name.to_string())
                        .font(TextStyle::new(14.0, 500.0))
                        .color(theme.text),
                ),
        )
        .child(
            col()
                .mt(8.0)
                .radius(12.0)
                .border(1.0, theme.border)
                .fill(theme.card_glass_bg())
                .clip_rounded(12.0)
                .child(widgets::skeleton_row(theme, true, false))
                .child(widgets::skeleton_row(theme, false, true)),
        )
}

fn url_link(theme: &Theme, label: &'static str, url: String) -> El {
    text(label)
        .mt(6.0)
        .truncate()
        .font(TextStyle::new(12.0, 400.0))
        .color(theme.text_muted.alpha(0.6))
        .cursor(rune::Cursor::Pointer)
        .on_press_tx(move |_app: &mut App, tx: &mut Tx| {
            let _ = tx.open_url(&url);
        })
}

fn login_dialog(page: &AccountsPage, theme: &Theme) -> Option<rune_kit::Dialog> {
    let login = page.login.as_ref()?;
    let kit = crate::shell::shell_kit(theme);
    let red_text = theme.danger_muted.alpha(0.9);
    let body: El = match login {
        LoginFlow::Starting { .. } => col()
            .w_full()
            .mt(8.0)
            .child(widgets::skeleton_row(theme, true, false))
            .child(widgets::skeleton_row(theme, false, false)),
        LoginFlow::PasteCode {
            start,
            submitting,
            error,
            ..
        } => {
            let submitting = *submitting;
            let input = rune_kit::TextInput::new(&page.code)
                .placeholder("Paste the authorization code")
                .label("Paste the authorization code")
                .text_style(TextStyle::new(13.0, 400.0).mono())
                .on_edit(|app: &mut App| &mut app.settings_ui.accounts.code)
                .on_submit(|app: &mut App, _tx: &mut Tx| submit_code(app))
                .build(&kit)
                .autofocus();
            let mut body = col()
                .w_full()
                .child(
                    text(
                        "A browser window opened. Sign in to the account you want to add, \
                         approve access, then paste the code shown below. Your current login is \
                         untouched until you switch.",
                    )
                    .font(TextStyle::new(12.5, 400.0))
                    .color(theme.text_muted.alpha(0.7))
                    .wrap()
                    .mt(8.0),
                )
                .child(url_link(
                    theme,
                    "Reopen the authorization page",
                    start.url.clone(),
                ))
                .child(col().mt(12.0).child(input));
            if let Some(message) = error.clone() {
                body = body.child(
                    text(message)
                        .font(TextStyle::new(12.0, 400.0))
                        .color(red_text)
                        .wrap()
                        .mt(8.0),
                );
            }
            body.child(
                row()
                    .mt(16.0)
                    .w_full()
                    .justify_end()
                    .gap(8.0)
                    .child(
                        rune_kit::Button::new("Cancel")
                            .variant(rune_kit::ButtonVariant::Ghost)
                            .on_press(|app: &mut App| cancel_login(app))
                            .build(&kit),
                    )
                    .child(
                        rune_kit::Button::new(if submitting {
                            "Verifying…"
                        } else {
                            "Add account"
                        })
                        .variant(rune_kit::ButtonVariant::Contrast)
                        .on_press(|app: &mut App| submit_code(app))
                        .build(&kit)
                        .opacity(if submitting { 0.5 } else { 1.0 }),
                    ),
            )
        }
        LoginFlow::Browser {
            start,
            message,
            error,
            ..
        } => {
            let has_error = error.is_some();
            let mut body = col()
                .w_full()
                .child(
                    text(
                        "Finish signing in to OpenAI in your browser. The new login is captured \
                         in an isolated profile — your current session is untouched until you \
                         switch.",
                    )
                    .font(TextStyle::new(12.5, 400.0))
                    .color(theme.text_muted.alpha(0.7))
                    .wrap()
                    .mt(8.0),
                )
                .child(url_link(
                    theme,
                    "Reopen the sign-in page",
                    start.url.clone(),
                ));
            if !has_error {
                body = body.child(
                    row()
                        .mt(16.0)
                        .gap(8.0)
                        .items_center()
                        .child(crate::loaders::gradient_spinner(theme, 3.0))
                        .child(
                            text(
                                message
                                    .clone()
                                    .unwrap_or_else(|| "Waiting for the browser…".to_string()),
                            )
                            .font(TextStyle::new(12.5, 400.0))
                            .color(theme.text_muted.alpha(0.7)),
                        ),
                );
            }
            if let Some(message) = error.clone() {
                body = body.child(
                    text(message)
                        .font(TextStyle::new(12.0, 400.0))
                        .color(red_text)
                        .wrap()
                        .mt(12.0),
                );
            }
            body.child(
                row().mt(16.0).w_full().justify_end().child(
                    rune_kit::Button::new(if has_error { "Close" } else { "Cancel" })
                        .variant(rune_kit::ButtonVariant::Ghost)
                        .on_press(|app: &mut App| cancel_login(app))
                        .build(&kit),
                ),
            )
        }
    };
    Some(
        rune_kit::Dialog::new("settings/add-account-dialog", login.title())
            .open(true)
            .width(440.0)
            .fit()
            .on_close(|app: &mut App| cancel_login(app))
            .child(body),
    )
}

/// The Accounts page body.
pub fn render(page: &AccountsPage, app: &App, theme: &Theme) -> El {
    page.ensure_loaded(app);
    let now = Utc::now();
    let refreshing = matches!(page.snapshot, Loadable::Loading);
    let account_count = page
        .snapshot
        .ready()
        .map(|s| s.accounts.len())
        .filter(|&n| n > 0);

    let mut layer: Vec<El> = Vec::new();
    let switcher = device_switcher(page, app, theme, &mut layer);

    let mut sections: Vec<El> = Vec::new();
    match &page.snapshot {
        Loadable::Idle | Loadable::Loading => {
            for (harness, name, _cli) in PROVIDERS {
                sections.push(skeleton_section(harness, name, theme));
            }
        }
        Loadable::Error(message) => {
            sections.push(
                col()
                    .w_full()
                    .child(
                        widgets::error_strip(theme, message.clone())
                            .cursor(rune::Cursor::Pointer)
                            .on_press(|app: &mut App| reload(app, LoadTrigger::Retry)),
                    )
                    .child(
                        text("Click to retry")
                            .mt(4.0)
                            .font(TextStyle::new(11.5, 400.0))
                            .color(theme.text_muted),
                    ),
            );
        }
        Loadable::Ready(snapshot) => {
            for (harness, name, cli) in PROVIDERS {
                sections.push(provider_section(
                    page, harness, name, cli, snapshot, theme, now,
                ));
            }
        }
    }

    let refresh = widgets::ghost_action(theme)
        .opacity(if refreshing { 0.5 } else { 1.0 })
        .hover(theme.ink(0.06))
        .on_press(|app: &mut App| reload(app, LoadTrigger::Refresh))
        .child(icons::icon(&icons::REFRESH, 16.0).color(theme.text_muted))
        .child(
            text("Refresh")
                .font(TextStyle::new(12.5, 400.0))
                .color(theme.text_muted),
        );

    let mut content = widgets::page_column()
        .child(
            row()
                .gap(10.0)
                .items_center()
                .child(widgets::page_header(theme, "Accounts", account_count))
                .child(el().grow(1.0))
                .child(refresh)
                .child(switcher),
        )
        .child(widgets::page_subtitle(
            theme,
            "The Codex logins on this device. Hearth detects the live session, keeps each \
             account backed up, and can swap between them.",
        ));
    if let Some(message) = page.error.clone() {
        content = content.child(
            widgets::error_strip(theme, message)
                .cursor(rune::Cursor::Pointer)
                .on_press(|app: &mut App| app.settings_ui.accounts.error = None),
        );
    }
    content = content.children(sections);
    content = content.child(
        text(
            "Switching rewrites the CLI\u{2019}s stored login, so new agent sessions use the \
             selected account immediately. On macOS, an already-running Claude Code can hold \
             the previous login for up to ~30 seconds (Keychain cache).",
        )
        .mt(24.0)
        .font(TextStyle::new(12.0, 400.0).lh(19.0))
        .color(theme.text_muted.alpha(0.6))
        .wrap(),
    );

    if let Some(dialog) = login_dialog(page, theme) {
        dialog.build(&crate::shell::shell_kit(theme), &mut layer);
    }

    rune_kit::root(widgets::page_root("accounts-page").child(content), layer)
}

/// No page-level key handling.
pub fn on_key(_app: &mut App, _ev: &rune::KeyEvent, _tx: &mut Tx) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn login_dialog_titles_name_the_provider() {
        assert_eq!(
            LoginFlow::Starting {
                harness: HarnessId::Codex
            }
            .title(),
            "Add Codex account"
        );
        assert_eq!(
            LoginFlow::Starting {
                harness: HarnessId::Huginn
            }
            .title(),
            "Add account"
        );
    }

    #[test]
    fn first_load_of_a_visit_forces_the_usage_probe() {
        assert!(force_usage_for(LoadTrigger::Mount));
        assert!(force_usage_for(LoadTrigger::Retry));
        assert!(force_usage_for(LoadTrigger::Refresh));
        assert!(force_usage_for(LoadTrigger::PostLogin));
        assert!(!force_usage_for(LoadTrigger::PostAction));
    }

    #[test]
    fn usage_thresholds_match_hearth() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
    }

    #[test]
    fn usage_colors_map_to_theme_accents() {
        let theme = Theme::dark();
        assert_eq!(usage_color(UsageLevel::Normal, &theme), theme.accent);
        assert_eq!(usage_color(UsageLevel::Warn, &theme), theme.warning);
        assert_eq!(usage_color(UsageLevel::Critical, &theme), theme.danger);
    }

    #[test]
    fn reset_formatting_is_absolute() {
        use chrono::Local;
        let now = Utc::now();
        assert_eq!(format_reset(None, now), None);
        let soon = now + TimeDelta::minutes(125);
        assert_eq!(
            format_reset(Some(soon), now),
            Some(format!(
                "resets {}",
                soon.with_timezone(&Local).format("%-I:%M %p")
            ))
        );
        let later = now + TimeDelta::days(3);
        assert_eq!(
            format_reset(Some(later), now),
            Some(format!(
                "resets {}",
                later.with_timezone(&Local).format("%a")
            ))
        );
        let monthly = now + TimeDelta::days(26);
        assert_eq!(
            format_reset(Some(monthly), now),
            Some(format!(
                "resets {}",
                monthly.with_timezone(&Local).format("%b %-d")
            ))
        );
    }

    #[test]
    fn provider_grouping_keeps_engine_order_even_when_active_is_later() {
        let account = |id: &str, harness: HarnessId, active: bool| AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("x1", HarnessId::Codex, false),
                account("x2", HarnessId::Codex, true),
            ],
            warnings: vec![],
        };
        let ids: Vec<&str> = provider_accounts(&snapshot, HarnessId::Codex)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["x1", "x2"],
            "engine (creation) order holds — switching must not move a card"
        );
        assert!(provider_accounts(&snapshot, HarnessId::Huginn).is_empty());
    }

    #[test]
    fn target_device_is_merged_only_when_present() {
        let direct = with_target(serde_json::json!({ "forceUsage": true }), None);
        assert!(direct.get("targetDeviceId").is_none());
        let relayed = with_target(serde_json::json!({ "forceUsage": true }), Some("dev-2"));
        assert_eq!(
            relayed.get("targetDeviceId").and_then(|v| v.as_str()),
            Some("dev-2")
        );
    }
}
