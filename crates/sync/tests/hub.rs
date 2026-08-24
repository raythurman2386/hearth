//! ChatClient + RegistryClient + HTTP against the real tailnet hub mux
//! (loopback, `skip_whois`). Replaces the deleted live-edge tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use hearth_doc::RegistryDoc;
use hearth_proto::{Chat, Device};
use hearth_sync::chat_client::{ChatDocSink, CheckpointFetcher};
use hearth_sync::chat_frames::{self as wire, frame_type};
use hearth_sync::hub::{Hub, HubConfig};
use hearth_sync::{ChatClient, RegistryClient, SyncError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn ts(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached in time");
}

async fn start_hub(dir: &std::path::Path) -> (Hub, SocketAddr) {
    let hub = Hub::bind(
        "127.0.0.1:0",
        HubConfig {
            data_dir: dir.to_path_buf(),
            serve_rooms: true,
            on_rpc: None,
            skip_whois: true,
        },
    )
    .await
    .expect("hub bind");
    let addr = hub.local_addr();
    (hub, addr)
}

struct RecordingSink {
    rows: Mutex<Vec<(Vec<u8>, u64)>>,
}

impl Default for RecordingSink {
    fn default() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
        }
    }
}

impl ChatDocSink for RecordingSink {
    fn apply_row(&self, bytes: &[u8], cursor: u64) -> bool {
        self.rows.lock().unwrap().push((bytes.to_vec(), cursor));
        true
    }
    fn apply_checkpoint(&self, _bytes: &[u8], _cursor: u64) -> Result<(), String> {
        Ok(())
    }
    fn contains_frontier(&self, _frontier: &[u8]) -> bool {
        false
    }
    fn advance_cursor(&self, _cursor: u64) {}
}

struct NoCheckpoint;

impl CheckpointFetcher for NoCheckpoint {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        Box::pin(async { Err(SyncError::Protocol("no checkpoint".into())) })
    }
}

fn device(id: &str) -> Device {
    Device {
        id: id.into(),
        name: format!("{id}-name"),
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
        title: Some("chat".into()),
        archived: false,
        cwd: Some("/tmp".into()),
        branch: None,
        checkout_id: None,
        config: None,
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

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, HashMap<String, String>, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("http connect");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    if !body.is_empty() {
        stream.write_all(body).await.unwrap();
    }
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http header terminator");
    let head = String::from_utf8_lossy(&buf[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut hdrs = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            hdrs.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    (status, hdrs, buf[split + 4..].to_vec())
}

fn decode_prefixed(body: &[u8]) -> Vec<wire::WireFrame> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= body.len() {
        let n = u32::from_le_bytes(body[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if i + n > body.len() {
            break;
        }
        if let Some(frame) = wire::decode(&body[i..i + n]) {
            out.push(frame);
        }
        i += n;
    }
    out
}

#[tokio::test]
async fn two_chat_clients_converge_live_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, addr) = start_hub(dir.path()).await;
    let _task = hub.spawn();

    let url = format!("ws://127.0.0.1:{}/chat2/c1/ws", addr.port());
    let sink_a = Arc::new(RecordingSink::default());
    let sink_b = Arc::new(RecordingSink::default());
    let a = ChatClient::connect(&url, sink_a.clone(), Arc::new(NoCheckpoint), "dev-a", 0)
        .await
        .expect("a joins empty chat2 room");
    let b = ChatClient::connect(&url, sink_b.clone(), Arc::new(NoCheckpoint), "dev-b", 0)
        .await
        .expect("b joins empty chat2 room");

    a.enqueue_update(vec![1, 2, 3]);
    wait_until(|| {
        sink_b
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|(bytes, _)| bytes == &[1, 2, 3])
    })
    .await;
    // Pusher does not apply its own live row.
    assert!(sink_a.rows.lock().unwrap().is_empty());

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn chat2_rows_survive_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, addr) = start_hub(dir.path()).await;
    let task = hub.spawn();
    let url = format!("ws://127.0.0.1:{}/chat2/persist/ws", addr.port());
    let sink_a = Arc::new(RecordingSink::default());
    let a = ChatClient::connect(&url, sink_a, Arc::new(NoCheckpoint), "dev-a", 0)
        .await
        .unwrap();
    a.enqueue_update(vec![9, 9, 9]);
    wait_until(|| a.stats().pending_pushes == 0 && a.stats().cursor >= 1).await;
    a.shutdown().await;
    task.abort();

    let (hub, addr) = start_hub(dir.path()).await;
    let _task = hub.spawn();
    let url = format!("ws://127.0.0.1:{}/chat2/persist/ws", addr.port());
    let sink_b = Arc::new(RecordingSink::default());
    let b = ChatClient::connect(&url, sink_b.clone(), Arc::new(NoCheckpoint), "dev-b", 0)
        .await
        .unwrap();
    wait_until(|| {
        sink_b
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|(bytes, _)| bytes == &[9, 9, 9])
    })
    .await;
    b.shutdown().await;
}

