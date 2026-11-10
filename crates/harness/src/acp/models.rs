//! Mapping a `session/new` response onto the picker's model list: the
//! advertised config options, the legacy first-class `models` state, the
//! `[1m]` long-context folding, and the non-model config options exposed as
//! model traits.

use serde_json::Value;

use hearth_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};

/// Map an advertised `thought_level` value id onto hearth's ladder.
fn reasoning_from_value(value: &str) -> Option<ReasoningLevel> {
    match norm_id(value).as_str() {
        "minimal" => Some(ReasoningLevel::Minimal),
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "xhigh" => Some(ReasoningLevel::XHigh),
        "max" => Some(ReasoningLevel::Max),
        "ultra" => Some(ReasoningLevel::Ultra),
        "ultracode" => Some(ReasoningLevel::Ultracode),
        "ultrathink" => Some(ReasoningLevel::Ultrathink),
        _ => None,
    }
}

/// Derive the model list a `session/new` response advertises. The `model`
/// config option's choices come FIRST, the legacy first-class `models` state
/// is only a fallback: the org adapters enumerate one `availableModels` entry
/// per model × effort combination on that deprecated surface (Zed dropped it
/// entirely), while their `configOptions` carry base model ids with effort as
/// a separate `thought_level` option. `[1m]`-suffixed long-context variants
/// collapse into the base model's Context Window trait, matching the static
/// catalogs. Traits come off the wire too — every select/boolean config
/// option outside mode/model/thought_level becomes a `ModelOption` — so
/// unmatched models keep fast mode etc.; the catalog only enriches matched
/// ids with label/description/per-model ladders.
fn context_window_option() -> ModelOption {
    ModelOption {
        id: "contextWindow".into(),
        label: "Context".into(),
        choices: vec![
            ModelOptionChoice {
                id: "200k".into(),
                label: "200K".into(),
            },
            ModelOptionChoice {
                id: "1m".into(),
                label: "1M".into(),
            },
        ],
        default_choice: "200k".into(),
    }
}

