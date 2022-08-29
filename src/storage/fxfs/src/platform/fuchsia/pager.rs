// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        debug_assert_not_too_long,
        log::*,
        platform::fuchsia::{file::FxFile, node::FxNode},
    },
    anyhow::{anyhow, Error},
    async_utils::event::{Event, EventWaitResult},
    fuchsia_zircon::{
        self as zx, sys::zx_page_request_command_t::ZX_PAGER_VMO_READ, AsHandleRef, PacketContents,
    },
    std::{collections::hash_map::Entry, collections::HashMap},
    std::{
        ops::Range,
        sync::{Arc, Mutex, Weak},
    },
};

pub struct Pager {
    thread: Arc<PagerThread>,
}

struct PagerThread {
    pager: zx::Pager,
    port: zx::Port,
    inner: Mutex<Inner>,
}

struct Inner {
    files: HashMap<u64, FileHolder>,
    terminate_event: Option<EventWaitResult>,
}

// FileHolder is used to retain either a strong or a weak reference to a file.  If there are any
// child VMOs that have been shared, then we will have a strong reference which is required to keep
// the file alive.  When we detect that there are no more children, we can downgrade to a weak
// reference which will allow the file to be cleaned up if there are no other uses.
enum FileHolder {
    Strong(Arc<FxFile>),
    Weak(Weak<FxFile>),
}

impl From<Arc<FxFile>> for FileHolder {
    fn from(file: Arc<FxFile>) -> FileHolder {
        FileHolder::Strong(file)
    }
}

impl From<Weak<FxFile>> for FileHolder {
    fn from(file: Weak<FxFile>) -> FileHolder {
        FileHolder::Weak(file)
    }
}

impl FileHolder {
    fn as_ptr(&self) -> *const FxFile {
        match self {
            FileHolder::Strong(file) => Arc::as_ptr(file),
            FileHolder::Weak(file) => file.as_ptr(),
        }
    }
}

/// Pager handles page requests. It is a per-volume object.
impl Pager {
    pub fn new() -> Result<Self, Error> {
        let event = Event::new();
        let thread = Arc::new(PagerThread {
            pager: zx::Pager::create(zx::PagerOptions::empty())?,
            port: zx::Port::create()?,
            inner: Mutex::new(Inner {
                files: HashMap::new(),
                terminate_event: Some(event.wait_or_dropped()),
            }),
        });
        let thread_clone = thread.clone();
        std::thread::spawn(move || {
            thread_clone.run(event);
        });
        Ok(Pager { thread })
    }

    /// Creates a new VMO to be used with the pager. Page requests will not be serviced until
    /// register_file is called.
    pub fn create_vmo(&self, object_id: u64, initial_size: u64) -> Result<zx::Vmo, Error> {
        Ok(self.thread.pager.create_vmo(
            zx::VmoOptions::RESIZABLE,
            &self.thread.port,
            object_id,
            initial_size,
        )?)
    }

    /// Registers a file with the pager.  Page requests are not properly serviced until
    /// start_servicing is called.  Any requests that arrive prior to that will be fulfilled with
    /// zero pages.
    pub fn register_file(&self, file: &Arc<FxFile>) {
        self.thread
            .inner
            .lock()
            .unwrap()
            .files
            .insert(file.object_id(), FileHolder::Weak(Arc::downgrade(file)));
    }

    /// Unregisters a file with the pager.
    pub fn unregister_file(&self, file: &FxFile) {
        let mut inner = self.thread.inner.lock().unwrap();
        let object_id = file.object_id();
        if let Entry::Occupied(o) = inner.files.entry(object_id) {
            if std::ptr::eq(file, o.get().as_ptr()) {
                if let FileHolder::Strong(_) = o.remove() {
                    file.on_zero_children();
                }
            }
        }
    }

