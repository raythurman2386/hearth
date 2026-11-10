//! Auth service tests: the fixed device identity every local/tailnet runtime
//! presents to the hub.

use std::sync::Arc;

use hearth_engine::{Auth, AuthConfig, AuthState};
use hearth_rpc::TokenSource;

fn auth_with(user: &str) -> Arc<Auth> {
    let mut config = AuthConfig::new();
    config.dev_user_id = user.into();
    Arc::new(Auth::new(config))
}

#[tokio::test]
async fn dev_identity_is_signed_in_with_the_configured_bearer() {
    let auth = auth_with("wing-dev");
    assert!(matches!(auth.state(), AuthState::SignedIn { user } if user.id == "wing-dev"));
    assert_eq!(auth.user_id().as_deref(), Some("wing-dev"));
    assert_eq!(auth.access_token().await.as_deref(), Some("wing-dev"));
    assert_eq!(auth.token().await.as_deref(), Some("wing-dev"));
}

#[tokio::test]
async fn user_id_mirrors_the_historical_user_at_org_bearer_parsing() {
    let auth = auth_with("alice@acme");
    assert_eq!(auth.user_id().as_deref(), Some("alice"));
    assert_eq!(
        auth.access_token().await.as_deref(),
        Some("alice@acme"),
        "the bearer itself is unchanged"
    );
}

#[tokio::test]
async fn sign_out_flips_the_status_broadcast_without_withholding_the_bearer() {
    let auth = auth_with("dev-user");
    let mut state = auth.watch_state();
    assert!(state.borrow().is_signed_in());

    let mut token_changes = auth.subscribe().expect("token signal");
    auth.sign_out();

    state.changed().await.expect("state channel open");
    assert_eq!(*state.borrow(), AuthState::SignedOut);
    token_changes
        .changed()
        .await
        .expect("sign-out wakes transport supervisors");
    // The device identity is not a credential — room dials keep their bearer.
    assert_eq!(auth.token().await.as_deref(), Some("dev-user"));
}
