// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod error;
pub mod open;
pub mod providers;
pub mod service;
pub use ::routing::error::RoutingError;
pub use error::OpenResourceError;
pub use open::*;

use {
    crate::{
        capability::{CapabilityProvider, CapabilitySource},
        model::{
            component::{ComponentInstance, ExtendedInstance, WeakComponentInstance},
            error::ModelError,
            hooks::{Event, EventPayload},
            routing::{
                providers::{DefaultComponentCapabilityProvider, NamespaceCapabilityProvider},
                service::FilteredServiceProvider,
            },
            storage,
        },
    },
    ::routing::{
        capability_source::ComponentCapability, component_instance::ComponentInstanceInterface,
        error::AvailabilityRoutingError, route_capability, route_storage_and_backing_directory,
    },
    cm_moniker::{InstancedExtendedMoniker, InstancedRelativeMoniker},
    cm_rust::{ExposeDecl, UseDecl, UseStorageDecl},
    cm_util::channel,
    fidl::{endpoints::ServerEnd, epitaph::ChannelEpitaphExt},
    fidl_fuchsia_io as fio, fuchsia_zircon as zx,
    futures::lock::Mutex,
    log::Level,
    std::sync::Arc,
};

pub type RouteRequest = ::routing::RouteRequest;
pub type RouteSource = ::routing::RouteSource<ComponentInstance>;

/// Routes a capability from `target` to its source. Opens the capability if routing succeeds.
///
/// If the capability is not allowed to be routed to the `target`, per the
/// [`crate::model::policy::GlobalPolicyChecker`], the capability is not opened and an error
/// is returned.
pub(super) async fn route_and_open_capability(
    route_request: RouteRequest,
    target: &Arc<ComponentInstance>,
    open_options: OpenOptions<'_>,
) -> Result<(), ModelError> {
    match route_request {
        RouteRequest::UseStorage(use_storage_decl) => {
            let (storage_source_info, relative_moniker, _storage_route, _dir_route) =
                route_storage_and_backing_directory(use_storage_decl, target).await?;
            open_storage_capability(storage_source_info, relative_moniker, target, open_options)
                .await
        }
        _ => {
            let (route_source, _route) = route_capability(route_request, target).await?;
            open_capability_at_source(OpenRequest::new(route_source, target, open_options)).await
        }
    }
}

/// Routes a capability from `target` to its source.
///
/// If the capability is not allowed to be routed to the `target`, per the
/// [`crate::model::policy::GlobalPolicyChecker`], the capability is not opened and an error
/// is returned.
pub async fn route(
    route_request: RouteRequest,
    target: &Arc<ComponentInstance>,
) -> Result<(), RoutingError> {
    match route_request {
        RouteRequest::UseStorage(use_storage_decl) => {
            route_storage_and_backing_directory(use_storage_decl, target).await?;
        }
        _ => {
            route_capability(route_request, target).await?;
        }
    }
    Ok(())
}

/// Routes a capability from `target` to its source, starting from a `use_decl`.
///
/// If the capability is allowed to be routed to the `target`, per the
/// [`crate::model::policy::GlobalPolicyChecker`], the capability is then opened at its source
/// triggering a `CapabilityRouted` event.
///
/// See [`fidl_fuchsia_io::Directory::Open`] for how the `flags`, `open_mode`, `relative_path`,
/// and `server_chan` parameters are used in the open call.
///
/// Only capabilities that can be installed in a namespace are supported: Protocol, Service,
/// Directory, and Storage.
pub(super) async fn route_and_open_namespace_capability(
    flags: fio::OpenFlags,
    open_mode: u32,
    relative_path: String,
    use_decl: UseDecl,
    target: &Arc<ComponentInstance>,
    server_chan: &mut zx::Channel,
) -> Result<(), ModelError> {
    let route_request = request_for_namespace_capability_use(use_decl)?;
    let open_options = OpenOptions::for_namespace_capability(
        &route_request,
        flags,
        open_mode,
        relative_path,
        server_chan,
    )?;
    route_and_open_capability(route_request, target, open_options).await
}

