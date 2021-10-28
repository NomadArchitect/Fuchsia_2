// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod component_model;
pub mod component_tree;
pub mod route;
pub mod serde_ext;

use {cm_rust::ComponentDecl, thiserror::Error};

/// Defines a custom AnalyzerError for a given component manifest.
#[derive(Error, Debug)]
pub enum AnalyzerError {}

/// The `CmAnalyzer` trait defines a common entry point for analyzers that are
/// passed in a component manifest. If an error is detected a AnalyzerError should
/// be returned. Analyzer errors may not represent an invalid manifest but may
/// offer suggestions on improvements or better idioms. This is distinct from
/// the `cm_validator` library which is concerned with direct validation of the
/// manifest.
pub trait ComponentDeclAnalyzer {
    /// Analyze the component manifest, only returning an Error if a analyze failure
    /// is detected.
    fn analyze(decl: &ComponentDecl) -> Result<(), AnalyzerError>;
}
