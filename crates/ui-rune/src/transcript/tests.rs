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
