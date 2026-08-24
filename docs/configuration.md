# Configuration guide

Hearth has **no** primary TOML config file like Raven. Behavior is controlled by:

1. **Environment variables** (and values baked into a `hearth daemon install` unit)
2. **Files under the data directory** (`~/.hearth` by default)
3. **In-app settings** (appearance, agents enablement, composer defaults)

There is no `HEARTH_CONFIG` overlay today. `RUST_LOG` filters tracing.

---

## Data directory

| Path | Meaning |
|---|---|
| `~/.hearth` | Default data dir (`HEARTH_DATA_DIR` overrides) |
| `~/.hearth/logs/` | Per-launch log files for headed/headless (previous launch kept as `.old`) |
| `~/.hearth/composer-defaults.json` | Sticky harness / model / mode / favorites |
| `~/.hearth/harness-prefs.json` | Per-device agent opt-outs (Settings → Agents) |
| `~/.hearth/ui-settings.json` | Shell UI prefs (tabs, appearance-related state) |
| `~/.hearth/adapters/` | Managed npm ACP adapter installs (`HEARTH_ADAPTERS_DIR` overrides) |
| `~/.hearth/device-id` | Stable device identity |
| `~/.hearth/engine.lock` | Single-instance data-dir lock |

On first run, if `~/.hearth` is missing but `~/.comet-native` exists, Hearth **renames** the old directory into `~/.hearth` (pre-rename migration).

Worktrees created for runs can be rooted under `HEARTH_WORKTREES_DIR` when set (tests and advanced hosts); otherwise the engine uses its normal checkout layout under the space’s repo.

---

## Environment variables

### Core

| Variable | Default | Meaning |
|---|---|---|
| `HEARTH_DATA_DIR` | `~/.hearth` | Engine + UI state root |
| `HEARTH_IPC_PORT` | `27654` | Localhost WebSocket port for UI ↔ engine |
| `HEARTH_HARNESS` | _(unset → `claude-code`)_ | Engine default harness id for chats **without** a config row. Kebab-case: `raven`, `claude-code` (via default), `codex`, `cursor`, `grok`, `hermes`, `pi`, `mock`. **Note:** `opencode` is registered in the catalog but is **not** accepted by this env parser today. |
| `RUST_LOG` | headed/headless: `info` (loro quieted); one-shot CLI: `warn` | `tracing` filter |

UI new-chat harness resolution prefers: chat config → sticky `composer-defaults.json` → first **offered** catalog entry (Raven is registered first among real agents when installed and enabled). The engine `HEARTH_HARNESS` default is a separate fallback for dispatch when a chat row has no harness.

### Sync (opt-in)

A bare run never dials the edge. Sync requires an explicit opt-in:

| Variable | Meaning |
|---|---|
| `HEARTH_WORKOS_CLIENT_ID` | Non-empty → WorkOS AuthKit client for real sign-in; empty string forces “no client” |
| `HEARTH_EDGE_TOKEN` | Dev bearer (no WorkOS); enables sync in development scope when set |
| `HEARTH_EDGE_URL` | Edge base URL (default `https://edge.hearth.sh`) |
| `HEARTH_ORG_ID` | Org scope for workspace rooms in dev / when needed |
| `HEARTH_WORKOS_API_BASE` | Override WorkOS API base when using real auth |
| `HEARTH_CALLBACK_PORT` | Auth callback port (engine + captured by daemon install) |
| `HEARTH_DEVICE_NAME` | Optional device display name (engine + daemon install) |

`hearth login` / `hearth logout` rewrite the saved session used on the **next** engine start. They do not swap a live engine’s storage mid-flight. See [ARCHITECTURE.md](../ARCHITECTURE.md) § Local-first workspace profiles.

### Agent / adapter overrides

| Variable | Meaning |
|---|---|
| `RAVEN_EXECUTABLE` | Absolute path to the Raven binary (else PATH + known install dirs) |
| `GROK_EXECUTABLE` | Grok Build ACP binary override |
| `HERMES_EXECUTABLE` | Hermes ACP binary override |
| `PI_ACP_EXECUTABLE` | `pi-acp` adapter override |
| `HEARTH_ADAPTERS_DIR` | Where managed npm adapters are installed |
| `HEARTH_NO_LOGIN_SHELL` | Set to a non-empty value to skip login-shell PATH snapshotting when resolving agent CLIs |

Harnesses that wrap CLIs also honor their own vendor env (for example Claude / Codex / Cursor / OpenCode installs). Prefer putting agent binaries on the login shell PATH so headed apps launched from a desktop entry still find them.

### Tuning / diagnostics (advanced)

| Variable | Meaning |
|---|---|
| `HEARTH_ACP_PROMPT_STALL_MS` | Override ACP first-byte stall bound; `0` disables |
| `HEARTH_ACP_QUIET_SETTLE_MS` | Override quiet-settle bound; `0` disables |
| `HEARTH_OPENCODE_STALL_MS` / `HEARTH_OPENCODE_STARTUP_TIMEOUT_SECS` | OpenCode native driver stalls / startup |
| `HEARTH_AUTO_UPDATE` | `1`/`true`/`yes` — headless may auto-apply updates |
| `HEARTH_OPEN_PICKER` | Dev: `model` \| `traits` \| `repo` \| `branch` — open a picker on boot |
| `HEARTH_MOCK_*` | Mock harness scripting knobs (`DELAY_MS`, `QUESTION`, `REPEAT`, …) when `HEARTH_HARNESS=mock` |
| `HEARTH_FRAME_STATS` / `HEARTH_NO_RENDER_CACHE` | UI render diagnostics |

---

## In-app settings

- **Settings → Agents** — enable/disable harnesses on this device (`harness-prefs.json` stores **opt-outs**; newly detected installs turn on by default except Mock).
- **Composer chips** — mode, model, traits; last picks persist in `composer-defaults.json`.
- **Appearance** — theme / system appearance (Ravenwood palette is the fork’s default look).

---

## Daemon service

```bash
HEARTH_HARNESS=raven hearth daemon install
hearth daemon status
hearth daemon restart
hearth daemon uninstall
```

Install records the `hearth` binary path and a fixed allowlist of `HEARTH_*` / logging variables from the install-time environment. Changing env later requires reinstall (or editing the unit) and a restart.

---

## Sync opt-in (summary)

| Goal | What to set |
|---|---|
| Local only (default) | Nothing; do not set WorkOS / edge token |
| Dev sync against a self-hosted or smoke edge | `HEARTH_EDGE_TOKEN=…` (and usually `HEARTH_EDGE_URL`) |
| WorkOS-backed sync | `HEARTH_WORKOS_CLIENT_ID=…`, then `hearth login` |

This fork does **not** bake a production WorkOS client id. If you do not set the above, the edge is never dialed.
