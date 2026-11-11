//! Composer pickers: the harness/model picker (brand rail + model rows), the
//! traits picker (reasoning ladder + advertised model options), the interaction
//! mode picker, the branch/checkout pickers, and the new-session device/project
//! selectors.
//!
//! This is the Rune port of `crates/ui/src/pickers` (gpui). The module owns
//! everything under `pickers/**`:
//!
//! - [`model`] — pure config/default/path logic (no framework types).
//! - [`selection`] — effective harness/model/mode/reasoning resolution, the
//!   flattened model rows and the pick handlers.
//! - [`loads`] — [`Loadable`] slots, RPC loads with stale-while-revalidate, the
//!   optimistic `setChatConfig` write path, and [`pump`] (the per-frame eager
//!   loader/owner sync).
//! - [`checkout`] — `SwitchRef`, the on-send checkout plan, labels.
//! - [`space_picker`] — project/device rows, ordering and picks.
//! - [`render`] — the composer chips/footer and the popover layer.
//! - [`defaults`] — the sticky `composer-defaults.json` store.
//!
//! ## Wiring (lead)
//!
//! ```ignore
//! // App::poll, once per frame:
//! changed |= app.pickers.poll();          // drain pickers RPC results
//! changed |= pickers::pump(app);          // init + owner sync + eager loads
//!
//! // wherever the composer actions are drawn:
//! pickers::render_cluster(&app.pickers, app, &app.theme)
//! // and after the root body is built:
//! pickers::popovers(&app.pickers, app, &app.theme, &mut layer)
//! ```
//!
//! Background work never touches [`crate::state::App`]: every load posts a
//! [`PickersEvent`] down the channel this state owns, and [`PickersState::poll`]
//! applies it on the UI thread.

pub mod checkout;
pub mod defaults;
pub mod loads;
pub mod model;
pub mod render;
pub mod selection;
pub mod space_picker;

pub use checkout::space_target;
pub use defaults::{ComposerDefaults, FavoriteModel, RememberedModel};
pub use loads::{
    Loadable, bump_harness_catalog, ensure_harnesses, ensure_models, ensure_refs, prefetch_models,
    pump,
};
pub use model::{
    CheckoutKind, CheckoutPlan, DraftConfig, ResolvedRunConfig, clamp_reasoning, default_model,
    default_reasoning, mode_label, reasoning_label, traits_customized, traits_summary,
    visible_harnesses,
};
pub use render::{
    popovers, render_cluster, render_footer, render_new_session, set_composer_width, toggle_picker,
};

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use hearth_engine::registry::HarnessDescriptor;
pub use hearth_proto::{ChatConfig, SandboxLevel};

use hearth_proto::{HarnessId, Model, RepoRef, Space};

use rune::TextEdit;

use crate::bridge::Bridge;
use crate::engine::EngineHandle;

/// The picker popover that is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Branch,
    /// The checkout-kind dropdown in the composer footer (Current
    /// checkout/worktree | New worktree).
    Checkout,
    HarnessModel,
    Traits,
    /// The interaction-mode picker (plan/agent/chat).
    Mode,
    /// New-session canvas only: which project the session mints into. A pick
    /// re-keys everything project-derived (refs, harness/model catalogs).
    Space,
    /// New-session canvas only: the device project-less sessions run on (a
    /// project pick implies its own host and overrides this).
    Device,
}

/// Which pane the harness/model picker's icon rail is showing. `Harness` means
/// "the effective harness's list" — the rail has no browse-without-commit
/// state; clicking a brand icon picks that harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelRail {
    Favorites,
    #[default]
    Harness,
}

/// One row of the model list: the model plus the harness it belongs to —
/// search results and the favorites view mix harnesses, and every row's
/// subline names its harness.
#[derive(Debug, Clone)]
pub struct ModelRowData {
    pub harness: HarnessId,
    pub harness_name: String,
    pub model: Model,
}

/// Cache key for the flattened model-row list: any input that changes the
/// list's CONTENT (not its highlight/selection, which render per-row).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ModelRowsKey {
    query: String,
    rail: ModelRail,
    effective: Option<HarnessId>,
    locked: bool,
    catalog_rev: u64,
}

