//! In-memory checkout change-request state for desktop views.
//!
//! The workspace document remains the source of truth for chats and projects;
//! pull-request metadata is host-local, short-lived capability state and is
//! deliberately never written back into a synced document.
//!
//! Rune port notes
//! ---------------
//! The gpui version held `ChangeRequestClientState` on `AppState` and one
//! `Task` per watched checkout. Here [`ChangeRequestsState`] owns the same
//! client cache plus its own `mpsc` channel: watch pumps post
//! [`ChangeRequestsEvent`]s and [`ChangeRequestsState::poll`] applies them on
//! the UI thread. `changes::poll` drives both [`ChangeRequestsState::refresh`]
//! and [`ChangeRequestsState::poll`] each frame — badges stay live even when
//! the diff pane is closed.
//!
//! The badge *model* is framework-free; the sidebar and composer render it.
//! [`badge_for_chat`] is the one helper they need.

// This module is declared from `changes/mod.rs` with a `#[path]` shim so the
// file lives at the crate root (`src/change_requests.rs`) as in the gpui
// crate without an edit to the lead-owned `lib.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};

use hearth_proto::{
    ChangeRequestSummary, Chat, CheckoutChangeRequestStatus, Device, Space,
};
use hearth_rpc::{RpcError, methods};
use rune::Color;

use crate::bridge::{Bridge, WATCH_RETRY};
use crate::engine::EngineHandle;
use crate::state::App;
use crate::theme::Theme;

/// The state colour a change-request badge paints with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeRequestBadgeTone {
    Open,
    Merged,
    Closed,
}

impl ChangeRequestBadgeTone {
    pub fn color(self, theme: &Theme) -> Color {
        match self {
            Self::Open => theme.success,
            Self::Merged => theme.code_text,
            Self::Closed => theme.danger,
        }
    }
}

/// Framework-free badge content: the sidebar/composer decide how to paint it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequestBadgeModel {
    pub number: String,
    pub state_label: &'static str,
    pub title: String,
    pub tone: ChangeRequestBadgeTone,
}

impl ChangeRequestBadgeModel {
    pub fn from_summary(summary: &ChangeRequestSummary) -> Self {
        use hearth_proto::ChangeRequestState;

        let (state_label, tone) = match summary.state {
            ChangeRequestState::Open => ("Open", ChangeRequestBadgeTone::Open),
            ChangeRequestState::Merged => ("Merged", ChangeRequestBadgeTone::Merged),
            ChangeRequestState::Closed => ("Closed", ChangeRequestBadgeTone::Closed),
        };
        Self {
            number: format!("#{}", summary.number),
            state_label,
            title: summary.title.replace(['\r', '\n'], " "),
            tone,
        }
    }
}

/// Which surface mounts the badge; the composer's is taller and carries an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeRequestBadgeSurface {
    Sidebar,
    Composer,
}

/// One host-side checkout watch. Multiple chats can share this target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeRequestWatchKey {
    pub device_id: String,
    pub cwd: String,
}

/// The cached snapshots and the per-device negative capability cache.
#[derive(Debug, Default)]
pub struct ChangeRequestClientState {
    snapshots: HashMap<ChangeRequestWatchKey, CheckoutChangeRequestStatus>,
    /// The engine version that rejected this versioned capability. A device
    /// update publishes its running version, so a host upgrade invalidates this
    /// negative cache without polling older hosts.
    unsupported_devices: HashMap<String, Option<String>>,
}

impl ChangeRequestClientState {
    pub fn is_supported(&self, device_id: &str) -> bool {
        !self.unsupported_devices.contains_key(device_id)
    }

    pub fn mark_unsupported(&mut self, device_id: String, engine_version: Option<String>) {
        self.unsupported_devices
            .insert(device_id.clone(), engine_version);
        self.snapshots.retain(|key, _| key.device_id != device_id);
    }

    /// Forget a version-skew rejection after the host advertises a different
    /// engine version. `UnknownMethod` describes one running engine, not the
    /// device permanently.
    pub fn clear_unsupported_on_version_change(
        &mut self,
        device_id: &str,
        engine_version: Option<&str>,
    ) -> bool {
        let Some(unsupported_version) = self.unsupported_devices.get(device_id) else {
            return false;
        };
        if unsupported_version.as_deref() == engine_version {
            return false;
        }
        self.unsupported_devices.remove(device_id);
        true
    }

