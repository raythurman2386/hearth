# Harnesses

Hearth’s **harness** is the adapter that talks to a coding agent. The engine catalogs them lazily, reports whether the CLI is installed, and lets Settings → Agents opt some out.

Registry order among real agents starts with **Huginn**, then Codex, then Grok. Mock is a test-only harness (shown in the UI only when `HEARTH_HARNESS=mock`, or when it is the only way to keep a catalog non-empty in special cases).

Retired wire ids (`claude-code`, `cursor`, `hermes`, `pi`, `opencode`) deserialize as **Huginn** so old chat rows stay readable. Hearth’s own legacy wire id `raven` also deserializes as **Huginn**.

## Matrix

| Id (`HarnessId`) | UI name | Wire | Typical binary / entry | Steering |
|---|---|---|---|---|
| `huginn` | Huginn | ACP `huginn --mode plan --acp` | `huginn` (`HUGINN_EXECUTABLE`) | Turn boundary |
| `codex` | Codex | Native Codex app-server | `codex` | Step boundary |
| `grok` | Grok | ACP `grok agent stdio` | `grok` (`GROK_EXECUTABLE`) | Turn boundary |
| `mock` | Mock | In-process scripted events | _(none)_ | For tests / demos |

Ids are kebab-case on the wire (`codex`, not `Codex`).

## Install expectations

- **Huginn** — install from the [Huginn project](https://github.com/raythurman2386/huginn) (`cargo install --path .` from a checkout, or Huginn’s install script). No npm adapter. Hearth launches it with `--mode plan --acp` and searches PATH, login-shell PATH, `~/.cargo/bin`, `~/.local/bin`, Homebrew paths.
- **Codex** — needs the Codex CLI on PATH (or discoverable via the login-shell snapshot).
- **Grok** — needs the Grok Build / xAI agent binary that serves ACP (`grok agent stdio`). Works with xAI cloud or a local Ollama-backed grok install. The npm wrapper (`@xai-official/grok`) can be installed once into `~/.hearth/adapters` when npm is available.

`installed: false` in the catalog means Hearth will not offer that agent as runnable until the CLI appears (or you point an `*_EXECUTABLE` override at it).

## Mode chip vs agent behavior

The composer always shows a **Mode** chip (`plan` / `agent` / `chat`). Semantics depend on the agent:

### Huginn (and similar ACP session modes)

Huginn advertises both legacy `modes` and a `mode` config option with values `plan` | `agent` | `chat`. Hearth applies your pick with `session/set_mode` and `session/set_config_option`.

| Pick | Effect in Huginn |
|---|---|
| Plan | Read-only tools + plan-first flow (Hearth starts Huginn in plan mode) |
| Agent | Full tools, no plan gate |
| Chat | Read-only tools, no plan gate |

### Codex permission modes

Codex’s ACP `category: "mode"` options are **permission / sandbox** values (for example `bypassPermissions`, `agent-full-access`). Hearth’s unattended path prefers a no-prompt full-access choice when advertised. It does **not** map the UI labels Plan/Agent/Chat onto those permission ids (an overlapping value named `agent` on Codex is left alone so it is not confused with Huginn’s Agent mode).

### Other harnesses

If the agent ignores `session/set_mode` / has no plan/agent/chat option, the chip still stores a preference on the chat config for ACP-capable runs, but the agent’s own default tooling rules apply.

## Models and traits

- Model lists are **harness-specific**. Huginn’s picker is live-probed from ACP config options (provider-qualified ids). Codex/Grok expose their own catalogs / ladders.
- **Traits** (reasoning effort, fast mode, context window, …) appear only when the selected model advertises options or a reasoning ladder.
- Sticky last-model-per-harness is stored in `composer-defaults.json`.
