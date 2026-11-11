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
///
/// Rows come from the per-entry fingerprint cache ([`super::TranscriptState`]'s
/// `rows_for_doc`): a subagent watch re-renders every frame, and re-parsing
/// each entry per frame was the port's hottest path.
pub fn render_subagent(app: &App, theme: &Theme, doc_id: &str) -> El {
    let entries = app.sub_transcript(doc_id);
    let mut rows: Vec<Row> = Vec::new();
    for entry in entries {
        rows.extend(app.transcript_state.rows_for_doc(doc_id, entry));
    }
    let version = super::row_set_version(&rows);
    render_rows(app, theme, Arc::new(rows), true, version)
}

/// The transcript's floating lightbox, for the shell's overlay layer
/// (`rune_kit::root(body, layer)`). The shell wires this next to
/// `composer::popups`; [`render_rows`] also mounts it in-tree as a fallback
/// while that line is pending (see docs/rune-port.md).
pub fn lightbox_overlay(app: &App, theme: &Theme) -> Option<El> {
    let preview = app.transcript_state.attachment_preview.clone()?;
    Some(crate::attachments::lightbox(
        theme,
        &preview,
        |app: &mut App| {
            app.transcript_state.attachment_preview = None;
            // The shell's poll hands focus back to the composer on the next frame.
            app.shell.focus_composer = true;
        },
    ))
}

fn apply_viewport_layout(app: &mut App, rect: [f32; 4], tx: &mut rune::Tx) {
    let next = [rect[2], rect[3]];
    if next[0] > 0.0 && next[1] > 0.0 && app.transcript_state.viewport_size.get() != next {
        app.transcript_state.viewport_size.set(next);
    }
    app.transcript_state.apply_layout_requests(false, tx);
}

fn apply_rows_layout(app: &mut App, rect: [f32; 4], tx: &mut rune::Tx) {
    app.transcript_state.apply_rows_layout(rect[3], tx);
}

