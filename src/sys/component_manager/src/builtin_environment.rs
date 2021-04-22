// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        builtin::{
            arguments::Arguments as BootArguments,
            capability::BuiltinCapability,
            debug_resource::DebugResource,
            fuchsia_boot_resolver::{FuchsiaBootResolver, SCHEME as BOOT_SCHEME},
            hypervisor_resource::HypervisorResource,
            info_resource::InfoResource,
            ioport_resource::IoportResource,
            irq_resource::IrqResource,
            kernel_stats::KernelStats,
            log::{ReadOnlyLog, WriteOnlyLog},
            mmio_resource::MmioResource,
            process_launcher::ProcessLauncher,
            root_job::{RootJob, ROOT_JOB_CAPABILITY_NAME, ROOT_JOB_FOR_INSPECT_CAPABILITY_NAME},
            root_resource::RootResource,
            runner::{BuiltinRunner, BuiltinRunnerFactory},
            smc_resource::SmcResource,
            system_controller::SystemController,
            time::{create_utc_clock, UtcTimeMaintainer},
            vmex_resource::VmexResource,
        },
        capability_ready_notifier::CapabilityReadyNotifier,
        config::RuntimeConfig,
        diagnostics::ComponentTreeStats,
        elf_runner::ElfRunner,
        framework::RealmCapabilityHost,
        fuchsia_pkg_resolver,
        model::{
            binding::Binder,
            component::ComponentManagerInstance,
            environment::{DebugRegistry, Environment, RunnerRegistry},
            error::ModelError,
            event_logger::EventLogger,
            events::{
                registry::{EventRegistry, ExecutionMode},
                running_provider::RunningProvider,
                source_factory::EventSourceFactory,
                stream_provider::EventStreamProvider,
            },
            hooks::EventType,
            hub::Hub,
            model::{Model, ModelParams},
            resolver::{BuiltinResolver, Resolver, ResolverRegistry},
            storage::admin_protocol::StorageAdmin,
        },
        root_stop_notifier::RootStopNotifier,
        work_scheduler::WorkScheduler,
    },
    anyhow::{bail, format_err, Context as _, Error},
    cm_rust::{CapabilityName, RunnerRegistration},
    cm_types::Url,
    fidl::endpoints::{create_endpoints, create_proxy, ServerEnd, ServiceMarker},
    fidl_fuchsia_component_internal::{BuiltinPkgResolver, OutDirContents},
    fidl_fuchsia_diagnostics_types::Task as DiagnosticsTask,
    fidl_fuchsia_io::{
        DirectoryMarker, DirectoryProxy, NodeMarker, MODE_TYPE_DIRECTORY, OPEN_RIGHT_READABLE,
        OPEN_RIGHT_WRITABLE,
    },
    fidl_fuchsia_sys::{LoaderMarker, LoaderProxy},
    fuchsia_async as fasync,
    fuchsia_component::{client, server::*},
    fuchsia_inspect::{component, health::Reporter, Inspector},
    fuchsia_runtime::{take_startup_handle, HandleType},
    fuchsia_zircon::{self as zx, Clock, HandleBased},
    futures::prelude::*,
    log::*,
    std::{path::PathBuf, sync::Arc},
};

// Allow shutdown to take up to an hour.
pub static SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

pub struct BuiltinEnvironmentBuilder {
    // TODO(60804): Make component manager's namespace injectable here.
    runtime_config: Option<RuntimeConfig>,
    runners: Vec<(CapabilityName, Arc<dyn BuiltinRunnerFactory>)>,
    resolvers: ResolverRegistry,
    utc_clock: Option<Arc<Clock>>,
    add_environment_resolvers: bool,
    inspector: Option<Inspector>,
    enable_hub: bool,
}

impl Default for BuiltinEnvironmentBuilder {
    fn default() -> Self {
        Self {
            runtime_config: None,
            runners: vec![],
            resolvers: ResolverRegistry::default(),
            utc_clock: None,
            add_environment_resolvers: false,
            inspector: None,
            enable_hub: true,
        }
    }
}

impl BuiltinEnvironmentBuilder {
    pub fn new() -> Self {
        BuiltinEnvironmentBuilder::default()
    }

    pub fn use_default_config(self) -> Self {
        self.set_runtime_config(RuntimeConfig::default())
    }

