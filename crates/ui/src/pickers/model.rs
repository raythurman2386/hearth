//! Pure config and path helpers: draft/default resolution, trait summaries, the
//! folder browser and harness visibility.

use super::*;

/// Display cap for the ref list (t3code shows pages of 100 with a status
/// footer; a flat cap + "Showing X of Y refs" reads the same without
/// pagination plumbing).
pub(super) const MAX_REF_ROWS: usize = 300;

/// Below this measured composer width, the traits summary yields its space to
/// the prompt and becomes a compact overflow affordance. The model remains
/// visible so the active agent is still identifiable at a glance.
const COMPACT_TRAITS_ENTER_WIDTH: f32 = 400.0;
const COMPACT_TRAITS_EXIT_WIDTH: f32 = 440.0;

pub(super) fn compact_traits_for_width(width: f32, currently_compact: bool) -> bool {
    if width <= 0.0 {
        return currently_compact;
    }
    if currently_compact {
        width < COMPACT_TRAITS_EXIT_WIDTH
    } else {
        width < COMPACT_TRAITS_ENTER_WIDTH
    }
}

// ---------------------------------------------------------------------------
// Catalog invalidation (Settings → Agents toggles)
// ---------------------------------------------------------------------------

/// Marker global: [`bump_harness_catalog`] pokes it whenever a Settings →
/// Agents toggle changes some device's enabled set, and every [`Pickers`]
/// observes it to force-refresh its cached harness catalog — without this the
/// composer served the boot-time list until restart (user report).
#[derive(Default)]
pub struct HarnessCatalogChanged;

impl gpui::Global for HarnessCatalogChanged {}

/// Notify all composers that some device's harness catalog changed. The
/// global carries no data — `default_global` pushes the observer effect, and
/// the observers re-fetch from the engine (the source of truth).
pub fn bump_harness_catalog(cx: &mut App) {
    cx.default_global::<HarnessCatalogChanged>();
}

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send. The folder
/// and device come from the selected SPACE — the draft only carries the git
/// extras (ref + checkout kind) and the run config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    /// The session interaction mode (plan/agent/chat); `None` = harness default.
    pub mode: Option<SessionMode>,
    pub reasoning: Option<ReasoningLevel>,
    /// option id → choice id (only non-defaults are meaningful).
    pub model_options: serde_json::Map<String, serde_json::Value>,
    /// The picked ref (base branch in NewWorktree mode; a worktree's branch
    /// when reusing one). `None` = the repo's current branch.
    pub branch: Option<String>,
    /// Where the new session runs (the t3code env-mode).
    pub checkout: CheckoutKind,
}

/// Where a new session runs (t3code's env-mode: `local | worktree`). "Current
/// worktree" is NOT a third mode — it's `Local` when the picked ref is already
/// materialized as a worktree (the session reuses that checkout's path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action (composer consumes this — see
/// [`Pickers::checkout_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (hearth mints a `hearth/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the
    /// space folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// The fully-resolved run configuration the composer sends: concrete harness,
/// model and reasoning (never a "default" passthrough once the catalog is
/// loaded), plus the explicit non-default option picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    /// The session interaction mode (plan/agent/chat); `None` = harness default.
    pub mode: Option<SessionMode>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
}

impl ResolvedRunConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known harness).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            mode: self.mode,
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: SandboxLevel::WorkspaceWrite,
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

/// The harness's default model: the first catalog row (both curated catalogs
/// lead with the flagship — hearth's `pickDefaultModel` Opus preference maps to
/// the same row here).
pub fn default_model(models: &[Model]) -> Option<&Model> {
    models.first()
}

/// A model's default reasoning: X-High when the ladder offers it (hearth
/// `DEFAULT_REASONING = "xhigh"`), else High, else the ladder's first entry.
/// `None` only for ladder-less models (e.g. Haiku's thinking toggle instead).
pub fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    // The recommended default is High (user-corrected — not X-High globally);
    // fall to Medium then the ladder's first entry for shorter ladders.
    if ladder.contains(&ReasoningLevel::High) {
        return Some(ReasoningLevel::High);
    }
    if ladder.contains(&ReasoningLevel::Medium) {
        return Some(ReasoningLevel::Medium);
    }
    ladder.first().copied()
}

/// Clamp a picked/remembered level to what the model actually offers: keep it
/// when the ladder lists it, else fall to the model's default (never a stale
/// or foreign level — hearth use-run-config.ts's derived-model discipline).
pub fn clamp_reasoning(
    level: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match level {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_reasoning(ladder),
    }
}

