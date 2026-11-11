//! Tool chips, input/error chips and expandable detail bodies.
//!
//! Ported from the gpui UI's `transcript/chips.rs`. The analytic constants are
//! kept (rows reserve the same heights); Rune lays the cards out itself, so the
//! open/close tween is delegated to the runtime's growth/motion where used.

use std::sync::Arc;

use hearth_doc::SubagentStatus;
use hearth_proto::ToolCall;
use hearth_proto::view::tool_chip_content;
use rune::{El, TextStyle, col, el, row, text};

use crate::state::App;
use crate::theme::Theme;

use super::model::{OUTPUT_LINE_HEIGHT, ToolDetail, ToolItem, is_agent_call};

/// Tool chip row height / gap — analytic, so fold heights need no measurement.
pub const CHIP_HEIGHT: f32 = 38.0;
pub const CHIP_GAP: f32 = 0.0;
pub const CHIP_CARD_HEIGHT: f32 = 30.0;
/// Inner height of the chip header (the card's border-box is 30).
pub const CHIP_HEADER_HEIGHT: f32 = CHIP_CARD_HEIGHT - 2.0;
pub const CHIPS_TOP_PAD: f32 = 2.0;
/// How long a user fold toggle keeps its height tween armed.
pub const FOLD_TWEEN_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
/// Height of the "Show full output/diff" affordance row.
pub const BLOB_AFFORDANCE_HEIGHT: f32 = 24.0;

/// What an open chip's affordance row offers.
#[derive(Clone)]
pub struct ChipAffordance {
    pub blob_ref: String,
    pub label: String,
}

/// Line cap for a FETCHED full output.
const FULL_OUTPUT_MAX_LINES: usize = 400;

/// Build the upgraded detail from a fetched sidecar blob.
pub fn blob_detail(text: &str, is_diff: bool) -> Option<ToolDetail> {
    if is_diff {
        let diff: hearth_proto::ToolDiff = serde_json::from_str(text).ok()?;
        return super::model::tool_detail(None, Some(&diff), None);
    }
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(FULL_OUTPUT_MAX_LINES);
    lines.truncate(FULL_OUTPUT_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Compact byte size for the fetch affordance label ("812 B", "12 KB").
pub fn format_kb(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[derive(Default, Clone, Copy)]
pub struct FoldState {
    /// User pin (click); `None` follows the auto-open rule.
    pub open: Option<bool>,
    /// Bumped per toggle — keys the height tween.
    pub epoch: usize,
    /// Height at the moment of the toggle.
    pub from: f32,
    /// When the toggle happened.
    pub toggled_at: Option<std::time::Instant>,
    /// Monotonic recency stamp for the detail-card cache.
    pub touched: u64,
}

impl FoldState {
    /// Whether a toggle tween is still armed (and recent enough to animate).
    pub fn animating(&self) -> bool {
        self.epoch > 0
            && self
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW)
    }
}

/// A small glyph standing in for the tool's icon.
fn tool_icon(call: &ToolCall) -> &'static rune::Icon {
    match call {
        ToolCall::Exec { .. } => &crate::icons::COMMAND,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => &crate::icons::DOCUMENT,
        ToolCall::WriteFile { .. } => &crate::icons::DOCUMENT_ADD,
        ToolCall::EditFile { .. } => &crate::icons::PEN,
        ToolCall::Search { .. } => &crate::icons::MAGNIFER,
        ToolCall::Glob { .. } => &crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => &crate::icons::GLOBAL,
        ToolCall::Todo { .. } => &crate::icons::CHECKLIST,
        call if is_agent_call(call) => &crate::icons::BOT,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => &crate::icons::WIDGET,
    }
}

/// The trailing tile on a chip header.
#[derive(Clone, Copy)]
pub enum ChipTrail {
    /// Expand/collapse chevron — flipped while the detail body is open.
    Chevron { open: bool },
    /// Top-right "opens elsewhere" arrow for a spawn chip.
    OpenArrow,
}

fn chip_header_row(tool: &ToolItem, trail: Option<ChipTrail>, theme: &Theme) -> El {
    let (label, detail) = if tool.is_thought {
        ("Thought process", String::new())
    } else {
        tool_chip_content(&tool.call)
    };
    let running = tool.subagent_ref.is_some()
        && matches!(tool.subagent_status, Some(SubagentStatus::Running));
    let failed = tool.is_error
        || (tool.subagent_ref.is_some()
            && matches!(tool.subagent_status, Some(SubagentStatus::Failed)));
    let tint = if failed {
        theme.danger
    } else {
        theme.text_muted
    };
    let mut out = row()
        .h(CHIP_HEADER_HEIGHT)
        .w_full()
        .min_w(0.0)
        .items_center()
        .gap(8.0)
        .px(8.0)
        .child(
            el().size(18.0, 18.0)
                .radius(5.0)
                .fill(theme.ink(0.08))
                .center()
                .child(
                    rune::icon(
                        if tool.is_thought {
                            &crate::icons::CHAT_ROUND_LINE
                        } else {
                            tool_icon(&tool.call)
                        },
                        12.0,
                    )
                    .color(theme.text_muted),
                ),
        )
        .child(
            text(label)
                .font(TextStyle::new(12.0, 500.0).lh(18.0))
                .color(tint)
                .single_line(),
        )
        .child(
            text(detail)
                .font(TextStyle::new(12.0, 400.0).lh(18.0))
                .color(if failed {
                    theme.danger
                } else {
                    theme.text.alpha(0.85)
                })
                .truncate()
                .min_w(0.0)
                .grow(1.0),
        );
    if let Some(model) = tool.call.subagent_model() {
        out = out.child(
            text(model.to_owned())
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_faint)
                .single_line()
                .shrink(),
        );
    }
    if running {
        out = out.child(el().size(6.0, 6.0).radius(3.0).fill(theme.busy).shrink());
    }
    if let Some(trail) = trail {
        let mut tile = el()
            .size(18.0, 18.0)
            .radius(5.0)
            .fill(theme.ink(0.06))
            .center()
            .color(theme.text_muted.alpha(0.8));
        tile = match trail {
            ChipTrail::Chevron { open } => tile.child(
                text(if open { "▾" } else { "▸" })
                    .font(TextStyle::new(10.0, 400.0))
                    .color(theme.text_muted.alpha(0.8)),
            ),
            ChipTrail::OpenArrow => tile.child(
                rune::icon(&crate::icons::ARROW_UP_RIGHT, 11.0).color(theme.text_muted.alpha(0.8)),
            ),
        };
        out = out.child(tile);
    }
    out
}