    pub fn set_runtime_config(mut self, runtime_config: RuntimeConfig) -> Self {
        self.runtime_config = Some(runtime_config);
        self
    }

    pub fn set_inspector(mut self, inspector: Inspector) -> Self {
        self.inspector = Some(inspector);
        self
    }

    pub fn enable_hub(mut self, val: bool) -> Self {
        self.enable_hub = val;
        self
    }

    /// Create a UTC clock if required.
    /// Not every instance of component_manager running on the system maintains a
    /// UTC clock. Only the root component_manager should have the `maintain-utc-clock`
    /// config flag set.
    pub async fn create_utc_clock(mut self) -> Result<Self, Error> {
        let runtime_config = self
            .runtime_config
            .as_ref()
            .ok_or(format_err!("Runtime config should be set to create utc clock."))?;
        self.utc_clock = if runtime_config.maintain_utc_clock {
            Some(Arc::new(create_utc_clock().await.context("failed to create UTC clock")?))
        } else {
            None
        };
        Ok(self)
    }

    pub fn set_utc_clock(mut self, clock: Arc<Clock>) -> Self {
        self.utc_clock = Some(clock);
        self
    }

    pub fn add_elf_runner(self) -> Result<Self, Error> {
        let runtime_config = self
            .runtime_config
            .as_ref()
            .ok_or(format_err!("Runtime config should be set to add elf runner."))?;

        let runner = Arc::new(ElfRunner::new(&runtime_config, self.utc_clock.clone()));
        Ok(self.add_runner("elf".into(), runner))
    }

    pub fn add_runner(
        mut self,
        name: CapabilityName,
        runner: Arc<dyn BuiltinRunnerFactory>,
    ) -> Self {
        // We don't wrap these in a BuiltinRunner immediately because that requires the
        // RuntimeConfig, which may be provided after this or may fall back to the default.
        self.runners.push((name, runner));
        self
    }

    pub fn add_resolver(
        mut self,
        scheme: String,
        resolver: Box<dyn Resolver + Send + Sync + 'static>,
    ) -> Self {
        self.resolvers.register(scheme, resolver);
        self
    }

    /// Adds standard resolvers whose dependencies are available in the process's namespace and for
    /// whose scheme no resolver is registered through `add_resolver` by the time `build()` is
    /// is called. This includes:
    ///   - A fuchsia-boot resolver if /boot is available.
    ///   - A fuchsia-pkg resolver, if /svc/fuchsia.sys.Loader is present.
    ///       - This resolver implementation proxies to that protocol (which is the v1 resolver
    ///         equivalent). This is used for tests or other scenarios where component_manager runs
    ///         as a v1 component.
    pub fn include_namespace_resolvers(mut self) -> Self {
        self.add_environment_resolvers = true;
        self
    }

    pub async fn build(mut self) -> Result<BuiltinEnvironment, Error> {
        let runtime_config = self
            .runtime_config
            .ok_or(format_err!("Runtime config is required for BuiltinEnvironment."))?;

        let root_component_url = match runtime_config.root_component_url.as_ref() {
            Some(url) => url.clone(),
            None => {
                return Err(format_err!("Root component url is required from RuntimeConfig."));
            }
        };

        let runner_map = self
            .runners
            .iter()
            .map(|(name, _)| {
                (
                    name.clone(),
                    RunnerRegistration {
                        source_name: name.clone(),
                        target_name: name.clone(),
                        source: cm_rust::RegistrationSource::Self_,
                    },
                )
            })
            .collect();

        let boot_resolver = if self.add_environment_resolvers {
            let boot_resolver = register_boot_resolver(&mut self.resolvers)?;
            register_appmgr_resolver(&mut self.resolvers, &runtime_config)?;
            boot_resolver
        } else {
            None
        };

        let runtime_config = Arc::new(runtime_config);
        let top_instance =
            Arc::new(ComponentManagerInstance::new(runtime_config.namespace_capabilities.clone()));
        let params = ModelParams {
            root_component_url: root_component_url.as_str().to_owned(),
            root_environment: Environment::new_root(
                &top_instance,
                RunnerRegistry::new(runner_map),
                self.resolvers,
                DebugRegistry::default(),
            ),
            runtime_config: Arc::clone(&runtime_config),
            top_instance,
        };
        let model = Model::new(params).await?;

        // Wrap BuiltinRunnerFactory in BuiltinRunner now that we have the definite RuntimeConfig.
        let builtin_runners = self
            .runners
            .into_iter()
            .map(|(name, runner)| {
                Arc::new(BuiltinRunner::new(name, runner, Arc::downgrade(&runtime_config)))
            })
            .collect();

        Ok(BuiltinEnvironment::new(
            model,
            root_component_url,
            runtime_config,
            builtin_runners,
            boot_resolver,
            self.utc_clock,
            self.inspector.unwrap_or(component::inspector().clone()),
            self.enable_hub,
        )
        .await?)
    }
}

