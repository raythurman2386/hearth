//! BlockTree → Rune elements.
//!
//! Each top-level block is rebuilt as Markdown source and drawn by
//! [`rune_kit::Markdown`]. That gives the transcript GFM tables, task lists,
//! fenced code with paint-only syntax colours, diff and mermaid fences, and one
//! selection across blocks (`selectable(true)`), while the block tree still
//! supplies the streaming split points and stable row identity.
//!
//! Deliberate deltas from the gpui renderer (documented per docs/rune-port.md):
//! - table geometry is rune-kit's (column floor 120 pt, cell padding 10/6)
//!   instead of the hand-rolled 96 pt floor and 12 px padding;
//! - inline code takes rune-kit's chip (mono, themed wash) rather than the
//!   violet `code_text`/`code_wash` pair;
//! - code blocks get a Copy overlay via `code_frame` and their own horizontal
//!   scroll; the gpui header strip is not reproduced (the fence's language is
//!   available to the frame if a caller wants to draw it);
//! - the streaming fade is rune-kit's word reveal instead of the paint veil.

use std::ops::Range;
use std::sync::Arc;

use rune::{El, Span, TextStyle, col, el, row, text};
use rune_kit::Markdown;
use rune_kit::markdown::MarkdownStyle;
use rune_kit::syntax::{Highlighter, Paint};

use crate::state::App;
use crate::theme::Theme;

use super::mend::PENDING_LINK_URL;
use super::parser::{Block, InlineRun, TableAlign};

/// Gap between markdown blocks inside one message (hearth mdBlockGap).
pub const MD_BLOCK_GAP: f32 = 12.0;
/// Body text size / line height (hearth: 14px / 22px).
pub const MD_TEXT_SIZE: f32 = 14.0;
pub const MD_LINE_HEIGHT: f32 = 22.0;
/// Code block metrics — the gpui constants, kept for callers that reserve room.
pub const CODE_TEXT_SIZE: f32 = 12.5;
pub const CODE_LINE_HEIGHT: f32 = 18.0;
pub const CODE_PADDING_X: f32 = 12.0;
pub const CODE_PADDING_Y: f32 = 10.0;

/// A faint code-block wash over the panel.
pub fn inline_code_text(theme: &Theme) -> rune::Color {
    theme.code_text
}
pub fn inline_code_wash(theme: &Theme) -> rune::Color {
    theme.code_wash
}

/// Map hearth_syntax tokens onto rune-kit's paint contract.
///
/// This is the same neutral `hearth_syntax` document the changes pane used, so
/// a fence in the transcript and the same fence in a diff colour identically.
pub fn hearth_highlights(
    lang: Option<&str>,
    code: &str,
    theme: &Theme,
) -> Vec<(Range<usize>, Paint)> {
    let Some(document) = hearth_syntax::highlight(hearth_syntax::HighlightRequest {
        source: code,
        path: None,
        fence_tag: lang,
    })
    .ok() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut line_start = 0usize;
    for line in &document.lines {
        for span in line {
            if span.range.start < span.range.end && span.range.end <= code.len() {
                out.push((
                    line_start + span.range.start..line_start + span.range.end,
                    Paint::Color(theme.syntax.color(span.kind)),
                ));
            }
        }
        line_start += code[line_start..]
            .find('\n')
            .map(|at| at + 1)
            .unwrap_or(code.len() - line_start);
    }
    out
}

/// The theme's semantic highlighter, keyed by appearance so two themes never
/// share cached paint.
pub fn highlighter(theme: &Theme) -> Highlighter {
    let paint = theme.clone();
    Highlighter::new(move |lang, code| hearth_highlights(lang, code, &paint)).key(theme.appearance)
}

/// The transcript's markdown style: the kit's structure and sizing, with
/// Hearth's foreground, muted, code and selection colours.
pub fn markdown_style(theme: &Theme) -> MarkdownStyle {
    MarkdownStyle::from_kit(&theme.kit)
        .text(theme.text)
        .muted(theme.text_muted)
        .link(theme.text)
        .link_underline(theme.text_muted)
        .inline_code_bg(theme.code_wash)
        .code_block(theme.ink(0.035), theme.border)
        .code_block_padding(CODE_PADDING_X, CODE_PADDING_Y)
        .body_font(
            TextStyle::new(MD_TEXT_SIZE, 400.0)
                .lh(MD_LINE_HEIGHT)
                .family(rune::Family::Sans),
        )
        .heading_font(
            1,
            TextStyle::new(19.0, 600.0)
                .lh(27.0)
                .family(rune::Family::Sans),
        )
        .heading_font(
            2,
            TextStyle::new(16.0, 600.0)
                .lh(24.0)
                .family(rune::Family::Sans),
        )
        .heading_font(
            3,
            TextStyle::new(15.0, 600.0)
                .lh(22.0)
                .family(rune::Family::Sans),
        )
        .code_block_font(
            TextStyle::new(CODE_TEXT_SIZE, 400.0)
                .lh(CODE_LINE_HEIGHT)
                .mono(),
        )
        .selection(theme.selection)
        .rule(theme.border)
        .quote_rule(theme.accent.alpha(0.6))
        .quote(theme.accent.alpha(0.6), 2.0, 12.0, theme.text_muted)
        .text_width(crate::transcript::MAX_CONTENT_WIDTH)
}