/// The header row of an expandable chip card.
pub fn chip_header(tool: &ToolItem, open: bool, theme: &Theme) -> El {
    chip_header_row(tool, Some(ChipTrail::Chevron { open }), theme)
}

/// Max chars a subagent tab title keeps.
pub const SUBAGENT_TITLE_MAX: usize = 40;

fn title_line(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    Some(out)
}

fn strip_spawn_prefix(text: &str) -> &str {
    let trimmed = text.trim();
    for prefix in ["agent", "task"] {
        if trimmed.len() >= prefix.len()
            && trimmed.is_char_boundary(prefix.len())
            && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let rest = &trimmed[prefix.len()..];
            if rest.is_empty() {
                return "";
            }
            if rest.starts_with(':') || rest.starts_with(char::is_whitespace) {
                return rest.trim_start_matches(':').trim();
            }
        }
    }
    trimmed
}

/// Tab title for a spawn chip's subagent surface: the bare task description.
pub fn subagent_tab_title(call: &ToolCall) -> String {
    let (name, input) = match call {
        ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
        ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
        _ => return "Subagent".into(),
    };
    let candidates = [
        Some(name),
        input.and_then(|input| input.get("description")?.as_str()),
        input.and_then(|input| input.get("prompt")?.as_str()),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(title) = title_line(strip_spawn_prefix(candidate), SUBAGENT_TITLE_MAX) {
            return title;
        }
    }
    "Subagent".into()
}

