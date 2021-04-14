// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The journal is implemented as an ever extending file which contains variable length records that
// describe mutations to be applied to various objects.  The journal file consists of blocks, with a
// checksum at the end of each block, but otherwise it can be considered a continuous stream.  The
// checksum is seeded with the checksum from the previous block.  To free space in the journal,
// records are replaced with sparse extents when it is known they are no longer needed to mount.  At
// mount time, the journal is replayed: the mutations are applied into memory.  Eventually, a
// checksum failure will indicate no more records exist to be replayed, at which point the mount can
// continue and the journal will be extended from that point with further mutations as required.
//
// The super-block contains the starting offset and checksum for the journal file and sufficient
// information to locate the initial extents for the journal.  The super-block is written using the
// same per-block checksum that is used for the journal file.

mod reader;
mod super_block;
mod writer;

use {
    crate::{
        object_handle::ObjectHandle,
        object_store::{
            allocator::{Allocator, SimpleAllocator},
            constants::SUPER_BLOCK_OBJECT_ID,
            filesystem::{Filesystem, Mutations, ObjectManager, SyncOptions},
            journal::{
                reader::{JournalReader, ReadResult},
                super_block::SuperBlock,
                writer::JournalWriter,
            },
            record::{ObjectItem, ObjectKey},
            transaction::{
                AssociatedObject, Mutation, ObjectStoreMutation, Transaction, TxnMutation,
            },
            HandleOptions, ObjectStore, StoreObjectHandle,
        },
    },
    anyhow::Error,
    bincode::serialize_into,
    byteorder::{ByteOrder, LittleEndian},
    rand::Rng,
    serde::{Deserialize, Serialize},
    std::{
        clone::Clone,
        iter::IntoIterator,
        sync::{Arc, Mutex},
        vec::Vec,
    },
};

// The journal file is written to in blocks of this size.
const BLOCK_SIZE: u64 = 8192;

// The journal file is extended by this amount when necessary.
const CHUNK_SIZE: u64 = 131_072;

// After replaying the journal, it's possible that the stream doesn't end cleanly, in which case the
// next journal block needs to indicate this.  This is done by setting its checksum to what it
// should be xored with this value.
const RESET_XOR: u64 = 0xffffffffffffffff;

type Checksum = u64;

// To keep track of offsets within a journal file, we need both the file offset and the check-sum of
// the preceding block, since the check-sum of the preceding block is an input to the check-sum of
// every block.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct JournalCheckpoint {
    pub file_offset: u64,

    // Starting check-sum for block that contains file_offset i.e. the checksum for the previous
    // block.
    pub checksum: Checksum,
}

impl JournalCheckpoint {
    fn new(file_offset: u64, checksum: Checksum) -> JournalCheckpoint {
        JournalCheckpoint { file_offset, checksum }
    }
}

