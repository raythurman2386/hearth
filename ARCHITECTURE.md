# hearth — Architecture

User and contributor guides live under [`docs/`](docs/README.md) (usage, configuration, harnesses, troubleshooting). This file is the **system architecture** note for the engine/UI/optional-tailnet topology. Sync paths described below are **opt-in** in this fork; a bare install stays local-only.

A ground-up native rewrite (Zeron/comet lineage) of a multi-device controller for coding agents
— in Rust, with a Rune UI. Fresh app; no backwards compatibility required.

**Pillars (from the goal):**
- Optional sync uses Loro CRDT docs (loro-mirror model) over the tailnet; the same docs persist locally when sync is disabled.
- Room servers (chat2 log + registry) run in Rust on the always-on hub. Everything device-side is Rust.
- Feature parity with hearth **except token-usage display** (poor fit for CRDTs; excluded).
- Frontend is **Rune** (pinned dev rev). Virtualization + markdown techniques ported from
  **mugen + pretext** (`docs/research/mugen-pretext.md`).
- One binary, **headed or headless**. Smooth transitions/animations matching the original
  (catalog in `docs/research/feature-inventory.md` §1.12).

## 1. Topology (unchanged shape, new materials)

```
Rune UI ─ localhost RPC ─ engine A ══ WS over tailnet ══ engine B (hub) ─ RPC ─ Rune UI
                                │  HEARTH_TAILNET_HOST:PORT
                                └── chat2 + registry rooms on the hub
                                └── /rpc on every device (direct peer RPC)
```

- **Engine = backend** (was `@hearth/backend`): runs agents, owns auth, terminals, repos/worktrees,
  diff sync, doc hosting. Pure Rust daemon, fully functional headless.
- **UI = viewport** (was Electron): Rune app rendering engine state. Talks the same typed RPC whether the engine is in-process or a separate daemon. Organized around **spaces** — (device, folder) pairs in the runtime's profile. The sidebar is the data: an attention-sorted Sessions list, filtered by a searchable spaces dropdown ("All spaces" included) that also hosts space management. The horizontal tabs are a **device-local viewport** onto that list (`ui-settings.json` `openTabs`, cross-space): closing a tab is local-only — archiving is an explicit sidebar action — and a sidebar click (re)opens a session as a tab. The new-session canvas carries a space picker (defaulting to the sidebar filter, else the last selected space); new sessions are minted onto the picked space's device via relay-forwardable RPCs.
- **Tailnet hub (Rust, `crates/sync`)**: the always-on host binds one TCP port and serves chat2
  rooms, registry rooms, optional `/releases/*`, and (on every device) direct `/rpc`. Identity is
  `tailscale whois`. There is no Cloudflare Worker, Durable Objects package, or WorkOS login on
  this path.

### Headed / headless
Single binary `hearth`:
- `hearth` — headed. If a local engine daemon is already listening on the IPC port, connect to it;
  otherwise run the engine **in-process** (RPC over an in-memory duplex — same protocol, zero
  serialization shortcuts, so the boundary stays honest) **and serve that same engine on the IPC
  port**. The embedded engine is not private: any other viewport can attach to the running app
  without it first being restarted as a daemon. Binding is best-effort — if the port is taken the
  window still opens, having lost only the ability to host peers.
- `hearth headless` — engine only. A clean installation serves its local profile over localhost IPC.
  With `HEARTH_TAILNET_HOST` set it also dials the hub (and may host rooms when this machine is the
  hub). A mini PC can run the hub while a laptop UI drives remote spaces over the tailnet.

### Local-first workspace profiles

The device is its own identity: there is no account service, sign-in, or
organization selection. Authentication is the tailnet (`tailscale whois` on the
hub); a bare install simply never dials out.

- `AuthState` is the live status broadcast the viewports share: `SignedOut` or `SignedIn { user }`. The engine boots signed in as the device identity.
- `WorkspaceScope` is the immutable storage boundary captured once at engine startup: `Local` or explicit `Development` (the shipped device/tailnet profile).

The engine never re-resolves an open store because `AuthState` changed.

| Startup condition | `WorkspaceScope` | Online transports |
| --- | --- | --- |
| No `HEARTH_TAILNET_HOST` and no explicit bearer | `Development` | Disabled |
| `HEARTH_TAILNET_HOST` set | `Development` (dummy bearer) | Enabled — dials the hub; hub bind when this host matches |
| `HEARTH_EDGE_TOKEN` set without a tailnet host | `Development` | Enabled — dials `HEARTH_EDGE_*` / the configured endpoint |

`hearth status` reports the resolved scope, device identity, and tailnet posture.

The resolved profile selects the session snapshots, registry snapshot, run journals, and attachment cache that may contain workspace data:

