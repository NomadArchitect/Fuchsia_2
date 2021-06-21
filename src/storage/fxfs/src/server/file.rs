// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        object_handle::ObjectHandle,
        object_store::{
            filesystem::SyncOptions, round_down, transaction::Options, StoreObjectHandle, Timestamp,
        },
        server::{directory::FxDirectory, errors::map_to_status, node::FxNode, volume::FxVolume},
    },
    anyhow::Error,
    async_trait::async_trait,
    fidl::endpoints::ServerEnd,
    fidl_fuchsia_io::{self as fio, NodeAttributes, NodeMarker},
    fidl_fuchsia_mem::Buffer,
    fuchsia_zircon::Status,
    std::{
        any::Any,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    },
    storage_device::buffer::MutableBufferRef,
    vfs::{
        common::send_on_open_with_error,
        directory::entry::{DirectoryEntry, EntryInfo},
        execution_scope::ExecutionScope,
        file::{
            connection::{self, io1::FileConnection},
            File, SharingMode,
        },
        filesystem::Filesystem,
        path::Path,
    },
};

/// FxFile represents an open connection to a file.
pub struct FxFile {
    handle: StoreObjectHandle<FxVolume>,
    open_count: AtomicUsize,
}

impl FxFile {
    pub fn new(handle: StoreObjectHandle<FxVolume>) -> Self {
        Self { handle, open_count: AtomicUsize::new(0) }
    }

    pub fn open_count(&self) -> usize {
        self.open_count.load(Ordering::Relaxed)
    }

    async fn write_or_append(
        &self,
        offset: Option<u64>,
        content: &[u8],
    ) -> Result<(u64, u64), Error> {
        // We must create the transaction first so that we lock the size in the case that this is
        // append.
        let mut transaction = self.handle.new_transaction().await?;
        let offset = offset.unwrap_or_else(|| self.handle.get_size());
        let start = round_down(offset, self.handle.block_size());
        let align = (offset - start) as usize;
        let mut buf = self.handle.allocate_buffer(align + content.len());
        buf.as_mut_slice()[align..].copy_from_slice(content);
        self.handle.txn_write(&mut transaction, offset, buf.subslice(align..)).await?;
        transaction.commit().await;
        Ok((content.len() as u64, offset + content.len() as u64))
    }
}

impl Drop for FxFile {
    fn drop(&mut self) {
        self.handle.owner().cache().remove(self.object_id());
    }
}

impl FxNode for FxFile {
    fn object_id(&self) -> u64 {
        self.handle.object_id()
    }

    fn parent(&self) -> Option<Arc<FxDirectory>> {
        unreachable!(); // Add a parent back-reference if needed.
    }

    fn set_parent(&self, _parent: Arc<FxDirectory>) {
        // NOP
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }

    fn try_into_directory_entry(self: Arc<Self>) -> Option<Arc<dyn DirectoryEntry>> {
        Some(self)
    }
}

impl DirectoryEntry for FxFile {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        flags: u32,
        mode: u32,
        path: Path,
        server_end: ServerEnd<NodeMarker>,
    ) {
        if !path.is_empty() {
            send_on_open_with_error(flags, server_end, Status::NOT_FILE);
            return;
        }
        // Since close decrements open_count, we need to increment it here as we create OpenFile
        // since it will call close when dropped.
        self.open_count.fetch_add(1, Ordering::Relaxed);
        FileConnection::<FxFile>::create_connection(
            // Note readable/writable do not override what's set in flags, they merely tell the
            // FileConnection that it's valid to open the file readable/writable.
            scope.clone(),
            connection::util::OpenFile::new(self, scope),
            flags,
            mode,
            server_end,
            /*readable=*/ true,
            /*writable=*/ true,
        );
    }

    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(self.object_id(), fio::DIRENT_TYPE_FILE)
    }

    fn can_hardlink(&self) -> bool {
        true
    }
}

#[async_trait]
impl File for FxFile {
    async fn open(&self, _flags: u32) -> Result<(), Status> {
        Ok(())
    }

