//! Agent specs: which binary an ACP harness spawns, where to find it, what
//! the model picker shows, and how the pinned adapter is pre-installed.

use std::path::PathBuf;
use std::time::Duration;

use hearth_proto::{HarnessId, Model, ReasoningLevel, SteeringMode};

use super::launch::find_on_paths;

pub(crate) struct AcpAgentSpec {
    pub(crate) id: HarnessId,
    pub(crate) display_name: &'static str,
    /// Binary name searched on PATH (and platform install dirs).
    pub(crate) executable: &'static str,
    /// Env var overriding executable resolution (tests, custom installs).
    pub(crate) env_override: &'static str,
    /// Arguments that put the binary in ACP-serving mode.
    pub(crate) args: &'static [&'static str],
    /// Pinned npm package (`name@version`) installed ONCE into the managed
    /// adapters dir when the binary isn't already present — the launch then
    /// spawns `node <entry>` directly, keeping npm (and every way a user's
    /// npm state can break) out of chat turns. See [`crate::adapter_install`].
    pub(crate) npm_package: Option<&'static str>,
    /// Extra install locations to probe after PATH.
    pub(crate) extra_paths: fn() -> Vec<PathBuf>,
    /// The agent's own CLI binary (`claude`, `codex`, …) — what "installed"
    /// means to the user. Distinct from `executable` where the spawned adapter
    /// wraps the CLI (`claude-agent-acp`, `codex-acp`, `pi-acp`), and the npx
    /// fallback deliberately doesn't count: npx can fetch an adapter on
    /// demand, but an absent CLI still means no logins/config to drive.
    pub(crate) cli_executable: &'static str,
    /// Extra install locations probed for [`Self::cli_executable`].
    pub(crate) cli_extra_paths: fn() -> Vec<PathBuf>,
    /// Search summary + install hint for the NotInstalled error.
    pub(crate) install_hint: &'static str,
    pub(crate) models: fn() -> Vec<Model>,
    pub(crate) steering_mode: SteeringMode,
    /// Effort ladder surfaced in the picker; applied per session via the
    /// `thought_level` config option (must mirror the registry descriptor).
    pub(crate) reasoning_levels: &'static [ReasoningLevel],
    /// Transform applied to the initial prompt and every steer — Claude's
    /// Ultrathink is a prompt-prefix convention, not an effort flag.
    pub(crate) prompt_transform: fn(Option<ReasoningLevel>, &str) -> String,
    /// Preference-ordered `thought_level` value ids for the run's reasoning
    /// (per-agent clamping, e.g. Claude xhigh→max off the xhigh family). The
    /// first value the agent actually advertises wins.
    pub(crate) effort_values: fn(Option<ReasoningLevel>, Option<&str>) -> Vec<&'static str>,
    /// Levels appended to a DISCOVERED model's non-empty ladder: modes the
    /// wire can't advertise because they aren't `thought_level` values.
    pub(crate) ladder_extras: &'static [ReasoningLevel],
    /// The agent emits `_x.ai/session/prompt_complete` (Grok): treat it as
    /// the AUTHORITATIVE turn end — grok's `session/prompt` RPC can hang
    /// silently after the turn really finished (field reports: total silent
    /// non-response on some machines). The prompt response stays as the
    /// fallback; whichever lands first settles the turn, exactly once.
    pub(crate) prompt_complete_extension: bool,
    /// Bound on prompt-send → FIRST sign of life on the wire. `None`
    /// disables. Grok's healthy runs acknowledge a prompt within
    /// milliseconds (queue bookkeeping precedes any model work), so total
    /// silence past this window is a wedged agent (a stale shared leader,
    /// a dead update check) — surface a visible error chip instead of
    /// indefinite Working.
    pub(crate) prompt_stall: Option<Duration>,
    /// Agent-specific advice appended to the stall error chip: what a wedge
    /// usually means for THIS agent and what the user can check.
    pub(crate) stall_hint: &'static str,
}

fn identity_transform(_reasoning: Option<ReasoningLevel>, text: &str) -> String {
    text.to_owned()
}

/// Generic effort ladder for agents without their own clamping rules.
fn default_effort_values(
    reasoning: Option<ReasoningLevel>,
    _model: Option<&str>,
) -> Vec<&'static str> {
    let Some(level) = reasoning else {
        return Vec::new();
    };
    match level {
        ReasoningLevel::Minimal => vec!["minimal", "low"],
        ReasoningLevel::Low => vec!["low", "minimal"],
        ReasoningLevel::Medium => vec!["medium"],
        ReasoningLevel::High => vec!["high"],
        ReasoningLevel::XHigh => vec!["xhigh", "x-high", "high"],
        ReasoningLevel::Max => vec!["max", "xhigh", "high"],
        ReasoningLevel::Ultra | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => {
            vec!["ultra", "max", "high"]
        }
    }
}