| Scope | Store and journals | Uploads |
| --- | --- | --- |
| `Local` | `{data_dir}/profiles/local/` | `{data_dir}/profiles/local/uploads/` |
| `Development` | `{data_dir}/orgs/{org_id}/{user_id}/` | `{data_dir}/orgs/{org_id}/{user_id}/uploads/` |

The development store root preserves the historical account-scoped layout while its attachment cache is account-scoped. Local identity lives in `{data_dir}/local-profile.json`; its UUID is stable across restarts and is not the development identity.

Older releases wrote every account-scoped attachment to `{data_dir}/uploads/`, and persisted those absolute paths in transcripts. On upgrade, the first development account that opens this legacy cache claims it in `{data_dir}/legacy-uploads-owner.json`. That account may read the cache as a compatibility fallback, but all new staging and commits use its account-scoped uploads root; other accounts cannot read or write the legacy cache.

Device identity and machine resources remain device-scoped under the common data directory: `device-id`, repository registration, managed worktrees, agent credentials/accounts, and UI settings. They are available across profiles, but they do not contain or expose another profile's transcripts or attachments.

#### Privacy boundary and follow-ups

Local attachments remain jailed under the local upload root and are not readable through the development attachment cache. Local sessions are never uploaded implicitly: only the profile the runtime was assembled with is open.

The following product work is intentionally deferred:

1. Explicit session selection and copy between local and development profiles, including attachment copying, provenance, and conflict behavior.
2. Browsing both scopes simultaneously or switching the visible scope without restarting the engine.
3. A supported self-hosted backend contract covering room APIs, authorization, persistence, and blob storage. Current endpoint and bearer overrides remain development/deployment seams, not a promised compatibility surface.

## 2. Data model — all Loro

Two persistent doc kinds. When sync is enabled, session docs ride the chat2 row protocol (loro updates as append-only rows + Range-resumable checkpoints, ChatRoom DO) and the registry rides its own row-frame protocol; local-only profiles persist the same docs without joining rooms:

1. **Session doc** (per chat) — the transcript + durable command queue. Schema is a Rust port of
   `packages/session-doc` (same container names/shapes so the edge's tail materializer keeps
   working): `meta` map, `messages` list (parts as list-of-maps with **LoroText bodies** — the
   measured 1.03× oplog shape; never LWW value rewrites), `commands` list with ledger rules 1–3
   (append-only per-device entries; host-only outcomes; dedupe/TTL/supersede evaluation).
   Continuation splitting at 256KB, render-only tool parts (full inputs stay in the host's local
   run journal), tail/diff sidecars. Constants carried over (`STREAM_COMMIT_MS=120`,
   `DO_FLUSH_MS=5s`, compaction at 8MB, retain 30d, tail 64).

2. **Workspace registry doc** (per profile) — the `registry1` snapshot stores spaces (id, deviceId, path, name?, gitDetected, checkoutId), the chats index (id, deviceId, title, archived, cwd, branch, checkoutId, spaceId, lastSeenAt, lastMessagePreview/At, config), devices, session-status rows, and checkout-diff summary pointers. A space is a device+folder pair in the active profile; the owning device's `SpacesSync` stamps git presence so branch pickers and the diff sidebar can gate without another RPC. Local scope keeps the registry entirely in its profile store. The development (tailnet) scope joins `/registry/{orgId}/ws`, backed by the private per-user room `reg1/{orgId}/{userId}`; rows are never visible to every member of an organization.

   Writer discipline: each device writes its own device and session-status rows, rows for chats it hosts, and git stamps for spaces it owns. Creates, renames, archives, and seen marks are LWW sets accepted from any device. `deleteSpace` tombstones the space and every chat/session row in it in one commit. Presence uses ephemeral room frames rather than durable heartbeat writes.

   *Why one registry and not N tiny docs:* the sidebar needs one subscription for the whole list (grouping, resort animations, unseen markers). Its rows contain indexes rather than transcripts, so one local snapshot and, when enabled, one room connection remain bounded and cheap.

3. **Mirror layer** (`hearth-doc` crate) — Rust equivalent of loro-mirror: typed structs for the
   schema, **incremental** application of `doc.subscribe` diffs into cached state (no full
   re-hydration per change — this is also what fixes hearth's known O(transcript) re-projection
   inefficiency, remaining-work item 1a), and a diff-reconcile write path (evaluate `lorosurgeon`
   0.2.x as a dep; our schema is small enough to hand-roll if it doesn't fit). The UI renders
   mirror state directly with per-entry change notifications — the "endgame" the TS
   implementation documented but never reached.

### Command plane
Send/steer/interrupt/respondInput = durable command entries in the session doc (`QueueCommand`),
executed by the chat's **host** device (executor gated on chat ownership; mark-processed BEFORE
execute; steer with no live run dispatches as the next turn). Offline sends queue in the doc.
This is hearth's proven design, kept verbatim.

