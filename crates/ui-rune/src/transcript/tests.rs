//! Pure transcript tests: row-model invariants, cache bounds, and the scroll
//! policy that survives the Rune port. The framework-specific render tests live
//! with their modules.

use std::collections::HashMap;

use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};

use super::chips::{FoldState, blob_detail, format_kb};
use super::model::{RowKind, ToolDetail, ToolItem, format_timestamp, wrap_thought_lines};
use super::*;

fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts: vec![MessagePart::Text {
            id: "p0".into(),
            text: text.into(),
        }],
        created_at: 0,
        device_id: "d".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    }
}

#[test]
fn blob_cache_evicts_least_recently_fetched() {
    let mut details = HashMap::new();
    let mut order = HashMap::new();
    for ix in 0..MAX_BLOB_DETAILS + 3 {
        let key = format!("blob{ix}");
        details.insert(key.clone(), BlobFetch::Failed);
        order.insert(key, ix as u64);
    }
    prune_blob_cache(&mut details, &mut order, "blob3");
    assert_eq!(details.len(), MAX_BLOB_DETAILS);
    assert!(details.contains_key("blob3"), "the inserted ref survives");
    assert!(!details.contains_key("blob0"));
}

#[test]
fn blob_cache_prefers_settled_entries() {
    let mut details = HashMap::new();
    let mut order = HashMap::new();
    details.insert("loading".into(), BlobFetch::Loading);
    order.insert("loading".into(), 0);
    details.insert("ready".into(), BlobFetch::Failed);
    order.insert("ready".into(), 1);
    details.insert("ready2".into(), BlobFetch::Failed);
    order.insert("ready2".into(), 2);
    // A map over the cap: the oldest settled entry goes before the loading one.
    while details.len() <= MAX_BLOB_DETAILS {
        let key = format!("extra{}", details.len());
        order.insert(key.clone(), details.len() as u64 + 10);
        details.insert(key, BlobFetch::Failed);
    }
    prune_blob_cache(&mut details, &mut order, "keep");
    assert!(details.contains_key("loading"));
    assert!(!details.contains_key("ready"));
}

#[test]
fn tool_detail_folds_are_capped() {
    let mut folds = HashMap::new();
    for ix in 0..MAX_TOOL_DETAILS + 4 {
        folds.insert(
            format!("k{ix}"),
            FoldState {
                touched: ix as u64,
                ..FoldState::default()
            },
        );
    }
    prune_tool_details(&mut folds, "k0");
    assert_eq!(folds.len(), MAX_TOOL_DETAILS);
    assert!(folds.contains_key("k0"));
}

#[test]
fn blob_detail_parses_output_and_diff_payloads() {
    let text = (0..30)
        .map(|ix| format!("line {ix}"))
        .collect::<Vec<_>>()
        .join("\n");
    let detail = blob_detail(&text, false).expect("output");
    assert!(matches!(detail, ToolDetail::Output { .. }));
    assert!(detail_height(&detail) > 0.0);
    assert_eq!(format_kb(2048), "2 KB");
}

#[test]
fn empty_thought_lines_are_dropped() {
    let lines = wrap_thought_lines("one\n\n\ntwo\n");
    assert_eq!(lines, vec!["one", "", "", "two"]);
    assert_eq!(
        format_timestamp(0, &chrono::FixedOffset::east_opt(0).unwrap()),
        "Jan 1, 12:00 AM"
    );
}

#[test]
fn row_kinds_keep_their_payloads() {
    let _ = RowKind::ErrorChip {
        message: "x".into(),
    };
    let _ = ToolDetail::Output {
        lines: vec!["a".into()],
        truncated_by: 0,
    };
    let _ = ToolItem {
        call: hearth_proto::ToolCall::Exec {
            command: "ls".into(),
        },
        is_error: false,
        resolved: true,
        detail: None,
        invocation: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        subagent_ref: None,
        subagent_status: None,
        subagent_tail: None,
        is_thought: false,
    };
}

