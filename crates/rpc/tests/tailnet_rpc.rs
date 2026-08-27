//! Direct `/rpc` over the tailnet hub mux (plain text RPC, no DeviceRoom
//! frames). Covers `DeviceLink::connect_plain` + `serve_websocket` keepalive.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hearth_rpc::{DeviceLink, RpcError, RpcReply, RpcService, serve_websocket};
use hearth_sync::hub::{Hub, HubConfig};

struct EchoService;

#[async_trait]
impl RpcService for EchoService {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            "Echo" => Ok(RpcReply::Value(params)),
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

#[tokio::test]
async fn hub_rpc_echo_and_idle_ping() {
    hearth_rpc::device_room::set_client_liveness_for_tests(
        Duration::from_millis(50),
        Duration::from_secs(5),
    );

    let dir = tempfile::tempdir().unwrap();
    let service: Arc<dyn RpcService> = Arc::new(EchoService);
    let on_rpc = Arc::new(move |ws| {
        let service = service.clone();
        tokio::spawn(serve_websocket(ws, service));
    });
    let hub = Hub::bind(
        "127.0.0.1:0",
        HubConfig {
            data_dir: dir.path().to_path_buf(),
            releases_dir: dir.path().join("releases"),
            serve_rooms: false,
            on_rpc: Some(on_rpc),
            skip_whois: true,
        },
    )
    .await
    .unwrap();
    let port = hub.local_addr().port();
    let _task = hub.spawn();

    let link = DeviceLink::connect_plain(&format!("ws://127.0.0.1:{port}/rpc"))
        .await
        .expect("plain rpc dial");
    let echoed = link
        .client()
        .call("Echo", serde_json::json!({"k": 1}))
        .await
        .expect("echo");
    assert_eq!(echoed, serde_json::json!({"k": 1}));

    // Several ping/pong cycles; the 25s production silence lease is compressed
    // via the test ping interval. The link must stay up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!link.is_closed(), "idle /rpc link must survive pings");
    let echoed = link
        .client()
        .call("Echo", serde_json::json!("still"))
        .await
        .expect("echo after idle");
    assert_eq!(echoed, serde_json::json!("still"));

    hearth_rpc::device_room::set_client_liveness_for_tests(Duration::ZERO, Duration::ZERO);
}