pub(crate) fn models_from_session(session_response: &Value, catalog: &[Model]) -> Vec<Model> {
    let config_options = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default();

    let ladder: Vec<ReasoningLevel> = config_options
        .iter()
        .find(|o| o.get("category").and_then(Value::as_str) == Some("thought_level"))
        .and_then(|o| o.get("options").and_then(Value::as_array))
        .map(|opts| {
            opts.iter()
                .filter_map(|o| o.get("value").and_then(Value::as_str))
                .filter_map(reasoning_from_value)
                .collect()
        })
        .unwrap_or_default();
    let wire_options: Vec<ModelOption> = config_options
        .iter()
        .filter_map(trait_from_config_option)
        .collect();

    let exact = |id: &str| catalog.iter().find(|m| norm_id(&m.id) == norm_id(id));
    // Family-alias catalog row: the claude adapter advertises bare aliases
    // (`opus`, `sonnet`, `haiku`) meaning "the current generation" — match
    // them to the first (flagship-ordered) catalog row of that family so
    // the picker shows the curated label/ladder ("Opus 5") instead of the
    // terse alias. Alphabetic-only ids ONLY: versioned ids
    // (`gpt-5.2-codex`) must never fuzzy-match a foreign row.
    let alias = |id: &str| {
        let norm = norm_id(id);
        (!norm.is_empty() && norm.chars().all(|c| c.is_ascii_alphabetic()))
            .then(|| catalog.iter().find(|m| norm_id(&m.id).contains(&norm)))
            .flatten()
    };
    let build = |id: &str,
                 name: Option<&str>,
                 description: Option<&str>,
                 options: Vec<ModelOption>|
     -> Model {
        let exact = exact(id);
        let aliased = if exact.is_none() { alias(id) } else { None };
        let known = exact.or(aliased);
        // The wire name wins for real ids; an ALIAS row's terse wire name
        // ("Opus") loses to the curated family label/description.
        Model {
            id: id.to_owned(),
            label: aliased
                .map(|m| m.label.clone())
                .or_else(|| name.map(str::to_owned))
                .or_else(|| known.map(|m| m.label.clone()))
                .unwrap_or_else(|| id.to_owned()),
            description: aliased
                .and_then(|m| m.description.clone())
                .or_else(|| description.map(str::to_owned))
                .or_else(|| known.and_then(|m| m.description.clone())),
            reasoning_levels: match known.filter(|m| !m.reasoning_levels.is_empty()) {
                Some(m) => m.reasoning_levels.clone(),
                None => ladder.clone(),
            },
            options,
        }
    };

    let model_select: Vec<&Value> = config_options
        .iter()
        .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
        .and_then(|o| o.get("options").and_then(Value::as_array))
        .map(|opts| opts.iter().collect())
        .unwrap_or_default();
    if !model_select.is_empty() {
        let raw_ids: Vec<&str> = model_select
            .iter()
            .filter_map(|o| o.get("value").and_then(Value::as_str))
            .collect();
        // `default` is an ALIAS row (Claude Code's "Default (recommended)"),
        // duplicating whichever real model the CLI resolves it to — dropped
        // whenever a real row exists (it read as clutter in the picker, user
        // request). Send-side, a chat that saved `default` still matches the
        // advertised value exactly.
        let has_real = raw_ids.iter().any(|id| norm_id(id) != "default");
        return model_select
            .iter()
            .filter_map(|o| {
                let id = o.get("value").and_then(Value::as_str)?;
                if has_real && norm_id(id) == "default" {
                    return None;
                }
                let name = o.get("name").and_then(Value::as_str);
                let description = o.get("description").and_then(Value::as_str);
                let mut options = wire_options.clone();
                if let Some(base) = strip_context_hint(id) {
                    // A 1M variant with its bare base advertised too folds
                    // into THAT row's Context Window trait (added below).
                    if raw_ids.contains(&base) {
                        return None;
                    }
                    // Orphan 1M variant (`opus[1m]` with no bare `opus` —
                    // the CLI pins the 1M window): present it AS the base
                    // model with the trait defaulting to 1M, instead of a
                    // one-off "Opus (1M context)" row (user request). The
                    // send path recomposes the advertised id via
                    // `pick_model_value`'s compose/family fallback.
                    let mut window = context_window_option();
                    window.default_choice = "1m".into();
                    options.push(window);
                    return Some(build(
                        base,
                        name.map(strip_trailing_parenthetical)
                            .filter(|n| !n.is_empty()),
                        description,
                        options,
                    ));
                }
                if raw_ids
                    .iter()
                    .any(|raw| strip_context_hint(raw) == Some(id))
                {
                    options.push(context_window_option());
                }
                Some(build(id, name, description, options))
            })
            .collect();
    }

    // Legacy fallback for agents predating session config options. The
    // catalog's own option sets apply here — nothing arrives on the wire.
    session_response
        .get("models")
        .and_then(|m| m.get("availableModels"))
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            let id = m.get("modelId").and_then(Value::as_str)?;
            Some(build(
                id,
                m.get("name").and_then(Value::as_str),
                m.get("description").and_then(Value::as_str),
                exact(id).map(|k| k.options.clone()).unwrap_or_default(),
            ))
        })
        .collect()
}

/// A session config option surfaced as a Traits-dropdown section. Mode is
/// hearth's own (forced to the no-prompts choice), model rides the model rows,
/// and thought_level is the Reasoning ladder — everything else the agent
/// advertises (fast mode, collaboration mode, agent persona, …) passes
/// through. `currentValue` doubles as the default: it is the state the
/// session opens in. Booleans render as an off/on select, mirroring the
/// catalogs (hearth never declares the boolean config capability, so adapters
/// send selects, but handle the shape defensively).
fn trait_from_config_option(option: &Value) -> Option<ModelOption> {
    if matches!(
        option.get("category").and_then(Value::as_str),
        Some("mode" | "model" | "thought_level")
    ) {
        return None;
    }
    let id = option.get("id").and_then(Value::as_str)?;
    let label = option.get("name").and_then(Value::as_str).unwrap_or(id);
    match option.get("type").and_then(Value::as_str)? {
        "select" => {
            let choices: Vec<ModelOptionChoice> = option
                .get("options")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|c| {
                    let id = c.get("value").and_then(Value::as_str)?;
                    Some(ModelOptionChoice {
                        id: id.to_owned(),
                        label: c
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_owned(),
                    })
                })
                .collect();
            let default_choice = option
                .get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| choices.first().map(|c| c.id.clone()))?;
            (choices.len() > 1).then(|| ModelOption {
                id: id.to_owned(),
                label: label.to_owned(),
                choices,
                default_choice,
            })
        }
        "boolean" => Some(ModelOption {
            id: id.to_owned(),
            label: label.to_owned(),
            choices: vec![
                ModelOptionChoice {
                    id: "off".into(),
                    label: "Off".into(),
                },
                ModelOptionChoice {
                    id: "on".into(),
                    label: "On".into(),
                },
            ],
            default_choice: if option.get("currentValue") == Some(&Value::Bool(true)) {
                "on".into()
            } else {
                "off".into()
            },
        }),
        _ => None,
    }
}

