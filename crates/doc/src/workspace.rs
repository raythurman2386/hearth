//! LEGACY migration reader for the pre-registry Loro workspace doc (`workspace2`).
//!
//! This is NOT the live workspace API — that is [`crate::registry`]
//! (`RegistryDoc`, doc id `ws/{orgId}`). What survives here is the on-disk
//! schema of the old doc plus a read path, so the first boot after the registry
//! cutover can materialize the snapshot ONCE and seed the registry
//! (`RegistryDoc::seed_from_workspace`, called from `WorkspaceHost`). The
//! `upsert_*` builders exist only so tests can construct a legacy fixture; no
//! production code writes through this type. The snapshot itself is left on
//! disk for rollback.
//!
//! Container layout — maps keyed by id, NOT lists:
//! - `devices`: LoroMap keyed by deviceId → row map {id, name, platform,
//!   lastSeenAt, createdAt, version?}
//! - `spaces`: LoroMap keyed by spaceId → row map {id, deviceId, path, name?,
//!   gitDetected, gitCheckedAt?, checkoutId?, createdAt}
//! - `chats`: LoroMap keyed by chatId → row map {id, deviceId, title?, archived,
//!   cwd?, branch?, checkoutId?, config?(json), lastMessagePreview?,
//!   lastMessageAt?, createdAt, harnessSessionId?, harnessSessionCwd?,
//!   spaceId?, lastSeenAt?, roomGen?}
//! - `sessions`: LoroMap keyed by chatId → row map {chatId, deviceId, status,
//!   startedAt?, updatedAt}
//!
//! Timestamps are stored as epoch millis (the session-doc convention) and
//! surface as `chrono::DateTime<Utc>` through the `hearth_proto` entity types.

use chrono::{DateTime, Utc};
use loro::{ExportMode, LoroDoc, LoroMap, LoroValue, ToJson};
use serde::{Deserialize, Serialize};

use hearth_proto::{Chat, ChatConfig, Device, Session, SessionStatus, Space};

use crate::schema::DocError;

/// Everything in the legacy workspace doc, materialized (`read_all`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceState {
    pub devices: Vec<Device>,
    pub spaces: Vec<Space>,
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
}

/// Shape of a legacy `delete_space` cascade: whether the space row existed and
/// the chat ids removed alongside it, so the engine can drop local run state /
/// doc-host handles. Kept because `RegistryDoc::delete_space` returns the same
/// shape.
#[derive(Debug, Clone, PartialEq)]
pub struct DeletedSpace {
    pub existed: bool,
    pub chat_ids: Vec<String>,
}

/// A read handle over a legacy workspace doc snapshot.
pub struct WorkspaceDoc {
    doc: LoroDoc,
}

