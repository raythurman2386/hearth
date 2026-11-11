//! Resolving and picking: effective harness/model/mode/reasoning, the flattened
//! model rows and the keyboard/pointer selection state.

use std::collections::HashSet;
use std::rc::Rc;

use hearth_engine::registry::HarnessDescriptor;
use hearth_proto::{Chat, HarnessId, Model, ReasoningLevel, RepoRef, SessionMode};

use crate::state::App;

use super::model::{
    CheckoutKind, MAX_REF_ROWS, NO_ACTIVE_ROW, clamp_reasoning, default_model, offered_harnesses,
    traits_customized, traits_summary,
};
use super::{
    ModelRail, ModelRowData, ModelRowsKey, PickersState, checkout, filter_indices, loads,
    search_row_rank, space_picker,
};

/// The selected chat row, if any.
pub fn selected_chat_row(app: &App) -> Option<&Chat> {
    let id = app.selected_chat.as_deref()?;
    app.chats.iter().find(|c| c.id == id)
}

impl PickersState {
    /// Harness is locked once the chat exists (feature-inventory §1.7).
    pub fn harness_locked(&self, app: &App) -> bool {
        app.selected_chat.is_some()
    }

    /// Effective harness: picked, or the chat's config, or the first offered.
    pub fn effective_harness(&self, app: &App) -> Option<HarnessId> {
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if let Some(config) = selected_chat_row(app).and_then(|c| c.config.as_ref()) {
            return Some(config.harness);
        }
        // New-chat canvas: the remembered last-used harness (sticky defaults),
        // when the loaded catalog still offers it (the device may have
        // disabled it in Settings → Agents since).
        if let Some(harness) = self.defaults.harness {
            let offered = match self.harnesses.ready() {
                Some(list) => offered_harnesses(list).iter().any(|d| d.id == harness),
                None => true, // catalog not loaded yet — trust the memory
            };
            if offered {
                return Some(harness);
            }
        }
        // Fall back to the first OFFERED harness: the registry lists the mock
        // harness first, and resolving chips against it would boot the
        // new-chat canvas onto "Mock" instead of Claude Code + its default
        // model (it stays available under `HEARTH_HARNESS=mock`).
        self.harnesses
            .ready()
            .and_then(|list| offered_harnesses(list).first().map(|d| d.id))
    }

    /// The poll-time effective harness (no `App`): draft, then the remembered
    /// pick, then the first offered. Used only for follow-up loads.
    pub(crate) fn effective_harness_self(&self) -> Option<HarnessId> {
        self.config.harness.or_else(|| {
            self.defaults
                .harness
                .filter(|harness| match self.harnesses.ready() {
                    Some(list) => offered_harnesses(list).iter().any(|d| d.id == *harness),
                    None => true,
                })
        })
    }

