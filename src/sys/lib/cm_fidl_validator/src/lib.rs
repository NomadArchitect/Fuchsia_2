// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    directed_graph::DirectedGraph,
    fidl_fuchsia_sys2 as fsys,
    itertools::Itertools,
    std::{
        collections::{HashMap, HashSet},
        fmt,
        path::Path,
    },
    thiserror::Error,
};

const MAX_PATH_LENGTH: usize = 1024;
const MAX_NAME_LENGTH: usize = 100;
const MAX_URL_LENGTH: usize = 4096;

/// Enum type that can represent any error encountered during validation.
#[derive(Debug, Error, PartialEq, Clone)]
pub enum Error {
    #[error("{} missing {}", .0.decl, .0.field)]
    MissingField(DeclField),
    #[error("{} has empty {}", .0.decl, .0.field)]
    EmptyField(DeclField),
    #[error("{} has extraneous {}", .0.decl, .0.field)]
    ExtraneousField(DeclField),
    #[error("\"{1}\" is a duplicate {} {}", .0.decl, .0.field)]
    DuplicateField(DeclField, String),
    #[error("{} has invalid {}", .0.decl, .0.field)]
    InvalidField(DeclField),
    #[error("{}'s {} is too long", .0.decl, .0.field)]
    FieldTooLong(DeclField),
    #[error("\"{0}\" cannot declare a capability of type {1}")]
    InvalidCapabilityType(DeclField, String),
    #[error("\"{0}\" target \"{1}\" is same as source")]
    OfferTargetEqualsSource(String, String),
    #[error("\"{1}\" is referenced in {0} but it does not appear in children")]
    InvalidChild(DeclField, String),
    #[error("\"{1}\" is referenced in {0} but it does not appear in collections")]
    InvalidCollection(DeclField, String),
    #[error("\"{1}\" is referenced in {0} but it does not appear in storage")]
    InvalidStorage(DeclField, String),
    #[error("\"{1}\" is referenced in {0} but it does not appear in environments")]
    InvalidEnvironment(DeclField, String),
    #[error("\"{1}\" is referenced in {0} but it does not appear in capabilities")]
    InvalidCapability(DeclField, String),
    #[error("\"{1}\" is referenced in {0} but it does not appear in runners")]
    InvalidRunner(DeclField, String),
    #[error("\"{1}\" is referenced in {0} but it does not appear in events")]
    EventStreamEventNotFound(DeclField, String),
    #[error("Event \"{1}\" is referenced in {0} with unsupported mode \"{2}\"")]
    EventStreamUnsupportedMode(DeclField, String, String),
    #[error("dependency cycle(s) exist: {0}")]
    DependencyCycle(String),
    #[error("{} \"{}\" path overlaps with {} \"{}\"", decl, path, other_decl, other_path)]
    InvalidPathOverlap { decl: DeclField, path: String, other_decl: DeclField, other_path: String },
    #[error("built-in capability decl {0} should not specify a source path, found \"{1}\"")]
    ExtraneousSourcePath(DeclField, String),
}

impl Error {
    pub fn missing_field(decl_type: impl Into<String>, keyword: impl Into<String>) -> Self {
        Error::MissingField(DeclField { decl: decl_type.into(), field: keyword.into() })
    }

    pub fn empty_field(decl_type: impl Into<String>, keyword: impl Into<String>) -> Self {
        Error::EmptyField(DeclField { decl: decl_type.into(), field: keyword.into() })
    }

    pub fn extraneous_field(decl_type: impl Into<String>, keyword: impl Into<String>) -> Self {
        Error::ExtraneousField(DeclField { decl: decl_type.into(), field: keyword.into() })
    }

    pub fn duplicate_field(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Error::DuplicateField(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            value.into(),
        )
    }

    pub fn invalid_field(decl_type: impl Into<String>, keyword: impl Into<String>) -> Self {
        Error::InvalidField(DeclField { decl: decl_type.into(), field: keyword.into() })
    }

    pub fn field_too_long(decl_type: impl Into<String>, keyword: impl Into<String>) -> Self {
        Error::FieldTooLong(DeclField { decl: decl_type.into(), field: keyword.into() })
    }

    pub fn invalid_capability_type(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        type_name: impl Into<String>,
    ) -> Self {
        Error::InvalidCapabilityType(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            type_name.into(),
        )
    }

    pub fn offer_target_equals_source(decl: impl Into<String>, target: impl Into<String>) -> Self {
        Error::OfferTargetEqualsSource(decl.into(), target.into())
    }

    pub fn invalid_child(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        child: impl Into<String>,
    ) -> Self {
        Error::InvalidChild(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            child.into(),
        )
    }

    pub fn invalid_collection(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        collection: impl Into<String>,
    ) -> Self {
        Error::InvalidCollection(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            collection.into(),
        )
    }

    pub fn invalid_storage(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        storage: impl Into<String>,
    ) -> Self {
        Error::InvalidStorage(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            storage.into(),
        )
    }

    pub fn invalid_environment(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        environment: impl Into<String>,
    ) -> Self {
        Error::InvalidEnvironment(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            environment.into(),
        )
    }

    // TODO: Replace with `invalid_capability`?
    pub fn invalid_runner(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        runner: impl Into<String>,
    ) -> Self {
        Error::InvalidRunner(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            runner.into(),
        )
    }

    pub fn invalid_capability(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        capability: impl Into<String>,
    ) -> Self {
        Error::InvalidCapability(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            capability.into(),
        )
    }

    pub fn event_stream_event_not_found(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        event_name: impl Into<String>,
    ) -> Self {
        Error::EventStreamEventNotFound(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            event_name.into(),
        )
    }

    pub fn event_stream_unsupported_mode(
        decl_type: impl Into<String>,
        keyword: impl Into<String>,
        event_name: impl Into<String>,
        event_mode: impl Into<String>,
    ) -> Self {
        Error::EventStreamUnsupportedMode(
            DeclField { decl: decl_type.into(), field: keyword.into() },
            event_name.into(),
            event_mode.into(),
        )
    }

    pub fn dependency_cycle(error: String) -> Self {
        Error::DependencyCycle(error)
    }

    pub fn invalid_path_overlap(
        decl: impl Into<String>,
        path: impl Into<String>,
        other_decl: impl Into<String>,
        other_path: impl Into<String>,
    ) -> Self {
        Error::InvalidPathOverlap {
            decl: DeclField { decl: decl.into(), field: "target_path".to_string() },
            path: path.into(),
            other_decl: DeclField { decl: other_decl.into(), field: "target_path".to_string() },
            other_path: other_path.into(),
        }
    }

    pub fn extraneous_source_path(decl_type: impl Into<String>, path: impl Into<String>) -> Self {
        Error::ExtraneousSourcePath(
            DeclField { decl: decl_type.into(), field: "source_path".to_string() },
            path.into(),
        )
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct DeclField {
    pub decl: String,
    pub field: String,
}

impl fmt::Display for DeclField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", &self.decl, &self.field)
    }
}

/// Represents a list of errors encountered during validation.
#[derive(Debug, Error, PartialEq, Clone)]
pub struct ErrorList {
    pub errs: Vec<Error>,
}

impl ErrorList {
    fn new(errs: Vec<Error>) -> ErrorList {
        ErrorList { errs }
    }
}

impl fmt::Display for ErrorList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let strs: Vec<String> = self.errs.iter().map(|e| format!("{}", e)).collect();
        write!(f, "{}", strs.join(", "))
    }
}

/// Validates a ComponentDecl.
///
/// The ComponentDecl may ultimately originate from a CM file, or be directly constructed by the
/// caller. Either way, a ComponentDecl should always be validated before it's used. Examples
/// of what is validated (which may evolve in the future):
///
/// - That all semantically required fields are present
/// - That a child_name referenced in a source actually exists in the list of children
/// - That there are no duplicate target paths.
/// - That a cap is not offered back to the child that exposed it.
///
/// All checks are local to this ComponentDecl.
pub fn validate(decl: &fsys::ComponentDecl) -> Result<(), ErrorList> {
    let ctx = ValidationContext::default();
    ctx.validate(decl).map_err(|errs| ErrorList::new(errs))
}

/// Validates a list of CapabilityDecls independently.
pub fn validate_capabilities(
    capabilities: &Vec<fsys::CapabilityDecl>,
    as_builtin: bool,
) -> Result<(), ErrorList> {
    let mut ctx = ValidationContext::default();
    for capability in capabilities {
        ctx.validate_capability_decl(capability, as_builtin);
    }
    if ctx.errors.is_empty() {
        Ok(())
    } else {
        Err(ErrorList::new(ctx.errors))
    }
}

/// Validates an independent ChildDecl. Performs the same validation on it as `validate`.
pub fn validate_child(child: &fsys::ChildDecl) -> Result<(), ErrorList> {
    let mut errors = vec![];
    check_name(child.name.as_ref(), "ChildDecl", "name", &mut errors);
    check_url(child.url.as_ref(), "ChildDecl", "url", &mut errors);
    if child.startup.is_none() {
        errors.push(Error::missing_field("ChildDecl", "startup"));
    }
    // Allow `on_terminate` to be unset since the default is almost always desired.
    if child.environment.is_some() {
        check_name(child.environment.as_ref(), "ChildDecl", "environment", &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ErrorList { errs: errors })
    }
}

/// Validates a collection of dynamic offers. Dynamic offers differ from static
/// offers, in that
///
/// 1. a dynamic offer's `target` field must be omitted;
/// 2. a dynamic offer's `source` _may_ be a dynamic child;
/// 3. since this crate isn't really designed to handle dynamic children, we
///    disable the checks that ensure that the source/target exist, and that the
///    offers don't introduce any cycles.
pub fn validate_dynamic_offers(offers: &Vec<fsys::OfferDecl>) -> Result<(), ErrorList> {
    let mut ctx = ValidationContext::default();
    for offer in offers {
        ctx.validate_offers_decl(offer, OfferType::Dynamic)
    }
    if ctx.errors.is_empty() {
        Ok(())
    } else {
        Err(ErrorList::new(ctx.errors))
    }
}

#[derive(Default)]
struct ValidationContext<'a> {
    all_children: HashMap<&'a str, &'a fsys::ChildDecl>,
    all_collections: HashSet<&'a str>,
    all_capability_ids: HashSet<&'a str>,
    all_storage_and_sources: HashMap<&'a str, Option<&'a fsys::Ref>>,
    all_services: HashSet<&'a str>,
    all_protocols: HashSet<&'a str>,
    all_directories: HashSet<&'a str>,
    all_runners: HashSet<&'a str>,
    all_resolvers: HashSet<&'a str>,
    all_environment_names: HashSet<&'a str>,
    all_events: HashMap<&'a str, fsys::EventMode>,
    all_event_streams: HashSet<&'a str>,
    strong_dependencies: DirectedGraph<DependencyNode<'a>>,
    target_ids: IdMap<'a>,
    errors: Vec<Error>,
}

