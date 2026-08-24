# Hearth documentation

User and contributor guides for **Hearth**. Start with the [root README](../README.md) for install and a short product overview.

Hearth is a **local-first control surface** for coding agents (Raven, Claude Code, Codex, Cursor, Grok, Hermes, Pi, OpenCode). It is not itself an agent: agents run as subprocesses (or HTTP servers) that Hearth drives over ACP or a native protocol.

## Guides

| Guide | Audience | Contents |
|---|---|---|
| [usage.md](usage.md) | Users | Day-to-day UI workflows, sessions, modes, worktrees, Raven |
| [configuration.md](configuration.md) | Users | Env vars, data dir, daemon, defaults, sync opt-in |
| [harnesses.md](harnesses.md) | Users | Per-agent install notes, protocols, mode behavior |
| [troubleshooting.md](troubleshooting.md) | Users | Common failures: read-only Raven, PATH, IPC, packaging |
| [contributing.md](contributing.md) | Contributors | Build, layout, style, how to change harnesses |
| [testing.md](testing.md) | Contributors | Test layout, smoke commands, fixtures |
| [security.md](security.md) | Security reviewers | Threat model, what Hearth does and does not confine |
| [../ARCHITECTURE.md](../ARCHITECTURE.md) | Contributors | Engine/UI/edge topology (includes optional sync design) |
| [../dist/README.md](../dist/README.md) | Packagers | Linux/macOS packaging scripts |

## Research and parity (upstream lineage)

These are **design notes**, not a getting-started path. Much of the content describes the Zeron/comet lineage and optional sync that this fork leaves **off by default**:

| Doc | Notes |
|---|---|
| [PARITY.md](PARITY.md) | Feature parity tracker vs upstream |
| [research/](research/) | ACP, harnesses, gpui, Loro, Durable Objects, etc. |
| [chat2-sync.md](chat2-sync.md), [registry-sync.md](registry-sync.md) | Sync protocols (opt-in) |
| [memory-plan.md](memory-plan.md), [syntax-highlighting.md](syntax-highlighting.md) | Implementation notes |

## Quick links

- [Install & run](../README.md#install--run)
- [Using Raven](usage.md#using-raven)
- [Plan / Agent / Chat modes](usage.md#interaction-modes-plan--agent--chat)
- [Environment variables](configuration.md#environment-variables)
- [Harness matrix](harnesses.md)