    /// Effective model id: the draft pick, the selected chat's config, or (on
    /// the new-chat canvas) the remembered last-used model for the harness.
    pub fn effective_model_id<'a>(&'a self, app: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = selected_chat_row(app) {
            return chat.config.as_ref().and_then(|c| c.model.as_deref());
        }
        let harness = self.effective_harness(app)?;
        self.defaults.model_for(harness).map(|m| m.id.as_str())
    }

    /// Effective interaction mode (plan/agent/chat): the draft pick, the
    /// selected chat's config, or the remembered last-used mode. `None` means
    /// the harness's own default applies.
    pub fn effective_mode(&self, app: &App) -> Option<SessionMode> {
        self.config.mode.or_else(|| match selected_chat_row(app) {
            Some(chat) => chat.config.as_ref().and_then(|c| c.mode),
            None => self.defaults.mode,
        })
    }

    /// Effective reasoning — always concrete once the model is known: the draft
    /// pick / chat config / remembered default, clamped to the selected model's
    /// ladder, falling back to the model's default level.
    pub fn effective_reasoning(&self, app: &App) -> Option<ReasoningLevel> {
        let explicit = self
            .config
            .reasoning
            .or_else(|| match selected_chat_row(app) {
                Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                None => self.defaults.reasoning,
            });
        if self.selected_model(app).is_none() {
            // Catalog not loaded yet: show the explicit value as-is (nothing to
            // clamp against); it resolves to a concrete level on load.
            return explicit;
        }
        clamp_reasoning(explicit, &self.trait_ladder(app))
    }

    /// The selected model — concrete from the moment the list loads: the
    /// effective id when the list still offers it, else the harness default
    /// (first row). Never `None` with a non-empty catalog.
    pub fn selected_model<'a>(&'a self, app: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(app)?;
        let models = self.models.get(&harness)?.ready()?;
        match self.effective_model_id(app) {
            Some(id) => models
                .iter()
                .find(|m| m.id == id)
                .or_else(|| default_model(models)),
            None => default_model(models),
        }
    }

    /// The explicit (non-default) option picks: the chat's persisted selections
    /// for existing chats, the draft's for the new-chat canvas.
    pub fn explicit_options(&self, app: &App) -> serde_json::Map<String, serde_json::Value> {
        match selected_chat_row(app).and_then(|c| c.config.as_ref()) {
            Some(config) => config.model_options.clone(),
            None => self.config.model_options.clone(),
        }
    }

    /// The resolved harness's steering mode, from the loaded descriptor list.
    /// `None` while the catalog is loading (callers should assume the common
    /// StepBoundary case and show nothing).
    pub fn resolved_steering_mode(&self, app: &App) -> Option<hearth_proto::SteeringMode> {
        let harness = self.effective_harness(app)?;
        self.harnesses
            .ready()
            .and_then(|list| list.iter().find(|d| d.id == harness))
            .map(|d| d.steering_mode)
    }

    /// The catalog is loaded and offers nothing runnable — the no-agents state.
    /// False while the catalog is still loading or failed (nothing to conclude
    /// yet; offline sends must not be blocked on it).
    pub fn no_agents_available(&self) -> bool {
        self.harnesses
            .ready()
            .is_some_and(|list| offered_harnesses(list).is_empty())
    }

    /// The fully-resolved config the composer threads into the Run request and
    /// `Mutate createChat`: concrete model + reasoning whenever the catalog is
    /// loaded (no "engine picks a default" passthrough).
    pub fn resolved(&self, app: &App) -> super::ResolvedRunConfig {
        super::ResolvedRunConfig {
            harness: self.effective_harness(app),
            model: self
                .selected_model(app)
                .map(|m| m.id.clone())
                // Catalog not loaded (offline): still send the id we know.
                .or_else(|| self.effective_model_id(app).map(str::to_string)),
            mode: self.effective_mode(app),
            reasoning: self.effective_reasoning(app),
            model_options: self.explicit_options(app),
        }
    }

    /// The traits popover's reasoning ladder (model levels, falling back to the
    /// harness's advertised ladder) — shared by render and keyboard nav.
    pub fn trait_ladder(&self, app: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(app) else {
            return Vec::new();
        };
        if !model.reasoning_levels.is_empty() {
            return model.reasoning_levels.clone();
        }
        self.effective_harness(app)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.reasoning_levels.clone())
            })
            .unwrap_or_default()
    }

    /// The harness descriptors the picker rail offers, with the committed
    /// harness force-included even when it's outside the offered set (a dev
    /// session's mock harness, or one disabled after the chat existed).
    pub fn rail_descriptors(&self, app: &App) -> Vec<HarnessDescriptor> {
        let Some(list) = self.harnesses.ready() else {
            return Vec::new();
        };
        let mut descriptors = offered_harnesses(list);
        if let Some(effective) = self.effective_harness(app)
            && !descriptors.iter().any(|d| d.id == effective)
            && let Some(descriptor) = list.iter().find(|d| d.id == effective)
        {
            descriptors.insert(0, descriptor.clone());
        }
        descriptors
    }

    /// The model rows the picker currently shows, flat and in render order —
    /// keyboard nav, ⌘N jumps, Enter and the render walk THE SAME list.
    ///
    /// A live search spans every ready harness; otherwise the rail selection
    /// decides — favorites across harnesses, or the effective harness's list
    /// with its starred rows floated to the top. A locked chat restricts every
    /// view to its own harness.
    pub fn model_rows(&self, app: &App) -> Rc<Vec<ModelRowData>> {
        let key = ModelRowsKey {
            query: self.search.text.trim().to_string(),
            rail: self.model_rail,
            effective: self.effective_harness(app),
            locked: self.harness_locked(app),
            catalog_rev: self.catalog_rev,
        };
        if let Some((cached_key, rows)) = self.rows_cache.borrow().as_ref()
            && *cached_key == key
        {
            return rows.clone();
        }
        let rows = Rc::new(self.visible_model_rows(app));
        *self.rows_cache.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    pub fn visible_model_rows(&self, app: &App) -> Vec<ModelRowData> {
        let effective = self.effective_harness(app);
        let mut descriptors = self.rail_descriptors(app);
        if self.harness_locked(app) {
            descriptors.retain(|d| Some(d.id) == effective);
        }
        let row = |descriptor: &HarnessDescriptor, model: &Model| ModelRowData {
            harness: descriptor.id,
            harness_name: descriptor.name.clone(),
            model: model.clone(),
        };
        let favorites: HashSet<(HarnessId, &str)> = self
            .defaults
            .favorites
            .iter()
            .map(|f| (f.harness, f.model.as_str()))
            .collect();
        let query = self.search.text.trim().to_string();
        if !query.is_empty() {
            // Rank: label prefix < label substring < harness-name hit; stars,
            // then input order, break ties.
            let mut ranked: Vec<(usize, usize, usize, ModelRowData)> = Vec::new();
            let mut input_ix = 0usize;
            for descriptor in &descriptors {
                let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready()) else {
                    continue;
                };
                for model in models {
                    let wider = format!(
                        "{} {} {}",
                        descriptor.name,
                        model.description.as_deref().unwrap_or(""),
                        model.label
                    );
                    let starred = favorites.contains(&(descriptor.id, model.id.as_str()));
                    if let Some((rank, unstarred)) =
                        search_row_rank(&query, &model.label, &wider, starred)
                    {
                        ranked.push((rank, unstarred, input_ix, row(descriptor, model)));
                    }
                    input_ix += 1;
                }
            }
            ranked.sort_by_key(|(rank, unstarred, ix, _)| (*rank, *unstarred, *ix));
            return ranked.into_iter().map(|(_, _, _, row)| row).collect();
        }
        match self.model_rail {
            ModelRail::Favorites => {
                let mut rows = Vec::new();
                for descriptor in &descriptors {
                    let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready())
                    else {
                        continue;
                    };
                    for model in models {
                        if favorites.contains(&(descriptor.id, model.id.as_str())) {
                            rows.push(row(descriptor, model));
                        }
                    }
                }
                rows
            }
            ModelRail::Harness => {
                let Some(descriptor) = descriptors.iter().find(|d| Some(d.id) == effective) else {
                    return Vec::new();
                };
                let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready()) else {
                    return Vec::new();
                };
                let (starred, rest): (Vec<&Model>, Vec<&Model>) = models
                    .iter()
                    .partition(|m| favorites.contains(&(descriptor.id, m.id.as_str())));
                starred
                    .into_iter()
                    .chain(rest)
                    .map(|model| row(descriptor, model))
                    .collect()
            }
        }
    }

    /// The row the keyboard-nav highlight starts on: the resolved selected
    /// model's index in the VISIBLE rows (the favorites/search views may not
    /// contain it — then 0), 0 while the list is loading.
    pub fn selected_model_index(&self, app: &App) -> usize {
        let selected = self.selected_model(app).map(|m| m.id.clone());
        let effective = self.effective_harness(app);
        self.model_rows(app)
            .iter()
            .position(|row| {
                Some(row.harness) == effective && selected.as_deref() == Some(row.model.id.as_str())
            })
            .unwrap_or(0)
    }

    /// Poll-time re-anchor (no `App`): the draft/default pick only.
    pub(crate) fn selected_model_index_self(&self) -> usize {
        let harness = self.effective_harness_self();
        let selected = self
            .config
            .model
            .clone()
            .or_else(|| harness.and_then(|h| self.defaults.model_for(h).map(|m| m.id.clone())));
        let rows = match self.harnesses.ready() {
            Some(_) => self.model_rows_self(harness),
            None => Vec::new(),
        };
        rows.iter()
            .position(|row| {
                Some(row.harness) == harness && selected.as_deref() == Some(row.model.id.as_str())
            })
            .unwrap_or(0)
    }

    fn model_rows_self(&self, effective: Option<HarnessId>) -> Vec<ModelRowData> {
        let Some(list) = self.harnesses.ready() else {
            return Vec::new();
        };
        let mut descriptors = offered_harnesses(list);
        if let Some(effective) = effective
            && !descriptors.iter().any(|d| d.id == effective)
            && let Some(descriptor) = list.iter().find(|d| d.id == effective)
        {
            descriptors.insert(0, descriptor.clone());
        }
        let mut rows = Vec::new();
        for descriptor in &descriptors {
            let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready()) else {
                continue;
            };
            for model in models {
                rows.push(ModelRowData {
                    harness: descriptor.id,
                    harness_name: descriptor.name.clone(),
                    model: model.clone(),
                });
            }
        }
        rows
    }

    /// The mode popover's highlighted row: the effective mode's index in the
    /// fixed plan/agent/chat list (0 when none is set).
    pub fn effective_mode_index(&self, app: &App) -> usize {
        const MODES: [SessionMode; 3] = [SessionMode::Plan, SessionMode::Agent, SessionMode::Chat];
        self.effective_mode(app)
            .and_then(|m| MODES.iter().position(|x| *x == m))
            .unwrap_or(0)
    }

    /// The picker's visible row count (keyboard nav bounds).
    pub fn model_rows_len(&self, app: &App) -> usize {
        self.model_rows(app).len()
    }

    pub fn filtered_ref_rows(&self, _app: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        filter_indices(&self.search.text, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    fn filtered_ref_rows_self(&self) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        filter_indices(&self.search.text, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    /// Index of the highlighted-by-default row in the (filtered) ref list: the
    /// session's branch on an existing chat, the draft pick on a new one, else
    /// the current branch. Capped to the displayed window.
    pub fn selected_ref_index(&self, app: &App) -> usize {
        let rows = self.filtered_ref_rows(app);
        let selected = selected_chat_row(app)
            .and_then(|c| c.branch.clone())
            .or_else(|| self.config.branch.clone());
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    pub(crate) fn selected_ref_index_self(&self) -> usize {
        let rows = self.filtered_ref_rows_self();
        let index = match self.config.branch.as_deref() {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The picked ref's row, else the repo's current branch's row.
    pub fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    pub fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    /// The existing worktree the picked ref is materialized in, if any.
    pub fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    /// Label of the ref trigger: `From <ref>` only when a NEW worktree will be
    /// created off it; the bare name otherwise.
    pub fn ref_label(&self) -> String {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => "Select ref".to_string(),
            (CheckoutKind::NewWorktree, Some(name)) => format!("From {name}"),
            (CheckoutKind::Local, Some(name)) => name,
        }
    }

    /// [`Self::filtered_space_rows`] matching the search query, ranked.
    pub fn filtered_space_rows(&self, app: &App) -> Vec<hearth_proto::Space> {
        let spaces = space_picker::scoped_space_rows(self, app);
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        filter_indices(&self.search.text, &names)
            .into_iter()
            .map(|ix| spaces[ix].clone())
            .collect()
    }

    /// Row index of the currently selected space (un-searched open).
    /// [`NO_ACTIVE_ROW`] when nothing is selected (the no-project canvas must
    /// not open with row 0 wearing a phantom highlight).
    pub fn selected_space_index(&self, app: &App) -> usize {
        let selected = app.selected_space_row().map(|s| s.id.clone());
        selected
            .as_deref()
            .and_then(|id| {
                space_picker::scoped_space_rows(self, app)
                    .iter()
                    .position(|s| s.id == id)
            })
            .unwrap_or(NO_ACTIVE_ROW)
    }

    /// [`space_picker::device_rows`] filtered by the search box.
    pub fn filtered_device_rows(&self, app: &App) -> Vec<hearth_proto::Device> {
        let rows = space_picker::device_rows(app);
        let names: Vec<String> = rows.iter().map(|d| d.name.clone()).collect();
        filter_indices(&self.search.text, &names)
            .into_iter()
            .map(|ix| rows[ix].clone())
            .collect()
    }

    pub fn selected_device_index(&self, app: &App) -> usize {
        let effective = app.effective_device_id();
        space_picker::device_rows(app)
            .iter()
            .position(|d| Some(d.id.as_str()) == effective.as_deref())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Picks
// ---------------------------------------------------------------------------

/// Pick the highlighted (or hovered) model row: a foreign-harness row
/// (favorites / search) switches the harness first, exactly like clicking its
/// rail icon and then the model.
pub fn activate_model_index(app: &mut App, ix: usize) {
    let row = app.pickers.model_rows(app).get(ix).cloned();
    let Some(row) = row else {
        return;
    };
    if app.pickers.effective_harness(app) != Some(row.harness) {
        if app.pickers.harness_locked(app) {
            return;
        }
        pick_harness(app, row.harness);
    }
    pick_model(app, row.model.id);
}

/// Enter on the harness/model popover: pick the highlighted model.
pub fn activate_model_row(app: &mut App) {
    let active = app.pickers.active;
    activate_model_index(app, active);
}

pub fn pick_harness(app: &mut App, harness: HarnessId) {
    if app.selected_chat.is_some() {
        return; // locked
    }
    {
        let pickers = &mut app.pickers;
        if pickers.config.harness != Some(harness) {
            // The remembered model for this harness takes over via the defaults
            // fallback; a foreign pick must not linger.
            pickers.config.model = None;
            pickers.config.reasoning = None;
            pickers.config.model_options.clear();
        }
        pickers.config.harness = Some(harness);
        pickers.defaults.harness = Some(harness);
        pickers.save_defaults();
    }
    loads::ensure_models(app, harness, false);
    // Re-anchor the keyboard highlight onto the new harness's selected row.
    let active = app.pickers.selected_model_index(app);
    app.pickers.active = active;
}

pub fn pick_model(app: &mut App, model_id: String) {
    app.pickers.open = None;
    if app.selected_chat.is_some() {
        // Existing chat: persist to the chat row (`Mutate setChatConfig`) —
        // survives restarts and syncs; next runs in this chat use it.
        loads::update_chat_config(app, move |config| config.model = Some(model_id));
    } else {
        // New chat: draft pick + sticky last-used memory for this harness.
        let label = {
            let harness = app.pickers.effective_harness(app);
            harness.and_then(|h| {
                app.pickers
                    .models
                    .get(&h)
                    .and_then(|l| l.ready())
                    .and_then(|models| models.iter().find(|m| m.id == model_id))
                    .map(|m| (h, m.label.clone()))
            })
        };
        app.pickers.config.model = Some(model_id.clone());
        if let Some(harness) = app.pickers.effective_harness(app) {
            let label = label
                .map(|(_, label)| label)
                .unwrap_or_else(|| model_id.clone());
            app.pickers
                .defaults
                .remember_model(harness, model_id, label);
            app.pickers.save_defaults();
        }
    }
}

pub fn pick_reasoning(app: &mut App, level: ReasoningLevel) {
    // Always a concrete selection (no toggle-back-to-default).
    if app.selected_chat.is_some() {
        loads::update_chat_config(app, move |config| config.reasoning = Some(level));
    } else {
        app.pickers.config.reasoning = Some(level);
        app.pickers.defaults.reasoning = Some(level);
        app.pickers.save_defaults();
    }
}

pub fn pick_mode(app: &mut App, mode: SessionMode) {
    // Always a concrete selection (no toggle-back-to-default).
    if app.selected_chat.is_some() {
        loads::update_chat_config(app, move |config| config.mode = Some(mode));
    } else {
        app.pickers.config.mode = Some(mode);
        app.pickers.defaults.mode = Some(mode);
        app.pickers.save_defaults();
    }
}

pub fn pick_option(app: &mut App, option_id: String, choice_id: String, default: bool) {
    if app.selected_chat.is_some() {
        loads::update_chat_config(app, move |config| {
            if default {
                config.model_options.remove(&option_id);
            } else {
                config
                    .model_options
                    .insert(option_id, serde_json::Value::String(choice_id));
            }
        });
    } else if default {
        app.pickers.config.model_options.remove(&option_id);
    } else {
        app.pickers
            .config
            .model_options
            .insert(option_id, serde_json::Value::String(choice_id));
    }
}

pub fn pick_ref(app: &mut App, row: RepoRef) {
    // Refs are fixed at creation: an existing session can never move (the
    // footer renders read-only labels there, so this is a belt-and-braces
    // guard).
    if app.selected_chat.is_some() {
        return;
    }
    if row.worktree_path.is_some() {
        // Reuse the ref's existing worktree ("Current worktree").
        app.pickers.config.branch = Some(row.name);
        app.pickers.config.checkout = CheckoutKind::Local;
    } else if app.pickers.config.checkout == CheckoutKind::NewWorktree || row.current {
        // Base pick for a new worktree, or the already-current ref.
        app.pickers.config.branch = Some(row.name);
    } else {
        // Local mode + a plain non-current ref: CHECK OUT the space folder
        // (full t3code `switchRef` — picking `main` means "put my local
        // checkout on main", it must never flip the mode).
        checkout::switch_draft_ref(app, row);
        return;
    }
    app.pickers.open = None;
}

/// Star/unstar a model and persist it with the sticky defaults.
pub fn toggle_model_favorite(app: &mut App, harness: HarnessId, model: &str) {
    app.pickers.defaults.toggle_favorite(harness, model);
    app.pickers.save_defaults();
    app.pickers.catalog_rev += 1;
    // Starring REORDERS the list (stars float to the top / leave the favorites
    // view) — re-home the keyboard highlight onto the SELECTED row so exactly
    // one row reads highlighted afterwards.
    let active = app.pickers.selected_model_index(app);
    app.pickers.active = active;
}

/// Whether the traits trigger should show its (non-default) bright treatment.
pub fn traits_active(app: &App) -> bool {
    let state = &app.pickers;
    let options = state.explicit_options(app);
    traits_customized(
        state.selected_model(app),
        state.effective_reasoning(app),
        &state.trait_ladder(app),
        &options,
    )
}

/// The traits trigger's joined summary ("High · 1M · Fast"), if any.
pub fn traits_label(app: &App) -> Option<String> {
    let state = &app.pickers;
    let options = state.explicit_options(app);
    traits_summary(
        state.selected_model(app),
        state.effective_reasoning(app),
        &options,
    )
}

/// `None` when the selected model has neither a ladder nor options — a dead
/// trigger reads as broken, so the chip is omitted.
pub fn has_traits(app: &App) -> bool {
    let state = &app.pickers;
    !state.trait_ladder(app).is_empty()
        || state
            .selected_model(app)
            .is_some_and(|m| !m.options.is_empty())
}
