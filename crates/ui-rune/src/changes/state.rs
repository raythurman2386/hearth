//! The `Changes` pane state: diff watching, scope/branch state, folding and
//! the highlight requests.
//!
//! Rune has no entity graph: background results arrive as [`ChangesEvent`]s on
//! a plain channel, and [`ChangesState::poll`] applies them on the UI thread.
//! [`ChangesState::refresh`] captures the parts of `App` the pane needs (the
//! selected chat, engine, bridge and staged comments) and starts any missing
//! work. The shell calls the module-level [`poll`] once per `App::poll`.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use hearth_proto::{CheckoutDiff, GitHistoryCommit, GitHistoryPage};
use hearth_rpc::{RpcError, methods};
use rune::TextEdit;

use crate::bridge::Bridge;
use crate::comments::{CommentSide, DiffComment};
use crate::engine::EngineHandle;
use crate::settings::UiSettings;
use crate::state::App;

use super::*;

/// The selected chat, flattened to the fields the diff pane addresses.
#[derive(Debug, Clone)]
pub struct ChangesChat {
    pub id: String,
    pub device_id: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub checkout_id: Option<String>,
}

/// An immutable capture of the root state for one refresh. Cloneable so the
/// async tasks can carry the pieces they need without holding `&App`.
#[derive(Clone)]
pub struct ChangesEnv {
    pub bridge: Bridge,
    pub engine: Option<EngineHandle>,
    pub data_dir: Option<PathBuf>,
    pub local_device_id: Option<String>,
    pub chat: Option<ChangesChat>,
    pub comments: Vec<DiffComment>,
    /// `App::composer_key()` — the comments map key and the draft's owner.
    pub comments_key: String,
    /// `crate::shell::right_pane_open(app)`: a closed pane watches nothing, but
    /// keeps its last snapshot for an instant reopen.
    pub visible: bool,
}

impl ChangesEnv {
    pub fn capture(app: &App) -> Self {
        let chat = app
            .selected_chat
            .as_deref()
            .and_then(|id| app.chats.iter().find(|chat| chat.id == id))
            .map(|chat| ChangesChat {
                id: chat.id.clone(),
                device_id: chat.device_id.clone(),
                cwd: chat.cwd.clone(),
                branch: chat.branch.clone(),
                checkout_id: chat.checkout_id.clone(),
            });
        let comments_key = app.composer_key();
        let comments = app.diff_comments(&comments_key).to_vec();
        Self {
            bridge: app.bridge.clone(),
            engine: app.engine.clone(),
            data_dir: app.data_dir.clone(),
            local_device_id: app.local_device_id.clone(),
            chat,
            comments,
            comments_key,
            visible: crate::shell::right_pane_open(app),
        }
    }

    /// The device the diff watch must target: `None` for the connected engine,
    /// `Some(id)` for a remote chat's host (relay-forwarded).
    pub fn target(&self) -> Option<String> {
        let chat = self.chat.as_ref()?;
        (self.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone())
    }
}

/// Parsed content + the identity it was parsed from.
#[derive(Clone)]
pub struct ParsedDiff {
    /// `checkout_id:checksum:scope` — identity of the parsed content.
    pub key: String,
    pub truncated: bool,
    pub additions: u32,
    pub deletions: u32,
    pub file_count: usize,
    pub files: Arc<Vec<FileDiff>>,
}

/// Where a file's highlight request stands. `Pending` and `Plain` produce no
/// runs; `Excerpt` is the hunk-only parse available immediately, `Ready` the
/// full-document parse once the engine returned the sources.
pub enum DiffHighlightState {
    Pending,
    Ready(Arc<DiffHighlights>),
    Excerpt(Arc<DiffHighlights>),
    Plain,
}

pub struct HighlightSlot {
    pub fingerprint: u64,
    pub state: DiffHighlightState,
}

/// Results posted by background work. Every request carries its identity key
/// so a late result from a previous chat/scope is dropped instead of applied.
enum ChangesEvent {
    Diffs {
        generation: u64,
        value: serde_json::Value,
    },
    WatchError {
        generation: u64,
        message: String,
    },
    Branches {
        key: String,
        result: Result<Vec<String>, String>,
    },
    Scoped {
        key: String,
        result: Result<CheckoutDiff, String>,
    },
    Parsed {
        key: String,
        files: Vec<FileDiff>,
    },
    Highlights {
        path: String,
        fingerprint: u64,
        full: bool,
        result: Option<Arc<DiffHighlights>>,
    },
    History {
        key: String,
        reset: bool,
        result: Result<GitHistoryPage, String>,
    },
    FetchedAll {
        key: String,
        result: Result<(), String>,
    },
    CopiedExpired {
        sha: String,
    },
    SettleFolds,
}

/// The Changes pane state. Lazy: no RPC until an engine and a selected chat
/// exist; [`ChangesState::refresh`] starts the watch.
pub struct ChangesState {
    rx: Receiver<ChangesEvent>,
    tx: Sender<ChangesEvent>,
    env: Option<ChangesEnv>,
    settings_loaded: bool,

    diffs: Vec<CheckoutDiff>,
    watch_task: Option<tokio::task::JoinHandle<()>>,
    watch_target: Option<String>,
    watch_generation: u64,
    started: bool,
    /// Watch error banner; the last content stays visible under it.
    pub error: Option<String>,

    pub parsed: Option<ParsedDiff>,
    parse_pending: Option<String>,

    pub folds: HashMap<String, FileFold>,
    /// The flattened row model the list virtualizes over (line granularity;
    /// collapsed bodies excluded) + each file's row span within it.
    pub rows: Vec<DiffRow>,
    pub row_ranges: Vec<Range<usize>>,
    /// Bumped whenever the row model changes in place; tells `virtual_rows`
    /// that its keys are new.
    pub row_version: u64,
    /// The virtual list's last reported visible range.
    pub visible_rows: Range<usize>,

    highlights: HashMap<String, HighlightSlot>,

    /// What the pane diffs against (toolbar dropdown).
    pub scope: DiffScope,
    /// Unified or side-by-side (toolbar toggle, persisted per user).
    pub mode: DiffMode,
    /// Comparison ref for [`DiffScope::Branch`] — preset to the repo's
    /// default branch once the branch list lands.
    pub base_ref: Option<String>,
    pub branches: Vec<String>,
    branches_for: Option<String>,

    /// One-shot scoped capture (Branch / Latest turn / Commit) + its fetch key.
    scoped: Option<CheckoutDiff>,
    scoped_for: Option<String>,
    pub scoped_error: Option<String>,
    scoped_inflight: Option<String>,

    /// Pinned commit for a [`DiffScope::Commit`] pane.
    pub commit: Option<GitHistoryCommit>,

    /// History scope: first page + paging cursor, loaded through
    /// `ListGitHistory` (the dedicated history module can take this over).
    pub history: Vec<GitHistoryCommit>,
    pub history_loading: bool,
    pub history_error: Option<String>,
    pub history_cursor: Option<usize>,
    pub history_lead_count: Option<usize>,
    pub history_total: Option<usize>,
    /// The commit HEAD points at — the lane graph's head ring.
    pub head_sha: Option<String>,
    /// Bumped whenever the history rows are replaced (the virtual list's keys
    /// are SHAs, so a same-length reset still needs a version change).
    pub history_version: u64,
    /// The commit cell's measured width; drives the ref-badge overflow count.
    pub history_commit_width: f32,
    history_for: Option<String>,
    /// `FetchAll` in flight for `fetch_for` (the history header's Fetch button).
    pub fetching_all: bool,
    pub fetch_error: Option<String>,
    fetch_for: Option<String>,
    /// The SHA chip the user just copied; reverts after 1.2 s.
    pub copied_sha: Option<String>,