fn grok_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local").join("bin").join("grok"));
        dirs.push(home.join(".grok").join("bin").join("grok"));
        dirs.push(home.join(".npm-global").join("bin").join("grok"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/grok"));
    dirs.push(PathBuf::from("/usr/local/bin/grok"));
    dirs
}

pub(crate) fn grok_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Grok,
        display_name: "Grok",
        executable: "grok",
        env_override: "GROK_EXECUTABLE",
        // Flag placement verified against grok 1.0.4: `--no-auto-update` is
        // TOP-LEVEL (before the subcommand) and kills the launch-time update
        // check (a silent multi-second network stall); `--no-leader` lives on
        // the `agent` subcommand and starts a fresh agent even when
        // `[cli] use_leader` is set — leader mode ATTACHES `agent stdio` to a
        // shared process via ~/.grok/leader.sock, so a wedged/stale leader
        // (the user's TUI) reads as total silent non-response in hearth.
        args: &["--no-auto-update", "agent", "--no-leader", "stdio"],
        npm_package: Some("@xai-official/grok@1.0.4"),
        extra_paths: grok_install_paths,
        cli_executable: "grok",
        cli_extra_paths: grok_install_paths,
        install_hint: "grok (searched PATH, the login shell's PATH, ~/.local/bin, \
             ~/.grok/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, and \
             fnm/nvm/volta/pnpm/bun install dirs; install with \
             `curl -fsSL https://x.ai/cli/install.sh | bash` or \
             `npm install -g @xai-official/grok`; set GROK_EXECUTABLE to override)",
        models: || {
            vec![Model {
                id: "grok-4.5".into(),
                label: "Grok 4.5".into(),
                description: Some("xAI's coding model — 500k context".into()),
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ],
                options: Vec::new(),
            }]
        },
        // No `_session/steering` extension: steers deliver at turn boundaries.
        steering_mode: SteeringMode::TurnBoundary,
        // Grok Build's advertised efforts (default high); applied through the
        // session's `thought_level` config option.
        reasoning_levels: &[
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ],
        prompt_transform: identity_transform,
        effort_values: default_effort_values,
        ladder_extras: &[],
        // Verified live (1.0.4): the notification fires with the echoed
        // `_meta.promptId`, just ahead of the RPC response.
        prompt_complete_extension: true,
        prompt_stall: Some(Duration::from_secs(30)),
        stall_hint: "The agent process is likely wedged — a stale shared leader \
             process or a hung startup check; hearth launches it with --no-leader \
             and --no-auto-update to avoid both.",
    }
}

fn huginn_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".cargo").join("bin").join("huginn"));
        dirs.push(home.join(".local").join("bin").join("huginn"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/huginn"));
    dirs.push(PathBuf::from("/usr/local/bin/huginn"));
    dirs
}

pub(crate) fn huginn_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Huginn,
        display_name: "Huginn",
        executable: "huginn",
        env_override: "HUGINN_EXECUTABLE",
        // `huginn --acp` serves ACP v1 over stdio.
        // No npm fallback exists — Huginn is a Cargo binary.
        // Hearth pins plan mode explicitly: upstream now defaults to Agent,
        // but Hearth's read-only default (and its docs) expect plan unless the
        // Mode chip says otherwise.
        args: &["--mode", "plan", "--acp"],
        npm_package: None,
        extra_paths: huginn_install_paths,
        cli_executable: "huginn",
        cli_extra_paths: huginn_install_paths,
        install_hint: "huginn (searched PATH, the login shell's PATH, ~/.cargo/bin, \
             ~/.local/bin, /opt/homebrew/bin, and /usr/local/bin; install with \
             `cargo install --path .` from your huginn checkout, or set \
             HUGINN_EXECUTABLE to override)",
        // Huginn's ACP server advertises its live model list (provider-qualified
        // ids) as the `model` config option at session/new; the picker live-
        // probes that, so the static catalog here is only a cold-open fallback.
        models: || {
            vec![Model {
                id: "default".into(),
                label: "Huginn default".into(),
                description: Some(
                    "Uses the model configured in Huginn (OLLAMA_API_KEY / HUGINN_API_KEY)".into(),
                ),
                reasoning_levels: Vec::new(),
                options: Vec::new(),
            }]
        },
        // No `_session/steering` extension: steers deliver at turn boundaries.
        steering_mode: SteeringMode::TurnBoundary,
        reasoning_levels: &[],
        prompt_transform: identity_transform,
        effort_values: default_effort_values,
        ladder_extras: &[],
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
    }
}

/// Background-install managed npm adapters for agents whose CLI is present
/// on this device, so a first chat never pays (or trips over) an npm run.
/// Skips agents whose adapter is already resolvable; failures are logged and
/// retried on the next daemon start or blocking launch. A no-op outside a
/// tokio runtime.
pub fn prewarm_managed_adapters() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    for spec in [grok_spec()] {
        let Some(pkg) = spec.npm_package else {
            continue;
        };
        let pin = crate::adapter_install::NpmPin::parse(pkg);
        if find_on_paths(spec.executable, (spec.extra_paths)()).is_some()
            || crate::adapter_install::installed_entry(&pin, spec.executable).is_some()
            || find_on_paths(spec.cli_executable, (spec.cli_extra_paths)()).is_none()
            || crate::adapter_install::find_npm().is_none()
        {
            continue;
        }
        let (bin_name, display_name) = (spec.executable, spec.display_name);
        handle.spawn(async move {
            match crate::adapter_install::ensure_installed(pin, bin_name, display_name).await {
                Ok(entry) => tracing::info!(
                    target: "hearth_harness::adapter_install",
                    adapter = %entry.display(),
                    "prewarmed {display_name} ACP adapter"
                ),
                Err(e) => tracing::warn!(
                    target: "hearth_harness::adapter_install",
                    "prewarm of the {display_name} ACP adapter failed: {e}"
                ),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huginn_spec_pins_plan_mode_and_the_huginn_binary() {
        let spec = huginn_spec();
        assert_eq!(spec.executable, "huginn");
        assert_eq!(spec.args, &["--mode", "plan", "--acp"]);
    }
}
