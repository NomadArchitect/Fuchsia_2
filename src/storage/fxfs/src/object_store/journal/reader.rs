// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        object_handle::ObjectHandle,
        object_store::journal::{fletcher64, Checksum, JournalCheckpoint, RESET_XOR},
    },
    anyhow::{bail, Error},
    bincode::deserialize_from,
    byteorder::{ByteOrder, LittleEndian},
    serde::Deserialize,
};

/// JournalReader supports reading from a journal file which consist of blocks that have a trailing
/// fletcher64 checksum in the last 8 bytes of each block.  The checksum takes the check-sum of the
/// preceding block as an input to the next block so that merely copying a block to a different
/// location will cause a checksum failure.  The serialization of a single record *must* fit within
/// a single block.
pub struct JournalReader<OH> {
    // The handle of the journal file that we are reading.
    handle: OH,

    // The block size for the journal file.
    block_size: u64,

    // A buffer containing the currently buffered data.
    buf: Vec<u8>,

    // The range within the buffer containing outstanding data.
    buf_range: std::ops::Range<usize>,

    // The next offset we should read from in handle.
    read_offset: u64,

    // The file offset for next record due to be deserialized.
    buf_file_offset: u64,

    // The checksums required for the currently buffered data.  The first checksum is for
    // the *preceding* block since that is what is required to generate a checkpoint.
    checksums: Vec<Checksum>,

    // Indicates a bad checksum has been detected and no more data can be read.
    bad_checksum: bool,

    // Indicates a reset checksum has been detected.
    found_reset: bool,
}

impl<OH: ObjectHandle> JournalReader<OH> {
    pub(super) fn new(handle: OH, block_size: u64, checkpoint: &JournalCheckpoint) -> Self {
        JournalReader {
            handle,
            block_size,
            buf: Vec::new(),
            buf_range: 0..0,
            read_offset: checkpoint.file_offset - checkpoint.file_offset % block_size,
            buf_file_offset: checkpoint.file_offset,
            checksums: vec![checkpoint.checksum],
            bad_checksum: false,
            found_reset: false,
        }
    }

    pub(super) fn journal_file_checkpoint(&self) -> JournalCheckpoint {
        JournalCheckpoint::new(self.buf_file_offset, self.checksums[0])
    }

    pub fn read_offset(&self) -> u64 {
        self.read_offset
    }

    pub fn last_read_checksum(&self) -> Checksum {
        *self.checksums.last().unwrap()
    }

    /// To allow users to flush a block, they can store a record that indicates the rest of the
    /// block should be skipped.  When that record is read, users should call this function.
    pub fn skip_to_end_of_block(&mut self) {
        let block_offset = self.buf_file_offset % self.block_size;
        if block_offset > 0 {
            self.consume(
                (self.block_size - block_offset) as usize - std::mem::size_of::<Checksum>(),
            );
        }
    }

    pub fn take_handle(self) -> OH {
        self.handle
    }

    pub fn handle(&mut self) -> &mut OH {
        &mut self.handle
    }

    /// Tries to deserialize a record of type T from the journal stream.  It might return
    /// ReadResult::Reset if a reset marker is encountered, or ReadResult::ChecksumMismatch (which
    /// is expected at the end of the journal stream).
    pub async fn deserialize<T>(&mut self) -> Result<ReadResult<T>, Error>
    where
        for<'de> T: Deserialize<'de>,
    {
        self.fill_buf().await?;
        let mut cursor = std::io::Cursor::new(self.buffer());
        match deserialize_from(&mut cursor) {
            Ok(record) => {
                let consumed = cursor.position() as usize;
                self.consume(consumed);
                Ok(ReadResult::Some(record))
            }
            Err(e) => {
                if self.found_reset {
                    self.found_reset = false;

                    // Fix up buf_range now...
                    self.consume(self.buf_range.end - self.buf_range.start);
                    self.buf_range = self.buf.len() - self.block_size as usize
                        ..self.buf.len() - std::mem::size_of::<Checksum>();
                    return Ok(ReadResult::Reset);
                } else if let bincode::ErrorKind::Io(io_error) = &*e {
                    if io_error.kind() == std::io::ErrorKind::UnexpectedEof && self.bad_checksum {
                        return Ok(ReadResult::ChecksumMismatch);
                    }
                }
                Err(e.into())
            }
        }
    }

