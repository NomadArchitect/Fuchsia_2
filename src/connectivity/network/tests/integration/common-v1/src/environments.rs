// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides utilities for test environments.

use fidl_fuchsia_net_filter;
use fidl_fuchsia_net_stack as net_stack;
use fidl_fuchsia_netemul_environment as netemul_environment;

use anyhow::Context as _;
use async_trait::async_trait;

use crate::Result;

/// Helper definition to help type system identify `None` as `IntoIterator`
/// where `Item: Into<netemul_environment::LaunchService`.
const NO_SERVICES: Option<netemul_environment::LaunchService> = None;

/// The Netstack version. Used to specify which Netstack version to use in a
/// Netstack-served [`KnownServices`].
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[allow(missing_docs)]
pub enum NetstackVersion {
    Netstack2,
    Netstack3,
}

impl NetstackVersion {
    /// Gets the Fuchsia URL for this Netstack component.
    pub fn get_url(&self) -> &'static str {
        match self {
            NetstackVersion::Netstack2 => {
                "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/netstack-debug.cmx"
            }
            NetstackVersion::Netstack3 => {
                "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/netstack3.cmx"
            }
        }
    }

    /// Gets the Netstack name.
    pub fn get_name(&self) -> &'static str {
        match self {
            NetstackVersion::Netstack2 => "ns2",
            NetstackVersion::Netstack3 => "ns3",
        }
    }
}

/// Known services used in tests.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[allow(missing_docs)]
pub enum KnownServices {
    Stack(NetstackVersion),
    Netstack(NetstackVersion),
    SocketProvider(NetstackVersion),
    Filter(NetstackVersion),
    RoutesState(NetstackVersion),
    InterfaceState(NetstackVersion),
    Log(NetstackVersion),
    NeighborView(NetstackVersion),
    NeighborController(NetstackVersion),
    MockCobalt,
    SecureStash,
    DhcpServer,
    Dhcpv6Client,
    LookupAdmin,
}

impl KnownServices {
    /// Gets the service name and its Fuchsia package URL.
    pub fn get_name_url(&self) -> (&'static str, &'static str) {
        match self {
            KnownServices::Stack(v) => (<net_stack::StackMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                        v.get_url()),
            KnownServices::MockCobalt => (<fidl_fuchsia_cobalt::LoggerFactoryMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                          "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/mock_cobalt.cmx"),
            KnownServices::Netstack(v) => (<fidl_fuchsia_netstack::NetstackMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                           v.get_url()),
            KnownServices::SocketProvider(v) => (<fidl_fuchsia_posix_socket::ProviderMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                                 v.get_url()),
            KnownServices::Filter(v) => (<fidl_fuchsia_net_filter::FilterMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                         v.get_url()),
            KnownServices::RoutesState(v) => (<fidl_fuchsia_net_routes::StateMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                              v.get_url()),
            KnownServices::InterfaceState(v) => (<fidl_fuchsia_net_interfaces::StateMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                                 v.get_url()),
            KnownServices::Log(v) => (<fidl_fuchsia_net_stack::LogMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                      v.get_url()),
            KnownServices::NeighborView(v) => (<fidl_fuchsia_net_neighbor::ViewMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                               v.get_url()),
            KnownServices::NeighborController(v) => (<fidl_fuchsia_net_neighbor::ControllerMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                                     v.get_url()),
            KnownServices::SecureStash => (<fidl_fuchsia_stash::SecureStoreMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                           "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/stash_secure.cmx"),
            KnownServices::DhcpServer => (<fidl_fuchsia_net_dhcp::Server_Marker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                          "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/dhcpd.cmx"),
            KnownServices::Dhcpv6Client => (<fidl_fuchsia_net_dhcpv6::ClientProviderMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                            "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/dhcpv6-client.cmx"),
            KnownServices::LookupAdmin => (<fidl_fuchsia_net_name::LookupAdminMarker as fidl::endpoints::DiscoverableService>::SERVICE_NAME,
                                           "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/dns-resolver.cmx")
        }
    }

