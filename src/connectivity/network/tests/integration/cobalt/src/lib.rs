// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use anyhow::Context as _;

#[fuchsia_async::run_singlethreaded(test)]
#[ignore = "TODO(https://fxbug.dev/56382): this test is flaky"]
async fn cobalt_metrics() -> Result<(), anyhow::Error> {
    // NB: netstack aggregates observations and logs them to cobalt once per
    // minute. We wait for calls to LogCobaltEvents to be made, so this test
    // takes about 180 seconds to run at the time of writing. If you're
    // modifying this test and have the ability to change the netstack
    // implementation, reducing that log period will improve the cycle time of
    // this test. The test assumes, nonetheless, a large enough cobalt logging
    // period so that the test always advances faster than the cobalt client in
    // Netstack produces new sets of logs.

    // netstack is launched here so that watch_logs2(networking_metrics::PROJECT_ID, ...)
    // can be called before the first socket is created.
    let netstack =
        fuchsia_component::client::connect_to_protocol::<fidl_fuchsia_net_stack::StackMarker>()
            .context("failed to connect to netstack")?;
    let interfaces = netstack.list_interfaces().await?;
    assert_eq!(
        &interfaces
            .into_iter()
            .map(|fidl_fuchsia_net_stack::InterfaceInfo { id, .. }| id)
            .collect::<Vec<_>>(),
        &[1],
    );

    let logger_querier = fuchsia_component::client::connect_to_protocol::<
        fidl_fuchsia_cobalt_test::LoggerQuerierMarker,
    >()
    .context("failed to connect to cobalt logger querier")?;

    async fn capture_log_events<T, F: FnOnce() -> Result<T, anyhow::Error>>(
        logger_querier: &fidl_fuchsia_cobalt_test::LoggerQuerierProxy,
        func: F,
    ) -> Result<(T, Vec<fidl_fuchsia_cobalt::CobaltEvent>), anyhow::Error> {
        let watch = logger_querier.watch_logs2(
            networking_metrics::PROJECT_ID,
            fidl_fuchsia_cobalt_test::LogMethod::LogCobaltEvents,
        );
        let res = func()?;
        let (events, more) = watch.await.context("failed to call watch_logs2")?;
        assert!(!more);
        Ok((res, events))
    }

    let (s1, events_post_bind) = capture_log_events(&logger_querier, move || {
        // Use port zero to allow the system to assign a port.
        std::net::TcpListener::bind("127.0.0.1:0").context("failed to bind to localhost")
    })
    .await?;

    let address = s1.local_addr().context("failed to get local address")?;

    let s1ref = &s1;
    let ((s2, s3), events_post_accept) = capture_log_events(&logger_querier, move || {
        let s2 = std::net::TcpStream::connect(address)?;
        let (s3, _sockaddr) = s1ref.accept()?;
        Ok((s2, s3))
    })
    .await?;

    let ((), events_post_first_drop) = capture_log_events(&logger_querier, move || {
        let () = std::mem::drop(s1);
        let () = std::mem::drop(s2);
        Ok(())
    })
    .await?;

    let ((), events_post_final_drop) = capture_log_events(&logger_querier, move || {
        let () = std::mem::drop(s3);
        Ok(())
    })
    .await?;

    matches::assert_matches!(
        events_with_id(&events_post_bind, networking_metrics::SOCKET_COUNT_MAX_METRIC_ID)
            .collect::<Vec<_>>()
            .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 1, period_duration_micros: _ }]
    );

    matches::assert_matches!(
        events_with_id(&events_post_bind, networking_metrics::SOCKETS_CREATED_METRIC_ID)
            .collect::<Vec<_>>()
            .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 1, period_duration_micros: _ }]
    );

    matches::assert_matches!(
        events_with_id(&events_post_bind, networking_metrics::SOCKETS_DESTROYED_METRIC_ID)
            .collect::<Vec<_>>()
            .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 0, period_duration_micros: _ }]
    );

    assert_eq!(
        events_with_id(&events_post_accept, networking_metrics::SOCKET_COUNT_MAX_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .max(),
        Some(3),
        "events: {:?}",
        MultilineSlice(&events_post_accept),
    );

    // The stack sees both the client and server side of the TCP connection.
    // Hence we see the TCP stats below accounting for both sides.
    matches::assert_matches!(
        {
            let mut events = events_with_id(
                &events_post_accept,
                networking_metrics::TCP_CONNECTIONS_ESTABLISHED_TOTAL_METRIC_ID,
            )
            .collect::<Vec<_>>();
            // Permit multiple identical events since this is a total metric.
            //
            // We sometimes observed multiple of these events (see https://fxbug.dev/56382).
            let () = events.dedup();
            events
        }
        .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 2, period_duration_micros: _ }]
    );

    const EXPECTED_PACKET_COUNT: i64 = 2;

    // TCP payload size (12) + TCP headers (20) + IP minimum size (20)
    const EXPECTED_PACKET_SIZE: i64 = 52;

    assert_eq!(
        events_with_id(&events_post_first_drop, networking_metrics::PACKETS_SENT_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .max(),
        Some(EXPECTED_PACKET_COUNT),
        "packets sent. events: {:?}",
        MultilineSlice(&events_post_first_drop),
    );
    assert_eq!(
        events_with_id(&events_post_first_drop, networking_metrics::PACKETS_RECEIVED_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .max(),
        Some(EXPECTED_PACKET_COUNT),
        "packets received. events: {:?}",
        MultilineSlice(&events_post_first_drop),
    );
    assert_eq!(
        events_with_id(&events_post_first_drop, networking_metrics::BYTES_SENT_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .max(),
        Some(EXPECTED_PACKET_COUNT * EXPECTED_PACKET_SIZE),
        "bytes sent. events: {:?}",
        MultilineSlice(&events_post_first_drop),
    );
    assert_eq!(
        events_with_id(&events_post_first_drop, networking_metrics::BYTES_RECEIVED_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .max(),
        Some(EXPECTED_PACKET_COUNT * EXPECTED_PACKET_SIZE),
        "bytes received. events: {:?}",
        MultilineSlice(&events_post_first_drop),
    );

    // In case of active-close, this metric is updated only on reaching
    // TIME_WAIT, after receiving FIN from the peer.
    assert_eq!(
        events_with_id(&events_post_first_drop, networking_metrics::SOCKETS_DESTROYED_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .sum::<i64>(),
        1,
        "sockets destroyed. events: {:?}",
        MultilineSlice(&events_post_first_drop),
    );

    assert_eq!(
        events_with_id(&events_post_final_drop, networking_metrics::SOCKET_COUNT_MAX_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .sum::<i64>(),
        0,
        "socket count max. events: {:?}",
        MultilineSlice(&events_post_final_drop),
    );

    assert_eq!(
        events_with_id(&events_post_final_drop, networking_metrics::SOCKETS_DESTROYED_METRIC_ID)
            .map(|fidl_fuchsia_cobalt::CountEvent { period_duration_micros: _, count }| *count)
            .sum::<i64>(),
        2,
        "sockets destroyed. events: {:?}",
        MultilineSlice(&events_post_final_drop)
    );

    // TODO(gvisor.dev/issue/1579) Check against the new counter that tracks
    // all connected TCP connections.
    matches::assert_matches!(
        events_with_id(
            &events_post_final_drop,
            networking_metrics::TCP_CONNECTIONS_ESTABLISHED_TOTAL_METRIC_ID,
        )
        .collect::<Vec<_>>()
        .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 0, period_duration_micros: _ }]
    );

    // TODO(gvisor.dev/issue/1400) There is currently no way the client can
    // avoid getting into time-wait on close. This means that there is no
    // reliable way to ensure that the connections are indeed closed at this
    // point. The TCP TIME-WAIT timeout, TCP FIN-WAIT2 timeout and the
    // cobalt-event polling interval are all 60sec, which can make the equality
    // assertions flaky resulting in following probable values for
    // tcp_connections_closed_events[0].count:
    // 0 : when fin-wait2 timeout fires at client because the test waited for
    //     60sec before closing the server socket. This causes the client
    //     connection to get closed (logged in events_post_first_drop).
    //     In this state, the FIN-ACK sent by the server results in a RST from
    //     the remote as the connection is purged at the client-side.
    //     This causes the server connection to get reset which is not accounted
    //     for by this counter. The above race, results in no new close events
    //     logged in events_post_final_drop and hence a value of 0.
    // 1 : when time-wait timeout fires after the cobalt-event poll interval
    //     accounting for only server connection close.
    // 2 : when time-wait timeout fires before the cobalt-event poll interval
    //     accounting for both server and client close.
    //
    // Once the fix for gvisor.dev/issue/1400 is in place, we can set the linger
    // timeout to zero, which would reset the connection instead of getting into
    // time-wait or fin-wait2. Then, the asserts below can be updated to account
    // for all closed connections with an equality check of 2.
    matches::assert_matches!(
        events_with_id(
            &events_post_final_drop,
            networking_metrics::TCP_CONNECTIONS_CLOSED_METRIC_ID,
        )
        .collect::<Vec<_>>()
        .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 0..=2, period_duration_micros: _ }]
    );

    // TODO(gvisor.dev/issue/1400) restore to equality check based on how the
    // reset of client and server connections are accounted for in gvisor.dev/issue/1400.
    matches::assert_matches!(
        events_with_id(
            &events_post_final_drop,
            networking_metrics::TCP_CONNECTIONS_RESET_METRIC_ID,
        )
        .collect::<Vec<_>>()
        .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 0..=1, period_duration_micros: _ }]
    );

    matches::assert_matches!(
        events_with_id(
            &events_post_final_drop,
            networking_metrics::TCP_CONNECTIONS_TIMED_OUT_METRIC_ID,
        )
        .collect::<Vec<_>>()
        .as_slice(),
        &[fidl_fuchsia_cobalt::CountEvent { count: 0, period_duration_micros: _ }]
    );

    Ok(())
}

struct MultilineSlice<'a, T>(&'a [T]);

impl<'a, T: std::fmt::Debug> std::fmt::Debug for MultilineSlice<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(slice) = self;
        let () = writeln!(f, "[")?;
        for item in slice.into_iter() {
            let () = writeln!(f, "{:?},", item)?;
        }
        let () = writeln!(f, "]")?;
        Ok(())
    }
}

// Returns the internal CountEvents of `events` that have the given `id`.
fn events_with_id<'a>(
    events: impl IntoIterator<Item = &'a fidl_fuchsia_cobalt::CobaltEvent>,
    id: u32,
) -> impl Iterator<Item = &'a fidl_fuchsia_cobalt::CountEvent> {
    events.into_iter().filter_map(
        move |fidl_fuchsia_cobalt::CobaltEvent { metric_id, payload, .. }| {
            if *metric_id == id {
                match payload {
                    fidl_fuchsia_cobalt::EventPayload::EventCount(count_event) => Some(count_event),
                    _ => None,
                }
            } else {
                None
            }
        },
    )
}
