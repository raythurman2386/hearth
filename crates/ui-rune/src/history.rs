//! Git history pane: paged commit rows plus a topological lane graph.
//!
//! Ported from the gpui `history.rs`. The pure lane layout, ref-badge overflow
//! math and date formatting are unchanged (same tests); presentation uses
//! Rune `canvas(Shape)` for the graph and a virtual row list for commits. The
//! pane is hosted by the Changes surface for now: `changes::render` calls
//! [`history_view`] for [`crate::changes::DiffScope::History`].
//!
//! Clicking a commit pins it as its own `DiffScope::Commit` page (the shell's
//! per-commit tab action); the SHA chip copies the full hash.

use std::borrow::Cow;
use std::collections::HashSet;

use chrono::DateTime;
use hearth_proto::{GitHistoryCommit, GitHistoryRef, GitHistoryRefKind};
use rune::{
    canvas, col, el, icon, rich, row, text, Align, Color, Cursor, El, GrowthSpec, LineCap,
    ScrollSpec, Shape, TextStyle,
};

use crate::changes::{ChangesState, DiffScope};
use crate::state::App;
use crate::theme::Theme;

pub const HISTORY_PAGE_SIZE: usize = 100;
pub const HISTORY_ROW_HEIGHT: f32 = 36.0;
const HISTORY_LANE_SPACING: f32 = 12.0;
const HISTORY_NODE_RADIUS: f32 = 3.0;
const HISTORY_HEAD_RING_PADDING: f32 = 2.0;
const HISTORY_STROKE_WIDTH: f32 = 1.5;
const HISTORY_GRAPH_SATURATION: f32 = 0.72;
const HISTORY_GRAPH_SIDE_PADDING: f32 = 5.0;
const HISTORY_GRAPH_TRAILING_PADDING: f32 = 10.0;
const HISTORY_GRAPH_ROW_OVERLAP: f32 = 0.75;
const HISTORY_COMMIT_SUBJECT_MIN_WIDTH: f32 = 80.0;
const HISTORY_REF_AREA_RATIO: f32 = 0.45;
const HISTORY_REF_BADGE_MAX_WIDTH: f32 = 112.0;
const HISTORY_REF_GAP: f32 = 5.0;
const HISTORY_AUTHOR_WIDTH: f32 = 88.0;
const HISTORY_DATE_WIDTH: f32 = 88.0;
const HISTORY_SHA_WIDTH: f32 = 74.0;
const HISTORY_HEADER_HEIGHT: f32 = 24.0;
/// Curve segments are sampled from the gpui cubic beziers; `Shape` strokes
/// polylines.
const GRAPH_CURVE_STEPS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentShape {
    Through,
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphSegment {
    from_lane: usize,
    to_lane: usize,
    color_id: usize,
    shape: SegmentShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    sha: String,
    node_lane: usize,
    node_color_id: usize,
    segments: Vec<GraphSegment>,
    is_head: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphLayout {
    rows: Vec<GraphRow>,
    max_lane_count: usize,
}

#[derive(Debug, Clone)]
struct ActiveLane {
    id: usize,
    color_id: usize,
    target_sha: String,
}

fn lane_index(lanes: &[ActiveLane], id: usize) -> usize {
    lanes
        .iter()
        .position(|lane| lane.id == id)
        .expect("active Git history lane exists")
}

/// Commits arrive child-before-parent from `git log --topo-order`. Active
/// lanes point at the parent commit that will eventually resolve each path.
fn layout_graph(commits: &[GitHistoryCommit], head_sha: Option<&str>) -> GraphLayout {
    let mut active_lanes: Vec<ActiveLane> = Vec::new();
    let mut next_lane_id = 0usize;
    let mut next_color_id = 0usize;
    let mut max_lane_count = 0usize;
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let before = active_lanes.clone();
        let incoming: Vec<usize> = before
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| (lane.target_sha == commit.sha).then_some(index))
            .collect();
        let primary_incoming = incoming.first().copied();
        let node_lane = primary_incoming.unwrap_or(before.len());
        let primary_lane = primary_incoming.and_then(|index| before.get(index));
        let node_color_id = primary_lane.map(|lane| lane.color_id).unwrap_or_else(|| {
            let color = next_color_id;
            next_color_id += 1;
            color
        });
        let resolved_ids: HashSet<usize> = incoming.iter().map(|&index| before[index].id).collect();
        let mut next_lanes: Vec<ActiveLane> = before
            .iter()
            .filter(|lane| !resolved_ids.contains(&lane.id))
            .cloned()
            .collect();
        let mut outgoing: Vec<(usize, usize)> = Vec::new();

        let mut primary_outgoing_id = None;
        if let Some(first_parent) = commit.parent_shas.first() {
            let id = primary_lane.map(|lane| lane.id).unwrap_or_else(|| {
                let id = next_lane_id;
                next_lane_id += 1;
                id
            });
            let lane = ActiveLane {
                id,
                color_id: node_color_id,
                target_sha: first_parent.clone(),
            };
            next_lanes.insert(node_lane.min(next_lanes.len()), lane.clone());
            primary_outgoing_id = Some(id);
            outgoing.push((id, lane.color_id));
        }

        let mut parent_offset = 1usize;
        for parent_sha in commit.parent_shas.iter().skip(1) {
            if let Some(existing) = next_lanes
                .iter()
                .find(|lane| lane.target_sha == *parent_sha)
            {
                outgoing.push((existing.id, existing.color_id));
                continue;
            }
            let lane = ActiveLane {
                id: next_lane_id,
                color_id: next_color_id,
                target_sha: parent_sha.clone(),
            };
            next_lane_id += 1;
            next_color_id += 1;
            let primary_index = primary_outgoing_id
                .map(|id| lane_index(&next_lanes, id))
                .unwrap_or_else(|| node_lane.min(next_lanes.len()));
            next_lanes.insert(
                (primary_index + parent_offset).min(next_lanes.len()),
                lane.clone(),
            );
            parent_offset += 1;
            outgoing.push((lane.id, lane.color_id));
        }

        let mut segments: Vec<GraphSegment> = before
            .iter()
            .enumerate()
            .filter(|(_, lane)| !resolved_ids.contains(&lane.id))
            .map(|(from_lane, lane)| GraphSegment {
                from_lane,
                to_lane: lane_index(&next_lanes, lane.id),
                color_id: lane.color_id,
                shape: SegmentShape::Through,
            })
            .collect();
        segments.extend(incoming.iter().map(|&from_lane| GraphSegment {
            from_lane,
            to_lane: node_lane,
            color_id: before[from_lane].color_id,
            shape: SegmentShape::Incoming,
        }));
        segments.extend(outgoing.into_iter().map(|(id, color_id)| GraphSegment {
            from_lane: node_lane,
            to_lane: lane_index(&next_lanes, id),
            color_id,
            shape: SegmentShape::Outgoing,
        }));

        max_lane_count = max_lane_count
            .max(before.len())
            .max(next_lanes.len())
            .max(node_lane + 1);
        rows.push(GraphRow {
            sha: commit.sha.clone(),
            node_lane,
            node_color_id,
            segments,
            is_head: head_sha == Some(commit.sha.as_str()),
        });
        active_lanes = next_lanes;
    }

    GraphLayout {
        rows,
        max_lane_count,
    }
}