    /// Long diff lines: `false` (default) puts the code in a horizontal
    /// scroll viewport with the gutters pinned; `true` wraps them.
    pub wrap: bool,

    /// Checkout change-request watches (PR badges) — independent of pane
    /// visibility, polled from `changes::poll`.
    pub change_requests: ChangeRequestsState,

    /// Dropdown state (the gpui popup replacement: anchored inline).
    pub scope_menu_open: bool,
    pub ref_menu_open: bool,
    pub ref_search: TextEdit,

    /// Only ever one: a second `+` moves the card rather than stacking two
    /// half-written notes.
    pub draft: Option<CommentDraft>,
    pub hover: Option<HoverRow>,
    /// Snapshot of the staged comments the row model was built from.
    pub comments: Vec<DiffComment>,
    comment_key: u64,
    comments_dirty: bool,
}

impl Default for ChangesState {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            env: None,
            settings_loaded: false,
            diffs: Vec::new(),
            watch_task: None,
            watch_target: None,
            watch_generation: 0,
            started: false,
            error: None,
            parsed: None,
            parse_pending: None,
            folds: HashMap::new(),
            rows: Vec::new(),
            row_ranges: Vec::new(),
            row_version: 0,
            visible_rows: 0..0,
            highlights: HashMap::new(),
            scope: DiffScope::default(),
            mode: DiffMode::default(),
            base_ref: None,
            branches: Vec::new(),
            branches_for: None,
            scoped: None,
            scoped_for: None,
            scoped_error: None,
            scoped_inflight: None,
            commit: None,
            history: Vec::new(),
            history_loading: false,
            history_error: None,
            history_cursor: None,
            history_lead_count: None,
            history_total: None,
            head_sha: None,
            history_version: 0,
            history_commit_width: 320.0,
            history_for: None,
            fetching_all: false,
            fetch_error: None,
            fetch_for: None,
            copied_sha: None,
            wrap: true,
            change_requests: ChangeRequestsState::default(),
            scope_menu_open: false,
            ref_menu_open: false,
            ref_search: TextEdit::new(""),
            draft: None,
            hover: None,
            comments: Vec::new(),
            comment_key: 0,
            comments_dirty: false,
        }
    }
}

impl ChangesState {
    // ---- per-frame sync -------------------------------------------------

    /// Capture the current state and start any missing work. Returns whether
    /// the visible model changed.
    pub fn refresh(&mut self, env: &ChangesEnv) -> bool {
        let mut changed = false;
        self.env = Some(env.clone());
        if !self.settings_loaded
            && let Some(dir) = env.data_dir.as_deref()
        {
            self.settings_loaded = true;
            self.mode = DiffMode::from_split(UiSettings::load(dir).diff_split);
        }
        // A draft belongs to the checkout it was opened over.
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.key != env.comments_key)
        {
            self.draft = None;
            changed = true;
        }
        let draft = self.draft_anchor();
        let key = comment_state_key(&env.comments, draft.as_ref());
        if self.comments_dirty || key != self.comment_key {
            self.comment_key = key;
            self.comments = env.comments.clone();
            self.comments_dirty = false;
            if self.parsed.is_some() {
                self.reflatten();
                changed = true;
            }
        }

        // A closed right pane watches nothing and parses nothing; the last
        // snapshot stays so reopening is instant. Badge watches live in
        // `ChangeRequestsState`, which `changes::poll` always pumps.
        if !env.visible {
            self.suspend();
            return changed;
        }

