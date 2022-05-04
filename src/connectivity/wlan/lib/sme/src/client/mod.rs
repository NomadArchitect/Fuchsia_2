// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod event;
mod inspect;
mod protection;
mod rsn;
mod scan;
mod state;

mod wpa;

#[cfg(test)]
pub mod test_utils;

use {
    self::{
        event::Event,
        protection::{Protection, SecurityContext},
        scan::{DiscoveryScan, ScanScheduler},
        state::{ClientState, ConnectCommand},
    },
    crate::{responder::Responder, Config, MlmeRequest, MlmeSink, MlmeStream},
    fidl_fuchsia_wlan_common as fidl_common,
    fidl_fuchsia_wlan_common_security::Authentication,
    fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211, fidl_fuchsia_wlan_internal as fidl_internal,
    fidl_fuchsia_wlan_mlme as fidl_mlme, fidl_fuchsia_wlan_sme as fidl_sme,
    fuchsia_inspect_contrib::auto_persist::{self, AutoPersist},
    fuchsia_zircon as zx,
    futures::channel::{mpsc, oneshot},
    ieee80211::{Bssid, Ssid},
    log::{error, info, warn},
    std::{
        convert::{TryFrom, TryInto},
        sync::Arc,
    },
    wlan_common::{
        self,
        bss::{BssDescription, Protection as BssProtection},
        capabilities::derive_join_capabilities,
        channel::Channel,
        hasher::WlanHasher,
        ie::{self, rsn::rsne, wsc},
        scan::ScanResult,
        security::SecurityAuthenticator,
        sink::UnboundedSink,
        timer::{self, TimedEvent},
    },
    wlan_rsn::auth,
};

// This is necessary to trick the private-in-public checker.
// A private module is not allowed to include private types in its interface,
// even though the module itself is private and will never be exported.
// As a workaround, we add another private module with public types.
mod internal {
    use {
        crate::{
            client::{event::Event, inspect, ConnectionAttemptId},
            MlmeSink,
        },
        fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_mlme as fidl_mlme,
        std::sync::Arc,
        wlan_common::timer::Timer,
    };

    pub struct Context {
        pub device_info: Arc<fidl_mlme::DeviceInfo>,
        pub mlme_sink: MlmeSink,
        pub(crate) timer: Timer<Event>,
        pub att_id: ConnectionAttemptId,
        pub(crate) inspect: Arc<inspect::SmeTree>,
        pub mac_sublayer_support: fidl_common::MacSublayerSupport,
        pub security_support: fidl_common::SecuritySupport,
    }
}

use self::internal::*;

pub type TimeStream = timer::TimeStream<Event>;

// An automatically increasing sequence number that uniquely identifies a logical
// connection attempt. For example, a new connection attempt can be triggered
// by a DisassociateInd message from the MLME.
pub type ConnectionAttemptId = u64;

pub type ScanTxnId = u64;

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    cfg: Config,
    pub wpa3_supported: bool,
}

impl ClientConfig {
    pub fn from_config(cfg: Config, wpa3_supported: bool) -> Self {
        Self { cfg, wpa3_supported }
    }

    /// Converts a given BssDescription into a ScanResult.
    pub fn create_scan_result(
        &self,
        timestamp: zx::Time,
        bss_description: BssDescription,
        device_info: &fidl_mlme::DeviceInfo,
        security_support: &fidl_common::SecuritySupport,
    ) -> ScanResult {
        ScanResult {
            compatible: self.is_bss_compatible(&bss_description, device_info, security_support),
            timestamp,
            bss_description,
        }
    }

    /// Determines whether a given BSS is compatible with this client SME configuration.
    pub fn is_bss_compatible(
        &self,
        bss: &BssDescription,
        device_info: &fidl_mlme::DeviceInfo,
        security_support: &fidl_common::SecuritySupport,
    ) -> bool {
        self.is_bss_protection_compatible(bss, security_support)
            && self.are_bss_channel_and_data_rates_compatible(bss, device_info)
    }

