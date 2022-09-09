// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        crypt::Crypt,
        filesystem::{Filesystem, FxFilesystem},
        fsck::errors::{FsckError, FsckFatal, FsckIssue, FsckWarning},
        log::*,
        lsm_tree::{
            simple_persistent_layer::SimplePersistentLayer,
            skip_list_layer::SkipListLayer,
            types::{
                BoxedLayerIterator, Item, Key, Layer, LayerIterator, OrdUpperBound, RangeKey, Value,
            },
        },
        object_handle::{ObjectHandle, ObjectHandleExt, INVALID_OBJECT_ID},
        object_store::{
            allocator::{Allocator, AllocatorKey, AllocatorValue, CoalescingIterator},
            journal::super_block::SuperBlockInstance,
            transaction::{LockKey, TransactionHandler},
            volume::root_volume,
            HandleOptions, ObjectKey, ObjectStore, ObjectValue, StoreInfo,
            MAX_STORE_INFO_SERIALIZED_SIZE,
        },
        serialized_types::VersionedLatest,
    },
    anyhow::{anyhow, Context, Error},
    futures::try_join,
    std::{
        collections::{BTreeMap, HashSet},
        iter::zip,
        ops::Bound,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
};

pub mod errors;

mod store_scanner;

#[cfg(test)]
mod tests;

pub struct FsckOptions<F: Fn(&FsckIssue)> {
    /// Whether to fail fsck if any warnings are encountered.
    pub fail_on_warning: bool,
    // Whether to halt after the first error encountered (fatal or not).
    pub halt_on_error: bool,
    /// Whether to perform slower, more complete checks.
    pub do_slow_passes: bool,
    /// A callback to be invoked for each detected error, e.g. to log the error.
    pub on_error: F,
    /// Whether to be noisy as we do checks.
    pub verbose: bool,
}

pub fn default_options() -> FsckOptions<impl Fn(&FsckIssue)> {
    FsckOptions {
        fail_on_warning: false,
        halt_on_error: false,
        do_slow_passes: true,
        on_error: FsckIssue::log,
        verbose: false,
    }
}

/// Verifies the integrity of Fxfs.  See errors.rs for a list of checks performed.
// TODO(fxbug.dev/87381): add checks for:
//  + The root parent object store ID and root object store ID must not conflict with any other
//    stores or the allocator.
//
// TODO(fxbug.dev/96075): This currently takes a write lock on the filesystem.  It would be nice if
// we could take a snapshot.
//
// TODO(fxbug.dev/97324): For now this takes a single crypt service to be used for all encrypted
// filesystems that aren't locked, but this will have to change as and when we support multiple
// encrypted volumes.
pub async fn fsck(
    filesystem: &Arc<FxFilesystem>,
    crypt: Option<Arc<dyn Crypt>>,
) -> Result<(), Error> {
    fsck_with_options(filesystem, crypt, default_options()).await
}

