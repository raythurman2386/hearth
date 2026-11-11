//! Slash commands: token scanning, the cached command list, key routing and the
//! prompt popup.

use std::ops::Range;

use hearth_proto::{HarnessId, SlashCommand};
use hearth_rpc::methods;
use rune::TextEdit;

use crate::state::App;

use super::ComposerEvent;
use super::mentions::{MentionToken, selected_chat_row};

/// The `/` must open the input: slash commands are whole-prompt prefixes
/// (`/compact`, `/goal ship it`), so only the first token triggers, and a
/// query containing another `/` (a typed path) never does.
pub fn slash_token(text: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) || !text.starts_with('/') {
        return None;
    }
    let end = text
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at))
        .unwrap_or(text.len());
    // Cursor outside the command token (typing the argument): popup closed.
    if cursor == 0 || cursor > end {
        return None;
    }
    let query = &text[1..cursor];
    if query.contains('/') {
        return None;
    }
    Some(MentionToken {
        range: 0..end,
        query: query.to_string(),
    })
}

/// Slash-command completion state: like [`super::mentions::FileMentionState`]
/// but the candidate list is fetched once per harness (`ListCommands`) and
/// filtered locally per keystroke — no RPC, debounce, or skeleton churn while
/// typing.
#[derive(Debug, Default)]
pub struct SlashState {
    pub token: Option<MentionToken>,
    /// Indices into the cached command list, filter-ranked for the query.
    pub filtered: Vec<usize>,
    pub active: Option<usize>,
    /// Harness the popup is showing commands for (cache key).
    pub harness: Option<HarnessId>,
    pub request: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub dismissed: Option<(Range<usize>, String)>,
}

/// A failed command discovery, translated for the popup.
pub fn slash_error_message(err: &hearth_rpc::RpcError) -> String {
    match err {
        hearth_rpc::RpcError::UnknownMethod(_) => {
            "The session's device runs an older hearth — update it to list commands".to_string()
        }
        hearth_rpc::RpcError::Transport(_) | hearth_rpc::RpcError::Closed => {
            "The session's device is unreachable".to_string()
        }
        hearth_rpc::RpcError::BadParams(_) | hearth_rpc::RpcError::Failed(_) => {
            "Couldn't load this agent's commands".to_string()
        }
    }
}

/// Track the `/` token on every edit: open/refresh the popup, fetch the
/// harness's command list on first open, filter locally per keystroke.
pub fn update_slash(app: &mut App, text: &str, cursor: usize) {
    let token = slash_token(text, cursor);
    let still_dismissed = token.as_ref().is_some_and(|token| {
        app.composer
            .slash
            .dismissed
            .as_ref()
            .is_some_and(|(range, value)| {
                token.range == *range && text.get(range.clone()) == Some(value.as_str())
            })
    });
    if still_dismissed {
        app.composer.slash.token = None;
        return;
    }
    let harness = app.pickers.resolved(app).harness;
    let harness_changed = app.composer.slash.harness != harness;
    if token == app.composer.slash.token && !harness_changed {
        refilter_slash(app);
        return;
    }
    app.composer.slash.token = token.clone();
    app.composer.slash.harness = harness;
    app.composer.slash.error = None;
    if token.is_none() {
        app.composer.slash.active = None;
        return;
    }
    // No resolved harness (catalog still loading): empty popup, no fetch.
    let Some(harness) = harness else {
        app.composer.slash.loading = false;
        refilter_slash(app);
        return;
    };
    if app.composer.slash_cache.contains_key(&harness) {
        app.composer.slash.loading = false;
        refilter_slash(app);
        return;
    }
    let Some(engine) = app.engine.clone() else {
        app.composer.slash.loading = false;
        return;
    };
    // First open for this harness: one ListCommands, targeted like file search
    // (the chat/space host device owns the agent binary).
    app.composer.slash.request = app.composer.slash.request.wrapping_add(1);
    app.composer.slash.loading = true;
    refilter_slash(app);
    let target = selected_chat_row(app)
        .map(|chat| chat.device_id.clone())
        .or_else(|| app.selected_space_row().map(|s| s.device_id.clone()));
    let request = app.composer.slash.request;
    let tx = app.composer.sender(&app.bridge);
    app.bridge.spawn(async move {
        let mut params = serde_json::json!({ "harness": harness });
        if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
            object.insert("targetDeviceId".into(), target.clone().into());
        }
        let result = engine.client.call(methods::LIST_COMMANDS, params).await;
        let outcome = match result {
            Ok(value) => match serde_json::from_value::<Vec<SlashCommand>>(value) {
                Ok(commands) => Ok(commands),
                Err(err) => {
                    tracing::warn!(%err, "slash command decode failed");
                    Ok(Vec::new())
                }
            },
            Err(err) => {
                tracing::debug!(%err, "slash command discovery failed");
                Err(slash_error_message(&err))
            }
        };
        tx.send(ComposerEvent::CommandsLoaded {
            request,
            harness,
            outcome,
        });
    });
}

/// Re-rank the cached list for the current query (pure local filter).
pub fn refilter_slash(app: &mut App) {
    refilter_slash_state(&mut app.composer.slash, &app.composer.slash_cache);
}

