// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Read debug logs, convert them to LogMessages and serve them.

use crate::{
    container::ComponentIdentity,
    events::types::ComponentIdentifier,
    logs::{error::LogsError, message::MessageWithStats, stored_message::StoredMessage},
};
use async_trait::async_trait;
use byteorder::{ByteOrder, LittleEndian};
use diagnostics_data::{BuilderArgs, LogsDataBuilder, Severity};
use fidl::endpoints::ServiceMarker;
use diagnostics_message::message::METADATA_SIZE;
use fidl_fuchsia_boot::ReadOnlyLogMarker;
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_zircon as zx;
use futures::stream::{unfold, Stream, TryStreamExt};
use lazy_static::lazy_static;
use tracing::warn;

pub const KERNEL_URL: &str = "fuchsia-boot://kernel";
lazy_static! {
    pub static ref KERNEL_IDENTITY: ComponentIdentity = {
        ComponentIdentity::from_identifier_and_url(
            &ComponentIdentifier::parse_from_moniker("./klog:0").unwrap(),
            KERNEL_URL,
        )
    };
}

#[async_trait]
pub trait DebugLog {
    /// Reads a single entry off the debug log into `buffer`.  Any existing
    /// contents in `buffer` are overwritten.
    async fn read(&self, buffer: &'_ mut Vec<u8>) -> Result<(), zx::Status>;

    /// Returns a future that completes when there is another log to read.
    async fn ready_signal(&self) -> Result<(), zx::Status>;
}

pub struct KernelDebugLog {
    debuglogger: zx::DebugLog,
}

#[async_trait]
impl DebugLog for KernelDebugLog {
    async fn read(&self, buffer: &'_ mut Vec<u8>) -> Result<(), zx::Status> {
        self.debuglogger.read(buffer)
    }

    async fn ready_signal(&self) -> Result<(), zx::Status> {
        fasync::OnSignals::new(&self.debuglogger, zx::Signals::LOG_READABLE).await.map(|_| ())
    }
}

impl KernelDebugLog {
    /// Connects to `fuchsia.boot.ReadOnlyLog` to retrieve a handle.
    pub async fn new() -> Result<Self, LogsError> {
        let boot_log = connect_to_protocol::<ReadOnlyLogMarker>().map_err(|source| {
            LogsError::ConnectingToService { protocol: ReadOnlyLogMarker::NAME, source }
        })?;
        let debuglogger =
            boot_log.get().await.map_err(|source| LogsError::RetrievingDebugLog { source })?;
        Ok(KernelDebugLog { debuglogger })
    }
}

pub struct DebugLogBridge<K: DebugLog> {
    debug_log: K,
    buf: Vec<u8>,
}

impl<K: DebugLog> DebugLogBridge<K> {
    pub fn create(debug_log: K) -> Self {
        DebugLogBridge { debug_log, buf: Vec::with_capacity(zx::sys::ZX_LOG_RECORD_MAX) }
    }

    async fn read_log(&mut self) -> Result<StoredMessage, zx::Status> {
        loop {
            self.debug_log.read(&mut self.buf).await?;
            if let Some(bytes) = StoredMessage::debuglog(&self.buf) {
                return Ok(bytes);
            }
        }
    }

    pub async fn existing_logs<'a>(&'a mut self) -> Result<Vec<StoredMessage>, zx::Status> {
        unfold(self, move |klogger| async move {
            match klogger.read_log().await {
                Err(zx::Status::SHOULD_WAIT) => None,
                x => Some((x, klogger)),
            }
        })
        .try_collect::<Vec<_>>()
        .await
    }

    pub fn listen(self) -> impl Stream<Item = Result<StoredMessage, zx::Status>> {
        unfold((true, self), move |(mut is_readable, mut klogger)| async move {
            loop {
                if !is_readable {
                    if let Err(e) = klogger.debug_log.ready_signal().await {
                        break Some((Err(e), (is_readable, klogger)));
                    }
                }
                is_readable = true;
                match klogger.read_log().await {
                    Err(zx::Status::SHOULD_WAIT) => {
                        is_readable = false;
                        continue;
                    }
                    x => break Some((x, (is_readable, klogger))),
                }
            }
        })
    }
}