// All journal blocks are covered by a fletcher64 checksum as the last 8 bytes in a block.
fn fletcher64(buf: &[u8], previous: u64) -> u64 {
    assert!(buf.len() % 4 == 0);
    let mut lo = previous as u32;
    let mut hi = (previous >> 32) as u32;
    for chunk in buf.chunks(4) {
        lo = lo.wrapping_add(LittleEndian::read_u32(chunk));
        hi = hi.wrapping_add(lo);
    }
    (hi as u64) << 32 | lo as u64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum JournalRecord {
    // Indicates no more records in this block.
    EndBlock,
    // Mutation for a particular object.  object_id here is for the collection i.e. the store or
    // allocator.
    Mutation { object_id: u64, mutation: Mutation },
    // Commits records in the transaction.
    Commit,
}

fn journal_handle_options() -> HandleOptions {
    HandleOptions { overwrite: true, ..Default::default() }
}

/// The journal records a stream of mutations that are to be applied to other objects.  At mount
/// time, these records can be replayed into memory.  It provides a way to quickly persist changes
/// without having to make a large number of writes; they can be deferred to a later time (e.g.
/// when a sufficient number have been queued).  It also provides support for transactions, the
/// ability to have mutations that are to be applied atomically together.
pub struct Journal {
    objects: Arc<ObjectManager>,
    writer: futures::lock::Mutex<JournalWriter<StoreObjectHandle>>,
    inner: Mutex<Inner>,
}

struct Inner {
    needs_super_block: bool,
    super_block: SuperBlock,
}

impl Journal {
    pub fn new(objects: Arc<ObjectManager>) -> Journal {
        let starting_checksum = rand::thread_rng().gen();
        Journal {
            objects: objects,
            writer: futures::lock::Mutex::new(JournalWriter::new(
                None,
                BLOCK_SIZE as usize,
                starting_checksum,
            )),
            inner: Mutex::new(Inner {
                needs_super_block: true,
                super_block: SuperBlock::default(),
            }),
        }
    }

    /// Reads a super-block and then replays journaled records.
    pub async fn replay(&self, filesystem: Arc<dyn Filesystem>) -> Result<(), Error> {
        let device = filesystem.device();
        let (super_block, mut reader) = SuperBlock::read(device).await?;
        log::info!("replaying journal, superblock: {:?}", super_block);

        let allocator = Arc::new(SimpleAllocator::new(
            filesystem.clone(),
            super_block.allocator_object_id,
            false,
        ));
        self.objects.set_allocator(allocator.clone());

        let root_parent =
            ObjectStore::new_empty(None, super_block.root_parent_store_object_id, filesystem);

        while let Some(item) = reader.next_item().await? {
            root_parent
                .apply_mutation(Mutation::insert_object(item.key, item.value), true, None)
                .await;
        }

        {
            let mut inner = self.inner.lock().unwrap();
            inner.needs_super_block = false;
            inner.super_block = super_block.clone();
        }
        self.objects.set_root_parent_store_object_id(root_parent.store_object_id());
        let mut mutations = Vec::new();
        let mut journal_file_checkpoint = None;
        let mut end_block = false;
        root_parent.lazy_open_store(super_block.root_store_object_id);
        self.objects.set_root_store_object_id(super_block.root_store_object_id);
        let mut reader = JournalReader::new(
            root_parent
                .open_object(super_block.journal_object_id, journal_handle_options())
                .await?,
            self.block_size(),
            &super_block.journal_checkpoint,
        );
        loop {
            let current_checkpoint = Some(reader.journal_file_checkpoint());
            match reader.deserialize().await? {
                ReadResult::Reset => mutations.clear(), // Discard pending mutations
                ReadResult::Some(record) => {
                    end_block = false;
                    match record {
                        JournalRecord::EndBlock => {
                            reader.skip_to_end_of_block();
                            end_block = true;
                        }
                        JournalRecord::Mutation { object_id, mutation } => {
                            if mutations.len() == 0 {
                                journal_file_checkpoint = current_checkpoint;
                            }
                            mutations.push((object_id, mutation));
                        }
                        JournalRecord::Commit => {
                            if let Some(checkpoint) = journal_file_checkpoint.take() {
                                log::debug!("REPLAY {}", checkpoint.file_offset);
                                for (object_id, mutation) in mutations {
                                    // Snoop the mutations for any that might apply to the journal
                                    // file to ensure that we accurately track changes in size.
                                    let associated_object = match (object_id, &mutation) {
                                        (
                                            store_object_id,
                                            Mutation::ObjectStore(ObjectStoreMutation {
                                                item:
                                                    ObjectItem {
                                                        key: ObjectKey { object_id, .. }, ..
                                                    },
                                                ..
                                            }),
                                        ) if store_object_id
                                            == super_block.root_parent_store_object_id
                                            && *object_id == super_block.journal_object_id =>
                                        {
                                            Some(reader.handle() as &_)
                                        }
                                        _ => None,
                                    };

                                    self.apply_mutation(
                                        object_id,
                                        &checkpoint,
                                        mutation,
                                        true,
                                        associated_object,
                                    )
                                    .await;
                                }
                                mutations = Vec::new();
                            }
                        }
                    }
                }
                // This is expected when we reach the end of the journal stream.
                ReadResult::ChecksumMismatch => break,
            }
        }
        // Configure the journal writer so that we can continue.
        {
            let checkpoint =
                JournalCheckpoint::new(reader.read_offset(), reader.last_read_checksum());
            let mut writer = self.writer.lock().await;
            writer.set_handle(reader.take_handle());
            // If the last entry wasn't an end_block, then we need to reset the stream.
            writer.seek_to_checkpoint(checkpoint, !end_block);
        }
        log::info!("replay done");
        Ok(())
    }

    /// Creates an empty filesystem with the minimum viable objects (including a root parent and
    /// root store but no further child stores).  Nothing is written to the device until sync is
    /// called.
    pub async fn init_empty(&self, filesystem: Arc<dyn Filesystem>) -> Result<(), Error> {
        // The following constants are only used at format time. When mounting, the recorded values
        // in the superblock should be used.  The root parent store does not have a parent, but
        // needs an object ID to be registered with ObjectManager, so it cannot collide (i.e. have
        // the same object ID) with any objects in the root store that use the journal to track
        // mutations.
        const INIT_ROOT_PARENT_STORE_OBJECT_ID: u64 = 2;
        const INIT_ROOT_STORE_OBJECT_ID: u64 = 3;
        const INIT_ALLOCATOR_OBJECT_ID: u64 = 4;

        let checkpoint = self.writer.lock().await.journal_file_checkpoint();

        let root_parent =
            ObjectStore::new_empty(None, INIT_ROOT_PARENT_STORE_OBJECT_ID, filesystem.clone());
        self.objects.set_root_parent_store_object_id(root_parent.store_object_id());

        let allocator =
            Arc::new(SimpleAllocator::new(filesystem.clone(), INIT_ALLOCATOR_OBJECT_ID, true));
        self.objects.set_allocator(allocator.clone());

        let journal_handle;
        let super_block_handle;
        let mut transaction = filesystem.new_transaction(&[]).await?;
        let root_store = root_parent
            .create_child_store_with_id(&mut transaction, INIT_ROOT_STORE_OBJECT_ID)
            .await?;
        self.objects.set_root_store_object_id(root_store.store_object_id());

        // Create the super-block object...
        super_block_handle = root_store
            .create_object_with_id(
                &mut transaction,
                SUPER_BLOCK_OBJECT_ID,
                HandleOptions { overwrite: true, ..Default::default() },
            )
            .await?;
        super_block_handle.extend(&mut transaction, super_block::first_extent()).await?;

        // and the journal object...
        journal_handle =
            root_parent.create_object(&mut transaction, journal_handle_options()).await?;
        journal_handle.preallocate_range(&mut transaction, 0..self.chunk_size()).await?;
        self.commit(transaction).await;

        // Cache the super-block.
        self.inner.lock().unwrap().super_block = SuperBlock::new(
            root_parent.store_object_id(),
            root_store.store_object_id(),
            allocator.object_id(),
            journal_handle.object_id(),
            checkpoint,
        );

        // Initialize the journal writer.
        self.writer.lock().await.set_handle(journal_handle);
        Ok(())
    }

    /// Commits a transaction.
    pub async fn commit(&self, mut transaction: Transaction<'_>) {
        if transaction.is_empty() {
            return;
        }
        let mut writer = self.writer.lock().await;
        // TODO(csuter): handle the case where we are unable to extend the journal file.
        self.maybe_extend_journal_file(&mut writer).await.unwrap();
        // TODO(csuter): writing to the journal here can be asynchronous.
        let journal_file_checkpoint = writer.journal_file_checkpoint();
        writer.write_mutations(transaction.mutations.iter().cloned());
        if let Err(e) = writer.maybe_flush_buffer().await {
            // TODO(csuter): if writes to the journal start failing then we should prevent the
            // creation of new transactions.
            log::warn!("journal write failed: {}", e);
        }
        self.apply_mutations(std::mem::take(&mut transaction.mutations), journal_file_checkpoint)
            .await;
    }

    pub fn root_volume_info_object_id(&self) -> u64 {
        self.inner.lock().unwrap().super_block.root_volume_info_object_id
    }

    pub fn set_root_volume_info_object_id(&self, object_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.super_block.root_volume_info_object_id = object_id;
        inner.needs_super_block = true;
    }

    async fn maybe_extend_journal_file(
        &self,
        writer: &mut JournalWriter<StoreObjectHandle>,
    ) -> Result<(), Error> {
        // TODO(csuter): this currently assumes that a transaction can fit in CHUNK_SIZE.
        let file_offset = writer.journal_file_checkpoint().file_offset;
        let handle = match writer.handle() {
            None => return Ok(()),
            Some(handle) => handle,
        };
        let size = handle.get_size();
        if file_offset + self.chunk_size() <= size {
            return Ok(());
        }
        let mut transaction = handle.new_transaction().await?;
        handle.preallocate_range(&mut transaction, size..size + self.chunk_size()).await?;
        let journal_file_checkpoint = writer.journal_file_checkpoint();

        // We have to apply the mutations before writing them because we borrowed the writer for the
        // transaction.  First we clone the mutations without the associated objects since that's
        // where the handle is borrowed.
        let cloned_mutations: Vec<_> = transaction
            .mutations
            .iter()
            .map(|m| TxnMutation {
                object_id: m.object_id,
                mutation: m.mutation.clone(),
                associated_object: None,
            })
            .collect();

        self.apply_mutations(std::mem::take(&mut transaction.mutations), journal_file_checkpoint)
            .await;

        std::mem::drop(transaction);
        writer.write_mutations(cloned_mutations);

        // We need to be sure that any journal records that arose from preallocation can fit in
        // within the old preallocated range.  If this situation arose (it shouldn't, so it would be
        // a bug if it did), then it could be fixed (e.g. by fsck) by forcing a sync of the root
        // store.
        assert!(writer.journal_file_checkpoint().file_offset <= size);
        let file_offset = writer.journal_file_checkpoint().file_offset;
        let handle = writer.handle().unwrap();
        assert!(file_offset + self.chunk_size() <= handle.get_size());
        Ok(())
    }

    async fn apply_mutations(
        &self,
        mutations: impl IntoIterator<Item = TxnMutation<'_>>,
        journal_file_checkpoint: JournalCheckpoint,
    ) {
        log::debug!("BEGIN TXN {}", journal_file_checkpoint.file_offset);
        for TxnMutation { object_id, mutation, associated_object } in mutations {
            self.apply_mutation(
                object_id,
                &journal_file_checkpoint,
                mutation,
                false,
                associated_object,
            )
            .await;
        }
        log::debug!("END TXN");
    }

    // Determines whether a mutation at the given checkpoint should be applied.  During replay, not
    // all records should be applied because the object store or allocator might already contain the
    // mutation.  After replay, that obviously isn't the case and we want to apply all mutations.
    // Regardless, we want to keep track of the earliest mutation in the journal for a given object.
    fn should_apply(&self, object_id: u64, journal_file_checkpoint: &JournalCheckpoint) -> bool {
        let super_block = &self.inner.lock().unwrap().super_block;
        let offset = super_block
            .journal_file_offsets
            .get(&object_id)
            .cloned()
            .unwrap_or(super_block.super_block_journal_file_offset);
        journal_file_checkpoint.file_offset >= offset
    }

    async fn apply_mutation(
        &self,
        object_id: u64,
        journal_file_checkpoint: &JournalCheckpoint,
        mutation: Mutation,
        filter: bool,
        object: Option<AssociatedObject<'_>>,
    ) {
        if !filter || self.should_apply(object_id, journal_file_checkpoint) {
            log::debug!("applying mutation: {}: {:?}, filter: {}", object_id, mutation, filter);
            self.objects
                .apply_mutation(object_id, mutation, filter, journal_file_checkpoint, object)
                .await;
        } else {
            log::debug!("ignoring mutation: {}, {:?}", object_id, mutation);
        }
    }

    async fn write_super_block(
        &self,
        journal_file_checkpoint: JournalCheckpoint,
    ) -> Result<(), Error> {
        let root_parent_store = self.objects.root_parent_store();
        let sync = self.objects.begin_object_sync(root_parent_store.store_object_id());
        let super_block = self.inner.lock().unwrap().super_block.clone();
        let (journal_file_offsets, min_checkpoint) = self.objects.journal_file_offsets();

        let mut new_super_block = super_block.clone();
        new_super_block.super_block_journal_file_offset = journal_file_checkpoint.file_offset;
        new_super_block.journal_checkpoint = min_checkpoint.unwrap_or(journal_file_checkpoint);
        new_super_block.journal_file_offsets = journal_file_offsets;

        new_super_block
            .write(
                &root_parent_store,
                self.objects
                    .root_store()
                    .open_object(SUPER_BLOCK_OBJECT_ID, journal_handle_options())
                    .await?,
            )
            .await?;

        self.inner.lock().unwrap().super_block = new_super_block;
        sync.commit();
        Ok(())
    }

    pub async fn sync(&self, _options: SyncOptions) -> Result<(), Error> {
        // TODO(csuter): There needs to be some kind of locking here.
        let needs_super_block = self.inner.lock().unwrap().needs_super_block;
        if needs_super_block {
            let checkpoint = self.writer.lock().await.journal_file_checkpoint();
            self.write_super_block(checkpoint).await?;
            self.inner.lock().unwrap().needs_super_block = false;
        }
        let mut writer = self.writer.lock().await;
        serialize_into(&mut *writer, &JournalRecord::EndBlock)?;
        writer.pad_to_block()?;
        writer.maybe_flush_buffer().await?;
        Ok(())
    }

    fn block_size(&self) -> u64 {
        BLOCK_SIZE
    }

    fn chunk_size(&self) -> u64 {
        CHUNK_SIZE
    }
}

impl<OH> JournalWriter<OH> {
    // Extends JournalWriter to write a transaction.
    fn write_mutations<'a>(&mut self, mutations: impl IntoIterator<Item = TxnMutation<'a>>) {
        for TxnMutation { object_id, mutation, .. } in mutations {
            self.write_record(&JournalRecord::Mutation { object_id, mutation });
        }
        self.write_record(&JournalRecord::Commit);
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            object_handle::{ObjectHandle, ObjectHandleExt},
            object_store::{
                filesystem::{FxFilesystem, SyncOptions},
                transaction::TransactionHandler,
                HandleOptions,
            },
            testing::fake_device::FakeDevice,
        },
        fuchsia_async as fasync,
        std::sync::Arc,
    };

    const TEST_DEVICE_BLOCK_SIZE: u32 = 512;

    #[fasync::run_singlethreaded(test)]
    async fn test_replay() {
        const TEST_DATA: &[u8] = b"hello";

        let device = Arc::new(FakeDevice::new(2048, TEST_DEVICE_BLOCK_SIZE));

        let object_id = {
            let fs = FxFilesystem::new_empty(device.clone()).await.expect("new_empty failed");
            let mut transaction =
                fs.clone().new_transaction(&[]).await.expect("new_transaction failed");
            let handle = fs
                .root_store()
                .create_object(&mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed");
            transaction.commit().await;
            let mut buf = handle.allocate_buffer(TEST_DATA.len());
            buf.as_mut_slice().copy_from_slice(TEST_DATA);
            handle.write(0, buf.as_ref()).await.expect("write failed");
            // As this is the first sync, this will actually trigger a new super-block, but normally
            // this would not be the case.
            fs.sync(SyncOptions::default()).await.expect("sync failed");
            handle.object_id()
        };

        {
            let fs = FxFilesystem::open(device).await.expect("open failed");
            let handle = fs
                .root_store()
                .open_object(object_id, HandleOptions::default())
                .await
                .expect("create_object failed");
            let mut buf = handle.allocate_buffer(TEST_DEVICE_BLOCK_SIZE as usize);
            assert_eq!(handle.read(0, buf.as_mut()).await.expect("read failed"), TEST_DATA.len());
            assert_eq!(&buf.as_slice()[..TEST_DATA.len()], TEST_DATA);
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_reset() {
        const TEST_DATA: &[u8] = b"hello";

        let device = Arc::new(FakeDevice::new(4096, TEST_DEVICE_BLOCK_SIZE));

        let mut object_ids = Vec::new();

        {
            let fs = FxFilesystem::new_empty(device.clone()).await.expect("new_empty failed");
            let mut transaction =
                fs.clone().new_transaction(&[]).await.expect("new_transaction failed");
            let handle = fs
                .root_store()
                .create_object(&mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed");
            transaction.commit().await;
            let mut buf = handle.allocate_buffer(TEST_DATA.len());
            buf.as_mut_slice().copy_from_slice(TEST_DATA);
            handle.write(0, buf.as_ref()).await.expect("write failed");
            fs.sync(SyncOptions::default()).await.expect("sync failed");
            object_ids.push(handle.object_id());

            // Create a lot of objects but don't sync at the end. This should leave the filesystem
            // with a half finished transaction that cannot be replayed.
            for _ in 0..1000 {
                let mut transaction =
                    fs.clone().new_transaction(&[]).await.expect("new_transaction failed");
                let handle = fs
                    .root_store()
                    .create_object(&mut transaction, HandleOptions::default())
                    .await
                    .expect("create_object failed");
                transaction.commit().await;
                let mut buf = handle.allocate_buffer(TEST_DATA.len());
                buf.as_mut_slice().copy_from_slice(TEST_DATA);
                handle.write(0, buf.as_ref()).await.expect("write failed");
                object_ids.push(handle.object_id());
            }
        }

        {
            let fs = FxFilesystem::open(device.clone()).await.expect("open failed");

            // Check the first two objects which should exist.
            for &object_id in &object_ids[0..1] {
                let handle = fs
                    .root_store()
                    .open_object(object_id, HandleOptions::default())
                    .await
                    .expect("create_object failed");
                let mut buf = handle.allocate_buffer(TEST_DEVICE_BLOCK_SIZE as usize);
                assert_eq!(
                    handle.read(0, buf.as_mut()).await.expect("read failed"),
                    TEST_DATA.len()
                );
                assert_eq!(&buf.as_slice()[..TEST_DATA.len()], TEST_DATA);
            }

            // Write one more object and sync.
            let mut transaction =
                fs.clone().new_transaction(&[]).await.expect("new_transaction failed");
            let handle = fs
                .root_store()
                .create_object(&mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed");
            transaction.commit().await;
            let mut buf = handle.allocate_buffer(TEST_DATA.len());
            buf.as_mut_slice().copy_from_slice(TEST_DATA);
            handle.write(0, buf.as_ref()).await.expect("write failed");
            fs.sync(SyncOptions::default()).await.expect("sync failed");
            object_ids.push(handle.object_id());
        }

        {
            let fs = FxFilesystem::open(device).await.expect("open failed");

            // Check the first two and the last objects.
            for &object_id in object_ids[0..1].iter().chain(object_ids.last().cloned().iter()) {
                let handle = fs
                    .root_store()
                    .open_object(object_id, HandleOptions::default())
                    .await
                    .expect("create_object failed");
                let mut buf = handle.allocate_buffer(TEST_DEVICE_BLOCK_SIZE as usize);
                assert_eq!(
                    handle.read(0, buf.as_mut()).await.expect("read failed"),
                    TEST_DATA.len()
                );
                assert_eq!(&buf.as_slice()[..TEST_DATA.len()], TEST_DATA);
            }
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_volume_info_object_id() {
        let device = Arc::new(FakeDevice::new(4096, TEST_DEVICE_BLOCK_SIZE));

        {
            let fs = FxFilesystem::new_empty(device.clone()).await.expect("new_empty failed");
            fs.set_root_volume_info_object_id(567);
            fs.sync(SyncOptions::default()).await.expect("sync failed");
        }

        {
            let fs = FxFilesystem::open(device).await.expect("open failed");
            assert_eq!(fs.root_volume_info_object_id(), 567);
        }
    }
}
