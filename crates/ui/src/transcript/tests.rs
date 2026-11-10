use super::chips::{
    CHIP_GAP, CHIP_HEIGHT, CHIPS_TOP_PAD, FoldState, SUBAGENT_TITLE_MAX, subagent_tab_title,
};
use super::highlights::{HighlightStore, MAX_HIGHLIGHT_ENTRIES};
use super::model::{
    CALL_WRAP_COLS, OUTPUT_DETAIL_MAX_LINES, Row, RowKind, THOUGHT_WRAP_COLS, ToolDetail, ToolItem,
    assistant_copy_text, call_block, chips_height, flavour_seed, flavour_word, format_elapsed,
    format_timestamp, is_agent_call, is_agent_tool, is_spawn_link, sending_bridge, tool_detail,
    tool_group_collapses, tool_group_summary, wrap_thought_lines,
};
use super::rows::{ParseOutcome, diff_rows, parse_for_row, rows_for_entry, top_gap_for};
use super::spring::{AT_BOTTOM_PX, SPRING_CHASE_MAX_LEAD, StickSpring, should_anchor_live_stream};
use super::viewport::{
    MAX_SAVED_VIEWPORTS, OwnTurnAnchor, SavedViewport, SavedViewportCache, TranscriptReplayState,
    ViewportAnchor, ViewportFinalizeToken, own_turn_reservation, selection_scroll_step,
};
use super::{
    BlobFetch, MAX_BLOB_DETAILS, MAX_TOOL_DETAILS, Transcript, prune_blob_cache, prune_tool_details,
};
use crate::changes::LineKind;
use crate::markdown::parser::{BlockTree, parse_full};
use crate::markdown::render;
use crate::syntax_cache::DocumentHighlightKey;
use crate::theme::Theme;
use chrono::FixedOffset;
use gpui::{Bounds, ListAlignment, ListOffset, ListState, SharedString, Task, px};
use hearth_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus};
use hearth_proto::ToolCall;
use hearth_proto::view::{single_line, tool_chip_content};
use hearth_syntax::LanguageId;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn selection_scroll_ramps_at_viewport_edges() {
    let bounds = Bounds::new(
        gpui::point(px(10.0), px(20.0)),
        gpui::size(px(300.0), px(200.0)),
    );
    assert_eq!(
        selection_scroll_step(bounds, gpui::point(px(20.0), px(120.0))),
        0.0
    );
    assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(20.0))) < 0.0);
    assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0))) > 0.0);
    assert!(
        selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0)))
            > selection_scroll_step(bounds, gpui::point(px(20.0), px(200.0)))
    );
}

// ---- streaming parse wiring (the transcript side, not the parser) ----

