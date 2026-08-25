//! Tailscale transport for hearth-sync — the peer-to-peer replacement for the
//! edge/DO endpoint seam (ARCHITECTURE §1, §6).
//!
//! Tailscale gives every device a stable MagicDNS name and a tailnet IP, and
//! the tailnet itself is the trust boundary. So instead of dialing a Cloudflare
//! Durable Object with a `?token=`, peers dial each other directly over the
//! tailnet and authenticate by `tailscale whois` on the peer's IP.
//!
//! This module provides the transport plumbing:
//! - [`discover_peers`] / [`Peer`] — `tailscale status --json` for who's online.
//! - [`whois`] / [`PeerIdentity`] — `tailscale whois` for peer identity/auth.
//! - [`PeerServer`] — a WS server bound to the tailnet interface.
//! - [`connect_peer`] — dial a peer's tailnet address.
//!
//! The room protocols (chat2 binary frames, registry JSON text) are unchanged;
//! they ride these WS streams exactly as they rode the DO sockets.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::types::SyncError;

/// A WebSocket stream over a plain TCP connection (no TLS — the tailnet is
/// already encrypted by WireGuard). Server accepts yield this; clients from
/// [`connect_peer`] wrap the same bytes in tungstenite's `MaybeTlsStream`.
pub type WsStream = WebSocketStream<TcpStream>;
/// Client-side stream (happy-eyeballs dial may go through the TLS wrapper
/// even for `ws://`).
pub type ClientWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Timeout for a `tailscale` subprocess call (status/whois).
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(10);

// ── discovery: `tailscale status --json` ─────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "Self")]
    this: TailscaleNode,
    #[serde(rename = "Peer", default)]
    peer: HashMap<String, TailscaleNode>,
    #[serde(rename = "User", default)]
    user: HashMap<String, TailscaleUser>,
}

#[derive(Debug, Clone, Deserialize)]
struct TailscaleUser {
    #[serde(rename = "ID")]
    id: u64,
    #[serde(rename = "LoginName", default)]
    login_name: String,
    #[serde(rename = "DisplayName", default)]
    #[allow(dead_code)]
    display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TailscaleNode {
    #[serde(rename = "HostName")]
    host_name: String,
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<String>,
    #[serde(rename = "OS", default)]
    os: String,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "UserID", default)]
    user_id: u64,
}

/// A peer on the tailnet (from `tailscale status --json`).
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: String,
    pub host_name: String,
    pub dns_name: String,
    pub tailscale_ips: Vec<String>,
    pub os: String,
    pub online: bool,
    pub user_id: u64,
    pub login_name: String,
}

impl Peer {
    /// The MagicDNS hostname without the trailing dot, e.g. `minis.tailnet.ts.net`.
    pub fn dns_host(&self) -> &str {
        self.dns_name.strip_suffix('.').unwrap_or(&self.dns_name)
    }
}

fn login_name_for(status: &TailscaleStatus, user_id: u64) -> String {
    status
        .user
        .values()
        .find(|u| u.id == user_id)
        .map(|u| u.login_name.clone())
        .unwrap_or_default()
}

fn peer_from_node(id: String, n: TailscaleNode, login_name: String) -> Peer {
    Peer {
        id,
        host_name: n.host_name,
        dns_name: n.dns_name,
        tailscale_ips: n.tailscale_ips,
        os: n.os,
        online: n.online,
        user_id: n.user_id,
        login_name,
    }
}

/// This device, from `tailscale status --json` `Self`.
pub async fn discover_self() -> Result<Peer, SyncError> {
    let out = run_tailscale(&["status", "--json"]).await?;
    let status: TailscaleStatus =
        serde_json::from_str(&out).map_err(|e| SyncError::Tailnet(format!("parse status: {e}")))?;
    let login = login_name_for(&status, status.this.user_id);
    Ok(peer_from_node("self".into(), status.this, login))
}

/// List the tailnet peers (excluding this device) via `tailscale status --json`.
pub async fn discover_peers() -> Result<Vec<Peer>, SyncError> {
    let out = run_tailscale(&["status", "--json"]).await?;
    let status: TailscaleStatus =
        serde_json::from_str(&out).map_err(|e| SyncError::Tailnet(format!("parse status: {e}")))?;
    let TailscaleStatus { peer, user, .. } = status;
    Ok(peer
        .into_iter()
        .map(|(id, n)| {
            let login = user
                .values()
                .find(|u| u.id == n.user_id)
                .map(|u| u.login_name.clone())
                .unwrap_or_default();
            peer_from_node(id, n, login)
        })
        .collect())
}

