// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::io::Directory,
    component_events::{
        events::{Event, EventMode, EventSubscription, Started},
        matcher::EventMatcher,
    },
    fidl_fuchsia_device::{ControllerMarker, ControllerProxy},
    fidl_fuchsia_hardware_block_partition::Guid,
    fidl_fuchsia_hardware_block_volume::{
        VolumeManagerMarker, VolumeManagerProxy, VolumeMarker, VolumeProxy,
    },
    fidl_fuchsia_io::OPEN_RIGHT_READABLE,
    fuchsia_component::client::connect_to_service_at_path,
    fuchsia_zircon::{sys::zx_status_t, AsHandleRef, Rights, Status, Vmo},
    ramdevice_client::{RamdiskClient, VmoRamdiskClientBuilder},
    rand::{rngs::SmallRng, FromEntropy, Rng},
    std::{
        fs::OpenOptions,
        os::{raw::c_int, unix::io::AsRawFd},
        path::PathBuf,
        time::Duration,
    },
    storage_isolated_driver_manager::{bind_fvm, rebind_fvm},
    test_utils_lib::opaque_test::OpaqueTest,
};

#[link(name = "fs-management")]
extern "C" {
    // This function initializes FVM on a fuchsia.hardware.block.Block device
    // with a given slice size.
    pub fn fvm_init(fd: c_int, slice_size: usize) -> zx_status_t;
}

async fn start_test() -> OpaqueTest {
    let test: OpaqueTest = OpaqueTest::default(
        "fuchsia-pkg://fuchsia.com/storage-isolated-devmgr#meta/isolated-devmgr.cm",
    )
    .await
    .unwrap();

    // Wait for the root component to start
    let event_source = test.connect_to_event_source().await.unwrap();
    let mut started_event_stream = event_source
        .subscribe(vec![EventSubscription::new(vec![Started::NAME], EventMode::Sync)])
        .await
        .unwrap();
    event_source.start_component_tree().await;
    EventMatcher::ok().moniker(".").expect_match::<Started>(&mut started_event_stream).await;

    test
}

fn create_ramdisk(test: &OpaqueTest, vmo: &Vmo, ramdisk_block_size: u64) -> RamdiskClient {
    // Wait until the ramctl driver is available
    let dev_path = test.get_hub_v2_path().join("exec/expose/dev");
    let ramctl_path = dev_path.join("misc/ramctl");
    let ramctl_path = ramctl_path.to_str().unwrap();
    ramdevice_client::wait_for_device(ramctl_path, Duration::from_secs(20)).unwrap();

    let duplicated_handle = vmo.as_handle_ref().duplicate(Rights::SAME_RIGHTS).unwrap();
    let duplicated_vmo = Vmo::from(duplicated_handle);

    // Create the ramdisks
    let dev_root = OpenOptions::new().read(true).write(true).open(&dev_path).unwrap();
    VmoRamdiskClientBuilder::new(duplicated_vmo)
        .block_size(ramdisk_block_size)
        .dev_root(dev_root)
        .build()
        .unwrap()
}

fn init_fvm(ramdisk_path: &str, fvm_slice_size: u64) {
    // Create the FVM filesystem
    let ramdisk_file = OpenOptions::new().read(true).write(true).open(ramdisk_path).unwrap();
    let ramdisk_fd = ramdisk_file.as_raw_fd();
    let status = unsafe { fvm_init(ramdisk_fd, fvm_slice_size as usize) };
    Status::ok(status).unwrap();
}

async fn start_fvm_driver(ramdisk_path: &str) -> (ControllerProxy, VolumeManagerProxy) {
    let controller = connect_to_service_at_path::<ControllerMarker>(ramdisk_path).unwrap();
    bind_fvm(&controller).await.unwrap();

    // Wait until the FVM driver is available
    let fvm_path = PathBuf::from(ramdisk_path).join("fvm");
    let fvm_path = fvm_path.to_str().unwrap();
    ramdevice_client::wait_for_device(fvm_path, Duration::from_secs(20)).unwrap();

    // Connect to the Volume Manager
    let proxy = connect_to_service_at_path::<VolumeManagerMarker>(fvm_path).unwrap();
    (controller, proxy)
}

