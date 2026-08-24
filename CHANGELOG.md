# Changelog

Notable changes to **Hearth**. Format inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project is a fork of Zeron/comet; earlier history lives in git upstream. This file starts at the fork’s declared `0.1.0` line.

## [Unreleased]

### Documentation

- Added a Raven-shaped docs set under `docs/`: usage, configuration, harnesses, troubleshooting, contributing, testing, security, and a docs index. Rewrote the root README so CLI claims match the binary (no fabricated `-p` / `--mode` one-shot).

## [0.1.0]

### Added

- Hearth fork identity: local-first defaults, Ravenwood theme, Raven as a first-class ACP harness.
- Composer **Mode** chip (`plan` / `agent` / `chat`) persisted on chat config / sticky defaults, sent to ACP via `session/set_mode`.

### Fixed

- Raven mode switching actually reaches the live session: mode is part of live runtime routing; Raven’s `mode` config option is applied from `RunRequest.mode`; chat-row rebuilds preserve mode; draft mode clears when changing the selected chat (so the Mode pill tracks the open chat).
