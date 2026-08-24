# Usage guide

Day-to-day workflows for **Hearth**. See the [root README](../README.md) for install and [configuration](configuration.md) for env vars and data paths.

## What Hearth is (and is not)

Hearth is a **desktop control surface**: sessions, composer, transcripts, diffs, terminals, and harness pickers. Coding agents run **outside** Hearth (for example `raven --acp`, Codex, Grok). Hearth starts them, streams events into a chat document, and lets you steer or stop a run.

Hearth’s CLI has **no** one-shot `-p` / `--mode` task flags. Those belong to agents such as Raven (`raven --mode agent -p "…"`). Running bare `hearth` opens the UI.

## Launch modes

| Command | What it does |
|---|---|
| `hearth` | Headed UI. Connects to a daemon on the IPC port if one is listening; otherwise runs the engine **in-process** and also serves that port. |
| `hearth headless` | Engine only (no UI). Other UIs can attach over localhost IPC. |
| `hearth status` | Workspace mode, optional auth, engine status. |
| `hearth sync` | Live sync room introspection (needs a running engine). Useful only when sync is enabled. |
| `hearth login` / `hearth logout` | Opt into / out of sync for the **next** engine start (see [configuration](configuration.md#sync-opt-in)). |
| `hearth daemon …` | Install/start/stop a user service for `hearth headless` (systemd `--user` / launchd). |
| `hearth update` / `hearth update --check` | Apply or check a newer release. |

Default IPC port: **27654** (`HEARTH_IPC_PORT`).

## First session

1. Install Hearth ([README](../README.md#install--run)) and at least one agent CLI (for Raven: [raythurman2386/raven](https://github.com/raythurman2386/raven)).
2. Run `hearth`.
3. Pick a **space** (a folder on a device). New chats need a project folder on the new-chat canvas.
4. Open the harness / model chips on the composer. Choose an installed agent (Raven is registered first among real harnesses and is the UI fallback when nothing is remembered yet and Raven is installed).
5. Type a prompt and send.

Sticky picks (harness, model, mode, favorites) live in `~/.hearth/composer-defaults.json` and restore on new chats. Enablement per device lives in `~/.hearth/harness-prefs.json` (Settings → Agents).

## Sessions and the composer

- **New chat** — canvas with space/project pickers and composer chips (mode, model, traits).
- **Existing chat** — transcript + composer; harness is usually locked to what the chat was created with.
- **Send** — queues a durable `Run` command. Mid-turn, the send button can become **Steer** (follow-up text delivered into the live run when the harness supports steering).
- **Stop** — interrupts the live run.

Attachments and `@` file mentions are supported on the composer; image bytes are staged through the engine’s upload path before the agent sees absolute paths.

## Interaction modes (Plan / Agent / Chat)

The composer **Mode** chip exposes `plan`, `agent`, and `chat`. Hearth sends the pick to ACP agents via `session/set_mode` and, when the agent advertises a plan/agent/chat config option (Raven does), also via `session/set_config_option`.

These ids match **Raven’s** interaction modes:

| Mode | Plan step? | Toolset (Raven) | Typical use |
|---|---|---|---|
| **Plan** | Yes | Read-only until the plan is approved | Default in Raven: propose, then execute |
| **Agent** | No | Full write / shell tools | Direct coding work |
| **Chat** | No | Read-only | Q&A without modifying the workspace |

Important truths:

- If the chip is still a dim **Mode** (no pick yet), Hearth sends `mode: null` and the **agent’s own default** applies. For Raven that default is **plan** (read-only tools until approval).
- **Chat** and **Plan** both look “read-only” to the model until plan approval. Prefer **Agent** when you want Raven to edit files and run shell without a plan gate.
- Changing mode on a **parked** ACP session restarts the harness runtime so the new mode is applied (same idea as changing model). Mid-turn steers only carry prompt text.
- For Claude/Codex, the ACP `category: "mode"` surface is a **permission / sandbox** select (`bypassPermissions`, `agent-full-access`, …), not Raven’s plan/agent/chat list. Hearth still shows the Mode chip; permission auto-picks for unattended runs stay on that Claude/Codex path and are not overwritten by a Hearth “Agent” label.

## Using Raven

Raven speaks ACP over stdio (`raven --acp`). Hearth spawns it as the **Raven** harness.

```bash
# Install Raven (separate project), then:
hearth
# Composer → harness Raven → Mode Agent → send
```

Override the binary with `RAVEN_EXECUTABLE` if `raven` is not on PATH (Hearth also searches `~/.cargo/bin`, `~/.local/bin`, Homebrew paths). Model lists come from Raven’s ACP `model` config option (provider-qualified ids such as `ollama/…`). Provider credentials and endpoints are Raven’s concern (`~/.raven/config.toml`, `OLLAMA_API_KEY`, etc.) — see Raven’s own docs.

## Worktrees, diffs, terminals

- **Checkout / worktree** chips on new chats can run in the space folder or create an isolated `hearth/<name>` worktree on send (host-side; see engine `WorktreeSpec`).
- The **changes** / diff pane reflects the working tree the session uses.
- A **terminal** pane is available for interactive shells on the host; it is separate from the agent’s own tool sandbox.

Exact UI affordances match the gpui shell; if a chip is missing, the harness or catalog may still be loading, or the agent may be disabled under Settings → Agents.

## Headless + remote UI (same machine)

```bash
hearth daemon install   # optional: systemd --user / launchd unit
hearth headless         # or rely on the service
# elsewhere:
hearth                  # attaches to the daemon on HEARTH_IPC_PORT
```

Daemon install captures current `HEARTH_*` env into the unit so PATH and edge overrides stick.

## Sync

Sync is **off** for a bare install. Multi-device CRDT sync remains in the tree for self-hosters; see [configuration](configuration.md#sync-opt-in) and [ARCHITECTURE.md](../ARCHITECTURE.md). Do not expect `hearth sync` to show live rooms unless you opted in.