/// Routes a capability from `target` to its source, starting from an `expose_decl`.
///
/// If the capability is allowed to be routed to the `target`, per the
/// [`crate::model::policy::GlobalPolicyChecker`], the capability is then opened at its source
/// triggering a `CapabilityRouted` event.
///
/// See [`fidl_fuchsia_io::Directory::Open`] for how the `flags`, `open_mode`, `relative_path`,
/// and `server_chan` parameters are used in the open call.
///
/// Only capabilities that can both be opened from a VFS and be exposed to their parent
/// are supported: Protocol, Service, and Directory.
pub(super) async fn route_and_open_namespace_capability_from_expose(
    flags: fio::OpenFlags,
    open_mode: u32,
    relative_path: String,
    expose_decl: ExposeDecl,
    target: &Arc<ComponentInstance>,
    server_chan: &mut zx::Channel,
) -> Result<(), ModelError> {
    let route_request = request_for_namespace_capability_expose(expose_decl)?;
    let open_options = OpenOptions::for_namespace_capability(
        &route_request,
        flags,
        open_mode,
        relative_path,
        server_chan,
    )?;
    route_and_open_capability(route_request, target, open_options).await
}

/// Create a new `RouteRequest` from a `UseDecl`, checking that the capability type can
/// be installed in a namespace.
pub fn request_for_namespace_capability_use(use_decl: UseDecl) -> Result<RouteRequest, ModelError> {
    match use_decl {
        UseDecl::Directory(decl) => Ok(RouteRequest::UseDirectory(decl)),
        UseDecl::Protocol(decl) => Ok(RouteRequest::UseProtocol(decl)),
        UseDecl::Service(decl) => Ok(RouteRequest::UseService(decl)),
        UseDecl::Storage(decl) => Ok(RouteRequest::UseStorage(decl)),
        _ => Err(ModelError::unsupported("capability cannot be installed in a namespace")),
    }
}

/// Create a new `RouteRequest` from an `ExposeDecl`, checking that the capability type can
/// be installed in a namespace.
pub fn request_for_namespace_capability_expose(
    expose_decl: ExposeDecl,
) -> Result<RouteRequest, ModelError> {
    match expose_decl {
        ExposeDecl::Directory(decl) => Ok(RouteRequest::ExposeDirectory(decl)),
        ExposeDecl::Protocol(decl) => Ok(RouteRequest::ExposeProtocol(decl)),
        ExposeDecl::Service(decl) => Ok(RouteRequest::ExposeService(decl)),
        _ => Err(ModelError::unsupported("capability cannot be installed in a namespace")),
    }
}

/// Returns an instance of the default capability provider for the capability at `source`, if supported.
async fn get_default_provider(
    target: WeakComponentInstance,
    source: &CapabilitySource,
) -> Result<Option<Box<dyn CapabilityProvider>>, ModelError> {
    match source {
        CapabilitySource::Component { capability, component } => {
            // Route normally for a component capability with a source path
            Ok(match capability.source_path() {
                Some(path) => Some(Box::new(DefaultComponentCapabilityProvider {
                    target,
                    source: component.clone(),
                    name: capability
                        .source_name()
                        .expect("capability with source path should have a name")
                        .clone(),
                    path: path.clone(),
                })),
                _ => None,
            })
        }
        CapabilitySource::Namespace { capability, .. } => match capability.source_path() {
            Some(path) => Ok(Some(Box::new(NamespaceCapabilityProvider { path: path.clone() }))),
            _ => Ok(None),
        },
        CapabilitySource::FilteredService {
            capability,
            component,
            source_instance_filter,
            instance_name_source_to_target,
        } => {
            // First get the base service capability provider
            match capability.source_path() {
                Some(path) => {
                    let base_capability_provider = Box::new(DefaultComponentCapabilityProvider {
                        target,
                        source: component.clone(),
                        name: capability
                            .source_name()
                            .expect("capability with source path should have a name")
                            .clone(),
                        path: path.clone(),
                    });

                    let source_component = component.upgrade()?;
                    let provider = FilteredServiceProvider::new(
                        &source_component,
                        source_instance_filter.clone(),
                        instance_name_source_to_target.clone(),
                        base_capability_provider,
                    )
                    .await?;
                    Ok(Some(Box::new(provider)))
                }
                _ => Ok(None),
            }
        }
        CapabilitySource::Framework { .. }
        | CapabilitySource::Capability { .. }
        | CapabilitySource::Builtin { .. }
        | CapabilitySource::Collection { .. } => {
            // There is no default provider for a framework or builtin capability
            Ok(None)
        }
    }
}

