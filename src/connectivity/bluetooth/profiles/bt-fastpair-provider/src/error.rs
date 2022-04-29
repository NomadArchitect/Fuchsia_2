// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_bluetooth_gatt2 as gatt;
use fidl_fuchsia_bluetooth_le as le;
use thiserror::Error;

/// Errors that occur during the operation of the Fast Pair Provider component.
#[derive(Error, Debug)]
pub enum Error {
    /// `fuchsia.bluetooth.gatt2` API errors.
    #[error("Error in gatt2 FIDL: {0:?}")]
    GattError(gatt::Error),

    /// Error encountered specifically in the `gatt2.Server.PublishService` method.
    #[error("Error publishing GATT service: {0:?}")]
    PublishError(gatt::PublishServiceError),

    /// Error encountered when trying to advertise via `le.Peripheral`.
    #[error("Error trying to advertise over LE: {:?}", .0)]
    AdvertiseError(le::PeripheralError),

    /// An invalid Model ID was provided to the component.
    #[error("Invalid device Model ID: {0}")]
    InvalidModelId(u32),

    /// Internal component Error.
    #[error("Internal component Error: {0}")]
    InternalError(#[from] anyhow::Error),

    #[error("Fidl Error: {0}")]
    Fidl(#[from] fidl::Error),
}

impl From<gatt::Error> for Error {
    fn from(src: gatt::Error) -> Error {
        Self::GattError(src)
    }
}

impl From<gatt::PublishServiceError> for Error {
    fn from(src: gatt::PublishServiceError) -> Error {
        Self::PublishError(src)
    }
}

impl From<le::PeripheralError> for Error {
    fn from(src: le::PeripheralError) -> Error {
        Self::AdvertiseError(src)
    }
}
