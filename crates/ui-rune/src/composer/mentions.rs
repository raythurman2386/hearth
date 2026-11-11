//! `@` file mentions: the strict `[basename](hearth-file:percent-encoded)`
//! transport, the token scan, and completion bound to Rune [`rune::TextEdit`]
//! pills.
//!
//! The prompt always submits the Markdown link: Rune pills carry only a label
//! plus an opaque id, so the id IS the encoded target and [`prompt_text`]
//! rebuilds the link on send. Drafts are stored in that same raw form and
//! re-parsed into pills by [`set_draft`].

use std::ops::Range;

use hearth_proto::FileSearchMatch;
use hearth_rpc::methods;
use rune::{Mark, TextEdit};

use crate::state::App;

use super::ComposerEvent;

/// A private URI scheme keeps file mentions distinguishable from ordinary
/// Markdown links pasted into the composer.
pub const FILE_MENTION_SCHEME: &str = "hearth-file:";

/// A strict, local-only Markdown representation of a file mention. The
/// underlying prompt always contains this form; the editor projects it to a
/// pill for display without leaking a second data model into submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMentionLink {
    pub range: Range<usize>,
    pub basename: String,
    pub path: String,
    pub is_dir: bool,
    /// The pill id: `hearth-file:{percent-encoded path}`.
    pub id: String,
}

pub fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

