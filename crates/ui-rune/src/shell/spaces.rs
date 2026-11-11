//! Spaces: the sidebar's space filter, space management and the add-space folder browser.
//!
//! A space is a synced `(device, folder)` pair. The filter only narrows the sidebar's session
//! list; management lives here too (rename/remove from a row's context menu). The add-space
//! palette is the ⌘K surface: a search bar, a folder browser, a devices/locations rail and a
//! key-cap footer, backed by `ListFolders` / `ListDrives` / `CloneRepo` / `CreateRepo` through
//! `App::bridge`. Results return as [`ShellTask`]s drained by [`super::poll`].

use rune::{
    Cursor, El, Key, ScrollSpec, TextEdit, TextStyle, Tx, col, el, icon, row, text, text_input,
};
use rune_kit::Placement;

use crate::icons;
use crate::state::App;
use crate::theme::Theme;

use super::{
    ShellTask, SpaceMenu, delete_space, land_in_space, open_rename_space, set_space_filter,
    submit_rename_space,
};

/// A remote listing's lifecycle.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Loadable<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Error(String),
}

impl<T> Loadable<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Error(message) => Some(message),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Idle | Self::Loading)
    }
}

/// The space-filter dropdown, `Some` while open.
#[derive(Debug)]
pub struct SpacesMenuState {
    pub search: TextEdit,
    /// Keyboard highlight within the filtered rows.
    pub active: usize,
}

/// Which repo-creating action the add-space footer is offering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    Clone,
    Create,
}

/// The inline clone/create form under the add-space folder list.
#[derive(Debug)]
pub struct RepoTool {
    pub kind: RepoKind,
    pub input: TextEdit,
    pub busy: bool,
}

/// The add-space palette state.
#[derive(Debug)]
pub struct AddSpaceFlow {
    /// The device currently browsed.
    pub device: Option<hearth_proto::Device>,
    pub search: TextEdit,
    pub browser: Loadable<hearth_proto::FolderListing>,
    pub drives: Loadable<Vec<hearth_proto::DriveEntry>>,
    pub browser_path: Option<String>,
    /// The device's home (the path a `None` browse resolved to).
    pub home: Option<String>,
    /// Best-effort git seed for the current path.
    pub browser_repo: bool,
    /// Keyboard highlight within the filtered folder rows.
    pub active: usize,
    pub submit_busy: bool,
    pub error: Option<String>,
    pub tool: Option<RepoTool>,
    /// Fences listing responses after a device/path switch.
    pub generation: u64,
    pub focus_pending: bool,
}

/// One row of the rail's Locations section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationRow {
    Home,
    Drive(usize),
}

// ---------------------------------------------------------------------------
// Pure path / filter helpers
// ---------------------------------------------------------------------------

/// Segment-aware "is `path` at or under `base`" (`/media/a` is not under `/media/ab`); a root
/// base covers everything.
pub fn path_under(path: &str, base: &str) -> bool {
    let base = base.trim_end_matches(['/', '\\']);
    base.is_empty() || path == base || path.starts_with(&format!("{base}/"))
}

/// `(label, full path)` per segment, with a leading root segment omitted.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut full = String::new();
    let absolute = path.starts_with('/') || path.starts_with('\\');
    for segment in path.split(['/', '\\']).filter(|s| !s.is_empty()) {
        // The root (or the prior segment) contributes the separator.
        if absolute || !full.is_empty() {
            full.push('/');
        }
        full.push_str(segment);
        out.push((segment.to_string(), full.clone()));
    }
    out
}

/// The parent of a path, or `None` at a root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind(['/', '\\']) {
        Some(0) => Some("/".to_string()),
        Some(ix) => Some(trimmed[..ix].to_string()),
        None => None,
    }
}

/// Join a folder name onto a listing path.
pub fn child_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else if base.ends_with(['/', '\\']) {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// An absolute or `~`-relative typed query resolved to a browse path.
pub fn typed_path_target(text: &str, home: Option<&str>) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('~') {
        let home = home?;
        let rest = rest.trim_start_matches('/');
        return Some(if rest.is_empty() {
            home.to_string()
        } else {
            child_path(home, rest)
        });
    }
    (text.starts_with('/') || text.contains(":\\")).then(|| text.to_string())
}

/// The folder a name query names: an exact name first, else a unique case-insensitive prefix.
pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    if let Some(ix) = names.iter().position(|name| *name == query) {
        return Some(ix);
    }
    let query_lower = query.to_lowercase();
    let mut matches = names
        .iter()
        .enumerate()
        .filter(|(_, name)| name.to_lowercase().starts_with(&query_lower));
    let (ix, _) = matches.next()?;
    matches.next().is_none().then_some(ix)
}

/// Length of the prefix `query` shares with `name`, or `None` when it is not a prefix.
pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    if query.is_empty() || name.len() < query.len() || !name.is_char_boundary(query.len()) {
        return None;
    }
    name[..query.len()]
        .eq_ignore_ascii_case(query)
        .then_some(query.len())
}

/// Case-insensitive substring filter, prefix matches first. An empty query keeps every row.
pub fn filter_indices(query: &str, names: &[&str]) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..names.len()).collect();
    }
    let mut hits: Vec<(u8, usize)> = names
        .iter()
        .enumerate()
        .filter_map(|(ix, name)| {
            let lower = name.to_lowercase();
            if lower.starts_with(&query) {
                Some((0, ix))
            } else if lower.contains(&query) {
                Some((1, ix))
            } else {
                None
            }
        })
        .collect();
    hits.sort_by_key(|(rank, ix)| (*rank, *ix));
    hits.into_iter().map(|(_, ix)| ix).collect()
}

/// Folder rows of a listing (files are not browsable).
pub fn browser_rows(listing: &hearth_proto::FolderListing) -> Vec<hearth_proto::FolderEntry> {
    listing
        .entries
        .iter()
        .filter(|entry| entry.is_dir)
        .cloned()
        .collect()
}

/// Display status dots (tab dots + session rows).
pub fn status_dot_color(status: hearth_proto::ChatIndicator, theme: &Theme) -> rune::Color {
    use hearth_proto::ChatIndicator as I;
    match status {
        I::Working => theme.busy.alpha(0.55),
        I::AwaitingInput => theme.accent.alpha(0.6),
        I::Errored => theme.danger.alpha(0.65),
        I::Completed => theme.success.alpha(0.9),
        I::Idle => theme.ink(0.14),
    }
}

// ---------------------------------------------------------------------------
// Add-space flow
// ---------------------------------------------------------------------------

/// Open the add-space palette on this device (else the first registered device).
pub fn open_add_space(app: &mut App) {
    let devices = app.devices.clone();
    let local = app.local_device_id.clone();
    let device = devices
        .iter()
        .find(|d| local.as_deref() == Some(d.id.as_str()))
        .or_else(|| devices.first())
        .cloned();
    let mut search = TextEdit::new("");
    search.focused = true;
    app.shell.space_generation += 1;
    let generation = app.shell.space_generation;
    let mut flow = AddSpaceFlow {
        device,
        search,
        browser: Loadable::Idle,
        drives: Loadable::Idle,
        browser_path: None,
        home: None,
        browser_repo: false,
        active: 0,
        submit_busy: false,
        error: None,
        tool: None,
        generation,
        focus_pending: true,
    };
    if flow.device.is_some() {
        flow.browser = Loadable::Loading;
        flow.drives = Loadable::Loading;
    }
    let has_device = flow.device.is_some();
    if has_device && app.engine.is_none() {
        flow.error = Some("Engine not connected".to_string());
        flow.browser = Loadable::Error("Engine not connected".to_string());
        flow.drives = Loadable::Ready(Vec::new());
    }
    app.shell.add_space = Some(flow);
    if has_device && app.engine.is_some() {
        load_space_folders(app, None);
        load_space_drives(app);
    }
}

