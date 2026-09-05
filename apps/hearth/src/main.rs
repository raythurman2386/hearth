//! hearth — headed by default; `hearth headless` runs the engine alone. Both start
//! local-only without credentials. `hearth login` and `hearth logout` select the
//! profile used by the next engine start without mutating a live runtime.

mod auth_cli;
mod daemon;
mod release_cli;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "hearth", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (local-only unless a saved session enables sync).
    Headless,
    /// Sign in and enable sync on the next engine start.
    Login,
    /// Remove the saved session and return to local-only on the next start.
    Logout,
    /// Show workspace mode, optional auth, and engine status.
    Status,
    /// Live sync introspection from the running engine: per-room connection
    /// state, last pushed-frame/ack ages, rejoin/probe/resync counters.
    Sync,
    /// Manage `hearth headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
    /// Publish release artifacts onto this machine's hub releases dir.
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures HEARTH_* env).
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

#[derive(Subcommand)]
enum ReleaseCommand {
    /// Verify artifacts and publish into `{data_dir}/releases/` (hub GET /releases/*).
    Publish {
        /// `github` (download a GitHub Release) or `dir` (local artifacts folder).
        #[arg(long, default_value = "github")]
        from: String,
        /// Tag (`v0.2.3`) for `--from github`, or path for `--from dir`.
        /// Omit with `--from github` to use the latest GitHub Release.
        target: Option<String>,
        /// Allow replacing an equal or newer hub version.
        #[arg(long)]
        force: bool,
        /// Verify only — do not write into the releases dir.
        #[arg(long)]
        check: bool,
    },
}

/// mimalloc: system malloc (macOS libmalloc especially) never returns the
/// streaming churn's high-water pages, so transient allocation became
/// permanent RSS (docs/memory-plan.md §1).
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    // Headed launches from a .desktop file (and macOS .app) do not inherit
    // the daemon unit's EnvironmentFile. Load `~/.hearth/env` first so a
    // spoke GUI sees HEARTH_TAILNET_HOST and can dial the hub. Existing
    // process env wins, so systemd Environment= / a user's export still
    // override the file.
    apply_hearth_env_file();
    let cli = Cli::parse();
    // Long-running modes log at info, one-shot CLI commands at warn (RUST_LOG
    // overrides either).
    // loro's internal block-encode diagnostics log at info and flood
    // journald on every snapshot export — enough to fill a disk on a
    // long-running headless host. Quiet them by default (RUST_LOG still
    // overrides the whole filter).
    let long_running = matches!(&cli.command, None | Some(Command::Headless));
    let default_filter = if long_running {
        "info,loro_internal=warn,loro=warn"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    // Long-running modes mirror stdout logging to {data_dir}/logs — a headed
    // app launched from Finder has no visible stdout, which left every sync
    // wedge report ("stale until restart") with zero diagnostics even though
    // the engine logs the exact failure line. One file per launch, previous
    // launch kept as `.old`.
    let log_file = if long_running {
        let mode = if cli.command.is_some() {
            "headless"
        } else {
            "headed"
        };
        open_log_file(mode)
    } else {
        None
    };
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer());
        match log_file {
            Some(file) => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Arc::new(file)),
                )
                .init(),
            None => registry.init(),
        }
    }

    match cli.command {
        Some(Command::Headless) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = hearth_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Login) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::login(engine_config_from_env()))
        }
        Some(Command::Logout) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::logout(engine_config_from_env()))
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(engine_config_from_env()))
        }
        Some(Command::Sync) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(sync_cli(engine_config_from_env().ipc_port))
        }
        Some(Command::Update { check }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            let cfg = engine_config_from_env();
            let base = cfg.tailnet_http_url().ok_or_else(|| {
                anyhow::anyhow!(
                    "set HEARTH_TAILNET_HOST to the hub MagicDNS name (releases are served \
                     from the hub at GET /releases/). On the hub run: \
                     hearth release publish --from github. See docs/configuration.md"
                )
            })?;
            runtime.block_on(update_cli::update(&base, check))
        }
        Some(Command::Release { command }) => match command {
            ReleaseCommand::Publish {
                from,
                target,
                force,
                check,
            } => {
                let runtime = tokio::runtime::Runtime::new()?;
                let source = release_cli::parse_from(&from, target)?;
                let data_dir = engine_config_from_env().data_dir;
                runtime.block_on(release_cli::publish(source, data_dir, force, check))
            }
        },
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            // Headed: the UI probes HEARTH_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            let cfg = engine_config_from_env();
            hearth_ui::run_app(hearth_ui::UiConfig {
                data_dir: cfg.data_dir,
                ipc_port: cfg.ipc_port,
                edge_url: cfg.edge_url,
                workos_client_id: None,
                edge_token: cfg.edge_token,
                org_id: cfg.org_id,
                default_harness: cfg.default_harness,
            });
            Ok(())
        }
    }
}

