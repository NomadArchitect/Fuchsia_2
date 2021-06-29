// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        capability::{CapabilityProvider, CapabilitySource, InternalCapability, OptionalTask},
        channel,
        model::{
            addable_directory::AddableDirectoryWithResult,
            component::WeakComponentInstance,
            dir_tree::{DirTree, DirTreeCapability},
            error::ModelError,
            hooks::{Event, EventPayload, EventType, Hook, HooksRegistration, RuntimeInfo},
            lifecycle_controller::LifecycleController,
            lifecycle_controller_factory::LifecycleControllerFactory,
            routing_fns::{route_expose_fn, route_use_fn},
        },
    },
    async_trait::async_trait,
    cm_rust::{CapabilityPath, ComponentDecl},
    directory_broker,
    fidl::endpoints::{ServerEnd, ServiceMarker},
    fidl_fuchsia_io::{DirectoryProxy, NodeMarker, CLONE_FLAG_SAME_RIGHTS, MODE_TYPE_DIRECTORY},
    fidl_fuchsia_sys2::LifecycleControllerMarker,
    fuchsia_async as fasync, fuchsia_trace as trace, fuchsia_zircon as zx,
    futures::lock::Mutex,
    moniker::AbsoluteMoniker,
    std::{
        collections::HashMap,
        convert::TryFrom,
        path::PathBuf,
        sync::{Arc, Weak},
    },
    vfs::{
        directory::entry::DirectoryEntry, directory::immutable::simple as pfs,
        execution_scope::ExecutionScope, file::vmo::asynchronous::read_only_static,
        path::Path as pfsPath,
    },
};

// Declare simple directory type for brevity
type Directory = Arc<pfs::Simple>;

struct HubCapabilityProvider {
    abs_moniker: AbsoluteMoniker,
    hub: Arc<Hub>,
}

impl HubCapabilityProvider {
    pub fn new(abs_moniker: AbsoluteMoniker, hub: Arc<Hub>) -> Self {
        HubCapabilityProvider { abs_moniker, hub }
    }
}

#[async_trait]
impl CapabilityProvider for HubCapabilityProvider {
    async fn open(
        self: Box<Self>,
        flags: u32,
        open_mode: u32,
        relative_path: PathBuf,
        server_end: &mut zx::Channel,
    ) -> Result<OptionalTask, ModelError> {
        let mut relative_path = relative_path
            .to_str()
            .ok_or_else(|| ModelError::path_is_not_utf8(relative_path.clone()))?
            .to_string();
        relative_path.push('/');
        let dir_path = pfsPath::validate_and_split(relative_path.clone()).map_err(|_| {
            ModelError::open_directory_error(self.abs_moniker.clone(), relative_path)
        })?;
        self.hub.open(&self.abs_moniker, flags, open_mode, dir_path, server_end).await?;
        Ok(None.into())
    }
}

/// Hub state on an instance of a component.
struct Instance {
    // TODO(fxbug.dev/77068): Remove this attribute.
    #[allow(dead_code)]
    pub abs_moniker: AbsoluteMoniker,
    // TODO(fxbug.dev/77068): Remove this attribute.
    #[allow(dead_code)]
    pub component_url: String,
    pub execution: Option<Execution>,
    pub has_resolved_directory: bool, // the existence of the resolved directory.
    pub directory: Directory,
    pub children_directory: Directory,
    pub deleting_directory: Directory,
}

/// The execution state for a component that has started running.
struct Execution {
    // TODO(fxbug.dev/77068): Remove this attribute.
    #[allow(dead_code)]
    pub resolved_url: String,
    // TODO(fxbug.dev/77068): Remove this attribute.
    #[allow(dead_code)]
    pub directory: Directory,
}

/// The Hub is a directory tree representing the component topology. Through the Hub,
/// debugging and instrumentation tools can query information about component instances
/// on the system, such as their component URLs, execution state and so on.
pub struct Hub {
    instances: Mutex<HashMap<AbsoluteMoniker, Instance>>,
    scope: ExecutionScope,
    lifecycle_controller_factory: LifecycleControllerFactory,
}

impl Hub {
    /// Create a new Hub given a `component_url` for the root component and a
    /// `lifecycle_controller_factory` which can create scoped LifecycleController services.
    pub fn new(
        component_url: String,
        lifecycle_controller_factory: LifecycleControllerFactory,
    ) -> Result<Self, ModelError> {
        let mut instances_map = HashMap::new();
        let abs_moniker = AbsoluteMoniker::root();

        let lifecycle_controller = lifecycle_controller_factory.create(&abs_moniker);

        Hub::add_instance_if_necessary(
            lifecycle_controller,
            &abs_moniker,
            component_url,
            &mut instances_map,
        )?
        .expect("Did not create directory.");

        Ok(Hub {
            instances: Mutex::new(instances_map),
            scope: ExecutionScope::new(),
            lifecycle_controller_factory,
        })
    }

