// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::format_err;
use fidl::{endpoints::RequestStream, endpoints::ServerEnd};
use fidl_fuchsia_wlan_common as fidl_common;
use fidl_fuchsia_wlan_mlme::{self as fidl_mlme, MlmeEventStream, MlmeProxy, ScanResultCode};
use fidl_fuchsia_wlan_sme::{self as fidl_sme, ClientSmeRequest};
use fuchsia_async as fasync;
use fuchsia_zircon::{self as zx, DurationNum};
use futures::channel::mpsc;
use futures::{prelude::*, select, stream::FuturesUnordered};
use itertools::Itertools;
use log::{error, info};
use pin_utils::pin_mut;
use std::marker::Unpin;
use std::sync::{Arc, Mutex};
use void::Void;
use wlan_common::hasher::WlanHasher;
use wlan_inspect;
use wlan_sme::client::{BssDiscoveryResult, BssInfo, ConnectFailure, ConnectResult, InfoEvent};
use wlan_sme::{self as sme, client as client_sme, InfoStream};

use crate::inspect;
use crate::stats_scheduler::StatsRequest;
use crate::telemetry;
use fuchsia_cobalt::CobaltSender;

pub type Endpoint = ServerEnd<fidl_sme::ClientSmeMarker>;
type Sme = client_sme::ClientSme;

pub async fn serve<S>(
    cfg: sme::Config,
    proxy: MlmeProxy,
    device_info: fidl_mlme::DeviceInfo,
    event_stream: MlmeEventStream,
    new_fidl_clients: mpsc::UnboundedReceiver<Endpoint>,
    stats_requests: S,
    cobalt_sender: CobaltSender,
    cobalt_1dot1_proxy: fidl_fuchsia_metrics::MetricEventLoggerProxy,
    inspect_tree: Arc<inspect::WlanstackTree>,
    iface_tree_holder: Arc<wlan_inspect::iface_mgr::IfaceTreeHolder>,
    hasher: WlanHasher,
) -> Result<(), anyhow::Error>
where
    S: Stream<Item = StatsRequest> + Unpin,
{
    let wpa3_supported = device_info.driver_features.iter().any(|f| {
        f == &fidl_common::DriverFeature::SaeSmeAuth
            || f == &fidl_common::DriverFeature::SaeDriverAuth
    });
    let cfg = client_sme::ClientConfig::from_config(cfg, wpa3_supported);
    let is_softmac = device_info.driver_features.contains(&fidl_common::DriverFeature::TempSoftmac);
    let (sme, mlme_stream, info_stream, time_stream) =
        Sme::new(cfg, device_info, iface_tree_holder, hasher, is_softmac);
    let sme = Arc::new(Mutex::new(sme));
    let mlme_sme = super::serve_mlme_sme(
        proxy,
        event_stream,
        Arc::clone(&sme),
        mlme_stream,
        stats_requests,
        time_stream,
    );
    let sme_fidl = serve_fidl(sme, new_fidl_clients);
    let telemetry = handle_telemetry(info_stream, cobalt_sender, cobalt_1dot1_proxy, inspect_tree);
    pin_mut!(mlme_sme);
    pin_mut!(sme_fidl);
    pin_mut!(telemetry);
    Ok(select! {
        mlme_sme = mlme_sme.fuse() => mlme_sme?,
        sme_fidl = sme_fidl.fuse() => match sme_fidl? {},
        telemetry = telemetry.fuse() => match telemetry? {},
    })
}

async fn serve_fidl(
    sme: Arc<Mutex<Sme>>,
    new_fidl_clients: mpsc::UnboundedReceiver<Endpoint>,
) -> Result<Void, anyhow::Error> {
    let mut new_fidl_clients = new_fidl_clients.fuse();
    let mut fidl_clients = FuturesUnordered::new();
    loop {
        select! {
            new_fidl_client = new_fidl_clients.next() => match new_fidl_client {
                Some(c) => fidl_clients.push(serve_fidl_endpoint(&sme, c)),
                None => return Err(format_err!("New FIDL client stream unexpectedly ended")),
            },
            () = fidl_clients.select_next_some() => {},
        }
    }
}

