//! The transcript viewport: a growth stack of virtual rows inside a
//! `stick_bottom` scroll viewport, plus the message-rail overlay.
//!
//! Public API for the shell:
//! - [`render`] — the primary transcript for the selected chat.
//! - [`render_subagent`] — a read-only transcript over a subagent doc.
//! - [`crate::transcript::transcript_jump_shown`] and
//!   [`crate::transcript::scroll_to_bottom`] — the floating jump pill.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use rune::{El, GrowthSpec, ScrollSpec, TextStyle, col, el, row as rrow, text};

use crate::rail::{self, RailTick};
use crate::state::App;
use crate::theme::Theme;

use super::BlobFetch;
use super::chips::{
    CHIP_CARD_HEIGHT, CHIP_GAP, CHIP_HEADER_HEIGHT, CHIP_HEIGHT, CHIPS_TOP_PAD, chip_header,
    detail_body, error_chip, input_chip, subagent_chip, tool_chip,
};
use super::model::{
    Row, RowKind, ToolItem, format_elapsed, format_timestamp, is_spawn_link, sending_bridge,
    tool_group_collapses, tool_group_summary,
};
use super::rows::top_gap_for;
use super::viewport::SCROLL_BUTTON_THRESHOLD_PX;
use crate::markdown::parser::parse_full;

/// List overdraw beyond the viewport (kept for parity with the gpui constant).
pub const OVERDRAW_PX: f32 = 320.0;
/// Transcript column max width (hearth 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;
/// The transcript scroll viewport's element key (`Tx::scroll_*` targets it).
pub const VIEWPORT_KEY: &str = "app/transcript/viewport";
/// Estimated row height for rows that have never been measured.
const ESTIMATED_ROW_HEIGHT: f32 = 96.0;

/// `HEARTH_FRAME_STATS=1` logs live-row render-cost percentiles at `warn`
/// level. Off by default; zero cost when off.
fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("HEARTH_FRAME_STATS").is_ok_and(|value| !value.is_empty() && value != "0")
    })
}

const FRAME_STATS_WINDOW: usize = 240;