/// The built-in environment consists of the set of the root services and framework services. Use
/// BuiltinEnvironmentBuilder to construct one.
///
/// The available built-in capabilities depends on the configuration provided in Arguments:
/// * If [RuntimeConfig::use_builtin_process_launcher] is true, a fuchsia.process.Launcher service
///   is available.
/// * If [RuntimeConfig::maintain_utc_clock] is true, a fuchsia.time.Maintenance service is
///   available.
pub struct BuiltinEnvironment {
    pub model: Arc<Model>,

    // Framework capabilities.
    pub boot_args: Arc<BootArguments>,
    pub debug_resource: Option<Arc<DebugResource>>,
    pub hypervisor_resource: Option<Arc<HypervisorResource>>,
    pub info_resource: Option<Arc<InfoResource>>,
    #[cfg(target_arch = "x86_64")]
    pub ioport_resource: Option<Arc<IoportResource>>,
    pub irq_resource: Option<Arc<IrqResource>>,
    pub kernel_stats: Option<Arc<KernelStats>>,
    pub process_launcher: Option<Arc<ProcessLauncher>>,
    pub root_job: Arc<RootJob>,
    pub root_job_for_inspect: Arc<RootJob>,
    pub read_only_log: Option<Arc<ReadOnlyLog>>,
    pub write_only_log: Option<Arc<WriteOnlyLog>>,
    pub mmio_resource: Option<Arc<MmioResource>>,
    pub root_resource: Option<Arc<RootResource>>,
    #[cfg(target_arch = "aarch64")]
    pub smc_resource: Option<Arc<SmcResource>>,
    pub system_controller: Arc<SystemController>,
    pub utc_time_maintainer: Option<Arc<UtcTimeMaintainer>>,
    pub vmex_resource: Option<Arc<VmexResource>>,

    pub work_scheduler: Arc<WorkScheduler>,
    pub realm_capability_host: Arc<RealmCapabilityHost>,
    pub storage_admin_capability_host: Arc<StorageAdmin>,
    pub hub: Option<Arc<Hub>>,
    pub builtin_runners: Vec<Arc<BuiltinRunner>>,
    pub event_registry: Arc<EventRegistry>,
    pub event_source_factory: Arc<EventSourceFactory>,
    pub stop_notifier: Arc<RootStopNotifier>,
    pub capability_ready_notifier: Arc<CapabilityReadyNotifier>,
    pub event_stream_provider: Arc<EventStreamProvider>,
    pub event_logger: Option<Arc<EventLogger>>,
    pub component_tree_stats: Arc<ComponentTreeStats<DiagnosticsTask>>,
    pub execution_mode: ExecutionMode,
    pub num_threads: usize,
    pub out_dir_contents: OutDirContents,
    pub inspector: Inspector,

    _service_fs_task: Option<fasync::Task<()>>,
}

