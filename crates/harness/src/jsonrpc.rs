//! Minimal JSON-RPC 2.0 client over a child agent's stdio (newline-delimited
//! frames, id-multiplexed), ported from codex.ts's `startAppServer`. Shared by
//! the Codex app-server harness and the ACP harness — both protocols are
//! newline-framed JSON-RPC 2.0 over stdio.
//!
//! - Responses are matched to callers by numeric id (a shared pending map the
//!   reader task resolves directly, so requests can be awaited from anywhere —
//!   including inside the session loop — without starving notifications).
//! - Notifications and server→client requests (approvals) are pumped into an
//!   [`Incoming`] channel the session loop drains.
//! - Writes to a dead child's stdin (EPIPE) are tolerated and logged, matching
//!   the TS harness's swallowed-EPIPE behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use crate::HarnessError;

/// Generous bound on a single request/response round trip: app-server
/// handshakes and thread resumes can be slow, but a reply that never comes is
/// a wedged process, and awaiting it forever strands the session Working.
pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `HEARTH_HARNESS_RPC_TIMEOUT_MS` overrides [`DEFAULT_REQUEST_TIMEOUT`]; `0`
/// disables the bound. Read per client so a discovery probe and a session can
/// differ if the environment changes mid-process (tests).
pub(crate) fn default_request_timeout() -> Option<Duration> {
    match std::env::var("HEARTH_HARNESS_RPC_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(DEFAULT_REQUEST_TIMEOUT),
    }
}

/// Decrements the shared in-flight count on every exit path (response, error,
/// timeout, dropped sender).
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A non-response line from the app server, in stdout order.
#[derive(Debug)]
pub(crate) enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Server→client request (approvals); must be answered via
    /// [`RpcClient::respond`] / [`RpcClient::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// stdout EOF: the app server exited. All pending requests fail.
    Eof,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
pub(crate) struct RpcClient {
    next_id: Arc<AtomicI64>,
    pending: Pending,
    writer: mpsc::UnboundedSender<String>,
    /// Per-request bound; `None` leaves a request unbounded (the ACP
    /// `session/prompt` turn, whose own stall watchdog owns its lifetime).
    request_timeout: Option<Duration>,
    /// Requests sent and not yet resolved/removed, shared with the owning
    /// harness so it can answer "is a response still outstanding?".
    in_flight: Arc<AtomicUsize>,
}

impl RpcClient {
    /// Spawn the writer + reader tasks over the child's stdio; returns the
    /// client and the incoming (notification/request) channel.
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<Incoming>) {
        Self::new_shared(stdin, stdout, default_request_timeout(), Arc::default())
    }

    /// [`Self::new`] with an explicit timeout and in-flight counter owner.
    pub fn new_shared(
        stdin: ChildStdin,
        stdout: ChildStdout,
        request_timeout: Option<Duration>,
        in_flight: Arc<AtomicUsize>,
    ) -> (Self, mpsc::Receiver<Incoming>) {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(write_loop(stdin, writer_rx));
        let pending: Pending = Arc::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), incoming_tx));
        (
            Self {
                next_id: Arc::new(AtomicI64::new(0)),
                pending,
                writer: writer_tx,
                request_timeout,
                in_flight,
            },
            incoming_rx,
        )
    }

    /// Send a request and await its response (resolved by the reader task).
    /// Bounded by [`Self::request_timeout`]: a wedged app server surfaces as a
    /// protocol error (which tears the session down) instead of a hang.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.request_bounded(method, params, self.request_timeout)
            .await
    }

    /// Send a request whose response is a whole agent TURN (ACP
    /// `session/prompt`) and await it without a fixed bound: the ACP session
    /// loop's stall/quiet-settle watchdogs own that turn's lifetime, and a
    /// blanket request timeout would abort legitimate long turns.
    pub async fn request_untimed(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, HarnessError> {
        self.request_bounded(method, params, None).await
    }

    async fn request_bounded(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, HarnessError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.writer.send(line.to_string()).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(HarnessError::Protocol(format!(
                "{method}: app-server stdin closed"
            )));
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let _in_flight = InFlight(Arc::clone(&self.in_flight));
        let response = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, rx).await {
                Ok(response) => response,
                Err(_) => {
                    // Withdraw the slot so a late reply is ignored, and fail
                    // the request: callers turn this into a Done{Errored} and
                    // reap the child.
                    self.pending.lock().expect("pending lock").remove(&id);
                    return Err(HarnessError::Protocol(format!(
                        "{method}: no response within {}s — the app server is unresponsive",
                        timeout.as_secs()
                    )));
                }
            },
            None => rx.await,
        };
        match response {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(HarnessError::Protocol(format!("{method}: {message}"))),
            // Sender dropped: the reader hit EOF and failed all pending.
            Err(_) => Err(HarnessError::Protocol(format!(
                "{method}: app-server exited before responding"
            ))),
        }
    }

    /// Fire a notification (no id, no response).
    pub fn notify(&self, method: &str, params: Option<Value>) {
        let line = match params {
            Some(params) => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            None => json!({ "jsonrpc": "2.0", "method": method }),
        };
        let _ = self.writer.send(line.to_string());
    }

    /// Answer a server→client request.
    pub fn respond(&self, id: &Value, result: Value) {
        let line = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.writer.send(line.to_string());
    }

    /// Reject a server→client request (e.g. unknown method).
    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        let _ = self.writer.send(line.to_string());
    }
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged.
async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            tracing::debug!(target: "hearth_harness::rpc", "stdin write failed (tolerated): {e}");
            return;
        }
    }
}

/// The id of a response, tolerantly. hearth always sends numeric ids, but
/// JSON-RPC lets a server echo them re-encoded — a string `"5"` or float
/// `5.0` still names request 5. Dropping such a response would strand its
/// caller forever (the session would spin Working with no per-turn timeout).
fn response_id(id: &Value) -> Option<i64> {
    if let Some(n) = id.as_i64() {
        return Some(n);
    }
    if let Some(s) = id.as_str() {
        return s.parse().ok();
    }
    id.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)
}

/// Parse stdout lines: responses resolve the pending map, everything else is
/// forwarded in order. Non-JSON noise is skipped; on EOF all pending requests
/// fail (their senders drop) and one final [`Incoming::Eof`] is delivered.
async fn read_loop(stdout: ChildStdout, pending: Pending, tx: mpsc::Sender<Incoming>) {
    let mut lines = BufReader::new(stdout).lines();
    // A read error ends the loop like EOF: either way the child's stdout is
    // unusable, pending requests must fail, and the session loop must know.
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(target: "hearth_harness::rpc", "non-JSON stdout line (skipped)");
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");
        match (method, id) {
            // Response: resolve the awaiting request.
            (None, Some(id)) => {
                let Some(id) = response_id(id) else { continue };
                let Some(sender) = pending.lock().expect("pending lock").remove(&id) else {
                    continue;
                };
                let outcome = match msg.get("error") {
                    Some(err) => Err(err
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| err.to_string())),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            // Server→client request (approvals).
            (Some(method), Some(id)) => {
                let incoming = Incoming::Request {
                    id: id.clone(),
                    method: method.to_owned(),
                    params: msg.get("params").cloned().unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            // Notification.
            (Some(method), None) => {
                let incoming = Incoming::Notification {
                    method: method.to_owned(),
                    params: msg.get("params").cloned().unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            (None, None) => {}
        }
    }
    // EOF/read error: fail every awaiting request, then signal the loop.
    pending.lock().expect("pending lock").clear();
    let _ = tx.send(Incoming::Eof).await;
}
