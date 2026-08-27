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
| `HEARTH_HARNESS` | _(unset → `raven`)_ | Engine default harness id for chats **without** a config row. Kebab-case: `raven`, `codex`, `grok`, `mock`. |
| `RUST_LOG` | headed/headless: `info` (loro quieted); one-shot CLI: `warn` | `tracing` filter |

UI new-chat harness resolution prefers: chat config → sticky `composer-defaults.json` → first **offered** catalog entry (Raven is registered first among real agents when installed and enabled). The engine `HEARTH_HARNESS` default is a separate fallback for dispatch when a chat row has no harness.

### Sync (opt-in, Tailscale)

A bare run never dials a remote host. Multi-device sync requires Tailscale and an explicit opt-in:

| Variable | Meaning |
|---|---|
| `HEARTH_TAILNET_HOST` | MagicDNS name (or Tailscale hostname) of the always-on hub, e.g. `minis.tailnet.ts.net`. Non-empty enables sync. |
| `HEARTH_TAILNET_PORT` | Hub listen/dial port (default `27655`) |
| `HEARTH_TAILNET_HUB` | `1`/`true`/`yes` — this process hosts chat2/registry rooms. If unset, the engine matches `tailscale status` `Self` against `HEARTH_TAILNET_HOST`. |
| `HEARTH_DEVICE_NAME` | Optional device display name (engine + daemon install) |
| `HEARTH_ORG_ID` | Optional registry org key (default `dev-org`) |

Auth is Tailscale: inbound connections are identified with `tailscale whois`. There is no WorkOS, no `session.json`, and `hearth login`/`logout` are no-ops when the tailnet host is set.

On the hub, publish release artifacts into `{data_dir}/releases/` (served at `/releases/*`) so `hearth update` works:

```bash
# After CI finishes a v* tag (or with a local artifacts dir that has manifest.json):
hearth release publish --from github           # latest GitHub Release
hearth release publish --from github v0.2.3
hearth release publish --from dir ./artifacts  # local / gh release download
hearth release publish --from dir ./artifacts --check  # verify only
```

Override the GitHub repo with `HEARTH_RELEASE_REPO` (default `raythurman2386/hearth`). Attachments stay on the chat's host device and transfer over the same tailnet RPC link.

### Agent / adapter overrides

| Variable | Meaning |
|---|---|
| `RAVEN_EXECUTABLE` | Absolute path to the Raven binary (else PATH + known install dirs) |
| `GROK_EXECUTABLE` | Grok Build ACP binary override |
| `HEARTH_ADAPTERS_DIR` | Where managed npm adapters are installed |
| `HEARTH_NO_LOGIN_SHELL` | Set to a non-empty value to skip login-shell PATH snapshotting when resolving agent CLIs |

Harnesses that wrap CLIs also honor their own vendor env (for example Codex / Grok installs). Prefer putting agent binaries on the login shell PATH so headed apps launched from a desktop entry still find them.

### Tuning / diagnostics (advanced)

| Variable | Meaning |
|---|---|
| `HEARTH_ACP_PROMPT_STALL_MS` | Override ACP first-byte stall bound; `0` disables |
| `HEARTH_ACP_QUIET_SETTLE_MS` | Override quiet-settle bound; `0` disables |
| `HEARTH_ACP_EXEC_QUIET_SETTLE_MS` | After an execute-kind tool this turn, quiet-settle uses at least this window (default `180000`); `0` keeps the generic bound |
| `HEARTH_TURN_QUIESCE_MS` | Engine turn-quiesce watchdog; `0` disables |
| `HEARTH_SELF_TURN_QUIESCE_MS` | Shorter quiesce for genuine background-wake turns (default `20000`); `0` uses the normal window |
| `HEARTH_SELF_CONTINUED_SHORT_AFTER_MS` | Park duration before a resume uses the short self-continued window (default `30000`); `0` = always short |
| `HEARTH_AUTO_UPDATE` | Managed installs auto-apply when idle by default; set `0`/`false`/`no`/`off` to opt out |
| `HEARTH_RELEASE_REPO` | `owner/repo` for `hearth release publish --from github` (default `raythurman2386/hearth`) |
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

On Linux, the unit also reads `~/.hearth/env` at start (`EnvironmentFile=`), so you can edit that file and `hearth daemon restart` instead of reinstalling. It is not read by interactive shells — `hearth update --check` and other hand-run commands only see `HEARTH_TAILNET_HOST` if your shell profile sources it:

```bash
# ~/.bashrc / ~/.zshrc
[ -f "$HOME/.hearth/env" ] && . "$HOME/.hearth/env"
```

---

## Sync opt-in (summary)

| Goal | What to set |
|---|---|
| Local only (default) | Nothing |
| Tailnet sync (spoke) | `HEARTH_TAILNET_HOST=minis.YOUR-TAILNET.ts.net` |
| Tailnet sync (hub, always-on host) | same, plus `HEARTH_TAILNET_HUB=1` (or a hostname that matches this machine) |

Install Tailscale on every device, add them to the same tailnet, and allow TCP `27655` between them. A typical layout:

```
# on minis (always-on)
HEARTH_TAILNET_HOST=minis.YOUR-TAILNET.ts.net HEARTH_TAILNET_HUB=1 hearth daemon install

# on a laptop
HEARTH_TAILNET_HOST=minis.YOUR-TAILNET.ts.net hearth
```