#[test]
fn live_row_parse_work_is_bounded_per_commit() {
    // Drive the EXACT wiring `rows_for` uses (`parse_for_row`) with the
    // prefix-extending commit snapshots the doc watch delivers, and prove
    // the per-commit parse work stays O(reparsed tail): a full-reparse
    // wiring would feed ~N/2 × final_len bytes through the parser across N
    // commits; the incremental path stays within a small multiple of the
    // final length regardless of N.
    let mut live_parsers = HashMap::new();
    let mut tree_cache = HashMap::new();
    let paragraph = "A paragraph of streaming prose that keeps arriving.\n\n";
    let commits = 120usize;
    let mut text = String::new();
    let mut total_parsed = 0usize;
    for i in 0..commits {
        // Each commit appends ~half a paragraph (crosses block boundaries).
        let chunk = &paragraph[..paragraph.len() / 2];
        text.push_str(if i % 2 == 0 {
            chunk
        } else {
            &paragraph[paragraph.len() / 2..]
        });
        let (tree, outcome) =
            parse_for_row(true, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert!(!tree.blocks.is_empty());
        let ParseOutcome::Incremental {
            parsed_bytes,
            stable_prefix_blocks,
        } = outcome
        else {
            panic!("streaming commit must take the incremental path");
        };
        total_parsed += parsed_bytes;
        // Per commit: never a full reparse once the doc has grown past the
        // tail window (last two complete blocks + the partial trailing
        // one + the delta ≤ 3 paragraphs here).
        assert!(
            parsed_bytes <= 3 * paragraph.len(),
            "commit {i}: parsed {parsed_bytes} bytes — not bounded by the tail window"
        );
        // The stable prefix grows with the doc — settled blocks are never
        // re-touched (this is what keeps render caches valid).
        assert!(stable_prefix_blocks + 2 >= tree.blocks.len().saturating_sub(1));
    }
    // Across the whole stream: work is commits × O(tail), an order of
    // magnitude under the ~commits × len/2 a full-reparse wiring costs.
    let final_len = text.len();
    let full_reparse_cost = commits * final_len / 2;
    assert!(total_parsed <= commits * 3 * paragraph.len());
    assert!(
        total_parsed * 10 < full_reparse_cost,
        "total parsed {total_parsed} vs full-reparse ~{full_reparse_cost}"
    );

    // Live→complete handoff: the completed part adopts the live parser's
    // exact tree without parsing a single byte.
    let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
    assert_eq!(outcome, ParseOutcome::Handoff);
    // And the settled cache serves repeats with no work at all.
    let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
    assert_eq!(outcome, ParseOutcome::Cached);
}

// ---- stick-to-bottom spring ----

#[test]
fn spring_converges_to_a_fixed_target() {
    let mut spring = StickSpring::new();
    let target = 400.0;
    let mut pos = 0.0;
    let mut frames = 0;
    while pos < target && frames < 600 {
        pos = spring.step(pos, target, 1.0);
        frames += 1;
    }
    assert_eq!(pos, target, "spring must land exactly on the target");
    assert!(
        frames < 300,
        "400px should converge within 5s of frames, took {frames}"
    );
    // Once landed it stays landed (and idles out).
    for _ in 0..120 {
        pos = spring.step(pos, target, 1.0);
        assert_eq!(pos, target);
    }
    assert!(spring.is_idle(), "no residual motion at rest");
}

#[test]
fn spring_never_overshoots_or_oscillates() {
    let mut spring = StickSpring::new();
    let target = 250.0;
    let mut pos = 0.0;
    let mut last = pos;
    for _ in 0..600 {
        pos = spring.step(pos, target, 1.0);
        assert!(pos <= target, "overshoot: {pos} > {target}");
        assert!(
            pos >= last - 1e-3,
            "oscillation: position moved backwards {last} -> {pos}"
        );
        last = pos;
    }
    assert_eq!(pos, target);
}

#[test]
fn spring_feed_forward_tracks_constant_growth() {
    // Target grows 2px/frame (≈120px/s — a typical stream). After warmup
    // the EMA feed-forward must carry the viewport at the same rate with a
    // bounded, stable lag — a glide, not 0,0,0,Npx steps.
    let growth = 2.0;
    let mut spring = StickSpring::new();
    let mut target = 600.0;
    let mut pos = 600.0;
    let mut deltas: Vec<f32> = Vec::new();
    for frame in 0..400 {
        target += growth;
        let next = spring.step(pos, target, 1.0);
        if frame >= 200 {
            deltas.push(next - pos);
        }
        pos = next;
    }
    // Steady state: per-frame movement ≈ growth rate…
    let mean = deltas.iter().sum::<f32>() / deltas.len() as f32;
    assert!(
        (mean - growth).abs() < 0.2,
        "steady-state speed {mean} should track growth {growth}"
    );
    // …with no stepping (every frame moves, none jumps).
    for d in &deltas {
        assert!(*d > 0.0, "viewport stalled mid-stream");
        assert!(*d < growth * 3.0, "viewport jumped: {d}px in one frame");
    }
    // The EMA growth estimate itself has locked on.
    assert!((spring.target_vel() - growth).abs() < 0.3);
    // Lag stays bounded by the chase lead.
    assert!(target - pos <= SPRING_CHASE_MAX_LEAD + growth);
}

#[test]
fn spring_feed_forward_resets_when_target_shrinks() {
    let mut spring = StickSpring::new();
    let mut pos = 0.0;
    for i in 1..=50 {
        pos = spring.step(pos, 100.0 + i as f32 * 4.0, 1.0);
    }
    assert!(spring.target_vel() > 1.0);
    // A collapse (target shrinks by more than 1px) drops the estimate.
    spring.step(pos.min(120.0), 120.0, 1.0);
    assert_eq!(spring.target_vel(), 0.0);
}

#[test]
fn spring_catchup_frames_glide_instead_of_teleporting() {
    // A 5-frame hitch advances roughly as far as 5 single steps would —
    // sub-stepped, still clamped at the target.
    let target = 300.0;
    let mut a = StickSpring::new();
    let mut pos_a = 0.0;
    for _ in 0..5 {
        pos_a = a.step(pos_a, target, 1.0);
    }
    let mut b = StickSpring::new();
    let pos_b = b.step(0.0, target, 5.0);
    assert!((pos_a - pos_b).abs() < 1.0, "{pos_a} vs {pos_b}");
    assert!(pos_b <= target);
}

#[test]
fn restick_is_direction_aware() {
    // Scrolling away from the bottom never resticks, even inside the band
    // (a 20px wheel notch from the pinned bottom must break the pin).
    assert!(!Transcript::should_restick(20.0, 0.0));
    assert!(!Transcript::should_restick(69.0, 30.0));
    // Returning toward the bottom resticks once inside the 70px band…
    assert!(Transcript::should_restick(69.0, 120.0));
    assert!(Transcript::should_restick(0.0, 30.0));
    // …but not while still outside it.
    assert!(!Transcript::should_restick(200.0, 300.0));
    // No movement — leave the pin alone.
    assert!(!Transcript::should_restick(50.0, 50.0));
}

#[test]
fn only_a_stream_at_the_bottom_gets_a_hard_end_anchor() {
    assert!(should_anchor_live_stream(true, 0.0, true));
    assert!(should_anchor_live_stream(true, AT_BOTTOM_PX, true));

    // A user who has moved away from the end keeps control of the
    // viewport, even if the transcript is still streaming.
    assert!(!should_anchor_live_stream(true, AT_BOTTOM_PX + 0.1, true));
    assert!(!should_anchor_live_stream(false, 0.0, true));

    // Ordinary transcript updates retain the existing spring behavior.
    assert!(!should_anchor_live_stream(true, 0.0, false));
}

#[test]
fn own_turn_reservation_is_a_min_height_for_the_turn() {
    let usable = 700.0;
    // A short turn reserves the rest of the usable viewport below it.
    assert_eq!(own_turn_reservation(usable, 100.0), 600.0);
    // Growth consumes the reservation 1:1 — total held height is stable.
    assert_eq!(own_turn_reservation(usable, 450.0), 250.0);
    // At/past the fill line nothing is reserved (bottom spring takes
    // over with no height jump).
    assert_eq!(own_turn_reservation(usable, 700.0), 0.0);
    assert_eq!(own_turn_reservation(usable, 1_200.0), 0.0);
}

fn viewport_row(id: &str, entry_id: &str) -> Row {
    Row {
        id: id.into(),
        version: 0,
        turn_start: true,
        kind: RowKind::ErrorChip {
            message: SharedString::default(),
        },
        entry_id: entry_id.into(),
        timestamp: None,
        copy_text: None,
    }
}

#[test]
fn viewport_anchor_tracks_a_stable_row_across_replay() {
    let rows = vec![
        viewport_row("a", "entry-a"),
        viewport_row("b", "entry-b"),
        viewport_row("c", "entry-c"),
    ];
    let anchor = ViewportAnchor::capture(
        &rows,
        ListOffset {
            item_ix: 1,
            offset_in_item: px(23.0),
        },
    )
    .expect("visible row");

    let replay = vec![
        viewport_row("new", "entry-new"),
        viewport_row("a", "entry-a"),
        viewport_row("b", "entry-b"),
        viewport_row("c", "entry-c"),
    ];
    let restored = anchor.resolve(&replay).expect("restored row");
    assert_eq!(restored.item_ix, 2);
    assert_eq!(restored.offset_in_item, px(23.0));
}

#[test]
fn viewport_anchor_has_entry_and_index_fallbacks() {
    let rows = vec![
        viewport_row("a", "entry-a"),
        viewport_row("b", "entry-b"),
        viewport_row("old-block", "entry-c"),
    ];
    let anchor = ViewportAnchor::capture(
        &rows,
        ListOffset {
            item_ix: 2,
            offset_in_item: px(31.0),
        },
    )
    .expect("visible row");

    let reshaped = vec![
        viewport_row("a", "entry-a"),
        viewport_row("b", "entry-b"),
        viewport_row("inserted", "entry-new"),
        viewport_row("new-block", "entry-c"),
    ];
    let same_entry = anchor.resolve(&reshaped).expect("entry fallback");
    assert_eq!(same_entry.item_ix, 3);
    assert_eq!(same_entry.offset_in_item, px(0.0));

    let entry_removed = vec![viewport_row("a", "entry-a"), viewport_row("b", "entry-b")];
    let clamped = anchor.resolve(&entry_removed).expect("index fallback");
    assert_eq!(clamped.item_ix, 1);
    assert_eq!(clamped.offset_in_item, px(0.0));
}

#[test]
fn optimistic_echo_cannot_consume_a_historical_viewport_before_replay() {
    let history = vec![viewport_row("historical", "historical-entry")];
    let saved = SavedViewport::capture(&history, ListOffset::default(), false, 480.0, None)
        .expect("historical viewport");
    let echo_only = vec![viewport_row("echo", "echo-entry")];

    assert!(
        saved.resolve(&echo_only, false).is_none(),
        "an unrelated echo is not an authoritative index fallback"
    );
    assert_eq!(
        saved
            .resolve(&echo_only, true)
            .expect("populated replay may use an index fallback")
            .offset
            .item_ix,
        0
    );
    assert!(TranscriptReplayState::Empty.authoritative_empty());
    assert!(!TranscriptReplayState::Empty.allows_fallback());
    assert!(!TranscriptReplayState::Pending.allows_fallback());
    assert!(TranscriptReplayState::Populated.allows_fallback());

    let echo_viewport = SavedViewport::capture(&echo_only, ListOffset::default(), false, 0.0, None)
        .expect("echo viewport");
    assert!(
        echo_viewport.resolve(&echo_only, false).is_some(),
        "the exact optimistic row is safe before replay"
    );
}

#[test]
fn saved_viewport_preserves_and_releases_an_active_turn_runway() {
    let rows = vec![viewport_row("prompt", "prompt")];
    let own_turn = OwnTurnAnchor {
        chat_id: "chat-a".into(),
        message_id: "prompt".into(),
        runway: 640.0,
        held: true,
        positioned: true,
        seen_prompt: true,
    };
    let saved = SavedViewport::capture(
        &rows,
        ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        },
        false,
        0.0,
        Some(&own_turn),
    )
    .expect("active chat viewport");
    let SavedViewport::Anchored {
        own_turn: Some(saved_turn),
        ..
    } = &saved
    else {
        panic!("an active turn must keep its runway with the viewport");
    };
    assert_eq!(saved_turn.runway, 640.0);
    assert!(saved_turn.held);
    assert!(saved_turn.positioned);

    let restored = saved
        .resolve(&rows, false)
        .expect("exact queued echo survives an empty replay");
    let restored_turn = restored.own_turn.expect("valid restored runway");
    assert_eq!(restored_turn.runway, 640.0);
    assert!(!restored_turn.held);
    assert!(!restored_turn.positioned);
    assert!(restored_turn.seen_prompt);

    let list_state = ListState::new(rows.len(), ListAlignment::Bottom, px(0.0));
    list_state.reset(0);
    list_state.splice(0..0, rows.len());
    list_state.scroll_to(restored.offset);
    assert_eq!(list_state.logical_scroll_top().item_ix, 0);
    assert_eq!(list_state.logical_scroll_top().offset_in_item, px(0.0));

    assert!(
        SavedViewport::capture(&[], ListOffset::default(), false, 0.0, Some(&own_turn)).is_none(),
        "an empty rapid-switch replay must not overwrite the older snapshot"
    );
}

