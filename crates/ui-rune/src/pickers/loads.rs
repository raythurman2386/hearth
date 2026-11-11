//! Loading the pickers' RPC-backed slots: harness catalogs, model lists, refs
//! and the chat-config write path.
//!
//! Rune has no `cx.spawn` returning into an entity: background tasks post a
//! [`PickersEvent`] down the state's channel and [`PickersState::poll`] applies
//! it on the UI thread. The engine/bridge snapshot lives in the state so a
//! follow-up load (models after a catalog, refs after a switch) needs no `App`.

use hearth_engine::registry::HarnessDescriptor;
use hearth_proto::{ChatConfig, HarnessId, Model, RepoRef};

use hearth_rpc::methods;

use crate::state::App;

use super::model;
use super::{PickersEvent, PickersState, selection};

/// The catalog target helper, also reachable as `pickers::loads::space_target`.
pub use super::checkout::space_target;

/// Snapshot the engine/bridge/space context into the picker state before a
/// load. A free function because `app.pickers.refresh(app)` would need a shared
/// borrow of `App` alongside the mutable borrow of its own field.
pub(crate) fn refresh_context(app: &mut App) {
    let bridge = app.bridge.clone();
    let engine = app.engine.clone();
    let target = super::checkout::space_target(app);
    let space = app.selected_space_row().cloned();
    let local = app.local_device_id.clone();
    app.pickers
        .set_load_context(bridge, engine, target, space, local);
}

// ---------------------------------------------------------------------------
// Loadable
// ---------------------------------------------------------------------------

/// One async-loaded slot. `Ready` data stays visible while a forced refresh
/// reloads (stale-while-revalidate); [`Loadable::Error`] never re-triggers a
/// non-forced load (otherwise the retry row would flash back to a skeleton).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loadable<T> {
    Idle,
    Loading,
    Ready(T),
    Error(String),
}

impl<T> Default for Loadable<T> {
    fn default() -> Self {
        Self::Idle
    }
}

impl<T> Loadable<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn ready_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Error(message) => Some(message),
            _ => None,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

// ---------------------------------------------------------------------------
// Per-frame entry point
// ---------------------------------------------------------------------------

/// The shell's per-frame picker hook: install the sticky defaults on the first
/// frame, re-home the draft when the selected chat/space changed, and kick the
/// eager loads (harness catalog, every offered harness's models, refs for the
/// footer). Returns true when visible state changed.
///
/// Call from `App::poll` after `app.pickers.poll()`:
///
/// ```ignore
/// let mut changed = app.drain_events();
/// changed |= app.pickers.poll();
/// changed |= hearth_ui_rune::pickers::pump(app);
/// ```
pub fn pump(app: &mut App) -> bool {
    let mut changed = false;
    if !app.pickers.initialised {
        let data_dir = app.data_dir.clone();
        app.pickers.install(data_dir);
        // Restore the last device/project picks (the canvas's "defaults to
        // last selected" rule). A remembered "Don't work in a project"
        // opt-out is deliberately NOT restored: the menu row is gone, so a
        // stale saved opt-out would strand the canvas in a state the picker
        // can no longer express.
        let device = app.pickers.defaults.device.clone();
        let project = app.pickers.defaults.project.clone();
        if app.selected_device.is_none() {
            app.selected_device = device;
        }
        if app.selected_space.is_none() && !app.no_project {
            app.selected_space = project;
        }
        app.pickers.draft_owner = app.selected_chat.clone();
        app.pickers.space_owner = if app.no_project {
            None
        } else {
            app.selected_space.clone()
        };
        changed = true;
    }
    let chat = app.selected_chat.clone();
    let space = app.selected_space.clone();
    let no_project = app.no_project;
    changed |= app.pickers.sync_owners(chat, space, no_project);
    changed |= ensure_harnesses(app, false);
    changed |= prefetch_models(app, false);
    // Refs only feed the new-session footer: an existing chat's footer is
    // read-only, so don't poll git for it.
    if app.selected_chat.is_none() {
        changed |= ensure_refs(app, false);
    }
    changed
}

/// Settings → Agents changed some device's enabled set: drop the cached
/// catalog and refetch from the engine (the source of truth), keeping current
/// rows visible until the fresh list lands.
pub fn bump_harness_catalog(app: &mut App) {
    app.pickers.harnesses = Loadable::Idle;
    app.pickers.models.clear();
    app.pickers.catalog_rev += 1;
    ensure_harnesses(app, false);
}

// ---------------------------------------------------------------------------
// Harness catalog
// ---------------------------------------------------------------------------

