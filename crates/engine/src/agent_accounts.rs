//! AgentAccounts — Codex logins on this device (feature-inventory §3.7
//! "Agent accounts").
//!
//! Codex stores exactly one live login in `$CODEX_HOME/auth.json` (default
//! `~/.codex`): a ChatGPT OAuth token set (identity inside the `id_token`
//! JWT) or a raw API key.
//!
//! Swap mechanics:
//!
//! 1. **Detect** the live login and auto-snapshot it into a slot under
//!    `{data_dir}/agent-accounts/codex/{slotId}.json` — the current session is
//!    always backed up before any swap, and refreshed tokens stay current.
//! 2. **Swap** (`activate`): overwrite the CLI's credential store with a saved
//!    slot. Detection runs first so a swap never strands the session it
//!    replaces.
//! 3. **Add** (`start_login`…): spawn `codex login` against a throwaway
//!    `CODEX_HOME` and poll until its loopback callback lands. The live
//!    `~/.codex` session is never touched until the user explicitly switches.
//!
//! Usage probes hit Codex's `/status` rate-limit view. Unlike the original
//! hearth (fetch on every list, 60s cache), native only hits the network when
//! `force_usage` is set — the default list stays offline-fast and
//! deterministic; the UI passes `forceUsage` on page mount/refresh. Cached
//! results (60s TTL) are served to non-forced lists in between.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use hearth_proto::{
    AgentAccount, AgentAccountWarning, AgentAccountsSnapshot, AgentAuthKind, AgentLoginMode,
    AgentLoginPoll, AgentLoginStart, AgentLoginStatus, AgentUsageWindow, HarnessId,
};

use crate::repos::home_dir;
use crate::{EngineError, new_id, now_ms};

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

const USAGE_TTL: Duration = Duration::from_secs(60);
/// An abandoned login flow (dialog dismissed without Cancel) is reaped past this.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Filesystem knobs — env-resolved in production ([`AgentAccountsConfig::detect`]),
/// explicit in tests.
#[derive(Debug, Clone)]
pub struct AgentAccountsConfig {
    /// Engine data dir; slots live under `{data_dir}/agent-accounts/`.
    pub data_dir: PathBuf,
    /// Codex home (`$CODEX_HOME` or `~/.codex`) — holds `auth.json`.
    pub codex_home: PathBuf,
}

impl AgentAccountsConfig {
    /// Production resolution: `CODEX_HOME` relocates the Codex auth file.
    pub fn detect(data_dir: &Path) -> Self {
        let env_dir = |name: &str| {
            std::env::var_os(name)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        };
        Self {
            data_dir: data_dir.to_path_buf(),
            codex_home: env_dir("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex")),
        }
    }

    fn codex_auth_file(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    fn root_dir(&self) -> PathBuf {
        self.data_dir.join("agent-accounts")
    }
}

// ── slot storage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlotProfile {
    email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    auth_kind: AgentAuthKind,
}

/// One saved login (`{slotId}.json`), same field surface as hearth's slot files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    id: String,
    harness: HarnessId,
    /// The provider-side identity the slot is keyed by (account uuid/email).
    account_key: String,
    profile: SlotProfile,
    /// Codex: `auth.json`.
    credentials: serde_json::Value,
    saved_at: i64,
    /// First time this account was saved — the STABLE sort key, so switching the
    /// active account (which re-snapshots and bumps `saved_at`) never reorders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
}

/// A live detection result (before it's persisted into a slot).
#[derive(Debug, Clone)]
struct Detected {
    account_key: String,
    profile: SlotProfile,
    /// `None` ⇒ we know a login exists but couldn't read the secret.
    credentials: Option<serde_json::Value>,
}

// ── login flows ─────────────────────────────────────────────────────────────

enum LoginFlow {
    /// A spawned login child polled to completion: `codex login` against a
    /// throwaway `CODEX_HOME`. The LIVE login is never touched; completion is
    /// the credential file appearing under `home`.
    Spawned {
        harness: HarnessId,
        /// The login child; monitored (try_wait) + killable from cancel.
        child: Arc<Mutex<Option<tokio::process::Child>>>,
        /// Throwaway credential dir, reclaimed on cancel/completion.
        home: PathBuf,
        started_at: Instant,
        output: Arc<Mutex<String>>,
        /// `Some(code)` once the child exited (`None` code = killed by signal).
        exit: Arc<Mutex<Option<Option<i32>>>>,
    },
}