fn lane_x(lane: usize) -> f32 {
    HISTORY_GRAPH_SIDE_PADDING + HISTORY_NODE_RADIUS + lane as f32 * HISTORY_LANE_SPACING
}

fn graph_width(lane_count: usize) -> f32 {
    let count = lane_count.max(1);
    HISTORY_GRAPH_SIDE_PADDING
        + HISTORY_GRAPH_TRAILING_PADDING
        + HISTORY_NODE_RADIUS * 2.0
        + (count - 1) as f32 * HISTORY_LANE_SPACING
}

fn estimated_ref_badge_width(reference: &GitHistoryRef) -> f32 {
    // 10 px icon + 2 px gap + 10 px horizontal padding + the 10 px label.
    (22.0 + reference.label.chars().count() as f32 * 5.7).min(HISTORY_REF_BADGE_MAX_WIDTH)
}

fn estimated_ref_overflow_width(hidden: usize) -> f32 {
    format!("+{hidden}").chars().count() as f32 * 5.7
}

fn ref_area_width(commit_column_width: f32) -> f32 {
    let inner = (commit_column_width - 8.0).max(0.0);
    (inner * HISTORY_REF_AREA_RATIO)
        .min((inner - HISTORY_COMMIT_SUBJECT_MIN_WIDTH - HISTORY_REF_GAP).max(0.0))
}

