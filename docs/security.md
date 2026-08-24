# Security model

Honest scope for **Hearth**: what it isolates, what it trusts, and what it does not claim.

Hearth is a **controller**. It launches coding agents, relays prompts and tool events, and stores transcripts and workspace metadata on disk. The security-relevant question is usually: **what can those agents do to the machine, and what can sync expose?**

---

## Threat model

### In scope

- **Misbehaving or prompt-injected agents** writing or executing outside what the *user* intended — mitigated primarily by **each agent’s own sandbox and approval UX**, not by a Hearth-wide syscall filter.
- **Confusion between UI mode labels and agent capability** (for example leaving Raven on plan/chat and expecting writes) — documented in [usage](usage.md) / [harnesses](harnesses.md); Hearth must apply mode switches faithfully when the agent supports them.
- **Optional sync** — if enabled, Tailscale membership gates who can read/write CRDT docs and attachments. A local-only install never dials the hub.
- **Local data-dir integrity** — single-instance lock, device id files, avoiding silent store swaps when auth state changes (see ARCHITECTURE workspace profiles).

### Out of scope / non-goals

- Hearth is **not** a replacement for Raven’s Landlock/seccomp (or Codex sandbox policies). If the agent process can `rm -rf`, Hearth’s UI will not stop it.
- Hearth does **not** protect against a malicious local user who can already run `hearth` or edit `~/.hearth`.
- Desktop auto-approval settings (for example UI permission defaults that always allow) increase convenience and **reduce** interactive friction; treat them as a trust decision.

---

## Trust boundaries

```
User → Hearth UI → Engine (localhost IPC) → Harness subprocess
                         ↓
                   ~/.hearth documents, journals, uploads
                         ↓ (only if HEARTH_TAILNET_HOST is set)
                   Tailnet hub (chat2 + registry) + peer /rpc
```

- **UI ↔ engine** — WebSocket/JSON-RPC on `127.0.0.1` (default port 27654). Anyone who can open that port on the machine can speak the engine protocol for that instance. Do not expose it on a public interface.
- **Engine ↔ agent** — stdio ACP or native protocols. Hearth declines client fs/terminal ACP capabilities so agents use their own filesystem/terminal access against the chosen cwd/worktree.
- **Data directory** — transcripts and configs are plaintext on disk under `HEARTH_DATA_DIR`. Disk encryption and OS user separation are the real controls.
- **Tailnet** — when sync is enabled, inbound peers are identified with `tailscale whois`. Tailscale ACLs are the trust boundary.

---

## Agent confinement

| Layer | Who owns it |
|---|---|
| Workspace cwd / worktree path | Hearth (host creates worktrees; agent is started with a cwd) |
| Tool allowlists, sandboxes, approvals | The agent (Raven, Codex, Grok, …) |
| Mode (plan/agent/chat) | Negotiated over ACP when supported; Raven enforces toolset |
| Shell PATH resolution | Hearth may snapshot login-shell env to find CLIs; that widens discovery, not agent powers |

When documenting or changing harnesses, prefer **failing closed** on unknown mode ids and never map Raven’s Agent label onto a weaker permission mode for another vendor.

---

## Sync and credentials

- Default: **no** tailnet dial.
- Opt-in via `HEARTH_TAILNET_HOST` (see [configuration](configuration.md)). The tailnet ACLs are the trust boundary; inbound peers are identified with `tailscale whois`.
- `hearth login` / `logout` are unused on the tailnet path.
- Attachments stay on the chat’s host device and transfer over peer `/rpc`, not a shared blob store.

---

## Reporting issues

Prefer private disclosure for vulnerabilities that expose local data or escalate agent reach beyond the selected workspace. Include Hearth version (`Cargo.toml` / package version), OS, harness id, and whether sync was enabled.