/// The result of one background picker RPC, delivered to the UI thread.
pub enum PickersEvent {
    Harnesses(Result<Vec<HarnessDescriptor>, String>),
    Models {
        harness: HarnessId,
        result: Result<Vec<Model>, String>,
    },
    Refs {
        space: String,
        result: Result<Vec<RepoRef>, String>,
    },
    Switched {
        name: String,
        result: Result<(), String>,
    },
    ChatConfigSaved {
        chat_id: String,
        result: Result<(), String>,
    },
}

/// All picker state: the draft config, sticky defaults, load slots, search /
/// keyboard cursor and the open popover.
pub struct PickersState {
    pub config: DraftConfig,
    pub defaults: ComposerDefaults,
    pub data_dir: Option<PathBuf>,
    /// Selection the draft picks belong to — switching chats drops them so a
    /// pick made in one chat never leaks into another.
    pub(crate) draft_owner: Option<String>,
    /// Space the branch draft/cache belong to.
    pub(crate) space_owner: Option<String>,
    /// The popover that is open (or that was just opened by
    /// `HEARTH_OPEN_PICKER`).
    pub open: Option<PickerKind>,
    /// The harness/model picker's rail selection.
    pub model_rail: ModelRail,
    pub harnesses: Loadable<Vec<HarnessDescriptor>>,
    pub models: HashMap<HarnessId, Loadable<Vec<Model>>>,
    pub refs: Loadable<Vec<RepoRef>>,
    /// Space id the `refs` slot belongs to (invalidated on space change).
    pub refs_space: Option<String>,
    /// Highlighted row in the open list (keyboard nav).
    pub active: usize,
    /// The shared search / URL / name field, filtered per popover.
    pub search: TextEdit,
    /// The traits chip's compact overflow presentation (measured width).
    pub compact_traits: bool,
    /// Bumped on every catalog/favorites mutation; invalidates the rows cache.
    pub catalog_rev: u64,
    /// In-flight mid-session `SwitchRef` (the ref being switched to).
    pub switching: Option<String>,
    /// Last mid-session switch failure (shown in the ref popover).
    pub switch_error: Option<String>,
    /// Set by the project popover's "New project…" row; the shell consumes it
    /// with [`PickersState::take_new_project_request`] and opens its add-space
    /// palette. (Rune has no gpui-style action dispatch for this.)
    pub new_project_requested: bool,

    pub(crate) rows_cache: RefCell<Option<(ModelRowsKey, Rc<Vec<ModelRowData>>)>>,
    /// Load plumbing: the channel producers post to, and the engine/bridge
    /// snapshot the consumer uses for follow-up loads (model prefetch after a
    /// catalog lands, refs after a switch). Background tasks only hold clones.
    pub(crate) tx: Sender<PickersEvent>,
    pub(crate) rx: Receiver<PickersEvent>,
    pub(crate) bridge: Option<Bridge>,
    pub(crate) engine: Option<EngineHandle>,
    pub(crate) catalog_target: Option<String>,
    pub(crate) space_snapshot: Option<Space>,
    pub(crate) local_device: Option<String>,
    /// `pump` has loaded the sticky defaults for `data_dir` once.
    pub(crate) initialised: bool,
}

impl Default for PickersState {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        // Dev/testing knob: `HEARTH_OPEN_PICKER=model|traits|branch|checkout|
        // project|device` boots with that popover open — synthetic input can't
        // reach the app on headless compositors, so captures need a data-side
        // path.
        let boot_open = match std::env::var("HEARTH_OPEN_PICKER").ok().as_deref() {
            Some("model") => Some(PickerKind::HarnessModel),
            Some("traits") => Some(PickerKind::Traits),
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            Some("project") => Some(PickerKind::Space),
            Some("device") => Some(PickerKind::Device),
            _ => None,
        };
        Self {
            config: DraftConfig::default(),
            defaults: ComposerDefaults::default(),
            data_dir: None,
            draft_owner: None,
            space_owner: None,
            open: boot_open,
            model_rail: ModelRail::default(),
            harnesses: Loadable::Idle,
            models: HashMap::new(),
            refs: Loadable::Idle,
            refs_space: None,
            active: 0,
            search: TextEdit::new(""),
            compact_traits: false,
            catalog_rev: 0,
            switching: None,
            switch_error: None,
            new_project_requested: false,
            rows_cache: RefCell::new(None),
            tx,
            rx,
            bridge: None,
            engine: None,
            catalog_target: None,
            space_snapshot: None,
            local_device: None,
            initialised: false,
        }
    }
}