#[test]
fn own_turn_waits_for_its_first_echo_then_retires_if_it_disappears() {
    let mut turn = OwnTurnAnchor {
        chat_id: "chat-a".into(),
        message_id: "prompt".into(),
        runway: 0.0,
        held: true,
        positioned: false,
        seen_prompt: false,
    };

    assert!(turn.observe_prompt(false), "fresh send waits one state gap");
    assert!(turn.observe_prompt(true), "echo activates the runway");
    assert!(turn.seen_prompt);
    assert!(
        !turn.observe_prompt(false),
        "failed echo retires the activated runway"
    );
}

#[test]
fn restored_viewport_discards_a_failed_optimistic_turn() {
    let outgoing = vec![viewport_row("prompt", "prompt")];
    let own_turn = OwnTurnAnchor {
        chat_id: "chat-a".into(),
        message_id: "prompt".into(),
        runway: 640.0,
        held: true,
        positioned: true,
        seen_prompt: true,
    };
    let saved = SavedViewport::capture(
        &outgoing,
        ListOffset::default(),
        false,
        420.0,
        Some(&own_turn),
    )
    .expect("outgoing viewport");

    // The failed echo vanished while A was hidden. The ordinary viewport
    // still restores by index, but no stale runway may intercept jump.
    let replay = vec![viewport_row("older", "older")];
    let restored = saved.resolve(&replay, true).expect("index fallback");
    assert!(restored.own_turn.is_none());
    assert_eq!(restored.offset.item_ix, 0);
    assert_eq!(restored.distance_from_bottom, 420.0);
}

#[test]
fn pinned_viewports_follow_tail_and_the_cache_is_bounded() {
    let rows = vec![viewport_row("row", "entry")];
    let pinned = SavedViewport::capture(&rows, ListOffset::default(), true, 999.0, None)
        .expect("pinned viewport");
    assert!(matches!(pinned, SavedViewport::FollowTail));

    let mut cache = SavedViewportCache::default();
    for ix in 0..MAX_SAVED_VIEWPORTS + 8 {
        cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
    }
    assert_eq!(cache.len(), MAX_SAVED_VIEWPORTS);
    assert!(cache.get_cloned_and_touch("chat-0").is_none());
    assert!(
        cache
            .get_cloned_and_touch(&format!("chat-{}", MAX_SAVED_VIEWPORTS + 7))
            .is_some()
    );
}

#[test]
fn reopening_the_oldest_cached_chat_protects_it_from_the_next_eviction() {
    let mut cache = SavedViewportCache::default();
    for ix in 0..MAX_SAVED_VIEWPORTS {
        cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
    }

    assert!(cache.get_cloned_and_touch("chat-0").is_some());
    cache.insert("outgoing-new".into(), SavedViewport::FollowTail);

    assert!(cache.by_chat.contains_key("chat-0"));
    assert!(!cache.by_chat.contains_key("chat-1"));
    assert!(cache.by_chat.contains_key("outgoing-new"));
}

#[test]
fn viewport_finalization_waits_for_current_generation_and_stable_layout() {
    let token = ViewportFinalizeToken {
        generation: 7,
        layout_revision: 11,
    };
    assert!(token.still_current(7));
    assert!(!token.still_current(8));
    assert!(token.layout_settled(11));
    assert!(!token.layout_settled(12));
}

fn parse(_: &str, text: &str) -> Arc<BlockTree> {
    Arc::new(parse_full(text))
}

fn assistant(id: &str, status: MessageStatus, parts: Vec<MessagePart>) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role: MessageRole::Assistant,
        parts,
        created_at: 0,
        device_id: "dev".into(),
        status: Some(status),
        continuation_of: None,
    }
}

fn text_part(id: &str, text: &str) -> MessagePart {
    MessagePart::Text {
        id: id.into(),
        text: text.into(),
    }
}

fn reasoning_part(id: &str, text: &str) -> MessagePart {
    MessagePart::Reasoning {
        id: id.into(),
        text: text.into(),
    }
}

#[test]
fn reasoning_joins_the_tool_group_accordion() {
    // Thought → tool → thought → tool folds into ONE group row (user
    // request: the thought process lives inside the combined accordion),
    // and the collapsed summary names the thinking.
    let entry = assistant(
        "a1",
        MessageStatus::Complete,
        vec![
            reasoning_part("r0", "planning the first step"),
            tool_part("t1", "ls"),
            reasoning_part("r2", "now the second step"),
            tool_part("t3", "pwd"),
        ],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    assert_eq!(rows.len(), 1, "one combined accordion row");
    let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
        panic!("expected a tool group");
    };
    assert_eq!(tools.len(), 4);
    assert!(tools[0].is_thought && tools[2].is_thought);
    assert!(!tools[1].is_thought && !tools[3].is_thought);
    // Thought chips carry their text as an output-style detail with an
    // ANALYTIC height, so the group's fold tween covers them.
    assert!(matches!(
        tools[0].detail.as_deref(),
        Some(ToolDetail::Output { lines, .. }) if !lines.is_empty()
    ));
    let summary = tool_group_summary(tools);
    assert!(summary.starts_with("Thought 2 times"), "{summary}");
    assert!(summary.contains("2 commands"), "{summary}");

    // A lone thought is still an accordion (with the group tween), named
    // plainly.
    let entry = assistant(
        "a2",
        MessageStatus::Complete,
        vec![reasoning_part("r0", "just thinking")],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    assert_eq!(rows.len(), 1);
    let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
        panic!("expected a tool group");
    };
    assert_eq!(tool_group_summary(tools), "Thought process");

    // Empty reasoning renders nothing.
    let entry = assistant(
        "a3",
        MessageStatus::Complete,
        vec![reasoning_part("r0", "   ")],
    );
    assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
}

#[test]
fn live_thought_streams_open_and_settles_closed() {
    let entry = assistant(
        "a1",
        MessageStatus::Streaming,
        vec![reasoning_part("r0", "thinking hard")],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
        panic!("expected a tool group");
    };
    // The live tail auto-opens the group; the chip itself is unresolved
    // (defaults open) until the part stops being the tail.
    assert!(*auto_open);
    assert!(!tools[0].resolved);

    let entry = assistant(
        "a2",
        MessageStatus::Streaming,
        vec![
            reasoning_part("r0", "thinking hard"),
            text_part("t1", "answer"),
        ],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
        panic!("expected a tool group");
    };
    assert!(tools[0].resolved, "a followed thought is settled");
}