/// Rebuild one block's Markdown source.
///
/// The tree is authoritative for splits and row identity; this is only the
/// input to the kit's renderer. Round-tripping is intentionally mechanical:
/// fenced code and tables keep their exact bytes, inline runs re-emit their
/// markers, and nested containers indent their children.
pub fn block_source(block: &Block) -> String {
    let mut out = String::new();
    write_block(block, 0, &mut out);
    out
}

fn write_block(block: &Block, depth: usize, out: &mut String) {
    match block {
        Block::Paragraph { runs } => {
            write_runs(runs, out);
            out.push('\n');
        }
        Block::Heading { level, runs } => {
            out.push_str(&"#".repeat((*level).clamp(1, 6) as usize));
            out.push(' ');
            write_runs(runs, out);
            out.push('\n');
        }
        Block::CodeBlock { language, code } => {
            // The fence must be longer than any backtick run inside the code,
            // or a fence in the payload closes it early.
            let fence = code_fence(code);
            out.push_str(&fence);
            if let Some(language) = language {
                out.push_str(language);
            }
            out.push('\n');
            out.push_str(code);
            out.push('\n');
            out.push_str(&fence);
            out.push('\n');
        }
        Block::BlockQuote { children } => {
            let mut inner = String::new();
            for child in children {
                write_block(child, depth + 1, &mut inner);
            }
            for line in inner.lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
        }
        Block::List {
            ordered_start,
            items,
        } => {
            for (ix, item) in items.iter().enumerate() {
                match ordered_start {
                    Some(start) => {
                        out.push_str(&format!("{}. ", start + ix as u64));
                    }
                    None => out.push_str("- "),
                }
                let mut inner = String::new();
                for child in item {
                    write_block(child, depth + 1, &mut inner);
                }
                let mut first = true;
                for line in inner.trim_end_matches('\n').split('\n') {
                    if first {
                        out.push_str(line);
                        first = false;
                    } else {
                        out.push('\n');
                        for _ in 0..(depth + 1) * 2 + 2 {
                            out.push(' ');
                        }
                        out.push_str(line);
                    }
                }
                out.push('\n');
            }
        }
        Block::Table {
            header,
            rows,
            align,
        } => {
            write_table_row(header, out);
            out.push('|');
            for column in align {
                out.push_str(match column {
                    TableAlign::Left => ":--|",
                    TableAlign::Center => ":-:|",
                    TableAlign::Right => "--:|",
                });
            }
            out.push('\n');
            for row in rows {
                write_table_row(row, out);
            }
        }
        Block::Rule => out.push_str("---\n"),
    }
}

/// A code fence longer than any backtick run in the payload (≥ 3).
fn code_fence(code: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for ch in code.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn write_table_row(cells: &[Vec<InlineRun>], out: &mut String) {
    out.push('|');
    for cell in cells {
        out.push(' ');
        let mut text = String::new();
        write_runs(cell, &mut text);
        out.push_str(&text.replace('|', "\\|").replace('\n', " "));
        out.push_str(" |");
    }
    out.push('\n');
}

fn write_runs(runs: &[InlineRun], out: &mut String) {
    for run in runs {
        if run.style.code {
            out.push_str(&code_span(&run.text));
            continue;
        }
        let text = escape_inline(&run.text).replace('\n', "  \n");
        if run.style.bold {
            out.push_str("**");
        }
        if run.style.italic {
            out.push('*');
        }
        if run.style.strikethrough {
            out.push_str("~~");
        }
        if let Some(url) = &run.style.link {
            out.push('[');
            out.push_str(&text);
            out.push_str("](");
            out.push_str(&escape_url(url));
            out.push(')');
        } else {
            out.push_str(&text);
        }
        if run.style.strikethrough {
            out.push_str("~~");
        }
        if run.style.italic {
            out.push('*');
        }
        if run.style.bold {
            out.push_str("**");
        }
    }
}

/// Backslash-escape the characters a LITERAL text run must not re-parse as
/// markup when its block is rebuilt.
///
/// The block tree carries runs, not source ranges, so the renderer re-emits
/// source. Without this, a prompt containing `*`, `_`, `[`, a backtick or a
/// line starting `- ` came back as emphasis, a link or a list on the round
/// trip. CommonMark treats a backslash before any ASCII punctuation as that
/// literal character, so escaping generously is safe (it never changes the
/// rendered text); `|` is left to the table writer, which owns cell escaping.
fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut line_start = true;
    let mut digits_only = true;
    for ch in text.chars() {
        if ch == '\n' {
            out.push(ch);
            line_start = true;
            digits_only = true;
            continue;
        }
        if line_start && ch == ' ' {
            out.push(ch);
            continue;
        }
        let ordered_marker = (ch == '.' || ch == ')') && digits_only && !line_start;
        if ordered_marker
            || matches!(
                ch,
                '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '~' | '-' | '+' | '=' | '!'
            )
        {
            out.push('\\');
        }
        out.push(ch);
        line_start = false;
        digits_only &= ch.is_ascii_digit();
    }
    out
}