impl PickersState {
    /// [`Default`] plus the sticky defaults loaded from `data_dir` (when
    /// known). The shell state uses `Default`; [`pump`] upgrades it on the
    /// first frame via [`Self::install`].
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        let mut state = Self::default();
        state.install(data_dir);
        state
    }

    pub(crate) fn install(&mut self, data_dir: Option<PathBuf>) {
        if self.initialised {
            return;
        }
        self.data_dir = data_dir;
        if let Some(dir) = self.data_dir.as_deref() {
            self.defaults = ComposerDefaults::load(dir);
        }
        self.initialised = true;
    }

    /// A sender for background picker loads (a clone per spawned task).
    pub fn sender(&self) -> Sender<PickersEvent> {
        self.tx.clone()
    }

    /// Persist the sticky defaults (best-effort; picks are rare and tiny).
    pub(crate) fn save_defaults(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    /// Re-home the draft when the selected chat or space changes. Called from
    /// [`pump`]; the Rune port has no `observe` graph, so the shell must run
    /// `pump` every frame (one cheap comparison when nothing moved).
    pub(crate) fn sync_owners(
        &mut self,
        selected_chat: Option<String>,
        selected_space: Option<String>,
        no_project: bool,
    ) -> bool {
        let mut changed = false;
        if selected_chat != self.draft_owner {
            self.draft_owner = selected_chat;
            self.config.harness = None;
            self.config.model = None;
            self.config.mode = None;
            self.config.reasoning = None;
            self.config.model_options.clear();
            self.switch_error = None;
            changed = true;
        }
        // A space switch invalidates the branch draft + cache — the folder
        // (and possibly the device) changed under them. Selecting "no project"
        // is a space change too: the target is gone.
        let space_owner = if no_project { None } else { selected_space };
        if space_owner != self.space_owner {
            self.space_owner = space_owner;
            self.config.branch = None;
            self.config.checkout = CheckoutKind::default();
            self.refs = Loadable::Idle;
            self.refs_space = None;
            // Catalogs are per-DEVICE (fetched from the space's host): a space
            // switch may land on another device, so refetch.
            self.harnesses = Loadable::Idle;
            self.models.clear();
            self.catalog_rev += 1;
            changed = true;
        }
        changed
    }

    /// Add a newly loaded label cache and save only when it changed.
    pub(crate) fn remember_labels<'a>(&mut self, models: impl Iterator<Item = (&'a str, &'a str)>) {
        if self.defaults.remember_labels(models) {
            self.save_defaults();
        }
    }

    /// Ask the shell to open its add-space palette (the project popover's
    /// "New project…" row). Returns true and clears the request.
    pub fn take_new_project_request(&mut self) -> bool {
        std::mem::take(&mut self.new_project_requested)
    }

    /// Feed the composer's live measured width into the actions presentation.
    /// Threshold-crossing only, so continuous resize does not churn renders.
    pub fn set_composer_width(&mut self, width: f32) -> bool {
        let compact = model::compact_traits_for_width(width, self.compact_traits);
        if compact == self.compact_traits {
            return false;
        }
        self.compact_traits = compact;
        true
    }

    /// Drain results posted by background picker loads. Returns true when any
    /// state changed (the shell then re-renders).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            changed = true;
            match event {
                PickersEvent::Harnesses(result) => {
                    self.harnesses = match result {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err),
                    };
                    self.catalog_rev += 1;
                    // The catalog decides which harnesses can run, so prefetch
                    // their model lists right away. A follow-up load needs the
                    // stored engine/bridge snapshot, not `App`.
                    self.prefetch_models_self(false);
                }
                PickersEvent::Models { harness, result } => {
                    let loaded = match result {
                        Ok(models) => Loadable::Ready(model::normalize_model_rows(harness, models)),
                        Err(err) => Loadable::Error(err),
                    };
                    if let Loadable::Ready(models) = &loaded {
                        let labels: Vec<(String, String)> = models
                            .iter()
                            .map(|m| (m.id.clone(), m.label.clone()))
                            .collect();
                        self.remember_labels(
                            labels
                                .iter()
                                .map(|(id, label)| (id.as_str(), label.as_str())),
                        );
                    }
                    self.models.insert(harness, loaded);
                    self.catalog_rev += 1;
                    // A list that landed while its popover is open re-anchors
                    // the keyboard highlight onto the selected row.
                    if self.open == Some(PickerKind::HarnessModel) {
                        self.active = self.selected_model_index_self();
                    }
                }
                PickersEvent::Refs { space, result } => {
                    if self.refs_space.as_deref() != Some(space.as_str()) {
                        continue; // a stale answer for a previous space
                    }
                    self.refs = match result {
                        Ok(refs) => Loadable::Ready(refs),
                        Err(err) => Loadable::Error(err),
                    };
                    // Rows landed under an open, un-searched popover: re-home
                    // the nav highlight to the selected row.
                    if self.open == Some(PickerKind::Branch) && self.search.text.is_empty() {
                        self.active = self.selected_ref_index_self();
                    }
                }
                PickersEvent::Switched { name, result } => {
                    self.switching = None;
                    match result {
                        Ok(()) => {
                            self.config.branch = Some(name);
                            self.open = None;
                            // The next `pump` sees an Idle refs slot for a
                            // fresh space and revalidates (the switch changed
                            // every row's `current` tag).
                            self.refs = Loadable::Idle;
                            self.refs_space = None;
                        }
                        Err(err) => self.switch_error = Some(err),
                    }
                }
                PickersEvent::ChatConfigSaved { chat_id, result } => {
                    if let Err(err) = result {
                        tracing::warn!(chat = %chat_id, error = %err, "setChatConfig mutate failed");
                    }
                }
            }
        }
        changed
    }

    /// The selected space's device when it differs from this engine's own —
    /// harness/model catalogs come from the device that RUNS the agents.
    ///
    /// Call through [`loads::refresh_context`], which snapshots `App` first:
    /// the state cannot borrow `App` while it is borrowed mutably through
    /// `app.pickers`.
    pub(crate) fn set_load_context(
        &mut self,
        bridge: Bridge,
        engine: Option<EngineHandle>,
        catalog_target: Option<String>,
        space_snapshot: Option<Space>,
        local_device: Option<String>,
    ) {
        self.bridge = Some(bridge);
        self.engine = engine;
        self.catalog_target = catalog_target;
        self.space_snapshot = space_snapshot;
        self.local_device = local_device;
    }
}