fn visible_ref_count(refs: &[GitHistoryRef], available_width: f32) -> usize {
    if refs.is_empty() {
        return 0;
    }

    // Start with no visible badges so the overflow target remains available
    // when even the first badge plus `+N` would exceed the ref area.
    let mut visible = 0;
    for count in 1..=refs.len() {
        let hidden = refs.len() - count;
        let item_count = count + usize::from(hidden > 0);
        let badges = refs[..count]
            .iter()
            .map(estimated_ref_badge_width)
            .sum::<f32>();
        let overflow = if hidden > 0 {
            estimated_ref_overflow_width(hidden)
        } else {
            Default::default()
        };
        let gaps = item_count.saturating_sub(1) as f32 * HISTORY_REF_GAP;
        if badges + overflow + gaps <= available_width {
            visible = count;
        }
    }
    visible
}

fn format_date(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.format("%b %-d, %Y").to_string())
        .unwrap_or_else(|_| "—".to_string())
}

fn ref_color(reference: &GitHistoryRef, theme: &Theme) -> Color {
    match reference.kind {
        GitHistoryRefKind::Branch => theme.accent,
        GitHistoryRefKind::Remote => theme.busy,
        GitHistoryRefKind::Tag => theme.warning,
    }
}

fn ref_icon(kind: GitHistoryRefKind) -> &'static rune::Icon {
    match kind {
        GitHistoryRefKind::Branch => &crate::icons::GIT_BRANCH,
        GitHistoryRefKind::Remote => &crate::icons::CLOUD,
        GitHistoryRefKind::Tag => &crate::icons::TAG,
    }
}

fn ref_description(reference: &GitHistoryRef) -> String {
    let kind = match reference.kind {
        GitHistoryRefKind::Branch => "Branch",
        GitHistoryRefKind::Remote => "Remote branch",
        GitHistoryRefKind::Tag => "Tag",
    };
    format!("{kind}: {}", reference.label)
}

/// Mute a lane colour's chroma without moving its lightness, hue or alpha.
fn graph_color(color: Color) -> Color {
    let [l, a, b, alpha] = color.to_oklab();
    Color::from_oklab([
        l,
        a * HISTORY_GRAPH_SATURATION,
        b * HISTORY_GRAPH_SATURATION,
        alpha,
    ])
}

fn graph_palette(theme: &Theme) -> [Color; 6] {
    [
        graph_color(theme.accent),
        graph_color(theme.busy),
        graph_color(theme.success),
        graph_color(theme.warning),
        graph_color(theme.danger),
        graph_color(theme.text_muted),
    ]
}

// ---------------------------------------------------------------------------
// Pane
// ---------------------------------------------------------------------------

/// The history pane body. Public API for `changes::render`.
pub fn history_view(state: &ChangesState, app: &App, theme: &Theme) -> El {
    let graph = layout_graph(&state.history, state.head_sha.as_deref());
    let lane_count = graph.max_lane_count;
    let body = if app.selected_chat.is_none() || state.chat_cwd().is_none() {
        centered("No repository selected", theme.text_faint, theme)
    } else if state.history_loading && state.history.is_empty() {
        col()
            .w_full()
            .h_full()
            .center()
            .gap(8.0)
            .child(crate::loaders::gradient_spinner(theme, 3.0))
            .child(
                text("Loading history…")
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_faint),
            )
    } else if state.history.is_empty() {
        match state.history_error.as_deref() {
            Some(error) => centered(error, theme.warning, theme),
            None => centered("No commits found", theme.text_faint, theme),
        }
    } else {
        history_list(state, lane_count, graph.rows, theme)
    };

    let mut root = col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .fill(theme.bg)
        .child(history_strip(state, theme));
    if let Some(error) = state.fetch_error.as_deref() {
        root = root.child(error_banner(format!("Fetch failed: {error}"), theme));
    }
    if let Some(error) = state
        .history_error
        .as_deref()
        .filter(|_| !state.history.is_empty())
    {
        root = root.child(error_banner(error.to_string(), theme));
    }
    if !state.history.is_empty() {
        root = root.child(column_header(lane_count, theme));
    }
    root.child(body)
}

