//! Picking the wire values a run's model/effort/trait selections map onto: the
//! `session/set_config_option` calls and the legacy `session/set_model` one.

use serde_json::Value;

use hearth_proto::SessionMode;

use crate::HarnessError;

use super::models::{context_hint_1m, norm_id};

/// Pick the advertised model value for a requested model id. Agents differ in
/// what they advertise: full ids (`claude-opus-5`), SDK aliases
/// (`opus`, `sonnet`, `haiku` — the claude adapter), and long-context
/// variants in either hint spelling. Exact match first (with the 1M compose
/// when the run selects the 1M window), then a family-token fallback that
/// prefers a variant matching the requested context window.
fn pick_model_value(requested: &str, available: &[&str], context_1m: bool) -> Option<String> {
    if context_1m {
        for composed in [format!("{requested}[1m]"), format!("{requested}-1m")] {
            if available.contains(&composed.as_str()) {
                return Some(composed);
            }
        }
    }
    if available.contains(&requested) {
        return Some(requested.to_owned());
    }
    // Family fallback: "claude-opus-5" → "opus" matches "opus[1m]".
    let family = ["fable", "opus", "sonnet", "haiku", "gpt"]
        .into_iter()
        .find(|f| norm_id(requested).contains(f))?;
    let candidates: Vec<&&str> = available
        .iter()
        .filter(|v| norm_id(v).contains(family))
        .collect();
    candidates
        .iter()
        .find(|v| context_hint_1m(v) == context_1m)
        .or_else(|| candidates.first())
        .map(|v| (**v).to_owned())
}

/// Pick a first-class ACP model switch when the session uses the legacy
/// `models` state instead of a category=model config option. Grok Build 1.0.5
/// has exactly this shape and accepts the selected id through
/// `session/set_model`; treating its advertised models as config options makes
/// the picker look functional while every selection is silently ignored.
///
/// Config options remain preferred when present: org adapters can expose a
/// legacy `models` matrix alongside the canonical base-model config option.
pub(crate) fn first_class_model_change(
    session_response: &Value,
    requested: Option<&str>,
) -> Result<Option<String>, HarnessError> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let has_model_config = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options.iter().any(|option| {
                option.get("type").and_then(Value::as_str) == Some("select")
                    && option.get("category").and_then(Value::as_str) == Some("model")
            })
        });
    if has_model_config {
        return Ok(None);
    }

    let Some(models) = session_response.get("models") else {
        return Ok(None);
    };
    let available: Vec<&str> = models
        .get("availableModels")
        .and_then(Value::as_array)
        .map(|models| models.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|model| model.get("modelId").and_then(Value::as_str))
        .collect();
    if available.is_empty() {
        return Ok(None);
    }
    if !available.contains(&requested) {
        return Err(HarnessError::Protocol(format!(
            "agent does not advertise requested model {requested}; available models: {}",
            available.join(", ")
        )));
    }
    if models.get("currentModelId").and_then(Value::as_str) == Some(requested) {
        return Ok(None);
    }
    Ok(Some(requested.to_owned()))
}

