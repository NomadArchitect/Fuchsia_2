// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Utility functions for fuchsia.io directories.

use {
    crate::node::{self, CloneError, CloseError, OpenError},
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_io::{
        DirectoryMarker, DirectoryProxy, FileMarker, FileProxy, NodeMarker, NodeProxy,
    },
    fuchsia_zircon_status as zx_status,
};

/// Opens the given `path` from the current namespace as a [`DirectoryProxy`]. The target is not
/// verified to be any particular type and may not implement the fuchsia.io.Directory protocol.
#[cfg(target_os = "fuchsia")]
pub fn open_in_namespace(path: &str, flags: u32) -> Result<DirectoryProxy, OpenError> {
    let (dir, server_end) =
        fidl::endpoints::create_proxy::<DirectoryMarker>().map_err(OpenError::CreateProxy)?;

    let flags = flags | fidl_fuchsia_io::OPEN_FLAG_DIRECTORY;
    node::connect_in_namespace(path, flags, server_end.into_channel())
        .map_err(OpenError::Namespace)?;

    Ok(dir)
}

/// Opens the given `path` from the given `parent` directory as a [`DirectoryProxy`]. The target is
/// not verified to be any particular type and may not implement the fuchsia.io.Directory protocol.
pub fn open_directory_no_describe(
    parent: &DirectoryProxy,
    path: &str,
    flags: u32,
) -> Result<DirectoryProxy, OpenError> {
    let (dir, server_end) =
        fidl::endpoints::create_proxy::<DirectoryMarker>().map_err(OpenError::CreateProxy)?;

    let flags = flags | fidl_fuchsia_io::OPEN_FLAG_DIRECTORY;
    let mode = fidl_fuchsia_io::MODE_TYPE_DIRECTORY;

    parent
        .open(flags, mode, path, ServerEnd::new(server_end.into_channel()))
        .map_err(OpenError::SendOpenRequest)?;

    Ok(dir)
}

/// Opens the given `path` from given `parent` directory as a [`DirectoryProxy`], verifying that
/// the target implements the fuchsia.io.Directory protocol.
pub async fn open_directory(
    parent: &DirectoryProxy,
    path: &str,
    flags: u32,
) -> Result<DirectoryProxy, OpenError> {
    let (dir, server_end) =
        fidl::endpoints::create_proxy::<DirectoryMarker>().map_err(OpenError::CreateProxy)?;

    let flags = flags | fidl_fuchsia_io::OPEN_FLAG_DIRECTORY | fidl_fuchsia_io::OPEN_FLAG_DESCRIBE;
    let mode = fidl_fuchsia_io::MODE_TYPE_DIRECTORY;

    parent
        .open(flags, mode, path, ServerEnd::new(server_end.into_channel()))
        .map_err(OpenError::SendOpenRequest)?;

    // wait for the directory to open and report success.
    node::verify_directory_describe_event(dir).await
}

/// Creates a directory named `path` within the `parent` directory.
pub async fn create_directory(
    parent: &DirectoryProxy,
    path: &str,
    flags: u32,
) -> Result<DirectoryProxy, OpenError> {
    let (dir, server_end) =
        fidl::endpoints::create_proxy::<DirectoryMarker>().map_err(OpenError::CreateProxy)?;

    // NB: POSIX does not allow open(2) to create dirs, but fuchsia.io does not have an equivalent
    // of mkdir(2), so on Fuchsia we're expected to call open on a DirectoryMarker with (flags &
    // OPEN_FLAG_CREATE) set.
    // (mode & MODE_TYPE_DIRECTORY) is also required, although it is redundant (the fact that we
    // opened a DirectoryMarker is the main way that the underlying filesystem understands our
    // intention.)
    let flags = flags
        | fidl_fuchsia_io::OPEN_FLAG_CREATE
        | fidl_fuchsia_io::OPEN_FLAG_DIRECTORY
        | fidl_fuchsia_io::OPEN_FLAG_DESCRIBE;
    let mode = fidl_fuchsia_io::MODE_TYPE_DIRECTORY;

    parent
        .open(flags, mode, path, ServerEnd::new(server_end.into_channel()))
        .map_err(OpenError::SendOpenRequest)?;

    // wait for the directory to open and report success.
    node::verify_directory_describe_event(dir).await
}