/// The pane's header controls (mounted by the shell in the titlebar strip):
/// commit count, branch and the Fetch button.
pub fn history_controls(state: &ChangesState, _app: &App, theme: &Theme) -> El {
    let count = state
        .history_lead_count
        .or(state.history_total)
        .unwrap_or(state.history.len());
    let mut controls = row()
        .shrink()
        .h_full()
        .center_y()
        .gap(10.0)
        .child(
            text(format!(
                "{count} commit{}",
                if count == 1 { "" } else { "s" }
            ))
            .font(TextStyle::new(11.0, 400.0))
            .color(theme.text_muted)
            .shrink(),
        )
        .child(
            text(state.chat_branch().unwrap_or("HEAD").to_string())
                .font(TextStyle::new(11.5, 400.0).mono())
                .color(theme.text_dim)
                .truncate()
                .min_w(0.0),
        )
        .child(refresh_button(theme));
    controls = controls.child(fetch_button(state, theme));
    controls
}

fn history_strip(state: &ChangesState, theme: &Theme) -> El {
    let count = state
        .history_lead_count
        .or(state.history_total)
        .unwrap_or(state.history.len());
    row()
        .w_full()
        .shrink()
        .h(36.0)
        .px(Theme::SPACE_LG)
        .gap(10.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], theme.hairline(0.06))
        .child(
            text(state.chat_cwd().unwrap_or("History").to_string())
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.text_muted)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .children((!state.history.is_empty()).then(|| {
            text(format!(
                "{count} commit{}",
                if count == 1 { "" } else { "s" }
            ))
            .font(TextStyle::new(11.0, 400.0))
            .color(theme.text_faint)
            .shrink()
        }))
        .child(fetch_button(state, theme))
}

fn column_header(lane_count: usize, theme: &Theme) -> El {
    let graph_column = graph_width(lane_count);
    row()
        .key("app/changes/history/columns")
        .w_full()
        .shrink()
        .h(HISTORY_HEADER_HEIGHT)
        .items_center()
        .border_sides([0.0, 0.0, 1.0, 0.0], theme.hairline(0.06))
        .child(el().w(graph_column).shrink())
        // The header is outside the scroll viewport, so this measures once
        // (and on resize) rather than every scrolled frame. Commit rows read
        // the width for their ref-badge overflow count.
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .items_center()
                .pr(8.0)
                .on_layout(|app: &mut App, [_, _, width, _]| {
                    app.changes.set_history_commit_width(width);
                })
                .child(
                    text("Commit")
                        .font(TextStyle::new(9.5, 400.0))
                        .color(theme.text_faint)
                        .grow(1.0)
                        .min_w(HISTORY_COMMIT_SUBJECT_MIN_WIDTH),
                ),
        )
        .child(column_label("Author", HISTORY_AUTHOR_WIDTH, theme))
        .child(column_label("Date", HISTORY_DATE_WIDTH, theme))
        .child(column_label("SHA", HISTORY_SHA_WIDTH, theme))
}

fn column_label(label: &'static str, width: f32, theme: &Theme) -> El {
    el().w(width)
        .shrink()
        .child(text(label).font(TextStyle::new(9.5, 400.0)).color(theme.text_faint))
}

fn history_list(
    state: &ChangesState,
    lane_count: usize,
    graph_rows: Vec<GraphRow>,
    theme: &Theme,
) -> El {
    let load_more =
        state.history_cursor.is_some() || (state.history_error.is_some() && !state.history.is_empty());
    let count = state.history.len() + usize::from(load_more);
    let row_theme = theme.clone();
    let row_graph = graph_rows;
    let rows = col()
        .key("app/changes/history/rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            count,
            HISTORY_ROW_HEIGHT,
            |app: &App, ix| {
                Cow::Owned(match app.changes.history.get(ix) {
                    Some(commit) => format!("app/changes/history/row/{}", commit.sha),
                    None => "app/changes/history/load-more".to_string(),
                })
            },
            move |app: &App, ix| {
                let state = &app.changes;
                match state.history.get(ix) {
                    Some(commit) => match row_graph.get(ix) {
                        Some(graph_row) => {
                            commit_row(state, ix, commit, graph_row, lane_count, &row_theme)
                        }
                        None => el(),
                    },
                    None => load_more_row(state, &row_theme),
                }
            },
        )
        .virtual_rows_version(state.history_version);
    let viewport = col()
        .key("app/changes/history/viewport")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(rows);
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(viewport)
}

// ---------------------------------------------------------------------------
// One commit row
// ---------------------------------------------------------------------------

