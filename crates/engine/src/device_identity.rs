//! Stable per-installation device identity: load, flock-guarded create, atomic
//! publish (with legacy empty-file repair).

use std::path::Path;

use crate::fsx::{self, PublishOutcome};

use super::*;

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
pub(crate) fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    // EngineInfo is resolved before the lifetime InstanceLock is acquired, so
    // identity creation and legacy repair need their own short critical section.
    // The OS releases this lock after a crash; unlike a create_new lockfile it
    // cannot strand an installation permanently.
    let _identity_lock = DeviceIdentityLock::acquire(data_dir)?;
    let path = data_dir.join("device-id");
    let recovering_empty = match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_string()),
        // Older releases used truncate+write. A crash between those operations
        // left a zero-byte file that is safe to replace with a fresh identity.
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };

    let id = new_id();
    // Fresh installs use create-if-absent. Legacy empty files need an atomic
    // same-directory replacement on Unix; the Windows fallback runs under the
    // identity lock and remains recoverable if interrupted.
    let publish_result = if recovering_empty {
        match std::fs::read_to_string(&path) {
            Ok(winner) if !winner.trim().is_empty() => return Ok(winner.trim().to_string()),
            Ok(_) => replace_empty_device_id(&path, id.as_bytes()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fsx::publish_new(&path, id.as_bytes())
            }
            Err(err) => return Err(err.into()),
        }
    } else {
        fsx::publish_new(&path, id.as_bytes())
    };
    match publish_result {
        Ok(PublishOutcome::Created) => Ok(id),
        Ok(PublishOutcome::Exists) => read_winner(&path),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => read_winner(&path),
        Err(err) => Err(err.into()),
    }
}

fn read_winner(path: &Path) -> Result<String, EngineError> {
    let winner = std::fs::read_to_string(path)?;
    if winner.trim().is_empty() {
        Err(EngineError::Other(format!(
            "invalid device identity {}: file is empty",
            path.display()
        )))
    } else {
        Ok(winner.trim().to_string())
    }
}

struct DeviceIdentityLock {
    _file: std::fs::File,
}

impl DeviceIdentityLock {
    fn acquire(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join("device-id.lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
            let mut retries = 200;
            let file = loop {
                match options.open(&path) {
                    Ok(file) => break file,
                    Err(err)
                        if err.kind() == std::io::ErrorKind::PermissionDenied && retries > 0 =>
                    {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(err) => return Err(err.into()),
                }
            };
            return Ok(Self { _file: file });
        }

        #[cfg(not(windows))]
        let file = options.open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINTR) {
                    return Err(err.into());
                }
            }
        }

        #[cfg(not(windows))]
        Ok(Self { _file: file })
    }
}

fn replace_empty_device_id(path: &Path, bytes: &[u8]) -> std::io::Result<PublishOutcome> {
    #[cfg(unix)]
    {
        fsx::replace_atomic(path, bytes, false)?;
        Ok(PublishOutcome::Created)
    }
    #[cfg(not(unix))]
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        fsx::publish_new(path, bytes)
    }
}
