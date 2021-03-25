// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        object_handle::ObjectHandle,
        object_store::journal::{fletcher64, Checksum, JournalCheckpoint, RESET_XOR},
    },
    anyhow::Error,
    bincode::serialize_into,
    byteorder::{LittleEndian, WriteBytesExt},
    serde::Serialize,
    std::{cmp::min, io::Write},
};

/// JournalWriter is responsible for writing log records to a journal file.  Each block contains a
/// fletcher64 checksum at the end of the block.  This is used by both the main journal file and the
/// super-block.
pub struct JournalWriter<OH> {
    // The handle to write to.  We allow lazy initialisation so that a writer can be instantiated
    // before replay has completed.
    handle: Option<OH>,

    // The block size used for this journal file.
    block_size: usize,

    // The offset in the file.
    offset: u64,

    // The last checksum we wrote.
    last_checksum: Checksum,

    // The buffered data for the current block.
    buf: Vec<u8>,

    // If true, the next block we write should indicate the stream was reset.
    reset: bool,
}

impl<OH> JournalWriter<OH> {
    pub fn new(handle: Option<OH>, block_size: usize) -> Self {
        JournalWriter {
            handle,
            block_size,
            offset: 0,
            last_checksum: 0,
            buf: Vec::new(),
            reset: false,
        }
    }

    /// Serializes a new journal record to the journal stream.
    pub fn write_record<T: Serialize>(&mut self, record: &T) {
        serialize_into(&mut *self, record).unwrap() // Our write implementation cannot fail at the
                                                    // moment.
    }

    /// Pads from the current offset in the buffer to the end of the block.
    pub fn pad_to_block(&mut self) -> std::io::Result<()> {
        let align = self.buf.len() % self.block_size;
        if align > 0 {
            self.write(&vec![0; self.block_size - std::mem::size_of::<Checksum>() - align])?;
        }
        Ok(())
    }

    /// Returns the checkpoint that corresponds to the current location in the journal stream
    /// assuming that it has been flushed.
    pub(super) fn journal_file_checkpoint(&self) -> JournalCheckpoint {
        JournalCheckpoint::new(self.offset + self.buf.len() as u64, self.last_checksum)
    }

    /// Flushes any outstanding complete blocks to the journal object.  Part blocks can be flushed
    /// by calling pad_to_block first.  Does nothing if no handle has been set yet.
    pub async fn maybe_flush_buffer(&mut self) -> Result<(), Error>
    where
        OH: ObjectHandle,
    {
        if let Some(ref handle) = self.handle {
            let to_do = self.buf.len() - self.buf.len() % self.block_size;
            if to_do > 0 {
                // TODO(jfsulliv): This is horribly inefficient. We should reuse the transfer
                // buffer. Doing so will require picking an appropriate size up front, and forcing
                // flush as we fill it up.
                let mut buf = handle.allocate_buffer(to_do);
                buf.as_mut_slice()[..to_do].copy_from_slice(self.buf.drain(..to_do).as_slice());
                buf.as_mut_slice()[to_do..].fill(0u8);
                handle.write(self.offset, buf.as_ref()).await?;
                self.offset += to_do as u64;
            }
        }
        Ok(())
    }

    /// Sets the handle, to be used once replay has finished.
    pub fn set_handle(&mut self, handle: OH, offset: u64, checksum: Checksum, reset: bool) {
        self.handle = Some(handle);
        self.offset = offset;
        self.last_checksum = checksum;
        self.reset = reset;
    }
}

