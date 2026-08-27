//! Two engines sharing a loopback hub (the replacement for the live-edge
//! workspace room test). Device rows and spaces converge through the real
//! registry WS protocol on `Hub`.

use std::sync::Arc;
use std::time::Duration;

use hearth_engine::{EdgeConfig, EngineCore, HarnessId, default_registry};
use hearth_sync::hub::{Hub, HubConfig};

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn assemble(dir: &std::path::Path, device_id: &str, hub: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let edge = Some(EdgeConfig::with_static_token(hub, "tailnet").with_device(device_id));
    EngineCore::assemble_with_identity(
        dir,
        Arc::new(default_registry()),
        HarnessId::Mock,
        edge,
        "hub-org",
        "alice",
    )
    .expect("engine core assembles")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_engines_converge_through_the_hub_registry() {
    let rooms = tempfile::tempdir().unwrap();
    let hub = Hub::bind(
        "127.0.0.1:0",
        HubConfig {
            data_dir: rooms.path().to_path_buf(),
            releases_dir: rooms.path().join("releases"),
            serve_rooms: true,
            on_rpc: None,
            skip_whois: true,
        },
    )
    .await
    .unwrap();
    let hub_url = format!("http://127.0.0.1:{}", hub.local_addr().port());
    let _task = hub.spawn();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-live-a", &hub_url);
    let b = assemble(dir_b.path(), "dev-live-b", &hub_url);

    for core in [&a, &b] {
        wait_for(
            || {
                let ids: Vec<String> = core
                    .workspace
                    .read_devices()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| d.id)
                    .collect();
                ids.contains(&"dev-live-a".into()) && ids.contains(&"dev-live-b".into())
            },
            "both device rows through the hub",
        )
        .await;
    }

    a.workspace
        .create_space(
            "space-1",
            "dev-live-a",
            "/tmp/proj",
            Some("proj".into()),
            false,
        )
        .expect("create space");
    wait_for(
        || {
            b.workspace
                .read_spaces()
                .unwrap_or_default()
                .iter()
                .any(|s| s.id == "space-1" && s.device_id == "dev-live-a")
        },
        "space from A lands on B",
    )
    .await;

    b.workspace
        .rename_device("dev-live-a", "renamed by b")
        .expect("rename");
    wait_for(
        || {
            a.workspace
                .read_devices()
                .unwrap_or_default()
                .iter()
                .any(|d| d.id == "dev-live-a" && d.name == "renamed by b")
        },
        "device rename through the hub",
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
}
