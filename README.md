# Hearth

A **local-first** control surface for your coding agents (Raven, Claude Code, Codex, Cursor, Grok, Hermes, Pi) on Linux and Windows.

Hearth is a fork of [**Zeron**](https://github.com/zeronsh/comet) — itself a ground-up native rewrite (Rust + [gpui](https://github.com/zed-industries/zed), the same UI framework Zed uses) of the `zeron` multi-device agent controller. This fork is intentionally trimmed to the part that matters for a single machine: a fast, private, high-performance harness runner you drive locally. No hosted account, no cloud sync, no telemetry.

## What it is

- **Local-first by default** — every run is local. No sign-in, no network, no account.
- **One binary** (`hearth`) — run it headed (a full gpui UI) or headless (an engine daemon a UI can attach to).
- **Harness-agnostic** — drives any ACP v1 agent over stdio. Raven is wired in as a first-class harness; Claude Code, Codex, Cursor, Grok, Hermes, Pi, and opencode all speak the same protocol.
- **Ravenwood themed** — the UI carries the [Ravenwood](https://github.com/raythurman2386/ravenwood-vscode) emerald-forest color scheme, warm beige on deep olive.
- **Workspace-aware** — git worktrees, diff pane, session transcripts, per-session steering, and a real terminal view.

## Install / run

Build from source (requires Rust stable + a model endpoint for your agents, e.g. local Ollama):

```bash
cargo build --release
# headed UI
cargo run -p hearth
# or headless engine daemon
cargo run -p hearth -- headless
```

The binary lands in `target/release/hearth` (or `target/debug/hearth` in dev).

## Using Raven inside Hearth

Raven speaks ACP over stdio (`raven --acp`), so Hearth drives it natively:

```bash
hearth                    # open the UI, pick "Raven" as the harness
hearth --mode agent -p "Explain this repo"   # one-shot
```

The harness picker live-probes Raven's configured models (via Ollama) so you can switch provider/model per session.

## Sync

Sync is **off** by default in this fork. The upstream multi-device sync (Loro CRDT rooms, WorkOS auth, edge workers) is still wired into the codebase if you ever want to self-host it, but a bare `hearth` run never dials out. To opt in you'd set `HEARTH_WORKOS_CLIENT_ID` / `HEARTH_EDGE_TOKEN` — see the engine config in `apps/hearth/src/main.rs`.

---

## Credit

This project is a fork of **Zeron** by the **zeronsh** maintainers ([zeronsh/comet](https://github.com/zeronsh/comet)). The architecture — the gpui UI, the Loro CRDT sync layer, the ACP harness design, the sessions engine, the diff and terminal panes — is theirs. This fork:

- renames the product to **Hearth**,
- adds **Raven** as a first-class ACP harness,
- ports the **Ravenwood** theme,
- defaults to **local-only** operation,
- drops the mobile/web apps and cloud CI.

Big thanks to the upstream `zeronsh` authors for the original work. See `ARCHITECTURE.md` and `docs/` for the full design.

Licensed under the [MIT License](LICENSE).