## 3. Cargo workspace

```
hearth/
  Cargo.toml                 # workspace
  crates/
    proto/        hearth-proto    # wire types: AgentEvent, ToolCall, RunRequest, Model,
                                 # entities, RPC envelopes (serde; ndjson framing);
                                 # `view` = the pure derivations both frontends share
                                 # (sort orders, staleness gating, grouping, boot gate)
    doc/          hearth-doc      # session-doc + workspace-registry schemas, mirror layer,
                                 # parts fold, continuations, command ledger, sidecars
    sync/         hearth-sync     # loro room client (join/VV backfill/fragments/backoff),
                                 # ephemeral presence, DocsStore (SQLite snapshots +
                                 # processed-command ledger)
    harness/      hearth-harness  # Harness trait + ACP (Huginn, Grok),
                                 # Codex (app-server JSON-RPC), mock; steering mailbox,
                                 # requestInput, models/reasoning/options catalogs
    engine/       hearth-engine   # sessions engine (pub/sub, run journal, recovery, stall
                                 # watchdog), doc host + command executor, repos/worktrees,
                                 # checkout-diff sync, terminals (portable-pty), uploads,
                                 # agent accounts (Codex cred swap), Tailscale identity,
                                 # device peer links
    rpc/          hearth-rpc      # UiRpc/ControlRpc: typed req/resp/stream over WS (tokio-
                                 # tungstenite) + in-memory transport; direct tailnet /rpc
                                 # peer links
    ui-rune/      hearth-ui-rune  # Rune app: shell, sidebar, conversation, composer,
                                 # terminal view, diff pane, settings, animation kit
  apps/
    hearth/                       # the binary (headed default, `headless` subcommand)
  docs/                          # this file + research reports
```

Engine async runtime: **tokio** throughout; the UI runs a private tokio runtime
(`hearth_ui_rune::bridge::Bridge`) and posts `EngineEvent`s to the Rune `poll`.
In-process mode runs the engine on its own tokio runtime thread; the UI never
blocks on it.

## 4. UI (Rune) — parity + smoothness

Reference: `docs/rune-port.md` (port architecture, ownership, verification),
feature spec `docs/research/feature-inventory.md` §1, `docs/research/mugen-pretext.md`.

- **Deps**: `rune`, `rune-kit`, `rune-motion` pinned to one `dev` rev. The
  engine/RPC/doc/sync crates are UI-agnostic; `crates/ui-rune` is the only
  frontend. No Zed/GPL crates.
- **Model**: one `App` state, `fn view(&App, &mut Cx<App>) -> El`, handlers
  `|&mut App|` / `&mut Tx`, and a retained element tree diffed by Rune (Taffy
  layout, springs, Parley text). Async work posts `EngineEvent`s from a private
  tokio runtime; `AppSpec::poll` drains them, and `WatchGuard` drop cancels the
  engine-side watch.
- **Transcript**: Rune `scroll(ScrollSpec{stick_bottom})` + `growth(GrowthSpec)`
  + `virtual_rows`, keyed by block id (`msgId#blockId`), with own-turn runway,
  per-chat viewport restore, block-granularity rows, tool groups/chips, message
  rail, and optimistic echoes.
- **Markdown**: `pulldown-cmark` incremental parser + display mender, rendered
  through `rune_kit::Markdown` with `hearth_syntax` paint-only colours; streaming
  reveal; **numbers drive layout, colors are paint**.
- **Composer**: Rune `TextEdit` / `text_input` (IME, selection, undo, pills),
  compact↔expanded hysteresis, auto-grow 76–260px, Send→Steer→Stop morph,
  per-chat drafts + attachments, question wizard, mention/slash popups.
- **Terminal**: `alacritty_terminal` emulator + engine-side PTY; Rune rows of
  `rich` spans over background/selection quads; tabs with drag-reorder,
  160px–55vh, 12ms input coalescing / 80ms resize debounce, 1MB replay,
  detach ≠ close.
- **Diff pane**: unified/split patch model → virtualized line rows, per-file
  folds, inline comments, `hearth_syntax` highlights, scope/wrap controls and a
  git-history lane graph.
- **Animation**: `rune_motion` springs/transitions; the hearth catalog (fade-in,
  splash-out, pulse, spinner, menu/dialog, 200/260/150ms transitions) is encoded
  as springs/curves with reduced-motion.
- **Theme**: the same Ravenwood dark/light palette as `rune::Color`, plus a
  `rune_kit::Kit` derived for components; Geist/Geist Mono bundled.

## 5. Engine plan

Direct ports of hearth behaviors (spec: feature-inventory §3):
- **Sessions engine**: per-session broadcast hub; on-disk run journal (resumable `seq` replay,
  crash auto-resume); persistent steerable sessions (steering mailbox at step/turn boundary; idle
  reaper; 10min stall watchdog); recovery stamps `aborted`.