impl BuiltinEnvironment {
    async fn new(
        model: Arc<Model>,
        root_component_url: Url,
        runtime_config: Arc<RuntimeConfig>,
        builtin_runners: Vec<Arc<BuiltinRunner>>,
        boot_resolver: Option<Arc<FuchsiaBootResolver>>,
        utc_clock: Option<Arc<Clock>>,
        inspector: Inspector,
        enable_hub: bool,
    ) -> Result<BuiltinEnvironment, Error> {
        let execution_mode = match runtime_config.debug {
            true => ExecutionMode::Debug,
            false => ExecutionMode::Production,
        };

        let num_threads = runtime_config.num_threads.clone();
        let out_dir_contents = runtime_config.out_dir_contents.clone();

        let event_logger = if runtime_config.log_all_events {
            let event_logger = Arc::new(EventLogger::new());
            model.root.hooks.install(event_logger.hooks()).await;
            Some(event_logger)
        } else {
            None
        };
        // Set up ProcessLauncher if available.
        let process_launcher = if runtime_config.use_builtin_process_launcher {
            let process_launcher = Arc::new(ProcessLauncher::new());
            model.root.hooks.install(process_launcher.hooks()).await;
            Some(process_launcher)
        } else {
            None
        };

        // Set up RootJob service.
        let root_job = RootJob::new(&ROOT_JOB_CAPABILITY_NAME, zx::Rights::SAME_RIGHTS);
        model.root.hooks.install(root_job.hooks()).await;

        // Set up RootJobForInspect service.
        let root_job_for_inspect = RootJob::new(
            &ROOT_JOB_FOR_INSPECT_CAPABILITY_NAME,
            zx::Rights::INSPECT
                | zx::Rights::ENUMERATE
                | zx::Rights::DUPLICATE
                | zx::Rights::TRANSFER
                | zx::Rights::GET_PROPERTY,
        );
        model.root.hooks.install(root_job_for_inspect.hooks()).await;

        let mmio_resource_handle =
            take_startup_handle(HandleType::MmioResource.into()).map(zx::Resource::from);

        let irq_resource_handle =
            take_startup_handle(HandleType::IrqResource.into()).map(zx::Resource::from);

        let root_resource_handle =
            take_startup_handle(HandleType::Resource.into()).map(zx::Resource::from);

        let system_resource_handle =
            take_startup_handle(HandleType::SystemResource.into()).map(zx::Resource::from);

        // Set up BootArguments service.
        let boot_args = BootArguments::new();
        model.root.hooks.install(boot_args.hooks()).await;

        // Set up KernelStats service.
        let info_resource_handle = system_resource_handle
            .as_ref()
            .map(|handle| {
                match handle.create_child(
                    zx::ResourceKind::SYSTEM,
                    None,
                    zx::sys::ZX_RSRC_SYSTEM_INFO_BASE,
                    1,
                    b"info",
                ) {
                    Ok(resource) => Some(resource),
                    Err(_) => None,
                }
            })
            .flatten();
        let kernel_stats = info_resource_handle.map(KernelStats::new);
        if let Some(kernel_stats) = kernel_stats.as_ref() {
            model.root.hooks.install(kernel_stats.hooks()).await;
        }

        // Set up ReadOnlyLog service.
        let read_only_log = root_resource_handle.as_ref().map(|handle| {
            ReadOnlyLog::new(
                handle
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("Failed to duplicate root resource handle"),
            )
        });
        if let Some(read_only_log) = read_only_log.as_ref() {
            model.root.hooks.install(read_only_log.hooks()).await;
        }

        // Set up WriteOnlyLog service.
        let write_only_log = root_resource_handle.as_ref().map(|handle| {
            WriteOnlyLog::new(zx::DebugLog::create(handle, zx::DebugLogOpts::empty()).unwrap())
        });
        if let Some(write_only_log) = write_only_log.as_ref() {
            model.root.hooks.install(write_only_log.hooks()).await;
        }

        // Register the UTC time maintainer.
        let utc_time_maintainer = if let Some(clock) = utc_clock {
            let utc_time_maintainer = Arc::new(UtcTimeMaintainer::new(clock));
            model.root.hooks.install(utc_time_maintainer.hooks()).await;
            Some(utc_time_maintainer)
        } else {
            None
        };

        // Set up the MmioResource service.
        let mmio_resource = mmio_resource_handle.map(MmioResource::new);
        if let Some(mmio_resource) = mmio_resource.as_ref() {
            model.root.hooks.install(mmio_resource.hooks()).await;
        }

        let _ioport_resource: Option<Arc<IoportResource>>;
        #[cfg(target_arch = "x86_64")]
        {
            let ioport_resource_handle =
                take_startup_handle(HandleType::IoportResource.into()).map(zx::Resource::from);
            _ioport_resource = ioport_resource_handle.map(IoportResource::new);
            if let Some(_ioport_resource) = _ioport_resource.as_ref() {
                model.root.hooks.install(_ioport_resource.hooks()).await;
            }
        }

        // Set up the IrqResource service.
        let irq_resource = irq_resource_handle.map(IrqResource::new);
        if let Some(irq_resource) = irq_resource.as_ref() {
            model.root.hooks.install(irq_resource.hooks()).await;
        }

        // Set up RootResource service.
        let root_resource = root_resource_handle.map(RootResource::new);
        if let Some(root_resource) = root_resource.as_ref() {
            model.root.hooks.install(root_resource.hooks()).await;
        }

        // Set up the SMC resource.
        let _smc_resource: Option<Arc<SmcResource>>;
        #[cfg(target_arch = "aarch64")]
        {
            let smc_resource_handle =
                take_startup_handle(HandleType::SmcResource.into()).map(zx::Resource::from);
            _smc_resource = smc_resource_handle.map(SmcResource::new);
            if let Some(_smc_resource) = _smc_resource.as_ref() {
                model.root.hooks.install(_smc_resource.hooks()).await;
            }
        }

        // Set up the DebugResource service.
        let debug_resource_handle = system_resource_handle
            .as_ref()
            .map(|handle| {
                match handle.create_child(
                    zx::ResourceKind::SYSTEM,
                    None,
                    zx::sys::ZX_RSRC_SYSTEM_DEBUG_BASE,
                    1,
                    b"debug",
                ) {
                    Ok(resource) => Some(resource),
                    Err(_) => None,
                }
            })
            .flatten();
        let debug_resource = debug_resource_handle.map(DebugResource::new);
        if let Some(debug_resource) = debug_resource.as_ref() {
            model.root.hooks.install(debug_resource.hooks()).await;
        }

        // Set up the HypervisorResource service.
        let hypervisor_resource_handle = system_resource_handle
            .as_ref()
            .map(|handle| {
                match handle.create_child(
                    zx::ResourceKind::SYSTEM,
                    None,
                    zx::sys::ZX_RSRC_SYSTEM_HYPERVISOR_BASE,
                    1,
                    b"hypervisor",
                ) {
                    Ok(resource) => Some(resource),
                    Err(_) => None,
                }
            })
            .flatten();
        let hypervisor_resource = hypervisor_resource_handle.map(HypervisorResource::new);
        if let Some(hypervisor_resource) = hypervisor_resource.as_ref() {
            model.root.hooks.install(hypervisor_resource.hooks()).await;
        }

        // Set up the InfoResource service.
        let info_resource_handle = system_resource_handle
            .as_ref()
            .map(|handle| {
                match handle.create_child(
                    zx::ResourceKind::SYSTEM,
                    None,
                    zx::sys::ZX_RSRC_SYSTEM_INFO_BASE,
                    1,
                    b"info",
                ) {
                    Ok(resource) => Some(resource),
                    Err(_) => None,
                }
            })
            .flatten();
        let info_resource = info_resource_handle.map(InfoResource::new);
        if let Some(info_resource) = info_resource.as_ref() {
            model.root.hooks.install(info_resource.hooks()).await;
        }

        // Set up the VmexResource service.
        let vmex_resource_handle = system_resource_handle
            .as_ref()
            .map(|handle| {
                match handle.create_child(
                    zx::ResourceKind::SYSTEM,
                    None,
                    zx::sys::ZX_RSRC_SYSTEM_VMEX_BASE,
                    1,
                    b"vmex",
                ) {
                    Ok(resource) => Some(resource),
                    Err(_) => None,
                }
            })
            .flatten();
        let vmex_resource = vmex_resource_handle.map(VmexResource::new);
        if let Some(vmex_resource) = vmex_resource.as_ref() {
            model.root.hooks.install(vmex_resource.hooks()).await;
        }

        // Set up System Controller service.
        let system_controller =
            Arc::new(SystemController::new(Arc::downgrade(&model), SHUTDOWN_TIMEOUT));
        model.root.hooks.install(system_controller.hooks()).await;

        // Set up work scheduler.
        let work_scheduler =
            WorkScheduler::new(Arc::new(Arc::downgrade(&model)) as Arc<dyn Binder>).await;
        model.root.hooks.install(work_scheduler.hooks()).await;

        // Set up the realm service.
        let realm_capability_host =
            Arc::new(RealmCapabilityHost::new(Arc::downgrade(&model), runtime_config.clone()));
        model.root.hooks.install(realm_capability_host.hooks()).await;

        // Set up the storage admin protocol
        let storage_admin_capability_host = Arc::new(StorageAdmin::new());
        model.root.hooks.install(storage_admin_capability_host.hooks()).await;

        // Set up the builtin runners.
        for runner in &builtin_runners {
            model.root.hooks.install(runner.hooks()).await;
        }

        // Set up the boot resolver so it is routable from "above root".
        if let Some(boot_resolver) = boot_resolver {
            model.root.hooks.install(boot_resolver.hooks()).await;
        }

        // Set up the root realm stop notifier.
        let stop_notifier = Arc::new(RootStopNotifier::new());
        model.root.hooks.install(stop_notifier.hooks()).await;

        let hub = if enable_hub {
            let hub = Arc::new(Hub::new(root_component_url.as_str().to_owned())?);
            model.root.hooks.install(hub.hooks()).await;
            Some(hub)
        } else {
            None
        };

        // Set up the Component Tree Diagnostics runtime statistics.
        let component_tree_stats =
            ComponentTreeStats::new(inspector.root().create_child("cpu_stats")).await;
        component_tree_stats.track_component_manager_stats().await;
        model.root.hooks.install(component_tree_stats.hooks()).await;

        // Serve stats about inspect in a lazy node.
        inspector.root().record_lazy_values("inspect_stats", || {
            async move {
                let inspector = Inspector::new();
                let stats_node = inspector.root().create_child("inspect_stats");
                inspector.write_stats_to(&stats_node);
                inspector.root().record(stats_node);
                Ok(inspector)
            }
            .boxed()
        });

        // Set up the capability ready notifier.
        let capability_ready_notifier =
            Arc::new(CapabilityReadyNotifier::new(Arc::downgrade(&model)));
        model.root.hooks.install(capability_ready_notifier.hooks()).await;

        // Set up the event registry.
        let event_registry = {
            let mut event_registry = EventRegistry::new(Arc::downgrade(&model));
            event_registry.register_synthesis_provider(
                EventType::CapabilityReady,
                capability_ready_notifier.clone(),
            );
            event_registry
                .register_synthesis_provider(EventType::Running, Arc::new(RunningProvider::new()));
            Arc::new(event_registry)
        };
        model.root.hooks.install(event_registry.hooks()).await;

        let event_stream_provider = Arc::new(EventStreamProvider::new(
            Arc::downgrade(&event_registry),
            execution_mode.clone(),
        ));

        // Set up the event source factory.
        let event_source_factory = Arc::new(EventSourceFactory::new(
            Arc::downgrade(&model),
            Arc::downgrade(&event_registry),
            Arc::downgrade(&event_stream_provider),
            execution_mode.clone(),
        ));
        model.root.hooks.install(event_source_factory.hooks()).await;
        model.root.hooks.install(event_stream_provider.hooks()).await;

        Ok(BuiltinEnvironment {
            model,
            boot_args,
            process_launcher,
            root_job,
            root_job_for_inspect,
            kernel_stats,
            read_only_log,
            write_only_log,
            debug_resource,
            mmio_resource,
            hypervisor_resource,
            info_resource,
            irq_resource,
            #[cfg(target_arch = "x86_64")]
            ioport_resource: _ioport_resource,
            #[cfg(target_arch = "aarch64")]
            smc_resource: _smc_resource,
            vmex_resource,
            root_resource,
            system_controller,
            utc_time_maintainer,
            work_scheduler,
            realm_capability_host,
            storage_admin_capability_host,
            hub,
            builtin_runners,
            event_registry,
            event_source_factory,
            stop_notifier,
            capability_ready_notifier,
            event_stream_provider,
            event_logger,
            component_tree_stats,
            execution_mode,
            num_threads,
            out_dir_contents,
            inspector,
            _service_fs_task: None,
        })
    }

