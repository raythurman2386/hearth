# Changelog

Notable changes to **Hearth**. Format inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project is a fork of Zeron/comet; earlier history lives in git upstream. This file starts at the fork’s declared `0.1.0` line.

## [Unreleased]

## [0.2.10] - 2026-08-29

### Fixed

- **aarch64-linux cross build runs in CI.** The re-added `cross` build failed
  on the runner: the cross aarch64 image is an amd64 container, and its
  pre-build installs `:arm64` dev packages — `libglib2.0-dev` pulls in
  `python3:arm64`, whose postinst runs the arm64 `python3.12` binary. Without
  arm64 binfmt on the runner that postinst died with "Exec format error" and
  aborted the whole `apt-get install`. The workflow now registers arm64
  binfmt (`tonistiigi/binfmt`) before the cross build, so the postinst runs
  under qemu and the `hearth-<ver>-linux-aarch64.tar.gz` artifact builds.

## [0.2.9] - 2026-08-29

### Fixed

- **aarch64-linux releases are back.** The release workflow dropped the
  `aarch64-unknown-linux-gnu` cross build to cut Actions billing, so Raspberry
  Pi / Apple-Silicon-Linux users had no `hearth-<ver>-linux-aarch64.tar.gz` to
  install or self-update to. The build is re-added via `cross` (the
  `Cross.toml` config already existed), and `scripts/package-linux.sh` now
  takes an `ARCH` override so the packaging step emits the correct
  `linux-aarch64` tarball name on an x86_64 runner. `scripts/install.sh` and
  `scripts/write-manifest.sh` already handled aarch64, so `hearth update` on
  aarch64 Linux now works end to end.

## [0.2.8] - 2026-08-28

### Fixed

- **Windows builds again.** The instance-lock liveness probe called the
  Unix-only `libc::kill` unconditionally, so the `x86_64-pc-windows-msvc`
  build failed to compile `hearth-engine`. The probe is now gated to Unix
  (it is only ever consulted by the `flock`-based lock path), restoring the
  Windows release build.

## [0.2.7] - 2026-08-28

### Fixed

- **ACP mid-turn agent deaths are now diagnosable.** When an ACP agent
  process exits mid-turn, the surfaced error carried only the RPC-side
  "app-server exited before responding" while the child's exit status and
  stderr tail — already in hand — were dropped in a race between the EOF
  arm and the terminal bookkeeping. The EOF arm now reads the child and
  emits the Done with both attached (in-flight interrupts excluded: the
  escalation path keeps its single Done{Interrupted}).

## [0.2.6] - 2026-08-27

### Fixed

- **The IPC accept loop can no longer be wedged by a stalled client.** Every
  accepted TCP connection now runs its WebSocket handshake in a detached task
  with a 10s timeout; the listener itself never awaits a handshake. This is
  the 0.2.5 wedge: one client that connected and stalled mid-handshake left
  the IPC port with a listen-queue backlog and zero successful handshakes, and
  every headed launch failed with "not an engine; embedding instead" → the
  instance-lock rejection → app exit. Regression tests pin both properties
  (`crates/rpc/tests/handshake_stall.rs`).
- **Engine shutdown is bounded and exits cleanly.** The whole teardown races a
  5s budget (snapshot flush gets its own 4s bound, off the stage budget), and
  the chat2/registry sync actors now honor shutdown *during* a dial or
  reconnect backoff instead of after it — the 0.2.4 stop that hung 90s waiting
  on sync actors and died to systemd's SIGKILL is gone. Verified live: SIGTERM
  → exit in 0.10s, lock released, immediate relaunch succeeds.
- **Self-update restarts survive the cgroup kill.** The updater staged
  `systemctl --user restart hearth.service` as a child of the very unit being
  restarted, so the stop's SIGTERM killed the systemctl child and every
  auto-update logged "service restart failed" even though the service did
  restart. The restart is now staged into a transient systemd user unit
  (sibling cgroup) that performs it after this process exits; the direct call
  remains as a non-systemd fallback, and the macOS launchctl path is
  unchanged.
- **A dead engine's instance lock is stolen, not worshipped.** When the pid
  stamped on the lock is gone (or `/proc/<pid>/cmdline` is not a hearth
  process), `InstanceLock::acquire` now steals the lock instead of refusing to
  start. When the holder is genuinely alive but wedged (dial times out AND the
  lock is held), the UI now says so: "engine (pid N) appears wedged — run:
  systemctl --user restart hearth" instead of a bare bootstrap failure.
