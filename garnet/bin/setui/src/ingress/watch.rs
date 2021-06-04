// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Watch request handling.
//!
//! This mod defines the components for handling hanging-get, or "watch", [Requests](Request). These
//! requests return a value to the requestor when a value different from the previously returned /
//! value is available. This pattern is common across the various setting service interfaces.
//! Since there is context involved between watch requests, these job workloads are [Sequential].
//!
//! Users of these components define three implementations to create "watch"-related jobs. First,
//! implementations of [From<SettingInfo>] and [From<Error>] are needed. Since these requests will
//! always return a value on success, the request handling automatically converts the [SettingInfo].
//! The built-in conversion to the user type with the [From] trait implementation helps reduce the
//! explicit conversion in the responding code. Lastly, the user must implement [Responder], which
//! returns a [Result] converted from the [Response](crate::handler::base::Response) returned from
//! the setting service.

use crate::base::{SettingInfo, SettingType};
use crate::handler::base::Error;
use crate::handler::base::{Payload, Request};
use crate::job::data::{self, Data, Key};
use crate::job::work::{Load, Sequential};
use crate::job::Job;
use crate::job::Signature;
use crate::message::base::Audience;
use crate::service::{message, Address};
use async_trait::async_trait;
use std::collections::HashMap;
use std::marker::PhantomData;

/// The key used to store the last value sent. This cache is scoped to the
/// [Job's Signature](Signature).
const LAST_VALUE_KEY: &str = "LAST_VALUE";

/// [Responder] is a trait for handing back results of a watch request. It is unique from other
/// work responders, since [Work] consumers expect a value to be present on success. The Responder
/// specifies the conversions for [Response](crate::handler::base::Response).
pub trait Responder<
    R: From<SettingInfo> + Send + Sync + 'static,
    E: From<Error> + Send + Sync + 'static,
>
{
    fn respond(self, response: Result<R, E>);
}

pub struct Work<
    R: From<SettingInfo> + Send + Sync + 'static,
    E: From<Error> + Send + Sync + 'static,
    T: Responder<R, E> + Send + Sync + 'static,
> {
    setting_type: SettingType,
    signature: Signature,
    responder: T,
    _response_type: PhantomData<R>,
    _error_type: PhantomData<E>,
}

impl<
        R: From<SettingInfo> + Send + Sync + 'static,
        E: From<Error> + Send + Sync + 'static,
        T: Responder<R, E> + Send + Sync + 'static,
    > Work<R, E, T>
{
    pub fn new(setting_type: SettingType, signature: Signature, responder: T) -> Self {
        Self {
            setting_type,
            signature,
            responder,
            _response_type: PhantomData,
            _error_type: PhantomData,
        }
    }

    /// Returns a non-empty value when the last response should be returned to the caller. The lack
    /// of a response indicates the watched value has not changed and watching will continue.
    fn process_response(
        &self,
        response: Result<Payload, anyhow::Error>,
        store: &mut HashMap<Key, Data>,
    ) -> Option<Result<SettingInfo, Error>> {
        match response {
            Ok(Payload::Response(Ok(Some(setting_info)))) => {
                let key = Key::Identifier(LAST_VALUE_KEY);

                let return_val = match store.get(&key) {
                    Some(Data::SettingInfo(info)) if *info == setting_info => None,
                    _ => Some(Ok(setting_info)),
                };

                if let Some(Ok(ref info)) = return_val {
                    store.insert(key, Data::SettingInfo(info.clone()));
                }

                return_val
            }
            Ok(Payload::Response(Err(error))) => Some(Err(error)),
            Err(_) => Some(Err(crate::handler::base::Error::CommunicationError)),
            _ => {
                panic!("invalid variant {:?}", response);
            }
        }
    }
}

#[async_trait]
impl<
        R: From<SettingInfo> + Send + Sync + 'static,
        E: From<Error> + Send + Sync + 'static,
        T: Responder<R, E> + Send + Sync + 'static,
    > Sequential for Work<R, E, T>
{
    async fn execute(
        self: Box<Self>,
        messenger: message::Messenger,
        store_handle: data::StoreHandle,
    ) {
        // Lock store for Job signature group.
        let mut store = store_handle.lock().await;

        // Begin listening for changes before fetching current value to ensure no changes are
        // missed.
        let mut listen_receptor = messenger
            .message(
                Payload::Request(Request::Listen).into(),
                Audience::Address(Address::Handler(self.setting_type)),
            )
            .send();

        // Get current value.
        let mut get_receptor = messenger
            .message(
                Payload::Request(Request::Get).into(),
                Audience::Address(Address::Handler(self.setting_type)),
            )
            .send();

        // If a value was returned from the get call and considered updated (no existing or
        // different), return new value immediately.
        if let Some(response) = self.process_response(
            get_receptor.next_of::<Payload>().await.map(|(payload, _)| payload),
            &mut store,
        ) {
            self.responder.respond(response.map(R::from).map_err(E::from));
            return;
        }

        // Otherwise, loop a watch until an updated value is available
        loop {
            if let Some(response) = self.process_response(
                listen_receptor.next_of::<Payload>().await.map(|(payload, _)| payload),
                &mut store,
            ) {
                self.responder.respond(response.map(R::from).map_err(E::from));
                return;
            }
        }
    }
}