    pub async fn open_root(
        &self,
        flags: u32,
        mut server_end: zx::Channel,
    ) -> Result<(), ModelError> {
        let root_moniker = AbsoluteMoniker::root();
        self.open(&root_moniker, flags, MODE_TYPE_DIRECTORY, pfsPath::empty(), &mut server_end)
            .await?;
        Ok(())
    }

    pub fn hooks(self: &Arc<Self>) -> Vec<HooksRegistration> {
        vec![HooksRegistration::new(
            "Hub",
            vec![
                EventType::CapabilityRouted,
                EventType::Discovered,
                EventType::Purged,
                EventType::Destroyed,
                EventType::Started,
                EventType::Resolved,
                EventType::Stopped,
            ],
            Arc::downgrade(self) as Weak<dyn Hook>,
        )]
    }

    pub async fn open(
        &self,
        abs_moniker: &AbsoluteMoniker,
        flags: u32,
        open_mode: u32,
        relative_path: pfsPath,
        server_end: &mut zx::Channel,
    ) -> Result<(), ModelError> {
        let instances_map = self.instances.lock().await;
        if !instances_map.contains_key(&abs_moniker) {
            return Err(ModelError::open_directory_error(
                abs_moniker.clone(),
                relative_path.into_string(),
            ));
        }
        let server_end = channel::take_channel(server_end);
        instances_map[&abs_moniker].directory.clone().open(
            self.scope.clone(),
            flags,
            open_mode,
            relative_path,
            ServerEnd::<NodeMarker>::new(server_end),
        );
        Ok(())
    }

    fn add_instance_if_necessary(
        lifecycle_controller: LifecycleController,
        abs_moniker: &AbsoluteMoniker,
        component_url: String,
        instance_map: &mut HashMap<AbsoluteMoniker, Instance>,
    ) -> Result<Option<Directory>, ModelError> {
        trace::duration!("component_manager", "hub:add_instance_if_necessary");
        if instance_map.contains_key(&abs_moniker) {
            return Ok(None);
        }

        let instance = pfs::simple();

        // Add a 'url' file.
        instance.add_node(
            "url",
            read_only_static(component_url.clone().into_bytes()),
            &abs_moniker,
        )?;

        // Add an 'id' file.
        // For consistency sake, the Hub assumes that the root instance also
        // has ID 0, like any other static instance.
        let id =
            if let Some(child_moniker) = abs_moniker.leaf() { child_moniker.instance() } else { 0 };
        let component_type = if id > 0 { "dynamic" } else { "static" };
        instance.add_node("id", read_only_static(id.to_string().into_bytes()), &abs_moniker)?;

        // Add a 'component_type' file.
        instance.add_node(
            "component_type",
            read_only_static(component_type.to_string().into_bytes()),
            &abs_moniker,
        )?;

        // Add a children directory.
        let children = pfs::simple();
        instance.add_node("children", children.clone(), &abs_moniker)?;

        // Add a deleting directory.
        let deleting = pfs::simple();
        instance.add_node("deleting", deleting.clone(), &abs_moniker)?;

        Self::add_debug_directory(lifecycle_controller, instance.clone(), abs_moniker)?;

        instance_map.insert(
            abs_moniker.clone(),
            Instance {
                abs_moniker: abs_moniker.clone(),
                component_url,
                execution: None,
                has_resolved_directory: false,
                directory: instance.clone(),
                children_directory: children.clone(),
                deleting_directory: deleting.clone(),
            },
        );

        Ok(Some(instance))
    }

