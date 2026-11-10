//! Local-first startup boundaries and tailnet routing behavior.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hearth_engine::{Engine, EngineConfig, HarnessId, WorkspaceScope};
use hearth_rpc::{connect_ws, methods};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(data_dir: &std::path::Path, edge_url: String, edge_token: Option<&str>) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        edge_url,
        edge_token: edge_token.map(str::to_string),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
        org_id: None,
        tailnet_host: None,
        tailnet_port: hearth_engine::DEFAULT_TAILNET_PORT,
        tailnet_hub: false,
    }
}

async fn rejecting_edge() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                seen.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"error":"revoked"}"#;
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), requests, task)
}

async fn wait_until(mut check: impl FnMut() -> bool, message: &str) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{message}");
}

#[tokio::test]
async fn development_without_an_explicit_bearer_stays_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, None);
    let auth = Engine::build_auth(&config);
    let scope = Engine::initial_workspace_scope(&auth);
    let profile =
        Engine::resolve_profile(&config, &auth, scope).expect("development profile is ready");

    assert_eq!(scope, WorkspaceScope::Development);
    assert_eq!(auth.access_token().await.as_deref(), Some("dev-user"));
    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Development);
    assert!(
        runtime.core().links().is_none(),
        "the synthetic dev identity must not enable Edge"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn explicit_dev_bearer_keeps_online_routing_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("dev-user@dev-org"),
    );
    let auth = Engine::build_auth(&config);
    let scope = Engine::initial_workspace_scope(&auth);
    let profile =
        Engine::resolve_profile(&config, &auth, scope).expect("development profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Development);
    assert!(runtime.core().links().is_some());
    assert!(dir.path().join("orgs/dev-org/dev-user").is_dir());
    runtime.shutdown().await;
}

#[tokio::test]
async fn headless_stop_rpc_drains_the_daemon_and_releases_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut engine_config = config(dir.path(), "http://127.0.0.1:1".into(), None);
    engine_config.ipc_port = port;
    let daemon = tokio::spawn(Engine::new(engine_config).run());

    let client = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(client) = connect_ws(&format!("ws://127.0.0.1:{port}")).await {
                break client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("headless IPC did not start");

    assert_eq!(
        client
            .call(methods::STOP_ENGINE, serde_json::json!({}))
            .await
            .unwrap(),
        serde_json::json!({ "ok": true })
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("headless engine did not stop")
        .expect("headless task panicked")
        .expect("headless shutdown failed");

    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("headless IPC port remained occupied after shutdown");
}

/// Replacing an online runtime must be a real ownership boundary: after
/// `shutdown()` returns, no worker may send another Edge request (updater,
/// room joins), and dropping the runtime must actually free the engine graph —
/// the sessions ⇄ doc-host cycle and the strong-`self` worker loops previously
/// kept a replaced runtime alive and polling forever.
#[tokio::test]
async fn online_runtime_shutdown_stops_edge_workers_and_retires_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("dev-user@dev-org"));
    let auth = Engine::build_auth(&config);
    let scope = Engine::initial_workspace_scope(&auth);
    let profile =
        Engine::resolve_profile(&config, &auth, scope).expect("development profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();
    let retired = runtime.core().doc_host.retirement_probe();

    // Live traffic proof: the woken release checker (and the room joins) must
    // be hitting the counting edge before the boundary is exercised.
    if let Some(updater) = runtime.core().updater() {
        updater.check_now();
    }
    wait_until(
        || requests.load(Ordering::SeqCst) >= 2,
        "edge workers never produced traffic before shutdown",
    )
    .await;

    tokio::time::timeout(std::time::Duration::from_secs(30), runtime.shutdown())
        .await
        .expect("shutdown never returned — an Edge worker did not join");
    let after = requests.load(Ordering::SeqCst);
    drop(runtime);

    wait_until(
        &*retired,
        "engine graph still reachable after shutdown + drop",
    )
    .await;
    // Long enough to cover a couple of worker retry periods: a surviving
    // Edge worker loop would land another request in this window.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        after,
        "edge received requests after shutdown returned"
    );
    edge_task.abort();
}
