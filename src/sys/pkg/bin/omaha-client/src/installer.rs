// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This is the Fuchsia Installer implementation that talks to fuchsia.update.installer FIDL API.

use crate::install_plan::FuchsiaInstallPlan;
use anyhow::{anyhow, Context as _};
use fidl_connector::{Connect, ServiceReconnector};
use fidl_fuchsia_hardware_power_statecontrol::RebootReason;
use fidl_fuchsia_update_installer::{
    InstallerMarker, InstallerProxy, RebootControllerMarker, RebootControllerProxy,
};
use fidl_fuchsia_update_installer_ext::{
    start_update, FetchFailureReason, Initiator, MonitorUpdateAttemptError, Options,
    PrepareFailureReason, State, StateId, UpdateAttemptError,
};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_zircon as zx;
use futures::future::BoxFuture;
use futures::prelude::*;
use log::info;
use omaha_client::{
    installer::{Installer, ProgressObserver},
    protocol::request::InstallSource,
};
use std::time::Duration;
use thiserror::Error;

/// Represents possible reasons the installer could have ended in a failure state. Not exhaustive.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InstallerFailureReason {
    Internal,
    OutOfSpace,
    UnsupportedDowngrade,
}

impl From<FetchFailureReason> for InstallerFailureReason {
    fn from(r: FetchFailureReason) -> InstallerFailureReason {
        match r {
            FetchFailureReason::Internal => InstallerFailureReason::Internal,
            FetchFailureReason::OutOfSpace => InstallerFailureReason::OutOfSpace,
        }
    }
}

impl From<PrepareFailureReason> for InstallerFailureReason {
    fn from(r: PrepareFailureReason) -> InstallerFailureReason {
        match r {
            PrepareFailureReason::Internal => InstallerFailureReason::Internal,
            PrepareFailureReason::OutOfSpace => InstallerFailureReason::OutOfSpace,
            PrepareFailureReason::UnsupportedDowngrade => {
                InstallerFailureReason::UnsupportedDowngrade
            }
        }
    }
}

/// Information from the config about whether an update is urgent.
#[derive(Debug)]
pub struct InstallResult {
    pub urgent_update: bool,
}

/// Information about a specific failure state that the installer ended in.
#[derive(Debug, Copy, Clone)]
pub struct InstallerFailure {
    state_name: &'static str,
    reason: InstallerFailureReason,
}

impl InstallerFailure {
    /// Returns the name of the system-updater state this failure occurred in
    pub fn state_name(self) -> &'static str {
        self.state_name
    }

    /// Returns the reason this failure occurred
    pub fn reason(self) -> InstallerFailureReason {
        self.reason
    }

    #[cfg(test)]
    pub fn new(state_name: &'static str, reason: InstallerFailureReason) -> Self {
        Self { state_name, reason }
    }
}

