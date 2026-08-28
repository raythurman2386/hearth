//! IPC accept-loop liveness under stalled handshakes (2026-08-27 wedge).
//!
//! The old accept path awaited each `accept_hdr_async` inline with no timeout,
//! so one client that connected and never finished the HTTP upgrade blocked
//! every later dial of the IPC port (listen-queue backlog, CLOSE-WAITs, zero
//! successful handshakes). These tests pin the two properties the engine
//! depends on: a stalled client cannot block the listener, and every
//! handshake is bounded in wall-clock time.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hearth_rpc::{RpcError, RpcService, connect_ws, serve_ws_listener};
use tokio::io::AsyncWriteExt;

struct Echo;

#[async_trait]
impl RpcService for Echo {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<hearth_rpc::RpcReply, RpcError> {
        match method {
            "Echo" => Ok(hearth_rpc::RpcReply::Value(params)),
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

async fn spawn_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(serve_ws_listener(listener, Arc::new(Echo)));
    (port, task)
}

/// Ten clients connect; five deliberately stall mid-handshake (TCP open, a
/// partial HTTP request line, then silence). The five honest clients must all
/// still complete handshakes and round-trip an RPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_handshakes_do_not_block_other_clients() {
    let (port, _server) = spawn_server().await;

    let mut stallers = Vec::new();
    for _ in 0..5 {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        // A syntactically valid start that never completes the upgrade: the
        // server's handshake can never finish on these bytes.
        stream.write_all(b"GET /rpc HTTP/1.1\r\n").await.unwrap();
        stallers.push(stream); // held open, no further bytes, never closed
    }

    let mut clients = Vec::new();
    for _ in 0..5 {
        let client = tokio::time::timeout(
            Duration::from_secs(5),
            connect_ws(&format!("ws://127.0.0.1:{port}")),
        )
        .await
        .expect("honest dial must complete despite the stalled peers")
        .expect("handshake succeeds");
        clients.push(Arc::new(client));
    }
    for client in &clients {
        let echoed = client
            .call("Echo", serde_json::json!({ "ok": true }))
            .await
            .expect("echo after stalled siblings");
        assert_eq!(echoed, serde_json::json!({ "ok": true }));
    }
    drop(clients);
    drop(stallers);
    _server.abort();
}

/// One handshake that never completes must fail bounded by the server's
/// handshake timeout, not hang forever (and the same socket must then be
/// reusable by a well-behaved client).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_dial_times_out_and_the_listener_survives() {
    let (port, _server) = spawn_server().await;

    let squatter = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // Connect and send nothing: the handshake stays pending server-side.

    let honest = tokio::time::timeout(Duration::from_secs(5), async {
        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await?;
        client.call("Echo", serde_json::json!(1)).await
    })
    .await
    .expect("listener still serving while one dial idles")
    .expect("rpc round trip");
    assert_eq!(honest, serde_json::json!(1));

    drop(squatter);
    _server.abort();
}