    async fn read_at(&self, offset: u64, buffer: MutableBufferRef<'_>) -> Result<u64, Status> {
        let bytes_read = self.handle.read(offset, buffer).await.map_err(map_to_status)?;
        Ok(bytes_read as u64)
    }

    async fn write_at(&self, offset: u64, content: &[u8]) -> Result<u64, Status> {
        self.write_or_append(Some(offset), content)
            .await
            .map(|(done, _)| done)
            .map_err(map_to_status)
    }

    async fn append(&self, content: &[u8]) -> Result<(u64, u64), Status> {
        self.write_or_append(None, content).await.map_err(map_to_status)
    }

    async fn truncate(&self, length: u64) -> Result<(), Status> {
        // It's safe to skip the space checks even if we're growing the file here because it won't
        // actually use any data on disk (either for data or metadata).
        let mut transaction = self
            .handle
            .new_transaction_with_options(Options {
                borrow_metadata_space: true,
                ..Default::default()
            })
            .await
            .map_err(map_to_status)?;
        self.handle.truncate(&mut transaction, length).await.map_err(map_to_status)?;
        transaction.commit().await;
        Ok(())
    }

    async fn get_buffer(&self, _mode: SharingMode, _flags: u32) -> Result<Option<Buffer>, Status> {
        log::error!("get_buffer not implemented");
        Err(Status::NOT_SUPPORTED)
    }

    async fn get_size(&self) -> Result<u64, Status> {
        Ok(self.handle.get_size())
    }

    async fn get_attrs(&self) -> Result<NodeAttributes, Status> {
        let props = self.handle.get_properties().await.map_err(map_to_status)?;
        // TODO(jfsulliv): This assumes that we always get the data attribute at index 0 of
        // |attribute_sizes|.
        Ok(NodeAttributes {
            mode: 0u32, // TODO(jfsulliv): Mode bits
            id: self.handle.object_id(),
            content_size: props.data_attribute_size,
            storage_size: props.allocated_size,
            link_count: props.refs,
            creation_time: props.creation_time.as_nanos(),
            modification_time: props.modification_time.as_nanos(),
        })
    }

    async fn set_attrs(
        &self,
        flags: u32,
        attrs: NodeAttributes,
        may_defer: bool,
    ) -> Result<(), Status> {
        let crtime = if flags & fidl_fuchsia_io::NODE_ATTRIBUTE_FLAG_CREATION_TIME > 0 {
            Some(Timestamp::from_nanos(attrs.creation_time))
        } else {
            None
        };
        let mtime = if flags & fidl_fuchsia_io::NODE_ATTRIBUTE_FLAG_MODIFICATION_TIME > 0 {
            Some(Timestamp::from_nanos(attrs.modification_time))
        } else {
            None
        };
        if let (None, None) = (crtime.as_ref(), mtime.as_ref()) {
            return Ok(());
        }
        let mut transaction = if may_defer {
            None
        } else {
            Some(
                self.handle
                    .new_transaction_with_options(Options {
                        borrow_metadata_space: true,
                        ..Default::default()
                    })
                    .await
                    .map_err(map_to_status)?,
            )
        };
        self.handle
            .update_timestamps(transaction.as_mut(), crtime, mtime)
            .await
            .map_err(map_to_status)?;
        if let Some(t) = transaction {
            t.commit().await;
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), Status> {
        assert!(self.open_count.fetch_sub(1, Ordering::Relaxed) > 0);
        Ok(())
    }

    async fn sync(&self) -> Result<(), Status> {
        // TODO(csuter): at the moment, this doesn't send a flush to the device, which doesn't
        // match minfs.
        self.handle.store().filesystem().sync(SyncOptions::default()).await.map_err(map_to_status)
    }

    fn get_filesystem(&self) -> &dyn Filesystem {
        self.handle.owner().as_ref()
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            object_handle::INVALID_OBJECT_ID,
            object_store,
            server::testing::{close_file_checked, open_file_checked, TestFixture},
        },
        fidl_fuchsia_io::{
            self as fio, SeekOrigin, MODE_TYPE_FILE, OPEN_FLAG_APPEND, OPEN_FLAG_CREATE,
            OPEN_RIGHT_READABLE, OPEN_RIGHT_WRITABLE,
        },
        fuchsia_async as fasync,
        fuchsia_zircon::Status,
        io_util::{read_file_bytes, write_file_bytes},
        storage_device::{fake_device::FakeDevice, DeviceHolder},
    };

