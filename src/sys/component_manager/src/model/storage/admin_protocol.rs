// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The storage admin protocol is a FIDL protocol that is hosted by the framework for clients to
//! perform privileged operations on isolated storage. Clients can perform tasks such as opening a
//! component's storage or outright deleting it.
//!
//! This API allows clients to perform a limited set of mutable operations on storage, without
//! direct access to the backing directory, with the goal of making it easier for clients to work
//! with isolated storage without needing to understand component_manager's storage layout.

use {
    crate::{
        capability::{CapabilityProvider, CapabilitySource, ComponentCapability, OptionalTask},
        channel,
        model::{
            component::{BindReason, WeakComponentInstance},
            error::ModelError,
            hooks::{Event, EventPayload, EventType, Hook, HooksRegistration},
            routing, storage,
        },
    },
    anyhow::{format_err, Error},
    async_trait::async_trait,
    cm_rust::{CapabilityName, ExposeDecl, OfferDecl, StorageDecl, UseDecl},
    fidl::endpoints::{ServerEnd, ServiceMarker},
    fidl_fuchsia_component as fcomponent,
    fidl_fuchsia_io::{MODE_TYPE_SERVICE, OPEN_RIGHT_READABLE, OPEN_RIGHT_WRITABLE},
    fidl_fuchsia_sys2 as fsys, fuchsia_async as fasync, fuchsia_zircon as zx,
    futures::TryStreamExt,
    lazy_static::lazy_static,
    log::*,
    moniker::AbsoluteMoniker,
    moniker::ExtendedMoniker,
    std::{
        convert::TryInto,
        path::PathBuf,
        sync::{Arc, Weak},
    },
};

lazy_static! {
    pub static ref STORAGE_ADMIN_PROTOCOL_NAME: CapabilityName =
        fsys::StorageAdminMarker::NAME.into();
}

struct StorageAdminProtocolProvider {
    storage_decl: StorageDecl,
    component: WeakComponentInstance,
    storage_admin: Arc<StorageAdmin>,
}

impl StorageAdminProtocolProvider {
    pub fn new(
        storage_decl: StorageDecl,
        component: WeakComponentInstance,
        storage_admin: Arc<StorageAdmin>,
    ) -> Self {
        Self { storage_decl, component, storage_admin }
    }
}

#[async_trait]
impl CapabilityProvider for StorageAdminProtocolProvider {
    async fn open(
        self: Box<Self>,
        flags: u32,
        open_mode: u32,
        in_relative_path: PathBuf,
        server_end: &mut zx::Channel,
    ) -> Result<OptionalTask, ModelError> {
        let server_end = channel::take_channel(server_end);
        if (flags & (OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE))
            != (OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE)
        {
            warn!("open request for the storage admin protocol rejected: access denied");
            return Ok(None.into());
        }
        if 0 == (open_mode & MODE_TYPE_SERVICE) {
            warn!("open request for the storage admin protocol rejected: incorrect mode");
            return Ok(None.into());
        }
        if in_relative_path != PathBuf::from("") {
            warn!("open request for the storage admin protocol rejected: invalid path");
            return Ok(None.into());
        }
        let storage_decl = self.storage_decl.clone();
        let component = self.component.clone();
        let storage_admin = self.storage_admin.clone();
        Ok(fasync::Task::spawn(async move {
            if let Err(e) = storage_admin.serve(storage_decl, component, server_end).await {
                warn!("failed to serve storage admin protocol: {:?}", e);
            }
        })
        .into())
    }
}

pub struct StorageAdmin {}

// `StorageAdmin` is a `Hook` that serves the `StorageAdmin` FIDL protocol.
impl StorageAdmin {
    pub fn new() -> Self {
        Self {}
    }

    pub fn hooks(self: &Arc<Self>) -> Vec<HooksRegistration> {
        vec![HooksRegistration::new(
            "StorageAdmin",
            vec![EventType::CapabilityRouted],
            Arc::downgrade(self) as Weak<dyn Hook>,
        )]
    }

    async fn extract_storage_decl(
        source_capability: &ComponentCapability,
        component: WeakComponentInstance,
    ) -> Result<Option<StorageDecl>, ModelError> {
        match source_capability {
            ComponentCapability::Offer(OfferDecl::Protocol(_))
            | ComponentCapability::Expose(ExposeDecl::Protocol(_))
            | ComponentCapability::Use(UseDecl::Protocol(_)) => (),
            _ => return Ok(None),
        }
        if source_capability.source_name() != Some(&fsys::StorageAdminMarker::NAME.into()) {
            return Ok(None);
        }
        let source_capability_name = source_capability.source_capability_name();
        if source_capability_name.is_none() {
            return Ok(None);
        }
        let source_component = component.upgrade()?;
        let source_component_state = source_component.lock_resolved_state().await?;
        let decl = source_component_state.decl();
        Ok(decl.find_storage_source(source_capability_name.unwrap()).cloned())
    }

