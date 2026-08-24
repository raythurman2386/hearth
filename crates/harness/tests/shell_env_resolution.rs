//! End-to-end: a CLI visible only through the login shell's PATH resolves.
//!
//! This file must stay a single test: it mutates process env (SHELL/PATH/HOME)
//! and warms the process-global login-shell snapshot cache, so it needs its
//! own test binary with no parallel siblings.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hearth_harness::AcpHarness;

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn cli_on_login_shell_path_only_is_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let shell_bin = dir.path().join("shell-bin");
    std::fs::create_dir(&shell_bin).unwrap();
    write_executable(&shell_bin.join("grok"), "#!/bin/sh\nexit 0\n");
    write_executable(&shell_bin.join("raven"), "#!/bin/sh\nexit 0\n");

    // A $SHELL whose init shapes PATH — the shape resolution must survive.
    let fake_shell = dir.path().join("fake-shell");
    write_executable(
        &fake_shell,
        &format!(
            "#!/bin/sh\nPATH=\"{}:/usr/bin:/bin\"; export PATH\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = \"-c\" ]; then shift; exec /bin/sh -c \"$1\"; fi\n\
               shift\n\
             done\nexit 1\n",
            shell_bin.display()
        ),
    );

    // A GUI/service-launch environment: minimal PATH, no CLIs reachable, HOME
    // pointed away from any real install dirs.
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("SHELL", &fake_shell);
        std::env::set_var("HOME", dir.path());
        std::env::set_var("PATH", "/usr/bin:/bin");
        std::env::remove_var("GROK_EXECUTABLE");
        std::env::remove_var("RAVEN_EXECUTABLE");
        std::env::remove_var("HEARTH_NO_LOGIN_SHELL");
    }

    let snapshot = hearth_harness::shell_env::login_shell_path().expect("snapshot captured");
    let snapshot = snapshot.to_string_lossy();
    assert!(
        snapshot.starts_with(&format!("{}:", shell_bin.display())),
        "snapshot should carry the shell-shaped PATH, got: {snapshot}"
    );

    // The agent binaries are only reachable through the snapshot; the
    // launch program (not the npx fallback) must be the shell-PATH binary,
    // proving resolution consulted the login-shell snapshot.
    // Native drivers consult the same snapshot for the agent CLI itself.
    let grok = AcpHarness::grok()
        .launch_program()
        .expect("grok resolves via login-shell PATH");
    assert_eq!(grok, shell_bin.join("grok"), "{grok:?}");
    let raven = AcpHarness::raven()
        .launch_program()
        .expect("raven resolves via login-shell PATH");
    assert_eq!(raven, shell_bin.join("raven"), "{raven:?}");
}
