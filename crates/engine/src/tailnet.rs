//! Tailnet peer matching: hub self-detection and device-id → dial-host resolution.

use super::*;

/// Trimmed env var or the given default.
pub(crate) async fn this_device_is_hub(config: &EngineConfig) -> bool {
    if config.tailnet_hub {
        return true;
    }
    let Some(want) = config.tailnet_host.as_deref() else {
        return false;
    };
    match hearth_sync::tailnet::discover_self().await {
        Ok(me) => host_matches(&me, want),
        Err(err) => {
            tracing::warn!(error = %err, "tailnet: could not read Self; not hosting rooms");
            false
        }
    }
}

fn dns_first_label(host: &str) -> &str {
    host.trim_end_matches('.').split('.').next().unwrap_or(host)
}

/// True when `configured` names this Tailscale peer: exact MagicDNS / hostname,
/// or first-label equality (`minis` ↔ `minis.tailnet.ts.net`). Prefix matches
/// like `mac` ↔ `macbook…` are rejected.
fn host_matches(peer: &hearth_sync::tailnet::Peer, configured: &str) -> bool {
    let want = configured.trim().trim_end_matches('.').to_ascii_lowercase();
    if want.is_empty() {
        return false;
    }
    let dns = peer.dns_host().to_ascii_lowercase();
    let host = peer.host_name.to_ascii_lowercase();
    dns == want || host == want || dns_first_label(&dns) == want || dns_first_label(&want) == host
}

/// Resolve a registry device id to a Tailscale dial host. Returns `None` when
/// the device's friendly name does not match any peer — callers must not fall
/// back to the hub (that would run peer RPC on the wrong machine).
pub(crate) async fn resolve_tailnet_host(
    workspace: &WorkspaceHost,
    device_id: &str,
) -> Option<String> {
    let name = workspace
        .read_devices()
        .ok()
        .and_then(|devs| devs.into_iter().find(|d| d.id == device_id).map(|d| d.name))?;
    let peers = hearth_sync::tailnet::discover_peers().await.ok()?;
    peers
        .iter()
        .find(|p| host_matches(p, &name))
        .map(|p| p.dns_host().to_string())
}

#[cfg(test)]
mod host_match_tests {
    use super::{dns_first_label, host_matches};
    use hearth_sync::tailnet::Peer;

    fn peer(host_name: &str, dns_name: &str) -> Peer {
        Peer {
            id: "p".into(),
            host_name: host_name.into(),
            dns_name: dns_name.into(),
            tailscale_ips: vec![],
            os: "linux".into(),
            online: true,
            user_id: 0,
            login_name: String::new(),
        }
    }

    #[test]
    fn hub_autodetect_matches_short_and_magicdns() {
        let me = peer("minis", "minis.tailnet.ts.net.");
        assert!(host_matches(&me, "minis"));
        assert!(host_matches(&me, "minis.tailnet.ts.net"));
        assert!(host_matches(&me, "minis.tailnet.ts.net."));
    }

    #[test]
    fn hub_autodetect_rejects_hostname_prefix_collisions() {
        let spoke = peer("mac", "mac.tailnet.ts.net.");
        assert!(!host_matches(&spoke, "macbook.tailnet.ts.net"));
        assert!(!host_matches(&spoke, "macbook"));
        let book = peer("macbook", "macbook.tailnet.ts.net.");
        assert!(!host_matches(&book, "mac"));
        assert!(!host_matches(&book, "mac.tailnet.ts.net"));
    }

    #[test]
    fn dns_first_label_strips_suffix() {
        assert_eq!(dns_first_label("minis.tailnet.ts.net"), "minis");
        assert_eq!(dns_first_label("minis."), "minis");
        assert_eq!(dns_first_label("minis"), "minis");
    }
}
