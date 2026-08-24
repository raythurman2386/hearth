//! Chat2 room server — Rust twin of `edge/src/chat-room.ts` + `chat-log.ts`.
//!
//! One SQLite log per chat: opaque Loro update rows + one checkpoint blob.
//! The room never imports Loro; it appends, relays, and serves bytes. Clients
//! still own CRDT semantics. Persistence lives under `{data_dir}/chat2/{id}.sqlite3`
//! so a hub restart is a table read, not a replay.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::{SinkExt, StreamExt};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::chat_frames::{self as wire, frame_type};
use crate::types::SyncError;

pub const MAX_ROW_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = MAX_ROW_BYTES + 8192;
const MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
const PRESENCE_TTL_MS: i64 = 30_000;
const QUOTA_WINDOW_MS: i64 = 60_000;
const QUOTA_MAX_PUSHES: u64 = 300;
const QUOTA_MAX_BYTES: u64 = 8 * 1024 * 1024;
const ROWS_BODY_CAP: usize = 4 * 1024 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
pub struct LogStats {
    pub head_seq: u64,
    pub seq_floor: u64,
    pub row_count: u64,
    pub row_bytes: u64,
    pub checkpoint_seq: u64,
    pub checkpoint_size: u64,
}

#[derive(Debug, Clone)]
pub struct LogRow {
    pub seq: u64,
    pub device: String,
    pub batch_id: String,
    pub bytes: Vec<u8>,
}

pub enum AppendOutcome {
    Ok { seq: u64, dup: bool },
    Empty,
    TooLarge,
}

pub enum CheckpointOutcome {
    Ok { seq_floor: u64, pruned: usize },
    Empty,
    FloorRegression,
    AheadOfHead,
}

/// One chat's append-only log.
pub struct ChatLog {
    conn: Mutex<Connection>,
}