// ---------------------------------------------------------------------------
// Pure: labels + traits summary
// ---------------------------------------------------------------------------

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

/// Display label for a session interaction mode (the mode chip + popover).
pub fn mode_label(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Plan => "Plan",
        SessionMode::Agent => "Agent",
        SessionMode::Chat => "Chat",
    }
}

/// The TraitsPicker trigger summary: the effective reasoning level plus every
/// model option's effective choice — the explicit pick when one is saved and
/// still offered, else the option's default — joined with " · " ("High · 1M ·
/// Fast", Cursor's "Agent · Balance"). Defaults are spelled out rather than
/// hidden so the run's configuration reads without opening the popover; `None`
/// only when the model has nothing to describe (no ladder, no options).
pub fn traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let choice_id = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .filter(|id| option.choices.iter().any(|c| c.id == *id))
                .unwrap_or(&option.default_choice);
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Whether any trait departs from its default — the trigger brightens only
/// then, so a customized run still stands out now that the summary always
/// names the effective choices.
pub fn traits_customized(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
    selections: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    if reasoning != default_reasoning(ladder) {
        return true;
    }
    model.is_some_and(|model| {
        model.options.iter().any(|option| {
            selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .is_some_and(|id| {
                    id != option.default_choice && option.choices.iter().any(|c| c.id == id)
                })
        })
    })
}

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation (used by the shell's add-space flow)
// ---------------------------------------------------------------------------

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Byte length of `name`'s prefix matching `query`, compared char-for-char
/// case-insensitively; `None` when `query` isn't a prefix of `name`. The
/// length indexes into `name` (not `query`) so the completion suffix keeps
/// the folder's real casing: `("Documents", "doc") → Some(3)` → `"uments"`.
pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    let mut len = 0;
    let mut name_chars = name.chars();
    for qc in query.chars() {
        let nc = name_chars.next()?;
        if !nc.to_lowercase().eq(qc.to_lowercase()) {
            return None;
        }
        len += nc.len_utf8();
    }
    Some(len)
}

/// Resolve a typed path segment against folder `names` (slash-descend):
/// exact match first — case-SENSITIVE before case-insensitive, so `GitHub/`
/// picks a `GitHub` sibling over `github` — then a unique case-insensitive
/// prefix. Ambiguity resolves to `None`: the slash stays in the query.
pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if let Some(ix) = names.iter().position(|n| *n == query) {
        return Some(ix);
    }
    if let Some(ix) = names
        .iter()
        .position(|n| completion_prefix_len(n, query) == Some(n.len()))
    {
        return Some(ix);
    }
    let mut hits = names
        .iter()
        .enumerate()
        .filter(|(_, n)| completion_prefix_len(n, query).is_some());
    let (ix, _) = hits.next()?;
    hits.next().is_none().then_some(ix)
}

/// Interpret a palette query as a typed path jump: absolute (`/disk2/projects`)
/// or home-relative (`~`, `~/github`). Returns the absolute path to browse,
/// trailing slash trimmed. `home` is the device's resolved home — `None`
/// until the first listing lands, when `~` can't expand yet. A query like
/// `~foo` is a folder name, not a path.
pub fn typed_path_target(query: &str, home: Option<&str>) -> Option<String> {
    let query = query.trim();
    if let Some(rest) = query.strip_prefix('~') {
        let home = home?.trim_end_matches('/');
        if rest.is_empty() {
            return Some(home.to_string());
        }
        let rest = rest.strip_prefix('/')?.trim_end_matches('/');
        return Some(if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        });
    }
    if query.starts_with('/') {
        let trimmed = query.trim_end_matches('/');
        return Some(if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        });
    }
    None
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&hearth_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

