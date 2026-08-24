# Changelog

Notable changes to **Hearth**. Format inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project is a fork of Zeron/comet; earlier history lives in git upstream. This file starts at the fork’s declared `0.1.0` line.

## [Unreleased]

### Changed

- Multi-device sync now runs over Tailscale instead of WorkOS + Cloudflare Durable Objects. Set `HEARTH_TAILNET_HOST` to the always-on hub's MagicDNS name; that host serves chat2/registry rooms and every device serves `/rpc` for direct peer control. Auth is `tailscale whois`. Attachments already transferred over RPC and now ride the same tailnet link. Updates are served from `{data_dir}/releases/` on the hub (`GET /releases/*`).
- Removed the TypeScript `edge/` Worker + Durable Objects package, live-edge tests, and WorkOS/Cloudflare env (`HEARTH_EDGE_*`, `HEARTH_WORKOS_*`) from the daemon unit. The curl installer now lives at `scripts/install.sh`.
- Dropped Claude Code, Cursor, Hermes, Pi, and OpenCode harnesses. Remaining agents are Raven (ACP, default), Codex (native app-server), Grok (ACP, including Ollama), and Mock (tests). Retired wire ids deserialize as Raven so old chat rows stay readable. Agent-account swap is Codex-only.

### Fixed

- Peer `/rpc` no longer falls back to the hub when a device’s friendly name does not match a Tailscale peer (wrong-host execution). Hub autodetection uses first-label equality so `mac` cannot claim `macbook…`. Hub WebSocket upgrades reject `Origin` (CSWSH), HTTP bodies are capped before buffering, and push acks use `serde_json`.

### Documentation

- Docs and ARCHITECTURE updated for Tailscale hub-and-spoke and the Raven/Codex/Grok harness set. Historical Cloudflare/WorkOS notes are marked as such in `docs/PARITY.md` and the sync protocol docs.

## [0.1.0]

### Added

- Hearth fork identity: local-first defaults, Ravenwood theme, Raven as a first-class ACP harness.
- Composer **Mode** chip (`plan` / `agent` / `chat`) persisted on chat config / sticky defaults, sent to ACP via `session/set_mode`.

### Fixed

- Raven mode switching actually reaches the live session: mode is part of live runtime routing; Raven’s `mode` config option is applied from `RunRequest.mode`; chat-row rebuilds preserve mode; draft mode clears when changing the selected chat (so the Mode pill tracks the open chat).
