# Contributing

How to build and extend **Hearth**. For product behavior see [usage](usage.md); for layout of optional sync see [ARCHITECTURE.md](../ARCHITECTURE.md).

## Build

Requires a recent **stable** Rust toolchain (`rust-toolchain.toml` pins `stable` with `rustfmt` and `clippy`). gpui needs a working graphics stack (Vulkan/Metal as appropriate for the host).

```bash
# Debug
cargo build -p hearth

# Release (workspace release profile uses thin LTO + strip for distribution)
cargo build --release -p hearth

# Run headed
cargo run -p hearth

# Headless engine
cargo run -p hearth -- headless
```

Linux package tarball + managed `install.sh`:

```bash
scripts/package-linux.sh
# → target/package/hearth-<ver>-linux-<arch>.tar.gz
scripts/write-manifest.sh target/package <ver>   # manifest.json + latest.txt
```

Tag `v*` runs `.github/workflows/release.yml`, which builds the Linux updater tarball, Windows ACP binaries, `manifest.json`, and `latest.txt` onto the GitHub Release. On the hub:

```bash
hearth release publish --from github
```

macOS DMG packaging is `scripts/package-macos.sh` (must run on macOS; not in CI yet — see [dist/README.md](../dist/README.md)).

## Workspace layout

```
apps/hearth/          # Binary: CLI + headed/headless entry
crates/
  proto/              # Shared wire types (HarnessId, RunRequest, AgentEvent, …)
  doc/                # Loro-backed session/workspace documents
  harness/            # Agent adapters (ACP Raven/Grok, native Codex, mock)
  engine/             # Sessions, registry, docs host, repos, terminals, uploads
  rpc/                # Localhost JSON-RPC / WebSocket between UI and engine
  sync/               # Optional tailnet room clients + hub (off by default)
  ui/                 # gpui shell, composer, pickers, transcript, settings
  syntax/             # Highlighting helpers
  update/             # Self-update helpers
docs/                 # Product + research docs
scripts/              # Packaging and smoke helpers
```

## Style

- Edition comes from the workspace (`edition = "2024"` in workspace package metadata).
- Prefer small, reviewable diffs. Match neighboring comment style: short, factual, explain non-obvious constraints — do not narrate the change.
- Do not commit secrets, personal `~/.hearth` state, or generated `target/` artifacts.
- `cargo fmt` and `cargo clippy -p <crate>` for touched crates before sending a PR.

## Adding or changing a harness

1. Add or extend a type under `crates/harness` implementing `Harness`.
2. Register it in `crates/engine/src/registry.rs` (lazy descriptor + install probe + factory). Keep descriptor fields aligned with the live harness (tests assert stability for several adapters).
3. Wire UI affordances if needed (`crates/ui` icons, catalogs, Settings → Agents).
4. If the engine default env should accept a new id, extend `harness_from_env` in `apps/hearth/src/main.rs` (today: `raven`, `codex`, `grok`, `mock`).
5. Add harness tests under `crates/harness/tests/` with fixtures where possible (see [testing](testing.md)).

## ACP session modes

Raven-style `plan` / `agent` / `chat` flow through:

- `hearth_proto::SessionMode` on `RunRequest` / `ChatConfig`
- UI Mode chip in `crates/ui/src/pickers.rs`
- ACP apply path in `crates/harness/src/acp/mod.rs` (`session/set_mode` + config option category `mode`)
- Live routing in `crates/engine/src/sessions.rs` (`RuntimeConfig.mode`)

Permission-mode agents (Claude/Codex) share the ACP `category: "mode"` select but must **not** be fed Raven’s plan/agent/chat ids; the harness code branches on whether `plan`/`chat` appear in the advertised values.

## Documentation

User-facing claims must match the binary. Prefer linking to code or citing defaults that appear in `apps/hearth/src/main.rs` / harness specs. Update these guides when behavior changes (especially CLI surface, env vars, and mode semantics).

## Upstream lineage

Hearth is a fork of Zeron/comet. Large design docs under `docs/research/` and `ARCHITECTURE.md` still describe optional multi-device sync. When contributing local-first features, do not assume sync is on; default paths must work with no edge credentials.
