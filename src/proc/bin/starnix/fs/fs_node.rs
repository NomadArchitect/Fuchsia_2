// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use once_cell::sync::OnceCell;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use fuchsia_zircon::Time;

use super::{FileOps, ObserverList};
use crate::devices::DeviceHandle;
use crate::fs::*;
use crate::syscalls::system::time_to_timespec;
use crate::types::*;

pub struct FsNode {
    /// The FsNodeOps for this FsNode.
    ///
    /// The FsNodeOps are implemented by the individual file systems to provide
    /// specific behaviors for this FsNode.
    ///
    /// The FsNode is created with this OnceCell empty in order to hold the
    /// slot in the children map during initialization. After initialization,
    /// this OnceCell is populated with the FsNodeOps returned by the parent.
    ops: Option<Box<dyn FsNodeOps>>,

    /// The tasks waiting on signals (e.g., POLLIN, POLLOUT) from this FsNode.
    pub observers: ObserverList,

    // TODO: replace with superblock handle
    device: DeviceHandle,

    /// The parent FsNode.
    ///
    /// The FsNode tree has strong references from child-to-parent and weak
    /// references from parent-to-child. This design ensures that the parent
    /// chain is always populated in the cache, but some children might be
    /// missing from the cache.
    parent: Option<FsNodeHandle>,

    /// The name that this parent calls this child.
    ///
    /// This name might not be reflected in the full path in the namespace that
    /// contains this FsNode. For example, this FsNode might be the root of a
    /// chroot.
    ///
    /// Most callers that want to work with names for FsNodes should use the
    /// NamespaceNodes.
    local_name: FsString,

    /// Mutable informationa about this node.
    ///
    /// This data is used to populate the stat_t structure.
    info: RwLock<FsNodeInfo>,

    /// A partial cache of the children of this node.
    ///
    /// FsNodes are added to this cache when they are looked up and removed
    /// when they are no longer referenced.
    ///
    /// This may include empty OnceCells for nodes that are in the process of being initialized. If
    /// initialization fails, the nodes will be dropped from the cache.
    children: RwLock<HashMap<FsString, OnceCell<Weak<FsNode>>>>,
}

pub type FsNodeHandle = Arc<FsNode>;

#[derive(Default)]
pub struct FsNodeInfo {
    pub inode_num: ino_t,
    pub mode: FileMode,
    pub size: usize,
    pub storage_size: usize,
    pub blksize: usize,
    pub uid: uid_t,
    pub gid: gid_t,
    pub link_count: u64,
    pub time_create: Time,
    pub time_access: Time,
    pub time_modify: Time,
    pub dev: dev_t,
    pub rdev: dev_t,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnlinkKind {
    /// Unlink a directory.
    Directory,

    /// Unlink a non-directory.
    NonDirectory,
}

pub trait FsNodeOps: Send + Sync {
    /// Open a FileObject for this node.
    ///
    /// The returned FileOps will be used to create a FileObject, which might
    /// be assigned an FdNumber.
    fn open(&self, node: &FsNode, flags: OpenFlags) -> Result<Box<dyn FileOps>, Errno>;

    /// Find an existing child node and populate the child parameter. Return the node.
    ///
    /// The child parameter is an empty node. Operations other than initialize may panic before
    /// initialize is called.
    fn lookup(&self, _node: &FsNode, _child: FsNode) -> Result<FsNodeHandle, Errno> {
        Err(ENOTDIR)
    }

    /// Create and return the given child node.
    ///
    /// The mode field of the FsNodeInfo indicates what kind of child to
    /// create.
    ///
    /// This function is never called with FileMode::IFDIR. The mkdir function
    /// is used to create directories instead.
    fn mknod(&self, _node: &FsNode, _child: FsNode) -> Result<FsNodeHandle, Errno> {
        Err(ENOTDIR)
    }

