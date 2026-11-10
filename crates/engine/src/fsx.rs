//! Shared atomic file-publication primitives.
//!
//! Every durable small file in the engine is written to a unique same-directory
//! temp file first, then either hard-linked into place (create-if-absent) or
//! renamed over the destination (atomic replace). Keeping both shapes here means
//! the temp naming, cleanup, and error handling stay identical everywhere.

use std::io::Write;
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Result of [`publish_new`]: whether this call created the file or lost to an
/// existing one. The caller decides what an existing file means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    Created,
    Exists,
}

/// Same-directory temp path: `.<name>.tmp-<pid>-<uuid>`.
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    path.with_file_name(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ))
}

/// Write `bytes` to a fresh temp file and sync it, returning its path. The temp
/// is removed again if any step fails.
fn write_temp(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let temp = temp_path(path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    match result {
        Ok(()) => Ok(temp),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Publish a fully-written file only if `path` does not exist yet.
///
/// The temp file is hard-linked into place, so concurrent creators race on the
/// link: exactly one wins with [`PublishOutcome::Created`], the rest observe
/// [`PublishOutcome::Exists`] and never overwrite the winner's bytes. The temp
/// is always removed.
pub(crate) fn publish_new(path: &Path, bytes: &[u8]) -> std::io::Result<PublishOutcome> {
    let temp = write_temp(path, bytes)?;
    let outcome = match std::fs::hard_link(&temp, path) {
        Ok(()) => Ok(PublishOutcome::Created),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(PublishOutcome::Exists),
        Err(err) => Err(err),
    };
    let _ = std::fs::remove_file(&temp);
    outcome
}

/// Atomically replace `path` with `bytes` via a same-directory temp + rename.
///
/// `secret` creates the temp with mode `0600` from birth on Unix so the content
/// is never readable by others, including before the rename. The temp is synced
/// before the rename so a crash cannot leave a shorter replacement, and a failed
/// write removes the temp and propagates the error.
pub(crate) fn replace_atomic(path: &Path, bytes: &[u8], secret: bool) -> std::io::Result<()> {
    let temp = temp_path(path);
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = secret;
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_new_creates_once_and_keeps_the_first_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("item.json");

        assert_eq!(
            publish_new(&path, b"first").expect("first publish"),
            PublishOutcome::Created
        );
        assert_eq!(
            publish_new(&path, b"second").expect("second publish"),
            PublishOutcome::Exists
        );
        assert_eq!(std::fs::read(&path).expect("read"), b"first");
        assert_eq!(leftover_temps(dir.path()), 0);
    }

    #[test]
    fn racing_publishers_create_exactly_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("raced");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let outcomes: Vec<PublishOutcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let path = path.clone();
                    let barrier = std::sync::Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        publish_new(&path, b"same-bytes").expect("publish")
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == PublishOutcome::Created)
                .count(),
            1
        );
        assert_eq!(std::fs::read(&path).expect("read"), b"same-bytes");
        assert_eq!(leftover_temps(dir.path()), 0);
    }

    #[test]
    fn replace_atomic_overwrites_and_cleans_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("item.json");
        std::fs::write(&path, b"old").expect("seed");

        replace_atomic(&path, b"new", false).expect("replace");

        assert_eq!(std::fs::read(&path).expect("read"), b"new");
        assert_eq!(leftover_temps(dir.path()), 0);
    }

    #[cfg(unix)]
    #[test]
    fn replace_atomic_secret_is_private_from_birth() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.json");

        replace_atomic(&path, b"token", true).expect("replace");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    fn leftover_temps(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count()
    }
}