    // Fills the buffer so that at least one blocks worth of data is contained within the buffer.
    // After reading a block, it verifies the checksum.  Once done, it should be possible to
    // deserialize records.
    async fn fill_buf(&mut self) -> Result<(), Error> {
        let bs = self.block_size as usize;

        if self.found_reset || self.bad_checksum || self.buf_range.end - self.buf_range.start > bs {
            return Ok(());
        }

        // Before we read, shuffle existing data to the beginning of the buffer.
        self.buf.copy_within(self.buf_range.clone(), 0);
        self.buf_range = 0..self.buf_range.end - self.buf_range.start;

        while self.buf_range.end - self.buf_range.start < bs {
            // Read the next block's worth.
            self.buf.resize(self.buf_range.end + bs, 0);
            let last_read_checksum = self.last_read_checksum();
            let slice = &mut self.buf[self.buf_range.end..];
            if self.handle.read(self.read_offset as u64, slice).await? != bs {
                // This shouldn't happen -- it shouldn't be possible to read to the end
                // of the journal file.
                bail!("unexpected end of journal file");
            }

            // Now check the checksum.
            let (contents_slice, checksum_slice) =
                slice.split_at(bs - std::mem::size_of::<Checksum>());
            let stored_checksum = LittleEndian::read_u64(checksum_slice);
            let computed_checksum = fletcher64(contents_slice, last_read_checksum);
            if stored_checksum != computed_checksum {
                if stored_checksum ^ RESET_XOR == computed_checksum
                    && self.buf_file_offset <= self.read_offset
                {
                    // Record that we've encountered a reset in the stream (a point where the
                    // journal wasn't cleanly closed in the past) and it starts afresh in this
                    // block.  We don't adjust buf_range until the reset has been processed, but we
                    // do push the checksum.
                    self.found_reset = true;
                    self.checksums.push(stored_checksum);
                    self.read_offset += self.block_size;
                } else {
                    self.bad_checksum = true;
                }
                return Ok(());
            }

            self.checksums.push(stored_checksum);

            // If this was our first read, our buffer offset might be somewhere within the middle of
            // the block, so we need to adjust buf_range accordingly.
            if self.buf_file_offset > self.read_offset {
                assert!(self.buf_range.start == 0);
                self.buf_range = (self.buf_file_offset - self.read_offset) as usize
                    ..self.buf.len() - std::mem::size_of::<Checksum>();
            } else {
                self.buf_range =
                    self.buf_range.start..self.buf.len() - std::mem::size_of::<Checksum>();
            }
            self.read_offset += self.block_size;
        }
        Ok(())
    }

    // Used after deserializing to indicate how many bytes were deserialized and adjusts the buffer
    // pointers accordingly.
    fn consume(&mut self, amount: usize) {
        assert!(amount < self.block_size as usize);
        let block_offset_before = self.buf_file_offset % self.block_size;
        self.buf_file_offset += amount as u64;
        // If we crossed a block boundary, then the file offset needs be incremented by the size of
        // the checksum.
        if block_offset_before + amount as u64
            >= self.block_size - std::mem::size_of::<Checksum>() as u64
        {
            self.buf_file_offset += std::mem::size_of::<Checksum>() as u64;
            self.checksums.drain(0..1);
        }
        self.buf_range = self.buf_range.start + amount..self.buf_range.end;
    }

