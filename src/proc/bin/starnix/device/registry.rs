// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::mem::*;
use crate::fs::{FileOps, FsNode};
use crate::lock::RwLock;
use crate::task::*;
use crate::types::*;

use std::collections::btree_map::{BTreeMap, Entry};
use std::marker::{Send, Sync};
use std::sync::Arc;

/// The mode or category of the device driver.
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub enum DeviceMode {
    Char,
    Block,
}

pub trait DeviceOps: Send + Sync {
    fn open(
        &self,
        _current_task: &CurrentTask,
        _id: DeviceType,
        _node: &FsNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno>;
}

/// Allows directly using a function or closure as an implementation of DeviceOps, avoiding having
/// to write a zero-size struct and an impl for it.
impl<F> DeviceOps for F
where
    F: Send
        + Sync
        + Fn(&CurrentTask, DeviceType, &FsNode, OpenFlags) -> Result<Box<dyn FileOps>, Errno>,
{
    fn open(
        &self,
        current_task: &CurrentTask,
        id: DeviceType,
        node: &FsNode,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        self(current_task, id, node, flags)
    }
}

/// The kernel's registry of drivers.
pub struct DeviceRegistry {
    /// Maps device identifier to character device implementation.
    char_devices: BTreeMap<u32, Box<dyn DeviceOps>>,
    misc_devices: Arc<RwLock<MiscRegistry>>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            char_devices: BTreeMap::new(),
            misc_devices: Arc::new(RwLock::new(MiscRegistry::new())),
        };
        registry.char_devices.insert(MISC_MAJOR, Box::new(Arc::clone(&registry.misc_devices)));
        registry
    }

    /// Creates a `DeviceRegistry` and populates it with common drivers such as /dev/null.
    pub fn new_with_common_devices() -> Self {
        let mut registry = Self::new();
        registry.register_chrdev_major(MemDevice, MEM_MAJOR).unwrap();
        registry
    }

    pub fn register_chrdev_major<D>(&mut self, device: D, major: u32) -> Result<(), Errno>
    where
        D: DeviceOps + 'static,
    {
        match self.char_devices.entry(major) {
            Entry::Vacant(e) => {
                e.insert(Box::new(device));
                Ok(())
            }
            Entry::Occupied(_) => {
                tracing::error!("dev major {:?} is already registered", major);
                error!(EINVAL)
            }
        }
    }

    pub fn register_misc_chrdev<D>(&mut self, device: D) -> Result<DeviceType, Errno>
    where
        D: DeviceOps + 'static,
    {
        self.misc_devices.write().register(device)
    }

    /// Opens a device file corresponding to the device identifier `dev`.
    pub fn open_device(
        &self,
        current_task: &CurrentTask,
        node: &FsNode,
        flags: OpenFlags,
        dev: DeviceType,
        mode: DeviceMode,
    ) -> Result<Box<dyn FileOps>, Errno> {
        match mode {
            DeviceMode::Char => self
                .char_devices
                .get(&dev.major())
                .ok_or_else(|| errno!(ENODEV))?
                .open(current_task, dev, node, flags),
            DeviceMode::Block => error!(ENODEV),
        }
    }
}

struct MiscRegistry {
    misc_devices: BTreeMap<u32, Box<dyn DeviceOps>>,
    next_dynamic_minor: u32,
}

impl MiscRegistry {
    fn new() -> Self {
        Self { misc_devices: BTreeMap::new(), next_dynamic_minor: 0 }
    }

    fn register(&mut self, device: impl DeviceOps + 'static) -> Result<DeviceType, Errno> {
        let minor = self.next_dynamic_minor;
        if minor > 255 {
            return error!(ENOMEM);
        }
        self.next_dynamic_minor += 1;
        self.misc_devices.insert(minor, Box::new(device));
        Ok(DeviceType::new(MISC_MAJOR, minor))
    }
}

impl DeviceOps for Arc<RwLock<MiscRegistry>> {
    fn open(
        &self,
        current_task: &CurrentTask,
        id: DeviceType,
        node: &FsNode,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let state = self.read();
        let device = state.misc_devices.get(&id.minor()).ok_or(errno!(ENODEV))?;
        device.open(current_task, id, node, flags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::*;
    use crate::testing::*;

    #[::fuchsia::test]
    fn registry_fails_to_add_duplicate_device() {
        let mut registry = DeviceRegistry::new();
        registry.register_chrdev_major(MemDevice, MEM_MAJOR).expect("registers once");
        registry.register_chrdev_major(MemDevice, 123).expect("registers unique");
        registry
            .register_chrdev_major(MemDevice, MEM_MAJOR)
            .expect_err("fail to register duplicate");
    }

    #[::fuchsia::test]
    fn registry_opens_device() {
        let (_kernel, current_task) = create_kernel_and_task();

        let mut registry = DeviceRegistry::new();
        registry.register_chrdev_major(MemDevice, MEM_MAJOR).unwrap();

        let node = FsNode::new_root(PlaceholderFsNodeOps);

        // Fail to open non-existent device.
        assert!(registry
            .open_device(
                &current_task,
                &node,
                OpenFlags::RDONLY,
                DeviceType::NONE,
                DeviceMode::Char
            )
            .is_err());

        // Fail to open in wrong mode.
        assert!(registry
            .open_device(
                &current_task,
                &node,
                OpenFlags::RDONLY,
                DeviceType::NULL,
                DeviceMode::Block
            )
            .is_err());

        // Open in correct mode.
        let _ = registry
            .open_device(
                &current_task,
                &node,
                OpenFlags::RDONLY,
                DeviceType::NULL,
                DeviceMode::Char,
            )
            .expect("opens device");
    }

    #[::fuchsia::test]
    fn test_dynamic_misc() {
        let (_kernel, current_task) = create_kernel_and_task();

        struct TestDevice;
        impl DeviceOps for TestDevice {
            fn open(
                &self,
                _current_task: &CurrentTask,
                _id: DeviceType,
                _node: &FsNode,
                _flags: OpenFlags,
            ) -> Result<Box<dyn FileOps>, Errno> {
                Ok(Box::new(PanicFileOps))
            }
        }

        let mut registry = DeviceRegistry::new();
        let device_type = registry.register_misc_chrdev(TestDevice).unwrap();
        assert_eq!(device_type.major(), MISC_MAJOR);

        let node = FsNode::new_root(PlaceholderFsNodeOps);
        let _ = registry
            .open_device(&current_task, &node, OpenFlags::RDONLY, device_type, DeviceMode::Char)
            .expect("opens device");
    }
}
