//! Rendering for the changes pane: rows, file headers, the scope/ref strip and
//! the split (side-by-side) layout.
//!
//! Everything here is pure over `&ChangesState` + `&App`; clicks are Rune
//! handlers that mutate `App` (and therefore `App::changes`). The virtual list
//! builds rows from `&App`, so a row can always see the latest folds, hover and
//! highlights.

use std::borrow::Cow;
use std::sync::Arc;

use rune::{Align, Color, El, GrowthSpec, ScrollSpec, Side, TextStyle, col, el, row, text};

use crate::state::App;
use crate::theme::Theme;

use super::*;

// ---------------------------------------------------------------------------
// Pane body
// ---------------------------------------------------------------------------

/// The pane's content: error banner, header strip and the virtualized list.
///
/// Public API for the shell:
/// `changes::render(&app.changes, app, &app.theme)`.
pub fn render(state: &ChangesState, app: &App, theme: &Theme) -> El {
    if state.scope == DiffScope::History {
        return crate::history::history_view(state, app, theme);
    }
    let scope = state.scope;
    let base = state.base_ref.clone();
    let phase = state.phase(app);

    let content = if let Some((message, warn)) = scoped_notice(state, app) {
        centered(
            message,
            if warn {
                theme.warning
            } else {
                theme.text_faint
            },
            theme,
        )
    } else {
        match phase {
            DiffPhase::Preparing => col().w_full().h_full().center().gap(Theme::SPACE_SM).child(
                text("Preparing diff…")
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_faint),
            ),
            DiffPhase::Clean => centered(
                clean_message(scope, base.as_deref()),
                theme.text_faint,
                theme,
            ),
            DiffPhase::List => {
                if state.parsed.is_some() {
                    list_view(state, theme)
                } else {
                    // Diff known, parse still running.
                    col().w_full().h_full().center().child(
                        text("Preparing diff…")
                            .font(TextStyle::new(12.0, 400.0))
                            .color(theme.text_faint),
                    )
                }
            }
        }
    };

    let mut root = col().w_full().h_full().min_h(0.0).fill(theme.bg);
    if let Some(error) = state.error.as_deref() {
        root = root.child(
            row()
                .w_full()
                .shrink()
                .px(Theme::SPACE_MD)
                .py(4.0)
                .border_sides([0.0, 0.0, 1.0, 0.0], theme.border)
                .child(
                    text(error)
                        .font(TextStyle::new(11.0, 400.0))
                        .color(theme.warning)
                        .wrap()
                        .min_w(0.0),
                ),
        );
    }
    root.child(content)
}

fn scoped_notice(state: &ChangesState, app: &App) -> Option<(String, bool)> {
    if app.selected_chat.is_none() || state.scope == DiffScope::WorkingTree {
        return None;
    }
    let message = state.scoped_error.clone()?;
    if message.contains("no turn recorded") {
        Some((
            "No turn recorded yet — send a message first".to_string(),
            false,
        ))
    } else if message.contains("unknown method") {
        Some((
            "This chat's device is running an older Hearth — update it to view branch and turn diffs"
                .to_string(),
            false,
        ))
    } else {
        Some((message, true))
    }
}

fn centered(message: String, color: Color, theme: &Theme) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .px(Theme::SPACE_LG)
        .child(
            text(message)
                .font(TextStyle::new(12.0, 400.0))
                .color(color)
                .align(Align::Center)
                .wrap()
                .min_w(0.0),
        )
        .fill(theme.bg)
}

fn list_view(state: &ChangesState, theme: &Theme) -> El {
    let count = state.rows.len();
    let version = state.row_version;
    let row_theme = theme.clone();
    let rows = col()
        .key("app/changes/rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            count,
            DIFF_LINE_HEIGHT,
            |app: &App, ix| {
                Cow::Owned(
                    app.changes
                        .rows
                        .get(ix)
                        .copied()
                        .map(DiffRow::key)
                        .unwrap_or_else(|| format!("app/changes/row/{ix}")),
                )
            },
            move |app: &App, ix| row_el(app, ix, &row_theme),
        )
        .virtual_rows_version(version)
        .on_visible_rows(|app: &mut App, range| {
            app.changes.set_visible(range);
            false
        });
    let viewport = col()
        .key("app/changes/viewport")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(rows);
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .children(header_strip(state, theme))
        .child(viewport)
}