pub fn close_add_space(app: &mut App) {
    app.shell.add_space = None;
}

fn flow_generation(app: &App) -> Option<u64> {
    app.shell.add_space.as_ref().map(|f| f.generation)
}

/// Devices-rail click: rebrowse the same palette on another device.
pub fn pick_device(app: &mut App, device: hearth_proto::Device) {
    let Some(flow) = app.shell.add_space.as_mut() else {
        return;
    };
    if flow.device.as_ref().is_some_and(|d| d.id == device.id) {
        return;
    }
    flow.generation += 1;
    flow.device = Some(device);
    flow.browser = Loadable::Loading;
    flow.drives = Loadable::Loading;
    flow.browser_path = None;
    flow.home = None;
    flow.browser_repo = false;
    flow.active = 0;
    flow.error = None;
    flow.search.clear();
    load_space_folders(app, None);
    load_space_drives(app);
}

/// Locations-rail click: rebrowse at a drive's mount point (or home).
pub fn goto_location(app: &mut App, path: Option<String>) {
    let Some(flow) = app.shell.add_space.as_mut() else {
        return;
    };
    let already = flow.browser.ready().is_some_and(|l| match &path {
        Some(p) => l.path == *p,
        None => flow.home.as_deref() == Some(l.path.as_str()),
    });
    if already {
        return;
    }
    flow.browser_repo = false;
    flow.search.clear();
    load_space_folders(app, path);
}

/// `ListFolders` on the flow's device (relay-forwarded when remote).
pub fn load_space_folders(app: &mut App, path: Option<String>) {
    let Some(engine) = app.engine.clone() else {
        return;
    };
    let local = app.local_device_id.clone();
    let Some(flow) = app.shell.add_space.as_mut() else {
        return;
    };
    let device_id = flow.device.as_ref().map(|d| d.id.clone());
    let generation = flow.generation;
    let went_home = path.is_none();
    flow.browser_path = path.clone();
    flow.browser = Loadable::Loading;
    flow.active = 0;

    let bridge = app.bridge.clone();
    let tx = app.shell.task_tx.clone();
    app.bridge.spawn(async move {
        let mut params = serde_json::Map::new();
        if let Some(p) = &path {
            params.insert("path".into(), serde_json::Value::String(p.clone()));
        }
        if let (Some(target), local) = (&device_id, &local)
            && local.as_deref() != Some(target.as_str())
        {
            params.insert(
                "targetDeviceId".into(),
                serde_json::Value::String(target.clone()),
            );
        }
        let result = engine
            .client
            .call(
                hearth_rpc::methods::LIST_FOLDERS,
                serde_json::Value::Object(params),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<hearth_proto::FolderListing>(value)
                    .map_err(|err| hearth_rpc::RpcError::Failed(err.to_string()))
            })
            .map_err(|err| err.to_string());
        let _ = tx.send(ShellTask::FolderListing {
            generation,
            path: if went_home { None } else { path.clone() },
            result,
        });
        bridge.wake();
    });
}

/// `ListDrives` on the flow's device. Failures stay silent: the rail just shows Home.
pub fn load_space_drives(app: &mut App) {
    let Some(engine) = app.engine.clone() else {
        return;
    };
    let local = app.local_device_id.clone();
    let Some(flow) = app.shell.add_space.as_mut() else {
        return;
    };
    let device_id = flow.device.as_ref().map(|d| d.id.clone());
    let generation = flow.generation;
    flow.drives = Loadable::Loading;

    let bridge = app.bridge.clone();
    let tx = app.shell.task_tx.clone();
    app.bridge.spawn(async move {
        let mut params = serde_json::Map::new();
        if let (Some(target), local) = (&device_id, &local)
            && local.as_deref() != Some(target.as_str())
        {
            params.insert(
                "targetDeviceId".into(),
                serde_json::Value::String(target.clone()),
            );
        }
        let result = engine
            .client
            .call(
                hearth_rpc::methods::LIST_DRIVES,
                serde_json::Value::Object(params),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<hearth_proto::DriveListing>(value)
                    .map_err(|err| hearth_rpc::RpcError::Failed(err.to_string()))
            })
            .map(|listing| listing.drives)
            .map_err(|err| err.to_string());
        let _ = tx.send(ShellTask::Drives { generation, result });
        bridge.wake();
    });
}

/// The current listing's folder rows filtered by the search query.
pub fn filtered_folders(app: &App) -> Vec<hearth_proto::FolderEntry> {
    let Some(flow) = app.shell.add_space.as_ref() else {
        return Vec::new();
    };
    let Some(listing) = flow.browser.ready() else {
        return Vec::new();
    };
    let dirs = browser_rows(listing);
    let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
    filter_indices(&flow.search.text, &names)
        .into_iter()
        .map(|ix| dirs[ix].clone())
        .collect()
}

/// Descend into the highlighted (filtered) folder; a path-shaped query with no match browses
/// the typed path instead.
pub fn open_active_folder(app: &mut App) {
    let rows = filtered_folders(app);
    if rows.is_empty() {
        if let Some(flow) = app.shell.add_space.as_ref() {
            let text = flow.search.text.clone();
            if let Some(target) = typed_path_target(&text, flow.home.as_deref()) {
                descend(app, target, false);
            }
        }
        return;
    }
    let Some(flow) = app.shell.add_space.as_ref() else {
        return;
    };
    let Some(listing) = flow.browser.ready() else {
        return;
    };
    let Some(entry) = rows.get(flow.active) else {
        return;
    };
    let full = child_path(&listing.path, &entry.name);
    let is_repo = entry.is_repo;
    descend(app, full, is_repo);
}

/// Descend into a specific folder (mouse path); clears the query.
pub fn descend(app: &mut App, full: String, is_repo: bool) {
    if let Some(flow) = app.shell.add_space.as_mut() {
        flow.browser_repo = is_repo;
        flow.search.clear();
    }
    load_space_folders(app, Some(full));
}

/// Go up to the parent folder.
pub fn go_up(app: &mut App) {
    let parent = app
        .shell
        .add_space
        .as_ref()
        .and_then(|f| f.browser.ready())
        .and_then(|l| parent_path(&l.path));
    if let Some(parent) = parent {
        if let Some(flow) = app.shell.add_space.as_mut() {
            flow.browser_repo = false;
        }
        load_space_folders(app, Some(parent));
    }
}

/// Slash-descend: a query ending in `/` that names a folder of the current listing descends.
pub fn slash_descend(app: &mut App) -> bool {
    let target = {
        let Some(flow) = app.shell.add_space.as_ref() else {
            return false;
        };
        let text = flow.search.text.clone();
        if text.ends_with('/') && (text.starts_with('/') || text.starts_with('~')) {
            typed_path_target(&text, flow.home.as_deref()).map(|path| (path, false))
        } else {
            let Some(query) = text.strip_suffix('/') else {
                return false;
            };
            if query.is_empty() || query.contains('/') {
                return false;
            }
            let Some(listing) = flow.browser.ready() else {
                return false;
            };
            let dirs = browser_rows(listing);
            let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
            segment_target(&names, query)
                .map(|ix| (child_path(&listing.path, &dirs[ix].name), dirs[ix].is_repo))
        }
    };
    let Some((full, is_repo)) = target else {
        return false;
    };
    descend(app, full, is_repo);
    true
}

