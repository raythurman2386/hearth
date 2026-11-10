# Hearth

A **local-first** control surface for coding agents (Huginn, Codex, Grok) on Linux, macOS, and Windows.

Hearth is a fork of [**Zeron**](https://github.com/zeronsh/comet) — a native rewrite (Rust + [gpui](https://github.com/zed-industries/zed)) of a multi-device agent controller. This fork keeps the part that matters on a single machine: a fast, private harness runner you drive locally. No hosted account, no cloud sync, and no telemetry is required for normal use.

## What it is

- **Local-first by default** — a bare `hearth` run does not dial the edge.
- **One binary** — headed UI by default; `hearth headless` for an engine daemon the UI can attach to.
- **Harness-agnostic** — ACP agents over stdio (Huginn, Grok) plus a native Codex driver. **Huginn** is registered first among real harnesses and is the usual new-chat fallback when it is installed.
- **Ravenwood themed** — UI colors follow the [Ravenwood](https://github.com/raythurman2386/ravenwood-vscode) palette.
- **Workspace-aware** — spaces (device + folder), optional git worktrees, diffs, session transcripts, steering, and a terminal pane.

Hearth is **not** the agent. Agents such as Huginn own tools, sandboxes, and model providers. Hearth starts them, streams events, and records chats.

## Install / run

One-line install (Linux, prebuilt binary — no build, no root):

```bash
curl -fsSL https://raw.githubusercontent.com/raythurman2386/hearth/main/scripts/install.sh | sh
```

This installs `hearth` into `~/.local/bin`. Then:

```bash
hearth          # headed UI
hearth headless # engine only
```

Make sure `~/.local/bin` is on your `PATH`. The background engine service is
opt-in: `hearth daemon install` (see [docs/configuration.md](docs/configuration.md)).

Build from source is a contributor path — see
[docs/contributing.md](docs/contributing.md). Agents need their own model
endpoints (for example local Ollama for Huginn).

Data directory: `~/.hearth` (override with `HEARTH_DATA_DIR`).

## CLI surface

```bash
hearth              # UI (embeds engine or attaches to HEARTH_IPC_PORT)
hearth headless     # engine daemon
hearth status       # workspace / device identity / engine snapshot
hearth sync         # sync room introspection (only useful if sync is enabled)
hearth daemon …     # install/start/stop user service for headless
hearth update       # apply release update (--check to report only)
hearth release publish  # on the hub: publish GitHub/local artifacts to /releases
```

There is **no** `hearth -p` / `hearth --mode` one-shot interface. For agent CLI flags, use the agent itself (for example `huginn --mode agent -p "…"`).

## Using Huginn

1. Install [Huginn](https://github.com/raythurman2386/huginn) — for example `cargo install --path .` from a Huginn checkout — so `huginn` is on PATH (or set `HUGINN_EXECUTABLE`).
2. Run `hearth`, pick harness **Huginn**, set Mode to **Agent** when you want full tools (Hearth starts Huginn in **plan** mode, which is read-only until approval).
3. Send prompts from the composer. Models are live-probed from Huginn’s ACP config (provider-qualified ids).

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

Sync is **off** by default. This fork syncs over **Tailscale** instead of a hosted account service: the tailnet is the trust boundary, and an always-on host (typically minis) serves the chat2/registry rooms.

To opt in, set `HEARTH_TAILNET_HOST` to the hub's MagicDNS name (see [docs/configuration.md](docs/configuration.md)). A bare install never dials out.

## Credit

Fork of **Zeron** by the **zeronsh** maintainers ([zeronsh/comet](https://github.com/zeronsh/comet)). This fork renames the product to Hearth, adds Huginn as a first-class ACP harness, ports the Ravenwood theme, defaults to local-only operation, and drops mobile/web apps and cloud-centric CI from the default path.

Licensed under the [MIT License](LICENSE).