    /// Store a frame. Returns whether the visible snapshot changed.
    pub fn store(
        &mut self,
        key: ChangeRequestWatchKey,
        snapshot: CheckoutChangeRequestStatus,
    ) -> bool {
        // A new frame replaces the old branch/checkout context for this path.
        // Rendering validates the complete identity again before exposing it.
        let changed = self.snapshots.get(&key) != Some(&snapshot);
        self.snapshots.insert(key, snapshot);
        changed
    }

    /// Drop snapshots whose checkout no longer has an active chat.
    pub fn retain_targets(&mut self, targets: &HashSet<ChangeRequestWatchKey>) {
        self.snapshots.retain(|key, _| targets.contains(key));
    }

    /// Forget every device rejection not backed by a live device row (the
    /// negative cache must not outlive the device list it describes).
    pub fn retain_unsupported(&mut self, devices: &HashSet<String>) -> bool {
        let before = self.unsupported_devices.len();
        self.unsupported_devices
            .retain(|device_id, _| devices.contains(device_id));
        self.unsupported_devices.len() != before
    }

    pub fn snapshot_count(&self) -> usize {
        self.snapshots.len()
    }

    pub fn change_request_for_chat<'a>(
        &'a self,
        chat: &Chat,
        spaces: &[Space],
    ) -> Option<&'a ChangeRequestSummary> {
        change_request_for_chat(chat, spaces, self.snapshots.values())
    }
}

/// Active, fully identified checkouts that need host-side PR resolution.
pub fn desired_watch_targets(
    chats: &[Chat],
    spaces: &[Space],
    is_unsupported: impl Fn(&str) -> bool,
) -> HashSet<ChangeRequestWatchKey> {
    chats
        .iter()
        .filter(|chat| !chat.archived)
        .filter(|chat| {
            chat.branch
                .as_deref()
                .is_some_and(|branch| !branch.trim().is_empty())
        })
        .filter(|chat| !is_unsupported(&chat.device_id))
        .filter_map(|chat| {
            let cwd = effective_chat_cwd(chat, spaces)?;
            Some(ChangeRequestWatchKey {
                device_id: chat.device_id.clone(),
                cwd: cwd.to_owned(),
            })
        })
        .collect()
}

pub fn watch_params(
    target: &ChangeRequestWatchKey,
    local_device_id: Option<&str>,
) -> serde_json::Value {
    let mut params = serde_json::json!({ "cwd": target.cwd });
    if local_device_id != Some(target.device_id.as_str())
        && let Some(object) = params.as_object_mut()
    {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.device_id.clone()),
        );
    }
    params
}

/// Resolve a snapshot for a row without trusting the cache key alone.
///
/// Canonical checkout identity wins when the chat has one. Older rows without
/// it fall back to device + effective cwd. Branch equality is mandatory in
/// both cases so a branch switch hides the stale PR immediately.
pub fn change_request_for_chat<'a>(
    chat: &Chat,
    spaces: &[Space],
    snapshots: impl IntoIterator<Item = &'a CheckoutChangeRequestStatus>,
) -> Option<&'a ChangeRequestSummary> {
    let branch = chat.branch.as_deref()?.trim();
    if branch.is_empty() {
        return None;
    }
    let cwd = effective_chat_cwd(chat, spaces)?;

    snapshots
        .into_iter()
        .find(|snapshot| {
            snapshot.device_id == chat.device_id
                && snapshot.cwd == cwd
                && snapshot.branch == branch
                && chat.checkout_id.as_deref().is_none_or(|checkout_id| {
                    !snapshot.checkout_id.is_empty() && snapshot.checkout_id == checkout_id
                })
        })
        .and_then(|snapshot| snapshot.change_request.as_ref())
}