/// Display-side model-list hygiene, mirroring the engine's discovery-side
/// fold (`models_from_session`) for catalogs served by OLDER engines (the
/// space's device may run any version): the `default` alias row drops when a
/// real row exists, and an orphan `<model>[1m]` variant presents as its base
/// id with the Context Window trait pinned to 1M. Idempotent over already-
/// clean lists. The send path recomposes the advertised id from the base +
/// trait (`pick_model_value`), so a folded pick still runs.
pub(crate) fn normalize_model_rows(_harness: HarnessId, models: Vec<Model>) -> Vec<Model> {
    fn strip_1m(id: &str) -> Option<&str> {
        id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
    }
    let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    let has_real = ids.iter().any(|id| !id.eq_ignore_ascii_case("default"));
    models
        .into_iter()
        .filter_map(|mut model| {
            if has_real && model.id.eq_ignore_ascii_case("default") {
                return None;
            }
            if let Some(base) = strip_1m(&model.id.clone()) {
                if ids.iter().any(|other| other == base) {
                    // The bare base is listed too — the engine already gave
                    // it the Context Window trait; the variant row is noise.
                    return None;
                }
                model.id = base.to_string();
                // "Opus (1M context)" → "Opus".
                if let Some(at) = model.label.rfind(" (")
                    && model.label.ends_with(')')
                {
                    model.label.truncate(at);
                    while model.label.ends_with(' ') {
                        model.label.pop();
                    }
                }
                if !model.options.iter().any(|o| o.id == "contextWindow") {
                    model.options.push(hearth_proto::ModelOption {
                        id: "contextWindow".into(),
                        label: "Context Window".into(),
                        choices: vec![
                            hearth_proto::ModelOptionChoice {
                                id: "200k".into(),
                                label: "200K".into(),
                            },
                            hearth_proto::ModelOptionChoice {
                                id: "1m".into(),
                                label: "1M".into(),
                            },
                        ],
                        default_choice: "1m".into(),
                    });
                }
            }
            Some(model)
        })
        .collect()
}

pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
        HarnessId::Grok => (crate::icons::GROK_MARK, None),
        HarnessId::Huginn | HarnessId::Mock => (crate::icons::HUGINN_MARK, None),
    }
}

/// `HEARTH_HARNESS=mock` (the e2e/dev rig) opts the mock harness into the UI;
/// production launches never set it, so the mock never surfaces there.
fn mock_harness_enabled() -> bool {
    std::env::var("HEARTH_HARNESS")
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("mock")
}

/// Production pickers AND chip resolution hide the mock harness — the
/// registry always lists it, but it must never surface in real UI (neither in
/// the picker rail nor as the eager default the chips resolve against).
/// `HEARTH_HARNESS=mock` shows it; otherwise it only remains when it's
/// literally all there is (a dev build with no real harness registered).
pub fn visible_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, mock_harness_enabled())
}

fn visible_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    if allow_mock {
        return list.to_vec();
    }
    let real: Vec<HarnessDescriptor> = list
        .iter()
        .filter(|d| d.id != HarnessId::Mock)
        .cloned()
        .collect();
    if real.is_empty() { list.to_vec() } else { real }
}

/// What the composer actually offers: [`visible_harnesses`] narrowed to the
/// catalog device's enabled set AND installed CLIs (Settings → Agents is
/// per-device state, so a space on another device follows THAT device's
/// toggles; a default-enabled agent whose CLI is missing would only
/// manufacture NotInstalled errors at send). The dev-rig mock opt-in
/// survives the filter. There is NO fallback: a catalog where nothing is
/// both enabled and installed offers nothing, and the composer surfaces the
/// no-agents empty state + blocks new sends — resurrecting descriptors that
/// can only fail with NotInstalled is the #128 bug.
pub fn offered_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    offered_harnesses_impl(list, mock_harness_enabled())
}