#[tokio::test]
async fn two_registry_clients_converge_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, addr) = start_hub(dir.path()).await;
    let task = hub.spawn();
    let url = format!("ws://127.0.0.1:{}/registry/org1/ws", addr.port());

    let doc_a = Arc::new(Mutex::new(RegistryDoc::new("dev-a")));
    let doc_b = Arc::new(Mutex::new(RegistryDoc::new("dev-b")));
    {
        let mut doc = doc_a.lock().unwrap();
        doc.upsert_device(&device("dev-a")).unwrap();
        doc.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    }
    let client_a = RegistryClient::connect(&url, doc_a.clone(), "dev-a")
        .await
        .expect("a connects");
    client_a.nudge();
    wait_until(|| doc_a.lock().unwrap().pending_len() == 0).await;

    let client_b = RegistryClient::connect(&url, doc_b.clone(), "dev-b")
        .await
        .expect("b connects");
    wait_until(|| doc_b.lock().unwrap().read_chats().unwrap().len() == 1).await;

    {
        let mut doc = doc_b.lock().unwrap();
        assert!(doc.rename_chat("chat-1", "from b").unwrap());
    }
    client_b.nudge();
    wait_until(|| {
        doc_a
            .lock()
            .unwrap()
            .chat("chat-1")
            .unwrap()
            .is_some_and(|c| c.title.as_deref() == Some("from b"))
    })
    .await;

    client_a.shutdown().await;
    client_b.shutdown().await;
    task.abort();

    let (hub, addr) = start_hub(dir.path()).await;
    let _task = hub.spawn();
    let url = format!("ws://127.0.0.1:{}/registry/org1/ws", addr.port());
    let doc_c = Arc::new(Mutex::new(RegistryDoc::new("dev-c")));
    let client_c = RegistryClient::connect(&url, doc_c.clone(), "dev-c")
        .await
        .expect("c reconnects to persisted room");
    wait_until(|| {
        doc_c
            .lock()
            .unwrap()
            .chat("chat-1")
            .unwrap()
            .is_some_and(|c| c.title.as_deref() == Some("from b"))
    })
    .await;
    client_c.shutdown().await;
}

#[tokio::test]
async fn http_checkpoint_range_rows_and_releases() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("releases")).unwrap();
    std::fs::write(dir.path().join("releases/hello.txt"), b"hi").unwrap();
    let (hub, addr) = start_hub(dir.path()).await;
    let _task = hub.spawn();

    let (status, _, body) = http(addr, "GET", "/health", &[], &[]).await;
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("tailnet"));

    let (status, _, body) = http(addr, "GET", "/releases/hello.txt", &[], &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body, b"hi");

    let (status, _, _) = http(addr, "GET", "/releases/../secret", &[], &[]).await;
    assert_eq!(status, 400);

    let (status, _, _) = http(
        addr,
        "POST",
        "/chat2/http1/rows?device=d1&batchId=b1",
        &[],
        &[7, 7, 7],
    )
    .await;
    assert_eq!(status, 200);

    let (status, _, body) = http(addr, "GET", "/chat2/http1/rows?after=0", &[], &[]).await;
    assert_eq!(status, 200);
    let frames = decode_prefixed(&body);
    assert_eq!(frames[0].kind, frame_type::STATE);
    assert!(
        frames
            .iter()
            .any(|f| f.kind == frame_type::ROW && f.payload == [7, 7, 7])
    );
    assert!(frames.iter().any(|f| f.kind == frame_type::ROWS_DONE));

    let frontier = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [1, 2]);
    let (status, _, _) = http(
        addr,
        "POST",
        "/chat2/http1/checkpoint?seqCovered=1",
        &[("x-chat2-frontier", frontier.as_str())],
        &[9, 9, 9, 9],
    )
    .await;
    assert_eq!(status, 200);

    let (status, hdrs, body) = http(addr, "GET", "/chat2/http1/checkpoint", &[], &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body, [9, 9, 9, 9]);
    assert_eq!(
        hdrs.get("x-chat2-checkpoint-seq").map(String::as_str),
        Some("1")
    );

    let (status, _, body) = http(
        addr,
        "GET",
        "/chat2/http1/checkpoint",
        &[("range", "bytes=2-")],
        &[],
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(body, [9, 9]);

    let (status, hdrs, _) = http(
        addr,
        "GET",
        "/chat2/http1/checkpoint",
        &[("range", "bytes=99-")],
        &[],
    )
    .await;
    assert_eq!(status, 416);
    assert_eq!(
        hdrs.get("content-range").map(String::as_str),
        Some("bytes */4")
    );
}
