//! Publish verified release artifacts into a hub `{data_dir}/releases/` tree.
//!
//! Layout written (atomically via a staging dir):
//!   latest.txt
//!   manifest.json
//!   hearth-<ver>-linux-<arch>.tar.gz
//!   (+ any other files listed in the source manifest)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use sha2::{Digest, Sha256};

use crate::{Manifest, version_newer};

/// Default GitHub repo used by `--from github` when `HEARTH_RELEASE_REPO` is unset.
pub const DEFAULT_RELEASE_REPO: &str = "raythurman2386/hearth";

pub fn release_repo() -> String {
    std::env::var("HEARTH_RELEASE_REPO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_REPO.to_string())
}

/// Load + validate a source directory that already contains `manifest.json`
/// and every file it names.
pub fn load_release_dir(dir: &Path) -> anyhow::Result<(Manifest, PathBuf)> {
    let manifest_path = dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "reading {} — expected a release dir with manifest.json \
             (CI writes it; or run scripts/write-manifest.sh)",
            manifest_path.display()
        )
    })?;
    let manifest: Manifest = serde_json::from_str(&raw).context("parsing manifest.json")?;
    if manifest.version.trim().is_empty() {
        bail!("manifest.json has an empty version");
    }
    if manifest.files.is_empty() {
        bail!("manifest.json lists no files");
    }
    verify_dir_checksums(dir, &manifest)?;
    Ok((manifest, dir.to_path_buf()))
}

/// sha256 every `manifest.files` entry against bytes on disk.
pub fn verify_dir_checksums(dir: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    for (name, meta) in &manifest.files {
        let Some(expected) = meta
            .sha256
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            bail!("manifest entry {name} is missing sha256");
        };
        let path = dir.join(name);
        let actual = file_sha256(&path).with_context(|| format!("hashing {}", path.display()))?;
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("checksum mismatch for {name}: manifest={expected}, disk={actual}");
        }
    }
    Ok(())
}

pub fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("reading {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Read the hub's current `manifest.json` if present.
pub fn read_hub_manifest(releases_dir: &Path) -> anyhow::Result<Option<Manifest>> {
    let path = releases_dir.join("manifest.json");
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: Manifest = serde_json::from_str(&raw).context("parsing hub manifest.json")?;
    Ok(Some(manifest))
}

/// Copy a verified release directory into `{releases_dir}` atomically.
///
/// Returns the published version.
pub fn publish_to_hub(
    source_dir: &Path,
    releases_dir: &Path,
    force: bool,
    check_only: bool,
) -> anyhow::Result<String> {
    let (manifest, _) = load_release_dir(source_dir)?;
    if let Some(existing) = read_hub_manifest(releases_dir)? {
        let newer = version_newer(&manifest.version, &existing.version);
        let same =
            manifest.version.trim_start_matches('v') == existing.version.trim_start_matches('v');
        if !force && !newer {
            if same {
                bail!(
                    "hub already has {} — pass --force to republish",
                    existing.version
                );
            }
            bail!(
                "refusing to publish {} over newer hub version {} (pass --force)",
                manifest.version,
                existing.version
            );
        }
    }

    if check_only {
        println!(
            "ok: would publish {} ({} file(s)) → {}",
            manifest.version,
            manifest.files.len(),
            releases_dir.display()
        );
        for name in manifest.files.keys() {
            println!("  {name}");
        }
        return Ok(manifest.version);
    }

    std::fs::create_dir_all(releases_dir)
        .with_context(|| format!("creating {}", releases_dir.display()))?;
    let stage = releases_dir.join(format!(
        ".stage-{}-{}",
        manifest.version,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;

    let result = (|| {
        for name in manifest.files.keys() {
            let src = source_dir.join(name);
            let dst = stage.join(name);
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {} → {}", src.display(), dst.display()))?;
        }
        // Re-verify staged bytes before promoting.
        verify_dir_checksums(&stage, &manifest)?;
        let manifest_json =
            serde_json::to_string_pretty(&manifest).context("serializing manifest")?;
        std::fs::write(stage.join("manifest.json"), format!("{manifest_json}\n"))
            .context("writing staged manifest.json")?;
        std::fs::write(stage.join("latest.txt"), format!("{}\n", manifest.version))
            .context("writing staged latest.txt")?;

        // Promote files into the live releases dir. Artifacts first, metadata last
        // so clients never observe a new latest pointing at missing bytes.
        for name in manifest.files.keys() {
            let from = stage.join(name);
            let to = releases_dir.join(name);
            std::fs::rename(&from, &to)
                .with_context(|| format!("promoting {} → {}", from.display(), to.display()))?;
        }
        atomic_replace(
            &stage.join("manifest.json"),
            &releases_dir.join("manifest.json"),
        )?;
        atomic_replace(&stage.join("latest.txt"), &releases_dir.join("latest.txt"))?;
        Ok(manifest.version.clone())
    })();

    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn atomic_replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    let tmp = to.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::rename(from, &tmp)
        .with_context(|| format!("moving {} → {}", from.display(), tmp.display()))?;
    std::fs::rename(&tmp, to).with_context(|| format!("replacing {}", to.display()))?;
    Ok(())
}

/// Download a GitHub Release's updater assets into `dest_dir`.
///
/// `tag` is `v0.2.3` or `0.2.3` or `None` for latest. Requires `manifest.json`
/// on the release (written by the release workflow).
pub async fn fetch_github_release(
    repo: &str,
    tag: Option<&str>,
    dest_dir: &Path,
) -> anyhow::Result<(Manifest, PathBuf)> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let client = github_client()?;
    let release_url = match tag {
        Some(tag) => {
            let tag = if tag.starts_with('v') {
                tag.to_string()
            } else {
                format!("v{tag}")
            };
            format!("https://api.github.com/repos/{repo}/releases/tags/{tag}")
        }
        None => format!("https://api.github.com/repos/{repo}/releases/latest"),
    };
    let release: GitHubRelease = client
        .get(&release_url)
        .send()
        .await
        .with_context(|| format!("fetching {release_url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {release_url}"))?
        .json()
        .await
        .context("parsing GitHub release JSON")?;

    let manifest_asset = release
        .assets
        .iter()
        .find(|a| a.name == "manifest.json")
        .with_context(|| {
            format!(
                "GitHub release {} has no manifest.json — re-run the Release workflow \
                 after the updater packaging change",
                release.tag_name
            )
        })?;

    download_asset(&client, manifest_asset, &dest_dir.join("manifest.json")).await?;
    let raw = std::fs::read_to_string(dest_dir.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_str(&raw).context("parsing downloaded manifest")?;

    // Prefer assets named in the manifest; also grab latest.txt when present.
    let mut wanted: BTreeMap<String, &GitHubAsset> = BTreeMap::new();
    for name in manifest.files.keys() {
        let asset = release
            .assets
            .iter()
            .find(|a| &a.name == name)
            .with_context(|| format!("GitHub release missing asset {name}"))?;
        wanted.insert(name.clone(), asset);
    }
    if let Some(latest) = release.assets.iter().find(|a| a.name == "latest.txt") {
        download_asset(&client, latest, &dest_dir.join("latest.txt")).await?;
    } else {
        std::fs::write(
            dest_dir.join("latest.txt"),
            format!("{}\n", manifest.version),
        )?;
    }
    for (name, asset) in wanted {
        download_asset(&client, asset, &dest_dir.join(&name)).await?;
    }
    verify_dir_checksums(dest_dir, &manifest)?;
    Ok((manifest, dest_dir.to_path_buf()))
}

fn github_client() -> anyhow::Result<reqwest::Client> {
    let mut builder =
        reqwest::Client::builder().user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")));
    // Optional token for private repos / higher rate limits.
    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}")
                    .parse()
                    .context("GITHUB_TOKEN / GH_TOKEN is not a valid header value")?,
            );
            headers.insert(
                reqwest::header::ACCEPT,
                "application/vnd.github+json".parse().unwrap(),
            );
            builder = builder.default_headers(headers);
        }
    }
    builder.build().context("building GitHub HTTP client")
}

