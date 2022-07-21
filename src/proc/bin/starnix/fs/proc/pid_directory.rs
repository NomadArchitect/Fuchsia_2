// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use lazy_static::lazy_static;
use regex::Regex;
use std::sync::Arc;

use crate::auth::FsCred;
use crate::fs::*;
use crate::lock::Mutex;
use crate::mm::{ProcMapsFile, ProcStatFile};
use crate::task::{CurrentTask, Task, ThreadGroup};
use crate::types::*;

/// Creates an [`FsNode`] that represents the `/proc/<pid>` directory for `task`.
pub fn pid_directory(fs: &FileSystemHandle, task: &Arc<Task>) -> Arc<FsNode> {
    static_directory_builder_with_common_task_entries(fs, task)
        .add_node_entry(
            b"task",
            dynamic_directory(fs, TaskListDirectory { thread_group: task.thread_group.clone() }),
        )
        .build()
}

/// Creates an [`FsNode`] that represents the `/proc/<pid>/task/<tid>` directory for `task`.
fn tid_directory(fs: &FileSystemHandle, task: &Arc<Task>) -> Arc<FsNode> {
    static_directory_builder_with_common_task_entries(fs, task).build()
}

/// Creates a [`StaticDirectoryBuilder`] and pre-populates it with files that are present in both
/// `/pid/<pid>` and `/pid/<pid>/task/<tid>`.
fn static_directory_builder_with_common_task_entries<'a>(
    fs: &'a FileSystemHandle,
    task: &Arc<Task>,
) -> StaticDirectoryBuilder<'a> {
    StaticDirectoryBuilder::new(fs)
        .add_node_entry(b"exe", ExeSymlink::new(fs, task.clone()))
        .add_node_entry(b"fd", dynamic_directory(fs, FdDirectory { task: task.clone() }))
        .add_node_entry(b"fdinfo", dynamic_directory(fs, FdInfoDirectory { task: task.clone() }))
        .add_node_entry(b"maps", ProcMapsFile::new(fs, task.clone()))
        .add_node_entry(b"stat", ProcStatFile::new(fs, task.clone()))
        .add_node_entry(b"cmdline", CmdlineFile::new(fs, task.clone()))
        .add_node_entry(b"comm", CommFile::new(fs, task.clone()))
        .add_node_entry(b"attr", attr_directory(fs))
        .add_node_entry(b"ns", dynamic_directory(fs, NsDirectory))
}

/// Creates an [`FsNode`] that represents the `/proc/<pid>/attr` directory.
fn attr_directory(fs: &FileSystemHandle) -> Arc<FsNode> {
    StaticDirectoryBuilder::new(fs)
        // The `current` security context is, with selinux disabled, unconfined.
        .add_entry(b"current", ByteVecFile::new(b"unconfined\n".to_vec()), mode!(IFREG, 0o666))
        .build()
}

/// `FdDirectory` implements the directory listing operations for a `proc/<pid>/fd` directory.
///
/// Reading the directory returns a list of all the currently open file descriptors for the
/// associated task.
struct FdDirectory {
    task: Arc<Task>,
}

impl DirectoryDelegate for FdDirectory {
    fn list(&self, _fs: &Arc<FileSystem>) -> Result<Vec<DynamicDirectoryEntry>, Errno> {
        Ok(fds_to_directory_entries(self.task.files.get_all_fds()))
    }

    fn lookup(&self, fs: &Arc<FileSystem>, name: &FsStr) -> Result<Arc<FsNode>, Errno> {
        let fd = FdNumber::from_fs_str(name).map_err(|_| errno!(ENOENT))?;
        // Make sure that the file descriptor exists before creating the node.
        let _ = self.task.files.get(fd).map_err(|_| errno!(ENOENT))?;
        Ok(fs.create_node(
            FdSymlink::new(self.task.clone(), fd),
            FdSymlink::file_mode(),
            self.task.as_fscred(),
        ))
    }
}

const NS_ENTRIES: &'static [&'static str] = &[
    "cgroup",
    "ipc",
    "mnt",
    "net",
    "pid",
    "pid_for_children",
    "time",
    "time_for_children",
    "user",
    "uts",
];

/// /proc/[pid]/ns directory
struct NsDirectory;

impl DirectoryDelegate for NsDirectory {
    fn list(&self, _fs: &Arc<FileSystem>) -> Result<Vec<DynamicDirectoryEntry>, Errno> {
        // For each namespace, this contains a link to the current identifier of the given namespace
        // for the current task.
        Ok(NS_ENTRIES
            .iter()
            .map(|name| DynamicDirectoryEntry {
                entry_type: DirectoryEntryType::LNK,
                name: name.as_bytes().to_vec(),
                inode: None,
            })
            .collect())
    }