fn commit_row(
    state: &ChangesState,
    index: usize,
    commit: &GitHistoryCommit,
    graph_row: &GraphRow,
    lane_count: usize,
    theme: &Theme,
) -> El {
    let subject = if commit.subject.is_empty() {
        "(no subject)".to_string()
    } else {
        commit.subject.clone()
    };
    let open = commit.clone();
    let sha = commit.sha.clone();
    let copy_sha = sha.clone();
    let short: String = sha.chars().take(7).collect();
    let copied = state.copied_sha.as_deref() == Some(sha.as_str());
    let refs = commit.refs.clone();
    let refs_width = ref_area_width(state.history_commit_width);
    let author = if commit.author_name.is_empty() {
        "Unknown".to_string()
    } else {
        commit.author_name.clone()
    };
    let date = format_date(&commit.authored_at);

    row()
        .w_full()
        .h(HISTORY_ROW_HEIGHT)
        .shrink()
        .items_center()
        .border_sides([0.0, 0.0, 1.0, 0.0], theme.hairline(0.04))
        .cursor(Cursor::Pointer)
        .hover(theme.ink(0.025))
        .role("button")
        .label(subject.clone())
        .on_press(move |app: &mut App| app.changes.pin_commit(open.clone()))
        .child(graph_cell(graph_row, lane_count, theme))
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .items_center()
                .gap(HISTORY_REF_GAP)
                .pr(8.0)
                .child(
                    rich(subject, Vec::new())
                        .font(TextStyle::new(12.0, 400.0))
                        .color(theme.text)
                        .single_line()
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .children((!refs.is_empty()).then(|| ref_area(&refs, index, refs_width, theme))),
        )
        .child(
            text(author)
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted)
                .w(HISTORY_AUTHOR_WIDTH)
                .shrink()
                .truncate()
                .pr(8.0),
        )
        .child(
            text(date)
                .font(TextStyle::new(10.5, 400.0))
                .color(theme.text_muted)
                .w(HISTORY_DATE_WIDTH)
                .shrink()
                .truncate()
                .pr(8.0),
        )
        .child(
            row()
                .key(format!("app/changes/history/sha/{index}"))
                .w(68.0)
                .h(24.0)
                .mr(6.0)
                .shrink()
                .center_y()
                .radius(4.0)
                .cursor(Cursor::Pointer)
                .hover(theme.ink(0.07))
                .on_press_tx(move |app: &mut App, tx: &mut rune::Tx| {
                    tx.copy(copy_sha.clone());
                    app.changes.copy_sha(copy_sha.clone());
                })
                .child(
                    text(if copied { "Copied".to_string() } else { short })
                        .font(TextStyle::new(10.5, 400.0).mono())
                        .color(if copied { theme.accent } else { theme.text_muted }),
                ),
        )
}