- **The packaged systemd unit stops in 15s, not ~90s** (`TimeoutStopSec=15`),
  so a hung shutdown escalates to SIGKILL inside the unit's own stop window.
- **Unobtainable chat2 row gaps now heal instead of freezing the transcript.**
  A row parked on causal deps the room can no longer deliver (the macpro
  chat-load stall: 2033-row checkpoint fetched, then rows parked forever at
  the same gap) exhausted the repair budget without end. Gap repair is now
  bounded *per gap*: after three failed repairs over the same cursor the
  client fires `GapUnrepairable` and the host rebuilds the doc from the
  checkpoint (retire + fresh open → checkpoint + rows re-import), cooled down
  to one attempt per 30s per chat. Parked rows now log seq, batch id, and
  device so dead gaps are diagnosable from logs alone.
- The Raven harness icon (agent pickers/settings) is the official mark from
  the raven repo — the previous hand-drawn 16px glyph rendered as an
  unrecognizable blob at icon sizes.

## [0.2.5] - 2026-08-27

### Fixed

- The hub now serves `GET /releases/*` from the device-level `{data_dir}/releases` —
  where `hearth release publish` writes — instead of the per-profile store root, so
  `hearth update` on peers (and the hub itself) no longer 404s with "hub has no
  releases" after a successful publish.
- Hardened the hub's `/releases/*` handler against absolute-path traversal
  (`GET /releases//etc/passwd` previously escaped the releases dir via
  `PathBuf::join` base replacement); such requests are now rejected with 400.
- Documented that `~/.hearth/env` is only read by the daemon unit: shells must
  source it for hand-run commands (`hearth update --check`) to see
  `HEARTH_TAILNET_HOST`.

## [0.2.4] - 2026-08-26

### Changed

- First-time install is a single prebuilt-binary step: the README now leads with
  `curl -fsSL …/scripts/install.sh | sh` instead of a source build, and the
  installer no longer auto-installs a systemd user service, enables linger, or
  probes for agent CLIs. The background engine stays opt-in via
  `hearth daemon install`.

## [0.2.3] - 2026-08-25

### Changed

- Release + update loop is closed for Linux: CI tags now publish
  `hearth-<ver>-linux-x86_64.tar.gz` plus `manifest.json` / `latest.txt` (required
  sha256s). On the hub, `hearth release publish --from github|dir` verifies and
  installs those into `{data_dir}/releases/`. Managed installs auto-update when
  idle by default (`HEARTH_AUTO_UPDATE=0` to opt out); the UI update strip applies
  managed Linux updates in-app; `scripts/install.sh` verifies checksums and can
  bootstrap from GitHub Releases when `HEARTH_BASE_URL` is unset.

## [0.2.2] - 2026-08-25

### Fixed

- Long silent tool calls (`cargo build`, lint, link) no longer look finished. ACP quiet-settle after an execute-kind tool waits at least 180s instead of synthesizing `Done` at 30s of no stdout, and a resume a few seconds after park keeps the normal 120s engine watchdog instead of the 20s self-continued window. That 20s path was oscillating Idle↔Working on the live hub and letting follow-up sends cancel the still-running tool.

## [0.2.1] - 2026-08-25

### Fixed

- The Unix-only `process_group` child-launch call in `source_control` is gated under `cfg(unix)` so the codebase compiles on non-Unix targets.
- The Windows build warnings are fixed for real: `on_selection_mouse_up` renames its unix-only `Context` param to `_cx`, and the `run_checked` audio helper is gated under `cfg(not(windows))` (it is only referenced by the macos/unix `run_player` branches).
- Fixed pre-existing clippy failures in `hearth-sync`/`hearth-doc` for real: `handle_conn`/`dispatch_http` take a shared `HubContext` (was `too_many_arguments`), the whois cache uses a `WhoisCacheEntry` struct (was `type_complexity`), and a `then(||…)` is `then_some(...)` (was `unnecessary_lazy_evaluations`). The lone remaining `result_large_err` allow on the tungstenite handshake callback is documented — `ErrorResponse` is a large type owned by the external crate.

### Changed