/// Load the harness catalog for the space's device. Non-forced calls (the
/// per-frame eager kick) load only from `Idle`; forced refreshes (a picker
/// open, a Settings → Agents toggle) also reload `Ready`/`Error` slots —
/// stale-while-revalidate keeps the current rows on screen.
pub fn ensure_harnesses(app: &mut App, force: bool) -> bool {
    let reload = match app.pickers.harnesses {
        Loadable::Idle => true,
        Loadable::Loading => false,
        Loadable::Ready(_) | Loadable::Error(_) => force,
    };
    if !reload || app.pickers.engine.is_none() {
        refresh_context(app);
        return false;
    }
    refresh_context(app);
    if app.pickers.engine.is_none() {
        return false;
    }
    if !matches!(app.pickers.harnesses, Loadable::Ready(_)) {
        app.pickers.harnesses = Loadable::Loading;
        app.pickers.catalog_rev += 1;
    }
    app.pickers.spawn_harness_load();
    true
}

impl PickersState {
    pub(crate) fn spawn_harness_load(&mut self) {
        let (Some(bridge), Some(engine)) = (self.bridge.clone(), self.engine.clone()) else {
            return;
        };
        let tx = self.tx.clone();
        let target = self.catalog_target.clone();
        bridge.spawn(async move {
            let mut params = serde_json::Map::new();
            if let Some(target) = &target {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client
                .call(methods::LIST_HARNESSES, serde_json::Value::Object(params))
                .await
                .map_err(|err| err.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<HarnessDescriptor>>(value)
                        .map_err(|err| err.to_string())
                });
            let _ = tx.send(PickersEvent::Harnesses(result));
        });
    }
}

// ---------------------------------------------------------------------------
// Model lists
// ---------------------------------------------------------------------------

/// Kick a model load for the effective harness AND every offered one, in
/// parallel — by the time the user opens the picker (or switches rail tabs)
/// the lists are already there. Each [`ensure_models`] call is guarded by its
/// slot state, so re-running this every frame is free.
pub fn prefetch_models(app: &mut App, force: bool) -> bool {
    let mut targets: Vec<HarnessId> = match app.pickers.harnesses.ready() {
        Some(list) => model::offered_harnesses(list)
            .iter()
            .map(|d| d.id)
            .collect(),
        None => Vec::new(),
    };
    // The committed chat's harness may be outside the offered set (e.g.
    // disabled after the chat was created) — its models still matter.
    if let Some(effective) = app.pickers.effective_harness(app)
        && !targets.contains(&effective)
    {
        targets.push(effective);
    }
    let mut spawned = false;
    for harness in targets {
        spawned |= ensure_models(app, harness, force);
    }
    spawned
}

/// Load one harness's models. Normal prefetches load absent/`Idle` slots once;
/// picker-open refreshes also retry `Ready`/`Error`, while an in-flight load is
/// always reused. Ready rows stay visible until the replacement lands.
pub fn ensure_models(app: &mut App, harness: HarnessId, force: bool) -> bool {
    let reload = match app.pickers.models.get(&harness) {
        None | Some(Loadable::Idle) => true,
        Some(Loadable::Loading) => false,
        Some(Loadable::Ready(_)) | Some(Loadable::Error(_)) => force,
    };
    if !reload {
        refresh_context(app);
        return false;
    }
    refresh_context(app);
    if app.pickers.engine.is_none() {
        return false;
    }
    if !matches!(app.pickers.models.get(&harness), Some(Loadable::Ready(_))) {
        app.pickers.models.insert(harness, Loadable::Loading);
        app.pickers.catalog_rev += 1;
    }
    app.pickers.spawn_model_load(harness);
    true
}

impl PickersState {
    pub(crate) fn spawn_model_load(&mut self, harness: HarnessId) {
        let (Some(bridge), Some(engine)) = (self.bridge.clone(), self.engine.clone()) else {
            return;
        };
        let tx = self.tx.clone();
        let target = self.catalog_target.clone();
        bridge.spawn(async move {
            let mut params = serde_json::json!({ "harness": harness });
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client
                .call(methods::LIST_MODELS, params)
                .await
                .map_err(|err| err.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<Model>>(value).map_err(|err| err.to_string())
                });
            let _ = tx.send(PickersEvent::Models { harness, result });
        });
    }

    /// The poll-time prefetch (no `App` available): every offered harness.
    pub(crate) fn prefetch_models_self(&mut self, force: bool) {
        let targets: Vec<HarnessId> = match self.harnesses.ready() {
            Some(list) => model::offered_harnesses(list)
                .iter()
                .map(|d| d.id)
                .collect(),
            None => Vec::new(),
        };
        for harness in targets {
            let reload = match self.models.get(&harness) {
                None | Some(Loadable::Idle) => true,
                Some(Loadable::Loading) => false,
                Some(Loadable::Ready(_)) | Some(Loadable::Error(_)) => force,
            };
            if !reload {
                continue;
            }
            if !matches!(self.models.get(&harness), Some(Loadable::Ready(_))) {
                self.models.insert(harness, Loadable::Loading);
                self.catalog_rev += 1;
            }
            self.spawn_model_load(harness);
        }
    }
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