pub fn percent_decode_path(encoded: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let raw = encoded.as_bytes();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

pub fn escape_mention_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

/// The canonical mention link for a workspace-relative path.
pub fn local_file_link(path: &str, is_dir: bool) -> String {
    let path = path.trim_end_matches('/');
    let basename = path
        .rsplit('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(path);
    format!(
        "[{}]({}{})",
        escape_mention_label(basename),
        FILE_MENTION_SCHEME,
        percent_encode_path(&format!("{path}{}", if is_dir { "/" } else { "" }))
    )
}

/// The pill id for a path (the link target, validated on send).
pub fn mention_pill_id(path: &str, is_dir: bool) -> String {
    format!(
        "{FILE_MENTION_SCHEME}{}",
        percent_encode_path(&format!(
            "{}{}",
            path.trim_end_matches('/'),
            if is_dir { "/" } else { "" }
        ))
    )
}

pub fn local_path_is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn label_close(text: &str, start: usize) -> Option<usize> {
    let mut escaped = false;
    for (at, ch) in text[start..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == ']' && text[start + at + 1..].starts_with('(') {
            return Some(start + at);
        }
    }
    None
}

/// Every valid mention link in `text`, in order.
pub fn file_mention_links(text: &str) -> Vec<FileMentionLink> {
    let mut links = Vec::new();
    let mut search = 0;
    while let Some(relative_start) = text[search..].find('[') {
        let start = search + relative_start;
        let Some(label_end) = label_close(text, start + 1) else {
            search = start + 1;
            continue;
        };
        let target_start = label_end + 2;
        let Some(relative_end) = text[target_start..].find(')') else {
            search = start + 1;
            continue;
        };
        let end = target_start + relative_end + 1;
        let label = &text[start + 1..label_end];
        let Some(encoded) = text[target_start..end - 1].strip_prefix(FILE_MENTION_SCHEME) else {
            search = end;
            continue;
        };
        let parsed = percent_decode_path(encoded).and_then(|target| {
            let is_dir = target.ends_with('/');
            let path = target.strip_suffix('/').unwrap_or(&target);
            (local_path_is_safe(path)
                && percent_encode_path(&target) == encoded
                && path
                    .rsplit('/')
                    .next()
                    .is_some_and(|basename| escape_mention_label(basename) == label))
            .then(|| (path.to_string(), is_dir))
        });
        if let Some((path, is_dir)) = parsed {
            let basename = path.rsplit('/').next().unwrap_or_default().to_string();
            links.push(FileMentionLink {
                range: start..end,
                basename,
                path,
                is_dir,
                id: format!("{FILE_MENTION_SCHEME}{encoded}"),
            });
        }
        search = end;
    }
    links
}

/// Basenames are compact in the common case. When the same basename appears
/// more than once, use the shortest unique path suffix so chips remain
/// distinguishable without always expanding to full paths.
pub fn mention_display_labels(links: &[FileMentionLink]) -> Vec<String> {
    links
        .iter()
        .enumerate()
        .map(|(ix, link)| {
            if links
                .iter()
                .filter(|other| other.basename == link.basename)
                .count()
                == 1
            {
                return link.basename.clone();
            }
            let parts: Vec<_> = link.path.split('/').collect();
            (1..=parts.len())
                .map(|count| parts[parts.len() - count..].join("/"))
                .find(|suffix| {
                    let suffix: Vec<_> = suffix.split('/').collect();
                    links.iter().enumerate().all(|(other_ix, other)| {
                        other_ix == ix
                            || !other
                                .path
                                .split('/')
                                .rev()
                                .take(suffix.len())
                                .eq(suffix.iter().rev().copied())
                    })
                })
                .unwrap_or_else(|| link.path.clone())
        })
        .collect()
}

/// Decode a pill id into `(path, is_dir)` — strict: a hostile or stale id is
/// not a mention and submits as its plain label.
pub fn mention_from_pill_id(id: &str) -> Option<(String, bool)> {
    let encoded = id.strip_prefix(FILE_MENTION_SCHEME)?;
    let target = percent_decode_path(encoded)?;
    let is_dir = target.ends_with('/');
    let path = target.strip_suffix('/').unwrap_or(&target);
    if local_path_is_safe(path) && percent_encode_path(&target) == encoded {
        Some((path.to_string(), is_dir))
    } else {
        None
    }
}

/// The prompt text a send submits: every valid mention pill is written back as
/// its Markdown link (the display label may be a shortened suffix, so the
/// basename is recomputed from the path). Everything else passes through.
pub fn prompt_text(edit: &TextEdit) -> String {
    let pills: Vec<(Range<usize>, String)> = edit
        .pills()
        .map(|(range, id)| (range, id.to_string()))
        .collect();
    if pills.is_empty() {
        return edit.text.clone();
    }
    let mut out = String::with_capacity(edit.text.len());
    let mut at = 0usize;
    for (range, id) in pills {
        if range.start < at || range.end > edit.text.len() {
            continue;
        }
        out.push_str(&edit.text[at..range.start]);
        match mention_from_pill_id(&id) {
            Some((path, is_dir)) => out.push_str(&local_file_link(&path, is_dir)),
            None => out.push_str(&edit.text[range.clone()]),
        }
        at = range.end;
    }
    out.push_str(&edit.text[at..]);
    out
}

/// Replace the edit's content with a stored raw draft, re-materialising every
/// valid mention link as a pill (one undo step).
pub fn set_draft(edit: &mut TextEdit, raw: &str) {
    edit.replace_text(raw);
    let links = file_mention_links(raw);
    if links.is_empty() {
        return;
    }
    let labels = mention_display_labels(&links);
    for (link, label) in links.into_iter().zip(labels).rev() {
        edit.replace_with_pill(link.range, link.id, &label);
    }
}

/// Replace a completed `@query` token with a mention pill, keeping the caret
/// after it and ensuring a separating space.
pub fn replace_mention(edit: &mut TextEdit, range: Range<usize>, path: &str, is_dir: bool) -> bool {
    let Some((_, label)) = file_mention_links(&local_file_link(path, is_dir))
        .into_iter()
        .map(|link| (link.path, link.basename))
        .next()
    else {
        return false;
    };
    let label = if label.is_empty() {
        path.to_string()
    } else {
        label
    };
    let id = mention_pill_id(path, is_dir);
    let next = edit.text[range.end.min(edit.text.len())..].chars().next();
    let has_separator = next.is_some_and(|ch| ch.is_whitespace() && ch != '\n' && ch != '\r');
    edit.replace_with_pill(range, id, &label);
    if !has_separator {
        edit.insert(" ");
    }
    true
}

/// One `@` completion token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    pub range: Range<usize>,
    pub query: String,
}

/// The `@` must begin a token. This intentionally excludes `name@example.com`
/// and ordinary words while allowing punctuation such as `(@src`.
pub fn mention_token(text: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let token_start = text[..cursor]
        .char_indices()
        .rev()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at + ch.len_utf8()))
        .unwrap_or(0);
    let relative_at = text[token_start..cursor].rfind('@')?;
    let at = token_start + relative_at;
    let valid_boundary = at == 0
        || text[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{'));
    if text[at + 1..cursor].contains('@') || !valid_boundary {
        return None;
    }
    let end = text[cursor..]
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(cursor + at))
        .unwrap_or(text.len());
    Some(MentionToken {
        range: at..end,
        query: text[at + 1..cursor].to_string(),
    })
}