fn effective_chat_cwd<'a>(chat: &'a Chat, spaces: &'a [Space]) -> Option<&'a str> {
    chat.cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .or_else(|| {
            let space_id = chat.space_id.as_deref()?;
            spaces
                .iter()
                .find(|space| space.id == space_id && space.device_id == chat.device_id)
                .map(|space| space.path.as_str())
                .filter(|path| !path.trim().is_empty())
        })
}

// ---------------------------------------------------------------------------
// Live state: watch pumps + one frame-loop poll
// ---------------------------------------------------------------------------

/// Everything one reconcile needs from the root state. Cloned so background
/// watch pumps never hold `&App`.
#[derive(Clone)]
pub struct ChangeRequestsEnv {
    pub bridge: Bridge,
    pub engine: Option<EngineHandle>,
    pub local_device_id: Option<String>,
    pub chats: Vec<Chat>,
    pub spaces: Vec<Space>,
    pub devices: Vec<Device>,
}

impl ChangeRequestsEnv {
    pub fn capture(app: &App) -> Self {
        Self {
            bridge: app.bridge.clone(),
            engine: app.engine.clone(),
            local_device_id: app.local_device_id.clone(),
            chats: app.chats.clone(),
            spaces: app.spaces.clone(),
            devices: app.devices.clone(),
        }
    }
}

enum ChangeRequestsEvent {
    Snapshot {
        target: ChangeRequestWatchKey,
        snapshot: Box<CheckoutChangeRequestStatus>,
    },
    Unsupported {
        device_id: String,
        engine_version: Option<String>,
    },
}

/// The change-request watches and their cached snapshots.
///
/// Rendered through [`badge_for_chat`]; `changes::poll` calls [`Self::refresh`]
/// and [`Self::poll`] once per frame, independent of the diff pane's visibility.
#[derive(Default)]
pub struct ChangeRequestsState {
    rx: Option<Receiver<ChangeRequestsEvent>>,
    tx: Option<Sender<ChangeRequestsEvent>>,
    client: ChangeRequestClientState,
    tasks: HashMap<ChangeRequestWatchKey, tokio::task::JoinHandle<()>>,
}

impl ChangeRequestsState {
    fn channel(&mut self) -> Sender<ChangeRequestsEvent> {
        if self.tx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        // `tx` is set right above; the channel is never replaced afterwards.
        self.tx
            .as_ref()
            .expect("change request channel initialised")
            .clone()
    }

    pub fn client(&self) -> &ChangeRequestClientState {
        &self.client
    }

    pub fn is_supported(&self, device_id: &str) -> bool {
        self.client.is_supported(device_id)
    }

    pub fn snapshot_count(&self) -> usize {
        self.client.snapshot_count()
    }

