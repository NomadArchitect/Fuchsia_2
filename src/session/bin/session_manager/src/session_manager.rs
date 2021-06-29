// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::startup,
    anyhow::{Context as _, Error},
    fidl_fuchsia_component as fcomponent, fidl_fuchsia_element as felement,
    fidl_fuchsia_input_injection::{
        InputDeviceRegistryMarker, InputDeviceRegistryProxy, InputDeviceRegistryRequest,
        InputDeviceRegistryRequestStream,
    },
    fidl_fuchsia_session::{
        ElementManagerMarker, ElementManagerProxy, ElementManagerRequest,
        ElementManagerRequestStream, LaunchConfiguration, LaunchError, LauncherRequest,
        LauncherRequestStream, RestartError, RestarterRequest, RestarterRequestStream,
    },
    fidl_fuchsia_sessionmanager::StartupRequestStream,
    fidl_fuchsia_sys2 as fsys,
    fuchsia_component::server::ServiceFs,
    fuchsia_zircon as zx,
    futures::{lock::Mutex, StreamExt, TryStreamExt},
    std::sync::Arc,
};

/// The services exposed by the session manager.
enum ExposedServices {
    Manager(felement::ManagerRequestStream),
    ElementManager(ElementManagerRequestStream),
    Launcher(LauncherRequestStream),
    Restarter(RestarterRequestStream),
    InputDeviceRegistry(InputDeviceRegistryRequestStream),
    Startup(StartupRequestStream),
}

struct SessionManagerState {
    /// The URL of the most recently launched session.
    ///
    /// If set, the session is not guaranteed to be running.
    session_url: Option<String>,

    /// A client-end channel to the most recently launched session's `exposed_dir`.
    ///
    /// If set, the session is not guaranteed to be running, and the channel is not
    /// guaranteed to be connected.
    session_exposed_dir_channel: Option<zx::Channel>,

    /// The realm in which sessions will be launched.
    realm: fsys::RealmProxy,
}

/// Manages the session lifecycle and provides services to control the session.
#[derive(Clone)]
pub struct SessionManager {
    state: Arc<Mutex<SessionManagerState>>,
}

impl SessionManager {
    /// Constructs a new SessionManager.
    ///
    /// # Parameters
    /// - `realm`: The realm in which sessions will be launched.
    pub fn new(realm: fsys::RealmProxy) -> Self {
        let state =
            SessionManagerState { session_url: None, session_exposed_dir_channel: None, realm };
        SessionManager { state: Arc::new(Mutex::new(state)) }
    }

    /// Launch the session specified in the session manager startup configuration, if any.
    ///
    /// # Errors
    /// Returns an error if the session could not be launched.
    pub async fn launch_startup_session(&mut self) -> Result<(), Error> {
        if let Some(session_url) = startup::get_session_url() {
            let mut state = self.state.lock().await;
            state.session_exposed_dir_channel =
                Some(startup::launch_session(&session_url, &state.realm).await?);
            state.session_url = Some(session_url);
        }
        Ok(())
    }