/// Opens the capability at `source`, triggering a `CapabilityRouted` event and binding
/// to the source component instance if necessary.
///
/// See [`fidl_fuchsia_io::Directory::Open`] for how the `flags`, `open_mode`, `relative_path`,
/// and `server_chan` parameters are used in the open call.
async fn open_capability_at_source(open_request: OpenRequest<'_>) -> Result<(), ModelError> {
    let OpenRequest { flags, open_mode, relative_path, source, target, server_chan } = open_request;

    let capability_provider =
        Arc::new(Mutex::new(get_default_provider(target.as_weak(), &source).await?));

    let event = Event::new(
        &target,
        Ok(EventPayload::CapabilityRouted {
            source: source.clone(),
            capability_provider: capability_provider.clone(),
        }),
    );

    // Get a capability provider from the tree
    target.hooks.dispatch(&event).await?;

    let capability_provider = capability_provider.lock().await.take();

    // If a hook in the component tree gave a capability provider, then use it.
    if let Some(capability_provider) = capability_provider {
        let source_instance = source.source_instance().upgrade()?;
        let task_scope = match source_instance {
            ExtendedInstance::AboveRoot(top) => top.task_scope(),
            ExtendedInstance::Component(component) => component.task_scope(),
        };
        capability_provider.open(task_scope, flags, open_mode, relative_path, server_chan).await?;
        Ok(())
    } else {
        match &source {
            CapabilitySource::Component { .. } => {
                unreachable!(
                    "Capability source is a component, which should have been caught by \
                    default_capability_provider: {:?}",
                    source
                );
            }
            CapabilitySource::FilteredService { .. } => {
                return Err(ModelError::unsupported("filtered service"));
            }
            CapabilitySource::Framework { capability, component } => {
                return Err(RoutingError::capability_from_framework_not_found(
                    &component.abs_moniker,
                    capability.source_name().to_string(),
                )
                .into());
            }
            CapabilitySource::Capability { source_capability, component } => {
                return Err(RoutingError::capability_from_capability_not_found(
                    &component.abs_moniker,
                    source_capability.to_string(),
                )
                .into());
            }
            CapabilitySource::Builtin { capability, .. } => {
                return Err(ModelError::from(
                    RoutingError::capability_from_component_manager_not_found(
                        capability.source_name().to_string(),
                    ),
                ));
            }
            CapabilitySource::Namespace { capability, .. } => {
                return Err(ModelError::from(
                    RoutingError::capability_from_component_manager_not_found(
                        capability.source_id(),
                    ),
                ));
            }
            CapabilitySource::Collection { .. } => {
                return Err(ModelError::unsupported("collections"));
            }
        };
    }
}

/// Routes a storage capability from `target` to its source and deletes its isolated storage.
pub(super) async fn route_and_delete_storage(
    use_storage_decl: UseStorageDecl,
    target: &Arc<ComponentInstance>,
) -> Result<(), ModelError> {
    let (storage_source_info, relative_moniker, _storage_route, _dir_route) =
        route_storage_and_backing_directory(use_storage_decl, target).await?;

    storage::delete_isolated_storage(
        storage_source_info,
        target.persistent_storage,
        relative_moniker,
        target.instance_id().as_ref(),
    )
    .await
}

