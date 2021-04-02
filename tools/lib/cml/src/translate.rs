// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::error::Error,
    crate::{AnyRef, Capability, RightsClause},
    cm_types::Name,
    fidl_fuchsia_io2 as fio2, fidl_fuchsia_sys2 as fsys,
    std::collections::HashSet,
    std::convert::Into,
};

pub fn translate_capabilities(
    capabilities_in: &Vec<Capability>,
) -> Result<Vec<fsys::CapabilityDecl>, Error> {
    let mut out_capabilities = vec![];
    for capability in capabilities_in {
        if let Some(service) = &capability.service {
            for n in service.to_vec() {
                let source_path = capability
                    .path
                    .clone()
                    .unwrap_or_else(|| format!("/svc/{}", n).parse().unwrap());
                out_capabilities.push(fsys::CapabilityDecl::Service(fsys::ServiceDecl {
                    name: Some(n.clone().into()),
                    source_path: Some(source_path.into()),
                    ..fsys::ServiceDecl::EMPTY
                }));
            }
        } else if let Some(protocol) = &capability.protocol {
            for n in protocol.to_vec() {
                let source_path = capability
                    .path
                    .clone()
                    .unwrap_or_else(|| format!("/svc/{}", n).parse().unwrap());
                out_capabilities.push(fsys::CapabilityDecl::Protocol(fsys::ProtocolDecl {
                    name: Some(n.clone().into()),
                    source_path: Some(source_path.into()),
                    ..fsys::ProtocolDecl::EMPTY
                }));
            }
        } else if let Some(n) = &capability.directory {
            let source_path =
                capability.path.clone().unwrap_or_else(|| format!("/svc/{}", n).parse().unwrap());
            let rights = extract_required_rights(capability, "capability")?;
            out_capabilities.push(fsys::CapabilityDecl::Directory(fsys::DirectoryDecl {
                name: Some(n.clone().into()),
                source_path: Some(source_path.into()),
                rights: Some(rights),
                ..fsys::DirectoryDecl::EMPTY
            }));
        } else if let Some(n) = &capability.storage {
            let backing_dir = capability
                .backing_dir
                .as_ref()
                .expect("storage has no path or backing_dir")
                .clone()
                .into();
            out_capabilities.push(fsys::CapabilityDecl::Storage(fsys::StorageDecl {
                name: Some(n.clone().into()),
                backing_dir: Some(backing_dir),
                source: Some(offer_source_from_ref(
                    capability.from.as_ref().unwrap().into(),
                    None,
                    None,
                )?),
                subdir: capability.subdir.clone().map(Into::into),
                ..fsys::StorageDecl::EMPTY
            }));
        } else if let Some(n) = &capability.runner {
            out_capabilities.push(fsys::CapabilityDecl::Runner(fsys::RunnerDecl {
                name: Some(n.clone().into()),
                source_path: Some(capability.path.clone().expect("missing path").into()),
                ..fsys::RunnerDecl::EMPTY
            }));
        } else if let Some(n) = &capability.resolver {
            out_capabilities.push(fsys::CapabilityDecl::Resolver(fsys::ResolverDecl {
                name: Some(n.clone().into()),
                source_path: Some(capability.path.clone().expect("missing path").into()),
                ..fsys::ResolverDecl::EMPTY
            }));
        } else {
            return Err(Error::internal(format!("no capability in use declaration")));
        }
    }
    Ok(out_capabilities)
}

pub fn extract_required_rights<T>(in_obj: &T, keyword: &str) -> Result<fio2::Operations, Error>
where
    T: RightsClause,
{
    match in_obj.rights() {
        Some(rights_tokens) => {
            let mut rights = Vec::new();
            for token in rights_tokens.0.iter() {
                rights.append(&mut token.expand())
            }
            if rights.is_empty() {
                return Err(Error::missing_rights(format!(
                    "Rights provided to `{}` are not well formed.",
                    keyword
                )));
            }
            let mut seen_rights = HashSet::with_capacity(rights.len());
            let mut operations: fio2::Operations = fio2::Operations::empty();
            for right in rights.iter() {
                if seen_rights.contains(&right) {
                    return Err(Error::duplicate_rights(format!(
                        "Rights provided to `{}` are not well formed.",
                        keyword
                    )));
                }
                seen_rights.insert(right);
                operations |= *right;
            }

            Ok(operations)
        }
        None => Err(Error::internal(format!(
            "No `{}` rights provided but required for directories",
            keyword
        ))),
    }
}

pub fn offer_source_from_ref(
    reference: AnyRef<'_>,
    all_capability_names: Option<&HashSet<Name>>,
    all_collection_names: Option<&HashSet<&Name>>,
) -> Result<fsys::Ref, Error> {
    match reference {
        AnyRef::Named(name) => {
            if all_capability_names.is_some() && all_capability_names.unwrap().contains(&name) {
                Ok(fsys::Ref::Capability(fsys::CapabilityRef { name: name.clone().into() }))
            } else if all_collection_names.is_some()
                && all_collection_names.unwrap().contains(&name)
            {
                Ok(fsys::Ref::Collection(fsys::CollectionRef { name: name.clone().into() }))
            } else {
                Ok(fsys::Ref::Child(fsys::ChildRef { name: name.clone().into(), collection: None }))
            }
        }
        AnyRef::Framework => Ok(fsys::Ref::Framework(fsys::FrameworkRef {})),
        AnyRef::Debug => Ok(fsys::Ref::Debug(fsys::DebugRef {})),
        AnyRef::Parent => Ok(fsys::Ref::Parent(fsys::ParentRef {})),
        AnyRef::Self_ => Ok(fsys::Ref::Self_(fsys::SelfRef {})),
    }
}
