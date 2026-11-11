# Hearth — Rune UI port

Status: **foundation landed** on `port/rune`. `crates/ui-rune` compiles and renders
headlessly; `crates/ui` (gpui) stays as the reference until parity, then is deleted.

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
cargo test  -p hearth-ui-rune
# headless snapshots (state scenes: ready, failure, empty)
cargo run -p hearth-ui-rune --example smoke -- --snap /tmp/hearth.png --scene ready --size 1320x880 --scale 2
cargo run -p hearth-ui-rune --example smoke -- --tree /tmp/tree.json --scene ready
```

The tree JSON is the layout regression net: check rects, not just pixels.
`cargo test -p hearth-ui-rune` runs the ported pure-logic tests.

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