// ── auth: `tailscale whois` ─────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct TailscaleWhois {
    #[serde(rename = "Node")]
    node: WhoisNode,
    #[serde(rename = "UserProfile")]
    user_profile: WhoisUserProfile,
}

#[derive(Debug, Clone, Deserialize)]
struct WhoisNode {
    /// Tailscale's `--json` whois emits a numeric `ID`; older/docs fixtures
    /// used a string. Accept either so hub auth does not reset peers.
    #[serde(rename = "ID", deserialize_with = "deserialize_stringish")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "User", default)]
    user: u64,
    /// `whois --json` emits CIDR-form (`100.64.0.2/32`), while `status --json`
    /// uses bare IPs (`100.64.0.2`). Do not compare these verbatim against a
    /// peer's address — strip any `/len` suffix first.
    #[serde(rename = "TailscaleIPs", alias = "Addresses", default)]
    tailscale_ips: Vec<String>,
}

/// Serde helper: JSON string or number → `String`.
fn deserialize_stringish<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Stringish;
    impl<'de> serde::de::Visitor<'de> for Stringish {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or number")
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_owned())
        }
        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<String, E> {
            Ok(v)
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
    }
    deserializer.deserialize_any(Stringish)
}

#[derive(Debug, Clone, Deserialize)]
struct WhoisUserProfile {
    #[serde(rename = "ID")]
    #[allow(dead_code)]
    id: u64,
    #[serde(rename = "LoginName")]
    login_name: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
}

/// The identity of a peer, resolved from its tailnet IP via `tailscale whois`.
/// This is the trust boundary: the tailnet ACLs already gate who can reach us,
/// and `whois` tells us who actually did.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    pub node_id: String,
    pub node_name: String,
    pub user_id: u64,
    pub tailscale_ips: Vec<String>,
    pub login_name: String,
    pub display_name: String,
}

const WHOIS_OK_TTL: Duration = Duration::from_secs(60);
const WHOIS_ERR_TTL: Duration = Duration::from_secs(2);

#[allow(clippy::type_complexity)]
fn whois_cache() -> &'static Mutex<HashMap<String, (Instant, Result<PeerIdentity, String>)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Result<PeerIdentity, String>)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a peer's identity from its tailnet IP via `tailscale whois`.
pub async fn whois(ip: &str) -> Result<PeerIdentity, SyncError> {
    if let Some((at, cached)) = whois_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(ip)
        .cloned()
    {
        let ttl = if cached.is_ok() {
            WHOIS_OK_TTL
        } else {
            WHOIS_ERR_TTL
        };
        if at.elapsed() < ttl {
            return cached.map_err(SyncError::Tailnet);
        }
    }
    let result = whois_uncached(ip).await;
    whois_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            ip.to_string(),
            (
                Instant::now(),
                match &result {
                    Ok(id) => Ok(id.clone()),
                    Err(err) => Err(err.to_string()),
                },
            ),
        );
    result
}

async fn whois_uncached(ip: &str) -> Result<PeerIdentity, SyncError> {
    let out = run_tailscale(&["whois", "--json", ip]).await?;
    let w: TailscaleWhois =
        serde_json::from_str(&out).map_err(|e| SyncError::Tailnet(format!("parse whois: {e}")))?;
    Ok(PeerIdentity {
        node_id: w.node.id,
        node_name: w.node.name,
        user_id: w.node.user,
        tailscale_ips: w.node.tailscale_ips,
        login_name: w.user_profile.login_name,
        display_name: w.user_profile.display_name,
    })
}

// ── peer server ────────────────────────────────────────────────────────────

/// A WebSocket server bound to the tailnet interface. Each accepted connection
/// is one room; the request path identifies which room. The caller routes the
/// stream to the room handler (chat2 binary / registry text) — this module only
/// owns the transport.
pub struct PeerServer {
    listener: TcpListener,
    addr: SocketAddr,
}