impl<
        R: From<SettingInfo> + Send + Sync + 'static,
        E: From<Error> + Send + Sync + 'static,
        T: Responder<R, E> + Send + Sync + 'static,
    > From<Work<R, E, T>> for Job
{
    fn from(work: Work<R, E, T>) -> Job {
        let signature = work.signature;
        Job::new(Load::Sequential(Box::new(work), signature))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{SettingInfo, UnknownInfo};
    use crate::message::base::MessengerType;
    use crate::service::{message, Address};
    use fuchsia_async as fasync;
    use futures::channel::oneshot::Sender;
    use futures::lock::Mutex;
    use matches::assert_matches;
    use std::sync::Arc;

    struct TestResponder {
        sender: Sender<Result<SettingInfo, Error>>,
    }

    impl TestResponder {
        pub fn new(sender: Sender<Result<SettingInfo, Error>>) -> Self {
            Self { sender }
        }
    }

    impl Responder<SettingInfo, Error> for TestResponder {
        fn respond(self, response: Result<SettingInfo, Error>) {
            self.sender.send(response).expect("send should succeed");
        }
    }

    #[fuchsia_async::run_until_stalled(test)]
    async fn test_watch_basic_functionality() {
        // Create store for job.
        let store_handle = Arc::new(Mutex::new(HashMap::new()));

        let get_info = SettingInfo::Unknown(UnknownInfo(true));
        let listen_info = SettingInfo::Unknown(UnknownInfo(false));

        // Make sure the first job execution returns the initial value (retrieved through get).
        verify_watch(store_handle.clone(), listen_info.clone(), get_info.clone(), get_info.clone())
            .await;
        // Make sure the second job execution returns the value returned through watching (listen
        // value).
        verify_watch(
            store_handle.clone(),
            listen_info.clone(),
            get_info.clone(),
            listen_info.clone(),
        )
        .await;
    }

    async fn verify_watch(
        store_handle: data::StoreHandle,
        listen_info: SettingInfo,
        get_info: SettingInfo,
        expected_info: SettingInfo,
    ) {
        // Create MessageHub for communication between components.
        let message_hub_delegate = message::create_hub();

        // Create mock handler endpoint to receive request.
        let mut handler_receiver = message_hub_delegate
            .create(MessengerType::Addressable(Address::Handler(SettingType::Unknown)))
            .await
            .expect("handler messenger should be created")
            .1;

        let (response_tx, response_rx) =
            futures::channel::oneshot::channel::<Result<SettingInfo, Error>>();

        let work = Box::new(Work::new(
            SettingType::Unknown,
            Signature::new(0),
            TestResponder::new(response_tx),
        ));

        // Execute work on async task.
        let work_messenger = message_hub_delegate
            .create(MessengerType::Unbound)
            .await
            .expect("messenger should be created")
            .0;

        let work_messenger_signature = work_messenger.get_signature();
        fasync::Task::spawn(async move {
            work.execute(work_messenger, store_handle).await;
        })
        .detach();

        // Ensure the listen request is received from the right sender.
        let (listen_request, listen_client) = handler_receiver
            .next_of::<Payload>()
            .await
            .expect("should successfully receive a listen request");
        assert_matches!(listen_request, Payload::Request(Request::Listen));
        assert!(listen_client.get_author() == work_messenger_signature);

        // Listen should be followed by a get request.
        let (get_request, get_client) = handler_receiver
            .next_of::<Payload>()
            .await
            .expect("should successfully receive a get request");
        assert_matches!(get_request, Payload::Request(Request::Get));
        assert!(get_client.get_author() == work_messenger_signature);

        // Reply to the get request.
        let _ = get_client.reply(Payload::Response(Ok(Some(get_info))).into()).send();
        let _ = listen_client.reply(Payload::Response(Ok(Some(listen_info))).into()).send();

        assert_matches!(response_rx.await.expect("should receive successful response"),
                Ok(x) if x == expected_info);
    }

    #[fuchsia_async::run_until_stalled(test)]
    async fn test_error_propagation() {
        // Create MessageHub for communication between components.
        let message_hub_delegate = message::create_hub();

        let (response_tx, response_rx) =
            futures::channel::oneshot::channel::<Result<SettingInfo, Error>>();

        // Create a listen request to a non-existent end-point.
        let work = Box::new(Work::new(
            SettingType::Unknown,
            Signature::new(0),
            TestResponder::new(response_tx),
        ));

        let work_messenger = message_hub_delegate
            .create(MessengerType::Unbound)
            .await
            .expect("messenger should be created")
            .0;

        // Execute work on async task.
        fasync::Task::spawn(async move {
            work.execute(work_messenger, Arc::new(Mutex::new(HashMap::new()))).await;
        })
        .detach();

        // Ensure an error is returned by the executed work.
        assert_matches!(response_rx.await.expect("should receive successful response"),
                Err(x) if x == crate::handler::base::Error::CommunicationError);
    }
}