/// Normalize an option/model-option id for matching across naming styles
/// (`fastMode` == `fast_mode` == `fast-mode`).
pub(crate) fn norm_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Whether a model id carries the 1M-context hint, in either spelling: the
/// display form `opus[1m]` or the SDK-id form `claude-opus-4-6-1m`.
pub(crate) fn context_hint_1m(id: &str) -> bool {
    id.contains("[1m]") || id.ends_with("-1m")
}

/// The id with a trailing long-context hint removed; `None` when it carries
/// none.
fn strip_context_hint(id: &str) -> Option<&str> {
    id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
}

/// A wire display name with its trailing parenthetical removed
/// ("Opus (1M context)" → "Opus") — a folded base row must not keep the
/// variant tag.
fn strip_trailing_parenthetical(name: &str) -> &str {
    match name.rfind(" (") {
        Some(at) if name.ends_with(')') => name[..at].trim_end(),
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn models_prefer_the_model_config_option_over_legacy_available_models() {
        // codex-acp shape: the legacy models state enumerates model × effort,
        // the config options carry base ids + a separate thought_level select.
        let response = json!({
            "sessionId": "s-1",
            "models": {
                "currentModelId": "gpt-5.6-sol low",
                "availableModels": [
                    { "modelId": "gpt-5.6-sol low", "name": "GPT-5.6-Sol (low)" },
                    { "modelId": "gpt-5.6-sol medium", "name": "GPT-5.6-Sol (medium)" },
                    { "modelId": "gpt-5.6-terra low", "name": "GPT-5.6-Terra (low)" },
                ],
            },
            "configOptions": [
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "agent",
                    "options": [
                        { "value": "read-only", "name": "Read Only" },
                        { "value": "agent", "name": "Agent" },
                        { "value": "agent-full-access", "name": "Agent (full access)" },
                    ],
                },
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "gpt-5.6-sol",
                    "options": [
                        { "value": "gpt-5.6-sol", "name": "GPT-5.6-Sol", "description": "Frontier" },
                        { "value": "gpt-5.6-terra", "name": "GPT-5.6-Terra" },
                    ],
                },
                {
                    "id": "reasoning_effort",
                    "name": "Reasoning effort",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "medium",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                    ],
                },
                {
                    "id": "fast-mode",
                    "name": "Fast mode",
                    "category": "model_config",
                    "type": "select",
                    "currentValue": "off",
                    "options": [
                        { "value": "off", "name": "Off" },
                        { "value": "on", "name": "On" },
                    ],
                },
            ],
        });
        let models = models_from_session(&response, &crate::codex::catalog::static_models());
        // Two base models — never one row per effort variant.
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gpt-5.6-sol", "gpt-5.6-terra"]
        );
        // Catalog match keeps the curated per-model ladder; wire wins on
        // label/description.
        assert_eq!(models[0].label, "GPT-5.6-Sol");
        assert_eq!(models[0].description.as_deref(), Some("Frontier"));
        assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
        // Wire config options become traits; mode/model/thought_level do not.
        assert_eq!(
            models[0]
                .options
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fast-mode"]
        );
        assert_eq!(models[0].options[0].default_choice, "off");
    }

    #[test]
    fn model_1m_variants_collapse_into_a_context_window_trait() {
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [
                {
                    "id": "model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "claude-sonnet-5",
                    "options": [
                        { "value": "claude-sonnet-5", "name": "Sonnet 5" },
                        { "value": "claude-sonnet-5[1m]", "name": "Sonnet 5 (1M)" },
                        // SDK-id hint spelling collapses too.
                        { "value": "claude-opus-4-6", "name": "Opus 4.6" },
                        { "value": "claude-opus-4-6-1m", "name": "Opus 4.6 (1M)" },
                        { "value": "claude-haiku-4-5", "name": "Haiku 4.5" },
                    ],
                },
            ],
        });
        let models = models_from_session(&response, &[]);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["claude-sonnet-5", "claude-opus-4-6", "claude-haiku-4-5"]
        );
        assert!(models[0].options.iter().any(|o| o.id == "contextWindow"));
        assert!(models[1].options.iter().any(|o| o.id == "contextWindow"));
        assert!(models[2].options.is_empty());
    }

    #[test]
    fn default_alias_drops_and_orphan_1m_variants_fold_to_their_base() {
        // The real claude adapter advertises a `default` alias row plus
        // `opus[1m]` with NO bare `opus` (the CLI pins the 1M window).
        // Both made the picker read like a settings dump (user report):
        // `default` duplicates a real model, and the orphan 1M variant now
        // presents AS its base model with the Context Window trait pinned
        // to 1M.
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "model",
                "category": "model",
                "type": "select",
                "currentValue": "claude-fable-5[1m]",
                "options": [
                    { "value": "default", "name": "Default (recommended)" },
                    { "value": "opus[1m]", "name": "Opus (1M context)" },
                    { "value": "claude-fable-5[1m]", "name": "Fable 5" },
                    { "value": "sonnet", "name": "Sonnet" },
                    { "value": "haiku", "name": "Haiku" },
                ],
            }],
        });
        let models = models_from_session(&response, &[]);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "claude-fable-5", "sonnet", "haiku"]
        );
        // The folded rows keep a de-parenthesized wire name (no catalog
        // here) and carry the 1M-pinned window trait.
        assert_eq!(models[0].label, "Opus");
        let window = models[0].options.iter().find(|o| o.id == "contextWindow");
        assert_eq!(window.map(|o| o.default_choice.as_str()), Some("1m"));
        assert!(
            models[1]
                .options
                .iter()
                .any(|o| o.id == "contextWindow" && o.default_choice == "1m")
        );
        // The bare aliases stay untouched.
        assert!(models[2].options.is_empty());
        assert!(models[3].options.is_empty());
    }

    #[test]
    fn claude_aliases_enrich_from_the_curated_catalog() {
        // Same wire shape, WITH the claude catalog: bare aliases pick up the
        // flagship row's curated label/description/ladder, versioned ids
        // keep their wire name.
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "model",
                "category": "model",
                "type": "select",
                "currentValue": "default",
                "options": [
                    { "value": "default", "name": "Default (recommended)" },
                    { "value": "opus[1m]", "name": "Opus (1M context)" },
                    { "value": "fable", "name": "Fable" },
                    { "value": "sonnet", "name": "Sonnet" },
                    { "value": "haiku", "name": "Haiku" },
                ],
            }],
        });
        let models = models_from_session(&response, &[]);
        assert!(
            models
                .iter()
                .any(|m| m.id.contains("opus") || m.label.contains("Opus"))
        );
        let foreign = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "model", "category": "model", "type": "select",
                "options": [{ "value": "claude-opus-9-mini", "name": "Opus 9 Mini" }],
            }],
        });
        let models = models_from_session(&foreign, &[]);
        assert_eq!(models[0].label, "Opus 9 Mini");
    }

    #[test]
    fn models_fall_back_to_legacy_state_with_catalog_options() {
        let response = json!({
            "sessionId": "s-1",
            "models": {
                "availableModels": [
                    { "modelId": "gpt-5.6-sol", "name": "GPT-5.6-Sol" },
                    { "modelId": "gpt-x", "name": "GPT-X" },
                ],
            },
        });
        let models = models_from_session(&response, &crate::codex::catalog::static_models());
        assert_eq!(models.len(), 2);
        // Catalog-matched id keeps the curated options on the legacy path…
        assert!(models[0].options.iter().any(|o| o.id == "serviceTier"));
        // …unknown ids get none.
        assert!(models[1].options.is_empty());
    }
}