async fn serve_fidl_endpoint(sme: &Mutex<Sme>, endpoint: Endpoint) {
    let stream = match endpoint.into_stream() {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create a stream from a zircon channel: {}", e);
            return;
        }
    };
    const MAX_CONCURRENT_REQUESTS: usize = 1000;
    let r = stream
        .try_for_each_concurrent(MAX_CONCURRENT_REQUESTS, move |request| {
            handle_fidl_request(sme, request)
        })
        .await;
    if let Err(e) = r {
        error!("Error serving FIDL: {}", e);
        return;
    }
}

async fn handle_fidl_request(
    sme: &Mutex<Sme>,
    request: fidl_sme::ClientSmeRequest,
) -> Result<(), fidl::Error> {
    match request {
        ClientSmeRequest::Scan { req, txn, .. } => Ok(scan(sme, txn, req)
            .await
            .unwrap_or_else(|e| error!("Error handling a scan transaction: {:?}", e))),
        ClientSmeRequest::Connect { req, txn, .. } => Ok(connect(sme, txn, req)
            .await
            .unwrap_or_else(|e| error!("Error handling a connect transaction: {:?}", e))),
        ClientSmeRequest::Disconnect { responder, reason } => {
            disconnect(sme, reason);
            responder.send()
        }
        ClientSmeRequest::Status { responder } => responder.send(&mut status(sme)),
        ClientSmeRequest::WmmStatus { responder } => wmm_status(sme, responder).await,
    }
}

async fn handle_telemetry(
    info_stream: InfoStream,
    mut cobalt_sender: CobaltSender,
    mut cobalt_1dot1_proxy: fidl_fuchsia_metrics::MetricEventLoggerProxy,
    inspect_tree: Arc<inspect::WlanstackTree>,
) -> Result<Void, anyhow::Error> {
    let mut info_stream = info_stream.fuse();
    let mut daily = fasync::Interval::new(24.hours());
    let mut disconnect_tracker = telemetry::DisconnectTracker::new();

    loop {
        select! {
            info_event = info_stream.next() => match info_event {
                Some(e) => handle_info_event(e, &mut cobalt_sender, &mut cobalt_1dot1_proxy, inspect_tree.clone(), &mut disconnect_tracker).await,
                None => return Err(format_err!("Info Event stream unexpectedly ended")),
            },
            _daily = daily.next() => {
                let disconnects_today = disconnect_tracker.disconnects_today();
                // Only log this metric for population that have low number of disconnects per day
                if disconnects_today.len() > 0 && disconnects_today.len() <= 2 {
                    for info in disconnects_today {
                        telemetry::log_disconnect_reason_avg_population(&mut cobalt_1dot1_proxy, &info).await;
                    }
                }
            },
        }
    }
}

async fn scan(
    sme: &Mutex<Sme>,
    txn: ServerEnd<fidl_sme::ScanTransactionMarker>,
    scan_request: fidl_sme::ScanRequest,
) -> Result<(), anyhow::Error> {
    let handle = txn.into_stream()?.control_handle();
    let receiver = sme.lock().unwrap().on_scan_command(scan_request);
    let result = receiver.await.unwrap_or(Err(fidl_mlme::ScanResultCode::InternalError));
    let send_result = send_scan_results(handle, result);
    filter_out_peer_closed(send_result)?;
    Ok(())
}

async fn connect(
    sme: &Mutex<Sme>,
    txn: Option<ServerEnd<fidl_sme::ConnectTransactionMarker>>,
    req: fidl_sme::ConnectRequest,
) -> Result<(), anyhow::Error> {
    let handle = match txn {
        None => None,
        Some(txn) => Some(txn.into_stream()?.control_handle()),
    };
    let receiver = sme.lock().unwrap().on_connect_command(req);
    let result = receiver.await.ok();
    let send_result = send_connect_result(handle, result);
    filter_out_peer_closed(send_result)?;
    Ok(())
}

pub fn filter_out_peer_closed(r: Result<(), fidl::Error>) -> Result<(), fidl::Error> {
    match r {
        Err(ref e) if e.is_closed() => Ok(()),
        other => other,
    }
}