    /// Setup a ServiceFs that contains the Hub and (optionally) the `EventSource` service.
    async fn create_service_fs<'a>(&self) -> Result<ServiceFs<ServiceObj<'a, ()>>, Error> {
        if let None = self.hub {
            bail!("Hub must be enabled if OutDirContents is not `None`");
        }

        // Create the ServiceFs
        let mut service_fs = ServiceFs::new();

        // Setup the hub
        let (hub_proxy, hub_server_end) = create_proxy::<DirectoryMarker>().unwrap();
        self.hub
            .as_ref()
            .unwrap()
            .open_root(OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE, hub_server_end.into_channel())
            .await?;
        service_fs.add_remote("hub", hub_proxy);

        // If component manager is in debug mode, create an event source scoped at the
        // root and offer it via ServiceFs to the outside world.
        if self.execution_mode.is_debug() {
            let event_source = self.event_source_factory.create_for_debug().await?;
            service_fs.dir("svc").add_fidl_service(move |stream| {
                let event_source = event_source.clone();
                // TODO(geb): Practically speaking, calling detach() here isn't a problem because
                // the builtin environment is never dropped. However, ideally, the task would be
                // stored in the BuiltinEnvironment. But it is not easy to do that here since this
                // callback is not async, so we can't grab a Mutex.
                event_source.serve(stream).detach();
            });
        }

        inspect_runtime::serve(component::inspector(), &mut service_fs)
            .map_err(|err| ModelError::Inspect { err })
            .unwrap_or_else(|err| {
                warn!("Failed to serve inspect: {:?}", err);
            });

        Ok(service_fs)
    }