impl PeerServer {
    /// Bind a listener. `addr` is typically `0.0.0.0:PORT` (reachable only via
    /// the tailnet ACLs) or a specific tailnet IP.
    pub async fn bind(addr: impl tokio::net::ToSocketAddrs) -> Result<Self, SyncError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| SyncError::Tailnet(format!("bind: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| SyncError::Tailnet(format!("local_addr: {e}")))?;
        Ok(Self { listener, addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Accept one connection and complete the WS handshake, returning the
    /// request path, the stream, and the peer's socket address. No auth — the
    /// test seam; production uses [`Self::accept`].
    pub async fn accept_ws(&self) -> Result<(String, WsStream, SocketAddr), SyncError> {
        let (stream, peer) = self
            .listener
            .accept()
            .await
            .map_err(|e| SyncError::Tailnet(format!("accept: {e}")))?;
        let path_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let path_cell_for_cb = path_cell.clone();
        let ws = tokio_tungstenite::accept_hdr_async(stream, {
            #[allow(clippy::result_large_err)]
            move |req: &Request, resp: Response| {
                *path_cell_for_cb.lock().unwrap() = Some(req.uri().path().to_string());
                Ok(resp)
            }
        })
        .await
        .map_err(|e| SyncError::Tailnet(format!("ws handshake: {e}")))?;
        let path = path_cell.lock().unwrap().clone().unwrap_or_default();
        Ok((path, ws, peer))
    }

    /// Accept one connection, authenticate the peer via `tailscale whois`, and
    /// return the request path, stream, and identity.
    pub async fn accept(&self) -> Result<(String, WsStream, PeerIdentity), SyncError> {
        let (path, ws, peer) = self.accept_ws().await?;
        let identity = whois(&peer.ip().to_string()).await?;
        Ok((path, ws, identity))
    }
}

/// Run the accept loop, handing each authenticated connection to `handler`.
/// The handler is responsible for spawning its own per-connection task.
pub fn spawn<F>(server: std::sync::Arc<PeerServer>, handler: F) -> tokio::task::JoinHandle<()>
where
    F: Fn(String, WsStream, PeerIdentity) + Send + Sync + 'static,
{
    tokio::spawn(async move {
        loop {
            match server.accept().await {
                Ok((path, ws, identity)) => handler(path, ws, identity),
                Err(err) => {
                    tracing::warn!(error = %err, "tailnet: accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    })
}

// ── peer client ─────────────────────────────────────────────────────────────

/// Dial a peer's tailnet address and complete the WS handshake. `path` is the
/// room path (e.g. `/chat2/{chatId}/ws`). Uses the happy-eyeballs dialer so
/// MagicDNS resolution and IPv6/IPv4 racing behave like every other socket.
pub async fn connect_peer(
    hostname: &str,
    port: u16,
    path: &str,
) -> Result<ClientWsStream, SyncError> {
    let url = format!("ws://{hostname}:{port}{path}");
    crate::dial::connect_ws(&url)
        .await
        .map_err(|e| SyncError::Tailnet(e.to_string()))
}

// ── tailscale subprocess helper ────────────────────────────────────────────

async fn run_tailscale(args: &[&str]) -> Result<String, SyncError> {
    let output = tokio::time::timeout(
        TAILSCALE_TIMEOUT,
        Command::new("tailscale").args(args).output(),
    )
    .await
    .map_err(|_| SyncError::Tailnet("tailscale timed out".into()))?
    .map_err(|e| SyncError::Tailnet(format!("spawn tailscale: {e}")))?;
    if !output.status.success() {
        return Err(SyncError::Tailnet(format!(
            "tailscale {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| SyncError::Tailnet(format!("tailscale stdout not utf8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    #[test]
    fn dns_host_strips_trailing_dot() {
        let p = Peer {
            id: "x".into(),
            host_name: "minis".into(),
            dns_name: "minis.tailnet.ts.net.".into(),
            tailscale_ips: vec![],
            os: "linux".into(),
            online: true,
            user_id: 1,
            login_name: "user@example.com".into(),
        };
        assert_eq!(p.dns_host(), "minis.tailnet.ts.net");
    }

    #[test]
    fn parses_tailscale_status_json() {
        // Representative `tailscale status --json` shape (fields we read).
        let json = r#"{
            "Self": {
                "HostName": "this",
                "DNSName": "this.tailnet.ts.net.",
                "TailscaleIPs": ["100.64.0.1"],
                "OS": "linux",
                "Online": true,
                "UserID": 1,
                "LoginName": "me@example.com"
            },
            "Peer": {
                "minis": {
                    "HostName": "minis",
                    "DNSName": "minis.tailnet.ts.net.",
                    "TailscaleIPs": ["100.64.0.2"],
                    "OS": "linux",
                    "Online": true,
                    "UserID": 1
                },
                "laptop": {
                    "HostName": "laptop",
                    "DNSName": "laptop.tailnet.ts.net.",
                    "TailscaleIPs": ["100.64.0.3"],
                    "OS": "macos",
                    "Online": false,
                    "UserID": 1
                }
            },
            "User": {
                "1": {
                    "ID": 1,
                    "LoginName": "me@example.com",
                    "DisplayName": "Me"
                }
            }
        }"#;
        let status: TailscaleStatus = serde_json::from_str(json).unwrap();
        assert_eq!(status.peer.len(), 2);
        let minis = &status.peer["minis"];
        assert_eq!(minis.dns_name, "minis.tailnet.ts.net.");
        assert!(minis.online);
        let laptop = &status.peer["laptop"];
        assert!(!laptop.online);
    }

    #[test]
    fn parses_tailscale_whois_json() {
        // Representative `tailscale whois <ip>` shape.
        let json = r#"{
            "Node": {
                "ID": "n123",
                "Name": "minis.tailnet.ts.net.",
                "User": 1,
                "TailscaleIPs": ["100.64.0.2"]
            },
            "UserProfile": {
                "ID": 1,
                "LoginName": "me@example.com",
                "DisplayName": "Me"
            }
        }"#;
        let w: TailscaleWhois = serde_json::from_str(json).unwrap();
        assert_eq!(w.node.id, "n123");
        assert_eq!(w.node.name, "minis.tailnet.ts.net.");
        assert_eq!(w.user_profile.login_name, "me@example.com");
        assert_eq!(w.user_profile.display_name, "Me");
    }

    #[test]
    fn parses_tailscale_whois_json_numeric_node_id() {
        // Live `tailscale whois --json` (1.x) emits numeric Node.ID and
        // CIDR Addresses instead of TailscaleIPs. Fixture values are fake.
        let json = r#"{
            "Node": {
                "ID": 1234567890123456,
                "StableID": "nTESTSTABLEID0001",
                "Name": "hub.tailnet-example.ts.net.",
                "User": 42,
                "Addresses": ["100.64.0.2/32", "fd7a:115c:a1e0::1/128"]
            },
            "UserProfile": {
                "ID": 42,
                "LoginName": "someone@example.com",
                "DisplayName": "Someone"
            }
        }"#;
        let w: TailscaleWhois = serde_json::from_str(json).unwrap();
        assert_eq!(w.node.id, "1234567890123456");
        assert_eq!(w.node.name, "hub.tailnet-example.ts.net.");
        assert_eq!(w.node.user, 42);
        assert_eq!(
            w.node.tailscale_ips,
            vec![
                "100.64.0.2/32".to_string(),
                "fd7a:115c:a1e0::1/128".to_string()
            ]
        );
        assert_eq!(w.user_profile.login_name, "someone@example.com");
    }

    #[tokio::test]
    async fn peer_server_accepts_and_echoes() {
        let server = PeerServer::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr();
        let server = std::sync::Arc::new(server);

        let server_task = tokio::spawn(async move {
            let (path, mut ws, _peer) = server.accept_ws().await.unwrap();
            assert_eq!(path, "/chat2/abc/ws");
            // echo the first binary frame back
            let msg = ws.next().await.unwrap().unwrap();
            ws.send(msg).await.unwrap();
        });

        let mut client = connect_peer("127.0.0.1", addr.port(), "/chat2/abc/ws")
            .await
            .unwrap();
        client
            .send(WsMessage::Binary(vec![1, 2, 3].into()))
            .await
            .unwrap();
        let echoed = client.next().await.unwrap().unwrap();
        assert_eq!(echoed, WsMessage::Binary(vec![1, 2, 3].into()));
        server_task.await.unwrap();
    }
}