/// A plain (non-expandable) chip, plus the group guide rail when it lives under
/// a collapsible header.
pub fn tool_chip(tool: &ToolItem, rail: bool, theme: &Theme) -> El {
    let mut root = row().h(CHIP_HEIGHT).w_full().shrink().items_center();
    if rail {
        root = root.child(el().ml(12.0).h_full().w(1.0).fill(theme.ink(0.08)));
    }
    let card_ml = if rail { 12.0 } else { 0.0 };
    root.child(
        el().ml(card_ml)
            .h(CHIP_CARD_HEIGHT)
            .min_w(0.0)
            .grow(1.0)
            .items_center()
            .radius(9.0)
            .border(1.0, theme.hairline(0.07))
            .fill(theme.ink(0.03))
            .clip_rounded(9.0)
            .child(chip_header_row(tool, None, theme)),
    )
}

/// A spawn chip: the whole card opens the subagent tab.
pub fn subagent_chip(tool: &ToolItem, rail: bool, theme: &Theme) -> El {
    let doc_id = tool.subagent_ref.clone().unwrap_or_default();
    let title = subagent_tab_title(&tool.call);
    let mut root = row().h(CHIP_HEIGHT).w_full().shrink().items_center();
    if rail {
        root = root.child(el().ml(12.0).h_full().w(1.0).fill(theme.ink(0.08)));
    }
    let card_ml = if rail { 12.0 } else { 0.0 };
    root.child(
        el().ml(card_ml)
            .h(CHIP_CARD_HEIGHT)
            .min_w(0.0)
            .grow(1.0)
            .items_center()
            .radius(9.0)
            .border(1.0, theme.hairline(0.07))
            .fill(theme.ink(0.03))
            .clip_rounded(9.0)
            .cursor(rune::Cursor::Pointer)
            .hover(theme.ink(0.05))
            .on_press(move |app: &mut App| {
                app.watch_subagent_doc(doc_id.clone());
                crate::shell::add_subagent_surface(app, doc_id.clone(), title.clone());
            })
            .child(chip_header_row(tool, Some(ChipTrail::OpenArrow), theme)),
    )
}

/// The transcript ErrorChip: a red-washed row with a DangerTriangle tile.
pub fn error_chip(message: &str, theme: &Theme) -> El {
    let danger = theme.danger;
    el().py(4.0).w_full().child(
        row()
            .min_h(34.0)
            .w_full()
            .items_center()
            .gap(8.0)
            .radius(10.0)
            .border(1.0, danger.alpha(0.16))
            .fill(danger.alpha(0.05))
            .px(8.0)
            .py(7.0)
            .child(
                el().size(20.0, 20.0)
                    .radius(6.0)
                    .fill(danger.alpha(0.12))
                    .center()
                    .child(
                        rune::icon(&crate::icons::DANGER_TRIANGLE, 12.0)
                            .color(theme.danger_muted.alpha(0.8)),
                    ),
            )
            .child(
                text("Error")
                    .font(TextStyle::new(12.0, 500.0))
                    .color(theme.danger_muted.alpha(0.8))
                    .single_line()
                    .shrink(),
            )
            .child(
                text(message.to_string())
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text.alpha(0.8))
                    .wrap()
                    .min_w(0.0)
                    .grow(1.0),
            ),
    )
}

/// A passive one-line chip marking a question the agent asked.
pub fn input_chip(header: &str, resolved: bool, theme: &Theme) -> El {
    let value = if resolved {
        header.to_string()
    } else {
        "Awaiting your answer…".to_string()
    };
    el().py(4.0).w_full().child(
        row()
            .h(34.0)
            .w_full()
            .items_center()
            .gap(8.0)
            .radius(10.0)
            .border(1.0, theme.hairline(0.08))
            .fill(theme.ink(0.045))
            .px(8.0)
            .child(
                el().size(20.0, 20.0)
                    .radius(6.0)
                    .fill(theme.ink(0.09))
                    .center()
                    .child(
                        rune::icon(&crate::icons::CHAT_ROUND_LINE, 12.0).color(theme.text_muted),
                    ),
            )
            .child(
                text("Question")
                    .font(TextStyle::new(12.0, 500.0))
                    .color(theme.text_muted)
                    .single_line()
                    .shrink(),
            )
            .child(
                text(value)
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text.alpha(0.9))
                    .truncate()
                    .min_w(0.0)
                    .grow(1.0),
            ),
    )
}