        changed |= self.sync_watch();
        if self.scope == DiffScope::Branch {
            self.ensure_branches();
        }
        if self.scope == DiffScope::History {
            self.ensure_history();
        } else {
            self.ensure_scoped();
        }
        changed |= self.sync_parse();
        if self.settle_folds_ready() {
            changed = true;
        }
        self.request_highlights();
        changed
    }

    /// Stop the diff watch while the right pane is closed. The parsed rows and
    /// the last diff frame stay, so reopening paints instantly while a fresh
    /// watch resubscribes.
    fn suspend(&mut self) {
        if let Some(task) = self.watch_task.take() {
            task.abort();
        }
        self.started = false;
        self.watch_target = None;
        self.scoped_inflight = None;
    }

    /// Drain background results. The shell calls this once per frame.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            changed |= self.apply(event);
        }
        if self.settle_folds_ready() {
            changed = true;
        }
        changed
    }

    fn apply(&mut self, event: ChangesEvent) -> bool {
        match event {
            ChangesEvent::Diffs { generation, value } => {
                if generation != self.watch_generation {
                    return false;
                }
                let had_error = self.error.take().is_some();
                apply_diff_frame(&mut self.diffs, value) || had_error
            }
            ChangesEvent::WatchError {
                generation,
                message,
            } => {
                if generation != self.watch_generation {
                    return false;
                }
                let changed = self.error.as_deref() != Some(message.as_str());
                self.error = Some(message);
                changed
            }
            ChangesEvent::Branches { key, result } => {
                if self.branches_for.as_deref() != Some(key.as_str()) {
                    return false;
                }
                match result {
                    Ok(branches) => {
                        self.branches = branches;
                        let keep = self
                            .base_ref
                            .as_ref()
                            .is_some_and(|base| self.branches.contains(base));
                        if !keep {
                            let current = self
                                .env
                                .as_ref()
                                .and_then(|env| env.chat.as_ref())
                                .and_then(|chat| chat.branch.as_deref());
                            self.base_ref = default_base_ref(&self.branches, current);
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "changes: branch list failed");
                        // Allow a retry on the next refresh.
                        self.branches_for = None;
                    }
                }
                true
            }
            ChangesEvent::Scoped { key, result } => {
                if self.scoped_inflight.as_deref() != Some(key.as_str()) {
                    return false;
                }
                self.scoped_inflight = None;
                match result {
                    Ok(diff) => {
                        self.scoped = Some(diff);
                        self.scoped_error = None;
                    }
                    Err(err) => {
                        self.scoped = None;
                        self.scoped_error = Some(err);
                    }
                }
                self.scoped_for = Some(key);
                true
            }
            ChangesEvent::Parsed { key, files } => {
                if self.parse_pending.as_deref() != Some(key.as_str()) {
                    return false;
                }
                if self.current_parse_key().as_deref() != Some(key.as_str()) {
                    return false;
                }
                self.parse_pending = None;
                let file_count = if self
                    .active_diff()
                    .is_some_and(|diff| !diff.files.is_empty())
                {
                    self.active_diff().map(|diff| diff.files.len()).unwrap_or(0)
                } else {
                    files.len()
                };
                let (truncated, additions, deletions) = self
                    .active_diff()
                    .map(|diff| (diff.truncated, diff.additions, diff.deletions))
                    .unwrap_or((false, 0, 0));
                self.materialize(ParsedDiff {
                    key,
                    truncated,
                    additions,
                    deletions,
                    file_count,
                    files: Arc::new(files),
                });
                true
            }
            ChangesEvent::Highlights {
                path,
                fingerprint,
                full,
                result,
            } => {
                let Some(slot) = self.highlights.get_mut(&path) else {
                    return false;
                };
                if slot.fingerprint != fingerprint {
                    return false;
                }
                match (full, result) {
                    (false, result) => {
                        if !matches!(slot.state, DiffHighlightState::Pending) {
                            return false;
                        }
                        slot.state = match result {
                            Some(highlights) => DiffHighlightState::Excerpt(highlights),
                            None => DiffHighlightState::Plain,
                        };
                        true
                    }
                    (true, Some(highlights)) => {
                        slot.state = DiffHighlightState::Ready(highlights);
                        true
                    }
                    (true, None) => false,
                }
            }
            ChangesEvent::History { key, reset, result } => {
                if self.history_for.as_deref() != Some(key.as_str()) {
                    return false;
                }
                self.history_loading = false;
                match result {
                    Ok(page) => {
                        self.history_error = None;
                        if reset {
                            self.history = page.commits;
                            self.history_total = page.total_count;
                            self.history_lead_count = page.head_commit_count;
                        } else {
                            let mut seen: HashSet<String> = self
                                .history
                                .iter()
                                .map(|commit| commit.sha.clone())
                                .collect();
                            for commit in page.commits {
                                if seen.insert(commit.sha.clone()) {
                                    self.history.push(commit);
                                }
                            }
                            if page.total_count.is_some() {
                                self.history_total = page.total_count;
                            }
                            if page.head_commit_count.is_some() {
                                self.history_lead_count = page.head_commit_count;
                            }
                        }
                        self.head_sha = page.head_sha;
                        self.history_cursor = page.next_cursor;
                        self.history_version = self.history_version.wrapping_add(1);
                    }
                    Err(err) => {
                        self.history_error = Some(err);
                        // Keep the key: a retry is explicit (refresh button).
                    }
                }
                true
            }
            ChangesEvent::FetchedAll { key, result } => {
                if self.fetch_for.as_deref() != Some(key.as_str()) {
                    return false;
                }
                self.fetching_all = false;
                self.fetch_for = None;
                match result {
                    Ok(()) => {
                        self.fetch_error = None;
                        // Cancel any pre-fetch page request: the next page must
                        // observe the updated remote refs.
                        self.history_loading = false;
                        self.history_for = None;
                        self.ensure_history();
                    }
                    Err(err) => self.fetch_error = Some(err),
                }
                true
            }
            ChangesEvent::CopiedExpired { sha } => {
                if self.copied_sha.as_deref() == Some(sha.as_str()) {
                    self.copied_sha = None;
                    true
                } else {
                    false
                }
            }
            ChangesEvent::SettleFolds => self.settle_folds_ready(),
        }
    }

    // ---- derived queries ------------------------------------------------

    pub fn comments_key(&self) -> &str {
        self.env
            .as_ref()
            .map(|env| env.comments_key.as_str())
            .unwrap_or("")
    }

    fn resolved(&self) -> Option<&CheckoutDiff> {
        let env = self.env.as_ref()?;
        let chat = env.chat.as_ref()?;
        resolve_diff_parts(
            &self.diffs,
            chat.checkout_id.as_deref(),
            Some(chat.device_id.as_str()),
            chat.cwd.as_deref(),
        )
    }

    /// The checkout root the scoped RPCs address: the watch-resolved diff's
    /// canonical cwd when available, else the chat row's own.
    fn scoped_cwd(&self) -> Option<String> {
        if let Some(diff) = self.resolved() {
            return Some(diff.cwd.clone());
        }
        self.env.as_ref()?.chat.as_ref()?.cwd.clone()
    }

    /// The diff the pane currently displays: the watch stream for the working
    /// tree, the one-shot scoped capture otherwise.
    pub fn active_diff(&self) -> Option<&CheckoutDiff> {
        match self.scope {
            DiffScope::WorkingTree => self.resolved(),
            DiffScope::Branch | DiffScope::LatestTurn | DiffScope::Commit => self.scoped.as_ref(),
            DiffScope::History => None,
        }
    }

    /// Scope discriminant folded into the parse key, so a scope or base switch
    /// re-parses even when checksums collide.
    fn scope_key(&self) -> String {
        match self.scope {
            DiffScope::WorkingTree => "wt".to_string(),
            DiffScope::Branch => format!("br:{}", self.base_ref.as_deref().unwrap_or("")),
            DiffScope::LatestTurn => "turn".to_string(),
            DiffScope::History => "history".to_string(),
            DiffScope::Commit => format!(
                "commit:{}",
                self.commit.as_ref().map(|c| c.sha.as_str()).unwrap_or("")
            ),
        }
    }

    fn parse_key(&self, diff: &CheckoutDiff) -> String {
        format!(
            "{}:{}:{}",
            diff.checkout_id,
            diff.checksum,
            self.scope_key()
        )
    }

    fn current_parse_key(&self) -> Option<String> {
        self.active_diff().map(|diff| self.parse_key(diff))
    }

    /// Headless-scene helper: install a parsed patch as the pane's content so
    /// the diff/history layouts can be rendered without an engine.
    #[doc(hidden)]
    pub fn seed_demo(&mut self, patch: &str, chat_id: &str, cwd: &str) {
        let files = super::model::parse_patch(patch);
        let (rows, ranges) = super::rows::flatten_rows(&files, &[], None, self.mode, |_| false);
        let additions = files.iter().map(|file| file.additions).sum();
        let deletions = files.iter().map(|file| file.deletions).sum();
        self.parsed = Some(ParsedDiff {
            key: "demo".into(),
            truncated: false,
            additions,
            deletions,
            file_count: files.len(),
            files: Arc::new(files),
        });
        self.rows = rows;
        self.row_ranges = ranges;
        self.row_version = self.row_version.wrapping_add(1);
        self.scope = DiffScope::LatestTurn;
        self.scoped = Some(CheckoutDiff {
            checkout_id: "demo".into(),
            device_id: "dev-local".into(),
            cwd: cwd.into(),
            patch: patch.into(),
            files: Vec::new(),
            additions,
            deletions,
            truncated: false,
            checksum: "demo".into(),
            updated_at: chrono::Utc::now(),
        });
        // Match `ensure_scoped`'s context key so the demo is not cleared and
        // re-fetched on the first poll.
        self.scoped_for = Some(format!("local|{chat_id}|{cwd}|{}|||", self.scope.mode()));
    }

    /// Headless-scene helper: install a history page without an engine.
    #[doc(hidden)]
    pub fn seed_history_demo(
        &mut self,
        cwd: &str,
        commits: Vec<GitHistoryCommit>,
        head_sha: Option<String>,
    ) {
        self.scope = DiffScope::History;
        self.history_lead_count = Some(commits.len());
        self.history_total = Some(commits.len());
        self.history = commits;
        self.history_loading = false;
        self.history_error = None;
        self.history_cursor = None;
        self.head_sha = head_sha;
        self.history_for = Some(format!("local:{cwd}"));
        self.history_version = self.history_version.wrapping_add(1);
    }

    pub fn phase(&self, app: &App) -> DiffPhase {
        if app.selected_chat.is_none() {
            // Nothing to prepare on the new-chat canvas: quiet empty state.
            return DiffPhase::Clean;
        }
        if self.scope == DiffScope::History {
            return if self.history.is_empty() && !self.history_loading {
                DiffPhase::Clean
            } else {
                DiffPhase::List
            };
        }
        diff_phase(self.active_diff())
    }

    pub fn file_count(&self) -> usize {
        self.parsed
            .as_ref()
            .map(|parsed| parsed.file_count)
            .or_else(|| self.active_diff().map(|diff| diff.files.len()))
            .unwrap_or(0)
    }

    pub fn additions(&self) -> u32 {
        self.parsed
            .as_ref()
            .map(|parsed| parsed.additions)
            .or_else(|| self.active_diff().map(|diff| diff.additions))
            .unwrap_or(0)
    }

    pub fn deletions(&self) -> u32 {
        self.parsed
            .as_ref()
            .map(|parsed| parsed.deletions)
            .or_else(|| self.active_diff().map(|diff| diff.deletions))
            .unwrap_or(0)
    }

    pub fn truncated(&self) -> bool {
        self.parsed
            .as_ref()
            .map(|parsed| parsed.truncated)
            .or_else(|| self.active_diff().map(|diff| diff.truncated))
            .unwrap_or(false)
    }

    pub fn tab_title(&self) -> String {
        if let Some(commit) = &self.commit {
            let subject = commit.subject.trim();
            if !subject.is_empty() {
                return subject.to_string();
            }
            return commit.sha.chars().take(7).collect();
        }
        self.scope.label().to_string()
    }

    /// The selected chat's checked-out branch (the branch-scope ref label).
    pub fn chat_branch(&self) -> Option<&str> {
        self.env.as_ref()?.chat.as_ref()?.branch.as_deref()
    }

    /// The selected chat's cwd (the history tab's repo label).
    pub fn chat_cwd(&self) -> Option<&str> {
        self.env.as_ref()?.chat.as_ref()?.cwd.as_deref()
    }

    pub fn comments_for(&self, path: &str) -> Vec<DiffComment> {
        self.comments
            .iter()
            .filter(|comment| comment.path == path)
            .cloned()
            .collect()
    }

    pub fn highlight_for(&self, path: &str) -> Option<Arc<DiffHighlights>> {
        match self.highlights.get(path).map(|slot| &slot.state) {
            Some(DiffHighlightState::Ready(highlights))
            | Some(DiffHighlightState::Excerpt(highlights)) => Some(highlights.clone()),
            _ => None,
        }
    }

    // ---- comments -------------------------------------------------------

    pub fn comments_changed(&mut self) {
        self.comments_dirty = true;
    }

    pub fn draft_anchor(&self) -> Option<(String, CommentSide, u32)> {
        self.draft
            .as_ref()
            .map(|draft| (draft.path.clone(), draft.side, draft.line))
    }

    pub fn draft_anchor_in(&self, path: &str) -> Option<(CommentSide, u32)> {
        self.draft
            .as_ref()
            .filter(|draft| draft.path == path)
            .map(|draft| (draft.side, draft.line))
    }

    pub fn draft_edit(&mut self) -> &mut TextEdit {
        &mut self
            .draft
            .as_mut()
            .expect("the draft field is only rendered while a draft is open")
            .edit
    }

    pub fn old_path_of(&self, path: &str) -> Option<String> {
        self.parsed
            .as_ref()?
            .files
            .iter()
            .find(|file| file.path == path)?
            .old_path
            .clone()
    }

    pub fn set_hover(&mut self, path: &str, anchor: Option<(CommentSide, u32)>) {
        self.hover = anchor.map(|(side, line)| HoverRow {
            path: path.to_string(),
            side,
            line,
        });
    }

    pub fn clear_hover(&mut self, path: &str, anchor: (CommentSide, u32)) {
        if self.hovering(path, anchor) {
            self.hover = None;
        }
    }

    pub fn hovering(&self, path: &str, anchor: (CommentSide, u32)) -> bool {
        self.hover
            .as_ref()
            .is_some_and(|hover| hover.path == path && (hover.side, hover.line) == anchor)
    }

    // ---- rows / folds ---------------------------------------------------

    pub fn set_visible(&mut self, rows: Range<usize>) {
        self.visible_rows = rows;
    }

    fn materialize(&mut self, parsed: ParsedDiff) {
        self.folds.clear();
        self.highlights.clear();
        let files = parsed.files.clone();
        let (rows, ranges) = flatten_rows(
            &files,
            &self.comments,
            self.draft_anchor()
                .as_ref()
                .map(|(path, side, line)| (path.as_str(), *side, *line)),
            self.mode,
            |_| false,
        );
        self.rows = rows;
        self.row_ranges = ranges;
        self.row_version = self.row_version.wrapping_add(1);
        self.visible_rows = 0..self.rows.len().min(40);
        self.parsed = Some(parsed);
    }

    fn replace_file_body(&mut self, file_ix: usize, new_body: Vec<DiffRow>) {
        let Some(range) = self.row_ranges.get(file_ix).cloned() else {
            return;
        };
        let body = range.start + 1..range.end;
        let delta = new_body.len() as isize - body.len() as isize;
        self.rows.splice(body, new_body);
        self.row_ranges[file_ix] = range.start..(range.end as isize + delta) as usize;
        for r in &mut self.row_ranges[file_ix + 1..] {
            *r = (r.start as isize + delta) as usize..(r.end as isize + delta) as usize;
        }
        self.row_version = self.row_version.wrapping_add(1);
    }

    pub fn toggle_fold(&mut self, file_ix: usize) {
        let Some(path) = self
            .parsed
            .as_ref()
            .and_then(|parsed| parsed.files.get(file_ix))
            .map(|file| file.path.clone())
        else {
            return;
        };
        self.toggle_fold_path(&path);
    }

    /// Fold/unfold a file by its path. Handlers must be path-keyed: a virtual
    /// row kept by `memo` may sit at a different index after rows move.
    pub fn toggle_fold_path(&mut self, path: &str) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let Some(file_ix) = parsed.files.iter().position(|file| file.path == path) else {
            return;
        };
        let file = parsed.files[file_ix].clone();
        let expanded_height = body_height_with(
            &file,
            &self.comments_for(&file.path),
            self.draft_anchor_in(&file.path),
            self.mode,
        );
        let fold = self.folds.entry(file.path.clone()).or_default();
        let currently_collapsed = fold.collapsed;
        fold.from = if currently_collapsed {
            0.0
        } else {
            expanded_height
        };
        fold.to = if currently_collapsed {
            expanded_height
        } else {
            0.0
        };
        fold.collapsed = !currently_collapsed;
        fold.epoch += 1;
        fold.toggled_at = Some(Instant::now());
        // The body swaps to ONE stand-in row; the settle sweep replaces it
        // with steady rows once the window elapses.
        self.replace_file_body(
            file_ix,
            vec![DiffRow::FoldingBody {
                file: file_ix as u32,
            }],
        );
        self.spawn_settle();
    }

    fn spawn_settle(&self) {
        let Some(env) = self.env.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let wake = env.bridge.clone();
        env.bridge.spawn(async move {
            tokio::time::sleep(FOLD_TWEEN_WINDOW).await;
            let _ = tx.send(ChangesEvent::SettleFolds);
            wake.wake();
        });
    }

    /// Replace every settled folding stand-in with its steady-state rows.
    /// Returns whether anything changed.
    fn settle_folds_ready(&mut self) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        let files = parsed.files.clone();
        let mut changed = false;
        for file_ix in (0..self.row_ranges.len()).rev() {
            let range = self.row_ranges[file_ix].clone();
            let folding = self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                });
            if !folding {
                continue;
            }
            let Some(file) = files.get(file_ix) else {
                continue;
            };
            let fold = self.folds.get(&file.path).copied().unwrap_or_default();
            if fold.animating() {
                continue;
            }
            let body = if fold.collapsed {
                Vec::new()
            } else {
                body_rows(
                    file_ix as u32,
                    file,
                    &self.comments_for(&file.path),
                    self.draft_anchor_in(&file.path),
                    self.mode,
                )
            };
            self.replace_file_body(file_ix, body);
            changed = true;
        }
        changed
    }

    fn all_collapsed(&self) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        !parsed.files.is_empty()
            && parsed.files.iter().all(|file| {
                self.folds
                    .get(&file.path)
                    .is_some_and(|fold| fold.collapsed)
            })
    }

    /// Collapse every file section, or expand them all when everything is
    /// already shut (the toolbar's fold button, t3code parity).
    pub fn toggle_collapse_all(&mut self) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let collapse = !self.all_collapsed();
        let files = parsed.files.clone();
        for file in files.iter() {
            let fold = self.folds.entry(file.path.clone()).or_default();
            fold.collapsed = collapse;
            fold.toggled_at = None;
        }
        self.reflatten();
    }

    /// Swap unified ⇄ split (toolbar toggle). The parse is untouched — only
    /// the flattening changes.
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.toggled();
        self.hover = None;
        if let Some(env) = self.env.clone()
            && let Some(dir) = env.data_dir
        {
            let split = self.mode.is_split();
            env.bridge.spawn(async move {
                let mut settings = UiSettings::load(&dir);
                settings.diff_split = split;
                if let Err(err) = settings.save(&dir) {
                    tracing::warn!(error = %err, "could not persist diff layout");
                }
            });
        }
        self.reflatten();
    }

    /// Wrap long code lines instead of scrolling them horizontally. Row heights
    /// become measured rather than analytic, so the virtual list re-measures.
    pub fn set_wrap(&mut self, wrap: bool) {
        if self.wrap != wrap {
            self.wrap = wrap;
            self.row_version = self.row_version.wrapping_add(1);
        }
    }

    pub fn toggle_wrap(&mut self) {
        self.set_wrap(!self.wrap);
    }

    fn reflatten(&mut self) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let files = parsed.files.clone();
        let collapsed: Vec<bool> = files
            .iter()
            .map(|file| {
                self.folds
                    .get(&file.path)
                    .is_some_and(|fold| fold.collapsed)
            })
            .collect();
        let (rows, ranges) = flatten_rows(
            &files,
            &self.comments,
            self.draft_anchor()
                .as_ref()
                .map(|(path, side, line)| (path.as_str(), *side, *line)),
            self.mode,
            |ix| collapsed.get(ix).copied().unwrap_or(false),
        );
        self.rows = rows;
        self.row_ranges = ranges;
        self.row_version = self.row_version.wrapping_add(1);
    }

    // ---- scope + menus --------------------------------------------------

    pub fn set_scope(&mut self, scope: DiffScope) {
        if self.scope != scope {
            self.scope = scope;
            self.scope_menu_open = false;
            // Content from the previous scope must not show under the new one.
            self.parsed = None;
            self.rows.clear();
            self.row_ranges.clear();
            self.row_version = self.row_version.wrapping_add(1);
        }
    }

    pub fn set_base_ref(&mut self, base: String) {
        if self.base_ref.as_deref() != Some(base.as_str()) {
            self.base_ref = Some(base);
            self.ref_menu_open = false;
            self.scoped_for = None;
            self.scoped_inflight = None;
            self.scoped_error = None;
            self.parsed = None;
            self.rows.clear();
            self.row_ranges.clear();
            self.row_version = self.row_version.wrapping_add(1);
        }
    }

    pub fn toggle_scope_menu(&mut self) {
        self.scope_menu_open = !self.scope_menu_open;
        self.ref_menu_open = false;
    }

    pub fn close_scope_menu(&mut self) {
        self.scope_menu_open = false;
    }

    pub fn toggle_ref_menu(&mut self) {
        self.ref_menu_open = !self.ref_menu_open;
        self.scope_menu_open = false;
    }

    pub fn close_ref_menu(&mut self) {
        self.ref_menu_open = false;
    }

    pub fn pin_commit(&mut self, commit: GitHistoryCommit) {
        self.commit = Some(commit);
        self.scope = DiffScope::Commit;
        self.scoped = None;
        self.scoped_for = None;
        self.scoped_inflight = None;
        self.parsed = None;
        self.rows.clear();
        self.row_ranges.clear();
        self.row_version = self.row_version.wrapping_add(1);
    }

    /// Leave a commit-pinned pane and go back to the working tree.
    pub fn unpin_commit(&mut self) {
        if self.commit.take().is_some() {
            self.scope = DiffScope::WorkingTree;
            self.parsed = None;
            self.rows.clear();
            self.row_ranges.clear();
            self.row_version = self.row_version.wrapping_add(1);
        }
    }

    /// Ranked branch indices for the open ref menu (case-insensitive substring
    /// match, earliest match first).
    pub fn ref_menu_rows(&self) -> Vec<usize> {
        filter_indices(&self.ref_search.text, &self.branches)
    }

    /// Retry the history first page.
    pub fn refresh_history(&mut self) {
        self.history_for = None;
        self.history_error = None;
        self.ensure_history();
    }

    /// The commit cell's measured width (ref-badge overflow math).
    pub fn set_history_commit_width(&mut self, width: f32) {
        if (self.history_commit_width - width).abs() >= 0.5 {
            self.history_commit_width = width;
        }
    }

    /// `FetchAll` for the selected checkout, then reload the first history page
    /// so newly fetched remote refs appear.
    pub fn fetch_all(&mut self) {
        if self.fetching_all {
            return;
        }
        let Some(env) = self.env.clone() else {
            return;
        };
        let Some(cwd) = self.scoped_cwd() else {
            return;
        };
        let Some(engine) = env.engine.clone() else {
            return;
        };
        let Some(key) = self.history_key() else {
            return;
        };
        let target = env.target();
        self.fetching_all = true;
        self.fetch_for = Some(key.clone());
        self.fetch_error = None;
        let tx = self.tx.clone();
        let wake = env.bridge.clone();
        env.bridge.spawn(async move {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), serde_json::Value::String(cwd));
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client
                .call(methods::FETCH_ALL, serde_json::Value::Object(params))
                .await
                .map(|_| ())
                .map_err(|err| err.to_string());
            let _ = tx.send(ChangesEvent::FetchedAll { key, result });
            wake.wake();
        });
    }

    /// Remember a just-copied SHA for 1.2 s (the chip reads "Copied").
    pub fn copy_sha(&mut self, sha: String) {
        self.copied_sha = Some(sha.clone());
        let Some(env) = self.env.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let wake = env.bridge.clone();
        env.bridge.spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            let _ = tx.send(ChangesEvent::CopiedExpired { sha });
            wake.wake();
        });
    }

    pub fn load_older_history(&mut self) {
        let Some(cursor) = self.history_cursor else {
            return;
        };
        if self.history_loading {
            return;
        }
        self.fetch_history(cursor, false);
    }

    // ---- async kicks ----------------------------------------------------

    /// Start the `WatchCheckoutDiffs` subscription (idempotent per target).
    fn sync_watch(&mut self) -> bool {
        let target = self.env.as_ref().and_then(ChangesEnv::target);
        if self.started && self.watch_target == target {
            return false;
        }
        let Some(env) = self.env.clone() else {
            return false;
        };
        let Some(_engine) = env.engine.clone() else {
            // Engine still booting — retry on the next refresh.
            return false;
        };
        let mut changed = false;
        if self.started {
            // Retarget: rows from the previous device would resolve against
            // the wrong checkouts, so clear them.
            self.diffs.clear();
            self.error = None;
            changed = true;
        }
        self.started = true;
        self.watch_target = target.clone();
        self.watch_generation = self.watch_generation.wrapping_add(1);
        if let Some(task) = self.watch_task.take() {
            task.abort();
        }
        self.watch_task = Some(spawn_diff_watch(
            &env,
            target,
            self.watch_generation,
            self.tx.clone(),
        ));
        changed
    }

    /// Fetch the branch list for the selected chat's checkout (idempotent per
    /// device+cwd); the repo's default branch becomes the comparison base
    /// unless the user already picked one that still exists.
    fn ensure_branches(&mut self) {
        let Some(env) = self.env.clone() else {
            return;
        };
        let Some(cwd) = self.scoped_cwd() else {
            return;
        };
        let Some(engine) = env.engine.clone() else {
            return;
        };
        let target = env.target();
        let key = format!("{}:{}", target.as_deref().unwrap_or("local"), cwd);
        if self.branches_for.as_deref() == Some(key.as_str()) {
            return;
        }
        self.branches_for = Some(key.clone());
        let tx = self.tx.clone();
        let wake = env.bridge.clone();
        env.bridge.spawn(async move {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), serde_json::Value::String(cwd));
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client
                .call(methods::LIST_BRANCHES, serde_json::Value::Object(params))
                .await
                .map_err(|err| err.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<String>>(value).map_err(|err| err.to_string())
                });
            let _ = tx.send(ChangesEvent::Branches { key, result });
            wake.wake();
        });
    }

    /// Keep the one-shot scoped capture fresh. The fetch key folds in the
    /// watch checksum, so any working-tree change (or commit — HEAD rides the
    /// checksum) re-captures; a context change (chat/scope/base) clears the
    /// stale content first so the pane shows the spinner, while a
    /// checksum-only refresh keeps the old diff visible until the new one
    /// lands.
    fn ensure_scoped(&mut self) {
        if matches!(self.scope, DiffScope::WorkingTree | DiffScope::History) {
            self.scoped_inflight = None;
            return;
        }
        let Some(env) = self.env.clone() else {
            return;
        };
        let Some(chat) = env.chat.clone() else {
            return;
        };
        let Some(cwd) = self.scoped_cwd() else {
            return;
        };
        let base = match self.scope {
            DiffScope::Branch => match &self.base_ref {
                Some(base) => Some(base.clone()),
                None => return, // branch list still loading
            },
            _ => None,
        };
        let commit_sha = match self.scope {
            DiffScope::Commit => match &self.commit {
                Some(commit) => Some(commit.sha.clone()),
                None => return, // a commit pane without its pin never fetches
            },
            _ => None,
        };
        let target = env.target();
        let context = format!(
            "{}|{}|{}|{}|{}|{}",
            target.as_deref().unwrap_or("local"),
            chat.id,
            cwd,
            self.scope.mode(),
            base.as_deref().unwrap_or(""),
            commit_sha.as_deref().unwrap_or("")
        );
        let watch_sum = self
            .resolved()
            .map(|d| d.checksum.clone())
            .unwrap_or_default();
        let key = format!("{context}|{watch_sum}");
        if self.scoped_for.as_deref() == Some(key.as_str())
            || self.scoped_inflight.as_deref() == Some(key.as_str())
        {
            return;
        }
        if self
            .scoped_for
            .as_deref()
            .is_none_or(|prev| !prev.starts_with(&format!("{context}|")))
        {
            self.scoped = None;
            self.scoped_error = None;
        }
        let Some(engine) = env.engine.clone() else {
            self.scoped = None;
            self.scoped_error = Some("Diff needs a running engine.".to_string());
            self.scoped_for = Some(key);
            return;
        };
        let mode = self.scope.mode().to_string();
        self.scoped_inflight = Some(key.clone());
        let tx = self.tx.clone();
        let wake = env.bridge.clone();
        env.bridge.spawn(async move {
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd));
            params.insert("mode".into(), serde_json::Value::String(mode));
            params.insert("chatId".into(), serde_json::Value::String(chat.id));
            if let Some(base) = base {
                params.insert("baseRef".into(), serde_json::Value::String(base));
            }
            if let Some(sha) = commit_sha {
                params.insert("commitSha".into(), serde_json::Value::String(sha));
            }
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = match tokio::time::timeout(
                Duration::from_secs(8),
                engine.client.call(
                    methods::GET_CHECKOUT_DIFF,
                    serde_json::Value::Object(params),
                ),
            )
            .await
            {
                Ok(Ok(value)) => serde_json::from_value::<CheckoutDiff>(value)
                    .map_err(|err| format!("Diff response: {err}")),
                Ok(Err(RpcError::UnknownMethod(_))) => {
                    Err("unknown method: scoped diffs are unsupported by this engine".into())
                }
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("Diff request timed out — is the engine responding?".into()),
            };
            let _ = tx.send(ChangesEvent::Scoped { key, result });
            wake.wake();
        });
    }

    /// Reconcile parsed content with the currently-active diff.
    fn sync_parse(&mut self) -> bool {
        let Some(diff) = self.active_diff() else {
            if self.parsed.take().is_some() {
                self.rows.clear();
                self.row_ranges.clear();
                self.folds.clear();
                self.highlights.clear();
                self.parse_pending = None;
                self.row_version = self.row_version.wrapping_add(1);
                return true;
            }
            return false;
        };
        let key = self.parse_key(diff);
        if self.parsed.as_ref().is_some_and(|parsed| parsed.key == key)
            || self.parse_pending.as_deref() == Some(key.as_str())
        {
            return false;
        }
        let Some(env) = self.env.clone() else {
            return false;
        };
        // Parse off the UI thread — patches run to megabytes.
        let patch = diff.patch.clone();
        let tx = self.tx.clone();
        let wake = env.bridge.clone();
        self.parse_pending = Some(key.clone());
        env.bridge.spawn(async move {
            let files = parse_patch(&patch);
            let _ = tx.send(ChangesEvent::Parsed { key, files });
            wake.wake();
        });
        false
    }

    // ---- history --------------------------------------------------------

    fn history_key(&self) -> Option<String> {
        let env = self.env.as_ref()?;
        let cwd = self.scoped_cwd()?;
        Some(format!(
            "{}:{}",
            env.target().as_deref().unwrap_or("local"),
            cwd
        ))
    }

    fn ensure_history(&mut self) {
        if self.history_loading {
            return;
        }
        let Some(key) = self.history_key() else {
            return;
        };
        let stale = self.history_for.as_deref() != Some(key.as_str());
        if stale {
            self.history_for = Some(key);
            self.history.clear();
            self.history_cursor = None;
            self.history_error = None;
            self.history_lead_count = None;
            self.history_total = None;
            self.head_sha = None;
            self.history_version = self.history_version.wrapping_add(1);
            self.fetch_history(0, true);
        }
    }

    fn fetch_history(&mut self, cursor: usize, reset: bool) {
        self.history_loading = true;
        self.history_error = None;
        let Some(env) = self.env.clone() else {
            self.history_loading = false;
            self.history_error = Some("History needs an engine connection.".to_string());
            return;
        };
        let Some(cwd) = self.scoped_cwd() else {
            self.history_loading = false;
            self.history_error = Some("This session has no working directory.".to_string());
            return;
        };
        let Some(engine) = env.engine.clone() else {
            self.history_loading = false;
            self.history_error = Some("History needs a running engine.".to_string());
            return;
        };
        let Some(key) = self.history_key() else {
            self.history_loading = false;
            return;
        };
        let target = env.target();
        let tx = self.tx.clone();
        let wake = env.bridge.clone();
        env.bridge.spawn(async move {
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd));
            params.insert("cursor".into(), serde_json::json!(cursor));
            params.insert(
                "limit".into(),
                serde_json::json!(crate::history::HISTORY_PAGE_SIZE),
            );
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = match tokio::time::timeout(
                Duration::from_secs(8),
                engine
                    .client
                    .call(methods::LIST_GIT_HISTORY, serde_json::Value::Object(params)),
            )
            .await
            {
                Ok(Ok(value)) => serde_json::from_value::<GitHistoryPage>(value)
                    .map_err(|err| format!("History response: {err}")),
                Ok(Err(RpcError::UnknownMethod(_))) => {
                    Err("This engine build does not support git history — update it.".to_string())
                }
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("History request timed out — is the engine responding?".to_string()),
            };
            let _ = tx.send(ChangesEvent::History { key, reset, result });
            wake.wake();
        });
    }

    // ---- highlights -----------------------------------------------------

    /// Request excerpt + full-document highlights for the files the viewport
    /// can see, one task per file. Already-parsed slots are left alone.
    fn request_highlights(&mut self) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let Some(env) = self.env.clone() else {
            return;
        };
        let files = parsed.files.clone();
        let parsed_key = parsed.key.clone();
        let range = if self.visible_rows.is_empty() {
            0..self.rows.len().min(40)
        } else {
            self.visible_rows.clone()
        };
        let mut wanted: Vec<usize> = self
            .rows
            .get(range)
            .into_iter()
            .flatten()
            .map(|row| match row {
                DiffRow::FileHeader { file }
                | DiffRow::Notice { file, .. }
                | DiffRow::HunkHeader { file, .. }
                | DiffRow::Line { file, .. }
                | DiffRow::SplitLine { file, .. }
                | DiffRow::CommentCard { file, .. }
                | DiffRow::CommentDraft { file }
                | DiffRow::BodyPad { file }
                | DiffRow::FoldingBody { file } => *file as usize,
            })
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        for file_ix in wanted {
            let Some(file) = files.get(file_ix) else {
                continue;
            };
            if self
                .folds
                .get(&file.path)
                .is_some_and(|fold| fold.collapsed)
            {
                continue;
            }
            let Some(language) = hearth_syntax::language_for_path(&file.path) else {
                continue;
            };
            let fingerprint = hash64(&[parsed_key.as_str(), file.path.as_str()]);
            if self
                .highlights
                .get(&file.path)
                .is_some_and(|slot| slot.fingerprint == fingerprint)
            {
                continue;
            }
            if !hearth_syntax::supports_language(language) {
                self.highlights.insert(
                    file.path.clone(),
                    HighlightSlot {
                        fingerprint,
                        state: DiffHighlightState::Plain,
                    },
                );
                continue;
            }
            self.highlights.insert(
                file.path.clone(),
                HighlightSlot {
                    fingerprint,
                    state: DiffHighlightState::Pending,
                },
            );
            let file = file.clone();
            let path = file.path.clone();
            let active = self.active_diff().cloned();
            let engine = env.engine.clone();
            let target = env.target();
            let mode = self.scope.mode().to_string();
            let base_ref = self.base_ref.clone();
            let chat_id = env.chat.as_ref().map(|chat| chat.id.clone());
            let commit_sha = (self.scope == DiffScope::Commit)
                .then(|| self.commit.as_ref().map(|commit| commit.sha.clone()))
                .flatten();
            let tx = self.tx.clone();
            let bridge = env.bridge.clone();
            let wake = bridge.clone();
            bridge.spawn(async move {
                let excerpt = excerpt_highlights(&file, language).map(Arc::new);
                let _ = tx.send(ChangesEvent::Highlights {
                    path: path.clone(),
                    fingerprint,
                    full: false,
                    result: excerpt,
                });
                wake.wake();
                let Some(engine) = engine else {
                    return;
                };
                let Some(diff) = active else {
                    return;
                };
                let request = hearth_proto::GetCheckoutFileDiffTextRequest {
                    checkout_id: diff.checkout_id,
                    cwd: diff.cwd,
                    path: path.clone(),
                    mode,
                    base_ref,
                    chat_id,
                    commit_sha,
                    diff_checksum: diff.checksum,
                };
                let mut params = serde_json::to_value(&request)
                    .ok()
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                if let Some(target) = target {
                    params.insert("targetDeviceId".into(), serde_json::Value::String(target));
                }
                let response = engine
                    .client
                    .call(
                        methods::GET_CHECKOUT_FILE_DIFF_TEXT,
                        serde_json::Value::Object(params),
                    )
                    .await
                    .ok()
                    .and_then(|value| {
                        serde_json::from_value::<hearth_proto::CheckoutFileDiffText>(value).ok()
                    });
                let full = response
                    .as_ref()
                    .and_then(|response| full_highlights(&file, language, response))
                    .map(Arc::new);
                let _ = tx.send(ChangesEvent::Highlights {
                    path: path.clone(),
                    fingerprint,
                    full: true,
                    result: full,
                });
                wake.wake();
            });
        }
    }
}

