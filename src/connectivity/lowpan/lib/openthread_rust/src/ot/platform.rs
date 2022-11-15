// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use std::task::Context;

/// Trait for OpenThread platform implementations.
pub trait Platform {
    /// Asynchronously process platform implementation tasks.
    ///
    /// # Safety
    ///
    /// This method is unsafe because it MUST ONLY be called from the
    /// same thread that the OpenThread instance is being used on.
    ///
    /// You should never need to call this directly.
    unsafe fn process_poll(
        &mut self,
        instance: &crate::ot::Instance,
        cx: &mut Context<'_>,
    ) -> Result<(), Error>;
}

/// Platform instance which does nothing.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct NullPlatform;

impl Platform for NullPlatform {
    unsafe fn process_poll(
        &mut self,
        _instance: &crate::ot::Instance,
        _cx: &mut Context<'_>,
    ) -> Result<(), Error> {
        Ok(())
    }
}