/// `@` completion state: token, ranked rows, the keyboard highlight, and the
/// request generation that guards stale RPC answers.
#[derive(Debug, Default)]
pub struct FileMentionState {
    pub token: Option<MentionToken>,
    pub results: Vec<FileSearchMatch>,
    pub active: Option<usize>,
    pub request: u64,
    pub loading: bool,
    /// Why the last search failed, for the popup. A failure MUST NOT render as
    /// "No matching files": cross-device searches fail for reasons the user can
    /// act on (host daemon too old for `SearchFiles`, device offline).
    pub error: Option<String>,
    /// Full token text, not just the cursor-relative query: moving within a
    /// dismissed token keeps it closed, while any edit re-enables completion.
    pub dismissed: Option<(Range<usize>, String)>,
}

pub fn mention_response_is_current(state: &FileMentionState, request: u64) -> bool {
    state.request == request && state.token.is_some()
}

/// A failed file search, translated for the popup.
pub fn mention_error_message(err: &hearth_rpc::RpcError) -> String {
    match err {
        hearth_rpc::RpcError::UnknownMethod(_) => {
            "The session's device runs an older hearth — update it to search its files".to_string()
        }
        hearth_rpc::RpcError::Transport(_) | hearth_rpc::RpcError::Closed => {
            "The session's device is unreachable".to_string()
        }
        hearth_rpc::RpcError::BadParams(_) | hearth_rpc::RpcError::Failed(_) => {
            "File search failed".to_string()
        }
    }
}

/// Feed the current edit text + caret through the mention state. Opens the
/// popup, debounces the search and posts results back as
/// [`ComposerEvent::MentionLoaded`].
pub fn update_mention(app: &mut App, text: &str, cursor: usize) {
    let still_dismissed = app.composer.mention.token.as_ref().is_some_and(|token| {
        app.composer
            .mention
            .dismissed
            .as_ref()
            .is_some_and(|(range, value)| {
                token.range == *range && text.get(range.clone()) == Some(value.as_str())
            })
    });
    if still_dismissed {
        app.composer.mention.token = None;
        return;
    }
    app.composer.mention.dismissed = None;
    let token = mention_token(text, cursor);
    if token == app.composer.mention.token {
        return;
    }
    let state = &mut app.composer.mention;
    state.request = state.request.wrapping_add(1);
    // Refining an open menu keeps the stale rows visible until the new
    // response lands — clearing here made the popup bounce through the
    // skeleton (and a different height) on every keystroke.
    let refining = state.token.is_some() && token.is_some();
    state.token = token.clone();
    if !refining {
        state.results.clear();
        state.active = None;
    }
    state.error = None;
    state.loading = token.is_some();
    let Some(token) = token else {
        return;
    };
    let selected_worktree = match app.pickers.checkout_plan() {
        crate::pickers::CheckoutPlan::ReuseWorktree { path, .. } => Some(path),
        _ => None,
    };
    let mut params = serde_json::Map::new();
    params.insert("query".into(), token.query.clone().into());
    let target = if let Some(chat) = selected_chat_row(app) {
        params.insert("chatId".into(), chat.id.clone().into());
        Some(chat.device_id.clone())
    } else if let Some(space) = app.selected_space_row() {
        params.insert("spaceId".into(), space.id.clone().into());
        if let Some(path) = selected_worktree {
            params.insert("path".into(), path.into());
        }
        Some(space.device_id.clone())
    } else {
        None
    };
    let Some(target) = target else {
        app.composer.mention.loading = false;
        return;
    };
    let Some(engine) = app.engine.clone() else {
        app.composer.mention.loading = false;
        return;
    };
    params.insert("targetDeviceId".into(), target.into());
    let params = serde_json::Value::Object(params);
    let request = app.composer.mention.request;
    let tx = app.composer.sender();
    app.bridge.spawn(async move {
        // A short debounce prevents one full workspace walk per keystroke
        // during normal typing. The generation check still guards requests
        // that were already in flight when the query changed.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let client = engine.client.clone();
        let mut result = client.call(methods::SEARCH_FILES, params.clone()).await;
        if matches!(
            result,
            Err(hearth_rpc::RpcError::Transport(_)) | Err(hearth_rpc::RpcError::Closed)
        ) {
            // One retry rides out a cold relay dial to the host device.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            result = client.call(methods::SEARCH_FILES, params).await;
        }
        let outcome = match result {
            Ok(value) => match serde_json::from_value::<Vec<FileSearchMatch>>(value) {
                Ok(results) => Ok(results),
                Err(err) => {
                    tracing::warn!(%err, "file mention response decode failed");
                    Ok(Vec::new())
                }
            },
            Err(err) => {
                tracing::warn!(%err, "file mention search failed");
                Err(mention_error_message(&err))
            }
        };
        let _ = tx.send(ComposerEvent::MentionLoaded { request, outcome });
    });
}