#[test]
fn thought_wrap_is_word_aware_and_bounded() {
    let lines = wrap_thought_lines("one two three");
    assert_eq!(lines.len(), 1);
    let long = "word ".repeat(200);
    let lines = wrap_thought_lines(&long);
    assert!(lines.iter().all(|l| l.chars().count() <= THOUGHT_WRAP_COLS));
    assert!(lines.len() > 5);
    let pathological = "x".repeat(300);
    let lines = wrap_thought_lines(&pathological);
    assert!(lines.iter().all(|l| l.chars().count() <= THOUGHT_WRAP_COLS));
}

fn tool_part(id: &str, command: &str) -> MessagePart {
    MessagePart::Tool {
        id: id.into(),
        call: ToolCall::Exec {
            command: command.into(),
        },
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

const MD: &str = "# Title\n\npara one\n\n```rust\nlet x = 1;\n```";

#[test]
fn live_entry_splits_per_block_with_id_continuity() {
    // Live rows split per block exactly like completed ones (the list
    // virtualizes them — the fading tail is the only per-frame work).
    let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
    let live_rows = rows_for_entry(&live, false, &mut parse);
    assert_eq!(live_rows.len(), 3, "one live row per top-level block");
    assert!(
        live_rows
            .iter()
            .all(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
    );
    assert_eq!(live_rows[0].id.as_ref(), "m1#t0.0");
    assert_eq!(live_rows[2].id.as_ref(), "m1#t0.2");

    let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
    let done_rows = rows_for_entry(&done, false, &mut parse);
    assert_eq!(done_rows.len(), 3, "three top-level blocks");
    // Every block row keeps its id across the flip — no flicker on handoff.
    for (live, done) in live_rows.iter().zip(&done_rows) {
        assert_eq!(live.id, done.id);
        // The flip changes the version even at identical text (the
        // streaming bit), forcing a splice.
        assert_ne!(live.version, done.version);
    }
    assert!(matches!(
        done_rows[0].kind,
        RowKind::Markdown { block_ix: 0, .. }
    ));
}

#[test]
fn live_commit_changes_only_tail_row_versions() {
    // Streaming commit: appending to the last block leaves every settled
    // block row's (id, version) untouched — the diff splices only the tail.
    let t1 = "para one\n\npara two\n\npara three";
    let t2 = "para one\n\npara two\n\npara three grows here";
    let live1 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t1)]);
    let live2 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t2)]);
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
fn split_sibling_gaps_match_live_internal_spacing() {
    // The live row spaces its internal blocks by MD_BLOCK_GAP; after the
    // live→split handoff the same boundaries are inter-row gaps. They must
    // be identical or the whole message jumps at completion.
    let done = assistant(
        "m1",
        MessageStatus::Complete,
        vec![
            text_part("t0", MD),
            tool_part("a", "ls"),
            text_part("t1", "tail para"),
        ],
    );
    let rows = rows_for_entry(&done, false, &mut parse);
    // Rows: t0.0, t0.1, t0.2 (three MD blocks), g0, t1.0.
    assert_eq!(rows.len(), 5);
    // Sibling markdown blocks from the same part: md block gap.
    assert_eq!(top_gap_for(Some(&rows[0]), &rows[1]), render::MD_BLOCK_GAP);
    assert_eq!(top_gap_for(Some(&rows[1]), &rows[2]), render::MD_BLOCK_GAP);
    // Markdown → tool group and tool group → next part: larger boundary.
    assert_eq!(top_gap_for(Some(&rows[2]), &rows[3]), Theme::SPACE_MD);
    assert_eq!(top_gap_for(Some(&rows[3]), &rows[4]), Theme::SPACE_MD);
    // Turn starts get the turn gap regardless.
    assert_eq!(top_gap_for(None, &rows[0]), Theme::SPACE_LG);
}

#[test]
fn consecutive_tools_fold_into_groups_between_text() {
    let entry = assistant(
        "m2",
        MessageStatus::Complete,
        vec![
            text_part("t0", "before"),
            tool_part("a", "ls"),
            tool_part("b", "pwd"),
            text_part("t1", "after"),
            tool_part("c", "make"),
        ],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
    assert_eq!(ids, ["m2#t0.0", "m2#g0", "m2#t1.0", "m2#g1"]);
    let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
        panic!("group expected")
    };
    assert_eq!(tools.len(), 2);
    assert!(rows[0].turn_start && !rows[1].turn_start);
}

fn agent_part(id: &str, description: &str) -> MessagePart {
    MessagePart::Tool {
        id: id.into(),
        call: ToolCall::Unknown {
            name: format!("Agent: {description}"),
            input: Some(serde_json::json!({ "description": description })),
        },
        is_error: false,
        resolved: true,
        output: None,
        diff: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        diff_stats: None,
        subagent_ref: Some(format!("chat--sub--{id}")),
        subagent_status: Some(SubagentStatus::Running),
        subagent_tail: None,
    }
}

#[test]
fn agent_calls_split_out_of_ordinary_tool_groups() {
    // Agent/spawn chips must not share a collapse with Reads/Runs: a
    // lone Agent used to hide behind "Called 1 tool", and a mixed
    // group hid the running subagent until the user opened the fold.
    let entry = assistant(
        "m-agent",
        MessageStatus::Complete,
        vec![
            text_part("t0", "before"),
            tool_part("a", "ls"),
            tool_part("b", "pwd"),
            agent_part("s1", "Map URL import ingest path"),
            tool_part("c", "make"),
            agent_part("s2", "Audit the fold path"),
            agent_part("s3", "Verify the commit cadence"),
            text_part("t1", "after"),
        ],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
    assert_eq!(
        ids,
        [
            "m-agent#t0.0",
            "m-agent#g0",
            "m-agent#g1",
            "m-agent#g2",
            "m-agent#g3",
            "m-agent#t1.0",
        ]
    );

    let RowKind::ToolGroup { tools, auto_open } = &rows[1].kind else {
        panic!("ordinary group expected")
    };
    assert_eq!(tools.len(), 2);
    assert!(tool_group_collapses(tools));
    assert!(!*auto_open);

    let RowKind::ToolGroup { tools, .. } = &rows[2].kind else {
        panic!("agent group expected")
    };
    assert_eq!(tools.len(), 1);
    assert!(!tool_group_collapses(tools));
    assert!(is_agent_tool(&tools[0]));

    let RowKind::ToolGroup { tools, .. } = &rows[3].kind else {
        panic!("ordinary group expected")
    };
    assert_eq!(tools.len(), 1);
    assert!(tool_group_collapses(tools));

    let RowKind::ToolGroup { tools, .. } = &rows[4].kind else {
        panic!("consecutive agents share a group")
    };
    assert_eq!(tools.len(), 2);
    assert!(!tool_group_collapses(tools));
    assert!(tools.iter().all(is_agent_tool));
}

#[test]
fn stray_subagent_ref_on_a_run_chip_stays_an_ordinary_tool() {
    // Docs written before the claude-driver fix carry subagent refs on
    // ordinary Run chips (a background shell's task_notification was
    // mis-tagged as subagent traffic). The ref alone must not change the
    // chip's genus: it folds with its neighbors and renders as a plain
    // tool, never as a spawn link to a doc that was never created.
    let mut stray = tool_part("b", "git clone …");
    if let MessagePart::Tool {
        subagent_ref,
        subagent_status,
        ..
    } = &mut stray
    {
        *subagent_ref = Some("chat--sub--b".into());
        *subagent_status = Some(SubagentStatus::Done);
    }
    let entry = assistant(
        "m-stray",
        MessageStatus::Complete,
        vec![tool_part("a", "ls"), stray, tool_part("c", "make")],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    assert_eq!(rows.len(), 1, "one folded group, no agent split");
    let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
        panic!("tool group expected")
    };
    assert_eq!(tools.len(), 3);
    assert!(tool_group_collapses(tools));
    assert!(tools.iter().all(|t| !is_agent_tool(t)));
    assert!(tools.iter().all(|t| !is_spawn_link(t)));
}

#[test]
fn lone_completed_agent_stays_uncollapsed() {
    let entry = assistant(
        "m-lone",
        MessageStatus::Complete,
        vec![agent_part("s1", "scan repo")],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    assert_eq!(rows.len(), 1);
    let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
        panic!("agent group expected")
    };
    assert_eq!(tools.len(), 1);
    assert!(!tool_group_collapses(tools), "no 'Called 1 tool' wrap");
    assert!(
        !*auto_open,
        "auto_open is a streaming flag; agent rows ignore it at paint"
    );
}

#[test]
fn pre_spawn_agent_name_is_enough_to_split() {
    // Before the engine stamps subagent_ref the chip is already named
    // "Agent: …" — that genus must split, or the spawn hides until the
    // first tagged event.
    let mut part = agent_part("s1", "scan repo");
    if let MessagePart::Tool {
        subagent_ref,
        subagent_status,
        ..
    } = &mut part
    {
        *subagent_ref = None;
        *subagent_status = None;
    }
    let entry = assistant(
        "m-pre",
        MessageStatus::Complete,
        vec![tool_part("a", "ls"), part],
    );
    let rows = rows_for_entry(&entry, false, &mut parse);
    assert_eq!(rows.len(), 2);
    let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
        panic!()
    };
    assert!(tool_group_collapses(tools));
    let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
        panic!()
    };
    assert!(!tool_group_collapses(tools));
    assert!(is_agent_call(&tools[0].call));
}