#[test]
fn file_mentions_project_to_display_labels_with_chip_ranges() {
    let link = crate::composer::mentions::local_file_link("src/main.rs", false);
    let text = format!("read {link} now");
    let (display, spans) = super::model::project_mentions(&text);
    assert_eq!(display, "read main.rs now");
    assert_eq!(spans.len(), 1);
    assert_eq!(&display[spans[0].range.clone()], "main.rs");
}

#[test]
fn rows_build_for_both_roles() {
    let mut parse =
        |_key: &str, text: &str| std::sync::Arc::new(crate::markdown::parser::parse_full(text));
    let user = rows_for_entry(&entry("u", MessageRole::User, "hi"), false, &mut parse);
    assert!(matches!(user[0].kind, RowKind::User { .. }));
    let assistant = rows_for_entry(&entry("a", MessageRole::Assistant, "hi"), false, &mut parse);
    assert!(matches!(assistant[0].kind, RowKind::Markdown { .. }));
}

#[test]
fn live_commit_changes_only_tail_row_versions() {
    // A streaming commit that appends to the last block leaves every settled
    // block row's (id, version) untouched, so the diff splices only the tail.
    let mut live1 = entry("m1", MessageRole::Assistant, "one\n\ntwo\n\nthree");
    live1.status = Some(MessageStatus::Streaming);
    let mut live2 = entry(
        "m1",
        MessageRole::Assistant,
        "one\n\ntwo\n\nthree grows here",
    );
    live2.status = Some(MessageStatus::Streaming);
    let mut parse =
        |_key: &str, text: &str| std::sync::Arc::new(crate::markdown::parser::parse_full(text));
    let r1 = rows_for_entry(&live1, false, &mut parse);
    let r2 = rows_for_entry(&live2, false, &mut parse);
    assert_eq!(r1.len(), 3);
    assert_eq!(r2.len(), 3);
    assert_eq!(r1[0].version, r2[0].version, "settled block untouched");
    assert_eq!(r1[1].version, r2[1].version, "settled block untouched");
    assert_ne!(r1[2].version, r2[2].version, "tail block respliced");
    assert_eq!(diff_rows(&r1, &r2), Some((2..3, 1)));
}

#[test]
fn user_rows_and_echo_versions() {
    let mut confirmed_entry = entry("u1", MessageRole::User, "hello");
    confirmed_entry.status = None;
    let mut parse =
        |_key: &str, text: &str| std::sync::Arc::new(crate::markdown::parser::parse_full(text));
    let confirmed = rows_for_entry(&confirmed_entry, false, &mut parse);
    let echoed = rows_for_entry(&confirmed_entry, true, &mut parse);
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0].id, echoed[0].id);
    // Pending → confirmed changes the version so the row re-renders.
    assert_ne!(confirmed[0].version, echoed[0].version);
    assert!(matches!(
        &echoed[0].kind,
        RowKind::User { pending: true, .. }
    ));
}

#[test]
fn subagent_rows_are_cached_by_entry_fingerprint() {
    let app = App::new(crate::UiConfig::default());
    let mut doc_entry = entry("a1", MessageRole::Assistant, "one\n\ntwo\n");
    let first = app.transcript_state.rows_for_doc("doc-1", &doc_entry);
    let second = app.transcript_state.rows_for_doc("doc-1", &doc_entry);
    assert_eq!(
        first.iter().map(|row| &row.id).collect::<Vec<_>>(),
        second.iter().map(|row| &row.id).collect::<Vec<_>>()
    );
    assert_eq!(app.transcript_state.sub_cache.borrow().len(), 1);
    assert!(!first.iter().all(|row| row.version == 0));

    // A changed entry re-parses (a new build replaces the cached one).
    doc_entry.parts = vec![MessagePart::Text {
        id: "p0".into(),
        text: "rewritten now".into(),
    }];
    let third = app.transcript_state.rows_for_doc("doc-1", &doc_entry);
    assert_eq!(app.transcript_state.sub_cache.borrow().len(), 1);
    assert_ne!(
        first[0].version, third[0].version,
        "a changed entry must rebuild"
    );

    // Streaming entries bypass the cache (the live parser advances).
    let mut streaming = entry("a2", MessageRole::Assistant, "live");
    streaming.status = Some(MessageStatus::Streaming);
    app.transcript_state.rows_for_doc("doc-1", &streaming);
    assert_eq!(app.transcript_state.sub_cache.borrow().len(), 1);
}