/// The selected chat row (a local helper: the composer must not depend on the
/// transcript module's accessors).
pub fn selected_chat_row(app: &App) -> Option<&hearth_proto::Chat> {
    let id = app.selected_chat.as_deref()?;
    app.chats.iter().find(|c| c.id == id)
}

/// Move the popup highlight.
pub fn move_mention(app: &mut App, delta: isize) {
    let len = app.composer.mention.results.len();
    app.composer.mention.active =
        crate::pickers::menu_step(app.composer.mention.active, len, delta);
}

/// Clear the popup without remembering a dismissal (draft navigation, wizard).
pub fn dismiss_mention_state(state: &mut super::ComposerState) {
    let request = state.mention.request.wrapping_add(1);
    state.mention = FileMentionState {
        request,
        ..FileMentionState::default()
    };
}

/// Close the popup and remember the token so moving inside it stays closed.
pub fn dismiss_mention(app: &mut App) {
    let dismissed = app.composer.mention.token.as_ref().and_then(|token| {
        app.composer
            .edit
            .text
            .get(token.range.clone())
            .map(|text| (token.range.clone(), text.to_string()))
    });
    let request = app.composer.mention.request.wrapping_add(1);
    app.composer.mention = FileMentionState {
        request,
        dismissed,
        ..FileMentionState::default()
    };
}

/// Accept the highlighted file: replace the token with a pill.
pub fn accept_mention(app: &mut App) {
    let Some(token) = app.composer.mention.token.clone() else {
        return;
    };
    let Some((path, is_dir)) = app
        .composer
        .mention
        .active
        .and_then(|active| app.composer.mention.results.get(active))
        .map(|result| (result.path.clone(), result.is_dir))
    else {
        return;
    };
    replace_mention(&mut app.composer.edit, token.range, &path, is_dir);
    dismiss_mention(app);
}

/// Accept a specific row (the popup's `on_choose`): find its `is_dir` in the
/// live results, replace the token and close.
pub fn accept_mention_path(app: &mut App, path: &str) {
    let Some(token) = app.composer.mention.token.clone() else {
        return;
    };
    let is_dir = app
        .composer
        .mention
        .results
        .iter()
        .find(|r| r.path == path)
        .map(|r| r.is_dir)
        .unwrap_or(false);
    replace_mention(&mut app.composer.edit, token.range, path, is_dir);
    dismiss_mention(app);
}