#[test]
fn trailing_group_auto_opens_only_while_streaming() {
    let parts = vec![text_part("t0", "hi"), tool_part("a", "ls")];
    let streaming = assistant("m3", MessageStatus::Streaming, parts.clone());
    let rows = rows_for_entry(&streaming, false, &mut parse);
    let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
        panic!()
    };
    assert!(auto_open, "trailing group opens while streaming");

    let complete = assistant("m3", MessageStatus::Complete, parts);
    let rows = rows_for_entry(&complete, false, &mut parse);
    let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
        panic!()
    };
    assert!(!auto_open);

    // A non-trailing group never auto-opens.
    let mid = assistant(
        "m4",
        MessageStatus::Streaming,
        vec![tool_part("a", "ls"), text_part("t0", "hi")],
    );
    let rows = rows_for_entry(&mid, false, &mut parse);
    let RowKind::ToolGroup { auto_open, .. } = rows[0].kind else {
        panic!()
    };
    assert!(!auto_open);
}

#[test]
fn user_rows_and_echo_versions() {
    let mut entry = assistant("u1", MessageStatus::Complete, vec![]);
    entry.role = MessageRole::User;
    entry.status = None;
    entry.parts = vec![text_part("t0", "hello")];
    let confirmed = rows_for_entry(&entry, false, &mut parse);
    let echoed = rows_for_entry(&entry, true, &mut parse);
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
fn user_rows_split_attachment_refs_from_text() {
    let content = crate::attachments::with_attachments(
        "what color is this?",
        &["/data/uploads/ab12-red.png".to_string()],
    );
    let mut entry = assistant("u2", MessageStatus::Complete, vec![]);
    entry.role = MessageRole::User;
    entry.status = None;
    entry.parts = vec![text_part("t0", &content)];
    let rows = rows_for_entry(&entry, false, &mut parse);
    assert_eq!(rows.len(), 1);
    let RowKind::User {
        text, attachments, ..
    } = &rows[0].kind
    else {
        panic!("expected a user row");
    };
    assert_eq!(text.as_ref(), "what color is this?");
    assert_eq!(rows[0].copy_text.as_deref(), Some("what color is this?"));
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].path, "/data/uploads/ab12-red.png");
    assert_eq!(attachments[0].name, "ab12-red.png");

    // Image-only send: no bubble text, refs parsed.
    let only = crate::attachments::with_attachments("", &["/a/p.png".to_string()]);
    entry.parts = vec![text_part("t0", &only)];
    let rows = rows_for_entry(&entry, false, &mut parse);
    let RowKind::User {
        text, attachments, ..
    } = &rows[0].kind
    else {
        panic!("expected a user row");
    };
    assert_eq!(text.as_ref(), "");
    assert!(rows[0].copy_text.is_none());
    assert_eq!(attachments.len(), 1);
}

/// A sent prompt's file mentions render as chips in the transcript: the
/// row carries the projected display text plus spans, while ordinary
/// prompts keep the empty-spans fast path. The row version derives from
/// the RAW text either way, so projection never perturbs the diff key.
#[test]
fn user_rows_project_file_mentions_into_chips() {
    let raw = "look at [composer.rs](hearth-file:crates/ui/src/composer.rs) please";
    let mut entry = assistant("u3", MessageStatus::Complete, vec![]);
    entry.role = MessageRole::User;
    entry.status = None;
    entry.parts = vec![text_part("t0", raw)];
    let rows = rows_for_entry(&entry, false, &mut parse);
    let RowKind::User { text, mentions, .. } = &rows[0].kind else {
        panic!("expected a user row");
    };
    assert!(
        !text.contains("hearth-file:"),
        "raw link left visible: {text}"
    );
    assert!(text.contains("composer.rs"));
    assert_eq!(mentions.len(), 1);
    assert!(!mentions[0].is_dir);
    assert_eq!(mentions[0].path.as_ref(), "crates/ui/src/composer.rs");
    assert_eq!(&text[mentions[0].range.clone()], {
        let projected: &str = "\u{00A0}@composer.rs\u{00A0}";
        projected
    });
    assert_eq!(rows[0].version, (raw.len() as u64) << 1);

    entry.parts = vec![text_part("t0", "no mentions here")];
    let rows = rows_for_entry(&entry, false, &mut parse);
    let RowKind::User { text, mentions, .. } = &rows[0].kind else {
        panic!("expected a user row");
    };
    assert_eq!(text.as_ref(), "no mentions here");
    assert!(mentions.is_empty());
}

#[test]
fn diff_rows_appends_and_middle_edits() {
    let entry1 = assistant("m1", MessageStatus::Complete, vec![text_part("t0", "one")]);
    let entry2 = assistant("m2", MessageStatus::Complete, vec![text_part("t0", "two")]);
    let r1 = rows_for_entry(&entry1, false, &mut parse);
    let mut both = r1.clone();
    both.extend(rows_for_entry(&entry2, false, &mut parse));

    // Identical → None.
    assert!(diff_rows(&r1, &r1.clone()).is_none());
    // Append → splice at the tail.
    assert_eq!(diff_rows(&r1, &both), Some((1..1, 1)));
    // Removal from the end.
    assert_eq!(diff_rows(&both, &r1), Some((1..2, 0)));

    // Middle content change: only the changed row splices.
    let entry1b = assistant(
        "m1",
        MessageStatus::Complete,
        vec![text_part("t0", "one more")],
    );
    let mut both_b = rows_for_entry(&entry1b, false, &mut parse);
    both_b.extend(rows_for_entry(&entry2, false, &mut parse));
    assert_eq!(diff_rows(&both, &both_b), Some((0..1, 1)));

    // Full reset when everything shifts.
    let r2 = rows_for_entry(&entry2, false, &mut parse);
    assert_eq!(diff_rows(&r1, &r2), Some((0..1, 1)));
}