async fn does_guid_match(volume_proxy: &VolumeProxy, expected_instance_guid: &Guid) -> bool {
    // The GUIDs must match
    let (status, actual_guid_instance) = volume_proxy.get_instance_guid().await.unwrap();

    // The ramdisk is also a block device, but does not support the Volume protocol
    if let Err(Status::NOT_SUPPORTED) = Status::ok(status) {
        return false;
    }

    let actual_guid_instance = actual_guid_instance.unwrap();
    *actual_guid_instance == *expected_instance_guid
}

/// This structs holds processes of component manager, isolated-devmgr
/// and the fvm driver.
///
/// NOTE: The order of fields in this struct is important.
/// Destruction happens top-down. Test must be destroyed last.
pub struct FvmInstance {
    /// A proxy to fuchsia.hardware.block.VolumeManager protocol
    /// Used to create new FVM volumes
    volume_manager: VolumeManagerProxy,

    /// A proxy to fuchsia.device.Controller protocol
    /// Used to bind/rebind the FVM driver to the ramdisk device
    controller: ControllerProxy,

    /// Manages the ramdisk device that is backed by a VMO
    _ramdisk: RamdiskClient,

    /// The component manager process that runs isolated-devmgr
    test: OpaqueTest,
}

impl FvmInstance {
    /// Kill the test's component manager process.
    /// This should take down the entire test's component tree with it,
    /// including the driver manager and ramdisk + fvm drivers.
    pub fn kill_component_manager(&mut self) {
        self.test.component_manager_app.kill().unwrap();
    }

    /// Force rebind the FVM driver. This is similar to a device disconnect/reconnect.
    pub async fn rebind_fvm_driver(&mut self) {
        rebind_fvm(&self.controller).await.unwrap();
    }

    /// Start an isolated FVM driver against the given VMO.
    /// If `init` is true, initialize the VMO with FVM layout first.
    pub async fn new(init: bool, vmo: &Vmo, fvm_slice_size: u64, ramdisk_block_size: u64) -> Self {
        let test = start_test().await;
        let ramdisk = create_ramdisk(&test, &vmo, ramdisk_block_size);

        let dev_path = test.get_hub_v2_path().join("exec/expose/dev");
        let ramdisk_path = dev_path.join(ramdisk.get_path());
        let ramdisk_path = ramdisk_path.to_str().unwrap();

        if init {
            init_fvm(ramdisk_path, fvm_slice_size);
        }

        let (controller, volume_manager) = start_fvm_driver(ramdisk_path).await;

        Self { test, controller, _ramdisk: ramdisk, volume_manager }
    }

    /// Get the full path to /dev/class/block from the devmgr running in this test
    pub fn block_path(&self) -> PathBuf {
        self.test.get_hub_v2_path().join("exec/expose/dev/class/block")
    }

    /// Create a new FVM volume with the given name and type GUID. This volume will consume
    /// exactly 1 slice. Returns the instance GUID used to uniquely identify this volume.
    pub async fn new_volume(&mut self, name: &str, mut type_guid: Guid) -> Guid {
        // Generate a random instance GUID
        let mut rng = SmallRng::from_entropy();
        let mut instance_guid = Guid { value: rng.gen() };

        // Create the new volume
        let status = self
            .volume_manager
            .allocate_partition(1, &mut type_guid, &mut instance_guid, name, 0)
            .await
            .unwrap();
        Status::ok(status).unwrap();

        instance_guid
    }

    /// Get the full path to a volume in this test that matches the given instance GUID.
    /// This function will wait until a matching volume is found.
    pub async fn get_volume_path(&self, instance_guid: &Guid) -> PathBuf {
        get_volume_path(self.block_path(), instance_guid).await
    }
}

/// Gets the full path to a volume matching the given instance GUID at the given
/// /dev/class/block path. This function will wait until a matching volume is found.
pub async fn get_volume_path(block_path: PathBuf, instance_guid: &Guid) -> PathBuf {
    let dir = Directory::from_namespace(block_path.clone(), OPEN_RIGHT_READABLE).unwrap();
    loop {
        // TODO(xbhatnag): Find a better way to wait for the volume to appear
        for entry in dir.entries().await.unwrap() {
            let volume_path = block_path.join(entry);
            let volume_path_str = volume_path.to_str().unwrap();

            // Connect to the Volume FIDL protocol
            let volume_proxy = connect_to_service_at_path::<VolumeMarker>(volume_path_str).unwrap();
            if does_guid_match(&volume_proxy, &instance_guid).await {
                return volume_path;
            }
        }
    }
}
