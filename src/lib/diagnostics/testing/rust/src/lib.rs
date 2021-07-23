// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

//! # Diagnostics testing
//!
//! This library provides utilities for starting a nested environment with an archivist and
//! a useful API for quickly gathering inspect and logs for components in that environment.
//!
//! Note that this API is only compatible with components v1.
//!
//! ## Exmaple usage
//!
//! ```rust
//! let mut test_realm = EnvWithDiagnostics::new().await;
//!
//! // Listen for logs in the nested environment. This returns a stream of logs.
//! let logs = test_realm.listen_to_logs();
//!
//! let (app: _app, reader} = test_realm.launch(/* some component url */);
//!
//! // Get the inspect data of the component that was launched above.
//! let inspect = reader.inspect().await:
//!
//! // Get the inspect data of some other component that was launched by the component above.
//! let nested_inspect = test_realm
//!     .reader_for("some_other_component_that_was_launched.cmx")
//!     .inspect().await;
//!
//! assert_data_tree!(inspect.payload.as_ref().unwrap(), root: {
//!   ...
//! });
//! assert_data_tree!(nested_inspect.payload.as_ref().unwrap(), root: {
//!   ...
//! });
//!
//!  // Assert the first message in the log stream.
//!  let logs_message = logs.next().await.unwrap();
//!  assert_eq!(logs_message, ...);
//! ```

use diagnostics_data::{Data, DiagnosticsData, InspectData};
use diagnostics_reader::{ArchiveReader, ComponentSelector};
use fidl::endpoints::ProtocolMarker;
use fidl_fuchsia_diagnostics::{ArchiveAccessorMarker, ArchiveAccessorProxy};
use fidl_fuchsia_logger::{LogFilterOptions, LogLevelFilter, LogMarker, LogMessage, LogSinkMarker};
use fidl_fuchsia_sys::{ComponentControllerEvent::*, LauncherProxy};
use fuchsia_async::Task;
use fuchsia_component::{
    client::{launch_with_options, App, LaunchOptions},
    server::ServiceFs,
};
use fuchsia_syslog_listener::run_log_listener_with_proxy;
use fuchsia_url::pkg_url::PkgUrl;
use fuchsia_zircon as zx;
use futures::{channel::mpsc, prelude::*};
use tracing::*;

pub use diagnostics_data::{Inspect, Lifecycle, LifecycleType, Logs, Severity};
pub use diagnostics_hierarchy::assert_data_tree;

const ARCHIVIST_URL: &str =
    "fuchsia-pkg://fuchsia.com/archivist-for-embedding#meta/archivist-for-embedding.cmx";

/// A nested environment providing utilities for accessing diagnostics data easily.
pub struct EnvWithDiagnostics {
    launcher: LauncherProxy,
    archivist: App,
    archive: ArchiveAccessorProxy,
    _env_task: Task<()>,
    listeners: Vec<Task<()>>,
}

impl EnvWithDiagnostics {
    /// Construct a new nested environment with a diagnostics archivist. Requires access to the
    /// `fuchsia.sys.Launcher` protocol.
    // TODO(fxbug.dev/58351) cooperate with run-test-component to avoid double-spawning archivist
    pub async fn new() -> Self {
        let mut fs = ServiceFs::new();
        let env = fs.create_salted_nested_environment("diagnostics").unwrap();
        let launcher = env.launcher().clone();
        let _env_task = Task::spawn(async move {
            let _env = env; // move env into the task so it stays alive
            fs.collect::<()>().await
        });

        // creating a proxy to logsink in our own environment, otherwise embedded archivist just
        // eats its own logs via logconnector
        let options = {
            let mut options = LaunchOptions::new();
            let (dir_client, dir_server) = zx::Channel::create().unwrap();
            let mut fs = ServiceFs::new();
            fs.add_proxy_service::<LogSinkMarker, _>().serve_connection(dir_server).unwrap();
            Task::spawn(fs.collect()).detach();
            options.set_additional_services(vec![LogSinkMarker::NAME.to_string()], dir_client);
            options
        };

        let archivist =
            launch_with_options(&launcher, ARCHIVIST_URL.to_string(), None, options).unwrap();
        let archive = archivist.connect_to_protocol::<ArchiveAccessorMarker>().unwrap();

        let mut archivist_events = archivist.controller().take_event_stream();
        if let OnTerminated { .. } = archivist_events.next().await.unwrap().unwrap() {
            panic!("archivist terminated early");
        }

        Self { archivist, archive, launcher, _env_task, listeners: vec![] }
    }

    /// Launch the app from the given URL with the given arguments, collecting its diagnostics.
    /// Returns a reader for the component's diagnostics.
    pub fn launch(&self, url: &str, args: Option<Vec<String>>) -> Launched {
        let mut launch_options = LaunchOptions::new();
        let (dir_client, dir_server) = zx::Channel::create().unwrap();
        let mut fs = ServiceFs::new();
        fs.add_proxy_service_to::<ArchiveAccessorMarker, _>(
            self.archivist.directory_request().clone(),
        )
        .serve_connection(dir_server)
        .unwrap();
        Task::spawn(fs.collect()).detach();
        launch_options
            .set_additional_services(vec![ArchiveAccessorMarker::NAME.to_string()], dir_client);

        let url = PkgUrl::parse(url).unwrap();
        let manifest = url.resource().unwrap().rsplit('/').next().unwrap();
        let reader = self.reader_for(manifest, &[]);
        let app =
            launch_with_options(&self.launcher, url.to_string(), args, launch_options).unwrap();
        Launched { app, reader }
    }