    async fn add_instance_to_parent_if_necessary<'a>(
        lifecycle_controller: LifecycleController,
        abs_moniker: &'a AbsoluteMoniker,
        component_url: String,
        mut instances_map: &'a mut HashMap<AbsoluteMoniker, Instance>,
    ) -> Result<(), ModelError> {
        if let Some(controlled) = Hub::add_instance_if_necessary(
            lifecycle_controller,
            &abs_moniker,
            component_url,
            &mut instances_map,
        )? {
            if let (Some(leaf), Some(parent_moniker)) = (abs_moniker.leaf(), abs_moniker.parent()) {
                // In the children directory, the child's instance id is not used
                trace::duration!("component_manager", "hub:add_instance_to_parent");
                let partial_moniker = leaf.to_partial();
                instances_map[&parent_moniker].children_directory.add_node(
                    partial_moniker.as_str(),
                    controlled.clone(),
                    &abs_moniker,
                )?;
            }
        }
        Ok(())
    }

    fn add_resolved_url_file(
        directory: Directory,
        resolved_url: String,
        abs_moniker: &AbsoluteMoniker,
    ) -> Result<(), ModelError> {
        directory.add_node(
            "resolved_url",
            read_only_static(resolved_url.into_bytes()),
            &abs_moniker,
        )?;
        Ok(())
    }

    fn add_use_directory(
        directory: Directory,
        component_decl: ComponentDecl,
        target_moniker: &AbsoluteMoniker,
        target: WeakComponentInstance,
    ) -> Result<(), ModelError> {
        let tree = DirTree::build_from_uses(route_use_fn, target, component_decl);
        let mut use_dir = pfs::simple();
        tree.install(target_moniker, &mut use_dir)?;
        directory.add_node("use", use_dir, target_moniker)?;
        Ok(())
    }

    fn add_in_directory(
        execution_directory: Directory,
        component_decl: ComponentDecl,
        package_dir: Option<DirectoryProxy>,
        target_moniker: &AbsoluteMoniker,
        target: WeakComponentInstance,
    ) -> Result<(), ModelError> {
        let tree = DirTree::build_from_uses(route_use_fn, target, component_decl);
        let mut in_dir = pfs::simple();
        tree.install(target_moniker, &mut in_dir)?;
        if let Some(pkg_dir) = package_dir {
            in_dir.add_node(
                "pkg",
                directory_broker::DirectoryBroker::from_directory_proxy(pkg_dir),
                target_moniker,
            )?;
        }
        execution_directory.add_node("in", in_dir, target_moniker)?;
        Ok(())
    }

    fn add_debug_directory(
        lifecycle_controller: LifecycleController,
        parent_directory: Directory,
        target_moniker: &AbsoluteMoniker,
    ) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:add_debug_directory");

        let mut debug_dir = pfs::simple();

        let lifecycle_controller_path =
            CapabilityPath::try_from(format!("/{}", LifecycleControllerMarker::NAME).as_str())
                .unwrap();
        let capabilities = vec![DirTreeCapability::new(
            lifecycle_controller_path,
            Box::new(
                move |_flags: u32,
                      _mode: u32,
                      _relative_path: String,
                      server_end: ServerEnd<NodeMarker>| {
                    let lifecycle_controller = lifecycle_controller.clone();
                    let server_end =
                        ServerEnd::<LifecycleControllerMarker>::new(server_end.into_channel());
                    let lifecycle_controller_stream = server_end.into_stream().unwrap();
                    fasync::Task::spawn(async move {
                        lifecycle_controller.serve(lifecycle_controller_stream).await;
                    })
                    .detach();
                },
            ),
        )];
        let tree = DirTree::build_from_capabilities(capabilities);
        tree.install(target_moniker, &mut debug_dir)?;

        parent_directory.add_node("debug", debug_dir, target_moniker)?;
        Ok(())
    }

    fn add_expose_directory(
        directory: Directory,
        component_decl: ComponentDecl,
        target_moniker: &AbsoluteMoniker,
        target: WeakComponentInstance,
    ) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:add_expose_directory");
        let tree = DirTree::build_from_exposes(route_expose_fn, target, component_decl);
        let mut expose_dir = pfs::simple();
        tree.install(target_moniker, &mut expose_dir)?;
        directory.add_node("expose", expose_dir, target_moniker)?;
        Ok(())
    }

    fn add_out_directory(
        execution_directory: Directory,
        outgoing_dir: Option<DirectoryProxy>,
        target_moniker: &AbsoluteMoniker,
    ) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:add_out_directory");
        if let Some(out_dir) = outgoing_dir {
            execution_directory.add_node(
                "out",
                directory_broker::DirectoryBroker::from_directory_proxy(out_dir),
                target_moniker,
            )?;
        }
        Ok(())
    }

    fn add_runtime_directory(
        execution_directory: Directory,
        runtime_dir: Option<DirectoryProxy>,
        abs_moniker: &AbsoluteMoniker,
    ) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:add_runtime_directory");
        if let Some(runtime_dir) = runtime_dir {
            execution_directory.add_node(
                "runtime",
                directory_broker::DirectoryBroker::from_directory_proxy(runtime_dir),
                abs_moniker,
            )?;
        }
        Ok(())
    }

    async fn on_resolved_async<'a>(
        &self,
        target_moniker: &AbsoluteMoniker,
        target: &WeakComponentInstance,
        resolved_url: String,
        component_decl: &'a ComponentDecl,
    ) -> Result<(), ModelError> {
        let mut instances_map = self.instances.lock().await;

        let instance = instances_map
            .get_mut(target_moniker)
            .expect(&format!("Unable to find instance {} in map.", target_moniker));

        // If the resolved directory already exists, report error.
        assert!(!instance.has_resolved_directory);
        let resolved_directory = pfs::simple();
        instance.has_resolved_directory = true;

        Self::add_resolved_url_file(
            resolved_directory.clone(),
            resolved_url.clone(),
            target_moniker,
        )?;

        Self::add_use_directory(
            resolved_directory.clone(),
            component_decl.clone(),
            target_moniker,
            target.clone(),
        )?;

        Self::add_expose_directory(
            resolved_directory.clone(),
            component_decl.clone(),
            target_moniker,
            target.clone(),
        )?;

        instance.directory.add_node("resolved", resolved_directory, &target_moniker)?;

        Ok(())
    }

    async fn on_started_async<'a>(
        &'a self,
        target_moniker: &AbsoluteMoniker,
        target: &WeakComponentInstance,
        runtime: &RuntimeInfo,
        component_decl: &'a ComponentDecl,
    ) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:on_start_instance_async");

        let mut instances_map = self.instances.lock().await;

        let instance = instances_map
            .get_mut(target_moniker)
            .expect(&format!("Unable to find instance {} in map.", target_moniker));

        // If we haven't already created an execution directory, create one now.
        if instance.execution.is_none() {
            trace::duration!("component_manager", "hub:create_execution");

            let execution_directory = pfs::simple();

            let exec = Execution {
                resolved_url: runtime.resolved_url.clone(),
                directory: execution_directory.clone(),
            };
            instance.execution = Some(exec);

            Self::add_resolved_url_file(
                execution_directory.clone(),
                runtime.resolved_url.clone(),
                target_moniker,
            )?;

            Self::add_in_directory(
                execution_directory.clone(),
                component_decl.clone(),
                Self::clone_dir(runtime.package_dir.as_ref()),
                target_moniker,
                target.clone(),
            )?;

            Self::add_expose_directory(
                execution_directory.clone(),
                component_decl.clone(),
                target_moniker,
                target.clone(),
            )?;

            Self::add_out_directory(
                execution_directory.clone(),
                Self::clone_dir(runtime.outgoing_dir.as_ref()),
                target_moniker,
            )?;

            Self::add_runtime_directory(
                execution_directory.clone(),
                Self::clone_dir(runtime.runtime_dir.as_ref()),
                &target_moniker,
            )?;

            instance.directory.add_node("exec", execution_directory, &target_moniker)?;
        }

        Ok(())
    }

    async fn on_discovered_async(
        &self,
        target_moniker: &AbsoluteMoniker,
        component_url: String,
    ) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:on_discovered_async");

        let lifecycle_controller = self.lifecycle_controller_factory.create(&target_moniker);

        let mut instances_map = self.instances.lock().await;

        Self::add_instance_to_parent_if_necessary(
            lifecycle_controller,
            target_moniker,
            component_url,
            &mut instances_map,
        )
        .await?;
        Ok(())
    }

    async fn on_purged_async(&self, target_moniker: &AbsoluteMoniker) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:on_purged_async");
        let mut instance_map = self.instances.lock().await;

        // TODO(xbhatnag): Investigate error handling scenarios here.
        //                 Can these errors arise from faulty components or from
        //                 a bug in ComponentManager?
        let parent_moniker = target_moniker.parent().expect("a root component cannot be dynamic");
        let leaf = target_moniker.leaf().expect("a root component cannot be dynamic");

        instance_map[&parent_moniker].deleting_directory.remove_node(leaf.as_str())?;
        instance_map
            .remove(&target_moniker)
            .expect("the dynamic component must exist in the instance map");
        Ok(())
    }

    async fn on_stopped_async(&self, target_moniker: &AbsoluteMoniker) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:on_stopped_async");
        let mut instance_map = self.instances.lock().await;
        instance_map[target_moniker].directory.remove_node("exec")?;
        instance_map.get_mut(target_moniker).expect("instance must exist").execution = None;
        Ok(())
    }

    async fn on_destroyed_async(&self, target_moniker: &AbsoluteMoniker) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:on_destroyed_async");
        let parent_moniker = target_moniker.parent().expect("A root component cannot be destroyed");
        let instance_map = self.instances.lock().await;
        if !instance_map.contains_key(&parent_moniker) {
            // Evidently this a duplicate dispatch of Destroyed.
            return Ok(());
        }

        let leaf = target_moniker.leaf().expect("A root component cannot be destroyed");

        // In the children directory, the child's instance id is not used
        // TODO: It's possible for the Destroyed event to be dispatched twice if there
        // are two concurrent `DestroyChild` operations. In such cases we should probably cause
        // this update to no-op instead of returning an error.
        let partial_moniker = leaf.to_partial();
        let directory = instance_map[&parent_moniker]
            .children_directory
            .remove_node(partial_moniker.as_str())
            .map_err(|_| ModelError::remove_entry_error(leaf.as_str()))?;

        instance_map[&parent_moniker].deleting_directory.add_node(
            leaf.as_str(),
            directory,
            target_moniker,
        )?;
        Ok(())
    }

    /// Given a `CapabilitySource`, determine if it is a framework-provided
    /// hub capability. If so, update the given `capability_provider` to
    /// provide a hub directory.
    async fn on_capability_routed_async(
        self: Arc<Self>,
        _target_moniker: &AbsoluteMoniker,
        source: CapabilitySource,
        capability_provider: Arc<Mutex<Option<Box<dyn CapabilityProvider>>>>,
    ) -> Result<(), ModelError> {
        trace::duration!("component_manager", "hub:on_capability_routed_async");
        // If this is a scoped framework directory capability, then check the source path
        if let CapabilitySource::Framework {
            capability: InternalCapability::Directory(source_name),
            component,
        } = source
        {
            if source_name.str() != "hub" {
                return Ok(());
            }

            // Set the capability provider, if not already set.
            let mut capability_provider = capability_provider.lock().await;
            if capability_provider.is_none() {
                *capability_provider = Some(Box::new(HubCapabilityProvider::new(
                    component.moniker.clone(),
                    self.clone(),
                )))
            }
        }
        Ok(())
    }

    // TODO(fsamuel): We should probably preserve the original error messages
    // instead of dropping them.
    fn clone_dir(dir: Option<&DirectoryProxy>) -> Option<DirectoryProxy> {
        dir.and_then(|d| io_util::clone_directory(d, CLONE_FLAG_SAME_RIGHTS).ok())
    }
}