/// Parses a raw debug log read from the kernel.  Returns the parsed message and
/// its size in memory on success, and None if parsing fails.
pub fn convert_debuglog_to_log_message(buf: &[u8]) -> Option<MessageWithStats> {
    if buf.len() < 32 {
        return None;
    }
    let data_len = LittleEndian::read_u16(&buf[4..6]) as usize;
    if buf.len() != 32 + data_len {
        return None;
    }

    let time = zx::Time::from_nanos(LittleEndian::read_i64(&buf[8..16]));
    let pid = LittleEndian::read_u64(&buf[16..24]);
    let tid = LittleEndian::read_u64(&buf[24..32]);

    let mut contents = match String::from_utf8(buf[32..(32 + data_len)].to_vec()) {
        Err(e) => {
            warn!(?e, "Received non-UTF8 from the debuglog.");
            return None;
        }
        Ok(s) => s,
    };
    if let Some(b'\n') = contents.bytes().last() {
        contents.pop();
    }

    // TODO(fxbug.dev/32998): Once we support structured logs we won't need this
    // hack to match a string in klogs.
    const MAX_STRING_SEARCH_SIZE: usize = 100;
    let last = contents
        .char_indices()
        .nth(MAX_STRING_SEARCH_SIZE)
        .map(|(i, _)| i)
        .unwrap_or(contents.len());

    // Don't look beyond the 100th character in the substring to limit the cost
    // of the substring search operation.
    let early_contents = &contents[..last];

    let severity = if early_contents.contains("ERROR:") {
        Severity::Error
    } else if early_contents.contains("WARNING:") {
        Severity::Warn
    } else {
        Severity::Info
    };

    let size = METADATA_SIZE + 5 /*'klog' tag*/ + contents.len() + 1;
    Some(MessageWithStats::from(
        LogsDataBuilder::new(BuilderArgs {
            timestamp_nanos: time.into(),
            component_url: KERNEL_IDENTITY.url.to_string(),
            moniker: KERNEL_IDENTITY.to_string(),
            severity,
            size_bytes: size,
        })
        .set_pid(pid)
        .set_tid(tid)
        .add_tag("klog".to_string())
        .set_dropped(0)
        .set_message(contents)
        .build(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testing::*;

    use fidl_fuchsia_logger::LogMessage;
    use futures::stream::{StreamExt, TryStreamExt};

    #[test]
    fn convert_debuglog_to_log_message_test() {
        let klog = TestDebugEntry::new("test log".as_bytes());
        let log_message = convert_debuglog_to_log_message(&klog.to_vec()).unwrap();
        assert_eq!(
            log_message,
            MessageWithStats::from(
                LogsDataBuilder::new(BuilderArgs {
                    timestamp_nanos: klog.timestamp.into(),
                    component_url: KERNEL_IDENTITY.url.clone(),
                    moniker: KERNEL_IDENTITY.to_string(),
                    severity: Severity::Info,
                    size_bytes: METADATA_SIZE + 6 + "test log".len(),
                })
                .set_pid(klog.pid)
                .set_tid(klog.tid)
                .add_tag("klog")
                .set_message("test log".to_string())
                .build()
            )
        );
        // make sure the `klog` tag still shows up for legacy listeners
        assert_eq!(
            log_message.for_listener(),
            LogMessage {
                pid: klog.pid,
                tid: klog.tid,
                time: klog.timestamp,
                severity: fuchsia_syslog::levels::INFO,
                dropped_logs: 0,
                tags: vec!["klog".to_string()],
                msg: "test log".to_string(),
            }
        );

        // maximum allowed klog size
        let klog = TestDebugEntry::new(&vec!['a' as u8; zx::sys::ZX_LOG_RECORD_MAX - 32]);
        let log_message = convert_debuglog_to_log_message(&klog.to_vec()).unwrap();
        assert_eq!(
            log_message,
            MessageWithStats::from(
                LogsDataBuilder::new(BuilderArgs {
                    timestamp_nanos: klog.timestamp.into(),
                    component_url: KERNEL_IDENTITY.url.clone(),
                    moniker: KERNEL_IDENTITY.to_string(),
                    severity: Severity::Info,
                    size_bytes: METADATA_SIZE + 6 + zx::sys::ZX_LOG_RECORD_MAX - 32,
                })
                .set_pid(klog.pid)
                .set_tid(klog.tid)
                .add_tag("klog")
                .set_message(
                    String::from_utf8(vec!['a' as u8; zx::sys::ZX_LOG_RECORD_MAX - 32]).unwrap()
                )
                .build()
            )
        );

        // empty message
        let klog = TestDebugEntry::new(&vec![]);
        let log_message = convert_debuglog_to_log_message(&klog.to_vec()).unwrap();
        assert_eq!(
            log_message,
            MessageWithStats::from(
                LogsDataBuilder::new(BuilderArgs {
                    timestamp_nanos: klog.timestamp.into(),
                    component_url: KERNEL_IDENTITY.url.clone(),
                    moniker: KERNEL_IDENTITY.to_string(),
                    severity: Severity::Info,
                    size_bytes: METADATA_SIZE + 6,
                })
                .set_pid(klog.pid)
                .set_tid(klog.tid)
                .add_tag("klog")
                .set_message("".to_string())
                .build()
            ),
        );

        // truncated header
        let klog = vec![3u8; 4];
        assert!(convert_debuglog_to_log_message(&klog).is_none());

        // invalid utf-8
        let klog = TestDebugEntry::new(&vec![0, 159, 146, 150]);
        assert!(convert_debuglog_to_log_message(&klog.to_vec()).is_none());

        // malformed
        let klog = vec![0xffu8; 64];
        assert!(convert_debuglog_to_log_message(&klog).is_none());
    }

    #[fasync::run_until_stalled(test)]
    async fn logger_existing_logs_test() {
        let debug_log = TestDebugLog::new();
        let klog = TestDebugEntry::new("test log".as_bytes());
        debug_log.enqueue_read_entry(&klog);
        debug_log.enqueue_read_fail(zx::Status::SHOULD_WAIT);
        let mut log_bridge = DebugLogBridge::create(debug_log);

        assert_eq!(
            log_bridge
                .existing_logs()
                .await
                .unwrap()
                .into_iter()
                .map(|m| m.parse(&*KERNEL_IDENTITY).unwrap())
                .collect::<Vec<_>>(),
            vec![MessageWithStats::from(
                LogsDataBuilder::new(BuilderArgs {
                    timestamp_nanos: klog.timestamp.into(),
                    component_url: KERNEL_IDENTITY.url.clone(),
                    moniker: KERNEL_IDENTITY.to_string(),
                    severity: Severity::Info,
                    size_bytes: METADATA_SIZE + 6 + "test log".len(),
                })
                .set_pid(klog.pid)
                .set_tid(klog.tid)
                .add_tag("klog")
                .set_message("test log".to_string())
                .build()
            )]
        );

        // unprocessable logs should be skipped.
        let debug_log = TestDebugLog::new();
        debug_log.enqueue_read(vec![]);
        debug_log.enqueue_read_fail(zx::Status::SHOULD_WAIT);
        let mut log_bridge = DebugLogBridge::create(debug_log);
        assert!(log_bridge.existing_logs().await.unwrap().is_empty());
    }

    #[fasync::run_until_stalled(test)]
    async fn logger_keep_listening_after_exhausting_initial_contents_test() {
        let debug_log = TestDebugLog::new();
        debug_log.enqueue_read_entry(&TestDebugEntry::new("test log".as_bytes()));
        debug_log.enqueue_read_fail(zx::Status::SHOULD_WAIT);
        debug_log.enqueue_read_entry(&TestDebugEntry::new("second test log".as_bytes()));
        let log_bridge = DebugLogBridge::create(debug_log);
        let mut log_stream =
            Box::pin(log_bridge.listen()).map(|r| r.unwrap().parse(&*KERNEL_IDENTITY));
        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), "test log");
        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), "second test log");

        // unprocessable logs should be skipped.
        let debug_log = TestDebugLog::new();
        debug_log.enqueue_read(vec![]);
        debug_log.enqueue_read_entry(&TestDebugEntry::new("test log".as_bytes()));
        let log_bridge = DebugLogBridge::create(debug_log);
        let mut log_stream = Box::pin(log_bridge.listen());
        let log_message =
            log_stream.try_next().await.unwrap().unwrap().parse(&*KERNEL_IDENTITY).unwrap();
        assert_eq!(log_message.msg().unwrap(), "test log");
    }

    #[fasync::run_until_stalled(test)]
    async fn severity_parsed_from_log() {
        let debug_log = TestDebugLog::new();
        debug_log.enqueue_read_entry(&TestDebugEntry::new("ERROR: first log".as_bytes()));
        // We look for the string 'ERROR:' to label this as a Severity::Error.
        debug_log.enqueue_read_entry(&TestDebugEntry::new("first log error".as_bytes()));
        debug_log.enqueue_read_entry(&TestDebugEntry::new("WARNING: second log".as_bytes()));
        debug_log.enqueue_read_entry(&TestDebugEntry::new("INFO: third log".as_bytes()));
        debug_log.enqueue_read_entry(&TestDebugEntry::new("fourth log".as_bytes()));
        // Create a string prefixed with multi-byte UTF-8 characters. This entry will be labeled as
        // Info rather than Error because the string "ERROR:" only appears after the
        // MAX_STRING_SEARCH_SIZE. It's crucial that we use multi-byte UTF-8 characters because we
        // want to verify that the search is character oriented rather than byte oriented and that
        // it can handle the MAX_STRING_SEARCH_SIZE boundary falling in the middle of a multi-byte
        // character.
        let long_padding = (0..100).map(|_| "\u{10FF}").collect::<String>();
        let long_log = format!("{}ERROR: fifth log", long_padding);
        debug_log.enqueue_read_entry(&TestDebugEntry::new(long_log.as_bytes()));

        let log_bridge = DebugLogBridge::create(debug_log);
        let mut log_stream =
            Box::pin(log_bridge.listen()).map(|r| r.unwrap().parse(&*KERNEL_IDENTITY));

        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), "ERROR: first log");
        assert_eq!(log_message.metadata.severity, Severity::Error);

        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), "first log error");
        assert_eq!(log_message.metadata.severity, Severity::Info);

        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), "WARNING: second log");
        assert_eq!(log_message.metadata.severity, Severity::Warn);

        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), "INFO: third log");
        assert_eq!(log_message.metadata.severity, Severity::Info);

        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), "fourth log");
        assert_eq!(log_message.metadata.severity, Severity::Info);

        let log_message = log_stream.try_next().await.unwrap().unwrap();
        assert_eq!(log_message.msg().unwrap(), &long_log);
        assert_eq!(log_message.metadata.severity, Severity::Info);
    }
}
