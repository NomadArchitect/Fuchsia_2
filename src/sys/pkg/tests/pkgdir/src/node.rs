// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::dirs_to_test,
    fidl_fuchsia_io::{DirectoryProxy, MODE_TYPE_DIRECTORY, MODE_TYPE_FILE, OPEN_RIGHT_READABLE},
    fuchsia_zircon as zx,
};

#[fuchsia::test]
async fn get_attr() {
    for dir in dirs_to_test().await {
        get_attr_per_package_source(dir).await
    }
}

trait U64Verifier {
    fn verify(&self, num: u64);
}

impl U64Verifier for u64 {
    fn verify(&self, num: u64) {
        assert_eq!(num, *self)
    }
}

struct AnyU64;
impl U64Verifier for AnyU64 {
    fn verify(&self, _num: u64) {}
}

struct PositiveU64;
impl U64Verifier for PositiveU64 {
    fn verify(&self, num: u64) {
        assert!(num > 0);
    }
}

async fn get_attr_per_package_source(root_dir: DirectoryProxy) {
    struct Args {
        open_flags: u32,
        open_mode: u32,
        expected_mode: u32,
        id_verifier: Box<dyn U64Verifier>,
        expected_content_size: u64,
        expected_storage_size: u64,
        time_verifier: Box<dyn U64Verifier>,
    }

    impl Default for Args {
        fn default() -> Self {
            Self {
                open_flags: 0,
                open_mode: 0,
                expected_mode: 0,
                id_verifier: Box::new(1),
                expected_content_size: 0,
                expected_storage_size: 0,
                time_verifier: Box::new(PositiveU64),
            }
        }
    }

    async fn verify_get_attrs(root_dir: &DirectoryProxy, path: &str, args: Args) {
        let node = io_util::directory::open_node(root_dir, path, args.open_flags, args.open_mode)
            .await
            .unwrap();
        let (status, attrs) = node.get_attr().await.unwrap();
        zx::Status::ok(status).unwrap();
        assert_eq!(attrs.mode, args.expected_mode);
        args.id_verifier.verify(attrs.id);
        assert_eq!(attrs.content_size, args.expected_content_size);
        assert_eq!(attrs.storage_size, args.expected_storage_size);
        assert_eq!(attrs.link_count, 1);
        args.time_verifier.verify(attrs.creation_time);
        args.time_verifier.verify(attrs.modification_time);
    }

    verify_get_attrs(
        &root_dir,
        ".",
        Args { expected_mode: MODE_TYPE_DIRECTORY | 0o755, ..Default::default() },
    )
    .await;
    verify_get_attrs(
        &root_dir,
        "dir",
        Args { expected_mode: MODE_TYPE_DIRECTORY | 0o755, ..Default::default() },
    )
    .await;
    verify_get_attrs(
        &root_dir,
        "file",
        Args {
            open_flags: OPEN_RIGHT_READABLE,
            expected_mode: MODE_TYPE_FILE | 0o500,
            id_verifier: Box::new(AnyU64),
            expected_content_size: 4,
            expected_storage_size: 8192,
            time_verifier: Box::new(0),
            ..Default::default()
        },
    )
    .await;
    verify_get_attrs(
        &root_dir,
        "meta",
        Args {
            open_mode: MODE_TYPE_FILE,
            expected_mode: MODE_TYPE_FILE | 0o644,
            expected_content_size: 64,
            expected_storage_size: 64,
            ..Default::default()
        },
    )
    .await;
    verify_get_attrs(
        &root_dir,
        "meta",
        Args {
            open_mode: MODE_TYPE_DIRECTORY,
            expected_mode: MODE_TYPE_DIRECTORY | 0o755,
            expected_content_size: 69,
            expected_storage_size: 69,
            ..Default::default()
        },
    )
    .await;
    verify_get_attrs(
        &root_dir,
        "meta/dir",
        Args {
            expected_mode: MODE_TYPE_DIRECTORY | 0o755,
            expected_content_size: 69,
            expected_storage_size: 69,
            ..Default::default()
        },
    )
    .await;
    verify_get_attrs(
        &root_dir,
        "meta/file",
        Args {
            expected_mode: MODE_TYPE_FILE | 0o644,
            expected_content_size: 9,
            expected_storage_size: 9,
            ..Default::default()
        },
    )
    .await;
}

#[fuchsia::test]
async fn close() {
    for dir in dirs_to_test().await {
        close_per_package_source(dir).await
    }
}

async fn close_per_package_source(root_dir: DirectoryProxy) {
    async fn verify_close(root_dir: &DirectoryProxy, path: &str, mode: u32) {
        let node =
            io_util::directory::open_node(root_dir, path, OPEN_RIGHT_READABLE, mode).await.unwrap();

        let _ = node.close().await.unwrap();

        matches::assert_matches!(
            node.close().await,
            Err(fidl::Error::ClientChannelClosed { status: zx::Status::PEER_CLOSED, .. })
        );
    }

    verify_close(&root_dir, ".", MODE_TYPE_DIRECTORY).await;
    verify_close(&root_dir, "dir", MODE_TYPE_DIRECTORY).await;
    verify_close(&root_dir, "meta", MODE_TYPE_DIRECTORY).await;
    verify_close(&root_dir, "meta/dir", MODE_TYPE_DIRECTORY).await;

    verify_close(&root_dir, "file", MODE_TYPE_FILE).await;
    verify_close(&root_dir, "meta/file", MODE_TYPE_FILE).await;
    verify_close(&root_dir, "meta", MODE_TYPE_FILE).await;
}