fn render_rows(app: &App, theme: &Theme, rows: Arc<Vec<Row>>, subagent: bool, version: u64) -> El {
    let state = &app.transcript_state;
    // Read the measured viewport before building rows: the own-turn
    // reservation (and the rail's tick capacity) depend on it.
    let measured = rune::memo::layout(VIEWPORT_KEY)
        .map(|rect| [rect[2], rect[3]])
        .filter(|size| size[0] > 0.0 && size[1] > 0.0)
        .unwrap_or_else(|| state.viewport_size.get());
    if measured[0] > 0.0 && measured[1] > 0.0 && state.viewport_size.get() != measured {
        state.viewport_size.set(measured);
    }
    if !subagent {
        state.update_own_turn(&rows);
    }
    let runway = state
        .own_turn
        .borrow()
        .as_ref()
        .filter(|anchor| {
            state
                .rows
                .borrow()
                .iter()
                .any(|candidate| candidate.entry_id == anchor.message_id)
        })
        .map_or(0.0, |anchor| anchor.runway);
    let bottom_space = state.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND + 8.0 + runway;
    let count = rows.len();
    let has_runway = bottom_space > 0.5;
    let total = count + usize::from(has_runway);
    let rail_enabled =
        !subagent && state.rail_enabled && rail::rail_visible(app.shell.viewport_width);
    let key_rows = rows.clone();
    let build_rows = rows.clone();
    let build_theme = theme.clone();

    // Keyed by the attached chat: a chat switch remounts the stack, whose
    // first layout is the commit the restore request is applied on (an
    // unchanged element only reports real moves/resizes).
    let chat_key = state.chat_id.borrow().clone().unwrap_or_default();
    let growth = col()
        .key(format!("app/transcript/rows/{chat_key}"))
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows_version(version)
        .virtual_rows(
            total,
            ESTIMATED_ROW_HEIGHT,
            move |_app: &App, ix| {
                Cow::Owned(
                    key_rows
                        .get(ix)
                        .map(|row| row.id.clone())
                        .unwrap_or_else(|| "app/transcript/runway".to_string()),
                )
            },
            move |app: &App, ix| {
                let Some(row) = build_rows.get(ix) else {
                    // The trailing runway row: the bottom clearance and the
                    // own-turn reservation as REAL scroll content (a growth
                    // stack must be its viewport's last child, so a spacer
                    // sibling is not allowed).
                    return el().key("app/transcript/runway").w_full().h(bottom_space);
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
        .on_visible_rows(move |app: &mut App, range| {
            // True when the rail's active tick moves: the rail reads this.
            let state = &app.transcript_state;
            let start = range.start.min(count.saturating_sub(1));
            if state.visible_start.get() == start {
                return false;
            }
            state.visible_start.set(start);
            true
        })
        // Growth changes (a replay splice, a send's runway, streamed text)
        // are where the restore and own-turn holds are applied: a Tx only
        // exists inside a handler.
        .on_layout_tx(|app: &mut App, rect, tx| apply_rows_layout(app, rect, tx));

    let viewport = col()
        .key(VIEWPORT_KEY)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_edge_fade(Theme::TRANSCRIPT_FADE_BAND)
        .on_at_bottom(|app: &mut App, at_bottom| {
            app.transcript_state.pinned.set(at_bottom);
            if at_bottom {
                app.transcript_state.last_scroll_distance.set(0.0);
            }
        })
        .on_far_from_bottom(SCROLL_BUTTON_THRESHOLD_PX, |app: &mut App, far| {
            app.transcript_state.show_jump_button.set(far);
        })
        .on_scroll_offset({
            let scroll_live = state.scroll_offset.clone();
            move |at| {
                // Live-only: records the offset AND the distance from the end
                // for the next chat switch's snapshot, without a view run.
                let distance = (at.max[1] - at.offset[1]).max(0.0);
                let _ = scroll_live.set([at.offset[1], distance]);
            }
        })
        .on_layout_tx(|app: &mut App, rect, tx| apply_viewport_layout(app, rect, tx))
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
        root = root.child(rail::render(
            app,
            theme,
            &pairs,
            active,
            state.viewport_height(),
        ));
    }

    // The preview is mounted once, by the shell's overlay layer (see
    // `lightbox_overlay` next to `composer::popups`).
    root
}

fn rail_pairs(app: &App, rows: &[Row]) -> Vec<(RailTick, String)> {
    let (entries, echoes) = match &app.transcript_state.doc_override {
        Some(doc_id) => (app.sub_transcript(doc_id).to_vec(), Vec::new()),
        // Optimistic echoes are prompts too: the rail must show the just-sent
        // turn before its doc frame lands.
        None => (app.transcript.clone(), app.pending_echoes().to_vec()),
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
    // The bottom clearance and the own-turn runway are a trailing spacer
    // AFTER the virtual list: padding on the last row is not counted in the
    // virtual list's content height, so the viewport could not scroll far
    // enough to hold the prompt at its inset (the pad rendered but the scroll
    // range ignored it).
    let bottom_pad = 0.0;

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
                            .clip_soft(0.0)
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
    let copied_message = state.copied_message.as_ref() == Some(&row.entry_id);
    let copy_text = row.copy_text.clone();
    let copy_entry = row.entry_id.clone();
    let strip = row.timestamp.map(|ms| {
        // A reserved 32pt lane: the timestamp/copy affordance is always laid
        // out and only its opacity follows the row's hover group, so the
        // pointer crossing rows never commits the view.
        let mut strip = rrow()
            .h(32.0)
            .pt(8.0)
            .w_full()
            .items_center()
            .gap(8.0)
            .group_opacity(0.0, 1.0);
        if is_user_row {
            strip = strip.justify_end();
        }
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
        strip
    });

    rrow()
        .w_full()
        .justify_center()
        .hover_group()
        .pt(top_gap)
        .pb(bottom_pad)
        .px(48.0)
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
        )
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
            .w_full()
            .min_w(0.0)
            .wrap_anywhere()
    } else {
        rune::rich(prompt.to_string(), spans)
            .font(TextStyle::new(14.0, 400.0).lh(22.0))
            .color(theme.text)
            .w_full()
            .min_w(0.0)
            .wrap_anywhere()
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
                        rune::icon(
                            if open {
                                &crate::icons::ALT_ARROW_DOWN
                            } else {
                                &crate::icons::ALT_ARROW_RIGHT
                            },
                            10.0,
                        )
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
///
/// Three states ride this line, in priority order:
/// - **Not delivered**: the send sat unadopted past [`crate::state::UNDELIVERED_GRACE_MS`].
///   The line becomes the retry affordance (re-kicks `RetryDelivery`).
/// - **Queued**: the delivery path is degraded (offline host, relay down) — say
///   so instead of faking progress.
/// - **Sending**: the send → turn bridge, before the session row restarts its
///   `started_at`. No timer: it would count the round-trip and restart.
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
            false,
            theme,
        ));
    }
    let chat_id = app.selected_chat.clone()?;
    // Failed-send state first: past the grace window the trailer IS the retry
    // affordance, whatever the indicator fell back to.
    if app.send_undelivered(&chat_id, now) {
        return Some(retry_line(theme));
    }
    let chat = app.chats.iter().find(|chat| chat.id == chat_id)?;
    if app.display_status_for(chat, now) != hearth_proto::ChatIndicator::Working {
        return None;
    }
    let turn_started = app
        .session_for(&chat_id)
        .and_then(|session| session.started_at);
    let sending = sending_bridge(app.pending_send_started(&chat_id, now), turn_started);
    // Degraded delivery path: the send is a durable local write waiting on
    // connectivity — say so instead of faking progress.
    let queued = sending && app.chat_delivery_degraded(&chat_id);
    let elapsed = turn_started
        .map(|start| now.signed_duration_since(start).num_seconds().max(0))
        .unwrap_or(0);
    let word = if queued {
        "Queued — will send automatically"
    } else if sending {
        "Sending"
    } else {
        super::model::flavour_word(super::model::flavour_seed(&chat_id), elapsed)
    };
    Some(trailer_line(word, elapsed, sending, queued, theme))
}