fn disconnect(sme: &Mutex<Sme>, policy_disconnect_reason: fidl_sme::UserDisconnectReason) {
    sme.lock().unwrap().on_disconnect_command(policy_disconnect_reason);
}

fn status(sme: &Mutex<Sme>) -> fidl_sme::ClientStatusResponse {
    let status = sme.lock().unwrap().status();
    fidl_sme::ClientStatusResponse {
        connected_to: status.connected_to.map(|bss| Box::new(convert_bss_info(bss))),
        connecting_to_ssid: status.connecting_to.unwrap_or(Vec::new()),
    }
}

async fn wmm_status(
    sme: &Mutex<Sme>,
    responder: fidl_sme::ClientSmeWmmStatusResponder,
) -> Result<(), fidl::Error> {
    let receiver = sme.lock().unwrap().wmm_status();
    let mut wmm_status = match receiver.await {
        Ok(result) => result,
        Err(_) => Err(zx::sys::ZX_ERR_CANCELED),
    };
    responder.send(&mut wmm_status)
}

async fn handle_info_event(
    e: InfoEvent,
    cobalt_sender: &mut CobaltSender,
    cobalt_1dot1_proxy: &mut fidl_fuchsia_metrics::MetricEventLoggerProxy,
    inspect_tree: Arc<inspect::WlanstackTree>,
    disconnect_tracker: &mut telemetry::DisconnectTracker,
) {
    match e {
        InfoEvent::DiscoveryScanStats(scan_stats) => {
            let is_join_scan = false;
            telemetry::log_scan_stats(cobalt_sender, inspect_tree, &scan_stats, is_join_scan);
        }
        InfoEvent::ConnectStats(connect_stats) => {
            telemetry::log_connect_stats(
                cobalt_sender,
                cobalt_1dot1_proxy,
                inspect_tree,
                &connect_stats,
            )
            .await
        }
        InfoEvent::ConnectionPing(info) => telemetry::log_connection_ping(cobalt_sender, &info),
        InfoEvent::DisconnectInfo(info) => {
            disconnect_tracker.add_event(info.clone());
            telemetry::log_disconnect(cobalt_sender, cobalt_1dot1_proxy, inspect_tree, &info).await
        }
    }
}

fn send_scan_results(
    handle: fidl_sme::ScanTransactionControlHandle,
    result: BssDiscoveryResult,
) -> Result<(), fidl::Error> {
    // Maximum number of scan results to send at a time so we don't exceed FIDL msg size limit.
    // A scan result may contain all IEs, which is at most 2304 bytes since that's the maximum
    // frame size. Let's be conservative and assume each scan result is 3k bytes.
    // At 15, maximum size is 45k bytes, which is well under the 64k bytes limit.
    const MAX_ON_SCAN_RESULT: usize = 15;
    match result {
        Ok(bss_list) => {
            info!("Sending scan results for {} APs", bss_list.len());
            for chunk in &bss_list.into_iter().chunks(MAX_ON_SCAN_RESULT) {
                let mut fidl_list = chunk.into_iter().map(convert_bss_info).collect::<Vec<_>>();
                handle.send_on_result(&mut fidl_list.iter_mut())?;
            }
            handle.send_on_finished()?;
        }
        Err(e) => {
            let mut fidl_err = match e {
                fidl_mlme::ScanResultCode::NotSupported => fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::NotSupported,
                    message: "Scanning not supported by device".to_string(),
                },
                fidl_mlme::ScanResultCode::ShouldWait => fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::ShouldWait,
                    message: "Scanning is temporarily unavailable".to_string(),
                },
                fidl_mlme::ScanResultCode::CanceledByDriverOrFirmware => fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::CanceledByDriverOrFirmware,
                    message: "Scanning was canceled by driver or FW".to_string(),
                },
                _ => fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::InternalError,
                    message: "Internal error occurred".to_string(),
                },
            };
            handle.send_on_error(&mut fidl_err)?;
        }
    }
    Ok(())
}