#[async_trait]
impl Hook for Hub {
    async fn on(self: Arc<Self>, event: &Event) -> Result<(), ModelError> {
        let target_moniker = event
            .target_moniker
            .unwrap_instance_moniker_or(ModelError::UnexpectedComponentManagerMoniker)?;
        match &event.result {
            Ok(EventPayload::CapabilityRouted { source, capability_provider }) => {
                self.on_capability_routed_async(
                    target_moniker,
                    source.clone(),
                    capability_provider.clone(),
                )
                .await?;
            }
            Ok(EventPayload::Purged) => {
                self.on_purged_async(target_moniker).await?;
            }
            Ok(EventPayload::Discovered) => {
                self.on_discovered_async(target_moniker, event.component_url.to_string()).await?;
            }
            Ok(EventPayload::Destroyed) => {
                self.on_destroyed_async(target_moniker).await?;
            }
            Ok(EventPayload::Started { component, runtime, component_decl, .. }) => {
                self.on_started_async(target_moniker, component, &runtime, &component_decl).await?;
            }
            Ok(EventPayload::Resolved { component, resolved_url, decl, .. }) => {
                self.on_resolved_async(target_moniker, component, resolved_url.clone(), &decl)
                    .await?;
            }
            Ok(EventPayload::Stopped { .. }) => {
                self.on_stopped_async(target_moniker).await?;
            }
            _ => {}
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            builtin_environment::BuiltinEnvironment,
            model::{
                binding::Binder,
                component::BindReason,
                model::Model,
                rights,
                testing::{
                    test_helpers::{
                        component_decl_with_test_runner, dir_contains, list_directory,
                        list_directory_recursive, read_file, TestEnvironmentBuilder,
                        TestModelResult,
                    },
                    test_hook::HubInjectionTestHook,
                },
            },
        },
        cm_rust::{
            self, CapabilityName, CapabilityPath, ComponentDecl, DependencyType, DirectoryDecl,
            EventMode, EventSubscription, ExposeDecl, ExposeDirectoryDecl, ExposeProtocolDecl,
            ExposeSource, ExposeTarget, ProtocolDecl, UseDecl, UseDirectoryDecl, UseEventDecl,
            UseEventStreamDecl, UseProtocolDecl, UseSource,
        },
        cm_rust_testing::ComponentDeclBuilder,
        fidl::endpoints::ServerEnd,
        fidl_fuchsia_io::{
            DirectoryMarker, DirectoryProxy, MODE_TYPE_DIRECTORY, OPEN_RIGHT_READABLE,
            OPEN_RIGHT_WRITABLE,
        },
        std::{convert::TryFrom, path::Path},
        vfs::{
            directory::entry::DirectoryEntry, execution_scope::ExecutionScope,
            file::vmo::asynchronous::read_only_static, path::Path as pfsPath, pseudo_directory,
        },
    };