/// A node in the DependencyGraph. The first string describes the type of node and the second
/// string is the name of the node.
#[derive(Copy, Clone, Hash, Ord, Debug, PartialOrd, PartialEq, Eq)]
enum DependencyNode<'a> {
    Self_,
    Child(&'a str),
    Collection(&'a str),
    Environment(&'a str),
    /// This variant is automatically translated to the source backing the capability by
    /// `add_strong_dep`, it does not appear in the dependency graph.
    Capability(&'a str),
}

impl<'a> DependencyNode<'a> {
    fn try_from_ref(ref_: Option<&'a fsys::Ref>) -> Option<DependencyNode<'a>> {
        if ref_.is_none() {
            return None;
        }
        match ref_.unwrap() {
            fsys::Ref::Child(fsys::ChildRef { name, .. }) => {
                Some(DependencyNode::Child(name.as_str()))
            }
            fsys::Ref::Collection(fsys::CollectionRef { name, .. }) => {
                Some(DependencyNode::Collection(name.as_str()))
            }
            fsys::Ref::Capability(fsys::CapabilityRef { name, .. }) => {
                Some(DependencyNode::Capability(name.as_str()))
            }
            fsys::Ref::Self_(_) => Some(DependencyNode::Self_),
            fsys::Ref::Parent(_) => {
                // We don't care about dependency cycles with the parent, as any potential issues
                // with that are resolved by cycle detection in the parent's manifest.
                None
            }
            fsys::Ref::Framework(_) => {
                // We don't care about dependency cycles with the framework, as the framework
                // always outlives the component.
                None
            }
            fsys::Ref::Debug(_) => {
                // We don't care about dependency cycles with any debug capabilities from the
                // environment, as those are put there by our parent, and any potential cycles with
                // our parent are handled by cycle detection in the parent's manifest.
                None
            }
            fsys::RefUnknown!() => {
                // We were unable to understand this FIDL value
                None
            }
        }
    }
}

impl<'a> fmt::Display for DependencyNode<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DependencyNode::Self_ => write!(f, "self"),
            DependencyNode::Child(name) => write!(f, "child {}", name),
            DependencyNode::Collection(name) => write!(f, "collection {}", name),
            DependencyNode::Environment(name) => write!(f, "environment {}", name),
            DependencyNode::Capability(name) => write!(f, "capability {}", name),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum AllowableIds {
    One,
    Many,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CollectionSource {
    Allow,
    Deny,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum TargetId<'a> {
    Component(&'a str),
    Collection(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfferType {
    Static,
    Dynamic,
}

type IdMap<'a> = HashMap<TargetId<'a>, HashMap<&'a str, AllowableIds>>;

impl<'a> ValidationContext<'a> {
    fn validate(mut self, decl: &'a fsys::ComponentDecl) -> Result<(), Vec<Error>> {
        // Collect all environment names first, so that references to them can be checked.
        if let Some(envs) = &decl.environments {
            self.collect_environment_names(&envs);
        }

        // Validate "program".
        if let Some(program) = decl.program.as_ref() {
            self.validate_program(program);
        }

        // Validate "children" and build the set of all children.
        if let Some(children) = decl.children.as_ref() {
            for child in children {
                self.validate_child_decl(&child);
            }
        }

        // Validate "collections" and build the set of all collections.
        if let Some(collections) = decl.collections.as_ref() {
            for collection in collections {
                self.validate_collection_decl(&collection);
            }
        }

        // Validate "capabilities" and build the set of all capabilities.
        if let Some(capabilities) = decl.capabilities.as_ref() {
            for capability in capabilities {
                self.validate_capability_decl(capability, false);
            }
        }

        // Validate "uses".
        if let Some(uses) = decl.uses.as_ref() {
            self.validate_use_decls(uses);
        }

        // Validate "exposes".
        if let Some(exposes) = decl.exposes.as_ref() {
            let mut target_ids = HashMap::new();
            for expose in exposes.iter() {
                self.validate_expose_decl(&expose, &mut target_ids);
            }
        }

        // Validate "offers".
        if let Some(offers) = decl.offers.as_ref() {
            for offer in offers.iter() {
                self.validate_offers_decl(&offer, OfferType::Static);
            }
        }

        // Validate "environments" after all other declarations are processed.
        if let Some(environment) = decl.environments.as_ref() {
            for environment in environment {
                self.validate_environment_decl(&environment);
            }
        }

        // Check that there are no strong cyclical dependencies
        if let Err(e) = self.strong_dependencies.topological_sort() {
            self.errors.push(Error::dependency_cycle(e.format_cycle()));
        }

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors)
        }
    }

    /// Collects all the environment names, watching for duplicates.
    fn collect_environment_names(&mut self, envs: &'a [fsys::EnvironmentDecl]) {
        for env in envs {
            if let Some(name) = env.name.as_ref() {
                if !self.all_environment_names.insert(name) {
                    self.errors.push(Error::duplicate_field("EnvironmentDecl", "name", name));
                }
            }
        }
    }

    /// Validates an individual capability declaration as either a built-in capability or (if
    /// `as_builtin = false`) as a component or namespace capability.
    // Storage capabilities are not currently allowed as built-ins, but there's no deep reason for this.
    // Update this method to allow built-in storage capabilities as needed.
    fn validate_capability_decl(&mut self, capability: &'a fsys::CapabilityDecl, as_builtin: bool) {
        match capability {
            fsys::CapabilityDecl::Service(service) => {
                self.validate_service_decl(&service, as_builtin)
            }
            fsys::CapabilityDecl::Protocol(protocol) => {
                self.validate_protocol_decl(&protocol, as_builtin)
            }
            fsys::CapabilityDecl::Directory(directory) => {
                self.validate_directory_decl(&directory, as_builtin)
            }
            fsys::CapabilityDecl::Storage(storage) => {
                if as_builtin {
                    self.errors.push(Error::invalid_capability_type(
                        "RuntimeConfig",
                        "capability",
                        "storage",
                    ))
                } else {
                    self.validate_storage_decl(&storage)
                }
            }
            fsys::CapabilityDecl::Runner(runner) => self.validate_runner_decl(&runner, as_builtin),
            fsys::CapabilityDecl::Resolver(resolver) => {
                self.validate_resolver_decl(&resolver, as_builtin)
            }
            fsys::CapabilityDecl::Event(event) => {
                if as_builtin {
                    self.validate_event_decl(&event)
                } else {
                    self.errors.push(Error::invalid_capability_type(
                        "ComponentDecl",
                        "capability",
                        "event",
                    ))
                }
            }
            fsys::CapabilityDeclUnknown!() => {
                if as_builtin {
                    self.errors.push(Error::invalid_capability_type(
                        "RuntimeConfig",
                        "capability",
                        "unknown",
                    ));
                } else {
                    self.errors.push(Error::invalid_capability_type(
                        "ComponentDecl",
                        "capability",
                        "unknown",
                    ));
                }
            }
        }
    }

    fn validate_use_decls(&mut self, uses: &'a [fsys::UseDecl]) {
        // Validate all events first so that we keep track of them for validation of event_streams.
        for use_ in uses.iter() {
            match use_ {
                fsys::UseDecl::Event(e) => self.validate_event(e),
                _ => {}
            }
        }

        // Validate individual fields.
        for use_ in uses.iter() {
            self.validate_use_decl(&use_);
        }

        self.validate_use_paths(&uses);
    }

    fn validate_use_decl(&mut self, use_: &'a fsys::UseDecl) {
        match use_ {
            fsys::UseDecl::Service(u) => {
                self.validate_use_source(
                    u.source.as_ref(),
                    u.source_name.as_ref(),
                    u.dependency_type.as_ref(),
                    "UseServiceDecl",
                    "source",
                );
                check_name(
                    u.source_name.as_ref(),
                    "UseServiceDecl",
                    "source_name",
                    &mut self.errors,
                );
                check_path(
                    u.target_path.as_ref(),
                    "UseServiceDecl",
                    "target_path",
                    &mut self.errors,
                );
            }
            fsys::UseDecl::Protocol(u) => {
                self.validate_use_source(
                    u.source.as_ref(),
                    u.source_name.as_ref(),
                    u.dependency_type.as_ref(),
                    "UseProtocolDecl",
                    "source",
                );
                check_name(
                    u.source_name.as_ref(),
                    "UseProtocolDecl",
                    "source_name",
                    &mut self.errors,
                );
                check_path(
                    u.target_path.as_ref(),
                    "UseProtocolDecl",
                    "target_path",
                    &mut self.errors,
                );
            }
            fsys::UseDecl::Directory(u) => {
                self.validate_use_source(
                    u.source.as_ref(),
                    u.source_name.as_ref(),
                    u.dependency_type.as_ref(),
                    "UseDirectoryDecl",
                    "source",
                );
                check_name(
                    u.source_name.as_ref(),
                    "UseDirectoryDecl",
                    "source_name",
                    &mut self.errors,
                );
                check_path(
                    u.target_path.as_ref(),
                    "UseDirectoryDecl",
                    "target_path",
                    &mut self.errors,
                );
                if u.rights.is_none() {
                    self.errors.push(Error::missing_field("UseDirectoryDecl", "rights"));
                }
                if let Some(subdir) = u.subdir.as_ref() {
                    check_relative_path(
                        Some(subdir),
                        "UseDirectoryDecl",
                        "subdir",
                        &mut self.errors,
                    );
                }
            }
            fsys::UseDecl::Storage(u) => {
                check_name(
                    u.source_name.as_ref(),
                    "UseStorageDecl",
                    "source_name",
                    &mut self.errors,
                );
                check_path(
                    u.target_path.as_ref(),
                    "UseStorageDecl",
                    "target_path",
                    &mut self.errors,
                );
            }
            fsys::UseDecl::EventStream(e) => {
                self.validate_event_stream(e);
            }
            fsys::UseDecl::Event(_) => {
                // Skip events. We must have already validated by this point.
                // See `validate_use_decls`.
            }
            fsys::UseDeclUnknown!() => {
                self.errors.push(Error::invalid_field("ComponentDecl", "use"));
            }
        }
    }

    /// Validates the "program" declaration. This does not check runner-specific properties
    /// since those are checked by the runner.
    fn validate_program(&mut self, program: &fsys::ProgramDecl) {
        if program.runner.is_none() {
            self.errors.push(Error::missing_field("ProgramDecl", "runner"));
        }
    }

    /// Validates that paths-based capabilities (service, directory, protocol)
    /// are different and not prefixes of each other.
    fn validate_use_paths(&mut self, uses: &[fsys::UseDecl]) {
        #[derive(Debug, PartialEq, Clone, Copy)]
        struct PathCapability<'a> {
            decl: &'a str,
            dir: &'a Path,
            use_: &'a fsys::UseDecl,
        }
        let mut used_paths = HashMap::new();
        for use_ in uses.iter() {
            match use_ {
                fsys::UseDecl::Service(fsys::UseServiceDecl {
                    target_path: Some(path), ..
                })
                | fsys::UseDecl::Protocol(fsys::UseProtocolDecl {
                    target_path: Some(path), ..
                })
                | fsys::UseDecl::Directory(fsys::UseDirectoryDecl {
                    target_path: Some(path),
                    ..
                }) => {
                    let capability = match use_ {
                        fsys::UseDecl::Service(_) => {
                            let dir = match Path::new(path).parent() {
                                Some(p) => p,
                                None => continue, // Invalid path, validated elsewhere
                            };
                            PathCapability { decl: "UseServiceDecl", dir, use_ }
                        }
                        fsys::UseDecl::Protocol(_) => {
                            let dir = match Path::new(path).parent() {
                                Some(p) => p,
                                None => continue, // Invalid path, validated elsewhere
                            };
                            PathCapability { decl: "UseProtocolDecl", dir, use_ }
                        }
                        fsys::UseDecl::Directory(_) => {
                            PathCapability { decl: "UseDirectoryDecl", dir: Path::new(path), use_ }
                        }
                        _ => unreachable!(),
                    };
                    if used_paths.insert(path, capability).is_some() {
                        // Disallow multiple capabilities for the same path.
                        self.errors.push(Error::duplicate_field(capability.decl, "path", path));
                    }
                }
                _ => {}
            }
        }
        for ((&path_a, capability_a), (&path_b, capability_b)) in
            used_paths.iter().tuple_combinations()
        {
            if match (capability_a.use_, capability_b.use_) {
                // Directories can't be the same or partially overlap.
                (fsys::UseDecl::Directory(_), fsys::UseDecl::Directory(_)) => {
                    capability_b.dir == capability_a.dir
                        || capability_b.dir.starts_with(capability_a.dir)
                        || capability_a.dir.starts_with(capability_b.dir)
                }

                // Protocols and Services can't overlap with Directories.
                (_, fsys::UseDecl::Directory(_)) | (fsys::UseDecl::Directory(_), _) => {
                    capability_b.dir == capability_a.dir
                        || capability_b.dir.starts_with(capability_a.dir)
                        || capability_a.dir.starts_with(capability_b.dir)
                }

                // Protocols and Services containing directories may be same, but
                // partial overlap is disallowed.
                (_, _) => {
                    capability_b.dir != capability_a.dir
                        && (capability_b.dir.starts_with(capability_a.dir)
                            || capability_a.dir.starts_with(capability_b.dir))
                }
            } {
                self.errors.push(Error::invalid_path_overlap(
                    capability_a.decl,
                    path_a,
                    capability_b.decl,
                    path_b,
                ));
            }
        }
    }

    fn validate_event(&mut self, event: &'a fsys::UseEventDecl) {
        self.validate_use_source(
            event.source.as_ref(),
            event.source_name.as_ref(),
            event.dependency_type.as_ref(),
            "UseEventDecl",
            "source",
        );
        if let Some(fsys::Ref::Self_(_)) = event.source {
            self.errors.push(Error::invalid_field("UseEventDecl", "source"));
        }
        check_name(event.source_name.as_ref(), "UseEventDecl", "source_name", &mut self.errors);
        check_name(event.target_name.as_ref(), "UseEventDecl", "target_name", &mut self.errors);
        check_events_mode(&event.mode, "UseEventDecl", "mode", &mut self.errors);
        if let Some(target_name) = event.target_name.as_ref() {
            if self
                .all_events
                .insert(target_name, event.mode.unwrap_or(fsys::EventMode::Async))
                .is_some()
            {
                self.errors.push(Error::duplicate_field(
                    "UseEventDecl",
                    "target_name",
                    target_name,
                ));
            }
        }
    }

    fn validate_event_stream(&mut self, event_stream: &'a fsys::UseEventStreamDecl) {
        check_name(event_stream.name.as_ref(), "UseEventStreamDecl", "name", &mut self.errors);
        if let Some(name) = event_stream.name.as_ref() {
            if !self.all_event_streams.insert(name) {
                self.errors.push(Error::duplicate_field("UseEventStreamDecl", "name", name));
            }
        }
        match event_stream.subscriptions.as_ref() {
            None => {
                self.errors.push(Error::missing_field("UseEventStreamDecl", "subscriptions"));
            }
            Some(subscriptions) if subscriptions.is_empty() => {
                self.errors.push(Error::empty_field("UseEventStreamDecl", "subscriptions"));
            }
            Some(subscriptions) => {
                for subscription in subscriptions {
                    check_name(
                        subscription.event_name.as_ref(),
                        "UseEventStreamDecl",
                        "event_name",
                        &mut self.errors,
                    );
                    let event_name = subscription.event_name.clone().unwrap_or_default();
                    let event_mode = subscription.mode.unwrap_or(fsys::EventMode::Async);
                    match self.all_events.get(event_name.as_str()) {
                        Some(mode) => {
                            if *mode != fsys::EventMode::Sync && event_mode == fsys::EventMode::Sync
                            {
                                self.errors.push(Error::event_stream_unsupported_mode(
                                    "UseEventStreamDecl",
                                    "events",
                                    event_name,
                                    format!("{:?}", event_mode),
                                ));
                            }
                        }
                        None => {
                            self.errors.push(Error::event_stream_event_not_found(
                                "UseEventStreamDecl",
                                "events",
                                event_name,
                            ));
                        }
                    }
                }
            }
        }
    }

    // disallow (use from #child dependency=strong) && (offer to #child from self)
    // - err: `use` must have dependency=weak to prevent cycle
    // add strong dependencies to dependency graph, so we can check for cycles
    fn validate_use_source(
        &mut self,
        source: Option<&'a fsys::Ref>,
        source_name: Option<&'a String>,
        dependency_type: Option<&fsys::DependencyType>,
        decl: &str,
        field: &str,
    ) {
        match source {
            Some(fsys::Ref::Parent(_)) => {}
            Some(fsys::Ref::Framework(_)) => {}
            Some(fsys::Ref::Debug(_)) => {}
            Some(fsys::Ref::Self_(_)) => {}
            Some(fsys::Ref::Capability(capability)) => {
                if !self.all_capability_ids.contains(capability.name.as_str()) {
                    self.errors.push(Error::invalid_capability(decl, field, &capability.name));
                } else if dependency_type == Some(&fsys::DependencyType::Strong) {
                    self.add_strong_dep(
                        source_name,
                        DependencyNode::try_from_ref(source),
                        Some(DependencyNode::Self_),
                    );
                }
            }
            Some(fsys::Ref::Child(child)) => {
                if !self.all_children.contains_key(&child.name as &str) {
                    self.errors.push(Error::invalid_child(decl, field, &child.name));
                } else if dependency_type == Some(&fsys::DependencyType::Strong) {
                    self.add_strong_dep(
                        source_name,
                        DependencyNode::try_from_ref(source),
                        Some(DependencyNode::Self_),
                    );
                }
            }
            Some(_) => {
                self.errors.push(Error::invalid_field(decl, field));
            }
            None => {
                self.errors.push(Error::missing_field(decl, field));
            }
        };

        let is_use_from_child = match source {
            Some(fsys::Ref::Child(_)) => true,
            _ => false,
        };
        match (is_use_from_child, dependency_type) {
            (
                false,
                Some(fsys::DependencyType::Weak) | Some(fsys::DependencyType::WeakForMigration),
            ) => {
                self.errors.push(Error::invalid_field(decl, "dependency_type"));
            }
            _ => {}
        }
    }

    fn validate_child_decl(&mut self, child: &'a fsys::ChildDecl) {
        if let Err(mut e) = validate_child(child) {
            self.errors.append(&mut e.errs);
        }
        if let Some(name) = child.name.as_ref() {
            let name: &str = name;
            if self.all_children.insert(name, child).is_some() {
                self.errors.push(Error::duplicate_field("ChildDecl", "name", name));
            }
            if let Some(env) = child.environment.as_ref() {
                let source = DependencyNode::Environment(env.as_str());
                let target = DependencyNode::Child(name);
                self.add_strong_dep(None, Some(source), Some(target));
            }
        }
        if let Some(environment) = child.environment.as_ref() {
            if !self.all_environment_names.contains(environment.as_str()) {
                self.errors.push(Error::invalid_environment(
                    "ChildDecl",
                    "environment",
                    environment,
                ));
            }
        }
    }

    fn validate_collection_decl(&mut self, collection: &'a fsys::CollectionDecl) {
        let name = collection.name.as_ref();
        if check_name(name, "CollectionDecl", "name", &mut self.errors) {
            let name: &str = name.unwrap();
            if !self.all_collections.insert(name) {
                self.errors.push(Error::duplicate_field("CollectionDecl", "name", name));
            }
        }
        if collection.durability.is_none() {
            self.errors.push(Error::missing_field("CollectionDecl", "durability"));
        }
        // Allow `allowed_offers` to be unset, for backwards compatibility.
        if let Some(environment) = collection.environment.as_ref() {
            if !self.all_environment_names.contains(environment.as_str()) {
                self.errors.push(Error::invalid_environment(
                    "CollectionDecl",
                    "environment",
                    environment,
                ));
            }
            if let Some(name) = collection.name.as_ref() {
                let source = DependencyNode::Environment(environment.as_str());
                let target = DependencyNode::Collection(name.as_str());
                self.add_strong_dep(None, Some(source), Some(target));
            }
        }
    }

    fn validate_environment_decl(&mut self, environment: &'a fsys::EnvironmentDecl) {
        let name = environment.name.as_ref();
        check_name(name, "EnvironmentDecl", "name", &mut self.errors);
        if environment.extends.is_none() {
            self.errors.push(Error::missing_field("EnvironmentDecl", "extends"));
        }
        if let Some(runners) = environment.runners.as_ref() {
            let mut registered_runners = HashSet::new();
            for runner in runners {
                self.validate_runner_registration(runner, name.clone(), &mut registered_runners);
            }
        }
        if let Some(resolvers) = environment.resolvers.as_ref() {
            let mut registered_schemes = HashSet::new();
            for resolver in resolvers {
                self.validate_resolver_registration(
                    resolver,
                    name.clone(),
                    &mut registered_schemes,
                );
            }
        }

        match environment.extends.as_ref() {
            Some(fsys::EnvironmentExtends::None) => {
                if environment.stop_timeout_ms.is_none() {
                    self.errors.push(Error::missing_field("EnvironmentDecl", "stop_timeout_ms"));
                }
            }
            None | Some(fsys::EnvironmentExtends::Realm) => {}
        }

        if let Some(debugs) = environment.debug_capabilities.as_ref() {
            for debug in debugs {
                self.validate_environment_debug_registration(debug, name.clone());
            }
        }
    }

    fn validate_runner_registration(
        &mut self,
        runner_registration: &'a fsys::RunnerRegistration,
        environment_name: Option<&'a String>,
        runner_names: &mut HashSet<&'a str>,
    ) {
        check_name(
            runner_registration.source_name.as_ref(),
            "RunnerRegistration",
            "source_name",
            &mut self.errors,
        );
        self.validate_registration_source(
            environment_name,
            runner_registration.source.as_ref(),
            "RunnerRegistration",
        );
        // If the source is `self`, ensure we have a corresponding RunnerDecl.
        if let (Some(fsys::Ref::Self_(_)), Some(ref name)) =
            (&runner_registration.source, &runner_registration.source_name)
        {
            if !self.all_runners.contains(name as &str) {
                self.errors.push(Error::invalid_runner("RunnerRegistration", "source_name", name));
            }
        }

        check_name(
            runner_registration.target_name.as_ref(),
            "RunnerRegistration",
            "target_name",
            &mut self.errors,
        );
        if let Some(name) = runner_registration.target_name.as_ref() {
            if !runner_names.insert(name.as_str()) {
                self.errors.push(Error::duplicate_field("RunnerRegistration", "target_name", name));
            }
        }
    }

    fn validate_resolver_registration(
        &mut self,
        resolver_registration: &'a fsys::ResolverRegistration,
        environment_name: Option<&'a String>,
        schemes: &mut HashSet<&'a str>,
    ) {
        check_name(
            resolver_registration.resolver.as_ref(),
            "ResolverRegistration",
            "resolver",
            &mut self.errors,
        );
        self.validate_registration_source(
            environment_name,
            resolver_registration.source.as_ref(),
            "ResolverRegistration",
        );
        check_url_scheme(
            resolver_registration.scheme.as_ref(),
            "ResolverRegistration",
            "scheme",
            &mut self.errors,
        );
        if let Some(scheme) = resolver_registration.scheme.as_ref() {
            if !schemes.insert(scheme.as_str()) {
                self.errors.push(Error::duplicate_field("ResolverRegistration", "scheme", scheme));
            }
        }
    }

    fn validate_registration_source(
        &mut self,
        environment_name: Option<&'a String>,
        source: Option<&'a fsys::Ref>,
        ty: &str,
    ) {
        match source {
            Some(fsys::Ref::Parent(_)) => {}
            Some(fsys::Ref::Self_(_)) => {}
            Some(fsys::Ref::Child(child_ref)) => {
                // Make sure the child is valid.
                self.validate_child_ref(ty, "source", &child_ref);
            }
            Some(_) => {
                self.errors.push(Error::invalid_field(ty, "source"));
            }
            None => {
                self.errors.push(Error::missing_field(ty, "source"));
            }
        }

        let source = DependencyNode::try_from_ref(source);
        if let Some(source) = source {
            if let Some(env_name) = &environment_name {
                let target = DependencyNode::Environment(env_name);
                self.strong_dependencies.add_edge(source, target);
            }
        }
    }

    fn validate_service_decl(&mut self, service: &'a fsys::ServiceDecl, as_builtin: bool) {
        if check_name(service.name.as_ref(), "ServiceDecl", "name", &mut self.errors) {
            let name = service.name.as_ref().unwrap();
            if !self.all_capability_ids.insert(name) {
                self.errors.push(Error::duplicate_field("ServiceDecl", "name", name.as_str()));
            }
            self.all_services.insert(name);
        }
        match as_builtin {
            true => {
                if let Some(path) = service.source_path.as_ref() {
                    self.errors.push(Error::extraneous_source_path("ServiceDecl", path))
                }
            }
            false => {
                check_path(
                    service.source_path.as_ref(),
                    "ServiceDecl",
                    "source_path",
                    &mut self.errors,
                );
            }
        }
    }

    fn validate_protocol_decl(&mut self, protocol: &'a fsys::ProtocolDecl, as_builtin: bool) {
        if check_name(protocol.name.as_ref(), "ProtocolDecl", "name", &mut self.errors) {
            let name = protocol.name.as_ref().unwrap();
            if !self.all_capability_ids.insert(name) {
                self.errors.push(Error::duplicate_field("ProtocolDecl", "name", name.as_str()));
            }
            self.all_protocols.insert(name);
        }
        match as_builtin {
            true => {
                if let Some(path) = protocol.source_path.as_ref() {
                    self.errors.push(Error::extraneous_source_path("ProtocolDecl", path))
                }
            }
            false => {
                check_path(
                    protocol.source_path.as_ref(),
                    "ProtocolDecl",
                    "source_path",
                    &mut self.errors,
                );
            }
        }
    }

    fn validate_directory_decl(&mut self, directory: &'a fsys::DirectoryDecl, as_builtin: bool) {
        if check_name(directory.name.as_ref(), "DirectoryDecl", "name", &mut self.errors) {
            let name = directory.name.as_ref().unwrap();
            if !self.all_capability_ids.insert(name) {
                self.errors.push(Error::duplicate_field("DirectoryDecl", "name", name.as_str()));
            }
            self.all_directories.insert(name);
        }
        match as_builtin {
            true => {
                if let Some(path) = directory.source_path.as_ref() {
                    self.errors.push(Error::extraneous_source_path("DirectoryDecl", path))
                }
            }
            false => {
                check_path(
                    directory.source_path.as_ref(),
                    "DirectoryDecl",
                    "source_path",
                    &mut self.errors,
                );
            }
        }
        if directory.rights.is_none() {
            self.errors.push(Error::missing_field("DirectoryDecl", "rights"));
        }
    }

    fn validate_storage_decl(&mut self, storage: &'a fsys::StorageDecl) {
        match storage.source.as_ref() {
            Some(fsys::Ref::Parent(_)) => {}
            Some(fsys::Ref::Self_(_)) => {}
            Some(fsys::Ref::Child(child)) => {
                self.validate_source_child(child, "StorageDecl", OfferType::Static);
            }
            Some(_) => {
                self.errors.push(Error::invalid_field("StorageDecl", "source"));
            }
            None => {
                self.errors.push(Error::missing_field("StorageDecl", "source"));
            }
        };
        if check_name(storage.name.as_ref(), "StorageDecl", "name", &mut self.errors) {
            let name = storage.name.as_ref().unwrap();
            if !self.all_capability_ids.insert(name) {
                self.errors.push(Error::duplicate_field("StorageDecl", "name", name.as_str()));
            }
            self.all_storage_and_sources.insert(name, storage.source.as_ref());
        }
        if storage.storage_id.is_none() {
            self.errors.push(Error::missing_field("StorageDecl", "storage_id"));
        }
        check_name(storage.backing_dir.as_ref(), "StorageDecl", "backing_dir", &mut self.errors);
    }

    fn validate_runner_decl(&mut self, runner: &'a fsys::RunnerDecl, as_builtin: bool) {
        if check_name(runner.name.as_ref(), "RunnerDecl", "name", &mut self.errors) {
            let name = runner.name.as_ref().unwrap();
            if !self.all_capability_ids.insert(name) {
                self.errors.push(Error::duplicate_field("RunnerDecl", "name", name.as_str()));
            }
            self.all_runners.insert(name);
        }
        match as_builtin {
            true => {
                if let Some(path) = runner.source_path.as_ref() {
                    self.errors.push(Error::extraneous_source_path("RunnerDecl", path))
                }
            }
            false => {
                check_path(
                    runner.source_path.as_ref(),
                    "RunnerDecl",
                    "source_path",
                    &mut self.errors,
                );
            }
        }
    }

    fn validate_resolver_decl(&mut self, resolver: &'a fsys::ResolverDecl, as_builtin: bool) {
        if check_name(resolver.name.as_ref(), "ResolverDecl", "name", &mut self.errors) {
            let name = resolver.name.as_ref().unwrap();
            if !self.all_capability_ids.insert(name) {
                self.errors.push(Error::duplicate_field("ResolverDecl", "name", name.as_str()));
            }
            self.all_resolvers.insert(name);
        }
        match as_builtin {
            true => {
                if let Some(path) = resolver.source_path.as_ref() {
                    self.errors.push(Error::extraneous_source_path("ResolverDecl", path))
                }
            }
            false => {
                check_path(
                    resolver.source_path.as_ref(),
                    "ResolverDecl",
                    "source_path",
                    &mut self.errors,
                );
            }
        }
    }

    fn validate_environment_debug_registration(
        &mut self,
        debug: &'a fsys::DebugRegistration,
        environment_name: Option<&'a String>,
    ) {
        match debug {
            fsys::DebugRegistration::Protocol(o) => {
                let decl = "DebugProtocolRegistration";
                self.validate_environment_debug_fields(
                    decl,
                    o.source.as_ref(),
                    o.source_name.as_ref(),
                    o.target_name.as_ref(),
                );

                if let (Some(fsys::Ref::Self_(_)), Some(ref name)) = (&o.source, &o.source_name) {
                    if !self.all_protocols.contains(&name as &str) {
                        self.errors.push(Error::invalid_field(decl, "source"));
                    }
                }

                if let Some(env_name) = &environment_name {
                    let source = DependencyNode::try_from_ref(o.source.as_ref());
                    let target = Some(DependencyNode::Environment(env_name));
                    self.add_strong_dep(None, source, target);
                }
            }
            fsys::DebugRegistrationUnknown!() => {
                self.errors.push(Error::invalid_field("EnvironmentDecl", "debug"));
            }
        }
    }

    fn validate_environment_debug_fields(
        &mut self,
        decl: &str,
        source: Option<&fsys::Ref>,
        source_name: Option<&String>,
        target_name: Option<&'a String>,
    ) {
        // We don't support "source" from "capability" for now.
        match source {
            Some(fsys::Ref::Parent(_)) => {}
            Some(fsys::Ref::Self_(_)) => {}
            Some(fsys::Ref::Framework(_)) => {}
            Some(fsys::Ref::Child(child)) => {
                self.validate_source_child(child, decl, OfferType::Static)
            }
            Some(_) => self.errors.push(Error::invalid_field(decl, "source")),
            None => self.errors.push(Error::missing_field(decl, "source")),
        }
        check_name(source_name, decl, "source_name", &mut self.errors);
        check_name(target_name, decl, "target_name", &mut self.errors);
    }

    fn validate_event_decl(&mut self, event: &'a fsys::EventDecl) {
        if check_name(event.name.as_ref(), "EventDecl", "name", &mut self.errors) {
            let name = event.name.as_ref().unwrap();
            if !self.all_capability_ids.insert(name) {
                self.errors.push(Error::duplicate_field("EventDecl", "name", name.as_str()));
            }
        }
    }

    fn validate_source_child(
        &mut self,
        child: &fsys::ChildRef,
        decl_type: &str,
        offer_type: OfferType,
    ) {
        let mut valid = true;
        valid &= check_name(Some(&child.name), decl_type, "source.child.name", &mut self.errors);
        match offer_type {
            OfferType::Static => {
                valid &= if child.collection.is_some() {
                    self.errors.push(Error::extraneous_field(decl_type, "source.child.collection"));
                    false
                } else {
                    true
                };
                if !valid {
                    return;
                }
                if !self.all_children.contains_key(&child.name as &str) {
                    self.errors.push(Error::invalid_child(
                        decl_type,
                        "source",
                        &child.name as &str,
                    ));
                }
            }
            OfferType::Dynamic => {}
        }
    }

    fn validate_source_collection(&mut self, collection: &fsys::CollectionRef, decl_type: &str) {
        if !check_name(
            Some(&collection.name),
            decl_type,
            "source.collection.name",
            &mut self.errors,
        ) {
            return;
        }
        if !self.all_collections.contains(&collection.name as &str) {
            self.errors.push(Error::invalid_collection(
                decl_type,
                "source",
                &collection.name as &str,
            ));
        }
    }

    fn validate_source_capability(
        &mut self,
        capability: &fsys::CapabilityRef,
        decl_type: &str,
        field: &str,
    ) {
        if !self.all_capability_ids.contains(capability.name.as_str()) {
            self.errors.push(Error::invalid_capability(decl_type, field, &capability.name));
        }
    }

    fn validate_storage_source(&mut self, source_name: &String, decl_type: &str) {
        if check_name(Some(source_name), decl_type, "source.storage.name", &mut self.errors) {
            if !self.all_storage_and_sources.contains_key(source_name.as_str()) {
                self.errors.push(Error::invalid_storage(decl_type, "source", source_name));
            }
        }
    }

    fn validate_expose_decl(
        &mut self,
        expose: &'a fsys::ExposeDecl,
        prev_target_ids: &mut HashMap<&'a str, AllowableIds>,
    ) {
        match expose {
            fsys::ExposeDecl::Service(e) => {
                let decl = "ExposeServiceDecl";
                self.validate_expose_fields(
                    decl,
                    AllowableIds::Many,
                    CollectionSource::Allow,
                    e.source.as_ref(),
                    e.source_name.as_ref(),
                    e.target_name.as_ref(),
                    e.target.as_ref(),
                    prev_target_ids,
                );
                // If the expose source is `self`, ensure we have a corresponding ServiceDecl.
                // TODO: Consider bringing this bit into validate_expose_fields.
                if let (Some(fsys::Ref::Self_(_)), Some(ref name)) = (&e.source, &e.source_name) {
                    if !self.all_services.contains(&name as &str) {
                        self.errors.push(Error::invalid_capability(decl, "source", name));
                    }
                }
            }
            fsys::ExposeDecl::Protocol(e) => {
                let decl = "ExposeProtocolDecl";
                self.validate_expose_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    e.source.as_ref(),
                    e.source_name.as_ref(),
                    e.target_name.as_ref(),
                    e.target.as_ref(),
                    prev_target_ids,
                );
                // If the expose source is `self`, ensure we have a corresponding ProtocolDecl.
                // TODO: Consider bringing this bit into validate_expose_fields.
                if let (Some(fsys::Ref::Self_(_)), Some(ref name)) = (&e.source, &e.source_name) {
                    if !self.all_protocols.contains(&name as &str) {
                        self.errors.push(Error::invalid_capability(decl, "source", name));
                    }
                }
            }
            fsys::ExposeDecl::Directory(e) => {
                let decl = "ExposeDirectoryDecl";
                self.validate_expose_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    e.source.as_ref(),
                    e.source_name.as_ref(),
                    e.target_name.as_ref(),
                    e.target.as_ref(),
                    prev_target_ids,
                );
                // If the expose source is `self`, ensure we have a corresponding DirectoryDecl.
                // TODO: Consider bringing this bit into validate_expose_fields.
                if let (Some(fsys::Ref::Self_(_)), Some(ref name)) = (&e.source, &e.source_name) {
                    if !self.all_directories.contains(&name as &str) {
                        self.errors.push(Error::invalid_capability(decl, "source", name));
                    }
                    if name.starts_with('/') && e.rights.is_none() {
                        self.errors.push(Error::missing_field(decl, "rights"));
                    }
                }

                // Subdir makes sense when routing, but when exposing to framework the subdirectory
                // can be exposed directly.
                match e.target.as_ref() {
                    Some(fsys::Ref::Framework(_)) => {
                        if e.subdir.is_some() {
                            self.errors.push(Error::invalid_field(decl, "subdir"));
                        }
                    }
                    _ => {}
                }

                if let Some(subdir) = e.subdir.as_ref() {
                    check_relative_path(Some(subdir), decl, "subdir", &mut self.errors);
                }
            }
            fsys::ExposeDecl::Runner(e) => {
                let decl = "ExposeRunnerDecl";
                self.validate_expose_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    e.source.as_ref(),
                    e.source_name.as_ref(),
                    e.target_name.as_ref(),
                    e.target.as_ref(),
                    prev_target_ids,
                );
                // If the expose source is `self`, ensure we have a corresponding RunnerDecl.
                if let (Some(fsys::Ref::Self_(_)), Some(ref name)) = (&e.source, &e.source_name) {
                    if !self.all_runners.contains(&name as &str) {
                        self.errors.push(Error::invalid_capability(decl, "source", name));
                    }
                }
            }
            fsys::ExposeDecl::Resolver(e) => {
                let decl = "ExposeResolverDecl";
                self.validate_expose_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    e.source.as_ref(),
                    e.source_name.as_ref(),
                    e.target_name.as_ref(),
                    e.target.as_ref(),
                    prev_target_ids,
                );
                // If the expose source is `self`, ensure we have a corresponding ResolverDecl.
                if let (Some(fsys::Ref::Self_(_)), Some(ref name)) = (&e.source, &e.source_name) {
                    if !self.all_resolvers.contains(&name as &str) {
                        self.errors.push(Error::invalid_capability(decl, "source", name));
                    }
                }
            }
            fsys::ExposeDeclUnknown!() => {
                self.errors.push(Error::invalid_field("ComponentDecl", "expose"));
            }
        }
    }

    fn validate_expose_fields(
        &mut self,
        decl: &str,
        allowable_ids: AllowableIds,
        collection_source: CollectionSource,
        source: Option<&fsys::Ref>,
        source_name: Option<&String>,
        target_name: Option<&'a String>,
        target: Option<&fsys::Ref>,
        prev_child_target_ids: &mut HashMap<&'a str, AllowableIds>,
    ) {
        match source {
            Some(r) => match r {
                fsys::Ref::Self_(_) => {}
                fsys::Ref::Framework(_) => {}
                fsys::Ref::Child(child) => {
                    self.validate_source_child(child, decl, OfferType::Static);
                }
                fsys::Ref::Capability(c) => {
                    self.validate_source_capability(c, decl, "source");
                }
                fsys::Ref::Collection(c) if collection_source == CollectionSource::Allow => {
                    self.validate_source_collection(c, decl);
                }
                _ => {
                    self.errors.push(Error::invalid_field(decl, "source"));
                }
            },
            None => {
                self.errors.push(Error::missing_field(decl, "source"));
            }
        }
        match target {
            Some(r) => match r {
                fsys::Ref::Parent(_) => {}
                fsys::Ref::Framework(_) => {
                    if source != Some(&fsys::Ref::Self_(fsys::SelfRef {})) {
                        self.errors.push(Error::invalid_field(decl, "target"));
                    }
                }
                _ => {
                    self.errors.push(Error::invalid_field(decl, "target"));
                }
            },
            None => {
                self.errors.push(Error::missing_field(decl, "target"));
            }
        }
        check_name(source_name, decl, "source_name", &mut self.errors);
        if check_name(target_name, decl, "target_name", &mut self.errors) {
            // TODO: This logic needs to pair the target name with the target before concluding
            // there's a duplicate.
            let target_name = target_name.unwrap();
            if let Some(prev_state) = prev_child_target_ids.insert(target_name, allowable_ids) {
                if prev_state == AllowableIds::One || prev_state != allowable_ids {
                    self.errors.push(Error::duplicate_field(decl, "target_name", target_name));
                }
            }
        }
    }

    /// Adds a strong dependency between two nodes in the dependency graph between `source` and
    /// `target`.
    ///
    /// `source_name` is the name of the capability being routed (if applicable). The function is
    /// a no-op if `source` or `target` is `None`; this behavior is a convenience so that the
    /// caller can directly pass the result of `DependencyNode::try_from_ref`.
    fn add_strong_dep(
        &mut self,
        source_name: Option<&'a String>,
        source: Option<DependencyNode<'a>>,
        target: Option<DependencyNode<'a>>,
    ) {
        if source.is_none() || target.is_none() {
            return;
        }
        let source = source.unwrap();
        let target = target.unwrap();
        let possible_storage_name = match (source, source_name) {
            (DependencyNode::Capability(name), _) => Some(name),
            (DependencyNode::Self_, Some(name)) => Some(name.as_str()),
            _ => None,
        };
        // A dependency on a storage capability is really a dependency on the backing dir. Perform
        // that translation here.
        let source = possible_storage_name
            .map(|name| self.all_storage_and_sources.get(name))
            .flatten()
            .map(|r| DependencyNode::try_from_ref(*r))
            .flatten()
            .unwrap_or(source);
        if source == target {
            // This is already its own error, or is a valid `use from self`, don't report this as a
            // cycle.
        } else {
            self.strong_dependencies.add_edge(source, target);
        }
    }

    fn validate_offers_decl(&mut self, offer: &'a fsys::OfferDecl, offer_type: OfferType) {
        match offer {
            fsys::OfferDecl::Service(o) => {
                let decl = "OfferServiceDecl";
                self.validate_offer_fields(
                    decl,
                    AllowableIds::Many,
                    CollectionSource::Allow,
                    offer_type,
                    o.source.as_ref(),
                    o.source_name.as_ref(),
                    o.target.as_ref(),
                    o.target_name.as_ref(),
                );
                match offer_type {
                    OfferType::Static => {
                        // If the offer source is `self`, ensure we have a corresponding ServiceDecl.
                        // TODO: Consider bringing this bit into validate_offer_fields
                        if let (Some(fsys::Ref::Self_(_)), Some(ref name)) =
                            (&o.source, &o.source_name)
                        {
                            if !self.all_services.contains(&name as &str) {
                                self.errors.push(Error::invalid_field(decl, "source"));
                            }
                        }
                        self.add_strong_dep(
                            o.source_name.as_ref(),
                            DependencyNode::try_from_ref(o.source.as_ref()),
                            DependencyNode::try_from_ref(o.target.as_ref()),
                        );
                    }
                    OfferType::Dynamic => {}
                }
            }
            fsys::OfferDecl::Protocol(o) => {
                let decl = "OfferProtocolDecl";
                self.validate_offer_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    offer_type,
                    o.source.as_ref(),
                    o.source_name.as_ref(),
                    o.target.as_ref(),
                    o.target_name.as_ref(),
                );
                if o.dependency_type.is_none() {
                    self.errors.push(Error::missing_field(decl, "dependency_type"));
                } else if o.dependency_type == Some(fsys::DependencyType::Strong) {
                    match offer_type {
                        OfferType::Static => {
                            self.add_strong_dep(
                                o.source_name.as_ref(),
                                DependencyNode::try_from_ref(o.source.as_ref()),
                                DependencyNode::try_from_ref(o.target.as_ref()),
                            );
                        }
                        OfferType::Dynamic => {}
                    }
                }
                match offer_type {
                    OfferType::Static => {
                        // If the offer source is `self`, ensure we have a
                        // corresponding ProtocolDecl.
                        // TODO: Consider bringing this bit into validate_offer_fields.
                        if let (Some(fsys::Ref::Self_(_)), Some(ref name)) =
                            (&o.source, &o.source_name)
                        {
                            if !self.all_protocols.contains(&name as &str) {
                                self.errors.push(Error::invalid_capability(decl, "source", name));
                            }
                        }
                    }
                    OfferType::Dynamic => {}
                }
            }
            fsys::OfferDecl::Directory(o) => {
                let decl = "OfferDirectoryDecl";
                self.validate_offer_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    offer_type,
                    o.source.as_ref(),
                    o.source_name.as_ref(),
                    o.target.as_ref(),
                    o.target_name.as_ref(),
                );
                if o.dependency_type.is_none() {
                    self.errors.push(Error::missing_field(decl, "dependency_type"));
                } else if o.dependency_type == Some(fsys::DependencyType::Strong) {
                    match offer_type {
                        OfferType::Static => {
                            self.add_strong_dep(
                                o.source_name.as_ref(),
                                DependencyNode::try_from_ref(o.source.as_ref()),
                                DependencyNode::try_from_ref(o.target.as_ref()),
                            );
                            // If the offer source is `self`, ensure we have a corresponding
                            // DirectoryDecl.
                            //
                            // TODO: Consider bringing this bit into validate_offer_fields.
                            if let (Some(fsys::Ref::Self_(_)), Some(ref name)) =
                                (&o.source, &o.source_name)
                            {
                                if !self.all_directories.contains(&name as &str) {
                                    self.errors
                                        .push(Error::invalid_capability(decl, "source", name));
                                }
                            }
                        }
                        OfferType::Dynamic => {}
                    }
                }

                if let Some(subdir) = o.subdir.as_ref() {
                    check_relative_path(
                        Some(subdir),
                        "OfferDirectoryDecl",
                        "subdir",
                        &mut self.errors,
                    );
                }
            }
            fsys::OfferDecl::Storage(o) => {
                self.validate_storage_offer_fields(
                    "OfferStorageDecl",
                    offer_type,
                    o.source_name.as_ref(),
                    o.source.as_ref(),
                    o.target.as_ref(),
                );

                match offer_type {
                    OfferType::Static => {
                        // Storage capabilities with a source of `Ref::Self_`
                        // don't interact with the component's runtime in any
                        // way, they're actually synthesized by the framework
                        // out of a pre-existing directory capability. Thus, its
                        // actual source is the backing directory capability.
                        match (o.source.as_ref(), o.source_name.as_ref()) {
                            (Some(fsys::Ref::Self_ { .. }), Some(source_name)) => {
                                if let Some(source) = DependencyNode::try_from_ref(
                                    *self
                                        .all_storage_and_sources
                                        .get(source_name.as_str())
                                        .unwrap_or(&None),
                                ) {
                                    if let Some(target) =
                                        DependencyNode::try_from_ref(o.target.as_ref())
                                    {
                                        self.strong_dependencies.add_edge(source, target);
                                    }
                                }
                            }
                            _ => self.add_strong_dep(
                                o.source_name.as_ref(),
                                DependencyNode::try_from_ref(o.source.as_ref()),
                                DependencyNode::try_from_ref(o.target.as_ref()),
                            ),
                        }
                    }
                    OfferType::Dynamic => {}
                }
            }
            fsys::OfferDecl::Runner(o) => {
                let decl = "OfferRunnerDecl";
                self.validate_offer_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    offer_type,
                    o.source.as_ref(),
                    o.source_name.as_ref(),
                    o.target.as_ref(),
                    o.target_name.as_ref(),
                );
                match offer_type {
                    OfferType::Static => {
                        // If the offer source is `self`, ensure we have a corresponding RunnerDecl.
                        if let (Some(fsys::Ref::Self_(_)), Some(ref name)) =
                            (&o.source, &o.source_name)
                        {
                            if !self.all_runners.contains(&name as &str) {
                                self.errors.push(Error::invalid_capability(decl, "source", name));
                            }
                        }
                        self.add_strong_dep(
                            o.source_name.as_ref(),
                            DependencyNode::try_from_ref(o.source.as_ref()),
                            DependencyNode::try_from_ref(o.target.as_ref()),
                        );
                    }
                    OfferType::Dynamic => {}
                }
            }
            fsys::OfferDecl::Resolver(o) => {
                let decl = "OfferResolverDecl";
                self.validate_offer_fields(
                    decl,
                    AllowableIds::One,
                    CollectionSource::Deny,
                    offer_type,
                    o.source.as_ref(),
                    o.source_name.as_ref(),
                    o.target.as_ref(),
                    o.target_name.as_ref(),
                );

                match offer_type {
                    OfferType::Static => {
                        // If the offer source is `self`, ensure we have a
                        // corresponding ResolverDecl.
                        if let (Some(fsys::Ref::Self_(_)), Some(ref name)) =
                            (&o.source, &o.source_name)
                        {
                            if !self.all_resolvers.contains(&name as &str) {
                                self.errors.push(Error::invalid_capability(decl, "source", name));
                            }
                        }
                        self.add_strong_dep(
                            o.source_name.as_ref(),
                            DependencyNode::try_from_ref(o.source.as_ref()),
                            DependencyNode::try_from_ref(o.target.as_ref()),
                        );
                    }
                    OfferType::Dynamic => {}
                }
            }
            fsys::OfferDecl::Event(e) => {
                self.validate_event_offer_fields(e, offer_type);
            }
            fsys::OfferDeclUnknown!() => {
                self.errors.push(Error::invalid_field("ComponentDecl", "offer"));
            }
        }
    }

    /// Validates that the offer target is to a valid child or collection.
    fn validate_offer_target(
        &mut self,
        target: &'a Option<fsys::Ref>,
        decl_type: &str,
        field_name: &str,
    ) -> Option<TargetId<'a>> {
        match target.as_ref() {
            Some(fsys::Ref::Child(child)) => {
                if self.validate_child_ref(decl_type, field_name, &child) {
                    Some(TargetId::Component(&child.name))
                } else {
                    None
                }
            }
            Some(fsys::Ref::Collection(collection)) => {
                if self.validate_collection_ref(decl_type, field_name, &collection) {
                    Some(TargetId::Collection(&collection.name))
                } else {
                    None
                }
            }
            Some(_) => {
                self.errors.push(Error::invalid_field(decl_type, field_name));
                None
            }
            None => {
                self.errors.push(Error::missing_field(decl_type, field_name));
                None
            }
        }
    }

    fn validate_offer_fields(
        &mut self,
        decl: &str,
        allowable_names: AllowableIds,
        collection_source: CollectionSource,
        offer_type: OfferType,
        source: Option<&fsys::Ref>,
        source_name: Option<&String>,
        target: Option<&'a fsys::Ref>,
        target_name: Option<&'a String>,
    ) {
        match source {
            Some(fsys::Ref::Parent(_)) => {}
            Some(fsys::Ref::Self_(_)) => {}
            Some(fsys::Ref::Framework(_)) => {}
            Some(fsys::Ref::Child(child)) => self.validate_source_child(child, decl, offer_type),
            Some(fsys::Ref::Capability(c)) => self.validate_source_capability(c, decl, "source"),
            Some(fsys::Ref::Collection(c)) if collection_source == CollectionSource::Allow => {
                self.validate_source_collection(c, decl)
            }
            Some(_) => self.errors.push(Error::invalid_field(decl, "source")),
            None => self.errors.push(Error::missing_field(decl, "source")),
        }
        check_name(source_name, decl, "source_name", &mut self.errors);
        match (offer_type, target) {
            (OfferType::Static, Some(fsys::Ref::Child(c))) => {
                self.validate_target_child(decl, allowable_names, c, source, target_name);
            }
            (OfferType::Static, Some(fsys::Ref::Collection(c))) => {
                self.validate_target_collection(decl, allowable_names, c, target_name);
            }
            (OfferType::Static, Some(_)) => {
                self.errors.push(Error::invalid_field(decl, "target"));
            }
            (OfferType::Static, None) => {
                self.errors.push(Error::missing_field(decl, "target"));
            }

            (OfferType::Dynamic, Some(_)) => {
                self.errors.push(Error::extraneous_field(decl, "target"));
            }
            (OfferType::Dynamic, None) => {}
        }
        check_name(target_name, decl, "target_name", &mut self.errors);
    }

    fn validate_storage_offer_fields(
        &mut self,
        decl: &str,
        offer_type: OfferType,
        source_name: Option<&'a String>,
        source: Option<&'a fsys::Ref>,
        target: Option<&'a fsys::Ref>,
    ) {
        if source_name.is_none() {
            self.errors.push(Error::missing_field(decl, "source_name"));
        }
        match source {
            Some(fsys::Ref::Parent(_)) => (),
            Some(fsys::Ref::Self_(_)) => {
                self.validate_storage_source(source_name.unwrap(), decl);
            }
            Some(_) => {
                self.errors.push(Error::invalid_field(decl, "source"));
            }
            None => {
                self.errors.push(Error::missing_field(decl, "source"));
            }
        }
        match offer_type {
            OfferType::Static => {
                self.validate_storage_target(decl, target);
            }
            OfferType::Dynamic => {
                if target.is_some() {
                    self.errors.push(Error::extraneous_field(decl, "target"));
                }
            }
        }
    }

    fn validate_event_offer_fields(
        &mut self,
        event: &'a fsys::OfferEventDecl,
        offer_type: OfferType,
    ) {
        let decl = "OfferEventDecl";
        check_name(event.source_name.as_ref(), decl, "source_name", &mut self.errors);

        // Only parent and framework are valid.
        match event.source {
            Some(fsys::Ref::Parent(_)) => {}
            Some(fsys::Ref::Framework(_)) => {}
            Some(_) => {
                self.errors.push(Error::invalid_field(decl, "source"));
            }
            None => {
                self.errors.push(Error::missing_field(decl, "source"));
            }
        };

        match offer_type {
            OfferType::Static => {
                let target_id = self.validate_offer_target(&event.target, decl, "target");
                if let (Some(target_id), Some(target_name)) =
                    (target_id, event.target_name.as_ref())
                {
                    // Assuming the target_name is valid, ensure the target_name isn't already used.
                    if let Some(_) = self
                        .target_ids
                        .entry(target_id)
                        .or_insert(HashMap::new())
                        .insert(target_name, AllowableIds::One)
                    {
                        self.errors.push(Error::duplicate_field(
                            decl,
                            "target_name",
                            target_name as &str,
                        ));
                    }
                }
            }
            OfferType::Dynamic => {
                if event.target.is_some() {
                    self.errors.push(Error::extraneous_field(decl, "target"));
                }
            }
        }
        check_name(event.target_name.as_ref(), decl, "target_name", &mut self.errors);
        check_events_mode(&event.mode, "OfferEventDecl", "mode", &mut self.errors);
    }

    /// Check a `ChildRef` contains a valid child that exists.
    ///
    /// We ensure the target child is statically defined (i.e., not a dynamic child inside
    /// a collection).
    fn validate_child_ref(&mut self, decl: &str, field_name: &str, child: &fsys::ChildRef) -> bool {
        // Ensure the name is valid, and the reference refers to a static child.
        //
        // We attempt to list all errors if possible.
        let mut valid = true;
        if !check_name(
            Some(&child.name),
            decl,
            &format!("{}.child.name", field_name),
            &mut self.errors,
        ) {
            valid = false;
        }
        if child.collection.is_some() {
            self.errors
                .push(Error::extraneous_field(decl, format!("{}.child.collection", field_name)));
            valid = false;
        }
        if !valid {
            return false;
        }

        // Ensure the child exists.
        let name: &str = &child.name;
        if !self.all_children.contains_key(name) {
            self.errors.push(Error::invalid_child(decl, field_name, name));
            return false;
        }

        true
    }

    /// Check a `CollectionRef` is valid and refers to an existing collection.
    fn validate_collection_ref(
        &mut self,
        decl: &str,
        field_name: &str,
        collection: &fsys::CollectionRef,
    ) -> bool {
        // Ensure the name is valid.
        if !check_name(
            Some(&collection.name),
            decl,
            &format!("{}.collection.name", field_name),
            &mut self.errors,
        ) {
            return false;
        }

        // Ensure the collection exists.
        if !self.all_collections.contains(&collection.name as &str) {
            self.errors.push(Error::invalid_collection(decl, field_name, &collection.name as &str));
            return false;
        }

        true
    }

    fn validate_target_child(
        &mut self,
        decl: &str,
        allowable_names: AllowableIds,
        child: &'a fsys::ChildRef,
        source: Option<&fsys::Ref>,
        target_name: Option<&'a String>,
    ) {
        if !self.validate_child_ref(decl, "target", child) {
            return;
        }
        if let Some(target_name) = target_name {
            let names_for_target =
                self.target_ids.entry(TargetId::Component(&child.name)).or_insert(HashMap::new());
            if let Some(prev_state) = names_for_target.insert(target_name, allowable_names) {
                if prev_state == AllowableIds::One || prev_state != allowable_names {
                    self.errors.push(Error::duplicate_field(
                        decl,
                        "target_name",
                        target_name as &str,
                    ));
                }
            }
            if let Some(source) = source {
                if let fsys::Ref::Child(source_child) = source {
                    if source_child.name == child.name {
                        self.errors
                            .push(Error::offer_target_equals_source(decl, &child.name as &str));
                    }
                }
            }
        }
    }

    fn validate_target_collection(
        &mut self,
        decl: &str,
        allowable_names: AllowableIds,
        collection: &'a fsys::CollectionRef,
        target_name: Option<&'a String>,
    ) {
        if !self.validate_collection_ref(decl, "target", &collection) {
            return;
        }
        if let Some(target_name) = target_name {
            let names_for_target = self
                .target_ids
                .entry(TargetId::Collection(&collection.name))
                .or_insert(HashMap::new());
            if let Some(prev_state) = names_for_target.insert(target_name, allowable_names) {
                if prev_state == AllowableIds::One || prev_state != allowable_names {
                    self.errors.push(Error::duplicate_field(
                        decl,
                        "target_name",
                        target_name as &str,
                    ));
                }
            }
        }
    }

    fn validate_storage_target(&mut self, decl: &str, target: Option<&'a fsys::Ref>) {
        match target {
            Some(fsys::Ref::Child(c)) => {
                self.validate_child_ref(decl, "target", &c);
            }
            Some(fsys::Ref::Collection(c)) => {
                self.validate_collection_ref(decl, "target", &c);
            }
            Some(_) => self.errors.push(Error::invalid_field(decl, "target")),
            None => self.errors.push(Error::missing_field(decl, "target")),
        }
    }
}

