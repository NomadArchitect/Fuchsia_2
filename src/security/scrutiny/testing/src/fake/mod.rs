// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::TEST_REPO_URL,
    fuchsia_url::{AbsolutePackageUrl, PackageName, UnpinnedAbsolutePackageUrl},
    scrutiny::model::model::DataModel,
    scrutiny_config::ModelConfig,
    std::{str::FromStr, sync::Arc},
    tempfile::tempdir,
};

/// Creates a simple fake model configuration that uses an in memory uri and
/// tempdata() directories for the required build locations.
pub fn fake_model_config() -> ModelConfig {
    let dir_path = tempdir().unwrap().into_path();
    let update_package_path = dir_path.join("update.far");
    let blobfs_paths = vec![dir_path.join("blob.blk"), dir_path.join("update.blob.blk")];
    ModelConfig {
        uri: "{memory}".to_string(),
        build_path: dir_path.clone(),
        update_package_path,
        blobfs_paths,
        config_data_package_url: AbsolutePackageUrl::Unpinned(UnpinnedAbsolutePackageUrl::new(
            TEST_REPO_URL.clone(),
            PackageName::from_str("config-data").unwrap(),
            None,
        )),
        devmgr_config_path: "config/devmgr".into(),
        component_tree_config_path: None,
        tmp_dir_path: None,
    }
}

/// Constructs a simple fake data model with an in memory uri and tempdata()
/// build directory.
pub fn fake_data_model() -> Arc<DataModel> {
    Arc::new(DataModel::connect(fake_model_config()).unwrap())
}