    /// Hosts an out directory with a 'foo' file.
    fn foo_out_dir_fn() -> Box<dyn Fn(ServerEnd<DirectoryMarker>) + Send + Sync> {
        Box::new(move |server_end: ServerEnd<DirectoryMarker>| {
            let out_dir = pseudo_directory!(
                "foo" => read_only_static(b"bar"),
                "test" => pseudo_directory!(
                    "aaa" => read_only_static(b"bbb"),
                ),
            );

            out_dir.clone().open(
                ExecutionScope::new(),
                OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                MODE_TYPE_DIRECTORY,
                pfsPath::empty(),
                ServerEnd::new(server_end.into_channel()),
            );
        })
    }

    /// Hosts a runtime directory with a 'bleep' file.
    fn bleep_runtime_dir_fn() -> Box<dyn Fn(ServerEnd<DirectoryMarker>) + Send + Sync> {
        Box::new(move |server_end: ServerEnd<DirectoryMarker>| {
            let pseudo_dir = pseudo_directory!(
                "bleep" => read_only_static(b"blah"),
            );

            pseudo_dir.clone().open(
                ExecutionScope::new(),
                OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                MODE_TYPE_DIRECTORY,
                pfsPath::empty(),
                ServerEnd::new(server_end.into_channel()),
            );
        })
    }