/// The tab-completion target: `(full name, remaining suffix)`.
pub fn completion(app: &App) -> Option<(String, String)> {
    let flow = app.shell.add_space.as_ref()?;
    let query = flow.search.text.clone();
    if query.is_empty() {
        return None;
    }
    let rows = filtered_folders(app);
    let entry = rows
        .get(flow.active)
        .filter(|e| completion_prefix_len(&e.name, &query).is_some())
        .or_else(|| {
            rows.iter()
                .find(|e| completion_prefix_len(&e.name, &query).is_some())
        })?;
    let len = completion_prefix_len(&entry.name, &query)?;
    if len >= entry.name.len() {
        return None;
    }
    Some((entry.name.clone(), entry.name[len..].to_string()))
}

/// Accept the completion: the query becomes the full folder name.
pub fn accept_completion(app: &mut App) {
    let Some((name, _)) = completion(app) else {
        return;
    };
    if let Some(flow) = app.shell.add_space.as_mut() {
        flow.search.replace_text(name);
    }
}

fn step_highlight(active: usize, count: usize, delta: i32) -> usize {
    if count == 0 {
        return 0;
    }
    let next = active as i32 + delta;
    next.rem_euclid(count as i32) as usize
}

/// Palette keys. Returns true when consumed.
pub fn add_space_key(app: &mut App, ev: &rune::KeyEvent, _tx: &mut Tx) -> bool {
    match &ev.key {
        Key::Escape => {
            close_add_space(app);
            true
        }
        Key::Up | Key::Down => {
            let count = filtered_folders(app).len();
            let delta = if matches!(ev.key, Key::Up) { -1 } else { 1 };
            if let Some(flow) = app.shell.add_space.as_mut() {
                flow.active = step_highlight(flow.active, count, delta);
            }
            true
        }
        Key::Right => {
            open_active_folder(app);
            true
        }
        Key::Left => {
            go_up(app);
            true
        }
        Key::Tab => {
            accept_completion(app);
            true
        }
        Key::Enter if ev.mods.cmd || ev.mods.ctrl => {
            submit_add_space(app);
            true
        }
        Key::Enter => {
            open_active_folder(app);
            true
        }
        Key::Backspace => {
            let empty = app
                .shell
                .add_space
                .as_ref()
                .is_some_and(|f| f.search.text.is_empty());
            if empty {
                go_up(app);
            }
            false
        }
        _ => false,
    }
}

/// Create the space for the browser's current folder.
pub fn submit_add_space(app: &mut App) {
    let Some(engine) = app.engine.clone() else {
        return;
    };
    let Some(flow) = app.shell.add_space.as_ref() else {
        return;
    };
    if flow.submit_busy {
        return;
    }
    let Some(device) = flow.device.clone() else {
        return;
    };
    let Some(listing) = flow.browser.ready() else {
        return;
    };
    let path = listing.path.clone();
    let git_detected = flow.browser_repo;
    if let Some(existing) = app
        .spaces
        .iter()
        .find(|s| s.device_id == device.id && s.path == path)
        .map(|s| s.id.clone())
    {
        close_add_space(app);
        land_in_space(app, existing);
        return;
    }
    let generation = flow.generation;
    let Some(flow) = app.shell.add_space.as_mut() else {
        return;
    };
    flow.submit_busy = true;
    flow.error = None;
    let space_id = uuid::Uuid::new_v4().to_string();
    let space = hearth_proto::Space {
        id: space_id.clone(),
        device_id: device.id.clone(),
        path: path.clone(),
        name: None,
        git_detected,
        git_checked_at: None,
        checkout_id: None,
        created_at: chrono::Utc::now(),
    };
    if !app.spaces.iter().any(|existing| existing.id == space.id) {
        app.spaces.push(space);
    }
    let params = serde_json::json!({
        "op": "createSpace",
        "spaceId": space_id,
        "deviceId": device.id,
        "path": path,
        "gitDetected": git_detected,
    });
    let bridge = app.bridge.clone();
    let tx = app.shell.task_tx.clone();
    let submit_id = space_id.clone();
    app.bridge.spawn(async move {
        let result = engine
            .client
            .call(hearth_rpc::methods::MUTATE, params)
            .await
            .map(|_| ())
            .map_err(|err| err.to_string());
        let _ = tx.send(ShellTask::SpaceCreated {
            generation,
            space_id: submit_id,
            result,
        });
        bridge.wake();
    });
}

/// Open the inline clone/create form.
pub fn open_repo_tool(app: &mut App, kind: RepoKind) {
    let Some(flow) = app.shell.add_space.as_mut() else {
        return;
    };
    let mut input = TextEdit::new("");
    input.focused = true;
    flow.search.focused = false;
    flow.tool = Some(RepoTool {
        kind,
        input,
        busy: false,
    });
}

/// Submit the inline clone/create form (`CloneRepo` / `CreateRepo`), then browse the result.
pub fn submit_repo_tool(app: &mut App) {
    let Some(engine) = app.engine.clone() else {
        return;
    };
    let Some(flow) = app.shell.add_space.as_ref() else {
        return;
    };
    let Some(tool) = flow.tool.as_ref() else {
        return;
    };
    if tool.busy {
        return;
    }
    let value = tool.input.text.trim().to_string();
    if value.is_empty() {
        return;
    }
    let generation = flow.generation;
    let kind = tool.kind;
    if let Some(flow) = app.shell.add_space.as_mut()
        && let Some(tool) = flow.tool.as_mut()
    {
        tool.busy = true;
    }
    let bridge = app.bridge.clone();
    let tx = app.shell.task_tx.clone();
    app.bridge.spawn(async move {
        let (method, params) = match kind {
            RepoKind::Clone => (
                hearth_rpc::methods::CLONE_REPO,
                serde_json::json!({ "url": value }),
            ),
            RepoKind::Create => (
                hearth_rpc::methods::CREATE_REPO,
                serde_json::json!({ "name": value }),
            ),
        };
        let result = engine
            .client
            .call(method, params)
            .await
            .and_then(|value| {
                value
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        hearth_rpc::RpcError::Failed("repo reply had no path".to_string())
                    })
            })
            .map_err(|err| err.to_string());
        let _ = tx.send(ShellTask::RepoReady { generation, result });
        bridge.wake();
    });
}

