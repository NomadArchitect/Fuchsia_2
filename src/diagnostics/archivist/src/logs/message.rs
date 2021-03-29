// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

use crate::{
    container::ComponentIdentity,
    events::types::ComponentIdentifier,
    logs::{error::StreamError, stats::LogStreamStats},
};
use byteorder::{ByteOrder, LittleEndian};
use diagnostics_data::{LogError, Timestamp};
use diagnostics_hierarchy::DiagnosticsHierarchy;
use diagnostics_log_encoding::{Severity as StreamSeverity, Value, ValueUnknown};
use fidl_fuchsia_logger::{LogLevelFilter, LogMessage, MAX_DATAGRAM_LEN_BYTES};
use fidl_fuchsia_sys_internal::SourceIdentity;
use fuchsia_syslog::COMPONENT_NAME_PLACEHOLDER_TAG;
use fuchsia_zircon as zx;
use lazy_static::lazy_static;
use libc::{c_char, c_int};
use serde::Serialize;
use std::{
    cmp::Ordering,
    convert::TryFrom,
    fmt::Write,
    mem,
    ops::{Deref, DerefMut},
    str,
    sync::Arc,
};

pub use diagnostics_data::{
    LogsData, LogsField, LogsHierarchy, LogsMetadata, LogsProperty, Severity,
};

const UNKNOWN_COMPONENT_NAME: &str = "UNKNOWN";

pub const METADATA_SIZE: usize = mem::size_of::<fx_log_metadata_t>();
pub const MIN_PACKET_SIZE: usize = METADATA_SIZE + 1;

pub const MAX_DATAGRAM_LEN: usize = MAX_DATAGRAM_LEN_BYTES as _;
pub const MAX_TAGS: usize = 5;
pub const MAX_TAG_LEN: usize = 64;

/// Our internal representation for a log message.
#[derive(Clone, Serialize)]
pub struct Message {
    #[serde(skip)]
    pub id: MessageId,

    // a reference to our component's stats node so we can increment dropped on our drop
    #[serde(skip)]
    stats: Arc<LogStreamStats>,

    #[serde(flatten)]
    data: LogsData,
}

impl PartialEq for Message {
    fn eq(&self, rhs: &Self) -> bool {
        self.data.eq(&rhs.data)
    }
}

impl Eq for Message {}

impl PartialOrd for Message {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Message {
    fn cmp(&self, other: &Self) -> Ordering {
        self.data.metadata.timestamp.cmp(&other.data.metadata.timestamp)
    }
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message").field("id", &self.id).field("data", &self.data).finish()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MessageId(u64);

impl MessageId {
    fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(0);