#[test]
fn diff_handles_live_to_split_growth() {
    let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
    let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
    let live_rows = rows_for_entry(&live, false, &mut parse);
    let done_rows = rows_for_entry(&done, false, &mut parse);
    // Same ids; every version flips its streaming bit → one 3-row splice.
    assert_eq!(diff_rows(&live_rows, &done_rows), Some((0..3, 3)));
}

#[test]
fn tool_diff_builds_real_hunks_with_context_and_numbers() {
    let old = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>();
    let mut new = old.clone();
    new[9] = "LINE 10".into();
    let diff = hearth_proto::ToolDiff {
        path: "/w/a.rs".into(),
        old_text: Some(old.join("\n") + "\n"),
        new_text: new.join("\n") + "\n",
    };
    let Some(ToolDetail::Diff {
        file,
        old_text,
        new_text,
    }) = tool_detail(None, Some(&diff), None)
    else {
        panic!("expected diff detail");
    };
    // One hunk: the change plus 3 context lines each side, real numbers.
    assert_eq!(file.hunks.len(), 1);
    let hunk = &file.hunks[0];
    assert_eq!(hunk.header, "@@ -7,7 +7,7 @@");
    assert_eq!(hunk.lines.len(), 8); // 6 context + 1 del + 1 add
    let del = hunk
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Del)
        .expect("del line");
    assert_eq!(del.old_no, Some(10));
    assert_eq!(del.new_no, None);
    assert_eq!(del.text, "line 10");
    let add = hunk
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Add)
        .expect("add line");
    assert_eq!(add.new_no, Some(10));
    assert_eq!(add.text, "LINE 10");
    assert_eq!((file.additions, file.deletions), (1, 1));
    assert_eq!(old_text.as_deref(), diff.old_text.as_deref());
    assert_eq!(new_text.as_deref(), Some(diff.new_text.as_str()));
    // New files carry Added status (and no old numbers).
    let created = hearth_proto::ToolDiff {
        path: "/w/new.txt".into(),
        old_text: None,
        new_text: "only\n".into(),
    };
    let Some(ToolDetail::Diff {
        file,
        old_text,
        new_text,
    }) = tool_detail(None, Some(&created), None)
    else {
        panic!("expected diff detail");
    };
    assert_eq!(file.status, crate::changes::FileStatus::Added);
    assert!(old_text.is_none());
    assert_eq!(new_text.as_deref(), Some("only\n"));

    // Output: verbatim lines (indentation intact), counted-tail cap.
    let output = (0..40)
        .map(|i| format!("    indented {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let Some(ToolDetail::Output {
        lines,
        truncated_by,
    }) = tool_detail(Some(&output), None, None)
    else {
        panic!("expected output detail");
    };
    assert_eq!(lines.len(), OUTPUT_DETAIL_MAX_LINES);
    assert_eq!(truncated_by, 40 - OUTPUT_DETAIL_MAX_LINES);
    assert_eq!(lines[0].as_ref(), "    indented 0");

    // Nothing → no affordance.
    assert!(tool_detail(None, None, None).is_none());
    assert!(tool_detail(Some("\n\n"), None, None).is_none());
}

#[test]
fn tool_group_summaries() {
    let exec = |c: &str| ToolItem {
        call: ToolCall::Exec { command: c.into() },
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
    let edit = |p: &str| ToolItem {
        call: ToolCall::EditFile {
            path: p.into(),
            old_string: None,
            new_string: None,
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
    let tools = vec![
        exec("ls"),
        exec("pwd"),
        exec("make"),
        edit("a.rs"),
        edit("b.rs"),
    ];
    assert_eq!(
        tool_group_summary(&tools),
        "Ran 3 commands · edited 2 files"
    );
    // Distinct-path dedupe: editing one file twice counts once.
    let tools = vec![edit("a.rs"), edit("a.rs")];
    assert_eq!(tool_group_summary(&tools), "Edited 1 file");
    // Failures append.
    let mut failing = exec("boom");
    failing.is_error = true;
    assert_eq!(tool_group_summary(&[failing]), "Ran 1 command · 1 failed");
    // Reads / searches / misc.
    let tools = vec![
        ToolItem {
            call: ToolCall::ReadFile { path: "x".into() },
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
        },
        ToolItem {
            call: ToolCall::Glob {
                pattern: "*.rs".into(),
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
        },
        ToolItem {
            call: ToolCall::WebSearch { query: "q".into() },
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
        },
    ];
    assert_eq!(tool_group_summary(&tools), "Read 1 file · searched 2 times");
}

#[test]
fn subagent_tab_titles() {
    // The tab is the BARE task — the "Agent:" genus is stripped.
    let named = ToolCall::Unknown {
        name: "Agent: scan repo".into(),
        input: None,
    };
    assert_eq!(subagent_tab_title(&named).as_ref(), "scan repo");
    // A bare "Task"/"Agent" digs the description out of the call input
    // (which sheds any genus of its own).
    let bare = ToolCall::Unknown {
        name: "Task".into(),
        input: Some(serde_json::json!({
            "description": "Agent: audit the auth flow",
            "prompt": "very long instructions…",
        })),
    };
    assert_eq!(subagent_tab_title(&bare).as_ref(), "audit the auth flow");
    // Word boundaries only — a name that merely STARTS with the genus
    // keeps itself.
    let compound = ToolCall::Unknown {
        name: "Taskmaster".into(),
        input: None,
    };
    assert_eq!(subagent_tab_title(&compound).as_ref(), "Taskmaster");
    // Nothing to derive → the generic label.
    let blank = ToolCall::Unknown {
        name: "agent".into(),
        input: None,
    };
    assert_eq!(subagent_tab_title(&blank).as_ref(), "Subagent");
    // Absurd lengths cap with an ellipsis; multiline prompts keep only
    // their first line.
    let long = ToolCall::Unknown {
        name: "x".repeat(120),
        input: None,
    };
    let title = subagent_tab_title(&long);
    assert_eq!(title.chars().count(), SUBAGENT_TITLE_MAX + 1);
    assert!(title.ends_with('…'));
    // Non-spawn-shaped calls stay generic.
    assert_eq!(
        subagent_tab_title(&ToolCall::Exec {
            command: "ls".into()
        })
        .as_ref(),
        "Subagent"
    );
}

#[test]
fn tool_chip_labels_per_kind() {
    assert_eq!(
        tool_chip_content(&ToolCall::Exec {
            command: "cargo test".into()
        }),
        ("Run", "cargo test".to_string())
    );
    assert_eq!(
        tool_chip_content(&ToolCall::Search {
            pattern: "foo".into(),
            path: Some("src".into())
        }),
        ("Search", "foo in src".to_string())
    );
    assert_eq!(
        tool_chip_content(&ToolCall::ApplyPatch { path: None }),
        ("Patch", "workspace".to_string())
    );
    assert_eq!(
        tool_chip_content(&ToolCall::Mcp {
            server: "gh".into(),
            tool: "issues".into(),
            input: None
        }),
        ("MCP", "gh · issues".to_string())
    );
    let todo = ToolCall::Todo {
        items: vec![
            hearth_proto::TodoItem {
                text: "a".into(),
                done: true,
            },
            hearth_proto::TodoItem {
                text: "b".into(),
                done: false,
            },
        ],
    };
    assert_eq!(tool_chip_content(&todo), ("Todo", "1/2 done".to_string()));
}

#[test]
fn multiline_command_flattens_to_one_chip_line() {
    // The user's breaker: a multi-line script in a Run chip. The detail
    // must come out as ONE sanitized line — the chip's fixed 30px card
    // then truncates it with an ellipsis like the original's CSS.
    let (label, detail) = tool_chip_content(&ToolCall::Exec {
        command: "set -e\nfixture_in_original=0\n\tgrep -c  \"x\"".into(),
    });
    assert_eq!(label, "Run");
    assert_eq!(detail, "set -e fixture_in_original=0 grep -c \"x\"");
    assert!(!detail.contains('\n'));
    // The chip row height is a constant, independent of content shape.
    assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
    // Every detail kind is sanitized (MCP inputs / queries are model text).
    let (_, q) = tool_chip_content(&ToolCall::WebSearch {
        query: "line one\nline two".into(),
    });
    assert_eq!(q, "line one line two");
}

#[test]
fn call_block_carries_the_full_invocation() {
    // Multi-line command: verbatim lines, not the flattened chip line.
    let Some(ToolDetail::Output {
        lines,
        truncated_by,
    }) = call_block(&ToolCall::Exec {
        command: "set -e\ncargo test".into(),
    })
    else {
        panic!("expected an output block")
    };
    assert_eq!(truncated_by, 0);
    assert_eq!(
        lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
        vec!["set -e", "cargo test"]
    );

    // A long single-line command soft-wraps instead of ellipsizing.
    let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Exec {
        command: "x".repeat(CALL_WRAP_COLS * 2 + 10),
    }) else {
        panic!("expected an output block")
    };
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().all(|l| l.chars().count() <= CALL_WRAP_COLS));

    // MCP input pretty-prints under the `server · tool` line.
    let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Mcp {
        server: "gh".into(),
        tool: "issues".into(),
        input: Some(serde_json::json!({"repo": "hearth"})),
    }) else {
        panic!("expected an output block")
    };
    assert_eq!(lines[0].as_ref(), "gh · issues");
    assert!(lines.iter().any(|l| l.contains("\"repo\": \"hearth\"")));

    // Todos list one item per line with checkbox state.
    let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Todo {
        items: vec![
            hearth_proto::TodoItem {
                text: "a".into(),
                done: true,
            },
            hearth_proto::TodoItem {
                text: "b".into(),
                done: false,
            },
        ],
    }) else {
        panic!("expected an output block")
    };
    assert_eq!(
        lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
        vec!["[x] a", "[ ] b"]
    );

    // Blank invocation → no block; the chip stays a plain card.
    assert!(
        call_block(&ToolCall::Exec {
            command: "  \n ".into()
        })
        .is_none()
    );
}