- The release workflow now builds the `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc` binaries (macOS and aarch64-linux cross builds were dropped to cut Actions billing); each release still publishes per-binary and archive checksums plus stable `hearth` / `hearth.exe`-named archives for the ACP registry. The Windows build now compiles end to end thanks to the `cfg` fixes above.

## [0.2.0] - 2026-08-24

### Changed

- Multi-device sync now runs over Tailscale instead of WorkOS + Cloudflare Durable Objects. Set `HEARTH_TAILNET_HOST` to the always-on hub's MagicDNS name; that host serves chat2/registry rooms and every device serves `/rpc` for direct peer control. Auth is `tailscale whois`. Attachments already transferred over RPC and now ride the same tailnet link. Updates are served from `{data_dir}/releases/` on the hub (`GET /releases/*`).
- Removed the TypeScript `edge/` Worker + Durable Objects package, live-edge tests, and WorkOS/Cloudflare env (`HEARTH_EDGE_*`, `HEARTH_WORKOS_*`) from the daemon unit. The curl installer now lives at `scripts/install.sh`.
- Dropped Claude Code, Cursor, Hermes, Pi, and OpenCode harnesses. Remaining agents are Raven (ACP, default), Codex (native app-server), Grok (ACP, including Ollama), and Mock (tests). Retired wire ids deserialize as Raven so old chat rows stay readable. Agent-account swap is Codex-only.

### Fixed

- Peer `/rpc` no longer falls back to the hub when a device’s friendly name does not match a Tailscale peer (wrong-host execution). Hub autodetection uses first-label equality so `mac` cannot claim `macbook…`. Hub WebSocket upgrades reject `Origin` (CSWSH), HTTP bodies are capped before buffering, and push acks use `serde_json`.
- Tailscale `whois` auth now accepts a numeric `Node.ID` (as live `tailscale whois --json` emits); treating it as a string previously failed hub peer auth and reset connections.
- Remote Tailscale hosts are woken via peer OpenChat RPC instead of the hub HTTP nudge (which 404s on the hub), so laptop→minis sends open the host chat and deliver. Legacy HTTP nudge remains a fallback, and wake is re-kicked when chat2 rows flush so flush alone is not treated as adoption.
- A chat2 row that parks on missing causal deps no longer advances the persisted cursor past it. The client restores the cursor to its pre-row value and requests a backfill repair so the parked row materializes; the engine sink persists cursor-1 for a parked row so on-disk cursor and content stay in lockstep. Fixes the empty-doc/advanced-cursor wedge seen on the 2026-08-24 hub.
- A GitHub CLI request timeout now kills the whole process group, preventing orphaned `mise` grandchildren from running forever at high CPU and accumulating across refresh cycles.

### Security

- Ignored `gpui`/`loro` transitive unsound advisories (`RUSTSEC-2026-0221`, `RUSTSEC-2023-0126`, `RUSTSEC-2026-0255`) in `cargo-audit`; they are warnings on pinned deps that cannot be bumped without a coordinated upgrade.

### Dependencies

- Bumped `tokio-tungstenite` 0.24 → 0.30, `portable-pty` 0.8 → 0.9, `base64` 0.22 → 0.23, `thiserror` 2.0.19 → 2.0.20, `pulldown-cmark` 0.12 → 0.13, `libc` 0.2.186 → 0.2.189, `tree-sitter` 0.26.11 → 0.26.12, `async-trait` 0.1.91 → 0.1.92, `loro` 1.13.7 → 1.13.9, and `ignore` 0.4.31 → 0.4.33.

### Documentation

- Docs and ARCHITECTURE updated for Tailscale hub-and-spoke and the Raven/Codex/Grok harness set. Historical Cloudflare/WorkOS notes are marked as such in `docs/PARITY.md` and the sync protocol docs.
- Added Raven-shaped user and contributor guides; documented that `hearth-sync` registry tests need `--features mock-server`.

## [0.1.0]

### Added

- Hearth fork identity: local-first defaults, Ravenwood theme, Raven as a first-class ACP harness.
- Composer **Mode** chip (`plan` / `agent` / `chat`) persisted on chat config / sticky defaults, sent to ACP via `session/set_mode`.

### Fixed

- Raven mode switching actually reaches the live session: mode is part of live runtime routing; Raven’s `mode` config option is applied from `RunRequest.mode`; chat-row rebuilds preserve mode; draft mode clears when changing the selected chat (so the Mode pill tracks the open chat).
