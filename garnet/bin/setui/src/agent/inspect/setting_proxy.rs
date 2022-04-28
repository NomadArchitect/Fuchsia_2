// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The inspect mod defines the [SettingProxyInspectAgent], which is responsible for logging
//! relevant service activity to Inspect. Since this activity might happen
//! before agent lifecycle states are communicated (due to agent priority
//! ordering), the [SettingProxyInspectAgent] begins listening to requests immediately
//! after creation.
//!
//! [SettingProxyInspectAgent]: inspect::SettingProxyInspectAgent

use crate::agent::storage::device_storage::DeviceStorageAccess;
use crate::agent::Context;
use crate::agent::Payload;
use crate::base::{SettingInfo, SettingType};
use crate::blueprint_definition;
use crate::clock;
use crate::handler::base::{Error, Payload as HandlerPayload, Request};
use crate::inspect::utils::inspect_map::InspectMap;
use crate::inspect::utils::inspect_queue::InspectQueue;
use crate::message::base::{filter, MessageEvent, MessengerType};
use crate::message::receptor::Receptor;
use crate::service::TryFromWithClient;
use crate::{service, trace};

use fuchsia_async as fasync;
use fuchsia_inspect::{self as inspect, component, Property};
use fuchsia_inspect_derive::{Inspect, WithInspect};
use futures::stream::FuturesUnordered;
use futures::StreamExt;

use std::sync::Arc;

blueprint_definition!(
    "setting_proxy",
    crate::agent::inspect::setting_proxy::SettingProxyInspectAgent::create
);

const INSPECT_REQUESTS_COUNT: usize = 25;

/// Information about a setting to be written to inspect.
#[derive(Default, Inspect)]
struct SettingTypeInspectInfo {
    /// Map from the name of the Request variant to a RequestInspectInfo that holds a list of
    /// recent requests.
    requests_by_type: InspectMap<InspectQueue<RequestInspectInfo>>,

    /// Incrementing count for all requests of this setting type.
    ///
    /// Count is used across all request types to easily see the order that requests occurred in.
    #[inspect(skip)]
    count: u64,

    /// Node of this info.
    inspect_node: inspect::Node,
}

impl SettingTypeInspectInfo {
    fn new(node: &inspect::Node, key: &str) -> Self {
        Self::default()
            .with_inspect(node, key)
            .expect("Failed to create SettingTypeInspectInfo node")
    }
}

/// Information about a request to be written to inspect.
#[derive(Default, Inspect)]
struct RequestInspectInfo {
    /// Debug string representation of this Request.
    request: inspect::StringProperty,

    /// Milliseconds since creation that this request arrived.
    request_timestamp: inspect::StringProperty,

    /// Debug string representation of a corresponding Response.
    response: inspect::StringProperty,

    /// Milliseconds since creation that this response is recorded to the inspect.
    response_timestamp: inspect::StringProperty,

    /// Node of this info.
    inspect_node: inspect::Node,

    /// A string that links request and response. The value is the request_timestamp.
    #[inspect(skip)]
    link_str_request_timestamp: String,
}

impl RequestInspectInfo {
    fn new(request: String, timestamp: String, node: &inspect::Node, key: &str) -> Self {
        let mut info = Self::default()
            .with_inspect(node, key)
            // `with_inspect` will only return an error on types with
            // interior mutability. Since none are used here, this should be
            // fine.
            .expect("failed to create RequestInspectInfo inspect node");
        info.request.set(&request);
        info.request_timestamp.set(&timestamp);
        info.link_str_request_timestamp = timestamp;
        info
    }
}

/// Store linking information about a response and its request.
struct RequestResponseLinkInfo {
    /// The setting type of a request.
    setting_type: SettingType,

    /// The Request variant for inspect.
    key: String,

    /// The linking string to link response and its request using request timestamp.
    link_str_request_timestamp: String,
}

/// The SettingProxyInspectAgent is responsible for listening to requests to the setting
/// handlers and recording the requests to Inspect.
pub(crate) struct SettingProxyInspectAgent {
    inspect_node: inspect::Node,
    /// Last requests for inspect to save.
    last_requests: InspectMap<SettingTypeInspectInfo>,
}