    /// Create and return the given child node as a subdirectory.
    fn mkdir(&self, _node: &FsNode, _child: FsNode) -> Result<FsNodeHandle, Errno> {
        Err(ENOTDIR)
    }

    /// Creates a symlink with the given `target` path.
    fn mksymlink(
        &self,
        _node: &FsNode,
        _child: FsNode,
        _target: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        Err(ENOTDIR)
    }

    /// Reads the symlink from this node.
    fn readlink<'a>(&'a self) -> Result<FsString, Errno> {
        Err(EINVAL)
    }

    /// Remove the child with the given name, if the child exists.
    ///
    /// The UnlinkKind parameter indicates whether the caller intends to unlink
    /// a directory or a non-directory child.
    fn unlink(
        &self,
        _node: &FsNode,
        _child: &FsNodeHandle,
        _kind: UnlinkKind,
    ) -> Result<(), Errno> {
        Err(ENOTDIR)
    }

    /// This node was unlinked from its parent.
    ///
    /// The node might still be referenced by open FileObjects.
    fn unlinked(&self, _node: &FsNodeHandle) {}

    /// Change the length of the file.
    fn truncate(&self, _node: &FsNode, _length: u64) -> Result<(), Errno> {
        Err(EINVAL)
    }

    /// Update node.info as needed.
    ///
    /// FsNode calls this method before converting the FsNodeInfo struct into
    /// the stat_t struct to give the file system a chance to update this data
    /// before it is used by clients.
    ///
    /// File systems that keep the FsNodeInfo up-to-date do not need to
    /// override this function.
    ///
    /// Return a reader lock on the updated information.
    fn update_info<'a>(&self, node: &'a FsNode) -> Result<RwLockReadGuard<'a, FsNodeInfo>, Errno> {
        Ok(node.info())
    }
}