    /// Starts serving [`ExposedServices`] from `svc`.
    ///
    /// This will return once the [`ServiceFs`] stops serving requests.
    ///
    /// # Errors
    /// Returns an error if there is an issue serving the `svc` directory handle.
    pub async fn expose_services(&mut self) -> Result<(), Error> {
        let mut fs = ServiceFs::new_local();
        fs.dir("svc")
            .add_fidl_service(ExposedServices::Manager)
            .add_fidl_service(ExposedServices::ElementManager)
            .add_fidl_service(ExposedServices::Launcher)
            .add_fidl_service(ExposedServices::Restarter)
            .add_fidl_service(ExposedServices::InputDeviceRegistry)
            .add_fidl_service(ExposedServices::Startup);
        fs.take_and_serve_directory_handle()?;

        while let Some(service_request) = fs.next().await {
            match service_request {
                ExposedServices::Manager(request_stream) => {
                    // Connect to element.Manager served by the session.
                    let (manager_proxy, server_end) =
                        fidl::endpoints::create_proxy::<felement::ManagerMarker>()
                            .expect("Failed to create ManagerProxy");
                    {
                        let state = self.state.lock().await;
                        let session_exposed_dir_channel =
                            state.session_exposed_dir_channel.as_ref().expect(
                                "Failed to connect to ManagerProxy because no session was started",
                            );
                        fdio::service_connect_at(
                            session_exposed_dir_channel,
                            "fuchsia.element.Manager",
                            server_end.into_channel(),
                        )
                        .expect("Failed to connect to Manager service");
                    }
                    SessionManager::handle_manager_request_stream(request_stream, manager_proxy)
                        .await
                        .expect("Manager request stream got an error.");
                }
                ExposedServices::ElementManager(request_stream) => {
                    // Connect to ElementManager served by the session.
                    let (element_manager_proxy, server_end) =
                        fidl::endpoints::create_proxy::<ElementManagerMarker>()
                            .expect("Failed to create ElementManagerProxy");
                    {
                        let state = self.state.lock().await;
                        let session_exposed_dir_channel = state.session_exposed_dir_channel.as_ref()
                            .expect("Failed to connect to ElementManagerProxy because no session was started");
                        fdio::service_connect_at(
                            session_exposed_dir_channel,
                            "fuchsia.session.ElementManager",
                            server_end.into_channel(),
                        )
                        .expect("Failed to connect to ElementManager service");
                    }
                    SessionManager::handle_element_manager_request_stream(
                        request_stream,
                        element_manager_proxy,
                    )
                    .await
                    .expect("Element Manager request stream got an error.");
                }
                ExposedServices::Launcher(request_stream) => {
                    self.handle_launcher_request_stream(request_stream)
                        .await
                        .expect("Session Launcher request stream got an error.");
                }
                ExposedServices::Restarter(request_stream) => {
                    self.handle_restarter_request_stream(request_stream)
                        .await
                        .expect("Session Restarter request stream got an error.");
                }
                ExposedServices::InputDeviceRegistry(request_stream) => {
                    // Connect to InputDeviceRegistry served by the session.
                    let (input_device_registry_proxy, server_end) =
                        fidl::endpoints::create_proxy::<InputDeviceRegistryMarker>()
                            .expect("Failed to create InputDeviceRegistryProxy");
                    {
                        let state = self.state.lock().await;
                        let session_exposed_dir_channel = state.session_exposed_dir_channel.as_ref()
                            .expect("Failed to connect to InputDeviceRegistryProxy because no session was started");
                        fdio::service_connect_at(
                            session_exposed_dir_channel,
                            "fuchsia.input.injection.InputDeviceRegistry",
                            server_end.into_channel(),
                        )
                        .expect("Failed to connect to InputDeviceRegistry service");
                    }

                    SessionManager::handle_input_device_registry_request_stream(
                        request_stream,
                        input_device_registry_proxy,
                    )
                    .await
                    .expect("Input device registry request stream got an error.");
                }
                ExposedServices::Startup(request_stream) => {
                    self.handle_startup_request_stream(request_stream)
                        .await
                        .expect("Sessionmanager Startup request stream got an error.");
                }
            }
        }

        Ok(())
    }

    /// Serves a specified [`ManagerRequestStream`].
    ///
    /// # Parameters
    /// - `request_stream`: the ManagerRequestStream.
    /// - `manager_proxy`: the ManagerProxy that will handle the relayed commands.
    ///
    /// # Errors
    /// When an error is encountered reading from the request stream.
    pub async fn handle_manager_request_stream(
        mut request_stream: felement::ManagerRequestStream,
        manager_proxy: felement::ManagerProxy,
    ) -> Result<(), Error> {
        while let Some(request) =
            request_stream.try_next().await.context("Error handling Manager request stream")?
        {
            match request {
                felement::ManagerRequest::ProposeElement { spec, controller, responder } => {
                    let mut result = manager_proxy.propose_element(spec, controller).await?;
                    responder.send(&mut result)?;
                }
            };
        }
        Ok(())
    }

    /// Serves a specified [`ElementManagerRequestStream`].
    ///
    /// # Parameters
    /// - `request_stream`: the ElementManagerRequestStream.
    /// - `element_manager_proxy`: the ElementManagerProxy that will handle the relayed commands.
    ///
    /// # Errors
    /// When an error is encountered reading from the request stream.
    pub async fn handle_element_manager_request_stream(
        mut request_stream: ElementManagerRequestStream,
        element_manager_proxy: ElementManagerProxy,
    ) -> Result<(), Error> {
        while let Some(request) = request_stream
            .try_next()
            .await
            .context("Error handling Element Manager request stream")?
        {
            match request {
                ElementManagerRequest::ProposeElement { spec, element_controller, responder } => {
                    let mut result =
                        element_manager_proxy.propose_element(spec, element_controller).await?;
                    responder.send(&mut result)?;
                }
            };
        }
        Ok(())
    }

