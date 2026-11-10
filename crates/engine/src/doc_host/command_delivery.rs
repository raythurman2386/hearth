//! Command delivery: queue → nudge/escort → drain → execute → retry.
//!
//! The doc is the outbox: a command commits locally and syncs whenever a room
//! exists. This module owns everything between that commit and the executor:
//! the durable queue write, the peer nudge, the attachment escort and relay
//! fallback, the host's drain loop, and the user-driven retry.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use base64::Engine as _;

use hearth_doc::{
    COMMAND_DEFAULT_TTL_MS, CommandBasedOn, CommandDisposition, EvaluationContext, MessagePart,
    MessageStatus, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus,
    evaluate_command,
};
use hearth_proto::{UserInputAnswer, UserInputQuestion};

use super::*;
use crate::sessions::{SessionsEngine, SteerOutcome};
use crate::{EngineError, new_id, now_ms};

/// Queued-attachment transfer pacing: chunk pushes are bounded per call (a
/// stalled-but-open relay link never fails on its own) and a timeout marks
/// the link suspect; attempts retry on this backoff, cut short by the online
/// bus / system wake.
const TRANSFER_CHUNK_B64: usize = 60_000;
const TRANSFER_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const TRANSFER_COMMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const TRANSFER_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(2);
const TRANSFER_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);
/// A command whose attachment bytes are still in transit waits at most this
/// long before the drain rejects it loudly (and the transfer task gives up on
/// the same clock) — a chat must never wedge behind bytes that aren't coming.
const ATTACHMENT_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const ATTACHMENT_WAIT_MAX_MS: i64 = ATTACHMENT_WAIT_MAX.as_millis() as i64;
/// Re-check cadence while a chat's queue is deferred on in-transit bytes
/// (the happy path is event-driven — UploadCommit kicks the drain — this
/// timer only covers the give-up transition and missed kicks).
const ATTACHMENT_WAIT_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// Transfer attempt outcome: transient failures retry (the link may heal),
/// permanent ones stop (the host actively refused, or the bytes are gone).
enum TransferError {
    Transient(String),
    Permanent(String),
}

/// Peer-relay delivery fallback pacing (`spawn_command_delivery`): the grace
/// the normal rows→edge path gets before the relay road opens, the poll while
/// waiting, the relay retry curve, its per-call deadline, and the give-up cap
/// (the command stays durably queued in the doc regardless).
const ROWS_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
const ROWS_POLL: std::time::Duration = std::time::Duration::from_secs(1);
const RELAY_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(5);
const RELAY_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);
const RELAY_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const RELAY_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Hosts below this stamped engine version don't serve `RelayCommand`.
const RELAY_MIN_VERSION: (u64, u64, u64) = (0, 2, 12);
/// The queued-attachment transfers a command's `pending://` refs imply —
/// shared by the retry escort and the retry re-issue.
/// Peer `/rpc` cold-host wake: ask `target` to open `chat_id` (drain follows).
async fn peer_open_chat(
    links: &std::sync::Arc<hearth_rpc::LinkCache>,
    target: &str,
    chat_id: &str,
) -> Result<(), String> {
    let client = links
        .client(target)
        .await
        .map_err(|e| format!("peer link: {e}"))?;
    let params = serde_json::json!({ "chatId": chat_id });
    let call = client.call(hearth_rpc::methods::OPEN_CHAT, params);
    match tokio::time::timeout(RELAY_CALL_TIMEOUT, call).await {
        Err(_) => {
            links.invalidate(target);
            Err("OpenChat timed out; peer link suspect".into())
        }
        Ok(Err(hearth_rpc::RpcError::Failed(err))) => Err(format!("host refused: {err}")),
        Ok(Err(err)) => {
            links.invalidate(target);
            Err(format!("OpenChat failed: {err}"))
        }
        Ok(Ok(_)) => Ok(()),
    }
}

fn command_transfers(entry: &SessionCommandEntry) -> Vec<crate::uploads::AttachmentTransfer> {
    let refs: Vec<String> = match &entry.payload {
        SessionCommandPayload::Run { request, .. } => request
            .attachments
            .iter()
            .filter(|p| crate::uploads::is_pending_ref(p))
            .cloned()
            .collect(),
        SessionCommandPayload::Steer { prompt, .. } => crate::uploads::pending_refs_in(prompt),
        _ => Vec::new(),
    };
    refs.iter()
        .filter_map(|r| crate::uploads::parse_pending_ref(r))
        .map(
            |(upload_id, file_name)| crate::uploads::AttachmentTransfer {
                upload_id: upload_id.to_string(),
                file_name: file_name.to_string(),
            },
        )
        .collect()
}

/// Retires a transfer's progress entry on drop — the one exit point for
/// `push_attachments`' many returns (commit landed, chunk timeout, host
/// refusal), so no failure path can leave a phantom ring behind.
struct TransferProgressGuard<'a> {
    host: &'a DocHost,
    upload_id: &'a str,
}

impl Drop for TransferProgressGuard<'_> {
    fn drop(&mut self) {
        self.host.transfer_progress_clear(self.upload_id);
    }
}
impl DocHost {
    /// Composer path: append an immutable pending command entry (rule 1). Durable by
    /// construction — the change subscription kicks the drain, so a local host executes
    /// immediately and an offline doc simply holds the entry until it syncs.
    pub fn queue_command(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
    ) -> Result<String, EngineError> {
        self.queue_command_with_transfers(chat_id, payload, Vec::new())
    }