    fn is_bss_protection_compatible(
        &self,
        bss: &BssDescription,
        security_support: &fidl_fuchsia_wlan_common::SecuritySupport,
    ) -> bool {
        let privacy = wlan_common::mac::CapabilityInfo(bss.capability_info).privacy();
        let protection = bss.protection();
        match &protection {
            BssProtection::Open => true,
            BssProtection::Wep => self.cfg.wep_supported,
            BssProtection::Wpa1 => self.cfg.wpa1_supported,
            BssProtection::Wpa2Wpa3Personal | BssProtection::Wpa3Personal
                if self.wpa3_supported =>
            {
                match bss.rsne() {
                    Some(rsne) if privacy => match rsne::from_bytes(rsne) {
                        Ok((_, a_rsne)) => a_rsne.is_wpa3_rsn_compatible(security_support),
                        _ => false,
                    },
                    _ => false,
                }
            }
            BssProtection::Wpa1Wpa2PersonalTkipOnly
            | BssProtection::Wpa2PersonalTkipOnly
            | BssProtection::Wpa1Wpa2Personal
            | BssProtection::Wpa2Personal
            | BssProtection::Wpa2Wpa3Personal => match bss.rsne() {
                Some(rsne) if privacy => match rsne::from_bytes(rsne) {
                    Ok((_, a_rsne)) => a_rsne.is_wpa2_rsn_compatible(security_support),
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        }
    }

    fn are_bss_channel_and_data_rates_compatible(
        &self,
        bss: &BssDescription,
        device_info: &fidl_mlme::DeviceInfo,
    ) -> bool {
        derive_join_capabilities(Channel::from(bss.channel), bss.rates(), device_info).is_ok()
    }
}

pub struct ClientSme {
    cfg: ClientConfig,
    state: Option<ClientState>,
    scan_sched: ScanScheduler<Responder<Result<Vec<ScanResult>, fidl_mlme::ScanResultCode>>>,
    wmm_status_responders: Vec<Responder<fidl_sme::ClientSmeWmmStatusResult>>,
    auto_persist_last_pulse: AutoPersist<()>,
    context: Context,
}

#[derive(Debug, PartialEq)]
pub enum ConnectResult {
    Success,
    Canceled,
    Failed(ConnectFailure),
}

impl<T: Into<ConnectFailure>> From<T> for ConnectResult {
    fn from(failure: T) -> Self {
        ConnectResult::Failed(failure.into())
    }
}

#[derive(Debug)]
pub struct ConnectTransactionSink {
    sink: UnboundedSink<ConnectTransactionEvent>,
    is_reconnecting: bool,
}

impl ConnectTransactionSink {
    pub fn new_unbounded() -> (Self, ConnectTransactionStream) {
        let (sender, receiver) = mpsc::unbounded();
        let sink =
            ConnectTransactionSink { sink: UnboundedSink::new(sender), is_reconnecting: false };
        (sink, receiver)
    }

    pub fn is_reconnecting(&self) -> bool {
        self.is_reconnecting
    }

    pub fn send_connect_result(&mut self, result: ConnectResult) {
        let event =
            ConnectTransactionEvent::OnConnectResult { result, is_reconnect: self.is_reconnecting };
        self.send(event);
    }

    pub fn send(&mut self, event: ConnectTransactionEvent) {
        if let ConnectTransactionEvent::OnDisconnect { info } = &event {
            self.is_reconnecting = info.is_sme_reconnecting;
        };
        self.sink.send(event);
    }
}

pub type ConnectTransactionStream = mpsc::UnboundedReceiver<ConnectTransactionEvent>;

#[derive(Debug, PartialEq)]
pub enum ConnectTransactionEvent {
    OnConnectResult { result: ConnectResult, is_reconnect: bool },
    OnDisconnect { info: fidl_sme::DisconnectInfo },
    OnSignalReport { ind: fidl_internal::SignalReportIndication },
    OnChannelSwitched { info: fidl_internal::ChannelSwitchInfo },
}

#[derive(Debug, PartialEq)]
pub enum ConnectFailure {
    SelectNetworkFailure(SelectNetworkFailure),
    // TODO(fxbug.dev/68531): SME no longer performs scans when connecting. Remove the
    //                        `ScanFailure` variant.
    ScanFailure(fidl_mlme::ScanResultCode),
    // TODO(fxbug.dev/96668): `JoinFailure` and `AuthenticationFailure` no longer needed when
    //                        state machine is fully transitioned to USME.
    JoinFailure(fidl_ieee80211::StatusCode),
    AuthenticationFailure(fidl_ieee80211::StatusCode),
    AssociationFailure(AssociationFailure),
    EstablishRsnaFailure(EstablishRsnaFailure),
}

impl ConnectFailure {
    // TODO(fxbug.dev/82654): ConnectFailure::is_timeout is not useful, remove it
    pub fn is_timeout(&self) -> bool {
        // Note: For association, we don't have a failure type for timeout, so cannot deduce
        //       whether an association failure is due to timeout.
        match self {
            ConnectFailure::AuthenticationFailure(failure) => match failure {
                fidl_ieee80211::StatusCode::RejectedSequenceTimeout => true,
                _ => false,
            },
            ConnectFailure::EstablishRsnaFailure(failure) => match failure {
                EstablishRsnaFailure {
                    reason: EstablishRsnaFailureReason::KeyFrameExchangeTimeout,
                    ..
                }
                | EstablishRsnaFailure {
                    reason: EstablishRsnaFailureReason::OverallTimeout(_),
                    ..
                } => true,
                _ => false,
            },
            _ => false,
        }
    }

    /// Returns true if failure was likely caused by rejected
    /// credentials. In some cases, we cannot be 100% certain that
    /// credentials were rejected, but it's worth noting when we
    /// observe a failure event that was more than likely caused by
    /// rejected credentials.
    pub fn likely_due_to_credential_rejected(&self) -> bool {
        match self {
            // Assuming the correct type of credentials are given, a
            // bad password will cause a variety of errors depending
            // on the security type. All of the following cases assume
            // no frames were dropped unintentionally. For example,
            // it's possible to conflate a WPA2 bad password error
            // with a dropped frame at just the right moment since the
            // error itself is *caused by* a dropped frame.

            // For WPA1 and WPA2, the error will be
            // EstablishRsnaFailure::KeyFrameExchangeTimeout.  When
            // the authenticator receives a bad MIC (derived from the
            // password), it will silently drop the EAPOL handshake
            // frame it received.
            //
            // NOTE: The alternative possibilities for seeing an
            // EstablishRsnaFailure::KeyFrameExchangeTimeout are an
            // error in our crypto parameter parsing and crypto
            // implementation, or a lost connection with the AP.
            ConnectFailure::EstablishRsnaFailure(EstablishRsnaFailure {
                auth_method: Some(auth::MethodName::Psk),
                reason:
                    EstablishRsnaFailureReason::OverallTimeout(wlan_rsn::Error::LikelyWrongCredential),
            }) => true,

            // For WEP, the entire association is always handled by
            // fullmac, so the best we can do is use
            // fidl_mlme::AssociateResultCode. The code that arises
            // when WEP fails with rejected credentials is
            // RefusedReasonUnspecified. This is a catch-all error for
            // a WEP authentication failure, but it is being
            // considered good enough for catching rejected
            // credentials for a deprecated WEP association.
            ConnectFailure::AssociationFailure(AssociationFailure {
                bss_protection: BssProtection::Wep,
                code: fidl_ieee80211::StatusCode::RefusedUnauthenticatedAccessNotSupported,
            }) => true,
            _ => false,
        }
    }

    pub fn status_code(&self) -> fidl_ieee80211::StatusCode {
        match self {
            ConnectFailure::JoinFailure(code)
            | ConnectFailure::AuthenticationFailure(code)
            | ConnectFailure::AssociationFailure(AssociationFailure { code, .. }) => *code,
            ConnectFailure::EstablishRsnaFailure(..) => {
                fidl_ieee80211::StatusCode::EstablishRsnaFailure
            }
            // SME no longer does join scan, so these two failures should no longer happen
            ConnectFailure::ScanFailure(fidl_mlme::ScanResultCode::ShouldWait) => {
                fidl_ieee80211::StatusCode::Canceled
            }
            ConnectFailure::SelectNetworkFailure(..) | ConnectFailure::ScanFailure(..) => {
                fidl_ieee80211::StatusCode::RefusedReasonUnspecified
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum SelectNetworkFailure {
    NoScanResultWithSsid,
    IncompatibleConnectRequest,
    InternalProtectionError,
}

impl From<SelectNetworkFailure> for ConnectFailure {
    fn from(failure: SelectNetworkFailure) -> Self {
        ConnectFailure::SelectNetworkFailure(failure)
    }
}

#[derive(Debug, PartialEq)]
pub struct AssociationFailure {
    pub bss_protection: BssProtection,
    pub code: fidl_ieee80211::StatusCode,
}

impl From<AssociationFailure> for ConnectFailure {
    fn from(failure: AssociationFailure) -> Self {
        ConnectFailure::AssociationFailure(failure)
    }
}

#[derive(Debug, PartialEq)]
pub struct EstablishRsnaFailure {
    pub auth_method: Option<auth::MethodName>,
    pub reason: EstablishRsnaFailureReason,
}

#[derive(Debug, PartialEq)]
pub enum EstablishRsnaFailureReason {
    StartSupplicantFailed,
    KeyFrameExchangeTimeout,
    OverallTimeout(wlan_rsn::Error),
    InternalError,
}

impl From<EstablishRsnaFailure> for ConnectFailure {
    fn from(failure: EstablishRsnaFailure) -> Self {
        ConnectFailure::EstablishRsnaFailure(failure)
    }
}

// Almost mirrors fidl_sme::ServingApInfo except that ServingApInfo
// contains more info here than it does in fidl_sme.
#[derive(Clone, Debug, PartialEq)]
pub struct ServingApInfo {
    pub bssid: Bssid,
    pub ssid: Ssid,
    pub rssi_dbm: i8,
    pub snr_db: i8,
    pub signal_report_time: zx::Time,
    pub channel: wlan_common::channel::Channel,
    pub protection: BssProtection,
    pub ht_cap: Option<fidl_internal::HtCapabilities>,
    pub vht_cap: Option<fidl_internal::VhtCapabilities>,
    pub probe_resp_wsc: Option<wsc::ProbeRespWsc>,
    pub wmm_param: Option<ie::WmmParam>,
}

impl From<ServingApInfo> for fidl_sme::ServingApInfo {
    fn from(ap: ServingApInfo) -> fidl_sme::ServingApInfo {
        fidl_sme::ServingApInfo {
            bssid: ap.bssid.0,
            ssid: ap.ssid.to_vec(),
            rssi_dbm: ap.rssi_dbm,
            snr_db: ap.snr_db,
            channel: ap.channel.into(),
            protection: ap.protection.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientSmeStatus {
    Connected(ServingApInfo),
    Connecting(Ssid),
    Idle,
}

impl ClientSmeStatus {
    pub fn is_connected(&self) -> bool {
        matches!(self, ClientSmeStatus::Connected(_))
    }
}

impl From<ClientSmeStatus> for fidl_sme::ClientStatusResponse {
    fn from(client_sme_status: ClientSmeStatus) -> fidl_sme::ClientStatusResponse {
        match client_sme_status {
            ClientSmeStatus::Connected(serving_ap_info) => {
                fidl_sme::ClientStatusResponse::Connected(serving_ap_info.into())
            }
            ClientSmeStatus::Connecting(ssid) => {
                fidl_sme::ClientStatusResponse::Connecting(ssid.to_vec())
            }
            ClientSmeStatus::Idle => fidl_sme::ClientStatusResponse::Idle(fidl_sme::Empty {}),
        }
    }
}

impl ClientSme {
    pub fn new(
        cfg: ClientConfig,
        info: fidl_mlme::DeviceInfo,
        iface_tree_holder: Arc<wlan_inspect::iface_mgr::IfaceTreeHolder>,
        hasher: WlanHasher,
        persistence_req_sender: auto_persist::PersistenceReqSender,
        mac_sublayer_support: fidl_common::MacSublayerSupport,
        security_support: fidl_common::SecuritySupport,
        spectrum_management_support: fidl_common::SpectrumManagementSupport,
    ) -> (Self, MlmeStream, TimeStream) {
        let device_info = Arc::new(info);
        let (mlme_sink, mlme_stream) = mpsc::unbounded();
        let (mut timer, time_stream) = timer::create_timer();
        let inspect = Arc::new(inspect::SmeTree::new(&iface_tree_holder.node, hasher));
        iface_tree_holder.add_iface_subtree(inspect.clone());
        timer.schedule(event::InspectPulseCheck);
        timer.schedule(event::InspectPulsePersist);
        let mut auto_persist_last_pulse =
            AutoPersist::new((), "wlanstack-last-pulse", persistence_req_sender);
        {
            // Request auto-persistence of pulse once on startup
            let _guard = auto_persist_last_pulse.get_mut();
        }

        (
            ClientSme {
                cfg,
                state: Some(ClientState::new(cfg)),
                scan_sched: <ScanScheduler<
                    Responder<Result<Vec<ScanResult>, fidl_mlme::ScanResultCode>>,
                >>::new(
                    Arc::clone(&device_info), spectrum_management_support
                ),
                wmm_status_responders: vec![],
                auto_persist_last_pulse,
                context: Context {
                    mlme_sink: MlmeSink::new(mlme_sink),
                    device_info,
                    timer,
                    att_id: 0,
                    inspect,
                    mac_sublayer_support,
                    security_support,
                },
            },
            mlme_stream,
            time_stream,
        )
    }

    pub fn on_connect_command(
        &mut self,
        req: fidl_sme::ConnectRequest,
    ) -> ConnectTransactionStream {
        let (mut connect_txn_sink, connect_txn_stream) = ConnectTransactionSink::new_unbounded();

        // Cancel any ongoing connect attempt
        self.state = self.state.take().map(|state| state.cancel_ongoing_connect(&mut self.context));

        let bss_description: BssDescription = match req.bss_description.try_into() {
            Ok(bss_description) => bss_description,
            Err(e) => {
                error!("Failed converting FIDL BssDescription in ConnectRequest: {:?}", e);
                connect_txn_sink
                    .send_connect_result(SelectNetworkFailure::IncompatibleConnectRequest.into());
                return connect_txn_stream;
            }
        };

        info!(
            "Received ConnectRequest for {}",
            bss_description.to_string(&self.context.inspect.hasher)
        );

        if !self.cfg.is_bss_compatible(
            &bss_description,
            &self.context.device_info,
            &self.context.security_support,
        ) {
            warn!("BSS is incompatible");
            connect_txn_sink
                .send_connect_result(SelectNetworkFailure::IncompatibleConnectRequest.into());
            return connect_txn_stream;
        }

        // TODO(fxbug.dev/95873): This code is temporary. It attempts to construct an
        //                        `Authentication` message from a security context using the
        //                        `Credential` in the `ConnectRequest` message. In the future, the
        //                        `Credential` field will be replaced with an `Authentication`
        //                        field and this step will no longer be needed.
        let authentication = Authentication::try_from(SecurityContext {
            security: &req.credential,
            device: &self.context.device_info,
            security_support: &self.context.security_support,
            config: &self.cfg,
            bss: &bss_description,
        });
        let protection = match authentication
            .map_err(From::from)
            .and_then(|authentication| {
                SecurityAuthenticator::try_from(authentication).map_err(From::from)
            })
            .and_then(|authenticator| {
                Protection::try_from(SecurityContext {
                    security: &authenticator,
                    device: &self.context.device_info,
                    security_support: &self.context.security_support,
                    config: &self.cfg,
                    bss: &bss_description,
                })
            }) {
            Ok(protection) => protection,
            Err(error) => {
                warn!(
                    "{:?}",
                    format!(
                        "Failed to configure protection for network {} ({}): {:?}",
                        self.context.inspect.hasher.hash_ssid(&bss_description.ssid),
                        self.context.inspect.hasher.hash_mac_addr(&bss_description.bssid.0),
                        error
                    )
                );
                connect_txn_sink
                    .send_connect_result(SelectNetworkFailure::IncompatibleConnectRequest.into());
                return connect_txn_stream;
            }
        };
        let cmd =
            ConnectCommand { bss: Box::new(bss_description.clone()), connect_txn_sink, protection };

        self.state = self.state.take().map(|state| state.connect(cmd, &mut self.context));
        connect_txn_stream
    }

    pub fn on_disconnect_command(
        &mut self,
        policy_disconnect_reason: fidl_sme::UserDisconnectReason,
    ) {
        self.state = self
            .state
            .take()
            .map(|state| state.disconnect(&mut self.context, policy_disconnect_reason));
        self.context.inspect.update_pulse(self.status());
    }

    pub fn on_scan_command(
        &mut self,
        scan_request: fidl_sme::ScanRequest,
    ) -> oneshot::Receiver<Result<Vec<wlan_common::scan::ScanResult>, fidl_mlme::ScanResultCode>>
    {
        info!(
            "SME received a scan command, initiating a{} discovery scan",
            match scan_request {
                fidl_sme::ScanRequest::Active(_) => "n active",
                fidl_sme::ScanRequest::Passive(_) => " passive",
            }
        );
        let (responder, receiver) = Responder::new();
        let scan = DiscoveryScan::new(responder, scan_request);
        let req = self.scan_sched.enqueue_scan_to_discover(scan);
        self.send_scan_request(req);
        receiver
    }

    pub fn status(&self) -> ClientSmeStatus {
        self.state.as_ref().expect("expected state to be always present").status()
    }

    pub fn wmm_status(&mut self) -> oneshot::Receiver<fidl_sme::ClientSmeWmmStatusResult> {
        let (responder, receiver) = Responder::new();
        self.wmm_status_responders.push(responder);
        self.context.mlme_sink.send(MlmeRequest::WmmStatusReq);
        receiver
    }

    fn send_scan_request(&mut self, req: Option<fidl_mlme::ScanRequest>) {
        if let Some(req) = req {
            self.context.mlme_sink.send(MlmeRequest::Scan(req));
        }
    }
}

impl super::Station for ClientSme {
    type Event = Event;

    fn on_mlme_event(&mut self, event: fidl_mlme::MlmeEvent) {
        match event {
            fidl_mlme::MlmeEvent::OnScanResult { result } => self
                .scan_sched
                .on_mlme_scan_result(result, &self.context.inspect)
                .unwrap_or_else(|e| error!("scan result error: {:?}", e)),
            fidl_mlme::MlmeEvent::OnScanEnd { end } => {
                match self.scan_sched.on_mlme_scan_end(end, &self.context.inspect) {
                    Err(e) => error!("scan end error: {:?}", e),
                    Ok((scan_end, next_request)) => {
                        // Finalize stats for previous scan before sending scan request for
                        // the next one, which start stats collection for new scan.
                        self.send_scan_request(next_request);

                        match scan_end.result_code {
                            fidl_mlme::ScanResultCode::Success => {
                                let scan_result_list: Vec<ScanResult> = scan_end
                                    .bss_description_list
                                    .into_iter()
                                    .map(|bss_description| {
                                        self.cfg.create_scan_result(
                                            // TODO(fxbug.dev/83882): ScanEnd drops the timestamp from MLME
                                            zx::Time::from_nanos(0),
                                            bss_description,
                                            &self.context.device_info,
                                            &self.context.security_support,
                                        )
                                    })
                                    .collect();
                                for responder in scan_end.tokens {
                                    responder.respond(Ok(scan_result_list.clone()));
                                }
                            }
                            result_code => {
                                let count = scan_end.bss_description_list.len();
                                if count > 0 {
                                    warn!("Incomplete scan with {} pending results.", count);
                                }
                                for responder in scan_end.tokens {
                                    responder.respond(Err(result_code));
                                }
                            }
                        }
                    }
                }
            }
            fidl_mlme::MlmeEvent::OnWmmStatusResp { status, resp } => {
                for responder in self.wmm_status_responders.drain(..) {
                    let result =
                        if status == zx::sys::ZX_OK { Ok(resp.clone()) } else { Err(status) };
                    responder.respond(result);
                }
                let event = fidl_mlme::MlmeEvent::OnWmmStatusResp { status, resp };
                self.state =
                    self.state.take().map(|state| state.on_mlme_event(event, &mut self.context));
            }
            other => {
                self.state =
                    self.state.take().map(|state| state.on_mlme_event(other, &mut self.context));
            }
        };

        self.context.inspect.update_pulse(self.status());
    }

    fn on_timeout(&mut self, timed_event: TimedEvent<Event>) {
        self.state = self.state.take().map(|state| match timed_event.event {
            event @ Event::EstablishingRsnaTimeout(..)
            | event @ Event::KeyFrameExchangeTimeout(..)
            | event @ Event::SaeTimeout(..) => {
                state.handle_timeout(timed_event.id, event, &mut self.context)
            }
            Event::InspectPulseCheck(..) => {
                self.context.mlme_sink.send(MlmeRequest::WmmStatusReq);
                self.context.timer.schedule(event::InspectPulseCheck);
                state
            }
            Event::InspectPulsePersist(..) => {
                // Auto persist based on a timer to avoid log spam. The default approach is
                // is to wrap AutoPersist around the Inspect PulseNode, but because the pulse
                // is updated every second (due to SignalIndication event), we'd send a request
                // to persistence service which'd log every second that it's queued until backoff.
                let _guard = self.auto_persist_last_pulse.get_mut();
                self.context.timer.schedule(event::InspectPulsePersist);
                state
            }
        });

        // Because `self.status()` relies on the value of `self.state` to be present, we cannot
        // retrieve it and update pulse node inside the closure above.
        self.context.inspect.update_pulse(self.status());
    }
}

fn report_connect_finished(connect_txn_sink: &mut ConnectTransactionSink, result: ConnectResult) {
    connect_txn_sink.send_connect_result(result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config as SmeConfig;
    use fidl_fuchsia_wlan_common as fidl_common;
    use fidl_fuchsia_wlan_common_security as fidl_security;
    use fidl_fuchsia_wlan_internal as fidl_internal;
    use fidl_fuchsia_wlan_mlme as fidl_mlme;
    use fuchsia_async as fasync;
    use fuchsia_inspect as finspect;
    use ieee80211::MacAddr;
    use std::convert::TryFrom;
    use wlan_common::{
        assert_variant,
        channel::Cbw,
        fake_bss_description, fake_fidl_bss_description,
        ie::{fake_ht_cap_bytes, fake_vht_cap_bytes, /*rsn::akm,*/ IeType},
        security::{wep::WEP40_KEY_BYTES, wpa::credential::PSK_SIZE_BYTES, SecurityAuthenticator},
        test_utils::{
            fake_features::{
                fake_mac_sublayer_support, fake_security_support, fake_security_support_empty,
                fake_spectrum_management_support_empty,
            },
            fake_stas::IesOverrides,
        },
    };

    use super::test_utils::{create_on_wmm_status_resp, fake_wmm_param, fake_wmm_status_resp};

    use crate::test_utils;
    use crate::Station;

    const CLIENT_ADDR: MacAddr = [0x7A, 0xE7, 0x76, 0xD9, 0xF2, 0x67];
    const DUMMY_HASH_KEY: [u8; 8] = [88, 77, 66, 55, 44, 33, 22, 11];

    fn authentication_open() -> fidl_security::Authentication {
        fidl_security::Authentication { protocol: fidl_security::Protocol::Open, credentials: None }
    }

    fn authentication_wep40() -> fidl_security::Authentication {
        fidl_security::Authentication {
            protocol: fidl_security::Protocol::Wep,
            credentials: Some(Box::new(fidl_security::Credentials::Wep(
                fidl_security::WepCredentials { key: [1; WEP40_KEY_BYTES].into() },
            ))),
        }
    }

    fn authentication_wpa2_personal_psk() -> fidl_security::Authentication {
        fidl_security::Authentication {
            protocol: fidl_security::Protocol::Wpa2Personal,
            credentials: Some(Box::new(fidl_security::Credentials::Wpa(
                fidl_security::WpaCredentials::Psk([1; PSK_SIZE_BYTES].into()),
            ))),
        }
    }

    fn authentication_wpa2_personal_passphrase() -> fidl_security::Authentication {
        fidl_security::Authentication {
            protocol: fidl_security::Protocol::Wpa2Personal,
            credentials: Some(Box::new(fidl_security::Credentials::Wpa(
                fidl_security::WpaCredentials::Passphrase(
                    b"password".as_slice().try_into().unwrap(),
                ),
            ))),
        }
    }

    fn authentication_wpa3_personal_passphrase() -> fidl_security::Authentication {
        fidl_security::Authentication {
            protocol: fidl_security::Protocol::Wpa3Personal,
            credentials: Some(Box::new(fidl_security::Credentials::Wpa(
                fidl_security::WpaCredentials::Passphrase(
                    b"password".as_slice().try_into().unwrap(),
                ),
            ))),
        }
    }

    fn report_fake_scan_result(
        sme: &mut ClientSme,
        timestamp_nanos: i64,
        bss: fidl_internal::BssDescription,
    ) {
        sme.on_mlme_event(fidl_mlme::MlmeEvent::OnScanResult {
            result: fidl_mlme::ScanResult { txn_id: 1, timestamp_nanos, bss },
        });
        sme.on_mlme_event(fidl_mlme::MlmeEvent::OnScanEnd {
            end: fidl_mlme::ScanEnd { txn_id: 1, code: fidl_mlme::ScanResultCode::Success },
        });
    }

    #[test]
    fn verify_protection_compatibility() {
        // Compatible:
        let cfg = ClientConfig::default();
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Open),
            &fake_security_support_empty()
        ));
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa1Wpa2TkipOnly),
            &fake_security_support_empty()
        ));
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa2TkipOnly),
            &fake_security_support_empty()
        ));
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa2),
            &fake_security_support_empty()
        ));
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa2Wpa3),
            &fake_security_support_empty()
        ));

        // Not compatible:
        assert!(!cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa1),
            &fake_security_support_empty()
        ));
        assert!(!cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa3),
            &fake_security_support_empty()
        ));
        assert!(!cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa3Transition),
            &fake_security_support_empty()
        ));
        assert!(!cfg.is_bss_protection_compatible(
            &fake_bss_description!(Eap),
            &fake_security_support_empty()
        ));

        // WEP support is configurable to be on or off:
        let cfg = ClientConfig::from_config(Config::default().with_wep(), false);
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wep),
            &fake_security_support_empty()
        ));

        // WPA1 support is configurable to be on or off:
        let cfg = ClientConfig::from_config(Config::default().with_wpa1(), false);
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa1),
            &fake_security_support_empty()
        ));

        // WPA3 support is configurable to be on or off:
        let cfg = ClientConfig::from_config(Config::default(), true);
        let mut security_support = fake_security_support_empty();
        security_support.mfp.supported = true;
        assert!(cfg.is_bss_protection_compatible(&fake_bss_description!(Wpa3), &security_support,));
        assert!(cfg.is_bss_protection_compatible(
            &fake_bss_description!(Wpa3Transition),
            &security_support,
        ));
    }

    #[test]
    fn verify_rates_compatibility() {
        // Compatible:
        let cfg = ClientConfig::default();
        let device_info = test_utils::fake_device_info([1u8; 6]);
        assert!(cfg
            .are_bss_channel_and_data_rates_compatible(&fake_bss_description!(Open), &device_info));

        // Not compatible:
        let bss = fake_bss_description!(Open, rates: vec![140, 255]);
        assert!(!cfg.are_bss_channel_and_data_rates_compatible(&bss, &device_info));
    }

    #[test]
    fn convert_scan_result() {
        let cfg = ClientConfig::default();
        let bss_description = fake_bss_description!(Wpa2,
            ssid: Ssid::empty(),
            bssid: [0u8; 6],
            rssi_dbm: -30,
            snr_db: 0,
            channel: Channel::new(1, Cbw::Cbw20),
            ies_overrides: IesOverrides::new()
                .set(IeType::HT_CAPABILITIES, fake_ht_cap_bytes().to_vec())
                .set(IeType::VHT_CAPABILITIES, fake_vht_cap_bytes().to_vec()),
        );
        let device_info = test_utils::fake_device_info([1u8; 6]);
        let timestamp = zx::Time::get_monotonic();
        let scan_result = cfg.create_scan_result(
            timestamp,
            bss_description.clone(),
            &device_info,
            &fake_security_support(),
        );

        assert_eq!(scan_result, ScanResult { compatible: true, timestamp, bss_description });

        let wmm_param = *ie::parse_wmm_param(&fake_wmm_param().bytes[..])
            .expect("expect WMM param to be parseable");
        let bss_description = fake_bss_description!(Wpa2,
            ssid: Ssid::empty(),
            bssid: [0u8; 6],
            rssi_dbm: -30,
            snr_db: 0,
            channel: Channel::new(1, Cbw::Cbw20),
            wmm_param: Some(wmm_param),
            ies_overrides: IesOverrides::new()
                .set(IeType::HT_CAPABILITIES, fake_ht_cap_bytes().to_vec())
                .set(IeType::VHT_CAPABILITIES, fake_vht_cap_bytes().to_vec()),
        );
        let timestamp = zx::Time::get_monotonic();
        let scan_result = cfg.create_scan_result(
            timestamp,
            bss_description.clone(),
            &device_info,
            &fake_security_support(),
        );

        assert_eq!(scan_result, ScanResult { compatible: true, timestamp, bss_description });

        let bss_description = fake_bss_description!(Wep,
            ssid: Ssid::empty(),
            bssid: [0u8; 6],
            rssi_dbm: -30,
            snr_db: 0,
            channel: Channel::new(1, Cbw::Cbw20),
            ies_overrides: IesOverrides::new()
                .set(IeType::HT_CAPABILITIES, fake_ht_cap_bytes().to_vec())
                .set(IeType::VHT_CAPABILITIES, fake_vht_cap_bytes().to_vec()),
        );
        let timestamp = zx::Time::get_monotonic();
        let scan_result = cfg.create_scan_result(
            timestamp,
            bss_description.clone(),
            &device_info,
            &fake_security_support(),
        );
        assert_eq!(scan_result, ScanResult { compatible: false, timestamp, bss_description },);

        let cfg = ClientConfig::from_config(Config::default().with_wep(), false);
        let bss_description = fake_bss_description!(Wep,
            ssid: Ssid::empty(),
            bssid: [0u8; 6],
            rssi_dbm: -30,
            snr_db: 0,
            channel: Channel::new(1, Cbw::Cbw20),
            ies_overrides: IesOverrides::new()
                .set(IeType::HT_CAPABILITIES, fake_ht_cap_bytes().to_vec())
                .set(IeType::VHT_CAPABILITIES, fake_vht_cap_bytes().to_vec()),
        );
        let timestamp = zx::Time::get_monotonic();
        let scan_result = cfg.create_scan_result(
            timestamp,
            bss_description.clone(),
            &device_info,
            &fake_security_support(),
        );
        assert_eq!(
            scan_result,
            wlan_common::scan::ScanResult { compatible: true, timestamp, bss_description },
        );
    }

    #[test]
    fn test_detection_of_rejected_wpa1_or_wpa2_credentials() {
        let failure = ConnectFailure::EstablishRsnaFailure(EstablishRsnaFailure {
            auth_method: Some(auth::MethodName::Psk),
            reason: EstablishRsnaFailureReason::OverallTimeout(
                wlan_rsn::Error::LikelyWrongCredential,
            ),
        });
        assert!(failure.likely_due_to_credential_rejected());
    }

    #[test]
    fn test_detection_of_rejected_wep_credentials() {
        let failure = ConnectFailure::AssociationFailure(AssociationFailure {
            bss_protection: BssProtection::Wep,
            code: fidl_ieee80211::StatusCode::RefusedUnauthenticatedAccessNotSupported,
        });
        assert!(failure.likely_due_to_credential_rejected());
    }

    #[test]
    fn test_no_detection_of_rejected_wpa1_or_wpa2_credentials() {
        let failure = ConnectFailure::ScanFailure(fidl_mlme::ScanResultCode::InternalError);
        assert!(!failure.likely_due_to_credential_rejected());

        let failure = ConnectFailure::AssociationFailure(AssociationFailure {
            bss_protection: BssProtection::Wpa2Personal,
            code: fidl_ieee80211::StatusCode::RefusedUnauthenticatedAccessNotSupported,
        });
        assert!(!failure.likely_due_to_credential_rejected());
    }

    #[test]
    fn test_protection_from_authentication() {
        let device = test_utils::fake_device_info(CLIENT_ADDR);
        let security_support = fake_security_support();
        let config = Default::default();

        // Open BSS with open authentication:
        let authenticator = SecurityAuthenticator::try_from(authentication_open()).unwrap();
        let bss = fake_bss_description!(Open);
        let protection = Protection::try_from(SecurityContext {
            security: &authenticator,
            device: &device,
            security_support: &security_support,
            config: &config,
            bss: &bss,
        });
        assert_variant!(protection, Ok(Protection::Open));

        // Open BSS with WPA2 Personal and passphrase authentication:
        let authenticator =
            SecurityAuthenticator::try_from(authentication_wpa2_personal_passphrase()).unwrap();
        let bss = fake_bss_description!(Open);
        Protection::try_from(SecurityContext {
            security: &authenticator,
            device: &device,
            security_support: &security_support,
            config: &config,
            bss: &bss,
        })
        .expect_err("cannot associate with open network using WPA");

        // RSN BSS with WPA2 Personal and passphrase authentication:
        let authenticator =
            SecurityAuthenticator::try_from(authentication_wpa2_personal_passphrase()).unwrap();
        let bss = fake_bss_description!(Wpa2);
        let protection = Protection::try_from(SecurityContext {
            security: &authenticator,
            device: &device,
            security_support: &security_support,
            config: &config,
            bss: &bss,
        });
        assert_variant!(protection, Ok(Protection::Rsna(_)));

        // RSN BSS with WPA2 Personal and PSK authentication:
        let authenticator =
            SecurityAuthenticator::try_from(authentication_wpa2_personal_psk()).unwrap();
        let bss = fake_bss_description!(Wpa2);
        let protection = Protection::try_from(SecurityContext {
            security: &authenticator,
            device: &device,
            security_support: &security_support,
            config: &config,
            bss: &bss,
        });
        assert_variant!(protection, Ok(Protection::Rsna(_)));

        // RSN BSS with open authentication:
        let authenticator = SecurityAuthenticator::try_from(authentication_open()).unwrap();
        let bss = fake_bss_description!(Wpa2);
        Protection::try_from(SecurityContext {
            security: &authenticator,
            device: &device,
            security_support: &security_support,
            config: &config,
            bss: &bss,
        })
        .expect_err("cannot associate with secure network using no security protocol");
    }

    #[test]
    fn status_connecting() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, _mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        // Issue a connect command and expect the status to change appropriately.
        let bss_description =
            fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap());
        let _recv = sme.on_connect_command(connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description,
            authentication_open(),
        ));
        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("foo").unwrap()), sme.status());

        // We should still be connecting to "foo", but the status should now come from the state
        // machine and not from the scanner.
        let ssid = assert_variant!(sme.state.as_ref().unwrap().status(), ClientSmeStatus::Connecting(ssid) => ssid);
        assert_eq!(Ssid::try_from("foo").unwrap(), ssid);
        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("foo").unwrap()), sme.status());

        // As soon as connect command is issued for "bar", the status changes immediately
        let bss_description =
            fake_fidl_bss_description!(Open, ssid: Ssid::try_from("bar").unwrap());
        let _recv2 = sme.on_connect_command(connect_req(
            Ssid::try_from("bar").unwrap(),
            bss_description,
            authentication_open(),
        ));
        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("bar").unwrap()), sme.status());
    }

    #[test]
    fn connecting_to_wep_network_supported() {
        let _executor = fuchsia_async::TestExecutor::new();
        let inspector = finspect::Inspector::new();
        let sme_root_node = inspector.root().create_child("sme");
        let (persistence_req_sender, _persistence_receiver) =
            test_utils::create_inspect_persistence_channel();
        let mut mac_sublayer_support = fake_mac_sublayer_support();
        // TODO(fxbug.dev/96668) - FullMAC still uses the old state machine. Once FullMAC is
        //                         fully transitioned, this override will no longer be
        //                         necessary.
        mac_sublayer_support.device.mac_implementation_type =
            fidl_common::MacImplementationType::Fullmac;
        let (mut sme, mut mlme_stream, _time_stream) = ClientSme::new(
            ClientConfig::from_config(SmeConfig::default().with_wep(), false),
            test_utils::fake_device_info(CLIENT_ADDR),
            Arc::new(wlan_inspect::iface_mgr::IfaceTreeHolder::new(sme_root_node)),
            WlanHasher::new(DUMMY_HASH_KEY),
            persistence_req_sender,
            mac_sublayer_support,
            fake_security_support(),
            fake_spectrum_management_support_empty(),
        );
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        // Issue a connect command and expect the status to change appropriately.
        let bss_description = fake_fidl_bss_description!(Wep, ssid: Ssid::try_from("foo").unwrap());
        let req =
            connect_req(Ssid::try_from("foo").unwrap(), bss_description, authentication_wep40());
        let _recv = sme.on_connect_command(req);
        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("foo").unwrap()), sme.status());
        assert_variant!(mlme_stream.try_next(), Ok(Some(MlmeRequest::Join(..))));
    }

    #[test]
    fn connecting_to_wep_network_unsupported() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut _mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        // Issue a connect command and expect the status to change appropriately.
        let bss_description = fake_fidl_bss_description!(Wep, ssid: Ssid::try_from("foo").unwrap());
        let req =
            connect_req(Ssid::try_from("foo").unwrap(), bss_description, authentication_wep40());
        let mut _connect_fut = sme.on_connect_command(req);
        assert_eq!(ClientSmeStatus::Idle, sme.state.as_ref().unwrap().status());
    }

    #[test]
    fn connecting_password_supplied_for_protected_network() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        // Issue a connect command and expect the status to change appropriately.
        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("foo").unwrap());
        let req = connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description,
            authentication_wpa2_personal_passphrase(),
        );
        let _recv = sme.on_connect_command(req);
        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("foo").unwrap()), sme.status());

        assert_variant!(mlme_stream.try_next(), Ok(Some(MlmeRequest::Join(..))));
    }

    #[test]
    fn connecting_psk_supplied_for_protected_network() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        // Issue a connect command and expect the status to change appropriately.
        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("IEEE").unwrap());
        let req = connect_req(
            Ssid::try_from("IEEE").unwrap(),
            bss_description,
            authentication_wpa2_personal_psk(),
        );
        let _recv = sme.on_connect_command(req);
        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("IEEE").unwrap()), sme.status());

        assert_variant!(mlme_stream.try_next(), Ok(Some(MlmeRequest::Join(..))));
    }

    #[test]
    fn connecting_password_supplied_for_unprotected_network() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut _mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        let bss_description =
            fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap());
        let req = connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description,
            authentication_wpa2_personal_passphrase(),
        );
        let mut connect_txn_stream = sme.on_connect_command(req);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        // User should get a message that connection failed
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn connecting_psk_supplied_for_unprotected_network() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut _mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        let bss_description =
            fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap());
        let req = connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description,
            authentication_wpa2_personal_psk(),
        );
        let mut connect_txn_stream = sme.on_connect_command(req);
        assert_eq!(ClientSmeStatus::Idle, sme.state.as_ref().unwrap().status());

        // User should get a message that connection failed
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn connecting_no_password_supplied_for_protected_network() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("foo").unwrap());
        let req =
            connect_req(Ssid::try_from("foo").unwrap(), bss_description, authentication_open());
        let mut connect_txn_stream = sme.on_connect_command(req);
        assert_eq!(ClientSmeStatus::Idle, sme.state.as_ref().unwrap().status());

        // No join request should be sent to MLME
        assert_no_join(&mut mlme_stream);

        // User should get a message that connection failed
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn connecting_bypass_join_scan_open() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        let bss_description =
            fake_fidl_bss_description!(Open, ssid: Ssid::try_from("bssname").unwrap());
        let req =
            connect_req(Ssid::try_from("bssname").unwrap(), bss_description, authentication_open());
        let mut connect_txn_stream = sme.on_connect_command(req);

        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("bssname").unwrap()), sme.status());
        assert_variant!(mlme_stream.try_next(), Ok(Some(MlmeRequest::Join(..))));
        // There should be no message in the connect_txn_stream
        assert_variant!(connect_txn_stream.try_next(), Err(_));
    }

    #[test]
    fn connecting_bypass_join_scan_protected() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("bssname").unwrap());
        let req = connect_req(
            Ssid::try_from("bssname").unwrap(),
            bss_description,
            authentication_wpa2_personal_passphrase(),
        );
        let mut connect_txn_stream = sme.on_connect_command(req);

        assert_eq!(ClientSmeStatus::Connecting(Ssid::try_from("bssname").unwrap()), sme.status());
        assert_variant!(mlme_stream.try_next(), Ok(Some(MlmeRequest::Join(..))));
        // There should be no message in the connect_txn_stream
        assert_variant!(connect_txn_stream.try_next(), Err(_));
    }

    #[test]
    fn connecting_bypass_join_scan_mismatched_credential() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("bssname").unwrap());
        let req =
            connect_req(Ssid::try_from("bssname").unwrap(), bss_description, authentication_open());
        let mut connect_txn_stream = sme.on_connect_command(req);

        assert_eq!(ClientSmeStatus::Idle, sme.status());
        assert_no_join(&mut mlme_stream);

        // User should get a message that connection failed
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn connecting_bypass_join_scan_unsupported_bss() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        let bss_description =
            fake_fidl_bss_description!(Wpa3Enterprise, ssid: Ssid::try_from("bssname").unwrap());
        let req = connect_req(
            Ssid::try_from("bssname").unwrap(),
            bss_description,
            authentication_wpa3_personal_passphrase(),
        );
        let mut connect_txn_stream = sme.on_connect_command(req);

        assert_eq!(ClientSmeStatus::Idle, sme.status());
        assert_no_join(&mut mlme_stream);

        // User should get a message that connection failed
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn connecting_right_credential_type_no_privacy() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, _mlme_stream, _time_stream) = create_sme(&exec);

        let bss_description = fake_fidl_bss_description!(
            Wpa2,
            ssid: Ssid::try_from("foo").unwrap(),
        );
        // Manually override the privacy bit since fake_fidl_bss_description!()
        // does not allow setting it directly.
        let bss_description = fidl_internal::BssDescription {
            capability_info: wlan_common::mac::CapabilityInfo(bss_description.capability_info)
                .with_privacy(false)
                .0,
            ..bss_description
        };
        let mut connect_txn_stream = sme.on_connect_command(connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description,
            authentication_wpa2_personal_passphrase(),
        ));

        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn connecting_mismatched_security_protocol() {
        let executor = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, _mlme_stream, _time_stream) = create_sme(&executor);

        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("wpa2").unwrap());
        let mut connect_txn_stream = sme.on_connect_command(connect_req(
            Ssid::try_from("wpa2").unwrap(),
            bss_description,
            authentication_wep40(),
        ));
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );

        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("wpa2").unwrap());
        let mut connect_txn_stream = sme.on_connect_command(connect_req(
            Ssid::try_from("wpa2").unwrap(),
            bss_description,
            // TODO(fxbug.dev/95873): The more interesting test case here is a WPA1 using
            //                        credentials that are compatible with both WPA1 and WPA2.
            //                        However, because the `Authentication`'s security protocol is
            //                        ignored, this fails, because a passphrase is compatible with
            //                        WPA2. Change this test case once `Authentication` is part of
            //                        the SME `Connect` FIDL API.
            //authentication_wpa1_passphrase(),
            authentication_open(),
        ));
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );

        let bss_description =
            fake_fidl_bss_description!(Wpa3, ssid: Ssid::try_from("wpa3").unwrap());
        let mut connect_txn_stream = sme.on_connect_command(connect_req(
            Ssid::try_from("wpa3").unwrap(),
            bss_description,
            // TODO(fxbug.dev/95873): See the TODO above.
            //authentication_wpa2_personal_passphrase(),
            authentication_wpa2_personal_psk(),
        ));
        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn connecting_right_credential_type_but_short_password() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, _mlme_stream, _time_stream) = create_sme(&exec);

        let bss_description =
            fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("foo").unwrap());
        let mut connect_txn_stream = sme.on_connect_command(connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description.clone(),
            fidl_security::Authentication {
                protocol: fidl_security::Protocol::Wpa2Personal,
                credentials: Some(Box::new(fidl_security::Credentials::Wpa(
                    fidl_security::WpaCredentials::Passphrase(
                        b"nope".as_slice().try_into().unwrap(),
                    ),
                ))),
            },
        ));
        report_fake_scan_result(&mut sme, zx::Time::get_monotonic().into_nanos(), bss_description);

        assert_variant!(
            connect_txn_stream.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult { result, is_reconnect: false })) => {
                assert_eq!(result, SelectNetworkFailure::IncompatibleConnectRequest.into());
            }
        );
    }

    #[test]
    fn new_connect_attempt_cancels_pending_connect() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, _mlme_stream, _time_stream) = create_sme(&exec);

        let bss_description =
            fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap());
        let req = connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description.clone(),
            authentication_open(),
        );
        let mut connect_txn_stream1 = sme.on_connect_command(req);

        let req2 = connect_req(
            Ssid::try_from("foo").unwrap(),
            bss_description.clone(),
            authentication_open(),
        );
        let mut connect_txn_stream2 = sme.on_connect_command(req2);

        // User should get a message that first connection attempt is canceled
        assert_variant!(
            connect_txn_stream1.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult {
                result: ConnectResult::Canceled,
                is_reconnect: false
            }))
        );

        // Report scan result to transition second connection attempt past scan. This is to verify
        // that connection attempt will be canceled even in the middle of joining the network
        report_fake_scan_result(
            &mut sme,
            zx::Time::get_monotonic().into_nanos(),
            fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap()),
        );

        let req3 =
            connect_req(Ssid::try_from("foo").unwrap(), bss_description, authentication_open());
        let mut _connect_fut3 = sme.on_connect_command(req3);

        // Verify that second connection attempt is canceled as new connect request comes in
        assert_variant!(
            connect_txn_stream2.try_next(),
            Ok(Some(ConnectTransactionEvent::OnConnectResult {
                result: ConnectResult::Canceled,
                is_reconnect: false
            }))
        );
    }

    #[test]
    fn test_simple_scan_error() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, _mlme_strem, _time_stream) = create_sme(&exec);
        let mut recv =
            sme.on_scan_command(fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {}));

        sme.on_mlme_event(fidl_mlme::MlmeEvent::OnScanEnd {
            end: fidl_mlme::ScanEnd {
                txn_id: 1,
                code: fidl_mlme::ScanResultCode::CanceledByDriverOrFirmware,
            },
        });

        assert_eq!(
            recv.try_recv(),
            Ok(Some(Err(fidl_mlme::ScanResultCode::CanceledByDriverOrFirmware)))
        );
    }

    #[test]
    fn test_scan_error_after_some_results_returned() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, _mlme_strem, _time_stream) = create_sme(&exec);
        let mut recv =
            sme.on_scan_command(fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {}));

        let mut bss = fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap());
        bss.bssid = [3; 6];
        sme.on_mlme_event(fidl_mlme::MlmeEvent::OnScanResult {
            result: fidl_mlme::ScanResult {
                txn_id: 1,
                timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
                bss,
            },
        });
        let mut bss = fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap());
        bss.bssid = [4; 6];
        sme.on_mlme_event(fidl_mlme::MlmeEvent::OnScanResult {
            result: fidl_mlme::ScanResult {
                txn_id: 1,
                timestamp_nanos: zx::Time::get_monotonic().into_nanos(),
                bss,
            },
        });

        sme.on_mlme_event(fidl_mlme::MlmeEvent::OnScanEnd {
            end: fidl_mlme::ScanEnd {
                txn_id: 1,
                code: fidl_mlme::ScanResultCode::CanceledByDriverOrFirmware,
            },
        });

        // Scan results are lost when an error occurs.
        assert_eq!(
            recv.try_recv(),
            Ok(Some(Err(fidl_mlme::ScanResultCode::CanceledByDriverOrFirmware)))
        );
    }

    #[test]
    fn test_wmm_status_success() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        let mut receiver = sme.wmm_status();

        assert_variant!(mlme_stream.try_next(), Ok(Some(MlmeRequest::WmmStatusReq)));

        let resp = fake_wmm_status_resp();
        sme.on_mlme_event(fidl_mlme::MlmeEvent::OnWmmStatusResp {
            status: zx::sys::ZX_OK,
            resp: resp.clone(),
        });

        assert_eq!(receiver.try_recv(), Ok(Some(Ok(resp))));
    }

    #[test]
    fn test_wmm_status_failed() {
        let exec = fuchsia_async::TestExecutor::new().unwrap();
        let (mut sme, mut mlme_stream, _time_stream) = create_sme(&exec);
        let mut receiver = sme.wmm_status();

        assert_variant!(mlme_stream.try_next(), Ok(Some(MlmeRequest::WmmStatusReq)));
        sme.on_mlme_event(create_on_wmm_status_resp(zx::sys::ZX_ERR_IO));
        assert_eq!(receiver.try_recv(), Ok(Some(Err(zx::sys::ZX_ERR_IO))));
    }

    #[test]
    fn test_inspect_pulse_persist() {
        let _executor = fuchsia_async::TestExecutor::new();
        let inspector = finspect::Inspector::new();
        let sme_root_node = inspector.root().create_child("sme");
        let (persistence_req_sender, mut persistence_receiver) =
            test_utils::create_inspect_persistence_channel();
        let (mut sme, _mlme_stream, mut time_stream) = ClientSme::new(
            ClientConfig::from_config(SmeConfig::default().with_wep(), false),
            test_utils::fake_device_info(CLIENT_ADDR),
            Arc::new(wlan_inspect::iface_mgr::IfaceTreeHolder::new(sme_root_node)),
            WlanHasher::new(DUMMY_HASH_KEY),
            persistence_req_sender,
            fake_mac_sublayer_support(),
            fake_security_support(),
            fake_spectrum_management_support_empty(),
        );
        assert_eq!(ClientSmeStatus::Idle, sme.status());

        // Verify we request persistence on startup
        assert_variant!(persistence_receiver.try_next(), Ok(Some(tag)) => {
            assert_eq!(&tag, "wlanstack-last-pulse");
        });

        let mut persist_event = None;
        while let Ok(Some((_timeout, timed_event))) = time_stream.try_next() {
            match timed_event.event {
                Event::InspectPulsePersist(..) => {
                    persist_event = Some(timed_event);
                    break;
                }
                _ => (),
            }
        }
        assert!(persist_event.is_some());
        sme.on_timeout(persist_event.unwrap());

        // Verify we request persistence again on timeout
        assert_variant!(persistence_receiver.try_next(), Ok(Some(tag)) => {
            assert_eq!(&tag, "wlanstack-last-pulse");
        });
    }

    fn assert_no_join(mlme_stream: &mut mpsc::UnboundedReceiver<MlmeRequest>) {
        loop {
            match mlme_stream.try_next() {
                Ok(event) => match event {
                    Some(MlmeRequest::Join(..)) => panic!("unexpected join request sent to MLME"),
                    None => break,
                    _ => (),
                },
                Err(e) => {
                    assert_eq!(e.to_string(), "receiver channel is empty");
                    break;
                }
            }
        }
    }

    fn connect_req(
        ssid: Ssid,
        bss_description: fidl_internal::BssDescription,
        authentication: fidl_security::Authentication,
    ) -> fidl_sme::ConnectRequest {
        // TODO(fxbug.dev/95873): This code is temporary and allows tests to be written against
        //                        `Authentication` rather than `Credential` prior to changes to the
        //                        SME FIDL API. This should be removed once the `credential` field
        //                        in `ConnectRequest` is replaced with an `authentication` field.
        //                        Note that this means that security protocols specified in test
        //                        code are not subject to the given `Authentication` but rather the
        //                        heuristics used in `on_connect_command`.
        // Discard information regarding security protocol.
        let credential = if let Some(credentials) = authentication.credentials {
            match *credentials {
                fidl_security::Credentials::Wep(fidl_security::WepCredentials { key }) => {
                    fidl_sme::Credential::Password(key)
                }
                fidl_security::Credentials::Wpa(wpa) => match wpa {
                    fidl_security::WpaCredentials::Passphrase(passphrase) => {
                        fidl_sme::Credential::Password(passphrase)
                    }
                    fidl_security::WpaCredentials::Psk(psk) => {
                        fidl_sme::Credential::Psk(psk.to_vec())
                    }
                    _ => panic!("Unkown `WpaCredentials` variant"),
                },
                _ => panic!("Unknown `Credentials` variant"),
            }
        } else {
            fidl_sme::Credential::None(fidl_sme::Empty)
        };
        fidl_sme::ConnectRequest {
            ssid: ssid.to_vec(),
            bss_description,
            multiple_bss_candidates: true,
            credential,
            deprecated_scan_type: fidl_common::ScanType::Passive,
        }
    }

    // The unused _exec parameter ensures that an executor exists for the lifetime of the SME.
    // Our internal timer implementation relies on the existence of a local executor.
    fn create_sme(_exec: &fasync::TestExecutor) -> (ClientSme, MlmeStream, TimeStream) {
        let inspector = finspect::Inspector::new();
        let sme_root_node = inspector.root().create_child("sme");
        let (persistence_req_sender, _persistence_receiver) =
            test_utils::create_inspect_persistence_channel();
        let mut mac_sublayer_support = fake_mac_sublayer_support();
        // TODO(fxbug.dev/96668) - FullMAC still uses the old state machine. Once FullMAC is
        //                         fully transitioned, this override will no longer be
        //                         necessary.
        mac_sublayer_support.device.mac_implementation_type =
            fidl_common::MacImplementationType::Fullmac;
        ClientSme::new(
            ClientConfig::default(),
            test_utils::fake_device_info(CLIENT_ADDR),
            Arc::new(wlan_inspect::iface_mgr::IfaceTreeHolder::new(sme_root_node)),
            WlanHasher::new(DUMMY_HASH_KEY),
            persistence_req_sender,
            mac_sublayer_support,
            fake_security_support(),
            fake_spectrum_management_support_empty(),
        )
    }
}