fn header_strip(state: &ChangesState, theme: &Theme) -> Option<El> {
    state.parsed.as_ref()?;
    let label = scope_label(state.scope, state.file_count(), state.base_ref.as_deref());
    Some(
        row()
            .key("app/changes/strip")
            .w_full()
            .shrink()
            .h(36.0)
            .px(Theme::SPACE_LG)
            .gap(10.0)
            .center_y()
            .border_sides([0.0, 0.0, 1.0, 0.0], theme.hairline(0.06))
            .child(
                text(label)
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_muted)
                    .truncate()
                    .min_w(0.0),
            )
            .child(
                text(format!("+{}", state.additions()))
                    .font(TextStyle::new(11.0, 400.0).mono())
                    .color(theme.diff_add)
                    .shrink(),
            )
            .child(
                text(format!("−{}", state.deletions()))
                    .font(TextStyle::new(11.0, 400.0).mono())
                    .color(theme.diff_del)
                    .shrink(),
            )
            .child(el().grow(1.0))
            .children(state.truncated().then(|| {
                text("Partial snapshot")
                    .font(TextStyle::new(10.0, 400.0))
                    .color(theme.warning.alpha(0.75))
                    .px(6.0)
                    .py(2.0)
                    .radius(4.0)
                    .fill(theme.warning.alpha(0.08))
                    .shrink()
            })),
    )
}

// ---------------------------------------------------------------------------
// One row
// ---------------------------------------------------------------------------

fn row_el(app: &App, ix: usize, theme: &Theme) -> El {
    let state = &app.changes;
    let Some(row) = state.rows.get(ix).copied() else {
        return el();
    };
    let Some(parsed) = state.parsed.as_ref() else {
        return el();
    };
    let files = parsed.files.clone();
    match row {
        DiffRow::FileHeader { file } => match files.get(file as usize) {
            Some(file_diff) => file_header(state, file as usize, file_diff, theme),
            None => el(),
        },
        DiffRow::Notice { file, notice } => files
            .get(file as usize)
            .and_then(|file| file_notices(file).into_iter().nth(notice as usize))
            .map(|text| notice_row(text, theme))
            .unwrap_or_else(el),
        DiffRow::HunkHeader { file, hunk } => files
            .get(file as usize)
            .and_then(|file| file.hunks.get(hunk as usize))
            .map(|hunk| hunk_header_row(&hunk.header, theme))
            .unwrap_or_else(el),
        DiffRow::Line {
            file, hunk, line, ..
        } => {
            let Some(file_diff) = files.get(file as usize) else {
                return el();
            };
            let Some(line) = file_diff
                .hunks
                .get(hunk as usize)
                .and_then(|hunk| hunk.lines.get(line as usize))
            else {
                return el();
            };
            diff_line_el(state, file_diff, line, theme)
        }
        DiffRow::SplitLine {
            file,
            hunk,
            left,
            right,
        } => {
            let Some(file_diff) = files.get(file as usize) else {
                return el();
            };
            let Some(lines) = file_diff.hunks.get(hunk as usize).map(|hunk| &hunk.lines) else {
                return el();
            };
            split_line_el(state, file_diff, lines, (left, right), theme)
        }
        DiffRow::CommentCard { file, card } => {
            let Some(file_diff) = files.get(file as usize) else {
                return el();
            };
            state
                .comments_for(&file_diff.path)
                .get(card as usize)
                .map(|comment| render_comment_card(comment, theme))
                .unwrap_or_else(el)
        }
        DiffRow::CommentDraft { file } => {
            let Some(file_diff) = files.get(file as usize) else {
                return el();
            };
            state
                .draft
                .as_ref()
                .filter(|draft| draft.path == file_diff.path)
                .map(|draft| render_comment_draft(draft, theme))
                .unwrap_or_else(el)
        }
        DiffRow::BodyPad { .. } => el().w_full().h(BODY_BOTTOM_PAD).shrink(),
        DiffRow::FoldingBody { file } => {
            let Some(file_diff) = files.get(file as usize) else {
                return el();
            };
            let fold = state
                .folds
                .get(&file_diff.path)
                .copied()
                .unwrap_or_default();
            let to = fold.to.max(0.0);
            let cap = fold.from.max(fold.to).clamp(0.0, FOLD_TWEEN_MAX_PX);
            let body = render_file_body_upto(state, file_diff, theme, cap, state.mode);
            if to <= 0.0 {
                el()
            } else {
                col().w_full().h(to).shrink().child(body)
            }
        }
    }
}

