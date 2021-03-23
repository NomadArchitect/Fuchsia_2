// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::base::SettingType;
use crate::handler::base::{
    Context, Environment, GenerateHandler, SettingHandlerFactory, SettingHandlerFactoryError,
};
use crate::handler::setting_handler::{Command, Payload, State};
use crate::message::base::{Audience, MessageEvent, MessengerType};
use crate::service;
use crate::service::message::{Factory, Signature};
use crate::service_context::ServiceContextHandle;
use async_trait::async_trait;
use fuchsia_syslog::fx_log_err;
use futures::StreamExt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// SettingHandlerFactoryImpl houses registered closures for generating setting
/// handlers.
pub struct SettingHandlerFactoryImpl {
    environment: Environment,
    generators: HashMap<SettingType, GenerateHandler>,

    /// Atomic counter used to generate new IDs, which uniquely identify a context.
    context_id_counter: Arc<AtomicU64>,
}

#[async_trait]
impl SettingHandlerFactory for SettingHandlerFactoryImpl {
    async fn generate(
        &mut self,
        setting_type: SettingType,
        messenger_factory: Factory,
        notifier_signature: Signature,
    ) -> Result<Signature, SettingHandlerFactoryError> {
        if !self.environment.settings.contains(&setting_type) {
            return Err(SettingHandlerFactoryError::SettingNotFound(setting_type));
        }

        let (messenger, receptor) = messenger_factory
            .create(MessengerType::Unbound)
            .await
            .map_err(|_| SettingHandlerFactoryError::HandlerMessengerError)?;
        let signature = receptor.get_signature();

        let generate_function = self
            .generators
            .get(&setting_type)
            .ok_or(SettingHandlerFactoryError::GeneratorNotFound(setting_type))?;

        (generate_function)(Context::new(
            setting_type,
            messenger,
            receptor,
            notifier_signature,
            self.environment.clone(),
            self.context_id_counter.fetch_add(1, Ordering::Relaxed),
        ))
        .await
        .map_err(|_| SettingHandlerFactoryError::HandlerStartupError(setting_type))?;

        let (controller_messenger, _) = messenger_factory
            .create(MessengerType::Unbound)
            .await
            .map_err(|_| SettingHandlerFactoryError::ControllerMessengerError)?;

        // At this point, we know the controller was constructed successfully.
        // Tell the controller to run the Startup phase to initialize its state.
        let mut controller_receptor = controller_messenger
            .message(
                Payload::Command(Command::ChangeState(State::Startup)).into(),
                Audience::Messenger(signature),
            )
            .send();

        // Wait for the startup phase to be over before continuing.
        while let Some(message_event) = controller_receptor.next().await {
            match message_event {
                MessageEvent::Status(_) => {} // no-op
                MessageEvent::Message(service::Payload::Controller(Payload::Result(result)), _) => {
                    // Startup phase is complete. If it had no errors the proxy can assume it
                    // has an active controller with create() and startup() already run on it
                    // before handling its request.
                    return result.map(|_| signature).map_err(|_| {
                        SettingHandlerFactoryError::HandlerStartupError(setting_type)
                    });
                }
                _ => {
                    fx_log_err!(
                        "Unexpected message response {:?} for {:?} controller startup request",
                        message_event,
                        setting_type
                    );
                    return Err(SettingHandlerFactoryError::HandlerStartupError(setting_type));
                }
            }
        }

        panic!("Did not get any responses from {:?} controller startup", setting_type);
    }
}

impl SettingHandlerFactoryImpl {
    pub fn new(
        settings: HashSet<SettingType>,
        service_context_handle: ServiceContextHandle,
        context_id_counter: Arc<AtomicU64>,
    ) -> SettingHandlerFactoryImpl {
        SettingHandlerFactoryImpl {
            environment: Environment::new(settings, service_context_handle),
            generators: HashMap::new(),
            context_id_counter,
        }
    }