    type DirectoryCallback = Box<dyn Fn(ServerEnd<DirectoryMarker>) + Send + Sync>;

    struct ComponentDescriptor {
        pub name: &'static str,
        pub decl: ComponentDecl,
        pub host_fn: Option<DirectoryCallback>,
        pub runtime_host_fn: Option<DirectoryCallback>,
    }

    async fn start_component_manager_with_hub(
        root_component_url: String,
        components: Vec<ComponentDescriptor>,
    ) -> (Arc<Model>, Arc<Mutex<BuiltinEnvironment>>, DirectoryProxy) {
        start_component_manager_with_hub_and_hooks(root_component_url, components, vec![]).await
    }

    async fn start_component_manager_with_hub_and_hooks(
        root_component_url: String,
        components: Vec<ComponentDescriptor>,
        additional_hooks: Vec<HooksRegistration>,
    ) -> (Arc<Model>, Arc<Mutex<BuiltinEnvironment>>, DirectoryProxy) {
        let resolved_root_component_url = format!("{}_resolved", root_component_url);
        let decls = components.iter().map(|c| (c.name, c.decl.clone())).collect();
        let TestModelResult { model, builtin_environment, mock_runner, .. } =
            TestEnvironmentBuilder::new().set_components(decls).build().await;
        for component in components.into_iter() {
            if let Some(host_fn) = component.host_fn {
                mock_runner.add_host_fn(&resolved_root_component_url, host_fn);
            }

            if let Some(runtime_host_fn) = component.runtime_host_fn {
                mock_runner.add_runtime_host_fn(&resolved_root_component_url, runtime_host_fn);
            }
        }

        let hub_proxy = builtin_environment
            .lock()
            .await
            .bind_service_fs_for_hub()
            .await
            .expect("unable to bind service_fs");

        model.root.hooks.install(additional_hooks).await;

        let root_moniker = AbsoluteMoniker::root();
        let res = model.bind(&root_moniker, &BindReason::Root).await;
        assert!(res.is_ok());

        (model, builtin_environment, hub_proxy)
    }