    async fn on_scoped_framework_capability_routed_async<'a>(
        self: Arc<Self>,
        source_capability: &'a ComponentCapability,
        component: WeakComponentInstance,
        capability_provider: Option<Box<dyn CapabilityProvider>>,
    ) -> Result<Option<Box<dyn CapabilityProvider>>, ModelError> {
        // If some other capability has already been installed, then there's nothing to
        // do here.
        if capability_provider.is_some() {
            return Ok(capability_provider);
        }
        // Find the storage decl, if it exists we're good to go
        let storage_decl = Self::extract_storage_decl(source_capability, component.clone()).await?;
        if let Some(storage_decl) = storage_decl {
            return Ok(Some(Box::new(StorageAdminProtocolProvider::new(
                storage_decl,
                component,
                self.clone(),
            )) as Box<dyn CapabilityProvider>));
        }
        // The declaration referenced either a nonexistent capability, or a capability that isn't a
        // storage capability. We can't be the provider for this.
        Ok(None)
    }

    async fn serve(
        self: Arc<Self>,
        storage_decl: StorageDecl,
        component: WeakComponentInstance,
        server_end: zx::Channel,
    ) -> Result<(), Error> {
        let component = component.upgrade().map_err(|e| {
            format_err!(
                "unable to serve storage admin protocol, model reference is no longer valid: {:?}",
                e,
            )
        })?;
        let storage_moniker = component.abs_moniker.clone();

        let storage_capability_source_info =
            routing::route_storage_backing_directory(storage_decl, component.clone()).await?;

        let mut stream = ServerEnd::<fsys::StorageAdminMarker>::new(server_end)
            .into_stream()
            .expect("could not convert channel into stream");

        while let Some(request) = stream.try_next().await? {
            match request {
                fsys::StorageAdminRequest::OpenComponentStorage {
                    relative_moniker,
                    flags,
                    mode,
                    object,
                    control_handle: _,
                } => {
                    let relative_moniker = relative_moniker.as_str().try_into()?;
                    let abs_moniker =
                        AbsoluteMoniker::from_relative(&component.abs_moniker, &relative_moniker)?;
                    let instance_id = component
                        .try_get_context()?
                        .component_id_index()
                        .look_up_moniker(&abs_moniker)
                        .cloned();

                    let dir_proxy = storage::open_isolated_storage(
                        storage_capability_source_info.clone(),
                        relative_moniker,
                        instance_id.as_ref(),
                        mode,
                        &BindReason::AccessCapability {
                            target: ExtendedMoniker::ComponentInstance(storage_moniker.clone()),
                            path: storage_capability_source_info.backing_directory_path.clone(),
                        },
                    )
                    .await?;
                    dir_proxy.clone(flags, object)?;
                }
                fsys::StorageAdminRequest::DeleteComponentStorage {
                    relative_moniker,
                    responder,
                } => {
                    let err_code = match relative_moniker.as_str().try_into() {
                        Err(e) => {
                            warn!("couldn't parse string as relative moniker for storage admin protocol: {:?}", e);
                            Err(fcomponent::Error::InvalidArguments)
                        }
                        Ok(relative_moniker) => {
                            let abs_moniker = AbsoluteMoniker::from_relative(
                                &component.abs_moniker,
                                &relative_moniker,
                            )?;
                            let instance_id = component
                                .try_get_context()?
                                .component_id_index()
                                .look_up_moniker(&abs_moniker)
                                .cloned();
                            let res = storage::delete_isolated_storage(
                                storage_capability_source_info.clone(),
                                relative_moniker,
                                instance_id.as_ref(),
                            )
                            .await;
                            match res {
                                Err(e) => {
                                    warn!(
                                        "couldn't delete storage for storage admin protocol: {:?}",
                                        e
                                    );
                                    Err(fcomponent::Error::Internal)
                                }
                                Ok(()) => Ok(()),
                            }
                        }
                    };
                    match err_code {
                        Err(e) => responder.send(&mut Err(e))?,
                        Ok(()) => responder.send(&mut Ok(()))?,
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Hook for StorageAdmin {
    async fn on(self: Arc<Self>, event: &Event) -> Result<(), ModelError> {
        match &event.result {
            Ok(EventPayload::CapabilityRouted {
                source: CapabilitySource::Capability { source_capability, component },
                capability_provider,
            }) => {
                let mut capability_provider = capability_provider.lock().await;
                *capability_provider = self
                    .on_scoped_framework_capability_routed_async(
                        source_capability,
                        component.clone(),
                        capability_provider.take(),
                    )
                    .await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