fn file_header(state: &ChangesState, ix: usize, file: &FileDiff, theme: &Theme) -> El {
    let fold = state.folds.get(&file.path).copied().unwrap_or_default();
    let chevron = if fold.collapsed { ">" } else { "v" };
    let collapsed = fold.collapsed;
    // Path-keyed, never index-keyed: a virtual row kept by `memo` can move to
    // a different index, and an index capture would toggle the wrong file.
    let path = file.path.clone();
    row()
        .key(format!("app/changes/fh/{ix}"))
        .w_full()
        .h(FILE_HEADER_HEIGHT)
        .shrink()
        .px(Theme::SPACE_MD)
        .gap(8.0)
        .center_y()
        .fill(theme.ink(0.025))
        .hover(theme.ink(0.05))
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label(format!(
            "{} {}",
            if collapsed { "Expand" } else { "Collapse" },
            file.path
        ))
        .on_press(move |app: &mut App| app.changes.toggle_fold_path(&path))
        .children((ix > 0).then(|| {
            el().absolute(0.0, 0.0)
                .w_full()
                .h(1.0)
                .fill(theme.hairline(0.04))
        }))
        .child(
            row().size(14.0, 14.0).shrink().center().child(
                text(chevron)
                    .font(TextStyle::new(11.0, 500.0).mono())
                    .color(theme.text_muted.alpha(0.7)),
            ),
        )
        .child(
            text(file.path.clone())
                .font(TextStyle::new(12.0, 400.0).mono())
                .color(theme.text_dim)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .children(file.binary.then(|| {
            text("BIN")
                .font(TextStyle::new(10.0, 400.0))
                .color(theme.text_faint)
                .shrink()
        }))
        .children((file.additions > 0 || !file.binary).then(|| {
            text(format!("+{}", file.additions))
                .font(TextStyle::new(11.0, 400.0).mono())
                .color(theme.diff_add)
                .shrink()
        }))
        .children((file.deletions > 0 || !file.binary).then(|| {
            text(format!("−{}", file.deletions))
                .font(TextStyle::new(11.0, 400.0).mono())
                .color(theme.diff_del)
                .shrink()
        }))
}

fn notice_row(notice: String, theme: &Theme) -> El {
    row()
        .w_full()
        .h(NOTICE_HEIGHT)
        .shrink()
        .px(Theme::SPACE_LG)
        .center_y()
        .child(
            text(notice)
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_faint)
                .truncate(),
        )
}

fn hunk_header_row(header: &str, theme: &Theme) -> El {
    row()
        .w_full()
        .h(HUNK_HEADER_HEIGHT)
        .shrink()
        .px(Theme::SPACE_LG)
        .center_y()
        .fill(theme.diff_hunk_bg)
        .child(
            text(header.to_string())
                .font(TextStyle::new(11.0, 400.0).mono())
                .color(theme.text_faint)
                .truncate(),
        )
}

/// One +/−/context/meta diff line: coloured accent bar, dual line-number
/// gutters (`gutter_px` wide — see [`gutter_width`]), marker column, and
/// paint-only syntax runs.
fn diff_line_el(state: &ChangesState, file: &FileDiff, line: &DiffLine, theme: &Theme) -> El {
    let gutter_px = gutter_width(file);
    if line.kind == LineKind::Meta {
        return meta_line_row(
            &line.text,
            theme,
            ACCENT_BAR_WIDTH + 2.0 * gutter_px + MARKER_WIDTH + 12.0,
        );
    }
    let (marker, marker_color, row_bg, accent, number_color) = line_colors(line.kind, theme);
    let mut root = row().w_full().shrink().center_y();
    root = if state.wrap {
        root.min_h(DIFF_LINE_HEIGHT)
    } else {
        root.h(DIFF_LINE_HEIGHT)
    };
    if let Some(bg) = row_bg {
        root = root.fill(bg);
    }
    root = root
        .child(
            el().w(ACCENT_BAR_WIDTH)
                .h_full()
                .shrink()
                .fill(accent.unwrap_or(Color::TRANSPARENT)),
        )
        .child(gutter(
            line.old_no,
            if line.kind == LineKind::Del {
                number_color
            } else {
                theme.text_faint.alpha(0.8)
            },
            gutter_px,
        ))
        .child(gutter(
            line.new_no,
            if line.kind == LineKind::Add {
                number_color
            } else {
                theme.text_faint.alpha(0.8)
            },
            gutter_px,
        ))
        .child(
            row().w(MARKER_WIDTH).shrink().center().child(
                text(marker)
                    .font(TextStyle::new(DIFF_TEXT_SIZE, 400.0).mono())
                    .color(marker_color),
            ),
        )
        .child(code_line_el(
            line,
            state.highlight_for(&file.path).as_deref(),
            theme,
            12.0,
            state.wrap,
        ));

    let Some((side, line_no)) = line_anchor(line) else {
        return root;
    };
    let path = file.path.clone();
    if state.hovering(&path, (side, line_no)) {
        root = root.child(render_comment_adder(&path, side, line_no, theme).absolute(
            comment_adder_left(side, gutter_px),
            (DIFF_LINE_HEIGHT - COMMENT_ADDER_SIZE) / 2.0,
        ));
    }
    let hover_path = path;
    root.on_hover_change(move |app: &mut App, hovered, _tx: &mut rune::Tx| {
        if hovered {
            app.changes.set_hover(&hover_path, Some((side, line_no)));
        } else {
            app.changes.clear_hover(&hover_path, (side, line_no));
        }
    })
}