/// The body of an expanded chip card, under the header's separator.
pub fn detail_body(
    detail: &ToolDetail,
    highlights: Option<Arc<crate::changes::DiffHighlights>>,
    theme: &Theme,
) -> El {
    match detail {
        ToolDetail::Diff { file, .. } => {
            crate::changes::render_file_body_with_syntax(file, highlights, theme)
        }
        ToolDetail::Stats { stats } => {
            let mut body = col().w_full().py(6.0);
            for stat in stats.iter() {
                body = body.child(
                    row()
                        .h(OUTPUT_LINE_HEIGHT)
                        .w_full()
                        .min_w(0.0)
                        .px(12.0)
                        .items_center()
                        .gap(8.0)
                        .child(
                            text(stat.path.clone())
                                .font(TextStyle::new(11.5, 400.0).mono())
                                .color(theme.text.alpha(0.85))
                                .truncate()
                                .min_w(0.0)
                                .grow(1.0),
                        )
                        .child(
                            text(format!("+{}", stat.additions))
                                .font(TextStyle::new(11.5, 400.0).mono())
                                .color(theme.success)
                                .shrink(),
                        )
                        .child(
                            text(format!("−{}", stat.deletions))
                                .font(TextStyle::new(11.5, 400.0).mono())
                                .color(theme.danger)
                                .shrink(),
                        ),
                );
            }
            body
        }
        ToolDetail::Output {
            lines,
            truncated_by,
        } => {
            let mut body = col()
                .w_full()
                .py(6.0)
                .font(TextStyle::new(11.5, 400.0).mono());
            for line in lines {
                body = body.child(
                    el().h(OUTPUT_LINE_HEIGHT)
                        .w_full()
                        .min_w(0.0)
                        .px(12.0)
                        .child(
                            text(line.clone())
                                .font(TextStyle::new(11.5, 400.0).mono())
                                .color(theme.text.alpha(0.85))
                                .truncate()
                                .w_full(),
                        ),
                );
            }
            if *truncated_by > 0 {
                body = body.child(
                    el().h(OUTPUT_LINE_HEIGHT).px(12.0).center_y().child(
                        text(format!("… {truncated_by} more lines"))
                            .font(TextStyle::new(10.5, 400.0).mono())
                            .color(theme.text_faint),
                    ),
                );
            }
            body
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parser::parse_full;

    #[test]
    fn kb_labels_round_up() {
        assert_eq!(format_kb(812), "812 B");
        assert_eq!(format_kb(1024), "1 KB");
        assert_eq!(format_kb(12_500), "13 KB");
    }

    #[test]
    fn spawn_titles_strip_the_genus() {
        let call = ToolCall::Unknown {
            name: "Task: verify the marker pipeline".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&call), "verify the marker pipeline");
        let call = ToolCall::Unknown {
            name: "Taskmaster".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&call), "Taskmaster");
        let call = ToolCall::Exec {
            command: "ls".into(),
        };
        assert_eq!(subagent_tab_title(&call), "Subagent");
    }

    #[test]
    fn fold_animation_window_is_short() {
        let mut fold = FoldState::default();
        assert!(!fold.animating());
        fold.epoch = 1;
        fold.toggled_at = Some(std::time::Instant::now());
        assert!(fold.animating());
        fold.toggled_at = Some(std::time::Instant::now() - FOLD_TWEEN_WINDOW * 2);
        assert!(!fold.animating());
    }

    #[test]
    fn blob_detail_parses_diffs_and_caps_output() {
        let diff = serde_json::json!({
            "path": "a.rs",
            "oldText": "one\n",
            "newText": "two\n"
        });
        let detail = blob_detail(&diff.to_string(), true).expect("diff");
        assert!(matches!(detail, ToolDetail::Diff { .. }));
        let text = (0..FULL_OUTPUT_MAX_LINES + 3)
            .map(|ix| format!("l{ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        let detail = blob_detail(&text, false).expect("output");
        let ToolDetail::Output {
            lines,
            truncated_by,
        } = detail
        else {
            panic!()
        };
        assert_eq!(lines.len(), FULL_OUTPUT_MAX_LINES);
        assert_eq!(truncated_by, 3);
    }

    #[test]
    fn parser_is_available_for_detail_views() {
        assert!(!parse_full("hello").is_empty());
    }
}
