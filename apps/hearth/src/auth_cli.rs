//! `hearth status` — the standalone runtime identity report.
//!
//! Hearth has no account service: the device is the identity, and the tailnet
//! is the trust boundary. `status` therefore reports the fixed workspace scope,
//! the device bearer the runtime will present to the hub, and engine liveness.

use hearth_engine::{Engine, EngineConfig, InstanceLock, WorkspaceScope};

#[derive(Debug, PartialEq, Eq)]
struct AccountStatus {
    mode: &'static str,
    identity: &'static str,
}

fn account_status(scope: WorkspaceScope) -> AccountStatus {
    match scope {
        WorkspaceScope::Local => AccountStatus {
            mode: "local only",
            identity: "device identity (no hub bearer)",
        },
        WorkspaceScope::Development => AccountStatus {
            mode: "development",
            identity: "device identity (tailnet bearer)",
        },
    }
}

/// `hearth status`: report the fixed scope a new engine would select, the
/// device identity, and engine liveness.
pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let auth = Engine::build_auth(&config);
    let next_scope = Engine::initial_workspace_scope(&auth);
    let scope = live_engine_scope(config.ipc_port)
        .await
        .unwrap_or(next_scope);
    let account = account_status(scope);
    println!("Data dir: {}", config.data_dir.display());
    println!(
        "Edge:     {}",
        if config.edge_url.trim().is_empty() {
            "local (no remote endpoint)"
        } else {
            config.edge_url.as_str()
        }
    );
    println!("Mode:     {}", account.mode);
    println!("Auth:     {}", account.identity);
    if let Some(user) = auth.user_id() {
        println!("Identity: {user}");
    }
    match config.tailnet_host.as_deref() {
        Some(host) if !host.trim().is_empty() => println!(
            "Tailnet:  {host} port {}{}",
            config.tailnet_port,
            if config.tailnet_hub {
                " (this device hosts rooms)"
            } else {
                ""
            }
        ),
        _ => println!("Tailnet:  off (local-only runtime)"),
    }
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.ipc_port));
    let ipc = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
    println!(
        "IPC:      {} 127.0.0.1:{}",
        if ipc.is_ok() {
            "listening on"
        } else {
            "not listening on"
        },
        config.ipc_port
    );
    Ok(())
}

/// Prefer the immutable scope of a live runtime. Falling back to the next-boot
/// derivation is correct when no engine is listening and tolerant of old
/// daemons that predate EngineInfo.
async fn live_engine_scope(ipc_port: u16) -> Option<WorkspaceScope> {
    let client = hearth_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .ok()?;
    let value = client
        .call(hearth_rpc::methods::ENGINE_INFO, serde_json::json!({}))
        .await
        .ok()?;
    serde_json::from_value::<hearth_engine::EngineInfo>(value)
        .ok()
        .map(|info| info.workspace_scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mode_follows_the_workspace_scope() {
        let local = account_status(WorkspaceScope::Local);
        assert_eq!(local.mode, "local only");
        assert!(local.identity.contains("device identity"));

        let development = account_status(WorkspaceScope::Development);
        assert_eq!(development.mode, "development");
        assert!(development.identity.contains("tailnet bearer"));
    }
}