    fn lookup(&self, fs: &Arc<FileSystem>, name: &FsStr) -> Result<Arc<FsNode>, Errno> {
        // If name is a given namespace, link to the current identifier of the that namespace for
        // the current task.
        // If name is {namespace}:[id], get a file descriptor for the given namespace.

        let name = String::from_utf8(name.to_vec()).map_err(|_| ENOENT)?;
        let mut elements = name.split(':');
        let ns = elements.next().expect("name must not be empty");
        // The name doesn't starts with a known namespace.
        if !NS_ENTRIES.contains(&ns) {
            return error!(ENOENT);
        }
        if let Some(id) = elements.next() {
            // The name starts with {namespace}:, check that it matches {namespace}:[id]
            lazy_static! {
                static ref NS_IDENTIFIER_RE: Regex = Regex::new("^\\[[0-9]+\\]$").unwrap();
            }
            if NS_IDENTIFIER_RE.is_match(id) {
                // TODO(qsr): For now, returns an empty file. In the future, this should create a
                // reference to to correct namespace, and ensures it keeps it alive.
                Ok(fs.create_node_with_ops(
                    ByteVecFile::new(vec![]),
                    mode!(IFREG, 0o444),
                    FsCred::root(),
                ))
            } else {
                error!(ENOENT)
            }
        } else {
            // The name is {namespace}, link to the correct one of the current task.
            Ok(fs.create_node(
                Box::new(NsDirectoryEntry { name }),
                mode!(IFLNK, 0o7777),
                FsCred::root(),
            ))
        }
    }
}

/// An entry in the ns pseudo directory. For now, all namespace have the identifier 1.
struct NsDirectoryEntry {
    name: String,
}

impl FsNodeOps for NsDirectoryEntry {
    fs_node_impl_symlink!();

    fn readlink(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
    ) -> Result<SymlinkTarget, Errno> {
        Ok(SymlinkTarget::Path(format!("{}:[1]", self.name).as_bytes().to_vec()))
    }
}

/// `FdInfoDirectory` implements the directory listing operations for a `proc/<pid>/fdinfo`
/// directory.
///
/// Reading the directory returns a list of all the currently open file descriptors for the
/// associated task.
struct FdInfoDirectory {
    task: Arc<Task>,
}

impl DirectoryDelegate for FdInfoDirectory {
    fn list(&self, _fs: &Arc<FileSystem>) -> Result<Vec<DynamicDirectoryEntry>, Errno> {
        Ok(fds_to_directory_entries(self.task.files.get_all_fds()))
    }

    fn lookup(&self, fs: &Arc<FileSystem>, name: &FsStr) -> Result<Arc<FsNode>, Errno> {
        let fd = FdNumber::from_fs_str(name).map_err(|_| errno!(ENOENT))?;
        let file = self.task.files.get(fd).map_err(|_| errno!(ENOENT))?;
        let pos = *file.offset.lock();
        let flags = file.flags();
        let data = format!("pos:\t{}flags:\t0{:o}\n", pos, flags.bits()).into_bytes();
        Ok(fs.create_node_with_ops(ByteVecFile::new(data), mode!(IFREG, 0o444), FsCred::root()))
    }
}

fn fds_to_directory_entries(fds: Vec<FdNumber>) -> Vec<DynamicDirectoryEntry> {
    fds.into_iter()
        .map(|fd| DynamicDirectoryEntry {
            entry_type: DirectoryEntryType::DIR,
            name: fd.raw().to_string().into_bytes(),
            inode: None,
        })
        .collect()
}

/// Directory that lists the task IDs (tid) in a process. Located at `/proc/<pid>/task/`.
struct TaskListDirectory {
    thread_group: Arc<ThreadGroup>,
}

impl DirectoryDelegate for TaskListDirectory {
    fn list(&self, _fs: &Arc<FileSystem>) -> Result<Vec<DynamicDirectoryEntry>, Errno> {
        Ok(self
            .thread_group
            .read()
            .tasks
            .keys()
            .map(|tid| DynamicDirectoryEntry {
                entry_type: DirectoryEntryType::DIR,
                name: tid.to_string().into_bytes(),
                inode: None,
            })
            .collect())
    }