    /// Bind ServiceFs to a provided channel
    async fn bind_service_fs(&mut self, channel: zx::Channel) -> Result<(), Error> {
        let mut service_fs = self.create_service_fs().await?;

        // Bind to the channel
        service_fs
            .serve_connection(channel)
            .map_err(|err| ModelError::namespace_creation_failed(err))?;

        self.emit_diagnostics(&mut service_fs).unwrap_or_else(|err| {
            warn!("Failed to serve diagnostics: {:?}", err);
        });

        // Start up ServiceFs
        self._service_fs_task = Some(fasync::Task::spawn(async move {
            service_fs.collect::<()>().await;
        }));
        Ok(())
    }

    /// Bind ServiceFs to the outgoing directory of this component, if it exists.
    pub async fn bind_service_fs_to_out(&mut self) -> Result<(), Error> {
        if let Some(handle) = fuchsia_runtime::take_startup_handle(
            fuchsia_runtime::HandleType::DirectoryRequest.into(),
        ) {
            self.bind_service_fs(zx::Channel::from(handle)).await?;
        } else {
            // The component manager running on startup does not get a directory handle. If it was
            // to run as a component itself, it'd get one. When we don't have a handle to the out
            // directory, create one.
            self.bind_service_fs(zx::Channel::create().expect("make channel").1).await?;
        }
        Ok(())
    }