        MessageId(NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl Message {
    pub fn new(
        timestamp: impl Into<Timestamp>,
        severity: impl Into<Severity>,
        size_bytes: usize,
        dropped_before: u64,
        source: &ComponentIdentity,
        mut payload: DiagnosticsHierarchy<LogsField>,
    ) -> Self {
        payload.sort();
        let mut errors = vec![];
        if dropped_before > 0 {
            errors.push(LogError::DroppedLogs { count: dropped_before });
        }
        Self {
            id: MessageId::next(),
            stats: Default::default(),
            data: LogsData::for_logs(
                source.to_string(),
                Some(payload),
                timestamp,
                &source.url,
                severity,
                size_bytes,
                errors,
            ),
        }
    }

    /// Returns a new Message which encodes a count of dropped messages in its metadata.
    pub fn for_dropped(count: u64, source: &ComponentIdentity, timestamp: i64) -> Self {
        let message = format!("Dropped {} logs", count);
        Self {
            id: MessageId::next(),
            stats: Default::default(),
            data: LogsData::for_logs(
                source.to_string(),
                Some(LogsHierarchy::new(
                    "root",
                    vec![LogsProperty::String(LogsField::Msg, message)],
                    vec![],
                )),
                timestamp,
                &source.url,
                Severity::Warn,
                0, // size_bytes
                vec![LogError::DroppedLogs { count }],
            ),
        }
    }

    pub(crate) fn with_stats(mut self, stats: &Arc<LogStreamStats>) -> Self {
        self.stats = stats.clone();
        self
    }

    /// Parse the provided buffer as if it implements the [logger/syslog wire format].
    ///
    /// Note that this is distinct from the parsing we perform for the debuglog log, which also
    /// takes a `&[u8]` and is why we don't implement this as `TryFrom`.
    ///
    /// [logger/syslog wire format]: https://fuchsia.googlesource.com/fuchsia/+/HEAD/zircon/system/ulib/syslog/include/lib/syslog/wire_format.h
    pub fn from_logger(source: &ComponentIdentity, bytes: &[u8]) -> Result<Self, StreamError> {
        if bytes.len() < MIN_PACKET_SIZE {
            return Err(StreamError::ShortRead { len: bytes.len() });
        }

        let terminator = bytes[bytes.len() - 1];
        if terminator != 0 {
            return Err(StreamError::NotNullTerminated { terminator });
        }

        let pid = LittleEndian::read_u64(&bytes[..8]);
        let tid = LittleEndian::read_u64(&bytes[8..16]);
        let time = zx::Time::from_nanos(LittleEndian::read_i64(&bytes[16..24]));

        let raw_severity = LittleEndian::read_i32(&bytes[24..28]);
        let severity = LegacySeverity::try_from(raw_severity)?;

        let dropped_logs = LittleEndian::read_u32(&bytes[28..METADATA_SIZE]) as u64;

        // start reading tags after the header
        let mut cursor = METADATA_SIZE;
        let mut tag_len = bytes[cursor] as usize;
        let mut tags = Vec::new();
        while tag_len != 0 {
            if tags.len() == MAX_TAGS {
                return Err(StreamError::TooManyTags);
            }

            if tag_len > MAX_TAG_LEN - 1 {
                return Err(StreamError::TagTooLong { index: tags.len(), len: tag_len });
            }

            if (cursor + tag_len + 1) > bytes.len() {
                return Err(StreamError::OutOfBounds);
            }

            let tag_start = cursor + 1;
            let tag_end = tag_start + tag_len;
            let tag = str::from_utf8(&bytes[tag_start..tag_end])?;

            if tag == COMPONENT_NAME_PLACEHOLDER_TAG {
                tags.push(source.to_string());
            } else {
                tags.push(tag.to_owned());
            }

            cursor = tag_end;
            tag_len = bytes[cursor] as usize;
        }

        let msg_start = cursor + 1;
        let mut msg_end = cursor + 1;
        while msg_end < bytes.len() {
            if bytes[msg_end] == 0 {
                let message = str::from_utf8(&bytes[msg_start..msg_end])?.to_owned();
                let message_len = message.len();
                let mut contents = LogsHierarchy::new(
                    "root",
                    vec![
                        LogsProperty::Uint(LogsField::ProcessId, pid),
                        LogsProperty::Uint(LogsField::ThreadId, tid),
                        LogsProperty::String(LogsField::Msg, message),
                    ],
                    vec![],
                );
                for tag in tags {
                    contents.add_property(&["root"], LogsProperty::String(LogsField::Tag, tag));
                }

                let (severity, verbosity) = severity.for_structured();
                let mut new = Message::new(
                    time,
                    severity,
                    cursor + message_len + 1,
                    dropped_logs,
                    source,
                    contents,
                );

                if let Some(verbosity) = verbosity {
                    new.set_legacy_verbosity(verbosity);
                }

                return Ok(new);
            }
            msg_end += 1;
        }

        Err(StreamError::OutOfBounds)
    }

    /// Constructs a `Message` from the provided bytes, assuming the bytes
    /// are in the format specified as in the [log encoding].
    ///
    /// [log encoding] https://fuchsia.dev/fuchsia-src/development/logs/encodings
    pub fn from_structured(source: &ComponentIdentity, bytes: &[u8]) -> Result<Self, StreamError> {
        let (record, _) = diagnostics_log_encoding::parse::parse_record(bytes)?;

        let mut properties = vec![];
        let mut dropped_logs = 0;
        // Raw value from the client that we don't trust (not yet sanitized)
        let mut severity_untrusted: Option<i64> = None;
        for a in record.arguments {
            let label = LogsField::from(a.name);

            if matches!(label, LogsField::Dropped) {
                match a.value {
                    Value::SignedInt(v) => Ok(dropped_logs = v as u64),
                    Value::UnsignedInt(v) => Ok(dropped_logs = v),
                    Value::Floating(f) => Err(StreamError::ExpectedInteger {
                        value: format!("{:?}", f),
                        found: "float",
                    }),
                    Value::Text(t) => Err(StreamError::ExpectedInteger { value: t, found: "text" }),
                    ValueUnknown!() => {
                        Err(StreamError::ExpectedInteger { value: "".into(), found: "unknown" })
                    }
                }?
            } else {
                if matches!(label, LogsField::Verbosity) {
                    match a.value {
                        Value::SignedInt(v) => {
                            severity_untrusted = Some(v);
                            Ok(())
                        }
                        Value::Text(t) => {
                            Err(StreamError::ExpectedInteger { value: t, found: "text" })
                        }
                        ValueUnknown!() => {
                            Err(StreamError::ExpectedInteger { value: "".into(), found: "unknown" })
                        }
                    }?
                } else {
                    properties.push(match a.value {
                        Value::SignedInt(v) => LogsProperty::Int(label, v),
                        Value::UnsignedInt(v) => LogsProperty::Uint(label, v),
                        Value::Floating(v) => LogsProperty::Double(label, v),
                        Value::Text(v) => LogsProperty::String(label, v),
                        ValueUnknown!() => return Err(StreamError::UnrecognizedValue),
                    })
                }
            }
        }

        let raw_severity = if severity_untrusted.is_some() {
            let transcoded_i32: i32 = severity_untrusted.unwrap().to_string().parse().unwrap();
            LegacySeverity::try_from(transcoded_i32)?
        } else {
            LegacySeverity::try_from(record.severity).unwrap()
        };
        let (severity, verbosity) = raw_severity.for_structured();
        let mut ret = Message::new(
            record.timestamp,
            severity,
            bytes.len(),
            dropped_logs,
            source,
            LogsHierarchy::new("root", properties, vec![]),
        );
        if severity_untrusted.is_some() && verbosity.is_some() {
            ret.set_legacy_verbosity(verbosity.unwrap())
        }
        Ok(ret)
    }

    /// The message's severity.
    pub fn legacy_severity(&self) -> LegacySeverity {
        if let Some(verbosity) = self.verbosity() {
            LegacySeverity::Verbose(verbosity)
        } else {
            match self.metadata.severity {
                Severity::Trace => LegacySeverity::Trace,
                Severity::Debug => LegacySeverity::Debug,
                Severity::Info => LegacySeverity::Info,
                Severity::Warn => LegacySeverity::Warn,
                Severity::Error => LegacySeverity::Error,
                Severity::Fatal => LegacySeverity::Fatal,
            }
        }
    }

    /// Removes the verbosity field from this message if it exists. Only for testing to reuse
    /// messages.
    #[cfg(test)]
    fn clear_legacy_verbosity(&mut self) {
        self.payload_mut()
            .properties
            .retain(|p| !matches!(p, LogsProperty::Int(LogsField::Verbosity, _)));
    }

    /// Assign the `legacy` value to this message.
    pub(crate) fn set_legacy_verbosity(&mut self, legacy: i8) {
        self.payload_mut().properties.push(LogsProperty::Int(LogsField::Verbosity, legacy.into()));
    }

    /// Unconditionally returns the payload because we always have a root hierarchy for logs.
    fn payload(&self) -> &LogsHierarchy {
        self.payload.as_ref().expect("Message is always constructed with a payload")
    }

    /// Unconditionally returns the payload because we always have a root hierarchy for logs.
    fn payload_mut(&mut self) -> &mut LogsHierarchy {
        self.payload.as_mut().expect("Message is always constructed with a payload")
    }

    pub fn verbosity(&self) -> Option<i8> {
        self.payload()
            .properties
            .iter()
            .filter_map(|property| match property {
                LogsProperty::Int(LogsField::Verbosity, verbosity) => Some(*verbosity as i8),
                _ => None,
            })
            .next()
    }

    /// Returns the pid associated with the message, if one exists.
    pub fn pid(&self) -> Option<u64> {
        self.payload()
            .properties
            .iter()
            .filter_map(|property| match property {
                LogsProperty::Uint(LogsField::ProcessId, pid) => Some(*pid),
                _ => None,
            })
            .next()
    }

    /// Returns the tid associated with the message, if one exists.
    pub fn tid(&self) -> Option<u64> {
        self.payload()
            .properties
            .iter()
            .filter_map(|property| match property {
                LogsProperty::Uint(LogsField::ThreadId, tid) => Some(*tid),
                _ => None,
            })
            .next()
    }

    /// Returns any tags associated with the message.
    pub fn tags(&'_ self) -> impl Iterator<Item = &'_ str> {
        // Multiple tags are supported for the `LogMessage` format and are represented
        // as multiple instances of LogsField::Tag arguments.
        self.payload().properties.iter().filter_map(|property| match property {
            LogsProperty::String(LogsField::Tag, tag) => Some(tag.as_str()),
            _ => None,
        })
    }

    /// Returns number of dropped logs if reported in the message.
    pub fn dropped_logs(&self) -> Option<u64> {
        self.metadata
            .errors
            .as_ref()
            .map(|errors| {
                errors.iter().find_map(|e| match e {
                    LogError::DroppedLogs { count } => Some(*count),
                    _ => None,
                })
            })
            .flatten()
    }

    fn non_legacy_contents(&self) -> impl Iterator<Item = &LogsProperty> {
        self.payload().properties.iter().filter_map(|prop| {
            if prop.key().is_legacy() {
                None
            } else {
                Some(prop)
            }
        })
    }

    /// The name of the component from which this message originated.
    pub fn component_name(&self) -> &str {
        self.moniker.rsplit("/").next().unwrap_or(UNKNOWN_COMPONENT_NAME)
    }

    /// Convert this `Message` to a FIDL representation suitable for sending to `LogListenerSafe`.
    pub fn for_listener(&self) -> LogMessage {
        let mut msg = self.msg().unwrap_or("").to_string();

        for property in self.non_legacy_contents() {
            write!(&mut msg, " {}", property).expect("allocations have to fail for this to fail");
        }

        let mut tags: Vec<_> = self.tags().map(String::from).collect();
        if tags.is_empty() {
            tags.push(self.component_name().to_string());
        }

        LogMessage {
            pid: self.pid().unwrap_or(zx::sys::ZX_KOID_INVALID),
            tid: self.tid().unwrap_or(zx::sys::ZX_KOID_INVALID),
            time: self.metadata.timestamp.into(),
            severity: self.legacy_severity().for_listener(),
            dropped_logs: self.dropped_logs().unwrap_or(0) as _,
            tags,
            msg,
        }
    }
}

impl Deref for Message {
    type Target = LogsData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl DerefMut for Message {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl Drop for Message {
    fn drop(&mut self) {
        self.stats.increment_dropped(&self.data);
    }
}

lazy_static! {
    pub static ref EMPTY_IDENTITY: ComponentIdentity = ComponentIdentity::unknown();
    pub static ref TEST_IDENTITY: Arc<ComponentIdentity> = {
        Arc::new(ComponentIdentity::from_identifier_and_url(
            &ComponentIdentifier::Legacy {
                moniker: vec!["fake-test-env", "test-component.cmx"].into(),
                instance_id: "".into(),
            },
            "fuchsia-pkg://fuchsia.com/testing123#test-component.cmx",
        ))
    };
}

trait IdentityExt {
    fn name(&self) -> &str;
    fn url(&self) -> &str;
    fn moniker(&self) -> String;
}

impl IdentityExt for SourceIdentity {
    fn name(&self) -> &str {
        self.component_name.as_ref().map(String::as_str).unwrap_or(UNKNOWN_COMPONENT_NAME)
    }

    fn url(&self) -> &str {
        self.component_url.as_ref().map(String::as_str).unwrap_or("UNKNOWN")
    }

    fn moniker(&self) -> String {
        let realm = self.realm_path.as_ref().map(|p| p.join("/")).unwrap_or_default();

        if !realm.is_empty() {
            format!("{}/{}", realm, self.name())
        } else {
            self.name().to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(i8)]
pub enum LegacySeverity {
    Trace,
    Debug,
    Verbose(i8),
    Info,
    Warn,
    Error,
    Fatal,
}

impl LegacySeverity {
    /// Splits this legacy value into a severity and an optional verbosity.
    fn for_structured(self) -> (Severity, Option<i8>) {
        match self {
            LegacySeverity::Trace => (Severity::Trace, None),
            LegacySeverity::Debug => (Severity::Debug, None),
            LegacySeverity::Info => (Severity::Info, None),
            LegacySeverity::Warn => (Severity::Warn, None),
            LegacySeverity::Error => (Severity::Error, None),
            LegacySeverity::Fatal => (Severity::Fatal, None),
            LegacySeverity::Verbose(v) => (Severity::Debug, Some(v)),
        }
    }

    pub fn for_listener(self) -> fx_log_severity_t {
        match self {
            LegacySeverity::Trace => LogLevelFilter::Trace as _,
            LegacySeverity::Debug => LogLevelFilter::Debug as _,
            LegacySeverity::Info => LogLevelFilter::Info as _,
            LegacySeverity::Warn => LogLevelFilter::Warn as _,
            LegacySeverity::Error => LogLevelFilter::Error as _,
            LegacySeverity::Fatal => LogLevelFilter::Fatal as _,
            LegacySeverity::Verbose(v) => (LogLevelFilter::Info as i8 - v) as _,
        }
    }
}

impl From<Severity> for LegacySeverity {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Trace => Self::Trace,
            Severity::Debug => Self::Debug,
            Severity::Info => Self::Info,
            Severity::Warn => Self::Warn,
            Severity::Error => Self::Error,
            Severity::Fatal => Self::Fatal,
        }
    }
}

impl From<StreamSeverity> for LegacySeverity {
    fn from(fidl_severity: StreamSeverity) -> Self {
        match fidl_severity {
            StreamSeverity::Trace => Self::Trace,
            StreamSeverity::Debug => Self::Debug,
            StreamSeverity::Info => Self::Info,
            StreamSeverity::Warn => Self::Warn,
            StreamSeverity::Error => Self::Error,
            StreamSeverity::Fatal => Self::Fatal,
        }
    }
}

impl TryFrom<fx_log_severity_t> for LegacySeverity {
    type Error = StreamError;

    fn try_from(raw: fx_log_severity_t) -> Result<Self, StreamError> {
        // Handle legacy/deprecated level filter values.
        if -10 <= raw && raw <= -3 {
            Ok(LegacySeverity::Verbose(-raw as i8))
        } else if raw == -2 {
            // legacy values from trace verbosity
            Ok(LegacySeverity::Trace)
        } else if raw == -1 {
            // legacy value from debug verbosity
            Ok(LegacySeverity::Debug)
        } else if raw == 0 {
            // legacy value for INFO
            Ok(LegacySeverity::Info)
        } else if raw == 1 {
            // legacy value for WARNING
            Ok(LegacySeverity::Warn)
        } else if raw == 2 {
            // legacy value for ERROR
            Ok(LegacySeverity::Error)
        } else if raw == 3 {
            // legacy value for FATAL
            Ok(LegacySeverity::Fatal)
        } else if raw < LogLevelFilter::Info as i32 && raw > LogLevelFilter::Debug as i32 {
            // Verbosity scale exists as incremental steps between INFO & DEBUG
            Ok(LegacySeverity::Verbose(LogLevelFilter::Info as i8 - raw as i8))
        } else if let Some(level) = LogLevelFilter::from_primitive(raw as i8) {
            // Handle current level filter values.
            match level {
                // Match defined severities at their given filter level.
                LogLevelFilter::Trace => Ok(LegacySeverity::Trace),
                LogLevelFilter::Debug => Ok(LegacySeverity::Debug),
                LogLevelFilter::Info => Ok(LegacySeverity::Info),
                LogLevelFilter::Warn => Ok(LegacySeverity::Warn),
                LogLevelFilter::Error => Ok(LegacySeverity::Error),
                LogLevelFilter::Fatal => Ok(LegacySeverity::Fatal),
                _ => Err(StreamError::InvalidSeverity { provided: raw }),
            }
        } else {
            Err(StreamError::InvalidSeverity { provided: raw })
        }
    }
}

#[allow(non_camel_case_types)]
pub(super) type fx_log_severity_t = c_int;

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
pub struct fx_log_metadata_t {
    pub pid: zx::sys::zx_koid_t,
    pub tid: zx::sys::zx_koid_t,
    pub time: zx::sys::zx_time_t,
    pub severity: fx_log_severity_t,
    pub dropped_logs: u32,
}

#[repr(C)]
#[derive(Clone)]
pub struct fx_log_packet_t {
    pub metadata: fx_log_metadata_t,
    // Contains concatenated tags and message and a null terminating character at
    // the end.
    // char(tag_len) + "tag1" + char(tag_len) + "tag2\0msg\0"
    pub data: [c_char; MAX_DATAGRAM_LEN - METADATA_SIZE],
}

impl Default for fx_log_packet_t {
    fn default() -> fx_log_packet_t {
        fx_log_packet_t {
            data: [0; MAX_DATAGRAM_LEN - METADATA_SIZE],
            metadata: Default::default(),
        }
    }
}

impl fx_log_packet_t {
    /// This struct has no padding bytes, but we can't use zerocopy because it needs const
    /// generics to support arrays this large.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                (self as *const Self) as *const u8,
                mem::size_of::<fx_log_packet_t>(),
            )
        }
    }

    /// Fills data with a single value for defined region.
    pub fn fill_data(&mut self, region: std::ops::Range<usize>, with: c_char) {
        self.data[region].iter_mut().for_each(|c| *c = with);
    }

    /// Copies bytes to data at specifies offset.
    pub fn add_data<T: std::convert::TryInto<c_char> + Copy>(&mut self, offset: usize, bytes: &[T])
    where
        <T as std::convert::TryInto<c_char>>::Error: std::fmt::Debug,
    {
        self.data[offset..(offset + bytes.len())]
            .iter_mut()
            .enumerate()
            .for_each(|(i, x)| *x = bytes[i].try_into().unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_data::*;
    use diagnostics_log_encoding::{encode::Encoder, Argument, Record};
    use fuchsia_syslog::levels::{DEBUG, ERROR, INFO, TRACE, WARN};
    use std::io::Cursor;

    #[repr(C, packed)]
    pub struct fx_log_metadata_t_packed {
        pub pid: zx::sys::zx_koid_t,
        pub tid: zx::sys::zx_koid_t,
        pub time: zx::sys::zx_time_t,
        pub severity: fx_log_severity_t,
        pub dropped_logs: u32,
    }

    #[repr(C, packed)]
    pub struct fx_log_packet_t_packed {
        pub metadata: fx_log_metadata_t_packed,
        /// Contains concatenated tags and message and a null terminating character at the end.
        /// `char(tag_len) + "tag1" + char(tag_len) + "tag2\0msg\0"`
        pub data: [c_char; MAX_DATAGRAM_LEN - METADATA_SIZE],
    }

    #[test]
    fn abi_test() {
        assert_eq!(METADATA_SIZE, 32);
        assert_eq!(MAX_TAGS, 5);
        assert_eq!(MAX_TAG_LEN, 64);
        assert_eq!(mem::size_of::<fx_log_metadata_t>(), METADATA_SIZE);
        assert_eq!(mem::size_of::<fx_log_packet_t>(), MAX_DATAGRAM_LEN);

        // Test that there is no padding
        assert_eq!(mem::size_of::<fx_log_packet_t>(), mem::size_of::<fx_log_packet_t_packed>());

        assert_eq!(mem::size_of::<fx_log_metadata_t>(), mem::size_of::<fx_log_metadata_t_packed>());
    }
    fn test_packet() -> fx_log_packet_t {
        let mut packet: fx_log_packet_t = Default::default();
        packet.metadata.pid = 1;
        packet.metadata.tid = 2;
        packet.metadata.time = 3;
        packet.metadata.severity = LogLevelFilter::Debug as i32;
        packet.metadata.dropped_logs = 10;
        packet
    }

    #[test]
    fn short_reads() {
        let packet = test_packet();
        let one_short = &packet.as_bytes()[..METADATA_SIZE];
        let two_short = &packet.as_bytes()[..METADATA_SIZE - 1];

        assert_eq!(
            Message::from_logger(&*TEST_IDENTITY, one_short),
            Err(StreamError::ShortRead { len: 32 })
        );

        assert_eq!(
            Message::from_logger(&*TEST_IDENTITY, two_short),
            Err(StreamError::ShortRead { len: 31 })
        );
    }

    #[test]
    fn unterminated() {
        let mut packet = test_packet();
        let end = 9;
        packet.data[end] = 1;

        let buffer = &packet.as_bytes()[..MIN_PACKET_SIZE + end];
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer);

        assert_eq!(parsed, Err(StreamError::NotNullTerminated { terminator: 1 }));
    }

    #[test]
    fn tags_no_message() {
        let mut packet = test_packet();
        let end = 12;
        packet.data[0] = end as c_char - 1;
        packet.fill_data(1..end, 'A' as _);
        packet.data[end] = 0;

        let buffer = &packet.as_bytes()[..MIN_PACKET_SIZE + end]; // omit null-terminated
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer);

        assert_eq!(parsed, Err(StreamError::OutOfBounds));
    }

    #[test]
    fn tags_with_message() {
        let mut packet = test_packet();
        let a_start = 1;
        let a_count = 11;
        let a_end = a_start + a_count;

        packet.data[0] = a_count as c_char;
        packet.fill_data(a_start..a_end, 'A' as _);
        packet.data[a_end] = 0; // terminate tags

        let b_start = a_start + a_count + 1;
        let b_count = 5;
        let b_end = b_start + b_count;
        packet.fill_data(b_start..b_end, 'B' as _);

        let data_size = b_start + b_count;

        let buffer = &packet.as_bytes()[..METADATA_SIZE + data_size + 1]; // null-terminate message
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        let expected = Message::new(
            packet.metadata.time,
            Severity::Debug,
            METADATA_SIZE + data_size,
            packet.metadata.dropped_logs as _,
            &*TEST_IDENTITY,
            LogsHierarchy::new(
                "root",
                vec![
                    LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
                    LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
                    LogsProperty::String(LogsField::Tag, "AAAAAAAAAAA".into()),
                    LogsProperty::String(LogsField::Msg, "BBBBB".into()),
                ],
                vec![],
            ),
        );

        assert_eq!(parsed, expected);
    }

    #[test]
    fn placeholder_tag_replaced_with_attributed_name() {
        let mut packet = test_packet();

        let t_count = COMPONENT_NAME_PLACEHOLDER_TAG.len();
        packet.data[0] = t_count as c_char;
        let t_start = 1;
        packet.add_data(1, COMPONENT_NAME_PLACEHOLDER_TAG.as_bytes());
        let t_end = t_start + t_count;

        let a_count = 5;
        packet.data[t_end] = a_count as c_char;
        let a_start = t_end + 1;
        let a_end = a_start + a_count;
        packet.fill_data(a_start..a_end, 'A' as _);
        packet.data[a_end] = 0; // terminate tags

        let b_start = a_end + 1;
        let b_count = 5;
        let b_end = b_start + b_count;
        packet.fill_data(b_start..b_end, 'B' as _);

        let buffer = &packet.as_bytes()[..METADATA_SIZE + b_end + 1]; // null-terminate message
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        let expected = Message::new(
            packet.metadata.time,
            Severity::Debug,
            METADATA_SIZE + b_end,
            packet.metadata.dropped_logs as _,
            &*TEST_IDENTITY,
            LogsHierarchy::new(
                "root",
                vec![
                    LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
                    LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
                    LogsProperty::String(LogsField::Tag, TEST_IDENTITY.to_string()),
                    LogsProperty::String(LogsField::Tag, "AAAAA".into()),
                    LogsProperty::String(LogsField::Msg, "BBBBB".into()),
                ],
                vec![],
            ),
        );
        assert_eq!(parsed, expected);
    }

    #[test]
    fn component_identity_preserved() {
        let test_message = Message::new(
            0i64, // time
            Severity::Debug,
            0usize, // size
            0u64,   // dropped
            &*TEST_IDENTITY,
            LogsHierarchy::new("root", vec![], vec![]),
        );
        assert_eq!(&test_message.moniker, &TEST_IDENTITY.relative_moniker.join("/"));
        assert_eq!(&test_message.metadata.component_url, &TEST_IDENTITY.url);
    }

    #[test]
    fn two_tags_no_message() {
        let mut packet = test_packet();
        let a_start = 1;
        let a_count = 11;
        let a_end = a_start + a_count;

        packet.data[0] = a_count as c_char;
        packet.fill_data(a_start..a_end, 'A' as _);

        let b_start = a_end + 1;
        let b_count = 5;
        let b_end = b_start + b_count;

        packet.data[a_end] = b_count as c_char;
        packet.fill_data(b_start..b_end, 'B' as _);

        let buffer = &packet.as_bytes()[..MIN_PACKET_SIZE + b_end];
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer);

        assert_eq!(parsed, Err(StreamError::OutOfBounds));
    }

    #[test]
    fn two_tags_with_message() {
        let mut packet = test_packet();
        let a_start = 1;
        let a_count = 11;
        let a_end = a_start + a_count;

        packet.data[0] = a_count as c_char;
        packet.fill_data(a_start..a_end, 'A' as _);

        let b_start = a_end + 1;
        let b_count = 5;
        let b_end = b_start + b_count;

        packet.data[a_end] = b_count as c_char;
        packet.fill_data(b_start..b_end, 'B' as _);

        let c_start = b_end + 1;
        let c_count = 5;
        let c_end = c_start + c_count;
        packet.fill_data(c_start..c_end, 'C' as _);

        let data_size = c_start + c_count;

        let buffer = &packet.as_bytes()[..METADATA_SIZE + data_size + 1]; // null-terminated
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        let expected = Message::new(
            packet.metadata.time,
            Severity::Debug,
            METADATA_SIZE + data_size,
            packet.metadata.dropped_logs as u64,
            &*TEST_IDENTITY,
            LogsHierarchy::new(
                "root",
                vec![
                    LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
                    LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
                    LogsProperty::String(LogsField::Tag, "AAAAAAAAAAA".to_string()),
                    LogsProperty::String(LogsField::Tag, "BBBBB".to_string()),
                    LogsProperty::String(LogsField::Msg, "CCCCC".to_string()),
                ],
                vec![],
            ),
        );

        assert_eq!(parsed, expected);
    }

    #[test]
    fn max_tags_with_message() {
        let mut packet = test_packet();

        let tags_start = 1;
        let tag_len = 2;
        let tag_size = tag_len + 1; // the length-prefix byte
        for tag_num in 0..MAX_TAGS {
            let start = tags_start + (tag_size * tag_num);
            let end = start + tag_len;

            packet.data[start - 1] = tag_len as c_char;
            let ascii = 'A' as c_char + tag_num as c_char;
            packet.fill_data(start..end, ascii);
        }

        let msg_start = tags_start + (tag_size * MAX_TAGS);
        let msg_len = 5;
        let msg_end = msg_start + msg_len;
        let msg_ascii = 'A' as c_char + MAX_TAGS as c_char;
        packet.fill_data(msg_start..msg_end, msg_ascii);

        let min_buffer = &packet.as_bytes()[..METADATA_SIZE + msg_end + 1]; // null-terminated
        let full_buffer = &packet.as_bytes()[..];

        let min_parsed = Message::from_logger(&*TEST_IDENTITY, min_buffer).unwrap();
        let full_parsed = Message::from_logger(&*TEST_IDENTITY, full_buffer).unwrap();

        let mut expected_properties = vec![
            LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
            LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
            LogsProperty::String(
                LogsField::Msg,
                String::from_utf8(vec![msg_ascii as u8; msg_len]).unwrap(),
            ),
        ];
        let mut tag_properties = (0..MAX_TAGS as _)
            .map(|tag_num| {
                LogsProperty::String(
                    LogsField::Tag,
                    String::from_utf8(vec![('A' as c_char + tag_num) as u8; tag_len]).unwrap(),
                )
            })
            .collect::<Vec<LogsProperty>>();
        expected_properties.append(&mut tag_properties);
        let mut expected_contents = LogsHierarchy::new("root", expected_properties, vec![]);

        // sort hierarchies so we can do direct comparison
        expected_contents.sort();

        let expected_message = Message::new(
            packet.metadata.time,
            Severity::Debug,
            METADATA_SIZE + msg_end,
            packet.metadata.dropped_logs as u64,
            &*TEST_IDENTITY,
            expected_contents,
        );

        assert_eq!(min_parsed, expected_message);
        assert_eq!(full_parsed, expected_message);
    }

    #[test]
    fn max_tags() {
        let mut packet = test_packet();
        let tags_start = 1;
        let tag_len = 2;
        let tag_size = tag_len + 1; // the length-prefix byte
        for tag_num in 0..MAX_TAGS {
            let start = tags_start + (tag_size * tag_num);
            let end = start + tag_len;

            packet.data[start - 1] = tag_len as c_char;
            let ascii = 'A' as c_char + tag_num as c_char;
            packet.fill_data(start..end, ascii);
        }

        let msg_start = tags_start + (tag_size * MAX_TAGS);

        let buffer_missing_terminator = &packet.as_bytes()[..METADATA_SIZE + msg_start];
        assert_eq!(
            Message::from_logger(&*TEST_IDENTITY, buffer_missing_terminator),
            Err(StreamError::OutOfBounds),
            "can't parse an empty message without a nul terminator"
        );

        let buffer = &packet.as_bytes()[..METADATA_SIZE + msg_start + 1]; // null-terminated
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();

        let mut expected_contents = LogsHierarchy::new(
            "root",
            vec![
                LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
                LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
                LogsProperty::String(LogsField::Msg, "".to_string()),
            ],
            vec![],
        );
        for tag_num in 0..MAX_TAGS as _ {
            expected_contents.add_property(
                &["root"],
                LogsProperty::String(
                    LogsField::Tag,
                    String::from_utf8(vec![('A' as c_char + tag_num) as u8; 2]).unwrap(),
                ),
            );
        }
        expected_contents.sort();

        assert_eq!(
            parsed,
            Message::new(
                packet.metadata.time,
                Severity::Debug,
                METADATA_SIZE + msg_start,
                packet.metadata.dropped_logs as u64,
                &*TEST_IDENTITY,
                expected_contents,
            ),
        );
    }

    #[test]
    fn no_tags_with_message() {
        let mut packet = test_packet();
        packet.data[0] = 0;
        packet.data[1] = 'A' as _;
        packet.data[2] = 'A' as _; // measured size ends here
        packet.data[3] = 0;

        let buffer = &packet.as_bytes()[..METADATA_SIZE + 4]; // 0 tag size + 2 byte message + null
        let parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();

        assert_eq!(
            parsed,
            Message::new(
                packet.metadata.time,
                Severity::Debug,
                METADATA_SIZE + 3,
                packet.metadata.dropped_logs as u64,
                &*TEST_IDENTITY,
                LogsHierarchy::new(
                    "root",
                    vec![
                        LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
                        LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
                        LogsProperty::String(LogsField::Msg, "AA".to_string())
                    ],
                    vec![],
                ),
            )
        );
    }

    #[test]
    fn message_severity() {
        let mut packet = test_packet();
        packet.metadata.severity = LogLevelFilter::Info as i32;
        packet.data[0] = 0; // tag size
        packet.data[1] = 0; // null terminated

        let mut buffer = &packet.as_bytes()[..METADATA_SIZE + 2]; // tag size + null
        let mut parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        let mut expected_message = Message::new(
            packet.metadata.time,
            Severity::Info,
            METADATA_SIZE + 1,
            packet.metadata.dropped_logs as u64,
            &*TEST_IDENTITY,
            LogsHierarchy::new(
                "root",
                vec![
                    LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
                    LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
                    LogsProperty::String(LogsField::Msg, "".to_string()),
                ],
                vec![],
            ),
        );

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = LogLevelFilter::Trace as i32;
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.metadata.severity = Severity::Trace;

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = LogLevelFilter::Debug as i32;
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.metadata.severity = Severity::Debug;

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = LogLevelFilter::Warn as i32;
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.metadata.severity = Severity::Warn;

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = LogLevelFilter::Error as i32;
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.metadata.severity = Severity::Error;

        assert_eq!(parsed, expected_message);
    }

    #[test]
    fn legacy_message_severity() {
        let mut packet = test_packet();
        // legacy verbosity where v=10
        packet.metadata.severity = LogLevelFilter::Info as i32 - 10;
        packet.data[0] = 0; // tag size
        packet.data[1] = 0; // null terminated

        let mut buffer = &packet.as_bytes()[..METADATA_SIZE + 2]; // tag size + null
        let mut parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        let mut expected_message = Message::new(
            zx::Time::from_nanos(packet.metadata.time),
            Severity::Debug,
            METADATA_SIZE + 1,
            packet.metadata.dropped_logs as u64,
            &*TEST_IDENTITY,
            LogsHierarchy::new(
                "root",
                vec![
                    LogsProperty::Uint(LogsField::ProcessId, packet.metadata.pid),
                    LogsProperty::Uint(LogsField::ThreadId, packet.metadata.tid),
                    LogsProperty::String(LogsField::Msg, "".to_string()),
                ],
                vec![],
            ),
        );
        expected_message.clear_legacy_verbosity();
        expected_message.set_legacy_verbosity(10);

        assert_eq!(parsed, expected_message);

        // legacy verbosity where v=2
        packet.metadata.severity = LogLevelFilter::Info as i32 - 2;
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.clear_legacy_verbosity();
        expected_message.set_legacy_verbosity(2);

        assert_eq!(parsed, expected_message);

        // legacy verbosity where v=1
        packet.metadata.severity = LogLevelFilter::Info as i32 - 1;
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.clear_legacy_verbosity();
        expected_message.set_legacy_verbosity(1);

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = 0; // legacy severity
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.clear_legacy_verbosity();
        expected_message.metadata.severity = Severity::Info;

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = 1; // legacy severity
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.metadata.severity = Severity::Warn;

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = 2; // legacy severity
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.metadata.severity = Severity::Error;

        assert_eq!(parsed, expected_message);

        packet.metadata.severity = 3; // legacy severity
        buffer = &packet.as_bytes()[..METADATA_SIZE + 2];
        parsed = Message::from_logger(&*TEST_IDENTITY, buffer).unwrap();
        expected_message.metadata.severity = Severity::Fatal;

        assert_eq!(parsed, expected_message);
    }

    #[test]
    fn test_from_structured() {
        let record = Record {
            timestamp: 72,
            severity: StreamSeverity::Error,
            arguments: vec![
                Argument { name: "arg1".to_string(), value: Value::SignedInt(-23) },
                Argument { name: PID_LABEL.to_string(), value: Value::UnsignedInt(43) },
                Argument { name: TID_LABEL.to_string(), value: Value::UnsignedInt(912) },
                Argument { name: DROPPED_LABEL.to_string(), value: Value::UnsignedInt(2) },
                Argument { name: TAG_LABEL.to_string(), value: Value::Text("tag".to_string()) },
                Argument { name: MESSAGE_LABEL.to_string(), value: Value::Text("msg".to_string()) },
            ],
        };

        let mut buffer = Cursor::new(vec![0u8; MAX_DATAGRAM_LEN]);
        let mut encoder = Encoder::new(&mut buffer);
        encoder.write_record(&record).unwrap();
        let encoded = &buffer.get_ref().as_slice()[..buffer.position() as usize];
        let parsed = Message::from_structured(&*TEST_IDENTITY, encoded).unwrap();
        assert_eq!(
            parsed,
            Message::new(
                zx::Time::from_nanos(72),
                Severity::Error,
                encoded.len(),
                2, // dropped
                &*TEST_IDENTITY,
                LogsHierarchy::new(
                    "root",
                    vec![
                        LogsProperty::Int(LogsField::Other("arg1".to_string()), -23),
                        LogsProperty::Uint(LogsField::ProcessId, 43),
                        LogsProperty::Uint(LogsField::ThreadId, 912),
                        LogsProperty::String(LogsField::Tag, "tag".to_string()),
                        LogsProperty::String(LogsField::Msg, "msg".to_string()),
                    ],
                    vec![],
                )
            )
        );
        assert_eq!(
            parsed.for_listener(),
            LogMessage {
                severity: LegacySeverity::Error.for_listener(),
                time: 72,
                dropped_logs: 2,
                pid: 43,
                tid: 912,
                msg: "msg arg1=-23".into(),
                tags: vec!["tag".into()]
            }
        );

        // multiple tags
        let record = Record {
            timestamp: 72,
            severity: StreamSeverity::Error,
            arguments: vec![
                Argument { name: TAG_LABEL.to_string(), value: Value::Text("tag1".to_string()) },
                Argument { name: TAG_LABEL.to_string(), value: Value::Text("tag2".to_string()) },
                Argument { name: TAG_LABEL.to_string(), value: Value::Text("tag3".to_string()) },
            ],
        };
        let mut buffer = Cursor::new(vec![0u8; MAX_DATAGRAM_LEN]);
        let mut encoder = Encoder::new(&mut buffer);
        encoder.write_record(&record).unwrap();
        let encoded = &buffer.get_ref().as_slice()[..buffer.position() as usize];
        let parsed = Message::from_structured(&*TEST_IDENTITY, encoded).unwrap();
        assert_eq!(
            parsed,
            Message::new(
                zx::Time::from_nanos(72),
                Severity::Error,
                encoded.len(),
                0, // dropped
                &*TEST_IDENTITY,
                LogsHierarchy::new(
                    "root",
                    vec![
                        LogsProperty::String(LogsField::Tag, "tag1".to_string()),
                        LogsProperty::String(LogsField::Tag, "tag2".to_string()),
                        LogsProperty::String(LogsField::Tag, "tag3".to_string()),
                    ],
                    vec![],
                )
            )
        );

        // empty record
        let record = Record { timestamp: 72, severity: StreamSeverity::Error, arguments: vec![] };
        let mut buffer = Cursor::new(vec![0u8; MAX_DATAGRAM_LEN]);
        let mut encoder = Encoder::new(&mut buffer);
        encoder.write_record(&record).unwrap();
        let encoded = &buffer.get_ref().as_slice()[..buffer.position() as usize];
        let parsed = Message::from_structured(&*TEST_IDENTITY, encoded).unwrap();
        assert_eq!(
            parsed,
            Message::new(
                zx::Time::from_nanos(72),
                Severity::Error,
                encoded.len(),
                0, // dropped
                &*TEST_IDENTITY,
                LogsHierarchy::new("root", vec![], vec![],)
            )
        );

        // parse error
        assert!(matches!(
            Message::from_structured(&*TEST_IDENTITY, &vec![]).unwrap_err(),
            StreamError::ParseError { .. }
        ));
    }

    macro_rules! severity_roundtrip_test {
        ($raw:expr, $expected:expr) => {
            let legacy = LegacySeverity::try_from($raw).unwrap();
            let (severity, verbosity) = legacy.for_structured();
            let mut msg = Message::new(
                0i64, // timestamp
                severity,
                1, // size
                0, // dropped logs
                &*TEST_IDENTITY,
                LogsHierarchy::new("root", vec![], vec![]),
            );
            if let Some(v) = verbosity {
                msg.set_legacy_verbosity(v);
            }

            let legacy_msg = msg.clone().for_listener();
            assert_eq!(
                legacy_msg.severity, $expected,
                "failed to round trip severity for {:?} (raw {}), intermediates: {:#?}\n{:#?}",
                legacy, $raw, msg, legacy_msg
            );
        };
    }

    #[test]
    fn verbosity_roundtrip_legacy_v10() {
        severity_roundtrip_test!(-10, INFO - 10);
    }

    #[test]
    fn verbosity_roundtrip_legacy_v5() {
        severity_roundtrip_test!(-5, INFO - 5);
    }

    #[test]
    fn verbosity_roundtrip_legacy_v4() {
        severity_roundtrip_test!(-4, INFO - 4);
    }

    #[test]
    fn verbosity_roundtrip_legacy_v3() {
        severity_roundtrip_test!(-3, INFO - 3);
    }

    #[test]
    fn verbosity_roundtrip_legacy_v2() {
        severity_roundtrip_test!(-2, TRACE);
    }

    #[test]
    fn severity_roundtrip_legacy_v1() {
        severity_roundtrip_test!(-1, DEBUG);
    }

    #[test]
    fn verbosity_roundtrip_legacy_v0() {
        severity_roundtrip_test!(0, INFO);
    }

    #[test]
    fn severity_roundtrip_legacy_warn() {
        severity_roundtrip_test!(1, WARN);
    }

    #[test]
    fn verbosity_roundtrip_legacy_error() {
        severity_roundtrip_test!(2, ERROR);
    }

    #[test]
    fn severity_roundtrip_trace() {
        severity_roundtrip_test!(TRACE, TRACE);
    }

    #[test]
    fn severity_roundtrip_debug() {
        severity_roundtrip_test!(DEBUG, DEBUG);
    }

    #[test]
    fn severity_roundtrip_info() {
        severity_roundtrip_test!(INFO, INFO);
    }

    #[test]
    fn severity_roundtrip_warn() {
        severity_roundtrip_test!(WARN, WARN);
    }

    #[test]
    fn severity_roundtrip_error() {
        severity_roundtrip_test!(ERROR, ERROR);
    }
}