async fn download_asset(
    client: &reqwest::Client,
    asset: &GitHubAsset,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = &asset.browser_download_url;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let bytes = resp.bytes().await.context("reading asset body")?;
    std::fs::write(dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, serde::Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileMeta;

    fn manifest_for_files(
        version: &str,
        files: impl IntoIterator<Item = (String, String)>,
    ) -> Manifest {
        let mut map = BTreeMap::new();
        for (name, sha) in files {
            map.insert(name, FileMeta { sha256: Some(sha) });
        }
        Manifest {
            version: version.to_string(),
            files: map,
        }
    }

    #[test]
    fn publish_roundtrip() {
        let src = tempfile::tempdir().unwrap();
        let hub = tempfile::tempdir().unwrap();
        let tarball = src.path().join("hearth-0.2.3-linux-x86_64.tar.gz");
        std::fs::write(&tarball, b"fake-tarball-bytes").unwrap();
        let sha = file_sha256(&tarball).unwrap();
        let manifest =
            manifest_for_files("0.2.3", [("hearth-0.2.3-linux-x86_64.tar.gz".into(), sha)]);
        std::fs::write(
            src.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(src.path().join("latest.txt"), "0.2.3\n").unwrap();

        let ver = publish_to_hub(src.path(), hub.path(), false, false).unwrap();
        assert_eq!(ver, "0.2.3");
        assert!(hub.path().join("manifest.json").is_file());
        assert!(hub.path().join("latest.txt").is_file());
        assert!(
            hub.path()
                .join("hearth-0.2.3-linux-x86_64.tar.gz")
                .is_file()
        );

        // Same version without --force fails.
        assert!(publish_to_hub(src.path(), hub.path(), false, false).is_err());
        // With --force succeeds.
        publish_to_hub(src.path(), hub.path(), true, false).unwrap();
        // --check does not write.
        let hub2 = tempfile::tempdir().unwrap();
        publish_to_hub(src.path(), hub2.path(), false, true).unwrap();
        assert!(!hub2.path().join("manifest.json").is_file());
    }

    #[test]
    fn refuses_missing_checksum() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("hearth-0.2.3-linux-x86_64.tar.gz"), b"x").unwrap();
        std::fs::write(
            src.path().join("manifest.json"),
            r#"{"version":"0.2.3","files":{"hearth-0.2.3-linux-x86_64.tar.gz":{}}}"#,
        )
        .unwrap();
        assert!(load_release_dir(src.path()).is_err());
    }
}
