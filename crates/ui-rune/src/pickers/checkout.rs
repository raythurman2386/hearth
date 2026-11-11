//! The checkout plan: which ref/worktree a new session runs in, how the footer
//! labels it, and the mid-session `SwitchRef` write path.

use hearth_proto::RepoRef;

use hearth_rpc::methods;

use crate::state::App;

use super::model::{CheckoutKind, CheckoutPlan};
use super::{PickersEvent, PickersState, loads, post};

/// The ref pick lives beside the switch path it falls back to, so the checkout
/// module exposes the whole "pick a ref" surface: `pickers::checkout::pick_ref`.
pub use super::selection::pick_ref;

/// The selected space's device when it differs from the connected engine's own
/// — harness/model catalogs come from the device that RUNS the agents (the CLIs
/// live there; the viewer may have neither huginn nor codex installed).
pub fn space_target(app: &App) -> Option<String> {
    let device = app.selected_space_row()?.device_id.clone();
    (app.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
}

impl PickersState {
    /// The resolved on-send checkout action for a new session.
    pub fn checkout_plan(&self) -> CheckoutPlan {
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_name(),
            },
            CheckoutKind::Local => match self.selected_ref_worktree() {
                Some(path) => CheckoutPlan::ReuseWorktree {
                    path,
                    branch: self.effective_ref_name().unwrap_or_default(),
                },
                None => CheckoutPlan::CurrentCheckout {
                    branch: self.effective_ref_name(),
                },
            },
        }
    }

    /// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel` /
    /// `resolveCurrentWorkspaceLabel`).
    pub fn checkout_label(&self) -> &'static str {
        match self.config.checkout {
            CheckoutKind::NewWorktree => "New worktree",
            CheckoutKind::Local => {
                if self.selected_ref_worktree().is_some() {
                    "Current worktree"
                } else {
                    "Current checkout"
                }
            }
        }
    }

    pub(crate) fn spawn_switch(&mut self, ref_name: String) {
        let (Some(bridge), Some(engine)) = (self.bridge.clone(), self.engine.clone()) else {
            return;
        };
        let Some(space) = self.space_snapshot.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let wake = bridge.clone();
        let local = self.local_device.clone();
        bridge.spawn(async move {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            params.insert(
                "refName".into(),
                serde_json::Value::String(ref_name.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await
                .map(|_| ())
                .map_err(|err| err.to_string());
            post(
                &tx,
                &wake,
                PickersEvent::Switched {
                    name: ref_name,
                    result,
                },
            );
        });
    }
}

/// Draft-mode checkout switch: `git checkout` in the SPACE's folder
/// (relay-forwarded for remote spaces). Success records the pick and refreshes
/// tags; failure keeps the popover open with git's message.
pub fn switch_draft_ref(app: &mut App, row: RepoRef) {
    if app.pickers.switching.is_some() {
        return; // one switch at a time
    }
    loads::refresh_context(app);
    if app.pickers.engine.is_none() || app.pickers.space_snapshot.is_none() {
        return;
    }
    app.pickers.switch_error = None;
    app.pickers.switching = Some(row.name.clone());
    app.pickers.spawn_switch(row.name);
}

pub fn pick_checkout(app: &mut App, kind: CheckoutKind) {
    if kind == CheckoutKind::Local
        && app.pickers.config.checkout == CheckoutKind::NewWorktree
        && app.pickers.selected_ref_worktree().is_none()
        && app.pickers.selected_ref().is_some_and(|r| !r.current)
    {
        // Back to "Current checkout" with a non-current plain ref picked: drop
        // the pick (we don't checkout the main folder) — the current branch
        // takes over.
        app.pickers.config.branch = None;
    }
    app.pickers.config.checkout = kind;
    app.pickers.open = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickers::Loadable;
    use hearth_proto::RepoRef;

    fn state_with_refs(refs: Vec<RepoRef>) -> PickersState {
        PickersState {
            refs: Loadable::Ready(refs),
            ..PickersState::default()
        }
    }

    fn bare(name: &str, current: bool, worktree: Option<&str>) -> RepoRef {
        RepoRef {
            name: name.into(),
            current,
            worktree_path: worktree.map(str::to_string),
        }
    }

    #[test]
    fn current_checkout_carries_the_current_branch() {
        let state = state_with_refs(vec![bare("main", true, None), bare("feature", false, None)]);
        assert_eq!(state.checkout_label(), "Current checkout");
        assert_eq!(
            state.checkout_plan(),
            CheckoutPlan::CurrentCheckout {
                branch: Some("main".into())
            }
        );
    }

    #[test]
    fn a_picked_worktree_reuses_its_path() {
        let mut state = state_with_refs(vec![
            bare("main", true, None),
            bare("feature", false, Some("/tmp/wt-feature")),
        ]);
        state.config.branch = Some("feature".into());
        assert_eq!(state.checkout_label(), "Current worktree");
        assert_eq!(
            state.checkout_plan(),
            CheckoutPlan::ReuseWorktree {
                path: "/tmp/wt-feature".into(),
                branch: "feature".into(),
            }
        );
    }

    #[test]
    fn new_worktree_carries_the_base_ref_and_labels_from() {
        let mut state =
            state_with_refs(vec![bare("main", true, None), bare("release", false, None)]);
        state.config.checkout = CheckoutKind::NewWorktree;
        state.config.branch = Some("release".into());
        assert_eq!(state.checkout_label(), "New worktree");
        assert_eq!(state.ref_label(), "From release");
        assert_eq!(
            state.checkout_plan(),
            CheckoutPlan::NewWorktree {
                base: Some("release".into())
            }
        );
    }

    #[test]
    fn missing_refs_fall_back_to_the_space_folder() {
        let state = PickersState::default();
        assert_eq!(state.checkout_label(), "Current checkout");
        assert_eq!(state.ref_label(), "Select ref");
        assert_eq!(
            state.checkout_plan(),
            CheckoutPlan::CurrentCheckout { branch: None }
        );
        let mut new = PickersState::default();
        new.config.checkout = CheckoutKind::NewWorktree;
        assert_eq!(
            new.checkout_plan(),
            CheckoutPlan::NewWorktree { base: None }
        );
    }
}