    fn buffer(&self) -> &[u8] {
        &self.buf[self.buf_range.clone()]
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReadResult<T> {
    Reset,
    Some(T),
    ChecksumMismatch,
}

#[cfg(test)]
mod tests {
    // The following tests use JournalWriter to test our reader implementation. This works so long
    // as JournalWriter doesn't use JournalReader to test its implementation.
    use {
        super::{JournalReader, ReadResult},
        crate::{
            object_handle::ObjectHandle,
            object_store::journal::{writer::JournalWriter, JournalCheckpoint},
            testing::fake_object::{FakeObject, FakeObjectHandle},
        },
        fuchsia_async as fasync,
        serde::Serialize,
        std::sync::{Arc, Mutex},
    };

    const TEST_BLOCK_SIZE: u64 = 512;

    async fn write_items<T: Serialize>(handle: FakeObjectHandle, items: &[T]) {
        let mut writer = JournalWriter::new(Some(handle), TEST_BLOCK_SIZE as usize);
        for item in items {
            writer.write_record(item);
        }
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_read_single_record() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        object.lock().unwrap().write(0, &[0; TEST_BLOCK_SIZE as usize * 2]).expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32]).await;

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            &JournalCheckpoint::default(),
        );
        let value = reader.deserialize().await.expect("deserialize failed");
        assert_eq!(value, ReadResult::Some(4u32));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_journal_file_checkpoint_and_take_handle() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            &JournalCheckpoint::default(),
        );
        assert_eq!(reader.journal_file_checkpoint(), JournalCheckpoint::default());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        object.lock().unwrap().write(0, &[0; TEST_BLOCK_SIZE as usize * 2]).expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32, 7u32]).await;

        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));

        // If we take the checkpoint here and then create another reader, we should see the second
        // item.
        let checkpoint = reader.journal_file_checkpoint();
        let mut reader = JournalReader::new(reader.take_handle(), TEST_BLOCK_SIZE, &checkpoint);
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(7u32));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_read_offset() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            &JournalCheckpoint::default(),
        );
        assert_eq!(reader.read_offset(), 0);
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        object.lock().unwrap().write(0, &[0; TEST_BLOCK_SIZE as usize * 2]).expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32, 7u32]).await;
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));
        assert_eq!(reader.read_offset(), TEST_BLOCK_SIZE);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_skip_to_end_of_block() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        object.lock().unwrap().write(0, &[0; TEST_BLOCK_SIZE as usize * 3]).expect("write failed");
        let mut writer = JournalWriter::new(
            Some(FakeObjectHandle::new(object.clone())),
            TEST_BLOCK_SIZE as usize,
        );
        writer.write_record(&4u32);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.write_record(&7u32);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");
        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            &JournalCheckpoint::default(),
        );
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));
        reader.skip_to_end_of_block();
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(7u32));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handle() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        object.lock().unwrap().write(0, &[0; TEST_BLOCK_SIZE as usize * 3]).expect("write failed");
        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            &JournalCheckpoint::default(),
        );
        assert_eq!(reader.handle().get_size(), TEST_BLOCK_SIZE * 3);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_item_spanning_block() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        object.lock().unwrap().write(0, &[0; TEST_BLOCK_SIZE as usize * 3]).expect("write failed");
        let mut writer = JournalWriter::new(
            Some(FakeObjectHandle::new(object.clone())),
            TEST_BLOCK_SIZE as usize,
        );
        // Write one byte so that everything else is misaligned.
        writer.write_record(&4u8);
        let mut count: i32 = 0;
        while writer.journal_file_checkpoint().file_offset < TEST_BLOCK_SIZE {
            writer.write_record(&12345678u32);
            count += 1;
        }
        // Check that writing didn't end up being aligned on a block.
        assert_ne!(writer.journal_file_checkpoint().file_offset, TEST_BLOCK_SIZE);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            &JournalCheckpoint::default(),
        );
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u8));
        for _ in 0..count {
            assert_eq!(
                reader.deserialize().await.expect("deserialize failed"),
                ReadResult::Some(12345678u32)
            );
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_reset() {
        let object = Arc::new(Mutex::new(FakeObject::new()));
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        object.lock().unwrap().write(0, &[0; TEST_BLOCK_SIZE as usize * 3]).expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32, 7u32]).await;

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            &JournalCheckpoint::default(),
        );
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(7u32));
        reader.skip_to_end_of_block();
        assert_eq!(
            reader.deserialize::<u32>().await.expect("deserialize failed"),
            ReadResult::ChecksumMismatch
        );

        let mut writer = JournalWriter::new(None, TEST_BLOCK_SIZE as usize);
        writer.set_handle(
            FakeObjectHandle::new(object.clone()),
            TEST_BLOCK_SIZE,
            reader.last_read_checksum(),
            true,
        );
        writer.write_record(&13u32);
        writer.pad_to_block().expect("pad_to_block failed");
        writer.maybe_flush_buffer().await.expect("flush_buffer failed");

        let checkpoint = reader.journal_file_checkpoint();
        let mut reader = JournalReader::new(reader.take_handle(), TEST_BLOCK_SIZE, &checkpoint);
        assert_eq!(
            reader.deserialize::<u32>().await.expect("deserialize failed"),
            ReadResult::Reset
        );
        assert_eq!(
            reader.deserialize().await.expect("deserialize failed"),
            ReadResult::Some(13u32)
        );
    }
}
