// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::peer::calls::{CallIdx, CallState};
use {std::error::Error as StdError, thiserror::Error};

/// Errors that occur during the operation of the HFP Bluetooth Profile component.
#[derive(Error, Debug)]
pub enum Error {
    #[error("Error using BR/EDR resource {:?}", .resource)]
    ProfileResourceError { resource: ProfileResource, source: Box<dyn StdError> },
    #[error("System error encountered: {}", .message)]
    System { message: String, source: Box<dyn StdError> },
    #[error("Peer removed")]
    PeerRemoved,
    #[error("Value out of range")]
    OutOfRange,
    #[error("Invalid request: {}", .0)]
    ClientProtocol(Box<dyn StdError>),
}

#[derive(Debug)]
pub enum ProfileResource {
    SearchResults,
    ConnectionReceiver,
    Advertise,
}

#[derive(Debug, Error)]
#[error("Advertisement Terminated")]
pub struct AdvertisementTerminated;

impl Error {
    /// Make a new ProfileResourceError
    fn profile_resource<E: StdError + 'static>(resource: ProfileResource, e: E) -> Self {
        Error::ProfileResourceError { resource, source: Box::new(e) }
    }

    /// An error occurred when attempting to register an advertisement.
    pub fn profile_advertise<E: StdError + 'static>(e: E) -> Self {
        Self::profile_resource(ProfileResource::Advertise, e)
    }

    /// An error occurred when attempting to use the fuchsia.bluetooth.bredr.SearchResults fidl
    /// protocol.
    pub fn profile_search_results<E: StdError + 'static>(e: E) -> Self {
        Self::profile_resource(ProfileResource::SearchResults, e)
    }

    /// An error occurred when attempting to use the fuchsia.bluetooth.bredr.ConnectionReceiver fidl
    /// protocol.
    pub fn profile_connection_receiver<E: StdError + 'static>(e: E) -> Self {
        Self::profile_resource(ProfileResource::ConnectionReceiver, e)
    }

    /// An error occurred when interacting with the system.
    ///
    /// This allocates memory which could fail if the error is an OOM.
    pub fn system<E: StdError + 'static>(message: impl Into<String>, e: E) -> Self {
        Self::System { message: message.into(), source: Box::new(e) }
    }
}

/// A request was made using an unknown call.
#[derive(Debug, PartialEq, Clone, Error)]
pub enum CallError {
    #[error("Unknown call index {}", .0)]
    UnknownIndexError(CallIdx),
    #[error("No call in states {:?}", .0)]
    None(Vec<CallState>),
}
