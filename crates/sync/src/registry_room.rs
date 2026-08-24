//! Registry room server — Rust twin of `edge/src/registry-room.ts`.
//!
//! Per-user workspace index: current-state rows with per-field LWW, one
//! monotonic seq per accepted batch, broadcast to live sockets. Persistence
//! is `{data_dir}/registry.sqlite3`. Merge uses the same [`hearth_doc::apply_op`]
//! the client tests already share with the mock server.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::{SinkExt, StreamExt};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use hearth_doc::{RegistryRow, RowOp, apply_op};

use crate::chat_room::is_safe_id;
use crate::types::SyncError;

const PRESENCE_TTL_MS: i64 = 30_000;
const MAX_BATCH_OPS: usize = 500;
const MAX_FRAME_BYTES: usize = 1_000_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct Shared {
    conn: Connection,
    presence: HashMap<String, i64>,
}

impl Shared {
    fn open(path: impl AsRef<Path>) -> Result<Self, SyncError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SyncError::Tailnet(format!("registry dir: {e}")))?;
        }
        let conn = Connection::open(path.as_ref())
            .map_err(|e| SyncError::Tailnet(format!("registry open: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| SyncError::Tailnet(format!("registry wal: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rows (
                kind TEXT NOT NULL,
                id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                deleted INTEGER NOT NULL,
                del_hlc TEXT,
                fields TEXT NOT NULL,
                clocks TEXT NOT NULL,
                PRIMARY KEY (kind, id)
             );
             CREATE INDEX IF NOT EXISTS rows_seq ON rows (seq);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(|e| SyncError::Tailnet(format!("registry schema: {e}")))?;
        Ok(Self {
            conn,
            presence: HashMap::new(),
        })
    }

    fn meta(&self, key: &str) -> Result<Option<String>, SyncError> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SyncError::Tailnet(format!("registry meta: {e}")))
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), SyncError> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| SyncError::Tailnet(format!("registry set meta: {e}")))?;
        Ok(())
    }

    fn seq(&self) -> Result<u64, SyncError> {
        Ok(self.meta("seq")?.and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    fn gc_floor(&self) -> Result<u64, SyncError> {
        Ok(self
            .meta("gcFloor")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }

    fn load_row(&self, kind: &str, id: &str) -> Result<Option<RegistryRow>, SyncError> {
        self.conn
            .query_row(
                "SELECT seq, deleted, del_hlc, fields, clocks FROM rows WHERE kind = ?1 AND id = ?2",
                params![kind, id],
                |row| {
                    Ok(RegistryRow {
                        kind: kind.to_string(),
                        id: id.to_string(),
                        seq: row.get::<_, i64>(0)? as u64,
                        deleted: row.get::<_, i64>(1)? == 1,
                        del_hlc: row.get(2)?,
                        fields: serde_json::from_str(&row.get::<_, String>(3)?)
                            .unwrap_or_default(),
                        clocks: serde_json::from_str(&row.get::<_, String>(4)?)
                            .unwrap_or_default(),
                    })
                },
            )
            .optional()
            .map_err(|e| SyncError::Tailnet(format!("registry load: {e}")))
    }

    fn save_row(&self, row: &RegistryRow) -> Result<(), SyncError> {
        self.conn
            .execute(
                "INSERT INTO rows (kind, id, seq, deleted, del_hlc, fields, clocks)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(kind, id) DO UPDATE SET
                    seq = excluded.seq,
                    deleted = excluded.deleted,
                    del_hlc = excluded.del_hlc,
                    fields = excluded.fields,
                    clocks = excluded.clocks",
                params![
                    row.kind,
                    row.id,
                    row.seq as i64,
                    if row.deleted { 1 } else { 0 },
                    row.del_hlc.as_deref(),
                    serde_json::to_string(&row.fields).unwrap_or_else(|_| "{}".into()),
                    serde_json::to_string(&row.clocks).unwrap_or_else(|_| "{}".into()),
                ],
            )
            .map_err(|e| SyncError::Tailnet(format!("registry save: {e}")))?;
        Ok(())
    }

    fn rows_since(&self, cursor: u64, full: bool) -> Result<Vec<RegistryRow>, SyncError> {
        let sql = if full {
            "SELECT kind, id, seq, deleted, del_hlc, fields, clocks FROM rows ORDER BY seq"
        } else {
            "SELECT kind, id, seq, deleted, del_hlc, fields, clocks FROM rows WHERE seq > ?1 ORDER BY seq"
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| SyncError::Tailnet(format!("registry rows: {e}")))?;
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<RegistryRow> {
            Ok(RegistryRow {
                kind: row.get(0)?,
                id: row.get(1)?,
                seq: row.get::<_, i64>(2)? as u64,
                deleted: row.get::<_, i64>(3)? == 1,
                del_hlc: row.get(4)?,
                fields: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
                clocks: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
            })
        };
        let rows = if full {
            stmt.query_map([], map)
        } else {
            stmt.query_map(params![cursor as i64], map)
        };
        let rows = rows.map_err(|e| SyncError::Tailnet(format!("registry rows: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| SyncError::Tailnet(format!("registry row: {e}")))?);
        }
        Ok(out)
    }

    fn apply_ops(&mut self, ops: &[RowOp]) -> Result<(u64, u64, Vec<RegistryRow>), SyncError> {
        let next_seq = self.seq()? + 1;
        let mut touched: Vec<RegistryRow> = Vec::new();
        let mut applied = 0u64;
        for op in ops {
            let before = touched
                .iter()
                .find(|r| r.kind == op.kind && r.id == op.id)
                .cloned()
                .or_else(|| self.load_row(&op.kind, &op.id).ok().flatten());
            let (next, changed) = apply_op(before.as_ref(), op);
            let Some(mut next) = next else { continue };
            if !changed {
                continue;
            }
            applied += 1;
            next.seq = next_seq;
            touched.retain(|r| !(r.kind == next.kind && r.id == next.id));
            touched.push(next);
        }
        if applied > 0 {
            for row in &touched {
                self.save_row(row)?;
            }
            self.set_meta("seq", &next_seq.to_string())?;
        }
        Ok((self.seq()?, applied, touched))
    }
}

/// Persistent registry room (one per hub).
pub struct RegistryRoom {
    state: Mutex<Shared>,
    bcast: broadcast::Sender<String>,
}

impl RegistryRoom {
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>, SyncError> {
        let state = Shared::open(path)?;
        let (bcast, _) = broadcast::channel(1024);
        Ok(Arc::new(Self {
            state: Mutex::new(state),
            bcast,
        }))
    }

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
                    let text = match frame {
                        Some(Ok(WsMessage::Text(text))) => text.to_string(),
                        Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return,
                        Some(Ok(_)) => continue,
                    };
                    if text == "ping" {
                        if sink.send(WsMessage::Text("pong".into())).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    if text.len() > MAX_FRAME_BYTES {
                        return;
                    }
                    let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                        return;
                    };
                    match frame.get("t").and_then(Value::as_str) {
                        Some("hello") => {
                            let cursor = frame.get("cursor").and_then(Value::as_u64);
                            device = frame
                                .get("device")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            ready = true;
                            let reply = {
                                let state = lock(&self.state);
                                let seq = state.seq().unwrap_or(0);
                                let gc_floor = state.gc_floor().unwrap_or(0);
                                let full = match cursor {
                                    None => true,
                                    Some(c) => c < gc_floor || c > seq,
                                };
                                let rows = state.rows_since(cursor.unwrap_or(0), full).unwrap_or_default();
                                json!({
                                    "t": "state",
                                    "seq": seq,
                                    "full": full,
                                    "gcFloor": gc_floor,
                                    "rows": rows,
                                    "presence": state.presence,
                                })
                                .to_string()
                            };
                            if sink.send(WsMessage::Text(reply.into())).await.is_err() {
                                return;
                            }
                        }
                        Some("push") if ready => {
                            let batch = frame
                                .get("batch")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let Ok(ops) = serde_json::from_value::<Vec<RowOp>>(
                                frame.get("ops").cloned().unwrap_or(Value::Null),
                            ) else {
                                let _ = sink
                                    .send(WsMessage::Text(
                                        json!({"t":"error","code":"invalid_op","message":"bad ops"})
                                            .to_string()
                                            .into(),
                                    ))
                                    .await;
                                continue;
                            };
                            if ops.len() > MAX_BATCH_OPS {
                                let _ = sink
                                    .send(WsMessage::Text(
                                        json!({"t":"error","code":"invalid_op","message":"batch too large"})
                                            .to_string()
                                            .into(),
                                    ))
                                    .await;
                                continue;
                            }
                            let (ack, rows_frame) = {
                                let mut state = lock(&self.state);
                                let (seq, applied, touched) = match state.apply_ops(&ops) {
                                    Ok(v) => v,
                                    Err(err) => {
                                        tracing::warn!(error = %err, "registry: apply failed");
                                        return;
                                    }
                                };
                                let ack = json!({
                                    "t": "ack",
                                    "batch": batch,
                                    "seq": seq,
                                    "applied": applied
                                })
                                .to_string();
                                let rows_frame = (applied > 0)
                                    .then(|| json!({"t":"rows","seq":seq,"rows":touched}).to_string());
                                (ack, rows_frame)
                            };
                            if let Some(rows) = rows_frame {
                                let _ = self.bcast.send(rows);
                                while let Ok(text) = rx.try_recv() {
                                    if sink.send(WsMessage::Text(text.into())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            if sink.send(WsMessage::Text(ack.into())).await.is_err() {
                                return;
                            }
                        }
                        Some("presence") if ready => {
                            let at = frame.get("at").and_then(Value::as_i64).unwrap_or_else(now_ms);
                            {
                                let mut state = lock(&self.state);
                                state.presence.insert(device.clone(), at);
                                let horizon = now_ms() - PRESENCE_TTL_MS;
                                state.presence.retain(|_, t| *t >= horizon);
                            }
                            let _ = self.bcast.send(
                                json!({"t":"presence","device":device,"at":at}).to_string(),
                            );
                        }
                        Some("probe") => {
                            let seq = lock(&self.state).seq().unwrap_or(0);
                            if sink
                                .send(WsMessage::Text(
                                    json!({"t":"probe-ok","seq":seq}).to_string().into(),
                                ))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        _ => {
                            let _ = sink
                                .send(WsMessage::Text(
                                    json!({"t":"error","code":"bad_frame","message":"unknown"})
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                    }
                }
                text = rx.recv() => {
                    match text {
                        Ok(text) => {
                            if ready && sink.send(WsMessage::Text(text.into())).await.is_err() {
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

/// One registry per org key (tailnet typically uses a single `"tailnet"` org).
pub struct RegistryRooms {
    dir: std::path::PathBuf,
    rooms: Mutex<HashMap<String, Arc<RegistryRoom>>>,
}

impl RegistryRooms {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            dir: data_dir.as_ref().join("registry"),
            rooms: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, org: &str) -> Result<Arc<RegistryRoom>, SyncError> {
        let key = if org.is_empty() { "tailnet" } else { org };
        if !is_safe_id(key) {
            return Err(SyncError::Tailnet("invalid registry id".into()));
        }
        let mut rooms = lock(&self.rooms);
        if let Some(room) = rooms.get(key) {
            return Ok(room.clone());
        }
        let path = self.dir.join(format!("{key}.sqlite3"));
        let room = RegistryRoom::open(path)?;
        rooms.insert(key.to_string(), room.clone());
        Ok(room)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_doc::{OpKind, encode_hlc};

    #[test]
    fn persists_and_reads_back_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let room = RegistryRoom::open(dir.path().join("r.sqlite3")).unwrap();
        let op = RowOp {
            kind: "devices".into(),
            id: "d1".into(),
            op: OpKind::Upsert,
            set: Some([("name".into(), json!("minis"))].into_iter().collect()),
            hlc: encode_hlc(1, 0, "d1"),
            clocks: None,
        };
        {
            let mut state = lock(&room.state);
            let (seq, applied, touched) = state.apply_ops(&[op]).unwrap();
            assert_eq!(seq, 1);
            assert_eq!(applied, 1);
            assert_eq!(touched.len(), 1);
        }
        let room2 = RegistryRoom::open(dir.path().join("r.sqlite3")).unwrap();
        let rows = lock(&room2.state).rows_since(0, true).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "d1");
    }
}