// ---------------------------------------------------------------------------
// Pure search helpers (shared by every picker)
// ---------------------------------------------------------------------------

/// Step the active row of a menu: wraps at both ends; `None` enters at the edge
/// matching the direction. Empty menus stay `None`.
pub fn menu_step(active: Option<usize>, count: usize, delta: isize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let count_i = count as isize;
    let next = match active {
        None => {
            if delta >= 0 {
                0
            } else {
                count_i - 1
            }
        }
        Some(at) => (at as isize + delta).rem_euclid(count_i),
    };
    Some(next as usize)
}

/// Match rank of a label against a query: `0` prefix match, `1` substring,
/// `None` no match. Case-insensitive; an empty query matches everything at
/// rank 1 (input order preserved).
pub fn match_rank(query: &str, label: &str) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(1);
    }
    let label = label.to_lowercase();
    if label.starts_with(&query) {
        Some(0)
    } else if label.contains(&query) {
        Some(1)
    } else {
        None
    }
}

/// Filter + rank labels for a search query: prefix matches first, then
/// substring matches, stable within each rank. Returns indices into `labels`.
pub fn filter_indices<S: AsRef<str>>(query: &str, labels: &[S]) -> Vec<usize> {
    let mut ranked: Vec<(usize, usize)> = labels
        .iter()
        .enumerate()
        .filter_map(|(ix, label)| match_rank(query, label.as_ref()).map(|rank| (rank, ix)))
        .collect();
    ranked.sort_by_key(|&(rank, ix)| (rank, ix));
    ranked.into_iter().map(|(_, ix)| ix).collect()
}