/// Opens the given `path` from the given `parent` directory as a [`FileProxy`]. The target is not
/// verified to be any particular type and may not implement the fuchsia.io.File protocol.
pub fn open_file_no_describe(
    parent: &DirectoryProxy,
    path: &str,
    flags: u32,
) -> Result<FileProxy, OpenError> {
    let (file, server_end) =
        fidl::endpoints::create_proxy::<FileMarker>().map_err(OpenError::CreateProxy)?;

    let mode = fidl_fuchsia_io::MODE_TYPE_FILE;

    parent
        .open(flags, mode, path, ServerEnd::new(server_end.into_channel()))
        .map_err(OpenError::SendOpenRequest)?;

    Ok(file)
}

/// Opens the given `path` from given `parent` directory as a [`FileProxy`], verifying that the
/// target implements the fuchsia.io.File protocol.
pub async fn open_file(
    parent: &DirectoryProxy,
    path: &str,
    flags: u32,
) -> Result<FileProxy, OpenError> {
    let (file, server_end) =
        fidl::endpoints::create_proxy::<FileMarker>().map_err(OpenError::CreateProxy)?;

    let flags = flags | fidl_fuchsia_io::OPEN_FLAG_DESCRIBE;
    let mode = fidl_fuchsia_io::MODE_TYPE_FILE;

    parent
        .open(flags, mode, path, ServerEnd::new(server_end.into_channel()))
        .map_err(OpenError::SendOpenRequest)?;

    // wait for the file to open and report success.
    node::verify_file_describe_event(file).await
}

/// Opens the given `path` from the given `parent` directory as a [`NodeProxy`], verifying that the
/// target implements the fuchsia.io.Node protocol.
pub async fn open_node(
    parent: &DirectoryProxy,
    path: &str,
    flags: u32,
    mode: u32,
) -> Result<NodeProxy, OpenError> {
    let (file, server_end) =
        fidl::endpoints::create_proxy::<NodeMarker>().map_err(OpenError::CreateProxy)?;

    let flags = flags | fidl_fuchsia_io::OPEN_FLAG_DESCRIBE;

    parent
        .open(flags, mode, path, ServerEnd::new(server_end.into_channel()))
        .map_err(OpenError::SendOpenRequest)?;

    // wait for the file to open and report success.
    node::verify_node_describe_event(file).await
}

/// Opens the given `path` from the given `parent` directory as a [`NodeProxy`]. The target is not
/// verified to be any particular type and may not implement the fuchsia.io.Node protocol.
pub fn open_node_no_describe(
    parent: &DirectoryProxy,
    path: &str,
    flags: u32,
    mode: u32,
) -> Result<NodeProxy, OpenError> {
    let (file, server_end) =
        fidl::endpoints::create_proxy::<NodeMarker>().map_err(OpenError::CreateProxy)?;

    parent
        .open(flags, mode, path, ServerEnd::new(server_end.into_channel()))
        .map_err(OpenError::SendOpenRequest)?;

    Ok(file)
}

/// Opens a new connection to the given directory using `flags` if provided, or
/// `fidl_fuchsia_io::OPEN_FLAG_SAME_RIGHTS` otherwise.
pub fn clone_no_describe(
    dir: &DirectoryProxy,
    flags: Option<u32>,
) -> Result<DirectoryProxy, CloneError> {
    let (clone, server_end) = fidl::endpoints::create_proxy().map_err(CloneError::CreateProxy)?;
    clone_onto_no_describe(dir, flags, server_end)?;
    Ok(clone)
}

/// Opens a new connection to the given directory onto the given server end using `flags` if
/// provided, or `fidl_fuchsia_io::OPEN_FLAG_SAME_RIGHTS` otherwise.
pub fn clone_onto_no_describe(
    dir: &DirectoryProxy,
    flags: Option<u32>,
    request: ServerEnd<DirectoryMarker>,
) -> Result<(), CloneError> {
    let node_request = ServerEnd::new(request.into_channel());
    let flags = flags.unwrap_or(fidl_fuchsia_io::CLONE_FLAG_SAME_RIGHTS);

    dir.clone(flags, node_request).map_err(CloneError::SendCloneRequest)?;
    Ok(())
}