    /// Gets the service name.
    pub fn get_name(&self) -> &'static str {
        self.get_name_url().0
    }

    /// Gets the service URL.
    pub fn get_url(&self) -> &'static str {
        self.get_name_url().1
    }

    /// Transforms into a [`netemul_environment::LaunchService`] with no
    /// arguments.
    pub fn into_launch_service(self) -> netemul_environment::LaunchService {
        let (name, url) = self.get_name_url();
        netemul_environment::LaunchService {
            name: name.to_string(),
            url: url.to_string(),
            arguments: vec![],
        }
    }
}

impl<'a> From<&'a KnownServices> for netemul_environment::LaunchService {
    fn from(s: &'a KnownServices) -> Self {
        s.into_launch_service()
    }
}

/// Abstraction for a Fuchsia component which offers network stack services.
pub trait Netstack: Copy + Clone {
    /// The Netstack version.
    const VERSION: NetstackVersion;
}

/// Uninstantiable type that represents Netstack2's implementation of a
/// network stack.
#[derive(Copy, Clone)]
pub enum Netstack2 {}

impl Netstack for Netstack2 {
    const VERSION: NetstackVersion = NetstackVersion::Netstack2;
}

/// Uninstantiable type that represents Netstack3's implementation of a
/// network stack.
#[derive(Copy, Clone)]
pub enum Netstack3 {}

impl Netstack for Netstack3 {
    const VERSION: NetstackVersion = NetstackVersion::Netstack3;
}

/// Abstraction for a Fuchsia component which offers network configuration services.
pub trait Manager: Copy + Clone {
    /// The Fuchsia package URL to the component.
    const PKG_URL: &'static str;

    /// Default arguments that should be passed to the component when run under a test
    /// environment.
    const TESTING_ARGS: &'static [&'static str];

    /// Returns `TESTING_ARGS` as a type that [`fuchsia_component::client::launch`]
    /// accepts.
    fn testing_args() -> Option<Vec<String>> {
        Some(Self::TESTING_ARGS.iter().cloned().map(String::from).collect())
    }
}

/// Uninstantiable type that represents NetCfg's implementation of a network manager.
#[derive(Copy, Clone)]
pub enum NetCfg {}

impl Manager for NetCfg {
    // Note, netcfg.cmx must never be used in a Netemul environment as it breaks
    // hermeticity.
    const PKG_URL: &'static str =
        "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/netcfg-netemul.cmx";
    // Specify an empty config file for NetCfg when it is run in netemul.
    const TESTING_ARGS: &'static [&'static str] = &["--config-data", "netcfg/empty.json"];
}

/// Uninstantiable type that represents NetworkManager's implementation of a network manager.
#[derive(Copy, Clone)]
pub enum NetworkManager {}

impl Manager for NetworkManager {
    // Note, network-manager.cmx must never be used in a Netemul environment as it breaks
    // hermeticity.
    const PKG_URL: &'static str =
        "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/network-manager-netemul.cmx";
    const TESTING_ARGS: &'static [&'static str] = &[];
}

/// Abstraction for a Fuchsia component which monitors reachability status.
pub trait Reachability: Copy + Clone {
    /// The Fuchsia package URL to the component.
    const PKG_URL: &'static str;
}

/// Uninstantiable type that represents a reachability monitor.
#[derive(Copy, Clone)]
pub enum ReachabilityMonitor {}

impl Reachability for ReachabilityMonitor {
    const PKG_URL: &'static str =
        "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/reachability.cmx";
}

/// Extensions to `netemul::TestSandbox`.
#[async_trait]
pub trait TestSandboxExt {
    /// Creates an environment with Netstack services.
    fn create_netstack_environment<N, S>(&self, name: S) -> Result<netemul::TestEnvironment<'_>>
    where
        N: Netstack,
        S: Into<String>;