#[derive(Debug, Error)]
pub enum FuchsiaInstallError {
    #[error("generic error")]
    Failure(#[from] anyhow::Error),

    #[error("FIDL error")]
    FIDL(#[from] fidl::Error),

    /// System update installer error.
    #[error("start update installer failed")]
    StartUpdate(#[from] UpdateAttemptError),

    #[error("monitor update installer failed")]
    MonitorUpdate(#[from] MonitorUpdateAttemptError),

    #[error("installer encountered failure state: {0:?}")]
    InstallerFailureState(InstallerFailure),

    #[error("installation ended unexpectedly")]
    InstallationEndedUnexpectedly,

    #[error("connect to installer service failed")]
    Connect(#[source] anyhow::Error),
}

#[derive(Debug)]
pub struct FuchsiaInstaller<C = ServiceReconnector<InstallerMarker>> {
    connector: C,
    reboot_controller: Option<RebootControllerProxy>,
}

impl FuchsiaInstaller<ServiceReconnector<InstallerMarker>> {
    pub fn new() -> Self {
        let connector = ServiceReconnector::<InstallerMarker>::new();
        Self { connector, reboot_controller: None }
    }
}

impl<C: Connect<Proxy = InstallerProxy> + Send> Installer for FuchsiaInstaller<C> {
    type InstallPlan = FuchsiaInstallPlan;
    type Error = FuchsiaInstallError;
    type InstallResult = InstallResult;

    fn perform_install<'a>(
        &'a mut self,
        install_plan: &'a FuchsiaInstallPlan,
        observer: Option<&'a dyn ProgressObserver>,
    ) -> BoxFuture<'a, Result<Self::InstallResult, FuchsiaInstallError>> {
        let options = Options {
            initiator: match install_plan.install_source {
                InstallSource::ScheduledTask => Initiator::Service,
                InstallSource::OnDemand => Initiator::User,
            },
            should_write_recovery: true,
            allow_attach_to_existing_attempt: true,
        };

        async move {
            let proxy = self.connector.connect().map_err(FuchsiaInstallError::Connect)?;
            let (reboot_controller, reboot_controller_server_end) =
                fidl::endpoints::create_proxy::<RebootControllerMarker>()
                    .map_err(FuchsiaInstallError::FIDL)?;

            self.reboot_controller = Some(reboot_controller);

            let mut update_attempt = start_update(
                &install_plan.url,
                options,
                &proxy,
                Some(reboot_controller_server_end),
            )
            .await?;

            while let Some(state) = update_attempt.try_next().await? {
                info!("Installer entered state: {}", state.name());
                if let Some(observer) = observer {
                    if let Some(progress) = state.progress() {
                        observer
                            .receive_progress(
                                Some(state.name()),
                                progress.fraction_completed(),
                                state.download_size(),
                                Some(progress.bytes_downloaded()),
                            )
                            .await;
                    } else {
                        observer.receive_progress(Some(state.name()), 0., None, None).await;
                    }
                }
                if state.id() == StateId::WaitToReboot || state.is_success() {
                    return Ok(InstallResult { urgent_update: install_plan.urgent_update });
                } else if state.is_failure() {
                    match state {
                        State::FailFetch(fail_fetch_data) => {
                            return Err(FuchsiaInstallError::InstallerFailureState(
                                InstallerFailure {
                                    state_name: state.name(),
                                    reason: fail_fetch_data.reason().into(),
                                },
                            ));
                        }
                        State::FailPrepare(prepare_failure_reason) => {
                            return Err(FuchsiaInstallError::InstallerFailureState(
                                InstallerFailure {
                                    state_name: state.name(),
                                    reason: prepare_failure_reason.into(),
                                },
                            ));
                        }
                        _ => {
                            return Err(FuchsiaInstallError::InstallerFailureState(
                                InstallerFailure {
                                    state_name: state.name(),
                                    reason: InstallerFailureReason::Internal,
                                },
                            ))
                        }
                    }
                }
            }

            Err(FuchsiaInstallError::InstallationEndedUnexpectedly)
        }
        .boxed()
    }

    fn perform_reboot(&mut self) -> BoxFuture<'_, Result<(), anyhow::Error>> {
        async move {
            match self.reboot_controller.take() {
                Some(reboot_controller) => {
                    reboot_controller
                        .unblock()
                        .context("notify installer it can reboot when ready")?;
                }
                None => {
                    // FIXME Need the direct reboot path anymore?
                    connect_to_protocol::<fidl_fuchsia_hardware_power_statecontrol::AdminMarker>()?
                        .reboot(RebootReason::SystemUpdate)
                        .await?
                        .map_err(zx::Status::from_raw)
                        .context("reboot error")?;
                }
            }

            // Device should be rebooting now, do not return because state machine expects
            // perform_reboot() to block, wait for 5 minutes and if reboot still hasn't happened,
            // return an error.
            fasync::Timer::new(Duration::from_secs(60 * 5)).await;

            Err(anyhow!("timed out while waiting for device to reboot"))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        fidl_fuchsia_update_installer::{
            FailPrepareData, InstallationProgress, InstallerRequest, InstallerRequestStream,
            RebootControllerRequest, State, UpdateInfo,
        },
        fuchsia_async as fasync,
        matches::assert_matches,
        parking_lot::Mutex,
        std::{convert::TryInto, sync::Arc, task::Poll},
    };

    const TEST_URL: &str = "fuchsia-pkg://fuchsia.com/update/0";

    #[derive(Debug, PartialEq)]
    struct Progress {
        operation: Option<String>,
        progress: f32,
        total_size: Option<u64>,
        size_so_far: Option<u64>,
    }

    impl Eq for Progress {}
    struct MockProgressObserver {
        progresses: Arc<Mutex<Vec<Progress>>>,
    }

    impl MockProgressObserver {
        fn new() -> Self {
            Self { progresses: Arc::new(Mutex::new(vec![])) }
        }
        fn progresses(&self) -> Arc<Mutex<Vec<Progress>>> {
            Arc::clone(&self.progresses)
        }
    }
    impl ProgressObserver for MockProgressObserver {
        fn receive_progress(
            &self,
            operation: Option<&str>,
            progress: f32,
            total_size: Option<u64>,
            size_so_far: Option<u64>,
        ) -> BoxFuture<'_, ()> {
            let operation = operation.map(|s| s.into());
            self.progresses.lock().push(Progress { operation, progress, total_size, size_so_far });
            future::ready(()).boxed()
        }
    }

    struct MockConnector {
        proxy: Option<InstallerProxy>,
    }

    impl MockConnector {
        fn new(proxy: InstallerProxy) -> Self {
            Self { proxy: Some(proxy) }
        }
        fn failing() -> Self {
            Self { proxy: None }
        }
    }

    impl Connect for MockConnector {
        type Proxy = InstallerProxy;

        fn connect(&self) -> Result<Self::Proxy, anyhow::Error> {
            self.proxy.clone().ok_or(anyhow::anyhow!("no proxy available"))
        }
    }

    fn new_mock_installer() -> (FuchsiaInstaller<MockConnector>, InstallerRequestStream) {
        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<InstallerMarker>().unwrap();
        let installer =
            FuchsiaInstaller { connector: MockConnector::new(proxy), reboot_controller: None };
        (installer, stream)
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_start_update() {
        let (mut installer, mut stream) = new_mock_installer();
        let plan = FuchsiaInstallPlan {
            url: TEST_URL.parse().unwrap(),
            install_source: InstallSource::OnDemand,
            urgent_update: false,
        };
        let observer = MockProgressObserver::new();
        let progresses = observer.progresses();
        let installer_fut = async move {
            let _install_result = installer.perform_install(&plan, Some(&observer)).await.unwrap();
            assert_matches!(installer.reboot_controller, Some(_));
        };
        let stream_fut = async move {
            match stream.next().await.unwrap() {
                Ok(InstallerRequest::StartUpdate {
                    url,
                    options,
                    monitor,
                    reboot_controller,
                    responder,
                }) => {
                    assert_eq!(url.url, TEST_URL);
                    let Options {
                        initiator,
                        should_write_recovery,
                        allow_attach_to_existing_attempt,
                    } = options.try_into().unwrap();
                    assert_eq!(initiator, Initiator::User);
                    assert_matches!(reboot_controller, Some(_));
                    assert_eq!(should_write_recovery, true);
                    assert_eq!(allow_attach_to_existing_attempt, true);
                    responder
                        .send(&mut Ok("00000000-0000-0000-0000-000000000001".to_owned()))
                        .unwrap();
                    let monitor = monitor.into_proxy().unwrap();
                    let () = monitor
                        .on_state(&mut State::Stage(fidl_fuchsia_update_installer::StageData {
                            info: Some(UpdateInfo {
                                download_size: Some(1000),
                                ..UpdateInfo::EMPTY
                            }),
                            progress: Some(InstallationProgress {
                                fraction_completed: Some(0.5),
                                bytes_downloaded: Some(500),
                                ..InstallationProgress::EMPTY
                            }),
                            ..fidl_fuchsia_update_installer::StageData::EMPTY
                        }))
                        .await
                        .unwrap();
                    let () = monitor
                        .on_state(&mut State::WaitToReboot(
                            fidl_fuchsia_update_installer::WaitToRebootData {
                                info: Some(UpdateInfo {
                                    download_size: Some(1000),
                                    ..UpdateInfo::EMPTY
                                }),
                                progress: Some(InstallationProgress {
                                    fraction_completed: Some(1.0),
                                    bytes_downloaded: Some(1000),
                                    ..InstallationProgress::EMPTY
                                }),
                                ..fidl_fuchsia_update_installer::WaitToRebootData::EMPTY
                            },
                        ))
                        .await
                        .unwrap();
                }
                request => panic!("Unexpected request: {:?}", request),
            }
        };
        future::join(installer_fut, stream_fut).await;
        assert_eq!(
            *progresses.lock(),
            vec![
                Progress {
                    operation: Some("stage".to_string()),
                    progress: 0.5,
                    total_size: Some(1000),
                    size_so_far: Some(500)
                },
                Progress {
                    operation: Some("wait_to_reboot".to_string()),
                    progress: 1.0,
                    total_size: Some(1000),
                    size_so_far: Some(1000)
                }
            ]
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_install_error() {
        let (mut installer, mut stream) = new_mock_installer();
        let plan = FuchsiaInstallPlan {
            url: TEST_URL.parse().unwrap(),
            install_source: InstallSource::OnDemand,
            urgent_update: false,
        };
        let installer_fut = async move {
            assert_matches!(
                installer.perform_install(&plan, None).await,
                Err(FuchsiaInstallError::InstallerFailureState(InstallerFailure {
                    state_name: "fail_prepare",
                    reason: InstallerFailureReason::OutOfSpace
                }))
            );
        };
        let stream_fut = async move {
            match stream.next().await.unwrap() {
                Ok(InstallerRequest::StartUpdate { monitor, responder, .. }) => {
                    responder
                        .send(&mut Ok("00000000-0000-0000-0000-000000000002".to_owned()))
                        .unwrap();

                    let monitor = monitor.into_proxy().unwrap();
                    let () = monitor
                        .on_state(&mut State::FailPrepare(FailPrepareData {
                            reason: Some(
                                fidl_fuchsia_update_installer::PrepareFailureReason::OutOfSpace,
                            ),
                            ..FailPrepareData::EMPTY
                        }))
                        .await
                        .unwrap();
                }
                request => panic!("Unexpected request: {:?}", request),
            }
        };
        future::join(installer_fut, stream_fut).await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_server_close_unexpectedly() {
        let (mut installer, mut stream) = new_mock_installer();
        let plan = FuchsiaInstallPlan {
            url: TEST_URL.parse().unwrap(),
            install_source: InstallSource::OnDemand,
            urgent_update: false,
        };
        let installer_fut = async move {
            assert_matches!(
                installer.perform_install(&plan, None).await,
                Err(FuchsiaInstallError::InstallationEndedUnexpectedly)
            );
        };
        let stream_fut = async move {
            match stream.next().await.unwrap() {
                Ok(InstallerRequest::StartUpdate { monitor, responder, .. }) => {
                    responder
                        .send(&mut Ok("00000000-0000-0000-0000-000000000003".to_owned()))
                        .unwrap();

                    let monitor = monitor.into_proxy().unwrap();
                    let () = monitor
                        .on_state(&mut State::Prepare(
                            fidl_fuchsia_update_installer::PrepareData::EMPTY,
                        ))
                        .await
                        .unwrap();
                    let () = monitor
                        .on_state(&mut State::Fetch(fidl_fuchsia_update_installer::FetchData {
                            info: Some(UpdateInfo { download_size: None, ..UpdateInfo::EMPTY }),
                            progress: Some(InstallationProgress {
                                fraction_completed: Some(0.0),
                                bytes_downloaded: None,
                                ..InstallationProgress::EMPTY
                            }),
                            ..fidl_fuchsia_update_installer::FetchData::EMPTY
                        }))
                        .await
                        .unwrap();
                }
                request => panic!("Unexpected request: {:?}", request),
            }
        };
        future::join(installer_fut, stream_fut).await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_connect_to_installer_failed() {
        let mut installer =
            FuchsiaInstaller { connector: MockConnector::failing(), reboot_controller: None };
        let plan = FuchsiaInstallPlan {
            url: TEST_URL.parse().unwrap(),
            install_source: InstallSource::OnDemand,
            urgent_update: false,
        };
        assert_matches!(
            installer.perform_install(&plan, None).await,
            Err(FuchsiaInstallError::Connect(_))
        );
    }

    #[test]
    fn test_reboot() {
        let mut exec = fasync::Executor::new().unwrap();

        let (mut installer, _) = new_mock_installer();
        let (reboot_controller, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<RebootControllerMarker>().unwrap();
        installer.reboot_controller = Some(reboot_controller);

        {
            let mut reboot_future = installer.perform_reboot();
            assert_matches!(exec.run_until_stalled(&mut reboot_future), Poll::Pending);
            assert_matches!(exec.wake_next_timer(), Some(_));
            assert_matches!(exec.run_until_stalled(&mut reboot_future), Poll::Ready(Err(_)));
        }

        assert_matches!(installer.reboot_controller, None);
        assert_matches!(
            exec.run_singlethreaded(stream.next()),
            Some(Ok(RebootControllerRequest::Unblock { .. }))
        );
        assert_matches!(exec.run_singlethreaded(stream.next()), None);
    }
}