/// Gracefully closes the directory proxy from the remote end.
pub async fn close(dir: DirectoryProxy) -> Result<(), CloseError> {
    let status = dir.close().await.map_err(CloseError::SendCloseRequest)?;
    zx_status::Status::ok(status).map_err(CloseError::CloseError)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        fidl_fuchsia_io as fio,
        fidl_fuchsia_io::{
            OPEN_FLAG_CREATE, OPEN_FLAG_CREATE_IF_ABSENT, OPEN_RIGHT_READABLE, OPEN_RIGHT_WRITABLE,
        },
        fuchsia_async as fasync,
        futures::prelude::*,
        matches::assert_matches,
        tempfile::TempDir,
        vfs::{
            directory::entry::DirectoryEntry,
            execution_scope::ExecutionScope,
            file::vmo::{read_only_static, read_write, simple_init_vmo_with_capacity, write_only},
            pseudo_directory,
        },
    };

    const DATA_FILE_CONTENTS: &str = "Hello World!\n";

    fn open_pkg() -> DirectoryProxy {
        open_in_namespace("/pkg", OPEN_RIGHT_READABLE).unwrap()
    }

    fn open_tmp() -> (TempDir, DirectoryProxy) {
        let tempdir = TempDir::new().expect("failed to create tmp dir");
        let proxy = open_in_namespace(
            tempdir.path().to_str().unwrap(),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .unwrap();
        (tempdir, proxy)
    }

    // open_in_namespace

    #[fasync::run_singlethreaded(test)]
    async fn open_in_namespace_opens_real_dir() {
        let exists = open_in_namespace("/pkg", OPEN_RIGHT_READABLE).unwrap();
        assert_matches!(close(exists).await, Ok(()));
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_in_namespace_opens_fake_subdir_of_root_namespace_entry() {
        let notfound = open_in_namespace("/pkg/fake", OPEN_RIGHT_READABLE).unwrap();
        // The open error is not detected until the proxy is interacted with.
        assert_matches!(close(notfound).await, Err(_));
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_in_namespace_rejects_fake_root_namespace_entry() {
        assert_matches!(
            open_in_namespace("/fake", OPEN_RIGHT_READABLE),
            Err(OpenError::Namespace(zx_status::Status::NOT_FOUND))
        );
    }

    // open_directory_no_describe

    #[fasync::run_singlethreaded(test)]
    async fn open_directory_no_describe_opens_real_dir() {
        let pkg = open_pkg();
        let data = open_directory_no_describe(&pkg, "data", OPEN_RIGHT_READABLE).unwrap();
        close(data).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_directory_no_describe_opens_fake_dir() {
        let pkg = open_pkg();
        let fake = open_directory_no_describe(&pkg, "fake", OPEN_RIGHT_READABLE).unwrap();
        // The open error is not detected until the proxy is interacted with.
        assert_matches!(close(fake).await, Err(_));
    }

    // open_directory

    #[fasync::run_singlethreaded(test)]
    async fn open_directory_opens_real_dir() {
        let pkg = open_pkg();
        let data = open_directory(&pkg, "data", OPEN_RIGHT_READABLE).await.unwrap();
        close(data).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_directory_rejects_fake_dir() {
        let pkg = open_pkg();

        assert_matches!(
            open_directory(&pkg, "fake", OPEN_RIGHT_READABLE).await,
            Err(OpenError::OpenError(zx_status::Status::NOT_FOUND))
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_directory_rejects_file() {
        let pkg = open_pkg();

        assert_matches!(
            open_directory(&pkg, "data/file", OPEN_RIGHT_READABLE).await,
            Err(OpenError::OpenError(zx_status::Status::NOT_DIR))
        );
    }

    // create_directory

    #[fasync::run_singlethreaded(test)]
    async fn create_directory_simple() {
        let (_tmp, proxy) = open_tmp();
        let dir = create_directory(&proxy, "dir", OPEN_RIGHT_READABLE).await.unwrap();
        crate::directory::close(dir).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn create_directory_add_file() {
        let (_tmp, proxy) = open_tmp();
        let dir = create_directory(&proxy, "dir", OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE)
            .await
            .unwrap();
        let file = open_file(
            &dir,
            "data",
            OPEN_FLAG_CREATE | OPEN_FLAG_CREATE_IF_ABSENT | OPEN_RIGHT_READABLE,
        )
        .await
        .unwrap();
        crate::file::close(file).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn create_directory_existing_dir_opens() {
        let (_tmp, proxy) = open_tmp();
        let dir = create_directory(&proxy, "dir", OPEN_RIGHT_READABLE).await.unwrap();
        crate::directory::close(dir).await.unwrap();
        create_directory(&proxy, "dir", OPEN_RIGHT_READABLE).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn create_directory_existing_dir_fails_if_flag_set() {
        let (_tmp, proxy) = open_tmp();
        let dir = create_directory(&proxy, "dir", OPEN_FLAG_CREATE_IF_ABSENT | OPEN_RIGHT_READABLE)
            .await
            .unwrap();
        crate::directory::close(dir).await.unwrap();
        assert_matches!(
            create_directory(&proxy, "dir", OPEN_FLAG_CREATE_IF_ABSENT | OPEN_RIGHT_READABLE).await,
            Err(_)
        );
    }

    // open_file_no_describe

    #[fasync::run_singlethreaded(test)]
    async fn open_file_no_describe_opens_real_file() {
        let pkg = open_pkg();
        let file = open_file_no_describe(&pkg, "data/file", OPEN_RIGHT_READABLE).unwrap();
        crate::file::close(file).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_file_no_describe_opens_fake_file() {
        let pkg = open_pkg();
        let fake = open_file_no_describe(&pkg, "data/fake", OPEN_RIGHT_READABLE).unwrap();
        // The open error is not detected until the proxy is interacted with.
        assert_matches!(crate::file::close(fake).await, Err(_));
    }

    // open_file

    #[fasync::run_singlethreaded(test)]
    async fn open_file_opens_real_file() {
        let pkg = open_pkg();
        let file = open_file(&pkg, "data/file", OPEN_RIGHT_READABLE).await.unwrap();
        assert_eq!(
            file.seek(0, fidl_fuchsia_io::SeekOrigin::End).await.unwrap(),
            (zx_status::Status::OK.into_raw(), DATA_FILE_CONTENTS.len() as u64)
        );
        crate::file::close(file).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_file_rejects_fake_file() {
        let pkg = open_pkg();

        assert_matches!(
            open_file(&pkg, "data/fake", OPEN_RIGHT_READABLE).await,
            Err(OpenError::OpenError(zx_status::Status::NOT_FOUND))
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_file_rejects_dir() {
        let pkg = open_pkg();

        assert_matches!(
            open_file(&pkg, "data", OPEN_RIGHT_READABLE).await,
            Err(OpenError::UnexpectedNodeKind {
                expected: node::Kind::File,
                actual: node::Kind::Directory,
            })
        );
    }

    #[fasync::run_until_stalled(test)]
    async fn open_file_flags() {
        let example_dir = pseudo_directory! {
            "read_only" => read_only_static("read_only"),
            "read_write" => read_write(
                simple_init_vmo_with_capacity("read_write".as_bytes(), 100),
                |_| future::ready(()),
            ),
            "write_only" => write_only(simple_init_vmo_with_capacity(&[], 100), |_| future::ready(())),
        };
        let (example_dir_proxy, example_dir_service) =
            fidl::endpoints::create_proxy::<DirectoryMarker>().unwrap();
        let scope = ExecutionScope::new();
        example_dir.open(
            scope,
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            fidl_fuchsia_io::MODE_TYPE_DIRECTORY,
            vfs::path::Path::empty(),
            ServerEnd::new(example_dir_service.into_channel()),
        );

        for (file_name, flags, should_succeed) in vec![
            ("read_only", OPEN_RIGHT_READABLE, true),
            ("read_only", OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE, false),
            ("read_only", OPEN_RIGHT_WRITABLE, false),
            ("read_write", OPEN_RIGHT_READABLE, true),
            ("read_write", OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE, true),
            ("read_write", OPEN_RIGHT_WRITABLE, true),
            ("write_only", OPEN_RIGHT_READABLE, false),
            ("write_only", OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE, false),
            ("write_only", OPEN_RIGHT_WRITABLE, true),
        ] {
            // open_file_no_describe

            let file = open_file_no_describe(&example_dir_proxy, file_name, flags).unwrap();
            match (should_succeed, file.describe().await) {
                (true, Ok(_)) => (),
                (false, Err(_)) => continue,
                (true, Err(e)) => {
                    panic!("failed to open when expected success, couldn't describe: {:?}", e)
                }
                (false, Ok(d)) => {
                    panic!("successfully opened when expected failure, could describe: {:?}", d)
                }
            }
            if flags & OPEN_RIGHT_READABLE != 0 {
                assert_eq!(crate::file::read_to_string(&file).await.unwrap(), file_name);
            }
            if flags & OPEN_RIGHT_WRITABLE != 0 {
                let (s, _) = file.write(b"write_only").await.unwrap();
                assert_eq!(zx_status::Status::ok(s), Ok(()));
            }
            crate::file::close(file).await.unwrap();

            // open_file

            match open_file(&example_dir_proxy, file_name, flags).await {
                Ok(file) if should_succeed => {
                    if flags & OPEN_RIGHT_READABLE != 0 {
                        assert_eq!(crate::file::read_to_string(&file).await.unwrap(), file_name);
                    }
                    if flags & OPEN_RIGHT_WRITABLE != 0 {
                        let (s, _) = file.write(b"write_only").await.unwrap();
                        assert_eq!(zx_status::Status::ok(s), Ok(()));
                    }
                    crate::file::close(file).await.unwrap();
                }
                Ok(_) => {
                    panic!("successfully opened when expected failure: {:?}", (file_name, flags))
                }
                Err(e) if should_succeed => {
                    panic!("failed to open when expected success: {:?}", (e, file_name, flags))
                }
                Err(_) => {}
            }
        }
    }

    // open_node_no_describe

    #[fasync::run_singlethreaded(test)]
    async fn open_node_no_describe_opens_real_node() {
        let pkg = open_pkg();
        let node = open_node_no_describe(&pkg, "data", OPEN_RIGHT_READABLE, 0).unwrap();
        crate::node::close(node).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_node_no_describe_opens_fake_node() {
        let pkg = open_pkg();
        let fake = open_node_no_describe(&pkg, "fake", OPEN_RIGHT_READABLE, 0).unwrap();
        // The open error is not detected until the proxy is interacted with.
        assert_matches!(crate::node::close(fake).await, Err(_));
    }

    // open_node

    #[fasync::run_singlethreaded(test)]
    async fn open_node_opens_real_node() {
        let pkg = open_pkg();
        let node = open_node(&pkg, "data", OPEN_RIGHT_READABLE, 0).await.unwrap();
        crate::node::close(node).await.unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_node_opens_fake_node() {
        let pkg = open_pkg();
        // The open error should be detected immediately.
        assert_matches!(open_node(&pkg, "fake", OPEN_RIGHT_READABLE, 0).await, Err(_));
    }

    #[fasync::run_singlethreaded(test)]
    async fn open_node_opens_service_node() {
        let svc = open_in_namespace("/svc", OPEN_RIGHT_READABLE).unwrap();
        let _node =
            open_node(&svc, "fuchsia.logger.LogSink", OPEN_RIGHT_READABLE, 0).await.unwrap();
        // Closing the node will hang forever, so don't bother.
    }

    // clone_no_describe

    #[fasync::run_singlethreaded(test)]
    async fn clone_no_describe_no_flags_same_rights() {
        let (dir, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<DirectoryMarker>().unwrap();

        clone_no_describe(&dir, None).unwrap();

        assert_matches!(
            stream.next().await,
            Some(Ok(fio::DirectoryRequest::Clone {
                flags: fidl_fuchsia_io::CLONE_FLAG_SAME_RIGHTS,
                ..
            }))
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn clone_no_describe_flags_passed_through() {
        let (dir, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<DirectoryMarker>().unwrap();

        clone_no_describe(&dir, Some(42)).unwrap();

        assert_matches!(
            stream.next().await,
            Some(Ok(fio::DirectoryRequest::Clone { flags: 42, .. }))
        );
    }
}