#[test]
fn own_send_reserves_a_runway_until_the_reply_outgrows_it() {
    let mut state = TranscriptState::default();
    *state.chat_id.borrow_mut() = Some("chat-1".into());
    state.viewport_size.set([900.0, 700.0]);
    state.rows.borrow_mut().push(super::Row {
        id: "m1".into(),
        version: 0,
        turn_start: true,
        kind: RowKind::User {
            text: "hello".into(),
            mentions: std::sync::Arc::new(Vec::new()),
            attachments: std::sync::Arc::new(Vec::new()),
            badges: std::sync::Arc::new(Vec::new()),
            pending: true,
        },
        entry_id: "m1".into(),
        timestamp: None,
        copy_text: None,
    });
    state.on_own_send("chat-1".into(), "m1".into());
    state.update_own_turn(&state.rows.borrow());
    let runway = state.own_turn.borrow().as_ref().expect("held turn").runway;
    assert!(runway > 0.0, "a fresh send reserves the runway");
    assert!(state.layout_request_pending(), "the hold owes a scroll");

    // Without a viewport measurement the anchor is kept but not sized.
    state.viewport_size.set([0.0, 0.0]);
    state.update_own_turn(&state.rows.borrow());
    assert!(state.own_turn.borrow().is_some());
}

#[test]
fn renders_a_transcript_tree_without_a_runtime() {
    use hearth_doc::{SubagentStatus, ToolDiffStat};
    use hearth_proto::ToolCall;

    let mut app = App::new(crate::UiConfig::default());
    let mut user = entry("u1", MessageRole::User, "read `src/main.rs` and fix it");
    user.created_at = 1_700_000_000_000;
    let assistant = SessionMessageEntry {
        id: "a1".into(),
        role: MessageRole::Assistant,
        parts: vec![
            MessagePart::Reasoning {
                id: "r1".into(),
                text: "The file needs a guard.".into(),
            },
            MessagePart::Tool {
                id: "t1".into(),
                call: ToolCall::ReadFile {
                    path: "src/main.rs".into(),
                },
                is_error: false,
                resolved: true,
                output: Some("1  fn main() {}".into()),
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: Some(vec![ToolDiffStat {
                    path: "src/main.rs".into(),
                    additions: 1,
                    deletions: 0,
                }]),
                subagent_ref: None,
                subagent_status: Some(SubagentStatus::Done),
                subagent_tail: None,
            },
            MessagePart::Text {
                id: "p1".into(),
                text: "# Done\n\nAdded the guard.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n".into(),
            },
        ],
        created_at: 1_700_000_001_000,
        device_id: "d".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    };
    app.transcript = vec![user, assistant];
    let theme = app.theme.clone();
    let _view = super::render(&app, &theme);
    let _subagent = super::render_subagent(&app, &theme, "doc");
    assert!(!app.transcript_state.rows.borrow().is_empty());
}

fn chat_entries(prefix: &str, turns: usize) -> Vec<SessionMessageEntry> {
    let mut entries = Vec::new();
    for i in 0..turns {
        let mut user = entry(
            &format!("{prefix}-u{i}"),
            MessageRole::User,
            &format!("question {i} with some words to give the row height"),
        );
        user.created_at = 1_700_000_000_000 + i as i64 * 2_000;
        let mut assistant = entry(
            &format!("{prefix}-a{i}"),
            MessageRole::Assistant,
            &format!("answer {i}\n\nsecond paragraph for {i}\n\nthird paragraph for {i}\n"),
        );
        assistant.created_at = user.created_at + 1_000;
        entries.push(user);
        entries.push(assistant);
    }
    entries
}