// Clippy's `new_without_default` gate; no caller uses `Default` directly.
impl Default for WorkspaceDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceDoc {
    /// Fresh, empty doc — fixture construction only.
    pub fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
        }
    }

    /// Wrap a snapshot imported on first boot (the migration read path).
    pub fn from_doc(doc: LoroDoc) -> Self {
        Self { doc }
    }

    /// Export a snapshot — fixture construction only.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, DocError> {
        self.doc
            .export(ExportMode::Snapshot)
            .map_err(|e| DocError::Schema(e.to_string()))
    }

    // ── fixture builders ────────────────────────────────────────────────────
    //
    // Write rows in the legacy shape so tests can seed a snapshot the migration
    // will later read. Production never calls these.

    pub fn upsert_device(&self, device: &Device) -> Result<(), DocError> {
        let row = self.row("devices", &device.id)?;
        row.insert("id", device.id.as_str())?;
        row.insert("name", device.name.as_str())?;
        row.insert("platform", device.platform.as_str())?;
        set_opt_ms(&row, "lastSeenAt", device.last_seen_at)?;
        set_opt_ms(&row, "createdAt", device.created_at)?;
        set_opt_str(&row, "version", device.version.as_deref())?;
        self.doc.commit();
        Ok(())
    }

    pub fn upsert_space(&self, space: &Space) -> Result<(), DocError> {
        let row = self.row("spaces", &space.id)?;
        row.insert("id", space.id.as_str())?;
        row.insert("deviceId", space.device_id.as_str())?;
        row.insert("path", space.path.as_str())?;
        set_opt_str(&row, "name", space.name.as_deref())?;
        row.insert("gitDetected", space.git_detected)?;
        set_opt_ms(&row, "gitCheckedAt", space.git_checked_at)?;
        set_opt_str(&row, "checkoutId", space.checkout_id.as_deref())?;
        row.insert("createdAt", space.created_at.timestamp_millis())?;
        self.doc.commit();
        Ok(())
    }

    pub fn upsert_chat(&self, chat: &Chat) -> Result<(), DocError> {
        let row = self.row("chats", &chat.id)?;
        row.insert("id", chat.id.as_str())?;
        row.insert("deviceId", chat.device_id.as_str())?;
        set_opt_str(&row, "title", chat.title.as_deref())?;
        row.insert("archived", chat.archived)?;
        set_opt_str(&row, "cwd", chat.cwd.as_deref())?;
        set_opt_str(&row, "branch", chat.branch.as_deref())?;
        set_opt_str(&row, "checkoutId", chat.checkout_id.as_deref())?;
        match &chat.config {
            Some(config) => row.insert("config", LoroValue::from(serde_json::to_value(config)?))?,
            None => row.delete("config")?,
        }
        set_opt_str(
            &row,
            "lastMessagePreview",
            chat.last_message_preview.as_deref(),
        )?;
        set_opt_ms(&row, "lastMessageAt", chat.last_message_at)?;
        row.insert("createdAt", chat.created_at.timestamp_millis())?;
        set_opt_str(&row, "harnessSessionId", chat.harness_session_id.as_deref())?;
        set_opt_str(
            &row,
            "harnessSessionCwd",
            chat.harness_session_cwd.as_deref(),
        )?;
        set_opt_str(&row, "spaceId", chat.space_id.as_deref())?;
        set_opt_ms(&row, "lastSeenAt", chat.last_seen_at)?;
        self.doc.commit();
        Ok(())
    }

    pub fn upsert_session(&self, session: &Session) -> Result<(), DocError> {
        let row = self.row("sessions", &session.chat_id)?;
        row.insert("chatId", session.chat_id.as_str())?;
        row.insert("deviceId", session.device_id.as_str())?;
        row.insert("status", status_str(session.status))?;
        set_opt_ms(&row, "startedAt", session.started_at)?;
        row.insert("updatedAt", session.updated_at.timestamp_millis())?;
        self.doc.commit();
        Ok(())
    }

    // ── reads (the migration path) ──────────────────────────────────────────

    pub fn read_all(&self) -> Result<WorkspaceState, DocError> {
        Ok(WorkspaceState {
            devices: self.read_devices()?,
            spaces: self.read_spaces()?,
            chats: self.read_chats()?,
            sessions: self.read_sessions()?,
        })
    }

    fn read_devices(&self) -> Result<Vec<Device>, DocError> {
        let mut devices: Vec<Device> = self
            .read_rows::<RawDevice>("devices")?
            .into_iter()
            .map(Device::from)
            .collect();
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(devices)
    }

    fn read_spaces(&self) -> Result<Vec<Space>, DocError> {
        let mut spaces: Vec<Space> = self
            .read_rows::<RawSpace>("spaces")?
            .into_iter()
            .map(Space::from)
            .collect();
        spaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(spaces)
    }

    fn read_chats(&self) -> Result<Vec<Chat>, DocError> {
        let mut chats: Vec<Chat> = self
            .read_rows::<RawChat>("chats")?
            .into_iter()
            .map(Chat::from)
            .collect();
        chats.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(chats)
    }

    fn read_sessions(&self) -> Result<Vec<Session>, DocError> {
        let mut sessions: Vec<Session> = self
            .read_rows::<RawSession>("sessions")?
            .into_iter()
            .map(Session::from)
            .collect();
        sessions.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        Ok(sessions)
    }

    // ── row plumbing ────────────────────────────────────────────────────────

    /// The row map for `key`, creating it when absent.
    fn row(&self, container: &str, key: &str) -> Result<LoroMap, DocError> {
        let parent = self.doc.get_map(container);
        match parent.get(key) {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
            _ => Ok(parent.insert_container(key, LoroMap::new())?),
        }
    }

    /// All rows of a container as typed values (malformed rows are skipped with a
    /// warning rather than failing the whole read — a bad peer must not blind us).
    fn read_rows<T: serde::de::DeserializeOwned>(
        &self,
        container: &str,
    ) -> Result<Vec<T>, DocError> {
        let value = self.doc.get_map(container).get_deep_value().to_json_value();
        let serde_json::Value::Object(rows) = value else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(rows.len());
        for (key, row) in rows {
            match serde_json::from_value::<T>(row) {
                Ok(parsed) => out.push(parsed),
                Err(err) => {
                    tracing::warn!(container, row = %key, error = %err, "skipping malformed workspace row");
                }
            }
        }
        Ok(out)
    }
}