impl DeviceStorageAccess for SettingProxyInspectAgent {
    const STORAGE_KEYS: &'static [&'static str] = &[];
}

impl SettingProxyInspectAgent {
    async fn create(context: Context) {
        // TODO(fxbug.dev/71295): Rename child node as switchboard is no longer in use.
        Self::create_with_node(context, component::inspector().root().create_child("switchboard"))
            .await;
    }

    async fn create_with_node(context: Context, node: inspect::Node) {
        let (_, message_rx) = context
            .delegate
            .create(MessengerType::Broker(Some(filter::Builder::single(
                filter::Condition::Custom(Arc::new(move |message| {
                    // Only catch setting handler requests.
                    matches!(
                        message.payload(),
                        service::Payload::Setting(HandlerPayload::Request(_))
                    )
                })),
            ))))
            .await
            .expect("should receive client");

        let mut agent = SettingProxyInspectAgent {
            inspect_node: node,
            last_requests: InspectMap::<SettingTypeInspectInfo>::new(),
        };

        fasync::Task::spawn({
            async move {
            let nonce = fuchsia_trace::generate_nonce();
            trace!(nonce, "setting_proxy_inspect_agent");
            let event = message_rx.fuse();
            let agent_event = context.receptor.fuse();
            futures::pin_mut!(agent_event, event);

            // Push reply_receptor to the FutureUnordered to avoid blocking codes when there are no
            // responses replied back.
            let mut unordered = FuturesUnordered::new();
            loop {
                futures::select! {
                    message_event = event.select_next_some() => {
                        trace!(
                            nonce,
                            "message_event"
                        );
                        if let Some((link_info, mut reply_receptor)) =
                            agent.process_message_event(message_event) {
                                unordered.push(async move {
                                    let payload = reply_receptor.next_payload().await;
                                    (link_info, payload)
                                });
                        };
                    },
                    reply = unordered.select_next_some() => {
                        let (link_info, payload) = reply;
                        if let Ok((
                            service::Payload::Setting(
                                HandlerPayload::Response(response)),
                            _,
                        )) = payload
                        {
                            agent.record_response(link_info, response);
                        }
                    },
                    agent_message = agent_event.select_next_some() => {
                        trace!(
                            nonce,
                            "agent_event"
                        );
                        if let MessageEvent::Message(
                                service::Payload::Agent(Payload::Invocation(_invocation)), client)
                                = agent_message {
                            // Since the agent runs at creation, there is no
                            // need to handle state here.
                            client.reply(Payload::Complete(Ok(())).into()).send().ack();
                        }
                    },
                }
            }
        }})
        .detach();
    }

    /// Identfies [`service::message::MessageEvent`] that contains a [`Request`]
    /// for setting handlers and records the [`Request`].
    fn process_message_event(
        &mut self,
        event: service::message::MessageEvent,
    ) -> Option<(
        RequestResponseLinkInfo,
        Receptor<service::Payload, service::Address, service::Role>,
    )> {
        if let Ok((HandlerPayload::Request(request), mut client)) =
            HandlerPayload::try_from_with_client(event)
        {
            for target in client.get_audience().flatten() {
                if let service::message::Audience::Address(service::Address::Handler(
                    setting_type,
                )) = target
                {
                    let link_str_request_timestamp = self.record_request(setting_type, &request);
                    // A Listen request will always send a Get request. We can always get the Get's
                    // response. However, Listen will return the Get's response only when it is
                    // considered updated. Therefore, we ignore Listen response.
                    if request != Request::Listen {
                        let response_info = RequestResponseLinkInfo {
                            setting_type,
                            key: request.for_inspect().to_string(),
                            link_str_request_timestamp,
                        };
                        return Some((response_info, client.spawn_observer()));
                    }
                }
            }
        }
        None
    }

