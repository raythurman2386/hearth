# Hearth

A **local-first** control surface for coding agents (Raven, Claude Code, Codex, Cursor, Grok, Hermes, Pi, OpenCode) on Linux, macOS, and Windows.

Hearth is a fork of [**Zeron**](https://github.com/zeronsh/comet) — a native rewrite (Rust + [gpui](https://github.com/zed-industries/zed)) of a multi-device agent controller. This fork keeps the part that matters on a single machine: a fast, private harness runner you drive locally. No hosted account, no cloud sync, and no telemetry is required for normal use.

## What it is

- **Local-first by default** — a bare `hearth` run does not dial the edge.
- **One binary** — headed UI by default; `hearth headless` for an engine daemon the UI can attach to.
- **Harness-agnostic** — ACP agents over stdio plus native drivers (Claude Code, Codex, Cursor, OpenCode). **Raven** is registered first among real harnesses and is the usual new-chat fallback when it is installed.
- **Ravenwood themed** — UI colors follow the [Ravenwood](https://github.com/raythurman2386/ravenwood-vscode) palette.
- **Workspace-aware** — spaces (device + folder), optional git worktrees, diffs, session transcripts, steering, and a terminal pane.

Hearth is **not** the agent. Agents such as Raven own tools, sandboxes, and model providers. Hearth starts them, streams events, and records chats.

## Install / run

Build from source (Rust stable; agents need their own model endpoints, for example local Ollama for Raven):

```bash
cargo build --release -p hearth
./target/release/hearth          # headed UI
./target/release/hearth headless # engine only
```

Linux packaging into `~/.local`:

```bash
scripts/package-linux.sh
# extract the tarball under target/package/, then:
./install.sh
```

Or copy the binary:

```bash
install -Dm755 target/release/hearth ~/.local/bin/hearth
```

Data directory: `~/.hearth` (override with `HEARTH_DATA_DIR`).

## CLI surface

```bash
hearth              # UI (embeds engine or attaches to HEARTH_IPC_PORT)
hearth headless     # engine daemon
hearth status       # workspace / auth / engine snapshot
hearth sync         # sync room introspection (only useful if sync is enabled)
hearth login|logout # opt into / out of sync for the next engine start
hearth daemon …     # install/start/stop user service for headless
hearth update       # apply release update (--check to report only)
```

There is **no** `hearth -p` / `hearth --mode` one-shot interface. For agent CLI flags, use the agent itself (for example `raven --mode agent -p "…"`).

## Using Raven

1. Install [Raven](https://github.com/raythurman2386/raven) so `raven` is on PATH (or set `RAVEN_EXECUTABLE`).
2. Run `hearth`, pick harness **Raven**, set Mode to **Agent** when you want full tools (Raven’s default is **plan**, which is read-only until approval).
3. Send prompts from the composer. Models are live-probed from Raven’s ACP config (provider-qualified ids).

Details: [docs/usage.md](docs/usage.md), [docs/harnesses.md](docs/harnesses.md).

## Documentation

| Doc | Contents |
|---|---|
| [docs/README.md](docs/README.md) | Doc index |
| [docs/usage.md](docs/usage.md) | Day-to-day UI workflows and modes |
| [docs/configuration.md](docs/configuration.md) | Env vars, data dir, daemon, sync opt-in |
| [docs/harnesses.md](docs/harnesses.md) | Agent matrix and install notes |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Common failures |
| [docs/contributing.md](docs/contributing.md) | Build and layout for contributors |
| [docs/testing.md](docs/testing.md) | How to run tests |
| [docs/security.md](docs/security.md) | Threat model (honest scope) |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Engine / UI / optional edge topology |

## Sync

Sync is **off** by default. Upstream multi-device sync (Loro CRDT rooms, WorkOS auth, edge workers) remains in the tree for self-hosters. To opt in, set `HEARTH_WORKOS_CLIENT_ID` and/or `HEARTH_EDGE_TOKEN` (see [docs/configuration.md](docs/configuration.md)). A bare install never dials out for sync.

## Credit

Fork of **Zeron** by the **zeronsh** maintainers ([zeronsh/comet](https://github.com/zeronsh/comet)). This fork renames the product to Hearth, adds Raven as a first-class ACP harness, ports the Ravenwood theme, defaults to local-only operation, and drops mobile/web apps and cloud-centric CI from the default path.

Licensed under the [MIT License](LICENSE).