/// Keep the popup's rows in sync with a completed search.
pub fn apply_mention_results(
    state: &mut FileMentionState,
    request: u64,
    outcome: Result<Vec<FileSearchMatch>, String>,
) -> bool {
    if !mention_response_is_current(state, request) {
        return false;
    }
    state.loading = false;
    match outcome {
        Ok(results) => {
            state.error = None;
            state.active = (!results.is_empty()).then_some(0);
            state.results = results;
        }
        Err(message) => {
            state.results.clear();
            state.active = None;
            state.error = Some(message);
        }
    }
    true
}

/// The label Rune should draw inside a mention pill.
pub fn pill_label(id: &str) -> Option<String> {
    let (path, _) = mention_from_pill_id(id)?;
    Some(path.rsplit('/').next().unwrap_or(&path).to_string())
}

/// Whether a pill id is a mention (for `PillStyle` dispatch).
pub fn is_mention_pill(id: &str) -> bool {
    matches!(mention_from_pill_id(id), Some(_))
}

/// A pill id of the `@` prefix (`Mark::Pill` ids are app-defined).
pub fn mention_ids(edit: &TextEdit) -> Vec<(Range<usize>, String)> {
    edit.pills()
        .filter(|(_, id)| id.starts_with(FILE_MENTION_SCHEME))
        .map(|(range, id)| (range, id.to_string()))
        .collect()
}

/// Marker so callers can see the mark kind used for mentions.
pub const MENTION_MARK: &str = "pill";