    #[fasync::run_singlethreaded(test)]
    async fn test_empty_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file =
            open_file_checked(&root, OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo")
                .await;

        let (status, buf) = file.read(fio::MAX_BUF).await.expect("FIDL call failed");
        Status::ok(status).expect("read failed");
        assert!(buf.is_empty());

        let (status, attrs) = file.get_attr().await.expect("FIDL call failed");
        Status::ok(status).expect("get_attr failed");
        // TODO(jfsulliv): Check mode
        assert_ne!(attrs.id, INVALID_OBJECT_ID);
        assert_eq!(attrs.content_size, 0u64);
        assert_eq!(attrs.storage_size, 0u64);
        assert_eq!(attrs.link_count, 1u64);
        assert_ne!(attrs.creation_time, 0u64);
        assert_ne!(attrs.modification_time, 0u64);
        assert_eq!(attrs.creation_time, attrs.modification_time);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_set_attrs() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        let (status, initial_attrs) = file.get_attr().await.expect("FIDL call failed");
        Status::ok(status).expect("get_attr failed");

        let crtime = initial_attrs.creation_time ^ 1u64;
        let mtime = initial_attrs.modification_time ^ 1u64;

        let mut attrs = initial_attrs.clone();
        attrs.creation_time = crtime;
        attrs.modification_time = mtime;
        let status = file
            .set_attr(fidl_fuchsia_io::NODE_ATTRIBUTE_FLAG_CREATION_TIME, &mut attrs)
            .await
            .expect("FIDL call failed");
        Status::ok(status).expect("set_attr failed");

        let mut expected_attrs = initial_attrs.clone();
        expected_attrs.creation_time = crtime; // Only crtime is updated so far.
        let (status, attrs) = file.get_attr().await.expect("FIDL call failed");
        Status::ok(status).expect("get_attr failed");
        assert_eq!(expected_attrs, attrs);

        let mut attrs = initial_attrs.clone();
        attrs.creation_time = 0u64; // This should be ignored since we don't set the flag.
        attrs.modification_time = mtime;
        let status = file
            .set_attr(fidl_fuchsia_io::NODE_ATTRIBUTE_FLAG_MODIFICATION_TIME, &mut attrs)
            .await
            .expect("FIDL call failed");
        Status::ok(status).expect("set_attr failed");

        let mut expected_attrs = initial_attrs.clone();
        expected_attrs.creation_time = crtime;
        expected_attrs.modification_time = mtime;
        let (status, attrs) = file.get_attr().await.expect("FIDL call failed");
        Status::ok(status).expect("get_attr failed");
        assert_eq!(expected_attrs, attrs);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_write_read() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        let inputs = vec!["hello, ", "world!"];
        let expected_output = "hello, world!";
        for input in inputs {
            let (status, bytes_written) = file.write(input.as_bytes()).await.expect("write failed");
            Status::ok(status).expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
        }

        let (status, buf) = file.read_at(fio::MAX_BUF, 0).await.expect("read_at failed");
        Status::ok(status).expect("File read was successful");
        assert_eq!(buf.len(), expected_output.as_bytes().len());
        assert!(buf.iter().eq(expected_output.as_bytes().iter()));

        let (status, attrs) = file.get_attr().await.expect("FIDL call failed");
        Status::ok(status).expect("get_attr failed");
        assert_eq!(attrs.content_size, expected_output.as_bytes().len() as u64);
        assert_eq!(attrs.storage_size, object_store::MIN_BLOCK_SIZE as u64);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_writes_persist() {
        let mut device = DeviceHolder::new(FakeDevice::new(8192, 512));
        for i in 0..2 {
            let fixture = TestFixture::open(device, /*format=*/ i == 0).await;
            let root = fixture.root();

            let flags = if i == 0 {
                OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE
            } else {
                OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE
            };
            let file = open_file_checked(&root, flags, MODE_TYPE_FILE, "foo").await;

            if i == 0 {
                let (status, _) =
                    file.write(&vec![0xaa as u8; 8192]).await.expect("FIDL call failed");
                Status::ok(status).expect("File write was successful");
            } else {
                let (status, buf) = file.read(8192).await.expect("FIDL call failed");
                Status::ok(status).expect("File read was successful");
                assert_eq!(buf, vec![0xaa as u8; 8192]);
            }

            let (status, attrs) = file.get_attr().await.expect("FIDL call failed");
            Status::ok(status).expect("get_attr failed");
            assert_eq!(attrs.content_size, 8192u64);
            assert_eq!(attrs.storage_size, 8192u64);

            close_file_checked(file).await;
            device = fixture.close().await;
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_append() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let inputs = vec!["hello, ", "world!"];
        let expected_output = "hello, world!";
        for input in inputs {
            let file = open_file_checked(
                &root,
                OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE | OPEN_FLAG_APPEND,
                MODE_TYPE_FILE,
                "foo",
            )
            .await;

            let (status, bytes_written) =
                file.write(input.as_bytes()).await.expect("FIDL call failed");
            Status::ok(status).expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
        }

        let file = open_file_checked(&root, OPEN_RIGHT_READABLE, MODE_TYPE_FILE, "foo").await;
        let (status, buf) = file.read_at(fio::MAX_BUF, 0).await.expect("FIDL call failed");
        Status::ok(status).expect("File read was successful");
        assert_eq!(buf.len(), expected_output.as_bytes().len());
        assert!(buf.iter().eq(expected_output.as_bytes().iter()));

        let (status, attrs) = file.get_attr().await.expect("FIDL call failed");
        Status::ok(status).expect("get_attr failed");
        assert_eq!(attrs.content_size, expected_output.as_bytes().len() as u64);
        assert_eq!(attrs.storage_size, object_store::MIN_BLOCK_SIZE as u64);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        let input = "hello, world!";
        let (status, _bytes_written) =
            file.write(input.as_bytes()).await.expect("FIDL call failed");
        Status::ok(status).expect("File write was successful");

        {
            let (status, offset) = file.seek(0, SeekOrigin::Start).await.expect("FIDL call failed");
            assert_eq!(offset, 0);
            Status::ok(status).expect("seek was successful");
            let (status, buf) = file.read(5).await.expect("FIDL call failed");
            Status::ok(status).expect("File read was successful");
            assert!(buf.iter().eq("hello".as_bytes().into_iter()));
        }
        {
            let (status, offset) =
                file.seek(2, SeekOrigin::Current).await.expect("FIDL call failed");
            assert_eq!(offset, 7);
            Status::ok(status).expect("seek was successful");
            let (status, buf) = file.read(5).await.expect("FIDL call failed");
            Status::ok(status).expect("File read was successful");
            assert!(buf.iter().eq("world".as_bytes().into_iter()));
        }
        {
            let (status, offset) =
                file.seek(-5, SeekOrigin::Current).await.expect("FIDL call failed");
            assert_eq!(offset, 7);
            Status::ok(status).expect("seek was successful");
            let (status, buf) = file.read(5).await.expect("FIDL call failed");
            Status::ok(status).expect("File read was successful");
            assert!(buf.iter().eq("world".as_bytes().into_iter()));
        }
        {
            let (status, offset) = file.seek(-1, SeekOrigin::End).await.expect("FIDL call failed");
            assert_eq!(offset, 12);
            Status::ok(status).expect("seek was successful");
            let (status, buf) = file.read(1).await.expect("FIDL call failed");
            Status::ok(status).expect("File read was successful");
            assert!(buf.iter().eq("!".as_bytes().into_iter()));
        }

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_truncate_extend() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        let input = "hello, world!";
        let len: usize = 16 * 1024;

        let (status, _bytes_written) =
            file.write(input.as_bytes()).await.expect("FIDL call failed");
        Status::ok(status).expect("File write was successful");

        let (status, offset) = file.seek(0, SeekOrigin::Start).await.expect("FIDL call failed");
        assert_eq!(offset, 0);
        Status::ok(status).expect("Seek was successful");

        let status = file.truncate(len as u64).await.expect("FIDL call failed");
        Status::ok(status).expect("File truncate was successful");

        let mut expected_buf = vec![0 as u8; len];
        expected_buf[..input.as_bytes().len()].copy_from_slice(input.as_bytes());

        let buf = read_file_bytes(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        // Write something at the end of the gap.
        expected_buf[len - 1..].copy_from_slice("a".as_bytes());

        let (status, _bytes_written) =
            file.write_at("a".as_bytes(), (len - 1) as u64).await.expect("FIDL call failed");
        Status::ok(status).expect("File write was successful");

        let (status, offset) = file.seek(0, SeekOrigin::Start).await.expect("FIDL call failed");
        assert_eq!(offset, 0);
        Status::ok(status).expect("Seek was successful");

        let buf = read_file_bytes(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_truncate_shrink() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        let len: usize = 2 * 1024;
        let input = {
            let mut v = vec![0 as u8; len];
            for i in 0..v.len() {
                v[i] = ('a' as u8) + (i % 13) as u8;
            }
            v
        };
        let short_len: usize = 513;

        write_file_bytes(&file, &input).await.expect("File write was successful");

        let status = file.truncate(short_len as u64).await.expect("truncate failed");
        Status::ok(status).expect("File truncate was successful");

        let (status, offset) = file.seek(0, SeekOrigin::Start).await.expect("FIDL call failed");
        assert_eq!(offset, 0);
        Status::ok(status).expect("Seek was successful");

        let buf = read_file_bytes(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), short_len);
        assert_eq!(buf, input[..short_len]);

        // Re-truncate to the original length and verify the data's zeroed.
        let status = file.truncate(len as u64).await.expect("FIDL call failed");
        Status::ok(status).expect("File truncate was successful");

        let expected_buf = {
            let mut v = vec![0 as u8; len];
            v[..short_len].copy_from_slice(&input[..short_len]);
            v
        };

        let (status, offset) = file.seek(0, SeekOrigin::Start).await.expect("seek failed");
        assert_eq!(offset, 0);
        Status::ok(status).expect("Seek was successful");

        let buf = read_file_bytes(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_truncate_shrink_repeated() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            OPEN_FLAG_CREATE | OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
            MODE_TYPE_FILE,
            "foo",
        )
        .await;

        let orig_len: usize = 4 * 1024;
        let mut len = orig_len;
        let input = {
            let mut v = vec![0 as u8; len];
            for i in 0..v.len() {
                v[i] = ('a' as u8) + (i % 13) as u8;
            }
            v
        };
        let short_len: usize = 513;

        write_file_bytes(&file, &input).await.expect("File write was successful");

        while len > short_len {
            let to_truncate = std::cmp::min(len - short_len, 512);
            len -= to_truncate;
            let status = file.truncate(len as u64).await.expect("FIDL call failed");
            Status::ok(status).expect("File truncate was successful");
            len -= to_truncate;
        }

        let (status, offset) = file.seek(0, SeekOrigin::Start).await.expect("truncate failed");
        assert_eq!(offset, 0);
        Status::ok(status).expect("Seek was successful");

        let buf = read_file_bytes(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), short_len);
        assert_eq!(buf, input[..short_len]);

        // Re-truncate to the original length and verify the data's zeroed.
        let status = file.truncate(orig_len as u64).await.expect("FIDL call failed");
        Status::ok(status).expect("File truncate was successful");

        let expected_buf = {
            let mut v = vec![0 as u8; orig_len];
            v[..short_len].copy_from_slice(&input[..short_len]);
            v
        };

        let (status, offset) = file.seek(0, SeekOrigin::Start).await.expect("seek failed");
        assert_eq!(offset, 0);
        Status::ok(status).expect("Seek was successful");

        let buf = read_file_bytes(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), orig_len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }
}