pub async fn fsck_with_options<F: Fn(&FsckIssue)>(
    filesystem: &Arc<FxFilesystem>,
    crypt: Option<Arc<dyn Crypt>>,
    options: FsckOptions<F>,
) -> Result<(), Error> {
    info!("Starting fsck");
    let _guard = filesystem.write_lock(&[LockKey::Filesystem]).await;

    let mut fsck = Fsck::new(options);

    let object_manager = filesystem.object_manager();
    let super_block = filesystem.super_block();

    // Keep track of all things that might exist in journal checkpoints so we can check for
    // unexpected entries.
    let mut journal_checkpoint_ids: HashSet<u64> = HashSet::new();
    journal_checkpoint_ids.insert(super_block.allocator_object_id);
    journal_checkpoint_ids.insert(super_block.root_store_object_id);

    // Scan the root parent object store.
    let mut root_objects = vec![super_block.root_store_object_id, super_block.journal_object_id];
    root_objects.append(&mut object_manager.root_store().parent_objects());
    fsck.verbose("Scanning root parent store...");
    store_scanner::scan_store(&fsck, object_manager.root_parent_store().as_ref(), &root_objects)
        .await?;
    fsck.verbose("Scanning root parent store done");

    let root_store = &object_manager.root_store();
    let mut root_store_root_objects = Vec::new();
    root_store_root_objects.append(&mut vec![
        super_block.allocator_object_id,
        SuperBlockInstance::A.object_id(),
        SuperBlockInstance::B.object_id(),
    ]);
    root_store_root_objects.append(&mut root_store.root_objects());

    let root_volume = root_volume(filesystem).await?;
    let volume_directory = root_volume.volume_directory();
    let layer_set = volume_directory.store().tree().layer_set();
    let mut merger = layer_set.merger();
    let mut iter = volume_directory.iter(&mut merger).await?;

    // TODO(fxbug.dev/96076): We could maybe iterate over stores concurrently.
    while let Some((name, store_id, _)) = iter.get() {
        journal_checkpoint_ids.insert(store_id);
        fsck.verbose(format!("Scanning volume \"{}\" (id {})...", name, store_id));
        fsck.check_child_store(&filesystem, store_id, &mut root_store_root_objects, crypt.clone())
            .await
            .context("Failed to check child store")?;
        iter.advance().await?;
        fsck.verbose("Scanning volume done");
    }

    // TODO(fxbug.dev/96077): It's a bit crude how details of SimpleAllocator are leaking here. Is
    // there a better way?
    let allocator = filesystem.allocator();
    root_store_root_objects.append(&mut allocator.parent_objects());

    if fsck.options.do_slow_passes {
        // Scan each layer file for the allocator.
        let layer_set = allocator.tree().immutable_layer_set();
        fsck.verbose(format!("Checking {} layers for allocator...", layer_set.layers.len()));
        for layer in layer_set.layers {
            if let Some(handle) = layer.handle() {
                fsck.verbose(format!(
                    "Layer file {} for allocator is {} bytes",
                    handle.object_id(),
                    handle.get_size()
                ));
            }
            fsck.check_layer_file_contents(
                allocator.object_id(),
                layer.handle().map(|h| h.object_id()).unwrap_or(INVALID_OBJECT_ID),
                layer.clone(),
            )
            .await?;
        }
        fsck.verbose("Checking layers done");
    }

    // Finally scan the root object store.
    fsck.verbose("Scanning root object store...");
    store_scanner::scan_store(&fsck, root_store.as_ref(), &root_store_root_objects).await?;
    fsck.verbose("Scanning root object store done");

    // Now compare our regenerated allocation map with what we actually have.
    fsck.verbose("Verifying allocations...");
    let layer_set = allocator.tree().layer_set();
    let mut merger = layer_set.merger();
    let mut actual = CoalescingIterator::new(allocator.iter(&mut merger, Bound::Unbounded).await?)
        .await
        .expect("filter failed");
    let mut expected =
        CoalescingIterator::new(fsck.allocations.seek(Bound::Unbounded).await?).await?;
    let mut expected_owner_allocated_bytes = BTreeMap::new();
    let mut extra_allocations: Vec<errors::Allocation> = vec![];
    let bs = filesystem.block_size();
    while let Some(actual_item) = actual.get() {
        if actual_item.key.device_range.start % bs > 0 || actual_item.key.device_range.end % bs > 0
        {
            fsck.error(FsckError::MisalignedAllocation(actual_item.into()))?;
        } else if actual_item.key.device_range.start >= actual_item.key.device_range.end {
            fsck.error(FsckError::MalformedAllocation(actual_item.into()))?;
        }
        match expected.get() {
            None => extra_allocations.push(actual_item.into()),
            Some(expected_item) => {
                if actual_item.key.device_range.end <= expected_item.key.device_range.start {
                    extra_allocations.push(actual_item.into());
                    actual.advance().await?;
                    continue;
                }
                let owner_object_id = match expected_item.value {
                    AllocatorValue::None => INVALID_OBJECT_ID,
                    AllocatorValue::Abs { owner_object_id, .. } => *owner_object_id,
                };
                let r = &expected_item.key.device_range;
                *expected_owner_allocated_bytes.entry(owner_object_id).or_insert(0) +=
                    (r.end - r.start) as i64;
                if expected_item.key.device_range.end <= actual_item.key.device_range.start {
                    fsck.error(FsckError::MissingAllocation(expected_item.into()))?;
                    expected.advance().await?;
                    continue;
                }
                // We can only reconstruct the key/value fields of Item.
                if actual_item.key != expected_item.key || actual_item.value != expected_item.value
                {
                    fsck.error(FsckError::AllocationMismatch(
                        expected_item.into(),
                        actual_item.into(),
                    ))?;
                }
            }
        }
        try_join!(actual.advance(), expected.advance())?;
    }
    let expected_allocated_bytes = expected_owner_allocated_bytes.values().sum::<i64>() as u64;
    fsck.verbose(format!(
        "Found {} bytes allocated (expected {} bytes). Total device size is {} bytes.",
        allocator.get_allocated_bytes(),
        expected_allocated_bytes,
        filesystem.device().block_count() * filesystem.device().block_size() as u64
    ));
    if !extra_allocations.is_empty() {
        fsck.error(FsckError::ExtraAllocations(extra_allocations))?;
    }
    if let Some(item) = expected.get() {
        fsck.error(FsckError::MissingAllocation(item.into()))?;
    }
    let owner_allocated_bytes = allocator.get_owner_allocated_bytes();
    if expected_allocated_bytes != allocator.get_allocated_bytes()
        || zip(expected_owner_allocated_bytes.iter(), owner_allocated_bytes.iter())
            .filter(|((k1, v1), (k2, v2))| (*k1, *v1) != (*k2, *v2))
            .count()
            != 0
    {
        fsck.error(FsckError::AllocatedBytesMismatch(
            expected_owner_allocated_bytes.iter().map(|(k, v)| (*k, *v)).collect(),
            owner_allocated_bytes.iter().map(|(k, v)| (*k, *v)).collect(),
        ))?;
    }
    for (k, v) in allocator.owner_byte_limits() {
        if !owner_allocated_bytes.contains_key(&k) {
            fsck.warning(FsckWarning::LimitForNonExistentStore(k, v))?;
        }
    }

    // Every key in journal_file_offsets should map to an lsm tree (ObjectStore or Allocator).
    // Excess entries mean we won't be able to reap the journal to free space.
    // Missing entries are OK. Entries only exist if there is data for the store that hasn't been
    // flushed yet.
    for object_id in super_block.journal_file_offsets.keys() {
        if !journal_checkpoint_ids.contains(object_id) {
            fsck.error(FsckError::UnexpectedJournalFileOffset(*object_id))?;
        }
    }

    let errors = fsck.errors();
    let warnings = fsck.warnings();
    if errors > 0 || (fsck.options.fail_on_warning && warnings > 0) {
        Err(anyhow!("Fsck encountered {} errors, {} warnings", errors, warnings))
    } else {
        if warnings > 0 {
            warn!(count = warnings, "Fsck encountered warnings");
        } else {
            info!("No issues detected");
        }
        Ok(())
    }
}

