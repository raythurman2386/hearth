//! Tailnet hub — HTTP + WebSocket mux that replaces the Cloudflare Worker + DOs.
//!
//! Binds one TCP port (typically on the always-on host). Paths match the old
//! edge so existing clients can keep their URL shape:
//!
//! - `WS  /chat2/{id}/ws`
//! - `GET/POST /chat2/{id}/checkpoint`
//! - `GET/POST /chat2/{id}/rows`
//! - `WS  /registry/{org}/ws`
//! - `WS  /rpc` — handed to the engine via [`HubConfig::on_rpc`]
//! - `GET /health`
//! - `GET /releases/*` — static files from `{data_dir}/releases/`
//!
//! Auth is the tailnet: loopback is allowed (tests); other peers are checked
//! with `tailscale whois`. No `?token=` required.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::chat_room::{
    AppendOutcome, ChatRooms, CheckpointOutcome, checkpoint_too_large, is_safe_id,
};
use crate::registry_room::RegistryRooms;
use crate::tailnet::whois;
use crate::types::SyncError;

const WS_MAGIC: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Hard cap before buffering an HTTP body (matches chat checkpoint max).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);

/// Called with an already-handshaken WebSocket for `/rpc`. The engine pumps
/// this into [`hearth_rpc::serve_connection`].
pub type RpcHandler = Arc<dyn Fn(WebSocketStream<TcpStream>) + Send + Sync>;

pub struct HubConfig {
    pub data_dir: PathBuf,
    /// When false, only `/rpc` (and `/health`) are served — spoke devices.
    pub serve_rooms: bool,
    pub on_rpc: Option<RpcHandler>,
    /// Skip `tailscale whois` (loopback tests). Production leaves this false.
    pub skip_whois: bool,
}

pub struct Hub {
    listener: TcpListener,
    addr: SocketAddr,
    chats: Arc<ChatRooms>,
    registries: Arc<RegistryRooms>,
    config: HubConfig,
}

