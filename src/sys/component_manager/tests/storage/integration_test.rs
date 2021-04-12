// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Error,
    async_trait::async_trait,
    component_events::{events::*, injectors::*, matcher::EventMatcher},
    fidl_fidl_test_components as ftest, fidl_fuchsia_io as fio, fuchsia_async as fasync,
    fuchsia_syslog as syslog, fuchsia_zircon as zx,
    futures::lock::Mutex,
    futures::StreamExt,
    io_util::{self, OPEN_RIGHT_READABLE, OPEN_RIGHT_WRITABLE},
    lazy_static::lazy_static,
    std::{path::PathBuf, sync::Arc},
    test_utils_lib::opaque_test::*,
};

lazy_static! {
    static ref LOGGER: Logger = Logger::new();
}

struct Logger {}
impl Logger {
    fn new() -> Self {
        syslog::init_with_tags(&[]).expect("could not initialize logging");
        Self {}
    }

    fn init(&self) {}
}

#[fasync::run_singlethreaded(test)]
async fn storage() {
    LOGGER.init();

    let test = OpaqueTest::default(
        "fuchsia-pkg://fuchsia.com/storage_integration_test#meta/storage_realm.cm",
    )
    .await
    .unwrap();

    let event_source = test.connect_to_event_source().await.unwrap();
    let mut event_stream = event_source
        .subscribe(vec![EventSubscription::new(vec![Started::NAME], EventMode::Async)])
        .await
        .unwrap();

    event_source.start_component_tree().await;

    // Expect the root component to be bound to
    EventMatcher::ok().moniker(".").expect_match::<Started>(&mut event_stream).await;

    // Expect the 2 children to be bound to
    EventMatcher::ok().expect_match::<Started>(&mut event_stream).await;

    EventMatcher::ok().expect_match::<Started>(&mut event_stream).await;

    let component_manager_path = test.get_component_manager_path();
    let memfs_path =
        component_manager_path.join("out/hub/children/memfs/exec/out/svc/fuchsia.io.Directory");
    let data_path = component_manager_path.join("out/hub/children/storage_user/exec/out/data");

    check_storage(memfs_path, data_path, "storage_user:0").await;
}

#[fasync::run_singlethreaded(test)]
async fn storage_from_collection() {
    LOGGER.init();

    let test = OpaqueTest::default(
        "fuchsia-pkg://fuchsia.com/storage_integration_test#meta/storage_realm_coll.cm",
    )
    .await
    .unwrap();

    let event_source = test.connect_to_event_source().await.unwrap();
    let mut event_stream = event_source
        .subscribe(vec![EventSubscription::new(
            vec![Started::NAME, Destroyed::NAME],
            EventMode::Async,
        )])
        .await
        .unwrap();

    // Create a mutex that is used to hold the response from the trigger
    // service until after the tests inspects the storage.
    let trigger_lock = Arc::new(Mutex::new(()));
    let trigger_guard = trigger_lock.lock().await;

    // The root component connects to the Trigger capability to create a
    // rendezvous so the test can inspect storage before the child is
    // destroyed.
    let trigger_capability = TriggerCapability::new(trigger_lock.clone());
    trigger_capability.inject(&event_source, EventMatcher::ok()).await;

    event_source.start_component_tree().await;

    // Expect the root component to be started
    EventMatcher::ok().moniker(".").wait::<Started>(&mut event_stream).await.unwrap();

    // Expect 2 children to be started - one static and one dynamic
    // Order is irrelevant
    EventMatcher::ok().wait::<Started>(&mut event_stream).await.unwrap();

    EventMatcher::ok().wait::<Started>(&mut event_stream).await.unwrap();

    // With all children started, do the test
    let component_manager_path = test.get_component_manager_path();
    let memfs_path =
        component_manager_path.join("out/hub/children/memfs/exec/out/svc/fuchsia.io.Directory");
    let data_path = component_manager_path.join("out/hub/children/coll:storage_user/exec/out/data");

    check_storage(memfs_path.clone(), data_path, "coll:storage_user:1").await;

    // The storage state is checked, drop the guard and allow the
    // TriggerService to respond to the root component.
    drop(trigger_guard);

    // Expect the dynamic child to be destroyed
    EventMatcher::ok()
        .moniker("./coll:storage_user:1")
        .wait::<Destroyed>(&mut event_stream)
        .await
        .unwrap();

    println!("checking that storage was destroyed");
    let memfs_proxy = io_util::open_directory_in_namespace(
        memfs_path.to_str().unwrap(),
        OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
    )
    .expect("failed to open storage");
    assert_eq!(list_directory(&memfs_proxy).await.unwrap(), Vec::<String>::new());
}

struct TriggerCapability {
    lock: Arc<Mutex<()>>,
}

impl TriggerCapability {
    fn new(l: Arc<Mutex<()>>) -> Arc<Self> {
        Arc::new(Self { lock: l })
    }
}

#[async_trait]
impl ProtocolInjector for TriggerCapability {
    type Marker = ftest::TriggerMarker;

    async fn serve(
        self: Arc<Self>,
        mut request_stream: ftest::TriggerRequestStream,
    ) -> Result<(), Error> {
        while let Some(Ok(ftest::TriggerRequest::Run { responder })) = request_stream.next().await {
            let _guard = self.lock.lock().await;
            responder.send("")?;
        }
        Ok(())
    }
}

async fn check_storage(memfs_path: PathBuf, data_path: PathBuf, user_moniker: &str) {
    let memfs_path = memfs_path.to_str().expect("unexpected chars");
    let data_path = data_path.to_str().expect("unexpected chars");

    let child_data_proxy =
        io_util::open_directory_in_namespace(data_path, OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE)
            .expect("failed to open storage");

    println!("successfully opened \"storage_user\" exposed data directory");

    let file_name = "hippo";
    let file_contents = "hippos_are_neat";

    let file = io_util::open_file(
        &child_data_proxy,
        &PathBuf::from(file_name),
        OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE | fio::OPEN_FLAG_CREATE,
    )
    .expect("failed to open file in storage");
    let (s, _) = file.write(&file_contents.as_bytes()).await.unwrap();
    assert_eq!(zx::Status::OK, zx::Status::from_raw(s), "writing to the file failed");

    println!("successfully wrote to file \"hippo\" in exposed data directory");

    let memfs_proxy =
        io_util::open_directory_in_namespace(memfs_path, OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE)
            .expect("failed to open storage");

    println!("successfully opened \"memfs\" exposed directory");

    let file_proxy = io_util::open_file(
        &memfs_proxy,
        &PathBuf::from(&format!("{}/data/hippo", user_moniker)),
        OPEN_RIGHT_READABLE,
    )
    .expect("failed to open file in memfs");
    let read_contents =
        io_util::read_file(&file_proxy).await.expect("failed to read file in memfs");

    println!("successfully read back contents of file from memfs directly");
    assert_eq!(read_contents, file_contents, "file contents did not match what was written");
}
