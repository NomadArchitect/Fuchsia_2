// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{constants::*, logs::utils::Listener, test_topology};
use archivist_lib::logs::message::fx_log_packet_t;
use diagnostics_hierarchy::assert_data_tree;
use diagnostics_reader::{ArchiveReader, Logs, Severity};
use fidl::{Socket, SocketOpts};
use fidl_fuchsia_diagnostics::ArchiveAccessorMarker;
use fidl_fuchsia_logger::{LogFilterOptions, LogLevelFilter, LogMarker, LogSinkMarker};
use fuchsia_async as fasync;
use fuchsia_component::client;
use fuchsia_syslog::levels::INFO;
use fuchsia_syslog_listener::run_log_listener_with_proxy;
use futures::{channel::mpsc, StreamExt};

// This test verifies that Archivist knows about logging from this component.
#[fuchsia::test]
async fn log_attribution() {
    let mut builder = test_topology::create(test_topology::Options::default())
        .await
        .expect("create base topology");
    test_topology::add_eager_component(&mut builder, "child", STUB_INSPECT_COMPONENT_URL)
        .await
        .expect("add child");

    let instance = builder.build().create().await.expect("create instance");

    let accessor =
        instance.root.connect_to_protocol_at_exposed_dir::<ArchiveAccessorMarker>().unwrap();
    let mut result = ArchiveReader::new()
        .with_archive(accessor)
        .snapshot_then_subscribe::<Logs>()
        .expect("snapshot then subscribe");

    for log_str in &["This is a syslog message", "This is another syslog message"] {
        let log_record = result.next().await.expect("received log").expect("log is not an error");

        assert_eq!(
            log_record.moniker,
            format!("fuchsia_component_test_collection:{}/test/child", instance.root.child_name())
        );
        assert_eq!(log_record.metadata.component_url, Some(STUB_INSPECT_COMPONENT_URL.to_string()));
        assert_eq!(log_record.metadata.severity, Severity::Info);
        assert_data_tree!(log_record.payload.unwrap(), root: contains {
            message: {
              value: log_str.to_string(),
            }
        });
    }
}

#[fuchsia::test]
async fn log_unattributed_stream() {
    let builder = test_topology::create(test_topology::Options::default())
        .await
        .expect("create base topology");

    let instance = builder.build().create().await.expect("create instance");

    // Bind to Log to start archivist.
    let log_proxy = instance.root.connect_to_protocol_at_exposed_dir::<LogMarker>().unwrap();

    let path = format!(
         "/hub/children/fuchsia_component_test_collection:{}/children/test/children/archivist/exec/expose/fuchsia.logger.LogSink",
         instance.root.child_name()
    );

    // connect multiple identical log sinks
    for _ in 0..50 {
        let (message_client, message_server) = Socket::create(SocketOpts::DATAGRAM).unwrap();
        let log_sink = client::connect_to_protocol_at_path::<LogSinkMarker>(&path).unwrap();
        log_sink.connect(message_server).unwrap();

        // each with the same message repeated multiple times
        let mut packet = fx_log_packet_t::default();
        packet.metadata.pid = 1000;
        packet.metadata.tid = 2000;
        packet.metadata.severity = LogLevelFilter::Info.into_primitive().into();
        packet.data[0] = 0;
        packet.add_data(1, "repeated log".as_bytes());
        for _ in 0..5 {
            message_client.write(&mut packet.as_bytes()).unwrap();
        }
    }

    // run log listener
    let (send_logs, recv_logs) = mpsc::unbounded();
    fasync::Task::spawn(async move {
        let listen = Listener { send_logs };
        let mut options = LogFilterOptions {
            filter_by_pid: true,
            pid: 1000,
            filter_by_tid: true,
            tid: 2000,
            verbosity: 0,
            min_severity: LogLevelFilter::None,
            tags: Vec::new(),
        };
        run_log_listener_with_proxy(&log_proxy, listen, Some(&mut options), false, None)
            .await
            .unwrap();
    })
    .detach();

    // collect all logs
    let logs = recv_logs
        .map(|message| (message.severity, message.msg))
        .take(250)
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        logs,
        std::iter::repeat((INFO, "repeated log".to_owned())).take(250).collect::<Vec<_>>()
    );
}