/// Re-kick every delivery road engine-side and restart the grace clock so the
/// trailer returns to Sending/Queued while the retry runs.
fn retry_delivery(app: &mut App) {
    let Some(chat_id) = app.selected_chat.clone() else {
        return;
    };
    app.retry_pending_send(&chat_id, chrono::Utc::now());
    let Some(engine) = app.engine.clone() else {
        return;
    };
    app.bridge.spawn(async move {
        let params = serde_json::json!({ "chatId": chat_id });
        if let Err(err) = engine
            .client
            .call(hearth_rpc::methods::RETRY_DELIVERY, params)
            .await
        {
            tracing::warn!(error = %err, "delivery retry RPC failed");
        }
    });
}

fn retry_line(theme: &Theme) -> El {
    rrow()
        .w_full()
        .items_center()
        .gap(Theme::SPACE_SM)
        .pt(Theme::SPACE_LG)
        .cursor(rune::Cursor::Pointer)
        .on_press(|app: &mut App| retry_delivery(app))
        .child(
            text("Not delivered — click to retry")
                .font(TextStyle::new(12.0, 400.0))
                .color(theme.danger),
        )
}

fn trailer_line(word: &str, elapsed: i64, sending: bool, queued: bool, theme: &Theme) -> El {
    let mut line = rrow()
        .w_full()
        .items_center()
        .gap(Theme::SPACE_SM)
        .pt(Theme::SPACE_LG)
        .child(el().size(8.0, 8.0).radius(4.0).fill(theme.busy))
        .child(
            text(if queued {
                word.to_string()
            } else if sending {
                "Sending…".to_string()
            } else {
                format!("{word}…")
            })
            .font(TextStyle::new(12.0, 400.0))
            .color(if queued {
                theme.warning
            } else {
                theme.text_muted
            }),
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
        const { assert!(MAX_CONTENT_WIDTH > 0.0) };
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