fn top_visible_row(rt: &rune::Runtime<App>) -> Option<String> {
    let (viewport, _) = rt.presented("app/transcript/viewport")?;
    let mut rows: Vec<_> = rt
        .elements()
        .into_iter()
        .filter(|element| {
            element
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with("one-") || key.starts_with("two-"))
        })
        .filter(|element| element.visible[3] > 0.0)
        .filter(|element| {
            element.visible[1] + element.visible[3] > viewport[1] + 0.5
                && element.visible[1] < viewport[1] + viewport[3] - 0.5
        })
        .collect();
    rows.sort_by(|a, b| a.visible[1].total_cmp(&b.visible[1]));
    rows.first().and_then(|element| element.key.clone())
}

#[test]
fn viewport_restore_survives_a_chat_switch() {
    use crate::state::{App as TheApp, ConnectionStatus};

    let mut app = TheApp::new(crate::UiConfig::default());
    app.connection = ConnectionStatus::Ready;
    app.selected_chat = Some("chat-1".into());
    app.transcript = chat_entries("one", 20);
    app.transcript_replayed = true;
    let mut rt = rune::Runtime::new(
        app,
        TheApp::view,
        (1320.0, 880.0),
        rune::Env::new(true, 1.0),
    );
    for _ in 0..8 {
        rt.frame(1.0 / 60.0);
    }

    // Scroll away from the tail and settle.
    for _ in 0..6 {
        rt.wheel(700.0, 400.0, -240.0);
        rt.frame(1.0 / 60.0);
    }
    for _ in 0..4 {
        rt.frame(1.0 / 60.0);
    }
    let before = top_visible_row(&rt).expect("a visible row before the switch");
    assert!(before.starts_with("one-"));

    // Away and back.
    let two = chat_entries("two", 20);
    rt.update(|app, _tx| {
        app.select_chat(Some("chat-2".into()));
        app.transcript = two;
        app.transcript_replayed = true;
    });
    for _ in 0..8 {
        rt.frame(1.0 / 60.0);
    }
    let other = top_visible_row(&rt).expect("the other chat rendered");
    assert!(other.starts_with("two-"), "landed in {other}");

    let one = chat_entries("one", 20);
    rt.update(|app, _tx| {
        app.select_chat(Some("chat-1".into()));
        app.transcript = one;
        app.transcript_replayed = true;
    });
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    let after = top_visible_row(&rt).expect("a visible row after the switch back");
    assert!(
        after.starts_with("one-"),
        "restoring chat-1 landed in {after}"
    );
    assert_eq!(
        before, after,
        "the viewport should restore the same top row (was {before}, now {after})"
    );
}

#[test]
fn own_send_holds_the_prompt_near_the_top() {
    use crate::state::{App as TheApp, ConnectionStatus};

    let mut app = TheApp::new(crate::UiConfig::default());
    app.connection = ConnectionStatus::Ready;
    app.selected_chat = Some("chat-1".into());
    app.transcript = chat_entries("one", 20);
    app.transcript_replayed = true;
    let mut rt = rune::Runtime::new(
        app,
        TheApp::view,
        (1320.0, 880.0),
        rune::Env::new(true, 1.0),
    );
    for _ in 0..8 {
        rt.frame(1.0 / 60.0);
    }

    let mut prompt = entry("prompt-1", MessageRole::User, "brand new prompt");
    prompt.created_at = 1_800_000_000_000;
    rt.update(|app, _tx| {
        app.transcript.push(prompt);
        app.transcript_state
            .on_own_send("chat-1".into(), "prompt-1".into());
    });
    for _ in 0..90 {
        rt.frame(1.0 / 60.0);
    }

    let (viewport, _) = rt.presented("app/transcript/viewport").expect("viewport");
    let held = rt
        .elements()
        .into_iter()
        .find(|element| element.key.as_deref() == Some("prompt-1"))
        .expect("the prompt row mounted");
    let offset = held.visible[1] - viewport[1];
    assert!(
        (0.0..140.0).contains(&offset),
        "the sent prompt should be held near the viewport top, is {offset}pt below it"
    );
}