fn record_live_frame_us(us: u64) {
    thread_local! {
        static SAMPLES: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    SAMPLES.with(|samples| {
        let mut samples = samples.borrow_mut();
        samples.push(us);
        if samples.len() >= FRAME_STATS_WINDOW {
            samples.sort_unstable();
            let p50 = samples[samples.len() / 2];
            let p95 = samples[samples.len() * 95 / 100];
            let max = *samples.last().unwrap();
            tracing::warn!(
                n = samples.len(),
                p50_us = p50,
                p95_us = p95,
                max_us = max,
                "live-row render cost"
            );
            samples.clear();
        }
    });
}

/// The primary transcript.
pub fn render(app: &App, theme: &Theme) -> El {
    app.transcript_state.sync(app);
    let rows = Arc::new(app.transcript_state.rows.borrow().clone());
    let version = app.transcript_state.row_version.get();
    render_rows(app, theme, rows, false, version)
}

/// A read-only transcript over one subagent doc.
pub fn render_subagent(app: &App, theme: &Theme, doc_id: &str) -> El {
    let entries = app.sub_transcript(doc_id);
    let mut rows: Vec<Row> = Vec::new();
    let mut parse = |_key: &str, source: &str| Arc::new(parse_full(source));
    for entry in entries {
        rows.extend(super::rows::rows_for_entry(entry, false, &mut parse));
    }
    let version = super::row_set_version(&rows);
    render_rows(app, theme, Arc::new(rows), true, version)
}

fn render_rows(app: &App, theme: &Theme, rows: Arc<Vec<Row>>, subagent: bool, version: u64) -> El {
    let state = &app.transcript_state;
    let count = rows.len();
    let rail_enabled =
        !subagent && state.rail_enabled && rail::rail_visible(app.shell.viewport_width);
    let theme_owned = theme.clone();
    let key_rows = rows.clone();
    let build_rows = rows.clone();
    let build_theme = theme.clone();

    let growth = col()
        .key("app/transcript/rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows_version(version)
        .virtual_rows(
            count,
            ESTIMATED_ROW_HEIGHT,
            move |_app: &App, ix| {
                Cow::Owned(
                    key_rows
                        .get(ix)
                        .map(|row| row.id.clone())
                        .unwrap_or_else(|| format!("app/transcript/row/{ix}")),
                )
            },
            move |app: &App, ix| {
                let Some(row) = build_rows.get(ix) else {
                    return el();
                };
                row_el(
                    app,
                    row,
                    build_rows.get(ix.wrapping_sub(1)),
                    &build_theme,
                    ix + 1 == count,
                    !subagent,
                )
            },
        )
        .on_visible_rows(|app: &mut App, range| {
            app.transcript_state.visible_start.set(range.start);
            false
        });

    let viewport = col()
        .key(VIEWPORT_KEY)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_edge_fade(Theme::TRANSCRIPT_FADE_BAND)
        .on_at_bottom(|app: &mut App, at_bottom| {
            app.transcript_state.pinned = at_bottom;
        })
        .on_far_from_bottom(SCROLL_BUTTON_THRESHOLD_PX, |app: &mut App, far| {
            app.transcript_state.show_jump_button = far;
        })
        .child(growth);

    let mut root = col()
        .key("app/transcript")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(viewport);

    if rail_enabled {
        let pairs = rail_pairs(app, &rows);
        let tick_rows: Vec<usize> = pairs
            .iter()
            .filter_map(|(_, row_id)| rows.iter().position(|row| &row.id == row_id))
            .collect();
        let active = rail::active_tick(&tick_rows, state.visible_start.get());
        root = root.child(rail::render(app, theme, &pairs, active, 0.0));
    }

    if let Some(preview) = app.transcript_state.attachment_preview.clone() {
        root = root.child(crate::attachments::lightbox(
            &preview,
            &theme_owned,
            |app: &mut App| app.transcript_state.attachment_preview = None,
        ));
    }
    root
}

fn rail_pairs(app: &App, rows: &[Row]) -> Vec<(RailTick, String)> {
    let (entries, echoes) = match &app.transcript_state.doc_override {
        Some(doc_id) => (app.sub_transcript(doc_id).to_vec(), Vec::new()),
        None => (app.transcript.clone(), Vec::new()),
    };
    rail::rail_ticks(&entries, &echoes)
        .into_iter()
        .filter_map(|tick| {
            rows.iter()
                .find(|row| row.id == tick.message_id)
                .map(|row| (tick, row.id.clone()))
        })
        .collect()
}

fn row_el(
    app: &App,
    row: &Row,
    prev: Option<&Row>,
    theme: &Theme,
    last: bool,
    show_trailer: bool,
) -> El {
    let state = &app.transcript_state;
    let is_first = prev.is_none();
    let top_gap = if is_first {
        if !show_trailer || state.doc_override.is_some() {
            Theme::SPACE_LG
        } else {
            Theme::TITLEBAR_HEIGHT + Theme::SPACE_LG + 10.0
        }
    } else {
        top_gap_for(prev, row)
    };
    let bottom_pad = if last {
        let runway = state
            .own_turn
            .as_ref()
            .filter(|anchor| {
                state
                    .rows
                    .borrow()
                    .iter()
                    .any(|candidate| candidate.entry_id == anchor.message_id)
            })
            .map_or(0.0, |anchor| anchor.runway);
        state.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND + 8.0 + runway
    } else {
        0.0
    };

    let inner: El = match &row.kind {
        RowKind::User {
            text: prompt,
            mentions,
            attachments,
            badges,
            pending,
        } => {
            let mut column = col().w_full();
            if !attachments.is_empty() {
                column = column.child(super::attachments::render_user_attachments(
                    app,
                    &row.id,
                    attachments,
                    theme,
                ));
            }
            if !badges.is_empty() {
                let mut badge_row = rrow()
                    .w_full()
                    .flex_wrap()
                    .justify_end()
                    .items_center()
                    .gap(6.0)
                    .pb(6.0);
                for (bix, badge) in badges.iter().enumerate() {
                    badge_row = badge_row.child(crate::badges::render(
                        format!("{}#badge{bix}", row.id),
                        badge,
                        theme,
                    ));
                }
                column = column.child(badge_row);
            }
            if !prompt.is_empty() {
                column = column.child(
                    rrow().w_full().justify_end().child(
                        el().min_w(0.0)
                            .max_w(MAX_CONTENT_WIDTH * 0.8)
                            .fill(crate::theme::user_bubble_bg(theme.appearance))
                            .radius(Theme::BUBBLE_RADIUS)
                            .px(16.0)
                            .py(10.0)
                            .opacity(if *pending { 0.65 } else { 1.0 })
                            .child(user_bubble_text(prompt, mentions, theme)),
                    ),
                );
            }
            column
        }
        RowKind::Markdown { tree, block_ix } => {
            crate::markdown::render::render_tree_block(tree, *block_ix, false, theme)
                .unwrap_or_else(el)
        }
        RowKind::LiveMarkdown { tree, block_ix } => {
            let timer = frame_stats_enabled().then(Instant::now);
            let rendered = crate::markdown::render::render_tree_block(tree, *block_ix, true, theme)
                .unwrap_or_else(el);
            if let Some(start) = timer {
                record_live_frame_us(start.elapsed().as_micros() as u64);
            }
            rendered
        }
        RowKind::ToolGroup { tools, auto_open } => {
            render_tool_group(app, &row.id, tools, *auto_open, theme)
        }
        RowKind::InputChip { header, resolved } => input_chip(header, *resolved, theme),
        RowKind::ErrorChip { message } => error_chip(message, theme),
    };

    let is_user_row = matches!(row.kind, RowKind::User { .. });
    let hovered = state
        .hovered_entry
        .as_ref()
        .is_some_and(|(_, entry)| entry == &row.entry_id);
    let copied_message = state.copied_message.as_ref() == Some(&row.entry_id);
    let copy_text = row.copy_text.clone();
    let copy_entry = row.entry_id.clone();
    let strip = row.timestamp.map(|ms| {
        let mut strip = rrow().h(32.0).pt(8.0).w_full().items_center().gap(8.0);
        if is_user_row {
            strip = strip.justify_end();
        }
        if hovered {
            strip = strip.child(
                text(format_timestamp(ms, &chrono::Local))
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_muted.alpha(0.55)),
            );
            if let Some(copy_text) = copy_text {
                let entry = copy_entry.clone();
                strip = strip.child(
                    el().size(Theme::SPACE_MD * 2.0, Theme::SPACE_MD * 2.0)
                        .center()
                        .radius(Theme::CONTROL_RADIUS)
                        .hover(theme.ink(0.08))
                        .cursor(rune::Cursor::Pointer)
                        .help("Copy message")
                        .on_press_tx(move |app: &mut App, tx: &mut rune::Tx| {
                            tx.copy(copy_text.clone());
                            app.transcript_state.copied_message = Some(entry.clone());
                        })
                        .child(
                            rune::icon(
                                if copied_message {
                                    &crate::icons::CHECK
                                } else {
                                    &crate::icons::COPY
                                },
                                14.0,
                            )
                            .color(theme.text_muted),
                        ),
                );
            }
        }
        strip
    });

    let row_id = row.id.clone();
    let entry_id = row.entry_id.clone();
    let outer = rrow()
        .w_full()
        .justify_center()
        .pt(top_gap)
        .pb(bottom_pad)
        .px(48.0)
        .on_hover_change(move |app: &mut App, inside, _tx| {
            if inside {
                let next = Some((row_id.clone(), entry_id.clone()));
                if app.transcript_state.hovered_entry != next {
                    app.transcript_state.hovered_entry = next;
                }
            } else if app
                .transcript_state
                .hovered_entry
                .as_ref()
                .is_some_and(|(row, _)| row == &row_id)
            {
                app.transcript_state.hovered_entry = None;
            }
        })
        .child(
            col()
                .w_full()
                .max_w(MAX_CONTENT_WIDTH)
                .min_w(0.0)
                .child(inner)
                .children(strip)
                .children(
                    (show_trailer && last)
                        .then(|| working_trailer(app, theme))
                        .flatten(),
                ),
        );
    outer
}

/// The user bubble's text, with file-mention chips as mono ranges over a
/// violet wash (the same recipe as the composer's pills).
fn user_bubble_text(prompt: &str, mentions: &[super::model::MentionSpan], theme: &Theme) -> El {
    let mut spans = Vec::with_capacity(mentions.len());
    for mention in mentions {
        if mention.range.start < mention.range.end && mention.range.end <= prompt.len() {
            spans.push(
                rune::Span::new(mention.range.start, mention.range.end)
                    .mono()
                    .color(theme.code_text)
                    .bg(theme.code_wash, 2.0),
            );
        }
    }
    if spans.is_empty() {
        text(prompt.to_string())
            .font(TextStyle::new(14.0, 400.0).lh(22.0))
            .color(theme.text)
            .wrap()
    } else {
        rune::rich(prompt.to_string(), spans)
            .font(TextStyle::new(14.0, 400.0).lh(22.0))
            .color(theme.text)
            .wrap()
    }
}

fn render_tool_group(
    app: &App,
    row_id: &str,
    tools: &Arc<Vec<ToolItem>>,
    auto_open: bool,
    theme: &Theme,
) -> El {
    let state = &app.transcript_state;
    let fold = state.folds.get(row_id).copied().unwrap_or_default();
    let collapses = tool_group_collapses(tools);
    let open = !collapses || fold.open.unwrap_or(auto_open);
    let summary = tool_group_summary(tools);

    let mut root = col().w_full();
    if collapses {
        let toggle_id = row_id.to_string();
        let mut header = rrow()
            .key(format!("{row_id}-hdr"))
            .w_full()
            .h(26.0)
            .items_center()
            .gap(8.0)
            .px(4.0)
            .cursor(rune::Cursor::Pointer)
            .on_press(move |app: &mut App| {
                let entry = app
                    .transcript_state
                    .folds
                    .entry(toggle_id.clone())
                    .or_default();
                let current = entry.open.unwrap_or(auto_open);
                entry.from = if current { 1.0 } else { 0.0 };
                entry.open = Some(!current);
                entry.epoch += 1;
                entry.toggled_at = Some(Instant::now());
            })
            .child(
                el().size(18.0, 18.0)
                    .radius(5.0)
                    .fill(theme.ink(0.06))
                    .center()
                    .child(
                        text(if open { "▾" } else { "▸" })
                            .font(TextStyle::new(10.0, 400.0))
                            .color(theme.text_muted.alpha(0.7)),
                    ),
            )
            .child(
                text(summary)
                    .font(TextStyle::new(12.0, 400.0).lh(18.0))
                    .color(theme.text_muted)
                    .truncate()
                    .min_w(0.0),
            );
        header = header.hover(theme.ink(0.04));
        root = root.child(header);
    }

    if open {
        let mut chips = col().w_full().pt(CHIPS_TOP_PAD).gap(CHIP_GAP);
        for (ix, tool) in tools.iter().enumerate() {
            chips = chips.child(render_chip(app, row_id, ix, tool, collapses, theme));
        }
        root = root.child(chips);
    }
    root
}

fn render_chip(
    app: &App,
    row_id: &str,
    ix: usize,
    tool: &ToolItem,
    collapses: bool,
    theme: &Theme,
) -> El {
    let state = &app.transcript_state;
    if is_spawn_link(tool) {
        return subagent_chip(tool, collapses, theme);
    }
    let detail = state.effective_detail(tool);
    let invocation = tool.invocation.clone().filter(|_| !is_spawn_link(tool));
    if detail.is_none() && invocation.is_none() {
        return tool_chip(tool, collapses, theme);
    }

    let detail_key = format!("{row_id}#d{ix}");
    let detail_fold = state
        .tool_details
        .get(&detail_key)
        .copied()
        .unwrap_or_default();
    let default_open = tool.is_thought && !tool.resolved;
    let detail_open = detail_fold.open.unwrap_or(default_open);
    let affordance = state.affordance_for(tool);
    let highlight = if detail_open {
        detail.as_deref().and_then(|detail| {
            super::highlights::tool_diff_highlight_for(
                &mut state.highlights.borrow_mut(),
                row_id,
                ix,
                detail,
            )
        })
    } else {
        None
    };

    // Header toggles the detail card.
    let toggle_key = detail_key.clone();
    let mut card = col()
        .min_w(0.0)
        .grow(1.0)
        .radius(9.0)
        .border(1.0, theme.hairline(0.07))
        .fill(theme.ink(0.03))
        .clip_rounded(9.0)
        .my((CHIP_HEIGHT - CHIP_CARD_HEIGHT) / 2.0)
        .child(
            rrow()
                .key(detail_key.clone())
                .h(CHIP_HEADER_HEIGHT)
                .w_full()
                .items_center()
                .cursor(rune::Cursor::Pointer)
                .on_press(move |app: &mut App| {
                    let entry = app
                        .transcript_state
                        .tool_detail_fold_mut(toggle_key.clone());
                    let current = entry.open.unwrap_or(default_open);
                    entry.from = if current { 1.0 } else { 0.0 };
                    entry.open = Some(!current);
                    entry.epoch += 1;
                    entry.toggled_at = Some(Instant::now());
                })
                .child(chip_header(tool, detail_open, theme)),
        );

    if detail_open {
        let separator = |theme: &Theme| el().h(1.0).w_full().fill(theme.hairline(0.06));
        if let Some(invocation) = invocation.as_deref() {
            card = card
                .child(separator(theme))
                .child(detail_body(invocation, None, theme));
        }
        if let Some(detail) = detail.as_deref() {
            card = card
                .child(separator(theme))
                .child(detail_body(detail, highlight, theme));
        }
        if let Some(affordance) = affordance {
            let loading = matches!(
                state.blob_details.get(&affordance.blob_ref),
                Some(BlobFetch::Loading)
            );
            let mut blob_row = rrow()
                .key(format!("{detail_key}-blob"))
                .h(super::chips::BLOB_AFFORDANCE_HEIGHT)
                .w_full()
                .px(12.0)
                .items_center()
                .child(
                    text(affordance.label.clone())
                        .font(TextStyle::new(10.5, 400.0))
                        .color(theme.text_faint)
                        .truncate()
                        .min_w(0.0),
                );
            if !loading {
                let blob_ref = affordance.blob_ref.clone();
                blob_row = blob_row
                    .cursor(rune::Cursor::Pointer)
                    .hover(theme.ink(0.04))
                    .on_press(move |app: &mut App| {
                        super::request_blob(app, blob_ref.clone());
                    });
            }
            card = card.child(blob_row);
        }
    }

    let card_ml = if collapses { 12.0 } else { 0.0 };
    let mut line = rrow().w_full().shrink().child(el().w(card_ml).shrink());
    if collapses {
        line = line.child(el().w(1.0).self_stretch().fill(theme.ink(0.08)));
    }
    line.child(card)
}

/// The working loader appended under the last row while a run is live.
fn working_trailer(app: &App, theme: &Theme) -> Option<El> {
    let now = chrono::Utc::now();
    if let Some(doc_id) = &app.transcript_state.doc_override {
        if !app.transcript_state.doc_live {
            return None;
        }
        let last = app.sub_transcript(doc_id).last()?;
        let live = last.status == Some(hearth_doc::MessageStatus::Streaming)
            || last.role == hearth_doc::MessageRole::User;
        if !live {
            return None;
        }
        let elapsed = (now.timestamp_millis() - last.created_at).max(0) / 1000;
        return Some(trailer_line(
            super::model::flavour_word(super::model::flavour_seed(doc_id), elapsed),
            elapsed,
            false,
            theme,
        ));
    }
    let chat_id = app.selected_chat.clone()?;
    if app.indicator_for(&chat_id) != Some(crate::state::Indicator::Working) {
        return None;
    }
    let turn_started = app
        .session_for(&chat_id)
        .and_then(|session| session.started_at);
    let sending = sending_bridge(None, turn_started);
    let elapsed = turn_started
        .map(|start| now.signed_duration_since(start).num_seconds().max(0))
        .unwrap_or(0);
    let word = if sending {
        "Sending"
    } else {
        super::model::flavour_word(super::model::flavour_seed(&chat_id), elapsed)
    };
    Some(trailer_line(word, elapsed, sending, theme))
}

#[allow(dead_code)]
fn trailer_line(word: &str, elapsed: i64, sending: bool, theme: &Theme) -> El {
    let mut line = rrow()
        .w_full()
        .items_center()
        .gap(Theme::SPACE_SM)
        .pt(Theme::SPACE_LG)
        .child(el().size(8.0, 8.0).radius(4.0).fill(theme.busy))
        .child(
            text(if sending {
                "Sending…".to_string()
            } else {
                format!("{word}…")
            })
            .font(TextStyle::new(12.0, 400.0))
            .color(theme.text_muted),
        );
    if !sending {
        line = line.child(
            text(format_elapsed(elapsed))
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_faint),
        );
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::render::MD_BLOCK_GAP;
    use crate::transcript::model::{ToolDetail, chips_height, detail_height};

    #[test]
    fn row_gaps_open_a_turn_and_separate_blocks() {
        assert_eq!(MD_BLOCK_GAP, 12.0);
        assert!(MAX_CONTENT_WIDTH > 0.0);
    }

    #[test]
    fn detail_height_is_analytic() {
        let detail = ToolDetail::Output {
            lines: vec!["a".into(), "b".into()],
            truncated_by: 0,
        };
        assert!(detail_height(&detail) > 0.0);
        assert_eq!(chips_height(0), 0.0);
    }
}