trait KeyExt: PartialEq {
    fn overlaps(&self, other: &Self) -> bool;
}

impl<K: RangeKey + PartialEq> KeyExt for K {
    fn overlaps(&self, other: &Self) -> bool {
        RangeKey::overlaps(self, other)
    }
}

struct Fsck<F: Fn(&FsckIssue)> {
    options: FsckOptions<F>,
    // A list of allocations generated based on all extents found across all scanned object stores.
    allocations: Arc<SkipListLayer<AllocatorKey, AllocatorValue>>,
    errors: AtomicU64,
    warnings: AtomicU64,
}

impl<F: Fn(&FsckIssue)> Fsck<F> {
    fn new(options: FsckOptions<F>) -> Self {
        Fsck {
            options,
            // TODO(fxbug.dev/95981): fix magic number
            allocations: SkipListLayer::new(2048),
            errors: AtomicU64::new(0),
            warnings: AtomicU64::new(0),
        }
    }

    // Log if in verbose mode.
    fn verbose(&self, message: impl AsRef<str>) {
        if self.options.verbose {
            info!(message = message.as_ref(), "fsck");
        }
    }

    fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    fn warnings(&self) -> u64 {
        self.warnings.load(Ordering::Relaxed)
    }

    fn assert<V>(&self, res: Result<V, Error>, error: FsckFatal) -> Result<V, Error> {
        if res.is_err() {
            (self.options.on_error)(&FsckIssue::Fatal(error.clone()));
            return Err(anyhow!(format!("{:?}", error)).context(res.err().unwrap()));
        }
        res
    }