impl LoginFlow {
    fn started_at(&self) -> Instant {
        match self {
            LoginFlow::Spawned { started_at, .. } => *started_at,
        }
    }
}

// ── service ─────────────────────────────────────────────────────────────────

/// Cached usage probe result: the windows (or a remembered miss) + fetch time.
type CachedUsage = (Option<UsageSnapshot>, Instant);

/// One live usage probe: rate-limit windows plus the plan label the provider
/// reported alongside them (Codex's usage endpoint carries a live `plan_type`,
/// which supersedes the login-time JWT claim — plan changes show up here
/// without a re-login).
#[derive(Clone, Default)]
struct UsageSnapshot {
    windows: Vec<AgentUsageWindow>,
    plan_label: Option<String>,
}

struct Inner {
    config: AgentAccountsConfig,
    http: reqwest::Client,
    flows: Mutex<HashMap<String, LoginFlow>>,
    /// `"{harness}:{accountKey}"` → cached usage windows.
    usage_cache: Mutex<HashMap<String, CachedUsage>>,
}

#[derive(Clone)]
pub struct AgentAccounts {
    inner: Arc<Inner>,
}

impl AgentAccounts {
    pub fn new(config: AgentAccountsConfig) -> Self {
        // Startup sweep: a previous process that crashed mid-login leaves
        // `.login-<uuid>` throwaway CODEX_HOME dirs — each may hold live OAuth
        // tokens — with no owner to clean them. Reclaim them at boot.
        let root = config.root_dir();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".login-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            inner: Arc::new(Inner {
                config,
                http,
                flows: Mutex::new(HashMap::new()),
                usage_cache: Mutex::new(HashMap::new()),
            }),
        }
    }

    // ── list ────────────────────────────────────────────────────────────────

    /// Detect the Codex CLI, auto-snapshot the live login, and assemble the view.
    pub async fn list(&self, force_usage: bool) -> Result<AgentAccountsSnapshot, EngineError> {
        if force_usage {
            lock(&self.inner.usage_cache).clear();
        }
        let warnings: Vec<AgentAccountWarning> = Vec::new();
        let mut active_keys: HashMap<HarnessId, String> = HashMap::new();

        if let Some(detected) = self.detect_codex() {
            active_keys.insert(HarnessId::Codex, detected.account_key.clone());
            self.snapshot_detected(HarnessId::Codex, &detected)?;
        }

        // Stable presentation order: provider, then slot creation order (never
        // active-first — switching must not reshuffle the cards).
        let mut accounts: Vec<AgentAccount> = Vec::new();
        for harness in [HarnessId::Codex] {
            let active_key = active_keys.get(&harness).cloned();
            let slots = self.read_slots(harness);
            for slot in &slots {
                let active = active_key.as_deref() == Some(slot.account_key.as_str());
                let usage = self.usage_for(harness, slot, force_usage).await;
                accounts.push(AgentAccount {
                    id: slot.id.clone(),
                    harness,
                    email: Some(slot.profile.email.clone()),
                    // A live plan from the usage probe (Codex `plan_type`)
                    // supersedes the login-time snapshot; fall back to the
                    // snapshot when the probe wasn't forced or failed.
                    plan_label: usage
                        .as_ref()
                        .and_then(|usage| usage.plan_label.clone())
                        .or_else(|| slot.profile.plan.clone()),
                    active,
                    usage_windows: usage.map(|usage| usage.windows).unwrap_or_default(),
                    display_name: slot.profile.display_name.clone(),
                    organization: slot.profile.organization.clone(),
                    auth_kind: Some(slot.profile.auth_kind),
                    switchable: true,
                    saved_at: Some(slot.saved_at),
                });
            }
        }
        Ok(AgentAccountsSnapshot { accounts, warnings })
    }

    // ── swap ────────────────────────────────────────────────────────────────

    /// Swap the CLI's live login to a saved slot. Detection runs first, so the
    /// CURRENT login is snapshotted into its slot before being overwritten —
    /// a swap never strands the session it replaces.
    pub async fn activate(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        self.list(false).await?;
        let slot = self
            .read_slots(harness)
            .into_iter()
            .find(|s| s.id == account_id)
            .ok_or_else(|| {
                EngineError::Other(
                    "That saved login no longer exists — refresh and try again.".into(),
                )
            })?;
        match harness {
            HarnessId::Codex => self.activate_codex(&slot)?,
            other => {
                return Err(EngineError::Other(format!(
                    "agent accounts are not supported for {other:?}"
                )));
            }
        }
        self.list(false).await
    }

    fn activate_codex(&self, slot: &Slot) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.inner.config.codex_home)?;
        let json = serde_json::to_string_pretty(&slot.credentials)
            .map_err(|e| EngineError::Other(format!("serialize codex auth: {e}")))?;
        write_file_atomic(&self.inner.config.codex_auth_file(), json.as_bytes(), true)
    }

    // ── forget ──────────────────────────────────────────────────────────────

    pub async fn forget(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        // Reject anything that isn't a slot id (16 lowercase hex) BEFORE touching
        // the filesystem: `account_id` is a raw RPC string that becomes a path,
        // so a crafted id (`../../…`) must never reach `remove_file`.
        if account_id.len() != 16
            || !account_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(EngineError::Other("Unknown account.".into()));
        }
        let snapshot = self.list(false).await?;
        let active = snapshot
            .accounts
            .iter()
            .any(|a| a.harness == harness && a.id == account_id && a.active);
        if active {
            return Err(EngineError::Other(
                "That's the live login — switch to another account first (it would just be \
                 re-detected)."
                    .into(),
            ));
        }
        let file = self.slots_dir(harness)?.join(format!("{account_id}.json"));
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        self.list(false).await
    }

    // ── add-account OAuth flows ─────────────────────────────────────────────

    pub async fn start_login(&self, harness: HarnessId) -> Result<AgentLoginStart, EngineError> {
        self.sweep_flows();
        match harness {
            HarnessId::Codex => self.start_codex_login().await,
            other => Err(EngineError::Other(format!(
                "agent logins are not supported for {other:?}"
            ))),
        }
    }

    /// Supersede — and reap — any pending spawned flow for `harness` (`codex
    /// login` binds a fixed loopback OAuth port, so a lingering flow makes
    /// every retry exit on EADDRINUSE).
    fn reap_spawned_flows(&self, harness: HarnessId) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| matches!(f, LoginFlow::Spawned { harness: h, .. } if *h == harness))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    async fn start_codex_login(&self) -> Result<AgentLoginStart, EngineError> {
        self.reap_spawned_flows(HarnessId::Codex);
        let login_id = new_id();
        // A throwaway CODEX_HOME isolates the new login completely — the live
        // ~/.codex session is never touched until the user explicitly switches.
        let home = self
            .inner
            .config
            .root_dir()
            .join(format!(".login-{login_id}"));
        std::fs::create_dir_all(&home)?;
        let mut command = tokio::process::Command::new("codex");
        command
            .arg("login")
            .env("CODEX_HOME", &home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // The CLI opens the authorization tab itself (via the `webbrowser`
        // crate) AND the app opens the page when this start reply lands —
        // users got TWO identical auth.openai.com tabs. `webbrowser` prefers
        // $BROWSER over xdg-open, so a no-op script there keeps the CLI's
        // open quiet; a failed open is advisory to `codex login` (it prints
        // the URL and keeps serving the loopback callback either way).
        #[cfg(unix)]
        if let Some(noop_browser) = ensure_noop_browser(&self.inner.config.root_dir()) {
            command.env("BROWSER", noop_browser);
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&home);
                return Err(EngineError::Other(
                    if err.kind() == std::io::ErrorKind::NotFound {
                        "The `codex` CLI was not found on this device — install it first.".into()
                    } else {
                        format!("Could not start codex login: {err}")
                    },
                ));
            }
        };

        // codex prints the authorize URL (to stderr as of 0.142 — scan both
        // streams); grab it so the app can open the single authorization tab
        // (the CLI's own browser-open is suppressed via BROWSER above).
        let (child, output, exit) = wire_login_child(child);
        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Spawned {
                harness: HarnessId::Codex,
                child,
                home,
                started_at: Instant::now(),
                output: output.clone(),
                exit: exit.clone(),
            },
        );
        let url = await_login_url(&output, &exit, scan_openai_url).await;
        Ok(AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::Browser,
        })
    }

    /// Paste-code completion was Claude-only. Kept as an RPC so old clients
    /// fail cleanly rather than hitting UnknownMethod.
    pub async fn complete_login(
        &self,
        _login_id: &str,
        _code: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        Err(EngineError::Other(
            "This sign-in attempt expired — start again.".into(),
        ))
    }

    pub async fn poll_login(&self, login_id: &str) -> Result<AgentLoginPoll, EngineError> {
        self.sweep_flows();
        let (harness, home, exit, output) = match lock(&self.inner.flows).get(login_id) {
            None => {
                return Err(EngineError::Other(
                    "This sign-in attempt expired — start again.".into(),
                ));
            }
            Some(LoginFlow::Spawned {
                harness,
                home,
                exit,
                output,
                ..
            }) => (*harness, home.clone(), exit.clone(), output.clone()),
        };
        let detected = read_json(&home.join("auth.json")).and_then(|auth| match harness {
            HarnessId::Codex => parse_codex_auth(auth),
            _ => None,
        });
        if let Some(detected) = detected {
            self.snapshot_detected(harness, &detected)?;
            self.cancel_login(login_id);
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Done,
                message: None,
            });
        }
        let exited = *lock(&exit);
        if let Some(code) = exited {
            self.cancel_login(login_id);
            let message = if code == Some(0) {
                "The sign-in finished without credentials.".to_string()
            } else {
                let output = lock(&output);
                output
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("sign-in failed")
                    .to_string()
            };
            return Ok(AgentLoginPoll {
                status: AgentLoginStatus::Error,
                message: Some(message),
            });
        }
        Ok(AgentLoginPoll {
            status: AgentLoginStatus::Pending,
            message: None,
        })
    }

    /// Drop a flow: kill a pending login child (`codex login` holds the fixed
    /// loopback OAuth port) and reclaim its throwaway home dir. Idempotent.
    pub fn cancel_login(&self, login_id: &str) {
        let flow = lock(&self.inner.flows).remove(login_id);
        if let Some(LoginFlow::Spawned { child, home, .. }) = flow {
            if let Some(c) = lock(&child).as_mut() {
                let _ = c.start_kill();
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    /// Engine shutdown: kill any in-flight login child so an orphan `codex login`
    /// can't survive the restart and brick the next attempt.
    pub fn shutdown(&self) {
        let ids: Vec<String> = lock(&self.inner.flows).keys().cloned().collect();
        for id in ids {
            self.cancel_login(&id);
        }
    }

    /// Lazy TTL sweep (hearth uses a background fiber; native reaps on the next
    /// accounts call — same bound, no standing task).
    fn sweep_flows(&self) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| f.started_at().elapsed() > FLOW_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    // ── detection ───────────────────────────────────────────────────────────

    fn detect_codex(&self) -> Option<Detected> {
        read_json(&self.inner.config.codex_auth_file()).and_then(parse_codex_auth)
    }

    /// Persist a detected login into its slot (refreshing stored tokens).
    fn snapshot_detected(&self, harness: HarnessId, d: &Detected) -> Result<(), EngineError> {
        let Some(credentials) = &d.credentials else {
            return Ok(());
        };
        self.write_slot(&Slot {
            id: slot_id_for(harness, &d.account_key),
            harness,
            account_key: d.account_key.clone(),
            profile: d.profile.clone(),
            credentials: credentials.clone(),
            saved_at: now_ms(),
            created_at: None,
        })
    }

    // ── slot files ──────────────────────────────────────────────────────────

    fn slots_dir(&self, harness: HarnessId) -> Result<PathBuf, EngineError> {
        let dir = self.inner.config.root_dir().join(harness_slug(harness));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn read_slots(&self, harness: HarnessId) -> Vec<Slot> {
        let Ok(dir) = self.slots_dir(harness) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut slots: Vec<Slot> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // One malformed slot file must skip THAT slot, not brick the page.
            if let Some(slot) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Slot>(&raw).ok())
            {
                slots.push(slot);
            }
        }
        // Creation order — stable across switches (saved_at churns on every
        // auto-snapshot; created_at never does). Slot id breaks creation-time
        // ties: two logins saved in the same millisecond otherwise land in
        // read_dir order, which is filesystem-arbitrary for UUID-named files
        // and reshuffles the page between restarts (issue #161).
        slots.sort_by(|a, b| {
            (a.created_at.unwrap_or(a.saved_at), &a.id)
                .cmp(&(b.created_at.unwrap_or(b.saved_at), &b.id))
        });
        slots
    }

    fn write_slot(&self, slot: &Slot) -> Result<(), EngineError> {
        let file = self
            .slots_dir(slot.harness)?
            .join(format!("{}.json", slot.id));
        let existing: Option<Slot> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let mut full = slot.clone();
        full.created_at = existing
            .and_then(|e| e.created_at.or(Some(e.saved_at)))
            .or(slot.created_at)
            .or_else(|| {
                // A brand-new slot: stamp it strictly after every sibling, so
                // two logins inside the same millisecond still list in the
                // order they were saved (creation order is the page's sort
                // key; ms-resolution ties otherwise fall to read_dir order).
                let floor = self
                    .read_slots(slot.harness)
                    .iter()
                    .map(|s| s.created_at.unwrap_or(s.saved_at))
                    .max()
                    .map(|newest| newest + 1)
                    .unwrap_or(slot.saved_at);
                Some(floor.max(slot.saved_at))
            });
        let json = serde_json::to_string_pretty(&full)
            .map_err(|e| EngineError::Other(format!("serialize slot: {e}")))?;
        // Atomic + 0600 from birth: tokens must never be world-readable, and a
        // crash mid-write must never leave torn JSON.
        write_file_atomic(&file, json.as_bytes(), true)
    }

    // ── remaining usage ─────────────────────────────────────────────────────

    async fn usage_for(
        &self,
        harness: HarnessId,
        slot: &Slot,
        force: bool,
    ) -> Option<UsageSnapshot> {
        let key = format!("{}:{}", harness_slug(harness), slot.account_key);
        if let Some((usage, at)) = lock(&self.inner.usage_cache).get(&key)
            && at.elapsed() < USAGE_TTL
        {
            return usage.clone();
        }
        if !force {
            // Non-forced lists never hit the network (see module docs).
            return None;
        }
        let usage = match harness {
            HarnessId::Codex => self.codex_usage(slot).await,
            _ => None,
        };
        lock(&self.inner.usage_cache).insert(key, (usage.clone(), Instant::now()));
        usage
    }

    async fn codex_usage(&self, slot: &Slot) -> Option<UsageSnapshot> {
        let tokens = slot.credentials.get("tokens")?;
        // api-key mode has no ChatGPT rate windows.
        let access_token = str_field(tokens, "access_token")?;
        let body: serde_json::Value = self
            .inner
            .http
            .get(CODEX_USAGE_URL)
            .bearer_auth(&access_token)
            .header(
                "chatgpt-account-id",
                str_field(tokens, "account_id").unwrap_or_default(),
            )
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let rl = body.get("rate_limit")?;
        let mut windows = Vec::new();
        for key in ["primary_window", "secondary_window"] {
            if let Some(w) = rl.get(key)
                && let Some(used) = w.get("used_percent").and_then(|v| v.as_f64())
            {
                let span = w
                    .get("limit_window_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                windows.push(AgentUsageWindow {
                    label: codex_window_label(span).to_string(),
                    used_fraction: (used / 100.0) as f32,
                    resets_at: parse_when(w.get("reset_at")),
                });
            }
        }
        if windows.is_empty() {
            return None;
        }
        // Live plan ("free"/"plus"/"pro"…) — beats the login-time JWT claim,
        // so a plan change shows up on the next forced refresh without a
        // re-login.
        let plan_label = codex_plan(str_field(&body, "plan_type").as_deref());
        Some(UsageSnapshot {
            windows,
            plan_label,
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn harness_slug(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Codex => "codex",
        HarnessId::Grok => "grok",
        HarnessId::Raven => "raven",
        HarnessId::Mock => "mock",
    }
}

fn read_json(file: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&raw)
        .ok()
        .filter(serde_json::Value::is_object)
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode a JWT payload without verifying — we only mine identity claims from a
/// token the user's own CLI already trusts.
fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = BASE64_URL
        .decode(payload)
        .or_else(|_| BASE64.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn slot_id_for(harness: HarnessId, account_key: &str) -> String {
    let digest = Sha256::digest(format!("{}:{account_key}", harness_slug(harness)).as_bytes());
    crate::repos::hex(&digest)[..16].to_string()
}

fn codex_plan(plan: Option<&str>) -> Option<String> {
    let plan = plan?;
    let mut chars = plan.chars();
    let first = chars.next()?;
    Some(format!(
        "ChatGPT {}{}",
        first.to_uppercase(),
        chars.as_str()
    ))
}

/// Meter label for a Codex rate-limit window from its `limit_window_seconds`:
/// the free tier's window is a 30-day month (2_592_000s), Plus runs a 5-hour
/// primary (~18_000s) with a weekly secondary (604_800s). A bare "> 1 day =
/// week" rule mislabeled the monthly window "Week"; thresholds in seconds
/// leave the middle gaps to the nearest label rather than guessing a plan.
fn codex_window_label(span_seconds: i64) -> &'static str {
    const DAY: i64 = 86_400;
    if span_seconds >= 28 * DAY {
        "Month"
    } else if span_seconds >= 5 * DAY {
        "Week"
    } else {
        "Session"
    }
}

/// Parse a codex `auth.json` (the live one or a fresh login's).
fn parse_codex_auth(auth: serde_json::Value) -> Option<Detected> {
    if let Some(id_token) = auth
        .get("tokens")
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str())
    {
        let claims = jwt_claims(id_token).unwrap_or_else(|| serde_json::json!({}));
        let oa = claims
            .get("https://api.openai.com/auth")
            .cloned()
            .unwrap_or_default();
        let email = str_field(&claims, "email")?;
        return Some(Detected {
            account_key: str_field(&oa, "chatgpt_account_id").unwrap_or_else(|| email.clone()),
            profile: SlotProfile {
                email,
                display_name: str_field(&claims, "name"),
                organization: None,
                plan: codex_plan(str_field(&oa, "chatgpt_plan_type").as_deref()),
                auth_kind: AgentAuthKind::Oauth,
            },
            credentials: Some(auth),
        });
    }
    let api_key = str_field(&auth, "OPENAI_API_KEY")?;
    let digest = Sha256::digest(api_key.as_bytes());
    let tail: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(Detected {
        account_key: format!("api-key:{}", &crate::repos::hex(&digest)[..12]),
        profile: SlotProfile {
            email: format!("API key ·…{tail}"),
            display_name: None,
            organization: None,
            plan: Some("API key".into()),
            auth_kind: AgentAuthKind::ApiKey,
        },
        credentials: Some(auth),
    })
}

/// ISO string or unix seconds → timestamp.
fn parse_when(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    match value? {
        serde_json::Value::Number(n) => DateTime::<Utc>::from_timestamp(n.as_i64()?, 0),
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        _ => None,
    }
}

fn scan_openai_url(output: &str) -> Option<String> {
    let start = output.find("https://auth.openai.com/")?;
    let rest = &output[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Path of the no-op "browser" script `start_codex_login` hands the CLI via
/// `BROWSER` so `codex login` doesn't open a second authorization tab (the
/// app opens the one tab). Unix only — `webbrowser` only consults `BROWSER`
/// on unix; elsewhere the CLI's own open is left as-is.
#[cfg(unix)]
fn ensure_noop_browser(root: &Path) -> Option<PathBuf> {
    const SCRIPT: &str = "#!/bin/sh\nexit 0\n";
    let path = root.join(".noop-browser");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SCRIPT) {
        std::fs::write(&path, SCRIPT).ok()?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
    }
    Some(path)
}

type LoginChildHandles = (
    Arc<Mutex<Option<tokio::process::Child>>>,
    Arc<Mutex<String>>,
    Arc<Mutex<Option<Option<i32>>>>,
);

/// Wire a spawned login child: both pipes accumulate into one output buffer
/// (the URL can land on either stream), and a monitor polls `try_wait` so the
/// child is reaped without owning it — the cancel path needs concurrent kill
/// access.
fn wire_login_child(mut child: tokio::process::Child) -> LoginChildHandles {
    let output = Arc::new(Mutex::new(String::new()));
    for pipe in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ]
    .into_iter()
    .flatten()
    {
        let sink = output.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut pipe = pipe;
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                lock(&sink).push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
    }
    let child = Arc::new(Mutex::new(Some(child)));
    let exit: Arc<Mutex<Option<Option<i32>>>> = Arc::new(Mutex::new(None));
    {
        let child = child.clone();
        let exit = exit.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut slot = lock(&child);
                    match slot.as_mut().map(|c| c.try_wait()) {
                        None => break,
                        Some(Ok(Some(status))) => {
                            *lock(&exit) = Some(status.code());
                            *slot = None;
                            break;
                        }
                        Some(Ok(None)) => {}
                        Some(Err(_)) => {
                            *lock(&exit) = Some(None);
                            *slot = None;
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
    }
    (child, output, exit)
}

/// Wait briefly for the login child to print its authorize URL (empty when it
/// exits or stays silent past the deadline — the flow still completes via
/// poll; the UI just can't offer an open-browser button).
async fn await_login_url(
    output: &Arc<Mutex<String>>,
    exit: &Arc<Mutex<Option<Option<i32>>>>,
    scan: fn(&str) -> Option<String>,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(url) = scan(&lock(output)) {
            break url;
        }
        if lock(exit).is_some() || Instant::now() > deadline {
            break String::new();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Atomic write via a same-dir temp file + rename; `secret` = 0600 from birth.
fn write_file_atomic(file: &Path, bytes: &[u8], secret: bool) -> Result<(), EngineError> {
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = secret;
        let mut handle = options.open(&tmp)?;
        handle.write_all(bytes)?;
    }
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_labels() {
        assert_eq!(codex_plan(Some("plus")).as_deref(), Some("ChatGPT Plus"));
        assert_eq!(codex_plan(Some("free")).as_deref(), Some("ChatGPT Free"));
        assert_eq!(codex_plan(None), None);
    }

    #[test]
    fn codex_window_labels_track_the_window_span() {
        // Codex free tier: one 30-day window (observed live:
        // limit_window_seconds = 2_592_000) — NOT a week.
        assert_eq!(codex_window_label(2_592_000), "Month");
        // Plus: 5-hour primary + weekly secondary.
        assert_eq!(codex_window_label(18_000), "Session");
        assert_eq!(codex_window_label(604_800), "Week");
        // Unknown/absent span falls back to the shortest label.
        assert_eq!(codex_window_label(0), "Session");
    }

    #[test]
    fn openai_url_scan() {
        assert_eq!(
            scan_openai_url("open https://auth.openai.com/authorize?x=1 in your browser\n")
                .as_deref(),
            Some("https://auth.openai.com/authorize?x=1")
        );
        assert_eq!(scan_openai_url("nothing here"), None);
    }

    #[cfg(unix)]
    #[test]
    fn noop_browser_script_is_stable_and_executable() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = ensure_noop_browser(root.path()).expect("script");
        assert!(path.ends_with(".noop-browser"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\nexit 0\n"
        );
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(path.metadata().unwrap().permissions().mode() & 0o111 != 0);
        }
        // A second ensure is idempotent (same path, same content).
        assert_eq!(ensure_noop_browser(root.path()), Some(path));
    }
}