    /// Creates an environment with the base Netstack services plus additional
    /// ones in `services`.
    fn create_netstack_environment_with<N, S, I>(
        &self,
        name: S,
        services: I,
    ) -> Result<netemul::TestEnvironment<'_>>
    where
        S: Into<String>,
        N: Netstack,
        I: IntoIterator,
        I::Item: Into<netemul_environment::LaunchService>;

    /// Helper function to create a new Netstack environment and connect to a
    /// netstack service on it.
    fn new_netstack<N, S, T>(&self, name: T) -> Result<(netemul::TestEnvironment<'_>, S::Proxy)>
    where
        N: Netstack,
        S: fidl::endpoints::ServiceMarker + fidl::endpoints::DiscoverableService,
        T: Into<String>;

    /// Helper function to create a new Netstack environment and a new
    /// unattached endpoint.
    async fn new_netstack_and_device<N, E, S, T>(
        &self,
        name: T,
    ) -> Result<(netemul::TestEnvironment<'_>, S::Proxy, netemul::TestEndpoint<'_>)>
    where
        N: Netstack,
        E: netemul::Endpoint,
        S: fidl::endpoints::ServiceMarker + fidl::endpoints::DiscoverableService,
        T: Into<String> + Copy + Send;
}

#[async_trait]
impl TestSandboxExt for netemul::TestSandbox {
    /// Creates an environment with Netstack services.
    fn create_netstack_environment<N, S>(&self, name: S) -> Result<netemul::TestEnvironment<'_>>
    where
        N: Netstack,
        S: Into<String>,
    {
        self.create_netstack_environment_with::<N, _, _>(name, NO_SERVICES)
    }

    /// Creates an environment with the base Netstack services plus additional
    /// ones in `services`.
    fn create_netstack_environment_with<N, S, I>(
        &self,
        name: S,
        services: I,
    ) -> Result<netemul::TestEnvironment<'_>>
    where
        S: Into<String>,
        N: Netstack,
        I: IntoIterator,
        I::Item: Into<netemul_environment::LaunchService>,
    {
        self.create_environment(
            name,
            [
                KnownServices::RoutesState(N::VERSION),
                KnownServices::Filter(N::VERSION),
                KnownServices::Stack(N::VERSION),
                KnownServices::Netstack(N::VERSION),
                KnownServices::SocketProvider(N::VERSION),
                KnownServices::InterfaceState(N::VERSION),
                KnownServices::Log(N::VERSION),
                KnownServices::NeighborView(N::VERSION),
                KnownServices::NeighborController(N::VERSION),
                KnownServices::MockCobalt,
            ]
            .iter()
            .map(netemul_environment::LaunchService::from)
            .chain(services.into_iter().map(Into::into)),
        )
    }

    /// Helper function to create a new Netstack environment and connect to a
    /// netstack service on it.
    fn new_netstack<N, S, T>(&self, name: T) -> Result<(netemul::TestEnvironment<'_>, S::Proxy)>
    where
        N: Netstack,
        S: fidl::endpoints::ServiceMarker + fidl::endpoints::DiscoverableService,
        T: Into<String>,
    {
        let env = self
            .create_netstack_environment::<N, _>(name)
            .context("failed to create test environment")?;
        let netstack_proxy =
            env.connect_to_service::<S>().context("failed to connect to netstack")?;
        Ok((env, netstack_proxy))
    }

    /// Helper function to create a new Netstack environment and a new
    /// unattached endpoint.
    async fn new_netstack_and_device<N, E, S, T>(
        &self,
        name: T,
    ) -> Result<(netemul::TestEnvironment<'_>, S::Proxy, netemul::TestEndpoint<'_>)>
    where
        N: Netstack,
        E: netemul::Endpoint,
        S: fidl::endpoints::ServiceMarker + fidl::endpoints::DiscoverableService,
        T: Into<String> + Copy + Send,
    {
        let (env, stack) = self.new_netstack::<N, S, _>(name)?;
        let endpoint =
            self.create_endpoint::<E, _>(name).await.context("failed to create endpoint")?;
        Ok((env, stack, endpoint))
    }
}
