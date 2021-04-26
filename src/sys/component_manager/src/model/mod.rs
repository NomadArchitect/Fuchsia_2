// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod actions;
pub mod binding;
pub mod component;
pub mod error;
pub mod event_logger;
pub mod hooks;
pub mod hub;
pub mod model;
// TODO: This would be #[cfg(test)], but it cannot be because some external crates depend on
// fuctionality in this module. Factor out the externally-depended code into its own module.
pub mod testing;

pub(crate) mod context;
pub(crate) mod environment;
pub(crate) mod events;
pub(crate) mod logging;
pub(crate) mod namespace;
pub(crate) mod policy;
pub(crate) mod resolve_component_factory;
pub(crate) mod resolver;
pub(crate) mod rights;
pub(crate) mod routing;
pub(crate) mod routing_fns;
pub(crate) mod runner;
pub(crate) mod storage;

mod addable_directory;
mod component_controller;
mod dir_tree;
mod exposed_dir;
mod resolve_component;

#[cfg(test)]
mod tests;