fn mono(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight).mono()
}

fn gutter(no: Option<u32>, color: Color, gutter_px: f32) -> El {
    row()
        .w(gutter_px)
        .shrink()
        .justify_end()
        .pr(8.0)
        .center_y()
        .child(
            text(no.map(|n| n.to_string()).unwrap_or_default())
                .font(mono(11.0, 400.0))
                .color(color)
                .single_line(),
        )
}

/// Diff line with its highlight spans attached (the helper above cannot build
/// a synthetic `DiffLine`; callers pass the real one).
///
/// Long lines must stay reachable: by default the code sits in a horizontal
/// scroll viewport (`scroll_x`), with the gutters and marker pinned outside it.
/// [`ChangesState::set_wrap`] swaps that for wrapping, which lets the row grow
/// to its measured height in the virtual list.
fn code_line_el(
    line: &DiffLine,
    highlights: Option<&DiffHighlights>,
    theme: &Theme,
    pad: f32,
    wrap: bool,
) -> El {
    let spans = spans_for_line(line, highlights, theme);
    if wrap {
        return text(line.text.clone())
            .font(mono(DIFF_TEXT_SIZE, 400.0))
            .color(theme.text.alpha(0.92))
            .wrap_anywhere()
            .grow(1.0)
            .min_w(0.0)
            .pl(pad)
            .spans(spans);
    }
    row().grow(1.0).min_w(0.0).h(DIFF_LINE_HEIGHT).child(
        el().grow(1.0).min_w(0.0).h_full().scroll_x().child(
            text(line.text.clone())
                .font(mono(DIFF_TEXT_SIZE, 400.0))
                .color(theme.text.alpha(0.92))
                .single_line()
                .pl(pad)
                .spans(spans),
        ),
    )
}

fn line_colors(
    kind: LineKind,
    theme: &Theme,
) -> (&'static str, Color, Option<Color>, Option<Color>, Color) {
    let add_bg = tint(theme.diff_add, 0.055);
    let del_bg = tint(theme.diff_del, 0.055);
    match kind {
        LineKind::Add => (
            "+",
            theme.diff_add,
            Some(add_bg),
            Some(theme.diff_add.alpha(0.55)),
            theme.diff_add.alpha(0.9),
        ),
        LineKind::Del => (
            "−",
            theme.diff_del,
            Some(del_bg),
            Some(theme.diff_del.alpha(0.55)),
            theme.diff_del.alpha(0.9),
        ),
        _ => (
            "·",
            theme.text_faint.alpha(0.5),
            None,
            None,
            theme.text_faint.alpha(0.8),
        ),
    }
}