    /// Returns the writer-half of a syslog socket, the reader half of which has been sent to
    /// the embedded archivist. The embedded archivist expects to receive logs in the legacy
    /// wire format. Pass this socket to [`fuchsia_syslog::init_with_socket_and_name`] to send
    /// the invoking component's logs to the embedded archivist.
    pub fn legacy_log_socket(&self) -> zx::Socket {
        let sink = self.archivist.connect_to_protocol::<LogSinkMarker>().unwrap();
        let (tx, rx) = zx::Socket::create(zx::SocketOpts::empty()).unwrap();
        sink.connect(rx).unwrap();
        tx
    }

    /// Returns a stream of logs for the whole environment.
    pub fn listen_to_logs(&mut self) -> impl Stream<Item = LogMessage> {
        // start listening
        let log_proxy = self.archivist.connect_to_protocol::<LogMarker>().unwrap();
        let mut options = LogFilterOptions {
            filter_by_pid: false,
            pid: 0,
            min_severity: LogLevelFilter::None,
            verbosity: 0,
            filter_by_tid: false,
            tid: 0,
            tags: vec![],
        };
        let (send_logs, recv_logs) = mpsc::unbounded();
        let listener = Task::spawn(async move {
            run_log_listener_with_proxy(&log_proxy, send_logs, Some(&mut options), false, None)
                .await
                .unwrap();
        });

        self.listeners.push(listener);
        recv_logs.filter(|m| {
            let from_archivist = m.tags.iter().any(|t| t == "archivist");
            async move { !from_archivist }
        })
    }

    /// Returns a reader for the provided manifest, assuming it was launched in this environment
    /// under the `realms` provided.
    pub fn reader_for(&self, manifest: &str, realms: &[&str]) -> AppReader {
        AppReader::new(self.archive.clone(), manifest, realms)
    }
}

/// A reference to the component that was launched inside the nested environment providing
/// utilities for accessing its diagnostics data.
pub struct Launched {
    pub app: App,
    pub reader: AppReader,
}

/// A reader for a launched component's inspect.
pub struct AppReader {
    reader: ArchiveReader,
    _logs_tasks: Vec<Task<()>>,
}

impl AppReader {
    /// Construct a new `AppReader` with the given `archive` for the given `manifest`. Pass `realms`
    /// a list of nested environments relative to the archive if the component is not launched as
    /// a sibling to the archive.
    pub fn new(archive: ArchiveAccessorProxy, manifest: &str, realms: &[&str]) -> Self {
        let mut moniker = realms.iter().map(ToString::to_string).collect::<Vec<_>>();
        moniker.push(manifest.to_string());

        let mut reader = ArchiveReader::new();
        reader
            .with_archive(archive)
            .with_minimum_schema_count(1)
            .add_selector(ComponentSelector::new(moniker));
        Self { reader, _logs_tasks: Vec::new() }
    }

    /// Returns a snapshot of the requested data for this component.
    pub async fn snapshot<D: DiagnosticsData>(&self) -> Vec<Data<D>> {
        self.reader.snapshot::<D>().await.expect("snapshot will succeed")
    }

    /// Returns inspect data for this component.
    pub async fn inspect(&self) -> InspectData {
        self.snapshot::<Inspect>().await.into_iter().next().expect(">=1 item in results")
    }

    /// Returns a stream of log messages for this component.
    pub fn logs(&mut self) -> impl Stream<Item = Data<Logs>> {
        let (sub, mut errors) =
            self.reader.snapshot_then_subscribe::<Logs>().unwrap().split_streams();

        self._logs_tasks.push(Task::spawn(async move {
            loop {
                match errors.next().await {
                    Some(error) => error!(%error, "log testing client encountered an error"),
                    None => break,
                }
            }
        }));

        sub
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_data::assert_data_tree;
    use fidl_fuchsia_diagnostics::Severity;
    use futures::pin_mut;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn nested_apps_with_diagnostics() {
        let mut test_realm = EnvWithDiagnostics::new().await;
        let logs = test_realm.listen_to_logs();

        // launch the diagnostics emitter
        let Launched { app: _emitter, reader: emitter_reader } = test_realm.launch(
            "fuchsia-pkg://fuchsia.com/diagnostics-testing-tests#meta/emitter-for-test.cmx",
            None,
        );

        let emitter_inspect = emitter_reader.inspect().await;
        let nested_inspect =
            test_realm.reader_for("inspect_test_component.cmx", &[]).inspect().await;

        assert_data_tree!(emitter_inspect.payload.as_ref().unwrap(), root: {
            other_int: 7u64,
        });

        assert_data_tree!(nested_inspect.payload.as_ref().unwrap(), root: {
            int: 3u64,
            "lazy-node": {
                a: "test",
                child: {
                    double: 3.14,
                },
            }
        });

        async fn check_next_message(
            logs: &mut (impl Stream<Item = LogMessage> + Unpin),
            expected: &'static str,
        ) {
            let next_message = logs.next().await.unwrap();
            assert_eq!(next_message.tags, &["emitter_bin"]);
            assert_eq!(next_message.severity, Severity::Info as i32);
            assert_eq!(next_message.msg, expected);
        }

        pin_mut!(logs);
        check_next_message(&mut logs, "emitter started").await;
        check_next_message(&mut logs, "launching child").await;
    }
}
