# Changelog

Notable changes to **Hearth**. Format inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project is a fork of Zeron/comet; earlier history lives in git upstream. This file starts at the fork’s declared `0.1.0` line.

## [Unreleased]

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
- The Unix-only `process_group` child-launch call in `source_control` is gated under `cfg(unix)`, fixing the Windows release build.
- The Unix-only `process_group` child-launch call in `source_control` is gated under `cfg(unix)`, fixing the Windows release build.
- The aarch64-linux cross build now pins the Ubuntu 24.04 cross image and installs the gpui system deps as multiarch `:arm64` packages, fixing the aarch64 release build.
- Silenced four pre-existing clippy lint failures in `hearth-sync`/`hearth-doc` (`too_many_arguments`, `type_complexity`, `result_large_err`, `unnecessary_lazy_evaluations`) so `cargo clippy --all-targets -- -D warnings` is clean.
- Shell-env imports are gated under `cfg(unix)` too, fixing a Windows build break.
- Repaired the CI and release workflows (format, lint, single test, audit; free disk space before the test build) and removed Windows dead code.

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
