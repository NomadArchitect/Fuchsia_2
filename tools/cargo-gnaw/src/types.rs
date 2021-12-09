// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::gn::add_version_suffix,
    anyhow::{anyhow, Error},
    cargo_metadata::Package,
    std::convert::TryFrom,
};

pub type Feature = String;
pub type Platform = String;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum GnRustType {
    ProcMacro,
    Library,
    StaticLibrary,
    Binary,
    Example,
    Test,
    Bench,
    BuildScript,
}

impl TryFrom<&Vec<String>> for GnRustType {
    type Error = Error;

    fn try_from(value: &Vec<String>) -> Result<Self, Self::Error> {
        match value.as_slice() {
            [value] => match value.as_str() {
                "bin" => Ok(GnRustType::Binary),
                "lib" => Ok(GnRustType::Library),
                "rlib" => Ok(GnRustType::StaticLibrary),
                "proc-macro" => Ok(GnRustType::ProcMacro),
                "test" => Ok(GnRustType::Test),
                "example" => Ok(GnRustType::Example),
                "bench" => Ok(GnRustType::Bench),
                "custom-build" => Ok(GnRustType::BuildScript),
                value => Err(anyhow!("unknown crate type: {}", value)),
            },
            value => Err(anyhow!("unhandled multiple crate types: {:?}", value)),
        }
    }
}

pub trait GnData {
    fn gn_name(&self) -> String;
    fn is_proc_macro(&self) -> bool;
}

impl GnData for Package {
    fn gn_name(&self) -> String {
        add_version_suffix(&self.name, &self.version)
    }

    fn is_proc_macro(&self) -> bool {
        for target in &self.targets {
            for kind in &target.kind {
                if kind == "proc-macro" {
                    return true;
                }
            }
        }
        false
    }
}