impl FsNode {
    pub fn new_root<T: FsNodeOps + 'static>(ops: T, device: DeviceHandle) -> FsNodeHandle {
        // TODO: apply_umask
        Self::new_orphan(ops, FileMode::IFDIR | FileMode::ALLOW_ALL, device)
    }
    pub fn new_orphan<T: FsNodeOps + 'static>(
        ops: T,
        mode: FileMode,
        device: DeviceHandle,
    ) -> FsNodeHandle {
        let ops: Box<dyn FsNodeOps> = Box::new(ops);
        Arc::new(FsNode::new(Some(ops), mode, device, None, FsString::new()))
    }

    fn new(
        ops: Option<Box<dyn FsNodeOps>>,
        mode: FileMode,
        device: DeviceHandle,
        parent: Option<FsNodeHandle>,
        local_name: FsString,
    ) -> FsNode {
        let now = fuchsia_runtime::utc_time();
        let info = FsNodeInfo {
            inode_num: device.allocate_inode_number(),
            dev: device.get_device_id(),
            mode,
            time_create: now,
            time_access: now,
            time_modify: now,
            ..Default::default()
        };
        Self {
            ops,
            observers: ObserverList::default(),
            device,
            parent,
            local_name,
            children: RwLock::new(HashMap::new()),
            info: RwLock::new(info),
        }
    }

    /// The name that this node's parent calls this node.
    ///
    /// If this node is mounted in a namespace, the parent of this node in that
    /// namespace might have a different name for the point in the namespace at
    /// which this node is mounted.
    pub fn local_name(&self) -> &FsStr {
        &self.local_name
    }
    pub fn parent<'a>(self: &'a FsNodeHandle) -> Option<&'a FsNodeHandle> {
        self.parent.as_ref()
    }

    fn ops(&self) -> &dyn FsNodeOps {
        // Empty nodes are never returned to users of the FsNode API, so most uses of the ops field
        // can assume it is non-empty.
        &**self.ops.as_ref().unwrap()
    }

    pub fn set_ops(&mut self, ops: impl FsNodeOps + 'static) {
        self.ops = Some(Box::new(ops));
    }

    pub fn open(self: &FsNodeHandle, flags: OpenFlags) -> Result<Box<dyn FileOps>, Errno> {
        self.ops().open(&self, flags)
    }

    pub fn component_lookup(self: &FsNodeHandle, name: &FsStr) -> Result<FsNodeHandle, Errno> {
        let (node, _) = self.create_child(name, |child| self.ops().lookup(&self, child))?;
        Ok(node)
    }

    /// Creates a new `FsNode` with the given `name`, `mode`, and `dev`.
    ///
    /// `create_function` is called once the node has been created and initialized: this is a good
    /// time to "initialize" the node's `FsNodeOps` (e.g., self.ops().mkdir(...)).
    ///
    /// If the node already exists, `mk_callback` is not called, and an error is generated.
    pub fn create_node<F>(
        self: &FsNodeHandle,
        name: &FsStr,
        mode: FileMode,
        dev: dev_t,
        mk_callback: F,
    ) -> Result<FsNodeHandle, Errno>
    where
        F: FnOnce(FsNode) -> Result<FsNodeHandle, Errno>,
    {
        assert!(mode & FileMode::IFMT != FileMode::EMPTY, "mknod called without node type.");
        let (node, exists) = self.create_child(name, |mut child| {
            let info = child.info.get_mut();
            info.mode = mode;
            if mode.is_blk() || mode.is_chr() {
                info.rdev = dev;
            }
            mk_callback(child)
        })?;
        if exists {
            return Err(EEXIST);
        }
        let now = fuchsia_runtime::utc_time();
        let mut info = self.info_write();
        info.time_access = now;
        info.time_modify = now;
        Ok(node)
    }

    #[cfg(test)]
    pub fn mkdir(self: &FsNodeHandle, name: &FsStr) -> Result<FsNodeHandle, Errno> {
        // TODO: apply_umask
        self.create_node(name, FileMode::IFDIR | FileMode::ALLOW_ALL, 0, |node| {
            self.ops().mkdir(&self, node)
        })
    }

    pub fn mknod(
        self: &FsNodeHandle,
        name: &FsStr,
        mode: FileMode,
        dev: dev_t,
    ) -> Result<FsNodeHandle, Errno> {
        self.create_node(name, mode, dev, |node| {
            if mode.is_dir() {
                self.ops().mkdir(&self, node)
            } else {
                self.ops().mknod(&self, node)
            }
        })
    }

    pub fn mksymlink(
        self: &FsNodeHandle,
        name: &FsStr,
        target: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        self.create_node(name, FileMode::IFLNK | FileMode::ALLOW_ALL, 0, |node| {
            self.ops().mksymlink(&self, node, target)
        })
    }

    pub fn readlink(&self) -> Result<FsString, Errno> {
        self.ops().readlink()
    }

    pub fn unlink(self: &FsNodeHandle, name: &FsStr, kind: UnlinkKind) -> Result<(), Errno> {
        let child = {
            let mut children = self.children.write();
            let child =
                children.get(name).and_then(OnceCell::get).ok_or(ENOENT)?.upgrade().unwrap();
            // TODO: Check _kind against the child's mode.
            self.ops().unlink(self, &child, kind)?;
            children.remove(name);
            child
        };
        // We hold a reference to child until we have released the children
        // lock so that we do not trigger a deadlock in the Drop trait for
        // FsNode, which attempts to remove the FsNode from its parent's child
        // list.
        child.ops().unlinked(&child);
        Ok(())
    }

    pub fn truncate(&self, length: u64) -> Result<(), Errno> {
        self.ops().truncate(self, length)
    }

    pub fn stat(&self) -> Result<stat_t, Errno> {
        let info = self.ops().update_info(self)?;
        /// st_blksize is measured in units of 512 bytes.
        const BYTES_PER_BLOCK: i64 = 512;
        Ok(stat_t {
            st_ino: info.inode_num,
            st_mode: info.mode.bits(),
            st_size: info.size as off_t,
            st_blocks: info.storage_size as i64 / BYTES_PER_BLOCK,
            st_nlink: info.link_count,
            st_uid: info.uid,
            st_gid: info.gid,
            st_ctim: time_to_timespec(&info.time_create),
            st_mtim: time_to_timespec(&info.time_modify),
            st_atim: time_to_timespec(&info.time_access),
            st_dev: info.dev,
            st_rdev: info.rdev,
            st_blksize: BYTES_PER_BLOCK,
            ..Default::default()
        })
    }

    pub fn info(&self) -> RwLockReadGuard<'_, FsNodeInfo> {
        self.info.read()
    }
    pub fn info_write(&self) -> RwLockWriteGuard<'_, FsNodeInfo> {
        self.info.write()
    }
    pub fn info_mut(&mut self) -> &mut FsNodeInfo {
        self.info.get_mut()
    }

    fn create_child<F>(
        self: &FsNodeHandle,
        name: &FsStr,
        init_fn: F,
    ) -> Result<(FsNodeHandle, bool), Errno>
    where
        F: FnOnce(FsNode) -> Result<FsNodeHandle, Errno>,
    {
        let mut child_cell;
        let mut children_read_guard;
        {
            children_read_guard = self.children.read();
            child_cell = children_read_guard.get(name);
        }
        if child_cell.is_none() {
            std::mem::drop(children_read_guard);
            let mut children_write_guard = self.children.write();
            children_write_guard.entry(name.to_vec()).or_insert(OnceCell::new());
            children_read_guard = RwLockWriteGuard::downgrade(children_write_guard);
            child_cell = children_read_guard.get(name);
        }
        let child_cell = child_cell.unwrap();

        let mut new_child_handle = None;
        let weak_child = child_cell.get_or_try_init(|| {
            let child = FsNode::new(
                None,
                FileMode::EMPTY,
                Arc::clone(&self.device),
                Some(Arc::clone(self)),
                name.to_vec(),
            );
            let child = init_fn(child)?;
            assert!(
                child.info().mode & FileMode::IFMT != FileMode::EMPTY,
                "FsNodeOps initialization did not populate the FileMode in FsNodeInfo."
            );
            let weak_child = Arc::downgrade(&child);
            // Stash the Arc so it doesn't get immediately freed
            new_child_handle = Some(child);
            Ok(weak_child)
        })?;
        if let Some(child) = new_child_handle {
            Ok((child, false))
        } else {
            Ok((weak_child.upgrade().unwrap(), true))
        }
    }

    pub fn into_handle(self) -> FsNodeHandle {
        Arc::new(self)
    }

    // This function is only useful for tests and has some oddities.
    //
    // For example, not all the children might have been looked up yet, which
    // means the returned vector could be missing some names.
    //
    // Also, the vector might have "extra" names that are in the process of
    // being looked up. If the lookup fails, they'll be removed.
    #[cfg(test)]
    pub fn copy_child_names(&self) -> Vec<FsString> {
        self.children
            .read()
            .values()
            .filter_map(|child| {
                child.get().and_then(Weak::upgrade).map(|c| c.local_name().to_owned())
            })
            .collect()
    }

    fn internal_remove_child(&self, child: &mut FsNode) {
        // possible deadlock? this is called from Drop, so we need to be careful about dropping any
        // FsNodeHandle while locking the children.

        // This avoids a deadlock: FsNode construction read-locks children which would make the
        // following write lock fail, but we can check and avoid taking a write lock in that case.
        if self.children.read().get(child.local_name()).and_then(OnceCell::get).is_none() {
            return;
        }

        self.children.write().remove(child.local_name());
    }
}

impl Drop for FsNode {
    fn drop(&mut self) {
        if let Some(parent) = self.parent.take() {
            parent.internal_remove_child(self);
        }
    }
}
