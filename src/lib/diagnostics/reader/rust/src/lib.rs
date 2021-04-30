// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use diagnostics_data::DiagnosticsData;
use fidl;
use fidl_fuchsia_diagnostics::{
    ArchiveAccessorMarker, ArchiveAccessorProxy, BatchIteratorMarker, BatchIteratorProxy,
    ClientSelectorConfiguration, Format, FormattedContent, PerformanceConfiguration, ReaderError,
    SelectorArgument, StreamMode, StreamParameters,
};
use fuchsia_async::{self as fasync, DurationExt, Task, TimeoutExt};
use fuchsia_component::client;
use fuchsia_zircon::{self as zx, Duration, DurationNum};
use futures::{channel::mpsc, prelude::*, sink::SinkExt, stream::FusedStream};
use pin_project::pin_project;
use serde_json::Value as JsonValue;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use thiserror::Error;

use parking_lot::Mutex;

pub use diagnostics_data::{Data, Inspect, Lifecycle, Logs, Severity};
pub use diagnostics_hierarchy::{
    assert_data_tree, testing::*, tree_assertion, DiagnosticsHierarchy, Property,
};

const RETRY_DELAY_MS: i64 = 300;

/// Errors that this library can return
#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to connect to the archive accessor")]
    ConnectToArchive(#[source] anyhow::Error),

    #[error("Failed to create the BatchIterator channel ends")]
    CreateIteratorProxy(#[source] fidl::Error),

    #[error("Failed to stream diagnostics from the accessor")]
    StreamDiagnostics(#[source] fidl::Error),

    #[error("Failed to call iterator server")]
    GetNextCall(#[source] fidl::Error),

    #[error("Received error from the GetNext response: {0:?}")]
    GetNextReaderError(ReaderError),

    #[error("Failed to read json received")]
    ReadJson(#[source] serde_json::Error),

    #[error("Failed to parse the diagnostics data from the json received")]
    ParseDiagnosticsData(#[source] serde_json::Error),

    #[error("Failed to read vmo from the response")]
    ReadVmo(#[source] zx::Status),
}

/// An inspect tree selector for a component.
pub struct ComponentSelector {
    relative_moniker: Vec<String>,
    tree_selectors: Vec<String>,
}

impl ComponentSelector {
    /// Create a new component event selector.
    /// By default it will select the whole tree unless tree selectors are provided.
    /// `relative_moniker` is the realm path relative to the realm of the running component plus the
    /// component name. For example: [a, b, component.cmx].
    pub fn new(relative_moniker: Vec<String>) -> Self {
        Self { relative_moniker, tree_selectors: Vec::new() }
    }

    /// Select a section of the inspect tree.
    pub fn with_tree_selector(mut self, tree_selector: impl Into<String>) -> Self {
        self.tree_selectors.push(tree_selector.into());
        self
    }

    fn relative_moniker_str(&self) -> String {
        self.relative_moniker.join("/")
    }
}

pub trait ToSelectorArguments {
    fn to_selector_arguments(self) -> Vec<String>;
}

impl ToSelectorArguments for String {
    fn to_selector_arguments(self) -> Vec<String> {
        vec![self]
    }
}

impl ToSelectorArguments for &str {
    fn to_selector_arguments(self) -> Vec<String> {
        vec![self.to_string()]
    }
}

impl ToSelectorArguments for ComponentSelector {
    fn to_selector_arguments(self) -> Vec<String> {
        let relative_moniker = self.relative_moniker_str();
        // If not tree selectors were provided, select the full tree.
        if self.tree_selectors.is_empty() {
            vec![format!("{}:root", relative_moniker.clone())]
        } else {
            self.tree_selectors
                .iter()
                .map(|s| format!("{}:{}", relative_moniker.clone(), s.clone()))
                .collect()
        }
    }
}

/// Utility for reading inspect data of a running component using the injected Archive
/// Reader service.
#[derive(Clone)]
pub struct ArchiveReader {
    archive: Arc<Mutex<Option<ArchiveAccessorProxy>>>,
    selectors: Vec<String>,
    should_retry: bool,
    minimum_schema_count: usize,
    timeout: Option<Duration>,
    batch_retrieval_timeout_seconds: Option<i64>,
    max_aggregated_content_size_bytes: Option<u64>,
}

impl ArchiveReader {
    /// Creates a new data fetcher with default configuration:
    ///  - Maximum retries: 2^64-1
    ///  - Timeout: Never. Use with_timeout() to set a timeout.
    pub fn new() -> Self {
        Self {
            timeout: None,
            selectors: vec![],
            should_retry: true,
            archive: Arc::new(Mutex::new(None)),
            minimum_schema_count: 1,
            batch_retrieval_timeout_seconds: None,
            max_aggregated_content_size_bytes: None,
        }
    }

    pub fn with_archive(self, archive: ArchiveAccessorProxy) -> Self {
        {
            let mut arc = self.archive.lock();
            *arc = Some(archive);
        }
        self
    }

    /// Requests a single component tree (or sub-tree).
    pub fn add_selector(mut self, selector: impl ToSelectorArguments) -> Self {
        self.selectors.extend(selector.to_selector_arguments().into_iter());
        self
    }

    /// Requests to retry when an empty result is received.
    pub fn retry_if_empty(mut self, retry: bool) -> Self {
        self.should_retry = retry;
        self
    }

    pub fn add_selectors<T, S>(self, selectors: T) -> Self
    where
        T: Iterator<Item = S>,
        S: ToSelectorArguments,
    {
        let mut this = self;
        for selector in selectors {
            this = this.add_selector(selector);
        }
        this
    }

    /// Sets the maximum time to wait for a response from the Archive.
    /// Do not use in tests unless timeout is the expected behavior.
    pub fn with_timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
    }

    pub fn with_aggregated_result_bytes_limit(mut self, limit_bytes: u64) -> Self {
        self.max_aggregated_content_size_bytes = Some(limit_bytes);
        self
    }

    /// Set the maximum time to wait for a wait for a single component
    /// to have its diagnostics data "pumped".
    pub fn with_batch_retrieval_timeout_seconds(mut self, timeout: i64) -> Self {
        self.batch_retrieval_timeout_seconds = Some(timeout);
        self
    }

    /// Sets the minumum number of schemas expected in a result in order for the
    /// result to be considered a success.
    pub fn with_minimum_schema_count(mut self, minimum_schema_count: usize) -> Self {
        self.minimum_schema_count = minimum_schema_count;
        self
    }

    /// Connects to the ArchiveAccessor and returns data matching provided selectors.
    pub async fn snapshot<D>(&self) -> Result<Vec<Data<D>>, Error>
    where
        D: DiagnosticsData,
    {
        let raw_json = self.snapshot_raw::<D>().await?;
        Ok(serde_json::from_value(raw_json).map_err(Error::ReadJson)?)
    }

    pub fn snapshot_then_subscribe<D>(&self) -> Result<Subscription<D>, Error>
    where
        D: DiagnosticsData + 'static,
    {
        let iterator = self.batch_iterator::<D>(StreamMode::SnapshotThenSubscribe)?;
        Ok(Subscription::new(iterator))
    }

    /// Connects to the ArchiveAccessor and returns inspect data matching provided selectors.
    /// Returns the raw json for each hierarchy fetched.
    pub async fn snapshot_raw<D>(&self) -> Result<JsonValue, Error>
    where
        D: DiagnosticsData,
    {
        let timeout = self.timeout;
        let data_future = self.snapshot_raw_inner::<D>();
        let data = match timeout {
            Some(timeout) => data_future.on_timeout(timeout.after_now(), || Ok(Vec::new())).await?,
            None => data_future.await?,
        };
        Ok(JsonValue::Array(data))
    }

    async fn snapshot_raw_inner<D>(&self) -> Result<Vec<JsonValue>, Error>
    where
        D: DiagnosticsData,
    {
        loop {
            let mut result = Vec::new();
            let iterator = self.batch_iterator::<D>(StreamMode::Snapshot)?;
            drain_batch_iterator(iterator, |d| {
                result.push(d);
                async {}
            })
            .await?;

            if result.len() < self.minimum_schema_count && self.should_retry {
                fasync::Timer::new(fasync::Time::after(RETRY_DELAY_MS.millis())).await;
            } else {
                return Ok(result);
            }
        }
    }

    fn batch_iterator<D>(&self, mode: StreamMode) -> Result<BatchIteratorProxy, Error>
    where
        D: DiagnosticsData,
    {
        // TODO(fxbug.dev/58051) this should be done in an ArchiveReaderBuilder -> Reader init
        let mut archive = self.archive.lock();
        if archive.is_none() {
            *archive = Some(
                client::connect_to_protocol::<ArchiveAccessorMarker>()
                    .map_err(Error::ConnectToArchive)?,
            )
        }

        let archive = archive.as_ref().unwrap();

        let (iterator, server_end) = fidl::endpoints::create_proxy::<BatchIteratorMarker>()
            .map_err(Error::CreateIteratorProxy)?;

        let mut stream_parameters = StreamParameters::EMPTY;
        stream_parameters.stream_mode = Some(mode);
        stream_parameters.data_type = Some(D::DATA_TYPE);
        stream_parameters.format = Some(Format::Json);

        stream_parameters.client_selector_configuration = if self.selectors.is_empty() {
            Some(ClientSelectorConfiguration::SelectAll(true))
        } else {
            Some(ClientSelectorConfiguration::Selectors(
                self.selectors
                    .iter()
                    .map(|selector| SelectorArgument::RawSelector(selector.clone()))
                    .collect(),
            ))
        };

        stream_parameters.performance_configuration = Some(PerformanceConfiguration {
            max_aggregate_content_size_bytes: self.max_aggregated_content_size_bytes,
            batch_retrieval_timeout_seconds: self.batch_retrieval_timeout_seconds,
            ..PerformanceConfiguration::EMPTY
        });

        archive
            .stream_diagnostics(stream_parameters, server_end)
            .map_err(Error::StreamDiagnostics)?;
        Ok(iterator)
    }
}

async fn drain_batch_iterator<Fut>(
    iterator: BatchIteratorProxy,
    mut send: impl FnMut(serde_json::Value) -> Fut,
) -> Result<(), Error>
where
    Fut: Future<Output = ()>,
{
    loop {
        let next_batch = iterator
            .get_next()
            .await
            .map_err(Error::GetNextCall)?
            .map_err(Error::GetNextReaderError)?;
        if next_batch.is_empty() {
            return Ok(());
        }
        for formatted_content in next_batch {
            match formatted_content {
                FormattedContent::Json(data) => {
                    let mut buf = vec![0; data.size as usize];
                    data.vmo.read(&mut buf, 0).map_err(Error::ReadVmo)?;
                    let hierarchy_json = std::str::from_utf8(&buf).unwrap();
                    let output: JsonValue =
                        serde_json::from_str(&hierarchy_json).map_err(Error::ReadJson)?;

                    match output {
                        output @ JsonValue::Object(_) => {
                            send(output).await;
                        }
                        JsonValue::Array(values) => {
                            for value in values {
                                send(value).await;
                            }
                        }
                        _ => unreachable!(
                            "ArchiveAccessor only returns top-level objects and arrays"
                        ),
                    }
                }
                _ => unreachable!("JSON was requested, no other data type should be received"),
            }
        }
    }
}

#[pin_project]
pub struct Subscription<M: DiagnosticsData> {
    #[pin]
    recv: mpsc::Receiver<Result<Data<M>, Error>>,
    _drain_task: Task<()>,
}

const DATA_CHANNEL_SIZE: usize = 32;
const ERROR_CHANNEL_SIZE: usize = 2;

impl<M> Subscription<M>
where
    M: DiagnosticsData + 'static,
{
    /// Creates a new subscription stream to a batch iterator.
    /// The stream will return diagnostics data structures.
    pub fn new(iterator: BatchIteratorProxy) -> Self {
        let (mut sender, recv) = mpsc::channel(DATA_CHANNEL_SIZE);
        let _drain_task = Task::spawn(async move {
            let drain_result = drain_batch_iterator(iterator, |d| {
                let mut sender = sender.clone();
                async move {
                    match serde_json::from_value(d) {
                        Ok(d) => sender.send(Ok(d)).await.ok(),
                        Err(e) => sender.send(Err(Error::ParseDiagnosticsData(e))).await.ok(),
                    };
                }
            })
            .await;

            if let Err(e) = drain_result {
                sender.send(Err(e)).await.ok();
            }
        });

        Subscription { recv, _drain_task }
    }

    /// Splits the subscription into two separate streams: results and errors.
    pub fn split_streams(mut self) -> (SubscriptionResultsStream<M>, mpsc::Receiver<Error>) {
        let (mut errors_sender, errors) = mpsc::channel(ERROR_CHANNEL_SIZE);
        let (mut results_sender, recv) = mpsc::channel(DATA_CHANNEL_SIZE);
        let _drain_task = fasync::Task::spawn(async move {
            while let Some(result) = self.next().await {
                match result {
                    Ok(value) => results_sender.send(value).await.ok(),
                    Err(e) => errors_sender.send(e).await.ok(),
                };
            }
        });
        (SubscriptionResultsStream { recv, _drain_task }, errors)
    }
}

impl<M> Stream for Subscription<M>
where
    M: DiagnosticsData + 'static,
{
    type Item = Result<Data<M>, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.recv.poll_next(cx)
    }
}

impl<M> FusedStream for Subscription<M>
where
    M: DiagnosticsData + 'static,
{
    fn is_terminated(&self) -> bool {
        self.recv.is_terminated()
    }
}

#[pin_project]
pub struct SubscriptionResultsStream<M: DiagnosticsData> {
    #[pin]
    recv: mpsc::Receiver<Data<M>>,
    _drain_task: fasync::Task<()>,
}

impl<M> Stream for SubscriptionResultsStream<M>
where
    M: DiagnosticsData + 'static,
{
    type Item = Data<M>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.recv.poll_next(cx)
    }
}

impl<M> FusedStream for SubscriptionResultsStream<M>
where
    M: DiagnosticsData + 'static,
{
    fn is_terminated(&self) -> bool {
        self.recv.is_terminated()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        anyhow::format_err,
        diagnostics_data::{Data, LifecycleType},
        diagnostics_hierarchy::assert_data_tree,
        fidl_fuchsia_diagnostics as fdiagnostics,
        fidl_fuchsia_sys::ComponentControllerEvent,
        fuchsia_component::{
            client::App,
            server::{NestedEnvironment, ServiceFs},
        },
        fuchsia_zircon as zx,
        futures::{StreamExt, TryStreamExt},
    };

    const TEST_COMPONENT_URL: &str =
        "fuchsia-pkg://fuchsia.com/diagnostics-reader-tests#meta/inspect_test_component.cmx";

    async fn start_component(env_label: &str) -> Result<(NestedEnvironment, App), anyhow::Error> {
        let mut service_fs = ServiceFs::new();
        let env = service_fs.create_nested_environment(env_label)?;
        let app = client::launch(&env.launcher(), TEST_COMPONENT_URL.to_string(), None)?;
        fasync::Task::spawn(service_fs.collect()).detach();
        let mut component_stream = app.controller().take_event_stream();
        match component_stream
            .next()
            .await
            .expect("component event stream ended before termination event")?
        {
            ComponentControllerEvent::OnTerminated { return_code, termination_reason } => {
                return Err(format_err!(
                    "Component terminated unexpectedly. Code: {}. Reason: {:?}",
                    return_code,
                    termination_reason
                ));
            }
            ComponentControllerEvent::OnDirectoryReady {} => {}
        }
        Ok((env, app))
    }

    #[fuchsia::test]
    async fn lifecycle_events_for_component() {
        let (_env, _app) = start_component("test-lifecycle").await.unwrap();

        // TODO(fxbug.dev/51165): use selectors for this filtering and remove the delayed retry
        // which would be taken care of by the ArchiveReader itself.
        loop {
            let results = ArchiveReader::new()
                .snapshot::<Lifecycle>()
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.moniker.starts_with("test-lifecycle"))
                .collect::<Vec<_>>();
            // Note: the ArchiveReader retries when the response is empty. However, here the
            // response might not be empty (it can contain the archivist itself) but when we filter
            // looking for the moniker we are interested on, that one might not be available.
            // Metadata selectors would solve this as the archivist response would be empty and we
            // wouldn't need to filter and retry here.
            if results.is_empty() {
                fasync::Timer::new(fasync::Time::after(RETRY_DELAY_MS.millis())).await;
                continue;
            }
            let started = &results[0];
            assert_eq!(started.metadata.lifecycle_event_type, LifecycleType::Started);
            assert_eq!(started.metadata.component_url, TEST_COMPONENT_URL);
            assert_eq!(started.moniker, "test-lifecycle/inspect_test_component.cmx");
            assert_eq!(started.payload, None);
            break;
        }
    }

    #[fuchsia::test]
    async fn inspect_data_for_component() -> Result<(), anyhow::Error> {
        let (_env, _app) = start_component("test-ok").await?;

        let results = ArchiveReader::new()
            .add_selector("test-ok/inspect_test_component.cmx:root".to_string())
            .snapshot::<Inspect>()
            .await?;

        assert_eq!(results.len(), 1);
        assert_data_tree!(results[0].payload.as_ref().unwrap(), root: {
            int: 3u64,
            "lazy-node": {
                a: "test",
                child: {
                    double: 3.14,
                },
            }
        });

        let response = ArchiveReader::new()
            .add_selector(
                ComponentSelector::new(vec![
                    "test-ok".to_string(),
                    "inspect_test_component.cmx".to_string(),
                ])
                .with_tree_selector("root:int")
                .with_tree_selector("root/lazy-node:a"),
            )
            .snapshot::<Inspect>()
            .await?;

        assert_eq!(response.len(), 1);

        assert_eq!(response[0].metadata.component_url, TEST_COMPONENT_URL);
        assert_eq!(response[0].moniker, "test-ok/inspect_test_component.cmx");

        assert_data_tree!(response[0].payload.as_ref().unwrap(), root: {
            int: 3u64,
            "lazy-node": {
                a: "test"
            }
        });

        Ok(())
    }

    #[fuchsia::test]
    async fn timeout() -> Result<(), anyhow::Error> {
        let (_env, _app) = start_component("test-timeout").await?;

        let result = ArchiveReader::new()
            .add_selector("test-timeout/inspect_test_component.cmx:root")
            .with_timeout(0.nanos())
            .snapshot::<Inspect>()
            .await;
        assert!(result.unwrap().is_empty());
        Ok(())
    }

    #[fuchsia::test]
    async fn component_selector() {
        let selector = ComponentSelector::new(vec!["a.cmx".to_string()]);
        assert_eq!(selector.relative_moniker_str(), "a.cmx");
        let arguments: Vec<String> = selector.to_selector_arguments();
        assert_eq!(arguments, vec!["a.cmx:root".to_string()]);

        let selector =
            ComponentSelector::new(vec!["b".to_string(), "c".to_string(), "a.cmx".to_string()]);
        assert_eq!(selector.relative_moniker_str(), "b/c/a.cmx");

        let selector = selector.with_tree_selector("root/b/c:d").with_tree_selector("root/e:f");
        let arguments: Vec<String> = selector.to_selector_arguments();
        assert_eq!(
            arguments,
            vec!["b/c/a.cmx:root/b/c:d".to_string(), "b/c/a.cmx:root/e:f".to_string(),]
        );
    }

    #[fuchsia::test]
    async fn custom_archive() {
        let proxy = spawn_fake_archive();
        let result = ArchiveReader::new()
            .with_archive(proxy)
            .snapshot::<Inspect>()
            .await
            .expect("got result");
        assert_eq!(result.len(), 1);
        assert_data_tree!(result[0].payload.as_ref().unwrap(), root: { x: 1u64 });
    }

    fn spawn_fake_archive() -> fdiagnostics::ArchiveAccessorProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fdiagnostics::ArchiveAccessorMarker>()
                .expect("create proxy");
        fasync::Task::spawn(async move {
            while let Some(request) = stream.try_next().await.expect("stream request") {
                match request {
                    fdiagnostics::ArchiveAccessorRequest::StreamDiagnostics {
                        result_stream,
                        ..
                    } => {
                        fasync::Task::spawn(async move {
                            let mut called = false;
                            let mut stream = result_stream.into_stream().expect("into stream");
                            while let Some(req) = stream.try_next().await.expect("stream request") {
                                match req {
                                    fdiagnostics::BatchIteratorRequest::GetNext { responder } => {
                                        if called {
                                            responder
                                                .send(&mut Ok(Vec::new()))
                                                .expect("send response");
                                            continue;
                                        }
                                        called = true;
                                        let result = Data::for_inspect(
                                            "moniker",
                                            Some(DiagnosticsHierarchy::new(
                                                "root",
                                                vec![Property::Uint("x".to_string(), 1)],
                                                vec![],
                                            )),
                                            0i64,
                                            "component-url",
                                            "filename",
                                            vec![],
                                        );
                                        let content = serde_json::to_string_pretty(&result)
                                            .expect("json pretty");
                                        let vmo_size = content.len() as u64;
                                        let vmo =
                                            zx::Vmo::create(vmo_size as u64).expect("create vmo");
                                        vmo.write(content.as_bytes(), 0).expect("write vmo");
                                        let buffer =
                                            fidl_fuchsia_mem::Buffer { vmo, size: vmo_size };
                                        responder
                                            .send(&mut Ok(vec![
                                                fdiagnostics::FormattedContent::Json(buffer),
                                            ]))
                                            .expect("send response");
                                    }
                                }
                            }
                        })
                        .detach();
                    }
                }
            }
        })
        .detach();
        return proxy;
    }
}
