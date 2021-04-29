// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    component_events::{
        events::{Event, EventMode, EventSource, EventSubscription, Resolved, Started},
        matcher::EventMatcher,
    },
    fidl_fuchsia_io as fio, fidl_fuchsia_sys2 as fsys, fuchsia_async as fasync,
    fuchsia_component::client::connect_to_service,
    fuchsia_syslog as syslog,
};

#[fasync::run_singlethreaded]
async fn main() {
    syslog::init_with_tags(&["resolved_error_reporter"]).unwrap();

    // Track all the starting child components.
    let event_source = EventSource::new().unwrap();
    let mut event_stream = event_source
        .subscribe(vec![EventSubscription::new(
            vec![Resolved::NAME, Started::NAME],
            EventMode::Async,
        )])
        .await
        .unwrap();

    event_source.start_component_tree().await;

    // This will trigger the resolution of the child.
    let realm = connect_to_service::<fsys::RealmMarker>().unwrap();
    let mut child_ref = fsys::ChildRef { name: "child_a".to_string(), collection: None };

    let (_, server_end) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>().unwrap();
    let _ = realm.bind_child(&mut child_ref, server_end).await;

    let _resolved_event = EventMatcher::err().expect_match::<Resolved>(&mut event_stream).await;

    // A started event should still be dispatched indicating failure due to a resolution
    // error.
    let _started_event = EventMatcher::err().expect_match::<Started>(&mut event_stream).await;
}