fn set_opt_str(row: &LoroMap, key: &str, value: Option<&str>) -> Result<(), DocError> {
    match value {
        Some(v) => row.insert(key, v)?,
        None => row.delete(key)?,
    }
    Ok(())
}

fn set_opt_ms(row: &LoroMap, key: &str, value: Option<DateTime<Utc>>) -> Result<(), DocError> {
    match value {
        Some(at) => row.insert(key, at.timestamp_millis())?,
        None => row.delete(key)?,
    }
    Ok(())
}

fn status_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaitingInput",
        SessionStatus::Errored => "errored",
    }
}

fn dt(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

// ── doc-resident row shapes (epoch-millis timestamps) ───────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawDevice {
    id: String,
    name: String,
    platform: String,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    version: Option<String>,
}

impl From<RawDevice> for Device {
    fn from(raw: RawDevice) -> Self {
        Device {
            id: raw.id,
            name: raw.name,
            platform: raw.platform,
            last_seen_at: raw.last_seen_at.map(dt),
            created_at: raw.created_at.map(dt),
            version: raw.version,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawSpace {
    id: String,
    device_id: String,
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    git_detected: bool,
    #[serde(default)]
    git_checked_at: Option<i64>,
    #[serde(default)]
    checkout_id: Option<String>,
    #[serde(default)]
    created_at: i64,
}

impl From<RawSpace> for Space {
    fn from(raw: RawSpace) -> Self {
        Space {
            id: raw.id,
            device_id: raw.device_id,
            path: raw.path,
            name: raw.name,
            git_detected: raw.git_detected,
            git_checked_at: raw.git_checked_at.map(dt),
            checkout_id: raw.checkout_id,
            created_at: dt(raw.created_at),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawChat {
    id: String,
    device_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    checkout_id: Option<String>,
    /// LENIENT: a config this build can't decode (a harness/reasoning/sandbox
    /// id from a NEWER peer) degrades to `None` instead of failing the row.
    /// Retired harness ids (`claude-code`, `cursor`, …) deserialize as Huginn
    /// so old chats stay runnable. Truly unknown future ids still cost only
    /// the config, not the row.
    #[serde(default, deserialize_with = "lenient_chat_config")]
    config: Option<ChatConfig>,
    #[serde(default)]
    last_message_preview: Option<String>,
    #[serde(default)]
    last_message_at: Option<i64>,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    harness_session_id: Option<String>,
    #[serde(default)]
    harness_session_cwd: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    room_gen: Option<u32>,
}

/// Decode a chat row's `config` leniently: unknown enum values (a newer
/// peer's harness id, reasoning level or sandbox mode) cost the CONFIG, not
/// the row. Mirrors the transcript salvage rule: a missing field must cost
/// at most what the field carried.
fn lenient_chat_config<'de, D>(deserializer: D) -> Result<Option<ChatConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match serde_json::from_value::<ChatConfig>(value) {
        Ok(config) => Some(config),
        Err(err) => {
            tracing::warn!(error = %err, "chat config from a newer peer; showing the row without it");
            None
        }
    }))
}

impl From<RawChat> for Chat {
    fn from(raw: RawChat) -> Self {
        Chat {
            id: raw.id,
            device_id: raw.device_id,
            title: raw.title,
            archived: raw.archived,
            cwd: raw.cwd,
            branch: raw.branch,
            checkout_id: raw.checkout_id,
            config: raw.config,
            last_message_preview: raw.last_message_preview,
            last_message_at: raw.last_message_at.map(dt),
            created_at: dt(raw.created_at),
            harness_session_id: raw.harness_session_id,
            harness_session_cwd: raw.harness_session_cwd,
            space_id: raw.space_id,
            last_seen_at: raw.last_seen_at.map(dt),
            room_gen: raw.room_gen,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawSession {
    chat_id: String,
    device_id: String,
    status: SessionStatus,
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    updated_at: i64,
}

impl From<RawSession> for Session {
    fn from(raw: RawSession) -> Self {
        Session {
            chat_id: raw.chat_id,
            device_id: raw.device_id,
            status: raw.status,
            started_at: raw.started_at.map(dt),
            updated_at: dt(raw.updated_at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_proto::{HarnessId, SandboxLevel};

    fn ts(ms: i64) -> DateTime<Utc> {
        dt(ms)
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "linux".into(),
            last_seen_at: Some(ts(1_000)),
            created_at: Some(ts(500)),
            version: Some("0.1.0".into()),
        }
    }

    fn chat(id: &str, device_id: &str) -> Chat {
        Chat {
            id: id.into(),
            device_id: device_id.into(),
            title: Some("First chat".into()),
            archived: false,
            cwd: Some("/tmp/repo".into()),
            branch: Some("main".into()),
            checkout_id: None,
            config: Some(ChatConfig {
                harness: HarnessId::Mock,
                model: Some("mock-1".into()),
                mode: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            last_message_preview: None,
            last_message_at: None,
            created_at: ts(2_000),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    fn session(chat_id: &str, device_id: &str, status: SessionStatus) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: device_id.into(),
            status,
            started_at: Some(ts(3_000)),
            updated_at: ts(3_500),
        }
    }

    fn cross_sync(a: &WorkspaceDoc, b: &WorkspaceDoc) {
        let a_update = a
            .doc
            .export(ExportMode::updates(&b.doc.oplog_vv()))
            .expect("export a");
        let b_update = b
            .doc
            .export(ExportMode::updates(&a.doc.oplog_vv()))
            .expect("export b");
        b.doc.import(&a_update).expect("import into b");
        a.doc.import(&b_update).expect("import into a");
    }

    #[test]
    fn rows_round_trip() {
        let ws = WorkspaceDoc::new();
        ws.upsert_device(&device("dev-a", "laptop")).unwrap();
        ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        ws.upsert_session(&session("chat-1", "dev-a", SessionStatus::Working))
            .unwrap();

        let state = ws.read_all().unwrap();
        assert_eq!(state.devices, vec![device("dev-a", "laptop")]);
        assert_eq!(state.chats, vec![chat("chat-1", "dev-a")]);
        assert_eq!(
            state.sessions,
            vec![session("chat-1", "dev-a", SessionStatus::Working)]
        );

        // Upsert refreshes in place — no duplicate rows, cleared options removed.
        let mut updated = chat("chat-1", "dev-a");
        updated.title = None;
        updated.last_message_preview = Some("hello".into());
        updated.last_message_at = Some(ts(9_000));
        ws.upsert_chat(&updated).unwrap();
        let chats = ws.read_all().unwrap().chats;
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].title, None);
        assert_eq!(chats[0].last_message_preview.as_deref(), Some("hello"));
    }

    #[test]
    fn snapshot_round_trips_between_docs() {
        let ws = WorkspaceDoc::new();
        ws.upsert_device(&device("dev-a", "laptop")).unwrap();
        ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        let bytes = ws.export_snapshot().unwrap();

        let other = LoroDoc::new();
        other.import(&bytes).unwrap();
        let restored = WorkspaceDoc::from_doc(other);
        assert_eq!(restored.read_all().unwrap(), ws.read_all().unwrap());
    }

    #[test]
    fn two_peers_converge_on_disjoint_rows() {
        let a = WorkspaceDoc::new();
        let b = WorkspaceDoc::new();
        // Writer discipline: each device writes its own rows, concurrently.
        a.upsert_device(&device("dev-a", "laptop")).unwrap();
        a.upsert_chat(&chat("chat-a", "dev-a")).unwrap();
        a.upsert_session(&session("chat-a", "dev-a", SessionStatus::Working))
            .unwrap();
        b.upsert_device(&device("dev-b", "vps")).unwrap();
        b.upsert_chat(&chat("chat-b", "dev-b")).unwrap();

        cross_sync(&a, &b);

        let sa = a.read_all().unwrap();
        let sb = b.read_all().unwrap();
        assert_eq!(sa, sb);
        assert_eq!(
            sa.devices.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["dev-a", "dev-b"]
        );
        assert_eq!(
            sa.chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["chat-a", "chat-b"]
        );
        assert_eq!(sa.sessions.len(), 1);
    }
}
