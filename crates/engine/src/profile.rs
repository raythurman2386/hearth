//! Workspace profile identity and storage boundaries.
//!
//! Stores, journals, and uploads are profile-scoped. Repositories, worktrees,
//! agent credentials, UI settings, and the device id remain device-scoped under
//! the engine data directory.

use std::path::{Path, PathBuf};

use hearth_proto::WorkspaceScope;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::EngineError;

const LOCAL_PROFILE_FILE: &str = "local-profile.json";
const LOCAL_ORG_ID: &str = "local";
const LEGACY_UPLOADS_DIR: &str = "uploads";
const LEGACY_UPLOADS_OWNER_FILE: &str = "legacy-uploads-owner.json";

/// A resolved, immutable identity and storage boundary for one engine runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineProfile {
    scope: WorkspaceScope,
    device_root: PathBuf,
    store_root: PathBuf,
    uploads_root: PathBuf,
    org_id: String,
    user_id: String,
}

impl EngineProfile {
    /// Load or create the installation's stable local identity.
    pub fn local(data_dir: &Path) -> Result<Self, EngineError> {
        let profile_id = load_or_create_local_profile_id(data_dir)?;
        let store_root = data_dir.join("profiles").join("local");
        Ok(Self {
            scope: WorkspaceScope::Local,
            device_root: data_dir.to_path_buf(),
            uploads_root: store_root.join("uploads"),
            store_root,
            org_id: LOCAL_ORG_ID.to_string(),
            user_id: profile_id,
        })
    }

    /// Resolve the explicit development profile under its account-specific storage root.
    pub fn development(data_dir: &Path, org_id: &str, user_id: &str) -> Self {
        let store_root = data_dir
            .join("orgs")
            .join(sanitize_path_id(org_id))
            .join(sanitize_path_id(user_id));
        Self {
            scope: WorkspaceScope::Development,
            device_root: data_dir.to_path_buf(),
            uploads_root: store_root.join("uploads"),
            store_root,
            org_id: org_id.to_string(),
            user_id: user_id.to_string(),
        }
    }

    pub fn scope(&self) -> WorkspaceScope {
        self.scope
    }

    pub fn device_root(&self) -> &Path {
        &self.device_root
    }

    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub fn uploads_root(&self) -> &Path {
        &self.uploads_root
    }

    pub fn org_id(&self) -> &str {
        &self.org_id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Claim the historical device-global uploads cache for one account.
    ///
    /// Legacy transcript entries persist absolute paths under `{data_dir}/uploads`,
    /// so moving the directory would break them. The first account opened after
    /// upgrade becomes its durable owner and may read it as a compatibility
    /// fallback. New writes always use [`Self::uploads_root`], and other accounts
    /// never receive the fallback root.
    pub(crate) fn claim_legacy_uploads_root(&self) -> Result<Option<PathBuf>, EngineError> {
        if self.scope == WorkspaceScope::Local {
            return Ok(None);
        }

        let legacy_root = self.device_root.join(LEGACY_UPLOADS_DIR);
        let metadata = match std::fs::metadata(&legacy_root) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if !metadata.is_dir() {
            return Err(EngineError::Other(format!(
                "invalid legacy uploads cache {}: expected a directory",
                legacy_root.display()
            )));
        }

        let owner_path = self.device_root.join(LEGACY_UPLOADS_OWNER_FILE);
        let expected = LegacyUploadsOwner {
            org_id: self.org_id.clone(),
            user_id: self.user_id.clone(),
        };
        let owner = match read_legacy_uploads_owner(&owner_path) {
            Ok(owner) => owner,
            Err(ProfileReadError::Missing) => {
                publish_legacy_uploads_owner(&owner_path, &expected)?;
                read_legacy_uploads_owner(&owner_path).map_err(ProfileReadError::into_engine)?
            }
            Err(ProfileReadError::Engine(err)) => return Err(err),
        };

        Ok((owner == expected).then_some(legacy_root))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalProfileFile {
    id: Uuid,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyUploadsOwner {
    org_id: String,
    user_id: String,
}

fn load_or_create_local_profile_id(data_dir: &Path) -> Result<String, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(LOCAL_PROFILE_FILE);
    match read_local_profile_id(&path) {
        Ok(id) => return Ok(id),
        Err(ProfileReadError::Missing) => {}
        Err(ProfileReadError::Engine(err)) => return Err(err),
    }

    let id = Uuid::new_v4();
    let mut bytes = serde_json::to_vec_pretty(&LocalProfileFile { id })
        .map_err(|err| EngineError::Other(format!("serialize local profile: {err}")))?;
    bytes.push(b'\n');

    // Publish a fully-written same-directory file without replacing a profile
    // another process may have created concurrently.
    match crate::fsx::publish_new(&path, &bytes)? {
        crate::fsx::PublishOutcome::Created => Ok(id.to_string()),
        crate::fsx::PublishOutcome::Exists => {
            read_local_profile_id(&path).map_err(ProfileReadError::into_engine)
        }
    }
}

enum ProfileReadError {
    Missing,
    Engine(EngineError),
}

impl ProfileReadError {
    fn into_engine(self) -> EngineError {
        match self {
            Self::Missing => EngineError::Other("local profile disappeared during creation".into()),
            Self::Engine(err) => err,
        }
    }
}

fn read_local_profile_id(path: &Path) -> Result<String, ProfileReadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProfileReadError::Missing);
        }
        Err(err) => return Err(ProfileReadError::Engine(err.into())),
    };
    let profile: LocalProfileFile = serde_json::from_slice(&bytes).map_err(|err| {
        ProfileReadError::Engine(EngineError::Other(format!(
            "invalid local profile {}: {err}",
            path.display()
        )))
    })?;
    if profile.id.is_nil() {
        return Err(ProfileReadError::Engine(EngineError::Other(format!(
            "invalid local profile {}: id must not be nil",
            path.display()
        ))));
    }
    Ok(profile.id.to_string())
}

