//! Transcript row model (pure).
//!
//! Ported from the gpui UI's `transcript/model.rs`. One row per block:
//! a user message is one bubble row; an assistant entry splits into one row per
//! top-level markdown block, plus consecutive-tool groups (agent/spawn chips
//! split out so they never collapse) and input/error chips. Ids are stable
//! (`{entry}#{partId}.{blockIx}` / `{entry}#g{n}`); versions are content hashes
//! so a commit only splices the rows whose bytes changed.

use std::sync::Arc;

use hearth_doc::{MessagePart, MessageStatus, SessionMessageEntry, SubagentStatus};
use hearth_proto::ToolCall;
use hearth_proto::view::tool_chip_content;

use crate::markdown::parser::BlockTree;

use super::chips::{CHIP_GAP, CHIP_HEIGHT, CHIPS_TOP_PAD};

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

/// One tool invocation inside a group row.
#[derive(Debug, Clone)]
pub struct ToolItem {
    pub call: ToolCall,
    pub is_error: bool,
    pub resolved: bool,
    /// Expandable detail: a code-block of output lines, or a real diff section
    /// rendered by the changes pane's component.
    pub detail: Option<Arc<ToolDetail>>,
    /// Expandable full-invocation block (the complete command / pattern / URL /
    /// input JSON the header collapses to one truncated line).
    pub invocation: Option<Arc<ToolDetail>>,
    /// Sidecar key of the full output (chat2-sync A3).
    pub output_ref: Option<String>,
    /// Full-output size, for the affordance label.
    pub output_bytes: Option<u64>,
    /// Sidecar key of the full diff.
    pub diff_ref: Option<String>,
    /// The spawned subagent's doc id.
    pub subagent_ref: Option<String>,
    /// Subagent lifecycle, distinct from `resolved`.
    pub subagent_status: Option<SubagentStatus>,
    /// Legacy one-line live tail (fingerprinted, never rendered).
    pub subagent_tail: Option<String>,
    /// A reasoning part riding the tool group as a chip.
    pub is_thought: bool,
}

/// Spawn chips stay out of the collapsible wrap.
pub fn is_agent_call(call: &ToolCall) -> bool {
    call.is_subagent_spawn()
}

/// The chip's genus is the call itself, never the ref.
pub fn is_agent_tool(item: &ToolItem) -> bool {
    is_agent_call(&item.call)
}

/// A spawn chip is a link only when an agent call is bound to its doc.
pub fn is_spawn_link(item: &ToolItem) -> bool {
    is_agent_call(&item.call) && item.subagent_ref.is_some()
}

/// Ordinary tool groups fold behind a summary header.
pub fn tool_group_collapses(tools: &[ToolItem]) -> bool {
    tools.iter().any(|t| !is_agent_tool(t))
}

/// Column budget for soft-wrapping thought text.
pub const THOUGHT_WRAP_COLS: usize = 96;