    fn lookup(&self, fs: &Arc<FileSystem>, name: &FsStr) -> Result<Arc<FsNode>, Errno> {
        let tid = std::str::from_utf8(name)
            .map_err(|_| errno!(ENOENT))?
            .parse::<pid_t>()
            .map_err(|_| errno!(ENOENT))?;
        // Make sure the tid belongs to this process.
        if !self.thread_group.read().tasks.contains_key(&tid) {
            return error!(ENOENT);
        }
        let task =
            self.thread_group.kernel.pids.read().get_task(tid).ok_or_else(|| errno!(ENOENT))?;
        Ok(tid_directory(fs, &task))
    }
}

/// An `ExeSymlink` points to the `executable_node` (the node that contains the task's binary) of
/// the task that reads it.
pub struct ExeSymlink {
    task: Arc<Task>,
}

impl ExeSymlink {
    fn new(fs: &FileSystemHandle, task: Arc<Task>) -> FsNodeHandle {
        fs.create_node_with_ops(ExeSymlink { task }, mode!(IFLNK, 0o777), FsCred::root())
    }
}

impl FsNodeOps for ExeSymlink {
    fs_node_impl_symlink!();

    fn readlink(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
    ) -> Result<SymlinkTarget, Errno> {
        if let Some(node) = self.task.mm.executable_node() {
            Ok(SymlinkTarget::Node(node))
        } else {
            error!(ENOENT)
        }
    }
}

/// `FdSymlink` is a symlink that points to a specific file descriptor in a task.
pub struct FdSymlink {
    /// The file descriptor that this symlink targets.
    fd: FdNumber,

    /// The task that `fd` is to be read from.
    task: Arc<Task>,
}

impl FdSymlink {
    fn new(task: Arc<Task>, fd: FdNumber) -> Box<dyn FsNodeOps> {
        Box::new(FdSymlink { fd, task })
    }

    fn file_mode() -> FileMode {
        mode!(IFLNK, 0o777)
    }
}

impl FsNodeOps for FdSymlink {
    fs_node_impl_symlink!();

    fn readlink(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
    ) -> Result<SymlinkTarget, Errno> {
        let file = self.task.files.get(self.fd).map_err(|_| errno!(ENOENT))?;
        Ok(SymlinkTarget::Node(file.name.clone()))
    }
}

/// `CmdlineFile` implements the `FsNodeOps` for a `proc/<pid>/cmdline` file.
pub struct CmdlineFile {
    /// The task from which the `CmdlineFile` fetches the command line parameters.
    task: Arc<Task>,
    seq: Mutex<SeqFileState<()>>,
}

impl CmdlineFile {
    fn new(fs: &FileSystemHandle, task: Arc<Task>) -> FsNodeHandle {
        fs.create_node_with_ops(
            SimpleFileNode::new(move || {
                Ok(CmdlineFile { task: Arc::clone(&task), seq: Mutex::new(SeqFileState::new()) })
            }),
            mode!(IFREG, 0o444),
            FsCred::root(),
        )
    }
}

impl FileOps for CmdlineFile {
    fileops_impl_seekable!();
    fileops_impl_nonblocking!();

    fn read_at(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let argv = self.task.read().argv.clone();
        let mut seq = self.seq.lock();
        let iter = move |_cursor, sink: &mut SeqFileBuf| {
            for arg in &argv {
                sink.write(arg.as_bytes_with_nul());
            }
            Ok(None)
        };
        seq.read_at(current_task, iter, offset, data)
    }

    fn write_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        Err(ENOSYS)
    }
}

/// `CommFile` implements the `FsNodeOps` for a `proc/<pid>/comm` file.
pub struct CommFile {
    /// The task from which the `CommFile` fetches the command line parameters.
    task: Arc<Task>,
    seq: Mutex<SeqFileState<()>>,
}

impl CommFile {
    fn new(fs: &FileSystemHandle, task: Arc<Task>) -> FsNodeHandle {
        fs.create_node_with_ops(
            SimpleFileNode::new(move || {
                Ok(CommFile { task: Arc::clone(&task), seq: Mutex::new(SeqFileState::new()) })
            }),
            mode!(IFREG, 0o444),
            FsCred::root(),
        )
    }
}

impl FileOps for CommFile {
    fileops_impl_seekable!();
    fileops_impl_nonblocking!();

    fn read_at(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        let comm = self.task.command();
        let mut seq = self.seq.lock();
        let iter = move |_cursor, sink: &mut SeqFileBuf| {
            sink.write(comm.as_bytes());
            sink.write(b"\n");
            Ok(None)
        };
        seq.read_at(current_task, iter, offset, data)
    }

    fn write_at(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &[UserBuffer],
    ) -> Result<usize, Errno> {
        Err(ENOSYS)
    }
}
