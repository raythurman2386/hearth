//! Auth — the engine's fixed device identity for local/tailnet runtimes.
//!
//! Hearth has no account service: this device IS the identity. The bearer a
//! runtime presents to the tailnet hub is the configured user id (a dummy
//! credential — the hub authorizes callers with `tailscale whois`), so there is
//! no token to exchange, refresh, or revoke and no organization to join.
//!
//! [`Auth`] still exposes a live [`AuthState`] watch so every viewport attaches
//! to one status shape, and implements [`hearth_rpc::TokenSource`] so the room
//! clients and peer link cache keep reading a fresh bearer on each (re)dial.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// The identity a client renders for the signed-in device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// AuthStatus stream payload. Serializes as the canonical
/// [`hearth_proto::AuthState`] wire shape (`{"state": "signedIn", …}`) so every
/// client parses one form.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthState {
    SignedOut,
    SignedIn { user: AuthUser },
}

impl AuthState {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, AuthState::SignedIn { .. })
    }

    /// The proto wire twin — the one shape the engine emits over AuthStatus.
    pub fn to_proto(&self) -> hearth_proto::AuthState {
        let profile = |user: &AuthUser| hearth_proto::UserProfile {
            id: user.id.clone(),
            email: user.email.clone(),
            name: user.name.clone(),
        };
        match self {
            AuthState::SignedOut => hearth_proto::AuthState::SignedOut,
            AuthState::SignedIn { user } => hearth_proto::AuthState::SignedIn {
                user: profile(user),
            },
        }
    }
}

impl Serialize for AuthState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_proto().serialize(serializer)
    }
}

/// The identity a runtime presents to the hub. There is nothing to configure
/// beyond the bearer/user id; the tailnet itself is the trust boundary.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The bearer/user id for hub room joins and peer RPC (the `HEARTH_EDGE_TOKEN`
    /// seam; defaults to `dev-user`).
    pub dev_user_id: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            dev_user_id: "dev-user".into(),
        }
    }
}

impl AuthConfig {
    pub fn new() -> Self {
        Self::default()
    }
}

struct AuthInner {
    config: AuthConfig,
    state_tx: watch::Sender<AuthState>,
    token_tx: watch::Sender<u64>,
}

/// The auth service — cheap to clone by `Arc`.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<AuthInner>,
}

impl Auth {
    /// Build the fixed device identity from config.
    pub fn new(config: AuthConfig) -> Self {
        let user = AuthUser {
            id: config.dev_user_id.clone(),
            email: config.dev_user_id.clone(),
            name: None,
        };
        let (state_tx, _) = watch::channel(AuthState::SignedIn { user });
        let (token_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(AuthInner {
                config,
                state_tx,
                token_tx,
            }),
        }
    }

    /// Live auth status (current value + changes).
    pub fn watch_state(&self) -> watch::Receiver<AuthState> {
        self.inner.state_tx.subscribe()
    }

    pub fn state(&self) -> AuthState {
        self.inner.state_tx.borrow().clone()
    }

    /// The signed-in user id — the identity that scopes workspace rooms
    /// (`ws/{orgId}` / `ws3/{orgId}/{userId}`) and local storage. A bare token
    /// IS the user id; `user@org` mirrors the historical dev-bearer parsing.
    pub fn user_id(&self) -> Option<String> {
        let dev = &self.inner.config.dev_user_id;
        Some(dev.split('@').next().unwrap_or(dev).to_string())
    }

    /// Current bearer for hub rooms / the device relay. The device identity is
    /// always present locally; there is no signed-out state that withholds it.
    pub async fn access_token(&self) -> Option<String> {
        Some(self.inner.config.dev_user_id.clone())
    }

    /// Mark the runtime signed out. The device identity and its bearer are
    /// unaffected (they are not credentials); the signal lets viewports agree
    /// on the status shape.
    pub fn sign_out(&self) {
        self.inner.state_tx.send_replace(AuthState::SignedOut);
        self.inner
            .token_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}

/// The relay/room token seam: `Auth` IS a [`hearth_rpc::TokenSource`], so the
/// host relay and link cache always dial with the current bearer.
#[async_trait::async_trait]
impl hearth_rpc::TokenSource for Auth {
    async fn token(&self) -> Option<String> {
        self.access_token().await
    }

    fn subscribe(&self) -> Option<watch::Receiver<u64>> {
        Some(self.inner.token_tx.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_state_serializes_as_proto_shape() {
        let user = AuthUser {
            id: "u1".into(),
            email: "u@x".into(),
            name: None,
        };
        let signed_in = AuthState::SignedIn { user: user.clone() };
        let value = serde_json::to_value(&signed_in).expect("json");
        assert_eq!(
            value,
            serde_json::json!({
                "state": "signedIn",
                "user": {"id": "u1", "email": "u@x", "name": null},
            })
        );
        // The proto type itself round-trips the emitted value.
        let parsed: hearth_proto::AuthState = serde_json::from_value(value).expect("proto parse");
        assert!(matches!(parsed, hearth_proto::AuthState::SignedIn { .. }));
        assert_eq!(
            serde_json::to_value(AuthState::SignedOut).expect("json"),
            serde_json::json!({"state": "signedOut"})
        );
    }
}