    /// [`Self::queue_command`] plus queued-attachment transfers: the command's
    /// `pending://` refs name bytes already committed to THIS device's uploads
    /// dir; when another device hosts the chat, a background task pushes them
    /// over the peer link (retry until they land) while the command is
    /// already durably queued. The send is a local write — attachment bytes
    /// chase it, never gate it (2026-08-19 incident).
    pub fn queue_command_with_transfers(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
        transfers: Vec<crate::uploads::AttachmentTransfer>,
    ) -> Result<String, EngineError> {
        let handle = self.open(chat_id)?;
        let id = new_id();
        let now = now_ms();
        let based_on = handle.doc.read_entries()?.last().map(|m| CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        });
        let is_message = matches!(
            payload,
            SessionCommandPayload::Run { .. } | SessionCommandPayload::Steer { .. }
        );
        let entry = SessionCommandEntry {
            id: id.clone(),
            payload,
            issued_by: self.inner.config.device_id.clone(),
            issued_at: now,
            based_on,
            expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
            status: SessionCommandStatus::Pending,
            resolution: None,
        };
        handle.doc.queue_command(&entry)?;
        // Sending a message revives an archived chat: the user is acting in it
        // again, so the LWW row flips back to active on every device. Best-
        // effort — the command itself is durable regardless.
        if is_message && let Some(workspace) = self.workspace() {
            match workspace.chat(chat_id) {
                Ok(Some(chat)) if chat.archived => {
                    if let Err(err) = workspace.set_chat_archived(chat_id, false) {
                        tracing::warn!(chat = %chat_id, error = %err, "unarchive on send failed");
                    }
                }
                _ => {}
            }
        }
        // §7 durable delivery: when another device hosts this chat, nudge its device
        // room so a cold host opens the doc and drains the queue. Fire-and-forget —
        // the command is durable in the doc either way (a host that opens the chat
        // for any other reason still executes it).
        self.nudge_remote_host(chat_id);
        self.spawn_command_delivery(chat_id, entry, transfers);
        Ok(id)
    }

    /// Wake a remote host so it opens this chat and drains pending commands.
    ///
    /// Tailscale path (preferred): peer RPC [`hearth_rpc::methods::OPEN_CHAT`]
    /// over `LinkCache` — DeviceRoom was removed; the hub has no
    /// `/device/{id}/nudge` route.
    ///
    /// Legacy path: `POST {edge}/device/{host}/nudge` when HostRelay is still
    /// wired to a classic edge (non-tailnet). Best-effort either way.
    fn nudge_remote_host(&self, chat_id: &str) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let host_device = match workspace.chat(chat_id) {
            Ok(Some(chat)) => chat.device_id,
            // Unclaimed chat: whoever drains first claims it — nobody to nudge.
            _ => return,
        };
        if host_device == self.inner.config.device_id {
            return;
        }
        // Only meaningful inside a runtime (RPC handlers, executors); bare sync
        // callers (unit tests) skip rather than panic.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let chat = chat_id.to_string();
        let links = self.inner.links.get().cloned();
        let edge = self.inner.config.edge.clone();
        self.spawn_worker_on(&runtime, async move {
            if let Some(links) = links.as_ref() {
                match peer_open_chat(links, &host_device, &chat).await {
                    Ok(()) => {
                        tracing::info!(
                            chat = %chat,
                            device = %host_device,
                            "host nudged via peer OpenChat"
                        );
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(
                            chat = %chat,
                            device = %host_device,
                            error = %err,
                            "peer OpenChat failed; trying HTTP device nudge if edge is set"
                        );
                    }
                }
            }
            let Some(edge) = edge else {
                return;
            };
            let url = format!(
                "{}/device/{}/nudge",
                edge.url.trim_end_matches('/'),
                host_device
            );
            // Fresh bearer per request — never the boot-time snapshot.
            let Some(bearer) = edge.bearer().await else {
                tracing::warn!(chat = %chat, "nudge skipped: signed out");
                return;
            };
            let send = reqwest::Client::new()
                .post(&url)
                .bearer_auth(&bearer)
                .json(&serde_json::json!({ "chatId": chat }))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match send {
                Ok(res) if res.status().is_success() => {
                    tracing::info!(chat = %chat, device = %host_device, "host nudged");
                }
                Ok(res) => tracing::warn!(chat = %chat, device = %host_device,
                    status = res.status().as_u16(), "nudge rejected"),
                Err(err) => {
                    tracing::warn!(chat = %chat, error = %err, "nudge failed (best-effort)")
                }
            }
        });
    }

    /// The chat's host device when it is NOT this engine (mirrors
    /// `nudge_remote_host`'s ownership read).
    fn remote_host_for(&self, chat_id: &str) -> Option<String> {
        let workspace = self.workspace()?;
        let host_device = match workspace.chat(chat_id) {
            Ok(Some(chat)) => chat.device_id,
            _ => return None,
        };
        (host_device != self.inner.config.device_id).then_some(host_device)
    }

    /// Durable-delivery escort for one queued command aimed at a REMOTE host:
    ///
    /// 1. push any queued attachment bytes over the peer link (retry until
    ///    they land — the relayed command must never outrun its bytes);
    /// 2. give the normal path (chat2 rows → edge → host's room) a short
    ///    grace to ack;
    /// 3. rows still not at the edge but the peer link alive → relay-forward
    ///    the entry itself ([`hearth_rpc::methods::RELAY_COMMAND`]). The
    ///    host's processed ledger claims the client-minted id, so the doc
    ///    row arriving later dedupes to a no-op — exactly-once by
    ///    construction (the 2026-08-18 03:45 incident shape: nudges flowed,
    ///    rows didn't; there was no second road for the command).
    ///
    /// Stops the moment any path lands. No-op for locally-hosted chats.
    fn spawn_command_delivery(
        &self,
        chat_id: &str,
        entry: SessionCommandEntry,
        transfers: Vec<crate::uploads::AttachmentTransfer>,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return; // bare sync callers (unit tests) skip rather than panic
        };
        let host = self.clone();
        let chat = chat_id.to_string();
        self.spawn_worker_on(&runtime, async move {
            let Some(target) = host.remote_host_for(&chat) else {
                return; // local host (or no row yet claimed remotely)
            };
            if !transfers.is_empty() && !host.deliver_attachments(&chat, &transfers).await {
                return; // gave up; the drain's wait cap surfaces the failure
            }
            let mut wake = hearth_sync::wake::subscribe();
            let mut online = hearth_sync::wake::subscribe_online();
            let give_up = tokio::time::Instant::now() + RELAY_GIVE_UP;
            let grace_end = tokio::time::Instant::now() + ROWS_GRACE;
            while tokio::time::Instant::now() < grace_end {
                if host.rows_flushed(&chat) {
                    // Rows on the hub are not host adoption — re-kick wake so
                    // a Tailscale host opens the chat (HTTP /device nudge 404s).
                    host.nudge_remote_host(&chat);
                    return;
                }
                tokio::time::sleep(ROWS_POLL).await;
            }
            let mut backoff = RELAY_BACKOFF_BASE;
            loop {
                if host.rows_flushed(&chat) {
                    host.nudge_remote_host(&chat);
                    return;
                }
                match host.relay_command(&target, &chat, &entry).await {
                    Ok(outcome) => {
                        tracing::info!(chat = %chat, device = %target, command = %entry.id,
                            outcome, "command delivered via peer relay");
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat, device = %target, error = %err,
                            backoff_ms = backoff.as_millis() as u64,
                            "peer-relay delivery retrying");
                    }
                }
                if tokio::time::Instant::now() >= give_up {
                    tracing::warn!(chat = %chat, command = %entry.id,
                        "peer-relay delivery gave up; command remains queued in the doc");
                    return;
                }
                while wake.try_recv().is_ok() {}
                while online.try_recv().is_ok() {}
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = wake.recv() => {}
                    _ = online.recv() => {}
                }
                backoff = (backoff * 2).min(RELAY_BACKOFF_CAP);
            }
        });
    }

    /// Step 1 of the escort: push staged bytes until they land (event-driven
    /// backoff), `true` on success. Re-resolves the host each attempt (a
    /// claim can move the chat).
    async fn deliver_attachments(
        &self,
        chat: &str,
        transfers: &[crate::uploads::AttachmentTransfer],
    ) -> bool {
        let mut wake = hearth_sync::wake::subscribe();
        let mut online = hearth_sync::wake::subscribe_online();
        let mut backoff = TRANSFER_BACKOFF_BASE;
        let deadline = tokio::time::Instant::now() + ATTACHMENT_WAIT_MAX;
        loop {
            let Some(target) = self.remote_host_for(chat) else {
                return true; // became locally hosted: bytes already here
            };
            match self.push_attachments(&target, transfers).await {
                Ok(()) => {
                    tracing::info!(chat = %chat, device = %target,
                        count = transfers.len(), "queued attachments delivered");
                    // The bytes beat the drain's next look — kick it via the
                    // durable nudge (the host's UploadCommit already kicked
                    // its local drains too).
                    self.nudge_remote_host(chat);
                    return true;
                }
                Err(TransferError::Permanent(err)) => {
                    tracing::warn!(chat = %chat, device = %target, error = %err,
                        "queued attachment transfer failed permanently");
                    return false;
                }
                Err(TransferError::Transient(err)) => {
                    tracing::warn!(chat = %chat, device = %target, error = %err,
                        backoff_ms = backoff.as_millis() as u64,
                        "queued attachment transfer retrying");
                }
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(chat = %chat, "queued attachment transfer gave up (wait cap)");
                return false;
            }
            while wake.try_recv().is_ok() {}
            while online.try_recv().is_ok() {}
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = wake.recv() => {}
                _ = online.recv() => {}
            }
            backoff = (backoff * 2).min(TRANSFER_BACKOFF_CAP);
        }
    }

    /// Every local chat2 batch acked while connected — our rows are ON the
    /// edge, and the host's own room connection will deliver them. (A room
    /// that hasn't joined keeps pre-join updates in a local buffer, so
    /// `connected` is load-bearing here, not just the empty queue.)
    fn rows_flushed(&self, chat_id: &str) -> bool {
        let handle = lock(&self.inner.handles).get(chat_id).cloned();
        handle
            .and_then(|h| lock(&h.chat2).as_ref().map(|c| c.stats()))
            .is_some_and(|s| s.connected && s.pending_pushes == 0)
    }

    /// One relay attempt: version-gate the host, then forward the entry over
    /// the peer link. Timeouts mark the link suspect (drop + redial next
    /// attempt); a host-side refusal is permanent for THIS attempt but the
    /// escort keeps retrying until the give-up cap (the refusal may be
    /// "attachments not landed yet").
    async fn relay_command(
        &self,
        target: &str,
        chat_id: &str,
        entry: &SessionCommandEntry,
    ) -> Result<&'static str, String> {
        let supported = self
            .workspace()
            .and_then(|ws| ws.read_devices().ok())
            .into_iter()
            .flatten()
            .find(|d| d.id == target)
            .and_then(|d| d.version.as_deref().and_then(hearth_proto::version_triple))
            .is_some_and(|v| v >= RELAY_MIN_VERSION);
        if !supported {
            return Err("host does not support relay delivery (version gate)".into());
        }
        let links = self
            .inner
            .links
            .get()
            .ok_or_else(|| "peer links not wired".to_string())?;
        let client = links
            .client(target)
            .await
            .map_err(|e| format!("peer link: {e}"))?;
        let params = serde_json::json!({ "chatId": chat_id, "entry": entry });
        let call = client.call(hearth_rpc::methods::RELAY_COMMAND, params);
        match tokio::time::timeout(RELAY_CALL_TIMEOUT, call).await {
            Err(_) => {
                links.invalidate(target);
                Err("relay call timed out; peer link suspect".into())
            }
            Ok(Err(hearth_rpc::RpcError::Failed(err))) => Err(format!("host refused: {err}")),
            Ok(Err(err)) => {
                links.invalidate(target);
                Err(format!("relay call failed: {err}"))
            }
            Ok(Ok(reply)) => Ok(match reply.get("outcome").and_then(|v| v.as_str()) {
                Some("executed") => "executed",
                Some("duplicate") => "duplicate",
                Some("expired") => "expired",
                Some("superseded") => "superseded",
                _ => "accepted",
            }),
        }
    }

    /// User-driven retry (the failed-send affordance): re-kick every
    /// delivery road for a chat whose queued sends haven't been adopted —
    /// fresh chat2 socket (a zombie room is the usual suspect), host nudge,
    /// a local drain pass, and a fresh delivery escort per still-pending
    /// command with its attachment transfers re-derived from the entries'
    /// `pending://` refs (idempotent: re-pushing landed bytes re-commits the
    /// same file; the processed ledger keeps execution exactly-once).
    pub fn retry_delivery(&self, chat_id: &str) -> Result<(), EngineError> {
        let handle = self.open(chat_id)?;
        if let Some(chat2) = lock(&handle.chat2).as_ref() {
            chat2.redial();
        }
        self.nudge_remote_host(chat_id);
        let commands = handle.doc.read_commands()?;
        let pending: Vec<SessionCommandEntry> = commands
            .iter()
            .filter(|c| {
                c.status == SessionCommandStatus::Pending
                    && !self.inner.store.is_processed(&c.id).unwrap_or(false)
            })
            .cloned()
            .collect();
        for entry in pending {
            let transfers = command_transfers(&entry);
            self.spawn_command_delivery(chat_id, entry, transfers);
        }
        // Dead attempts: a Run/Steer whose user message never landed and
        // whose command can never execute again — Rejected (execute failed,
        // or the dead-command sweep terminalized it), or consumed by the
        // ledger with no outcome and not currently executing (crash between
        // mark and resolve). Exactly-once is per command ID, so a
        // user-driven retry mints a FRESH attempt: new id, same payload and
        // message id (the executor's user-entry pre-write dedupes by message
        // id). One re-issue per message — the LATEST attempt speaks for it.
        let messages = handle.doc.read_entries().unwrap_or_default();
        let message_landed = |mid: &str| messages.iter().any(|m| m.id == mid);
        let mut latest_dead: HashMap<String, &SessionCommandEntry> = HashMap::new();
        for c in &commands {
            let Some(mid) = (match &c.payload {
                SessionCommandPayload::Run { message_id, .. } => Some(message_id.as_str()),
                SessionCommandPayload::Steer { message_id, .. } => message_id.as_deref(),
                _ => None,
            }) else {
                continue;
            };
            if message_landed(mid) {
                continue;
            }
            let dead = match c.status {
                // Rejected: execute failed or the sweep terminalized a crash
                // window. Expired: the entry outlived its TTL undelivered —
                // an explicit user retry is exactly the consent to re-send.
                SessionCommandStatus::Rejected | SessionCommandStatus::Expired => true,
                SessionCommandStatus::Pending => {
                    self.inner.store.is_processed(&c.id).unwrap_or(false)
                        && !lock(&self.inner.executing).contains(&c.id)
                }
                _ => false,
            };
            if !dead {
                continue;
            }
            // A LIVE pending attempt for the same message (queued or being
            // escorted above) makes a re-issue a duplicate — skip.
            let live_attempt = commands.iter().any(|o| {
                o.id != c.id
                    && o.status == SessionCommandStatus::Pending
                    && !self.inner.store.is_processed(&o.id).unwrap_or(false)
                    && match (&o.payload, &c.payload) {
                        (
                            SessionCommandPayload::Run { message_id: a, .. },
                            SessionCommandPayload::Run { message_id: b, .. },
                        ) => a == b,
                        (
                            SessionCommandPayload::Steer { message_id: a, .. },
                            SessionCommandPayload::Steer { message_id: b, .. },
                        ) => a == b,
                        _ => false,
                    }
            });
            if live_attempt {
                continue;
            }
            match latest_dead.entry(mid.to_string()) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    if c.issued_at > slot.get().issued_at {
                        slot.insert(c);
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(c);
                }
            }
        }
        for old in latest_dead.values() {
            if old.status == SessionCommandStatus::Pending {
                // Terminalize the consumed-but-dead original so the doc tells
                // the truth and the next retry pass doesn't see it again.
                self.resolve_command(
                    &handle,
                    &old.id,
                    SessionCommandStatus::Rejected,
                    Some("interrupted before completion — superseded by retry"),
                );
            }
            let now = now_ms();
            let reissue = SessionCommandEntry {
                id: new_id(),
                payload: old.payload.clone(),
                issued_by: self.inner.config.device_id.clone(),
                issued_at: now,
                based_on: messages.last().map(|m| CommandBasedOn {
                    turn_id: Some(m.id.clone()),
                    frontier: None,
                }),
                expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
                status: SessionCommandStatus::Pending,
                resolution: None,
            };
            tracing::info!(chat = %chat_id, old = %old.id, new = %reissue.id,
                "retry re-issues a dead send attempt");
            handle.doc.queue_command(&reissue)?;
            let transfers = command_transfers(&reissue);
            self.spawn_command_delivery(chat_id, reissue, transfers);
        }
        // Locally-hosted (or already-synced) commands: a drain pass is the
        // whole retry.
        if tokio::runtime::Handle::try_current().is_ok() {
            let host = self.clone();
            let handle = handle.clone();
            self.spawn_worker(async move { host.drain_commands(&handle).await });
        }
        Ok(())
    }

    /// Host side of [`hearth_rpc::methods::RELAY_COMMAND`]: evaluate the
    /// forwarded entry against OUR doc (dedupe/TTL/supersede rules apply
    /// unchanged), claim its client-minted id in the processed ledger, then
    /// execute. The claim is what makes the doc row arriving later — over
    /// chat2 sync — a no-op in the drain: exactly-once across both roads.
    pub async fn ingest_relayed_command(
        &self,
        chat_id: &str,
        entry: SessionCommandEntry,
    ) -> Result<&'static str, EngineError> {
        let handle = self.open(chat_id)?;
        // The sender sequences attachment transfers BEFORE the relay; refuse
        // (retryably) rather than run without the images.
        if !self.missing_attachments(&entry).is_empty() {
            return Err(EngineError::Other("attachments not landed yet".into()));
        }
        let sessions = self
            .sessions()
            .ok_or_else(|| EngineError::Other("executor unavailable".into()))?;
        let commands = handle.doc.read_commands()?;
        let messages = handle.doc.read_entries().unwrap_or_default();
        let current_turn_id = messages.last().map(|m| m.id.clone());
        let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
        let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
        let disposition = evaluate_command(
            &entry,
            &EvaluationContext {
                is_processed: &is_processed,
                now_ms: now_ms(),
                entries: &commands,
                current_turn_id: current_turn_id.as_deref(),
                turn_is_past: &turn_is_past,
            },
        );
        if matches!(disposition, CommandDisposition::Skip) {
            return Ok("duplicate");
        }
        // In-flight claim first (the drain's dead-command sweep must see this
        // id as alive, not crashed, while the execute below runs).
        if !lock(&self.inner.executing).insert(entry.id.clone()) {
            return Ok("duplicate");
        }
        // Claim BEFORE executing (the drain's own mark-before-execute rule).
        let marked = self.inner.store.mark_processed(&entry.id);
        let result = match marked {
            Err(err) => Err(err.into()),
            Ok(false) => Ok("duplicate"),
            Ok(true) => match disposition {
                CommandDisposition::Skip => Ok("duplicate"),
                CommandDisposition::Expired => Ok("expired"),
                CommandDisposition::Superseded => Ok("superseded"),
                CommandDisposition::Execute => match self.execute(&sessions, &handle, &entry).await
                {
                    Ok(_) => Ok("executed"),
                    Err(err) => Err(err),
                },
            },
        };
        lock(&self.inner.executing).remove(&entry.id);
        result
    }

    /// One transfer attempt: chunked `UploadChunk` + `UploadCommit` straight
    /// over the peer link (same wire the UI's legacy path used, so old and
    /// new engines interoperate). Timeouts mark the link suspect —
    /// `invalidate` drops the cached socket so the retry dials fresh instead
    /// of feeding a zombie pipe forever (2026-08-19 incident).
    async fn push_attachments(
        &self,
        target: &str,
        transfers: &[crate::uploads::AttachmentTransfer],
    ) -> Result<(), TransferError> {
        use TransferError::{Permanent, Transient};
        let Some(links) = self.inner.links.get() else {
            return Err(Permanent("peer links not wired".into()));
        };
        let Some(uploads) = self.inner.uploads.get() else {
            return Err(Permanent("uploads not wired".into()));
        };
        let client = links
            .client(target)
            .await
            .map_err(|e| Transient(format!("peer link: {e}")))?;
        for transfer in transfers {
            // Bytes come from the uploads jail only — a transfer names an
            // upload identity, never an arbitrary path.
            let source = uploads.pending_target(&transfer.upload_id, &transfer.file_name);
            let bytes = tokio::fs::read(&source)
                .await
                .map_err(|e| Permanent(format!("staged attachment missing: {e}")))?;
            // Progress entry for the sender's thumbnail ring, updated per
            // landed chunk. The guard retires it on EVERY exit — commit,
            // timeout, refusal — so a dead attempt falls back to the
            // indeterminate spinner and the retry re-publishes from 0.
            let total = bytes.len() as u64;
            self.transfer_progress_set(&transfer.upload_id, &transfer.file_name, 0, total);
            let _progress = TransferProgressGuard {
                host: self,
                upload_id: &transfer.upload_id,
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let mut start = 0usize;
            let mut seq = 0u64;
            loop {
                let end = (start + TRANSFER_CHUNK_B64).min(b64.len());
                let params = serde_json::json!({
                    "uploadId": transfer.upload_id, "seq": seq, "data": &b64[start..end],
                });
                let call = client.call(hearth_rpc::methods::UPLOAD_CHUNK, params);
                match tokio::time::timeout(TRANSFER_CHUNK_TIMEOUT, call).await {
                    Err(_) => {
                        links.invalidate(target);
                        return Err(Transient("chunk push timed out; peer link suspect".into()));
                    }
                    Ok(Err(hearth_rpc::RpcError::Failed(err))) => {
                        return Err(Permanent(format!("host refused chunk: {err}")));
                    }
                    Ok(Err(err)) => {
                        links.invalidate(target);
                        return Err(Transient(format!("chunk push failed: {err}")));
                    }
                    Ok(Ok(_)) => {}
                }
                start = end;
                seq += 1;
                // b64 → raw: 4 chars carry 3 bytes; min-clamp absorbs the
                // final chunk's padding overshoot.
                let done = ((start as u64) * 3 / 4).min(total);
                self.transfer_progress_set(&transfer.upload_id, &transfer.file_name, done, total);
                if start >= b64.len() {
                    break;
                }
            }
            let params = serde_json::json!({
                "uploadId": transfer.upload_id, "fileName": transfer.file_name,
            });
            let call = client.call(hearth_rpc::methods::UPLOAD_COMMIT, params);
            match tokio::time::timeout(TRANSFER_COMMIT_TIMEOUT, call).await {
                Err(_) => {
                    links.invalidate(target);
                    return Err(Transient("commit timed out; peer link suspect".into()));
                }
                Ok(Err(hearth_rpc::RpcError::Failed(err))) => {
                    return Err(Permanent(format!("host refused commit: {err}")));
                }
                Ok(Err(err)) => {
                    links.invalidate(target);
                    return Err(Transient(format!("commit failed: {err}")));
                }
                Ok(Ok(_)) => {}
            }
        }
        Ok(())
    }
}
impl DocHost {
    /// Drain pending commands (host-only): evaluate → mark processed BEFORE execute →
    /// execute → write the outcome as the sole outcome writer.
    pub async fn drain_commands(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.sessions() else {
            return; // executor not wired yet (or retired); the set_sessions kick re-drains
        };
        if !self.is_host(&handle.chat_id) {
            return;
        }
        // Entries this pass decided to leave alone (processed dedupe hits).
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            let commands = match handle.doc.read_commands() {
                Ok(commands) => commands,
                Err(err) => {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "command read failed");
                    return;
                }
            };
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            // Dead-command sweep: Pending in the doc, consumed by the ledger,
            // and NOT mid-execution in this process — the crash window
            // between mark-processed and the outcome write. Left alone it is
            // a send no drain or retry can ever reach ("Sending…" forever,
            // 2026-08-19); terminalize it so the truth lands in the doc and
            // a user retry can mint a fresh attempt.
            for c in &commands {
                if c.status == SessionCommandStatus::Pending
                    && !skipped.contains(&c.id)
                    && is_processed(&c.id)
                    && !lock(&self.inner.executing).contains(&c.id)
                {
                    tracing::warn!(chat = %handle.chat_id, command = %c.id,
                        "command consumed but never resolved (crash mid-execute?); rejecting");
                    self.resolve_command(
                        handle,
                        &c.id,
                        SessionCommandStatus::Rejected,
                        Some("interrupted before completion — retry to send again"),
                    );
                    skipped.insert(c.id.clone());
                }
            }
            let Some(entry) = commands
                .iter()
                .find(|c| {
                    c.status == SessionCommandStatus::Pending
                        && !skipped.contains(&c.id)
                        && !is_processed(&c.id)
                })
                .cloned()
            else {
                return;
            };
            let messages = handle.doc.read_entries().unwrap_or_default();
            let current_turn_id = messages.last().map(|m| m.id.clone());
            let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
            let disposition = evaluate_command(
                &entry,
                &EvaluationContext {
                    is_processed: &is_processed,
                    now_ms: now_ms(),
                    entries: &commands,
                    current_turn_id: current_turn_id.as_deref(),
                    turn_is_past: &turn_is_past,
                },
            );
            // Queued-attachment gate (BEFORE the processed mark — a deferred
            // command must stay eligible): a Run/Steer naming `pending://`
            // refs whose bytes haven't landed on this device yet waits for
            // the transfer instead of running without its images. The wait is
            // bounded; past it the command fails loudly.
            if matches!(disposition, CommandDisposition::Execute) {
                let missing = self.missing_attachments(&entry);
                if !missing.is_empty() {
                    if now_ms().saturating_sub(entry.issued_at) < ATTACHMENT_WAIT_MAX_MS {
                        tracing::info!(chat = %handle.chat_id, command = %entry.id,
                            missing = missing.len(), "command deferred: attachment bytes in transit");
                        self.arm_attachment_wait(handle);
                        return; // preserve order; UploadCommit / the wait timer re-kick
                    }
                    if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                        tracing::error!(chat = %handle.chat_id, error = %err,
                            "processed-ledger write failed; halting drain");
                        return;
                    }
                    tracing::warn!(chat = %handle.chat_id, command = %entry.id,
                        "command rejected: attachments never arrived");
                    self.resolve_command(
                        handle,
                        &entry.id,
                        SessionCommandStatus::Rejected,
                        Some("attachments never arrived"),
                    );
                    continue;
                }
            }
            // In-flight claim: guards the dead-command sweep (an id in
            // `executing` is alive, not crashed) and serializes racing
            // drains on the same entry.
            if !lock(&self.inner.executing).insert(entry.id.clone()) {
                skipped.insert(entry.id.clone());
                continue;
            }
            // Mark BEFORE executing: a crash mid-execution must never double-run a
            // command whose side effect may already have happened.
            if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                tracing::error!(chat = %handle.chat_id, error = %err, "processed-ledger write failed; halting drain");
                lock(&self.inner.executing).remove(&entry.id);
                return;
            }
            match disposition {
                CommandDisposition::Skip => {
                    skipped.insert(entry.id.clone());
                }
                CommandDisposition::Expired => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Expired, None);
                }
                CommandDisposition::Superseded => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Superseded, None);
                }
                CommandDisposition::Execute => {
                    let (status, resolution) = match self.execute(&sessions, handle, &entry).await {
                        Ok(outcome) => outcome,
                        Err(err) => (SessionCommandStatus::Rejected, Some(err.to_string())),
                    };
                    self.resolve_command(handle, &entry.id, status, resolution.as_deref());
                }
            }
            lock(&self.inner.executing).remove(&entry.id);
        }
    }

    /// The command's `pending://` attachment refs whose bytes are NOT on this
    /// device's disk yet. Empty when the command names none, when everything
    /// has landed, or when no uploads store is wired (tests) — absence of the
    /// subsystem must never wedge a queue.
    fn missing_attachments(&self, entry: &SessionCommandEntry) -> Vec<String> {
        let refs: Vec<String> = match &entry.payload {
            SessionCommandPayload::Run { request, .. } => request
                .attachments
                .iter()
                .filter(|p| crate::uploads::is_pending_ref(p))
                .cloned()
                .collect(),
            SessionCommandPayload::Steer { prompt, .. } => crate::uploads::pending_refs_in(prompt),
            _ => Vec::new(),
        };
        if refs.is_empty() {
            return refs;
        }
        let Some(uploads) = self.inner.uploads.get() else {
            return Vec::new();
        };
        refs.into_iter()
            .filter(|r| uploads.resolve_pending(r).is_none())
            .collect()
    }

    /// Arm (once per chat) the deferred-command re-check loop: while a
    /// pending unprocessed command still waits on attachment bytes, re-drain
    /// on a cadence so the bounded wait actually expires even if every
    /// event-driven kick was missed.
    fn arm_attachment_wait(&self, handle: &Arc<ChatDocHandle>) {
        let chat = handle.chat_id.clone();
        if !lock(&self.inner.drain_waiting).insert(chat.clone()) {
            return;
        }
        let weak = Arc::downgrade(handle);
        let host = self.clone();
        self.spawn_worker(async move {
            loop {
                tokio::time::sleep(ATTACHMENT_WAIT_RECHECK).await;
                let Some(handle) = weak.upgrade() else { break };
                if !host.awaiting_attachments(&handle) {
                    break;
                }
                host.drain_commands(&handle).await;
                let Some(handle) = weak.upgrade() else { break };
                if !host.awaiting_attachments(&handle) {
                    break;
                }
            }
            lock(&host.inner.drain_waiting).remove(&chat);
        });
    }

    /// True while some pending, unprocessed command still waits on bytes.
    fn awaiting_attachments(&self, handle: &Arc<ChatDocHandle>) -> bool {
        let commands = handle.doc.read_commands().unwrap_or_default();
        commands.iter().any(|c| {
            c.status == SessionCommandStatus::Pending
                && !self.inner.store.is_processed(&c.id).unwrap_or(false)
                && !self.missing_attachments(c).is_empty()
        })
    }

    /// Rewrite a request's landed `pending://` refs to this device's absolute
    /// paths — in the attachments list AND the prompt text — so the harness
    /// (and the persisted user entry) see ordinary local files, exactly like
    /// the legacy pre-upload flow produced.
    fn resolve_request_attachments(&self, request: &mut hearth_proto::RunRequest) {
        let Some(uploads) = self.inner.uploads.get() else {
            return;
        };
        for path in request.attachments.iter_mut() {
            if let Some(abs) = uploads.resolve_pending(path) {
                request.prompt = request.prompt.replace(path.as_str(), &abs);
                *path = abs;
            }
        }
    }

    /// [`Self::resolve_request_attachments`] for a bare prompt (Steer).
    fn resolve_prompt_attachments(&self, prompt: &str) -> String {
        let Some(uploads) = self.inner.uploads.get() else {
            return prompt.to_string();
        };
        let mut out = prompt.to_string();
        for r in crate::uploads::pending_refs_in(prompt) {
            if let Some(abs) = uploads.resolve_pending(&r) {
                out = out.replace(&r, &abs);
            }
        }
        out
    }

    /// Host-only outcome write (ledger rule 2).
    fn resolve_command(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) {
        if let Err(err) = handle
            .doc
            .set_command_status(command_id, status, resolution)
        {
            tracing::warn!(
                chat = %handle.chat_id,
                command = %command_id,
                error = %err,
                "command outcome write failed"
            );
        }
    }

    async fn execute(
        &self,
        sessions: &SessionsEngine,
        handle: &Arc<ChatDocHandle>,
        entry: &SessionCommandEntry,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        let chat_id = &handle.chat_id;
        match &entry.payload {
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                let mut request = request.clone();
                // Queued-attachment refs (`pending://`) resolve to this
                // host's absolute paths before anything persists or
                // dispatches — the drain already gated on the bytes being
                // present, so every ref resolves here.
                self.resolve_request_attachments(&mut request);
                // Worktree directive (WorktreeSpec): materialize on THIS host at
                // drain time — the durable command plane replaces the sender's
                // old blocking CreateWorktree relay RPC, whose lost reply wedged
                // the composer on "Sending…" while the run proceeded anyway.
                // `take()` resolves the request before dispatch, so the journal
                // and steer→new-turn fallbacks reuse the created path instead of
                // minting another checkout.
                let fresh_worktree = match request.worktree.take() {
                    Some(spec) => {
                        let (cwd, fresh) = self.materialize_worktree(chat_id, &spec).await?;
                        request.cwd = cwd;
                        fresh
                    }
                    None => None,
                };
                // Claim-on-first-command: a run for a chat with no workspace row
                // creates the row under our device id (we are about to host it).
                if let Some(ws) = self.workspace() {
                    ws.claim_chat(chat_id, Some(&request.cwd))?;
                    // A pre-existing row (the client's createChat raced ahead)
                    // still carries the repo folder — repoint it at the fresh
                    // worktree, and stamp the actual `hearth/<name>` branch so
                    // the footer and the title-rename flow see it.
                    if let Some(wt) = &fresh_worktree {
                        if let Err(err) = ws.set_chat_cwd(chat_id, &wt.path) {
                            tracing::warn!(chat = %chat_id, error = %err, "worktree cwd stamp failed");
                        }
                        if let Err(err) = ws.set_chat_branch(chat_id, &wt.branch) {
                            tracing::warn!(chat = %chat_id, error = %err, "worktree branch stamp failed");
                        }
                    }
                }
                let harness = self.harness_for_request(chat_id, &request);
                // A row with no config renders no harness glyph (and every
                // later dispatch falls back to the engine default), so stamp
                // what this run actually executes with. Claimed rows and
                // catalog-not-loaded createChats both land here; the racing
                // real createChat carries the same picked values.
                if let Some(ws) = self.workspace()
                    && ws.chat_config(chat_id).is_none()
                {
                    let config = hearth_proto::ChatConfig {
                        harness,
                        model: request.model.clone(),
                        mode: request.mode,
                        reasoning: request.reasoning,
                        model_options: request.model_options.clone(),
                        sandbox: request.sandbox,
                    };
                    if let Err(err) = ws.set_chat_config(chat_id, &config) {
                        tracing::warn!(chat = %chat_id, error = %err, "run-config backfill failed");
                    }
                }
                // Timestamp canonicalization: the user message lands in
                // history at the moment the user SENT it (the entry's
                // issued_at, clamped against clock skew) — not whenever this
                // host got around to draining a queued command. Idempotent by
                // id, so the dispatch path's own execution-time write dedupes
                // to a no-op.
                if let Err(err) = handle.write_user_message(
                    message_id,
                    &request.prompt,
                    entry.issued_at.min(now_ms()),
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "canonical user-message write failed");
                }
                sessions
                    .dispatch(chat_id, harness, request, Some(message_id.clone()))
                    .await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                // Same `pending://` → absolute rewrite as the Run arm.
                let prompt = &self.resolve_prompt_attachments(prompt);
                // Same send-time canonicalization as the Run arm.
                if let Some(message_id) = message_id
                    && let Err(err) =
                        handle.write_user_message(message_id, prompt, entry.issued_at.min(now_ms()))
                {
                    tracing::warn!(chat = %chat_id, error = %err, "canonical user-message write failed");
                }
                match sessions.steer(chat_id, prompt, message_id.clone()).await? {
                    SteerOutcome::Accepted => Ok((SessionCommandStatus::Applied, None)),
                    SteerOutcome::NotSteerable => {
                        // No live steerable run: the durable command still delivers —
                        // run it as the next turn (hearth's fallback, executor-side).
                        // After an engine restart `last_request` is empty too, so
                        // rebuild the run config from the chat's workspace row
                        // (hearth derived dispatch config from the chat row the
                        // same way — sessions.ts:601-620); dispatch's engine-owned
                        // resume then reattaches the prior harness conversation.
                        let request = sessions
                            .last_request(chat_id)
                            .or_else(|| self.request_from_chat_row(chat_id, prompt));
                        let Some(mut request) = request else {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("no live run and no prior run config".into()),
                            ));
                        };
                        request.prompt = prompt.clone();
                        request.resume = None; // dispatch re-derives the harness session
                        // A reused config must not re-inline the PREVIOUS
                        // turn's images; this steer's own refs (if any) already
                        // ride the prompt text.
                        request.attachments = Vec::new();
                        let harness = self.harness_for_request(chat_id, &request);
                        sessions
                            .dispatch(chat_id, harness, request, message_id.clone())
                            .await?;
                        Ok((
                            SessionCommandStatus::Applied,
                            Some("queued as new turn".into()),
                        ))
                    }
                }
            }
            SessionCommandPayload::Interrupt {} => {
                sessions.interrupt(chat_id).await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::RespondInput {
                request_id,
                answers,
            } => {
                if sessions.respond_input(chat_id, request_id, answers.clone())? {
                    return Ok((SessionCommandStatus::Applied, None));
                }
                // No live resolver. Only a request id the doc shows as an
                // OPEN question on a SETTLED entry gets the orphan fallback:
                // a mismatched or already-resolved id is a stale/buggy answer
                // and must still reject, and a still-streaming entry's
                // question belongs to the live run (a just-consumed resolver
                // racing a second answer must not spawn a duplicate turn).
                let questions = handle.doc.read_entries().ok().and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .filter(|e| e.status != Some(MessageStatus::Streaming))
                        .find_map(|e| {
                            e.parts.iter().find_map(|p| match p {
                                MessagePart::Input {
                                    request_id: rid,
                                    questions,
                                    resolved: false,
                                    ..
                                } if rid == request_id => Some(questions.clone()),
                                _ => None,
                            })
                        })
                });
                let Some(questions) = questions else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request".into()),
                    ));
                };
                // The run died under the question (engine restart, crash).
                // The question is still open in the doc and the command is
                // durable, so honor it anyway — stamp the part resolved and
                // deliver the answers as the next (resumed) turn, the same
                // fallback a dead-run steer takes. The question UI stays up
                // until the user answers (user requirement); this is what
                // makes that answer still WORK.
                let request = sessions
                    .last_request(chat_id)
                    .or_else(|| self.request_from_chat_row(chat_id, ""));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request and no prior run config".into()),
                    ));
                };
                request.prompt = respond_input_prompt(&questions, answers);
                request.resume = None; // dispatch re-derives the harness session
                request.attachments = Vec::new();
                if let Err(err) = handle.doc.resolve_input(request_id) {
                    tracing::warn!(chat = %chat_id, request = %request_id, error = %err,
                        "orphaned input resolve failed");
                }
                let harness = self.harness_for_request(chat_id, &request);
                sessions.dispatch(chat_id, harness, request, None).await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some("answered as new turn".into()),
                ))
            }
        }
    }

    /// Create (or reuse) the isolated worktree a Run's [`hearth_proto::WorktreeSpec`]
    /// asks for, returning the resolved cwd plus the fresh worktree when one was
    /// actually created. Reuse guard: a chat whose row already points inside a
    /// linked worktree of the same repo keeps it — a duplicate Run (client retry
    /// after a lost ack, ledger reset) must not mint a second checkout.
    async fn materialize_worktree(
        &self,
        chat_id: &str,
        spec: &hearth_proto::WorktreeSpec,
    ) -> Result<(String, Option<hearth_proto::Worktree>), EngineError> {
        if let Some(ws) = self.workspace()
            && let Ok(Some(chat)) = ws.chat(chat_id)
            && let Some(cwd) = chat.cwd
            && cwd != spec.repo_path
            && crate::workspace_host::linked_worktree_root(std::path::Path::new(&cwd)).as_deref()
                == Some(spec.repo_path.as_str())
        {
            tracing::info!(chat = %chat_id, cwd = %cwd, "worktree spec: reusing the chat's existing worktree");
            return Ok((cwd, None));
        }
        let repos = self
            .inner
            .repos
            .get()
            .ok_or_else(|| EngineError::Other("repos engine not wired".into()))?;
        let worktree = repos
            .create_worktree(std::path::Path::new(&spec.repo_path), &spec.base)
            .await?;
        tracing::info!(
            chat = %chat_id,
            path = %worktree.path,
            branch = %worktree.branch,
            "worktree materialized for run"
        );
        Ok((worktree.path.clone(), Some(worktree)))
    }
}