    /// Bind ServiceFs to a new channel and return the Hub directory.
    /// Used mainly by integration tests.
    pub async fn bind_service_fs_for_hub(&mut self) -> Result<DirectoryProxy, Error> {
        // Create a channel that ServiceFs will operate on
        let (service_fs_proxy, service_fs_server_end) = create_proxy::<DirectoryMarker>().unwrap();

        self.bind_service_fs(service_fs_server_end.into_channel()).await?;

        // Open the Hub from within ServiceFs
        let (hub_client_end, hub_server_end) = create_endpoints::<DirectoryMarker>().unwrap();
        service_fs_proxy
            .open(
                OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                MODE_TYPE_DIRECTORY,
                "hub",
                ServerEnd::new(hub_server_end.into_channel()),
            )
            .map_err(|err| ModelError::namespace_creation_failed(err))?;
        let hub_proxy = hub_client_end.into_proxy().unwrap();

        Ok(hub_proxy)
    }

    fn emit_diagnostics<'a>(
        &self,
        service_fs: &mut ServiceFs<ServiceObj<'a, ()>>,
    ) -> Result<(), ModelError> {
        let (service_fs_proxy, service_fs_server_end) = create_proxy::<DirectoryMarker>().unwrap();
        service_fs
            .serve_connection(service_fs_server_end.into_channel())
            .map_err(|err| ModelError::namespace_creation_failed(err))?;

        let (node, server_end) = fidl::endpoints::create_proxy::<NodeMarker>().unwrap();
        service_fs_proxy
            .open(
                OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                MODE_TYPE_DIRECTORY,
                "diagnostics",
                ServerEnd::new(server_end.into_channel()),
            )
            .map_err(|err| ModelError::namespace_creation_failed(err))?;

        self.capability_ready_notifier.register_component_manager_capability("diagnostics", node);

        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn emit_diagnostics_for_test<'a>(
        &self,
        service_fs: &mut ServiceFs<ServiceObj<'a, ()>>,
    ) -> Result<(), ModelError> {
        self.emit_diagnostics(service_fs)
    }

    pub async fn wait_for_root_stop(&self) {
        self.stop_notifier.wait_for_root_stop().await;
    }

    pub async fn run_root(&mut self) -> Result<(), Error> {
        match self.out_dir_contents {
            OutDirContents::None => {
                info!("Field `out_dir_contents` is set to None.");
                Ok(())
            }
            OutDirContents::Hub => {
                info!("Field `out_dir_contents` is set to Hub.");
                self.bind_service_fs_to_out().await?;
                self.model.start().await;
                component::health().set_ok();
                Ok(self.wait_for_root_stop().await)
            }
            OutDirContents::Svc => {
                info!("Field `out_dir_contents` is set to Svc.");
                let hub_proxy = self.bind_service_fs_for_hub().await?;
                self.model.start().await;
                // List the services exposed by the root component.
                let expose_dir_proxy = io_util::open_directory(
                    &hub_proxy,
                    &PathBuf::from("exec/expose"),
                    OPEN_RIGHT_READABLE | OPEN_RIGHT_WRITABLE,
                )
                .expect("Failed to open directory");

                // Bind the root component's expose/ to out/svc of this component, so sysmgr can
                // find it and route service connections to it.
                let mut fs = ServiceFs::<ServiceObj<'_, ()>>::new();
                fs.add_remote("svc", expose_dir_proxy);

                fs.take_and_serve_directory_handle()?;

                component::health().set_ok();

                Ok(fs.collect::<()>().await)
            }
        }
    }
}

