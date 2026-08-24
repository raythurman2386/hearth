# Testing

How Hearth’s tests are organized and how to run a useful subset locally.

## Layout

| Location | What |
|---|---|
| `crates/*/src/**` `#[cfg(test)]` | Unit tests next to code |
| `crates/harness/tests/` | Integration tests against fake agent scripts (`fixtures/fake-*.sh`) and real CLI probes (some ignored / env-gated) |
| `crates/engine/tests/` | Engine e2e: sessions, worktrees, resume, routing, sync-ish flows |
| `crates/doc/tests/`, `crates/rpc/tests/`, `crates/sync/tests/` | Crate-focused integration (`hub.rs` = chat2/registry/HTTP against the real mux) |
| `crates/engine/tests/tailnet_hub.rs` | Two engines sharing a loopback hub registry |
| `crates/rpc/tests/tailnet_rpc.rs` | Direct `/rpc` echo + idle ping over the hub |
| `scripts/e2e-smoke.sh` | Two-process tailnet-hub smoke (loopback; not in CI) |

## Common commands

```bash
# Focused unit tests
cargo test -p hearth-harness --lib
cargo test -p hearth-engine --lib
cargo test -p hearth-proto --lib

# hearth-sync requires the mock-server feature for its registry tests
# (`registry::mock_server` is cfg-gated); without it the test build fails.
cargo test -p hearth-sync --features mock-server

# Harness ACP fixture tests (fake-acp.sh)
cargo test -p hearth-harness --test acp

# One engine integration test by name
cargo test -p hearth-engine --test e2e -- <filter>

# Format / lint (CI-oriented)
cargo fmt --check
cargo clippy -p hearth -- -D warnings   # adjust package set to what you changed
```

Release builds are heavier; prefer `--lib` filters while iterating.

## Fixtures and fakes

- `crates/harness/tests/fixtures/fake-acp.sh` — scripted ACP agent used by harness tests (including `session/set_mode`).
- Mock harness — in-process; enable in the UI with `HEARTH_HARNESS=mock`. Env knobs such as `HEARTH_MOCK_DELAY_MS`, `HEARTH_MOCK_QUESTION`, `HEARTH_MOCK_SUBAGENT` script behavior for demos and UI tests.

## Ignored / external tests

Some engine tests require extra binaries and are `#[ignore]` with instructions in the test attribute. Do not assume `cargo test -- --ignored` passes on a stock laptop without that setup.

## What “green” means for a docs-accurate change

For harness/mode/routing changes, a minimum bar that matches recent fixes:

```bash
cargo test -p hearth-harness --lib raven_mode_config_option
cargo test -p hearth-harness --lib mode_config_option_prefers
cargo test -p hearth-harness --test acp requested_mode_is_sent
cargo test -p hearth-engine --lib live_routing_requires
```

Expand to the full crate tests before merging larger harness work.