fn convert_bss_info(bss: BssInfo) -> fidl_sme::BssInfo {
    fidl_sme::BssInfo {
        bssid: bss.bssid,
        ssid: bss.ssid,
        rssi_dbm: bss.rssi_dbm,
        snr_db: bss.snr_db,
        channel: bss.channel.to_fidl(),
        protection: bss.protection.into(),
        compatible: bss.compatible,
        bss_desc: bss.bss_desc.map(Box::new),
    }
}

fn convert_connect_result(result: &ConnectResult) -> fidl_sme::ConnectResultCode {
    match result {
        ConnectResult::Success => fidl_sme::ConnectResultCode::Success,
        ConnectResult::Canceled => fidl_sme::ConnectResultCode::Canceled,
        ConnectResult::Failed(ConnectFailure::ScanFailure(ScanResultCode::ShouldWait)) => {
            fidl_sme::ConnectResultCode::Canceled
        }
        ConnectResult::Failed(failure) if failure.likely_due_to_credential_rejected() => {
            fidl_sme::ConnectResultCode::CredentialRejected
        }
        ConnectResult::Failed(..) => fidl_sme::ConnectResultCode::Failed,
    }
}

fn send_connect_result(
    handle: Option<fidl_sme::ConnectTransactionControlHandle>,
    result: Option<ConnectResult>,
) -> Result<(), fidl::Error> {
    if let Some(handle) = handle {
        let code = match result {
            Some(connect_result) => {
                if let ConnectResult::Failed(_) = connect_result {
                    error!("Connection failed: {:?}", connect_result);
                }
                convert_connect_result(&connect_result)
            }
            None => {
                error!("Connection failed. No result from SME.");
                fidl_sme::ConnectResultCode::Failed
            }
        };
        handle.send_on_finished(code)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::test_helper::fake_inspect_tree,
        fidl::endpoints::{create_proxy, create_proxy_and_stream},
        fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211,
        fidl_fuchsia_wlan_mlme::ScanResultCode,
        fidl_fuchsia_wlan_sme::{self as fidl_sme},
        fuchsia_async as fasync, fuchsia_zircon as zx,
        futures::task::Poll,
        pin_utils::pin_mut,
        rand::{prelude::ThreadRng, Rng},
        std::convert::TryInto,
        telemetry::test_helper::{fake_cobalt_sender, fake_disconnect_info, CobaltExt},
        test_case::test_case,
        wlan_common::{assert_variant, bss::Protection, channel::Channel},
        wlan_rsn::auth,
        wlan_sme::client::{
            ConnectFailure, ConnectResult, EstablishRsnaFailure, EstablishRsnaFailureReason,
        },
    };

    #[test_case(
        fidl_mlme::ScanResultCode::ShouldWait,
        fidl_sme::ScanErrorCode::ShouldWait,
        "Scanning is temporarily unavailable"
    )]
    #[test_case(
        fidl_mlme::ScanResultCode::NotSupported,
        fidl_sme::ScanErrorCode::NotSupported,
        "Scanning not supported by device"
    )]
    #[test_case(
        fidl_mlme::ScanResultCode::CanceledByDriverOrFirmware,
        fidl_sme::ScanErrorCode::CanceledByDriverOrFirmware,
        "Scanning was canceled by driver or FW"
    )]
    #[test_case(
        fidl_mlme::ScanResultCode::InvalidArgs,
        fidl_sme::ScanErrorCode::InternalError,
        "Internal error occurred"
    )]
    #[test_case(
        fidl_mlme::ScanResultCode::InternalError,
        fidl_sme::ScanErrorCode::InternalError,
        "Internal error occurred"
    )]
    fn test_send_scan_error(
        scan_code: fidl_mlme::ScanResultCode,
        scan_error: fidl_sme::ScanErrorCode,
        err_msg: &str,
    ) {
        let mut exec = fuchsia_async::TestExecutor::new().expect("Failed to create executor");
        let (proxy, server) =
            create_proxy::<fidl_sme::ScanTransactionMarker>().expect("failed to create scan proxy");
        let handle = server.into_stream().expect("Failed to create stream").control_handle();
        let mut stream = proxy.take_event_stream();

        send_scan_results(handle, Err(scan_code)).expect("Failed to send scan");
        assert_variant!(exec.run_until_stalled(&mut stream.next()), Poll::Ready(Some(Ok(scan_event))) => {
            let scan_error = fidl_sme::ScanError{
                code: scan_error,
                message: err_msg.to_string(),
            };
            assert_variant!(scan_event, fidl_sme::ScanTransactionEvent::OnError{ error } => {
                assert_eq!(error, scan_error);
            });
        });
    }

    #[test]
    fn test_convert_connect_result() {
        assert_eq!(
            convert_connect_result(&ConnectResult::Success),
            fidl_sme::ConnectResultCode::Success
        );
        assert_eq!(
            convert_connect_result(&ConnectResult::Canceled),
            fidl_sme::ConnectResultCode::Canceled
        );
        assert_eq!(
            convert_connect_result(&ConnectResult::Failed(ConnectFailure::ScanFailure(
                ScanResultCode::ShouldWait
            ))),
            fidl_sme::ConnectResultCode::Canceled
        );

        let connect_result =
            ConnectResult::Failed(ConnectFailure::EstablishRsnaFailure(EstablishRsnaFailure {
                auth_method: Some(auth::MethodName::Psk),
                reason: EstablishRsnaFailureReason::KeyFrameExchangeTimeout,
            }));
        assert_eq!(
            convert_connect_result(&connect_result),
            fidl_sme::ConnectResultCode::CredentialRejected
        );

        assert_eq!(
            convert_connect_result(&ConnectResult::Failed(ConnectFailure::ScanFailure(
                ScanResultCode::InternalError
            ))),
            fidl_sme::ConnectResultCode::Failed
        );
    }

    // Verify that we don't exceed FIDL maximum message limit when sending scan results
    #[test]
    fn test_large_on_scan_result() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let (proxy, txn) = create_proxy::<fidl_sme::ScanTransactionMarker>()
            .expect("failed to create ScanTransaction proxy");
        let handle = txn.into_stream().expect("expect into_stream to succeed").control_handle();

        let mut rng = rand::thread_rng();
        let scan_results = (0..1000).map(|_| random_bss_info(&mut rng)).collect::<Vec<_>>();
        // If we exceed size limit, it should already fail here
        send_scan_results(handle, Ok(scan_results.clone()))
            .expect("expect send_scan_results to succeed");

        // Sanity check that we receive all scan results
        let results_fut = collect_scan(&proxy);
        pin_mut!(results_fut);
        assert_variant!(exec.run_until_stalled(&mut results_fut), Poll::Ready(results) => {
            let sent_scan_results = scan_results.into_iter().map(|bss| bss.bss_desc.unwrap()).collect::<Vec<_>>();
            let received_scan_results = results.into_iter().map(|bss| *bss.bss_desc.unwrap()).collect::<Vec<_>>();
            assert_eq!(sent_scan_results, received_scan_results);
        })
    }

    #[test]
    fn test_logging_disconnects_today_avg_population() {
        let mut exec =
            fasync::TestExecutor::new_with_fake_time().expect("failed to create an executor");
        exec.set_fake_time(fasync::Time::from_nanos(0));

        let (info_sink, info_stream) = mpsc::unbounded();
        let (cobalt_sender, _cobalt_receiver) = fake_cobalt_sender();
        let (cobalt_1dot1_proxy, mut cobalt_1dot1_stream) =
            create_proxy_and_stream::<fidl_fuchsia_metrics::MetricEventLoggerMarker>()
                .expect("failed to create Cobalt 1.1 proxy and stream");
        let (inspect_tree, _persistence_stream) = fake_inspect_tree();

        let telemetry_fut =
            handle_telemetry(info_stream, cobalt_sender, cobalt_1dot1_proxy, inspect_tree);
        pin_mut!(telemetry_fut);
        assert_variant!(exec.run_until_stalled(&mut telemetry_fut), Poll::Pending);

        let disconnect_info = InfoEvent::DisconnectInfo(fake_disconnect_info([1u8; 6]));
        info_sink.unbounded_send(disconnect_info).expect("expect send info to succeed");

        assert_variant!(exec.run_until_stalled(&mut telemetry_fut), Poll::Pending);
        while let Poll::Ready(Some(Ok(req))) =
            exec.run_until_stalled(&mut cobalt_1dot1_stream.next())
        {
            // Telemetry should only log disconnect reason for average population on daily timer,
            // so at this point, verify it's not logged yet.
            assert_ne!(
                req.metric_id(),
                wlan_metrics_registry::DISCONNECT_REASON_AVERAGE_POPULATION_METRIC_ID
            );
            req.respond(fidl_fuchsia_metrics::Status::Ok);
            assert_variant!(exec.run_until_stalled(&mut telemetry_fut), Poll::Pending);
        }

        // Set time to 24 hours later. Verify that the metric is now logged.
        exec.set_fake_time(fasync::Time::after(24.hours()));
        exec.wake_expired_timers();
        assert_variant!(exec.run_until_stalled(&mut telemetry_fut), Poll::Pending);
        assert_variant!(exec.run_until_stalled(&mut cobalt_1dot1_stream.next()), Poll::Ready(Some(Ok(req))) => {
            assert_eq!(req.metric_id(), wlan_metrics_registry::DISCONNECT_REASON_AVERAGE_POPULATION_METRIC_ID);
            req.respond(fidl_fuchsia_metrics::Status::Ok);
        });
    }

    async fn collect_scan(proxy: &fidl_sme::ScanTransactionProxy) -> Vec<fidl_sme::BssInfo> {
        let mut stream = proxy.take_event_stream();
        let mut results = vec![];
        while let Some(Ok(event)) = stream.next().await {
            match event {
                fidl_sme::ScanTransactionEvent::OnResult { aps } => {
                    results.extend(aps);
                }
                fidl_sme::ScanTransactionEvent::OnFinished {} => {
                    return results;
                }
                fidl_sme::ScanTransactionEvent::OnError { error } => {
                    panic!("Did not expect scan error: {:?}", error);
                }
            }
        }
        panic!("Did not receive fidl_sme::ScanTransactionEvent::OnFinished");
    }

    // Create roughly over 2k bytes BssInfo
    fn random_bss_info(rng: &mut ThreadRng) -> BssInfo {
        let mut ies = vec![];
        // SSID
        let ssid =
            (0..fidl_ieee80211::MAX_SSID_BYTE_LEN).map(|_| rng.gen::<u8>()).collect::<Vec<_>>();
        ies.extend_from_slice(&[0, 32]);
        ies.extend_from_slice(&ssid[..]);
        // Supported rates
        ies.extend_from_slice(&[1, 5]);
        ies.extend_from_slice(&rng.gen::<[u8; 5]>()[..]);
        // Eight giant vendor IEs
        for _j in 0..8 {
            ies.extend_from_slice(&[221, 250]);
            ies.extend((0..250).map(|_| rng.gen::<u8>()))
        }
        let bss_desc = fidl_fuchsia_wlan_internal::BssDescription {
            bssid: (0..6).map(|_| rng.gen::<u8>()).collect::<Vec<u8>>().try_into().unwrap(),
            bss_type: fidl_fuchsia_wlan_internal::BssTypes::Infrastructure,
            beacon_period: rng.gen::<u16>(),
            timestamp: rng.gen::<u64>(),
            local_time: rng.gen::<u64>(),
            cap: rng.gen::<u16>(),
            ies,
            rssi_dbm: rng.gen::<i8>(),
            chan: fidl_common::WlanChan {
                primary: rng.gen_range(1, 255),
                cbw: fidl_common::Cbw::Cbw20,
                secondary80: 0,
            },
            snr_db: rng.gen::<i8>(),
        };
        let bss_info = BssInfo {
            bssid: bss_desc.bssid.clone(),
            ssid,
            rssi_dbm: bss_desc.rssi_dbm,
            snr_db: bss_desc.snr_db,
            signal_report_time: zx::Time::ZERO,
            channel: Channel::from_fidl(bss_desc.chan),
            protection: Protection::Open,
            compatible: rng.gen::<bool>(),
            ht_cap: None,
            vht_cap: None,
            probe_resp_wsc: None,
            wmm_param: None,
            bss_desc: Some(bss_desc),
        };
        bss_info
    }
}