/// `ListRefs` for the selected SPACE's folder — targeted at the space's device
/// (relay-forwarded when remote), keyed/invalidated by space id. Rows carry
/// checkout state (`current`, `worktreePath`) so the picker can tag refs and
/// the checkout-kind selector can offer worktree reuse.
pub fn ensure_refs(app: &mut App, force: bool) -> bool {
    let Some(space) = app.selected_space_row().cloned() else {
        app.pickers.space_snapshot = None;
        return false;
    };
    if !space.git_detected {
        return false;
    }
    let fresh = app.pickers.refs_space.as_deref() == Some(space.id.as_str());
    if fresh && matches!(app.pickers.refs, Loadable::Loading) {
        return false; // a load is already in flight
    }
    // Non-forced (the footer's eager kick, re-run every frame) only loads from
    // Idle: an Error must WAIT for an explicit retry/reopen (force), or a
    // re-render would flip Error back to Loading before the retry row ever
    // paints — an eternal skeleton plus an RPC storm.
    if !force && fresh && !matches!(app.pickers.refs, Loadable::Idle) {
        return false;
    }
    refresh_context(app);
    if app.pickers.engine.is_none() {
        return false;
    }
    // Stale-while-revalidate: a forced refresh of an already-loaded space keeps
    // current rows on screen while the reload runs.
    if !(force && fresh && matches!(app.pickers.refs, Loadable::Ready(_))) {
        app.pickers.refs = Loadable::Loading;
    }
    app.pickers.refs_space = Some(space.id.clone());
    app.pickers.space_snapshot = Some(space);
    app.pickers.spawn_refs_load();
    true
}

impl PickersState {
    pub(crate) fn spawn_refs_load(&mut self) {
        let (Some(bridge), Some(engine)) = (self.bridge.clone(), self.engine.clone()) else {
            return;
        };
        let Some(space) = self.space_snapshot.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let local = self.local_device.clone();
        let space_id = space.id.clone();
        bridge.spawn(async move {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client
                .call(methods::LIST_REFS, serde_json::Value::Object(params))
                .await
                .map_err(|err| err.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<RepoRef>>(value).map_err(|err| err.to_string())
                });
            let _ = tx.send(PickersEvent::Refs {
                space: space_id,
                result,
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Chat config writes
// ---------------------------------------------------------------------------

/// Apply `change` to the selected chat's effective config and persist it:
/// optimistic row stamp (chips update on click) + `Mutate setChatConfig` (LWW
/// workspace write — restarts and other devices see it). The written row always
/// carries the CONCRETE resolved model/reasoning, with the reasoning re-clamped
/// to the (possibly just-changed) model's ladder.
pub fn update_chat_config(app: &mut App, change: impl FnOnce(&mut ChatConfig)) {
    let Some(chat_id) = app.selected_chat.clone() else {
        return;
    };
    let resolved = app.pickers.resolved(app);
    let Some(mut config) = resolved.chat_config() else {
        return; // harness unknown (catalog + chat row both missing)
    };
    // Preserve fields the pickers don't own.
    let existing_sandbox = selection::selected_chat_row(app)
        .and_then(|c| c.config.as_ref())
        .map(|c| c.sandbox);
    if let Some(sandbox) = existing_sandbox {
        config.sandbox = sandbox;
    }
    change(&mut config);
    // Reasoning must stay concrete for whatever model the row now names — the
    // same ladder resolution as `trait_ladder` (model levels, else the
    // harness's advertised ladder).
    if let Some(models) = app
        .pickers
        .models
        .get(&config.harness)
        .and_then(|l| l.ready())
    {
        let mut ladder = config
            .model
            .as_deref()
            .and_then(|id| models.iter().find(|m| m.id == id))
            .map(|m| m.reasoning_levels.clone())
            .unwrap_or_default();
        if ladder.is_empty()
            && let Some(descriptor) = app
                .pickers
                .harnesses
                .ready()
                .and_then(|list| list.iter().find(|d| d.id == config.harness))
        {
            ladder = descriptor.reasoning_levels.clone();
        }
        if !ladder.is_empty() {
            config.reasoning = model::clamp_reasoning(config.reasoning, &ladder);
        }
    }
    app.apply_chat_config(&chat_id, config.clone());
    refresh_context(app);
    app.pickers.spawn_chat_config(chat_id, config);
}

impl PickersState {
    pub(crate) fn spawn_chat_config(&mut self, chat_id: String, config: ChatConfig) {
        let (Some(bridge), Some(engine)) = (self.bridge.clone(), self.engine.clone()) else {
            return;
        };
        let tx = self.tx.clone();
        bridge.spawn(async move {
            let params = serde_json::json!({
                "op": "setChatConfig",
                "chatId": chat_id,
                "config": config,
            });
            let result = engine
                .client
                .call(methods::MUTATE, params)
                .await
                .map(|_| ())
                .map_err(|err| err.to_string());
            let _ = tx.send(PickersEvent::ChatConfigSaved {
                chat_id: chat_id.clone(),
                result,
            });
        });
    }
}
