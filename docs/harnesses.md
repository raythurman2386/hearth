# Harnesses

Hearth’s **harness** is the adapter that talks to a coding agent. The engine catalogs them lazily, reports whether the CLI is installed, and lets Settings → Agents opt some out.

Registry order among real agents starts with **Raven**, then Claude Code, Codex, Cursor, Grok, Hermes, Pi, OpenCode. Mock is a test-only harness (shown in the UI only when `HEARTH_HARNESS=mock`, or when it is the only way to keep a catalog non-empty in special cases).

## Matrix

| Id (`HarnessId`) | UI name | Wire | Typical binary / entry | Steering |
|---|---|---|---|---|
| `raven` | Raven | ACP `raven --acp` | `raven` (`RAVEN_EXECUTABLE`) | Turn boundary |
| `claude-code` | Claude Code | Native Claude stream-json CLI | `claude` | Step boundary |
| `codex` | Codex | Native Codex app-server | `codex` | Step boundary |
| `cursor` | Cursor | Cursor agent + local shim | Cursor CLI / shim | (adapter-defined) |
| `grok` | Grok | ACP `grok agent stdio` | `grok` (`GROK_EXECUTABLE`) | Turn boundary |
| `hermes` | Hermes | ACP `hermes acp` | `hermes` (`HERMES_EXECUTABLE`) | Turn boundary |
| `pi` | Pi | ACP via managed `pi-acp` | `pi` + `pi-acp` (`PI_ACP_EXECUTABLE`) | Turn boundary |
| `opencode` | OpenCode | Native HTTP/SSE (`opencode serve`) | `opencode` | Turn boundary |
| `mock` | Mock | In-process scripted events | _(none)_ | For tests / demos |

Ids are kebab-case on the wire (`claude-code`, not `ClaudeCode`).

## Install expectations

- **Raven** — install from the Raven project (`cargo install --path .` or Raven’s install script). No npm adapter. Hearth searches PATH, login-shell PATH, `~/.cargo/bin`, `~/.local/bin`, Homebrew paths.
- **Claude Code / Codex / Cursor / OpenCode / Hermes** — need their vendor CLIs on PATH (or discoverable via the login-shell snapshot). Some ACP wrappers are installed once into `~/.hearth/adapters` when npm is available.
- **Pi** — needs the pi CLI; Hearth can install the pinned `pi-acp` adapter when npm is present.
- **Grok** — needs the Grok Build / xAI agent binary that serves ACP (`grok agent stdio`).

`installed: false` in the catalog means Hearth will not offer that agent as runnable until the CLI appears (or you point an `*_EXECUTABLE` override at it).

## Mode chip vs agent behavior

The composer always shows a **Mode** chip (`plan` / `agent` / `chat`). Semantics depend on the agent:

### Raven (and similar ACP session modes)

Raven advertises both legacy `modes` and a `mode` config option with values `plan` | `agent` | `chat`. Hearth applies your pick with `session/set_mode` and `session/set_config_option`.

| Pick | Effect in Raven |
|---|---|
| Plan | Read-only tools + plan-first flow (Raven default if unset) |
| Agent | Full tools, no plan gate |
| Chat | Read-only tools, no plan gate |

### Claude / Codex permission modes

Those agents’ ACP `category: "mode"` options are **permission / sandbox** values (for example `bypassPermissions`, `agent-full-access`). Hearth’s unattended path prefers a no-prompt full-access choice when advertised. It does **not** map the UI labels Plan/Agent/Chat onto those permission ids (an overlapping value named `agent` on Codex is left alone so it is not confused with Raven’s Agent mode).

### Other harnesses

If the agent ignores `session/set_mode` / has no plan/agent/chat option, the chip still stores a preference on the chat config for ACP-capable runs, but the agent’s own default tooling rules apply.

## Models and traits

- Model lists are **harness-specific**. Raven’s picker is live-probed from ACP config options (provider-qualified ids). Claude/Codex/Grok expose their own catalogs / ladders.
- **Traits** (reasoning effort, fast mode, context window, …) appear only when the selected model advertises options or a reasoning ladder.
- Sticky last-model-per-harness is stored in `composer-defaults.json`.

## OpenCode note

OpenCode uses a **native** HTTP/SSE driver, not ACP. It is in the catalog and UI, but `HEARTH_HARNESS=opencode` is **not** wired in the env parser in `apps/hearth/src/main.rs` — pick OpenCode in the UI (or extend that match arm if you need an engine-default).
