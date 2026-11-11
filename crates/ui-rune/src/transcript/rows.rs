//! Block-granularity row construction.
//!
//! Ported from the gpui UI's `transcript/rows.rs`. `rows_for_entry` is pure
//! over one entry plus a parse callback; `parse_for_row` wires the incremental
//! parser and the settled tree cache; `diff_rows` is the minimal splice keyed
//! by `(id, version)`.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use hearth_proto::view::single_line;

use crate::markdown::parser::{BlockTree, IncrementalParser, parse_full};

use super::TranscriptState;
use super::model::{
    Row, RowKind, ToolItem, assistant_copy_text, call_block, entry_fingerprint, fnv1a,
    is_agent_tool, thought_item, tool_detail, tool_fingerprint,
};

/// Build the block rows of one (already continuation-joined) entry.
pub fn rows_for_entry(
    entry: &SessionMessageEntry,
    pending: bool,
    parse: &mut dyn FnMut(&str, &str) -> Arc<BlockTree>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let streaming = entry.status == Some(MessageStatus::Streaming);
    let entry_id = entry.id.clone();

    if entry.role == MessageRole::User {
        let raw: String = entry
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let parsed = crate::attachments::parse_user_message_images(&raw);
        let (body, badges) = crate::badges::split(&parsed.text);
        let (text, mentions) = super::model::project_mentions(&body);
        let copy_text = (!text.trim().is_empty()).then(|| text.clone());
        return vec![Row {
            id: entry.id.clone(),
            version: (raw.len() as u64) << 1 | pending as u64,
            turn_start: true,
            kind: RowKind::User {
                text,
                mentions: Arc::new(mentions),
                attachments: Arc::new(parsed.attachments),
                badges: Arc::new(badges),
                pending,
            },
            entry_id,
            timestamp: Some(entry.created_at),
            copy_text,
        }];
    }

    // Assistant/system: split parts into block rows, folding consecutive
    // ordinary tools. Agent/spawn chips flush into their own group.
    let last_part_ix = entry.parts.len().saturating_sub(1);
    let mut group_ix = 0usize;
    let mut pending_group: Vec<ToolItem> = Vec::new();
    let mut group_last_part_ix = 0usize;

    let flush_group =
        |rows: &mut Vec<Row>, group: &mut Vec<ToolItem>, group_ix: &mut usize, last_ix: usize| {
            if group.is_empty() {
                return;
            }
            let tools = std::mem::take(group);
            let auto_open = streaming && last_ix == last_part_ix;
            rows.push(Row {
                id: format!("{}#g{}", entry.id, group_ix),
                version: tool_fingerprint(&tools, auto_open),
                turn_start: false,
                kind: RowKind::ToolGroup {
                    tools: Arc::new(tools),
                    auto_open,
                },
                entry_id: entry.id.clone(),
                timestamp: None,
                copy_text: None,
            });
            *group_ix += 1;
        };

    for (part_ix, part) in entry.parts.iter().enumerate() {
        match part {
            MessagePart::Tool {
                call,
                is_error,
                resolved,
                output,
                diff,
                output_ref,
                output_bytes,
                diff_ref,
                diff_stats,
                subagent_ref,
                subagent_status,
                subagent_tail,
                ..
            } => {
                let item = ToolItem {
                    call: call.clone(),
                    is_error: *is_error,
                    resolved: *resolved,
                    detail: tool_detail(output.as_deref(), diff.as_ref(), diff_stats.as_deref())
                        .map(Arc::new),
                    invocation: call_block(call).map(Arc::new),
                    output_ref: output_ref.clone(),
                    output_bytes: *output_bytes,
                    diff_ref: diff_ref.clone(),
                    subagent_ref: subagent_ref.clone(),
                    subagent_status: *subagent_status,
                    subagent_tail: subagent_tail.clone(),
                    is_thought: false,
                };
                if pending_group
                    .first()
                    .is_some_and(|head| is_agent_tool(head) != is_agent_tool(&item))
                {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
                group_last_part_ix = part_ix;
            }
            // Thinking rides the same accordion as the tools around it.
            MessagePart::Reasoning { id: _, text } => {
                if text.trim().is_empty() {
                    continue;
                }
                let live = streaming && part_ix == last_part_ix;
                let item = thought_item(text, live);
                if pending_group.first().is_some_and(is_agent_tool) {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
                group_last_part_ix = part_ix;
            }
            other => {
                flush_group(
                    &mut rows,
                    &mut pending_group,
                    &mut group_ix,
                    group_last_part_ix,
                );
                match other {
                    MessagePart::Text { id: part_id, text } => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        let key = format!("{}#{}", entry.id, part_id);
                        let tree = parse(&key, text);
                        for block_ix in 0..tree.blocks.len() {
                            let range = &tree.blocks[block_ix].range;
                            let end = range.end.min(text.len());
                            let bytes = text
                                .as_bytes()
                                .get(range.start.min(end)..end)
                                .unwrap_or_default();
                            let version = (fnv1a(bytes) << 1) | streaming as u64;
                            rows.push(Row {
                                id: format!("{key}.{block_ix}"),
                                version,
                                turn_start: false,
                                entry_id: entry_id.clone(),
                                timestamp: None,
                                copy_text: None,
                                kind: if streaming {
                                    RowKind::LiveMarkdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                } else {
                                    RowKind::Markdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                },
                            });
                        }
                    }
                    MessagePart::Input {
                        id: part_id,
                        questions,
                        resolved,
                        ..
                    } => {
                        let header = single_line(
                            &questions
                                .first()
                                .map(|question| question.header.clone())
                                .unwrap_or_else(|| "Question".to_string()),
                        );
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id),
                            version: fnv1a(header.as_bytes()) << 1 | *resolved as u64,
                            turn_start: false,
                            kind: RowKind::InputChip {
                                header,
                                resolved: *resolved,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Error {
                        id: part_id,
                        message,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id),
                            version: message.len() as u64,
                            turn_start: false,
                            kind: RowKind::ErrorChip {
                                message: single_line(message),
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Tool { .. } | MessagePart::Reasoning { .. } => {}
                }
            }
        }
    }
    flush_group(
        &mut rows,
        &mut pending_group,
        &mut group_ix,
        group_last_part_ix,
    );

    if let Some(first) = rows.first_mut() {
        first.turn_start = true;
    }
    // Timestamp strip under the entry's LAST row once the turn has settled.
    if !streaming && let Some(last) = rows.last_mut() {
        last.timestamp = Some(entry.created_at);
        last.copy_text = assistant_copy_text(entry);
        last.version ^= 1 << 62;
    }
    rows
}

/// How [`parse_for_row`] produced its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Streaming row: the live parser advanced by one commit.
    Incremental {
        /// Bytes fed through `parse_full` for this commit (the reparse tail).
        parsed_bytes: usize,
        /// Leading top-level blocks left untouched.
        stable_prefix_blocks: usize,
    },
    /// Completed row served from the settled tree cache.
    Cached,
    /// Live→complete handoff: the live parser's exact tree was adopted.
    Handoff,
    /// Completed row parsed from scratch.
    Full,
}

/// The transcript's markdown parse wiring: streaming parts keep one
/// [`IncrementalParser`] per part key; completed parts hit the settled cache,
/// adopt the live parser's tree on the flip, or do one full parse.
pub fn parse_for_row(
    streaming: bool,
    key: &str,
    text: &str,
    live_parsers: &mut HashMap<String, IncrementalParser>,
    tree_cache: &mut HashMap<String, (usize, Arc<BlockTree>)>,
) -> (Arc<BlockTree>, ParseOutcome) {
    if streaming {
        let parser = live_parsers.entry(key.to_string()).or_default();
        parser.set_text(text);
        (
            Arc::new(parser.display_tree()),
            ParseOutcome::Incremental {
                parsed_bytes: parser.last_parse_bytes(),
                stable_prefix_blocks: parser.stable_prefix_blocks(),
            },
        )
    } else {
        if let Some((len, tree)) = tree_cache.get(key)
            && *len == text.len()
        {
            return (tree.clone(), ParseOutcome::Cached);
        }
        let (tree, outcome) = match live_parsers.remove(key) {
            Some(parser) if parser.source() == text => {
                (Arc::new(parser.tree().clone()), ParseOutcome::Handoff)
            }
            _ => (Arc::new(parse_full(text)), ParseOutcome::Full),
        };
        tree_cache.insert(key.to_string(), (text.len(), tree.clone()));
        (tree, outcome)
    }
}

/// Markdown row ids are `{entry}#{part}.{blockIx}`.
fn part_prefix(id: &str) -> &str {
    id.rsplit_once('.').map(|(prefix, _)| prefix).unwrap_or(id)
}

/// Vertical gap opening `row` given its predecessor.
pub fn top_gap_for(prev: Option<&Row>, row: &Row) -> f32 {
    if row.turn_start {
        return crate::theme::Theme::SPACE_LG;
    }
    let is_md = |kind: &RowKind| {
        matches!(
            kind,
            RowKind::Markdown { .. } | RowKind::LiveMarkdown { .. }
        )
    };
    let same_part_markdown = prev.is_some_and(|prev| {
        is_md(&prev.kind) && is_md(&row.kind) && part_prefix(&prev.id) == part_prefix(&row.id)
    });
    if same_part_markdown {
        crate::markdown::render::MD_BLOCK_GAP
    } else if matches!(row.kind, RowKind::ToolGroup { .. })
        || prev.is_some_and(|row| matches!(row.kind, RowKind::ToolGroup { .. }))
    {
        crate::theme::Theme::SPACE_MD
    } else {
        crate::theme::Theme::SPACE_SM
    }
}

/// Minimal splice for a row-set change: `Some((old_range, new_count))`, or
/// `None` when the sets are identical by (id, version).
pub fn diff_rows(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let eq = |a: &Row, b: &Row| a.id == b.id && a.version == b.version;
    let mut prefix = 0usize;
    let max_prefix = old.len().min(new.len());
    while prefix < max_prefix && eq(&old[prefix], &new[prefix]) {
        prefix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut suffix = 0usize;
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    while suffix < max_suffix && eq(&old[old.len() - 1 - suffix], &new[new.len() - 1 - suffix]) {
        suffix += 1;
    }
    Some((prefix..old.len() - suffix, new.len() - suffix - prefix))
}

pub(super) struct CachedRows {
    pub(super) fingerprint: u64,
    pub(super) rows: Vec<Row>,
}

impl TranscriptState {
    /// Cached row build for one entry (streaming entries bypass the cache).
    pub(super) fn rows_for(&self, entry: &SessionMessageEntry, pending: bool) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        let fingerprint = entry_fingerprint(entry, pending);
        if !streaming
            && let Some(cached) = self.row_cache.borrow().get(&entry.id)
            && cached.fingerprint == fingerprint
        {
            return cached.rows.clone();
        }

        let mut live_parsers = self.live_parsers.borrow_mut();
        let mut tree_cache = self.tree_cache.borrow_mut();
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            parse_for_row(streaming, key, text, &mut live_parsers, &mut tree_cache).0
        };
        let rows = rows_for_entry(entry, pending, &mut parse);
        drop(parse);
        drop(live_parsers);
        drop(tree_cache);

        if !streaming {
            self.row_cache.borrow_mut().insert(
                entry.id.clone(),
                CachedRows {
                    fingerprint,
                    rows: rows.clone(),
                },
            );
        }
        rows
    }

    /// Cached row build for one SUBAGENT doc entry. The subagent transcript
    /// re-renders every frame from a watch that mostly delivers unchanged
    /// entries, so the same fingerprint cache as the primary transcript is
    /// required here too (keys are doc-prefixed; the LRU is bounded by
    /// [`super::MAX_SUBAGENT_ROWS`]).
    pub(super) fn rows_for_doc(&self, doc_id: &str, entry: &SessionMessageEntry) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        let fingerprint = entry_fingerprint(entry, false);
        let cache_key = format!("{doc_id}\u{1}{}", entry.id);
        if !streaming {
            let cached = self
                .sub_cache
                .borrow()
                .get(&cache_key)
                .filter(|cached| cached.fingerprint == fingerprint)
                .map(|cached| cached.rows.clone());
            if let Some(rows) = cached {
                self.touch_sub_cache(&cache_key);
                return rows;
            }
        }

        let mut live_parsers = self.live_parsers.borrow_mut();
        let mut tree_cache = self.tree_cache.borrow_mut();
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            let doc_key = format!("{doc_id}\u{1}{key}");
            parse_for_row(
                streaming,
                &doc_key,
                text,
                &mut live_parsers,
                &mut tree_cache,
            )
            .0
        };
        let rows = rows_for_entry(entry, false, &mut parse);
        drop(parse);
        drop(live_parsers);
        drop(tree_cache);

        if !streaming {
            self.sub_cache.borrow_mut().insert(
                cache_key.clone(),
                CachedRows {
                    fingerprint,
                    rows: rows.clone(),
                },
            );
            self.touch_sub_cache(&cache_key);
        }
        rows
    }

    fn touch_sub_cache(&self, cache_key: &str) {
        let mut recency = self.sub_recency.borrow_mut();
        recency.retain(|candidate| candidate != cache_key);
        recency.push_back(cache_key.to_string());
        let mut cache = self.sub_cache.borrow_mut();
        while cache.len() > super::MAX_SUBAGENT_ROWS {
            let Some(victim) = recency.pop_front() else {
                break;
            };
            if victim != cache_key {
                cache.remove(&victim);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_doc::MessagePart;
    use hearth_proto::ToolCall;

    fn entry(id: &str, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 1_700_000_000_000,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn user(id: &str, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            role: MessageRole::User,
            ..entry(
                id,
                vec![MessagePart::Text {
                    id: "p0".into(),
                    text: text.into(),
                }],
            )
        }
    }

    fn text_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    fn tool_part(id: &str, call: ToolCall) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call,
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    fn parse_all(_key: &str, text: &str) -> Arc<BlockTree> {
        Arc::new(parse_full(text))
    }

    #[test]
    fn user_rows_are_one_turn_start_bubble() {
        let rows = rows_for_entry(&user("u1", "hello"), false, &mut parse_all);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].turn_start);
        assert_eq!(rows[0].id, "u1");
        assert_eq!(rows[0].timestamp, Some(1_700_000_000_000));
        assert!(matches!(&rows[0].kind, RowKind::User { text, .. } if text == "hello"));
    }

    #[test]
    fn assistant_rows_split_per_block_with_stable_ids() {
        let e = entry(
            "a1",
            vec![text_part("p0", "one\n\ntwo\n"), text_part("p1", "three\n")],
        );
        let rows = rows_for_entry(&e, false, &mut parse_all);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "a1#p0.0");
        assert_eq!(rows[1].id, "a1#p0.1");
        assert_eq!(rows[2].id, "a1#p1.0");
        assert!(rows[0].turn_start);
        // Only the last settled row carries the strip and copy text.
        assert_eq!(rows[0].timestamp, None);
        assert_eq!(rows[2].timestamp, Some(e.created_at));
        assert_eq!(
            rows[2].copy_text.as_deref(),
            Some("one\n\ntwo\n\n\nthree\n")
        );
    }

    #[test]
    fn consecutive_tools_fold_and_agent_chips_flush() {
        let e = entry(
            "a1",
            vec![
                tool_part(
                    "t0",
                    ToolCall::ReadFile {
                        path: "a.rs".into(),
                    },
                ),
                tool_part(
                    "t1",
                    ToolCall::Exec {
                        command: "ls".into(),
                    },
                ),
                tool_part(
                    "t2",
                    ToolCall::Unknown {
                        name: "Agent: test".into(),
                        input: None,
                    },
                ),
            ],
        );
        let rows = rows_for_entry(&e, false, &mut parse_all);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "a1#g0");
        assert_eq!(rows[1].id, "a1#g1");
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!()
        };
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn reasoning_rides_the_tool_group() {
        let e = entry(
            "a1",
            vec![
                MessagePart::Reasoning {
                    id: "r0".into(),
                    text: "thinking".into(),
                },
                tool_part(
                    "t0",
                    ToolCall::Exec {
                        command: "ls".into(),
                    },
                ),
            ],
        );
        let rows = rows_for_entry(&e, false, &mut parse_all);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!()
        };
        assert_eq!(tools.len(), 2);
        assert!(tools[0].is_thought);
    }

    #[test]
    fn input_and_error_chips_are_their_own_rows() {
        let e = entry(
            "a1",
            vec![
                MessagePart::Input {
                    id: "i0".into(),
                    request_id: "q0".into(),
                    questions: vec![hearth_proto::UserInputQuestion {
                        id: "q0".into(),
                        header: "Which one?".into(),
                        question: String::new(),
                        options: Vec::new(),
                        multi_select: false,
                    }],
                    resolved: true,
                },
                MessagePart::Error {
                    id: "e0".into(),
                    message: "boom\nmore".into(),
                },
            ],
        );
        let rows = rows_for_entry(&e, false, &mut parse_all);
        assert_eq!(rows.len(), 2);
        assert!(
            matches!(&rows[0].kind, RowKind::InputChip { header, .. } if header == "Which one?")
        );
        assert!(matches!(&rows[1].kind, RowKind::ErrorChip { message } if message == "boom more"));
    }

    #[test]
    fn streaming_rows_use_live_markdown() {
        let mut e = entry("a1", vec![text_part("p0", "one\n")]);
        e.status = Some(MessageStatus::Streaming);
        let rows = rows_for_entry(&e, false, &mut parse_all);
        assert!(matches!(rows[0].kind, RowKind::LiveMarkdown { .. }));
        assert_eq!(rows[0].timestamp, None);
        assert_eq!(rows[0].version & 1, 1);
    }

    #[test]
    fn parse_for_row_handoff_and_cache() {
        let mut live = HashMap::new();
        let mut cache = HashMap::new();
        let (streamed, outcome) = parse_for_row(true, "k", "hello", &mut live, &mut cache);
        assert!(matches!(outcome, ParseOutcome::Incremental { .. }));
        assert_eq!(&*streamed, &parse_full("hello"));
        let (settled, outcome) = parse_for_row(false, "k", "hello", &mut live, &mut cache);
        assert_eq!(outcome, ParseOutcome::Handoff);
        assert_eq!(&*settled, &parse_full("hello"));
        let (_, outcome) = parse_for_row(false, "k", "hello", &mut live, &mut cache);
        assert_eq!(outcome, ParseOutcome::Cached);
    }

    #[test]
    fn incremental_row_parse_is_bounded_by_the_tail() {
        let mut live = HashMap::new();
        let mut cache = HashMap::new();
        let head = "paragraph\n\n".repeat(1_500);
        let mut source = head.clone();
        parse_for_row(true, "k", &source, &mut live, &mut cache);
        source.push_str("streamed tail");
        let (_, outcome) = parse_for_row(true, "k", &source, &mut live, &mut cache);
        let ParseOutcome::Incremental { parsed_bytes, .. } = outcome else {
            panic!()
        };
        assert!(
            parsed_bytes < 4_096,
            "append reparsed {parsed_bytes} bytes, not just the tail"
        );
    }

    #[test]
    fn diff_rows_finds_the_minimal_splice() {
        let a = rows_for_entry(&user("u1", "one"), false, &mut parse_all);
        let b = rows_for_entry(&user("u1", "a longer prompt"), false, &mut parse_all);
        let (range, count) = diff_rows(&a, &b).unwrap();
        assert_eq!(range, 0..1);
        assert_eq!(count, 1);
        assert!(diff_rows(&a, &a).is_none());
    }

    #[test]
    fn gaps_match_the_block_and_tool_rules() {
        let rows = rows_for_entry(
            &entry(
                "a1",
                vec![
                    text_part("p0", "one\n\ntwo\n"),
                    tool_part(
                        "t0",
                        ToolCall::Exec {
                            command: "ls".into(),
                        },
                    ),
                    text_part("p1", "three\n"),
                ],
            ),
            false,
            &mut parse_all,
        );
        // same part markdown → block gap
        assert_eq!(
            top_gap_for(Some(&rows[0]), &rows[1]),
            crate::markdown::render::MD_BLOCK_GAP
        );
        // tool group boundary → mid space
        assert_eq!(
            top_gap_for(Some(&rows[1]), &rows[2]),
            crate::theme::Theme::SPACE_MD
        );
    }
}