    pub fn change_request_for_chat<'a>(
        &'a self,
        chat: &Chat,
        spaces: &[Space],
    ) -> Option<&'a ChangeRequestSummary> {
        self.client.change_request_for_chat(chat, spaces)
    }

    /// Reconcile watch targets with the current chats/spaces/devices and start
    /// any missing `WatchCheckoutChangeRequest` pump. Returns whether badge
    /// visibility changed (a device upgrade or a pruned snapshot).
    pub fn refresh(&mut self, env: &ChangeRequestsEnv) -> bool {
        let mut changed = false;
        // Prune rejections for devices no chat references any more. A device
        // row that is missing from an early frame must NOT clear its cache:
        // that would spawn a watch, fail `UnknownMethod`, and repeat each frame.
        let chat_devices: HashSet<String> = env.chats.iter().map(|c| c.device_id.clone()).collect();
        changed |= self.client.retain_unsupported(&chat_devices);
        for device in &env.devices {
            changed |=
                self.client
                    .clear_unsupported_on_version_change(&device.id, device.version.as_deref());
        }

        let Some(engine) = env.engine.clone() else {
            self.stop_all();
            return changed;
        };

        let targets = desired_watch_targets(&env.chats, &env.spaces, |device| {
            !self.client.is_supported(device)
        });

        self.tasks.retain(|target, task| {
            let keep = targets.contains(target);
            if !keep {
                task.abort();
            }
            keep
        });

        let before = self.client.snapshot_count();
        self.client.retain_targets(&targets);
        if self.client.snapshot_count() != before {
            changed = true;
        }

        let tx = self.channel();
        for target in targets {
            if self.tasks.contains_key(&target) {
                continue;
            }
            let engine_version = env
                .devices
                .iter()
                .find(|device| device.id == target.device_id)
                .and_then(|device| device.version.clone());
            let task = spawn_change_request_watch(
                env,
                engine.clone(),
                target.clone(),
                engine_version,
                tx.clone(),
            );
            self.tasks.insert(target, task);
        }
        changed
    }

    /// Apply frames drained from the watch pumps.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = self.rx.take() else {
            return false;
        };
        let mut changed = false;
        while let Ok(event) = rx.try_recv() {
            changed |= self.apply(event);
        }
        self.rx = Some(rx);
        changed
    }

    fn apply(&mut self, event: ChangeRequestsEvent) -> bool {
        match event {
            ChangeRequestsEvent::Snapshot { target, snapshot } => {
                self.client.store(target, *snapshot)
            }
            ChangeRequestsEvent::Unsupported {
                device_id,
                engine_version,
            } => {
                self.client.mark_unsupported(device_id, engine_version);
                true
            }
        }
    }

    fn stop_all(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

fn spawn_change_request_watch(
    env: &ChangeRequestsEnv,
    engine: EngineHandle,
    target: ChangeRequestWatchKey,
    engine_version: Option<String>,
    tx: Sender<ChangeRequestsEvent>,
) -> tokio::task::JoinHandle<()> {
    let bridge = env.bridge.clone();
    let local_device_id = env.local_device_id.clone();
    env.bridge.runtime().spawn(async move {
        loop {
            let params = watch_params(&target, local_device_id.as_deref());
            match engine
                .client
                .subscribe_checked(methods::WATCH_CHECKOUT_CHANGE_REQUEST, params)
                .await
            {
                Ok(mut subscription) => {
                    while let Some(value) = subscription.recv().await {
                        let snapshot: CheckoutChangeRequestStatus =
                            match serde_json::from_value(value) {
                                Ok(snapshot) => snapshot,
                                Err(err) => {
                                    tracing::warn!(
                                        device = %target.device_id,
                                        cwd = %target.cwd,
                                        error = %err,
                                        "dropping malformed checkout change request frame"
                                    );
                                    continue;
                                }
                            };
                        let _ = tx.send(ChangeRequestsEvent::Snapshot {
                            target: target.clone(),
                            snapshot: Box::new(snapshot),
                        });
                        bridge.wake();
                    }
                    tracing::debug!(
                        device = %target.device_id,
                        cwd = %target.cwd,
                        "checkout change request stream ended; resubscribing"
                    );
                }
                Err(RpcError::UnknownMethod(_)) => {
                    tracing::debug!(
                        device = %target.device_id,
                        "checkout change requests unsupported on device"
                    );
                    let _ = tx.send(ChangeRequestsEvent::Unsupported {
                        device_id: target.device_id.clone(),
                        engine_version,
                    });
                    bridge.wake();
                    return;
                }
                Err(err) => {
                    tracing::debug!(
                        device = %target.device_id,
                        cwd = %target.cwd,
                        error = %err,
                        "checkout change request watch unavailable; retrying"
                    );
                }
            }
            tokio::time::sleep(WATCH_RETRY).await;
        }
    })
}

/// The badge the sidebar/composer shows for `chat_id`, if a matching snapshot
/// exists. Wired by the shell as:
///
/// ```ignore
/// if let Some(badge) = crate::changes::badge_for_chat(app, &chat.id) {
///     // paint badge.number / badge.state_label / badge.title with badge.tone
/// }
/// ```
pub fn badge_for_chat(app: &App, chat_id: &str) -> Option<ChangeRequestBadgeModel> {
    let chat = app.chats.iter().find(|chat| chat.id == chat_id)?;
    badge_for_chat_row(app, chat)
}

/// [`badge_for_chat`] when the caller already holds the chat row.
pub fn badge_for_chat_row(app: &App, chat: &Chat) -> Option<ChangeRequestBadgeModel> {
    app.changes
        .change_requests
        .change_request_for_chat(chat, &app.spaces)
        .map(ChangeRequestBadgeModel::from_summary)
}

/// Paint the badge for a chat row (sidebar) or the composer chip. `None` when
/// the chat has no snapshot; the shell keeps its normal layout then.
///
/// Public API:
/// `changes::badge_el(app, &chat.id, ChangeRequestBadgeSurface::Sidebar, theme)`.
pub fn badge_el(
    app: &App,
    chat_id: &str,
    surface: ChangeRequestBadgeSurface,
    theme: &Theme,
) -> Option<rune::El> {
    let chat = app.chats.iter().find(|chat| chat.id == chat_id)?;
    let summary = app
        .changes
        .change_requests
        .change_request_for_chat(chat, &app.spaces)?;
    let model = ChangeRequestBadgeModel::from_summary(summary);
    Some(pull_request_badge(
        chat_id,
        &model,
        &summary.url,
        surface,
        theme,
    ))
}

fn pull_request_badge(
    chat_id: &str,
    model: &ChangeRequestBadgeModel,
    url: &str,
    surface: ChangeRequestBadgeSurface,
    theme: &Theme,
) -> rune::El {
    use rune::{row, text, Cursor, TextStyle};

    let color = model.tone.color(theme);
    let composer = surface == ChangeRequestBadgeSurface::Composer;
    let open_url = url.to_string();
    let mut badge = row()
        .key(format!(
            "app/pr-badge/{chat_id}/{}",
            if composer { "composer" } else { "sidebar" }
        ))
        .h(if composer { 20.0 } else { 16.0 })
        .shrink()
        .center_y()
        .gap(if composer { 5.0 } else { 0.0 })
        .px(if composer { 7.0 } else { 4.0 })
        .radius(if composer { 6.0 } else { 4.0 })
        .fill(color.alpha(0.08))
        .hover(color.alpha(0.16))
        .cursor(Cursor::Pointer)
        .role("button")
        .label(format!(
            "PR {} · {} · {}",
            model.number, model.state_label, model.title
        ))
        .on_press_tx(move |_app: &mut App, tx: &mut rune::Tx| {
            let _ = tx.open_url(&open_url);
        });
    if composer {
        badge = badge.child(
            crate::icons::icon(&crate::icons::PULL_REQUEST, 11.0)
                .color(color.alpha(0.85)),
        );
    }
    badge
        // Monospace digits give the badge a stable tabular width as PR numbers change.
        .child(
            text(model.number.clone())
                .font(TextStyle::new(if composer { 11.0 } else { 10.0 }, 500.0).mono())
                .color(color.alpha(0.85)),
        )
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use hearth_proto::ChangeRequestState;

    use super::*;

    fn chat(id: &str, device: &str, cwd: Option<&str>, checkout: Option<&str>) -> Chat {
        Chat {
            id: id.into(),
            device_id: device.into(),
            title: None,
            archived: false,
            cwd: cwd.map(str::to_owned),
            branch: Some("feature/pr".into()),
            checkout_id: checkout.map(str::to_owned),
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some("space".into()),
            last_seen_at: None,
            room_gen: None,
        }
    }

    fn space(device: &str) -> Space {
        Space {
            id: "space".into(),
            device_id: device.into(),
            path: "/project".into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: Some("checkout".into()),
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
        }
    }

    fn snapshot(device: &str, cwd: &str, checkout: &str) -> CheckoutChangeRequestStatus {
        CheckoutChangeRequestStatus {
            checkout_id: checkout.into(),
            device_id: device.into(),
            cwd: cwd.into(),
            branch: "feature/pr".into(),
            change_request: Some(ChangeRequestSummary {
                provider: "github".into(),
                number: 90,
                title: "Add pull request badges".into(),
                url: "https://github.com/acme/hearth/pull/90".into(),
                state: ChangeRequestState::Open,
                base_ref: "main".into(),
                head_ref: "feature/pr".into(),
            }),
            updated_at: Utc.timestamp_opt(1, 0).unwrap(),
        }
    }

    #[test]
    fn local_chat_finds_local_snapshot() {
        let chat = chat("chat", "local", Some("/repo"), Some("checkout"));
        let status = snapshot("local", "/repo", "checkout");
        assert_eq!(
            change_request_for_chat(&chat, &[], [&status]).map(|pr| pr.number),
            Some(90)
        );
    }

    #[test]
    fn remote_chat_requires_the_same_device() {
        let chat = chat("chat", "remote", Some("/repo"), Some("checkout"));
        let status = snapshot("local", "/repo", "checkout");
        assert!(change_request_for_chat(&chat, &[], [&status]).is_none());
    }

    #[test]
    fn mismatched_checkout_rejects_cwd_match() {
        let chat = chat("chat", "local", Some("/repo"), Some("new-checkout"));
        let status = snapshot("local", "/repo", "old-checkout");
        assert!(change_request_for_chat(&chat, &[], [&status]).is_none());
    }

    #[test]
    fn legacy_row_falls_back_to_cwd() {
        let chat = chat("chat", "local", Some("/repo"), None);
        let status = snapshot("local", "/repo", "checkout");
        assert_eq!(
            change_request_for_chat(&chat, &[], [&status]).map(|pr| pr.number),
            Some(90)
        );
    }

    #[test]
    fn project_root_is_used_only_when_chat_has_no_cwd() {
        let chat = chat("chat", "local", None, None);
        let status = snapshot("local", "/project", "checkout");
        assert_eq!(
            change_request_for_chat(&chat, &[space("local")], [&status]).map(|pr| pr.number),
            Some(90)
        );
    }

    #[test]
    fn different_branch_hides_snapshot() {
        let mut chat = chat("chat", "local", Some("/repo"), Some("checkout"));
        chat.branch = Some("feature/other".into());
        let status = snapshot("local", "/repo", "checkout");
        assert!(change_request_for_chat(&chat, &[], [&status]).is_none());
    }

    #[test]
    fn shared_chats_deduplicate_and_last_archive_removes_target() {
        let first = chat("one", "local", Some("/repo"), Some("checkout"));
        let mut second = chat("two", "local", Some("/repo"), Some("checkout"));
        let targets = desired_watch_targets(&[first.clone(), second.clone()], &[], |_| false);
        assert_eq!(targets.len(), 1);

        second.archived = true;
        let targets = desired_watch_targets(&[first.clone(), second.clone()], &[], |_| false);
        assert_eq!(targets.len(), 1);

        let mut first = first;
        first.archived = true;
        assert!(desired_watch_targets(&[first, second], &[], |_| false).is_empty());
    }

    #[test]
    fn unsupported_device_has_no_targets_or_visible_snapshot() {
        let chat = chat("chat", "old-engine", Some("/repo"), Some("checkout"));
        let mut state = ChangeRequestClientState::default();
        state.store(
            ChangeRequestWatchKey {
                device_id: "old-engine".into(),
                cwd: "/repo".into(),
            },
            snapshot("old-engine", "/repo", "checkout"),
        );
        state.mark_unsupported("old-engine".into(), Some("0.2.2".into()));

        assert!(state.change_request_for_chat(&chat, &[]).is_none());
        assert!(
            desired_watch_targets(&[chat], &[], |device| !state.is_supported(device)).is_empty()
        );
    }

    #[test]
    fn host_upgrade_reenables_a_previously_unsupported_device() {
        let chat = chat("chat", "host", Some("/repo"), Some("checkout"));
        let mut state = ChangeRequestClientState::default();
        state.mark_unsupported("host".into(), Some("0.2.2".into()));

        assert!(!state.is_supported("host"));
        assert!(!state.clear_unsupported_on_version_change("host", Some("0.2.2")));
        assert!(state.clear_unsupported_on_version_change("host", Some("0.2.3")));
        assert!(state.is_supported("host"));
        assert_eq!(
            desired_watch_targets(&[chat], &[], |device| !state.is_supported(device)).len(),
            1
        );
    }

    #[test]
    fn successful_none_clears_a_previous_pr() {
        let chat = chat("chat", "local", Some("/repo"), Some("checkout"));
        let key = ChangeRequestWatchKey {
            device_id: "local".into(),
            cwd: "/repo".into(),
        };
        let mut state = ChangeRequestClientState::default();
        state.store(key.clone(), snapshot("local", "/repo", "checkout"));
        assert!(state.change_request_for_chat(&chat, &[]).is_some());

        let mut none = snapshot("local", "/repo", "checkout");
        none.change_request = None;
        state.store(key, none);
        assert!(state.change_request_for_chat(&chat, &[]).is_none());
    }

    #[test]
    fn local_and_remote_watch_params_route_to_the_checkout_host() {
        let target = ChangeRequestWatchKey {
            device_id: "host".into(),
            cwd: "/repo".into(),
        };
        assert_eq!(
            watch_params(&target, Some("host")),
            serde_json::json!({
                "cwd": "/repo"
            })
        );
        assert_eq!(
            watch_params(&target, Some("viewport")),
            serde_json::json!({
                "cwd": "/repo",
                "targetDeviceId": "host"
            })
        );
    }

    #[test]
    fn badge_models_cover_open_merged_and_closed() {
        let cases = [
            (
                ChangeRequestState::Open,
                "Open",
                ChangeRequestBadgeTone::Open,
            ),
            (
                ChangeRequestState::Merged,
                "Merged",
                ChangeRequestBadgeTone::Merged,
            ),
            (
                ChangeRequestState::Closed,
                "Closed",
                ChangeRequestBadgeTone::Closed,
            ),
        ];
        for (state, label, tone) in cases {
            let mut summary = snapshot("local", "/repo", "checkout")
                .change_request
                .unwrap();
            summary.state = state;
            summary.title = "First line\nSecond line".into();
            let model = ChangeRequestBadgeModel::from_summary(&summary);
            assert_eq!(model.number, "#90");
            assert_eq!(model.state_label, label);
            assert_eq!(model.tone, tone);
            assert_eq!(model.title, "First line Second line");
        }
    }

    #[test]
    fn retain_prunes_stale_targets_and_unsupported_rows() {
        let mut state = ChangeRequestClientState::default();
        let key = ChangeRequestWatchKey {
            device_id: "local".into(),
            cwd: "/repo".into(),
        };
        state.store(key.clone(), snapshot("local", "/repo", "checkout"));
        state.mark_unsupported("old".into(), Some("0.1.0".into()));

        let mut targets = HashSet::new();
        targets.insert(key);
        state.retain_targets(&targets);
        assert_eq!(state.snapshot_count(), 1);

        state.retain_targets(&HashSet::new());
        assert_eq!(state.snapshot_count(), 0);

        let mut devices = HashSet::new();
        devices.insert("local".to_string());
        assert!(state.retain_unsupported(&devices));
        assert!(state.is_supported("old"));
    }

    #[test]
    fn state_starts_with_no_snapshots_and_polls_idle() {
        let mut state = ChangeRequestsState::default();
        assert_eq!(state.snapshot_count(), 0);
        assert!(!state.poll());
        assert!(state.is_supported("anything"));
    }

    #[test]
    fn badge_for_chat_reads_the_live_state_and_paints() {
        let mut app = App::new(crate::UiConfig::default());
        app.chats = vec![chat("chat", "local", Some("/repo"), Some("checkout"))];
        let mut client = ChangeRequestClientState::default();
        client.store(
            ChangeRequestWatchKey {
                device_id: "local".into(),
                cwd: "/repo".into(),
            },
            snapshot("local", "/repo", "checkout"),
        );
        app.changes.change_requests = ChangeRequestsState {
            client,
            ..Default::default()
        };

        let badge = badge_for_chat(&app, "chat").expect("badge for the chat");
        assert_eq!(badge.number, "#90");
        assert_eq!(badge.state_label, "Open");
        assert!(badge_el(&app, "chat", ChangeRequestBadgeSurface::Sidebar, &app.theme).is_some());
        assert!(
            badge_el(
                &app,
                "chat",
                ChangeRequestBadgeSurface::Composer,
                &app.theme
            )
            .is_some()
        );
        assert!(badge_for_chat(&app, "missing").is_none());
        assert!(badge_el(&app, "missing", ChangeRequestBadgeSurface::Sidebar, &app.theme).is_none());
    }
}
