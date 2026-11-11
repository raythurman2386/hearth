//! Root application state: the engine connection, entity lists, selection, and
//! the selected chat's transcript — one plain struct the whole shell renders
//! from.
//!
//! This is the Rune port of the gpui `AppState` entity. The reducers are pure
//! `&mut self` functions; the bridge ([`crate::bridge`]) replaces gpui's
//! `cx.spawn`/`Subscription` graph with watch tasks that post [`EngineEvent`]s
//! drained in [`App::drain_events`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use hearth_doc::{SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use hearth_proto::{
    AuthState, Chat, ChatConfig, ChatIndicator, Connectivity, Device, Session, Space,
    TransferProgress, WorkspaceScope,
};
use hearth_update::UpdateStatus;

use crate::UiConfig;
use crate::appearance::AppearanceState;
use crate::bridge::{Bridge, EngineEvent, WatchGuard};
use crate::changes::ChangesState;
use crate::composer::ComposerState;
use crate::engine::{EngineBootConfig, EngineHandle};
use crate::pickers::PickersState;
use crate::settings::UiSettings;
use crate::shell::ShellState;
use crate::terminal::TerminalState;
use crate::theme::{Appearance, Theme};
use crate::transcript::TranscriptState;

pub use hearth_proto::version_triple;
pub use hearth_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, SESSION_STALE_MS, attention_rank,
    chat_location, display_status, effective_indicator, format_time_ago, gate_phase, group_chats,
    parse_auth_state, project_label, sort_active, sort_chats, sort_spaces, sort_tabs,
};

/// A composer send whose doc command is queued but not yet executed.
#[derive(Debug, Clone)]
struct PendingSend {
    message_id: String,
    started: DateTime<Utc>,
}

/// How long an unadopted send reads as Working/Sending before flipping to the
/// explicit failed state with a retry affordance.
pub const UNDELIVERED_GRACE_MS: i64 = 120_000;

/// A send's attachment-upload leg in flight.
pub struct UploadProgress {
    done: Arc<std::sync::atomic::AtomicU64>,
    total: u64,
}

/// Root application state.
pub struct App {
    pub config: UiConfig,
    pub settings: UiSettings,
    pub appearance: AppearanceState,
    pub theme: Theme,
    /// The OS appearance seen by the view this frame (`Cx::system_dark`), so
    /// `poll` can reconcile `AppearanceMode::System` without a `Cx`.
    pub system_dark: std::cell::Cell<bool>,
    /// The window scale seen by the view this frame, for device-pixel hairlines.
    pub scale: std::cell::Cell<f32>,
    pub bridge: Bridge,

    pub engine: Option<EngineHandle>,
    pub connection: ConnectionStatus,
    pub workspace_scope: Option<WorkspaceScope>,
    pub auth: Option<AuthState>,
    pub devices: Vec<Device>,
    pub connectivity: Connectivity,
    pub spaces: Vec<Space>,
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,

    pub selected_space: Option<String>,
    pub no_project: bool,
    pub selected_device: Option<String>,
    pub selected_chat: Option<String>,
    pub auto_selected: bool,
    pub chats_synced: bool,
    pub spaces_synced: bool,

    pub transcript: Vec<SessionMessageEntry>,
    pub transcript_replayed: bool,
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    pending_sends: HashMap<String, PendingSend>,
    upload_progress: Option<UploadProgress>,
    transfers: HashMap<String, (u64, u64)>,
    pub diff_comments: HashMap<String, Vec<crate::comments::DiffComment>>,
    pub local_device_id: Option<String>,
    pub update: Option<UpdateStatus>,
    pub data_dir: Option<PathBuf>,

    /// Watch pumps. Cancelling a guard cancels the engine-side watch.
    watch_guards: Vec<WatchGuard>,
    transcript_guard: Option<WatchGuard>,
    sub_watch_guards: HashMap<String, WatchGuard>,
    sub_transcripts: HashMap<String, Vec<SessionMessageEntry>>,

    // ---- sub-states owned by their modules ----
    pub shell: ShellState,
    pub transcript_state: TranscriptState,
    pub composer: ComposerState,
    pub terminal: TerminalState,
    pub changes: ChangesState,
    pub pickers: PickersState,
    pub settings_ui: crate::settings::SettingsUi,

    /// The active Omarchy theme, when one is installed. `theme` is built from
    /// it; the Appearance setting is only the fallback.
    pub omarchy: Option<crate::omarchy::OmarchyTheme>,
    pub omarchy_generation: Option<String>,
    pub omarchy_checked_at: std::time::Instant,
}