/// Rank one (harness, model) catalog entry for the model search: the label
/// ladder first (`0` prefix, `1` substring), then the wider haystack (harness
/// name, description, label) two ranks behind, then favorites, then input
/// order. Pure so the ranking can be tested without a UI.
pub(crate) fn search_row_rank(
    query: &str,
    label: &str,
    wider: &str,
    favorite: bool,
) -> Option<(usize, usize)> {
    let by_label = match_rank(query, label);
    let by_wider = match_rank(query, wider).map(|rank| rank + 2);
    by_label
        .into_iter()
        .chain(by_wider)
        .min()
        .map(|rank| (rank, (!favorite) as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_steps_wrap_and_enter_at_edges() {
        assert_eq!(menu_step(None, 0, 1), None);
        assert_eq!(menu_step(None, 3, 1), Some(0));
        assert_eq!(menu_step(None, 3, -1), Some(2));
        assert_eq!(menu_step(Some(0), 3, 1), Some(1));
        assert_eq!(menu_step(Some(2), 3, 1), Some(0));
        assert_eq!(menu_step(Some(0), 3, -1), Some(2));
        assert_eq!(menu_step(Some(1), 3, -1), Some(0));
    }

    #[test]
    fn search_ranking_is_prefix_then_substring_then_wider() {
        // Prefix beats substring.
        assert_eq!(match_rank("op", "Opus"), Some(0));
        assert_eq!(match_rank("pu", "Opus"), Some(1));
        assert_eq!(match_rank("zz", "Opus"), None);
        // Empty query preserves order.
        assert_eq!(match_rank("", "anything"), Some(1));
        assert_eq!(filter_indices("", &["b", "a"]), vec![0, 1]);
        assert_eq!(
            filter_indices("a", &["banana", "apple", "cherry"]),
            vec![1, 0]
        );
        // A wider-haystack hit ranks behind every label hit.
        assert_eq!(
            search_row_rank("anthropic", "GLM-5.2", "Anthropic GLM-5.2", false),
            Some((2, 1))
        );
        assert_eq!(
            search_row_rank("glm", "GLM-5.2", "Anthropic GLM-5.2", true),
            Some((0, 0))
        );
        assert_eq!(
            search_row_rank("nope", "GLM-5.2", "Anthropic GLM-5.2", false),
            None
        );
        // Favorites only break ties.
        assert_eq!(search_row_rank("glm", "GLM-5.2", "", false), Some((0, 1)));
    }

    #[test]
    fn compact_width_hysteresis_lives_in_model() {
        // A zero width (not measured yet) keeps the current presentation.
        assert!(!model::compact_traits_for_width(0.0, false));
        assert!(model::compact_traits_for_width(0.0, true));
        assert!(model::compact_traits_for_width(399.0, false));
        assert!(!model::compact_traits_for_width(400.0, false));
        assert!(model::compact_traits_for_width(439.0, true));
        assert!(!model::compact_traits_for_width(440.0, true));
    }

    #[test]
    fn owners_reset_the_draft_on_chat_and_space_changes() {
        let mut state = PickersState::default();
        // The first pump stamps the current owners; edits after that are what
        // the sync sees.
        assert!(state.sync_owners(Some("c1".into()), Some("s1".into()), false));
        state.config.branch = Some("main".into());
        state.config.checkout = CheckoutKind::NewWorktree;
        state.config.harness = Some(HarnessId::Codex);
        // A chat change drops the run picks but keeps the branch draft.
        assert!(state.sync_owners(Some("c2".into()), Some("s1".into()), false));
        assert_eq!(state.config.harness, None);
        assert_eq!(state.config.branch, Some("main".into()));
        // Same owners: stable.
        assert!(!state.sync_owners(Some("c2".into()), Some("s1".into()), false));
        // A different space drops the branch/checkout cache too.
        assert!(state.sync_owners(Some("c2".into()), Some("s2".into()), false));
        assert_eq!(state.config.branch, None);
        assert_eq!(state.config.checkout, CheckoutKind::Local);
        // "No project" is a space change as well.
        assert!(state.sync_owners(Some("c2".into()), Some("s2".into()), true));
        assert_eq!(state.space_owner, None);
    }

    #[test]
    fn composer_width_crossings_only() {
        let mut state = PickersState::default();
        assert!(!state.set_composer_width(800.0));
        assert!(state.set_composer_width(399.0));
        assert!(!state.set_composer_width(420.0)); // inside the hysteresis band
        assert!(state.set_composer_width(450.0));
        assert!(!state.set_composer_width(450.0));
    }

    #[test]
    fn new_project_request_is_one_shot() {
        let mut state = PickersState::default();
        assert!(!state.take_new_project_request());
        state.new_project_requested = true;
        assert!(state.take_new_project_request());
        assert!(!state.take_new_project_request());
    }
}