fn tint(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

fn meta_line_row(value: &str, theme: &Theme, pad_left: f32) -> El {
    row()
        .w_full()
        .h(DIFF_LINE_HEIGHT)
        .shrink()
        .center_y()
        .pl(pad_left)
        .child(
            text(value.to_string())
                .font(TextStyle::new(10.5, 400.0).italic())
                .color(theme.text_faint)
                .single_line()
                .truncate(),
        )
}

// ---------------------------------------------------------------------------
// Split (side-by-side) rendering
// ---------------------------------------------------------------------------

fn split_line_el(
    state: &ChangesState,
    file: &FileDiff,
    lines: &[DiffLine],
    pair: (Option<u32>, Option<u32>),
    theme: &Theme,
) -> El {
    let gutter_px = gutter_width(file);
    let wrap = state.wrap;
    let (left, right) = (
        pair.0.and_then(|ix| lines.get(ix as usize)),
        pair.1.and_then(|ix| lines.get(ix as usize)),
    );
    // `\ No newline at end of file` is not code on one side — it is a note
    // about the row, so it spans both columns.
    if let Some(line) = [left, right]
        .into_iter()
        .flatten()
        .find(|line| line.kind == LineKind::Meta)
    {
        return meta_line_row(&line.text, theme, 2.0 * (ACCENT_BAR_WIDTH + gutter_px));
    }
    let highlights = state.highlight_for(&file.path);
    let left_cell = left
        .map(|line| {
            split_line_cell(
                line,
                line.old_no,
                highlights.as_deref(),
                theme,
                gutter_px,
                wrap,
            )
        })
        .unwrap_or_else(|| split_filler(theme));
    let mut right_cell = match right {
        Some(line) => split_line_cell(
            line,
            line.new_no,
            highlights.as_deref(),
            theme,
            gutter_px,
            wrap,
        ),
        None => split_filler(theme),
    };
    if let Some(line) = right
        && let Some(anchor) = line_anchor(line)
    {
        let (side, line_no) = anchor;
        let path = file.path.clone();
        if state.hovering(&path, anchor) {
            right_cell =
                right_cell.child(render_comment_adder(&path, side, line_no, theme).absolute(
                    split_adder_left(gutter_px),
                    (DIFF_LINE_HEIGHT - COMMENT_ADDER_SIZE) / 2.0,
                ));
        }
        let hover_path = path;
        right_cell =
            right_cell.on_hover_change(move |app: &mut App, hovered, _tx: &mut rune::Tx| {
                if hovered {
                    app.changes.set_hover(&hover_path, Some(anchor));
                } else {
                    app.changes.clear_hover(&hover_path, anchor);
                }
            });
    }
    let mut outer = row().w_full().shrink().items_stretch();
    outer = if wrap {
        outer.min_h(DIFF_LINE_HEIGHT)
    } else {
        outer.h(DIFF_LINE_HEIGHT)
    };
    outer
        .child(left_cell)
        .child(
            el().w(SPLIT_DIVIDER_WIDTH)
                .h_full()
                .shrink()
                .fill(theme.hairline(0.06)),
        )
        .child(right_cell)
}

fn split_line_cell(
    line: &DiffLine,
    number: Option<u32>,
    highlights: Option<&DiffHighlights>,
    theme: &Theme,
    gutter_px: f32,
    wrap: bool,
) -> El {
    let (marker, marker_color, row_bg, accent, number_color) = line_colors(line.kind, theme);
    let mut cell = row().grow(1.0).min_w(0.0).h_full().center_y();
    if let Some(bg) = row_bg {
        cell = cell.fill(bg);
    }
    cell.child(
        el().w(ACCENT_BAR_WIDTH)
            .h_full()
            .shrink()
            .fill(accent.unwrap_or(Color::TRANSPARENT)),
    )
    .child(
        row()
            .w(gutter_px)
            .shrink()
            .justify_end()
            .pr(8.0)
            .center_y()
            .child(
                text(number.map(|n| n.to_string()).unwrap_or_default())
                    .font(mono(11.0, 400.0))
                    .color(number_color)
                    .single_line(),
            ),
    )
    .child(
        row().w(SPLIT_MARKER_WIDTH).shrink().center().child(
            text(marker)
                .font(mono(DIFF_TEXT_SIZE, 400.0))
                .color(marker_color),
        ),
    )
    .child(code_line_el(line, highlights, theme, 6.0, wrap))
}

fn split_filler(theme: &Theme) -> El {
    el().grow(1.0).min_w(0.0).h_full().fill(theme.ink(0.03))
}

// ---------------------------------------------------------------------------
// Stacked file body (transcript tool diffs and the fold stand-in)
// ---------------------------------------------------------------------------

/// The expanded body of one file section: notices, hunk headers, +/-/context
/// lines with a coloured accent bar, dual line-number gutters, a marker
/// column, and paint-only syntax runs.
pub fn render_file_body_with_syntax(
    file: &FileDiff,
    highlights: Option<Arc<DiffHighlights>>,
    theme: &Theme,
) -> El {
    let mut children: Vec<El> = Vec::new();
    let gutter_px = gutter_width(file);
    for notice in file_notices(file) {
        children.push(notice_row(notice, theme));
    }
    for hunk in &file.hunks {
        children.push(hunk_header_row(&hunk.header, theme));
        for line in &hunk.lines {
            children.push(stacked_line(
                line,
                highlights.as_deref(),
                theme,
                gutter_px,
                false,
            ));
        }
    }
    col()
        .w_full()
        .shrink()
        .pb(BODY_BOTTOM_PAD)
        .children(children)
}

fn stacked_line(
    line: &DiffLine,
    highlights: Option<&DiffHighlights>,
    theme: &Theme,
    gutter_px: f32,
    wrap: bool,
) -> El {
    if line.kind == LineKind::Meta {
        return meta_line_row(
            &line.text,
            theme,
            ACCENT_BAR_WIDTH + 2.0 * gutter_px + MARKER_WIDTH + 12.0,
        );
    }
    let (marker, marker_color, row_bg, accent, number_color) = line_colors(line.kind, theme);
    let mut root = row().w_full().shrink().center_y();
    root = if wrap {
        root.min_h(DIFF_LINE_HEIGHT)
    } else {
        root.h(DIFF_LINE_HEIGHT)
    };
    if let Some(bg) = row_bg {
        root = root.fill(bg);
    }
    root.child(
        el().w(ACCENT_BAR_WIDTH)
            .h_full()
            .shrink()
            .fill(accent.unwrap_or(Color::TRANSPARENT)),
    )
    .child(gutter(
        line.old_no,
        if line.kind == LineKind::Del {
            number_color
        } else {
            theme.text_faint.alpha(0.8)
        },
        gutter_px,
    ))
    .child(gutter(
        line.new_no,
        if line.kind == LineKind::Add {
            number_color
        } else {
            theme.text_faint.alpha(0.8)
        },
        gutter_px,
    ))
    .child(
        row().w(MARKER_WIDTH).shrink().center().child(
            text(marker)
                .font(mono(DIFF_TEXT_SIZE, 400.0))
                .color(marker_color),
        ),
    )
    .child(code_line_el(line, highlights, theme, 12.0, wrap))
}

/// Build only rows that start above `max_px`, so the fold stand-in never
/// materializes lines its clip cannot reveal.
fn render_file_body_upto(
    state: &ChangesState,
    file: &FileDiff,
    theme: &Theme,
    max_px: f32,
    mode: DiffMode,
) -> El {
    let mut children: Vec<El> = Vec::new();
    let mut y = 0.0f32;
    let gutter_px = gutter_width(file);
    let highlights = state.highlight_for(&file.path);

    'build: {
        for notice in file_notices(file) {
            if y >= max_px {
                break 'build;
            }
            children.push(notice_row(notice, theme));
            y += NOTICE_HEIGHT;
        }
        for hunk in &file.hunks {
            if y >= max_px {
                break 'build;
            }
            children.push(hunk_header_row(&hunk.header, theme));
            y += HUNK_HEADER_HEIGHT;
            match mode {
                DiffMode::Unified => {
                    for line in &hunk.lines {
                        if y >= max_px {
                            break 'build;
                        }
                        children.push(stacked_line(
                            line,
                            highlights.as_deref(),
                            theme,
                            gutter_px,
                            state.wrap,
                        ));
                        y += DIFF_LINE_HEIGHT;
                    }
                }
                DiffMode::Split => {
                    let budget = ((max_px - y) / DIFF_LINE_HEIGHT).ceil().max(0.0) as usize;
                    for (left, right) in split_pairs_upto(&hunk.lines, budget) {
                        if y >= max_px {
                            break 'build;
                        }
                        let line_at =
                            |slot: Option<u32>| slot.and_then(|slot| hunk.lines.get(slot as usize));
                        let (left, right) = (line_at(left), line_at(right));
                        let marker = [left, right]
                            .into_iter()
                            .flatten()
                            .find(|line| line.kind == LineKind::Meta);
                        children.push(match marker {
                            Some(line) => meta_line_row(
                                &line.text,
                                theme,
                                2.0 * (ACCENT_BAR_WIDTH + gutter_px),
                            ),
                            None => {
                                let cell = |line: Option<&DiffLine>, old: bool| match line {
                                    Some(line) => split_line_cell(
                                        line,
                                        if old { line.old_no } else { line.new_no },
                                        highlights.as_deref(),
                                        theme,
                                        gutter_px,
                                        state.wrap,
                                    ),
                                    None => split_filler(theme),
                                };
                                let mut pair = row().w_full().shrink().items_stretch();
                                pair = if state.wrap {
                                    pair.min_h(DIFF_LINE_HEIGHT)
                                } else {
                                    pair.h(DIFF_LINE_HEIGHT)
                                };
                                pair.child(cell(left, true))
                                    .child(
                                        el().w(SPLIT_DIVIDER_WIDTH)
                                            .h_full()
                                            .shrink()
                                            .fill(theme.hairline(0.06)),
                                    )
                                    .child(cell(right, false))
                            }
                        });
                        y += DIFF_LINE_HEIGHT;
                    }
                }
            }
        }
    }

    col()
        .w_full()
        .shrink()
        .pb(BODY_BOTTOM_PAD)
        .children(children)
}

