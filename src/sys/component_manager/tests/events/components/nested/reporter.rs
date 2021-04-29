// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    component_events::{
        events::{Event, EventMode, EventSource, EventSubscription, Started},
        matcher::EventMatcher,
        sequence::*,
    },
    fuchsia_async as fasync, fuchsia_syslog as syslog,
};

#[fasync::run_singlethreaded]
async fn main() {
    syslog::init_with_tags(&["nested_reporter"]).unwrap();

    // Track all the starting child components.
    let event_source = EventSource::new().unwrap();
    let event_stream = event_source
        .subscribe(vec![EventSubscription::new(vec![Started::NAME], EventMode::Async)])
        .await
        .unwrap();
    event_source.start_component_tree().await;

    EventSequence::new()
        .all_of(
            vec![
                EventMatcher::ok().moniker("./child_a:0"),
                EventMatcher::ok().moniker("./child_b:0"),
                EventMatcher::ok().moniker("./child_c:0"),
            ],
            Ordering::Unordered,
        )
        .expect(event_stream)
        .await
        .unwrap();
}
