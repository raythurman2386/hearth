# Hearth — Rune UI port

Status: **ported and shipped on `port/rune`**. `crates/ui-rune` is the headed
viewport; `crates/ui` (gpui) has been removed and `apps/hearth` links
`hearth-ui-rune`. Rune is pinned to `dev` rev `424e0f5`.

## Goal

Port the full headed UI from gpui to [Rune](https://github.com/anomalyco/rune)
without losing layout or functionality. The engine/RPC/doc/sync crates are
UI-agnostic and shared unchanged.

## Crate layout

```
crates/ui-rune/
  Cargo.toml
  assets/fonts/            # Geist + Geist Mono
  examples/smoke.rs        # headless snapshot harness
  src/
    lib.rs                 # run_app, UiConfig, module tree
    app.rs                 # AppSpec, setup/poll/on_key, scenes
    state.rs               # App: engine + lists + selection + transcript
    bridge.rs              # tokio runtime, EngineEvent channel, WatchGuard
    engine.rs              # EngineHandle (Arc<RpcClient>), bootstrap
    theme.rs               # Theme tokens (Rune Color), rune-kit Kit
    appearance.rs          # AppearanceMode/AppearanceState
    settings.rs            # UiSettings, KeymapConfig (framework-free)
    comments.rs            # DiffComment (framework-free)
    shell/                 # root, sidebar, spaces, titlebar, overlays, routes
    transcript/            # rows, model, render, viewport, rail
    markdown/              # parser, render, selection, veil
    composer/              # input, send, mentions, slash, attachments
    terminal/              # emulator, view, panel
    changes/               # model, rows, state, render, comments, highlights
    pickers/               # model, selection, loads, render, space_picker
    settings/              # page views
```

## Rune model (what changed from gpui)

| gpui | Rune |
|---|---|
| `Entity<T>` / `Context<T>` / `Render` | one `App` struct; `fn view(&App, &mut Cx<App>) -> El` |
| `cx.spawn` / `Task` / `gpui_tokio` | `Bridge` (tokio runtime + `EngineEvent` channel + `WatchGuard`) |
| `cx.notify()` | mutate state; `poll` returns `true` |
| `div()` / `flex` | `col()` / `row()` / `el()` |
| `list` + `ListState` | `.scroll(ScrollSpec)` + `.growth(..).virtualize(..)` |
| `with_animation` | `rune_motion` springs, `.motion(..)`, `Tx` |
| `deferred(anchored(..))` | `.anchored(..)` / rune-kit `Popover`/`Menu`/`Dialog` |
| `on_click` | `.on_press` / `.on_press_tx` |
| custom `Element` paint | Rune `canvas(Shape)` / per-element nodes |
| `Hsla` | `rune::Color` (linear sRGB) |
| `KeyBinding`/`actions!` | `AppSpec::on_key`, `Chord`/`Keybind`, `El::on_key` |

Rune gotchas (from the Rune README):
- `.grow(g)` is `flex: g 1 0`; a grown scroll viewport in an auto-height column
  collapses. Bound an ancestor (`.h_full()`/`.min_h(0.0)`).
- Every flex column between a bounded ancestor and a scroll viewport needs
  `.min_h(0.0)`.
- `.virtualize(est)` requires `.growth(GrowthSpec)` on the same element and must
  be the scroll viewport's first child.
- Rows need unique keys and **internal vertical padding**, not margins/gaps.
- A pixel hairline is `1.0 / cx.env().scale`.
- Text does not wrap unless `.wrap()`.
- Keys are one namespace; prefix app keys (`"app/…"`) because rune-kit claims
  `label`, `value`, `icon`, `control`, etc.

## State and handlers

- `App` is the single root state (`crate::state::App`). Sub-states are plain
  structs: `shell: ShellState`, `transcript_state: TranscriptState`,
  `composer: ComposerState`, `terminal: TerminalState`, `changes: ChangesState`,
  `pickers: PickersState`.
- The view takes `&App`; a handler takes `&mut App` (optionally `&mut Tx`).
  Handler closures are `'static` and cannot borrow `app`; capture ids/values.
- All async work goes through `Bridge`:
  - one-shot: `app.bridge.spawn(async move { … })`
  - watches: `app.bridge.spawn_watch(client, method, params, map) -> WatchGuard`
  - results come back as `EngineEvent`s, drained in `App::drain_events`.
- Cancellation is dropping/cancelling the `WatchGuard` (the RPC client sends
  the cancel frame on drop).

## Shared-file ownership (avoid parallel edit conflicts)

| Owner | Files |
|---|---|
| lead | `lib.rs`, `app.rs`, `state.rs`, `bridge.rs`, `engine.rs`, `theme.rs`, `appearance.rs`, `Cargo.toml` |
| settings | `settings.rs`, `settings/**` |
| terminal | `terminal/**` |
| changes | `changes/**`, `comments.rs`, `syntax_cache.rs` |
| pickers | `pickers/**` |
| transcript | `transcript/**`, `markdown/**`, `rail.rs`, `badges.rs`, `attachments.rs` |
| composer | `composer/**`, `notify.rs`, `sound.rs` |
| shell | `shell/**`, `icons.rs`, `loaders.rs`, `motion.rs`, `edge_fade.rs`, `frost.rs` |

Module files declare their own submodules (`pub mod …` in their own `mod.rs`).
Do **not** edit another owner's files; expose a small public API and the lead
wires it into `shell/mod.rs` / `state.rs`.

## Verification

```sh
cargo check -p hearth-ui-rune
cargo test  -p hearth-ui-rune          # 541 lib + integration tests
cargo clippy -p hearth-ui-rune --all-targets -- -D warnings
# headless snapshots (state scenes: ready, failure, empty, demo, settings, narrow)
cargo run -p hearth-ui-rune --example smoke -- --snap /tmp/hearth.png --scene demo --size 1320x880 --scale 2
cargo run -p hearth-ui-rune --example smoke -- --snap /tmp/narrow.png --scene narrow --size 941x700 --scale 2
cargo run -p hearth-ui-rune --example smoke -- --tree /tmp/tree.json --scene demo
```

The tree JSON is the layout regression net: check rects, not just pixels.
`cargo test -p hearth-ui-rune` runs the ported pure-logic tests, the state/theme
tests, and the Runtime interaction/layout tests.

Headed check:

```sh
HEARTH_DATA_DIR=/tmp/hearth-run HEARTH_HARNESS=mock HEARTH_IPC_PORT=27699 \
  cargo run -p hearth
```

## Porting rules

1. No `gpui`/`gpui_tokio`/`gpui_platform` imports in `crates/ui-rune`.
2. Keep pure logic and its tests: row/version math, patch parsing, markdown
   parsing/mending, keymap, settings, comment folding, diff pairing, terminal
   ANSI/palette/pointer math, picker normalization.
3. Layout numbers (spacing, heights, widths, radii, type sizes) must match the
   gpui constants. Colors come from `crate::theme::Theme`; do not hard-code.
4. Use rune-kit components where the gpui code hand-rolled the same control
   (`Button`, `Select`, `Combobox`, `Menu`, `Dialog`, `Markdown`, `DiffView`,
   `Tree`, `Table`, `SplitView`, `Sidebar`, `TabStrip`).
5. Keep behavior identical: same keybindings, same send/steer/stop logic, same
   reconnect/resubscribe rules, same empty/error states.
6. Add a `#[cfg(test)]` test for every pure function you port.
7. Before declaring done: `cargo check -p hearth-ui-rune` is clean (warnings
   acceptable only for genuinely unused scaffolding) and the smoke snapshot
   still renders.

## Transcript + markdown hardening (port/rune)

**Per-chat viewport restore.** On a chat switch `TranscriptState::sync` snapshots
the OUTGOING chat before the caches reset: the row anchor (first visible row from
`on_visible_rows`, mapped through `SavedViewportCache`'s LRU 256), the live
distance from the end (`on_scroll_offset` → a `Live<[f32; 2]>`), and an active
own-turn runway. The incoming snapshot resolves once its replay lands: an exact
row while the replay is pending, entry/index fallbacks only after an
authoritative populated reset, retired on an authoritative empty reset.
Deltas from gpui: the pixels scrolled into the anchor row are captured as 0
(`scroll_top` is an index, not a `ListOffset`), so a restore lands the row top at
the viewport top; and the restore executes in the rows' `on_layout_tx` (the only
place a `Tx` exists) as `scroll_into_view` + `scroll_key_to`, so it costs one
layout commit. The rows stack is keyed by chat, whose first layout is the commit
the request is applied on.

**Own-turn hold.** A send reserves the runway from the reservation math
(`usable − turn height`) sized in the view build from `memo::layout` of the
prompt and last rows; a fresh send takes the full usable height provisionally.
The runway is a trailing virtual row AFTER the real rows — bottom padding on the
last row renders but is not counted in the virtual list's scroll extent, so the
viewport could not reach the prompt's inset. The bottom clearance (composer /
terminal overlay) rides the same row.
The prompt is then placed at its inset with `scroll_into_view` + `scroll_key_to`,
and the hold across reply growth is `Tx::keep_on_screen` on the prompt row —
Rune's equivalent of gpui's absolute per-frame re-assert (no eased corrections;
the runtime's own scroll/clamp owns the motion). When the reply outgrows the
reservation the anchor is released and the viewport hands back to
`stick_bottom` via `scroll_to_bottom`.

**Subagent transcripts** build rows through the same per-entry fingerprint cache
as the primary transcript (`rows_for_doc`, LRU 2048 across docs), so a live
subagent watch no longer re-parses every entry every frame.

**Rail.** The rail receives the measured transcript viewport height
(`on_layout_tx` on the viewport → `viewport_height`) and includes optimistic
echoes in its ticks, so a just-sent prompt appears immediately.
`on_visible_rows` returns `true` only when the active row changes.

**Markdown round-trip.** `block_source` now backslash-escapes literal inline
text (`\\ \\` `*` `_` `[` `]` `<` `>` `#` `~` `-` `+` `=` `!`, plus ordered-list
`.`/`)` at a line start), chooses inline-code fences longer than any backtick run
in their text, and picks a code fence longer than any run in the payload.
Without this, literal metacharacters re-parsed as markup.

**Lightbox.** `crate::attachments::lightbox` is modal (`modal(true, close)`),
sized from the live `shell-root` rect (≤90% × 85%), closes on click/Escape and
hands focus back to the composer. It is exposed as
`transcript::lightbox_overlay(app, theme)` for the shell's overlay layer;
`shell::view` should push it next to `composer::popups` (the transcript still
mounts it in-tree as an interim fallback until that line lands).

**Attachments.** Pending `pending://{uploadId}/{name}` refs render the dimmer,
the transfer-percent ring (`App::transfer_percent`), or the indeterminate
spinner, over the loaded thumbnail, and a pulsing skeleton while loading. The
project-wide decoded-image cache is bounded by bytes (256 MiB) as well as entry
count (128).

**Working trailer.** The trailer reads `App::display_status_for` (Working), the
`App::pending_send_started` → turn bridge ("Sending…"), the degraded-path queued
line, and `App::send_undelivered` → the "Not delivered — click to retry"
affordance, whose retry restarts the grace clock and fires `RetryDelivery` via
`app.bridge.spawn`.