- **Doc host**: per-chat handle (join room, VV backfill, write user entries + stream assistant
  segments at 120ms commits, drain commands host-only with processed-ledger idempotence, publish
  diff sidecar, presence); warm-open recent chats (14d/cap 30); nudge-driven cold open; SQLite
  snapshot store.
- **Harness** (research pending — `docs/research/harness.md`): trait mirroring hearth's
  `HarnessShape`; Huginn and Grok over ACP (`huginn --acp`, `grok agent stdio`); Codex via
  app-server JSON-RPC; model/reasoning/option catalogs from each driver.
- **Repos/diffs**: git2 or `git` subprocess (subprocess — matches hearth, avoids libgit2 edge
  cases); worktrees under `~/.hearth/worktrees`; fs watchers (`notify`) + 2min repair; diff
  capture (patch + numstat + untracked, 3MiB cap, sha256) → workspace registry summary + DO diff
  sidecar.
- **Agent accounts**: Codex credential-slot swap (`~/.codex/auth.json`), plan labels,
  usage probes, browser-poll `codex login` flows.
- **Auth**: Tailscale. The tailnet is the trust boundary; inbound peers are identified with
  `tailscale whois`. No WorkOS, no JWKS, no `session.json`.

## 6. Tailnet hub (Rust, `crates/sync`)

The always-on host (`HEARTH_TAILNET_HUB=1`, typically minis) binds one TCP port and serves:

1. Chat2 rooms (`/chat2/{id}/ws` plus HTTP checkpoint/rows) — the dumb log relay, no Loro in the server.
2. Registry rooms (`/registry/{org}/ws`) — current-state LWW rows, same merge as the client.
3. Direct RPC (`/rpc`) on every device so `targetDeviceId` dials MagicDNS instead of a DeviceRoom DO.
4. Static releases (`GET /releases/*` from `{data_dir}/releases/`). Populate with
   `hearth release publish --from github` (or `--from dir`) after a `v*` GitHub
   Release; clients expect `manifest.json` (version + per-file sha256) plus the
   `hearth-<ver>-linux-<arch>.tar.gz` updater tarball.

Loopback is allowed without whois (tests). Other peers must resolve through `tailscale whois`.

## 7. Parity exclusions & deliberate changes

- **Excluded**: token-usage display (profile heatmap, lifetime stats, per-message token columns,
  `WatchUsage`). Rate-limit meters on agent accounts are *kept* (separate concern; probed from
  CLIs, not CRDT-synced).
- **Changed**: Postgres entity sync/server → workspace registry; Electron/React/mugen → Rune with
  ported techniques; Node harness SDKs → subprocess protocols; WebRTC/DeviceRoom DO → direct
  tailnet `/rpc`; mobile app → out of scope for this repo.
- **Kept verbatim**: session-doc schema shape + constants, command ledger rules, chat2/registry
  wire frames, render-parts privacy policy, UX behaviors and animation timings.
- **Harness cut**: Claude Code, Cursor, Hermes, Pi, and OpenCode removed. Remaining: Huginn (ACP,
  default), Codex (native), Grok (ACP), Mock (tests). Retired wire ids deserialize as Huginn.

## 8. Milestones

Status legend: ✅ shipped · 🟡 shipped with named gaps (see `docs/PARITY.md` for upstream lineage).

- ✅ **M0–M3** — scaffold, doc/sync core, engine core, Rune UI (sessions, composer, transcript).
- ✅ **M4 Multi-device (this fork)** — Tailscale hub-and-spoke: chat2 + registry rooms on the hub,
  direct `/rpc` on every device, `tailscale whois` identity. Proven by `scripts/e2e-smoke.sh`
  (loopback hub) and `crates/sync/tests/hub.rs`.
- 🟡 **M5 Full surface** — terminals, diff pane, repo/branch/folder pickers + worktrees,
  Codex agent-accounts UI, settings. Gaps: composer attachment UI (engine upload RPCs exist).
- 🟡 **M6 Polish** — wire reconciliation, keyboard map, clippy/fmt sweep, Linux packaging
  (`scripts/package-linux.sh` + release profile), macOS bundling config (`dist/macos/`,
  not executed — needs a Mac). Gaps: prefers-reduced-motion, engine hardening
  (instance lock, watchdogs).

## 9. Open questions (tracked, non-blocking)

1. `lorosurgeon` fit for the mirror write path vs hand-rolled reconcile.
2. Text shaping performance for analytic row heights: Rune (Parley) measures shaped text natively
   (Rust ⇒ cheap), so we start with `virtual_rows` measurement + memoization rather than porting
   pretext's full analytic kernel; revisit only if cold-open of huge transcripts measures slow.
