// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    cm_fidl_validator,
    cm_rust_derive::{
        CapabilityDeclCommon, ExposeDeclCommon, FidlDecl, OfferDeclCommon, UseDeclCommon,
    },
    cm_types, fidl_fuchsia_data as fdata, fidl_fuchsia_io2 as fio2, fidl_fuchsia_sys2 as fsys,
    from_enum::FromEnum,
    lazy_static::lazy_static,
    std::collections::HashMap,
    std::convert::{From, TryFrom},
    std::fmt,
    std::path::PathBuf,
    std::str::FromStr,
    thiserror::Error,
};

pub mod data;

lazy_static! {
    static ref DATA_TYPENAME: CapabilityName = CapabilityName("Data".to_string());
    static ref CACHE_TYPENAME: CapabilityName = CapabilityName("Cache".to_string());
    static ref META_TYPENAME: CapabilityName = CapabilityName("Meta".to_string());
}

/// Converts a fidl object into its corresponding native representation.
pub trait FidlIntoNative<T> {
    fn fidl_into_native(self) -> T;
}

pub trait NativeIntoFidl<T> {
    fn native_into_fidl(self) -> T;
}

/// Generates `FidlIntoNative` and `NativeIntoFidl` implementations that leaves the input unchanged.
macro_rules! fidl_translations_identical {
    ($into_type:ty) => {
        impl FidlIntoNative<$into_type> for $into_type {
            fn fidl_into_native(self) -> $into_type {
                self
            }
        }
        impl NativeIntoFidl<$into_type> for $into_type {
            fn native_into_fidl(self) -> Self {
                self
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ComponentDecl {
    pub program: Option<ProgramDecl>,
    pub uses: Vec<UseDecl>,
    pub exposes: Vec<ExposeDecl>,
    pub offers: Vec<OfferDecl>,
    pub capabilities: Vec<CapabilityDecl>,
    pub children: Vec<ChildDecl>,
    pub collections: Vec<CollectionDecl>,
    pub facets: Option<fsys::Object>,
    pub environments: Vec<EnvironmentDecl>,
}

impl FidlIntoNative<ComponentDecl> for fsys::ComponentDecl {
    fn fidl_into_native(self) -> ComponentDecl {
        // When transforming ExposeDecl::Service and OfferDecl::Service from
        // FIDL to native, we aggregate the declarations by target.
        let mut exposes = vec![];
        if let Some(e) = self.exposes {
            let mut services: HashMap<(ExposeTarget, CapabilityName), Vec<_>> = HashMap::new();
            for expose in e.into_iter() {
                match expose {
                    fsys::ExposeDecl::Service(s) => services
                        .entry((
                            s.target.unwrap().fidl_into_native(),
                            s.target_name.unwrap().fidl_into_native(),
                        ))
                        .or_default()
                        .push(ServiceSource::<ExposeSource> {
                            source: s.source.unwrap().fidl_into_native(),
                            source_name: s.source_name.unwrap().fidl_into_native(),
                        }),
                    fsys::ExposeDecl::Protocol(ls) => {
                        exposes.push(ExposeDecl::Protocol(ls.fidl_into_native()))
                    }
                    fsys::ExposeDecl::Directory(d) => {
                        exposes.push(ExposeDecl::Directory(d.fidl_into_native()))
                    }
                    fsys::ExposeDecl::Runner(r) => {
                        exposes.push(ExposeDecl::Runner(r.fidl_into_native()))
                    }
                    fsys::ExposeDecl::Resolver(r) => {
                        exposes.push(ExposeDecl::Resolver(r.fidl_into_native()))
                    }
                    fsys::ExposeDeclUnknown!() => panic!("invalid variant"),
                }
            }
            for ((target, target_name), sources) in services.into_iter() {
                exposes.push(ExposeDecl::Service(ExposeServiceDecl {
                    sources,
                    target,
                    target_name,
                }))
            }
        }
        let mut offers = vec![];
        if let Some(o) = self.offers {
            let mut services: HashMap<(OfferTarget, CapabilityName), Vec<_>> = HashMap::new();
            for offer in o.into_iter() {
                match offer {
                    fsys::OfferDecl::Service(s) => services
                        .entry((
                            s.target.unwrap().fidl_into_native(),
                            s.target_name.unwrap().fidl_into_native(),
                        ))
                        .or_default()
                        .push(ServiceSource::<OfferSource> {
                            source: s.source.unwrap().fidl_into_native(),
                            source_name: s.source_name.unwrap().fidl_into_native(),
                        }),
                    fsys::OfferDecl::Protocol(ls) => {
                        offers.push(OfferDecl::Protocol(ls.fidl_into_native()))
                    }
                    fsys::OfferDecl::Directory(d) => {
                        offers.push(OfferDecl::Directory(d.fidl_into_native()))
                    }
                    fsys::OfferDecl::Storage(s) => {
                        offers.push(OfferDecl::Storage(s.fidl_into_native()))
                    }
                    fsys::OfferDecl::Runner(s) => {
                        offers.push(OfferDecl::Runner(s.fidl_into_native()))
                    }
                    fsys::OfferDecl::Resolver(r) => {
                        offers.push(OfferDecl::Resolver(r.fidl_into_native()))
                    }
                    fsys::OfferDecl::Event(e) => {
                        offers.push(OfferDecl::Event(e.fidl_into_native()))
                    }
                    fsys::OfferDeclUnknown!() => panic!("invalid variant"),
                }
            }
            for ((target, target_name), sources) in services.into_iter() {
                offers.push(OfferDecl::Service(OfferServiceDecl { sources, target, target_name }))
            }
        }
        ComponentDecl {
            program: self.program.map(FidlIntoNative::fidl_into_native),
            uses: self
                .uses
                .into_iter()
                .map(IntoIterator::into_iter)
                .flatten()
                .map(FidlIntoNative::fidl_into_native)
                .collect(),
            exposes,
            offers,
            capabilities: self
                .capabilities
                .into_iter()
                .map(IntoIterator::into_iter)
                .flatten()
                .map(FidlIntoNative::fidl_into_native)
                .collect(),
            children: self
                .children
                .into_iter()
                .map(IntoIterator::into_iter)
                .flatten()
                .map(FidlIntoNative::fidl_into_native)
                .collect(),
            collections: self
                .collections
                .into_iter()
                .map(IntoIterator::into_iter)
                .flatten()
                .map(FidlIntoNative::fidl_into_native)
                .collect(),
            facets: self.facets.map(FidlIntoNative::fidl_into_native),
            environments: self
                .environments
                .into_iter()
                .map(IntoIterator::into_iter)
                .flatten()
                .map(FidlIntoNative::fidl_into_native)
                .collect(),
        }
    }
}

impl NativeIntoFidl<fsys::ComponentDecl> for ComponentDecl {
    fn native_into_fidl(self) -> fsys::ComponentDecl {
        // When transforming ExposeDecl::Service and OfferDecl::Service from
        // native to FIDL, we disaggregate the declarations.
        let mut exposes = vec![];
        for expose in self.exposes.into_iter() {
            match expose {
                ExposeDecl::Service(s) => {
                    for es in s.sources.into_iter() {
                        exposes.push(fsys::ExposeDecl::Service(fsys::ExposeServiceDecl {
                            source: Some(es.source.native_into_fidl()),
                            source_name: Some(es.source_name.native_into_fidl()),
                            target: Some(s.target.clone().native_into_fidl()),
                            target_name: Some(s.target_name.clone().native_into_fidl()),
                            ..fsys::ExposeServiceDecl::EMPTY
                        }))
                    }
                }
                ExposeDecl::Protocol(ls) => {
                    exposes.push(fsys::ExposeDecl::Protocol(ls.native_into_fidl()))
                }
                ExposeDecl::Directory(d) => {
                    exposes.push(fsys::ExposeDecl::Directory(d.native_into_fidl()))
                }
                ExposeDecl::Runner(r) => {
                    exposes.push(fsys::ExposeDecl::Runner(r.native_into_fidl()))
                }
                ExposeDecl::Resolver(r) => {
                    exposes.push(fsys::ExposeDecl::Resolver(r.native_into_fidl()))
                }
            }
        }
        let mut offers = vec![];
        for offer in self.offers.into_iter() {
            match offer {
                OfferDecl::Service(s) => {
                    for os in s.sources.into_iter() {
                        offers.push(fsys::OfferDecl::Service(fsys::OfferServiceDecl {
                            source: Some(os.source.native_into_fidl()),
                            source_name: Some(os.source_name.native_into_fidl()),
                            target: Some(s.target.clone().native_into_fidl()),
                            target_name: Some(s.target_name.clone().native_into_fidl()),
                            ..fsys::OfferServiceDecl::EMPTY
                        }))
                    }
                }
                OfferDecl::Protocol(ls) => {
                    offers.push(fsys::OfferDecl::Protocol(ls.native_into_fidl()))
                }
                OfferDecl::Directory(d) => {
                    offers.push(fsys::OfferDecl::Directory(d.native_into_fidl()))
                }
                OfferDecl::Storage(s) => {
                    offers.push(fsys::OfferDecl::Storage(s.native_into_fidl()))
                }
                OfferDecl::Runner(s) => offers.push(fsys::OfferDecl::Runner(s.native_into_fidl())),
                OfferDecl::Resolver(r) => {
                    offers.push(fsys::OfferDecl::Resolver(r.native_into_fidl()))
                }
                OfferDecl::Event(e) => offers.push(fsys::OfferDecl::Event(e.native_into_fidl())),
            }
        }
        fsys::ComponentDecl {
            program: self.program.map(NativeIntoFidl::native_into_fidl),
            uses: native_into_fidl_vec(self.uses),
            exposes: if exposes.is_empty() { None } else { Some(exposes) },
            offers: if offers.is_empty() { None } else { Some(offers) },
            capabilities: native_into_fidl_vec(self.capabilities),
            children: native_into_fidl_vec(self.children),
            collections: native_into_fidl_vec(self.collections),
            facets: self.facets.map(NativeIntoFidl::native_into_fidl),
            environments: native_into_fidl_vec(self.environments),
            ..fsys::ComponentDecl::EMPTY
        }
    }
}

fn native_into_fidl_vec<N: NativeIntoFidl<F>, F>(vec: Vec<N>) -> Option<Vec<F>> {
    if vec.is_empty() {
        None
    } else {
        Some(vec.into_iter().map(NativeIntoFidl::native_into_fidl).collect())
    }
}

impl ComponentDecl {
    /// Returns the runner used by this component, or `None` if this is a non-executable component.
    pub fn get_runner(&self) -> Option<&CapabilityName> {
        self.program.as_ref().and_then(|p| p.runner.as_ref())
    }

    /// Returns the `StorageDecl` corresponding to `storage_name`.
    pub fn find_storage_source<'a>(
        &'a self,
        storage_name: &CapabilityName,
    ) -> Option<&'a StorageDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Storage(s) if &s.name == storage_name => Some(s),
            _ => None,
        })
    }

    /// Returns the `ProtocolDecl` corresponding to `protocol_name`.
    pub fn find_protocol_source<'a>(
        &'a self,
        protocol_name: &CapabilityName,
    ) -> Option<&'a ProtocolDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Protocol(r) if &r.name == protocol_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `DirectoryDecl` corresponding to `directory_name`.
    pub fn find_directory_source<'a>(
        &'a self,
        directory_name: &CapabilityName,
    ) -> Option<&'a DirectoryDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Directory(r) if &r.name == directory_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `RunnerDecl` corresponding to `runner_name`.
    pub fn find_runner_source<'a>(
        &'a self,
        runner_name: &CapabilityName,
    ) -> Option<&'a RunnerDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Runner(r) if &r.name == runner_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `ResolverDecl` corresponding to `resolver_name`.
    pub fn find_resolver_source<'a>(
        &'a self,
        resolver_name: &CapabilityName,
    ) -> Option<&'a ResolverDecl> {
        self.capabilities.iter().find_map(|c| match c {
            CapabilityDecl::Resolver(r) if &r.name == resolver_name => Some(r),
            _ => None,
        })
    }

    /// Returns the `CollectionDecl` corresponding to `collection_name`.
    pub fn find_collection<'a>(&'a self, collection_name: &str) -> Option<&'a CollectionDecl> {
        self.collections.iter().find(|c| c.name == collection_name)
    }

    /// Indicates whether the capability specified by `target_name` is exposed to the framework.
    pub fn is_protocol_exposed_to_framework(&self, in_target_name: &CapabilityName) -> bool {
        self.exposes.iter().any(|expose| match expose {
            ExposeDecl::Protocol(ExposeProtocolDecl { target, target_name, .. })
                if target == &ExposeTarget::Framework =>
            {
                target_name == in_target_name
            }
            _ => false,
        })
    }

    /// Indicates whether the capability specified by `source_name` is requested.
    pub fn uses_protocol(&self, source_name: &CapabilityName) -> bool {
        self.uses.iter().any(|use_decl| match use_decl {
            UseDecl::Protocol(ls) => &ls.source_name == source_name,
            _ => false,
        })
    }
}