    #[fuchsia::test]
    async fn hub_basic() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![
                ComponentDescriptor {
                    name: "root",
                    decl: ComponentDeclBuilder::new().add_lazy_child("a").build(),
                    host_fn: None,
                    runtime_host_fn: None,
                },
                ComponentDescriptor {
                    name: "a",
                    decl: component_decl_with_test_runner(),
                    host_fn: None,
                    runtime_host_fn: None,
                },
            ],
        )
        .await;

        assert_eq!(root_component_url, read_file(&hub_proxy, "url").await);
        assert_eq!(
            format!("{}_resolved", root_component_url),
            read_file(&hub_proxy, "exec/resolved_url").await
        );

        // Verify IDs
        assert_eq!("0", read_file(&hub_proxy, "id").await);
        assert_eq!("0", read_file(&hub_proxy, "children/a/id").await);

        // Verify Component Type
        assert_eq!("static", read_file(&hub_proxy, "component_type").await);
        assert_eq!("static", read_file(&hub_proxy, "children/a/component_type").await);

        assert_eq!("test:///a", read_file(&hub_proxy, "children/a/url").await);
    }

    #[fuchsia::test]
    async fn hub_out_directory() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new().add_lazy_child("a").build(),
                host_fn: Some(foo_out_dir_fn()),
                runtime_host_fn: None,
            }],
        )
        .await;

        assert!(dir_contains(&hub_proxy, "exec", "out").await);
        assert!(dir_contains(&hub_proxy, "exec/out", "foo").await);
        assert!(dir_contains(&hub_proxy, "exec/out/test", "aaa").await);
        assert_eq!("bar", read_file(&hub_proxy, "exec/out/foo").await);
        assert_eq!("bbb", read_file(&hub_proxy, "exec/out/test/aaa").await);
    }

    #[fuchsia::test]
    async fn hub_runtime_directory() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new().add_lazy_child("a").build(),
                host_fn: None,
                runtime_host_fn: Some(bleep_runtime_dir_fn()),
            }],
        )
        .await;

        assert_eq!("blah", read_file(&hub_proxy, "exec/runtime/bleep").await);
    }

    #[fuchsia::test]
    async fn hub_test_hook_interception() {
        let root_component_url = "test:///root".to_string();
        let hub_injection_test_hook = Arc::new(HubInjectionTestHook::new());
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub_and_hooks(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new()
                    .add_lazy_child("a")
                    .use_(UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: DependencyType::Strong,
                        source: UseSource::Framework,
                        source_name: "hub".into(),
                        target_path: CapabilityPath::try_from("/hub").unwrap(),
                        rights: *rights::READ_RIGHTS,
                        subdir: None,
                    }))
                    .build(),
                host_fn: None,
                runtime_host_fn: None,
            }],
            hub_injection_test_hook.hooks(),
        )
        .await;

        let in_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("exec/in"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(vec!["hub"], list_directory(&in_dir).await);

        let scoped_hub_dir_proxy = io_util::open_directory(
            &hub_proxy,
            &Path::new("exec/in/hub"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        // There are no out or runtime directories because there is no program running.
        assert_eq!(vec!["old_hub"], list_directory(&scoped_hub_dir_proxy).await);

        let old_hub_dir_proxy = io_util::open_directory(
            &hub_proxy,
            &Path::new("exec/in/hub/old_hub/exec"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(
            vec!["expose", "in", "out", "resolved_url", "runtime"],
            list_directory(&old_hub_dir_proxy).await
        );
    }

    #[fuchsia::test]
    async fn hub_resolved_directory() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new()
                    .add_lazy_child("a")
                    .use_(UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: DependencyType::Strong,
                        source: UseSource::Framework,
                        source_name: "hub".into(),
                        target_path: CapabilityPath::try_from("/hub").unwrap(),
                        rights: *rights::READ_RIGHTS,
                        subdir: Some("resolved".into()),
                    }))
                    .build(),
                host_fn: None,
                runtime_host_fn: None,
            }],
        )
        .await;

        let resolved_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("resolved"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(vec!["expose", "resolved_url", "use"], list_directory(&resolved_dir).await);
        assert_eq!(
            format!("{}_resolved", root_component_url),
            read_file(&hub_proxy, "resolved/resolved_url").await
        );
    }

    #[fuchsia::test]
    async fn hub_use_directory_in_resolved() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new()
                    .add_lazy_child("a")
                    .use_(UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: DependencyType::Strong,
                        source: UseSource::Framework,
                        source_name: "hub".into(),
                        target_path: CapabilityPath::try_from("/hub").unwrap(),
                        rights: *rights::READ_RIGHTS,
                        subdir: None,
                    }))
                    .build(),
                host_fn: None,
                runtime_host_fn: None,
            }],
        )
        .await;

        let use_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("resolved/use"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");

        assert_eq!(vec!["hub"], list_directory(&use_dir).await);

        let hub_dir = io_util::open_directory(
            &use_dir,
            &Path::new("hub"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");

        assert_eq!(
            vec![
                "children",
                "component_type",
                "debug",
                "deleting",
                "exec",
                "id",
                "resolved",
                "url"
            ],
            list_directory(&hub_dir).await
        );
    }

    #[fuchsia::test]
    // TODO(b/65870): change function name to hub_expose_directory after the expose directory
    // is removed from exec and the original hub_expose_directory test is deleted.
    async fn hub_expose_directory_in_resolved() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new()
                    .add_lazy_child("a")
                    .protocol(ProtocolDecl {
                        name: "foo".into(),
                        source_path: "/svc/foo".parse().unwrap(),
                    })
                    .directory(DirectoryDecl {
                        name: "baz".into(),
                        source_path: "/data".parse().unwrap(),
                        rights: *rights::READ_RIGHTS,
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "foo".into(),
                        target_name: "bar".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: ExposeSource::Self_,
                        source_name: "baz".into(),
                        target_name: "hippo".into(),
                        target: ExposeTarget::Parent,
                        rights: None,
                        subdir: None,
                    }))
                    .build(),
                host_fn: None,
                runtime_host_fn: None,
            }],
        )
        .await;

        let expose_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("resolved/expose"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(vec!["bar", "hippo"], list_directory_recursive(&expose_dir).await);
    }

    #[fuchsia::test]
    async fn hub_in_directory() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new()
                    .add_lazy_child("a")
                    .use_(UseDecl::Directory(UseDirectoryDecl {
                        source: UseSource::Framework,
                        source_name: "hub".into(),
                        target_path: CapabilityPath::try_from("/hub").unwrap(),
                        rights: *rights::READ_RIGHTS,
                        subdir: Some("exec".into()),
                        dependency_type: DependencyType::Strong,
                    }))
                    .use_(UseDecl::Protocol(UseProtocolDecl {
                        source: UseSource::Parent,
                        source_name: "baz-svc".into(),
                        target_path: CapabilityPath::try_from("/svc/hippo").unwrap(),
                        dependency_type: DependencyType::Strong,
                    }))
                    .use_(UseDecl::Directory(UseDirectoryDecl {
                        source: UseSource::Parent,
                        source_name: "foo-dir".into(),
                        target_path: CapabilityPath::try_from("/data/bar").unwrap(),
                        rights: *rights::READ_RIGHTS | *rights::WRITE_RIGHTS,
                        subdir: None,
                        dependency_type: DependencyType::Strong,
                    }))
                    .build(),
                host_fn: None,
                runtime_host_fn: None,
            }],
        )
        .await;

        let in_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("exec/in"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(vec!["data", "hub", "svc"], list_directory(&in_dir).await);

        let scoped_hub_dir_proxy = io_util::open_directory(
            &hub_proxy,
            &Path::new("exec/in/hub"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(
            vec!["expose", "in", "out", "resolved_url", "runtime"],
            list_directory(&scoped_hub_dir_proxy).await
        );
    }

    #[fuchsia::test]
    async fn hub_no_event_stream_in_incoming_directory() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new()
                    .add_lazy_child("a")
                    .use_(UseDecl::Event(UseEventDecl {
                        dependency_type: DependencyType::Strong,
                        source: UseSource::Framework,
                        source_name: "started".into(),
                        target_name: "started".into(),
                        filter: None,
                        mode: cm_rust::EventMode::Async,
                    }))
                    .use_(UseDecl::EventStream(UseEventStreamDecl {
                        name: CapabilityName::try_from("EventStream").unwrap(),
                        subscriptions: vec![EventSubscription {
                            event_name: "started".to_string(),
                            mode: EventMode::Async,
                        }],
                    }))
                    .build(),
                host_fn: None,
                runtime_host_fn: None,
            }],
        )
        .await;

        let in_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("exec/in"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(0, list_directory(&in_dir).await.len());
    }

    #[fuchsia::test]
    async fn hub_expose_directory() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new()
                    .add_lazy_child("a")
                    .protocol(ProtocolDecl {
                        name: "foo".into(),
                        source_path: "/svc/foo".parse().unwrap(),
                    })
                    .directory(DirectoryDecl {
                        name: "baz".into(),
                        source_path: "/data".parse().unwrap(),
                        rights: *rights::READ_RIGHTS,
                    })
                    .expose(ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: ExposeSource::Self_,
                        source_name: "foo".into(),
                        target_name: "bar".into(),
                        target: ExposeTarget::Parent,
                    }))
                    .expose(ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: ExposeSource::Self_,
                        source_name: "baz".into(),
                        target_name: "hippo".into(),
                        target: ExposeTarget::Parent,
                        rights: None,
                        subdir: None,
                    }))
                    .build(),
                host_fn: None,
                runtime_host_fn: None,
            }],
        )
        .await;

        let expose_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("exec/expose"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");
        assert_eq!(vec!["bar", "hippo"], list_directory_recursive(&expose_dir).await);
    }

    #[fuchsia::test]
    async fn hub_debug_directory() {
        let root_component_url = "test:///root".to_string();
        let (_model, _builtin_environment, hub_proxy) = start_component_manager_with_hub(
            root_component_url.clone(),
            vec![ComponentDescriptor {
                name: "root",
                decl: ComponentDeclBuilder::new().add_lazy_child("a").build(),
                host_fn: None,
                runtime_host_fn: Some(bleep_runtime_dir_fn()),
            }],
        )
        .await;

        let debug_svc_dir = io_util::open_directory(
            &hub_proxy,
            &Path::new("debug"),
            OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
        )
        .expect("Failed to open directory");

        assert_eq!(
            vec!["fuchsia.sys2.LifecycleController"],
            list_directory_recursive(&debug_svc_dir).await
        );
    }
}