impl ChatLog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SyncError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SyncError::Tailnet(format!("chat log dir: {e}")))?;
        }
        let conn = Connection::open(path.as_ref())
            .map_err(|e| SyncError::Tailnet(format!("chat log open: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| SyncError::Tailnet(format!("chat log wal: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rows (
                seq INTEGER PRIMARY KEY,
                device TEXT NOT NULL,
                batch_id TEXT NOT NULL UNIQUE,
                bytes BLOB NOT NULL,
                received_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS blobs (name TEXT PRIMARY KEY, bytes BLOB NOT NULL);",
        )
        .map_err(|e| SyncError::Tailnet(format!("chat log schema: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn meta(&self, key: &str) -> Result<Option<String>, SyncError> {
        lock(&self.conn)
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SyncError::Tailnet(format!("chat log meta: {e}")))
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), SyncError> {
        lock(&self.conn)
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| SyncError::Tailnet(format!("chat log set meta: {e}")))?;
        Ok(())
    }

    pub fn head_seq(&self) -> Result<u64, SyncError> {
        Ok(self
            .meta("headSeq")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }

    pub fn seq_floor(&self) -> Result<u64, SyncError> {
        Ok(self
            .meta("seqFloor")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }

    pub fn stats(&self) -> Result<LogStats, SyncError> {
        let conn = lock(&self.conn);
        let (row_count, row_bytes): (u64, u64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(bytes)), 0) FROM rows",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| SyncError::Tailnet(format!("chat log stats: {e}")))?;
        drop(conn);
        Ok(LogStats {
            head_seq: self.head_seq()?,
            seq_floor: self.seq_floor()?,
            row_count,
            row_bytes,
            checkpoint_seq: self
                .meta("checkpointSeq")?
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            checkpoint_size: self
                .meta("checkpointSize")?
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        })
    }

    pub fn blob(&self, name: &str) -> Result<Option<Vec<u8>>, SyncError> {
        lock(&self.conn)
            .query_row(
                "SELECT bytes FROM blobs WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SyncError::Tailnet(format!("chat log blob: {e}")))
    }

    fn put_blob(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError> {
        lock(&self.conn)
            .execute(
                "INSERT INTO blobs (name, bytes) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET bytes = excluded.bytes",
                params![name, bytes],
            )
            .map_err(|e| SyncError::Tailnet(format!("chat log put blob: {e}")))?;
        Ok(())
    }

    pub fn append(
        &self,
        device: &str,
        batch_id: &str,
        bytes: &[u8],
    ) -> Result<AppendOutcome, SyncError> {
        if bytes.is_empty() {
            return Ok(AppendOutcome::Empty);
        }
        if bytes.len() > MAX_ROW_BYTES {
            return Ok(AppendOutcome::TooLarge);
        }
        let conn = lock(&self.conn);
        let existing: Option<u64> = conn
            .query_row(
                "SELECT seq FROM rows WHERE batch_id = ?1",
                params![batch_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SyncError::Tailnet(format!("chat log dup: {e}")))?;
        if let Some(seq) = existing {
            return Ok(AppendOutcome::Ok { seq, dup: true });
        }
        let head: u64 = conn
            .query_row("SELECT value FROM meta WHERE key = 'headSeq'", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()
            .map_err(|e| SyncError::Tailnet(format!("chat log head: {e}")))?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let seq = head + 1;
        conn.execute(
            "INSERT INTO rows (seq, device, batch_id, bytes, received_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seq as i64, device, batch_id, bytes, now_ms()],
        )
        .map_err(|e| SyncError::Tailnet(format!("chat log insert: {e}")))?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('headSeq', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![seq.to_string()],
        )
        .map_err(|e| SyncError::Tailnet(format!("chat log bump head: {e}")))?;
        Ok(AppendOutcome::Ok { seq, dup: false })
    }

    pub fn rows_after(
        &self,
        after: u64,
        exclude_device: Option<&str>,
    ) -> Result<Vec<LogRow>, SyncError> {
        let conn = lock(&self.conn);
        let mut rows = Vec::new();
        if let Some(device) = exclude_device.filter(|d| !d.is_empty()) {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, device, batch_id, bytes FROM rows
                     WHERE seq > ?1 AND device != ?2 ORDER BY seq",
                )
                .map_err(|e| SyncError::Tailnet(format!("chat log rows: {e}")))?;
            let iter = stmt
                .query_map(params![after as i64, device], map_log_row)
                .map_err(|e| SyncError::Tailnet(format!("chat log rows: {e}")))?;
            for row in iter {
                rows.push(row.map_err(|e| SyncError::Tailnet(format!("chat log row: {e}")))?);
            }
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, device, batch_id, bytes FROM rows WHERE seq > ?1 ORDER BY seq",
                )
                .map_err(|e| SyncError::Tailnet(format!("chat log rows: {e}")))?;
            let iter = stmt
                .query_map(params![after as i64], map_log_row)
                .map_err(|e| SyncError::Tailnet(format!("chat log rows: {e}")))?;
            for row in iter {
                rows.push(row.map_err(|e| SyncError::Tailnet(format!("chat log row: {e}")))?);
            }
        }
        Ok(rows)
    }

    pub fn commit_checkpoint(
        &self,
        seq_covered: u64,
        frontier: &[u8],
        bytes: &[u8],
    ) -> Result<CheckpointOutcome, SyncError> {
        if bytes.is_empty() {
            return Ok(CheckpointOutcome::Empty);
        }
        let floor = self.seq_floor()?;
        let head = self.head_seq()?;
        if seq_covered < floor {
            return Ok(CheckpointOutcome::FloorRegression);
        }
        if seq_covered > head {
            return Ok(CheckpointOutcome::AheadOfHead);
        }
        self.put_blob("checkpoint", bytes)?;
        self.put_blob("checkpoint-frontier", frontier)?;
        let conn = lock(&self.conn);
        let before: usize = conn
            .query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))
            .map_err(|e| SyncError::Tailnet(format!("chat log count: {e}")))?;
        conn.execute(
            "DELETE FROM rows WHERE seq <= ?1",
            params![seq_covered as i64],
        )
        .map_err(|e| SyncError::Tailnet(format!("chat log prune: {e}")))?;
        let after: usize = conn
            .query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))
            .map_err(|e| SyncError::Tailnet(format!("chat log count: {e}")))?;
        drop(conn);
        self.set_meta("seqFloor", &seq_covered.to_string())?;
        self.set_meta("checkpointSeq", &seq_covered.to_string())?;
        self.set_meta("checkpointSize", &bytes.len().to_string())?;
        self.set_meta("checkpointAt", &now_ms().to_string())?;
        Ok(CheckpointOutcome::Ok {
            seq_floor: seq_covered,
            pruned: before.saturating_sub(after),
        })
    }

    pub fn encode_state_frame(&self) -> Result<Vec<u8>, SyncError> {
        let stats = self.stats()?;
        let frontier = self.blob("checkpoint-frontier")?.unwrap_or_default();
        Ok(wire::encode(
            frame_type::STATE,
            &serde_json::json!({
                "headSeq": stats.head_seq,
                "seqFloor": stats.seq_floor,
                "checkpointSeq": stats.checkpoint_seq,
                "checkpointSize": stats.checkpoint_size,
                "rowCount": stats.row_count,
                "rowBytes": stats.row_bytes,
            }),
            &frontier,
        ))
    }

    pub fn encode_pull_body(
        &self,
        after: u64,
        exclude_device: Option<&str>,
    ) -> Result<Vec<u8>, SyncError> {
        let mut frames = vec![self.encode_state_frame()?];
        let mut body_bytes = 0usize;
        let mut truncated = false;
        for row in self.rows_after(after, exclude_device)? {
            let frame = wire::encode(
                frame_type::ROW,
                &serde_json::json!({
                    "seq": row.seq,
                    "device": row.device,
                    "batchId": row.batch_id,
                }),
                &row.bytes,
            );
            if body_bytes + 4 + frame.len() > ROWS_BODY_CAP {
                truncated = true;
                break;
            }
            body_bytes += 4 + frame.len();
            frames.push(frame);
        }
        if !truncated {
            frames.push(wire::encode(
                frame_type::ROWS_DONE,
                &serde_json::json!({ "headSeq": self.head_seq()? }),
                &[],
            ));
        }
        let total: usize = frames.iter().map(|f| 4 + f.len()).sum();
        let mut body = Vec::with_capacity(total);
        for frame in frames {
            body.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            body.extend_from_slice(&frame);
        }
        Ok(body)
    }
}

fn map_log_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogRow> {
    Ok(LogRow {
        seq: row.get::<_, i64>(0)? as u64,
        device: row.get(1)?,
        batch_id: row.get(2)?,
        bytes: row.get(3)?,
    })
}

struct QuotaWindow {
    since: i64,
    pushes: u64,
    bytes: u64,
}

struct ChatRoomInner {
    log: ChatLog,
    presence: HashMap<String, i64>,
    quotas: HashMap<String, QuotaWindow>,
}

/// Live sockets + durable log for one chat.
pub struct ChatRoom {
    inner: Mutex<ChatRoomInner>,
    bcast: broadcast::Sender<Vec<u8>>,
}

impl ChatRoom {
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>, SyncError> {
        let log = ChatLog::open(path)?;
        let (bcast, _) = broadcast::channel(1024);
        Ok(Arc::new(Self {
            inner: Mutex::new(ChatRoomInner {
                log,
                presence: HashMap::new(),
                quotas: HashMap::new(),
            }),
            bcast,
        }))
    }

    fn inner(&self) -> MutexGuard<'_, ChatRoomInner> {
        lock(&self.inner)
    }

    pub fn stats(&self) -> Result<LogStats, SyncError> {
        self.inner().log.stats()
    }

    pub fn checkpoint_bytes(&self) -> Result<Option<Vec<u8>>, SyncError> {
        self.inner().log.blob("checkpoint")
    }

    pub fn checkpoint_seq(&self) -> Result<u64, SyncError> {
        Ok(self.inner().log.stats()?.checkpoint_seq)
    }

    pub fn encode_pull_body(
        &self,
        after: u64,
        exclude_device: Option<&str>,
    ) -> Result<Vec<u8>, SyncError> {
        self.inner().log.encode_pull_body(after, exclude_device)
    }

    pub fn commit_checkpoint(
        &self,
        seq_covered: u64,
        frontier: &[u8],
        bytes: &[u8],
    ) -> Result<CheckpointOutcome, SyncError> {
        self.inner()
            .log
            .commit_checkpoint(seq_covered, frontier, bytes)
    }

    pub fn append(
        &self,
        device: &str,
        batch_id: &str,
        bytes: &[u8],
    ) -> Result<AppendOutcome, SyncError> {
        let outcome = self.inner().log.append(device, batch_id, bytes)?;
        if let AppendOutcome::Ok { seq, dup: false } = &outcome {
            let relay = wire::encode(
                frame_type::ROW,
                &serde_json::json!({
                    "seq": seq,
                    "device": device,
                    "batchId": batch_id,
                }),
                bytes,
            );
            let _ = self.bcast.send(relay);
        }
        Ok(outcome)
    }

    fn admit_quota(&self, device: &str, bytes: usize) -> bool {
        let now = now_ms();
        let mut inner = lock(&self.inner);
        let window = inner
            .quotas
            .entry(device.to_string())
            .or_insert(QuotaWindow {
                since: now,
                pushes: 0,
                bytes: 0,
            });
        if now - window.since >= QUOTA_WINDOW_MS {
            window.since = now;
            window.pushes = 0;
            window.bytes = 0;
        }
        if window.pushes >= QUOTA_MAX_PUSHES || window.bytes + bytes as u64 > QUOTA_MAX_BYTES {
            return false;
        }
        window.pushes += 1;
        window.bytes += bytes as u64;
        true
    }

    /// Serve one already-handshaken WebSocket as a chat2 client.
    pub async fn serve_ws<S>(self: Arc<Self>, ws: tokio_tungstenite::WebSocketStream<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = ws.split();
        let mut rx = self.bcast.subscribe();
        let mut device = String::new();
        let mut ready = false;
        loop {
            tokio::select! {
                frame = stream.next() => {
                    match frame {
                        Some(Ok(WsMessage::Text(text))) if text == "ping" => {
                            if sink.send(WsMessage::Text("pong".into())).await.is_err() {
                                return;
                            }
                        }
                        Some(Ok(WsMessage::Text(_))) => {
                            let _ = sink.send(WsMessage::Close(None)).await;
                            return;
                        }
                        Some(Ok(WsMessage::Binary(bytes))) => {
                            if bytes.len() > MAX_FRAME_BYTES {
                                let _ = sink.send(WsMessage::Close(None)).await;
                                return;
                            }
                            let Some(decoded) = wire::decode(&bytes) else {
                                let err = wire::encode(
                                    frame_type::ERROR,
                                    &serde_json::json!({"code":"bad_frame","message":"malformed frame"}),
                                    &[],
                                );
                                let _ = sink.send(WsMessage::Binary(err.into())).await;
                                continue;
                            };
                            match decoded.kind {
                                frame_type::HELLO => {
                                    if let Some(d) = decoded.header.get("device").and_then(|v| v.as_str())
                                        && !d.is_empty()
                                    {
                                        device = d.to_string();
                                    }
                                    ready = true;
                                    let state = match self.inner().log.encode_state_frame() {
                                        Ok(bytes) => bytes,
                                        Err(err) => {
                                            tracing::warn!(error = %err, "chat room: state encode failed");
                                            return;
                                        }
                                    };
                                    if sink.send(WsMessage::Binary(state.into())).await.is_err() {
                                        return;
                                    }
                                }
                                frame_type::ROWS_REQ if ready => {
                                    let after = decoded
                                        .header
                                        .get("after")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let exclude = decoded
                                        .header
                                        .get("excludeOwn")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false)
                                        .then_some(device.as_str());
                                    let rows = match self.inner().log.rows_after(after, exclude) {
                                        Ok(rows) => rows,
                                        Err(err) => {
                                            tracing::warn!(error = %err, "chat room: rows_after failed");
                                            return;
                                        }
                                    };
                                    for row in rows {
                                        let frame = wire::encode(
                                            frame_type::ROW,
                                            &serde_json::json!({
                                                "seq": row.seq,
                                                "device": row.device,
                                                "batchId": row.batch_id,
                                            }),
                                            &row.bytes,
                                        );
                                        if sink.send(WsMessage::Binary(frame.into())).await.is_err() {
                                            return;
                                        }
                                    }
                                    let head = self.inner().log.head_seq().unwrap_or(0);
                                    let done = wire::encode(
                                        frame_type::ROWS_DONE,
                                        &serde_json::json!({ "headSeq": head }),
                                        &[],
                                    );
                                    if sink.send(WsMessage::Binary(done.into())).await.is_err() {
                                        return;
                                    }
                                }
                                frame_type::PUSH if ready => {
                                    let batch_id = decoded
                                        .header
                                        .get("batchId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    if batch_id.is_empty() || batch_id.len() > 128 {
                                        let err = wire::encode(
                                            frame_type::ERROR,
                                            &serde_json::json!({
                                                "code":"bad_push",
                                                "message":"hello first / malformed push",
                                                "batchId": batch_id,
                                            }),
                                            &[],
                                        );
                                        let _ = sink.send(WsMessage::Binary(err.into())).await;
                                        continue;
                                    }
                                    if !self.admit_quota(&device, decoded.payload.len()) {
                                        let err = wire::encode(
                                            frame_type::ERROR,
                                            &serde_json::json!({
                                                "code":"quota",
                                                "message":"per-device push quota exceeded",
                                                "batchId": batch_id,
                                            }),
                                            &[],
                                        );
                                        let _ = sink.send(WsMessage::Binary(err.into())).await;
                                        continue;
                                    }
                                    match self.append(&device, &batch_id, &decoded.payload) {
                                        Ok(AppendOutcome::Ok { seq, dup }) => {
                                            let ack = wire::encode(
                                                frame_type::ACK,
                                                &serde_json::json!({
                                                    "batchId": batch_id,
                                                    "seq": seq,
                                                    "dup": dup,
                                                }),
                                                &[],
                                            );
                                            if sink.send(WsMessage::Binary(ack.into())).await.is_err() {
                                                return;
                                            }
                                        }
                                        Ok(rejected) => {
                                            let code = match rejected {
                                                AppendOutcome::TooLarge => "too_large",
                                                _ => "empty",
                                            };
                                            let err = wire::encode(
                                                frame_type::ERROR,
                                                &serde_json::json!({
                                                    "code": code,
                                                    "message": format!("push rejected: {code}"),
                                                    "batchId": batch_id,
                                                }),
                                                &[],
                                            );
                                            let _ = sink.send(WsMessage::Binary(err.into())).await;
                                        }
                                        Err(err) => {
                                            tracing::warn!(error = %err, "chat room: append failed");
                                            return;
                                        }
                                    }
                                }
                                frame_type::PRESENCE if ready && !device.is_empty() => {
                                    let at = decoded
                                        .header
                                        .get("at")
                                        .and_then(|v| v.as_i64())
                                        .unwrap_or_else(now_ms);
                                    {
                                        let mut inner = lock(&self.inner);
                                        inner.presence.insert(device.clone(), at);
                                        let horizon = now_ms() - PRESENCE_TTL_MS;
                                        inner.presence.retain(|_, t| *t >= horizon);
                                    }
                                    let relay = wire::encode(
                                        frame_type::PRESENCE,
                                        &serde_json::json!({ "device": device, "at": at }),
                                        &decoded.payload,
                                    );
                                    let _ = self.bcast.send(relay);
                                }
                                frame_type::PROBE => {
                                    let head = self.inner().log.head_seq().unwrap_or(0);
                                    let ok = wire::encode(
                                        frame_type::PROBE_OK,
                                        &serde_json::json!({ "headSeq": head }),
                                        &[],
                                    );
                                    if sink.send(WsMessage::Binary(ok.into())).await.is_err() {
                                        return;
                                    }
                                }
                                _ => {
                                    let err = wire::encode(
                                        frame_type::ERROR,
                                        &serde_json::json!({
                                            "code":"bad_frame",
                                            "message": format!("unexpected type {}", decoded.kind),
                                        }),
                                        &[],
                                    );
                                    let _ = sink.send(WsMessage::Binary(err.into())).await;
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return,
                        Some(Ok(_)) => {}
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Ok(bytes) => {
                            if !ready {
                                continue;
                            }
                            // Match the DO: the pusher already has the bytes + ack;
                            // do not echo its own row/presence back on this socket.
                            if let Some(decoded) = wire::decode(&bytes)
                                && matches!(decoded.kind, frame_type::ROW | frame_type::PRESENCE)
                                && decoded.header.get("device").and_then(|v| v.as_str())
                                    == Some(device.as_str())
                            {
                                continue;
                            }
                            if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    }
}

/// Lazy map of chat id → live room (one SQLite file each).
pub struct ChatRooms {
    dir: PathBuf,
    rooms: Mutex<HashMap<String, Arc<ChatRoom>>>,
}

impl ChatRooms {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            dir: data_dir.as_ref().join("chat2"),
            rooms: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, chat_id: &str) -> Result<Arc<ChatRoom>, SyncError> {
        if !is_safe_id(chat_id) {
            return Err(SyncError::Tailnet("invalid chat id".into()));
        }
        let mut rooms = lock(&self.rooms);
        if let Some(room) = rooms.get(chat_id) {
            return Ok(room.clone());
        }
        let path = self.dir.join(format!("{chat_id}.sqlite3"));
        let room = ChatRoom::open(path)?;
        rooms.insert(chat_id.to_string(), room.clone());
        Ok(room)
    }
}

pub(crate) fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub fn checkpoint_too_large(len: usize) -> bool {
    len > MAX_CHECKPOINT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_dedupes_batch_id() {
        let dir = tempfile::tempdir().unwrap();
        let log = ChatLog::open(dir.path().join("c.sqlite3")).unwrap();
        match log.append("d1", "b1", &[1, 2, 3]).unwrap() {
            AppendOutcome::Ok { seq, dup } => {
                assert_eq!(seq, 1);
                assert!(!dup);
            }
            _ => panic!("first append"),
        }
        match log.append("d1", "b1", &[1, 2, 3]).unwrap() {
            AppendOutcome::Ok { seq, dup } => {
                assert_eq!(seq, 1);
                assert!(dup);
            }
            _ => panic!("dup append"),
        }
        assert_eq!(log.head_seq().unwrap(), 1);
        assert_eq!(log.rows_after(0, None).unwrap().len(), 1);
    }

    #[test]
    fn checkpoint_prunes_covered_rows() {
        let dir = tempfile::tempdir().unwrap();
        let log = ChatLog::open(dir.path().join("c.sqlite3")).unwrap();
        log.append("d", "a", &[1]).unwrap();
        log.append("d", "b", &[2]).unwrap();
        match log.commit_checkpoint(1, &[9], &[7, 7, 7]).unwrap() {
            CheckpointOutcome::Ok { seq_floor, pruned } => {
                assert_eq!(seq_floor, 1);
                assert_eq!(pruned, 1);
            }
            _ => panic!("checkpoint"),
        }
        assert_eq!(log.seq_floor().unwrap(), 1);
        assert_eq!(log.rows_after(0, None).unwrap().len(), 1);
        assert_eq!(log.blob("checkpoint").unwrap().unwrap(), vec![7, 7, 7]);
    }
}