impl<OH> std::io::Write for JournalWriter<OH> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut offset = 0;
        while offset < buf.len() {
            let space = self.block_size
                - std::mem::size_of::<Checksum>()
                - self.buf.len() % self.block_size;
            let to_copy = min(space, buf.len() - offset);
            self.buf.write(&buf[offset..offset + to_copy])?;
            if to_copy == space {
                let end = self.buf.len();
                let start = end + std::mem::size_of::<Checksum>() - self.block_size;
                self.last_checksum = fletcher64(&self.buf[start..end], self.last_checksum);
                if self.reset {
                    self.last_checksum ^= RESET_XOR;
                    self.reset = false;
                }
                self.buf.write_u64::<LittleEndian>(self.last_checksum)?;
            }
            offset += to_copy;
        }
        Ok(buf.len())
    }

    // This does nothing because it's sync.  Users must call the async flush_buffer function to
    // flush outstanding data.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<OH> Drop for JournalWriter<OH> {
    fn drop(&mut self) {
        // If this message is logged it means we forgot to call flush_buffer(), which in turn might
        // mean Journal::sync() was not called.
        if self.buf.len() > 0 {
            log::warn!("journal data dropped!");
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::JournalWriter,
        crate::{
            object_handle::ObjectHandle,
            object_store::journal::{fletcher64, Checksum, JournalCheckpoint, RESET_XOR},
            testing::fake_object::{FakeObject, FakeObjectHandle},
        },
        bincode::deserialize_from,
        byteorder::{ByteOrder, LittleEndian},
        fuchsia_async as fasync,
        std::sync::{Arc, Mutex},
    };

    const TEST_BLOCK_SIZE: usize = 512;

    #[fasync::run_singlethreaded(test)]
    async fn test_write_single_record_and_pad() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        let mut writer =
            JournalWriter::new(Some(FakeObjectHandle::new(object.clone())), TEST_BLOCK_SIZE);
        writer.write_record(&4u32);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");

        let handle = FakeObjectHandle::new(object.clone());
        let mut buf = handle.allocate_buffer(object.lock().unwrap().get_size() as usize);
        assert_eq!(buf.len(), TEST_BLOCK_SIZE);
        handle.read(0, buf.as_mut()).await.expect("read failed");
        let value: u32 = deserialize_from(buf.as_slice()).expect("deserialize_from failed");
        assert_eq!(value, 4u32);
        let (payload, checksum_slice) =
            buf.as_slice().split_at(buf.len() - std::mem::size_of::<Checksum>());
        let checksum = LittleEndian::read_u64(checksum_slice);
        assert_eq!(checksum, fletcher64(payload, 0));
        assert_eq!(
            writer.journal_file_checkpoint(),
            JournalCheckpoint { file_offset: TEST_BLOCK_SIZE as u64, checksum }
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_journal_file_checkpoint() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        let mut writer =
            JournalWriter::new(Some(FakeObjectHandle::new(object.clone())), TEST_BLOCK_SIZE);
        writer.write_record(&4u32);
        let checkpoint = writer.journal_file_checkpoint();
        assert_eq!(checkpoint.checksum, 0);
        writer.write_record(&17u64);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");

        let handle = FakeObjectHandle::new(object.clone());
        let mut buf = handle.allocate_buffer(object.lock().unwrap().get_size() as usize);
        assert_eq!(buf.len(), TEST_BLOCK_SIZE);
        handle.read(0, buf.as_mut()).await.expect("read failed");
        let value: u64 = deserialize_from(&buf.as_slice()[checkpoint.file_offset as usize..])
            .expect("deserialize_from failed");
        assert_eq!(value, 17);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_set_handle() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        let mut writer = JournalWriter::new(None, TEST_BLOCK_SIZE);
        writer.set_handle(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE as u64 * 5,
            12345,
            false,
        );
        writer.write_record(&12);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");

        let handle = FakeObjectHandle::new(object.clone());
        let mut buf = handle.allocate_buffer(object.lock().unwrap().get_size() as usize);
        assert_eq!(buf.len(), TEST_BLOCK_SIZE * 6);
        handle.read(0, buf.as_mut()).await.expect("read failed");
        let (first_5_blocks, last_block) = buf.as_slice().split_at(TEST_BLOCK_SIZE * 5);
        assert_eq!(first_5_blocks, &vec![0u8; TEST_BLOCK_SIZE * 5]);
        let value: u64 =
            deserialize_from(&last_block[..TEST_BLOCK_SIZE]).expect("deserialize_from failed");
        assert_eq!(value, 12);
        let (payload, checksum_slice) =
            last_block.split_at(last_block.len() - std::mem::size_of::<Checksum>());
        let checksum = LittleEndian::read_u64(checksum_slice);
        assert_eq!(checksum, fletcher64(payload, 12345));
        assert_eq!(
            writer.journal_file_checkpoint(),
            JournalCheckpoint { file_offset: TEST_BLOCK_SIZE as u64 * 6, checksum }
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_set_reset() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        let mut writer = JournalWriter::new(None, TEST_BLOCK_SIZE);
        writer.set_handle(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE as u64 * 5,
            12345,
            true,
        );
        writer.write_record(&12);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");

        let handle = FakeObjectHandle::new(object.clone());
        let mut buf = handle.allocate_buffer(object.lock().unwrap().get_size() as usize);
        assert_eq!(buf.len(), TEST_BLOCK_SIZE * 6);
        handle.read(0, buf.as_mut()).await.expect("read failed");
        let (first_5_blocks, last_block) = buf.as_slice().split_at(TEST_BLOCK_SIZE * 5);
        assert_eq!(first_5_blocks, &vec![0u8; TEST_BLOCK_SIZE * 5]);
        let value: u64 =
            deserialize_from(&last_block[..TEST_BLOCK_SIZE]).expect("deserialize_from failed");
        assert_eq!(value, 12);
        let (payload, checksum_slice) =
            last_block.split_at(last_block.len() - std::mem::size_of::<Checksum>());
        let checksum = LittleEndian::read_u64(checksum_slice);
        assert_eq!(checksum, fletcher64(payload, 12345) ^ RESET_XOR);
        assert_eq!(
            writer.journal_file_checkpoint(),
            JournalCheckpoint { file_offset: TEST_BLOCK_SIZE as u64 * 6, checksum }
        );
    }
}