/// Soft-wrap a thought's prose at word boundaries.
pub fn wrap_thought_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.lines() {
        let paragraph = paragraph.trim_end();
        if paragraph.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if !current.is_empty()
                && current.chars().count() + 1 + word.chars().count() > THOUGHT_WRAP_COLS
            {
                lines.push(std::mem::take(&mut current));
            }
            if word.chars().count() > THOUGHT_WRAP_COLS {
                let mut chunk = String::new();
                for c in word.chars() {
                    chunk.push(c);
                    if chunk.chars().count() == THOUGHT_WRAP_COLS {
                        lines.push(std::mem::take(&mut chunk));
                    }
                }
                current = chunk;
                continue;
            }
            if current.is_empty() {
                current = word.to_owned();
            } else {
                current.push(' ');
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// Max verbatim output lines per chip before the counted tail row.
pub const OUTPUT_DETAIL_MAX_LINES: usize = 24;

/// Max diff lines an inline tool-diff detail renders.
pub const DIFF_DETAIL_MAX_LINES: usize = 600;

/// Per-line height of an output detail block.
pub const OUTPUT_LINE_HEIGHT: f32 = 18.0;

/// Vertical padding of an output detail body (py(6) × 2).
pub const OUTPUT_BODY_PAD: f32 = 12.0;

/// The hairline between an expanded chip's header row and its detail body.
pub const DETAIL_SEPARATOR: f32 = 1.0;

/// A chip's expandable detail payload.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDetail {
    /// Command/tool output as verbatim lines, capped with a counted tail.
    Output {
        lines: Vec<String>,
        truncated_by: usize,
    },
    /// A file diff, in the changes pane's model.
    Diff {
        file: Arc<crate::changes::FileDiff>,
        old_text: Option<Arc<str>>,
        new_text: Option<Arc<str>>,
    },
    /// Per-file `+N −N` stat rows.
    Stats {
        stats: Arc<Vec<hearth_doc::ToolDiffStat>>,
    },
}

/// A reasoning part as a tool-group chip.
pub fn thought_item(text: &str, live: bool) -> ToolItem {
    let mut lines = wrap_thought_lines(text);
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    if truncated_by > 0 {
        if live {
            lines.drain(..truncated_by);
        } else {
            lines.truncate(OUTPUT_DETAIL_MAX_LINES);
        }
    }
    ToolItem {
        call: ToolCall::Unknown {
            name: "Thought process".into(),
            input: None,
        },
        is_error: false,
        resolved: !live,
        detail: Some(Arc::new(ToolDetail::Output {
            lines,
            truncated_by,
        })),
        invocation: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        subagent_ref: None,
        subagent_status: None,
        subagent_tail: None,
        is_thought: true,
    }
}

/// Build a tool part's expandable detail.
pub fn tool_detail(
    output: Option<&str>,
    diff: Option<&hearth_proto::ToolDiff>,
    diff_stats: Option<&[hearth_doc::ToolDiffStat]>,
) -> Option<ToolDetail> {
    if let Some(diff) = diff {
        let mut file = diff_to_file(diff);
        if file.hunks.is_empty() {
            return None;
        }
        crate::changes::truncate_file_lines(&mut file, DIFF_DETAIL_MAX_LINES);
        return Some(ToolDetail::Diff {
            file: Arc::new(file),
            old_text: diff.old_text.as_deref().map(Arc::from),
            new_text: Some(Arc::from(diff.new_text.as_str())),
        });
    }
    if let Some(stats) = diff_stats.filter(|stats| !stats.is_empty()) {
        return Some(ToolDetail::Stats {
            stats: Arc::new(stats.to_vec()),
        });
    }
    let output = output?;
    let mut lines: Vec<String> = output.lines().map(str::to_owned).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    lines.truncate(OUTPUT_DETAIL_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Columns at which an invocation line soft-wraps.
pub const CALL_WRAP_COLS: usize = 80;

fn wrap_cols(line: &str, cols: usize) -> Vec<String> {
    if line.chars().count() <= cols {
        return vec![line.to_owned()];
    }
    line.chars()
        .collect::<Vec<_>>()
        .chunks(cols)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

/// Build a chip's full-invocation block.
pub fn call_block(call: &ToolCall) -> Option<ToolDetail> {
    let text: String = match call {
        ToolCall::Exec { command } => command.clone(),
        ToolCall::ReadFile { path } => path.clone(),
        ToolCall::WriteFile { path, content } => match content {
            Some(content) => format!("{path}\n{content}"),
            None => path.clone(),
        },
        ToolCall::EditFile { path, .. } => path.clone(),
        ToolCall::ApplyPatch { path } => path.clone().unwrap_or_else(|| "workspace".into()),
        ToolCall::Search { pattern, path } => match path {
            Some(path) => format!("{pattern} in {path}"),
            None => pattern.clone(),
        },
        ToolCall::Glob { pattern } => pattern.clone(),
        ToolCall::WebFetch { url, prompt } => match prompt {
            Some(prompt) => format!("{url}\n{prompt}"),
            None => url.clone(),
        },
        ToolCall::WebSearch { query } => query.clone(),
        ToolCall::Todo { items } => items
            .iter()
            .map(|item| format!("{} {}", if item.done { "[x]" } else { "[ ]" }, item.text))
            .collect::<Vec<_>>()
            .join("\n"),
        ToolCall::Mcp {
            server,
            tool,
            input,
        } => match input {
            Some(input) => format!(
                "{server} · {tool}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => format!("{server} · {tool}"),
        },
        ToolCall::Unknown { name, input } => match input {
            Some(input) => format!(
                "{name}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => name.clone(),
        },
    };
    let mut lines: Vec<String> = text
        .lines()
        .flat_map(|line| wrap_cols(line, CALL_WRAP_COLS))
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    lines.truncate(OUTPUT_DETAIL_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Reduce an inline [`hearth_proto::ToolDiff`] to the changes pane's model.
pub fn diff_to_file(diff: &hearth_proto::ToolDiff) -> crate::changes::FileDiff {
    use crate::changes::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};
    let old = diff.old_text.as_deref().unwrap_or("");
    let text_diff = similar::TextDiff::from_lines(old, &diff.new_text);
    let mut hunks = Vec::new();
    let (mut additions, mut deletions) = (0u32, 0u32);
    let mut max_line = 0u32;
    for group in text_diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let header = format!(
            "@@ -{},{} +{},{} @@",
            old_range.start + 1,
            old_range.len(),
            new_range.start + 1,
            new_range.len(),
        );
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Delete => {
                        deletions += 1;
                        LineKind::Del
                    }
                    similar::ChangeTag::Insert => {
                        additions += 1;
                        LineKind::Add
                    }
                    similar::ChangeTag::Equal => LineKind::Context,
                };
                let old_no = change.old_index().map(|n| n as u32 + 1);
                let new_no = change.new_index().map(|n| n as u32 + 1);
                max_line = max_line.max(old_no.unwrap_or(0)).max(new_no.unwrap_or(0));
                lines.push(DiffLine {
                    kind,
                    old_no,
                    new_no,
                    text: change.value().trim_end_matches('\n').to_owned(),
                });
            }
        }
        hunks.push(Hunk { header, lines });
    }
    FileDiff {
        path: diff.path.clone(),
        old_path: None,
        status: if diff.old_text.is_none() {
            FileStatus::Added
        } else {
            FileStatus::Modified
        },
        binary: false,
        notices: Vec::new(),
        hunks,
        additions,
        deletions,
        max_line,
    }
}

/// A file mention projected into the bubble's display text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionSpan {
    /// Byte range of the label in [`RowKind::User::text`].
    pub range: std::ops::Range<usize>,
    pub label: String,
}

/// Replace `[basename](hearth-file:…)` links with their display labels and
/// return the chip ranges over the resulting text.
pub fn project_mentions(text: &str) -> (String, Vec<MentionSpan>) {
    let links = crate::composer::mentions::file_mention_links(text);
    if links.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let labels = crate::composer::mentions::mention_display_labels(&links);
    let mut out = String::with_capacity(text.len());
    let mut spans = Vec::with_capacity(links.len());
    let mut at = 0usize;
    for (link, label) in links.into_iter().zip(labels) {
        if link.range.start < at {
            continue;
        }
        out.push_str(&text[at..link.range.start]);
        let start = out.len();
        out.push_str(&label);
        spans.push(MentionSpan {
            range: start..out.len(),
            label,
        });
        at = link.range.end;
    }
    out.push_str(&text[at..]);
    (out, spans)
}

#[derive(Clone)]
pub enum RowKind {
    User {
        /// Visible prompt (attachment-ref trailer stripped, mentions projected).
        text: String,
        /// File-mention chips over `text`, in display-byte terms.
        mentions: Arc<Vec<MentionSpan>>,
        /// Image refs parsed out of the message text.
        attachments: Arc<Vec<crate::attachments::UserImageAttachment>>,
        /// Context the prompt folded in as text, lifted back out by `badges`.
        badges: Arc<Vec<crate::badges::MessageBadge>>,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    /// One top-level markdown block of a completed message.
    Markdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    /// One top-level block of a STREAMING message.
    LiveMarkdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    ToolGroup {
        tools: Arc<Vec<ToolItem>>,
        auto_open: bool,
    },
    InputChip {
        header: String,
        resolved: bool,
    },
    ErrorChip {
        message: String,
    },
}

/// A transcript row: stable id + content version (diff key) + block payload.
#[derive(Clone)]
pub struct Row {
    pub id: String,
    pub version: u64,
    /// First row of its message entry.
    pub turn_start: bool,
    pub kind: RowKind,
    /// The owning message entry.
    pub entry_id: String,
    /// Epoch-ms for the hover-timestamp strip under this row.
    pub timestamp: Option<i64>,
    /// Text copied by the entry-level hover action.
    pub copy_text: Option<String>,
}

/// Absolute hover-timestamp label, e.g. "Jul 1, 3:45 PM".
pub fn format_timestamp<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(utc) => utc
            .with_timezone(tz)
            .format("%b %-d, %-I:%M %p")
            .to_string(),
        None => String::new(),
    }
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

pub(super) fn tool_fingerprint(tools: &[ToolItem], auto_open: bool) -> u64 {
    let mut acc = Vec::with_capacity(tools.len() * 8 + 1);
    for tool in tools {
        let (label, detail) = tool_chip_content(&tool.call);
        acc.extend_from_slice(label.as_bytes());
        acc.extend_from_slice(&(detail.len() as u32).to_le_bytes());
        acc.push(tool.is_error as u8 | (tool.resolved as u8) << 1);
        match tool.detail.as_deref() {
            None => acc.push(0),
            Some(ToolDetail::Output {
                lines,
                truncated_by,
            }) => {
                acc.push(1);
                acc.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
                let bytes: usize = lines.iter().map(|line| line.len()).sum();
                acc.extend_from_slice(&(bytes as u32).to_le_bytes());
            }
            Some(ToolDetail::Diff { file, .. }) => {
                acc.push(2);
                acc.extend_from_slice(file.path.as_bytes());
                acc.extend_from_slice(&file.additions.to_le_bytes());
                acc.extend_from_slice(&file.deletions.to_le_bytes());
                acc.extend_from_slice(&(file.hunks.len() as u32).to_le_bytes());
            }
            Some(ToolDetail::Stats { stats }) => {
                acc.push(3);
                for stat in stats.iter() {
                    acc.extend_from_slice(stat.path.as_bytes());
                    acc.extend_from_slice(&stat.additions.to_le_bytes());
                    acc.extend_from_slice(&stat.deletions.to_le_bytes());
                }
            }
        }
        if let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = tool.invocation.as_deref()
        {
            for line in lines {
                acc.extend_from_slice(line.as_bytes());
            }
            acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
        }
        acc.push(tool.output_ref.is_some() as u8 | (tool.diff_ref.is_some() as u8) << 1);
        acc.push(
            tool.subagent_ref.is_some() as u8
                | match tool.subagent_status {
                    None => 0,
                    Some(SubagentStatus::Running) => 1 << 1,
                    Some(SubagentStatus::Done) => 2 << 1,
                    Some(SubagentStatus::Failed) => 3 << 1,
                },
        );
        if let Some(tail) = &tool.subagent_tail {
            acc.extend_from_slice(tail.as_bytes());
        }
    }
    acc.push(auto_open as u8);
    fnv1a(&acc)
}

/// Clipboard payload for an assistant/system entry.
pub fn assistant_copy_text(entry: &SessionMessageEntry) -> Option<String> {
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------
// Tool summaries / chips
// ---------------------------------------------------------------------------

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
pub fn tool_group_summary(tools: &[ToolItem]) -> String {
    let pairs: Vec<(ToolCall, bool)> = tools
        .iter()
        .filter(|tool| !tool.is_thought)
        .map(|tool| (tool.call.clone(), tool.is_error))
        .collect();
    let thoughts = tools.iter().filter(|tool| tool.is_thought).count();
    let base = if pairs.is_empty() {
        String::new()
    } else {
        hearth_proto::view::tool_group_summary(&pairs)
    };
    match (base.is_empty(), thoughts) {
        (_, 0) => base,
        (true, 1) => "Thought process".into(),
        (true, n) => format!("Thought {n} times"),
        (false, 1) => format!("Thought · {base}"),
        (false, n) => format!("Thought {n} times · {base}"),
    }
}

/// Analytic expanded-chips height.
pub fn chips_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    CHIPS_TOP_PAD + count as f32 * CHIP_HEIGHT + (count as f32 - 1.0) * CHIP_GAP
}

/// Analytic height an open detail adds to its chip's card.
pub fn detail_height(detail: &ToolDetail) -> f32 {
    let body = match detail {
        ToolDetail::Output {
            lines,
            truncated_by,
        } => {
            let rows = lines.len() + usize::from(*truncated_by > 0);
            rows as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD
        }
        ToolDetail::Diff { file, .. } => crate::changes::body_height(file),
        ToolDetail::Stats { stats } => stats.len() as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD,
    };
    DETAIL_SEPARATOR + body
}

// ---------------------------------------------------------------------------
// Working indicator flavour (pure)
// ---------------------------------------------------------------------------

pub const FLAVOUR_WORDS: [&str; 20] = [
    "Thinking",
    "Pondering",
    "Scheming",
    "Brewing",
    "Weaving",
    "Tinkering",
    "Musing",
    "Composing",
    "Sifting",
    "Untangling",
    "Distilling",
    "Sketching",
    "Plotting",
    "Riffing",
    "Combobulating",
    "Percolating",
    "Marinating",
    "Noodling",
    "Puzzling",
    "Conjuring",
];

pub const FLAVOUR_ROTATE_SECS: i64 = 7;

/// The flavour word for a seed at an elapsed time.
pub fn flavour_word(seed: u64, elapsed_secs: i64) -> &'static str {
    let step = (elapsed_secs.max(0) / FLAVOUR_ROTATE_SECS) as u64;
    FLAVOUR_WORDS[((seed.wrapping_add(step)) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// A stable per-chat seed.
pub fn flavour_seed(chat_id: &str) -> u64 {
    fnv1a(chat_id.as_bytes())
}

/// The working trailer's "Sending…" bridge.
pub fn sending_bridge(
    send_started: Option<chrono::DateTime<chrono::Utc>>,
    turn_started: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    match (send_started, turn_started) {
        (Some(send), Some(turn)) => turn <= send,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// "1m 32s"-style elapsed formatting.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

pub fn entry_fingerprint(entry: &SessionMessageEntry, pending: bool) -> u64 {
    let mut acc: Vec<u8> = Vec::with_capacity(entry.parts.len() * 8 + 16);
    acc.extend_from_slice(entry.id.as_bytes());
    acc.push(match entry.status {
        None => 0,
        Some(MessageStatus::Streaming) => 1,
        Some(MessageStatus::Complete) => 2,
        Some(MessageStatus::Aborted) => 3,
    });
    acc.push(pending as u8);
    for part in &entry.parts {
        acc.extend_from_slice(part.id().as_bytes());
        acc.extend_from_slice(&(part.byte_len() as u64).to_le_bytes());
        if let MessagePart::Tool {
            is_error,
            resolved,
            subagent_ref,
            subagent_status,
            subagent_tail,
            ..
        } = part
        {
            acc.push(*is_error as u8 | (*resolved as u8) << 1);
            acc.push(
                subagent_ref.is_some() as u8
                    | match subagent_status {
                        None => 0,
                        Some(SubagentStatus::Running) => 1 << 1,
                        Some(SubagentStatus::Done) => 2 << 1,
                        Some(SubagentStatus::Failed) => 3 << 1,
                    },
            );
            if let Some(tail) = subagent_tail {
                acc.extend_from_slice(tail.as_bytes());
            }
        }
        if let MessagePart::Input { resolved, .. } = part {
            acc.push(0x10 | *resolved as u8);
        }
    }
    fnv1a(&acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parser::parse_full;

    #[test]
    fn fnv_is_stable_and_content_sensitive() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
        assert_eq!(fnv1a(b"same"), fnv1a(b"same"));
    }

    #[test]
    fn thought_lines_wrap_at_the_column_budget() {
        let text = "word ".repeat(40);
        let lines = wrap_thought_lines(&text);
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() <= THOUGHT_WRAP_COLS)
        );
        // A pathological token hard-splits.
        let long = "x".repeat(THOUGHT_WRAP_COLS + 7);
        let lines = wrap_thought_lines(&long);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn thought_item_truncates_tail_when_live_and_head_when_settled() {
        let text = (0..OUTPUT_DETAIL_MAX_LINES + 4)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        let live = thought_item(&text, true);
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = live.detail.as_deref()
        else {
            panic!()
        };
        assert_eq!(*truncated_by, 4);
        assert_eq!(lines.len(), OUTPUT_DETAIL_MAX_LINES);
        assert_eq!(
            lines.last().unwrap(),
            &format!("line {}", OUTPUT_DETAIL_MAX_LINES + 3)
        );
        let settled = thought_item(&text, false);
        let Some(ToolDetail::Output { lines, .. }) = settled.detail.as_deref() else {
            panic!()
        };
        assert_eq!(lines.first().unwrap(), "line 0");
    }

    #[test]
    fn call_blocks_render_todo_and_unknown_inputs() {
        let call = ToolCall::Todo {
            items: vec![
                hearth_proto::TodoItem {
                    text: "one".into(),
                    done: true,
                },
                hearth_proto::TodoItem {
                    text: "two".into(),
                    done: false,
                },
            ],
        };
        let Some(ToolDetail::Output { lines, .. }) = call_block(&call) else {
            panic!()
        };
        assert_eq!(lines, vec!["[x] one", "[ ] two"]);
    }

    #[test]
    fn timestamps_use_the_short_month_shape() {
        let tz = chrono::FixedOffset::east_opt(0).unwrap();
        assert_eq!(format_timestamp(0, &tz), "Jan 1, 12:00 AM");
    }

    #[test]
    fn flavour_rotates_every_seven_seconds() {
        let seed = flavour_seed("chat");
        let first = flavour_word(seed, 0);
        assert_eq!(first, flavour_word(seed, 6));
        assert_ne!(first, flavour_word(seed, 7));
        assert_eq!(format_elapsed(92), "1m 32s");
        assert_eq!(format_elapsed(59), "59s");
    }

    #[test]
    fn sending_bridge_only_while_the_turn_has_not_started() {
        let now = chrono::Utc::now();
        assert!(sending_bridge(Some(now), None));
        assert!(sending_bridge(Some(now), Some(now)));
        assert!(!sending_bridge(
            Some(now),
            Some(now + chrono::TimeDelta::seconds(1))
        ));
        assert!(!sending_bridge(None, Some(now)));
    }

    #[test]
    fn tool_group_summary_names_thoughts() {
        let plain = vec![ToolItem {
            call: ToolCall::ReadFile {
                path: "a.rs".into(),
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
        }];
        assert!(!tool_group_summary(&plain).is_empty());
        let thoughts = vec![thought_item("hmm", false)];
        assert_eq!(tool_group_summary(&thoughts), "Thought process");
    }

    #[test]
    fn diff_to_file_groups_with_context() {
        let diff = hearth_proto::ToolDiff {
            path: "src/a.rs".into(),
            old_text: Some("one\ntwo\nthree\n".into()),
            new_text: "one\nTWO\nthree\n".into(),
        };
        let file = diff_to_file(&diff);
        assert_eq!(file.path, "src/a.rs");
        assert_eq!(file.additions, 1);
        assert_eq!(file.deletions, 1);
        assert_eq!(file.hunks.len(), 1);
    }

    #[test]
    fn tree_rows_use_stable_block_ids() {
        // The parse helper is exercised in rows.rs; this keeps the tree type
        // honest about ranges used by the row versions.
        let tree = parse_full("a\n\nb\n");
        assert_eq!(tree.blocks.len(), 2);
        assert!(tree.blocks[0].range.end <= tree.blocks[1].range.start);
    }
}
