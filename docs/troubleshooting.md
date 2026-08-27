# Troubleshooting

If your issue is not listed, capture logs from `~/.hearth/logs/` (headed/headless keep a file per launch) and/or run with `RUST_LOG=hearth_harness=debug,hearth_engine=debug`.

---

## Raven says it only has read-only tools

Usually one of:

1. **Mode is Plan or Chat** (or unset). Raven’s default is **plan**, which is read-only until a plan is approved. **Chat** is also read-only. Pick **Agent** on the Mode chip, then send again.
2. **Mode changed on a live parked session without a restart.** Current Hearth treats a mode change like a model change and should replace the runtime on the next `Run`. If you only steered mid-turn, the steer carries prompt text only — send a normal turn after the mode pick.
3. **Stale binary.** Rebuild/reinstall Hearth if you are on a build from before the mode-routing fix (`session/set_mode` + Raven `mode` config option + runtime config includes mode).

Verify the chip label shows **Agent** (not a dim **Mode**) before sending.

---

## Mode chip does not match the selected chat

Draft picks clear when you change the selected chat. If a chip still looks wrong:

- Confirm `~/.hearth/composer-defaults.json` vs the chat’s saved `config.mode`.
- Open the Mode popover — the checkmark follows `effective_mode` (draft → chat config → sticky default).

---

## “No agents” / harness missing from the picker

1. Settings → Agents — the harness may be disabled (`harness-prefs.json` opt-out).
2. CLI not installed / not on PATH. Desktop launches often see a thinner PATH than your terminal; Hearth snapshots the **login shell** environment unless `HEARTH_NO_LOGIN_SHELL` is set.
3. Set an override: `RAVEN_EXECUTABLE=/path/to/raven`, `GROK_EXECUTABLE=…`, etc.
4. For managed adapters (the Grok npm wrapper), ensure `npm` works; installs land under `~/.hearth/adapters` (or `HEARTH_ADAPTERS_DIR`).

---

## Agent not found / spawn failed

- Run the CLI yourself in a terminal (`raven --acp`, `codex`, `grok agent stdio`, …) to confirm it starts.
- Check `~/.hearth/logs/` for the harness spawn error and stderr tail.
- ACP agents must speak JSON-RPC over stdio; a binary that only opens a TUI will hang or fail the handshake.

---

## UI cannot attach / “no engine listening”

```text
no engine listening on 127.0.0.1:27654
```

- Start `hearth` (headed embeds and serves the engine) or `hearth headless` / `hearth daemon start`.
- Check `HEARTH_IPC_PORT` matches between UI and daemon.
- Another process may hold the port; headed UI still opens if bind fails, but peers cannot attach.

`hearth status` and `hearth sync` also need a live engine on that port.

---

## Sync / login confusion on a local-only install

Bare Hearth **does not** dial the tailnet. If you never set `HEARTH_TAILNET_HOST`, `hearth login` is not required for normal local use. `hearth sync` will not show useful rooms without an opted-in, running engine that can reach the hub.

## `hearth update --check` says HEARTH_TAILNET_HOST is not set (on the hub)

`~/.hearth/env` is read by the daemon unit (`EnvironmentFile=`), not by your shell. Source it from your shell profile (`[ -f "$HOME/.hearth/env" ] && . "$HOME/.hearth/env"` in `~/.bashrc`/`~/.zshrc`), or export the variable per command.

If the variable is set but you instead get `hub has no releases`, the hub daemon predates the device-level `/releases/` fix — `hearth daemon restart` with a current build, and confirm `~/.hearth/releases/` holds `manifest.json` + `latest.txt` (`hearth release publish --from github`). Check with: `curl -fsS http://<hub-host>:27655/releases/latest.txt`.

---

## Package install / desktop entry

Linux (one-line, prebuilt):

```bash
curl -fsSL https://raw.githubusercontent.com/raythurman2386/hearth/main/scripts/install.sh | sh
```

Ensure `~/.local/bin` is on PATH. After replacing the binary, **fully quit** any running Hearth (including a headless daemon) so the new file is what launches next — Linux can refuse to overwrite a running text image (`ETXTBSY`).

---

## Worktree / cwd surprises

- New chats need a project space on the canvas (project-less `~` minting from the canvas is blocked).
- Isolated worktree runs are created on the **host** when the send carries a worktree spec; an older peer that ignores the field would run in the main checkout instead.
- Agent tools still obey **that agent’s** sandbox, not Hearth’s UI folder picker alone.

---

## Tests or CI need the mock harness

```bash
HEARTH_HARNESS=mock hearth
```

Mock is hidden from normal picker offers unless that env is set (or special catalog conditions apply).

---

## Still stuck

1. `hearth status`
2. Newest file under `~/.hearth/logs/`
3. `RUST_LOG=debug` reproduction
4. Open an issue with OS, install method, harness id, and whether Mode was Plan/Agent/Chat