fn read_legacy_uploads_owner(path: &Path) -> Result<LegacyUploadsOwner, ProfileReadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProfileReadError::Missing);
        }
        Err(err) => return Err(ProfileReadError::Engine(err.into())),
    };
    serde_json::from_slice(&bytes).map_err(|err| {
        ProfileReadError::Engine(EngineError::Other(format!(
            "invalid legacy uploads owner {}: {err}",
            path.display()
        )))
    })
}

fn publish_legacy_uploads_owner(
    path: &Path,
    owner: &LegacyUploadsOwner,
) -> Result<(), EngineError> {
    let mut bytes = serde_json::to_vec_pretty(owner)
        .map_err(|err| EngineError::Other(format!("serialize legacy uploads owner: {err}")))?;
    bytes.push(b'\n');
    match crate::fsx::publish_new(path, &bytes)? {
        // Someone else claimed the legacy cache first; their file stands.
        crate::fsx::PublishOutcome::Created | crate::fsx::PublishOutcome::Exists => Ok(()),
    }
}

/// Filesystem-safe form of an org/user id used by the account-scoped layout.
fn sanitize_path_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_profile_id_is_stable_and_not_the_dev_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = EngineProfile::local(dir.path()).unwrap();
        let second = EngineProfile::local(dir.path()).unwrap();

        assert!(!first.user_id().is_empty());
        assert_ne!(first.user_id(), "dev-user");
        assert_eq!(first.user_id(), second.user_id());
        assert!(Uuid::parse_str(first.user_id()).is_ok());
    }

    #[test]
    fn local_profiles_in_different_data_dirs_have_different_ids() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();

        let first = EngineProfile::local(first_dir.path()).unwrap();
        let second = EngineProfile::local(second_dir.path()).unwrap();

        assert_ne!(first.user_id(), second.user_id());
    }

    #[test]
    fn local_profile_resolves_isolated_store_and_upload_roots() {
        let dir = tempfile::tempdir().unwrap();
        let profile = EngineProfile::local(dir.path()).unwrap();
        let local_root = dir.path().join("profiles/local");

        assert_eq!(profile.scope(), WorkspaceScope::Local);
        assert_eq!(profile.store_root(), local_root);
        assert_eq!(profile.uploads_root(), local_root.join("uploads"));
    }

    #[test]
    fn development_profile_uses_account_scoped_roots() {
        let dir = tempfile::tempdir().unwrap();
        let profile = EngineProfile::development(dir.path(), "org/example", "user@example.com");

        assert_eq!(profile.scope(), WorkspaceScope::Development);
        assert_eq!(
            profile.store_root(),
            dir.path().join("orgs/org_example/user_example_com")
        );
        assert_eq!(
            profile.uploads_root(),
            dir.path().join("orgs/org_example/user_example_com/uploads")
        );
    }

    #[test]
    fn legacy_uploads_are_claimed_by_only_one_account() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("uploads")).unwrap();
        let first = EngineProfile::development(dir.path(), "org-a", "user-a");
        let second = EngineProfile::development(dir.path(), "org-b", "user-b");

        assert_eq!(
            first.claim_legacy_uploads_root().unwrap(),
            Some(dir.path().join("uploads"))
        );
        assert_eq!(second.claim_legacy_uploads_root().unwrap(), None);
        assert_eq!(
            first.claim_legacy_uploads_root().unwrap(),
            Some(dir.path().join("uploads"))
        );
    }

    #[tokio::test]
    async fn profile_assembler_uses_the_resolved_local_roots() {
        let dir = tempfile::tempdir().unwrap();
        let profile = EngineProfile::local(dir.path()).unwrap();
        let core = crate::EngineCore::assemble_with_profile(
            profile,
            std::sync::Arc::new(crate::default_registry()),
            hearth_proto::HarnessId::Mock,
            None,
        )
        .unwrap();

        assert_eq!(core.workspace_scope(), WorkspaceScope::Local);
        assert_eq!(
            core.uploads.dir(),
            dir.path().join("profiles/local/uploads")
        );
        assert!(dir.path().join("profiles/local").is_dir());
        assert!(!dir.path().join("orgs/dev-org/dev-user").exists());
        core.shutdown().await;
    }

    #[tokio::test]
    async fn development_constructor_uses_account_scoped_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let org_id = crate::env_or("HEARTH_ORG_ID", crate::DEFAULT_ORG_ID);
        let user_id = crate::env_or("HEARTH_USER_ID", crate::DEFAULT_USER_ID);
        let expected = EngineProfile::development(dir.path(), &org_id, &user_id);
        let core = crate::EngineCore::assemble(
            dir.path(),
            std::sync::Arc::new(crate::default_registry()),
            hearth_proto::HarnessId::Mock,
            None,
        )
        .unwrap();

        assert_eq!(core.workspace_scope(), WorkspaceScope::Development);
        assert_eq!(core.uploads.dir(), expected.store_root().join("uploads"));
        assert!(expected.store_root().is_dir());
        if std::env::var("HEARTH_ORG_ID").is_err() && std::env::var("HEARTH_USER_ID").is_err() {
            assert_eq!(
                expected.store_root(),
                dir.path().join("orgs/dev-org/dev-user")
            );
        }
        core.shutdown().await;
    }
}