    /// Starts servicing page requests for the given object.  Returns false if the file is already
    /// being serviced.  When there are no more references, FxFile::on_zero_children will be called.
    pub fn start_servicing(&self, object_id: u64) -> Result<bool, Error> {
        let mut inner = self.thread.inner.lock().unwrap();
        let file = inner.files.get_mut(&object_id).unwrap();
        if let FileHolder::Weak(weak) = file {
            // Should never fail because start_servicing should be called by FxFile.
            let strong = weak.upgrade().unwrap();
            self.thread.watch_for_zero_children(&strong)?;
            *file = FileHolder::Strong(strong);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Terminate the pager thread.  This will block until it has finished.
    pub async fn terminate(&self) {
        let files = std::mem::take(&mut self.thread.inner.lock().unwrap().files);
        for (_, file) in files {
            if let FileHolder::Strong(file) = file {
                file.on_zero_children();
            }
        }
        self.thread
            .port
            .queue(&zx::Packet::from_user_packet(0, 0, zx::UserPacket::from_u8_array([0; 32])))
            .unwrap();
        let event = self.thread.inner.lock().unwrap().terminate_event.take().unwrap();
        let _ = debug_assert_not_too_long!(event);
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        assert!(self.thread.inner.lock().unwrap().terminate_event.is_none());
    }
}

impl PagerThread {
    fn run(self: &Arc<Self>, terminate_event: Event) {
        loop {
            match self.port.wait(zx::Time::INFINITE) {
                Ok(packet) => {
                    if self.process_packet(&terminate_event, packet.key(), packet.contents()) {
                        break;
                    }
                }
                Err(e) => error!(error = e.as_value(), "Port::wait failed"),
            }
        }
    }

    // Processes a packet on the port.  Returns true if we have been asked to terminate.
    fn process_packet(
        self: &Arc<Self>,
        terminate_event: &Event,
        key: u64,
        contents: PacketContents,
    ) -> bool {
        match contents {
            PacketContents::Pager(contents) => {
                if contents.command() != ZX_PAGER_VMO_READ {
                    return false;
                }
                let file = {
                    let inner = self.inner.lock().unwrap();
                    match inner.files.get(&key) {
                        Some(FileHolder::Strong(file)) => file.clone(),
                        Some(FileHolder::Weak(file)) => {
                            if let Some(file) = file.upgrade() {
                                file
                            } else {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                };
                // page_in can spawn a task to service the request. If that happens and we then want
                // to terminate, we want to wait for the task to finish before we regard the pager
                // as terminated, so we pass in the terminate event which will get dropped when the
                // PageInRequest object is dropped.
                file.page_in(
                    PageInRequest {
                        thread: self.clone(),
                        _terminate_event: terminate_event.clone(),
                    },
                    contents.range(),
                );
            }
            PacketContents::SignalOne(signals) => {
                assert!(signals.observed().contains(zx::Signals::VMO_ZERO_CHILDREN));
                // To workaround races, we must check to see if the vmo really does have no
                // children.
                let _strong;
                let mut inner = self.inner.lock().unwrap();
                if let Some(holder) = inner.files.get_mut(&key) {
                    if let FileHolder::Strong(file) = holder {
                        match file.vmo().info() {
                            Ok(info) => {
                                if info.num_children == 0 {
                                    file.on_zero_children();
                                    // Downgrade to a weak reference.  Keep a strong reference until
                                    // we drop the lock because otherwise there's the potential to
                                    // deadlock (when the file is dropped, it will call
                                    // unregister_file which needs to take the lock).
                                    let weak = Arc::downgrade(file);
                                    _strong = std::mem::replace(holder, FileHolder::Weak(weak));
                                } else {
                                    // There's not much we can do here if this fails, so we panic.
                                    self.watch_for_zero_children(file).unwrap();
                                }
                            }
                            Err(e) => {
                                error!(error = e.as_value(), "Vmo::info failed");
                            }
                        }
                    }
                }
            }
            PacketContents::User(_) => {
                // We are being asked to terminate.
                return true;
            }
            _ => unreachable!(), // We don't expect any other kinds of packets
        }
        false
    }

    fn watch_for_zero_children(&self, file: &FxFile) -> Result<(), Error> {
        file.vmo()
            .as_handle_ref()
            .wait_async_handle(
                &self.port,
                file.object_id(),
                zx::Signals::VMO_ZERO_CHILDREN,
                zx::WaitAsyncOpts::empty(),
            )
            .map_err(|s| anyhow!(s))
    }
}

/// The primary purpose of this wrapper is to ensure we drop the reference when the request has
/// completed.
pub struct PageInRequest {
    thread: Arc<PagerThread>,
    _terminate_event: Event,
}

impl PageInRequest {
    /// Supplies pages in response to a page request.
    pub fn supply_pages(
        &self,
        vmo: &zx::Vmo,
        range: Range<u64>,
        transfer_vmo: &zx::Vmo,
        transfer_offset: u64,
    ) {
        if let Err(e) = self.thread.pager.supply_pages(vmo, range, transfer_vmo, transfer_offset) {
            error!(error = e.as_value(), "supply_pages failed");
        }
    }

    /// Report failure for a page request.
    pub fn report_failure(&self, vmo: &zx::Vmo, range: Range<u64>, status: zx::Status) {
        let pager_status = match status {
            zx::Status::IO_DATA_INTEGRITY => zx::Status::IO_DATA_INTEGRITY,
            // Shamelessly stolen from src/storage/blobfs/page_loader.h
            zx::Status::IO
            | zx::Status::IO_DATA_LOSS
            | zx::Status::IO_INVALID
            | zx::Status::IO_MISSED_DEADLINE
            | zx::Status::IO_NOT_PRESENT
            | zx::Status::IO_OVERRUN
            | zx::Status::IO_REFUSED
            | zx::Status::PEER_CLOSED => zx::Status::IO,
            _ => zx::Status::BAD_STATE,
        };
        if let Err(e) = self.thread.pager.op_range(zx::PagerOp::Fail(pager_status), vmo, range) {
            error!(error = e.as_value(), "op_range failed");
        }
    }
}