fn check_presence_and_length(
    max_len: usize,
    prop: Option<&String>,
    decl_type: &str,
    keyword: &str,
    errors: &mut Vec<Error>,
) {
    match prop {
        Some(prop) if prop.len() == 0 => errors.push(Error::empty_field(decl_type, keyword)),
        Some(prop) if prop.len() > max_len => {
            errors.push(Error::field_too_long(decl_type, keyword))
        }
        Some(_) => (),
        None => errors.push(Error::missing_field(decl_type, keyword)),
    }
}

fn check_path(
    prop: Option<&String>,
    decl_type: &str,
    keyword: &str,
    errors: &mut Vec<Error>,
) -> bool {
    let start_err_len = errors.len();
    check_presence_and_length(MAX_PATH_LENGTH, prop, decl_type, keyword, errors);
    if let Some(path) = prop {
        // Paths must be more than 1 character long
        if path.len() < 2 {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
        // Paths must start with `/`
        if !path.starts_with('/') {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
        // Paths cannot have two `/`s in a row
        if path.contains("//") {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
        // Paths cannot end with `/`
        if path.ends_with('/') {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
    }
    start_err_len == errors.len()
}

fn check_relative_path(
    prop: Option<&String>,
    decl_type: &str,
    keyword: &str,
    errors: &mut Vec<Error>,
) -> bool {
    let start_err_len = errors.len();
    check_presence_and_length(MAX_PATH_LENGTH, prop, decl_type, keyword, errors);
    if let Some(path) = prop {
        // Relative paths must be nonempty
        if path.is_empty() {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
        // Relative paths cannot start with `/`
        if path.starts_with('/') {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
        // Relative paths cannot have two `/`s in a row
        if path.contains("//") {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
        // Relative paths cannot end with `/`
        if path.ends_with('/') {
            errors.push(Error::invalid_field(decl_type, keyword));
            return false;
        }
    }
    start_err_len == errors.len()
}

fn check_name(
    prop: Option<&String>,
    decl_type: &str,
    keyword: &str,
    errors: &mut Vec<Error>,
) -> bool {
    let start_err_len = errors.len();
    check_presence_and_length(MAX_NAME_LENGTH, prop, decl_type, keyword, errors);
    let mut invalid_field = false;
    if let Some(name) = prop {
        let mut char_iter = name.chars();
        if let Some(first_char) = char_iter.next() {
            if !first_char.is_ascii_alphanumeric() && first_char != '_' {
                invalid_field = true;
            }
        }
        for c in char_iter {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                // Ok
            } else {
                invalid_field = true;
            }
        }
    }
    if invalid_field {
        errors.push(Error::invalid_field(decl_type, keyword));
    }
    start_err_len == errors.len()
}

// TODO: This should probably be checking with the `url` crate
fn check_url(
    prop: Option<&String>,
    decl_type: &str,
    keyword: &str,
    errors: &mut Vec<Error>,
) -> bool {
    let start_err_len = errors.len();
    check_presence_and_length(MAX_URL_LENGTH, prop, decl_type, keyword, errors);
    if let Some(url) = prop {
        let mut chars_iter = url.chars();
        let mut first_char = true;
        while let Some(c) = chars_iter.next() {
            match c {
                '0'..='9' | 'a'..='z' | '+' | '-' | '.' => first_char = false,
                ':' => {
                    if first_char {
                        // There must be at least one character in the schema
                        errors.push(Error::invalid_field(decl_type, keyword));
                        return false;
                    }
                    // Once a `:` character is found, it must be followed by two `/` characters and
                    // then at least one more character. Note that these sequential calls to
                    // `.next()` without checking the result won't panic because `Chars` implements
                    // `FusedIterator`.
                    match (chars_iter.next(), chars_iter.next(), chars_iter.next()) {
                        (Some('/'), Some('/'), Some(_)) => return start_err_len == errors.len(),
                        _ => {
                            errors.push(Error::invalid_field(decl_type, keyword));
                            return false;
                        }
                    }
                }
                // If the first character is # then it's a relative URL.
                // It must have at least one more character.
                '#' => {
                    if first_char && chars_iter.next().is_some() {
                        return start_err_len == errors.len();
                    }
                    errors.push(Error::invalid_field(decl_type, keyword));
                    return false;
                }
                _ => {
                    errors.push(Error::invalid_field(decl_type, keyword));
                    return false;
                }
            }
        }
        // If we've reached here then the string terminated unexpectedly
        errors.push(Error::invalid_field(decl_type, keyword));
    }
    start_err_len == errors.len()
}

fn check_url_scheme(
    prop: Option<&String>,
    decl_type: &str,
    keyword: &str,
    errors: &mut Vec<Error>,
) -> bool {
    if let Some(scheme) = prop {
        if let Err(err) = cm_types::UrlScheme::validate(scheme) {
            errors.push(match err {
                cm_types::ParseError::InvalidLength => {
                    if scheme.is_empty() {
                        Error::empty_field(decl_type, keyword)
                    } else {
                        Error::field_too_long(decl_type, keyword)
                    }
                }
                cm_types::ParseError::InvalidValue => Error::invalid_field(decl_type, keyword),
                e => {
                    panic!("unexpected parse error: {:?}", e);
                }
            });
            return false;
        }
    } else {
        errors.push(Error::missing_field(decl_type, keyword));
        return false;
    }
    true
}

/// Events mode should be always present.
fn check_events_mode(
    mode: &Option<fsys::EventMode>,
    decl_type: &str,
    field_name: &str,
    errors: &mut Vec<Error>,
) {
    if mode.is_none() {
        errors.push(Error::missing_field(decl_type, field_name));
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, fidl_fuchsia_data as fdata, fidl_fuchsia_io2 as fio2, fidl_fuchsia_sys2::*,
        lazy_static::lazy_static, proptest::prelude::*, regex::Regex, test_case::test_case,
    };

    const PATH_REGEX_STR: &str = r"(/[^/]+)+";
    const NAME_REGEX_STR: &str = r"[0-9a-zA-Z_][0-9a-zA-Z_\-\.]*";
    const URL_REGEX_STR: &str = r"([0-9a-z\+\-\.]+://.+|#.+)";

    lazy_static! {
        static ref PATH_REGEX: Regex =
            Regex::new(&("^".to_string() + PATH_REGEX_STR + "$")).unwrap();
        static ref NAME_REGEX: Regex =
            Regex::new(&("^".to_string() + NAME_REGEX_STR + "$")).unwrap();
        static ref URL_REGEX: Regex = Regex::new(&("^".to_string() + URL_REGEX_STR + "$")).unwrap();
    }

    proptest! {
        #[test]
        fn check_path_matches_regex(s in PATH_REGEX_STR) {
            if s.len() < MAX_PATH_LENGTH {
                let mut errors = vec![];
                prop_assert!(check_path(Some(&s), "", "", &mut errors));
                prop_assert!(errors.is_empty());
            }
        }
        #[test]
        fn check_name_matches_regex(s in NAME_REGEX_STR) {
            if s.len() < MAX_NAME_LENGTH {
                let mut errors = vec![];
                prop_assert!(check_name(Some(&s), "", "", &mut errors));
                prop_assert!(errors.is_empty());
            }
        }
        #[test]
        fn check_url_matches_regex(s in URL_REGEX_STR) {
            if s.len() < MAX_URL_LENGTH {
                let mut errors = vec![];
                prop_assert!(check_url(Some(&s), "", "", &mut errors));
                prop_assert!(errors.is_empty());
            }
        }
        #[test]
        fn check_path_fails_invalid_input(s in ".*") {
            if !PATH_REGEX.is_match(&s) {
                let mut errors = vec![];
                prop_assert!(!check_path(Some(&s), "", "", &mut errors));
                prop_assert!(!errors.is_empty());
            }
        }
        #[test]
        fn check_name_fails_invalid_input(s in ".*") {
            if !NAME_REGEX.is_match(&s) {
                let mut errors = vec![];
                prop_assert!(!check_name(Some(&s), "", "", &mut errors));
                prop_assert!(!errors.is_empty());
            }
        }
        #[test]
        fn check_url_fails_invalid_input(s in ".*") {
            if !URL_REGEX.is_match(&s) {
                let mut errors = vec![];
                prop_assert!(!check_url(Some(&s), "", "", &mut errors));
                prop_assert!(!errors.is_empty());
            }
        }
    }

    fn validate_test(input: ComponentDecl, expected_res: Result<(), ErrorList>) {
        let res = validate(&input);
        assert_eq!(res, expected_res);
    }

    fn validate_test_any_result(input: ComponentDecl, expected_res: Vec<Result<(), ErrorList>>) {
        let res = format!("{:?}", validate(&input));
        let expected_res_debug = format!("{:?}", expected_res);

        let matched_exp =
            expected_res.into_iter().find(|expected| res == format!("{:?}", expected));

        assert!(
            matched_exp.is_some(),
            "assertion failed: Expected one of:\n{:?}\nActual:\n{:?}",
            expected_res_debug,
            res
        );
    }

    fn validate_capabilities_test(
        input: Vec<CapabilityDecl>,
        as_builtin: bool,
        expected_res: Result<(), ErrorList>,
    ) {
        let res = validate_capabilities(&input, as_builtin);
        assert_eq!(res, expected_res);
    }

    fn check_test<F>(check_fn: F, input: &str, expected_res: Result<(), ErrorList>)
    where
        F: FnOnce(Option<&String>, &str, &str, &mut Vec<Error>) -> bool,
    {
        let mut errors = vec![];
        let res: Result<(), ErrorList> =
            match check_fn(Some(&input.to_string()), "FooDecl", "foo", &mut errors) {
                true => Ok(()),
                false => Err(ErrorList::new(errors)),
            };
        assert_eq!(format!("{:?}", res), format!("{:?}", expected_res));
    }

    fn new_component_decl() -> ComponentDecl {
        ComponentDecl {
            program: None,
            uses: None,
            exposes: None,
            offers: None,
            facets: None,
            capabilities: None,
            children: None,
            collections: None,
            environments: None,
            ..ComponentDecl::EMPTY
        }
    }

    #[test]
    fn test_errors() {
        assert_eq!(format!("{}", Error::missing_field("Decl", "keyword")), "Decl missing keyword");
        assert_eq!(format!("{}", Error::empty_field("Decl", "keyword")), "Decl has empty keyword");
        assert_eq!(
            format!("{}", Error::duplicate_field("Decl", "keyword", "foo")),
            "\"foo\" is a duplicate Decl keyword"
        );
        assert_eq!(
            format!("{}", Error::invalid_field("Decl", "keyword")),
            "Decl has invalid keyword"
        );
        assert_eq!(
            format!("{}", Error::field_too_long("Decl", "keyword")),
            "Decl's keyword is too long"
        );
        assert_eq!(
            format!("{}", Error::invalid_child("Decl", "source", "child")),
            "\"child\" is referenced in Decl.source but it does not appear in children"
        );
        assert_eq!(
            format!("{}", Error::invalid_collection("Decl", "source", "child")),
            "\"child\" is referenced in Decl.source but it does not appear in collections"
        );
        assert_eq!(
            format!("{}", Error::invalid_storage("Decl", "source", "name")),
            "\"name\" is referenced in Decl.source but it does not appear in storage"
        );
    }

    macro_rules! test_validate {
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
                    validate_test($input, $result);
                }
            )+
        }
    }

    macro_rules! test_validate_any_result {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    results = $results:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    validate_test_any_result($input, $results);
                }
            )+
        }
    }

    macro_rules! test_validate_capabilities {
        (
            $(
                $test_name:ident => {
                    input = $input:expr,
                    as_builtin = $as_builtin:expr,
                    result = $result:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    validate_capabilities_test($input, $as_builtin, $result);
                }
            )+
        }
    }

    macro_rules! test_string_checks {
        (
            $(
                $test_name:ident => {
                    check_fn = $check_fn:expr,
                    input = $input:expr,
                    result = $result:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    check_test($check_fn, $input, $result);
                }
            )+
        }
    }

    macro_rules! test_dependency {
        (
            $(
                $test_name:ident => {
                    ty = $ty:expr,
                    offer_decl = $offer_decl:expr,
                },
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    let mut decl = new_component_decl();
                    let dependencies = vec![
                        ("a", "b"),
                        ("b", "a"),
                    ];
                    let offers = dependencies.into_iter().map(|(from,to)| {
                        let mut offer_decl = $offer_decl;
                        offer_decl.source = Some(Ref::Child(
                           ChildRef { name: from.to_string(), collection: None },
                        ));
                        offer_decl.target = Some(Ref::Child(
                           ChildRef { name: to.to_string(), collection: None },
                        ));
                        $ty(offer_decl)
                    }).collect();
                    let children = ["a", "b"].iter().map(|name| {
                        ChildDecl {
                            name: Some(name.to_string()),
                            url: Some(format!("fuchsia-pkg://fuchsia.com/pkg#meta/{}.cm", name)),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            environment: None,
                            ..ChildDecl::EMPTY
                        }
                    }).collect();
                    decl.offers = Some(offers);
                    decl.children = Some(children);
                    let result = Err(ErrorList::new(vec![
                        Error::dependency_cycle(
                            directed_graph::Error::CyclesDetected([vec!["child a", "child b", "child a"]].iter().cloned().collect()).format_cycle()),
                    ]));
                    validate_test(decl, result);
                }
            )+
        }
    }

    macro_rules! test_weak_dependency {
        (
            $(
                $test_name:ident => {
                    ty = $ty:expr,
                    offer_decl = $offer_decl:expr,
                },
            )+
        ) => {
            $(
                #[test_case(DependencyType::Weak)]
                #[test_case(DependencyType::WeakForMigration)]
                fn $test_name(weak_dep: DependencyType) {
                    let mut decl = new_component_decl();
                    let offers = vec![
                        {
                            let mut offer_decl = $offer_decl;
                            offer_decl.source = Some(Ref::Child(
                               ChildRef { name: "a".to_string(), collection: None },
                            ));
                            offer_decl.target = Some(Ref::Child(
                               ChildRef { name: "b".to_string(), collection: None },
                            ));
                            offer_decl.dependency_type = Some(DependencyType::Strong);
                            $ty(offer_decl)
                        },
                        {
                            let mut offer_decl = $offer_decl;
                            offer_decl.source = Some(Ref::Child(
                               ChildRef { name: "b".to_string(), collection: None },
                            ));
                            offer_decl.target = Some(Ref::Child(
                               ChildRef { name: "a".to_string(), collection: None },
                            ));
                            offer_decl.dependency_type = Some(weak_dep);
                            $ty(offer_decl)
                        },
                    ];
                    let children = ["a", "b"].iter().map(|name| {
                        ChildDecl {
                            name: Some(name.to_string()),
                            url: Some(format!("fuchsia-pkg://fuchsia.com/pkg#meta/{}.cm", name)),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            environment: None,
                            ..ChildDecl::EMPTY
                        }
                    }).collect();
                    decl.offers = Some(offers);
                    decl.children = Some(children);
                    let result = Ok(());
                    validate_test(decl, result);
                }
            )+
        }
    }

    test_string_checks! {
        // path
        test_identifier_path_valid => {
            check_fn = check_path,
            input = "/foo/bar",
            result = Ok(()),
        },
        test_identifier_path_invalid_empty => {
            check_fn = check_path,
            input = "",
            result = Err(ErrorList::new(vec![
                Error::empty_field("FooDecl", "foo"),
                Error::invalid_field("FooDecl", "foo"),
            ])),
        },
        test_identifier_path_invalid_root => {
            check_fn = check_path,
            input = "/",
            result = Err(ErrorList::new(vec![Error::invalid_field("FooDecl", "foo")])),
        },
        test_identifier_path_invalid_relative => {
            check_fn = check_path,
            input = "foo/bar",
            result = Err(ErrorList::new(vec![Error::invalid_field("FooDecl", "foo")])),
        },
        test_identifier_path_invalid_trailing => {
            check_fn = check_path,
            input = "/foo/bar/",
            result = Err(ErrorList::new(vec![Error::invalid_field("FooDecl", "foo")])),
        },
        test_identifier_path_too_long => {
            check_fn = check_path,
            input = &format!("/{}", "a".repeat(1024)),
            result = Err(ErrorList::new(vec![Error::field_too_long("FooDecl", "foo")])),
        },

        // name
        test_identifier_name_valid => {
            check_fn = check_name,
            input = "abcdefghijklmnopqrstuvwxyz0123456789_-.",
            result = Ok(()),
        },
        test_identifier_name_invalid => {
            check_fn = check_name,
            input = "^bad",
            result = Err(ErrorList::new(vec![Error::invalid_field("FooDecl", "foo")])),
        },
        test_identifier_name_too_long => {
            check_fn = check_name,
            input = &format!("{}", "a".repeat(101)),
            result = Err(ErrorList::new(vec![Error::field_too_long("FooDecl", "foo")])),
        },

        // url
        test_identifier_url_valid => {
            check_fn = check_url,
            input = "my+awesome-scheme.2://abc123!@#$%.com",
            result = Ok(()),
        },
        test_identifier_url_invalid => {
            check_fn = check_url,
            input = "fuchsia-pkg://",
            result = Err(ErrorList::new(vec![Error::invalid_field("FooDecl", "foo")])),
        },
        test_identifier_url_too_long => {
            check_fn = check_url,
            input = &format!("fuchsia-pkg://{}", "a".repeat(4083)),
            result = Err(ErrorList::new(vec![Error::field_too_long("FooDecl", "foo")])),
        },
    }

    test_validate_any_result! {
        test_validate_use_disallows_nested_dirs => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("abc".to_string()),
                        target_path: Some("/foo/bar".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..UseDirectoryDecl::EMPTY
                    }),
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("abc".to_string()),
                        target_path: Some("/foo/bar/baz".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..UseDirectoryDecl::EMPTY
                    }),
                ]);
                decl
            },
            results = vec![
                Err(ErrorList::new(vec![
                    Error::invalid_path_overlap(
                        "UseDirectoryDecl", "/foo/bar/baz", "UseDirectoryDecl", "/foo/bar"),
                ])),
                Err(ErrorList::new(vec![
                    Error::invalid_path_overlap(
                        "UseDirectoryDecl", "/foo/bar", "UseDirectoryDecl", "/foo/bar/baz"),
                ])),
            ],
        },
        test_validate_use_disallows_common_prefixes_protocol => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("abc".to_string()),
                        target_path: Some("/foo/bar".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..UseDirectoryDecl::EMPTY
                    }),
                    UseDecl::Protocol(UseProtocolDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("crow".to_string()),
                        target_path: Some("/foo/bar/fuchsia.2".to_string()),
                        ..UseProtocolDecl::EMPTY
                    }),
                ]);
                decl
            },
            results = vec![
                Err(ErrorList::new(vec![
                    Error::invalid_path_overlap(
                        "UseProtocolDecl", "/foo/bar/fuchsia.2", "UseDirectoryDecl", "/foo/bar"),
                ])),
                Err(ErrorList::new(vec![
                    Error::invalid_path_overlap(
                        "UseDirectoryDecl", "/foo/bar", "UseProtocolDecl", "/foo/bar/fuchsia.2"),
                ])),
            ],
        },
        test_validate_use_disallows_common_prefixes_service => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("abc".to_string()),
                        target_path: Some("/foo/bar".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..UseDirectoryDecl::EMPTY
                    }),
                    UseDecl::Service(UseServiceDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("space".to_string()),
                        target_path: Some("/foo/bar/baz/fuchsia.logger.Log".to_string()),
                        dependency_type: Some(fsys::DependencyType::Strong),
                        ..UseServiceDecl::EMPTY
                    }),
                ]);
                decl
            },
            results = vec![
                Err(ErrorList::new(vec![
                    Error::invalid_path_overlap(
                        "UseServiceDecl", "/foo/bar/baz/fuchsia.logger.Log", "UseDirectoryDecl", "/foo/bar"),
                ])),
                Err(ErrorList::new(vec![
                    Error::invalid_path_overlap(
                        "UseDirectoryDecl", "/foo/bar", "UseServiceDecl", "/foo/bar/baz/fuchsia.logger.Log"),
                ])),
            ],
        },
    }

    test_validate! {
        // uses
        test_validate_uses_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.program = Some(ProgramDecl {
                    runner: Some("elf".to_string()),
                    info: Some(fdata::Dictionary {
                        entries: None,
                        ..fdata::Dictionary::EMPTY
                    }),
                    ..ProgramDecl::EMPTY
                });
                decl.uses = Some(vec![
                    UseDecl::Service(UseServiceDecl {
                        source: None,
                        source_name: None,
                        target_path: None,
                        dependency_type: Some(fsys::DependencyType::Strong),
                        ..UseServiceDecl::EMPTY
                    }),
                    UseDecl::Protocol(UseProtocolDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: None,
                        source_name: None,
                        target_path: None,
                        ..UseProtocolDecl::EMPTY
                    }),
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: None,
                        source_name: None,
                        target_path: None,
                        rights: None,
                        subdir: None,
                        ..UseDirectoryDecl::EMPTY
                    }),
                    UseDecl::Storage(UseStorageDecl {
                        source_name: None,
                        target_path: None,
                        ..UseStorageDecl::EMPTY
                    }),
                    UseDecl::Storage(UseStorageDecl {
                        source_name: Some("cache".to_string()),
                        target_path: None,
                        ..UseStorageDecl::EMPTY
                    }),
                    UseDecl::Event(UseEventDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: None,
                        source_name: None,
                        target_name: None,
                        filter: None,
                        mode: None,
                        ..UseEventDecl::EMPTY
                    }),
                    UseDecl::EventStream(UseEventStreamDecl {
                        name: None,
                        subscriptions: None,
                        ..UseEventStreamDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("UseEventDecl", "source"),
                Error::missing_field("UseEventDecl", "source_name"),
                Error::missing_field("UseEventDecl", "target_name"),
                Error::missing_field("UseEventDecl", "mode"),
                Error::missing_field("UseServiceDecl", "source"),
                Error::missing_field("UseServiceDecl", "source_name"),
                Error::missing_field("UseServiceDecl", "target_path"),
                Error::missing_field("UseProtocolDecl", "source"),
                Error::missing_field("UseProtocolDecl", "source_name"),
                Error::missing_field("UseProtocolDecl", "target_path"),
                Error::missing_field("UseDirectoryDecl", "source"),
                Error::missing_field("UseDirectoryDecl", "source_name"),
                Error::missing_field("UseDirectoryDecl", "target_path"),
                Error::missing_field("UseDirectoryDecl", "rights"),
                Error::missing_field("UseStorageDecl", "source_name"),
                Error::missing_field("UseStorageDecl", "target_path"),
                Error::missing_field("UseStorageDecl", "target_path"),
                Error::missing_field("UseEventStreamDecl", "name"),
                Error::missing_field("UseEventStreamDecl", "subscriptions"),
            ])),
        },
        test_validate_uses_invalid_identifiers_service => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Service(UseServiceDecl {
                        source: Some(fsys::Ref::Self_(fsys::SelfRef {})),
                        source_name: Some("foo/".to_string()),
                        target_path: Some("/".to_string()),
                        dependency_type: Some(fsys::DependencyType::Strong),
                        ..UseServiceDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("UseServiceDecl", "source_name"),
                Error::invalid_field("UseServiceDecl", "target_path"),
            ])),
        },
        test_validate_uses_invalid_identifiers_protocol => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Protocol(UseProtocolDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Self_(fsys::SelfRef {})),
                        source_name: Some("foo/".to_string()),
                        target_path: Some("/".to_string()),
                        ..UseProtocolDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("UseProtocolDecl", "source_name"),
                Error::invalid_field("UseProtocolDecl", "target_path"),
            ])),
        },
        test_validate_uses_invalid_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Self_(fsys::SelfRef {})),
                        source_name: Some("foo/".to_string()),
                        target_path: Some("/".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: Some("/foo".to_string()),
                        ..UseDirectoryDecl::EMPTY
                    }),
                    UseDecl::Storage(UseStorageDecl {
                        source_name: Some("/cache".to_string()),
                        target_path: Some("/".to_string()),
                        ..UseStorageDecl::EMPTY
                    }),
                    UseDecl::Storage(UseStorageDecl {
                        source_name: Some("temp".to_string()),
                        target_path: Some("tmp".to_string()),
                        ..UseStorageDecl::EMPTY
                    }),
                    UseDecl::Event(UseEventDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Self_(fsys::SelfRef {})),
                        source_name: Some("/foo".to_string()),
                        target_name: Some("/foo".to_string()),
                        filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                        mode: Some(EventMode::Async),
                        ..UseEventDecl::EMPTY
                    }),
                    UseDecl::Event(UseEventDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Framework(fsys::FrameworkRef {})),
                        source_name: Some("started".to_string()),
                        target_name: Some("started".to_string()),
                        filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                        mode: Some(EventMode::Async),
                        ..UseEventDecl::EMPTY
                    }),
                    UseDecl::EventStream(UseEventStreamDecl {
                        name: Some("bar".to_string()),
                        subscriptions: Some(vec!["a".to_string(), "b".to_string()].into_iter().map(|name| fsys::EventSubscription {
                            event_name: Some(name),
                            mode: Some(fsys::EventMode::Async),
                            ..fsys::EventSubscription::EMPTY
                        }).collect()),
                        ..UseEventStreamDecl::EMPTY
                    }),
                    UseDecl::EventStream(UseEventStreamDecl {
                        name: Some("bleep".to_string()),
                        subscriptions: Some(vec![fsys::EventSubscription {
                            event_name: Some("started".to_string()),
                            mode: Some(fsys::EventMode::Sync),
                            ..fsys::EventSubscription::EMPTY
                        }]),
                        ..UseEventStreamDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("UseEventDecl", "source"),
                Error::invalid_field("UseEventDecl", "source_name"),
                Error::invalid_field("UseEventDecl", "target_name"),
                Error::invalid_field("UseDirectoryDecl", "source_name"),
                Error::invalid_field("UseDirectoryDecl", "target_path"),
                Error::invalid_field("UseDirectoryDecl", "subdir"),
                Error::invalid_field("UseStorageDecl", "source_name"),
                Error::invalid_field("UseStorageDecl", "target_path"),
                Error::invalid_field("UseStorageDecl", "target_path"),
                Error::event_stream_event_not_found("UseEventStreamDecl", "events", "a".to_string()),
                Error::event_stream_event_not_found("UseEventStreamDecl", "events", "b".to_string()),
                Error::event_stream_unsupported_mode("UseEventStreamDecl", "events", "started".to_string(), "Sync".to_string()),
            ])),
        },
        test_validate_uses_missing_source => {
            input = {
                ComponentDecl {
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Capability(fsys::CapabilityRef {
                                name: "this-storage-doesnt-exist".to_string(),
                            })),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target_path: Some("/svc/fuchsia.sys2.StorageAdmin".to_string()),
                            ..UseProtocolDecl::EMPTY
                        })
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_capability("UseProtocolDecl", "source", "this-storage-doesnt-exist"),
            ])),
        },
        test_validate_uses_invalid_child => {
            input = {
                ComponentDecl {
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "no-such-child".to_string(), collection: None})),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target_path: Some("/svc/fuchsia.sys2.StorageAdmin".to_string()),
                            ..UseProtocolDecl::EMPTY
                        }),
                        UseDecl::Service(UseServiceDecl {
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "no-such-child".to_string(), collection: None})),
                            source_name: Some("service_name".to_string()),
                            target_path: Some("/svc/service_name".to_string()),
                            dependency_type: Some(fsys::DependencyType::Strong),
                            ..UseServiceDecl::EMPTY
                        }),
                        UseDecl::Directory(UseDirectoryDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "no-such-child".to_string(), collection: None})),
                            source_name: Some("DirectoryName".to_string()),
                            target_path: Some("/data/DirectoryName".to_string()),
                            rights: Some(fio2::Operations::Connect),
                            subdir: None,
                            ..UseDirectoryDecl::EMPTY
                        }),
                        UseDecl::Event(UseEventDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "no-such-child".to_string(), collection: None})),
                            source_name: Some("abc".to_string()),
                            target_name: Some("abc".to_string()),
                            filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                            mode: Some(EventMode::Async),
                            ..UseEventDecl::EMPTY
                        }),
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_child("UseEventDecl", "source", "no-such-child"),
                Error::invalid_child("UseProtocolDecl", "source", "no-such-child"),
                Error::invalid_child("UseServiceDecl", "source", "no-such-child"),
                Error::invalid_child("UseDirectoryDecl", "source", "no-such-child"),
            ])),
        },
        test_validate_use_from_child_offer_to_child_strong_cycle => {
            input = {
                ComponentDecl {
                    capabilities: Some(vec![
                        CapabilityDecl::Service(ServiceDecl {
                            name: Some("a".to_string()),
                            source_path: Some("/a".to_string()),
                            ..ServiceDecl::EMPTY
                        })]),
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target_path: Some("/svc/fuchsia.sys2.StorageAdmin".to_string()),
                            ..UseProtocolDecl::EMPTY
                        }),
                        UseDecl::Service(UseServiceDecl {
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("service_name".to_string()),
                            target_path: Some("/svc/service_name".to_string()),
                            dependency_type: Some(fsys::DependencyType::Strong),
                            ..UseServiceDecl::EMPTY
                        }),
                        UseDecl::Directory(UseDirectoryDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("DirectoryName".to_string()),
                            target_path: Some("/data/DirectoryName".to_string()),
                            rights: Some(fio2::Operations::Connect),
                            subdir: None,
                            ..UseDirectoryDecl::EMPTY
                        }),
                        UseDecl::Event(UseEventDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("abc".to_string()),
                            target_name: Some("abc".to_string()),
                            filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                            mode: Some(EventMode::Async),
                            ..UseEventDecl::EMPTY
                        })
                    ]),
                    offers: Some(vec![
                        OfferDecl::Service(OfferServiceDecl {
                            source: Some(Ref::Self_(SelfRef{})),
                            source_name: Some("a".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                            target_name: Some("a".to_string()),
                            ..OfferServiceDecl::EMPTY
                        })
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{self -> child child -> self}}".to_string()),
            ])),
        },
        test_validate_use_from_child_storage_no_cycle => {
            input = {
                ComponentDecl {
                    capabilities: Some(vec![
                        CapabilityDecl::Storage(StorageDecl {
                            name: Some("data".to_string()),
                            source: Some(fsys::Ref::Child(fsys::ChildRef { name: "child2".to_string(), collection: None } )),
                            backing_dir: Some("minfs".to_string()),
                            storage_id: Some(StorageId::StaticInstanceIdOrMoniker),
                            ..StorageDecl::EMPTY
                        })
                    ]),
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child1".to_string(), collection: None})),
                            source_name: Some("a".to_string()),
                            target_path: Some("/svc/a".to_string()),
                            ..UseProtocolDecl::EMPTY
                        }),
                    ]),
                    offers: Some(vec![
                        OfferDecl::Storage(OfferStorageDecl {
                            source: Some(Ref::Self_(SelfRef{})),
                            source_name: Some("data".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                            target_name: Some("data".to_string()),
                            ..OfferStorageDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child1".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        },
                        ChildDecl {
                            name: Some("child2".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo2".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Ok(()),
        },
        test_validate_storage_strong_cycle_between_children => {
            input = {
                ComponentDecl {
                    capabilities: Some(vec![
                        CapabilityDecl::Storage(StorageDecl {
                            name: Some("data".to_string()),
                            source: Some(fsys::Ref::Child(fsys::ChildRef { name: "child1".to_string(), collection: None } )),
                            backing_dir: Some("minfs".to_string()),
                            storage_id: Some(StorageId::StaticInstanceIdOrMoniker),
                            ..StorageDecl::EMPTY
                        })
                    ]),
                    offers: Some(vec![
                        OfferDecl::Storage(OfferStorageDecl {
                            source: Some(Ref::Self_(SelfRef{})),
                            source_name: Some("data".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child2".to_string(), collection: None })),
                            target_name: Some("data".to_string()),
                            ..OfferStorageDecl::EMPTY
                        }),
                        OfferDecl::Service(OfferServiceDecl {
                            source: Some(Ref::Child(ChildRef { name: "child2".to_string(), collection: None })),
                            source_name: Some("a".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                            target_name: Some("a".to_string()),
                            ..OfferServiceDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child1".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        },
                        ChildDecl {
                            name: Some("child2".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo2".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{child child1 -> child child2 -> child child1}}".to_string()),
            ])),
        },
        test_validate_strong_cycle_between_children_through_environment_debug => {
            input = {
                ComponentDecl {
                    environments: Some(vec![
                        EnvironmentDecl {
                            name: Some("env".to_string()),
                            extends: Some(EnvironmentExtends::Realm),
                            debug_capabilities: Some(vec![
                                DebugRegistration::Protocol(DebugProtocolRegistration {
                                    source: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                                    source_name: Some("fuchsia.foo.Bar".to_string()),
                                    target_name: Some("fuchsia.foo.Bar".to_string()),
                                    ..DebugProtocolRegistration::EMPTY
                                }),
                            ]),
                            ..EnvironmentDecl::EMPTY
                        },
                    ]),
                    offers: Some(vec![
                        OfferDecl::Service(OfferServiceDecl {
                            source: Some(Ref::Child(ChildRef { name: "child2".to_string(), collection: None })),
                            source_name: Some("a".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                            target_name: Some("a".to_string()),
                            ..OfferServiceDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child1".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        },
                        ChildDecl {
                            name: Some("child2".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo2".to_string()),
                            startup: Some(StartupMode::Lazy),
                            environment: Some("env".to_string()),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{child child1 -> environment env -> child child2 -> child child1}}".to_string()),
            ])),
        },
        test_validate_strong_cycle_between_children_through_environment_runner => {
            input = {
                ComponentDecl {
                    environments: Some(vec![
                        EnvironmentDecl {
                            name: Some("env".to_string()),
                            extends: Some(EnvironmentExtends::Realm),
                            runners: Some(vec![
                                RunnerRegistration {
                                    source: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                                    source_name: Some("coff".to_string()),
                                    target_name: Some("coff".to_string()),
                                    ..RunnerRegistration::EMPTY
                                }
                            ]),
                            ..EnvironmentDecl::EMPTY
                        },
                    ]),
                    offers: Some(vec![
                        OfferDecl::Service(OfferServiceDecl {
                            source: Some(Ref::Child(ChildRef { name: "child2".to_string(), collection: None })),
                            source_name: Some("a".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                            target_name: Some("a".to_string()),
                            ..OfferServiceDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child1".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        },
                        ChildDecl {
                            name: Some("child2".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo2".to_string()),
                            startup: Some(StartupMode::Lazy),
                            environment: Some("env".to_string()),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{child child1 -> environment env -> child child2 -> child child1}}".to_string()),
            ])),
        },
        test_validate_strong_cycle_between_children_through_environment_resolver => {
            input = {
                ComponentDecl {
                    environments: Some(vec![
                        EnvironmentDecl {
                            name: Some("env".to_string()),
                            extends: Some(EnvironmentExtends::Realm),
                            resolvers: Some(vec![
                                ResolverRegistration {
                                    resolver: Some("gopher".to_string()),
                                    source: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                                    scheme: Some("gopher".to_string()),
                                    ..ResolverRegistration::EMPTY
                                }
                            ]),
                            ..EnvironmentDecl::EMPTY
                        },
                    ]),
                    offers: Some(vec![
                        OfferDecl::Service(OfferServiceDecl {
                            source: Some(Ref::Child(ChildRef { name: "child2".to_string(), collection: None })),
                            source_name: Some("a".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                            target_name: Some("a".to_string()),
                            ..OfferServiceDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child1".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        },
                        ChildDecl {
                            name: Some("child2".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo2".to_string()),
                            startup: Some(StartupMode::Lazy),
                            environment: Some("env".to_string()),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{child child1 -> environment env -> child child2 -> child child1}}".to_string()),
            ])),
        },
        test_validate_strong_cycle_between_self_and_two_children => {
            input = {
                ComponentDecl {
                    capabilities: Some(vec![
                        CapabilityDecl::Protocol(ProtocolDecl {
                            name: Some("fuchsia.foo.Bar".to_string()),
                            source_path: Some("/svc/fuchsia.foo.Bar".to_string()),
                            ..ProtocolDecl::EMPTY
                        })
                    ]),
                    offers: Some(vec![
                        OfferDecl::Protocol(OfferProtocolDecl {
                            source: Some(Ref::Self_(SelfRef{})),
                            source_name: Some("fuchsia.foo.Bar".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                            target_name: Some("fuchsia.foo.Bar".to_string()),
                            dependency_type: Some(fsys::DependencyType::Strong),
                            ..OfferProtocolDecl::EMPTY
                        }),
                        OfferDecl::Protocol(OfferProtocolDecl {
                            source: Some(Ref::Child(ChildRef { name: "child1".to_string(), collection: None })),
                            source_name: Some("fuchsia.bar.Baz".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child2".to_string(), collection: None })),
                            target_name: Some("fuchsia.bar.Baz".to_string()),
                            dependency_type: Some(fsys::DependencyType::Strong),
                            ..OfferProtocolDecl::EMPTY
                        }),
                    ]),
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child2".to_string(), collection: None})),
                            source_name: Some("fuchsia.baz.Foo".to_string()),
                            target_path: Some("/svc/fuchsia.baz.Foo".to_string()),
                            dependency_type: Some(DependencyType::Strong),
                            ..UseProtocolDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child1".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        },
                        ChildDecl {
                            name: Some("child2".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo2".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{self -> child child1 -> child child2 -> self}}".to_string()),
            ])),
        },
        test_validate_strong_cycle_with_self_storage => {
            input = {
                ComponentDecl {
                    capabilities: Some(vec![
                        CapabilityDecl::Storage(StorageDecl {
                            name: Some("data".to_string()),
                            source: Some(fsys::Ref::Self_(fsys::SelfRef{})),
                            backing_dir: Some("minfs".to_string()),
                            storage_id: Some(StorageId::StaticInstanceIdOrMoniker),
                            ..StorageDecl::EMPTY
                        }),
                        CapabilityDecl::Directory(DirectoryDecl {
                            name: Some("minfs".to_string()),
                            source_path: Some("/minfs".to_string()),
                            rights: Some(fio2::RW_STAR_DIR),
                            ..DirectoryDecl::EMPTY
                        }),
                    ]),
                    offers: Some(vec![
                        OfferDecl::Storage(OfferStorageDecl {
                            source: Some(Ref::Self_(SelfRef{})),
                            source_name: Some("data".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                            target_name: Some("data".to_string()),
                            ..OfferStorageDecl::EMPTY
                        }),
                    ]),
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("fuchsia.foo.Bar".to_string()),
                            target_path: Some("/svc/fuchsia.foo.Bar".to_string()),
                            ..UseProtocolDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            ..ChildDecl::EMPTY
                        },
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{self -> child child -> self}}".to_string()),
            ])),
        },
        test_validate_strong_cycle_with_self_storage_admin_protocol => {
            input = {
                ComponentDecl {
                    capabilities: Some(vec![
                        CapabilityDecl::Storage(StorageDecl {
                            name: Some("data".to_string()),
                            source: Some(fsys::Ref::Self_(fsys::SelfRef{})),
                            backing_dir: Some("minfs".to_string()),
                            storage_id: Some(StorageId::StaticInstanceIdOrMoniker),
                            ..StorageDecl::EMPTY
                        }),
                        CapabilityDecl::Directory(DirectoryDecl {
                            name: Some("minfs".to_string()),
                            source_path: Some("/minfs".to_string()),
                            rights: Some(fio2::RW_STAR_DIR),
                            ..DirectoryDecl::EMPTY
                        }),
                    ]),
                    offers: Some(vec![
                        OfferDecl::Protocol(OfferProtocolDecl {
                            source: Some(Ref::Capability(CapabilityRef { name: "data".to_string() })),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                            target_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            dependency_type: Some(fsys::DependencyType::Strong),
                            ..OfferProtocolDecl::EMPTY
                        }),
                    ]),
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Strong),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("fuchsia.foo.Bar".to_string()),
                            target_path: Some("/svc/fuchsia.foo.Bar".to_string()),
                            ..UseProtocolDecl::EMPTY
                        }),
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            ..ChildDecl::EMPTY
                        },
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{self -> child child -> self}}".to_string()),
            ])),
        },
        test_validate_use_from_child_offer_to_child_weak_cycle => {
            input = {
                ComponentDecl {
                    capabilities: Some(vec![
                        CapabilityDecl::Service(ServiceDecl {
                            name: Some("a".to_string()),
                            source_path: Some("/a".to_string()),
                            ..ServiceDecl::EMPTY
                        })]),
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Weak),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target_path: Some("/svc/fuchsia.sys2.StorageAdmin".to_string()),
                            ..UseProtocolDecl::EMPTY
                        }),
                        UseDecl::Service(UseServiceDecl {
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("service_name".to_string()),
                            target_path: Some("/svc/service_name".to_string()),
                            dependency_type: Some(fsys::DependencyType::Weak),
                            ..UseServiceDecl::EMPTY
                        }),
                        UseDecl::Directory(UseDirectoryDecl {
                            dependency_type: Some(DependencyType::WeakForMigration),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("DirectoryName".to_string()),
                            target_path: Some("/data/DirectoryName".to_string()),
                            rights: Some(fio2::Operations::Connect),
                            subdir: None,
                            ..UseDirectoryDecl::EMPTY
                        }),
                        UseDecl::Event(UseEventDecl {
                            dependency_type: Some(DependencyType::WeakForMigration),
                            source: Some(fsys::Ref::Child(fsys::ChildRef{ name: "child".to_string(), collection: None})),
                            source_name: Some("abc".to_string()),
                            target_name: Some("abc".to_string()),
                            filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                            mode: Some(EventMode::Async),
                            ..UseEventDecl::EMPTY
                        })
                    ]),
                    offers: Some(vec![
                        OfferDecl::Service(OfferServiceDecl {
                            source: Some(Ref::Self_(SelfRef{})),
                            source_name: Some("a".to_string()),
                            target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                            target_name: Some("a".to_string()),
                            ..OfferServiceDecl::EMPTY
                        })
                    ]),
                    children: Some(vec![
                        ChildDecl {
                            name: Some("child".to_string()),
                            url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                            startup: Some(StartupMode::Lazy),
                            on_terminate: None,
                            ..ChildDecl::EMPTY
                        }
                    ]),
                    ..new_component_decl()
                }
            },
            result = Ok(()),
        },
        test_validate_use_from_not_child_weak => {
            input = {
                ComponentDecl {
                    uses: Some(vec![
                        UseDecl::Protocol(UseProtocolDecl {
                            dependency_type: Some(DependencyType::Weak),
                            source: Some(fsys::Ref::Parent(ParentRef{})),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target_path: Some("/svc/fuchsia.sys2.StorageAdmin".to_string()),
                            ..UseProtocolDecl::EMPTY
                        }),
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("UseProtocolDecl", "dependency_type"),
            ])),
        },
        test_validate_has_events_in_event_stream => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::EventStream(UseEventStreamDecl {
                        name: Some("bar".to_string()),
                        subscriptions: None,
                        ..UseEventStreamDecl::EMPTY
                    }),
                    UseDecl::EventStream(UseEventStreamDecl {
                        name: Some("barbar".to_string()),
                        subscriptions: Some(vec![]),
                        ..UseEventStreamDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("UseEventStreamDecl", "subscriptions"),
                Error::empty_field("UseEventStreamDecl", "subscriptions"),
            ])),
        },
        test_validate_uses_no_runner => {
            input = {
                let mut decl = new_component_decl();
                decl.program = Some(ProgramDecl {
                    runner: None,
                    info: Some(fdata::Dictionary {
                        entries: None,
                        ..fdata::Dictionary::EMPTY
                    }),
                    ..ProgramDecl::EMPTY
                });
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("ProgramDecl", "runner"),
            ])),
        },
        test_validate_uses_long_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.program = Some(ProgramDecl {
                    runner: Some("elf".to_string()),
                    info: Some(fdata::Dictionary {
                        entries: None,
                        ..fdata::Dictionary::EMPTY
                    }),
                    ..ProgramDecl::EMPTY
                });
                decl.uses = Some(vec![
                    UseDecl::Service(UseServiceDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target_path: Some(format!("/s/{}", "b".repeat(1024))),
                        dependency_type: Some(fsys::DependencyType::Strong),
                        ..UseServiceDecl::EMPTY
                    }),
                    UseDecl::Protocol(UseProtocolDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target_path: Some(format!("/p/{}", "c".repeat(1024))),
                        ..UseProtocolDecl::EMPTY
                    }),
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target_path: Some(format!("/d/{}", "d".repeat(1024))),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..UseDirectoryDecl::EMPTY
                    }),
                    UseDecl::Storage(UseStorageDecl {
                        source_name: Some("cache".to_string()),
                        target_path: Some(format!("/{}", "e".repeat(1024))),
                        ..UseStorageDecl::EMPTY
                    }),
                    UseDecl::Event(UseEventDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target_name: Some(format!("{}", "a".repeat(101))),
                        filter: None,
                        mode: Some(EventMode::Sync),
                        ..UseEventDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("UseEventDecl", "source_name"),
                Error::field_too_long("UseEventDecl", "target_name"),
                Error::field_too_long("UseServiceDecl", "source_name"),
                Error::field_too_long("UseServiceDecl", "target_path"),
                Error::field_too_long("UseProtocolDecl", "source_name"),
                Error::field_too_long("UseProtocolDecl", "target_path"),
                Error::field_too_long("UseDirectoryDecl", "source_name"),
                Error::field_too_long("UseDirectoryDecl", "target_path"),
                Error::field_too_long("UseStorageDecl", "target_path"),
            ])),
        },
        test_validate_conflicting_paths => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Service(UseServiceDecl {
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("foo".to_string()),
                        target_path: Some("/bar".to_string()),
                        dependency_type: Some(fsys::DependencyType::Strong),
                        ..UseServiceDecl::EMPTY
                    }),
                    UseDecl::Protocol(UseProtocolDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("space".to_string()),
                        target_path: Some("/bar".to_string()),
                        ..UseProtocolDecl::EMPTY
                    }),
                    UseDecl::Directory(UseDirectoryDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Parent(fsys::ParentRef {})),
                        source_name: Some("crow".to_string()),
                        target_path: Some("/bar".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..UseDirectoryDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::duplicate_field("UseProtocolDecl", "path", "/bar"),
                Error::duplicate_field("UseDirectoryDecl", "path", "/bar"),
            ])),
        },
        test_validate_events_can_come_before_or_after_event_stream => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Event(UseEventDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Framework(fsys::FrameworkRef {})),
                        source_name: Some("started".to_string()),
                        target_name: Some("started".to_string()),
                        filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                        mode: Some(EventMode::Async),
                        ..UseEventDecl::EMPTY
                    }),
                    UseDecl::EventStream(UseEventStreamDecl {
                        name: Some("bar".to_string()),
                        subscriptions: Some(
                            vec!["started".to_string(), "stopped".to_string()]
                                .into_iter()
                                .map(|name| fsys::EventSubscription {
                                    event_name: Some(name),
                                    mode: Some(fsys::EventMode::Async),
                                    ..fsys::EventSubscription::EMPTY
                                })
                                .collect()
                            ),
                        ..UseEventStreamDecl::EMPTY
                    }),
                    UseDecl::Event(UseEventDecl {
                        dependency_type: Some(DependencyType::Strong),
                        source: Some(fsys::Ref::Framework(fsys::FrameworkRef {})),
                        source_name: Some("stopped".to_string()),
                        target_name: Some("stopped".to_string()),
                        filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                        mode: Some(EventMode::Async),
                        ..UseEventDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Ok(()),
        },
        test_validate_uses_invalid_self_source => {
            input = {
                let mut decl = new_component_decl();
                decl.uses = Some(vec![
                    UseDecl::Event(UseEventDecl {
                        source: Some(fsys::Ref::Self_(fsys::SelfRef {})),
                        source_name: Some("started".to_string()),
                        target_name: Some("foo_started".to_string()),
                        mode: Some(EventMode::Async),
                        ..UseEventDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("UseEventDecl", "source"),
            ])),
        },
        // exposes
        test_validate_exposes_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: None,
                        source_name: None,
                        target_name: None,
                        target: None,
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: None,
                        source_name: None,
                        target_name: None,
                        target: None,
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: None,
                        source_name: None,
                        target_name: None,
                        target: None,
                        rights: None,
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: None,
                        source_name: None,
                        target: None,
                        target_name: None,
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: None,
                        source_name: None,
                        target: None,
                        target_name: None,
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("ExposeServiceDecl", "source"),
                Error::missing_field("ExposeServiceDecl", "target"),
                Error::missing_field("ExposeServiceDecl", "source_name"),
                Error::missing_field("ExposeServiceDecl", "target_name"),
                Error::missing_field("ExposeProtocolDecl", "source"),
                Error::missing_field("ExposeProtocolDecl", "target"),
                Error::missing_field("ExposeProtocolDecl", "source_name"),
                Error::missing_field("ExposeProtocolDecl", "target_name"),
                Error::missing_field("ExposeDirectoryDecl", "source"),
                Error::missing_field("ExposeDirectoryDecl", "target"),
                Error::missing_field("ExposeDirectoryDecl", "source_name"),
                Error::missing_field("ExposeDirectoryDecl", "target_name"),
                Error::missing_field("ExposeRunnerDecl", "source"),
                Error::missing_field("ExposeRunnerDecl", "target"),
                Error::missing_field("ExposeRunnerDecl", "source_name"),
                Error::missing_field("ExposeRunnerDecl", "target_name"),
                Error::missing_field("ExposeResolverDecl", "source"),
                Error::missing_field("ExposeResolverDecl", "target"),
                Error::missing_field("ExposeResolverDecl", "source_name"),
                Error::missing_field("ExposeResolverDecl", "target_name"),
            ])),
        },
        test_validate_exposes_extraneous => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("logger".to_string()),
                        target_name: Some("logger".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("legacy_logger".to_string()),
                        target_name: Some("legacy_logger".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("data".to_string()),
                        target_name: Some("data".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("elf".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("elf".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("pkg".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("pkg".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::extraneous_field("ExposeServiceDecl", "source.child.collection"),
                Error::extraneous_field("ExposeProtocolDecl", "source.child.collection"),
                Error::extraneous_field("ExposeDirectoryDecl", "source.child.collection"),
                Error::extraneous_field("ExposeRunnerDecl", "source.child.collection"),
                Error::extraneous_field("ExposeResolverDecl", "source.child.collection"),
            ])),
        },
        test_validate_exposes_invalid_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("foo/".to_string()),
                        target_name: Some("/".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("foo/".to_string()),
                        target_name: Some("/".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("foo/".to_string()),
                        target_name: Some("/".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: Some("/foo".to_string()),
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("/path".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("elf!".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("/path".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("pkg!".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("ExposeServiceDecl", "source.child.name"),
                Error::invalid_field("ExposeServiceDecl", "source_name"),
                Error::invalid_field("ExposeServiceDecl", "target_name"),
                Error::invalid_field("ExposeProtocolDecl", "source.child.name"),
                Error::invalid_field("ExposeProtocolDecl", "source_name"),
                Error::invalid_field("ExposeProtocolDecl", "target_name"),
                Error::invalid_field("ExposeDirectoryDecl", "source.child.name"),
                Error::invalid_field("ExposeDirectoryDecl", "source_name"),
                Error::invalid_field("ExposeDirectoryDecl", "target_name"),
                Error::invalid_field("ExposeDirectoryDecl", "subdir"),
                Error::invalid_field("ExposeRunnerDecl", "source.child.name"),
                Error::invalid_field("ExposeRunnerDecl", "source_name"),
                Error::invalid_field("ExposeRunnerDecl", "target_name"),
                Error::invalid_field("ExposeResolverDecl", "source.child.name"),
                Error::invalid_field("ExposeResolverDecl", "source_name"),
                Error::invalid_field("ExposeResolverDecl", "target_name"),
            ])),
        },
        test_validate_exposes_invalid_source_target => {
            input = {
                let mut decl = new_component_decl();
                decl.children = Some(vec![ChildDecl{
                    name: Some("logger".to_string()),
                    url: Some("fuchsia-pkg://fuchsia.com/logger#meta/logger.cm".to_string()),
                    startup: Some(StartupMode::Lazy),
                    on_terminate: None,
                    environment: None,
                    ..ChildDecl::EMPTY
                }]);
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: None,
                        source_name: Some("a".to_string()),
                        target_name: Some("b".to_string()),
                        target: None,
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("c".to_string()),
                        target_name: Some("d".to_string()),
                        target: Some(Ref::Self_(SelfRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Collection(CollectionRef {name: "z".to_string()})),
                        source_name: Some("e".to_string()),
                        target_name: Some("f".to_string()),
                        target: Some(Ref::Collection(CollectionRef {name: "z".to_string()})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("g".to_string()),
                        target_name: Some("h".to_string()),
                        target: Some(Ref::Framework(FrameworkRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("i".to_string()),
                        target: Some(Ref::Framework(FrameworkRef {})),
                        target_name: Some("j".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("k".to_string()),
                        target: Some(Ref::Framework(FrameworkRef {})),
                        target_name: Some("l".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("m".to_string()),
                        target_name: Some("n".to_string()),
                        target: Some(Ref::Framework(FrameworkRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("ExposeServiceDecl", "source"),
                Error::missing_field("ExposeServiceDecl", "target"),
                Error::invalid_field("ExposeProtocolDecl", "source"),
                Error::invalid_field("ExposeProtocolDecl", "target"),
                Error::invalid_field("ExposeDirectoryDecl", "source"),
                Error::invalid_field("ExposeDirectoryDecl", "target"),
                Error::invalid_field("ExposeDirectoryDecl", "source"),
                Error::invalid_field("ExposeDirectoryDecl", "target"),
                Error::invalid_field("ExposeRunnerDecl", "source"),
                Error::invalid_field("ExposeRunnerDecl", "target"),
                Error::invalid_field("ExposeResolverDecl", "source"),
                Error::invalid_field("ExposeResolverDecl", "target"),
                Error::invalid_field("ExposeDirectoryDecl", "target"),
            ])),
        },
        test_validate_exposes_invalid_source_collection => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![CollectionDecl{
                    name: Some("col".to_string()),
                    durability: Some(Durability::Transient),
                    allowed_offers: None,
                    ..CollectionDecl::EMPTY
                }]);
                decl.exposes = Some(vec![
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("a".to_string()),
                        target_name: Some("a".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Collection(CollectionRef {name: "col".to_string()})),
                        source_name: Some("b".to_string()),
                        target_name: Some("b".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Collection(CollectionRef {name: "col".to_string()})),
                        source_name: Some("c".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("c".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Collection(CollectionRef {name: "col".to_string()})),
                        source_name: Some("d".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("d".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("ExposeProtocolDecl", "source"),
                Error::invalid_field("ExposeDirectoryDecl", "source"),
                Error::invalid_field("ExposeRunnerDecl", "source"),
                Error::invalid_field("ExposeResolverDecl", "source"),
            ])),
        },
        test_validate_exposes_sources_collection => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![
                    CollectionDecl {
                        name: Some("col".to_string()),
                        durability: Some(Durability::Transient),
                        allowed_offers: Some(AllowedOffers::StaticOnly),
                        ..CollectionDecl::EMPTY
                    }
                ]);
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("a".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("a".to_string()),
                        ..ExposeServiceDecl::EMPTY
                    })
                ]);
                decl
            },
            result = Ok(()),
        },
        test_validate_exposes_long_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "b".repeat(101),
                            collection: None,
                        })),
                        source_name: Some(format!("{}", "a".repeat(1025))),
                        target_name: Some(format!("{}", "b".repeat(1025))),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "b".repeat(101),
                            collection: None,
                        })),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "b".repeat(101),
                            collection: None,
                        })),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        target: Some(Ref::Parent(ParentRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "b".repeat(101),
                            collection: None,
                        })),
                        source_name: Some("a".repeat(101)),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("b".repeat(101)),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "b".repeat(101),
                            collection: None,
                        })),
                        source_name: Some("a".repeat(101)),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("b".repeat(101)),
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("ExposeServiceDecl", "source.child.name"),
                Error::field_too_long("ExposeServiceDecl", "source_name"),
                Error::field_too_long("ExposeServiceDecl", "target_name"),
                Error::field_too_long("ExposeProtocolDecl", "source.child.name"),
                Error::field_too_long("ExposeProtocolDecl", "source_name"),
                Error::field_too_long("ExposeProtocolDecl", "target_name"),
                Error::field_too_long("ExposeDirectoryDecl", "source.child.name"),
                Error::field_too_long("ExposeDirectoryDecl", "source_name"),
                Error::field_too_long("ExposeDirectoryDecl", "target_name"),
                Error::field_too_long("ExposeRunnerDecl", "source.child.name"),
                Error::field_too_long("ExposeRunnerDecl", "source_name"),
                Error::field_too_long("ExposeRunnerDecl", "target_name"),
                Error::field_too_long("ExposeResolverDecl", "source.child.name"),
                Error::field_too_long("ExposeResolverDecl", "source_name"),
                Error::field_too_long("ExposeResolverDecl", "target_name"),
            ])),
        },
        test_validate_exposes_invalid_child => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("fuchsia.logger.Log".to_string()),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        target_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("data".to_string()),
                        target_name: Some("data".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("elf".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("elf".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        source_name: Some("pkg".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("pkg".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_child("ExposeServiceDecl", "source", "netstack"),
                Error::invalid_child("ExposeProtocolDecl", "source", "netstack"),
                Error::invalid_child("ExposeDirectoryDecl", "source", "netstack"),
                Error::invalid_child("ExposeRunnerDecl", "source", "netstack"),
                Error::invalid_child("ExposeResolverDecl", "source", "netstack"),
            ])),
        },
        test_validate_exposes_invalid_source_capability => {
            input = {
                ComponentDecl {
                    exposes: Some(vec![
                        ExposeDecl::Protocol(ExposeProtocolDecl {
                            source: Some(Ref::Capability(CapabilityRef {
                                name: "this-storage-doesnt-exist".to_string(),
                            })),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target: Some(Ref::Parent(ParentRef {})),
                            ..ExposeProtocolDecl::EMPTY
                        }),
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_capability("ExposeProtocolDecl", "source", "this-storage-doesnt-exist"),
            ])),
        },
        test_validate_exposes_duplicate_target => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("netstack".to_string()),
                        target_name: Some("fuchsia.net.Stack".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("netstack2".to_string()),
                        target_name: Some("fuchsia.net.Stack".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("fonts".to_string()),
                        target_name: Some("fuchsia.fonts.Provider".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("fonts2".to_string()),
                        target_name: Some("fuchsia.fonts.Provider".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("assets".to_string()),
                        target_name: Some("stuff".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        rights: None,
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("assets2".to_string()),
                        target_name: Some("stuff".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        rights: None,
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("source_elf".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("elf".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("source_elf".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("elf".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("source_pkg".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("pkg".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("source_pkg".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("pkg".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl.capabilities = Some(vec![
                    CapabilityDecl::Service(ServiceDecl {
                        name: Some("netstack".to_string()),
                        source_path: Some("/path".to_string()),
                        ..ServiceDecl::EMPTY
                    }),
                    CapabilityDecl::Service(ServiceDecl {
                        name: Some("netstack2".to_string()),
                        source_path: Some("/path".to_string()),
                        ..ServiceDecl::EMPTY
                    }),
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: Some("fonts".to_string()),
                        source_path: Some("/path".to_string()),
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: Some("fonts2".to_string()),
                        source_path: Some("/path".to_string()),
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: Some("assets".to_string()),
                        source_path: Some("/path".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        ..DirectoryDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: Some("assets2".to_string()),
                        source_path: Some("/path".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        ..DirectoryDecl::EMPTY
                    }),
                    CapabilityDecl::Runner(RunnerDecl {
                        name: Some("source_elf".to_string()),
                        source_path: Some("/path".to_string()),
                        ..RunnerDecl::EMPTY
                    }),
                    CapabilityDecl::Resolver(ResolverDecl {
                        name: Some("source_pkg".to_string()),
                        source_path: Some("/path".to_string()),
                        ..ResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                // Duplicate services are allowed.
                Error::duplicate_field("ExposeProtocolDecl", "target_name",
                                       "fuchsia.fonts.Provider"),
                Error::duplicate_field("ExposeDirectoryDecl", "target_name",
                                       "stuff"),
                Error::duplicate_field("ExposeRunnerDecl", "target_name",
                                       "elf"),
                Error::duplicate_field("ExposeResolverDecl", "target_name", "pkg"),
            ])),
        },
        // TODO: Add analogous test for offer
        test_validate_exposes_invalid_capability_from_self => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![
                    ExposeDecl::Service(ExposeServiceDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("fuchsia.netstack.Netstack".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("foo".to_string()),
                        ..ExposeServiceDecl::EMPTY
                    }),
                    ExposeDecl::Protocol(ExposeProtocolDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("fuchsia.netstack.Netstack".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("bar".to_string()),
                        ..ExposeProtocolDecl::EMPTY
                    }),
                    ExposeDecl::Directory(ExposeDirectoryDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("dir".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("assets".to_string()),
                        rights: None,
                        subdir: None,
                        ..ExposeDirectoryDecl::EMPTY
                    }),
                    ExposeDecl::Runner(ExposeRunnerDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("source_elf".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("elf".to_string()),
                        ..ExposeRunnerDecl::EMPTY
                    }),
                    ExposeDecl::Resolver(ExposeResolverDecl {
                        source: Some(Ref::Self_(SelfRef{})),
                        source_name: Some("source_pkg".to_string()),
                        target: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("pkg".to_string()),
                        ..ExposeResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_capability("ExposeServiceDecl", "source", "fuchsia.netstack.Netstack"),
                Error::invalid_capability("ExposeProtocolDecl", "source", "fuchsia.netstack.Netstack"),
                Error::invalid_capability("ExposeDirectoryDecl", "source", "dir"),
                Error::invalid_capability("ExposeRunnerDecl", "source", "source_elf"),
                Error::invalid_capability("ExposeResolverDecl", "source", "source_pkg"),
            ])),
        },

        // offers
        test_validate_offers_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: None,
                        source_name: None,
                        target: None,
                        target_name: None,
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: None,
                        source_name: None,
                        target: None,
                        target_name: None,
                        dependency_type: None,
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: None,
                        source_name: None,
                        target: None,
                        target_name: None,
                        rights: None,
                        subdir: None,
                        dependency_type: None,
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Storage(OfferStorageDecl {
                        source_name: None,
                        source: None,
                        target: None,
                        target_name: None,
                        ..OfferStorageDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: None,
                        source_name: None,
                        target: None,
                        target_name: None,
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source: None,
                        source_name: None,
                        target: None,
                        target_name: None,
                        filter: None,
                        mode: None,
                        ..OfferEventDecl::EMPTY
                    })
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("OfferServiceDecl", "source"),
                Error::missing_field("OfferServiceDecl", "source_name"),
                Error::missing_field("OfferServiceDecl", "target"),
                Error::missing_field("OfferServiceDecl", "target_name"),
                Error::missing_field("OfferProtocolDecl", "source"),
                Error::missing_field("OfferProtocolDecl", "source_name"),
                Error::missing_field("OfferProtocolDecl", "target"),
                Error::missing_field("OfferProtocolDecl", "target_name"),
                Error::missing_field("OfferProtocolDecl", "dependency_type"),
                Error::missing_field("OfferDirectoryDecl", "source"),
                Error::missing_field("OfferDirectoryDecl", "source_name"),
                Error::missing_field("OfferDirectoryDecl", "target"),
                Error::missing_field("OfferDirectoryDecl", "target_name"),
                Error::missing_field("OfferDirectoryDecl", "dependency_type"),
                Error::missing_field("OfferStorageDecl", "source_name"),
                Error::missing_field("OfferStorageDecl", "source"),
                Error::missing_field("OfferStorageDecl", "target"),
                Error::missing_field("OfferRunnerDecl", "source"),
                Error::missing_field("OfferRunnerDecl", "source_name"),
                Error::missing_field("OfferRunnerDecl", "target"),
                Error::missing_field("OfferRunnerDecl", "target_name"),
                Error::missing_field("OfferEventDecl", "source_name"),
                Error::missing_field("OfferEventDecl", "source"),
                Error::missing_field("OfferEventDecl", "target"),
                Error::missing_field("OfferEventDecl", "target_name"),
                Error::missing_field("OfferEventDecl", "mode"),
            ])),
        },
        test_validate_offers_long_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "a".repeat(101),
                            collection: None,
                        })),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "b".repeat(101),
                               collection: None,
                           }
                        )),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("a".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef {
                               name: "b".repeat(101),
                           }
                        )),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "a".repeat(101),
                            collection: None,
                        })),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "b".repeat(101),
                               collection: None,
                           }
                        )),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("a".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef {
                               name: "b".repeat(101),
                           }
                        )),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "a".repeat(101),
                            collection: None,
                        })),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "b".repeat(101),
                               collection: None,
                           }
                        )),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("a".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef {
                               name: "b".repeat(101),
                           }
                        )),
                        target_name: Some(format!("{}", "b".repeat(101))),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Storage(OfferStorageDecl {
                        source_name: Some("data".to_string()),
                        source: Some(Ref::Parent(ParentRef {})),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "b".repeat(101),
                                collection: None,
                            }
                        )),
                        target_name: Some("data".to_string()),
                        ..OfferStorageDecl::EMPTY
                    }),
                    OfferDecl::Storage(OfferStorageDecl {
                        source_name: Some("data".to_string()),
                        source: Some(Ref::Parent(ParentRef {})),
                        target: Some(Ref::Collection(
                            CollectionRef { name: "b".repeat(101) }
                        )),
                        target_name: Some("data".to_string()),
                        ..OfferStorageDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "a".repeat(101),
                            collection: None,
                        })),
                        source_name: Some("b".repeat(101)),
                        target: Some(Ref::Collection(
                           CollectionRef {
                               name: "c".repeat(101),
                           }
                        )),
                        target_name: Some("d".repeat(101)),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "a".repeat(101),
                            collection: None,
                        })),
                        source_name: Some("b".repeat(101)),
                        target: Some(Ref::Collection(
                            CollectionRef {
                                name: "c".repeat(101),
                            }
                        )),
                        target_name: Some("d".repeat(101)),
                        ..OfferResolverDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some(format!("{}", "a".repeat(101))),
                        target: Some(Ref::Child(ChildRef {
                            name: "a".repeat(101),
                            collection: None
                        })),
                        target_name: Some(format!("{}", "a".repeat(101))),
                        filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                        mode: Some(EventMode::Async),
                        ..OfferEventDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("OfferServiceDecl", "source.child.name"),
                Error::field_too_long("OfferServiceDecl", "source_name"),
                Error::field_too_long("OfferServiceDecl", "target.child.name"),
                Error::field_too_long("OfferServiceDecl", "target_name"),
                Error::field_too_long("OfferServiceDecl", "target.collection.name"),
                Error::field_too_long("OfferServiceDecl", "target_name"),
                Error::field_too_long("OfferProtocolDecl", "source.child.name"),
                Error::field_too_long("OfferProtocolDecl", "source_name"),
                Error::field_too_long("OfferProtocolDecl", "target.child.name"),
                Error::field_too_long("OfferProtocolDecl", "target_name"),
                Error::field_too_long("OfferProtocolDecl", "target.collection.name"),
                Error::field_too_long("OfferProtocolDecl", "target_name"),
                Error::field_too_long("OfferDirectoryDecl", "source.child.name"),
                Error::field_too_long("OfferDirectoryDecl", "source_name"),
                Error::field_too_long("OfferDirectoryDecl", "target.child.name"),
                Error::field_too_long("OfferDirectoryDecl", "target_name"),
                Error::field_too_long("OfferDirectoryDecl", "target.collection.name"),
                Error::field_too_long("OfferDirectoryDecl", "target_name"),
                Error::field_too_long("OfferStorageDecl", "target.child.name"),
                Error::field_too_long("OfferStorageDecl", "target.collection.name"),
                Error::field_too_long("OfferRunnerDecl", "source.child.name"),
                Error::field_too_long("OfferRunnerDecl", "source_name"),
                Error::field_too_long("OfferRunnerDecl", "target.collection.name"),
                Error::field_too_long("OfferRunnerDecl", "target_name"),
                Error::field_too_long("OfferResolverDecl", "source.child.name"),
                Error::field_too_long("OfferResolverDecl", "source_name"),
                Error::field_too_long("OfferResolverDecl", "target.collection.name"),
                Error::field_too_long("OfferResolverDecl", "target_name"),
                Error::field_too_long("OfferEventDecl", "source_name"),
                Error::field_too_long("OfferEventDecl", "target.child.name"),
                Error::field_too_long("OfferEventDecl", "target_name"),
            ])),
        },
        test_validate_offers_extraneous => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("fuchsia.logger.Log".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: Some("modular".to_string()),
                            }
                        )),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("fuchsia.logger.Log".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: Some("modular".to_string()),
                            }
                        )),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("assets".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: Some("modular".to_string()),
                            }
                        )),
                        target_name: Some("assets".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Storage(OfferStorageDecl {
                        source_name: Some("data".to_string()),
                        source: Some(Ref::Parent(ParentRef{ })),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: Some("modular".to_string()),
                            }
                        )),
                        target_name: Some("data".to_string()),
                        ..OfferStorageDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("elf".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: Some("modular".to_string()),
                            }
                        )),
                        target_name: Some("elf".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: Some("modular".to_string()),
                        })),
                        source_name: Some("pkg".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: Some("modular".to_string()),
                            }
                        )),
                        target_name: Some("pkg".to_string()),
                        ..OfferResolverDecl::EMPTY
                    }),
                ]);
                decl.capabilities = Some(vec![
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: Some("fuchsia.logger.Log".to_string()),
                        source_path: Some("/svc/logger".to_string()),
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: Some("assets".to_string()),
                        source_path: Some("/data/assets".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        ..DirectoryDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::extraneous_field("OfferServiceDecl", "source.child.collection"),
                Error::extraneous_field("OfferServiceDecl", "target.child.collection"),
                Error::extraneous_field("OfferProtocolDecl", "source.child.collection"),
                Error::extraneous_field("OfferProtocolDecl", "target.child.collection"),
                Error::extraneous_field("OfferDirectoryDecl", "source.child.collection"),
                Error::extraneous_field("OfferDirectoryDecl", "target.child.collection"),
                Error::extraneous_field("OfferStorageDecl", "target.child.collection"),
                Error::extraneous_field("OfferRunnerDecl", "source.child.collection"),
                Error::extraneous_field("OfferRunnerDecl", "target.child.collection"),
                Error::extraneous_field("OfferResolverDecl", "source.child.collection"),
                Error::extraneous_field("OfferResolverDecl", "target.child.collection"),
            ])),
        },
        test_validate_offers_invalid_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("foo/".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "%bad".to_string(),
                            collection: None,
                        })),
                        target_name: Some("/".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("foo/".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "%bad".to_string(),
                            collection: None,
                        })),
                        target_name: Some("/".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("foo/".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "%bad".to_string(),
                            collection: None,
                        })),
                        target_name: Some("/".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: Some("/foo".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("/path".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "%bad".to_string(),
                            collection: None,
                        })),
                        target_name: Some("elf!".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "^bad".to_string(),
                            collection: None,
                        })),
                        source_name: Some("/path".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "%bad".to_string(),
                            collection: None,
                        })),
                        target_name: Some("pkg!".to_string()),
                        ..OfferResolverDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("/path".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "%bad".to_string(),
                            collection: None,
                        })),
                        target_name: Some("/path".to_string()),
                        filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                        mode: Some(fsys::EventMode::Sync),
                        ..OfferEventDecl::EMPTY
                    })
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("OfferServiceDecl", "source.child.name"),
                Error::invalid_field("OfferServiceDecl", "source_name"),
                Error::invalid_field("OfferServiceDecl", "target.child.name"),
                Error::invalid_field("OfferServiceDecl", "target_name"),
                Error::invalid_field("OfferProtocolDecl", "source.child.name"),
                Error::invalid_field("OfferProtocolDecl", "source_name"),
                Error::invalid_field("OfferProtocolDecl", "target.child.name"),
                Error::invalid_field("OfferProtocolDecl", "target_name"),
                Error::invalid_field("OfferDirectoryDecl", "source.child.name"),
                Error::invalid_field("OfferDirectoryDecl", "source_name"),
                Error::invalid_field("OfferDirectoryDecl", "target.child.name"),
                Error::invalid_field("OfferDirectoryDecl", "target_name"),
                Error::invalid_field("OfferDirectoryDecl", "subdir"),
                Error::invalid_field("OfferRunnerDecl", "source.child.name"),
                Error::invalid_field("OfferRunnerDecl", "source_name"),
                Error::invalid_field("OfferRunnerDecl", "target.child.name"),
                Error::invalid_field("OfferRunnerDecl", "target_name"),
                Error::invalid_field("OfferResolverDecl", "source.child.name"),
                Error::invalid_field("OfferResolverDecl", "source_name"),
                Error::invalid_field("OfferResolverDecl", "target.child.name"),
                Error::invalid_field("OfferResolverDecl", "target_name"),
                Error::invalid_field("OfferEventDecl", "source_name"),
                Error::invalid_field("OfferEventDecl", "target.child.name"),
                Error::invalid_field("OfferEventDecl", "target_name"),
            ])),
        },
        test_validate_offers_target_equals_source => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("logger".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "logger".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("logger".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("legacy_logger".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "logger".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("legacy_logger".to_string()),
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("assets".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "logger".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("assets".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("web".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "logger".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("web".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("pkg".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "logger".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("pkg".to_string()),
                        ..OfferResolverDecl::EMPTY
                    }),
                ]);
                decl.children = Some(vec![ChildDecl{
                    name: Some("logger".to_string()),
                    url: Some("fuchsia-pkg://fuchsia.com/logger#meta/logger.cm".to_string()),
                    startup: Some(StartupMode::Lazy),
                    on_terminate: None,
                    environment: None,
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::offer_target_equals_source("OfferServiceDecl", "logger"),
                Error::offer_target_equals_source("OfferProtocolDecl", "logger"),
                Error::offer_target_equals_source("OfferDirectoryDecl", "logger"),
                Error::offer_target_equals_source("OfferRunnerDecl", "logger"),
                Error::offer_target_equals_source("OfferResolverDecl", "logger"),
            ])),
        },
        test_validate_offers_storage_target_equals_source => {
            input = ComponentDecl {
                offers: Some(vec![
                    OfferDecl::Storage(OfferStorageDecl {
                        source_name: Some("data".to_string()),
                        source: Some(Ref::Self_(SelfRef { })),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "logger".to_string(),
                                collection: None,
                            }
                        )),
                        target_name: Some("data".to_string()),
                        ..OfferStorageDecl::EMPTY
                    })
                ]),
                capabilities: Some(vec![
                    CapabilityDecl::Storage(StorageDecl {
                        name: Some("data".to_string()),
                        backing_dir: Some("minfs".to_string()),
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        subdir: None,
                        storage_id: Some(fsys::StorageId::StaticInstanceIdOrMoniker),
                        ..StorageDecl::EMPTY
                    }),
                ]),
                children: Some(vec![
                    ChildDecl {
                        name: Some("logger".to_string()),
                        url: Some("fuchsia-pkg://fuchsia.com/logger/stable#meta/logger.cm".to_string()),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        environment: None,
                        ..ChildDecl::EMPTY
                    },
                ]),
                ..new_component_decl()
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle("{{child logger -> child logger}}".to_string()),
            ])),
        },
        test_validate_offers_invalid_child => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("fuchsia.logger.Log".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        source_name: Some("assets".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("assets".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                ]);
                decl.capabilities = Some(vec![
                    CapabilityDecl::Storage(StorageDecl {
                        name: Some("memfs".to_string()),
                        backing_dir: Some("memfs".to_string()),
                        source: Some(Ref::Child(ChildRef {
                            name: "logger".to_string(),
                            collection: None,
                        })),
                        subdir: None,
                        storage_id: Some(fsys::StorageId::StaticInstanceIdOrMoniker),
                        ..StorageDecl::EMPTY
                    }),
                ]);
                decl.children = Some(vec![
                    ChildDecl {
                        name: Some("netstack".to_string()),
                        url: Some("fuchsia-pkg://fuchsia.com/netstack/stable#meta/netstack.cm".to_string()),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        environment: None,
                        ..ChildDecl::EMPTY
                    },
                ]);
                decl.collections = Some(vec![
                    CollectionDecl {
                        name: Some("modular".to_string()),
                        durability: Some(Durability::Persistent),
                        allowed_offers: Some(AllowedOffers::StaticAndDynamic),
                        environment: None,
                        ..CollectionDecl::EMPTY
                    },
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_child("StorageDecl", "source", "logger"),
                Error::invalid_child("OfferServiceDecl", "source", "logger"),
                Error::invalid_child("OfferProtocolDecl", "source", "logger"),
                Error::invalid_child("OfferDirectoryDecl", "source", "logger"),
            ])),
        },
        test_validate_offers_invalid_source_capability => {
            input = {
                ComponentDecl {
                    offers: Some(vec![
                        OfferDecl::Protocol(OfferProtocolDecl {
                            source: Some(Ref::Capability(CapabilityRef {
                                name: "this-storage-doesnt-exist".to_string(),
                            })),
                            source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            target: Some(Ref::Child(
                               ChildRef {
                                   name: "netstack".to_string(),
                                   collection: None,
                               }
                            )),
                            target_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                            dependency_type: Some(DependencyType::Strong),
                            ..OfferProtocolDecl::EMPTY
                        }),
                    ]),
                    ..new_component_decl()
                }
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_capability("OfferProtocolDecl", "source", "this-storage-doesnt-exist"),
                Error::invalid_child("OfferProtocolDecl", "target", "netstack"),
            ])),
        },
        test_validate_offers_target => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("logger".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("logger2".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("assets".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("assets".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("assets".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("assets".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("elf".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("duplicated".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("elf".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("duplicated".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("pkg".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string() }
                        )),
                        target_name: Some("duplicated".to_string()),
                        ..OfferResolverDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("stopped".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        target_name: Some("started".to_string()),
                        filter: None,
                        mode: Some(EventMode::Async),
                        ..OfferEventDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source: Some(Ref::Parent(ParentRef {})),
                        source_name: Some("started_on_x".to_string()),
                        target: Some(Ref::Child(ChildRef {
                            name: "netstack".to_string(),
                            collection: None,
                        })),
                        target_name: Some("started".to_string()),
                        filter: None,
                        mode: Some(EventMode::Async),
                        ..OfferEventDecl::EMPTY
                    }),
                ]);
                decl.children = Some(vec![
                    ChildDecl{
                        name: Some("netstack".to_string()),
                        url: Some("fuchsia-pkg://fuchsia.com/netstack/stable#meta/netstack.cm".to_string()),
                        startup: Some(StartupMode::Eager),
                        on_terminate: None,
                        environment: None,
                        ..ChildDecl::EMPTY
                    },
                ]);
                decl.collections = Some(vec![
                    CollectionDecl{
                        name: Some("modular".to_string()),
                        durability: Some(Durability::Persistent),
                        allowed_offers: Some(AllowedOffers::StaticOnly),
                        environment: None,
                        ..CollectionDecl::EMPTY
                    },
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                // Duplicate services are allowed.
                Error::duplicate_field("OfferProtocolDecl", "target_name", "fuchsia.logger.LegacyLog"),
                Error::duplicate_field("OfferDirectoryDecl", "target_name", "assets"),
                Error::duplicate_field("OfferRunnerDecl", "target_name", "duplicated"),
                Error::duplicate_field("OfferResolverDecl", "target_name", "duplicated"),
                Error::duplicate_field("OfferEventDecl", "target_name", "started"),
            ])),
        },
        test_validate_offers_target_invalid => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("logger".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("logger".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string(), }
                        )),
                        target_name: Some("fuchsia.logger.Log".to_string()),
                        ..OfferServiceDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("legacy_logger".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("legacy_logger".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string(), }
                        )),
                        target_name: Some("fuchsia.logger.LegacyLog".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("assets".to_string()),
                        target: Some(Ref::Child(
                           ChildRef {
                               name: "netstack".to_string(),
                               collection: None,
                           }
                        )),
                        target_name: Some("data".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("assets".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string(), }
                        )),
                        target_name: Some("data".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Weak),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Storage(OfferStorageDecl {
                        source_name: Some("data".to_string()),
                        source: Some(Ref::Parent(ParentRef{})),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: None,
                            }
                        )),
                        target_name: Some("data".to_string()),
                        ..OfferStorageDecl::EMPTY
                    }),
                    OfferDecl::Storage(OfferStorageDecl {
                        source_name: Some("data".to_string()),
                        source: Some(Ref::Parent(ParentRef{})),
                        target: Some(Ref::Collection(
                            CollectionRef { name: "modular".to_string(), }
                        )),
                        target_name: Some("data".to_string()),
                        ..OfferStorageDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("elf".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: None,
                            }
                        )),
                        target_name: Some("elf".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("elf".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string(), }
                        )),
                        target_name: Some("elf".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("pkg".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: None,
                            }
                        )),
                        target_name: Some("pkg".to_string()),
                        ..OfferResolverDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Parent(ParentRef{})),
                        source_name: Some("pkg".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string(), }
                        )),
                        target_name: Some("pkg".to_string()),
                        ..OfferResolverDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source_name: Some("started".to_string()),
                        source: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("started".to_string()),
                        target: Some(Ref::Child(
                            ChildRef {
                                name: "netstack".to_string(),
                                collection: None,
                            }
                        )),
                        filter: None,
                        mode: Some(EventMode::Async),
                        ..OfferEventDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source_name: Some("started".to_string()),
                        source: Some(Ref::Parent(ParentRef {})),
                        target_name: Some("started".to_string()),
                        target: Some(Ref::Collection(
                           CollectionRef { name: "modular".to_string(), }
                        )),
                        filter: None,
                        mode: Some(EventMode::Async),
                        ..OfferEventDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_child("OfferServiceDecl", "target", "netstack"),
                Error::invalid_collection("OfferServiceDecl", "target", "modular"),
                Error::invalid_child("OfferProtocolDecl", "target", "netstack"),
                Error::invalid_collection("OfferProtocolDecl", "target", "modular"),
                Error::invalid_child("OfferDirectoryDecl", "target", "netstack"),
                Error::invalid_collection("OfferDirectoryDecl", "target", "modular"),
                Error::invalid_child("OfferStorageDecl", "target", "netstack"),
                Error::invalid_collection("OfferStorageDecl", "target", "modular"),
                Error::invalid_child("OfferRunnerDecl", "target", "netstack"),
                Error::invalid_collection("OfferRunnerDecl", "target", "modular"),
                Error::invalid_child("OfferResolverDecl", "target", "netstack"),
                Error::invalid_collection("OfferResolverDecl", "target", "modular"),
                Error::invalid_child("OfferEventDecl", "target", "netstack"),
                Error::invalid_collection("OfferEventDecl", "target", "modular"),
            ])),
        },
        test_validate_offers_invalid_source_collection => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![
                    CollectionDecl {
                        name: Some("col".to_string()),
                        durability: Some(Durability::Transient),
                        allowed_offers: Some(AllowedOffers::StaticOnly),
                        ..CollectionDecl::EMPTY
                    }
                ]);
                decl.children = Some(vec![
                    ChildDecl {
                        name: Some("child".to_string()),
                        url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        ..ChildDecl::EMPTY
                    }
                ]);
                decl.offers = Some(vec![
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("a".to_string()),
                        target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                        target_name: Some("a".to_string()),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    }),
                    OfferDecl::Directory(OfferDirectoryDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("b".to_string()),
                        target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                        target_name: Some("b".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        subdir: None,
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferDirectoryDecl::EMPTY
                    }),
                    OfferDecl::Storage(OfferStorageDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("c".to_string()),
                        target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                        target_name: Some("c".to_string()),
                        ..OfferStorageDecl::EMPTY
                    }),
                    OfferDecl::Runner(OfferRunnerDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("d".to_string()),
                        target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                        target_name: Some("d".to_string()),
                        ..OfferRunnerDecl::EMPTY
                    }),
                    OfferDecl::Resolver(OfferResolverDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("e".to_string()),
                        target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                        target_name: Some("e".to_string()),
                        ..OfferResolverDecl::EMPTY
                    }),
                    OfferDecl::Event(OfferEventDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("f".to_string()),
                        target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                        target_name: Some("f".to_string()),
                        filter: None,
                        mode: Some(EventMode::Async),
                        ..OfferEventDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("OfferProtocolDecl", "source"),
                Error::invalid_field("OfferDirectoryDecl", "source"),
                Error::invalid_field("OfferStorageDecl", "source"),
                Error::invalid_field("OfferRunnerDecl", "source"),
                Error::invalid_field("OfferResolverDecl", "source"),
                Error::invalid_field("OfferEventDecl", "source"),
            ])),
        },
        test_validate_offers_source_collection => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![
                    CollectionDecl {
                        name: Some("col".to_string()),
                        durability: Some(Durability::Transient),
                        allowed_offers: Some(AllowedOffers::StaticOnly),
                        ..CollectionDecl::EMPTY
                    }
                ]);
                decl.children = Some(vec![
                    ChildDecl {
                        name: Some("child".to_string()),
                        url: Some("fuchsia-pkg://fuchsia.com/foo".to_string()),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        ..ChildDecl::EMPTY
                    }
                ]);
                decl.offers = Some(vec![
                    OfferDecl::Service(OfferServiceDecl {
                        source: Some(Ref::Collection(CollectionRef { name: "col".to_string() })),
                        source_name: Some("a".to_string()),
                        target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                        target_name: Some("a".to_string()),
                        ..OfferServiceDecl::EMPTY
                    })
                ]);
                decl
            },
            result = Ok(()),
        },
        test_validate_offers_event_from_realm => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(
                    vec![
                        Ref::Self_(SelfRef {}),
                        Ref::Child(ChildRef {name: "netstack".to_string(), collection: None }),
                        Ref::Collection(CollectionRef {name: "modular".to_string() }),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, source)| {
                        OfferDecl::Event(OfferEventDecl {
                            source: Some(source),
                            source_name: Some("started".to_string()),
                            target: Some(Ref::Child(ChildRef {
                                name: "netstack".to_string(),
                                collection: None,
                            })),
                            target_name: Some(format!("started_{}", i)),

                            filter: Some(fdata::Dictionary { entries: None, ..fdata::Dictionary::EMPTY }),
                            mode: Some(EventMode::Sync),
                            ..OfferEventDecl::EMPTY
                        })
                    })
                    .collect());
                decl.children = Some(vec![
                    ChildDecl{
                        name: Some("netstack".to_string()),
                        url: Some("fuchsia-pkg://fuchsia.com/netstack/stable#meta/netstack.cm".to_string()),
                        startup: Some(StartupMode::Eager),
                        on_terminate: None,
                        environment: None,
                        ..ChildDecl::EMPTY
                    },
                ]);
                decl.collections = Some(vec![
                    CollectionDecl {
                        name: Some("modular".to_string()),
                        durability: Some(Durability::Persistent),
                        allowed_offers: Some(AllowedOffers::StaticOnly),
                        environment: None,
                        ..CollectionDecl::EMPTY
                    },
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("OfferEventDecl", "source"),
                Error::invalid_field("OfferEventDecl", "source"),
                Error::invalid_field("OfferEventDecl", "source"),
            ])),
        },
        test_validate_offers_long_dependency_cycle => {
            input = {
                let mut decl = new_component_decl();
                let dependencies = vec![
                    ("d", "b"),
                    ("a", "b"),
                    ("b", "c"),
                    ("b", "d"),
                    ("c", "a"),
                ];
                let offers = dependencies.into_iter().map(|(from,to)|
                    OfferDecl::Protocol(OfferProtocolDecl {
                        source: Some(Ref::Child(
                           ChildRef { name: from.to_string(), collection: None },
                        )),
                        source_name: Some(format!("thing_{}", from)),
                        target: Some(Ref::Child(
                           ChildRef { name: to.to_string(), collection: None },
                        )),
                        target_name: Some(format!("thing_{}", from)),
                        dependency_type: Some(DependencyType::Strong),
                        ..OfferProtocolDecl::EMPTY
                    })).collect();
                let children = ["a", "b", "c", "d"].iter().map(|name| {
                    ChildDecl {
                        name: Some(name.to_string()),
                        url: Some(format!("fuchsia-pkg://fuchsia.com/pkg#meta/{}.cm", name)),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        environment: None,
                        ..ChildDecl::EMPTY
                    }
                }).collect();
                decl.offers = Some(offers);
                decl.children = Some(children);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle(directed_graph::Error::CyclesDetected([vec!["child a", "child b", "child c", "child a"], vec!["child b", "child d", "child b"]].iter().cloned().collect()).format_cycle()),
            ])),
        },

        // environments
        test_validate_environment_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: None,
                    extends: None,
                    runners: None,
                    resolvers: None,
                    stop_timeout_ms: None,
                    debug_capabilities: None,
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("EnvironmentDecl", "name"),
                Error::missing_field("EnvironmentDecl", "extends"),
            ])),
        },

        test_validate_environment_no_stop_timeout => {
            input = {  let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("env".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: None,
                    resolvers: None,
                    stop_timeout_ms: None,
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![Error::missing_field("EnvironmentDecl", "stop_timeout_ms")])),
        },

        test_validate_environment_extends_stop_timeout => {
            input = {  let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("env".to_string()),
                    extends: Some(EnvironmentExtends::Realm),
                    runners: None,
                    resolvers: None,
                    stop_timeout_ms: None,
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Ok(()),
        },
        test_validate_environment_long_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("a".repeat(101)),
                    extends: Some(EnvironmentExtends::None),
                    runners: Some(vec![
                        RunnerRegistration {
                            source_name: Some("a".repeat(101)),
                            source: Some(Ref::Parent(ParentRef{})),
                            target_name: Some("a".repeat(101)),
                            ..RunnerRegistration::EMPTY
                        },
                    ]),
                    resolvers: Some(vec![
                        ResolverRegistration {
                            resolver: Some("a".repeat(101)),
                            source: Some(Ref::Parent(ParentRef{})),
                            scheme: Some("a".repeat(101)),
                            ..ResolverRegistration::EMPTY
                        },
                    ]),
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("EnvironmentDecl", "name"),
                Error::field_too_long("RunnerRegistration", "source_name"),
                Error::field_too_long("RunnerRegistration", "target_name"),
                Error::field_too_long("ResolverRegistration", "resolver"),
                Error::field_too_long("ResolverRegistration", "scheme"),
            ])),
        },
        test_validate_environment_empty_runner_resolver_fields => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("a".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: Some(vec![
                        RunnerRegistration {
                            source_name: None,
                            source: None,
                            target_name: None,
                            ..RunnerRegistration::EMPTY
                        },
                    ]),
                    resolvers: Some(vec![
                        ResolverRegistration {
                            resolver: None,
                            source: None,
                            scheme: None,
                            ..ResolverRegistration::EMPTY
                        },
                    ]),
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("RunnerRegistration", "source_name"),
                Error::missing_field("RunnerRegistration", "source"),
                Error::missing_field("RunnerRegistration", "target_name"),
                Error::missing_field("ResolverRegistration", "resolver"),
                Error::missing_field("ResolverRegistration", "source"),
                Error::missing_field("ResolverRegistration", "scheme"),
            ])),
        },
        test_validate_environment_invalid_fields => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("a".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: Some(vec![
                        RunnerRegistration {
                            source_name: Some("^a".to_string()),
                            source: Some(Ref::Framework(fsys::FrameworkRef{})),
                            target_name: Some("%a".to_string()),
                            ..RunnerRegistration::EMPTY
                        },
                    ]),
                    resolvers: Some(vec![
                        ResolverRegistration {
                            resolver: Some("^a".to_string()),
                            source: Some(Ref::Framework(fsys::FrameworkRef{})),
                            scheme: Some("9scheme".to_string()),
                            ..ResolverRegistration::EMPTY
                        },
                    ]),
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("RunnerRegistration", "source_name"),
                Error::invalid_field("RunnerRegistration", "source"),
                Error::invalid_field("RunnerRegistration", "target_name"),
                Error::invalid_field("ResolverRegistration", "resolver"),
                Error::invalid_field("ResolverRegistration", "source"),
                Error::invalid_field("ResolverRegistration", "scheme"),
            ])),
        },
        test_validate_environment_missing_runner => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("a".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: Some(vec![
                        RunnerRegistration {
                            source_name: Some("dart".to_string()),
                            source: Some(Ref::Self_(SelfRef{})),
                            target_name: Some("dart".to_string()),
                            ..RunnerRegistration::EMPTY
                        },
                    ]),
                    resolvers: None,
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_runner("RunnerRegistration", "source_name", "dart"),
            ])),
        },
        test_validate_environment_duplicate_registrations => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("a".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: Some(vec![
                        RunnerRegistration {
                            source_name: Some("dart".to_string()),
                            source: Some(Ref::Parent(ParentRef{})),
                            target_name: Some("dart".to_string()),
                            ..RunnerRegistration::EMPTY
                        },
                        RunnerRegistration {
                            source_name: Some("other-dart".to_string()),
                            source: Some(Ref::Parent(ParentRef{})),
                            target_name: Some("dart".to_string()),
                            ..RunnerRegistration::EMPTY
                        },
                    ]),
                    resolvers: Some(vec![
                        ResolverRegistration {
                            resolver: Some("pkg_resolver".to_string()),
                            source: Some(Ref::Parent(ParentRef{})),
                            scheme: Some("fuchsia-pkg".to_string()),
                            ..ResolverRegistration::EMPTY
                        },
                        ResolverRegistration {
                            resolver: Some("base_resolver".to_string()),
                            source: Some(Ref::Parent(ParentRef{})),
                            scheme: Some("fuchsia-pkg".to_string()),
                            ..ResolverRegistration::EMPTY
                        },
                    ]),
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::duplicate_field("RunnerRegistration", "target_name", "dart"),
                Error::duplicate_field("ResolverRegistration", "scheme", "fuchsia-pkg"),
            ])),
        },
        test_validate_environment_from_missing_child => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("a".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: Some(vec![
                        RunnerRegistration {
                            source_name: Some("elf".to_string()),
                            source: Some(Ref::Child(ChildRef{
                                name: "missing".to_string(),
                                collection: None,
                            })),
                            target_name: Some("elf".to_string()),
                            ..RunnerRegistration::EMPTY
                        },
                    ]),
                    resolvers: Some(vec![
                        ResolverRegistration {
                            resolver: Some("pkg_resolver".to_string()),
                            source: Some(Ref::Child(ChildRef{
                                name: "missing".to_string(),
                                collection: None,
                            })),
                            scheme: Some("fuchsia-pkg".to_string()),
                            ..ResolverRegistration::EMPTY
                        },
                    ]),
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_child("RunnerRegistration", "source", "missing"),
                Error::invalid_child("ResolverRegistration", "source", "missing"),
            ])),
        },
        test_validate_environment_runner_child_cycle => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("env".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: Some(vec![
                        RunnerRegistration {
                            source_name: Some("elf".to_string()),
                            source: Some(Ref::Child(ChildRef{
                                name: "child".to_string(),
                                collection: None,
                            })),
                            target_name: Some("elf".to_string()),
                            ..RunnerRegistration::EMPTY
                        },
                    ]),
                    resolvers: None,
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl.children = Some(vec![ChildDecl {
                    name: Some("child".to_string()),
                    startup: Some(StartupMode::Lazy),
                    on_terminate: None,
                    url: Some("fuchsia-pkg://child".to_string()),
                    environment: Some("env".to_string()),
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle(
                    directed_graph::Error::CyclesDetected([vec!["child child", "environment env", "child child"]].iter().cloned().collect()).format_cycle()
                ),
            ])),
        },
        test_validate_environment_resolver_child_cycle => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("env".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: None,
                    resolvers: Some(vec![
                        ResolverRegistration {
                            resolver: Some("pkg_resolver".to_string()),
                            source: Some(Ref::Child(ChildRef{
                                name: "child".to_string(),
                                collection: None,
                            })),
                            scheme: Some("fuchsia-pkg".to_string()),
                            ..ResolverRegistration::EMPTY
                        },
                    ]),
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl.children = Some(vec![ChildDecl {
                    name: Some("child".to_string()),
                    startup: Some(StartupMode::Lazy),
                    on_terminate: None,
                    url: Some("fuchsia-pkg://child".to_string()),
                    environment: Some("env".to_string()),
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle(
                    directed_graph::Error::CyclesDetected([vec!["child child", "environment env", "child child"]].iter().cloned().collect()).format_cycle()
                ),
            ])),
        },
        test_validate_environment_resolver_multiple_children_cycle => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![EnvironmentDecl {
                    name: Some("env".to_string()),
                    extends: Some(EnvironmentExtends::None),
                    runners: None,
                    resolvers: Some(vec![
                        ResolverRegistration {
                            resolver: Some("pkg_resolver".to_string()),
                            source: Some(Ref::Child(ChildRef{
                                name: "a".to_string(),
                                collection: None,
                            })),
                            scheme: Some("fuchsia-pkg".to_string()),
                            ..ResolverRegistration::EMPTY
                        },
                    ]),
                    stop_timeout_ms: Some(1234),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl.children = Some(vec![
                    ChildDecl {
                        name: Some("a".to_string()),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        url: Some("fuchsia-pkg://child-a".to_string()),
                        environment: None,
                        ..ChildDecl::EMPTY
                    },
                    ChildDecl {
                        name: Some("b".to_string()),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        url: Some("fuchsia-pkg://child-b".to_string()),
                        environment: Some("env".to_string()),
                        ..ChildDecl::EMPTY
                    },
                ]);
                decl.offers = Some(vec![OfferDecl::Service(OfferServiceDecl {
                    source: Some(Ref::Child(ChildRef {
                        name: "b".to_string(),
                        collection: None,
                    })),
                    source_name: Some("thing".to_string()),
                    target: Some(Ref::Child(
                       ChildRef {
                           name: "a".to_string(),
                           collection: None,
                       }
                    )),
                    target_name: Some("thing".to_string()),
                    ..OfferServiceDecl::EMPTY
                })]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::dependency_cycle(
                    directed_graph::Error::CyclesDetected([vec!["child a", "environment env", "child b", "child a"]].iter().cloned().collect()).format_cycle()
                ),
            ])),
        },
        test_validate_environment_debug_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![
                    EnvironmentDecl {
                        name: Some("a".to_string()),
                        extends: Some(EnvironmentExtends::None),
                        stop_timeout_ms: Some(2),
                        debug_capabilities:Some(vec![
                            DebugRegistration::Protocol(DebugProtocolRegistration {
                                source: None,
                                source_name: None,
                                target_name: None,
                                ..DebugProtocolRegistration::EMPTY
                            }),
                    ]),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("DebugProtocolRegistration", "source"),
                Error::missing_field("DebugProtocolRegistration", "source_name"),
                Error::missing_field("DebugProtocolRegistration", "target_name"),
            ])),
        },
        test_validate_environment_debug_log_identifier => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![
                    EnvironmentDecl {
                        name: Some("a".to_string()),
                        extends: Some(EnvironmentExtends::None),
                        stop_timeout_ms: Some(2),
                        debug_capabilities:Some(vec![
                            DebugRegistration::Protocol(DebugProtocolRegistration {
                                source: Some(Ref::Child(ChildRef {
                                    name: "a".repeat(101),
                                    collection: None,
                                })),
                                source_name: Some(format!("{}", "a".repeat(101))),
                                target_name: Some(format!("{}", "b".repeat(101))),
                                ..DebugProtocolRegistration::EMPTY
                            }),
                            DebugRegistration::Protocol(DebugProtocolRegistration {
                                source: Some(Ref::Parent(ParentRef {})),
                                source_name: Some("a".to_string()),
                                target_name: Some(format!("{}", "b".repeat(101))),
                                ..DebugProtocolRegistration::EMPTY
                            }),
                    ]),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("DebugProtocolRegistration", "source.child.name"),
                Error::field_too_long("DebugProtocolRegistration", "source_name"),
                Error::field_too_long("DebugProtocolRegistration", "target_name"),
                Error::field_too_long("DebugProtocolRegistration", "target_name"),
            ])),
        },
        test_validate_environment_debug_log_extraneous => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![
                    EnvironmentDecl {
                        name: Some("a".to_string()),
                        extends: Some(EnvironmentExtends::None),
                        stop_timeout_ms: Some(2),
                        debug_capabilities:Some(vec![
                            DebugRegistration::Protocol(DebugProtocolRegistration {
                                source: Some(Ref::Child(ChildRef {
                                    name: "logger".to_string(),
                                    collection: Some("modular".to_string()),
                                })),
                                source_name: Some("fuchsia.logger.Log".to_string()),
                                target_name: Some("fuchsia.logger.Log".to_string()),
                                ..DebugProtocolRegistration::EMPTY
                            }),
                    ]),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::extraneous_field("DebugProtocolRegistration", "source.child.collection"),
            ])),
        },
        test_validate_environment_debug_log_invalid_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![
                    EnvironmentDecl {
                        name: Some("a".to_string()),
                        extends: Some(EnvironmentExtends::None),
                        stop_timeout_ms: Some(2),
                        debug_capabilities:Some(vec![
                            DebugRegistration::Protocol(DebugProtocolRegistration {
                                source: Some(Ref::Child(ChildRef {
                                    name: "^bad".to_string(),
                                    collection: None,
                                })),
                                source_name: Some("foo/".to_string()),
                                target_name: Some("/".to_string()),
                                ..DebugProtocolRegistration::EMPTY
                            }),
                    ]),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("DebugProtocolRegistration", "source.child.name"),
                Error::invalid_field("DebugProtocolRegistration", "source_name"),
                Error::invalid_field("DebugProtocolRegistration", "target_name"),
            ])),
        },
        test_validate_environment_debug_log_invalid_child => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![
                    EnvironmentDecl {
                        name: Some("a".to_string()),
                        extends: Some(EnvironmentExtends::None),
                        stop_timeout_ms: Some(2),
                        debug_capabilities:Some(vec![
                            DebugRegistration::Protocol(DebugProtocolRegistration {
                                source: Some(Ref::Child(ChildRef {
                                    name: "logger".to_string(),
                                    collection: None,
                                })),
                                source_name: Some("fuchsia.logger.LegacyLog".to_string()),
                                target_name: Some("fuchsia.logger.LegacyLog".to_string()),
                                ..DebugProtocolRegistration::EMPTY
                            }),
                    ]),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl.children = Some(vec![
                    ChildDecl {
                        name: Some("netstack".to_string()),
                        url: Some("fuchsia-pkg://fuchsia.com/netstack/stable#meta/netstack.cm".to_string()),
                        startup: Some(StartupMode::Lazy),
                        on_terminate: None,
                        environment: None,
                        ..ChildDecl::EMPTY
                    },
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_child("DebugProtocolRegistration", "source", "logger"),

            ])),
        },
        test_validate_environment_debug_source_capability => {
            input = {
                let mut decl = new_component_decl();
                decl.environments = Some(vec![
                    EnvironmentDecl {
                        name: Some("a".to_string()),
                        extends: Some(EnvironmentExtends::None),
                        stop_timeout_ms: Some(2),
                        debug_capabilities:Some(vec![
                            DebugRegistration::Protocol(DebugProtocolRegistration {
                                source: Some(Ref::Capability(CapabilityRef {
                                    name: "storage".to_string(),
                                })),
                                source_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                                target_name: Some("fuchsia.sys2.StorageAdmin".to_string()),
                                ..DebugProtocolRegistration::EMPTY
                            }),
                    ]),
                    ..EnvironmentDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("DebugProtocolRegistration", "source"),
            ])),
        },


        // children
        test_validate_children_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.children = Some(vec![ChildDecl{
                    name: None,
                    url: None,
                    startup: None,
                    on_terminate: None,
                    environment: None,
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("ChildDecl", "name"),
                Error::missing_field("ChildDecl", "url"),
                Error::missing_field("ChildDecl", "startup"),
                // `on_terminate` is allowed to be None
            ])),
        },
        test_validate_children_invalid_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.children = Some(vec![ChildDecl{
                    name: Some("^bad".to_string()),
                    url: Some("bad-scheme&://blah".to_string()),
                    startup: Some(StartupMode::Lazy),
                    on_terminate: None,
                    environment: None,
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("ChildDecl", "name"),
                Error::invalid_field("ChildDecl", "url"),
            ])),
        },
        test_validate_children_long_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.children = Some(vec![ChildDecl{
                    name: Some("a".repeat(1025)),
                    url: Some(format!("fuchsia-pkg://{}", "a".repeat(4083))),
                    startup: Some(StartupMode::Lazy),
                    on_terminate: None,
                    environment: Some("a".repeat(1025)),
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("ChildDecl", "name"),
                Error::field_too_long("ChildDecl", "url"),
                Error::field_too_long("ChildDecl", "environment"),
                Error::invalid_environment("ChildDecl", "environment", "a".repeat(1025)),
            ])),
        },
        test_validate_child_references_unknown_env => {
            input = {
                let mut decl = new_component_decl();
                decl.children = Some(vec![ChildDecl{
                    name: Some("foo".to_string()),
                    url: Some("fuchsia-pkg://foo".to_string()),
                    startup: Some(StartupMode::Lazy),
                    on_terminate: None,
                    environment: Some("test_env".to_string()),
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_environment("ChildDecl", "environment", "test_env"),
            ])),
        },

        // collections
        test_validate_collections_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![CollectionDecl{
                    name: None,
                    durability: None,
                    allowed_offers: None,
                    environment: None,
                    ..CollectionDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("CollectionDecl", "name"),
                Error::missing_field("CollectionDecl", "durability"),
            ])),
        },
        test_validate_collections_invalid_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![CollectionDecl{
                    name: Some("^bad".to_string()),
                    durability: Some(Durability::Persistent),
                    allowed_offers: Some(AllowedOffers::StaticOnly),
                    environment: None,
                    ..CollectionDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("CollectionDecl", "name"),
            ])),
        },
        test_validate_collections_long_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![CollectionDecl{
                    name: Some("a".repeat(1025)),
                    durability: Some(Durability::Transient),
                    allowed_offers: Some(AllowedOffers::StaticOnly),
                    environment: None,
                    ..CollectionDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("CollectionDecl", "name"),
            ])),
        },
        test_validate_collection_references_unknown_env => {
            input = {
                let mut decl = new_component_decl();
                decl.collections = Some(vec![CollectionDecl {
                    name: Some("foo".to_string()),
                    durability: Some(Durability::Transient),
                    allowed_offers: Some(AllowedOffers::StaticOnly),
                    environment: Some("test_env".to_string()),
                    ..CollectionDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_environment("CollectionDecl", "environment", "test_env"),
            ])),
        },

        // capabilities
        test_validate_capabilities_empty => {
            input = {
                let mut decl = new_component_decl();
                decl.capabilities = Some(vec![
                    CapabilityDecl::Service(ServiceDecl {
                        name: None,
                        source_path: None,
                        ..ServiceDecl::EMPTY
                    }),
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: None,
                        source_path: None,
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: None,
                        source_path: None,
                        rights: None,
                        ..DirectoryDecl::EMPTY
                    }),
                    CapabilityDecl::Storage(StorageDecl {
                        name: None,
                        source: None,
                        backing_dir: None,
                        subdir: None,
                        storage_id: None,
                        ..StorageDecl::EMPTY
                    }),
                    CapabilityDecl::Runner(RunnerDecl {
                        name: None,
                        source_path: None,
                        ..RunnerDecl::EMPTY
                    }),
                    CapabilityDecl::Resolver(ResolverDecl {
                        name: None,
                        source_path: None,
                        ..ResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::missing_field("ServiceDecl", "name"),
                Error::missing_field("ServiceDecl", "source_path"),
                Error::missing_field("ProtocolDecl", "name"),
                Error::missing_field("ProtocolDecl", "source_path"),
                Error::missing_field("DirectoryDecl", "name"),
                Error::missing_field("DirectoryDecl", "source_path"),
                Error::missing_field("DirectoryDecl", "rights"),
                Error::missing_field("StorageDecl", "source"),
                Error::missing_field("StorageDecl", "name"),
                Error::missing_field("StorageDecl", "storage_id"),
                Error::missing_field("StorageDecl", "backing_dir"),
                Error::missing_field("RunnerDecl", "name"),
                Error::missing_field("RunnerDecl", "source_path"),
                Error::missing_field("ResolverDecl", "name"),
                Error::missing_field("ResolverDecl", "source_path"),
            ])),
        },
        test_validate_capabilities_invalid_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.capabilities = Some(vec![
                    CapabilityDecl::Service(ServiceDecl {
                        name: Some("^bad".to_string()),
                        source_path: Some("&bad".to_string()),
                        ..ServiceDecl::EMPTY
                    }),
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: Some("^bad".to_string()),
                        source_path: Some("&bad".to_string()),
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: Some("^bad".to_string()),
                        source_path: Some("&bad".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        ..DirectoryDecl::EMPTY
                    }),
                    CapabilityDecl::Storage(StorageDecl {
                        name: Some("^bad".to_string()),
                        source: Some(Ref::Collection(CollectionRef {
                            name: "/bad".to_string()
                        })),
                        backing_dir: Some("&bad".to_string()),
                        subdir: None,
                        storage_id: Some(fsys::StorageId::StaticInstanceIdOrMoniker),
                        ..StorageDecl::EMPTY
                    }),
                    CapabilityDecl::Runner(RunnerDecl {
                        name: Some("^bad".to_string()),
                        source_path: Some("&bad".to_string()),
                        ..RunnerDecl::EMPTY
                    }),
                    CapabilityDecl::Resolver(ResolverDecl {
                        name: Some("^bad".to_string()),
                        source_path: Some("&bad".to_string()),
                        ..ResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("ServiceDecl", "name"),
                Error::invalid_field("ServiceDecl", "source_path"),
                Error::invalid_field("ProtocolDecl", "name"),
                Error::invalid_field("ProtocolDecl", "source_path"),
                Error::invalid_field("DirectoryDecl", "name"),
                Error::invalid_field("DirectoryDecl", "source_path"),
                Error::invalid_field("StorageDecl", "source"),
                Error::invalid_field("StorageDecl", "name"),
                Error::invalid_field("StorageDecl", "backing_dir"),
                Error::invalid_field("RunnerDecl", "name"),
                Error::invalid_field("RunnerDecl", "source_path"),
                Error::invalid_field("ResolverDecl", "name"),
                Error::invalid_field("ResolverDecl", "source_path"),
            ])),
        },
        test_validate_capabilities_invalid_child => {
            input = {
                let mut decl = new_component_decl();
                decl.capabilities = Some(vec![
                    CapabilityDecl::Storage(StorageDecl {
                        name: Some("foo".to_string()),
                        source: Some(Ref::Collection(CollectionRef {
                            name: "invalid".to_string(),
                        })),
                        backing_dir: Some("foo".to_string()),
                        subdir: None,
                        storage_id: Some(fsys::StorageId::StaticInstanceIdOrMoniker),
                        ..StorageDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_field("StorageDecl", "source"),
            ])),
        },
        test_validate_capabilities_long_identifiers => {
            input = {
                let mut decl = new_component_decl();
                decl.capabilities = Some(vec![
                    CapabilityDecl::Service(ServiceDecl {
                        name: Some("a".repeat(101)),
                        source_path: Some(format!("/{}", "c".repeat(1024))),
                        ..ServiceDecl::EMPTY
                    }),
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: Some("a".repeat(101)),
                        source_path: Some(format!("/{}", "c".repeat(1024))),
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: Some("a".repeat(101)),
                        source_path: Some(format!("/{}", "c".repeat(1024))),
                        rights: Some(fio2::Operations::Connect),
                        ..DirectoryDecl::EMPTY
                    }),
                    CapabilityDecl::Storage(StorageDecl {
                        name: Some("a".repeat(101)),
                        source: Some(Ref::Child(ChildRef {
                            name: "b".repeat(101),
                            collection: None,
                        })),
                        backing_dir: Some(format!("{}", "c".repeat(101))),
                        subdir: None,
                        storage_id: Some(fsys::StorageId::StaticInstanceIdOrMoniker),
                        ..StorageDecl::EMPTY
                    }),
                    CapabilityDecl::Runner(RunnerDecl {
                        name: Some("a".repeat(101)),
                        source_path: Some(format!("/{}", "c".repeat(1024))),
                        ..RunnerDecl::EMPTY
                    }),
                    CapabilityDecl::Resolver(ResolverDecl {
                        name: Some("a".repeat(101)),
                        source_path: Some(format!("/{}", "b".repeat(1024))),
                        ..ResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::field_too_long("ServiceDecl", "name"),
                Error::field_too_long("ServiceDecl", "source_path"),
                Error::field_too_long("ProtocolDecl", "name"),
                Error::field_too_long("ProtocolDecl", "source_path"),
                Error::field_too_long("DirectoryDecl", "name"),
                Error::field_too_long("DirectoryDecl", "source_path"),
                Error::field_too_long("StorageDecl", "source.child.name"),
                Error::field_too_long("StorageDecl", "name"),
                Error::field_too_long("StorageDecl", "backing_dir"),
                Error::field_too_long("RunnerDecl", "name"),
                Error::field_too_long("RunnerDecl", "source_path"),
                Error::field_too_long("ResolverDecl", "name"),
                Error::field_too_long("ResolverDecl", "source_path"),
            ])),
        },
        test_validate_capabilities_duplicate_name => {
            input = {
                let mut decl = new_component_decl();
                decl.capabilities = Some(vec![
                    CapabilityDecl::Service(ServiceDecl {
                        name: Some("service".to_string()),
                        source_path: Some("/service".to_string()),
                        ..ServiceDecl::EMPTY
                    }),
                    CapabilityDecl::Service(ServiceDecl {
                        name: Some("service".to_string()),
                        source_path: Some("/service".to_string()),
                        ..ServiceDecl::EMPTY
                    }),
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: Some("protocol".to_string()),
                        source_path: Some("/protocol".to_string()),
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Protocol(ProtocolDecl {
                        name: Some("protocol".to_string()),
                        source_path: Some("/protocol".to_string()),
                        ..ProtocolDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: Some("directory".to_string()),
                        source_path: Some("/directory".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        ..DirectoryDecl::EMPTY
                    }),
                    CapabilityDecl::Directory(DirectoryDecl {
                        name: Some("directory".to_string()),
                        source_path: Some("/directory".to_string()),
                        rights: Some(fio2::Operations::Connect),
                        ..DirectoryDecl::EMPTY
                    }),
                    CapabilityDecl::Storage(StorageDecl {
                        name: Some("storage".to_string()),
                        source: Some(Ref::Self_(SelfRef{})),
                        backing_dir: Some("directory".to_string()),
                        subdir: None,
                        storage_id: Some(fsys::StorageId::StaticInstanceIdOrMoniker),
                        ..StorageDecl::EMPTY
                    }),
                    CapabilityDecl::Storage(StorageDecl {
                        name: Some("storage".to_string()),
                        source: Some(Ref::Self_(SelfRef{})),
                        backing_dir: Some("directory".to_string()),
                        subdir: None,
                        storage_id: Some(fsys::StorageId::StaticInstanceIdOrMoniker),
                        ..StorageDecl::EMPTY
                    }),
                    CapabilityDecl::Runner(RunnerDecl {
                        name: Some("runner".to_string()),
                        source_path: Some("/runner".to_string()),
                        ..RunnerDecl::EMPTY
                    }),
                    CapabilityDecl::Runner(RunnerDecl {
                        name: Some("runner".to_string()),
                        source_path: Some("/runner".to_string()),
                        ..RunnerDecl::EMPTY
                    }),
                    CapabilityDecl::Resolver(ResolverDecl {
                        name: Some("resolver".to_string()),
                        source_path: Some("/resolver".to_string()),
                        ..ResolverDecl::EMPTY
                    }),
                    CapabilityDecl::Resolver(ResolverDecl {
                        name: Some("resolver".to_string()),
                        source_path: Some("/resolver".to_string()),
                        ..ResolverDecl::EMPTY
                    }),
                ]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::duplicate_field("ServiceDecl", "name", "service"),
                Error::duplicate_field("ProtocolDecl", "name", "protocol"),
                Error::duplicate_field("DirectoryDecl", "name", "directory"),
                Error::duplicate_field("StorageDecl", "name", "storage"),
                Error::duplicate_field("RunnerDecl", "name", "runner"),
                Error::duplicate_field("ResolverDecl", "name", "resolver"),
            ])),
        },

        test_validate_resolvers_missing_from_offer => {
            input = {
                let mut decl = new_component_decl();
                decl.offers = Some(vec![OfferDecl::Resolver(OfferResolverDecl {
                    source: Some(Ref::Self_(SelfRef {})),
                    source_name: Some("a".to_string()),
                    target: Some(Ref::Child(ChildRef { name: "child".to_string(), collection: None })),
                    target_name: Some("a".to_string()),
                    ..OfferResolverDecl::EMPTY
                })]);
                decl.children = Some(vec![ChildDecl {
                    name: Some("child".to_string()),
                    url: Some("test:///child".to_string()),
                    startup: Some(StartupMode::Eager),
                    on_terminate: None,
                    environment: None,
                    ..ChildDecl::EMPTY
                }]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_capability("OfferResolverDecl", "source", "a"),
            ])),
        },
        test_validate_resolvers_missing_from_expose => {
            input = {
                let mut decl = new_component_decl();
                decl.exposes = Some(vec![ExposeDecl::Resolver(ExposeResolverDecl {
                    source: Some(Ref::Self_(SelfRef {})),
                    source_name: Some("a".to_string()),
                    target: Some(Ref::Parent(ParentRef {})),
                    target_name: Some("a".to_string()),
                    ..ExposeResolverDecl::EMPTY
                })]);
                decl
            },
            result = Err(ErrorList::new(vec![
                Error::invalid_capability("ExposeResolverDecl", "source", "a"),
            ])),
        },
    }

    test_validate_capabilities! {
        test_validate_capabilities_individually_ok => {
            input = vec![
                CapabilityDecl::Protocol(ProtocolDecl {
                    name: Some("foo_svc".into()),
                    source_path: Some("/svc/foo".into()),
                    ..ProtocolDecl::EMPTY
                }),
                CapabilityDecl::Directory(DirectoryDecl {
                    name: Some("foo_dir".into()),
                    source_path: Some("/foo".into()),
                    rights: Some(fio2::Operations::Connect),
                    ..DirectoryDecl::EMPTY
                }),
            ],
            as_builtin = false,
            result = Ok(()),
        },
        test_validate_capabilities_individually_err => {
            input = vec![
                CapabilityDecl::Protocol(ProtocolDecl {
                    name: None,
                    source_path: None,
                    ..ProtocolDecl::EMPTY
                }),
                CapabilityDecl::Directory(DirectoryDecl {
                    name: None,
                    source_path: None,
                    rights: None,
                    ..DirectoryDecl::EMPTY
                }),
                CapabilityDecl::Event(EventDecl {
                    name: None,
                    ..EventDecl::EMPTY
                }),
            ],
            as_builtin = false,
            result = Err(ErrorList::new(vec![
                Error::missing_field("ProtocolDecl", "name"),
                Error::missing_field("ProtocolDecl", "source_path"),
                Error::missing_field("DirectoryDecl", "name"),
                Error::missing_field("DirectoryDecl", "source_path"),
                Error::missing_field("DirectoryDecl", "rights"),
                Error::invalid_capability_type("ComponentDecl", "capability", "event")
            ])),
        },
        test_validate_builtin_capabilities_individually_ok => {
            input = vec![
                CapabilityDecl::Protocol(ProtocolDecl {
                    name: Some("foo_protocol".into()),
                    source_path: None,
                    ..ProtocolDecl::EMPTY
                }),
                CapabilityDecl::Directory(DirectoryDecl {
                    name: Some("foo_dir".into()),
                    source_path: None,
                    rights: Some(fio2::Operations::Connect),
                    ..DirectoryDecl::EMPTY
                }),
                CapabilityDecl::Service(ServiceDecl {
                    name: Some("foo_svc".into()),
                    source_path: None,
                    ..ServiceDecl::EMPTY
                }),
                CapabilityDecl::Runner(RunnerDecl {
                    name: Some("foo_runner".into()),
                    source_path: None,
                    ..RunnerDecl::EMPTY
                }),
                CapabilityDecl::Resolver(ResolverDecl {
                    name: Some("foo_resolver".into()),
                    source_path: None,
                    ..ResolverDecl::EMPTY
                }),
                CapabilityDecl::Event(EventDecl {
                    name: Some("foo_event".into()),
                    ..EventDecl::EMPTY
                }),
            ],
            as_builtin = true,
            result = Ok(()),
        },
        test_validate_builtin_capabilities_individually_err => {
            input = vec![
                CapabilityDecl::Protocol(ProtocolDecl {
                    name: None,
                    source_path: Some("/svc/foo".into()),
                    ..ProtocolDecl::EMPTY
                }),
                CapabilityDecl::Directory(DirectoryDecl {
                    name: None,
                    source_path: Some("/foo".into()),
                    rights: None,
                    ..DirectoryDecl::EMPTY
                }),
                CapabilityDecl::Service(ServiceDecl {
                    name: None,
                    source_path: Some("/svc/foo".into()),
                    ..ServiceDecl::EMPTY
                }),
                CapabilityDecl::Runner(RunnerDecl {
                    name: None,
                    source_path:  Some("/foo".into()),
                    ..RunnerDecl::EMPTY
                }),
                CapabilityDecl::Resolver(ResolverDecl {
                    name: None,
                    source_path:  Some("/foo".into()),
                    ..ResolverDecl::EMPTY
                }),
                CapabilityDecl::Event(EventDecl {
                    name: None,
                    ..EventDecl::EMPTY
                }),
                CapabilityDecl::Storage(StorageDecl {
                    name: None,
                    ..StorageDecl::EMPTY
                }),
            ],
            as_builtin = true,
            result = Err(ErrorList::new(vec![
                Error::missing_field("ProtocolDecl", "name"),
                Error::extraneous_source_path("ProtocolDecl", "/svc/foo"),
                Error::missing_field("DirectoryDecl", "name"),
                Error::extraneous_source_path("DirectoryDecl", "/foo"),
                Error::missing_field("DirectoryDecl", "rights"),
                Error::missing_field("ServiceDecl", "name"),
                Error::extraneous_source_path("ServiceDecl", "/svc/foo"),
                Error::missing_field("RunnerDecl", "name"),
                Error::extraneous_source_path("RunnerDecl", "/foo"),
                Error::missing_field("ResolverDecl", "name"),
                Error::extraneous_source_path("ResolverDecl", "/foo"),
                Error::missing_field("EventDecl", "name"),
                Error::invalid_capability_type("RuntimeConfig", "capability", "storage"),
            ])),
        },
    }

    #[test]
    fn test_validate_dynamic_offers_empty() {
        assert_eq!(validate_dynamic_offers(&vec![]), Ok(()));
    }

    #[test]
    fn test_validate_dynamic_offers_okay() {
        assert_eq!(
            validate_dynamic_offers(&vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    dependency_type: Some(DependencyType::Strong),
                    source: Some(Ref::Self_(SelfRef)),
                    source_name: Some("thing".to_string()),
                    target_name: Some("thing".to_string()),
                    ..OfferProtocolDecl::EMPTY
                }),
                OfferDecl::Service(OfferServiceDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    source_name: Some("thang".to_string()),
                    target_name: Some("thang".to_string()),
                    ..OfferServiceDecl::EMPTY
                }),
                OfferDecl::Directory(OfferDirectoryDecl {
                    dependency_type: Some(DependencyType::Strong),
                    source: Some(Ref::Parent(ParentRef)),
                    source_name: Some("thung1".to_string()),
                    target_name: Some("thung1".to_string()),
                    ..OfferDirectoryDecl::EMPTY
                }),
                OfferDecl::Storage(OfferStorageDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    source_name: Some("thung2".to_string()),
                    target_name: Some("thung2".to_string()),
                    ..OfferStorageDecl::EMPTY
                }),
                OfferDecl::Runner(OfferRunnerDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    source_name: Some("thung3".to_string()),
                    target_name: Some("thung3".to_string()),
                    ..OfferRunnerDecl::EMPTY
                }),
                OfferDecl::Resolver(OfferResolverDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    source_name: Some("thung4".to_string()),
                    target_name: Some("thung4".to_string()),
                    ..OfferResolverDecl::EMPTY
                }),
                OfferDecl::Event(OfferEventDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    source_name: Some("thung5".to_string()),
                    target_name: Some("thung5".to_string()),
                    mode: Some(EventMode::Async),
                    ..OfferEventDecl::EMPTY
                }),
            ]),
            Ok(())
        );
    }

    #[test]
    fn test_validate_dynamic_offers_specify_target() {
        assert_eq!(
            validate_dynamic_offers(&vec![
                OfferDecl::Protocol(OfferProtocolDecl {
                    dependency_type: Some(DependencyType::Strong),
                    source: Some(Ref::Self_(SelfRef)),
                    target: Some(Ref::Child(ChildRef {
                        name: "foo".to_string(),
                        collection: None
                    })),
                    source_name: Some("thing".to_string()),
                    target_name: Some("thing".to_string()),
                    ..OfferProtocolDecl::EMPTY
                }),
                OfferDecl::Service(OfferServiceDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    target: Some(Ref::Child(ChildRef {
                        name: "bar".to_string(),
                        collection: Some("baz".to_string())
                    })),
                    source_name: Some("thang".to_string()),
                    target_name: Some("thang".to_string()),
                    ..OfferServiceDecl::EMPTY
                }),
                OfferDecl::Directory(OfferDirectoryDecl {
                    dependency_type: Some(DependencyType::Strong),
                    source: Some(Ref::Parent(ParentRef)),
                    target: Some(Ref::Framework(FrameworkRef)),
                    source_name: Some("thung1".to_string()),
                    target_name: Some("thung1".to_string()),
                    ..OfferDirectoryDecl::EMPTY
                }),
                OfferDecl::Storage(OfferStorageDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    target: Some(Ref::Child(ChildRef {
                        name: "bar".to_string(),
                        collection: Some("baz".to_string())
                    })),
                    source_name: Some("thung2".to_string()),
                    target_name: Some("thung2".to_string()),
                    ..OfferStorageDecl::EMPTY
                }),
                OfferDecl::Runner(OfferRunnerDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    target: Some(Ref::Child(ChildRef {
                        name: "bar".to_string(),
                        collection: Some("baz".to_string())
                    })),
                    source_name: Some("thung3".to_string()),
                    target_name: Some("thung3".to_string()),
                    ..OfferRunnerDecl::EMPTY
                }),
                OfferDecl::Resolver(OfferResolverDecl {
                    source: Some(Ref::Parent(ParentRef)),
                    target: Some(Ref::Child(ChildRef {
                        name: "bar".to_string(),
                        collection: Some("baz".to_string())
                    })),
                    source_name: Some("thung4".to_string()),
                    target_name: Some("thung4".to_string()),
                    ..OfferResolverDecl::EMPTY
                }),
                OfferDecl::Event(OfferEventDecl {
                    target: Some(Ref::Child(ChildRef {
                        name: "bar".to_string(),
                        collection: Some("baz".to_string())
                    })),
                    source: Some(Ref::Parent(ParentRef)),
                    source_name: Some("thung5".to_string()),
                    target_name: Some("thung5".to_string()),
                    mode: Some(EventMode::Async),
                    ..OfferEventDecl::EMPTY
                }),
            ]),
            Err(ErrorList::new(vec![
                Error::extraneous_field("OfferProtocolDecl", "target"),
                Error::extraneous_field("OfferServiceDecl", "target"),
                Error::extraneous_field("OfferDirectoryDecl", "target"),
                Error::extraneous_field("OfferStorageDecl", "target"),
                Error::extraneous_field("OfferRunnerDecl", "target"),
                Error::extraneous_field("OfferResolverDecl", "target"),
                Error::extraneous_field("OfferEventDecl", "target"),
            ]))
        );
    }

    #[test]
    fn test_validate_dynamic_offers_missing_stuff() {
        assert_eq!(
            validate_dynamic_offers(&vec![
                OfferDecl::Protocol(OfferProtocolDecl::EMPTY),
                OfferDecl::Service(OfferServiceDecl::EMPTY),
                OfferDecl::Directory(OfferDirectoryDecl::EMPTY),
                OfferDecl::Storage(OfferStorageDecl::EMPTY),
                OfferDecl::Runner(OfferRunnerDecl::EMPTY),
                OfferDecl::Resolver(OfferResolverDecl::EMPTY),
                OfferDecl::Event(OfferEventDecl::EMPTY),
            ]),
            Err(ErrorList::new(vec![
                Error::missing_field("OfferProtocolDecl", "source"),
                Error::missing_field("OfferProtocolDecl", "source_name"),
                Error::missing_field("OfferProtocolDecl", "target_name"),
                Error::missing_field("OfferProtocolDecl", "dependency_type"),
                Error::missing_field("OfferServiceDecl", "source"),
                Error::missing_field("OfferServiceDecl", "source_name"),
                Error::missing_field("OfferServiceDecl", "target_name"),
                Error::missing_field("OfferDirectoryDecl", "source"),
                Error::missing_field("OfferDirectoryDecl", "source_name"),
                Error::missing_field("OfferDirectoryDecl", "target_name"),
                Error::missing_field("OfferDirectoryDecl", "dependency_type"),
                Error::missing_field("OfferStorageDecl", "source_name"),
                Error::missing_field("OfferStorageDecl", "source"),
                Error::missing_field("OfferRunnerDecl", "source"),
                Error::missing_field("OfferRunnerDecl", "source_name"),
                Error::missing_field("OfferRunnerDecl", "target_name"),
                Error::missing_field("OfferResolverDecl", "source"),
                Error::missing_field("OfferResolverDecl", "source_name"),
                Error::missing_field("OfferResolverDecl", "target_name"),
                Error::missing_field("OfferEventDecl", "source_name"),
                Error::missing_field("OfferEventDecl", "source"),
                Error::missing_field("OfferEventDecl", "target_name"),
                Error::missing_field("OfferEventDecl", "mode"),
            ]))
        );
    }

    test_dependency! {
        test_validate_offers_protocol_dependency_cycle => {
            ty = OfferDecl::Protocol,
            offer_decl = OfferProtocolDecl {
                source: None,  // Filled by macro
                target: None,  // Filled by macro
                source_name: Some(format!("thing")),
                target_name: Some(format!("thing")),
                dependency_type: Some(DependencyType::Strong),
                ..OfferProtocolDecl::EMPTY
            },
        },
        test_validate_offers_directory_dependency_cycle => {
            ty = OfferDecl::Directory,
            offer_decl = OfferDirectoryDecl {
                source: None,  // Filled by macro
                target: None,  // Filled by macro
                source_name: Some(format!("thing")),
                target_name: Some(format!("thing")),
                rights: Some(fio2::Operations::Connect),
                subdir: None,
                dependency_type: Some(DependencyType::Strong),
                ..OfferDirectoryDecl::EMPTY
            },
        },
        test_validate_offers_service_dependency_cycle => {
            ty = OfferDecl::Service,
            offer_decl = OfferServiceDecl {
                source: None,  // Filled by macro
                target: None,  // Filled by macro
                source_name: Some(format!("thing")),
                target_name: Some(format!("thing")),
                ..OfferServiceDecl::EMPTY
            },
        },
        test_validate_offers_runner_dependency_cycle => {
            ty = OfferDecl::Runner,
            offer_decl = OfferRunnerDecl {
                source: None,  // Filled by macro
                target: None,  // Filled by macro
                source_name: Some(format!("thing")),
                target_name: Some(format!("thing")),
                ..OfferRunnerDecl::EMPTY
            },
        },
        test_validate_offers_resolver_dependency_cycle => {
            ty = OfferDecl::Resolver,
            offer_decl = OfferResolverDecl {
                source: None,  // Filled by macro
                target: None,  // Filled by macro
                source_name: Some(format!("thing")),
                target_name: Some(format!("thing")),
                ..OfferResolverDecl::EMPTY
            },
        },
    }
    test_weak_dependency! {
        test_validate_offers_protocol_weak_dependency_cycle => {
            ty = OfferDecl::Protocol,
            offer_decl = OfferProtocolDecl {
                source: None,  // Filled by macro
                target: None,  // Filled by macro
                source_name: Some(format!("thing")),
                target_name: Some(format!("thing")),
                dependency_type: None, // Filled by macro
                ..OfferProtocolDecl::EMPTY
            },
        },
        test_validate_offers_directory_weak_dependency_cycle => {
            ty = OfferDecl::Directory,
            offer_decl = OfferDirectoryDecl {
                source: None,  // Filled by macro
                target: None,  // Filled by macro
                source_name: Some(format!("thing")),
                target_name: Some(format!("thing")),
                rights: Some(fio2::Operations::Connect),
                subdir: None,
                dependency_type: None,  // Filled by macro
                ..OfferDirectoryDecl::EMPTY
            },
        },
    }
}
