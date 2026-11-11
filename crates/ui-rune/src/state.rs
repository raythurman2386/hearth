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
    AuthState, Chat, ChatConfig, Connectivity, Device, Session, Space, TransferProgress,
    WorkspaceScope,
};
use hearth_update::UpdateStatus;

use crate::appearance::AppearanceState;
use crate::bridge::{Bridge, EngineEvent, WatchGuard};
use crate::composer::ComposerState;
use crate::engine::{EngineBootConfig, EngineHandle};
use crate::changes::ChangesState;
use crate::pickers::PickersState;
use crate::shell::ShellState;
use crate::settings::UiSettings;
use crate::terminal::TerminalState;
use crate::theme::{Appearance, Theme};
use crate::transcript::TranscriptState;
use crate::UiConfig;

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
}

impl App {
    pub fn new(config: UiConfig) -> Self {
        let data_dir = config.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        let system = Appearance::Dark;
        let appearance = AppearanceState::new(settings.appearance, system, data_dir.clone());
        let theme = Theme::for_appearance(appearance.resolved());
        Self {
            config,
            settings,
            appearance,
            theme,
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

    pub fn begin_upload_progress(
        &mut self,
        total: u64,
        done: Arc<std::sync::atomic::AtomicU64>,
    ) {
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

    pub fn send_pending(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends
            .get(chat_id)
            .is_some_and(|p| (now - p.started).num_milliseconds() < UNDELIVERED_GRACE_MS)
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
            self.apply_event(event);
            changed = true;
        }
        changed
    }

    fn apply_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Ready(handle) => self.attach_engine(*handle),
            EngineEvent::Failed(message) => {
                tracing::error!(%message, "engine bootstrap failed");
                self.connection = ConnectionStatus::Failed(message);
            }
            EngineEvent::Sessions(sessions) => self.apply_sessions(sessions),
            EngineEvent::Chats(chats) => self.apply_chats(chats),
            EngineEvent::Devices(devices) => self.apply_devices(devices),
            EngineEvent::Spaces(spaces) => self.apply_spaces(spaces),
            EngineEvent::Connectivity(connectivity) => self.apply_connectivity(connectivity),
            EngineEvent::Auth(auth) => self.apply_auth(auth),
            EngineEvent::Update(status) => self.apply_update(status),
            EngineEvent::Transfers(transfers) => self.apply_transfers(transfers),
            EngineEvent::TranscriptFrame { chat_id, frame } => {
                if self.selected_chat.as_deref() == Some(chat_id.as_str())
                    && let Err(err) = self.apply_transcript_frame(frame)
                {
                    tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                    self.respawn_transcript_watch();
                }
            }
            EngineEvent::SubagentFrame { doc_id, frame } => {
                if let Some(rows) = self.sub_transcripts.get_mut(&doc_id)
                    && let Err(err) = hearth_doc::apply_transcript_frame(rows, frame)
                {
                    tracing::warn!(%doc_id, error = %err, "resubscribing subagent watch");
                    if let Some(handle) = self.engine.clone() {
                        if let Some(guard) = self.sub_watch_guards.remove(&doc_id) {
                            guard.cancel();
                        }
                        let guard = spawn_subagent_watch(&self.bridge, handle, doc_id.clone());
                        self.sub_watch_guards.insert(doc_id, guard);
                    }
                }
            }
        }
    }

    fn attach_engine(&mut self, handle: EngineHandle) {
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
            |v| serde_json::from_value::<Vec<Chat>>(v)
                .map(EngineEvent::Chats)
                .ok(),
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
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_guard = Some(spawn_transcript_watch(
                &self.bridge,
                handle,
                chat_id,
            ));
        }
    }

    fn respawn_transcript_watch(&mut self) {
        self.transcript_guard = None;
        if let (Some(chat_id), Some(handle)) = (self.selected_chat.clone(), self.engine.clone()) {
            self.transcript_guard = Some(spawn_transcript_watch(
                &self.bridge,
                handle,
                chat_id,
            ));
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
        let resolved = self.appearance.resolved();
        if resolved == self.theme.appearance {
            return false;
        }
        self.theme = Theme::for_appearance(resolved);
        true
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

fn spawn_transcript_watch(
    bridge: &Bridge,
    handle: EngineHandle,
    chat_id: String,
) -> WatchGuard {
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

fn spawn_subagent_watch(
    bridge: &Bridge,
    handle: EngineHandle,
    doc_id: String,
) -> WatchGuard {
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