impl Hub {
    pub async fn bind(
        addr: impl tokio::net::ToSocketAddrs,
        config: HubConfig,
    ) -> Result<Self, SyncError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| SyncError::Tailnet(format!("hub bind: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| SyncError::Tailnet(format!("hub local_addr: {e}")))?;
        let rooms_root = config.data_dir.join("tailnet-rooms");
        Ok(Self {
            listener,
            addr,
            chats: Arc::new(ChatRooms::new(&rooms_root)),
            registries: Arc::new(RegistryRooms::new(&rooms_root)),
            config,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.listener.accept().await {
                    Ok((stream, peer)) => {
                        let ctx = HubContext::from_hub(&self);
                        tokio::spawn(async move {
                            if let Err(err) = handle_conn(stream, peer, ctx).await {
                                // Whois/auth failures reset the TCP socket from the
                                // client's POV — keep them visible at warn.
                                let msg = err.to_string();
                                if msg.contains("whois") || msg.contains("parse whois") {
                                    tracing::warn!(error = %err, %peer, "hub: peer auth failed");
                                } else {
                                    tracing::debug!(error = %err, %peer, "hub: connection ended");
                                }
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "hub: accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        })
    }
}

/// Per-connection state shared from the accept loop into [`handle_conn`].
#[derive(Clone)]
struct HubContext {
    chats: Arc<ChatRooms>,
    registries: Arc<RegistryRooms>,
    on_rpc: Option<RpcHandler>,
    serve_rooms: bool,
    skip_whois: bool,
    releases: PathBuf,
}

impl HubContext {
    /// Capture the shared state from a bound [`Hub`].
    fn from_hub(hub: &Hub) -> Self {
        Self {
            chats: hub.chats.clone(),
            registries: hub.registries.clone(),
            on_rpc: hub.config.on_rpc.clone(),
            serve_rooms: hub.config.serve_rooms,
            skip_whois: hub.config.skip_whois,
            releases: hub.config.data_dir.join("releases"),
        }
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    peer: SocketAddr,
    ctx: HubContext,
) -> Result<(), SyncError> {
    if !ctx.skip_whois && !peer.ip().is_loopback() {
        whois(&peer.ip().to_string()).await?;
    }

    let req = read_request(&mut stream).await?;
    let path = req.path.clone();
    let upgrade = req
        .headers
        .get("upgrade")
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if upgrade {
        // Match localhost IPC: browsers always send Origin. Reject so a page
        // the user visits cannot open hub WebSockets (especially `/rpc`).
        if req.headers.contains_key("origin") {
            let _ = write_http(
                &mut stream,
                403,
                "text/plain",
                b"origin not allowed on hub websocket",
                &[],
            )
            .await;
            return Err(SyncError::Tailnet(
                "rejecting websocket carrying an Origin header".into(),
            ));
        }
        complete_ws_handshake(&mut stream, &req).await?;
        let ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
        if let Some(chat_id) = strip_prefix_suffix(&path, "/chat2/", "/ws") {
            if !ctx.serve_rooms {
                return Err(SyncError::Tailnet("this peer does not host rooms".into()));
            }
            let room = ctx.chats.get(chat_id)?;
            room.serve_ws(ws).await;
            return Ok(());
        }
        if let Some(org) = strip_prefix_suffix(&path, "/registry/", "/ws") {
            if !ctx.serve_rooms {
                return Err(SyncError::Tailnet("this peer does not host rooms".into()));
            }
            let room = ctx.registries.get(org)?;
            room.serve_ws(ws).await;
            return Ok(());
        }
        if path == "/registry/ws" {
            if !ctx.serve_rooms {
                return Err(SyncError::Tailnet("this peer does not host rooms".into()));
            }
            let room = ctx.registries.get("tailnet")?;
            room.serve_ws(ws).await;
            return Ok(());
        }
        if path == "/rpc" || path == "/rpc/" {
            if let Some(on_rpc) = ctx.on_rpc {
                on_rpc(ws);
                return Ok(());
            }
            return Err(SyncError::Tailnet("rpc not served on this peer".into()));
        }
        return Err(SyncError::Tailnet(format!("unknown ws path {path}")));
    }

    dispatch_http(&mut stream, &req, &ctx).await
}

struct HttpReq {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<HttpReq, SyncError> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = tokio::time::timeout(HEADER_TIMEOUT, stream.read(&mut tmp))
            .await
            .map_err(|_| SyncError::Tailnet("header read timeout".into()))?
            .map_err(|e| SyncError::Tailnet(format!("header read: {e}")))?;
        if n == 0 {
            return Err(SyncError::Tailnet("empty request".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(SyncError::Tailnet("headers too large".into()));
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]);
    let leftover = buf[header_end + 4..].to_vec();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/");
    let (path_raw, query_raw) = target.split_once('?').unwrap_or((target, ""));
    let path = path_raw.to_string();
    let query = parse_query(query_raw);
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_len = headers
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    if content_len > MAX_BODY_BYTES {
        return Err(SyncError::Tailnet(format!(
            "body too large ({content_len} > {MAX_BODY_BYTES})"
        )));
    }
    let mut body = leftover;
    while body.len() < content_len {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| SyncError::Tailnet(format!("body read: {e}")))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);
    Ok(HttpReq {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn complete_ws_handshake(stream: &mut TcpStream, req: &HttpReq) -> Result<(), SyncError> {
    let key = req
        .headers
        .get("sec-websocket-key")
        .ok_or_else(|| SyncError::Tailnet("missing sec-websocket-key".into()))?;
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_MAGIC);
    let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\
         \r\n"
    );
    stream
        .write_all(resp.as_bytes())
        .await
        .map_err(|e| SyncError::Tailnet(format!("ws handshake write: {e}")))?;
    Ok(())
}

async fn dispatch_http(
    stream: &mut TcpStream,
    req: &HttpReq,
    ctx: &HubContext,
) -> Result<(), SyncError> {
    if req.path == "/health" && req.method == "GET" {
        return write_http(
            stream,
            200,
            "application/json",
            br#"{"ok":true,"auth":"tailnet"}"#,
            &[],
        )
        .await;
    }

    if let Some(rest) = req.path.strip_prefix("/releases/")
        && req.method == "GET"
    {
        if rest.contains("..") || !is_safe_release_path(rest) {
            return write_http(stream, 400, "text/plain", b"bad path", &[]).await;
        }
        let file = ctx.releases.join(rest);
        match std::fs::read(&file) {
            Ok(bytes) => {
                return write_http(stream, 200, "application/octet-stream", &bytes, &[]).await;
            }
            Err(_) => return write_http(stream, 404, "text/plain", b"not found", &[]).await,
        }
    }

    if !ctx.serve_rooms {
        return write_http(stream, 404, "text/plain", b"not found", &[]).await;
    }

    if let Some(chat_id) = strip_prefix_suffix(&req.path, "/chat2/", "/checkpoint") {
        let room = ctx.chats.get(chat_id)?;
        if req.method == "GET" {
            let Some(bytes) = room.checkpoint_bytes()? else {
                return write_http(
                    stream,
                    404,
                    "application/json",
                    br#"{"error":"not_found"}"#,
                    &[],
                )
                .await;
            };
            let seq = room.checkpoint_seq()?.to_string();
            let extra = [("x-chat2-checkpoint-seq", seq.as_str())];
            if let Some(range) = req.headers.get("range").and_then(|v| parse_range_start(v)) {
                if range >= bytes.len() {
                    let cr = format!("bytes */{}", bytes.len());
                    return write_http(
                        stream,
                        416,
                        "application/octet-stream",
                        b"",
                        &[("content-range", cr.as_str())],
                    )
                    .await;
                }
                let slice = &bytes[range..];
                let cr = format!(
                    "bytes {range}-{}/{}",
                    bytes.len().saturating_sub(1),
                    bytes.len()
                );
                return write_http(
                    stream,
                    206,
                    "application/octet-stream",
                    slice,
                    &[
                        ("x-chat2-checkpoint-seq", seq.as_str()),
                        ("content-range", cr.as_str()),
                        ("accept-ranges", "bytes"),
                    ],
                )
                .await;
            }
            let extra_owned: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, *v)).collect();
            return write_http(
                stream,
                200,
                "application/octet-stream",
                &bytes,
                &extra_owned,
            )
            .await;
        }
        if req.method == "POST" {
            if checkpoint_too_large(req.body.len()) {
                return write_http(
                    stream,
                    413,
                    "application/json",
                    br#"{"error":"too_large"}"#,
                    &[],
                )
                .await;
            }
            let seq_covered = req
                .query
                .get("seqCovered")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let frontier = req
                .headers
                .get("x-chat2-frontier")
                .and_then(|v| decode_b64(v))
                .unwrap_or_default();
            if frontier.is_empty() && seq_covered > 0 {
                return write_http(
                    stream,
                    400,
                    "application/json",
                    br#"{"error":"bad_frontier"}"#,
                    &[],
                )
                .await;
            }
            match room.commit_checkpoint(seq_covered, &frontier, &req.body)? {
                CheckpointOutcome::Ok { seq_floor, pruned } => {
                    let body = format!(r#"{{"ok":true,"seqFloor":{seq_floor},"pruned":{pruned}}}"#);
                    return write_http(stream, 200, "application/json", body.as_bytes(), &[]).await;
                }
                CheckpointOutcome::Empty => {
                    return write_http(
                        stream,
                        400,
                        "application/json",
                        br#"{"error":"empty"}"#,
                        &[],
                    )
                    .await;
                }
                CheckpointOutcome::FloorRegression | CheckpointOutcome::AheadOfHead => {
                    return write_http(
                        stream,
                        409,
                        "application/json",
                        br#"{"error":"conflict"}"#,
                        &[],
                    )
                    .await;
                }
            }
        }
    }

    if let Some(chat_id) = strip_prefix_suffix(&req.path, "/chat2/", "/rows") {
        let room = ctx.chats.get(chat_id)?;
        if req.method == "GET" {
            let after = req
                .query
                .get("after")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let device = req.query.get("device").cloned().unwrap_or_default();
            let exclude = (req.query.get("excludeOwn").map(String::as_str) == Some("1")
                && !device.is_empty())
            .then_some(device.as_str());
            let body = room.encode_pull_body(after, exclude)?;
            return write_http(stream, 200, "application/octet-stream", &body, &[]).await;
        }
        if req.method == "POST" {
            let device = req.query.get("device").cloned().unwrap_or_default();
            let batch_id = req.query.get("batchId").cloned().unwrap_or_default();
            if batch_id.is_empty() || batch_id.len() > 128 {
                return write_http(
                    stream,
                    400,
                    "application/json",
                    br#"{"error":"bad_push"}"#,
                    &[],
                )
                .await;
            }
            if !is_safe_id(&batch_id) {
                return write_http(
                    stream,
                    400,
                    "application/json",
                    br#"{"error":"bad_push"}"#,
                    &[],
                )
                .await;
            }
            match room.append(&device, &batch_id, &req.body)? {
                AppendOutcome::Ok { seq, dup } => {
                    let body = serde_json::json!({
                        "batchId": batch_id,
                        "seq": seq,
                        "dup": dup,
                    })
                    .to_string();
                    return write_http(stream, 200, "application/json", body.as_bytes(), &[]).await;
                }
                AppendOutcome::TooLarge => {
                    return write_http(
                        stream,
                        413,
                        "application/json",
                        br#"{"error":"too_large"}"#,
                        &[],
                    )
                    .await;
                }
                AppendOutcome::Empty => {
                    return write_http(
                        stream,
                        400,
                        "application/json",
                        br#"{"error":"empty"}"#,
                        &[],
                    )
                    .await;
                }
            }
        }
    }

    write_http(
        stream,
        404,
        "application/json",
        br#"{"error":"not found"}"#,
        &[],
    )
    .await
}

fn strip_prefix_suffix<'a>(path: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    rest.strip_suffix(suffix)
        .filter(|id| !id.is_empty() && is_safe_id(id))
}

fn is_safe_release_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
}

fn parse_range_start(value: &str) -> Option<usize> {
    let rest = value.strip_prefix("bytes=")?;
    let start = rest.split('-').next()?;
    start.parse().ok()
}

fn decode_b64(value: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> Result<(), SyncError> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| SyncError::Tailnet(format!("http write: {e}")))?;
    stream
        .write_all(body)
        .await
        .map_err(|e| SyncError::Tailnet(format!("http write: {e}")))?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_frames::{self as wire, frame_type};
    use crate::tailnet::connect_peer;
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    #[tokio::test]
    async fn hub_serves_chat2_ws_and_http_health() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::bind(
            "127.0.0.1:0",
            HubConfig {
                data_dir: dir.path().to_path_buf(),
                serve_rooms: true,
                on_rpc: None,
                skip_whois: true,
            },
        )
        .await
        .unwrap();
        let addr = hub.local_addr();
        let _task = hub.spawn();

        let mut health = tokio::net::TcpStream::connect(addr).await.unwrap();
        health
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        health.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("tailnet"), "{text}");

        let mut client = connect_peer("127.0.0.1", addr.port(), "/chat2/abc/ws")
            .await
            .unwrap();
        let hello = wire::encode(
            frame_type::HELLO,
            &serde_json::json!({"cursor": 0, "device": "d1"}),
            &[],
        );
        client.send(WsMessage::Binary(hello.into())).await.unwrap();
        let reply = client.next().await.unwrap().unwrap();
        let WsMessage::Binary(bytes) = reply else {
            panic!("expected binary state");
        };
        let decoded = wire::decode(&bytes).unwrap();
        assert_eq!(decoded.kind, frame_type::STATE);
        assert_eq!(decoded.header["headSeq"], 0);
    }
}