fn offered_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, allow_mock)
        .into_iter()
        .filter(|d| {
            d.installed
                && (hearth_engine::registry::descriptor_enabled(d)
                    || (allow_mock && d.id == HarnessId::Mock))
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use hearth_proto::{FolderEntry, Model, ModelOption, ModelOptionChoice};

    #[test]
    fn traits_trigger_compacts_only_at_narrow_measured_widths() {
        assert!(!compact_traits_for_width(0.0, false));
        assert!(compact_traits_for_width(
            COMPACT_TRAITS_ENTER_WIDTH - 1.0,
            false
        ));
        assert!(!compact_traits_for_width(COMPACT_TRAITS_ENTER_WIDTH, false));
        // The small gap between entry and exit prevents chatter if the divider
        // jitters around the boundary, without delaying restoration on a wide
        // composer.
        assert!(compact_traits_for_width(
            COMPACT_TRAITS_EXIT_WIDTH - 1.0,
            true
        ));
        assert!(!compact_traits_for_width(COMPACT_TRAITS_EXIT_WIDTH, true));
    }

    fn bare_model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        }
    }

    #[test]
    fn normalize_drops_default_alias_and_folds_orphan_1m_rows() {
        // The shape an OLDER engine serves: a `default` alias row plus
        // 1M-pinned variants with no bare base. A non-claude harness keeps
        // wire labels (no curated catalog to borrow from).
        let models = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("default", "Default (recommended)"),
                bare_model("titan[1m]", "Titan (1M context)"),
                bare_model("gpt-x-9[1m]", "GPT X-9"),
                bare_model("nano", "Nano"),
            ],
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["titan", "gpt-x-9", "nano"]
        );
        assert_eq!(models[0].label, "Titan");
        assert_eq!(models[1].label, "GPT X-9");
        // Folded rows pin the Context Window trait to 1M.
        assert!(
            models[0]
                .options
                .iter()
                .any(|o| o.id == "contextWindow" && o.default_choice == "1m")
        );
        assert!(models[2].options.is_empty());

        // A `default`-only list survives (nothing real to prefer).
        let only_default =
            normalize_model_rows(HarnessId::Codex, vec![bare_model("default", "Default")]);
        assert_eq!(only_default.len(), 1);

        // A base-plus-variant pair (already folded by a NEWER engine — the
        // variant never reaches us; belt-and-braces if it does): variant
        // drops, base is untouched.
        let paired = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("titan-5", "Titan 5"),
                bare_model("titan-5[1m]", "Titan 5 (1M)"),
            ],
        );
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0].id, "titan-5");

        // Idempotent over a clean list.
        let clean = vec![bare_model("titan-5", "Titan 5")];
        assert_eq!(normalize_model_rows(HarnessId::Codex, clean.clone()), clean);
    }

    #[test]
    fn traits_summary_formats_non_defaults() {
        let model = Model {
            id: "opus".into(),
            label: "Opus".into(),
            description: None,
            reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            options: vec![
                ModelOption {
                    id: "context".into(),
                    label: "Context window".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "standard".into(),
                            label: "Standard".into(),
                        },
                        ModelOptionChoice {
                            id: "1m".into(),
                            label: "1M".into(),
                        },
                    ],
                    default_choice: "standard".into(),
                },
                ModelOption {
                    id: "speed".into(),
                    label: "Speed".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "normal".into(),
                            label: "Normal".into(),
                        },
                        ModelOptionChoice {
                            id: "fast".into(),
                            label: "Fast".into(),
                        },
                    ],
                    default_choice: "normal".into(),
                },
            ],
        };
        let mut selections = serde_json::Map::new();
        selections.insert("context".into(), serde_json::Value::String("1m".into()));
        selections.insert("speed".into(), serde_json::Value::String("fast".into()));
        assert_eq!(
            traits_summary(Some(&model), Some(ReasoningLevel::High), &selections),
            Some("High · 1M · Fast".to_string())
        );
        // All defaults: the effective choices still read on the trigger.
        assert_eq!(
            traits_summary(Some(&model), None, &serde_json::Map::new()),
            Some("Standard · Normal".to_string())
        );
        // A saved choice the option no longer offers falls back to the default
        // label rather than vanishing or echoing a stale id.
        let mut stale = serde_json::Map::new();
        stale.insert(
            "speed".into(),
            serde_json::Value::String("ludicrous".into()),
        );
        assert_eq!(
            traits_summary(Some(&model), None, &stale),
            Some("Standard · Normal".to_string())
        );
        // Reasoning shows without a model too.
        assert_eq!(
            traits_summary(
                None,
                Some(ReasoningLevel::Ultrathink),
                &serde_json::Map::new()
            ),
            Some("Ultrathink".to_string())
        );
        // Nothing to describe → "Traits" fallback upstream.
        assert_eq!(traits_summary(None, None, &serde_json::Map::new()), None);

        // Customized (bright trigger) only when something departs from its
        // default: default-choice selections and the default reasoning level
        // don't count; stale ids don't either.
        let ladder = model.reasoning_levels.clone();
        assert!(traits_customized(
            Some(&model),
            Some(ReasoningLevel::High),
            &ladder,
            &selections
        ));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &serde_json::Map::new()
        ));
        let mut defaults = serde_json::Map::new();
        defaults.insert("speed".into(), serde_json::Value::String("normal".into()));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &defaults
        ));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &stale
        ));
        assert!(traits_customized(
            Some(&model),
            Some(ReasoningLevel::Medium),
            &ladder,
            &serde_json::Map::new()
        ));
    }

    #[test]
    fn folder_paths_and_breadcrumbs() {
        assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/"), Some("/".to_string()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(child_path("/home", "w"), "/home/w");
        assert_eq!(child_path("/", "home"), "/home");
        let crumbs = breadcrumbs("/home/w/dev");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn completion_prefix_lengths() {
        // Case-insensitive; the length indexes into the NAME's bytes.
        assert_eq!(completion_prefix_len("Documents", "doc"), Some(3));
        assert_eq!(&"Documents"[3..], "uments");
        assert_eq!(completion_prefix_len("hearth", "hearth"), Some(6));
        assert_eq!(completion_prefix_len("hearth", ""), Some(0));
        assert_eq!(completion_prefix_len("hearth", "dev"), None);
        // Longer than the name → not a prefix.
        assert_eq!(completion_prefix_len("dev", "devel"), None);
        // Multibyte names slice on a char boundary.
        assert_eq!(completion_prefix_len("héllo", "hé"), Some(3));
        assert_eq!(&"héllo"[3..], "llo");
    }

    #[test]
    fn segment_target_resolution() {
        let names = ["github", "GitHub", "worktree"];
        // Exact casing beats the earlier case-insensitive sibling…
        assert_eq!(segment_target(&names, "GitHub"), Some(1));
        assert_eq!(segment_target(&names, "github"), Some(0));
        // …but with no exact-cased hit, case-insensitive exact still lands.
        assert_eq!(segment_target(&names, "WORKTREE"), Some(2));
        // Unique prefix descends; an ambiguous one keeps the slash honest.
        assert_eq!(segment_target(&names, "work"), Some(2));
        assert_eq!(segment_target(&names, "g"), None);
        assert_eq!(segment_target(&names, "x"), None);
    }

    #[test]
    fn typed_path_target_expands_absolute_and_home_paths() {
        let home = Some("/home/wing");
        assert_eq!(typed_path_target("/disk2/", home), Some("/disk2".into()));
        assert_eq!(
            typed_path_target("/disk2/projects", home),
            Some("/disk2/projects".into())
        );
        assert_eq!(typed_path_target("/", home), Some("/".into()));
        assert_eq!(typed_path_target("~", home), Some("/home/wing".into()));
        assert_eq!(typed_path_target("~/", home), Some("/home/wing".into()));
        assert_eq!(
            typed_path_target("~/github/", home),
            Some("/home/wing/github".into())
        );
        // `~x` is a folder name; relative queries are searches, not paths.
        assert_eq!(typed_path_target("~x", home), None);
        assert_eq!(typed_path_target("src", home), None);
        // `~` can't expand before the device's home is known.
        assert_eq!(typed_path_target("~/github", None), None);
        assert_eq!(typed_path_target("/disk2", None), Some("/disk2".into()));
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                FolderEntry {
                    name: "hearth".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        // Files never show as rows.
        assert_eq!(browser_rows(&listing).len(), 2);
        assert_eq!(browser_rows(&listing)[1].name, "hearth");
    }

    #[test]
    fn resolved_chat_config_requires_harness() {
        let mut resolved = ResolvedRunConfig::default();
        assert!(resolved.chat_config().is_none());
        resolved.harness = Some(HarnessId::Huginn);
        resolved.model = Some("opus".into());
        resolved.reasoning = Some(ReasoningLevel::High);
        let config = resolved.chat_config().expect("harness set");
        assert_eq!(config.harness, HarnessId::Huginn);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn default_model_is_first_catalog_row() {
        let models = vec![
            Model {
                id: "flagship".into(),
                label: "Flagship".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            },
            Model {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            },
        ];
        assert_eq!(default_model(&models).map(|m| &*m.id), Some("flagship"));
        assert!(default_model(&[]).is_none());
    }

    #[test]
    fn default_reasoning_prefers_high_then_medium() {
        use ReasoningLevel::*;
        // Recommended default is High (user-corrected), even on full ladders.
        assert_eq!(
            default_reasoning(&[Low, Medium, High, XHigh, Max, Ultracode, Ultrathink]),
            Some(High)
        );
        assert_eq!(default_reasoning(&[Low, Medium, High, Max]), Some(High));
        // No High: Medium.
        assert_eq!(default_reasoning(&[Minimal, Low, Medium]), Some(Medium));
        // Neither offered: first entry.
        assert_eq!(default_reasoning(&[Minimal, Low]), Some(Minimal));
        // Ladder-less model (Haiku): no reasoning at all.
        assert_eq!(default_reasoning(&[]), None);
    }

    #[test]
    fn clamp_reasoning_keeps_offered_levels_and_heals_foreign_ones() {
        use ReasoningLevel::*;
        let ladder = [Low, Medium, High, Max];
        // A pick the ladder offers survives.
        assert_eq!(clamp_reasoning(Some(Max), &ladder), Some(Max));
        // A remembered level the new model doesn't offer heals to its default.
        assert_eq!(clamp_reasoning(Some(XHigh), &ladder), Some(High));
        // No pick at all resolves to the concrete default too.
        assert_eq!(clamp_reasoning(None, &ladder), Some(High));
        assert_eq!(clamp_reasoning(Some(High), &[]), None);
    }

    #[test]
    fn mock_harness_hidden_unless_alone() {
        let descriptor = |id: HarnessId, name: &str| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: hearth_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled: None,
        };
        let mixed = vec![
            descriptor(HarnessId::Mock, "Mock"),
            descriptor(HarnessId::Huginn, "Claude Code"),
        ];
        // Env-independent core: mock hidden in production…
        let visible = visible_harnesses_impl(&mixed, false);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, HarnessId::Huginn);
        let only_mock = vec![descriptor(HarnessId::Mock, "Mock")];
        assert_eq!(visible_harnesses_impl(&only_mock, false).len(), 1);
        // …and opted back in by HEARTH_HARNESS=mock (the e2e rig).
        assert_eq!(visible_harnesses_impl(&mixed, true).len(), 2);
        assert_eq!(visible_harnesses_impl(&mixed, true)[0].id, HarnessId::Mock);
    }

    #[test]
    fn offered_harnesses_follow_the_catalog_enabled_flags() {
        let descriptor = |id: HarnessId, name: &str, enabled: Option<bool>| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: hearth_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled,
        };
        let catalog = |claude: Option<bool>, codex: Option<bool>, grok: Option<bool>| {
            vec![
                descriptor(HarnessId::Mock, "Mock", Some(false)),
                descriptor(HarnessId::Huginn, "Huginn", claude),
                descriptor(HarnessId::Codex, "Codex", codex),
                descriptor(HarnessId::Grok, "Grok", grok),
            ]
        };
        // A catalog from an engine predating the flag (all None) follows its
        // installed probes, so every detected real harness is offered.
        let offered = offered_harnesses_impl(&catalog(None, None, None), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Huginn, HarnessId::Codex, HarnessId::Grok]
        );
        // The device's flags win: Grok on, Codex off; catalog order holds.
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), Some(true)), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Huginn, HarnessId::Grok]
        );
        // The dev-rig mock opt-in survives the enabled filter (and Grok's
        // unknown flag still resolves through its installed probe).
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), None), true);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Mock, HarnessId::Huginn, HarnessId::Grok]
        );
        // Nothing enabled offers nothing — the composer renders the
        // no-agents empty state instead of resurrecting disabled agents.
        let offered =
            offered_harnesses_impl(&catalog(Some(false), Some(false), Some(false)), false);
        assert!(offered.is_empty());
        // So does a legacy catalog whose installed probes all failed: never
        // resurface unrunnable agents just to avoid an empty picker.
        let mut missing = catalog(None, None, None);
        missing.iter_mut().for_each(|d| d.installed = false);
        assert!(offered_harnesses_impl(&missing, false).is_empty());
    }

    #[test]
    fn offered_harnesses_require_an_installed_cli() {
        let descriptor =
            |id: HarnessId, name: &str, enabled: Option<bool>, installed: bool| HarnessDescriptor {
                id,
                name: name.into(),
                supports_steering: true,
                steering_mode: hearth_proto::SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed,
                enabled,
            };
        // Enabled-but-missing-CLI agents stay out of the rail; an installed
        // enabled one rides along. A live engine no longer stamps that
        // combination (enablement follows detection), but a catalog from an
        // older engine still can — the filter is the cross-version defense.
        let catalog = vec![
            descriptor(HarnessId::Huginn, "Claude Code", Some(true), false),
            descriptor(HarnessId::Codex, "Codex", Some(true), false),
            descriptor(HarnessId::Grok, "Grok", Some(true), true),
        ];
        let offered = offered_harnesses_impl(&catalog, false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Grok]
        );
        // Nothing enabled AND installed: an empty offered set — the fresh
        // machine where the default-enabled Claude/Codex have no CLIs (#128).
        // No fallback: offering them again would only manufacture
        // NotInstalled errors at send; the composer shows the no-agents
        // state and blocks new sends instead.
        let catalog = vec![
            descriptor(HarnessId::Huginn, "Claude Code", Some(true), false),
            descriptor(HarnessId::Codex, "Codex", Some(false), false),
            descriptor(HarnessId::Grok, "Grok", Some(false), true),
        ];
        let offered = offered_harnesses_impl(&catalog, false);
        assert!(offered.is_empty());
    }
}