/// The env-resolved engine configuration shared by `headless`, `login`,
/// `logout`, and `status` — one resolution so the CLI auth commands always
/// operate on the exact session the daemon will load.
fn engine_config_from_env() -> hearth_engine::EngineConfig {
    // Tailnet opt-in: HEARTH_TAILNET_HOST points at the always-on hub.
    // When set, WorkOS/edge-token are unused; the dummy bearer just enables
    // the existing room-client seam against the hub's HTTP+WS mux.
    let tailnet_host = std::env::var("HEARTH_TAILNET_HOST")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tailnet_port = std::env::var("HEARTH_TAILNET_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(hearth_engine::DEFAULT_TAILNET_PORT);
    let tailnet_hub = matches!(
        std::env::var("HEARTH_TAILNET_HUB").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    let edge_token = if tailnet_host.is_some() {
        Some("tailnet".into())
    } else {
        std::env::var("HEARTH_EDGE_TOKEN").ok()
    };
    let edge_url = tailnet_host
        .as_deref()
        .map(|host| format!("http://{host}:{tailnet_port}"))
        .unwrap_or_default();
    hearth_engine::EngineConfig {
        data_dir: std::env::var_os("HEARTH_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        edge_url,
        ipc_port: std::env::var("HEARTH_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
        org_id: std::env::var("HEARTH_ORG_ID").ok(),
        workos_client_id: None,
        edge_token,
        tailnet_host,
        tailnet_port,
        tailnet_hub,
    }
}

/// `HEARTH_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `raven`.
fn harness_from_env() -> hearth_engine::HarnessId {
    match std::env::var("HEARTH_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => hearth_engine::HarnessId::Mock,
        Ok("codex") => hearth_engine::HarnessId::Codex,
        Ok("grok") => hearth_engine::HarnessId::Grok,
        Ok("raven") => hearth_engine::HarnessId::Raven,
        _ => hearth_engine::HarnessId::Raven,
    }
}

/// systemd `EnvironmentFile=` shape: `KEY=VALUE`, optional `export `,
/// `#` comments, quoted values. Does not override variables already set.
fn apply_hearth_env_file() {
    let data_dir = std::env::var_os("HEARTH_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            home.map(|h| h.join(".hearth"))
                .unwrap_or_else(|| std::path::PathBuf::from(".hearth"))
        });
    let path = data_dir.join("env");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    for (key, value) in parse_hearth_env_file(&text) {
        if std::env::var_os(&key).is_none() {
            // SAFETY: called from `main` before any threads (tokio/gpui) start.
            unsafe { std::env::set_var(&key, value) };
        }
    }
}

fn parse_hearth_env_file(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            continue;
        }
        let mut value = rest.trim().to_string();
        if value.len() >= 2 {
            let bytes = value.as_bytes();
            if (bytes[0] == b'"' && *bytes.last().unwrap() == b'"')
                || (bytes[0] == b'\'' && *bytes.last().unwrap() == b'\'')
            {
                value = value[1..value.len() - 1].to_string();
            }
        }
        out.push((key.to_string(), value));
    }
    out
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME not set"));
    let dir = home.join(".hearth");
    // One-shot 0.2.0 migration: adopt the pre-rename data dir (sign-in,
    // device identity, prefs) instead of starting fresh.
    if !dir.exists() {
        let old = home.join(".comet-native");
        if old.exists() && std::fs::rename(&old, &dir).is_ok() {
            eprintln!("migrated data dir {} -> {}", old.display(), dir.display());
        }
    }
    dir
}

/// `hearth sync`: dial the running engine's IPC and print per-room sync state.
/// The introspection surface every 2026-08 incident was missing — "is this
/// device's workspace room actually receiving?" as a one-liner.
async fn sync_cli(ipc_port: u16) -> anyhow::Result<()> {
    let client = hearth_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "no engine listening on 127.0.0.1:{ipc_port} ({e}) — is hearth running?"
            )
        })?;
    let status = client
        .call(hearth_rpc::methods::SYNC_STATUS, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("SyncStatus failed: {e}"))?;
    let now = status.get("nowMs").and_then(|v| v.as_i64()).unwrap_or(0);
    let age = |ms: i64| -> String {
        if ms <= 0 {
            return "never".into();
        }
        let s = (now - ms).max(0) / 1000;
        if s >= 3600 {
            format!("{}h{}m ago", s / 3600, (s % 3600) / 60)
        } else if s >= 60 {
            format!("{}m{}s ago", s / 60, s % 60)
        } else {
            format!("{s}s ago")
        }
    };
    let room_line = |room: Option<&serde_json::Value>| -> String {
        let Some(room) = room else {
            return "no room (dialing or edge-less)".into();
        };
        let get = |k: &str| room.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        // REJECTED is loud and only shown when nonzero: rejected writes with
        // a fresh-looking room is exactly the latched-session wedge
        // (2026-08-04) this readout previously masked.
        let rejected = get("rejected");
        format!(
            "{} pushed {} · acked {} · rejoins {} probes {} resyncs {} drops {}{}",
            if room.get("connected").and_then(|v| v.as_bool()) == Some(true) {
                "connected ·"
            } else {
                "DISCONNECTED ·"
            },
            age(get("lastPushedMs")),
            age(get("lastAckMs")),
            get("rejoins"),
            get("probes"),
            get("fullResyncs"),
            get("disconnects"),
            if rejected > 0 {
                format!(" REJECTED {rejected}")
            } else {
                String::new()
            },
        )
    };
    println!(
        "Device:    {}",
        status
            .get("deviceId")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
    println!(
        "Workspace: {}",
        room_line(status.get("workspace").filter(|v| !v.is_null()))
    );
    let chats = status
        .get("chats")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if chats.is_empty() {
        println!("Chats:     none open");
    }
    // Chat rooms speak chat2: cursor/head tell "am I caught up?", pending
    // tells "did my writes leave?", resets/rejected are the loud tells.
    let chat_line = |room: Option<&serde_json::Value>| -> String {
        let Some(room) = room else {
            return "no room (dialing or edge-less)".into();
        };
        let get = |k: &str| room.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let resets = get("serverResets");
        let rejected = get("rejected");
        format!(
            "{} cursor {}/{} · pending {} · rows {} ({}KB) · rejoins {} drops {}{}{}",
            if room.get("connected").and_then(|v| v.as_bool()) == Some(true) {
                "connected ·"
            } else {
                "DISCONNECTED ·"
            },
            get("cursor"),
            get("headSeq"),
            get("pendingPushes"),
            get("rowCount"),
            get("rowBytes") / 1024,
            get("rejoins"),
            get("disconnects"),
            if resets > 0 {
                format!(" RESETS {resets}")
            } else {
                String::new()
            },
            if rejected > 0 {
                format!(" REJECTED {rejected}")
            } else {
                String::new()
            },
        )
    };
    for chat in &chats {
        println!(
            "Chat {}: {}",
            chat.get("chatId")
                .and_then(|v| v.as_str())
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("?"),
            chat_line(chat.get("room").filter(|v| !v.is_null()))
        );
    }
    Ok(())
}