/// The resumed-turn prompt for answers to a question whose run died: each
/// answer paired with its question text so the reattached conversation reads
/// naturally. Pure.
fn respond_input_prompt(questions: &[UserInputQuestion], answers: &[UserInputAnswer]) -> String {
    let mut lines = vec!["Answering your earlier question:".to_string()];
    for answer in answers {
        let picked = answer.labels.join(", ");
        let question = questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .map(|q| q.question.trim())
            .filter(|q| !q.is_empty());
        match question {
            Some(question) => lines.push(format!("{question} — {picked}")),
            None => lines.push(picked),
        }
    }
    lines.join("\n")
}
#[cfg(test)]
mod transfer_progress_tests {
    use super::{DocHost, DocHostConfig, TransferProgressGuard};
    use std::sync::Arc;

    fn host() -> (tempfile::TempDir, DocHost) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(hearth_sync::DocsStore::open(dir.path()).expect("store opens"));
        let host = DocHost::new(
            store,
            DocHostConfig {
                device_id: "dev-test".into(),
                default_harness: hearth_proto::HarnessId::Mock,
                edge: None,
            },
        );
        (dir, host)
    }

    #[test]
    fn set_upserts_by_upload_id_and_clear_retires_only_its_entry() {
        let (_dir, host) = host();
        let rx = host.watch_transfers();
        assert!(rx.borrow().is_empty());

        host.transfer_progress_set("u1", "a.png", 0, 1_000);
        host.transfer_progress_set("u2", "b.png", 0, 400);
        host.transfer_progress_set("u1", "a.png", 300, 1_000);
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.len(), 2, "upsert must not duplicate u1");
        let u1 = snapshot.iter().find(|t| t.upload_id == "u1").unwrap();
        assert_eq!((u1.done, u1.total), (300, 1_000));

        host.transfer_progress_clear("u1");
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.len(), 1, "u2 must survive u1's retirement");
        assert_eq!(snapshot[0].upload_id, "u2");
    }

    #[test]
    fn guard_retires_the_entry_on_every_exit_path() {
        let (_dir, host) = host();
        let rx = host.watch_transfers();
        host.transfer_progress_set("u1", "a.png", 0, 9);
        {
            let _guard = TransferProgressGuard {
                host: &host,
                upload_id: "u1",
            };
            assert_eq!(rx.borrow().len(), 1);
            // An early return / error propagation drops the guard here.
        }
        assert!(
            rx.borrow().is_empty(),
            "a failed attempt must not leave a phantom ring behind"
        );
    }

    #[test]
    fn late_subscriber_sees_current_set_first() {
        let (_dir, host) = host();
        host.transfer_progress_set("u1", "a.png", 750, 1_000);
        // watch_stream's contract: current value first, then changes — a UI
        // attaching mid-transfer must render the ring immediately.
        let rx = host.watch_transfers();
        assert_eq!(rx.borrow().len(), 1);
        assert_eq!(rx.borrow()[0].done, 750);
    }
}