/// Apply a background shell task. Returns true when visible state changed.
pub fn apply_task(app: &mut App, task: ShellTask) -> bool {
    match task {
        ShellTask::FolderListing {
            generation,
            path,
            result,
        } => {
            let Some(flow) = app.shell.add_space.as_mut() else {
                return false;
            };
            if flow.generation != generation {
                return false;
            }
            flow.browser = match result {
                Ok(listing) => {
                    if path.is_none() {
                        flow.home = Some(listing.path.clone());
                    }
                    Loadable::Ready(listing)
                }
                Err(message) => Loadable::Error(message),
            };
            true
        }
        ShellTask::Drives { generation, result } => {
            let Some(flow) = app.shell.add_space.as_mut() else {
                return false;
            };
            if flow.generation != generation {
                return false;
            }
            flow.drives = match result {
                Ok(drives) => Loadable::Ready(drives),
                // Best effort: no rail section beyond Home.
                Err(_) => Loadable::Ready(Vec::new()),
            };
            true
        }
        ShellTask::SpaceCreated {
            generation,
            space_id,
            result,
        } => {
            if flow_generation(app) != Some(generation) {
                return false;
            }
            match result {
                Ok(()) => {
                    close_add_space(app);
                    land_in_space(app, space_id);
                }
                Err(message) => {
                    app.spaces.retain(|space| space.id != space_id);
                    if let Some(flow) = app.shell.add_space.as_mut() {
                        flow.submit_busy = false;
                        flow.error = Some(message);
                    }
                }
            }
            true
        }
        ShellTask::RepoReady { generation, result } => {
            if flow_generation(app) != Some(generation) {
                return false;
            }
            match result {
                Ok(path) => {
                    if let Some(flow) = app.shell.add_space.as_mut() {
                        flow.tool = None;
                        flow.search.focused = true;
                        flow.browser_repo = true;
                        flow.browser = Loadable::Loading;
                    }
                    load_space_folders(app, Some(path));
                }
                Err(message) => {
                    if let Some(flow) = app.shell.add_space.as_mut()
                        && let Some(tool) = flow.tool.as_mut()
                    {
                        tool.busy = false;
                    }
                    if let Some(flow) = app.shell.add_space.as_mut() {
                        flow.error = Some(message);
                    }
                }
            }
            true
        }
        ShellTask::Notice(message) => {
            app.shell.sidebar_notice = Some(message);
            true
        }
        // Update payloads are consumed by `super::poll` before they reach the
        // add-space task router.
        ShellTask::UpdateStaged(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Space filter UI
// ---------------------------------------------------------------------------

/// The filter menu's rows: All, then matching spaces, then New project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpacesMenuRow {
    All,
    Space(String),
    AddSpace,
}

/// One rendered spaces-menu row: `(kind, label, @ device tag + offline, selected)`.
type SpacesMenuEntry = (SpacesMenuRow, String, Option<(String, bool)>, bool);

pub fn spaces_menu_rows(app: &App) -> Vec<SpacesMenuRow> {
    let query = app
        .shell
        .spaces_menu
        .as_ref()
        .map(|m| m.search.text.clone())
        .unwrap_or_default();
    let spaces = app.spaces_sorted();
    let names: Vec<String> = spaces
        .iter()
        .map(|s| s.display_name().to_string())
        .collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut rows = Vec::new();
    if query.trim().is_empty() {
        rows.push(SpacesMenuRow::All);
    }
    rows.extend(
        filter_indices(&query, &name_refs)
            .into_iter()
            .map(|ix| SpacesMenuRow::Space(spaces[ix].id.clone())),
    );
    rows.push(SpacesMenuRow::AddSpace);
    rows
}

pub fn open_spaces_menu(app: &mut App) {
    let mut search = TextEdit::new("");
    search.focused = true;
    app.shell.spaces_menu = Some(SpacesMenuState { search, active: 0 });
    let current = app.settings.space_filter.clone();
    let rows = spaces_menu_rows(app);
    let start = match &current {
        None => 0,
        Some(id) => rows
            .iter()
            .position(|row| matches!(row, SpacesMenuRow::Space(s) if s == id))
            .unwrap_or(0),
    };
    if let Some(menu) = app.shell.spaces_menu.as_mut() {
        menu.active = start;
    }
    app.shell.user_menu_open = false;
}

pub fn close_spaces_menu(app: &mut App) {
    app.shell.spaces_menu = None;
}

pub fn activate_spaces_menu_row(app: &mut App, row: SpacesMenuRow) {
    match row {
        SpacesMenuRow::All => set_space_filter(app, None),
        SpacesMenuRow::Space(id) => set_space_filter(app, Some(id)),
        SpacesMenuRow::AddSpace => {
            close_spaces_menu(app);
            open_add_space(app);
        }
    }
}

/// Dropdown keys (bubbling from the focused search input).
pub fn spaces_menu_key(app: &mut App, ev: &rune::KeyEvent, _tx: &mut Tx) -> bool {
    if app.shell.spaces_menu.is_none() {
        return false;
    }
    match &ev.key {
        Key::Escape => {
            close_spaces_menu(app);
            true
        }
        Key::Up | Key::Down => {
            let count = spaces_menu_rows(app).len();
            let delta = if matches!(ev.key, Key::Up) { -1 } else { 1 };
            if let Some(menu) = app.shell.spaces_menu.as_mut() {
                menu.active = step_highlight(menu.active, count, delta);
            }
            true
        }
        Key::Enter => {
            let row = {
                let active = app.shell.spaces_menu.as_ref().map_or(0, |m| m.active);
                spaces_menu_rows(app).get(active).cloned()
            };
            if let Some(row) = row {
                activate_spaces_menu_row(app, row);
            }
            true
        }
        _ => false,
    }
}

fn small_key_chip(theme: &Theme, label: &str) -> El {
    el().h(22.0)
        .px(6.0)
        .radius(5.0)
        .center()
        .fill(theme.ink(0.05))
        .child(
            text(label)
                .font(TextStyle::new(11.0, 500.0).mono())
                .color(theme.text_muted.alpha(0.7)),
        )
}

fn menu_row_shell(theme: &Theme, selected: bool) -> El {
    el().h(30.0)
        .w_full()
        .px(8.0)
        .radius(6.0)
        .cursor(Cursor::Pointer)
        .items_center()
        .hover(theme.element_hover)
        .fill(if selected {
            crate::theme::glass_selected_bg(theme.appearance)
        } else {
            theme.ink(0.0)
        })
}

/// The sidebar's space-filter row, with its dropdown floating beneath while open.
pub fn render_filter(app: &App, theme: &Theme, layer: &mut Vec<El>) -> El {
    let filter = app.settings.space_filter.clone();
    let (label, device_tag) = match filter.as_deref().and_then(|id| app.space_row(id)) {
        Some(space) => {
            let offline = !app.device_online(&space.device_id, chrono::Utc::now());
            let device = app
                .device_name(&space.device_id)
                .unwrap_or("Unknown device");
            (
                space.display_name().to_string(),
                Some((format!("@ {device}"), offline)),
            )
        }
        None => ("All projects".to_string(), None),
    };
    let left = if app.settings.sidebar_collapsed {
        0.0
    } else {
        Theme::TITLEBAR_HEIGHT
    };
    let _ = left;
    let trigger = row()
        .key("spaces-filter")
        .grow(1.0)
        .min_w(0.0)
        .h(29.0)
        .gap(Theme::SPACE_SM)
        .px(Theme::SPACE_SM)
        .radius(8.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .hover(theme.glass_hover())
        .fill(if app.shell.spaces_menu.is_some() {
            theme.glass_hover()
        } else {
            theme.ink(0.0)
        })
        .on_press(|app: &mut App| {
            if app.shell.spaces_menu.is_some() {
                close_spaces_menu(app);
            } else {
                open_spaces_menu(app);
            }
        })
        .child(icon(&icons::FOLDER, 16.0).color(theme.text_muted))
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .gap(6.0)
                .center_y()
                .child(
                    text(label)
                        .font(TextStyle::new(13.0, 500.0))
                        .color(theme.text.alpha(0.8))
                        .truncate(),
                )
                .children(device_tag.map(|(tag, offline)| {
                    row()
                        .gap(4.0)
                        .center_y()
                        .child(
                            text(tag)
                                .font(TextStyle::new(10.0, 400.0))
                                .color(theme.text_muted.alpha(0.45)),
                        )
                        .child(
                            icon(&icons::WIFI_OFF, 12.0)
                                .color(theme.warning.alpha(0.8))
                                .opacity(if offline { 1.0 } else { 0.0 }),
                        )
                })),
        )
        .child(icon(&icons::ALT_ARROW_DOWN, 14.0).color(theme.text_muted.alpha(0.6)));

    let open = app.shell.spaces_menu.is_some();
    let mut menu_rows: Vec<SpacesMenuEntry> = Vec::new();
    if let Some(menu) = app.shell.spaces_menu.as_ref() {
        for row in spaces_menu_rows(app) {
            let (label, tag, _offline, selected) = match &row {
                SpacesMenuRow::All => ("All projects".to_string(), None, false, filter.is_none()),
                SpacesMenuRow::Space(id) => match app.space_row(id) {
                    Some(space) => {
                        let offline = !app.device_online(&space.device_id, chrono::Utc::now());
                        let device = app
                            .device_name(&space.device_id)
                            .unwrap_or("Unknown device");
                        (
                            space.display_name().to_string(),
                            Some((format!("@ {device}"), offline)),
                            offline,
                            filter.as_deref() == Some(id.as_str()),
                        )
                    }
                    None => ("?".to_string(), None, false, false),
                },
                SpacesMenuRow::AddSpace => ("New project…".to_string(), None, false, false),
            };
            menu_rows.push((row, label, tag, selected || menu.active == menu_rows.len()));
        }
    }

    let body = if let Some(menu_state) = app.shell.spaces_menu.as_ref() {
        col()
            .key("spaces-menu-body")
            .w_full()
            .gap(6.0)
            .child(
                row()
                    .w_full()
                    .h(32.0)
                    .px(8.0)
                    .radius(6.0)
                    .center_y()
                    .fill(theme.ink(0.05))
                    .child(
                        text_input(&menu_state.search, TextStyle::new(13.0, 400.0))
                            .grow(1.0)
                            .placeholder("Search projects…", theme.text_faint)
                            .caret(theme.caret, theme.selection)
                            .on_edit(|app: &mut App| {
                                &mut app
                                    .shell
                                    .spaces_menu
                                    .as_mut()
                                    .expect("spaces menu open")
                                    .search
                            })
                            .on_input_key(|app: &mut App, ev, tx| spaces_menu_key(app, ev, tx)),
                    ),
            )
            .child(
                col()
                    .w_full()
                    .gap(2.0)
                    .max_h(224.0)
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .scroll_edge_fade(12.0)
                    .children(menu_rows.into_iter().enumerate().map(
                        |(ix, (row, label, tag, selected))| {
                            let leading = if matches!(row, SpacesMenuRow::AddSpace) {
                                &icons::PLUS
                            } else {
                                &icons::FOLDER
                            };
                            let space_id = match &row {
                                SpacesMenuRow::Space(id) => Some(id.clone()),
                                _ => None,
                            };
                            let activate = row.clone();
                            let mut menu = menu_row_shell(theme, selected)
                                .key(format!("spaces-menu-row-{ix}"))
                                .gap(8.0)
                                .px(8.0)
                                .on_press(move |app: &mut App| {
                                    activate_spaces_menu_row(app, activate.clone());
                                })
                                .child(icon(leading, 15.0).color(theme.text_muted.alpha(0.8)))
                                .child(
                                    text(label)
                                        .font(TextStyle::new(13.0, 400.0))
                                        .color(theme.text)
                                        .truncate()
                                        .grow(1.0),
                                );
                            if let Some((tag, offline)) = tag {
                                menu = menu.child(
                                    text(tag)
                                        .font(TextStyle::new(10.0, 400.0))
                                        .color(theme.text_muted.alpha(0.45)),
                                );
                                if offline {
                                    menu = menu.child(
                                        icon(&icons::WIFI_OFF, 12.0)
                                            .color(theme.warning.alpha(0.8)),
                                    );
                                }
                            }
                            if let Some(space_id) = space_id {
                                menu = menu.on_secondary_press(
                                    move |app: &mut App, press: rune::PointerPress, _| {
                                        app.shell.space_menu = Some(SpaceMenu {
                                            space_id: space_id.clone(),
                                            x: press.x,
                                            y: press.y,
                                        });
                                    },
                                );
                            }
                            menu
                        },
                    )),
            )
    } else {
        col()
    };

    let popover = rune_kit::Popover::new("spaces-menu")
        .trigger(trigger)
        .open(open)
        .padding(6.0)
        .placement(Placement::BottomStart)
        .gutter(4.0)
        .on_close(|app: &mut App| close_spaces_menu(app))
        .child(body);
    popover.build(&super::shell_kit(theme), layer)
}

// ---------------------------------------------------------------------------
// Add-space palette UI
// ---------------------------------------------------------------------------

fn key_hint(theme: &Theme, glyph: Option<&'static rune::Icon>, label: &str) -> El {
    row()
        .gap(3.0)
        .center_y()
        .children(glyph.map(|g| icon(g, 11.0).color(theme.text_muted.alpha(0.7))))
        .child(
            text(label)
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_muted.alpha(0.7)),
        )
}

/// The add-space palette (a command-K surface). Returns the overlay layer entries.
pub fn render_add_space(app: &App, theme: &Theme, layer: &mut Vec<El>) {
    let Some(flow) = app.shell.add_space.as_ref() else {
        return;
    };
    let rows = filtered_folders(app);
    let listing = flow.browser.ready().cloned();
    let load_error = flow.browser.error().map(str::to_string);
    let loading = flow.browser.is_loading();
    let devices = app.devices.clone();
    let drives = flow.drives.ready().cloned().unwrap_or_default();
    let device = flow.device.clone();
    let query_empty = flow.search.text.trim().is_empty();
    let home = flow.home.clone();
    let card_radius = 14.0;
    let hairline = theme.hairline(0.08);
    let inch = 1.0 / app.scale.get().max(1.0);

    // ---- breadcrumbs -----------------------------------------------------
    let crumbs: El = match &listing {
        Some(listing) => {
            let segments = breadcrumbs(&listing.path);
            let last = segments.len().saturating_sub(1);
            let at_home = home.as_deref() == Some(listing.path.as_str());
            let mut crumbs = row()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(0.0)
                .px(13.0)
                .pt(10.0)
                .pb(2.0);
            let device_name = device
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "This device".to_string());
            crumbs = crumbs.child(
                text(device_name)
                    .font(TextStyle::new(11.0, 400.0).mono())
                    .color(if at_home {
                        theme.text.alpha(0.85)
                    } else {
                        theme.text_muted.alpha(0.55)
                    })
                    .px(3.0)
                    .on_press(|app: &mut App| {
                        if let Some(flow) = app.shell.add_space.as_mut() {
                            flow.browser_repo = false;
                        }
                        load_space_folders(app, None);
                    }),
            );
            let folded = 1 + match home.as_deref() {
                Some(h) if listing.path == h || listing.path.starts_with(&format!("{h}/")) => {
                    h.split('/').filter(|s| !s.is_empty()).count()
                }
                _ => 0,
            };
            for (ix, (label, full)) in segments.into_iter().enumerate().skip(folded) {
                let is_last = ix == last;
                crumbs = crumbs
                    .child(
                        text("/")
                            .font(TextStyle::new(11.0, 400.0).mono())
                            .color(theme.text_faint.alpha(0.7)),
                    )
                    .child(
                        text(label)
                            .font(TextStyle::new(11.0, 400.0).mono())
                            .color(if is_last {
                                theme.text.alpha(0.85)
                            } else {
                                theme.text_muted.alpha(0.55)
                            })
                            .px(3.0)
                            .on_press(move |app: &mut App| {
                                if let Some(flow) = app.shell.add_space.as_mut() {
                                    flow.browser_repo = false;
                                }
                                load_space_folders(app, Some(full.clone()));
                            }),
                    );
            }
            crumbs
        }
        None => col().pt(6.0),
    };

    // ---- folder list -----------------------------------------------------
    let base_path = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
    let list: El = if loading {
        skeleton_rows(theme, 6)
    } else if let Some(message) = load_error {
        let message = if message.contains("folder") {
            message
        } else {
            device
                .as_ref()
                .map(|d| format!("{} didn't respond — is it online?", d.name))
                .unwrap_or(message)
        };
        col()
            .w_full()
            .px(14.0)
            .py(10.0)
            .gap(8.0)
            .child(
                text(message)
                    .font(TextStyle::new(12.5, 400.0))
                    .color(theme.danger)
                    .wrap(),
            )
            .child(
                el().px(Theme::SPACE_SM)
                    .py(3.0)
                    .radius(Theme::CONTROL_RADIUS)
                    .border(1.0, theme.border)
                    .cursor(Cursor::Pointer)
                    .hover(theme.element_hover)
                    .on_press(|app: &mut App| {
                        let path = app
                            .shell
                            .add_space
                            .as_ref()
                            .and_then(|f| f.browser_path.clone());
                        load_space_folders(app, path);
                    })
                    .child(
                        text("Retry")
                            .font(TextStyle::new(12.0, 400.0))
                            .color(theme.text),
                    ),
            )
    } else if rows.is_empty() {
        col().px(14.0).py(16.0).child(
            text(if query_empty {
                "No folders here"
            } else {
                "No folders match"
            })
            .font(TextStyle::new(12.5, 400.0))
            .color(theme.text_faint),
        )
    } else {
        col().w_full().grow(1.0).min_h(0.0).py(6.0).child(
            col()
                .key("add-space-folders")
                .w_full()
                .h_full()
                .px(8.0)
                .gap(2.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_edge_fade(12.0)
                .children(rows.into_iter().enumerate().map(|(ix, entry)| {
                    let name = entry.name.clone();
                    let full = child_path(&base_path, &name);
                    let is_repo = entry.is_repo;
                    let mut row = menu_row_shell(theme, false)
                        .key(format!("add-space-folder-{ix}"))
                        .h(30.0)
                        .gap(8.0)
                        .on_press(move |app: &mut App| {
                            descend(app, full.clone(), is_repo);
                        })
                        .child(icon(&icons::FOLDER, 15.0).color(theme.text_muted.alpha(0.8)))
                        .child(
                            text(name)
                                .font(TextStyle::new(13.0, 400.0))
                                .color(theme.text)
                                .truncate()
                                .grow(1.0),
                        );
                    if is_repo {
                        row = row.child(
                            icon(&icons::GIT_BRANCH, 13.0).color(theme.text_muted.alpha(0.5)),
                        );
                    }
                    row
                })),
        )
    };

    // ---- device + location rail -----------------------------------------
    let active_location: Option<LocationRow> = listing.as_ref().and_then(|l| {
        let mut best = home
            .as_deref()
            .filter(|h| path_under(&l.path, h))
            .map(|h| (h.trim_end_matches('/').len() + 1, LocationRow::Home));
        for (ix, drive) in drives.iter().enumerate() {
            if !path_under(&l.path, &drive.path) {
                continue;
            }
            let len = drive.path.trim_end_matches('/').len();
            if best.is_none_or(|(b, _)| len > b) {
                best = Some((len, LocationRow::Drive(ix)));
            }
        }
        best.map(|(_, row)| row)
    });
    let mut rail = col()
        .key("add-space-rail")
        .w(196.0)
        .h_full()
        .border(1.0, hairline)
        .px(8.0)
        .py(8.0)
        .gap(2.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            text("Devices")
                .font(TextStyle::new(11.0, 500.0))
                .color(theme.text_muted.alpha(0.6))
                .px(8.0)
                .pt(2.0)
                .pb(4.0),
        );
    for (ix, dev) in devices.iter().enumerate() {
        let is_active = device.as_ref().is_some_and(|d| d.id == dev.id);
        let online = app.device_online(&dev.id, chrono::Utc::now());
        let platform_icon = match dev.platform.as_str() {
            "macos" | "darwin" => &icons::LAPTOP,
            "web" => &icons::GLOBAL,
            "ios" | "android" => &icons::SMARTPHONE,
            _ => &icons::MONITOR,
        };
        let pick = dev.clone();
        rail = rail.child(
            row()
                .key(format!("add-space-device-{ix}"))
                .h(28.0)
                .px(8.0)
                .radius(8.0)
                .gap(8.0)
                .center_y()
                .cursor(Cursor::Pointer)
                .fill(if is_active {
                    crate::theme::card_selected_bg(theme.appearance)
                } else {
                    theme.ink(0.0)
                })
                .hover(theme.element_hover)
                .on_press(move |app: &mut App| pick_device(app, pick.clone()))
                .child(icon(platform_icon, 14.0).color(theme.text_muted.alpha(0.8)))
                .child(
                    text(dev.name.clone())
                        .font(TextStyle::new(12.5, 400.0))
                        .color(if is_active {
                            theme.text
                        } else {
                            theme.text_muted.alpha(0.7)
                        })
                        .truncate()
                        .grow(1.0),
                )
                .child(el().size(5.0, 5.0).radius(2.5).fill(if online {
                    theme.success.alpha(0.9)
                } else {
                    theme.ink(0.22)
                })),
        );
    }
    if device.is_some() {
        rail = rail
            .child(el().h(inch).mx(2.0).my(6.0).fill(hairline))
            .child(
                text("Locations")
                    .font(TextStyle::new(11.0, 500.0))
                    .color(theme.text_muted.alpha(0.6))
                    .px(8.0)
                    .pb(4.0),
            )
            .child(location_row(
                theme,
                "Home",
                &icons::HOME,
                None,
                active_location == Some(LocationRow::Home),
            ))
            .children(drives.iter().enumerate().map(|(ix, drive)| {
                location_row(
                    theme,
                    &drive.name,
                    &icons::HARD_DRIVE,
                    Some(drive.path.clone()),
                    active_location == Some(LocationRow::Drive(ix)),
                )
            }));
    }
    let device_name = device
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "This device".to_string());
    rail = rail
        .child(el().h(inch).mx(2.0).my(6.0).fill(hairline))
        .child(
            row()
                .gap(6.0)
                .items_start()
                .px(8.0)
                .child(icon(&icons::INFO_CIRCLE, 12.0).color(theme.text_muted.alpha(0.5)))
                .child(
                    text(format!("Showing folders from {device_name} only"))
                        .font(TextStyle::new(11.0, 400.0).lh(15.0))
                        .color(theme.text_muted.alpha(0.5))
                        .wrap()
                        .w(168.0),
                ),
        );

    // ---- inline clone / create ------------------------------------------
    let tool_bar: Option<El> = flow.tool.as_ref().map(|tool| {
        let kind = tool.kind;
        let placeholder = match kind {
            RepoKind::Clone => "Repository URL…",
            RepoKind::Create => "New repository name…",
        };
        let title = match kind {
            RepoKind::Clone => "Clone a repository",
            RepoKind::Create => "Create a repository",
        };
        row()
            .w_full()
            .gap(8.0)
            .center_y()
            .px(12.0)
            .py(6.0)
            .fill(theme.band)
            .border(1.0, hairline)
            .child(
                text(title)
                    .font(TextStyle::new(11.0, 500.0))
                    .color(theme.text_muted)
                    .w(130.0),
            )
            .child(
                text_input(&tool.input, TextStyle::new(12.0, 400.0))
                    .grow(1.0)
                    .h(26.0)
                    .placeholder(placeholder, theme.text_faint)
                    .caret(theme.caret, theme.selection)
                    .on_edit(|app: &mut App| {
                        &mut app
                            .shell
                            .add_space
                            .as_mut()
                            .and_then(|f| f.tool.as_mut())
                            .expect("repo tool open")
                            .input
                    })
                    .on_input_key(|app: &mut App, ev, _| match &ev.key {
                        Key::Escape => {
                            if let Some(flow) = app.shell.add_space.as_mut() {
                                flow.tool = None;
                                flow.search.focused = true;
                            }
                            true
                        }
                        Key::Enter => {
                            submit_repo_tool(app);
                            true
                        }
                        _ => false,
                    }),
            )
            .child(
                el().px(10.0)
                    .py(4.0)
                    .radius(5.0)
                    .fill(theme.solid)
                    .cursor(Cursor::Pointer)
                    .on_press(|app: &mut App| submit_repo_tool(app))
                    .child(
                        text(if tool.busy { "Working…" } else { "Add" })
                            .font(TextStyle::new(12.0, 500.0))
                            .color(theme.on_solid),
                    ),
            )
            .child(
                el().px(8.0)
                    .py(4.0)
                    .radius(5.0)
                    .hover(theme.element_hover)
                    .cursor(Cursor::Pointer)
                    .on_press(|app: &mut App| {
                        if let Some(flow) = app.shell.add_space.as_mut() {
                            flow.tool = None;
                            flow.search.focused = true;
                        }
                    })
                    .child(
                        text("Cancel")
                            .font(TextStyle::new(12.0, 400.0))
                            .color(theme.text_muted),
                    ),
            )
    });

    // ---- search / header -------------------------------------------------
    let header = row()
        .w_full()
        .h(46.0)
        .corners([card_radius, card_radius, 0.0, 0.0])
        .pl(12.0)
        .pr(10.0)
        .gap(10.0)
        .center_y()
        .fill(theme.band)
        .border(1.0, hairline)
        .child(
            row()
                .h(22.0)
                .px(6.0)
                .radius(5.0)
                .gap(2.0)
                .center_y()
                .fill(theme.ink(0.05))
                .child(icon(&icons::COMMAND, 11.0).color(theme.text_muted.alpha(0.7)))
                .child(
                    text("K")
                        .font(TextStyle::new(11.0, 500.0).mono())
                        .color(theme.text_muted.alpha(0.7)),
                ),
        )
        .child(
            text_input(&flow.search, TextStyle::new(14.0, 400.0))
                .grow(1.0)
                .placeholder("Search folders…", theme.text_faint)
                .caret(theme.caret, theme.selection)
                .autofocus()
                .on_edit(|app: &mut App| {
                    &mut app.shell.add_space.as_mut().expect("add-space open").search
                })
                .on_input_change(|app: &mut App, _| {
                    // Typing `/` after a query that names a folder descends into it.
                    let _ = slash_descend(app);
                    if let Some(flow) = app.shell.add_space.as_mut() {
                        flow.active = 0;
                    }
                })
                .on_input_key(|app: &mut App, ev, tx| add_space_key(app, ev, tx)),
        )
        .child(
            el().h(22.0)
                .px(8.0)
                .radius(5.0)
                .center()
                .fill(theme.solid)
                .cursor(Cursor::Pointer)
                .opacity(if flow.submit_busy || listing.is_none() {
                    0.6
                } else {
                    1.0
                })
                .on_press(|app: &mut App| submit_add_space(app))
                .children(if flow.submit_busy {
                    vec![
                        text("Adding…")
                            .font(TextStyle::new(12.0, 500.0))
                            .color(theme.on_solid),
                    ]
                } else {
                    vec![
                        icon(&icons::COMMAND, 11.0).color(theme.on_solid.alpha(0.8)),
                        text("Enter")
                            .font(TextStyle::new(12.0, 500.0))
                            .color(theme.on_solid),
                    ]
                }),
        )
        .child(
            small_key_chip(theme, "esc")
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| close_add_space(app)),
        );

    // ---- footer ----------------------------------------------------------
    let footer = row()
        .w_full()
        .corners([0.0, 0.0, card_radius, card_radius])
        .fill(theme.band)
        .border(1.0, hairline)
        .px(12.0)
        .py(8.0)
        .gap(12.0)
        .center_y()
        .child(key_hint(theme, Some(&icons::ARROW_UP), ""))
        .child(key_hint(theme, Some(&icons::ARROW_DOWN), "Navigate"))
        .child(key_hint(theme, Some(&icons::ARROW_LEFT), "Up"))
        .child(key_hint(theme, Some(&icons::ARROW_RIGHT), "Open"))
        .child(key_hint(theme, None, "tab Complete"))
        .child(key_hint(theme, None, "⌘⏎ Add"))
        .child(el().grow(1.0))
        .child(
            el().px(6.0)
                .py(2.0)
                .radius(5.0)
                .hover(theme.element_hover)
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| open_repo_tool(app, RepoKind::Clone))
                .child(
                    text("Clone repo…")
                        .font(TextStyle::new(11.0, 500.0))
                        .color(theme.text_muted),
                ),
        )
        .child(
            el().px(6.0)
                .py(2.0)
                .radius(5.0)
                .hover(theme.element_hover)
                .cursor(Cursor::Pointer)
                .on_press(|app: &mut App| open_repo_tool(app, RepoKind::Create))
                .child(
                    text("New repo…")
                        .font(TextStyle::new(11.0, 500.0))
                        .color(theme.text_muted),
                ),
        )
        .children(flow.error.as_ref().map(|message| {
            text(message.clone())
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.danger)
                .truncate()
        }));

    let body = row()
        .w_full()
        .h(330.0)
        .items_stretch()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .min_h(0.0)
                .child(crumbs)
                .child(list),
        )
        .child(rail);

    let mut card = col()
        .key("add-space-palette")
        .w(680.0)
        .radius(card_radius)
        .border(1.0, theme.hairline(0.10))
        .fill(if theme.is_frost() {
            theme.glass_overlay()
        } else {
            theme.surface_overlay
        })
        .clip_rounded(card_radius)
        .color(theme.text)
        .child(header)
        .child(body);
    if let Some(tool) = tool_bar {
        card = card.child(tool);
    }
    card = card.child(footer);

    layer.push(
        el().key("add-space-scrim")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .fill(theme.scrim())
            .on_press(|app: &mut App| close_add_space(app))
            .a11y_hidden(),
    );
    layer.push(
        el().key("add-space-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .center()
            .padding(16.0)
            .child(card.modal(true, |app: &mut App| close_add_space(app))),
    );
}