/// `{data_dir}/logs/hearth-{mode}.log`, previous launch preserved as `.old`.
/// Headed and headless are separate files so an embedded-engine app and a
/// daemon on the same machine never interleave writes.
///
/// The returned file holds an exclusive `flock` for the process lifetime:
/// rotate-on-launch is only safe when nothing is still WRITING the current
/// file. On 2026-08-04 a dev build launched twice next to the running
/// installed app — the first rename put the daemon's live log at `.old`, the
/// second unlinked it entirely, and the daemon spent the rest of the incident
/// logging to an orphaned inode (an entire day of sync diagnostics gone at
/// the exact moment they were needed). A launch that finds the canonical file
/// locked logs to `hearth-{mode}.{pid}.log` instead; the next lock-holding
/// launch sweeps pid-suffixed files older than a week.
fn open_log_file(mode: &str) -> Option<std::fs::File> {
    let dir = std::env::var_os("HEARTH_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dirs_data_dir)
        .join("logs");
    open_log_file_in(&dir, mode)
}

/// Dir-parameterized body of [`open_log_file`] (unit-testable without env).
fn open_log_file_in(dir: &std::path::Path, mode: &str) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("hearth-{mode}.log"));
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // Probe the CURRENT inode for a live writer before touching it.
        let preexisting = path.exists();
        let existing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let rc = unsafe { libc::flock(existing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            // A live process owns the canonical log — leave it alone.
            return std::fs::File::create(
                dir.join(format!("hearth-{mode}.{}.log", std::process::id())),
            )
            .ok();
        }
        // No live writer: rotate, create fresh, and lock it as ours. (The
        // probe's flock dies with `existing`; a first-ever launch has nothing
        // to rotate — the probe itself created the empty file.)
        drop(existing);
        if preexisting {
            let _ = std::fs::rename(&path, dir.join(format!("hearth-{mode}.log.old")));
        }
        let file = std::fs::File::create(&path).ok()?;
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        sweep_stale_pid_logs(dir, mode);
        Some(file)
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::rename(&path, dir.join(format!("hearth-{mode}.log.old")));
        std::fs::File::create(&path).ok()
    }
}