    /// Write a request to inspect.
    fn record_request(&mut self, setting_type: SettingType, request: &Request) -> String {
        let inspect_node = &self.inspect_node;
        let setting_type_info =
            self.last_requests.get_or_insert(format!("{:?}", setting_type), || {
                SettingTypeInspectInfo::new(inspect_node, &format!("{:?}", setting_type))
            });

        let key = request.for_inspect();
        let inspect_queue_node = &setting_type_info.inspect_node;
        let inspect_queue =
            setting_type_info.requests_by_type.get_or_insert(key.to_string(), || {
                InspectQueue::<RequestInspectInfo>::new(INSPECT_REQUESTS_COUNT)
                    .with_inspect(inspect_queue_node, key)
                    // `with_inspect` will only return an error on types with
                    // interior mutability. Since none are used here, this should be
                    // fine.
                    .expect("failed to create InspectQueue inspect node")
            });

        let count = setting_type_info.count;
        setting_type_info.count += 1;

        let timestamp = clock::inspect_format_now();
        inspect_queue.push(RequestInspectInfo::new(
            format!("{:?}", request),
            timestamp.clone(),
            &inspect_queue.inspect_node,
            &format!("{:020}", count),
        ));

        timestamp
    }

    /// Write a response to inspect.
    fn record_response(
        &mut self,
        link_info: RequestResponseLinkInfo,
        response: Result<Option<SettingInfo>, Error>,
    ) {
        let setting_type_info = self
            .last_requests
            .get_mut(&format!("{:?}", &link_info.setting_type))
            .expect("Should find a SettyingTypeInspectInfo node to record the response.");
        let request_info_queue = setting_type_info
            .requests_by_type
            .get_mut(&link_info.key)
            .expect("Should find a RequestInspectInfo to record the response.");

        let last_requests = &mut request_info_queue.items;
        // The match should be the last item in the queue if a response reply back immediately.
        for request in last_requests.iter().rev() {
            if request.link_str_request_timestamp == link_info.link_str_request_timestamp {
                request.response.set(&format!("{:?}", &response));
                request.response_timestamp.set(&clock::inspect_format_now());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::types::SetDisplayInfo;
    use crate::intl::types::{IntlInfo, LocaleId, TemperatureUnit};
    use crate::message::MessageHubUtil;
    use crate::service;

    use fuchsia_inspect::assert_data_tree;
    use fuchsia_inspect::testing::{AnyProperty, TreeAssertion};
    use fuchsia_zircon::Time;
    use std::collections::HashSet;

    /// The `RequestProcessor` handles sending a request through a MessageHub
    /// From caller to recipient. This is useful when testing brokers in
    /// between.
    struct RequestProcessor {
        delegate: service::message::Delegate,
    }

    impl RequestProcessor {
        fn new(delegate: service::message::Delegate) -> Self {
            RequestProcessor { delegate }
        }

        async fn send_and_receive(&self, setting_type: SettingType, setting_request: Request) {
            let (messenger, _) =
                self.delegate.create(MessengerType::Unbound).await.expect("should be created");

            let (_, mut receptor) = self
                .delegate
                .create(MessengerType::Addressable(service::Address::Handler(setting_type)))
                .await
                .expect("should be created");

            let mut reply_receptor = messenger
                .message(
                    HandlerPayload::Request(setting_request).into(),
                    service::message::Audience::Address(service::Address::Handler(setting_type)),
                )
                .send();

            if let Some(message_event) = futures::StreamExt::next(&mut receptor).await {
                if let Ok((_, reply_client)) = HandlerPayload::try_from_with_client(message_event) {
                    reply_client.reply(HandlerPayload::Response(Ok(None)).into()).send().ack();
                }
            }
            let _ = reply_receptor.next_payload().await;
        }
    }

    async fn create_context() -> Context {
        Context::new(
            service::MessageHub::create_hub()
                .create(MessengerType::Unbound)
                .await
                .expect("should be present")
                .1,
            service::MessageHub::create_hub(),
            HashSet::new(),
            HashSet::new(),
            None,
        )
        .await
    }

    #[fuchsia_async::run_until_stalled(test)]
    async fn test_inspect() {
        // Set the clock so that timestamps can be controlled.
        clock::mock::set(Time::from_nanos(0));

        let inspector = inspect::Inspector::new();
        let inspect_node = inspector.root().create_child("switchboard");
        let context = create_context().await;

        let request_processor = RequestProcessor::new(context.delegate.clone());

        SettingProxyInspectAgent::create_with_node(context, inspect_node).await;

        // Send a few requests to make sure they get written to inspect properly.
        let turn_off_auto_brightness = Request::SetDisplayInfo(SetDisplayInfo {
            auto_brightness: Some(false),
            ..SetDisplayInfo::default()
        });
        request_processor
            .send_and_receive(SettingType::Display, turn_off_auto_brightness.clone())
            .await;

        // Set to a different time so that a response can correctly link to its request.
        clock::mock::set(Time::from_nanos(100));
        request_processor.send_and_receive(SettingType::Display, turn_off_auto_brightness).await;

        // Set to a different time so that a response can correctly link to its request.
        clock::mock::set(Time::from_nanos(200));
        request_processor
            .send_and_receive(
                SettingType::Intl,
                Request::SetIntlInfo(IntlInfo {
                    locales: Some(vec![LocaleId { id: "en-US".to_string() }]),
                    temperature_unit: Some(TemperatureUnit::Celsius),
                    time_zone_id: Some("UTC".to_string()),
                    hour_cycle: None,
                }),
            )
            .await;

        assert_data_tree!(inspector, root: {
            switchboard: {
                "Display": {
                    "SetDisplayInfo": {
                        "00000000000000000000": {
                            request: "SetDisplayInfo(SetDisplayInfo { \
                                manual_brightness_value: None, \
                                auto_brightness_value: None, \
                                auto_brightness: Some(false), \
                                screen_enabled: None, \
                                low_light_mode: None, \
                                theme: None \
                            })",
                            request_timestamp: "0.000000000",
                            response: "Ok(None)",
                            response_timestamp: "0.000000000",
                        },
                        "00000000000000000001": {
                            request: "SetDisplayInfo(SetDisplayInfo { \
                                manual_brightness_value: None, \
                                auto_brightness_value: None, \
                                auto_brightness: Some(false), \
                                screen_enabled: None, \
                                low_light_mode: None, \
                                theme: None \
                            })",
                            request_timestamp: "0.000000100",
                            response: "Ok(None)",
                            response_timestamp: "0.000000100",
                        },
                    },
                },
                "Intl": {
                    "SetIntlInfo": {
                        "00000000000000000000": {
                            request: "SetIntlInfo(IntlInfo { \
                                locales: Some([LocaleId { id: \"en-US\" }]), \
                                temperature_unit: Some(Celsius), \
                                time_zone_id: Some(\"UTC\"), \
                                hour_cycle: None })",
                            request_timestamp: "0.000000200",
                            response: "Ok(None)",
                            response_timestamp: "0.000000200",
                        }
                    },
                }
            }
        });
    }

    #[fuchsia_async::run_until_stalled(test)]
    async fn test_inspect_mixed_request_types() {
        // Set the clock so that timestamps can be controlled.
        clock::mock::set(Time::from_nanos(0));

        let inspector = inspect::Inspector::new();
        let inspect_node = inspector.root().create_child("switchboard");
        let context = create_context().await;

        let request_processor = RequestProcessor::new(context.delegate.clone());

        let _agent = SettingProxyInspectAgent::create_with_node(context, inspect_node).await;

        // Interlace different request types to make sure the counter is correct.
        request_processor
            .send_and_receive(
                SettingType::Display,
                Request::SetDisplayInfo(SetDisplayInfo {
                    auto_brightness: Some(false),
                    ..SetDisplayInfo::default()
                }),
            )
            .await;

        // Set to a different time so that a response can correctly link to its request.
        clock::mock::set(Time::from_nanos(100));
        request_processor.send_and_receive(SettingType::Display, Request::Get).await;

        // Set to a different time so that a response can correctly link to its request.
        clock::mock::set(Time::from_nanos(200));
        request_processor
            .send_and_receive(
                SettingType::Display,
                Request::SetDisplayInfo(SetDisplayInfo {
                    auto_brightness: Some(true),
                    ..SetDisplayInfo::default()
                }),
            )
            .await;

        clock::mock::set(Time::from_nanos(300));
        request_processor.send_and_receive(SettingType::Display, Request::Get).await;

        assert_data_tree!(inspector, root: {
            switchboard: {
                "Display": {
                    "SetDisplayInfo": {
                        "00000000000000000000": {
                            request: "SetDisplayInfo(SetDisplayInfo { \
                                manual_brightness_value: None, \
                                auto_brightness_value: None, \
                                auto_brightness: Some(false), \
                                screen_enabled: None, \
                                low_light_mode: None, \
                                theme: None \
                            })",
                            request_timestamp: "0.000000000",
                            response: "Ok(None)",
                            response_timestamp: "0.000000000",
                        },
                        "00000000000000000002": {
                            request: "SetDisplayInfo(SetDisplayInfo { \
                                manual_brightness_value: None, \
                                auto_brightness_value: None, \
                                auto_brightness: Some(true), \
                                screen_enabled: None, \
                                low_light_mode: None, \
                                theme: None \
                            })",
                            request_timestamp: "0.000000200",
                            response: "Ok(None)",
                            response_timestamp: "0.000000200",
                        },
                    },
                    "Get": {
                        "00000000000000000001": {
                            request: "Get",
                            request_timestamp: "0.000000100",
                            response: "Ok(None)",
                            response_timestamp: "0.000000100",
                        },
                        "00000000000000000003": {
                            request: "Get",
                            request_timestamp: "0.000000300",
                            response: "Ok(None)",
                            response_timestamp: "0.000000300",
                        },
                    },
                },
            }
        });
    }

    #[fuchsia_async::run_until_stalled(test)]
    async fn inspect_queue_test() {
        // Set the clock so that timestamps will always be 0.
        clock::mock::set(Time::from_nanos(0));
        let inspector = inspect::Inspector::new();
        let inspect_node = inspector.root().create_child("switchboard");
        let context = create_context().await;
        let request_processor = RequestProcessor::new(context.delegate.clone());

        let _agent = SettingProxyInspectAgent::create_with_node(context, inspect_node).await;

        request_processor
            .send_and_receive(
                SettingType::Intl,
                Request::SetIntlInfo(IntlInfo {
                    locales: Some(vec![LocaleId { id: "en-US".to_string() }]),
                    temperature_unit: Some(TemperatureUnit::Celsius),
                    time_zone_id: Some("UTC".to_string()),
                    hour_cycle: None,
                }),
            )
            .await;

        // Send one more than the max requests to make sure they get pushed off the end of the queue
        for _ in 0..INSPECT_REQUESTS_COUNT + 1 {
            request_processor
                .send_and_receive(
                    SettingType::Display,
                    Request::SetDisplayInfo(SetDisplayInfo {
                        auto_brightness: Some(false),
                        ..SetDisplayInfo::default()
                    }),
                )
                .await;
        }

        // Ensures we have INSPECT_REQUESTS_COUNT items and that the queue dropped the earliest one
        // when hitting the limit.
        fn display_subtree_assertion() -> TreeAssertion {
            let mut tree_assertion = TreeAssertion::new("Display", true);
            let mut request_assertion = TreeAssertion::new("SetDisplayInfo", true);

            for i in 1..INSPECT_REQUESTS_COUNT + 1 {
                // We don't need to set clock here since we don't do exact match.
                request_assertion
                    .add_child_assertion(TreeAssertion::new(&format!("{:020}", i), false));
            }
            tree_assertion.add_child_assertion(request_assertion);
            tree_assertion
        }

        assert_data_tree!(inspector, root: {
            switchboard: {
                display_subtree_assertion(),
                "Intl": {
                    "SetIntlInfo": {
                        "00000000000000000000": {
                            request: AnyProperty,
                            request_timestamp: "0.000000000",
                            response: "Ok(None)",
                            response_timestamp: "0.000000000",
                        }
                    }
                }
            }
        });
    }
}
