//! `hearth update` — check for and apply a newer release, natively (the same
//! flow `scripts/install.sh` performs: download → verify → symlink swap →
//! service restart). macOS app bundles swap the bundle instead; source builds
//! are report-only.

use anyhow::bail;
use hearth_update::{InstallKind, current_version, version_newer};

/// `--check` prints the verdict and exits (nonzero when an update is available,
/// so scripts can gate on it).
pub async fn update(edge_url: &str, check_only: bool) -> anyhow::Result<()> {
    let manifest = hearth_update::fetch_latest(edge_url).await?;
    let current = current_version();
    if !version_newer(&manifest.version, current) {
        println!(
            "hearth {current} is up to date (latest: {}).",
            manifest.version
        );
        return Ok(());
    }
    println!("hearth {current} → {} available", manifest.version);
    if check_only {
        std::process::exit(1);
    }

    match hearth_update::detect_install() {
        InstallKind::Managed { app_root } => {
            println!(
                "downloading {}…",
                hearth_update::headless_artifact(&manifest.version)
            );
            hearth_update::stage_headless(edge_url, &manifest, &app_root).await?;
            hearth_update::apply_headless(&app_root, &manifest.version)?;
            println!(
                "installed {} (current → {})",
                app_root.join(&manifest.version).display(),
                manifest.version
            );
            match hearth_update::restart_service() {
                Ok(()) => println!("engine service restarted."),
                Err(err) => println!(
                    "note: service restart failed ({err:#}) — restart the engine manually to finish."
                ),
            }
            Ok(())
        }
        InstallKind::MacApp { bundle } => {
            println!(
                "downloading {}…",
                hearth_update::mac_app_artifact(&manifest.version)
            );
            let data_dir = std::env::var_os("HEARTH_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(super::dirs_data_dir);
            let staged = hearth_update::stage_mac_app(edge_url, &manifest, &data_dir).await?;
            hearth_update::apply_mac_app(&staged, &bundle)?;
            println!("updated {} — relaunch Hearth to finish.", bundle.display());
            Ok(())
        }
        InstallKind::Unmanaged => {
            bail!(
                "this binary is not update-managed (source build or hand-copied).\n\
                 Linux: curl -fsSL https://hearth.sh/install.sh | sh\n\
                 macOS: download the new Hearth.app dmg, or rebuild from source."
            )
        }
    }
}