    pub fn register(&mut self, setting_type: SettingType, generate_function: GenerateHandler) {
        self.generators.insert(setting_type, generate_function);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::base::Request;
    use crate::handler::setting_handler::controller::{Create, Handle};
    use crate::handler::setting_handler::{
        BoxedController, ClientImpl, ClientProxy, ControllerError, ControllerStateResult,
        SettingHandlerResult,
    };
    use crate::message::base::{filter, Message, MessageType};
    use crate::service;
    use crate::service_context::ServiceContext;
    use fuchsia_async as fasync;
    use futures::future::FutureExt;
    use futures::select;
    use std::convert::TryFrom;

    /// Test controller used to test startup waiting behavior in SettingHandlerFactoryImpl.
    struct TestController;

    #[async_trait]
    impl Create for TestController {
        async fn create(_client: ClientProxy) -> Result<Self, ControllerError> {
            Ok(Self)
        }
    }

    #[async_trait]
    impl Handle for TestController {
        // Not relevant.
        async fn handle(&self, _request: Request) -> Option<SettingHandlerResult> {
            None
        }

        // When we see a startup message, send a signal to signify we're done.
        async fn change_state(&mut self, state: State) -> Option<ControllerStateResult> {
            match state {
                State::Startup => Some(Ok(())),
                _ => None,
            }
        }
    }

    #[fasync::run_until_stalled(test)]
    async fn ensure_startup_is_awaited() {
        let messenger_factory = service::message::create_hub();
        let mut factory_impl = SettingHandlerFactoryImpl::new(
            {
                let mut settings = HashSet::new();
                settings.insert(SettingType::Unknown);
                settings
            },
            ServiceContext::create(None, Some(messenger_factory.clone())),
            Arc::new(AtomicU64::new(0)),
        );

        // Register generation of controller with factory_impl.
        let generate_handler: GenerateHandler = Box::new(move |context| {
            Box::pin(ClientImpl::create(
                context,
                Box::new(move |proxy| {
                    Box::pin(async move {
                        Ok(Box::new(TestController::create(proxy).await.unwrap())
                            as BoxedController)
                    })
                }),
            ))
        });
        factory_impl.register(SettingType::Unknown, generate_handler);

        // Create a broker that only listens to replies.
        let (_, broker_receptor) = messenger_factory
            .create(MessengerType::Broker(Some(filter::Builder::single(
                filter::Condition::Custom(Arc::new(move |message: &Message<_, _, _>| {
                    // Only filter for reply's that contain results.
                    matches!(message.get_type(), MessageType::Reply(_))
                        && Payload::try_from(message.payload())
                            .map_or(false, |payload| matches!(payload, Payload::Result(_)))
                })),
            ))))
            .await
            .expect("could not create broker receptor");

        let (_, receptor) = messenger_factory
            .create(MessengerType::Unbound)
            .await
            .expect("messenger should be created");
        // Generate the controller, but don't await it yet so we can time it with the response
        // from the broker.
        let mut generate_future = factory_impl
            .generate(SettingType::Unknown, messenger_factory, receptor.get_signature())
            .into_stream()
            .fuse();
        let mut broker_receptor = broker_receptor.fuse();

        // We need to validate that the generate_done always completes after the startup_done
        // signal. If this ever fails, it implies that the generator does not properly wait for
        // startup to complete.
        let mut generate_done = None;
        let mut startup_done = None;
        let mut idx: u8 = 0;
        while generate_done.is_none() || startup_done.is_none() {
            select! {
                result = generate_future.select_next_some() => {
                    result.expect("should have received a signature");
                    generate_done = Some(idx);
                    idx += 1;
                }
                maybe = broker_receptor.next() => {
                    maybe.expect("should have gotten a reply");
                    startup_done = Some(idx);
                    idx += 1;
                }
                complete => break,
            }
        }

        // Validate that the id for generate is larger than the one for startup,
        // implying that generate finished after startup was done.
        assert!(generate_done.unwrap() > startup_done.unwrap());
    }
}