#[test]
fn timestamp_strip_lands_on_the_last_settled_row() {
    // Fixed zone (UTC−4): "Jul 1, 3:45 PM" — the exact formatTimestamp
    // shape (short month, numeric day, no leading zero, 2-digit minutes).
    let tz = FixedOffset::west_opt(4 * 3600).unwrap();
    let ms = chrono::DateTime::parse_from_rfc3339("2026-07-01T19:45:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(format_timestamp(ms, &tz), "Jul 1, 3:45 PM");

    // User entries carry the strip on their single row (pending too).
    let user = SessionMessageEntry {
        id: "u1".into(),
        role: MessageRole::User,
        parts: vec![text_part("p1", "hi")],
        created_at: ms,
        device_id: "dev".into(),
        status: None,
        continuation_of: None,
    };
    let rows = rows_for_entry(&user, true, &mut parse);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].timestamp, Some(ms));

    // Assistant entries: strip on the LAST row once settled…
    let done = assistant(
        "a1",
        MessageStatus::Complete,
        vec![text_part("p1", "one\n\ntwo")],
    );
    let rows = rows_for_entry(&done, false, &mut parse);
    assert!(rows.len() >= 2);
    assert_eq!(rows.last().unwrap().timestamp, Some(done.created_at));
    assert_eq!(
        rows.last().unwrap().copy_text.as_deref(),
        Some("one\n\ntwo")
    );
    assert!(rows[..rows.len() - 1].iter().all(|r| r.timestamp.is_none()));
    assert!(rows[..rows.len() - 1].iter().all(|r| r.copy_text.is_none()));

    // …but never mid-stream (chat-view.tsx: no hover under a moving reply).
    let live = assistant(
        "a2",
        MessageStatus::Streaming,
        vec![text_part("p1", "streaming…")],
    );
    let rows = rows_for_entry(&live, false, &mut parse);
    assert!(rows.iter().all(|r| r.timestamp.is_none()));
    assert!(rows.iter().all(|r| r.copy_text.is_none()));
    // Every row knows its entry (the hover group).
    assert!(rows.iter().all(|r| r.entry_id.as_ref() == live.id));
}

#[test]
fn message_copy_keeps_authored_text_and_excludes_tool_traces() {
    let entry = assistant(
        "a-copy",
        MessageStatus::Complete,
        vec![
            text_part("p1", "First **paragraph**."),
            tool_part("tool", "printf hidden"),
            text_part("p2", "    indented code\n    stays indented"),
        ],
    );
    assert_eq!(
        assistant_copy_text(&entry).as_deref(),
        Some("First **paragraph**.\n\n    indented code\n    stays indented")
    );
}

#[test]
fn single_line_collapses_all_whitespace_runs() {
    assert_eq!(single_line("a\nb"), "a b");
    assert_eq!(single_line("  a\t\t b \r\n c  "), "a b c");
    assert_eq!(single_line("plain"), "plain");
    assert_eq!(single_line(""), "");
    assert_eq!(single_line("\n\n"), "");
}

#[test]
fn chips_height_is_analytic() {
    assert_eq!(chips_height(0), 0.0);
    assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
    assert_eq!(
        chips_height(3),
        CHIPS_TOP_PAD + 3.0 * CHIP_HEIGHT + 2.0 * CHIP_GAP
    );
}

#[test]
fn flavour_words_rotate_every_seven_seconds() {
    let seed = flavour_seed("chat-1");
    assert_eq!(flavour_word(seed, 0), flavour_word(seed, 6));
    assert_ne!(flavour_word(seed, 0), flavour_word(seed, 7));
    // Deterministic per chat; different chats usually differ in phase.
    assert_eq!(flavour_word(seed, 3), flavour_word(seed, 3));
    assert_eq!(format_elapsed(59), "59s");
    assert_eq!(format_elapsed(92), "1m 32s");
    assert_eq!(format_elapsed(-5), "0s");
}

#[test]
fn sending_bridge_holds_until_the_turn_outdates_the_send() {
    let send = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
        .unwrap()
        .to_utc();
    let before = send - chrono::Duration::seconds(90);
    let after = send + chrono::Duration::seconds(2);
    // In flight, row still on the previous turn (or no row yet).
    assert!(sending_bridge(Some(send), Some(before)));
    assert!(sending_bridge(Some(send), None));
    // The turn started after the send fired — timer takes over.
    assert!(!sending_bridge(Some(send), Some(after)));
    // No send in flight: never a bridge, whatever the row says.
    assert!(!sending_bridge(None, Some(before)));
    assert!(!sending_bridge(None, None));
}

#[test]
fn empty_text_parts_produce_no_rows() {
    let entry = assistant(
        "m9",
        MessageStatus::Streaming,
        vec![text_part("t0", ""), text_part("t1", "   ")],
    );
    assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
}

// ---- bounded long-chat caches ----

fn blob_ref(ix: usize) -> SharedString {
    SharedString::from(format!("chat-1/part{ix}.output"))
}