/// [`refilter_slash`] over split borrows (the poll path drains events without
/// an `App`).
pub fn refilter_slash_state(
    state: &mut SlashState,
    cache: &std::collections::HashMap<HarnessId, Vec<SlashCommand>>,
) {
    let query = state
        .token
        .as_ref()
        .map(|t| t.query.clone())
        .unwrap_or_default();
    let commands: Vec<String> = state
        .harness
        .and_then(|h| cache.get(&h))
        .map(|list| list.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default();
    let names: Vec<&str> = commands.iter().map(String::as_str).collect();
    state.filtered = crate::pickers::filter_indices(&query, &names);
    state.active = (!state.filtered.is_empty()).then_some(0);
}

/// Clear the popup without remembering a dismissal (draft navigation).
pub fn dismiss_slash_state(state: &mut SlashState) {
    let request = state.request.wrapping_add(1);
    let harness = state.harness;
    *state = SlashState {
        request,
        harness,
        ..SlashState::default()
    };
}

pub fn move_slash(app: &mut App, delta: isize) {
    let len = app.composer.slash.filtered.len();
    app.composer.slash.active = crate::pickers::menu_step(app.composer.slash.active, len, delta);
}

pub fn dismiss_slash(app: &mut App) {
    let dismissed = app.composer.slash.token.as_ref().and_then(|token| {
        app.composer
            .edit
            .text
            .get(token.range.clone())
            .map(|text| (token.range.clone(), text.to_string()))
    });
    let request = app.composer.slash.request.wrapping_add(1);
    let harness = app.composer.slash.harness;
    app.composer.slash = SlashState {
        request,
        dismissed,
        harness,
        ..SlashState::default()
    };
}

/// Replace the `/query` token with the command's plain name.
pub fn replace_plain_token(edit: &mut TextEdit, range: Range<usize>, replacement: &str) {
    let next = edit.text[range.end.min(edit.text.len())..].chars().next();
    let has_separator = next.is_some_and(|ch| ch.is_whitespace() && ch != '\n' && ch != '\r');
    edit.set_selection(range.start, range.end);
    edit.insert(replacement);
    if !has_separator {
        edit.insert(" ");
    }
}

/// Accept the highlighted command.
pub fn accept_slash(app: &mut App) {
    let Some(token) = app.composer.slash.token.clone() else {
        return;
    };
    let Some(command) = app
        .composer
        .slash
        .active
        .and_then(|active| app.composer.slash.filtered.get(active))
        .and_then(|&ix| {
            app.composer
                .slash
                .harness
                .and_then(|h| app.composer.slash_cache.get(&h))
                .and_then(|c| c.get(ix))
        })
        .cloned()
    else {
        return;
    };
    replace_plain_token(
        &mut app.composer.edit,
        token.range,
        &format!("/{}", command.name),
    );
    dismiss_slash(app);
}

/// Accept a specific command (the popup's `on_choose`).
pub fn accept_slash_name(app: &mut App, name: &str) {
    let Some(token) = app.composer.slash.token.clone() else {
        return;
    };
    replace_plain_token(&mut app.composer.edit, token.range, &format!("/{name}"));
    dismiss_slash(app);
}

/// Keep the cached list current from a completed `ListCommands`.
pub fn apply_command_results(
    cache: &mut std::collections::HashMap<HarnessId, Vec<SlashCommand>>,
    state: &mut SlashState,
    request: u64,
    harness: HarnessId,
    outcome: Result<Vec<SlashCommand>, String>,
) -> bool {
    if state.request != request {
        return false;
    }
    state.loading = false;
    match outcome {
        Ok(commands) => {
            cache.insert(harness, commands);
        }
        Err(message) => state.error = Some(message),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_token_only_opens_the_prompt() {
        assert_eq!(
            slash_token("/comp", 5),
            Some(MentionToken {
                range: 0..5,
                query: "comp".into(),
            })
        );
        // Token range spans the whole command word even mid-cursor.
        assert_eq!(
            slash_token("/compact now", 3),
            Some(MentionToken {
                range: 0..8,
                query: "co".into(),
            })
        );
        // Not at offset 0 → prose, not a command.
        assert!(slash_token("run /compact", 12).is_none());
        // Cursor past the command word (typing the argument) → closed.
        assert!(slash_token("/goal ship it", 10).is_none());
        // A typed absolute path is not a command.
        assert!(slash_token("/usr/bin", 8).is_none());
        // Bare "/" with cursor at 0 → closed; cursor after it → open-all.
        assert!(slash_token("/", 0).is_none());
        assert_eq!(slash_token("/", 1).map(|t| t.query), Some(String::new()));
    }

    #[test]
    fn plain_token_replacement_keeps_a_separator() {
        let mut edit = TextEdit::new("/comp");
        replace_plain_token(&mut edit, 0..5, "/compact");
        assert_eq!(edit.text, "/compact ");
        let mut edit = TextEdit::new("/comp now");
        replace_plain_token(&mut edit, 0..5, "/compact");
        assert_eq!(edit.text, "/compact now");
    }

    #[test]
    fn command_results_only_apply_to_the_live_generation() {
        let mut cache = std::collections::HashMap::new();
        let mut state = SlashState {
            request: 2,
            loading: true,
            ..SlashState::default()
        };
        assert!(apply_command_results(
            &mut cache,
            &mut state,
            2,
            HarnessId::Huginn,
            Ok(vec![SlashCommand {
                name: "compact".into(),
                description: String::new(),
                input_hint: None,
            }])
        ));
        assert_eq!(cache.get(&HarnessId::Huginn).map(Vec::len), Some(1));
        assert!(!state.loading);
        assert!(!apply_command_results(
            &mut cache,
            &mut state,
            1,
            HarnessId::Huginn,
            Err("stale".into())
        ));
        assert!(state.error.is_none());
    }
}