/// The `session/set_config_option` calls a session response's `configOptions`
/// warrant for this run:
/// - the requested model (category `model`; a `contextWindow: "1m"` model
///   option composes the `<model>[1m]` id first, the CLI's own convention),
/// - the effort (category `thought_level`, first advertised value from the
///   spec's preference list),
/// - any remaining `model_options` matched by normalized id — selects take
///   the choice id, booleans take `on`/`true` truthiness (fastMode, thinking).
///
/// Matched against advertised values and skipped when already current. Pure
/// so it's testable; the returned value is the request's flattened `value`
/// payload (select: `{"value": id}`, boolean: `{"type":"boolean","value": b}`).
pub(crate) fn config_option_sets(
    session_response: &Value,
    model: Option<&str>,
    efforts: &[&'static str],
    model_options: &serde_json::Map<String, Value>,
    mode: Option<SessionMode>,
) -> Vec<(String, Value)> {
    let Some(options) = session_response
        .get("configOptions")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let context_1m = model_options
        .get("contextWindow")
        .and_then(Value::as_str)
        .is_some_and(|w| w.eq_ignore_ascii_case("1m"));
    let mut sets = Vec::new();
    for option in options {
        let Some(config_id) = option.get("id").and_then(Value::as_str) else {
            continue;
        };
        let kind = option.get("type").and_then(Value::as_str).unwrap_or("");
        let category = option.get("category").and_then(Value::as_str);
        let current = option.get("currentValue");
        let available: Vec<&str> = option
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.get("value").and_then(Value::as_str))
            .collect();

        let wanted: Option<Value> = match (kind, category) {
            ("select", Some("model")) => model
                .and_then(|m| pick_model_value(m, &available, context_1m))
                .map(Value::String),
            // Two different "mode" selects share this category:
            //
            // - Raven (and similar) advertise plan/agent/chat as the
            //   session interaction mode. Honor `request.mode` so the
            //   composer chip actually switches the live toolset.
            // - Claude/Codex advertise permission/sandbox modes
            //   (bypassPermissions, agent-full-access). Those stay on
            //   the unattended no-prompts pick unless a trait overrode
            //   them. Mixing the two lists would map Hearth's "Agent"
            //   chip onto Codex's `agent` instead of `agent-full-access`.
            ("select", Some("mode")) => {
                let session_modes = available.iter().any(|v| matches!(*v, "plan" | "chat"));
                if session_modes {
                    mode.map(|m| m.as_str())
                        .filter(|c| available.contains(c))
                        .map(|c| Value::String(c.to_owned()))
                } else {
                    model_options
                        .get("mode")
                        .and_then(Value::as_str)
                        .filter(|c| available.contains(c))
                        .map(|c| Value::String(c.to_owned()))
                        .or_else(|| {
                            [
                                "bypassPermissions",
                                "bypass_permissions",
                                "yolo",
                                "agent-full-access",
                                "danger-full-access",
                                "full-access",
                            ]
                            .into_iter()
                            .find(|v| available.contains(v))
                            .map(|v| Value::String(v.to_owned()))
                        })
                }
            }
            ("select", Some("thought_level")) => efforts
                .iter()
                .find(|c| available.contains(*c))
                .map(|c| Value::String((*c).to_owned())),
            // Everything else: best-effort match against the run's
            // model-option selections by normalized id.
            _ => model_options.iter().find_map(|(opt_id, choice)| {
                if norm_id(opt_id) != norm_id(config_id) || opt_id == "contextWindow" {
                    return None;
                }
                match kind {
                    "select" => choice
                        .as_str()
                        .filter(|c| available.contains(c))
                        .map(|c| Value::String(c.to_owned())),
                    "boolean" => {
                        let on = choice == &Value::Bool(true)
                            || choice
                                .as_str()
                                .is_some_and(|c| c.eq_ignore_ascii_case("on"));
                        Some(Value::Bool(on))
                    }
                    _ => None,
                }
            }),
        };
        if let Some(value) = wanted
            && current != Some(&value)
        {
            let payload = match value {
                Value::Bool(b) => serde_json::json!({ "type": "boolean", "value": b }),
                other => serde_json::json!({ "value": other }),
            };
            sets.push((config_id.to_owned(), payload));
        }
    }
    sets
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_option_sets_map_model_effort_and_model_options() {
        let response = json!({
            "sessionId": "s-1",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "claude-sonnet-5",
                    "options": [
                        { "value": "claude-sonnet-5", "name": "Sonnet 5" },
                        { "value": "claude-opus-5", "name": "Opus 5" },
                        { "value": "claude-opus-5[1m]", "name": "Opus 5 (1M)" },
                    ],
                },
                {
                    "id": "effort",
                    "name": "Reasoning effort",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "high",
                    "options": [
                        { "value": "low", "name": "Low" },
                        { "value": "medium", "name": "Medium" },
                        { "value": "high", "name": "High" },
                        { "value": "max", "name": "Max" },
                    ],
                },
                {
                    "id": "fast_mode",
                    "name": "Fast mode",
                    "category": "model_config",
                    "type": "boolean",
                    "currentValue": false,
                },
            ],
        });
        let no_opts = serde_json::Map::new();
        // Model switch + effort preference list; fastMode untouched without a
        // model-option selection.
        assert_eq!(
            config_option_sets(
                &response,
                Some("claude-opus-5"),
                &["medium"],
                &no_opts,
                None
            ),
            vec![
                ("model".to_owned(), json!({ "value": "claude-opus-5" })),
                ("effort".to_owned(), json!({ "value": "medium" })),
            ]
        );
        // Effort preference order: first ADVERTISED candidate wins.
        assert_eq!(
            config_option_sets(&response, None, &["xhigh", "max"], &no_opts, None),
            vec![("effort".to_owned(), json!({ "value": "max" }))]
        );
        // contextWindow=1m composes the [1m] model id; fastMode=on matches the
        // boolean option across naming styles (fastMode vs fast_mode).
        let mut opts = serde_json::Map::new();
        opts.insert("contextWindow".into(), json!("1m"));
        opts.insert("fastMode".into(), json!("on"));
        assert_eq!(
            config_option_sets(&response, Some("claude-opus-5"), &["high"], &opts, None),
            vec![
                ("model".to_owned(), json!({ "value": "claude-opus-5[1m]" })),
                (
                    "fast_mode".to_owned(),
                    json!({ "type": "boolean", "value": true })
                ),
            ]
        );
        // Already-current values and unadvertised models set nothing.
        assert_eq!(
            config_option_sets(
                &response,
                Some("claude-sonnet-5"),
                &["high"],
                &no_opts,
                None
            ),
            Vec::new()
        );
        assert_eq!(
            config_option_sets(&response, Some("gpt-5.6-sol"), &[], &no_opts, None),
            Vec::new()
        );
        // No configOptions advertised → nothing to set.
        assert_eq!(
            config_option_sets(
                &json!({"sessionId": "s"}),
                Some("x"),
                &["high"],
                &no_opts,
                None
            ),
            Vec::new()
        );
    }

    #[test]
    fn first_class_models_use_session_set_model_without_config_option() {
        let response = json!({
            "models": {
                "currentModelId": "grok-4.6",
                "availableModels": [
                    { "modelId": "grok-4.6", "name": "Grok 4.6" },
                    { "modelId": "grok-4.5", "name": "Grok 4.5" },
                ],
            },
        });
        assert_eq!(
            first_class_model_change(&response, Some("grok-4.5")).unwrap(),
            Some("grok-4.5".into())
        );
        assert_eq!(
            first_class_model_change(&response, Some("grok-4.6")).unwrap(),
            None
        );
        assert!(first_class_model_change(&response, Some("unknown")).is_err());
    }

    #[test]
    fn model_config_option_takes_precedence_over_legacy_models_state() {
        let response = json!({
            "models": {
                "currentModelId": "gpt-5.6-sol low",
                "availableModels": [
                    { "modelId": "gpt-5.6-sol low", "name": "GPT-5.6-Sol (low)" },
                ],
            },
            "configOptions": [{
                "id": "model",
                "category": "model",
                "type": "select",
                "currentValue": "gpt-5.6-sol",
                "options": [
                    { "value": "gpt-5.6-sol", "name": "GPT-5.6-Sol" },
                    { "value": "gpt-5.6-terra", "name": "GPT-5.6-Terra" },
                ],
            }],
        });
        assert_eq!(
            first_class_model_change(&response, Some("gpt-5.6-terra")).unwrap(),
            None
        );
    }

    #[test]
    fn mode_config_option_prefers_a_no_prompt_mode_per_adapter_naming() {
        let codex = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "mode",
                "category": "mode",
                "type": "select",
                "currentValue": "agent",
                "options": [
                    { "value": "read-only" },
                    { "value": "agent" },
                    { "value": "agent-full-access" },
                ],
            }],
        });
        let no_opts = serde_json::Map::new();
        assert_eq!(
            config_option_sets(&codex, None, &[], &no_opts, None),
            vec![("mode".to_owned(), json!({ "value": "agent-full-access" }))]
        );
        // A Hearth plan/agent/chat pick must not override Codex's permission
        // mode with the overlapping "agent" value.
        assert_eq!(
            config_option_sets(&codex, None, &[], &no_opts, Some(SessionMode::Agent)),
            vec![("mode".to_owned(), json!({ "value": "agent-full-access" }))]
        );
    }

    #[test]
    fn raven_mode_config_option_uses_the_requested_session_mode() {
        let raven = json!({
            "sessionId": "s-1",
            "configOptions": [{
                "id": "mode",
                "category": "mode",
                "type": "select",
                "currentValue": "plan",
                "options": [
                    { "value": "plan" },
                    { "value": "agent" },
                    { "value": "chat" },
                ],
            }],
        });
        let no_opts = serde_json::Map::new();
        assert_eq!(
            config_option_sets(&raven, None, &[], &no_opts, Some(SessionMode::Agent)),
            vec![("mode".to_owned(), json!({ "value": "agent" }))]
        );
        // No explicit pick: leave the agent's advertised default (plan).
        assert_eq!(
            config_option_sets(&raven, None, &[], &no_opts, None),
            Vec::new()
        );
        // Already-current: nothing to set.
        assert_eq!(
            config_option_sets(&raven, None, &[], &no_opts, Some(SessionMode::Plan)),
            Vec::new()
        );
    }
}