// ---------------------------------------------------------------------------
// Header controls (mounted by the shell in the titlebar strip)
// ---------------------------------------------------------------------------

/// The pane-header controls: scope dropdown, `{branch} → {base ⌄}` ref
/// selector (branch scope), split toggle and fold-all. The shell renders this
/// inside the session titlebar's trailing section; expand/close buttons stay
/// shell-owned.
///
/// Public API: `changes::header_controls(&app.changes, app, &app.theme)`.
pub fn header_controls(state: &ChangesState, app: &App, theme: &Theme) -> El {
    if let Some(commit) = &state.commit {
        let short: String = commit.sha.chars().take(7).collect();
        return row()
            .w_full()
            .h_full()
            .center_y()
            .gap(8.0)
            .child(
                row()
                    .shrink()
                    .h(22.0)
                    .px(6.0)
                    .radius(5.0)
                    .center_y()
                    .fill(theme.ink(0.05))
                    .child(text(short).font(mono(10.5, 400.0)).color(theme.text_muted)),
            )
            .child(
                text(commit.subject.clone())
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text)
                    .truncate()
                    .grow(1.0)
                    .min_w(0.0),
            )
            .child(split_toggle(state, theme))
            .child(wrap_toggle(state, theme))
            .child(fold_all_button(theme));
    }

    let scope = state.scope;
    let trigger = row()
        .key("app/changes/scope-trigger")
        .h(24.0)
        .px(8.0)
        .shrink()
        .center_y()
        .gap(6.0)
        .radius(6.0)
        .cursor(rune::Cursor::Pointer)
        .fill(theme.wash(0.05))
        .hover(theme.wash(0.14))
        .role("button")
        .label(scope.label())
        .on_press(|app: &mut App| app.changes.toggle_scope_menu())
        .child(
            text(scope.label())
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.text),
        )
        .child(
            text("v")
                .font(mono(10.0, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        );

    let trailing = if scope == DiffScope::History {
        crate::history::history_controls(state, app, theme)
    } else {
        row()
            .shrink()
            .center_y()
            .gap(2.0)
            .child(split_toggle(state, theme))
            .child(wrap_toggle(state, theme))
            .child(fold_all_button(theme))
    };

    let mut strip = row().w_full().h_full().center_y().gap(6.0).child(trigger);
    if let Some(menu) = state.scope_menu_open.then(|| scope_menu(state, theme)) {
        strip = strip.child(menu);
    }
    if scope == DiffScope::Branch
        && let Some(selector) = ref_selector(state, theme)
    {
        strip = strip.child(selector);
    }
    strip.child(el().grow(1.0)).child(trailing)
}

fn split_toggle(state: &ChangesState, theme: &Theme) -> El {
    let active = state.mode.is_split();
    header_button("app/changes/split", "||", active, theme).on_press(|app: &mut App| {
        app.changes.toggle_mode();
    })
}

fn wrap_toggle(state: &ChangesState, theme: &Theme) -> El {
    header_button("app/changes/wrap", "↩", state.wrap, theme).on_press(|app: &mut App| {
        app.changes.toggle_wrap();
    })
}

fn fold_all_button(theme: &Theme) -> El {
    header_button("app/changes/fold-all", "≡", false, theme)
        .on_press(|app: &mut App| app.changes.toggle_collapse_all())
}

fn header_button(id: &'static str, glyph: &'static str, active: bool, theme: &Theme) -> El {
    let mut button = row()
        .key(id)
        .size(24.0, 24.0)
        .shrink()
        .center()
        .radius(6.0)
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label(id.rsplit('/').next().unwrap_or(id));
    button = if active {
        button
            .fill(theme.wash(0.14))
            .child(text(glyph).font(mono(12.0, 500.0)).color(theme.text))
    } else {
        button.hover(theme.wash(0.14)).child(
            text(glyph)
                .font(mono(12.0, 500.0))
                .color(theme.text_muted.alpha(0.7)),
        )
    };
    button
}

fn scope_menu(state: &ChangesState, theme: &Theme) -> El {
    col()
        .key("app/changes/scope-menu")
        .w(180.0)
        .padding(4.0)
        .gap(2.0)
        .radius(8.0)
        .fill(theme.surface_overlay)
        .border(1.0, theme.border)
        .shadow(8.0, 24.0, theme.hairline(0.15))
        .anchored("app/changes/scope-trigger", Side::Bottom, Align::Start, 6.0)
        .children(DiffScope::ALL.into_iter().map(|scope| {
            let current = state.scope == scope;
            let mut item = row()
                .key(format!("app/changes/scope-row/{}", scope.mode()))
                .w_full()
                .h(28.0)
                .px(8.0)
                .center_y()
                .radius(5.0)
                .cursor(rune::Cursor::Pointer)
                .role("menuitem")
                .label(scope.label())
                .on_press(move |app: &mut App| app.changes.set_scope(scope));
            if current {
                item = item.fill(theme.wash(0.11));
            } else {
                item = item.hover(theme.wash(0.08));
            }
            item.child(
                text(scope.label())
                    .font(TextStyle::new(12.0, 400.0))
                    .color(if current {
                        theme.text
                    } else {
                        theme.text_muted
                    }),
            )
        }))
}

fn ref_selector(state: &ChangesState, theme: &Theme) -> Option<El> {
    let branch = state.chat_branch().unwrap_or("HEAD").to_string();
    let base = state.base_ref.clone().unwrap_or_else(|| "…".to_string());
    let trigger = row()
        .key("app/changes/ref-trigger")
        .h(22.0)
        .px(6.0)
        .min_w(0.0)
        .center_y()
        .gap(4.0)
        .radius(6.0)
        .cursor(rune::Cursor::Pointer)
        .hover(theme.wash(0.12))
        .role("button")
        .label(format!("Base ref: {base}"))
        .on_press(|app: &mut App| app.changes.toggle_ref_menu())
        .child(
            text(base)
                .font(mono(11.5, 400.0))
                .color(theme.text)
                .truncate()
                .min_w(0.0),
        )
        .child(
            text("v")
                .font(mono(9.0, 400.0))
                .shrink()
                .color(theme.text_muted.alpha(0.7)),
        );
    let mut selector = row()
        .min_w(0.0)
        .center_y()
        .gap(6.0)
        .ml(6.0)
        .child(
            text(branch)
                .font(mono(11.5, 400.0))
                .color(theme.text_dim)
                .truncate()
                .min_w(0.0),
        )
        .child(
            text("→")
                .font(mono(10.0, 400.0))
                .shrink()
                .color(theme.text_faint),
        )
        .child(trigger);
    if state.ref_menu_open {
        selector = selector.child(ref_menu(state, theme));
    }
    Some(selector)
}

fn ref_menu(state: &ChangesState, theme: &Theme) -> El {
    let rows = state.ref_menu_rows();
    let current = state.base_ref.clone();
    let list: El = if rows.is_empty() {
        col().px(8.0).py(6.0).child(
            text(if state.branches.is_empty() {
                "No branches"
            } else {
                "No matching branches"
            })
            .font(TextStyle::new(12.0, 400.0))
            .color(theme.text_faint),
        )
    } else {
        col()
            .key("app/changes/ref-list")
            .w_full()
            .max_h(240.0)
            .gap(2.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .children(rows.into_iter().map(|branch_ix| {
                let name = state.branches[branch_ix].clone();
                let selected = current.as_deref() == Some(name.as_str());
                let label = name.clone();
                let mut item = row()
                    .key(format!("app/changes/ref-row/{branch_ix}"))
                    .w_full()
                    .h(26.0)
                    .px(6.0)
                    .center_y()
                    .radius(5.0)
                    .cursor(rune::Cursor::Pointer)
                    .role("menuitem")
                    .label(label.clone())
                    .on_press(move |app: &mut App| {
                        app.changes.set_base_ref(name.clone());
                    });
                if selected {
                    item = item.fill(theme.wash(0.11));
                } else {
                    item = item.hover(theme.wash(0.08));
                }
                item.child(
                    text(label)
                        .font(mono(12.0, 400.0))
                        .color(theme.text)
                        .truncate(),
                )
            }))
    };
    col()
        .key("app/changes/ref-menu")
        .w(240.0)
        .padding(6.0)
        .gap(6.0)
        .radius(8.0)
        .fill(theme.surface_overlay)
        .border(1.0, theme.border)
        .shadow(8.0, 24.0, theme.hairline(0.15))
        .anchored("app/changes/ref-trigger", Side::Bottom, Align::Start, 6.0)
        .child(
            rune::text_input(&state.ref_search, TextStyle::new(12.0, 400.0).lh(18.0))
                .key("app/changes/ref-search")
                .w_full()
                .h(26.0)
                .shrink()
                .single_line()
                .placeholder("Search branches…", theme.text_faint)
                .caret(theme.caret, theme.selection)
                .on_edit(|app: &mut App| &mut app.changes.ref_search),
        )
        .child(list)
}
