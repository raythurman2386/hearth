//! Regression: Settings → Devices → Remove (Mutate `deleteDevice`).
//!
//! Removing an old machine tombstones its registry row; the local device is
//! refused (an engine cannot forget itself) and an unknown id is a no-op.
//! This also pins the serde error shape an app newer than its daemon sees when
//! the op is missing from that build (`unknown variant`), which the UI turns
//! into an actionable "engine is older than this app" message.

mod common;

use std::sync::Arc;

use chrono::Utc;
use hearth_engine::HarnessRegistry;
use hearth_proto::Device;
use hearth_rpc::methods;

fn device(id: &str, name: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
        platform: "linux".into(),
        last_seen_at: Some(Utc::now()),
        created_at: Some(Utc::now()),
        version: Some("0.2.11".into()),
    }
}

fn device_ids(core: &hearth_engine::EngineCore) -> Vec<String> {
    core.workspace
        .read_devices()
        .expect("read devices")
        .into_iter()
        .map(|d| d.id)
        .collect()
}

#[tokio::test]
async fn delete_device_removes_foreign_row_refuses_local_and_ignores_missing() {
    let dir = tempfile::tempdir().unwrap();
    let core =
        common::assemble_with_device(dir.path(), Arc::new(HarnessRegistry::new()), "dev-local");
    // A second, older machine in the registry (production rows arrive via
    // registry sync; the test seam writes one directly).
    core.workspace
        .upsert_device_row(&device("dev-old", "Old Laptop"));
    assert_eq!(device_ids(&core), ["dev-local", "dev-old"]);

    let client = hearth_rpc::memory_client(core.rpc_service());

    // Remove the foreign machine: succeeds and the row is gone.
    client
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteDevice", "deviceId": "dev-old" }),
        )
        .await
        .expect("deleteDevice for a foreign device");
    assert_eq!(
        device_ids(&core),
        ["dev-local"],
        "the removed row must not be readable any more"
    );

    // The local device is refused with a message the UI can show verbatim.
    let err = client
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteDevice", "deviceId": "dev-local" }),
        )
        .await
        .expect_err("the local device cannot be removed");
    let message = err.to_string();
    assert!(
        message.contains("local device"),
        "refusal should name the local device, got: {message}"
    );
    assert_eq!(device_ids(&core), ["dev-local"], "refusal keeps the row");

    // An id that is not in the registry is a no-op success (retrying a remove
    // after a concurrent sync must not surface an error).
    client
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteDevice", "deviceId": "dev-missing" }),
        )
        .await
        .expect("deleting a missing device is a no-op success");
    assert_eq!(device_ids(&core), ["dev-local"]);
}

/// The wire error an app sees from a daemon whose build predates the op: serde
/// rejects the tagged variant during `MutateParams` deserialization. Pin the
/// text so `device_mutation_error`'s match stays honest.
#[tokio::test]
async fn unknown_mutate_op_reports_serde_unknown_variant() {
    let dir = tempfile::tempdir().unwrap();
    let core = common::assemble(dir.path());
    let client = hearth_rpc::memory_client(core.rpc_service());

    let err = client
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteDeviceFromAnOldApp", "deviceId": "dev-x" }),
        )
        .await
        .expect_err("an op this build does not know must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("unknown variant"),
        "serde's tagged-enum rejection should say `unknown variant`, got: {message}"
    );
}