fn location_row(
    theme: &Theme,
    name: &str,
    glyph: &'static rune::Icon,
    path: Option<String>,
    active: bool,
) -> El {
    row()
        .h(28.0)
        .px(8.0)
        .radius(8.0)
        .gap(8.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .fill(if active {
            crate::theme::card_selected_bg(theme.appearance)
        } else {
            theme.ink(0.0)
        })
        .hover(theme.element_hover)
        .on_press(move |app: &mut App| goto_location(app, path.clone()))
        .child(icon(glyph, 14.0).color(theme.text_muted.alpha(0.8)))
        .child(
            text(name.to_string())
                .font(TextStyle::new(12.5, 400.0))
                .color(if active {
                    theme.text
                } else {
                    theme.text_muted.alpha(0.7)
                })
                .truncate()
                .grow(1.0),
        )
}

fn skeleton_rows(theme: &Theme, count: usize) -> El {
    col()
        .w_full()
        .px(8.0)
        .py(6.0)
        .gap(6.0)
        .children((0..count).map(|i| {
            el().w_full()
                .h(24.0)
                .radius(6.0)
                .fill(theme.ink(0.05))
                .opacity(0.5 + 0.03 * (i as f32))
        }))
}

// ---------------------------------------------------------------------------
// Space context menu / dialogs
// ---------------------------------------------------------------------------

/// The space-row context menu + rename/delete dialogs (appended to the shell overlay layer).
pub fn render_space_overlays(app: &App, theme: &Theme, layer: &mut Vec<El>) {
    if let Some(menu) = app.shell.space_menu.clone() {
        let space_id = menu.space_id.clone();
        let delete_id = space_id.clone();
        let menu = rune_kit::Menu::new("space-context")
            .trigger(el())
            .open(true)
            .at(menu.x, menu.y)
            .min_width(170.0)
            .on_close(|app: &mut App| app.shell.space_menu = None)
            .item(
                rune_kit::MenuItem::new("Rename…")
                    .icon(&icons::PEN)
                    .on_select(move |app: &mut App| open_rename_space(app, space_id.clone())),
            )
            .separator()
            .item(
                rune_kit::MenuItem::new("Remove…")
                    .icon(&icons::TRASH_BIN_MINIMALISTIC)
                    .danger(true)
                    .on_select(move |app: &mut App| {
                        app.shell.space_menu = None;
                        app.shell.delete_space_confirm = Some(delete_id.clone());
                    }),
            );
        let _ = menu.build(&super::shell_kit(theme), layer);
    }

    if let Some(dialog) = app.shell.rename_space.as_ref() {
        let body = col()
            .gap(12.0)
            .child(
                text_input(&dialog.input, TextStyle::new(14.0, 400.0))
                    .w_full()
                    .h(34.0)
                    .px(8.0)
                    .radius(6.0)
                    .fill(theme.input_bg)
                    .border(1.0, theme.border)
                    .selectable(true)
                    .autofocus()
                    .on_edit(|app: &mut App| {
                        &mut app
                            .shell
                            .rename_space
                            .as_mut()
                            .expect("rename space open")
                            .input
                    })
                    .on_submit(|app: &mut App, _| submit_rename_space(app)),
            )
            .child(
                row()
                    .w_full()
                    .justify_end()
                    .gap(8.0)
                    .child(
                        dialog_button(theme, "Cancel", false).on_press(|app: &mut App| {
                            app.shell.rename_space = None;
                        }),
                    )
                    .child(
                        dialog_button(theme, "Rename", true)
                            .on_press(|app: &mut App| submit_rename_space(app)),
                    ),
            );
        rune_kit::Dialog::new("rename-space-dialog", "Rename project")
            .open(true)
            .width(420.0)
            .fit()
            .on_close(|app: &mut App| app.shell.rename_space = None)
            .child(body)
            .build(&super::shell_kit(theme), layer);
    }

    if let Some(space_id) = app.shell.delete_space_confirm.clone() {
        let (name, device, count) = {
            let space = app.space_row(&space_id);
            (
                space
                    .map(|s| s.display_name().to_string())
                    .unwrap_or_else(|| "this project".into()),
                space
                    .and_then(|s| app.device_name(&s.device_id))
                    .unwrap_or("its device")
                    .to_string(),
                app.visible_chats()
                    .filter(|c| c.space_id.as_deref() == Some(space_id.as_str()))
                    .count(),
            )
        };
        let copy = if count == 1 {
            format!(
                "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone."
            )
        } else {
            format!(
                "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone."
            )
        };
        let close_id = space_id.clone();
        let body = row()
            .w_full()
            .justify_end()
            .gap(8.0)
            .child(
                dialog_button(theme, "Cancel", false).on_press(|app: &mut App| {
                    app.shell.delete_space_confirm = None;
                }),
            )
            .child(
                dialog_button(theme, "Remove", true)
                    .on_press(move |app: &mut App| delete_space(app, close_id.clone())),
            );
        rune_kit::Dialog::new("delete-space-dialog", "Remove project?")
            .open(true)
            .width(440.0)
            .fit()
            .description(copy)
            .on_close(|app: &mut App| app.shell.delete_space_confirm = None)
            .child(body)
            .build(&super::shell_kit(theme), layer);
    }
}

fn dialog_button(theme: &Theme, label: &str, primary: bool) -> El {
    el().h(30.0)
        .px(12.0)
        .radius(Theme::CONTROL_RADIUS)
        .center()
        .cursor(Cursor::Pointer)
        .fill(if primary { theme.solid } else { theme.ink(0.0) })
        .hover(if primary {
            theme.surface_raised_hover
        } else {
            theme.element_hover
        })
        .child(
            text(label.to_string())
                .font(TextStyle::new(13.0, 500.0))
                .color(if primary { theme.on_solid } else { theme.text }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_under_is_segment_aware() {
        assert!(path_under("/media/a", "/media/a"));
        assert!(path_under("/media/a/b", "/media/a"));
        assert!(!path_under("/media/ab", "/media/a"));
        assert!(path_under("/", ""));
        assert!(path_under("/anything", "/"));
    }

    #[test]
    fn breadcrumbs_carry_full_paths() {
        assert_eq!(
            breadcrumbs("/home/dev/hearth"),
            vec![
                ("home".into(), "/home".into()),
                ("dev".into(), "/home/dev".into()),
                ("hearth".into(), "/home/dev/hearth".into()),
            ]
        );
        assert!(breadcrumbs("/").is_empty());
    }

    #[test]
    fn parent_and_child_round_trip() {
        assert_eq!(parent_path("/home/dev/hearth"), Some("/home/dev".into()));
        assert_eq!(parent_path("/home"), Some("/".into()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(child_path("/home/dev", "hearth"), "/home/dev/hearth");
        assert_eq!(child_path("/home/dev/", "hearth"), "/home/dev/hearth");
    }

    #[test]
    fn typed_paths_resolve_against_home() {
        assert_eq!(
            typed_path_target("~/Projects", Some("/home/dev")),
            Some("/home/dev/Projects".into())
        );
        assert_eq!(
            typed_path_target("/disk2", Some("/home/dev")),
            Some("/disk2".into())
        );
        assert_eq!(typed_path_target("relative", Some("/home/dev")), None);
        assert_eq!(typed_path_target("~/x", None), None);
    }

    #[test]
    fn segment_target_prefers_exact_then_unique_prefix() {
        let names = ["Alpha", "alpine", "beta"];
        assert_eq!(segment_target(&names, "Alpha"), Some(0));
        assert_eq!(segment_target(&names, "alp"), None);
        assert_eq!(segment_target(&names, "beta"), Some(2));
        assert_eq!(segment_target(&names, "be"), Some(2));
        assert_eq!(segment_target(&names, "gamma"), None);
    }

    #[test]
    fn completion_prefix_len_is_byte_safe() {
        assert_eq!(completion_prefix_len("hearth", "hea"), Some(3));
        assert_eq!(completion_prefix_len("hearth", "hea"), Some(3));
        assert_eq!(completion_prefix_len("hearth", "xyz"), None);
        assert_eq!(completion_prefix_len("hé", "h"), Some(1));
        // Cutting inside a multibyte character is not a completion.
        assert_eq!(completion_prefix_len("hé", "h\x00"), None);
    }

    #[test]
    fn filter_ranks_prefix_matches_first() {
        let names = ["application", "map", "app"];
        assert_eq!(filter_indices("app", &names), vec![0, 2]);
        assert_eq!(filter_indices("", &names), vec![0, 1, 2]);
        assert_eq!(filter_indices("zzz", &names), Vec::<usize>::new());
    }

    #[test]
    fn highlight_wraps() {
        assert_eq!(step_highlight(0, 3, 1), 1);
        assert_eq!(step_highlight(2, 3, 1), 0);
        assert_eq!(step_highlight(0, 3, -1), 2);
        assert_eq!(step_highlight(0, 0, 1), 0);
    }

    #[test]
    fn loadable_states_are_distinct() {
        let idle: Loadable<u8> = Loadable::Idle;
        assert!(idle.is_loading());
        assert!(Loadable::Ready(3u8).ready() == Some(&3));
        assert_eq!(Loadable::<u8>::Error("x".into()).error(), Some("x"));
    }
}