#[cfg(all(test, unix))]
mod log_file_tests {
    use super::open_log_file_in;

    #[test]
    fn second_launch_never_rotates_a_live_processes_log() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        // First launch owns the canonical file and keeps writing.
        let first = open_log_file_in(dir, "headed").expect("first log");
        assert!(dir.join("hearth-headed.log").is_file());
        // Second launch while the first is alive: canonical file untouched,
        // pid-suffixed overflow file instead (the 2026-08-04 clobber).
        let second = open_log_file_in(dir, "headed").expect("second log");
        let pid_path = dir.join(format!("hearth-headed.{}.log", std::process::id()));
        assert!(pid_path.is_file(), "expected pid-suffixed overflow log");
        assert!(
            !dir.join("hearth-headed.log.old").exists(),
            "live canonical log must not be rotated away"
        );
        drop(second);
        // After the owner exits, a fresh launch rotates normally.
        drop(first);
        let third = open_log_file_in(dir, "headed").expect("third log");
        assert!(
            dir.join("hearth-headed.log.old").is_file(),
            "rotation resumes"
        );
        drop(third);
    }
}

/// Delete `hearth-{mode}.{pid}.log` overflow files older than a week — they
/// only exist when a second instance raced a live one for the canonical log.
#[cfg(unix)]
fn sweep_stale_pid_logs(dir: &std::path::Path, mode: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("hearth-{mode}.");
    let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(middle) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".log"))
        else {
            continue;
        };
        if !middle.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > week);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod env_file_tests {
    use super::parse_hearth_env_file;

    #[test]
    fn parses_systemd_environment_file_shape() {
        let parsed = parse_hearth_env_file(
            "# Hearth tailnet sync\n\
             HEARTH_TAILNET_HOST=minis.tailc2d479.ts.net\n\
             export HEARTH_TAILNET_HUB=1\n\
             HEARTH_DEVICE_NAME=\"Studio Mac\"\n\
             \n\
             not-a-line\n\
             =emptykey\n",
        );
        assert_eq!(
            parsed,
            vec![
                (
                    "HEARTH_TAILNET_HOST".into(),
                    "minis.tailc2d479.ts.net".into()
                ),
                ("HEARTH_TAILNET_HUB".into(), "1".into()),
                ("HEARTH_DEVICE_NAME".into(), "Studio Mac".into()),
            ]
        );
    }
}