    /// Serves a specified [`LauncherRequestStream`].
    ///
    /// # Parameters
    /// - `request_stream`: the LauncherRequestStream.
    ///
    /// # Errors
    /// When an error is encountered reading from the request stream.
    pub async fn handle_launcher_request_stream(
        &mut self,
        mut request_stream: LauncherRequestStream,
    ) -> Result<(), Error> {
        while let Some(request) =
            request_stream.try_next().await.context("Error handling Launcher request stream")?
        {
            match request {
                LauncherRequest::Launch { configuration, responder } => {
                    let mut result = self.handle_launch_request(configuration).await;
                    let _ = responder.send(&mut result);
                }
            };
        }
        Ok(())
    }

    pub async fn handle_startup_request_stream(
        &mut self,
        mut request_stream: StartupRequestStream,
    ) -> Result<(), Error> {
        while let Some(request) =
            request_stream.try_next().await.context("Error handling Startup request stream")?
        {
            match request {
                _ => {
                    // No-op
                    fuchsia_syslog::fx_log_info!("Received startup request.");
                }
            };
        }
        Ok(())
    }

    /// Serves a specified [`RestarterRequestStream`].
    ///
    /// # Parameters
    /// - `request_stream`: the RestarterRequestStream.
    ///
    /// # Errors
    /// When an error is encountered reading from the request stream.
    pub async fn handle_restarter_request_stream(
        &mut self,
        mut request_stream: RestarterRequestStream,
    ) -> Result<(), Error> {
        while let Some(request) =
            request_stream.try_next().await.context("Error handling Restarter request stream")?
        {
            match request {
                RestarterRequest::Restart { responder } => {
                    let mut result = self.handle_restart_request().await;
                    let _ = responder.send(&mut result);
                }
            };
        }
        Ok(())
    }

    /// Serves a specified [`InputDeviceRegistryRequestStream`].
    ///
    /// # Parameters
    /// - `request_stream`: the InputDeviceRegistryRequestStream.
    /// - `input_device_registry_proxy`: the downstream InputDeviceRegistryProxy
    ///   to which requests will be relayed.
    ///
    /// # Errors
    /// When an error is encountered reading from the request stream.
    pub async fn handle_input_device_registry_request_stream(
        mut request_stream: InputDeviceRegistryRequestStream,
        input_device_registry_proxy: InputDeviceRegistryProxy,
    ) -> Result<(), Error> {
        while let Some(request) = request_stream
            .try_next()
            .await
            .context("Error handling input device registry request stream")?
        {
            match request {
                InputDeviceRegistryRequest::Register { device, .. } => {
                    input_device_registry_proxy
                        .register(device)
                        .context("Error handling InputDeviceRegistryRequest::Register")?;
                }
            }
        }
        Ok(())
    }

    /// Handles calls to Launcher.Launch().
    ///
    /// # Parameters
    /// - configuration: The launch configuration for the new session.
    async fn handle_launch_request(
        &mut self,
        configuration: LaunchConfiguration,
    ) -> Result<(), LaunchError> {
        if let Some(session_url) = configuration.session_url {
            let mut state = self.state.lock().await;
            startup::launch_session(&session_url, &state.realm)
                .await
                .map_err(|err| match err {
                    startup::StartupError::NotDestroyed { .. } => {
                        LaunchError::DestroyComponentFailed
                    }
                    startup::StartupError::NotCreated { err, .. } => match err {
                        fcomponent::Error::InstanceCannotResolve => LaunchError::NotFound,
                        _ => LaunchError::CreateComponentFailed,
                    },
                    startup::StartupError::NotBound { .. } => LaunchError::CreateComponentFailed,
                })
                .map(|session_exposed_dir_channel| {
                    state.session_url = Some(session_url);
                    state.session_exposed_dir_channel = Some(session_exposed_dir_channel);
                })
        } else {
            Err(LaunchError::NotFound)
        }
    }