fn highlight_slot(ix: usize) -> (SharedString, usize) {
    (SharedString::from(format!("m{ix}#p1")), 0)
}

fn highlight_key(ix: usize) -> DocumentHighlightKey {
    DocumentHighlightKey::new(LanguageId::Rust, &format!("fn block_{ix}() {{}}"))
}

/// A blob cache filled to the cap, ordered oldest first (`blob_ref(0)` is the
/// least recently fetched).
fn filled_blob_cache(
    fetch: impl Fn(usize) -> BlobFetch,
) -> (HashMap<SharedString, BlobFetch>, HashMap<SharedString, u64>) {
    let mut details = HashMap::new();
    let mut order = HashMap::new();
    for ix in 0..MAX_BLOB_DETAILS {
        let blob_ref = blob_ref(ix);
        order.insert(blob_ref.clone(), ix as u64 + 1);
        details.insert(blob_ref, fetch(ix));
    }
    (details, order)
}

#[test]
fn blob_cache_is_bounded_and_keeps_the_most_recent_refs() {
    let mut details = HashMap::new();
    let mut order = HashMap::new();
    for ix in 0..MAX_BLOB_DETAILS + 32 {
        let blob_ref = blob_ref(ix);
        order.insert(blob_ref.clone(), ix as u64 + 1);
        details.insert(blob_ref.clone(), BlobFetch::Failed);
        prune_blob_cache(&mut details, &mut order, &blob_ref);
    }
    assert_eq!(details.len(), MAX_BLOB_DETAILS);
    assert_eq!(order.len(), details.len());
    // The first insertions drained out; the most recently fetched survive.
    for ix in 0..32 {
        assert!(!details.contains_key(&blob_ref(ix)), "ref {ix} not evicted");
        assert!(!order.contains_key(&blob_ref(ix)), "order {ix} drifted");
    }
    for ix in 32..MAX_BLOB_DETAILS + 32 {
        assert!(
            details.contains_key(&blob_ref(ix)),
            "recent ref {ix} evicted"
        );
    }
}

#[test]
fn blob_cache_never_evicts_the_ref_being_fetched() {
    let (mut details, mut order) = filled_blob_cache(|_| BlobFetch::Failed);
    // A completion insert can carry an older order stamp than everything
    // cached (the click that ranked it happened long ago) — its own slot
    // must still survive the prune it triggers.
    let keep = blob_ref(MAX_BLOB_DETAILS);
    order.insert(keep.clone(), 0);
    details.insert(keep.clone(), BlobFetch::Failed);
    prune_blob_cache(&mut details, &mut order, &keep);
    assert_eq!(details.len(), MAX_BLOB_DETAILS);
    assert!(details.contains_key(&keep));
    assert!(order.contains_key(&keep));
    assert_eq!(order.len(), details.len());
    assert!(
        !details.contains_key(&blob_ref(0)),
        "oldest other ref stayed"
    );
}

#[test]
fn blob_cache_prefers_settled_entries_over_in_flight_fetches() {
    // The OLDEST slot is still loading; a settled slot sits right behind it.
    // Dropping the loading slot would cancel a live fetch, so the settled
    // entry must go even though it is newer.
    let (mut details, mut order) = filled_blob_cache(|ix| {
        if ix == 0 {
            BlobFetch::Loading(Task::ready(()))
        } else {
            BlobFetch::Failed
        }
    });
    let keep = blob_ref(MAX_BLOB_DETAILS);
    order.insert(keep.clone(), MAX_BLOB_DETAILS as u64 + 1);
    details.insert(keep.clone(), BlobFetch::Failed);
    prune_blob_cache(&mut details, &mut order, &keep);
    assert!(matches!(
        details.get(&blob_ref(0)),
        Some(BlobFetch::Loading(_))
    ));
    assert!(!details.contains_key(&blob_ref(1)));
    assert_eq!(order.len(), details.len());
}

#[test]
fn blob_cache_evicts_a_loading_slot_only_as_a_last_resort() {
    let (mut details, mut order) = filled_blob_cache(|_| BlobFetch::Loading(Task::ready(())));
    let keep = blob_ref(MAX_BLOB_DETAILS);
    order.insert(keep.clone(), MAX_BLOB_DETAILS as u64 + 1);
    details.insert(keep.clone(), BlobFetch::Loading(Task::ready(())));
    prune_blob_cache(&mut details, &mut order, &keep);
    // Every slot is in flight: the oldest loses (its task is cancelled and
    // the chip re-arms as a retry), and the new slot is kept.
    assert!(!details.contains_key(&blob_ref(0)));
    assert!(details.contains_key(&keep));
    assert_eq!(details.len(), MAX_BLOB_DETAILS);
    assert_eq!(order.len(), details.len());
}

#[test]
fn highlight_store_is_bounded_and_keeps_the_newest_slots() {
    let mut store = HighlightStore::default();
    for ix in 0..MAX_HIGHLIGHT_ENTRIES + 32 {
        store.insert_for_test(highlight_slot(ix), highlight_key(ix));
    }
    assert_eq!(store.len(), MAX_HIGHLIGHT_ENTRIES);
    for ix in 0..32 {
        assert!(
            !store.entries.contains_key(&highlight_slot(ix)),
            "slot {ix} not evicted"
        );
    }
    for ix in 32..MAX_HIGHLIGHT_ENTRIES + 32 {
        assert!(
            store.entries.contains_key(&highlight_slot(ix)),
            "newest slot {ix} evicted"
        );
    }
}

#[test]
fn highlight_store_touch_keeps_a_revisited_block_alive() {
    let mut store = HighlightStore::default();
    for ix in 0..MAX_HIGHLIGHT_ENTRIES {
        store.insert_for_test(highlight_slot(ix), highlight_key(ix));
    }
    // Re-requesting the oldest slot (a paint hit, as `request` does) makes it
    // the most recent — the next insertion evicts its neighbour instead.
    store.touch_for_test(&highlight_slot(0));
    store.insert_for_test(
        highlight_slot(MAX_HIGHLIGHT_ENTRIES),
        highlight_key(MAX_HIGHLIGHT_ENTRIES),
    );
    assert!(store.entries.contains_key(&highlight_slot(0)));
    assert!(!store.entries.contains_key(&highlight_slot(1)));
    assert!(
        store
            .entries
            .contains_key(&highlight_slot(MAX_HIGHLIGHT_ENTRIES))
    );
    assert_eq!(store.len(), MAX_HIGHLIGHT_ENTRIES);
}

#[test]
fn tool_detail_folds_are_bounded_and_keep_the_newest() {
    let mut folds: HashMap<SharedString, FoldState> = HashMap::new();
    for ix in 0..MAX_TOOL_DETAILS + 32 {
        let key = SharedString::from(format!("m{ix}#p1#d0"));
        // Same insertion shape as `Transcript::tool_detail_fold_mut`: stamp
        // newest, insert, then prune protecting the new key.
        folds.insert(
            key.clone(),
            FoldState {
                touched: ix as u64 + 1,
                ..FoldState::default()
            },
        );
        prune_tool_details(&mut folds, &key);
    }
    assert_eq!(folds.len(), MAX_TOOL_DETAILS);
    for ix in 0..32 {
        let key = SharedString::from(format!("m{ix}#p1#d0"));
        assert!(!folds.contains_key(&key), "fold {ix} not evicted");
    }
    for ix in 32..MAX_TOOL_DETAILS + 32 {
        let key = SharedString::from(format!("m{ix}#p1#d0"));
        assert!(folds.contains_key(&key), "newest fold {ix} evicted");
    }
}