impl App {
    pub fn new(config: UiConfig) -> Self {
        let data_dir = config.data_dir.clone();
        // Headless snapshots must not depend on the developer's settings file.
        let settings = if rune::tooling::headless_run().is_some() {
            UiSettings::default()
        } else {
            UiSettings::load(&data_dir)
        };
        let omarchy = crate::omarchy::OmarchyTheme::load();
        let system = omarchy
            .as_ref()
            .map(|om| om.appearance)
            .unwrap_or(Appearance::Dark);
        let appearance = AppearanceState::new(settings.appearance, system, data_dir.clone());
        let theme = omarchy
            .as_ref()
            .map(Theme::from_omarchy)
            .unwrap_or_else(|| Theme::for_appearance(appearance.resolved()));
        let omarchy_generation = omarchy.as_ref().map(|om| om.generation.clone());
        Self {
            config,
            settings,
            appearance,
            theme,
            system_dark: std::cell::Cell::new(system.is_dark()),
            scale: std::cell::Cell::new(1.0),
            bridge: Bridge::new(),
            engine: None,
            connection: ConnectionStatus::Connecting,
            workspace_scope: None,
            auth: None,
            devices: Vec::new(),
            connectivity: Connectivity::default(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            selected_space: None,
            no_project: false,
            selected_device: None,
            selected_chat: None,
            auto_selected: false,
            chats_synced: false,
            spaces_synced: false,
            transcript: Vec::new(),
            transcript_replayed: false,
            echoes: HashMap::new(),
            pending_sends: HashMap::new(),
            upload_progress: None,
            transfers: HashMap::new(),
            diff_comments: HashMap::new(),
            local_device_id: None,
            update: None,
            data_dir: Some(data_dir),
            watch_guards: Vec::new(),
            transcript_guard: None,
            sub_watch_guards: HashMap::new(),
            sub_transcripts: HashMap::new(),
            shell: ShellState::default(),
            transcript_state: TranscriptState::default(),
            composer: ComposerState::default(),
            terminal: TerminalState::default(),
            changes: ChangesState::default(),
            pickers: PickersState::default(),
            settings_ui: crate::settings::SettingsUi::default(),
            omarchy,
            omarchy_generation,
            omarchy_checked_at: std::time::Instant::now(),
        }
    }

    /// The view's entry point: the shell root.
    pub fn view(&self, cx: &mut rune::Cx<Self>) -> rune::El {
        crate::shell::view(self, cx)
    }

    /// The selected chat, or `""` on the new-chat canvas.
    pub fn composer_key(&self) -> String {
        self.selected_chat.clone().unwrap_or_default()
    }

    // ---- diff comments ----

    pub fn diff_comments(&self, key: &str) -> &[crate::comments::DiffComment] {
        self.diff_comments
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_diff_comment(&mut self, key: &str, comment: crate::comments::DiffComment) {
        self.diff_comments
            .entry(key.to_string())
            .or_default()
            .push(comment);
    }

    pub fn remove_diff_comment(&mut self, key: &str, id: &str) {
        if let Some(list) = self.diff_comments.get_mut(key) {
            list.retain(|c| c.id != id);
            if list.is_empty() {
                self.diff_comments.remove(key);
            }
        }
    }

    pub fn take_diff_comments(&mut self, key: &str) -> Vec<crate::comments::DiffComment> {
        self.diff_comments.remove(key).unwrap_or_default()
    }

    pub fn purge_diff_comments(&mut self, key: &str) {
        self.diff_comments.remove(key);
    }

    // ---- reducers ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        self.chats = chats;
        self.chats_synced = true;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            self.selected_chat = None;
            self.transcript.clear();
            self.transcript_replayed = false;
            self.transcript_guard = None;
        }
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        self.spaces_synced = true;
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| &s.id == selected)
        {
            self.selected_space = self.first_space_on_picked_device();
        }
        if self.selected_space.is_none() && !self.no_project {
            self.selected_space = self.first_space_on_picked_device();
        }
    }

    pub fn apply_chat_config(&mut self, chat_id: &str, config: ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_connectivity(&mut self, connectivity: Connectivity) {
        self.connectivity = connectivity;
    }

    pub fn apply_devices(&mut self, mut devices: Vec<Device>) {
        if self.workspace_scope == Some(WorkspaceScope::Local)
            && let Some(local_id) = self.local_device_id.as_deref()
            && let Some(device) = devices.iter_mut().find(|device| device.id == local_id)
            && device.name == "unknown-device"
        {
            device.name = "Local".to_string();
        }
        self.devices = devices;
    }

    pub fn apply_update(&mut self, status: UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        self.auth = Some(auth);
    }

    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        match parse_auth_state(&value) {
            Some(auth) => self.apply_auth(auth),
            None => tracing::warn!("dropping unrecognized AuthStatus frame"),
        }
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.transcript_replayed = true;
        self.ack_pending_send_from_transcript();
    }

    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        let is_reset = matches!(&frame, TranscriptFrame::Reset { .. });
        hearth_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        if is_reset {
            self.transcript_replayed = true;
        }
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        self.ack_pending_send_from_transcript();
        Ok(())
    }

    pub fn sub_transcript(&self, doc_id: &str) -> &[SessionMessageEntry] {
        self.sub_transcripts
            .get(doc_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Optimistic echoes for the selected chat, shown until the doc frame
    /// carrying the same message id arrives.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn watch_subagent_doc(&mut self, doc_id: String) {
        if self.sub_watch_guards.contains_key(&doc_id) {
            return;
        }
        let Some(handle) = self.engine.clone() else {
            return;
        };
        self.sub_transcripts.entry(doc_id.clone()).or_default();
        let guard = spawn_subagent_watch(&self.bridge, handle, doc_id.clone());
        self.sub_watch_guards.insert(doc_id, guard);
    }

    pub fn unwatch_subagent_doc(&mut self, doc_id: &str) {
        if let Some(guard) = self.sub_watch_guards.remove(doc_id) {
            guard.cancel();
        }
        self.sub_transcripts.remove(doc_id);
    }

    pub fn set_subagent_snapshot(&mut self, doc_id: String, entries: Vec<SessionMessageEntry>) {
        if let Some(guard) = self.sub_watch_guards.remove(&doc_id) {
            guard.cancel();
        }
        self.sub_transcripts.insert(doc_id, entries);
    }

    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    pub fn begin_pending_send(&mut self, chat_id: &str, message_id: &str, now: DateTime<Utc>) {
        self.pending_sends.insert(
            chat_id.to_string(),
            PendingSend {
                message_id: message_id.to_string(),
                started: now,
            },
        );
    }

    pub fn end_pending_send(&mut self, chat_id: &str, message_id: &str) {
        if self
            .pending_sends
            .get(chat_id)
            .is_some_and(|p| p.message_id == message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    pub fn begin_upload_progress(&mut self, total: u64, done: Arc<std::sync::atomic::AtomicU64>) {
        self.upload_progress = Some(UploadProgress { done, total });
    }

    pub fn end_upload_progress(&mut self) {
        self.upload_progress = None;
    }

    pub fn upload_progress_percent(&self) -> Option<u8> {
        let progress = self.upload_progress.as_ref()?;
        if progress.total == 0 {
            return None;
        }
        let done = progress
            .done
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(progress.total);
        Some(((done * 100) / progress.total).min(99) as u8)
    }

    pub fn apply_transfers(&mut self, transfers: Vec<TransferProgress>) {
        self.transfers = transfers
            .into_iter()
            .map(|t| (t.upload_id, (t.done, t.total)))
            .collect();
    }

    fn ack_pending_send_from_transcript(&mut self) {
        let Some(chat_id) = self.selected_chat.clone() else {
            return;
        };
        let Some(pending) = self.pending_sends.get(&chat_id).cloned() else {
            return;
        };
        if self.transcript.iter().any(|e| e.id == pending.message_id) {
            self.pending_sends.remove(&chat_id);
        }
    }

    // ---- derived queries ----

    pub fn spaces_sorted(&self) -> Vec<&Space> {
        let mut spaces: Vec<&Space> = self.spaces.iter().collect();
        spaces.sort_by_key(|s| (s.display_name().to_lowercase(), s.id.clone()));
        spaces
    }

    fn first_space_on_picked_device(&self) -> Option<String> {
        let device = self
            .selected_device
            .as_deref()
            .or(self.local_device_id.as_deref());
        let sorted = self.spaces_sorted();
        device
            .and_then(|d| sorted.iter().find(|s| s.device_id == d).copied())
            .or_else(|| sorted.first().copied())
            .map(|s| s.id.clone())
    }

    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        if self.no_project {
            return None;
        }
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    /// The device the new-session canvas targets: the picked project's host
    /// when one is selected, else the explicit device pick, else this device.
    pub fn effective_device_id(&self) -> Option<String> {
        if let Some(space) = self.selected_space_row() {
            return Some(space.device_id.clone());
        }
        self.selected_device
            .clone()
            .or_else(|| self.local_device_id.clone())
    }

    pub fn select_device(&mut self, device_id: Option<String>) {
        self.selected_device = device_id;
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    pub fn indicator_for(&self, chat_id: &str) -> Option<Indicator> {
        Some(effective_indicator(self.session_for(chat_id), Utc::now()))
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection)
    }

    /// Is a send still in flight for this chat (unacked)? Inside the grace
    /// window normally; while the delivery path is degraded the overlay holds
    /// indefinitely — the truth IS "Queued".
    pub fn send_pending(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|p| {
            (now - p.started).num_milliseconds() <= UNDELIVERED_GRACE_MS
                || self.chat_delivery_degraded(chat_id)
        })
    }

    /// The send has sat unadopted past the grace window: surface the explicit
    /// failed state instead of faking progress or forgetting it.
    pub fn send_undelivered(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends
            .get(chat_id)
            .is_some_and(|p| (now - p.started).num_milliseconds() > UNDELIVERED_GRACE_MS)
    }

    /// Retry pressed: restart the grace clock so the overlay returns to its
    /// Sending/Queued phase while the re-kicked delivery runs.
    pub fn retry_pending_send(&mut self, chat_id: &str, now: DateTime<Utc>) {
        if let Some(p) = self.pending_sends.get_mut(chat_id) {
            p.started = now;
        }
    }

    /// When the in-flight send (if any) was fired — the elapsed-timer base
    /// while the overlay reads as Working.
    pub fn pending_send_started(&self, chat_id: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.pending_sends
            .get(chat_id)
            .filter(|p| {
                (now - p.started).num_milliseconds() <= UNDELIVERED_GRACE_MS
                    || self.chat_delivery_degraded(chat_id)
            })
            .map(|p| p.started)
    }

    /// Percent of one queued attachment's relay transfer, by uploadId.
    pub fn transfer_percent(&self, upload_id: &str) -> Option<u8> {
        let (done, total) = self.transfers.get(upload_id)?;
        if *total == 0 {
            return None;
        }
        Some(((done.min(total) * 100) / total).min(99) as u8)
    }

    /// Full display status for a chat: a send in flight reads as Working.
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        if self.send_pending(&chat.id, now) {
            return ChatIndicator::Working;
        }
        display_status(chat, self.session_for(&chat.id), now)
    }

    pub fn chat_delivery_degraded(&self, chat_id: &str) -> bool {
        use hearth_proto::ConnectivityState as S;
        if self.connectivity.state == S::Disabled {
            return false;
        }
        let Some(chat) = self.chats.iter().find(|c| c.id == chat_id) else {
            return self.connectivity.state == S::Offline;
        };
        if Some(chat.device_id.as_str()) == self.local_device_id.as_deref() {
            return false;
        }
        if self.connectivity.state == S::Offline {
            return true;
        }
        let room_down = match self
            .connectivity
            .chats
            .iter()
            .find(|c| c.chat_id == chat_id)
        {
            Some(net) => !net.connected,
            None => self.connectivity.state != S::Connected,
        };
        room_down || !self.device_online(&chat.device_id, Utc::now())
    }

    pub fn send_queued(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.send_pending(chat_id, now) && self.chat_delivery_degraded(chat_id)
    }

    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        let Some(device) = self.devices.iter().find(|d| d.id == device_id) else {
            return true;
        };
        match device.last_seen_at {
            Some(seen) => (now - seen).num_seconds() <= 70,
            None => true,
        }
    }

    // ---- engine lifecycle ----

    /// Kick off (or retry) the engine bootstrap.
    pub fn bootstrap_engine(&mut self) {
        self.connection = ConnectionStatus::Connecting;
        self.workspace_scope = None;
        self.auth = None;
        let config = EngineBootConfig {
            data_dir: self.config.data_dir.clone(),
            ipc_port: self.config.ipc_port,
            edge_url: self.config.edge_url.clone(),
            edge_token: self.config.edge_token.clone(),
            org_id: self.config.org_id.clone(),
            default_harness: self.config.default_harness,
        };
        let bridge = self.bridge.clone();
        self.bridge.spawn(async move {
            match EngineHandle::bootstrap(config).await {
                Ok(handle) => bridge.send(EngineEvent::Ready(Box::new(handle))),
                Err(err) => bridge.send(EngineEvent::Failed(format!("{err:#}"))),
            }
        });
    }

    /// Drain queued engine events. Returns true when visible state changed.
    pub fn drain_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.bridge.try_recv() {
            changed |= self.apply_event(event);
        }
        changed
    }

    fn apply_event(&mut self, event: EngineEvent) -> bool {
        match event {
            EngineEvent::Ready(handle) => {
                self.attach_engine(*handle);
                true
            }
            EngineEvent::Failed(message) => {
                tracing::error!(%message, "engine bootstrap failed");
                self.connection = ConnectionStatus::Failed(message);
                true
            }
            EngineEvent::Sessions(sessions) => {
                self.apply_sessions(sessions);
                true
            }
            EngineEvent::Chats(chats) => {
                self.apply_chats(chats);
                true
            }
            EngineEvent::Devices(devices) => {
                self.apply_devices(devices);
                true
            }
            EngineEvent::Spaces(spaces) => {
                self.apply_spaces(spaces);
                true
            }
            EngineEvent::Connectivity(connectivity) => {
                self.apply_connectivity(connectivity);
                true
            }
            EngineEvent::Auth(auth) => {
                self.apply_auth(auth);
                true
            }
            EngineEvent::Update(status) => {
                self.apply_update(status);
                true
            }
            EngineEvent::Transfers(transfers) => {
                self.apply_transfers(transfers);
                true
            }
            EngineEvent::TranscriptFrame { chat_id, frame } => {
                if self.selected_chat.as_deref() != Some(chat_id.as_str()) {
                    return false;
                }
                if let Err(err) = self.apply_transcript_frame(frame) {
                    tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                    self.respawn_transcript_watch();
                }
                true
            }
            EngineEvent::SubagentFrame { doc_id, frame } => {
                let Some(rows) = self.sub_transcripts.get_mut(&doc_id) else {
                    return false;
                };
                if let Err(err) = hearth_doc::apply_transcript_frame(rows, frame) {
                    tracing::warn!(%doc_id, error = %err, "resubscribing subagent watch");
                    if let Some(handle) = self.engine.clone() {
                        if let Some(guard) = self.sub_watch_guards.remove(&doc_id) {
                            guard.cancel();
                        }
                        let guard = spawn_subagent_watch(&self.bridge, handle, doc_id.clone());
                        self.sub_watch_guards.insert(doc_id, guard);
                    }
                }
                true
            }
        }
    }

    fn attach_engine(&mut self, handle: EngineHandle) {
        // A retry must not stack a second set of watches on the first engine.
        for guard in self.watch_guards.drain(..) {
            guard.cancel();
        }
        let sub_docs: Vec<String> = self.sub_transcripts.keys().cloned().collect();
        for (_doc_id, guard) in self.sub_watch_guards.drain() {
            guard.cancel();
        }
        if let Some(old) = self.engine.take() {
            let old_mode = old.mode();
            let new_mode = handle.mode();
            if old.info.device_id != handle.info.device_id || old_mode != new_mode {
                self.bridge.spawn(async move { old.shutdown().await });
            }
        }
        self.workspace_scope = Some(handle.info.workspace_scope);
        self.local_device_id = Some(handle.info.device_id.clone());
        self.engine = Some(handle.clone());
        let client = handle.client.clone();

        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::WATCH_SESSIONS,
            serde_json::json!({}),
            |v| {
                serde_json::from_value::<Vec<Session>>(v)
                    .map(EngineEvent::Sessions)
                    .ok()
            },
        ));
        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::WATCH_CHATS,
            serde_json::json!({}),
            |v| {
                serde_json::from_value::<Vec<Chat>>(v)
                    .map(EngineEvent::Chats)
                    .ok()
            },
        ));
        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::WATCH_DEVICES,
            serde_json::json!({}),
            |v| {
                serde_json::from_value::<Vec<Device>>(v)
                    .map(EngineEvent::Devices)
                    .ok()
            },
        ));
        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::WATCH_CONNECTIVITY,
            serde_json::json!({}),
            |v| {
                serde_json::from_value::<Connectivity>(v)
                    .map(EngineEvent::Connectivity)
                    .ok()
            },
        ));
        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::WATCH_TRANSFERS,
            serde_json::json!({}),
            |v| {
                serde_json::from_value::<Vec<TransferProgress>>(v)
                    .map(EngineEvent::Transfers)
                    .ok()
            },
        ));
        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::WATCH_SPACES,
            serde_json::json!({}),
            |v| {
                serde_json::from_value::<Vec<Space>>(v)
                    .map(EngineEvent::Spaces)
                    .ok()
            },
        ));
        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::AUTH_STATUS,
            serde_json::json!({}),
            |v| parse_auth_state(&v).map(EngineEvent::Auth),
        ));
        self.watch_guards.push(spawn_typed_watch(
            &self.bridge,
            client.clone(),
            hearth_rpc::methods::UPDATE_STATUS,
            serde_json::json!({}),
            |v| {
                serde_json::from_value::<UpdateStatus>(v)
                    .map(EngineEvent::Update)
                    .ok()
            },
        ));

        self.connection = ConnectionStatus::Ready;
        self.transcript_guard = None;
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_guard = Some(spawn_transcript_watch(
                &self.bridge,
                handle.clone(),
                chat_id,
            ));
        }
        // Re-attach subagent tabs to the new engine's docs.
        for doc_id in sub_docs {
            let guard = spawn_subagent_watch(&self.bridge, handle.clone(), doc_id.clone());
            self.sub_watch_guards.insert(doc_id, guard);
        }
    }

    fn respawn_transcript_watch(&mut self) {
        self.transcript_guard = None;
        if let (Some(chat_id), Some(handle)) = (self.selected_chat.clone(), self.engine.clone()) {
            self.transcript_guard = Some(spawn_transcript_watch(&self.bridge, handle, chat_id));
        }
    }

    /// Select a chat (or clear).
    pub fn select_chat(&mut self, chat_id: Option<String>) {
        if self.selected_chat == chat_id {
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        self.transcript.clear();
        self.transcript_replayed = false;
        self.transcript_guard = None;
        if let Some(id) = chat_id.as_deref() {
            if let Some(chat) = self.chats.iter().find(|c| c.id == id) {
                match chat.space_id.clone() {
                    Some(space_id) => {
                        self.selected_space = Some(space_id);
                        self.no_project = false;
                    }
                    None => {
                        self.no_project = true;
                        self.selected_device = Some(chat.device_id.clone());
                    }
                }
            }
            self.mark_chat_seen(id);
        }
        self.respawn_transcript_watch();
    }

    pub fn select_space(&mut self, space_id: Option<String>) {
        match &space_id {
            Some(id) => {
                self.no_project = false;
                if let Some(device) = self.space_row(id).map(|s| s.device_id.clone()) {
                    self.selected_device = Some(device);
                }
            }
            None => self.no_project = true,
        }
        if space_id.is_some() {
            self.selected_space = space_id;
        }
    }

    pub fn mark_chat_seen(&mut self, chat_id: &str) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        let bridge = &self.bridge;
        bridge.spawn(async move {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle
                .client
                .call(hearth_rpc::methods::MUTATE, params)
                .await
            {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        });
    }

    /// Rebuild the theme after an appearance change. Returns true when the
    /// palette changed.
    pub fn refresh_theme(&mut self) -> bool {
        if let Some(om) = &self.omarchy {
            self.theme = Theme::from_omarchy(om);
            return true;
        }
        let resolved = self.appearance.resolved();
        if resolved == self.theme.appearance {
            return false;
        }
        self.theme = Theme::for_appearance(resolved);
        true
    }

    /// Follow an Omarchy theme change. Cheap: reads the generation file at most
    /// once a second and only rebuilds when it changed.
    pub fn refresh_omarchy_if_changed(&mut self) -> bool {
        if self.omarchy_checked_at.elapsed() < std::time::Duration::from_secs(1) {
            return false;
        }
        self.omarchy_checked_at = std::time::Instant::now();
        let generation = crate::omarchy::current_generation();
        if generation.as_deref() == self.omarchy_generation.as_deref() {
            return false;
        }
        match generation {
            Some(generation) => {
                let Some(om) = crate::omarchy::OmarchyTheme::load() else {
                    return false;
                };
                self.appearance.system = om.appearance;
                self.omarchy = Some(om);
                self.omarchy_generation = Some(generation);
                let theme = Theme::from_omarchy(self.omarchy.as_ref().expect("just set"));
                self.theme = theme;
                true
            }
            None => {
                if self.omarchy.take().is_some() {
                    self.omarchy_generation = None;
                    self.refresh_theme();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Persist settings (debounced by the caller).
    pub fn save_settings(&self) {
        let Some(data_dir) = self.data_dir.clone() else {
            return;
        };
        let settings = self.settings.clone();
        self.bridge.spawn(async move {
            if let Err(err) = settings.save(&data_dir) {
                tracing::warn!(error = %err, "could not save ui-settings");
            }
        });
    }
}

/// Spawn a typed watch with a serde parser.
fn spawn_typed_watch<F>(
    bridge: &Bridge,
    client: Arc<hearth_rpc::RpcClient>,
    method: &'static str,
    params: serde_json::Value,
    map: F,
) -> WatchGuard
where
    F: Fn(serde_json::Value) -> Option<EngineEvent> + Send + 'static,
{
    bridge.spawn_watch(client, method, params, map)
}

fn spawn_transcript_watch(bridge: &Bridge, handle: EngineHandle, chat_id: String) -> WatchGuard {
    let client = handle.client.clone();
    let params = serde_json::json!({ "chatId": chat_id });
    bridge.spawn_watch(
        client,
        hearth_rpc::methods::WATCH_DOC_MESSAGES,
        params,
        move |value| {
            serde_json::from_value::<TranscriptFrame>(value)
                .map(|frame| EngineEvent::TranscriptFrame {
                    chat_id: chat_id.clone(),
                    frame,
                })
                .ok()
        },
    )
}

fn spawn_subagent_watch(bridge: &Bridge, handle: EngineHandle, doc_id: String) -> WatchGuard {
    let client = handle.client.clone();
    let params = serde_json::json!({ "chatId": doc_id });
    bridge.spawn_watch(
        client,
        hearth_rpc::methods::WATCH_DOC_MESSAGES,
        params,
        move |value| {
            serde_json::from_value::<TranscriptFrame>(value)
                .map(|frame| EngineEvent::SubagentFrame {
                    doc_id: doc_id.clone(),
                    frame,
                })
                .ok()
        },
    )
}

#[cfg(test)]
mod tests {
    //! Pure reducer coverage for [`App`]: the gpui `AppState` behaviour this
    //! port replaced, driven with fixed clocks so every grace-window boundary
    //! is exact instead of timing-dependent.

    use super::*;
    use chrono::TimeDelta;
    use hearth_doc::{MessageRole, SessionMessageEntry};
    use hearth_proto::{ChatConnectivity, ChatIndicator, ConnectivityState, SessionStatus};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn base() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    /// A test app with a throwaway data dir, so the developer's
    /// `ui-settings.json` cannot leak into assertions.
    fn app() -> App {
        let dir = tempfile::tempdir().expect("temp data dir");
        App::new(crate::UiConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        })
    }

    fn chat(id: &str, device: &str, created_min: i64, last_message_min: Option<i64>) -> Chat {
        Chat {
            id: id.into(),
            device_id: device.into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_message_min.map(|m| base() + TimeDelta::minutes(m)),
            created_at: base() + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    fn space(id: &str, device: &str, name: &str, created_min: i64) -> Space {
        Space {
            id: id.into(),
            device_id: device.into(),
            path: format!("/repos/{name}"),
            name: Some(name.into()),
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base() + TimeDelta::minutes(created_min),
        }
    }

    fn entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::User,
            parts: Vec::new(),
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn ids(chats: &[Chat]) -> Vec<&str> {
        chats.iter().map(|c| c.id.as_str()).collect()
    }

    // ---- lists and selection ----

    #[test]
    fn apply_chats_sorts_by_recency_then_creation() {
        let mut app = app();
        app.apply_chats(vec![
            chat("no-messages-new", "dev", 5, None),
            chat("old-message", "dev", 0, Some(1)),
            chat("new-message", "dev", 0, Some(20)),
        ]);
        assert_eq!(
            ids(&app.chats),
            ["new-message", "no-messages-new", "old-message"],
            "last message, falling back to creation, newest first"
        );
        assert!(app.chats_synced);
    }

    #[test]
    fn apply_chats_drops_a_vanished_selection_and_clears_the_transcript() {
        let mut app = app();
        app.selected_chat = Some("gone".into());
        app.transcript = vec![entry("m1")];
        app.transcript_replayed = true;

        app.apply_chats(vec![chat("kept", "dev", 0, None)]);

        assert_eq!(app.selected_chat, None);
        assert!(app.transcript.is_empty());
        assert!(!app.transcript_replayed);
    }

    #[test]
    fn apply_chats_keeps_a_selection_that_survived() {
        let mut app = app();
        app.selected_chat = Some("kept".into());
        app.transcript = vec![entry("m1")];
        app.transcript_replayed = true;

        app.apply_chats(vec![chat("kept", "dev", 0, None)]);

        assert_eq!(app.selected_chat.as_deref(), Some("kept"));
        assert_eq!(app.transcript.len(), 1);
        assert!(app.transcript_replayed);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_a_vanished_selection() {
        let mut app = app();
        app.selected_device = Some("dev-b".into());
        app.selected_space = Some("gone".into());

        app.apply_spaces(vec![
            space("sp-b", "dev-b", "beta", 2),
            space("sp-a", "dev-b", "alpha", 1),
            space("sp-c", "dev-a", "gamma", 0),
        ]);

        assert_eq!(
            app.spaces.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["sp-c", "sp-a", "sp-b"],
            "creation order, id tiebreak"
        );
        assert_eq!(app.selected_space.as_deref(), Some("sp-a"));
        assert!(app.spaces_synced);
    }

    #[test]
    fn apply_spaces_prefers_the_local_device_when_nothing_is_picked() {
        let mut app = app();
        app.local_device_id = Some("dev-local".into());
        app.selected_space = None;

        app.apply_spaces(vec![
            space("remote", "dev-remote", "aaa", 0),
            space("local", "dev-local", "zzz", 1),
        ]);

        assert_eq!(app.selected_space.as_deref(), Some("local"));
    }

    #[test]
    fn apply_spaces_leaves_the_canvas_alone_when_no_project_is_picked() {
        let mut app = app();
        app.no_project = true;
        app.selected_space = None;

        app.apply_spaces(vec![space("sp-a", "dev", "alpha", 0)]);

        assert_eq!(app.selected_space, None, "the new-session canvas stays put");
    }

    #[test]
    fn gate_follows_the_connection_and_never_lies() {
        let mut app = app();
        app.connection = ConnectionStatus::Connecting;
        assert_eq!(app.gate(), GatePhase::Loading);
        app.connection = ConnectionStatus::Failed("boom".into());
        assert_eq!(app.gate(), GatePhase::Failed("boom".into()));
        app.connection = ConnectionStatus::Ready;
        assert_eq!(app.gate(), GatePhase::Ready);
    }

    #[test]
    fn visible_chats_hides_archived_rows() {
        let mut app = app();
        let mut archived = chat("archived", "dev", 0, Some(9));
        archived.archived = true;
        app.apply_chats(vec![chat("live", "dev", 0, None), archived]);
        let visible: Vec<_> = app.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["live"]);
    }

    #[test]
    fn select_chat_scopes_the_space_or_the_device_and_clears_the_transcript() {
        let mut app = app();
        let mut with_space = chat("in-space", "dev-a", 0, None);
        with_space.space_id = Some("sp-1".into());
        let loose = chat("loose", "dev-b", 0, None);
        app.apply_chats(vec![with_space, loose]);

        app.transcript = vec![entry("stale")];
        app.select_chat(Some("in-space".into()));
        assert_eq!(app.selected_chat.as_deref(), Some("in-space"));
        assert_eq!(app.selected_space.as_deref(), Some("sp-1"));
        assert!(!app.no_project);
        assert!(app.transcript.is_empty(), "a different chat starts empty");
        assert!(app.auto_selected);

        app.select_chat(Some("loose".into()));
        assert!(app.no_project);
        assert_eq!(app.selected_device.as_deref(), Some("dev-b"));

        app.transcript = vec![entry("keep-on-reselect")];
        app.select_chat(Some("loose".into()));
        assert_eq!(
            app.transcript.len(),
            1,
            "re-selecting the same chat must not wipe its transcript"
        );

        app.select_chat(None);
        assert_eq!(app.selected_chat, None);
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn select_space_marks_the_canvas_and_follows_the_owner_device() {
        let mut app = app();
        app.apply_spaces(vec![space("sp-a", "dev-a", "alpha", 0)]);

        app.select_space(Some("sp-a".into()));
        assert_eq!(app.selected_space.as_deref(), Some("sp-a"));
        assert_eq!(app.selected_device.as_deref(), Some("dev-a"));
        assert!(!app.no_project);

        app.select_space(None);
        assert!(app.no_project);
        assert_eq!(
            app.selected_space.as_deref(),
            Some("sp-a"),
            "None means the new-session canvas, not 'forget the project'"
        );
    }

    // ---- presence and delivery ----

    fn device(id: &str, seen_secs: Option<i64>) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            platform: "linux".into(),
            last_seen_at: seen_secs.map(|s| base() - TimeDelta::seconds(s)),
            created_at: Some(base()),
            version: None,
        }
    }

    #[test]
    fn device_online_is_local_fresh_or_unknown() {
        let mut app = app();
        app.local_device_id = Some("me".into());
        app.apply_devices(vec![
            device("fresh", Some(10)),
            device("edge", Some(70)),
            device("stale", Some(71)),
            Device {
                last_seen_at: None,
                ..device("never", None)
            },
        ]);

        assert!(
            app.device_online("me", base()),
            "this device is always here"
        );
        assert!(app.device_online("fresh", base()));
        assert!(
            app.device_online("edge", base()),
            "70s is still inside the window"
        );
        assert!(!app.device_online("stale", base()));
        assert!(app.device_online("never", base()), "no clock, no claim");
        assert!(
            app.device_online("unknown-id", base()),
            "an id no device row mentions is not evidence of absence"
        );
    }

    /// A device that checked in on the wall clock — `chat_delivery_degraded`
    /// reads presence from `Utc::now()`, not from a test-supplied instant.
    fn device_now(id: &str) -> Device {
        Device {
            last_seen_at: Some(Utc::now()),
            ..device(id, None)
        }
    }

    fn remote_chat() -> Chat {
        chat("remote-chat", "dev-remote", 0, Some(0))
    }

    fn connectivity(state: ConnectivityState, rooms: Vec<ChatConnectivity>) -> Connectivity {
        Connectivity {
            state,
            chats: rooms,
            ..Connectivity::default()
        }
    }

    #[test]
    fn chat_delivery_degraded_explains_every_offline_shape() {
        let mut app = app();
        app.local_device_id = Some("dev-local".into());
        let mut local = chat("local-chat", "dev-local", 0, Some(0));
        local.space_id = None;
        app.apply_chats(vec![local, remote_chat()]);
        app.apply_devices(vec![device_now("dev-remote")]);

        app.apply_connectivity(connectivity(ConnectivityState::Disabled, vec![]));
        assert!(!app.chat_delivery_degraded("remote-chat"));

        app.apply_connectivity(connectivity(ConnectivityState::Offline, vec![]));
        assert!(
            !app.chat_delivery_degraded("local-chat"),
            "a local chat never needs the edge"
        );
        assert!(app.chat_delivery_degraded("remote-chat"));
        assert!(
            app.chat_delivery_degraded("no-such-chat"),
            "with no room to consult, an offline edge is the only truth"
        );

        app.apply_connectivity(connectivity(
            ConnectivityState::Connected,
            vec![ChatConnectivity {
                chat_id: "remote-chat".into(),
                connected: false,
                pending_pushes: 0,
            }],
        ));
        assert!(app.chat_delivery_degraded("remote-chat"), "room is down");

        app.apply_connectivity(connectivity(
            ConnectivityState::Connected,
            vec![ChatConnectivity {
                chat_id: "remote-chat".into(),
                connected: true,
                pending_pushes: 0,
            }],
        ));
        assert!(!app.chat_delivery_degraded("remote-chat"));

        // The room is fine, but its host stopped checking in.
        app.apply_devices(vec![device("dev-remote", Some(600))]);
        assert!(app.chat_delivery_degraded("remote-chat"));
    }

    #[test]
    fn send_pending_overlays_working_until_the_grace_window() {
        let mut app = app();
        app.selected_chat = Some("c".into());
        app.begin_pending_send("c", "msg-1", base());

        assert!(app.send_pending("c", base()));
        assert!(!app.send_undelivered("c", base()));
        assert_eq!(app.pending_send_started("c", base()), Some(base()));

        let edge = base() + TimeDelta::milliseconds(UNDELIVERED_GRACE_MS);
        assert!(app.send_pending("c", edge), "the boundary is inclusive");
        assert!(!app.send_undelivered("c", edge));

        let past = base() + TimeDelta::milliseconds(UNDELIVERED_GRACE_MS + 1);
        assert!(!app.send_pending("c", past));
        assert!(app.send_undelivered("c", past));
        assert_eq!(app.pending_send_started("c", past), None);
    }

    #[test]
    fn degraded_delivery_holds_the_send_as_queued_indefinitely() {
        let mut app = app();
        app.local_device_id = Some("dev-local".into());
        app.apply_chats(vec![remote_chat()]);
        app.apply_devices(vec![device_now("dev-remote")]);
        app.apply_connectivity(connectivity(ConnectivityState::Offline, vec![]));
        app.begin_pending_send("remote-chat", "msg-1", base());

        let much_later = base() + TimeDelta::minutes(10);
        assert!(app.send_pending("remote-chat", much_later));
        assert!(
            app.send_undelivered("remote-chat", much_later),
            "the truth is still 'not delivered'"
        );
        assert!(app.send_queued("remote-chat", much_later));
        assert_eq!(
            app.pending_send_started("remote-chat", much_later),
            Some(base()),
            "the elapsed timer keeps its original base while queued"
        );
    }

    #[test]
    fn retry_restarts_the_grace_clock_and_end_only_ends_its_own_send() {
        let mut app = app();
        app.selected_chat = Some("c".into());
        app.begin_pending_send("c", "msg-1", base());

        let past = base() + TimeDelta::milliseconds(UNDELIVERED_GRACE_MS + 5);
        app.retry_pending_send("c", past);
        assert!(!app.send_undelivered("c", past));
        assert_eq!(app.pending_send_started("c", past), Some(past));

        app.end_pending_send("c", "someone-else");
        assert!(app.send_pending("c", past), "a stale ack must not clear it");
        app.end_pending_send("c", "msg-1");
        assert!(!app.send_pending("c", past));
    }

    #[test]
    fn display_status_prefers_the_in_flight_send_over_the_session() {
        let mut app = app();
        let mut c = chat("c", "dev", 0, Some(1));
        c.last_seen_at = None;
        app.apply_chats(vec![c.clone()]);
        app.selected_chat = Some("c".into());

        assert_eq!(
            app.display_status_for(&c, base()),
            ChatIndicator::Completed,
            "unseen activity"
        );

        app.begin_pending_send("c", "msg-1", base());
        assert_eq!(app.display_status_for(&c, base()), ChatIndicator::Working);
        let past = base() + TimeDelta::milliseconds(UNDELIVERED_GRACE_MS + 1);
        assert_eq!(
            app.display_status_for(&c, past),
            ChatIndicator::Completed,
            "past the window the overlay stops faking progress"
        );

        app.end_pending_send("c", "msg-1");
        c.last_seen_at = Some(base() + TimeDelta::minutes(10));
        app.chats[0].last_seen_at = c.last_seen_at;
        assert_eq!(app.display_status_for(&c, base()), ChatIndicator::Idle);

        app.apply_sessions(vec![Session {
            chat_id: "c".into(),
            device_id: "dev".into(),
            status: SessionStatus::AwaitingInput,
            started_at: None,
            updated_at: base(),
        }]);
        assert_eq!(
            app.display_status_for(&c, base()),
            ChatIndicator::AwaitingInput
        );
    }

    // ---- transfers and uploads ----

    #[test]
    fn transfers_track_by_upload_id_and_clamp_to_99() {
        let mut app = app();
        app.apply_transfers(vec![
            TransferProgress {
                upload_id: "u1".into(),
                file_name: "a.png".into(),
                done: 50,
                total: 100,
            },
            TransferProgress {
                upload_id: "u2".into(),
                file_name: "b.png".into(),
                done: 100,
                total: 100,
            },
        ]);

        assert_eq!(app.transfer_percent("u1"), Some(50));
        assert_eq!(app.transfer_percent("u2"), Some(99), "100% stays a spinner");
        assert_eq!(app.transfer_percent("missing"), None);

        app.apply_transfers(vec![TransferProgress {
            upload_id: "zero".into(),
            file_name: "empty".into(),
            done: 0,
            total: 0,
        }]);
        assert_eq!(app.transfer_percent("zero"), None);

        app.apply_transfers(vec![TransferProgress {
            upload_id: "over".into(),
            file_name: "over".into(),
            done: 300,
            total: 100,
        }]);
        assert_eq!(app.transfer_percent("over"), Some(99));
    }

    #[test]
    fn upload_progress_reads_the_shared_counter_and_clears() {
        let mut app = app();
        let done = Arc::new(AtomicU64::new(0));
        app.begin_upload_progress(200, done.clone());
        assert_eq!(app.upload_progress_percent(), Some(0));

        done.store(100, Ordering::Relaxed);
        assert_eq!(app.upload_progress_percent(), Some(50));

        done.store(200, Ordering::Relaxed);
        assert_eq!(app.upload_progress_percent(), Some(99));

        done.store(500, Ordering::Relaxed);
        assert_eq!(
            app.upload_progress_percent(),
            Some(99),
            "overshoot clamps, never exceeds the bar"
        );

        app.end_upload_progress();
        assert_eq!(app.upload_progress_percent(), None);

        app.begin_upload_progress(0, Arc::new(AtomicU64::new(0)));
        assert_eq!(
            app.upload_progress_percent(),
            None,
            "an empty transfer has no percentage to show"
        );
    }

    // ---- echoes and acks ----

    #[test]
    fn echoes_are_per_chat_deduped_and_retired_by_the_doc_frame() {
        let mut app = app();
        app.push_echo("a", entry("m1"));
        app.push_echo("a", entry("m1"));
        app.push_echo("b", entry("m2"));
        assert_eq!(app.pending_echoes().len(), 0, "nothing selected yet");

        app.selected_chat = Some("a".into());
        assert_eq!(app.pending_echoes().len(), 1, "push_echo dedupes by id");

        app.remove_echo("a", "m1");
        assert!(app.pending_echoes().is_empty());

        app.push_echo("a", entry("m3"));
        app.push_echo("a", entry("m4"));
        app.apply_transcript(vec![entry("m3")]);
        assert_eq!(
            app.pending_echoes()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["m4"],
            "only the confirmed echo retires"
        );
        assert!(app.transcript_replayed);
    }

    #[test]
    fn ack_clears_only_the_selected_chats_matching_send() {
        let mut app = app();
        app.apply_chats(vec![
            chat("a", "dev", 0, Some(0)),
            chat("b", "dev", 0, Some(0)),
        ]);
        app.selected_chat = Some("a".into());
        app.begin_pending_send("a", "msg-a", base());
        app.begin_pending_send("b", "msg-b", base());

        app.apply_transcript(vec![entry("msg-b")]);
        assert!(
            app.send_pending("a", base()),
            "another chat's message is not an ack"
        );

        app.apply_transcript(vec![entry("msg-a")]);
        assert!(!app.send_pending("a", base()));
        assert!(
            app.send_pending("b", base()),
            "a's ack does not clear b's pending send"
        );
    }

    // ---- diff comments ----

    #[test]
    fn diff_comments_add_remove_and_take_by_file() {
        let mut app = app();
        let one = crate::comments::DiffComment::new(
            "src/main.rs",
            crate::comments::CommentSide::New,
            12,
            "nit",
        );
        let two = crate::comments::DiffComment::new(
            "src/lib.rs",
            crate::comments::CommentSide::Old,
            3,
            "why?",
        );
        let one_id = one.id.clone();

        app.add_diff_comment("src/main.rs", one);
        app.add_diff_comment("src/main.rs", two.clone());
        assert_eq!(app.diff_comments("src/main.rs").len(), 2);

        app.remove_diff_comment("src/main.rs", &one_id);
        assert_eq!(app.diff_comments("src/main.rs").first(), Some(&two));

        app.remove_diff_comment("src/main.rs", "not-a-comment");
        assert_eq!(app.diff_comments("src/main.rs").len(), 1);

        app.add_diff_comment("empty-after-remove", two.clone());
        app.remove_diff_comment("empty-after-remove", &two.id);
        assert!(
            app.diff_comments("empty-after-remove").is_empty(),
            "the map drops empty entries"
        );

        let taken = app.take_diff_comments("src/main.rs");
        assert_eq!(taken.len(), 1);
        assert!(app.diff_comments("src/main.rs").is_empty());
        assert!(app.take_diff_comments("never-seen").is_empty());
    }

    #[test]
    fn purging_diff_comments_forgets_the_file_entirely() {
        let mut app = app();
        app.add_diff_comment(
            "a.rs",
            crate::comments::DiffComment::new("a.rs", crate::comments::CommentSide::New, 1, "x"),
        );
        app.purge_diff_comments("a.rs");
        assert!(app.diff_comments("a.rs").is_empty());
    }

    // ---- devices ----

    #[test]
    fn a_local_workspace_names_the_unknown_device_sentinel() {
        let mut app = app();
        app.workspace_scope = Some(WorkspaceScope::Local);
        app.local_device_id = Some("local".into());
        app.apply_devices(vec![
            device_with_name("local", "unknown-device"),
            device_with_name("remote", "unknown-device"),
        ]);
        assert_eq!(app.device_name("local"), Some("Local"));
        assert_eq!(app.device_name("remote"), Some("unknown-device"));

        app.apply_devices(vec![device_with_name("local", "Studio")]);
        assert_eq!(
            app.device_name("local"),
            Some("Studio"),
            "a real name is never overwritten"
        );
    }

    fn device_with_name(id: &str, name: &str) -> Device {
        Device {
            name: name.into(),
            ..device(id, Some(0))
        }
    }
}
