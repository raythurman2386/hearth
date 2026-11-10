//! Locating the device's installed Codex CLI: the `CODEX_EXECUTABLE`
//! override, then PATH, the login-shell PATH snapshot, and the known install
//! locations.

use std::path::PathBuf;

/// Locate the device's installed Codex CLI: `CODEX_EXECUTABLE`, then our own
/// PATH, then the login-shell PATH snapshot (the user's shell init shapes
/// PATH in ways a GUI/service launch never sees — see [`crate::shell_env`]),
/// then known install locations as a last resort. Resolved per call — cheap
/// after the snapshot is cached.
pub(crate) fn resolve_codex_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CODEX_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(shell_path) = crate::shell_env::login_shell_path() {
        candidates.extend(
            std::env::split_paths(shell_path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe)),
        );
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local").join("bin").join("codex"));
        candidates.push(home.join(".codex").join("bin").join("codex"));
        candidates.push(home.join(".npm-global").join("bin").join("codex"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
    candidates.push(PathBuf::from("/usr/local/bin/codex"));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|d| d.join(exe)),
    );
    candidates.into_iter().find(|p| p.exists())
}