    fn warning(&self, error: FsckWarning) -> Result<(), Error> {
        (self.options.on_error)(&FsckIssue::Warning(error.clone()));
        self.warnings.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn error(&self, error: FsckError) -> Result<(), Error> {
        (self.options.on_error)(&FsckIssue::Error(error.clone()));
        self.errors.fetch_add(1, Ordering::Relaxed);
        if self.options.halt_on_error {
            Err(anyhow!(format!("{:?}", error)))
        } else {
            Ok(())
        }
    }

    fn fatal(&self, error: FsckFatal) -> Result<(), Error> {
        (self.options.on_error)(&FsckIssue::Fatal(error.clone()));
        Err(anyhow!(format!("{:?}", error)))
    }

    async fn check_child_store(
        &mut self,
        filesystem: &FxFilesystem,
        store_id: u64,
        root_store_root_objects: &mut Vec<u64>,
        mut crypt: Option<Arc<dyn Crypt>>,
    ) -> Result<(), Error> {
        let root_store = filesystem.root_store();
        let store =
            filesystem.object_manager().store(store_id).context("open_store failed").unwrap();

        if store.is_locked() {
            if let Some(crypt) = &crypt {
                store.unlock(crypt.clone()).await?;
            } else {
                // We can't check this store.
                info!(store_id, "Skipping locked encrypted store");
                return Ok(());
            }
        } else {
            crypt = store.crypt();
        }

        // Manually open the store so we can do our own validation.  Later, we will call open_store
        // to get a regular ObjectStore wrapper.
        let handle = self.assert(
            ObjectStore::open_object(&root_store, store_id, HandleOptions::default(), None).await,
            FsckFatal::MissingStoreInfo(store_id),
        )?;
        let object_layer_file_object_ids = {
            let info = if handle.get_size() > 0 {
                let serialized_info = handle.contents(MAX_STORE_INFO_SERIALIZED_SIZE).await?;
                let mut cursor = std::io::Cursor::new(&serialized_info[..]);
                let (store_info, _version) = self.assert(
                    StoreInfo::deserialize_with_version(&mut cursor)
                        .context("Failed to deserialize StoreInfo"),
                    FsckFatal::MalformedStore(store_id),
                )?;
                store_info
            } else {
                // The store_info will be absent for a newly created and empty object store.
                StoreInfo::default()
            };
            // We don't replay the store ReplayInfo here, since it doesn't affect what we
            // want to check (mainly the existence of the layer files).  If that changes,
            // we'll need to update this.
            info.layers.clone()
        };
        self.verbose(format!(
            "Store {} has {} object tree layers",
            store_id,
            object_layer_file_object_ids.len(),
        ));
        for layer_file_object_id in object_layer_file_object_ids {
            self.check_layer_file::<ObjectKey, ObjectValue>(
                &root_store,
                store_id,
                layer_file_object_id,
                crypt.as_deref(),
            )
            .await?;
        }

        store_scanner::scan_store(self, store.as_ref(), &store.root_objects())
            .await
            .context("scan_store failed")?;
        let mut parent_objects = store.parent_objects();
        root_store_root_objects.append(&mut parent_objects);
        Ok(())
    }

    async fn check_layer_file<
        K: Key + KeyExt + OrdUpperBound + std::fmt::Debug,
        V: Value + std::fmt::Debug,
    >(
        &self,
        root_store: &Arc<ObjectStore>,
        store_object_id: u64,
        layer_file_object_id: u64,
        crypt: Option<&dyn Crypt>,
    ) -> Result<(), Error> {
        let layer_file = self.assert(
            ObjectStore::open_object(
                root_store,
                layer_file_object_id,
                HandleOptions::default(),
                crypt,
            )
            .await,
            FsckFatal::MissingLayerFile(store_object_id, layer_file_object_id),
        )?;
        if self.options.do_slow_passes {
            self.verbose(format!(
                "Layer file {} for store {} is {} bytes",
                layer_file_object_id,
                store_object_id,
                layer_file.get_size()
            ));
            let layer = SimplePersistentLayer::open(layer_file).await?;
            self.check_layer_file_contents(
                store_object_id,
                layer_file_object_id,
                layer as Arc<dyn Layer<K, V>>,
            )
            .await?;
        }
        Ok(())
    }

    async fn check_layer_file_contents<
        K: Key + KeyExt + OrdUpperBound + std::fmt::Debug,
        V: Value + std::fmt::Debug,
    >(
        &self,
        store_object_id: u64,
        layer_file_object_id: u64,
        layer: Arc<dyn Layer<K, V>>,
    ) -> Result<(), Error> {
        let mut iter: BoxedLayerIterator<'_, K, V> = self.assert(
            layer.seek(Bound::Unbounded).await,
            FsckFatal::MalformedLayerFile(store_object_id, layer_file_object_id),
        )?;

        let mut last_item: Option<Item<K, V>> = None;
        while let Some(item) = iter.get() {
            if let Some(last) = last_item {
                if !last.key.cmp_upper_bound(&item.key).is_le() {
                    self.fatal(FsckFatal::MisOrderedLayerFile(
                        store_object_id,
                        layer_file_object_id,
                    ))?;
                }
                if last.key.overlaps(&item.key) {
                    self.fatal(FsckFatal::OverlappingKeysInLayerFile(
                        store_object_id,
                        layer_file_object_id,
                        item.into(),
                        last.as_item_ref().into(),
                    ))?;
                }
            }
            last_item = Some(item.cloned());
            self.assert(
                iter.advance().await,
                FsckFatal::MalformedLayerFile(store_object_id, layer_file_object_id),
            )?;
        }
        Ok(())
    }
}
