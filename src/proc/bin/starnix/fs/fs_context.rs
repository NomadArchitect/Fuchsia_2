// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use crate::fs::*;
use crate::lock::RwLock;
use crate::types::*;

/// The mutable state for an FsContext.
///
/// This state is cloned in FsContext::fork.
#[derive(Clone)]
struct FsContextState {
    /// The namespace tree for this FsContext.
    ///
    /// This field owns the mount table for this FsContext.
    namespace: Arc<Namespace>,

    /// The current working directory.
    cwd: NamespaceNode,

    // See <https://man7.org/linux/man-pages/man2/umask.2.html>
    umask: FileMode,
}

/// The file system context associated with a task.
///
/// File system operations, such as opening a file or mounting a directory, are
/// performed using this context.
pub struct FsContext {
    /// The root of the namespace tree for this FsContext.
    ///
    /// Operations on the file system are typically either relative to this
    /// root or to the cwd().
    pub root: NamespaceNode,

    /// The mutable state for this FsContext.
    state: RwLock<FsContextState>,
}

impl FsContext {
    /// Create an FsContext for the given namespace.
    ///
    /// The root and cwd of the FsContext are initialized to the root of the
    /// namespace.
    pub fn new(root: FileSystemHandle) -> Arc<FsContext> {
        let namespace = Namespace::new(root);
        let root = namespace.root();
        Arc::new(FsContext {
            root: root.clone(),
            state: RwLock::new(FsContextState {
                namespace,
                cwd: root,
                umask: FileMode::DEFAULT_UMASK,
            }),
        })
    }

    pub fn fork(&self) -> Arc<FsContext> {
        // A child process created via fork(2) inherits its parent's umask.
        // The umask is left unchanged by execve(2).
        //
        // See <https://man7.org/linux/man-pages/man2/umask.2.html>

        Arc::new(FsContext {
            root: self.root.clone(),
            state: RwLock::new(self.state.read().clone()),
        })
    }

    /// Returns a reference to the current working directory.
    pub fn cwd(&self) -> NamespaceNode {
        let state = self.state.read();
        state.cwd.clone()
    }

    /// Change the current working directory.
    pub fn chdir(&self, name: NamespaceNode) {
        let mut state = self.state.write();
        state.cwd = name;
    }

    pub fn apply_umask(&self, mode: FileMode) -> FileMode {
        let umask = self.state.read().umask;
        mode & !umask
    }

    pub fn set_umask(&self, umask: FileMode) -> FileMode {
        let mut state = self.state.write();
        let old_umask = state.umask;

        // umask() sets the calling process's file mode creation mask
        // (umask) to mask & 0o777 (i.e., only the file permission bits of
        // mask are used), and returns the previous value of the mask.
        //
        // See <https://man7.org/linux/man-pages/man2/umask.2.html>
        state.umask = umask & FileMode::ALLOW_ALL;

        old_umask
    }

    pub fn namespace_root(&self) -> NamespaceNode {
        self.state.read().namespace.root()
    }

    pub fn unshare_namespace(&self) {
        let mut state = self.state.write();
        state.namespace = state.namespace.clone_namespace();
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::fs::tmpfs::TmpFs;
    use crate::testing::*;

    #[::fuchsia::test]
    fn test_umask() {
        let (kernel, _current_task) = create_kernel_and_task_with_pkgfs();
        let fs = FsContext::new(TmpFs::new(&kernel));

        assert_eq!(FileMode::from_bits(0o22), fs.set_umask(FileMode::from_bits(0o3020)));
        assert_eq!(FileMode::from_bits(0o646), fs.apply_umask(FileMode::from_bits(0o666)));
        assert_eq!(FileMode::from_bits(0o3646), fs.apply_umask(FileMode::from_bits(0o3666)));
        assert_eq!(FileMode::from_bits(0o20), fs.set_umask(FileMode::from_bits(0o11)));
    }

    #[::fuchsia::test]
    fn test_chdir() {
        let (_kernel, current_task) = create_kernel_and_task_with_pkgfs();

        assert_eq!(b"/".to_vec(), current_task.fs.cwd().path());

        let bin = current_task.open_file(b"bin", OpenFlags::RDONLY).expect("missing bin directory");
        current_task.fs.chdir(bin.name.clone());
        assert_eq!(b"/bin".to_vec(), current_task.fs.cwd().path());

        // Now that we have changed directories to bin, we're opening a file
        // relative to that directory, which doesn't exist.
        assert!(current_task.open_file(b"bin", OpenFlags::RDONLY).is_err());

        // However, bin still exists in the root directory.
        assert!(current_task.open_file(b"/bin", OpenFlags::RDONLY).is_ok());

        current_task.fs.chdir(
            current_task
                .open_file(b"..", OpenFlags::RDONLY)
                .expect("failed to open ..")
                .name
                .clone(),
        );
        assert_eq!(b"/".to_vec(), current_task.fs.cwd().path());

        // Now bin exists again because we've gone back to the root.
        assert!(current_task.open_file(b"bin", OpenFlags::RDONLY).is_ok());

        // Repeating the .. doesn't do anything because we're already at the root.
        current_task.fs.chdir(
            current_task
                .open_file(b"..", OpenFlags::RDONLY)
                .expect("failed to open ..")
                .name
                .clone(),
        );
        assert_eq!(b"/".to_vec(), current_task.fs.cwd().path());
        assert!(current_task.open_file(b"bin", OpenFlags::RDONLY).is_ok());
    }
}