static ROUTE_ERROR_HELP: &'static str = "To learn more, see \
https://fuchsia.dev/go/components/connect-errors";

/// Sets an epitaph on `server_end` for a capability routing failure, and logs the error. Logs a
/// failure to route a capability. Formats `err` as a `String`, but elides the type if the error is
/// a `RoutingError`, the common case.
pub async fn report_routing_failure(
    target: &Arc<ComponentInstance>,
    cap: &ComponentCapability,
    err: &ModelError,
    server_end: zx::Channel,
) {
    server_end
        .close_with_epitaph(err.as_zx_status())
        .unwrap_or_else(|e| log::debug!("failed to send epitaph: {}", e));
    let err_str = match err {
        ModelError::RoutingError { err } => err.to_string(),
        _ => err.to_string(),
    };
    let log_level = match err {
        // If the route failed because the capability is intentionally not provided, then this
        // failure is expected and the warn level is unwarranted, so use the debug level in this
        // case.
        ModelError::RoutingError {
            err:
                RoutingError::AvailabilityRoutingError(
                    AvailabilityRoutingError::OfferFromVoidToOptionalTarget,
                ),
        } => Level::Debug,
        _ => Level::Warn,
    };
    target
        .with_logger_as_default(|| {
            if log_level == Level::Debug {
                log::debug!(
                    "Failed to route {} `{}` with target component `{}`: {}\n{}",
                    cap.type_name(),
                    cap.source_id(),
                    &target.abs_moniker,
                    &err_str,
                    ROUTE_ERROR_HELP
                );
            } else {
                log::warn!(
                    "Failed to route {} `{}` with target component `{}`: {}\n{}",
                    cap.type_name(),
                    cap.source_id(),
                    &target.abs_moniker,
                    &err_str,
                    ROUTE_ERROR_HELP
                );
            }
        })
        .await
}

/// Routes a storage capability from `target` to its source and opens its backing directory
/// capability, binding to the component instance if necessary.
///
/// See [`fidl_fuchsia_io::Directory::Open`] for how the `flags`, `open_mode`, `relative_path`,
/// and `server_chan` parameters are used in the open call.
async fn open_storage_capability(
    source: storage::StorageCapabilitySource,
    relative_moniker: InstancedRelativeMoniker,
    target: &Arc<ComponentInstance>,
    options: OpenOptions<'_>,
) -> Result<(), ModelError> {
    let dir_source = source.storage_provider.clone();
    let relative_moniker_2 = relative_moniker.clone();
    match options {
        OpenOptions::Storage(OpenStorageOptions { open_mode, server_chan, start_reason }) => {
            let storage_dir_proxy = storage::open_isolated_storage(
                source,
                target.persistent_storage,
                relative_moniker,
                target.instance_id().as_ref(),
                open_mode,
                &start_reason,
            )
            .await
            .map_err(|e| ModelError::from(e))?;

            // clone the final connection to connect the channel we're routing to its destination
            let server_chan = channel::take_channel(server_chan);
            storage_dir_proxy
                .clone(fio::OpenFlags::CLONE_SAME_RIGHTS, ServerEnd::new(server_chan))
                .map_err(|e| {
                    let moniker = match &dir_source {
                        Some(r) => InstancedExtendedMoniker::ComponentInstance(
                            r.instanced_moniker().clone(),
                        ),
                        None => InstancedExtendedMoniker::ComponentManager,
                    };
                    ModelError::from(OpenResourceError::open_storage_failed(
                        &moniker,
                        &relative_moniker_2,
                        "",
                        e,
                    ))
                })?;
            return Ok(());
        }
        _ => unreachable!("expected OpenStorageOptions"),
    }
}