/// The `Mark::Pill` variant accessor (documents the binding explicitly).
pub fn is_pill_mark(mark: &Mark) -> bool {
    matches!(mark, Mark::Pill(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_token_requires_a_token_boundary_and_tracks_full_token() {
        assert_eq!(
            mention_token("Fix @src/com", 12),
            Some(MentionToken {
                range: 4..12,
                query: "src/com".into(),
            })
        );
        assert!(mention_token("mail@example.com", 16).is_none());
        assert!(mention_token("word@file", 9).is_none());
        assert!(mention_token("path/@file", 10).is_none());
        assert_eq!(
            mention_token("See (@lib", 9).map(|token| token.range),
            Some(5..9)
        );
    }

    #[test]
    fn dismissed_mentions_reject_stale_responses() {
        let mut state = FileMentionState {
            token: mention_token("@src", 4),
            request: 7,
            ..FileMentionState::default()
        };
        assert!(mention_response_is_current(&state, 7));
        state.request += 1;
        state.token = None;
        assert!(!mention_response_is_current(&state, 7));
        assert!(!mention_response_is_current(&state, 8));
    }

    #[test]
    fn file_mentions_serialize_to_strict_local_markdown() {
        let raw = local_file_link("src/a file#[x].rs", false);
        assert_eq!(
            raw,
            "[a file#\\[x\\].rs](hearth-file:src/a%20file%23%5Bx%5D.rs)"
        );
        let links = file_mention_links(&raw);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path, "src/a file#[x].rs");
        assert_eq!(links[0].basename, "a file#[x].rs");
        assert!(!links[0].is_dir);
        assert_eq!(links[0].id, "hearth-file:src/a%20file%23%5Bx%5D.rs");

        let folder = local_file_link("src/components", true);
        assert_eq!(folder, "[components](hearth-file:src/components/)");
        let links = file_mention_links(&folder);
        assert_eq!(links[0].path, "src/components");
        assert!(links[0].is_dir);
    }

    #[test]
    fn file_mentions_reject_external_or_noncanonical_markdown() {
        assert!(file_mention_links("[site](https://example.com/a)").is_empty());
        assert!(file_mention_links("[a.rs](../a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a file.rs)").is_empty());
        assert!(file_mention_links("[other](src/a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src%5Cfake%5Ca.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a%0A.rs)").is_empty());
    }

    #[test]
    fn duplicate_mention_basenames_use_unique_suffixes() {
        let raw = format!(
            "{} {}",
            local_file_link("src/one/mod.rs", false),
            local_file_link("src/two/mod.rs", false)
        );
        let links = file_mention_links(&raw);
        let labels = mention_display_labels(&links);
        assert_eq!(
            labels,
            vec!["one/mod.rs".to_string(), "two/mod.rs".to_string()]
        );
    }

    #[test]
    fn mention_suffixes_compare_path_components() {
        let links = vec![
            FileMentionLink {
                range: 0..0,
                basename: "mod.rs".into(),
                path: "foo/mod.rs".into(),
                is_dir: false,
                id: String::new(),
            },
            FileMentionLink {
                range: 0..0,
                basename: "oomod.rs".into(),
                path: "bar/oomod.rs".into(),
                is_dir: false,
                id: String::new(),
            },
        ];
        assert_eq!(
            mention_display_labels(&links),
            vec!["mod.rs".to_string(), "oomod.rs".to_string()]
        );
    }

    #[test]
    fn pill_ids_round_trip_and_reject_hostile_paths() {
        let id = mention_pill_id("src/composer.rs", false);
        assert_eq!(id, "hearth-file:src/composer.rs");
        assert_eq!(
            mention_from_pill_id(&id),
            Some(("src/composer.rs".into(), false))
        );
        assert_eq!(
            mention_from_pill_id("hearth-file:src/components/"),
            Some(("src/components".into(), true))
        );
        assert_eq!(mention_from_pill_id("hearth-file:../a.rs"), None);
        assert_eq!(mention_from_pill_id("file:src/a.rs"), None);
        assert_eq!(mention_from_pill_id("hearth-file:src%5Cfake.rs"), None);
    }

    #[test]
    fn prompt_text_rebuilds_links_from_pills() {
        let mut edit = TextEdit::new("");
        edit.replace_text("open  now");
        edit.replace_with_pill(
            5..5,
            mention_pill_id("src/composer.rs", false),
            "composer.rs",
        );
        assert_eq!(
            prompt_text(&edit),
            "open [composer.rs](hearth-file:src/composer.rs) now"
        );
        // A non-mention pill submits its label text.
        let mut plain = TextEdit::new("a  b");
        plain.replace_with_pill(2..2, "image:1", "image");
        assert_eq!(prompt_text(&plain), "a image b");
    }

    #[test]
    fn draft_round_trip_restores_pills() {
        let raw = format!(
            "check {} and {}",
            local_file_link("src/composer.rs", false),
            local_file_link("src/components", true)
        );
        let mut edit = TextEdit::new("");
        set_draft(&mut edit, &raw);
        assert_eq!(edit.text, "check composer.rs and components");
        assert_eq!(edit.pills().count(), 2);
        assert_eq!(prompt_text(&edit), raw);
    }

    #[test]
    fn results_apply_only_for_the_live_generation() {
        let mut state = FileMentionState {
            token: mention_token("@src", 4),
            request: 3,
            loading: true,
            ..FileMentionState::default()
        };
        assert!(apply_mention_results(
            &mut state,
            3,
            Ok(vec![FileSearchMatch {
                path: "src/a.rs".into(),
                is_dir: false,
            }])
        ));
        assert_eq!(state.active, Some(0));
        assert!(!state.loading);
        // Stale answer: unchanged.
        let mut state2 = FileMentionState {
            token: mention_token("@src", 4),
            request: 4,
            ..FileMentionState::default()
        };
        assert!(!apply_mention_results(&mut state2, 3, Ok(Vec::new())));
        assert!(apply_mention_results(&mut state2, 4, Err("offline".into())));
        assert_eq!(state2.error.as_deref(), Some("offline"));
        assert_eq!(state2.active, None);
    }

    #[test]
    fn pill_labels_are_basenames() {
        assert_eq!(
            pill_label(&mention_pill_id("src/composer.rs", false)).as_deref(),
            Some("composer.rs")
        );
        assert!(is_mention_pill(&mention_pill_id("src/a.rs", false)));
        assert!(!is_mention_pill("image:1"));
        assert!(is_pill_mark(&Mark::Pill("x".into())));
    }
}
