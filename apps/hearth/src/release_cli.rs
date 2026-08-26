//! `hearth release publish` — verify release artifacts and publish them into
//! the hub's `{data_dir}/releases/` tree so peers can `hearth update`.

use std::path::PathBuf;

use anyhow::{Context as _, bail};

#[derive(Debug, Clone)]
pub enum PublishSource {
    /// Local directory that already contains `manifest.json` + artifacts
    /// (CI download, `target/package` after `write-manifest.sh`, etc.).
    Dir(PathBuf),
    /// Fetch updater assets from a GitHub Release (`None` = latest).
    GitHub { tag: Option<String> },
}

pub async fn publish(
    source: PublishSource,
    data_dir: PathBuf,
    force: bool,
    check_only: bool,
) -> anyhow::Result<()> {
    let releases_dir = data_dir.join("releases");
    let (source_dir, cleanup) = match source {
        PublishSource::Dir(dir) => {
            let dir = dir
                .canonicalize()
                .with_context(|| format!("resolving {}", dir.display()))?;
            (dir, None)
        }
        PublishSource::GitHub { tag } => {
            let tmp = tempfile_dir()?;
            let repo = hearth_update::release_repo();
            println!(
                "fetching GitHub release {} from {repo}…",
                tag.as_deref().unwrap_or("latest")
            );
            let (manifest, dir) =
                hearth_update::fetch_github_release(&repo, tag.as_deref(), &tmp).await?;
            println!(
                "downloaded {} ({} file(s))",
                manifest.version,
                manifest.files.len()
            );
            (dir, Some(tmp))
        }
    };

    let version = hearth_update::publish_to_hub(&source_dir, &releases_dir, force, check_only)?;
    if let Some(tmp) = cleanup {
        let _ = std::fs::remove_dir_all(tmp);
    }
    if check_only {
        return Ok(());
    }
    println!("published hearth {version} → {}", releases_dir.display());
    println!("  {}/manifest.json", releases_dir.display());
    println!("  {}/latest.txt", releases_dir.display());
    if let Ok(host) = std::env::var("HEARTH_TAILNET_HOST") {
        let host = host.trim();
        if !host.is_empty() {
            let port = std::env::var("HEARTH_TAILNET_PORT")
                .ok()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(27655);
            println!(
                "peers can check with:\n  curl -fsS http://{host}:{port}/releases/manifest.json\n  hearth update --check"
            );
        }
    } else {
        println!("tip: set HEARTH_TAILNET_HOST so peers can reach this hub's /releases/*");
    }
    Ok(())
}

fn tempfile_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::temp_dir().join(format!(
        "hearth-release-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    Ok(base)
}

/// Parse `--from github|dir` + optional tag/path for clap wiring in `main`.
pub fn parse_from(from: &str, target: Option<String>) -> anyhow::Result<PublishSource> {
    match from.trim().to_ascii_lowercase().as_str() {
        "github" | "gh" => Ok(PublishSource::GitHub { tag: target }),
        "dir" | "path" => {
            let path = target.ok_or_else(|| {
                anyhow::anyhow!("--from dir requires a path (e.g. --from dir ./artifacts)")
            })?;
            Ok(PublishSource::Dir(PathBuf::from(path)))
        }
        other => bail!("unknown --from {other:?} (expected github or dir)"),
    }
}