/// An inline code span that survives backticks in its own text: a fence longer
/// than the longest run inside, padded when the content starts or ends with a
/// backtick or a space.
fn code_span(text: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`')
        || text.ends_with('`')
        || text.starts_with(' ')
        || text.ends_with(' ')
    {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// A link destination in `(...)` form: angle-bracketed when it holds
/// whitespace or parentheses, otherwise backslash-escaped for `)` and `\`.
fn escape_url(url: &str) -> String {
    if url
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '(' || ch == ')' || ch == '<' || ch == '>')
    {
        format!("<{}>", url.replace('<', "%3C").replace('>', "%3E"))
    } else {
        url.replace('\\', "\\\\").replace(')', "\\)")
    }
}

/// Render one top-level (or nested) markdown block.
///
/// `streaming` turns on the kit's streaming parse and word reveal. `selectable`
/// is on for transcript rows; previews can pass `false` to keep the text inert.
pub fn render_block(block: &Block, streaming: bool, selectable: bool, theme: &Theme) -> El {
    let source = block_source(block);
    if source.trim().is_empty() {
        return el();
    }
    let code_theme = theme.clone();
    Markdown::new(source)
        .style(markdown_style(theme))
        .streaming(streaming)
        .reveal(if streaming { 0.18 } else { -1.0 })
        .selectable(selectable)
        .highlighter(highlighter(theme))
        .on_link_tx(|_app: &mut App, url: &str, tx| {
            // The mender's sentinel styles a still-streaming link without
            // making it clickable (its destination does not exist yet).
            if url != PENDING_LINK_URL {
                let _ = tx.open_url(url);
            }
        })
        .code_frame(move |card, frame| {
            let code = frame.source.to_string();
            col().w_full().child(card).child(
                row().w_full().justify_end().px(6.0).pb(4.0).child(
                    row()
                        .gap(4.0)
                        .center_y()
                        .px(6.0)
                        .py(2.0)
                        .radius(5.0)
                        .hover(code_theme.ink(0.08))
                        .cursor(rune::Cursor::Pointer)
                        .help("Copy code")
                        .on_press_tx(move |_app: &mut App, tx| tx.copy(code.clone()))
                        .child(
                            text("Copy")
                                .font(TextStyle::new(10.5, 400.0))
                                .color(code_theme.text_muted),
                        ),
                ),
            )
        })
        .scroll_wide(true)
        .build(&theme.kit)
        .w_full()
        .min_w(0.0)
        .clip_soft(0.0)
}

/// A settled block from a tree; `None` when the index is out of range.
pub fn render_tree_block(
    tree: &Arc<super::parser::BlockTree>,
    block_ix: usize,
    streaming: bool,
    theme: &Theme,
) -> Option<El> {
    let top = tree.blocks.get(block_ix)?;
    Some(render_block(&top.block, streaming, true, theme))
}

/// Plain rich text for a non-markdown surface (a user bubble) from inline runs.
pub fn inline_text(runs: &[InlineRun], size: f32, line_height: f32, theme: &Theme) -> El {
    let mut content = String::new();
    let mut spans: Vec<Span> = Vec::new();
    for run in runs {
        let start = content.len();
        content.push_str(&run.text);
        let end = content.len();
        if start == end {
            continue;
        }
        let mut span = Span::new(start, end);
        if run.style.bold {
            span.weight = Some(600.0);
        }
        if run.style.italic {
            span.italic = true;
        }
        if run.style.code {
            span.mono = true;
            span.bg = Some(inline_code_wash(theme));
            span.bg_pad = 2.0;
        }
        if run.style.strikethrough {
            span = span.color(theme.text_muted);
        }
        spans.push(span);
    }
    rune::rich(content, spans)
        .font(TextStyle::new(size, 400.0).lh(line_height))
        .color(theme.text)
        .wrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parser::parse_full;

    #[test]
    fn block_sources_round_trip_through_the_parser() {
        let cases = [
            "# title\n\npara **bold** and `code`\n",
            "- one\n- two\n  - nested\n",
            "1. first\n2. second\n",
            "```rust\nlet x = 1;\n```\n",
            "| a | b |\n|---|---|\n| 1 \\| 2 | 2 |\n",
            "> quoted\n> text\n",
            "para with **literal \\* stars** and \\`tick\\`\n",
        ];
        for source in cases {
            let tree = parse_full(source);
            let rebuilt: String = tree
                .blocks
                .iter()
                .map(|top| block_source(&top.block))
                .collect::<Vec<_>>()
                .join("\n");
            let again = parse_full(&rebuilt);
            assert_eq!(
                tree.blocks.len(),
                again.blocks.len(),
                "block count changed for {source:?}:\n{rebuilt}"
            );
            for (a, b) in tree.blocks.iter().zip(again.blocks.iter()) {
                assert_eq!(
                    std::mem::discriminant(&a.block),
                    std::mem::discriminant(&b.block),
                    "block kind changed for {source:?}:\n{rebuilt}"
                );
            }
        }
    }

    #[test]
    fn code_fences_keep_their_bytes() {
        let tree = parse_full("```ts\nlet x = 1;\n```\n");
        let Block::CodeBlock { language, code } = &tree.blocks[0].block else {
            panic!("expected code");
        };
        assert_eq!(language.as_deref(), Some("ts"));
        assert_eq!(code, "let x = 1;");
        let rebuilt = block_source(&tree.blocks[0].block);
        assert!(rebuilt.contains("```ts\nlet x = 1;\n```"));
    }

    #[test]
    fn highlight_ranges_stay_inside_the_line() {
        let theme = Theme::dark();
        let code = "let x = 1; // done";
        let spans = hearth_highlights(Some("rust"), code, &theme);
        assert!(!spans.is_empty());
        for (range, _) in spans {
            assert!(range.start < range.end && range.end <= code.len());
        }
    }

    /// Rebuild every block of `source` and parse the result again; the blocks
    /// must match the original tree (kind + inline runs), proving the renderer
    /// did not turn literal text into markup.
    fn assert_round_trip(source: &str) {
        let tree = parse_full(source);
        let rebuilt: String = tree
            .blocks
            .iter()
            .map(|top| block_source(&top.block))
            .collect::<Vec<_>>()
            .join("\n");
        let again = parse_full(&rebuilt);
        assert_eq!(
            tree.blocks.len(),
            again.blocks.len(),
            "block count changed for {source:?}:\n{rebuilt}"
        );
        for (a, b) in tree.blocks.iter().zip(again.blocks.iter()) {
            assert_eq!(a.block, b.block, "block changed for {source:?}:\n{rebuilt}");
        }
    }

    #[test]
    fn literal_markdown_metacharacters_survive_the_round_trip() {
        let cases = [
            "a \\* b \\* c",
            "literal \\_underscores\\_ here",
            "\\[not a link\\] and \\[x\\]\\(y\\)",
            "a backtick \\` in prose",
            "\\- not a list\nsecond line",
            "1\\. not an ordered list",
            "\\# not a heading",
            "\\> not a quote",
            "\\!\\[not an image\\](x)",
            "\\<not html\\>",
            "a \\~tilde\\~ and \\|pipe\\|",
            "\\\\ a literal backslash",
        ];
        for source in cases {
            assert_round_trip(source);
        }
    }

    #[test]
    fn inline_code_with_backticks_uses_a_longer_fence() {
        assert_round_trip("``code with a ` tick``\n");
        let tree = parse_full("``code with a ` tick``\n");
        let rebuilt = block_source(&tree.blocks[0].block);
        assert!(rebuilt.contains("``"), "{rebuilt}");
        assert_round_trip(&rebuilt);
    }

    #[test]
    fn code_fence_longer_than_the_payload() {
        let source = "````text\ncontains ``` inside\n````\n";
        let tree = parse_full(source);
        let rebuilt = block_source(&tree.blocks[0].block);
        assert!(rebuilt.starts_with("````text"), "{rebuilt}");
        assert_round_trip(source);
    }

    #[test]
    fn paragraph_line_starts_cannot_become_blocks() {
        for source in ["\\- dashed\n", "\\+ plus\n", "\\= equals\n", "\\# hash\n"] {
            let tree = parse_full(source);
            assert!(
                matches!(tree.blocks[0].block, Block::Paragraph { .. }),
                "{source:?} parsed as {:?}",
                tree.blocks[0].block
            );
            let rebuilt = block_source(&tree.blocks[0].block);
            let again = parse_full(&rebuilt);
            assert!(
                matches!(again.blocks[0].block, Block::Paragraph { .. }),
                "round trip of {source:?} became {:?}:\n{rebuilt}",
                again.blocks[0].block
            );
        }
    }
}
