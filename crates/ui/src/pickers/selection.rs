//! Resolving and picking: effective harness/model/mode/reasoning, the flattened
//! model rows and the keyboard/pointer selection state.

use super::*;

impl Pickers {
    /// Effective harness: picked, or the chat's config, or the first listed.
    pub(super) fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
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

    /// Effective model id: the draft pick, the selected chat's config, or (on
    /// the new-chat canvas) the remembered last-used model for the harness.
    pub(super) fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat.config.as_ref().and_then(|c| c.model.as_deref());
        }
        let harness = self.effective_harness(cx)?;
        self.defaults.model_for(harness).map(|m| m.id.as_str())
    }

    /// Effective interaction mode (plan/agent/chat): the draft pick, the
    /// selected chat's config, or the remembered last-used mode. `None` means
    /// the harness's own default applies.
    pub(super) fn effective_mode(&self, cx: &App) -> Option<SessionMode> {
        self.config.mode.or_else(|| {
            match self.state.read(cx).selected_chat_row() {
                Some(chat) => chat.config.as_ref().and_then(|c| c.mode),
                // New chat: the remembered last-used mode.
                None => self.defaults.mode,
            }
        })
    }

    /// Effective reasoning — always concrete once the model is known: the
    /// draft pick / chat config / remembered default, clamped to the selected
    /// model's ladder, falling back to the model's default level.
    pub(super) fn effective_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        let explicit = self.config.reasoning.or_else(|| {
            match self.state.read(cx).selected_chat_row() {
                Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                // New chat: the remembered last-used level.
                None => self.defaults.reasoning,
            }
        });
        if self.selected_model(cx).is_none() {
            // Catalog not loaded yet: show the explicit value as-is (nothing
            // to clamp against); it resolves to a concrete level on load.
            return explicit;
        }
        clamp_reasoning(explicit, &self.trait_ladder(cx))
    }

    /// The selected model — concrete from the moment the list loads: the
    /// effective id when the list still offers it, else the harness default
    /// (first row). Never `None` with a non-empty catalog.
    pub(super) fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(cx)?;
        let models = self.models.get(&harness)?.ready()?;
        match self.effective_model_id(cx) {
            Some(id) => models
                .iter()
                .find(|m| m.id == id)
                .or_else(|| default_model(models)),
            None => default_model(models),
        }
    }

    /// The explicit (non-default) option picks: the chat's persisted
    /// selections for existing chats, the draft's for the new-chat canvas.
    pub(super) fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        match self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            Some(config) => config.model_options.clone(),
            None => self.config.model_options.clone(),
        }
    }

    /// The fully-resolved config the composer threads into the Run request and
    /// `Mutate createChat`: concrete model + reasoning whenever the catalog is
    /// loaded (no "engine picks a default" passthrough).
    /// The resolved harness's steering mode, from the loaded descriptor list.
    /// `None` while the catalog is loading (callers should assume the common
    /// StepBoundary case and show nothing).
    pub fn resolved_steering_mode(&self, cx: &App) -> Option<hearth_proto::SteeringMode> {
        let harness = self.effective_harness(cx)?;
        self.harnesses
            .ready()
            .and_then(|list| list.iter().find(|d| d.id == harness))
            .map(|d| d.steering_mode)
    }

    /// The catalog is loaded and offers nothing runnable — the no-agents
    /// state (every enabled harness is missing its CLI, or nothing is
    /// enabled). False while the catalog is still loading or failed
    /// (nothing to conclude yet; offline sends must not be blocked on it).
    pub fn no_agents_available(&self) -> bool {
        self.harnesses
            .ready()
            .is_some_and(|list| offered_harnesses(list).is_empty())
    }

    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            harness: self.effective_harness(cx),
            model: self
                .selected_model(cx)
                .map(|m| m.id.clone())
                // Catalog not loaded (offline): still send the id we know.
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
            mode: self.effective_mode(cx),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    // ---- selections ----

    pub(super) fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        // Refs are fixed at creation: an existing session can never move
        // (wing's rule — the footer renders read-only labels there, so this
        // is a belt-and-braces guard).
        if self.state.read(cx).selected_chat_row().is_some() {
            return;
        }
        if row.worktree_path.is_some() {
            // Reuse the ref's existing worktree ("Current worktree") — the
            // t3code `reuseExistingWorktree` path.
            self.config.branch = Some(row.name.clone());
            self.config.checkout = CheckoutKind::Local;
        } else if self.config.checkout == CheckoutKind::NewWorktree || row.current {
            // Base pick for a new worktree, or the already-current ref.
            self.config.branch = Some(row.name.clone());
        } else {
            // Local mode + a plain non-current ref: CHECK OUT the space
            // folder (full t3code `switchRef` — picking `main` means "put my
            // local checkout on main", it must never flip the mode).
            self.switch_draft_ref(row, cx);
            return;
        }
        self.animate_close(cx);
        cx.notify();
    }

    pub(super) fn pick_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.harness_locked(cx) {
            return;
        }
        if self.config.harness != Some(harness) {
            // The remembered model for this harness takes over via the
            // defaults fallback; a foreign pick must not linger.
            self.config.model = None;
            self.config.reasoning = None;
            self.config.model_options.clear();
        }
        self.config.harness = Some(harness);
        self.defaults.harness = Some(harness);
        self.save_defaults();
        self.model_scroll_base().set_offset(gpui::Point::default());
        self.ensure_models(harness, false, cx);
        // Re-anchor the keyboard highlight onto the new harness's selected row.
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        self.animate_close(cx);
        if self.state.read(cx).selected_chat.is_some() {
            // Existing chat: persist to the chat row (Mutate setChatConfig) —
            // survives restarts and syncs; next runs in this chat use it.
            self.update_chat_config(cx, move |config| config.model = Some(model_id));
        } else {
            // New chat: draft pick + sticky last-used memory for this harness.
            self.config.model = Some(model_id.clone());
            if let Some(harness) = self.effective_harness(cx) {
                let label = self
                    .models
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .and_then(|models| models.iter().find(|m| m.id == model_id))
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| model_id.clone());
                self.defaults.remember_model(harness, model_id, label);
                self.save_defaults();
            }
        }
        cx.notify();
    }

    pub(super) fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        // Always a concrete selection (no toggle-back-to-default).
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.reasoning = Some(level));
        } else {
            self.config.reasoning = Some(level);
            self.defaults.reasoning = Some(level);
            self.save_defaults();
        }
        cx.notify();
    }

    pub(super) fn pick_mode(&mut self, mode: SessionMode, cx: &mut Context<Self>) {
        // Always a concrete selection (no toggle-back-to-default).
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.mode = Some(mode));
        } else {
            self.config.mode = Some(mode);
            self.defaults.mode = Some(mode);
            self.save_defaults();
        }
        cx.notify();
    }

    pub(super) fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| {
                if default {
                    config.model_options.remove(&option_id);
                } else {
                    config
                        .model_options
                        .insert(option_id, serde_json::Value::String(choice_id));
                }
            });
        } else if default {
            self.config.model_options.remove(&option_id);
        } else {
            self.config
                .model_options
                .insert(option_id, serde_json::Value::String(choice_id));
        }
        cx.notify();
    }

    // ---- keyboard ----

    /// The traits popover's reasoning ladder (model levels, falling back to
    /// the harness's advertised ladder) — shared by render and keyboard nav.
    pub(super) fn trait_ladder(&self, cx: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(cx) else {
            return Vec::new();
        };
        if !model.reasoning_levels.is_empty() {
            return model.reasoning_levels.clone();
        }
        self.effective_harness(cx)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.reasoning_levels.clone())
            })
            .unwrap_or_default()
    }

    /// The harness descriptors the picker rail offers, with the committed
    /// harness force-included even when it's outside the offered set (a
    /// dev session's mock harness, or one disabled after the chat existed).
    pub(super) fn rail_descriptors(&self, cx: &App) -> Vec<HarnessDescriptor> {
        let Some(list) = self.harnesses.ready() else {
            return Vec::new();
        };
        let mut descriptors = offered_harnesses(list);
        if let Some(effective) = self.effective_harness(cx)
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
    /// A live search spans every ready harness (t3: the sidebar hides and
    /// the query ignores it); otherwise the rail selection decides —
    /// favorites across harnesses, or the effective harness's list with its
    /// starred rows floated to the top (t3 `groupFavorites`). A locked chat
    /// restricts every view to its own harness.
    /// Cached [`Self::visible_model_rows`]: selection/highlight changes and
    /// re-renders share one flattened list until an input actually changes.
    pub(super) fn model_rows(&self, cx: &App) -> std::sync::Arc<Vec<ModelRowData>> {
        let key = ModelRowsKey {
            query: self.search.read(cx).text().trim().to_string(),
            rail: self.model_rail,
            effective: self.effective_harness(cx),
            locked: self.harness_locked(cx),
            catalog_rev: self.catalog_rev,
        };
        if let Some((cached_key, rows)) = self.model_rows_cache.borrow().as_ref()
            && *cached_key == key
        {
            return rows.clone();
        }
        let rows = std::sync::Arc::new(self.visible_model_rows(cx));
        *self.model_rows_cache.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    fn visible_model_rows(&self, cx: &App) -> Vec<ModelRowData> {
        let effective = self.effective_harness(cx);
        let mut descriptors = self.rail_descriptors(cx);
        if self.harness_locked(cx) {
            descriptors.retain(|d| Some(d.id) == effective);
        }
        let row = |descriptor: &HarnessDescriptor, model: &Model| ModelRowData {
            harness: descriptor.id,
            harness_name: SharedString::from(descriptor.name.clone()),
            model: model.clone(),
        };
        // Favorite lookups are per-row; the Vec scan made the flatten
        // O(models × favorites).
        let favorites: std::collections::HashSet<(HarnessId, &str)> = self
            .defaults
            .favorites
            .iter()
            .map(|f| (f.harness, f.model.as_str()))
            .collect();
        let query = self.search.read(cx).text().trim().to_string();
        if !query.is_empty() {
            // Rank: label prefix < label substring < harness-name hit;
            // stars, then input order, break ties (t3 modelPickerSearch's
            // field ladder + favorite boost, collapsed to our ranks).
            let mut ranked: Vec<(usize, usize, usize, ModelRowData)> = Vec::new();
            let mut input_ix = 0usize;
            for descriptor in &descriptors {
                let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready()) else {
                    continue;
                };
                for model in models {
                    let by_label = popover::match_rank(&query, &model.label);
                    // The wider haystack ranks behind label hits: harness
                    // name AND the description (opencode's provider
                    // attribution — "anthropic" must find its models).
                    let by_harness = popover::match_rank(
                        &query,
                        &format!(
                            "{} {} {}",
                            descriptor.name,
                            model.description.as_deref().unwrap_or(""),
                            model.label
                        ),
                    )
                    .map(|rank| rank + 2);
                    if let Some(rank) = by_label.into_iter().chain(by_harness).min() {
                        let starred = !favorites.contains(&(descriptor.id, model.id.as_str()));
                        ranked.push((rank, starred as usize, input_ix, row(descriptor, model)));
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
    pub(super) fn selected_model_index(&self, cx: &App) -> usize {
        let selected = self.selected_model(cx).map(|m| m.id.clone());
        let effective = self.effective_harness(cx);
        self.model_rows(cx)
            .iter()
            .position(|row| {
                Some(row.harness) == effective && selected.as_deref() == Some(row.model.id.as_str())
            })
            .unwrap_or(0)
    }

    /// The mode popover's highlighted row: the effective mode's index in the
    /// fixed plan/agent/chat list (0 when none is set).
    pub(super) fn effective_mode_index(&self, cx: &App) -> usize {
        const MODES: [SessionMode; 3] = [SessionMode::Plan, SessionMode::Agent, SessionMode::Chat];
        self.effective_mode(cx)
            .and_then(|m| MODES.iter().position(|x| *x == m))
            .unwrap_or(0)
    }

    /// The picker's visible row count (keyboard nav bounds).
    pub(super) fn model_rows_len(&self, cx: &App) -> usize {
        self.model_rows(cx).len()
    }

    /// Enter on the harness/model popover: pick the highlighted model.
    pub(super) fn activate_model_row(&mut self, cx: &mut Context<Self>) {
        self.activate_model_index(self.active, cx);
    }

    /// Pick the visible row at `ix` — a foreign-harness row (favorites /
    /// search) switches the harness first, exactly like clicking its rail
    /// icon and then the model.
    pub(super) fn activate_model_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.model_rows(cx).get(ix).cloned() else {
            return;
        };
        if self.effective_harness(cx) != Some(row.harness) {
            if self.harness_locked(cx) {
                return;
            }
            self.pick_harness(row.harness, cx);
        }
        self.pick_model(row.model.id, cx);
    }

    /// Star/unstar a model and persist it with the sticky defaults.
    pub(super) fn toggle_model_favorite(
        &mut self,
        harness: HarnessId,
        model: &str,
        cx: &mut Context<Self>,
    ) {
        self.defaults.toggle_favorite(harness, model);
        self.save_defaults();
        self.catalog_rev += 1;
        // Starring REORDERS the list (stars float to the top / leave the
        // favorites view) — re-home the keyboard highlight onto the SELECTED
        // row so exactly one row reads highlighted afterwards. Following the
        // starred row instead left its cursor wash next to the selected
        // row's ring: "two highlighted rows" (user report, twice).
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    pub(super) fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    // ---- checkout resolution (the t3code env-mode semantics) ----

    /// Index of the highlighted-by-default row in the (filtered) ref list:
    /// the session's branch on an existing chat, the draft pick on a new one,
    /// else the current branch. Capped to the displayed window.
    pub(super) fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone())
            .or_else(|| self.config.branch.clone());
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The picked ref's row, else the repo's current branch's row.
    pub(super) fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    pub(super) fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    /// The existing worktree the picked ref is materialized in, if any.
    pub(super) fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    /// Label of the ref trigger: `From <ref>` only when a NEW worktree will be
    /// created off it (t3code `getBranchTriggerLabel`); the bare name otherwise.
    pub(super) fn ref_label(&self) -> SharedString {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => SharedString::from("Select ref"),
            (CheckoutKind::NewWorktree, Some(name)) => SharedString::from(format!("From {name}")),
            (CheckoutKind::Local, Some(name)) => SharedString::from(name),
        }
    }

    /// [`Self::scoped_space_rows`] matching the search query, ranked
    /// (`popover::filter_indices`).
    pub(super) fn filtered_space_rows(&self, cx: &App) -> Vec<Space> {
        let query = self.search.read(cx).text().to_string();
        let spaces = self.scoped_space_rows(cx);
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| spaces[ix].clone())
            .collect()
    }

    /// Row index of the currently selected space (un-searched open) — within
    /// the scoped order [`filtered_space_rows`] lists on an empty query.
    /// [`NO_ACTIVE_ROW`] when nothing is selected (the no-project canvas must
    /// not open with row 0 wearing a phantom highlight — user report).
    pub(super) fn selected_space_index(&self, cx: &App) -> usize {
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        selected
            .as_deref()
            .and_then(|id| self.scoped_space_rows(cx).iter().position(|s| s.id == id))
            .unwrap_or(NO_ACTIVE_ROW)
    }

    /// [`Self::device_rows`] filtered by the search box (same ranked
    /// substring match as the project rows).
    pub(super) fn filtered_device_rows(&self, cx: &App) -> Vec<hearth_proto::Device> {
        let query = self.search.read(cx).text().to_string();
        let rows = self.device_rows(cx);
        let names: Vec<String> = rows.iter().map(|d| d.name.clone()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| rows[ix].clone())
            .collect()
    }

    pub(super) fn selected_device_index(&self, cx: &App) -> usize {
        let effective = self.state.read(cx).effective_device_id();
        self.device_rows(cx)
            .iter()
            .position(|d| Some(d.id.as_str()) == effective.as_deref())
            .unwrap_or(0)
    }
}
