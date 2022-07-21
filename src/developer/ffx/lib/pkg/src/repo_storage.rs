// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::repo_keys::RepoKeys,
    anyhow::{Context as _, Result},
    chrono::{DateTime, Duration, NaiveDateTime, Utc},
    tuf::{interchange::Json, repository::RepositoryStorageProvider},
};

/// Number of days from now before the root metadata is expired.
const DEFAULTROOT_EXPIRATION: i64 = 365;

/// Number of days from now before the targets metadata is expired.
const DEFAULT_TARGETS_EXPIRATION: i64 = 90;

/// Number of days from now before the snapshot metadata is expired.
const DEFAULT_SNAPSHOT_EXPIRATION: i64 = 30;

/// Number of days from now before the timestamp metadata is expired.
const DEFAULT_TIMESTAMP_EXPIRATION: i64 = 30;

pub trait RepoStorage {
    fn get_tuf_repo_storage(
        &self,
    ) -> Result<Box<dyn RepositoryStorageProvider<Json> + Send + Sync>>;
}

pub async fn refresh_repository(repo: &dyn RepoStorage, repo_keys: &RepoKeys) -> Result<()> {
    let tuf_repo = repo.get_tuf_repo_storage()?;
    let mut tuf_client = crate::repository::get_tuf_client(tuf_repo).await?;

    // Download the metadata. We'll use the unix epoch for the update timestamp so we can work with
    // expired metadata.
    let start_time = DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(0, 0), Utc);
    tuf_client.update_with_start_time(&start_time).await?;
    let parts = tuf_client.into_parts();

    // Create a repo builder for the metadata, and initialize it with our repository keys.
    let mut repo_builder =
        tuf::repo_builder::RepoBuilder::from_database(parts.remote, &parts.database);

    for key in repo_keys.root_keys() {
        repo_builder = repo_builder.trusted_root_keys(&[&**key]);
    }

    for key in repo_keys.targets_keys() {
        repo_builder = repo_builder.trusted_targets_keys(&[&**key]);
    }

    for key in repo_keys.snapshot_keys() {
        repo_builder = repo_builder.trusted_snapshot_keys(&[&**key]);
    }

    for key in repo_keys.timestamp_keys() {
        repo_builder = repo_builder.trusted_timestamp_keys(&[&**key]);
    }

    // Update all the expiration for all the metadata we have keys for.
    let now = Utc::now();

    let repo_builder = if repo_keys.root_keys().is_empty() {
        repo_builder.skip_root()
    } else {
        repo_builder
            .stage_root_with_builder(|b| b.expires(now + Duration::days(DEFAULTROOT_EXPIRATION)))?
    };

    let repo_builder = if repo_keys.targets_keys().is_empty() {
        repo_builder.skip_targets()
    } else {
        repo_builder.stage_targets_with_builder(|b| {
            b.expires(now + Duration::days(DEFAULT_TARGETS_EXPIRATION))
        })?
    };

    let repo_builder = if repo_keys.snapshot_keys().is_empty() {
        repo_builder.skip_snapshot()
    } else {
        repo_builder.stage_snapshot_with_builder(|b| {
            b.expires(now + Duration::days(DEFAULT_SNAPSHOT_EXPIRATION))
        })?
    };

    let repo_builder = if repo_keys.timestamp_keys().is_empty() {
        repo_builder.skip_timestamp()
    } else {
        repo_builder.stage_timestamp_with_builder(|b| {
            b.expires(now + Duration::days(DEFAULT_TIMESTAMP_EXPIRATION))
        })?
    };

    repo_builder.commit().await.context("publishing metadata")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            repository::{get_tuf_client, PmRepository, RepositoryBackend as _},
            test_utils,
        },
        assert_matches::assert_matches,
        camino::Utf8Path,
    };

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_refresh_metadata_with_all_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        // Load up the test metadata, which was created some time ago, and has a different
        // expiration date.
        test_utils::make_repo_dir(&dir).unwrap();
        let repo = PmRepository::new(dir.to_path_buf());

        // Download the older metadata before we refresh it.
        let tuf_repo = repo.get_tuf_repo().unwrap();
        let mut tuf_client = get_tuf_client(tuf_repo).await.unwrap();
        assert_matches!(tuf_client.update().await, Ok(true));

        let root1 = (*tuf_client.database().trusted_root()).clone();
        let targets1 = tuf_client.database().trusted_targets().cloned().unwrap();
        let snapshot1 = tuf_client.database().trusted_snapshot().cloned().unwrap();
        let timestamp1 = tuf_client.database().trusted_timestamp().cloned().unwrap();

        // Update the metadata expiration.
        let keys = RepoKeys::from_dir(&dir.join("keys").into_std_path_buf()).unwrap();
        refresh_repository(&repo, &keys).await.unwrap();

        // Finally, make sure the metadata has changed.
        assert_matches!(tuf_client.update().await, Ok(true));

        let root2 = (*tuf_client.database().trusted_root()).clone();
        let targets2 = tuf_client.database().trusted_targets().cloned().unwrap();
        let snapshot2 = tuf_client.database().trusted_snapshot().cloned().unwrap();
        let timestamp2 = tuf_client.database().trusted_timestamp().cloned().unwrap();

        // Make sure we generated new metadata.
        assert_ne!(root1, root2);
        assert_ne!(targets1, targets2);
        assert_ne!(snapshot1, snapshot2);
        assert_ne!(timestamp1, timestamp2);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_refresh_metadata_with_some_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        // Load the repo.
        test_utils::make_repo_dir(&dir).unwrap();
        let repo = PmRepository::new(dir.to_path_buf());

        // Download the older metadata before we refresh it.
        let tuf_repo = repo.get_tuf_repo().unwrap();
        let mut tuf_client = get_tuf_client(tuf_repo).await.unwrap();
        assert_matches!(tuf_client.update().await, Ok(true));

        let root1 = (*tuf_client.database().trusted_root()).clone();
        let targets1 = tuf_client.database().trusted_targets().cloned().unwrap();
        let snapshot1 = tuf_client.database().trusted_snapshot().cloned().unwrap();
        let timestamp1 = tuf_client.database().trusted_timestamp().cloned().unwrap();

        // Load the repo, but delete the root private key file.
        let keys_dir = dir.join("keys");
        std::fs::remove_file(keys_dir.join("root.json")).unwrap();

        // Update the metadata expiration.
        let keys = RepoKeys::from_dir(&dir.join("keys").into_std_path_buf()).unwrap();
        refresh_repository(&repo, &keys).await.unwrap();

        // Make sure the metadata has changed.
        assert_matches!(tuf_client.update().await, Ok(true));

        let root2 = (*tuf_client.database().trusted_root()).clone();
        let targets2 = tuf_client.database().trusted_targets().cloned().unwrap();
        let snapshot2 = tuf_client.database().trusted_snapshot().cloned().unwrap();
        let timestamp2 = tuf_client.database().trusted_timestamp().cloned().unwrap();

        // Make sure we generated new metadata, except for the root metadata.
        assert_eq!(root1, root2);
        assert_ne!(targets1, targets2);
        assert_ne!(snapshot1, snapshot2);
        assert_ne!(timestamp1, timestamp2);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_refresh_metadata_with_no_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        // Load the repo.
        test_utils::make_repo_dir(&dir).unwrap();
        let repo = PmRepository::new(dir.to_path_buf());

        // Download the older metadata before we refresh it.
        let tuf_repo = repo.get_tuf_repo().unwrap();
        let mut tuf_client = get_tuf_client(tuf_repo).await.unwrap();
        tuf_client.update().await.unwrap();

        let root1 = (*tuf_client.database().trusted_root()).clone();
        let targets1 = tuf_client.database().trusted_targets().cloned().unwrap();
        let snapshot1 = tuf_client.database().trusted_snapshot().cloned().unwrap();
        let timestamp1 = tuf_client.database().trusted_timestamp().cloned().unwrap();

        // Try to refresh the metadata with an empty key set, which should do nothing.
        let keys = RepoKeys::builder().build();
        refresh_repository(&repo, &keys).await.unwrap();

        // Updating the client should return that there were no changes.
        assert_matches!(tuf_client.update().await, Ok(false));

        let root2 = (*tuf_client.database().trusted_root()).clone();
        let targets2 = tuf_client.database().trusted_targets().cloned().unwrap();
        let snapshot2 = tuf_client.database().trusted_snapshot().cloned().unwrap();
        let timestamp2 = tuf_client.database().trusted_timestamp().cloned().unwrap();

        // We should not have changed the metadata.
        assert_eq!(root1, root2);
        assert_eq!(targets1, targets2);
        assert_eq!(snapshot1, snapshot2);
        assert_eq!(timestamp1, timestamp2);
    }
}