    /// Handles calls to Restarter.Restart().
    async fn handle_restart_request(&mut self) -> Result<(), RestartError> {
        let mut state = self.state.lock().await;
        if let Some(ref session_url) = state.session_url {
            startup::launch_session(&session_url, &state.realm)
                .await
                .map_err(|err| match err {
                    startup::StartupError::NotDestroyed { .. } => {
                        RestartError::DestroyComponentFailed
                    }
                    startup::StartupError::NotCreated { err, .. } => match err {
                        fcomponent::Error::InstanceCannotResolve => RestartError::NotFound,
                        _ => RestartError::CreateComponentFailed,
                    },
                    startup::StartupError::NotBound { .. } => RestartError::CreateComponentFailed,
                })
                .map(|session_exposed_dir_channel| {
                    state.session_exposed_dir_channel = Some(session_exposed_dir_channel);
                })
        } else {
            Err(RestartError::NotRunning)
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::SessionManager,
        fidl::encoding::Decodable,
        fidl::endpoints::{create_endpoints, create_proxy_and_stream, spawn_stream_handler},
        fidl_fuchsia_input_injection::{InputDeviceRegistryMarker, InputDeviceRegistryRequest},
        fidl_fuchsia_input_report::InputDeviceMarker,
        fidl_fuchsia_session::{
            ElementManagerMarker, ElementManagerRequest, ElementSpec, LaunchConfiguration,
            LauncherMarker, LauncherProxy, RestartError, RestarterMarker, RestarterProxy,
        },
        fidl_fuchsia_sys2 as fsys, fuchsia_async as fasync,
        futures::prelude::*,
        matches::assert_matches,
    };

    fn serve_session_manager_services(
        session_manager: SessionManager,
    ) -> (LauncherProxy, RestarterProxy) {
        let (launcher_proxy, launcher_stream) =
            create_proxy_and_stream::<LauncherMarker>().unwrap();
        {
            let mut session_manager_ = session_manager.clone();
            fuchsia_async::Task::spawn(async move {
                session_manager_
                    .handle_launcher_request_stream(launcher_stream)
                    .await
                    .expect("Session launcher request stream got an error.");
            })
            .detach();
        }

        let (restarter_proxy, restarter_stream) =
            create_proxy_and_stream::<RestarterMarker>().unwrap();
        {
            let mut session_manager_ = session_manager.clone();
            fuchsia_async::Task::spawn(async move {
                session_manager_
                    .handle_restarter_request_stream(restarter_stream)
                    .await
                    .expect("Session restarter request stream got an error.");
            })
            .detach();
        }

        (launcher_proxy, restarter_proxy)
    }

    /// Verifies that Launcher.Launch creates a new session.
    #[fasync::run_until_stalled(test)]
    async fn test_launch() {
        let session_url = "session";

        let realm = spawn_stream_handler(move |realm_request| async move {
            match realm_request {
                fsys::RealmRequest::DestroyChild { child: _, responder } => {
                    let _ = responder.send(&mut Ok(()));
                }
                fsys::RealmRequest::CreateChild { collection: _, decl, args: _, responder } => {
                    assert_eq!(decl.url.unwrap(), session_url);
                    let _ = responder.send(&mut Ok(()));
                }
                fsys::RealmRequest::BindChild { child: _, exposed_dir: _, responder } => {
                    let _ = responder.send(&mut Ok(()));
                }
                _ => panic!("Realm handler received an unexpected request"),
            };
        })
        .unwrap();

        let session_manager = SessionManager::new(realm);
        let (launcher, _restarter) = serve_session_manager_services(session_manager);

        assert!(launcher
            .launch(LaunchConfiguration {
                session_url: Some(session_url.to_string()),
                ..LaunchConfiguration::EMPTY
            })
            .await
            .is_ok());
    }

    /// Verifies that Launcher.Restart restarts an existing session.
    #[fasync::run_until_stalled(test)]
    async fn test_restart() {
        let session_url = "session";

        let realm = spawn_stream_handler(move |realm_request| async move {
            match realm_request {
                fsys::RealmRequest::DestroyChild { child: _, responder } => {
                    let _ = responder.send(&mut Ok(()));
                }
                fsys::RealmRequest::CreateChild { collection: _, decl, args: _, responder } => {
                    assert_eq!(decl.url.unwrap(), session_url);
                    let _ = responder.send(&mut Ok(()));
                }
                fsys::RealmRequest::BindChild { child: _, exposed_dir: _, responder } => {
                    let _ = responder.send(&mut Ok(()));
                }
                _ => panic!("Realm handler received an unexpected request"),
            };
        })
        .unwrap();

        let session_manager = SessionManager::new(realm);
        let (launcher, restarter) = serve_session_manager_services(session_manager);

        assert!(launcher
            .launch(LaunchConfiguration {
                session_url: Some(session_url.to_string()),
                ..LaunchConfiguration::EMPTY
            })
            .await
            .expect("could not call Launch")
            .is_ok());

        assert!(restarter.restart().await.expect("could not call Restart").is_ok());
    }

    /// Verifies that Launcher.Restart return an error if there is no running existing session.
    #[fasync::run_until_stalled(test)]
    async fn test_restart_error_not_running() {
        let realm = spawn_stream_handler(move |_realm_request| async move {
            panic!("Realm should not receive any requests as there is no session to launch")
        })
        .unwrap();

        let session_manager = SessionManager::new(realm);
        let (_launcher, restarter) = serve_session_manager_services(session_manager);

        assert_eq!(
            Err(RestartError::NotRunning),
            restarter.restart().await.expect("could not call Restart")
        );
    }

    #[fasync::run_until_stalled(test)]
    async fn handle_input_device_registry_request_stream_propagates_request_to_downstream_service()
    {
        let (local_proxy, local_request_stream) =
            create_proxy_and_stream::<InputDeviceRegistryMarker>()
                .expect("Failed to create local InputDeviceRegistry proxy and stream");
        let (downstream_proxy, mut downstream_request_stream) =
            create_proxy_and_stream::<InputDeviceRegistryMarker>()
                .expect("Failed to create downstream InputDeviceRegistry proxy and stream");
        let mut num_devices_registered = 0;

        let local_server_fut = SessionManager::handle_input_device_registry_request_stream(
            local_request_stream,
            downstream_proxy,
        );
        let downstream_server_fut = async {
            while let Some(request) = downstream_request_stream.try_next().await.unwrap() {
                match request {
                    InputDeviceRegistryRequest::Register { .. } => num_devices_registered += 1,
                }
            }
        };

        let (input_device_client, _input_device_server) = create_endpoints::<InputDeviceMarker>()
            .expect("Failed to create InputDevice endpoints");
        local_proxy
            .register(input_device_client)
            .expect("Failed to send registration request locally");
        std::mem::drop(local_proxy); // Drop proxy to terminate `server_fut`.

        assert_matches!(local_server_fut.await, Ok(()));
        downstream_server_fut.await;
        assert_eq!(num_devices_registered, 1);
    }

    #[fasync::run_until_stalled(test)]
    async fn handle_element_manager_request_stream_propagates_request_to_downstream_service() {
        let (local_proxy, local_request_stream) = create_proxy_and_stream::<ElementManagerMarker>()
            .expect("Failed to create local ElementManager proxy and stream");

        let (downstream_proxy, mut downstream_request_stream) =
            create_proxy_and_stream::<ElementManagerMarker>()
                .expect("Failed to create downstream ElementManager proxy and stream");

        let element_url = "element_url";
        let mut num_elements_proposed = 0;

        let local_server_fut = SessionManager::handle_element_manager_request_stream(
            local_request_stream,
            downstream_proxy,
        );

        let downstream_server_fut = async {
            while let Some(request) = downstream_request_stream.try_next().await.unwrap() {
                match request {
                    ElementManagerRequest::ProposeElement { spec, responder, .. } => {
                        num_elements_proposed += 1;
                        assert_eq!(Some(element_url.to_string()), spec.component_url);
                        let _ = responder.send(&mut Ok(()));
                    }
                }
            }
        };

        let propose_and_drop_fut = async {
            local_proxy
                .propose_element(
                    ElementSpec {
                        component_url: Some(element_url.to_string()),
                        ..ElementSpec::new_empty()
                    },
                    None,
                )
                .await
                .expect("Failed to propose element")
                .expect("Failed to propose element");

            std::mem::drop(local_proxy); // Drop proxy to terminate `server_fut`.
        };

        let _ = future::join3(propose_and_drop_fut, local_server_fut, downstream_server_fut).await;

        assert_eq!(num_elements_proposed, 1);
    }
}