/// Spawn the resubscribing diff watch. A task generation guards against a
/// retargeted watch's late frames; aborting the handle stops the old one.
fn spawn_diff_watch(
    env: &ChangesEnv,
    target: Option<String>,
    generation: u64,
    tx: Sender<ChangesEvent>,
) -> tokio::task::JoinHandle<()> {
    let Some(engine) = env.engine.clone() else {
        return env.bridge.runtime().spawn(async {});
    };
    let bridge = env.bridge.clone();
    env.bridge.runtime().spawn(async move {
        loop {
            let mut params = serde_json::Map::new();
            if let Some(target) = &target {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            match tokio::time::timeout(
                Duration::from_secs(8),
                engine.client.subscribe_checked(
                    methods::WATCH_CHECKOUT_DIFFS,
                    serde_json::Value::Object(params),
                ),
            )
            .await
            {
                Ok(Ok(mut subscription)) => {
                    while let Some(value) = subscription.recv().await {
                        let _ = tx.send(ChangesEvent::Diffs { generation, value });
                        bridge.wake();
                    }
                    let _ = tx.send(ChangesEvent::WatchError {
                        generation,
                        message: "Diff stream interrupted — retrying".to_string(),
                    });
                }
                Ok(Err(RpcError::UnknownMethod(_))) => {
                    let _ = tx.send(ChangesEvent::WatchError {
                        generation,
                        message: "unknown method: live diffs are unsupported by this engine build"
                            .to_string(),
                    });
                    bridge.wake();
                    return;
                }
                Ok(Err(err)) => {
                    let _ = tx.send(ChangesEvent::WatchError {
                        generation,
                        message: format!("Diff watch unavailable: {err}"),
                    });
                }
                Err(_) => {
                    let _ = tx.send(ChangesEvent::WatchError {
                        generation,
                        message: "Diff watch timed out — is the engine responding?".to_string(),
                    });
                }
            }
            bridge.wake();
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
}

/// Case-insensitive ranked substring filter, the base-ref menu's search.
pub fn filter_indices<S: AsRef<str>>(query: &str, labels: &[S]) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..labels.len()).collect();
    }
    let mut ranked: Vec<(usize, usize)> = labels
        .iter()
        .enumerate()
        .filter_map(|(ix, label)| {
            let label = label.as_ref().to_lowercase();
            label.find(&query).map(|at| (at, ix))
        })
        .collect();
    ranked.sort_by_key(|&(at, ix)| (at, ix));
    ranked.into_iter().map(|(_, ix)| ix).collect()
}

/// One-line hook for `App::poll`:
///
/// ```ignore
/// fn poll(app: &mut App, _now: f64, _tx: &mut Tx) -> bool {
///     crate::changes::poll(app)
/// }
/// ```
pub fn poll(app: &mut App) -> bool {
    let env = ChangesEnv::capture(app);
    let requests = ChangeRequestsEnv::capture(app);
    let mut changed = app.changes.refresh(&env);
    changed |= app.changes.poll();
    // PR badges are sidebar/composer state: keep watching even while the diff
    // pane is closed (its own diff watch is suspended by `refresh`).
    changed |= app.changes.change_requests.refresh(&requests);
    changed |= app.changes.change_requests.poll();
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(checkout: &str, device: &str, cwd: &str, patch: &str) -> CheckoutDiff {
        CheckoutDiff {
            checkout_id: checkout.into(),
            device_id: device.into(),
            cwd: cwd.into(),
            patch: patch.into(),
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            truncated: false,
            checksum: format!("sum-{}", patch.len()),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn target_is_only_a_remote_device() {
        let chat = ChangesChat {
            id: "c".into(),
            device_id: "remote".into(),
            cwd: None,
            branch: None,
            checkout_id: None,
        };
        let env = ChangesEnv {
            bridge: crate::bridge::Bridge::new(),
            engine: None,
            data_dir: None,
            local_device_id: Some("remote".into()),
            chat: Some(chat.clone()),
            comments: Vec::new(),
            comments_key: "c".into(),
            visible: true,
        };
        assert_eq!(env.target(), None);
        let mut remote = env.clone();
        remote.local_device_id = Some("local".into());
        assert_eq!(remote.target().as_deref(), Some("remote"));
        let mut none = env;
        none.chat = None;
        assert_eq!(none.target(), None);
    }

    #[test]
    fn branch_filter_ranks_substring_matches() {
        let branches = ["feature/login", "main", "login-fix", "develop"];
        assert_eq!(filter_indices("", &branches), vec![0, 1, 2, 3]);
        // Earliest match first, then list order: "login-fix" matches at 0,
        // "feature/login" at 8.
        assert_eq!(filter_indices("login", &branches), vec![2, 0]);
        assert_eq!(filter_indices("MAIN", &branches), vec![1]);
        assert!(filter_indices("nope", &branches).is_empty());
    }

    #[test]
    fn parse_keys_fold_scope_and_base() {
        let mut state = ChangesState::default();
        let diff = diff("co", "d", "/w", "p");
        state.scope = DiffScope::WorkingTree;
        let wt = state.parse_key(&diff);
        state.scope = DiffScope::Branch;
        state.base_ref = Some("main".into());
        let branch = state.parse_key(&diff);
        assert_ne!(wt, branch);
        state.base_ref = Some("develop".into());
        assert_ne!(branch, state.parse_key(&diff));
    }

    #[test]
    fn scoped_fetches_are_skipped_for_the_watch_scope() {
        let mut state = ChangesState {
            scope: DiffScope::WorkingTree,
            scoped_inflight: Some("stale".into()),
            ..Default::default()
        };
        // No env: still clears the inflight marker for watch scopes.
        state.ensure_scoped();
        assert!(state.scoped_inflight.is_none());
    }

    #[test]
    fn hidden_refresh_suspends_the_watch_and_keeps_rows() {
        let mut state = ChangesState {
            started: true,
            ..Default::default()
        };
        let env = ChangesEnv {
            bridge: crate::bridge::Bridge::new(),
            engine: None,
            data_dir: None,
            local_device_id: None,
            chat: None,
            comments: Vec::new(),
            comments_key: String::new(),
            visible: false,
        };
        assert!(!state.refresh(&env));
        assert!(!state.started);
        assert!(state.watch_task.is_none());
        assert!(state.watch_target.is_none());
    }

    #[test]
    fn wrap_toggle_bumps_the_row_version_once() {
        let mut state = ChangesState::default();
        state.set_wrap(false);
        let before = state.row_version;
        state.set_wrap(true);
        assert!(state.wrap);
        assert_ne!(state.row_version, before);
        let after = state.row_version;
        state.set_wrap(true);
        assert_eq!(state.row_version, after);
        state.toggle_wrap();
        assert!(!state.wrap);
    }

    fn changes_test_view(state: &App, _cx: &mut rune::Cx<App>) -> rune::El {
        crate::changes::render(&state.changes, state, &state.theme)
    }

    fn chat_row(id: &str, device: &str, cwd: Option<&str>) -> hearth_proto::Chat {
        hearth_proto::Chat {
            id: id.into(),
            device_id: device.into(),
            title: None,
            archived: false,
            cwd: cwd.map(str::to_owned),
            branch: Some("main".into()),
            checkout_id: Some("checkout".into()),
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn diff_pane_lays_out_scrolled_and_wrapped_long_lines() {
        let mut app = App::new(crate::UiConfig::default());
        app.local_device_id = Some("local".into());
        app.chats = vec![chat_row("c1", "local", Some("/repo"))];
        app.selected_chat = Some("c1".into());
        let env = ChangesEnv::capture(&app);
        app.changes.env = Some(env);
        app.changes.scope = DiffScope::WorkingTree;
        app.changes.diffs = vec![diff("checkout", "local", "/repo", PATCH)];
        let files = parse_patch(PATCH);
        app.changes.materialize(ParsedDiff {
            key: app
                .changes
                .current_parse_key()
                .expect("the watch diff resolves"),
            truncated: false,
            additions: 0,
            deletions: 0,
            file_count: files.len(),
            files: Arc::new(files),
        });
        assert!(!app.changes.rows.is_empty());
        app.changes.set_wrap(false);

        let mut rt = rune::Runtime::new(
            app,
            changes_test_view,
            (1000.0, 700.0),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        assert!(rt.find("app/changes/rows").is_some());
        assert!(rt.find("app/changes/fh/0").is_some());

        // Wrap mode: rows measure at their wrapped height instead of 21 px.
        rt.state.changes.toggle_wrap();
        rt.frame(2.0 / 60.0);
        assert!(rt.state.changes.wrap);
        assert!(rt.find("app/changes/rows").is_some());
    }

    #[test]
    fn rank_then_pin_and_unpin_a_commit() {
        let mut state = ChangesState::default();
        let commit = GitHistoryCommit {
            sha: "abcdef1234567890".into(),
            parent_shas: Vec::new(),
            subject: "Fix the thing".into(),
            author_name: "A".into(),
            author_email: "a@example.com".into(),
            authored_at: "2026-01-01T00:00:00Z".into(),
            refs: Vec::new(),
        };
        state.set_scope(DiffScope::History);
        state.pin_commit(commit);
        assert_eq!(state.scope, DiffScope::Commit);
        assert_eq!(state.tab_title(), "Fix the thing");
        state.unpin_commit();
        assert_eq!(state.scope, DiffScope::WorkingTree);
        assert!(state.commit.is_none());
        assert_eq!(state.tab_title(), "Working tree");
    }
}