// Creates a FuchsiaBootResolver if the /boot directory is installed in component_manager's
// namespace, and registers it with the ResolverRegistry. The resolver is returned to so that
// it can be installed as a Builtin capability.
fn register_boot_resolver(
    resolvers: &mut ResolverRegistry,
) -> Result<Option<Arc<FuchsiaBootResolver>>, Error> {
    let boot_resolver = FuchsiaBootResolver::new().context("Failed to create boot resolver")?;
    match boot_resolver {
        None => {
            info!("No /boot directory in namespace, fuchsia-boot resolver unavailable");
            Ok(None)
        }
        Some(boot_resolver) => {
            let resolver = Arc::new(boot_resolver);
            resolvers
                .register(BOOT_SCHEME.to_string(), Box::new(BuiltinResolver(resolver.clone())));
            Ok(Some(resolver))
        }
    }
}

/// Adds the namespace resolvers according to the policy in the RuntimeConfig.
fn register_appmgr_resolver(
    resolvers: &mut ResolverRegistry,
    runtime_config: &RuntimeConfig,
) -> Result<(), Error> {
    if let Some(loader) = connect_sys_loader()? {
        match &runtime_config.builtin_pkg_resolver {
            BuiltinPkgResolver::None => {
                warn!(
                    "Appmgr bridge package resolver is available, but not enabled, verify \
                    configuration correctness"
                );
            }
            BuiltinPkgResolver::AppmgrBridge => {
                resolvers.register(
                    fuchsia_pkg_resolver::SCHEME.to_string(),
                    Box::new(fuchsia_pkg_resolver::FuchsiaPkgResolver::new(loader)),
                );
            }
        }
    }
    Ok(())
}

/// Checks if the appmgr loader service is available through our namespace and connects to it if
/// so. If not available, returns Ok(None).
fn connect_sys_loader() -> Result<Option<LoaderProxy>, Error> {
    let service_path = PathBuf::from(format!("/svc/{}", LoaderMarker::NAME));
    if !service_path.exists() {
        return Ok(None);
    }

    let loader = client::connect_to_service::<LoaderMarker>()
        .context("error connecting to system loader")?;
    return Ok(Some(loader));
}