#[derive(FidlDecl, FromEnum, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fsys::UseDecl")]
pub enum UseDecl {
    Service(UseServiceDecl),
    Protocol(UseProtocolDecl),
    Directory(UseDirectoryDecl),
    Storage(UseStorageDecl),
    Event(UseEventDecl),
    EventStream(UseEventStreamDecl),
}

#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::UseServiceDecl")]
pub struct UseServiceDecl {
    pub source: UseSource,
    pub source_name: CapabilityName,
    pub target_path: CapabilityPath,
}

#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::UseProtocolDecl")]
pub struct UseProtocolDecl {
    pub source: UseSource,
    pub source_name: CapabilityName,
    pub target_path: CapabilityPath,
}

#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::UseDirectoryDecl")]
pub struct UseDirectoryDecl {
    pub source: UseSource,
    pub source_name: CapabilityName,
    pub target_path: CapabilityPath,
    pub rights: fio2::Operations,
    pub subdir: Option<PathBuf>,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::UseStorageDecl")]
pub struct UseStorageDecl {
    pub source_name: CapabilityName,
    pub target_path: CapabilityPath,
}

impl SourceName for UseStorageDecl {
    fn source_name(&self) -> &CapabilityName {
        &self.source_name
    }
}

impl UseDeclCommon for UseStorageDecl {
    fn source(&self) -> &UseSource {
        &UseSource::Parent
    }
}

#[derive(FidlDecl, UseDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::UseEventDecl")]
pub struct UseEventDecl {
    pub source: UseSource,
    pub source_name: CapabilityName,
    pub target_name: CapabilityName,
    pub filter: Option<HashMap<String, DictionaryValue>>,
    pub mode: EventMode,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::EventSubscription")]
pub struct EventSubscription {
    pub event_name: String,
    pub mode: EventMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventMode {
    Async,
    Sync,
}

impl NativeIntoFidl<fsys::EventMode> for EventMode {
    fn native_into_fidl(self) -> fsys::EventMode {
        match self {
            EventMode::Sync => fsys::EventMode::Sync,
            EventMode::Async => fsys::EventMode::Async,
        }
    }
}

impl FidlIntoNative<EventMode> for fsys::EventMode {
    fn fidl_into_native(self) -> EventMode {
        match self {
            fsys::EventMode::Sync => EventMode::Sync,
            fsys::EventMode::Async => EventMode::Async,
        }
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::UseEventStreamDecl")]
pub struct UseEventStreamDecl {
    pub name: CapabilityName,
    pub subscriptions: Vec<EventSubscription>,
}

#[derive(FromEnum, Debug, Clone, PartialEq, Eq)]
pub enum OfferDecl {
    Service(OfferServiceDecl),
    Protocol(OfferProtocolDecl),
    Directory(OfferDirectoryDecl),
    Storage(OfferStorageDecl),
    Runner(OfferRunnerDecl),
    Resolver(OfferResolverDecl),
    Event(OfferEventDecl),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferServiceDecl {
    pub sources: Vec<ServiceSource<OfferSource>>,
    pub target: OfferTarget,
    pub target_name: CapabilityName,
}

impl SourceName for OfferServiceDecl {
    fn source_name(&self) -> &CapabilityName {
        // NOTE: The FidlIntoNative transformation guarantees there is at least one source.
        // TODO(fxbug.dev/71881): Generalize to all sources.
        &self.sources[0].source_name
    }
}

impl OfferDeclCommon for OfferServiceDecl {
    fn source(&self) -> &OfferSource {
        // NOTE: The FidlIntoNative transformation guarantees there is at least one source.
        // TODO(fxbug.dev/71881): Generalize to all sources.
        &self.sources[0].source
    }

    fn target(&self) -> &OfferTarget {
        &self.target
    }

    fn target_name(&self) -> &CapabilityName {
        &self.target_name
    }
}

#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::OfferProtocolDecl")]
pub struct OfferProtocolDecl {
    pub source: OfferSource,
    pub source_name: CapabilityName,
    pub target: OfferTarget,
    pub target_name: CapabilityName,
    pub dependency_type: DependencyType,
}

#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::OfferDirectoryDecl")]
pub struct OfferDirectoryDecl {
    pub source: OfferSource,
    pub source_name: CapabilityName,
    pub target: OfferTarget,
    pub target_name: CapabilityName,
    pub dependency_type: DependencyType,
    pub rights: Option<fio2::Operations>,
    pub subdir: Option<PathBuf>,
}

#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::OfferStorageDecl")]
pub struct OfferStorageDecl {
    pub source: OfferSource,
    pub source_name: CapabilityName,
    pub target: OfferTarget,
    pub target_name: CapabilityName,
}

#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::OfferRunnerDecl")]
pub struct OfferRunnerDecl {
    pub source: OfferSource,
    pub source_name: CapabilityName,
    pub target: OfferTarget,
    pub target_name: CapabilityName,
}

#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::OfferResolverDecl")]
pub struct OfferResolverDecl {
    pub source: OfferSource,
    pub source_name: CapabilityName,
    pub target: OfferTarget,
    pub target_name: CapabilityName,
}

#[derive(FidlDecl, OfferDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::OfferEventDecl")]
pub struct OfferEventDecl {
    pub source: OfferSource,
    pub source_name: CapabilityName,
    pub target: OfferTarget,
    pub target_name: CapabilityName,
    pub filter: Option<HashMap<String, DictionaryValue>>,
    pub mode: EventMode,
}

#[derive(FromEnum, Debug, Clone, PartialEq, Eq)]
pub enum ExposeDecl {
    Service(ExposeServiceDecl),
    Protocol(ExposeProtocolDecl),
    Directory(ExposeDirectoryDecl),
    Runner(ExposeRunnerDecl),
    Resolver(ExposeResolverDecl),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposeServiceDecl {
    pub sources: Vec<ServiceSource<ExposeSource>>,
    pub target: ExposeTarget,
    pub target_name: CapabilityName,
}

impl SourceName for ExposeServiceDecl {
    fn source_name(&self) -> &CapabilityName {
        // NOTE: The FidlIntoNative transformation guarantees there is at least one source.
        // TODO(fxbug.dev/71881): Generalize to all sources.
        &self.sources[0].source_name
    }
}

impl ExposeDeclCommon for ExposeServiceDecl {
    fn source(&self) -> &ExposeSource {
        // NOTE: The FidlIntoNative transformation guarantees there is at least one source.
        // TODO(fxbug.dev/71881): Generalize to all sources.
        &self.sources[0].source
    }

    fn target(&self) -> &ExposeTarget {
        &self.target
    }

    fn target_name(&self) -> &CapabilityName {
        &self.target_name
    }
}

#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ExposeProtocolDecl")]
pub struct ExposeProtocolDecl {
    pub source: ExposeSource,
    pub source_name: CapabilityName,
    pub target: ExposeTarget,
    pub target_name: CapabilityName,
}

#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ExposeDirectoryDecl")]
pub struct ExposeDirectoryDecl {
    pub source: ExposeSource,
    pub source_name: CapabilityName,
    pub target: ExposeTarget,
    pub target_name: CapabilityName,
    pub rights: Option<fio2::Operations>,
    pub subdir: Option<PathBuf>,
}

#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ExposeRunnerDecl")]
pub struct ExposeRunnerDecl {
    pub source: ExposeSource,
    pub source_name: CapabilityName,
    pub target: ExposeTarget,
    pub target_name: CapabilityName,
}

#[derive(FidlDecl, ExposeDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ExposeResolverDecl")]
pub struct ExposeResolverDecl {
    pub source: ExposeSource,
    pub source_name: CapabilityName,
    pub target: ExposeTarget,
    pub target_name: CapabilityName,
}

#[derive(FidlDecl, FromEnum, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fsys::CapabilityDecl")]
pub enum CapabilityDecl {
    Service(ServiceDecl),
    Protocol(ProtocolDecl),
    Directory(DirectoryDecl),
    Storage(StorageDecl),
    Runner(RunnerDecl),
    Resolver(ResolverDecl),
}

#[derive(FidlDecl, CapabilityDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ServiceDecl")]
pub struct ServiceDecl {
    pub name: CapabilityName,
    pub source_path: CapabilityPath,
}

#[derive(FidlDecl, CapabilityDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ProtocolDecl")]
pub struct ProtocolDecl {
    pub name: CapabilityName,
    pub source_path: CapabilityPath,
}

#[derive(FidlDecl, CapabilityDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::DirectoryDecl")]
pub struct DirectoryDecl {
    pub name: CapabilityName,
    pub source_path: CapabilityPath,
    pub rights: fio2::Operations,
}

#[derive(FidlDecl, CapabilityDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::StorageDecl")]
pub struct StorageDecl {
    pub name: CapabilityName,
    pub source: StorageDirectorySource,
    pub backing_dir: CapabilityName,
    pub subdir: Option<PathBuf>,
}

#[derive(FidlDecl, CapabilityDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::RunnerDecl")]
pub struct RunnerDecl {
    pub name: CapabilityName,
    pub source_path: CapabilityPath,
}

#[derive(FidlDecl, CapabilityDeclCommon, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ResolverDecl")]
pub struct ResolverDecl {
    pub name: CapabilityName,
    pub source_path: CapabilityPath,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ChildDecl")]
pub struct ChildDecl {
    pub name: String,
    pub url: String,
    pub startup: fsys::StartupMode,
    pub environment: Option<String>,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::CollectionDecl")]
pub struct CollectionDecl {
    pub name: String,
    pub durability: fsys::Durability,
    pub environment: Option<String>,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::EnvironmentDecl")]
pub struct EnvironmentDecl {
    pub name: String,
    pub extends: fsys::EnvironmentExtends,
    pub runners: Vec<RunnerRegistration>,
    pub resolvers: Vec<ResolverRegistration>,
    pub debug_capabilities: Vec<DebugRegistration>,
    pub stop_timeout_ms: Option<u32>,
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::RunnerRegistration")]
pub struct RunnerRegistration {
    pub source_name: CapabilityName,
    pub target_name: CapabilityName,
    pub source: RegistrationSource,
}

impl SourceName for RunnerRegistration {
    fn source_name(&self) -> &CapabilityName {
        &self.source_name
    }
}

impl RegistrationDeclCommon for RunnerRegistration {
    const TYPE: &'static str = "runner";

    fn source(&self) -> &RegistrationSource {
        &self.source
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::ResolverRegistration")]
pub struct ResolverRegistration {
    pub resolver: CapabilityName,
    pub source: RegistrationSource,
    pub scheme: String,
}

impl SourceName for ResolverRegistration {
    fn source_name(&self) -> &CapabilityName {
        &self.resolver
    }
}

impl RegistrationDeclCommon for ResolverRegistration {
    const TYPE: &'static str = "resolver";

    fn source(&self) -> &RegistrationSource {
        &self.source
    }
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_union = "fsys::DebugRegistration")]
pub enum DebugRegistration {
    Protocol(DebugProtocolRegistration),
}

#[derive(FidlDecl, Debug, Clone, PartialEq, Eq)]
#[fidl_decl(fidl_table = "fsys::DebugProtocolRegistration")]
pub struct DebugProtocolRegistration {
    pub source_name: CapabilityName,
    pub source: RegistrationSource,
    pub target_name: CapabilityName,
}

#[derive(FidlDecl, Debug, Clone, PartialEq)]
#[fidl_decl(fidl_table = "fsys::ProgramDecl")]
pub struct ProgramDecl {
    pub runner: Option<CapabilityName>,
    pub info: fdata::Dictionary,
}

impl Default for ProgramDecl {
    fn default() -> Self {
        Self { runner: None, info: fdata::Dictionary::EMPTY.clone() }
    }
}

fidl_translations_identical!(u32);
fidl_translations_identical!(bool);
fidl_translations_identical!(String);
fidl_translations_identical!(fsys::StartupMode);
fidl_translations_identical!(fsys::Durability);
fidl_translations_identical!(fsys::Object);
fidl_translations_identical!(fdata::Dictionary);
fidl_translations_identical!(fio2::Operations);
fidl_translations_identical!(fsys::EnvironmentExtends);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyType {
    Strong,
    Weak,
    WeakForMigration,
}

impl FidlIntoNative<DependencyType> for fsys::DependencyType {
    fn fidl_into_native(self) -> DependencyType {
        match self {
            fsys::DependencyType::Strong => DependencyType::Strong,
            fsys::DependencyType::Weak => DependencyType::Weak,
            fsys::DependencyType::WeakForMigration => DependencyType::WeakForMigration,
        }
    }
}

impl NativeIntoFidl<fsys::DependencyType> for DependencyType {
    fn native_into_fidl(self) -> fsys::DependencyType {
        match self {
            DependencyType::Strong => fsys::DependencyType::Strong,
            DependencyType::Weak => fsys::DependencyType::Weak,
            DependencyType::WeakForMigration => fsys::DependencyType::WeakForMigration,
        }
    }
}

/// A path to a capability.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityPath {
    /// The directory containing the last path element, e.g. `/svc/foo` in `/svc/foo/bar`.
    pub dirname: String,
    /// The last path element: e.g. `bar` in `/svc/foo/bar`.
    pub basename: String,
}

impl CapabilityPath {
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(self.to_string())
    }

    /// Splits the path according to "/", ignoring empty path components
    pub fn split(&self) -> Vec<String> {
        self.to_string().split("/").map(|s| s.to_string()).filter(|s| !s.is_empty()).collect()
    }
}

impl FromStr for CapabilityPath {
    type Err = Error;

    fn from_str(path: &str) -> Result<CapabilityPath, Error> {
        cm_types::Path::validate(path)
            .map_err(|_| Error::InvalidCapabilityPath { raw: path.to_string() })?;
        let idx = path.rfind('/').expect("path validation is wrong");
        Ok(CapabilityPath {
            dirname: if idx == 0 { "/".to_string() } else { path[0..idx].to_string() },
            basename: path[idx + 1..].to_string(),
        })
    }
}

impl TryFrom<&str> for CapabilityPath {
    type Error = Error;

    fn try_from(path: &str) -> Result<CapabilityPath, Error> {
        Self::from_str(path)
    }
}

impl fmt::Display for CapabilityPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if &self.dirname == "/" {
            write!(f, "/{}", self.basename)
        } else {
            write!(f, "{}/{}", self.dirname, self.basename)
        }
    }
}

impl UseDecl {
    pub fn path(&self) -> Option<&CapabilityPath> {
        match self {
            UseDecl::Service(d) => Some(&d.target_path),
            UseDecl::Protocol(d) => Some(&d.target_path),
            UseDecl::Directory(d) => Some(&d.target_path),
            UseDecl::Storage(d) => Some(&d.target_path),
            UseDecl::Event(_) | UseDecl::EventStream(_) => None,
        }
    }

    pub fn name(&self) -> Option<&CapabilityName> {
        match self {
            UseDecl::Event(event_decl) => Some(&event_decl.source_name),
            UseDecl::Storage(storage_decl) => Some(&storage_decl.source_name),
            UseDecl::EventStream(event_stream_decl) => Some(&event_stream_decl.name),
            UseDecl::Service(_) | UseDecl::Protocol(_) | UseDecl::Directory(_) => None,
        }
    }
}

/// A named capability.
///
/// Unlike a `CapabilityPath`, a `CapabilityName` doesn't encode any form
/// of hierarchy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityName(pub String);

impl CapabilityName {
    pub fn str(&self) -> &str {
        &self.0
    }
}

impl From<CapabilityName> for String {
    fn from(name: CapabilityName) -> String {
        name.0
    }
}

impl From<&str> for CapabilityName {
    fn from(name: &str) -> CapabilityName {
        CapabilityName(name.to_string())
    }
}

impl From<String> for CapabilityName {
    fn from(name: String) -> CapabilityName {
        CapabilityName(name)
    }
}

impl<'a> PartialEq<&'a str> for CapabilityName {
    fn eq(&self, other: &&'a str) -> bool {
        self.0 == *other
    }
}

impl fmt::Display for CapabilityName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The trait for all declarations that have a source name.
pub trait SourceName {
    fn source_name(&self) -> &CapabilityName;
}

/// The common properties of a [Use](fsys2::UseDecl) declaration.
pub trait UseDeclCommon: SourceName + Send + Sync {
    fn source(&self) -> &UseSource;
}

/// The common properties of a Registration-with-environment declaration.
pub trait RegistrationDeclCommon: SourceName + Send + Sync {
    /// The name of the registration type, for error messages.
    const TYPE: &'static str;
    fn source(&self) -> &RegistrationSource;
}

/// The common properties of an [Offer](fsys2::OfferDecl) declaration.
pub trait OfferDeclCommon: SourceName + Send + Sync {
    fn target_name(&self) -> &CapabilityName;
    fn target(&self) -> &OfferTarget;
    fn source(&self) -> &OfferSource;
}

/// The common properties of an [Expose](fsys2::ExposeDecl) declaration.
pub trait ExposeDeclCommon: SourceName + Send + Sync {
    fn target_name(&self) -> &CapabilityName;
    fn target(&self) -> &ExposeTarget;
    fn source(&self) -> &ExposeSource;
}

/// The common properties of a [Capability](fsys2::CapabilityDecl) declaration.
pub trait CapabilityDeclCommon: Send + Sync {
    fn name(&self) -> &CapabilityName;
}

/// A named capability type.
///
/// `CapabilityTypeName` provides a user friendly type encoding for a capability.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CapabilityTypeName {
    Directory,
    Event,
    EventStream,
    Protocol,
    Resolver,
    Runner,
    Service,
    Storage,
}

impl fmt::Display for CapabilityTypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let display_name = match &self {
            CapabilityTypeName::Directory => "directory",
            CapabilityTypeName::Event => "event",
            CapabilityTypeName::EventStream => "event_stream",
            CapabilityTypeName::Protocol => "protocol",
            CapabilityTypeName::Resolver => "resolver",
            CapabilityTypeName::Runner => "runner",
            CapabilityTypeName::Service => "service",
            CapabilityTypeName::Storage => "storage",
        };
        write!(f, "{}", display_name)
    }
}

// TODO: Runners and third parties can use this to parse `facets`.
impl FidlIntoNative<HashMap<String, Value>> for fsys::Object {
    fn fidl_into_native(self) -> HashMap<String, Value> {
        from_fidl_obj(self)
    }
}

impl FidlIntoNative<HashMap<String, DictionaryValue>> for fdata::Dictionary {
    fn fidl_into_native(self) -> HashMap<String, DictionaryValue> {
        from_fidl_dict(self)
    }
}

impl NativeIntoFidl<fdata::Dictionary> for HashMap<String, DictionaryValue> {
    fn native_into_fidl(self) -> fdata::Dictionary {
        to_fidl_dict(self)
    }
}

impl FidlIntoNative<CapabilityPath> for String {
    fn fidl_into_native(self) -> CapabilityPath {
        self.as_str().parse().expect("invalid capability path")
    }
}

impl NativeIntoFidl<String> for CapabilityPath {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

impl FidlIntoNative<PathBuf> for String {
    fn fidl_into_native(self) -> PathBuf {
        PathBuf::from(self)
    }
}

impl NativeIntoFidl<String> for PathBuf {
    fn native_into_fidl(self) -> String {
        self.into_os_string().into_string().expect("invalid utf8")
    }
}

impl FidlIntoNative<CapabilityName> for String {
    fn fidl_into_native(self) -> CapabilityName {
        self.into()
    }
}

impl NativeIntoFidl<String> for CapabilityName {
    fn native_into_fidl(self) -> String {
        self.to_string()
    }
}

#[derive(Debug, PartialEq)]
pub enum Value {
    Bit(bool),
    Inum(i64),
    Fnum(f64),
    Str(String),
    Vec(Vec<Value>),
    Obj(HashMap<String, Value>),
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictionaryValue {
    Str(String),
    StrVec(Vec<String>),
    Null,
}

impl FidlIntoNative<Value> for Option<Box<fsys::Value>> {
    fn fidl_into_native(self) -> Value {
        match self {
            Some(v) => match *v {
                fsys::Value::Bit(b) => Value::Bit(b),
                fsys::Value::Inum(i) => Value::Inum(i),
                fsys::Value::Fnum(f) => Value::Fnum(f),
                fsys::Value::Str(s) => Value::Str(s),
                fsys::Value::Vec(v) => Value::Vec(from_fidl_vec(v)),
                fsys::Value::Obj(d) => Value::Obj(from_fidl_obj(d)),
            },
            None => Value::Null,
        }
    }
}

impl FidlIntoNative<DictionaryValue> for Option<Box<fdata::DictionaryValue>> {
    fn fidl_into_native(self) -> DictionaryValue {
        match self {
            Some(v) => match *v {
                fdata::DictionaryValue::Str(s) => DictionaryValue::Str(s),
                fdata::DictionaryValue::StrVec(ss) => DictionaryValue::StrVec(ss),
            },
            None => DictionaryValue::Null,
        }
    }
}

impl NativeIntoFidl<Option<Box<fdata::DictionaryValue>>> for DictionaryValue {
    fn native_into_fidl(self) -> Option<Box<fdata::DictionaryValue>> {
        match self {
            DictionaryValue::Str(s) => Some(Box::new(fdata::DictionaryValue::Str(s))),
            DictionaryValue::StrVec(ss) => Some(Box::new(fdata::DictionaryValue::StrVec(ss))),
            DictionaryValue::Null => None,
        }
    }
}

fn from_fidl_vec(vec: fsys::Vector) -> Vec<Value> {
    vec.values.into_iter().map(|v| v.fidl_into_native()).collect()
}

fn from_fidl_obj(obj: fsys::Object) -> HashMap<String, Value> {
    obj.entries.into_iter().map(|e| (e.key, e.value.fidl_into_native())).collect()
}

fn from_fidl_dict(dict: fdata::Dictionary) -> HashMap<String, DictionaryValue> {
    match dict.entries {
        Some(entries) => entries.into_iter().map(|e| (e.key, e.value.fidl_into_native())).collect(),
        _ => HashMap::new(),
    }
}

fn to_fidl_dict(dict: HashMap<String, DictionaryValue>) -> fdata::Dictionary {
    fdata::Dictionary {
        entries: Some(
            dict.into_iter()
                .map(|(key, value)| fdata::DictionaryEntry { key, value: value.native_into_fidl() })
                .collect(),
        ),
        ..fdata::Dictionary::EMPTY
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseSource {
    Parent,
    Framework,
    Debug,
    Capability(CapabilityName),
    Child(String),
}

impl FidlIntoNative<UseSource> for fsys::Ref {
    fn fidl_into_native(self) -> UseSource {
        match self {
            fsys::Ref::Parent(_) => UseSource::Parent,
            fsys::Ref::Framework(_) => UseSource::Framework,
            fsys::Ref::Debug(_) => UseSource::Debug,
            fsys::Ref::Capability(c) => UseSource::Capability(c.name.into()),
            fsys::Ref::Child(c) => UseSource::Child(c.name),
            _ => panic!("invalid UseSource variant"),
        }
    }
}

impl NativeIntoFidl<fsys::Ref> for UseSource {
    fn native_into_fidl(self) -> fsys::Ref {
        match self {
            UseSource::Parent => fsys::Ref::Parent(fsys::ParentRef {}),
            UseSource::Framework => fsys::Ref::Framework(fsys::FrameworkRef {}),
            UseSource::Debug => fsys::Ref::Debug(fsys::DebugRef {}),
            UseSource::Capability(name) => {
                fsys::Ref::Capability(fsys::CapabilityRef { name: name.to_string() })
            }
            UseSource::Child(name) => fsys::Ref::Child(fsys::ChildRef { name, collection: None }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferSource {
    Framework,
    Parent,
    Child(String),
    Collection(String),
    Self_,
    Capability(CapabilityName),
}

impl FidlIntoNative<OfferSource> for fsys::Ref {
    fn fidl_into_native(self) -> OfferSource {
        match self {
            fsys::Ref::Parent(_) => OfferSource::Parent,
            fsys::Ref::Self_(_) => OfferSource::Self_,
            fsys::Ref::Child(c) => OfferSource::Child(c.name),
            fsys::Ref::Collection(c) => OfferSource::Collection(c.name),
            fsys::Ref::Framework(_) => OfferSource::Framework,
            fsys::Ref::Capability(c) => OfferSource::Capability(c.name.into()),
            _ => panic!("invalid OfferSource variant"),
        }
    }
}

impl NativeIntoFidl<fsys::Ref> for OfferSource {
    fn native_into_fidl(self) -> fsys::Ref {
        match self {
            OfferSource::Parent => fsys::Ref::Parent(fsys::ParentRef {}),
            OfferSource::Self_ => fsys::Ref::Self_(fsys::SelfRef {}),
            OfferSource::Child(child_name) => {
                fsys::Ref::Child(fsys::ChildRef { name: child_name, collection: None })
            }
            OfferSource::Collection(name) => fsys::Ref::Collection(fsys::CollectionRef { name }),
            OfferSource::Framework => fsys::Ref::Framework(fsys::FrameworkRef {}),
            OfferSource::Capability(name) => {
                fsys::Ref::Capability(fsys::CapabilityRef { name: name.to_string() })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposeSource {
    Self_,
    Child(String),
    Collection(String),
    Framework,
    Capability(CapabilityName),
}

impl FidlIntoNative<ExposeSource> for fsys::Ref {
    fn fidl_into_native(self) -> ExposeSource {
        match self {
            fsys::Ref::Self_(_) => ExposeSource::Self_,
            fsys::Ref::Child(c) => ExposeSource::Child(c.name),
            fsys::Ref::Collection(c) => ExposeSource::Collection(c.name),
            fsys::Ref::Framework(_) => ExposeSource::Framework,
            fsys::Ref::Capability(c) => ExposeSource::Capability(c.name.into()),
            _ => panic!("invalid ExposeSource variant"),
        }
    }
}

impl NativeIntoFidl<fsys::Ref> for ExposeSource {
    fn native_into_fidl(self) -> fsys::Ref {
        match self {
            ExposeSource::Self_ => fsys::Ref::Self_(fsys::SelfRef {}),
            ExposeSource::Child(child_name) => {
                fsys::Ref::Child(fsys::ChildRef { name: child_name, collection: None })
            }
            ExposeSource::Collection(name) => fsys::Ref::Collection(fsys::CollectionRef { name }),
            ExposeSource::Framework => fsys::Ref::Framework(fsys::FrameworkRef {}),
            ExposeSource::Capability(name) => {
                fsys::Ref::Capability(fsys::CapabilityRef { name: name.to_string() })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExposeTarget {
    Parent,
    Framework,
}

impl FidlIntoNative<ExposeTarget> for fsys::Ref {
    fn fidl_into_native(self) -> ExposeTarget {
        match self {
            fsys::Ref::Parent(_) => ExposeTarget::Parent,
            fsys::Ref::Framework(_) => ExposeTarget::Framework,
            _ => panic!("invalid ExposeTarget variant"),
        }
    }
}

impl NativeIntoFidl<fsys::Ref> for ExposeTarget {
    fn native_into_fidl(self) -> fsys::Ref {
        match self {
            ExposeTarget::Parent => fsys::Ref::Parent(fsys::ParentRef {}),
            ExposeTarget::Framework => fsys::Ref::Framework(fsys::FrameworkRef {}),
        }
    }
}

/// A source for a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSource<T> {
    /// The provider of the service, relative to a component.
    pub source: T,
    /// The name of the service.
    pub source_name: CapabilityName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageDirectorySource {
    Parent,
    Self_,
    Child(String),
}

impl FidlIntoNative<StorageDirectorySource> for fsys::Ref {
    fn fidl_into_native(self) -> StorageDirectorySource {
        match self {
            fsys::Ref::Parent(_) => StorageDirectorySource::Parent,
            fsys::Ref::Self_(_) => StorageDirectorySource::Self_,
            fsys::Ref::Child(c) => StorageDirectorySource::Child(c.name),
            _ => panic!("invalid OfferDirectorySource variant"),
        }
    }
}

impl NativeIntoFidl<fsys::Ref> for StorageDirectorySource {
    fn native_into_fidl(self) -> fsys::Ref {
        match self {
            StorageDirectorySource::Parent => fsys::Ref::Parent(fsys::ParentRef {}),
            StorageDirectorySource::Self_ => fsys::Ref::Self_(fsys::SelfRef {}),
            StorageDirectorySource::Child(child_name) => {
                fsys::Ref::Child(fsys::ChildRef { name: child_name, collection: None })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationSource {
    Parent,
    Self_,
    Child(String),
}

impl FidlIntoNative<RegistrationSource> for fsys::Ref {
    fn fidl_into_native(self) -> RegistrationSource {
        match self {
            fsys::Ref::Parent(_) => RegistrationSource::Parent,
            fsys::Ref::Self_(_) => RegistrationSource::Self_,
            fsys::Ref::Child(c) => RegistrationSource::Child(c.name),
            _ => panic!("invalid RegistrationSource variant"),
        }
    }
}

impl NativeIntoFidl<fsys::Ref> for RegistrationSource {
    fn native_into_fidl(self) -> fsys::Ref {
        match self {
            RegistrationSource::Parent => fsys::Ref::Parent(fsys::ParentRef {}),
            RegistrationSource::Self_ => fsys::Ref::Self_(fsys::SelfRef {}),
            RegistrationSource::Child(child_name) => {
                fsys::Ref::Child(fsys::ChildRef { name: child_name, collection: None })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OfferTarget {
    Child(String),
    Collection(String),
}

impl FidlIntoNative<OfferTarget> for fsys::Ref {
    fn fidl_into_native(self) -> OfferTarget {
        match self {
            fsys::Ref::Child(c) => OfferTarget::Child(c.name),
            fsys::Ref::Collection(c) => OfferTarget::Collection(c.name),
            _ => panic!("invalid OfferTarget variant"),
        }
    }
}

impl NativeIntoFidl<fsys::Ref> for OfferTarget {
    fn native_into_fidl(self) -> fsys::Ref {
        match self {
            OfferTarget::Child(child_name) => {
                fsys::Ref::Child(fsys::ChildRef { name: child_name, collection: None })
            }
            OfferTarget::Collection(collection_name) => {
                fsys::Ref::Collection(fsys::CollectionRef { name: collection_name })
            }
        }
    }
}

/// Converts the contents of a CM-FIDL declaration and produces the equivalent CM-Rust
/// struct.
/// This function applies cm_fidl_validator to check correctness.
impl TryFrom<fsys::ComponentDecl> for ComponentDecl {
    type Error = Error;

    fn try_from(decl: fsys::ComponentDecl) -> Result<Self, Self::Error> {
        cm_fidl_validator::validate(&decl).map_err(|err| Error::Validate { err })?;
        Ok(decl.fidl_into_native())
    }
}

// Converts the contents of a CM-Rust declaration into a CM_FIDL declaration
impl TryFrom<ComponentDecl> for fsys::ComponentDecl {
    type Error = Error;
    fn try_from(decl: ComponentDecl) -> Result<Self, Self::Error> {
        Ok(decl.native_into_fidl())
    }
}

/// Errors produced by cm_rust.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Fidl validation failed: {}", err)]
    Validate {
        #[source]
        err: cm_fidl_validator::ErrorList,
    },
    #[error("Invalid capability path: {}", raw)]
    InvalidCapabilityPath { raw: String },
}

#[cfg(test)]
mod tests {
    use {super::*, maplit::hashmap, std::convert::TryInto};

    macro_rules! test_try_from_decl {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    result = $result:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    {
                        let res = ComponentDecl::try_from($input).expect("try_from failed");
                        assert_eq!(res, $result);
                    }
                    {
                        let res = fsys::ComponentDecl::try_from($result).expect("try_from failed");
                        assert_eq!(res, $input);
                    }
                }
            )+
        }
    }

    macro_rules! test_fidl_into_and_from {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    input_type = $input_type:ty,
                    result = $result:expr,
                    result_type = $result_type:ty,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    {
                        let res: Vec<$result_type> =
                            $input.into_iter().map(|e| e.fidl_into_native()).collect();
                        assert_eq!(res, $result);
                    }
                    {
                        let res: Vec<$input_type> =
                            $result.into_iter().map(|e| e.native_into_fidl()).collect();
                        assert_eq!(res, $input);
                    }
                }
            )+
        }
    }

    macro_rules! test_fidl_into {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    result = $result:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    test_fidl_into_helper($input, $result);
                }
            )+
        }
    }

    fn test_fidl_into_helper<T, U>(input: T, expected_res: U)
    where
        T: FidlIntoNative<U>,
        U: std::cmp::PartialEq + std::fmt::Debug,
    {
        let res: U = input.fidl_into_native();
        assert_eq!(res, expected_res);
    }

    macro_rules! test_capability_path {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    result = $result:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    test_capability_path_helper($input, $result);
                }
            )+
        }
    }

    fn test_capability_path_helper(input: &str, result: Result<CapabilityPath, Error>) {
        let res = CapabilityPath::try_from(input);
        assert_eq!(format!("{:?}", res), format!("{:?}", result));
        if let Ok(p) = res {
            assert_eq!(&p.to_string(), input);
        }
    }

    test_try_from_decl! {
        try_from_empty => {
            input = fsys::ComponentDecl {
                program: None,
                uses: None,
                exposes: None,
                offers: None,
                capabilities: None,
                children: None,
                collections: None,
                facets: None,
                environments: None,
                ..fsys::ComponentDecl::EMPTY
            },
            result = ComponentDecl {
                program: None,
                uses: vec![],
                exposes: vec![],
                offers: vec![],
                capabilities: vec![],
                children: vec![],
                collections: vec![],
                facets: None,
                environments: vec![],
            },
        },
        try_from_all => {
            input = fsys::ComponentDecl {
                program: Some(fsys::ProgramDecl {
                    runner: Some("elf".to_string()),
                    info: Some(fdata::Dictionary {
                        entries: Some(vec![
                            fdata::DictionaryEntry {
                                key: "args".to_string(),
                                value: Some(Box::new(fdata::DictionaryValue::StrVec(vec!["foo".to_string(), "bar".to_string()]))),
                            },
                            fdata::DictionaryEntry {
                                key: "binary".to_string(),
                                value: Some(Box::new(fdata::DictionaryValue::Str("bin/app".to_string()))),
                            },
                        ]),
                        ..fdata::Dictionary::EMPTY
                    }),
                    ..fsys::ProgramDecl::EMPTY
                }),
                uses: Some(vec![
                    fsys::UseDecl::Service(fsys::UseServiceDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("netstack".to_string()),
                        target_path: Some("/svc/mynetstack".to_string()),
                        ..fsys::UseServiceDecl::EMPTY
                    }),
                    fsys::UseDecl::Protocol(fsys::UseProtocolDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("legacy_netstack".to_string()),
                        target_path: Some("/svc/legacy_mynetstack".to_string()),
                        ..fsys::UseProtocolDecl::EMPTY
                    }),
                    fsys::UseDecl::Protocol(fsys::UseProtocolDecl {
                        source: Some(fsys::Ref::Child(fsys::ChildRef { name: "echo".to_string(), collection: None})),
                        source_name: Some("echo_service".to_string()),
                        target_path: Some("/svc/echo_service".to_string()),
                        ..fsys::UseProtocolDecl::EMPTY
                    }),
                    fsys::UseDecl::Directory(fsys::UseDirectoryDecl {
                        source: Some(fsys::Ref::Framework(fsys::FrameworkRef {})),
                        source_name: Some("dir".to_string()),
                        target_path: Some("/data".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: Some("foo/bar".to_string()),
                        ..fsys::UseDirectoryDecl::EMPTY
                    }),
                    fsys::UseDecl::Storage(fsys::UseStorageDecl {
                        source_name: Some("cache".to_string()),
                        target_path: Some("/cache".to_string()),
                        ..fsys::UseStorageDecl::EMPTY
                    }),
                    fsys::UseDecl::Storage(fsys::UseStorageDecl {
                        source_name: Some("temp".to_string()),
                        target_path: Some("/temp".to_string()),
                        ..fsys::UseStorageDecl::EMPTY
                    }),
                    fsys::UseDecl::Event(fsys::UseEventDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("capability_ready".to_string()),
                        target_name: Some("diagnostics_ready".to_string()),
                        filter: Some(fdata::Dictionary{
                            entries: Some(vec![
                               fdata::DictionaryEntry {
                                   key: "path".to_string(),
                                   value: Some(Box::new(fdata::DictionaryValue::Str("/diagnostics".to_string()))),
                               },
                            ]),
                            ..fdata::Dictionary::EMPTY
                        }),
                        mode: Some(fsys::EventMode::Sync),
                        ..fsys::UseEventDecl::EMPTY
                    }),
                ]),
                exposes: Some(vec![
                    fsys::ExposeDecl::Protocol(fsys::ExposeProtocolDecl {
                        source: Some(fsys::Ref::Child(fsys::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("legacy_netstack".to_string()),
                        target_name: Some("legacy_mynetstack".to_string()),
                        target: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        ..fsys::ExposeProtocolDecl::EMPTY
                    }),
                    fsys::ExposeDecl::Directory(fsys::ExposeDirectoryDecl {
                        source: Some(fsys::Ref::Child(fsys::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("dir".to_string()),
                        target_name: Some("data".to_string()),
                        target: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: Some("foo/bar".to_string()),
                        ..fsys::ExposeDirectoryDecl::EMPTY
                    }),
                    fsys::ExposeDecl::Runner(fsys::ExposeRunnerDecl {
                        source: Some(fsys::Ref::Child(fsys::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("elf".to_string()),
                        target: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        target_name: Some("elf".to_string()),
                        ..fsys::ExposeRunnerDecl::EMPTY
                    }),
                    fsys::ExposeDecl::Resolver(fsys::ExposeResolverDecl{
                        source: Some(fsys::Ref::Child(fsys::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("pkg".to_string()),
                        target: Some(fsys::Ref::Parent(fsys::ParentRef{})),
                        target_name: Some("pkg".to_string()),
                        ..fsys::ExposeResolverDecl::EMPTY
                    }),
                    fsys::ExposeDecl::Service(fsys::ExposeServiceDecl {
                        source: Some(fsys::Ref::Child(fsys::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("netstack1".to_string()),
                        target_name: Some("mynetstack".to_string()),
                        target: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        ..fsys::ExposeServiceDecl::EMPTY
                    }),
                    fsys::ExposeDecl::Service(fsys::ExposeServiceDecl {
                        source: Some(fsys::Ref::Child(fsys::ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("netstack2".to_string()),
                        target_name: Some("mynetstack".to_string()),
                        target: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        ..fsys::ExposeServiceDecl::EMPTY
                    }),
                ]),
                offers: Some(vec![
                    fsys::OfferDecl::Protocol(fsys::OfferProtocolDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("legacy_netstack".to_string()),
                        target: Some(fsys::Ref::Child(
                           fsys::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("legacy_mynetstack".to_string()),
                        dependency_type: Some(fsys::DependencyType::WeakForMigration),
                        ..fsys::OfferProtocolDecl::EMPTY
                    }),
                    fsys::OfferDecl::Directory(fsys::OfferDirectoryDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("dir".to_string()),
                        target: Some(fsys::Ref::Collection(
                            fsys::CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("data".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(fsys::DependencyType::Strong),
                        ..fsys::OfferDirectoryDecl::EMPTY
                    }),
                    fsys::OfferDecl::Storage(fsys::OfferStorageDecl {
                        source_name: Some("cache".to_string()),
                        source: Some(fsys::Ref::Self_(fsys::SelfRef {})),
                        target: Some(fsys::Ref::Collection(
                            fsys::CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("cache".to_string()),
                        ..fsys::OfferStorageDecl::EMPTY
                    }),
                    fsys::OfferDecl::Runner(fsys::OfferRunnerDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("elf".to_string()),
                        target: Some(fsys::Ref::Child(
                           fsys::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("elf2".to_string()),
                        ..fsys::OfferRunnerDecl::EMPTY
                    }),
                    fsys::OfferDecl::Resolver(fsys::OfferResolverDecl{
                        source: Some(fsys::Ref::Parent(fsys::ParentRef{})),
                        source_name: Some("pkg".to_string()),
                        target: Some(fsys::Ref::Child(
                           fsys::ChildRef {
                              name: "echo".to_string(),
                              collection: None,
                           }
                        )),
                        target_name: Some("pkg".to_string()),
                        ..fsys::OfferResolverDecl::EMPTY
                    }),
                    fsys::OfferDecl::Event(fsys::OfferEventDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("started".to_string()),
                        target: Some(fsys::Ref::Child(
                           fsys::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("mystarted".to_string()),
                        filter: Some(fdata::Dictionary {
                            entries: Some(vec![
                               fdata::DictionaryEntry {
                                   key: "path".to_string(),
                                   value: Some(Box::new(fdata::DictionaryValue::Str("/a".to_string()))),
                               },
                            ]),
                            ..fdata::Dictionary::EMPTY
                        }),
                        mode: Some(fsys::EventMode::Sync),
                        ..fsys::OfferEventDecl::EMPTY
                    }),
                    fsys::OfferDecl::Service(fsys::OfferServiceDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("netstack1".to_string()),
                        target: Some(fsys::Ref::Child(
                           fsys::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("mynetstack".to_string()),
                        ..fsys::OfferServiceDecl::EMPTY
                    }),
                    fsys::OfferDecl::Service(fsys::OfferServiceDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("netstack2".to_string()),
                        target: Some(fsys::Ref::Child(
                           fsys::ChildRef {
                               name: "echo".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("mynetstack".to_string()),
                        ..fsys::OfferServiceDecl::EMPTY
                    }),
                ]),
                capabilities: Some(vec![
                    fsys::CapabilityDecl::Service(fsys::ServiceDecl {
                        name: Some("netstack".to_string()),
                        source_path: Some("/netstack".to_string()),
                        ..fsys::ServiceDecl::EMPTY
                    }),
                    fsys::CapabilityDecl::Protocol(fsys::ProtocolDecl {
                        name: Some("netstack2".to_string()),
                        source_path: Some("/netstack2".to_string()),
                        ..fsys::ProtocolDecl::EMPTY
                    }),
                    fsys::CapabilityDecl::Directory(fsys::DirectoryDecl {
                        name: Some("data".to_string()),
                        source_path: Some("/data".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        ..fsys::DirectoryDecl::EMPTY
                    }),
                    fsys::CapabilityDecl::Storage(fsys::StorageDecl {
                        name: Some("cache".to_string()),
                        backing_dir: Some("data".to_string()),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        subdir: Some("cache".to_string()),
                        ..fsys::StorageDecl::EMPTY
                    }),
                    fsys::CapabilityDecl::Runner(fsys::RunnerDecl {
                        name: Some("elf".to_string()),
                        source_path: Some("/elf".to_string()),
                        ..fsys::RunnerDecl::EMPTY
                    }),
                    fsys::CapabilityDecl::Resolver(fsys::ResolverDecl {
                        name: Some("pkg".to_string()),
                        source_path: Some("/pkg_resolver".to_string()),
                        ..fsys::ResolverDecl::EMPTY
                    }),
                ]),
                children: Some(vec![
                     fsys::ChildDecl {
                         name: Some("netstack".to_string()),
                         url: Some("fuchsia-pkg://fuchsia.com/netstack#meta/netstack.cm"
                                   .to_string()),
                         startup: Some(fsys::StartupMode::Lazy),
                         environment: None,
                         ..fsys::ChildDecl::EMPTY
                     },
                     fsys::ChildDecl {
                         name: Some("gtest".to_string()),
                         url: Some("fuchsia-pkg://fuchsia.com/gtest#meta/gtest.cm".to_string()),
                         startup: Some(fsys::StartupMode::Lazy),
                         environment: None,
                         ..fsys::ChildDecl::EMPTY
                     },
                     fsys::ChildDecl {
                         name: Some("echo".to_string()),
                         url: Some("fuchsia-pkg://fuchsia.com/echo#meta/echo.cm"
                                   .to_string()),
                         startup: Some(fsys::StartupMode::Eager),
                         environment: Some("test_env".to_string()),
                         ..fsys::ChildDecl::EMPTY
                     },
                ]),
                collections: Some(vec![
                     fsys::CollectionDecl {
                         name: Some("modular".to_string()),
                         durability: Some(fsys::Durability::Persistent),
                         environment: None,
                         ..fsys::CollectionDecl::EMPTY
                     },
                     fsys::CollectionDecl {
                         name: Some("tests".to_string()),
                         durability: Some(fsys::Durability::Transient),
                         environment: Some("test_env".to_string()),
                         ..fsys::CollectionDecl::EMPTY
                     },
                ]),
                facets: Some(fsys::Object{entries: vec![
                    fsys::Entry{
                        key: "author".to_string(),
                        value: Some(Box::new(fsys::Value::Str("Fuchsia".to_string()))),
                    },
                ]}),
                environments: Some(vec![
                    fsys::EnvironmentDecl {
                        name: Some("test_env".to_string()),
                        extends: Some(fsys::EnvironmentExtends::Realm),
                        runners: Some(vec![
                            fsys::RunnerRegistration {
                                source_name: Some("runner".to_string()),
                                source: Some(fsys::Ref::Child(fsys::ChildRef {
                                    name: "gtest".to_string(),
                                    collection: None,
                                })),
                                target_name: Some("gtest-runner".to_string()),
                                ..fsys::RunnerRegistration::EMPTY
                            }
                        ]),
                        resolvers: Some(vec![
                            fsys::ResolverRegistration {
                                resolver: Some("pkg_resolver".to_string()),
                                source: Some(fsys::Ref::Parent(fsys::ParentRef{})),
                                scheme: Some("fuchsia-pkg".to_string()),
                                ..fsys::ResolverRegistration::EMPTY
                            }
                        ]),
                        debug_capabilities: Some(vec![
                         fsys::DebugRegistration::Protocol(fsys::DebugProtocolRegistration {
                             source_name: Some("some_protocol".to_string()),
                             source: Some(fsys::Ref::Child(fsys::ChildRef {
                                 name: "gtest".to_string(),
                                 collection: None,
                             })),
                             target_name: Some("some_protocol".to_string()),
                             ..fsys::DebugProtocolRegistration::EMPTY
                            })
                        ]),
                        stop_timeout_ms: Some(4567),
                        ..fsys::EnvironmentDecl::EMPTY
                    }
                ]),
                ..fsys::ComponentDecl::EMPTY
            },
            result = {
                ComponentDecl {
                    program: Some(ProgramDecl {
                        runner: Some("elf".try_into().unwrap()),
                        info: fdata::Dictionary {
                            entries: Some(vec![
                                fdata::DictionaryEntry {
                                    key: "args".to_string(),
                                    value: Some(Box::new(fdata::DictionaryValue::StrVec(vec!["foo".to_string(), "bar".to_string()]))),
                                },
                                fdata::DictionaryEntry{
                                    key: "binary".to_string(),
                                    value: Some(Box::new(fdata::DictionaryValue::Str("bin/app".to_string()))),
                                },
                            ]),
                            ..fdata::Dictionary::EMPTY
                        },
                    }),
                    uses: vec![
                        UseDecl::Service(UseServiceDecl {
                            source: UseSource::Parent,
                            source_name: "netstack".try_into().unwrap(),
                            target_path: "/svc/mynetstack".try_into().unwrap(),
                        }),
                        UseDecl::Protocol(UseProtocolDecl {
                            source: UseSource::Parent,
                            source_name: "legacy_netstack".try_into().unwrap(),
                            target_path: "/svc/legacy_mynetstack".try_into().unwrap(),
                        }),
                        UseDecl::Protocol(UseProtocolDecl {
                            source: UseSource::Child("echo".to_string()),
                            source_name: "echo_service".try_into().unwrap(),
                            target_path: "/svc/echo_service".try_into().unwrap(),
                        }),
                        UseDecl::Directory(UseDirectoryDecl {
                            source: UseSource::Framework,
                            source_name: "dir".try_into().unwrap(),
                            target_path: "/data".try_into().unwrap(),
                            rights: fio2::Operations::Connect,
                            subdir: Some("foo/bar".into()),
                        }),
                        UseDecl::Storage(UseStorageDecl {
                            source_name: "cache".into(),
                            target_path: "/cache".try_into().unwrap(),
                        }),
                        UseDecl::Storage(UseStorageDecl {
                            source_name: "temp".into(),
                            target_path: "/temp".try_into().unwrap(),
                        }),
                        UseDecl::Event(UseEventDecl {
                            source: UseSource::Parent,
                            source_name: "capability_ready".into(),
                            target_name: "diagnostics_ready".into(),
                            filter: Some(hashmap!{"path".to_string() =>  DictionaryValue::Str("/diagnostics".to_string())}),
                            mode: EventMode::Sync,
                        }),
                    ],
                    exposes: vec![
                        ExposeDecl::Protocol(ExposeProtocolDecl {
                            source: ExposeSource::Child("netstack".to_string()),
                            source_name: "legacy_netstack".try_into().unwrap(),
                            target_name: "legacy_mynetstack".try_into().unwrap(),
                            target: ExposeTarget::Parent,
                        }),
                        ExposeDecl::Directory(ExposeDirectoryDecl {
                            source: ExposeSource::Child("netstack".to_string()),
                            source_name: "dir".try_into().unwrap(),
                            target_name: "data".try_into().unwrap(),
                            target: ExposeTarget::Parent,
                            rights: Some(fio2::Operations::Connect),
                            subdir: Some("foo/bar".into()),
                        }),
                        ExposeDecl::Runner(ExposeRunnerDecl {
                            source: ExposeSource::Child("netstack".to_string()),
                            source_name: "elf".try_into().unwrap(),
                            target: ExposeTarget::Parent,
                            target_name: "elf".try_into().unwrap(),
                        }),
                        ExposeDecl::Resolver(ExposeResolverDecl {
                            source: ExposeSource::Child("netstack".to_string()),
                            source_name: "pkg".try_into().unwrap(),
                            target: ExposeTarget::Parent,
                            target_name: "pkg".try_into().unwrap(),
                        }),
                        ExposeDecl::Service(ExposeServiceDecl {
                            sources: vec![
                                ServiceSource::<ExposeSource> {
                                    source: ExposeSource::Child("netstack".to_string()),
                                    source_name: "netstack1".try_into().unwrap(),
                                },
                                ServiceSource::<ExposeSource> {
                                    source: ExposeSource::Child("netstack".to_string()),
                                    source_name: "netstack2".try_into().unwrap(),
                                },
                            ],
                            target_name: "mynetstack".try_into().unwrap(),
                            target: ExposeTarget::Parent,
                        }),
                    ],
                    offers: vec![
                        OfferDecl::Protocol(OfferProtocolDecl {
                            source: OfferSource::Parent,
                            source_name: "legacy_netstack".try_into().unwrap(),
                            target: OfferTarget::Child("echo".to_string()),
                            target_name: "legacy_mynetstack".try_into().unwrap(),
                            dependency_type: DependencyType::WeakForMigration,
                        }),
                        OfferDecl::Directory(OfferDirectoryDecl {
                            source: OfferSource::Parent,
                            source_name: "dir".try_into().unwrap(),
                            target: OfferTarget::Collection("modular".to_string()),
                            target_name: "data".try_into().unwrap(),
                            rights: Some(fio2::Operations::Connect),
                            subdir: None,
                            dependency_type: DependencyType::Strong,
                        }),
                        OfferDecl::Storage(OfferStorageDecl {
                            source_name: "cache".try_into().unwrap(),
                            source: OfferSource::Self_,
                            target: OfferTarget::Collection("modular".to_string()),
                            target_name: "cache".try_into().unwrap(),
                        }),
                        OfferDecl::Runner(OfferRunnerDecl {
                            source: OfferSource::Parent,
                            source_name: "elf".try_into().unwrap(),
                            target: OfferTarget::Child("echo".to_string()),
                            target_name: "elf2".try_into().unwrap(),
                        }),
                        OfferDecl::Resolver(OfferResolverDecl {
                            source: OfferSource::Parent,
                            source_name: "pkg".try_into().unwrap(),
                            target: OfferTarget::Child("echo".to_string()),
                            target_name: "pkg".try_into().unwrap(),
                        }),
                        OfferDecl::Event(OfferEventDecl {
                            source: OfferSource::Parent,
                            source_name: "started".into(),
                            target: OfferTarget::Child("echo".to_string()),
                            target_name: "mystarted".into(),
                            filter: Some(hashmap!{"path".to_string() => DictionaryValue::Str("/a".to_string())}),
                            mode: EventMode::Sync,
                        }),
                        OfferDecl::Service(OfferServiceDecl {
                            sources: vec![
                                ServiceSource::<OfferSource> {
                                    source: OfferSource::Parent,
                                    source_name: "netstack1".try_into().unwrap(),
                                },
                                ServiceSource::<OfferSource> {
                                    source: OfferSource::Parent,
                                    source_name: "netstack2".try_into().unwrap(),
                                },
                            ],
                            target: OfferTarget::Child("echo".to_string()),
                            target_name: "mynetstack".try_into().unwrap(),
                        }),
                    ],
                    capabilities: vec![
                        CapabilityDecl::Service(ServiceDecl {
                            name: "netstack".into(),
                            source_path: "/netstack".try_into().unwrap(),
                        }),
                        CapabilityDecl::Protocol(ProtocolDecl {
                            name: "netstack2".into(),
                            source_path: "/netstack2".try_into().unwrap(),
                        }),
                        CapabilityDecl::Directory(DirectoryDecl {
                            name: "data".into(),
                            source_path: "/data".try_into().unwrap(),
                            rights: fio2::Operations::Connect,
                        }),
                        CapabilityDecl::Storage(StorageDecl {
                            name: "cache".into(),
                            backing_dir: "data".try_into().unwrap(),
                            source: StorageDirectorySource::Parent,
                            subdir: Some("cache".try_into().unwrap()),
                        }),
                        CapabilityDecl::Runner(RunnerDecl {
                            name: "elf".into(),
                            source_path: "/elf".try_into().unwrap(),
                        }),
                        CapabilityDecl::Resolver(ResolverDecl {
                            name: "pkg".into(),
                            source_path: "/pkg_resolver".try_into().unwrap(),
                        }),
                    ],
                    children: vec![
                        ChildDecl {
                            name: "netstack".to_string(),
                            url: "fuchsia-pkg://fuchsia.com/netstack#meta/netstack.cm".to_string(),
                            startup: fsys::StartupMode::Lazy,
                            environment: None,
                        },
                        ChildDecl {
                            name: "gtest".to_string(),
                            url: "fuchsia-pkg://fuchsia.com/gtest#meta/gtest.cm".to_string(),
                            startup: fsys::StartupMode::Lazy,
                            environment: None,
                        },
                        ChildDecl {
                            name: "echo".to_string(),
                            url: "fuchsia-pkg://fuchsia.com/echo#meta/echo.cm".to_string(),
                            startup: fsys::StartupMode::Eager,
                            environment: Some("test_env".to_string()),
                        },
                    ],
                    collections: vec![
                        CollectionDecl {
                            name: "modular".to_string(),
                            durability: fsys::Durability::Persistent,
                            environment: None,
                        },
                        CollectionDecl {
                            name: "tests".to_string(),
                            durability: fsys::Durability::Transient,
                            environment: Some("test_env".to_string()),
                        },
                    ],
                    facets: Some(fsys::Object{entries: vec![
                       fsys::Entry{
                           key: "author".to_string(),
                           value: Some(Box::new(fsys::Value::Str("Fuchsia".to_string()))),
                       },
                    ]}),
                    environments: vec![
                        EnvironmentDecl {
                            name: "test_env".into(),
                            extends: fsys::EnvironmentExtends::Realm,
                            runners: vec![
                                RunnerRegistration {
                                    source_name: "runner".into(),
                                    source: RegistrationSource::Child("gtest".to_string()),
                                    target_name: "gtest-runner".into(),
                                }
                            ],
                            resolvers: vec![
                                ResolverRegistration {
                                    resolver: "pkg_resolver".into(),
                                    source: RegistrationSource::Parent,
                                    scheme: "fuchsia-pkg".to_string(),
                                }
                            ],
                            debug_capabilities: vec![
                                DebugRegistration::Protocol(DebugProtocolRegistration {
                                    source_name: "some_protocol".into(),
                                    source: RegistrationSource::Child("gtest".to_string()),
                                    target_name: "some_protocol".into(),
                                })
                            ],
                            stop_timeout_ms: Some(4567),
                        }
                    ]
                }
            },
        },
    }

    test_capability_path! {
        capability_path_one_part => {
            input = "/foo",
            result = Ok(CapabilityPath{dirname: "/".to_string(), basename: "foo".to_string()}),
        },
        capability_path_two_parts => {
            input = "/foo/bar",
            result = Ok(CapabilityPath{dirname: "/foo".to_string(), basename: "bar".to_string()}),
        },
        capability_path_many_parts => {
            input = "/foo/bar/long/path",
            result = Ok(CapabilityPath{
                dirname: "/foo/bar/long".to_string(),
                basename: "path".to_string()
            }),
        },
        capability_path_invalid_empty_part => {
            input = "/foo/bar//long/path",
            result = Err(Error::InvalidCapabilityPath{raw: "/foo/bar//long/path".to_string()}),
        },
        capability_path_invalid_empty => {
            input = "",
            result = Err(Error::InvalidCapabilityPath{raw: "".to_string()}),
        },
        capability_path_invalid_root => {
            input = "/",
            result = Err(Error::InvalidCapabilityPath{raw: "/".to_string()}),
        },
        capability_path_invalid_relative => {
            input = "foo/bar",
            result = Err(Error::InvalidCapabilityPath{raw: "foo/bar".to_string()}),
        },
        capability_path_invalid_trailing => {
            input = "/foo/bar/",
            result = Err(Error::InvalidCapabilityPath{raw: "/foo/bar/".to_string()}),
        },
    }

    test_fidl_into_and_from! {
        fidl_into_and_from_use_source => {
            input = vec![
                fsys::Ref::Parent(fsys::ParentRef{}),
                fsys::Ref::Framework(fsys::FrameworkRef{}),
                fsys::Ref::Debug(fsys::DebugRef{}),
                fsys::Ref::Capability(fsys::CapabilityRef {name: "capability".to_string()}),
                fsys::Ref::Child(fsys::ChildRef {
                    name: "foo".to_string(),
                    collection: None,
                }),
            ],
            input_type = fsys::Ref,
            result = vec![
                UseSource::Parent,
                UseSource::Framework,
                UseSource::Debug,
                UseSource::Capability(CapabilityName("capability".to_string())),
                UseSource::Child("foo".to_string()),
            ],
            result_type = UseSource,
        },
        fidl_into_and_from_expose_source => {
            input = vec![
                fsys::Ref::Self_(fsys::SelfRef {}),
                fsys::Ref::Child(fsys::ChildRef {
                    name: "foo".to_string(),
                    collection: None,
                }),
                fsys::Ref::Framework(fsys::FrameworkRef {}),
                fsys::Ref::Collection(fsys::CollectionRef { name: "foo".to_string() }),
            ],
            input_type = fsys::Ref,
            result = vec![
                ExposeSource::Self_,
                ExposeSource::Child("foo".to_string()),
                ExposeSource::Framework,
                ExposeSource::Collection("foo".to_string()),
            ],
            result_type = ExposeSource,
        },
        fidl_into_and_from_offer_source => {
            input = vec![
                fsys::Ref::Self_(fsys::SelfRef {}),
                fsys::Ref::Child(fsys::ChildRef {
                    name: "foo".to_string(),
                    collection: None,
                }),
                fsys::Ref::Framework(fsys::FrameworkRef {}),
                fsys::Ref::Capability(fsys::CapabilityRef { name: "foo".to_string() }),
                fsys::Ref::Parent(fsys::ParentRef {}),
                fsys::Ref::Collection(fsys::CollectionRef { name: "foo".to_string() }),
            ],
            input_type = fsys::Ref,
            result = vec![
                OfferSource::Self_,
                OfferSource::Child("foo".to_string()),
                OfferSource::Framework,
                OfferSource::Capability(CapabilityName("foo".to_string())),
                OfferSource::Parent,
                OfferSource::Collection("foo".to_string()),
            ],
            result_type = OfferSource,
        },
        fidl_into_and_from_storage_capability => {
            input = vec![
                fsys::StorageDecl {
                    name: Some("minfs".to_string()),
                    backing_dir: Some("minfs".into()),
                    source: Some(fsys::Ref::Child(fsys::ChildRef {
                        name: "foo".to_string(),
                        collection: None,
                    })),
                    subdir: None,
                    ..fsys::StorageDecl::EMPTY
                },
            ],
            input_type = fsys::StorageDecl,
            result = vec![
                StorageDecl {
                    name: "minfs".into(),
                    backing_dir: "minfs".into(),
                    source: StorageDirectorySource::Child("foo".to_string()),
                    subdir: None,
                },
            ],
            result_type = StorageDecl,
        },
    }

    test_fidl_into! {
        fidl_into_object => {
            input = {
                let obj_inner = fsys::Object{entries: vec![
                    fsys::Entry{
                        key: "string".to_string(),
                        value: Some(Box::new(fsys::Value::Str("bar".to_string()))),
                    },
                ]};
                let vector = fsys::Vector{values: vec![
                    Some(Box::new(fsys::Value::Obj(obj_inner))),
                    Some(Box::new(fsys::Value::Inum(-42)))
                ]};
                let obj_outer = fsys::Object{entries: vec![
                    fsys::Entry{
                        key: "array".to_string(),
                        value: Some(Box::new(fsys::Value::Vec(vector))),
                    },
                ]};
                let obj = fsys::Object {entries: vec![
                    fsys::Entry {
                        key: "bool".to_string(),
                        value: Some(Box::new(fsys::Value::Bit(true))),
                    },
                    fsys::Entry {
                        key: "obj".to_string(),
                        value: Some(Box::new(fsys::Value::Obj(obj_outer))),
                    },
                    fsys::Entry {
                        key: "float".to_string(),
                        value: Some(Box::new(fsys::Value::Fnum(3.14))),
                    },
                    fsys::Entry {
                        key: "int".to_string(),
                        value: Some(Box::new(fsys::Value::Inum(-42))),
                    },
                    fsys::Entry {
                        key: "null".to_string(),
                        value: None,
                    },
                    fsys::Entry {
                        key: "string".to_string(),
                        value: Some(Box::new(fsys::Value::Str("bar".to_string()))),
                    },
                ]};
                obj
            },
            result = {
                let mut obj_inner = HashMap::new();
                obj_inner.insert("string".to_string(), Value::Str("bar".to_string()));
                let mut obj_outer = HashMap::new();
                let vector = vec![Value::Obj(obj_inner), Value::Inum(-42)];
                obj_outer.insert("array".to_string(), Value::Vec(vector));

                let mut obj: HashMap<String, Value> = HashMap::new();
                obj.insert("bool".to_string(), Value::Bit(true));
                obj.insert("float".to_string(), Value::Fnum(3.14));
                obj.insert("int".to_string(), Value::Inum(-42));
                obj.insert("string".to_string(), Value::Str("bar".to_string()));
                obj.insert("obj".to_string(), Value::Obj(obj_outer));
                obj.insert("null".to_string(), Value::Null);
                obj
            },
        },
    }
}