fn load_more_row(state: &ChangesState, theme: &Theme) -> El {
    let label = if state.history_loading {
        "Loading…"
    } else if state.history_error.is_some() {
        "Retry"
    } else {
        "Load more"
    };
    let mut button = row()
        .key("app/changes/history/load-older")
        .h(28.0)
        .px(11.0)
        .center_y()
        .gap(6.0)
        .radius(7.0)
        .border(1.0, theme.border)
        .fill(theme.surface_raised.alpha(0.72))
        .child(
            text(label)
                .font(TextStyle::new(11.0, 400.0))
                .color(if state.history_loading {
                    theme.text_faint
                } else {
                    theme.text_muted
                }),
        );
    if !state.history_loading {
        button = button
            .cursor(Cursor::Pointer)
            .hover(theme.element_hover)
            .on_press(|app: &mut App| {
                if app.changes.history_cursor.is_some() {
                    app.changes.load_older_history();
                } else {
                    app.changes.refresh_history();
                }
            });
    }
    row()
        .w_full()
        .h(48.0)
        .shrink()
        .center()
        .child(button)
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

fn graph_cell(graph_row: &GraphRow, lane_count: usize, theme: &Theme) -> El {
    let width = graph_width(lane_count);
    let palette = graph_palette(theme);
    let color = palette[graph_row.node_color_id % palette.len()];
    let node_x = lane_x(graph_row.node_lane);
    let middle = HISTORY_ROW_HEIGHT / 2.0;

    let mut shape = Shape::new();
    for segment in &graph_row.segments {
        let points = segment_points(segment);
        shape = shape.stroke(
            points,
            HISTORY_STROKE_WIDTH,
            palette[segment.color_id % palette.len()],
        );
    }

    let mut cell = el().w(width).h(HISTORY_ROW_HEIGHT).shrink();
    if !graph_row.segments.is_empty() {
        cell = cell.child(
            canvas(shape)
                .absolute(0.0, 0.0)
                .w(width)
                .h(HISTORY_ROW_HEIGHT),
        );
    }
    if graph_row.is_head {
        let ring = Shape::new().stroke_with(
            circle_points(
                node_x,
                middle,
                HISTORY_NODE_RADIUS + HISTORY_HEAD_RING_PADDING,
                20,
            ),
            true,
            HISTORY_STROKE_WIDTH,
            LineCap::Round,
            color,
        );
        cell = cell.child(
            canvas(ring)
                .absolute(0.0, 0.0)
                .w(width)
                .h(HISTORY_ROW_HEIGHT),
        );
    }
    cell.child(
        el()
            .absolute(
                node_x - HISTORY_NODE_RADIUS,
                middle - HISTORY_NODE_RADIUS,
            )
            .size(
                HISTORY_NODE_RADIUS * 2.0,
                HISTORY_NODE_RADIUS * 2.0,
            )
            .radius(HISTORY_NODE_RADIUS)
            .fill(color),
    )
}

/// Sample one lane segment (the gpui version used cubic beziers).
fn segment_points(segment: &GraphSegment) -> Vec<[f32; 2]> {
    let from_x = lane_x(segment.from_lane);
    let to_x = lane_x(segment.to_lane);
    let middle = HISTORY_ROW_HEIGHT / 2.0;
    match segment.shape {
        SegmentShape::Incoming => cubic_points(
            [from_x, -HISTORY_GRAPH_ROW_OVERLAP],
            [from_x, middle * 0.55],
            [to_x, middle * 0.55],
            [to_x, middle],
        ),
        SegmentShape::Outgoing => cubic_points(
            [from_x, middle],
            [from_x, middle * 1.45],
            [to_x, middle * 1.45],
            [to_x, HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP],
        ),
        SegmentShape::Through => {
            let start = [from_x, -HISTORY_GRAPH_ROW_OVERLAP];
            let end = [
                to_x,
                HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP,
            ];
            if segment.from_lane == segment.to_lane {
                vec![start, end]
            } else {
                cubic_points(start, [from_x, middle], [to_x, middle], end)
            }
        }
    }
}

fn cubic_points(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2]) -> Vec<[f32; 2]> {
    (0..=GRAPH_CURVE_STEPS)
        .map(|step| {
            let t = step as f32 / GRAPH_CURVE_STEPS as f32;
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            [
                a * p0[0] + b * p1[0] + c * p2[0] + d * p3[0],
                a * p0[1] + b * p1[1] + c * p2[1] + d * p3[1],
            ]
        })
        .collect()
}

fn circle_points(cx: f32, cy: f32, radius: f32, steps: usize) -> Vec<[f32; 2]> {
    (0..steps)
        .map(|step| {
            let angle = step as f32 / steps as f32 * std::f32::consts::TAU;
            [cx + radius * angle.cos(), cy + radius * angle.sin()]
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Ref badges
// ---------------------------------------------------------------------------

fn ref_area(refs: &[GitHistoryRef], row_index: usize, available_width: f32, theme: &Theme) -> El {
    let visible = visible_ref_count(refs, available_width);
    let hidden = refs.len() - visible;
    row()
        .max_w(available_width.max(0.0))
        .min_w(0.0)
        .shrink()
        .items_center()
        .gap(HISTORY_REF_GAP)
        .clip_rounded(0.0)
        .children(
            refs.iter()
                .take(visible)
                .enumerate()
                .map(|(ref_index, reference)| ref_badge(reference, row_index, ref_index, theme)),
        )
        .children((hidden > 0).then(|| {
            text(format!("+{hidden}"))
                .font(TextStyle::new(10.0, 400.0))
                .color(theme.text_faint)
                .shrink()
        }))
}

fn ref_badge(
    reference: &GitHistoryRef,
    row_index: usize,
    ref_index: usize,
    theme: &Theme,
) -> El {
    let color = ref_color(reference, theme);
    let description = ref_description(reference);
    row()
        .key(format!(
            "app/changes/history/ref/{row_index}/{ref_index}"
        ))
        .h(16.0)
        .max_w(HISTORY_REF_BADGE_MAX_WIDTH)
        .px(5.0)
        .shrink()
        .center_y()
        .gap(2.0)
        .radius(4.0)
        .fill(color.alpha(0.07))
        .role("img")
        .label(description)
        .child(icon(ref_icon(reference.kind), 10.0).color(color.alpha(0.78)))
        .child(
            text(reference.label.clone())
                .font(TextStyle::new(10.0, 400.0))
                .color(color.alpha(0.9))
                .truncate()
                .min_w(0.0),
        )
}

// ---------------------------------------------------------------------------
// Header buttons
// ---------------------------------------------------------------------------

fn refresh_button(theme: &Theme) -> El {
    row()
        .key("app/changes/history/refresh")
        .size(24.0, 24.0)
        .shrink()
        .center()
        .radius(6.0)
        .cursor(Cursor::Pointer)
        .hover(theme.wash(0.14))
        .role("button")
        .label("Refresh history")
        .on_press(|app: &mut App| app.changes.refresh_history())
        .child(icon(&crate::icons::REFRESH, 12.0).color(theme.text_muted))
}

fn fetch_button(state: &ChangesState, theme: &Theme) -> El {
    let fetching = state.fetching_all;
    let mut button = row()
        .key("app/changes/history/fetch-all")
        .h(24.0)
        .px(8.0)
        .shrink()
        .center_y()
        .gap(6.0)
        .radius(6.0)
        .role("button")
        .label("Fetch all");
    if fetching {
        button = button
            .fill(theme.wash(0.05))
            .child(crate::loaders::mini_gradient_spinner(theme, 1.75));
    } else {
        button = button
            .hover(theme.wash(0.14))
            .cursor(Cursor::Pointer)
            .on_press(|app: &mut App| app.changes.fetch_all())
            .child(icon(&crate::icons::CLOUD, 12.0).color(theme.text_muted.alpha(0.75)));
    }
    button.child(
        text(if fetching { "Fetching…" } else { "Fetch all" })
            .font(TextStyle::new(11.0, 400.0))
            .color(if fetching {
                theme.text_faint
            } else {
                theme.text_muted
            }),
    )
}

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

fn centered(message: impl Into<String>, color: Color, theme: &Theme) -> El {
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

fn error_banner(message: String, theme: &Theme) -> El {
    row()
        .w_full()
        .shrink()
        .px(8.0)
        .py(4.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], theme.danger.alpha(0.16))
        .fill(theme.danger.alpha(0.05))
        .child(
            text(message)
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.danger_muted)
                .wrap()
                .min_w(0.0),
        )
}

/// Whether a commit row wants its own tab (the shell may open a `DiffScope::Commit`
/// surface from this): the pane pins the commit in place today.
pub fn commit_scope() -> DiffScope {
    DiffScope::Commit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> GitHistoryCommit {
        GitHistoryCommit {
            sha: sha.into(),
            parent_shas: parents.iter().map(|value| (*value).into()).collect(),
            subject: sha.into(),
            author_name: "Test".into(),
            author_email: "test@example.com".into(),
            authored_at: "2026-08-12T12:00:00Z".into(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn graph_splits_and_rejoins_merge_lanes() {
        let commits = vec![
            commit("merge", &["main", "feature"]),
            commit("main", &["base"]),
            commit("feature", &["base"]),
            commit("base", &[]),
        ];
        let graph = layout_graph(&commits, Some("merge"));
        assert_eq!(graph.rows.len(), commits.len());
        assert!(graph.max_lane_count >= 2);
        assert!(graph.rows[0].is_head);
        assert_eq!(graph.rows[0].segments.len(), 2);
        assert_eq!(graph.rows[3].segments.len(), 2);
        assert_eq!(graph.rows[3].node_lane, 0);
    }

    #[test]
    fn appending_older_commits_preserves_the_loaded_prefix_layout() {
        let prefix = vec![commit("tip", &["parent"]), commit("parent", &["root"])];
        let before = layout_graph(&prefix, Some("tip"));
        let mut all = prefix.clone();
        all.push(commit("root", &[]));
        let after = layout_graph(&all, Some("tip"));
        assert_eq!(before.rows, after.rows[..before.rows.len()]);
    }

    #[test]
    fn graph_palette_only_reduces_chroma() {
        let source = rune::oklcha(0.62, 0.18, 250.0, 0.9);
        let muted = graph_color(source);
        let [sl, sa, sb, salpha] = source.to_oklab();
        let [ml, ma, mb, malpha] = muted.to_oklab();
        assert!((ml - sl).abs() < 1e-4);
        assert!((ma - sa * HISTORY_GRAPH_SATURATION).abs() < 1e-4);
        assert!((mb - sb * HISTORY_GRAPH_SATURATION).abs() < 1e-4);
        assert!((malpha - salpha).abs() < 1e-6);
    }

    #[test]
    fn ref_badges_expand_with_the_available_width() {
        let reference = |label: &str| GitHistoryRef {
            kind: GitHistoryRefKind::Branch,
            label: label.into(),
        };
        let refs = vec![reference("main"), reference("tag"), reference("origin")];
        assert_eq!(visible_ref_count(&[], 100.0), 0);
        assert_eq!(visible_ref_count(&refs, 70.0), 1);
        assert_eq!(visible_ref_count(&refs, 115.0), 2);
        assert_eq!(visible_ref_count(&refs, 200.0), 3);
    }

    #[test]
    fn ref_badges_preserve_overflow_when_the_first_badge_is_too_wide() {
        let reference = |label: &str| GitHistoryRef {
            kind: GitHistoryRefKind::Branch,
            label: label.into(),
        };
        let refs = vec![
            reference("feature/accessibility-polish"),
            reference("main"),
            reference("origin/main"),
            reference("upstream/main"),
            reference("v0.1.53"),
            reference("HEAD"),
        ];

        assert_eq!(visible_ref_count(&refs, 120.0), 0);
    }

    #[test]
    fn ref_area_preserves_subject_space_and_caps_at_forty_five_percent() {
        assert_eq!(ref_area_width(80.0), 0.0);
        assert!((ref_area_width(200.0) - 86.4).abs() < 0.001);
        assert!((ref_area_width(400.0) - 176.4).abs() < 0.001);
    }

    #[test]
    fn ref_tooltip_describes_each_reference_kind() {
        let reference = |kind: GitHistoryRefKind, label: &str| GitHistoryRef {
            kind,
            label: label.into(),
        };
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Branch, "main")),
            "Branch: main"
        );
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Remote, "origin/main")),
            "Remote branch: origin/main"
        );
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Tag, "v0.1.52")),
            "Tag: v0.1.52"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn history_views_build_without_a_runtime() {
        let app = App::new(crate::UiConfig::default());
        let state = ChangesState::default();
        let _empty = history_view(&state, &app, &app.theme);
        let _controls = history_controls(&state, &app, &app.theme);

        let mut state = ChangesState::default();
        state.history = vec![
            commit("merge", &["main", "feature"]),
            commit("main", &["base"]),
            commit("feature", &["base"]),
            commit("base", &[]),
        ];
        state.head_sha = Some("merge".into());
        let mut tagged = state.history[1].clone();
        tagged.refs = vec![
            GitHistoryRef {
                kind: GitHistoryRefKind::Branch,
                label: "main".into(),
            },
            GitHistoryRef {
                kind: GitHistoryRefKind::Tag,
                label: "v1.0.0".into(),
            },
        ];
        state.history[1] = tagged;
        let graph = layout_graph(&state.history, state.head_sha.as_deref());
        for (ix, row) in graph.rows.iter().enumerate() {
            let _ = commit_row(
                &state,
                ix,
                &state.history[ix],
                row,
                graph.max_lane_count,
                &app.theme,
            );
        }
        let _list = history_list(&state, graph.max_lane_count, graph.rows, &app.theme);
    }

    fn history_test_view(state: &App, _cx: &mut rune::Cx<App>) -> El {
        crate::changes::render(&state.changes, state, &state.theme)
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn history_layout_mounts_virtual_rows_and_the_graph() {
        use chrono::{TimeZone as _, Utc};
        use hearth_proto::Chat;

        let mut app = App::new(crate::UiConfig::default());
        app.local_device_id = Some("local".into());
        app.chats = vec![Chat {
            id: "chat".into(),
            device_id: "local".into(),
            title: None,
            archived: false,
            cwd: Some("/repo".into()),
            branch: Some("main".into()),
            checkout_id: Some("checkout".into()),
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }];
        app.selected_chat = Some("chat".into());
        app.changes.scope = DiffScope::History;
        app.changes.history = vec![
            commit("merge", &["main", "feature"]),
            commit("main", &["base"]),
            commit("feature", &["base"]),
            commit("base", &[]),
        ];
        app.changes.head_sha = Some("merge".into());
        let env = crate::changes::ChangesEnv::capture(&app);
        app.changes.refresh(&env);

        let mut rt = rune::Runtime::new(
            app,
            history_test_view,
            (900.0, 600.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        assert!(rt.find("app/changes/history/rows").is_some());
        assert!(rt.find("app/changes/history/row/merge").is_some());
        assert!(rt.find("app/changes/history/columns").is_some());
    }
}
