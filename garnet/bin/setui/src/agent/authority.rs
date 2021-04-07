// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::{AgentError, BlueprintHandle, Context, Invocation, Lifespan, Payload};

use crate::base::SettingType;
use crate::message::base::{Audience, MessengerType};
use crate::monitor;
use crate::service;
use crate::service_context::ServiceContext;
use anyhow::{format_err, Error};
use std::collections::HashSet;
use std::sync::Arc;

/// Authority provides the ability to execute agents sequentially or simultaneously for a given
/// stage.
pub struct Authority {
    // A mapping of agent addresses
    agent_signatures: Vec<service::message::Signature>,
    // Factory passed to agents for communicating with the service.
    messenger_factory: service::message::Factory,
    // Messenger
    messenger: service::message::Messenger,
    // Available components
    available_components: HashSet<SettingType>,
    // Available resource monitors
    resource_monitor_actor: Option<monitor::environment::Actor>,
}

impl Authority {
    pub async fn create(
        messenger_factory: service::message::Factory,
        available_components: HashSet<SettingType>,
        resource_monitor_actor: Option<monitor::environment::Actor>,
    ) -> Result<Authority, Error> {
        let (client, _) = messenger_factory
            .create(MessengerType::Unbound)
            .await
            .map_err(|_| anyhow::format_err!("could not create agent messenger for authority"))?;

        return Ok(Authority {
            agent_signatures: Vec::new(),
            messenger_factory,
            messenger: client,
            available_components,
            resource_monitor_actor,
        });
    }

    pub async fn register(&mut self, blueprint: BlueprintHandle) {
        let agent_receptor = self
            .messenger_factory
            .create(MessengerType::Unbound)
            .await
            .expect("agent receptor should be created")
            .1;
        let signature = agent_receptor.get_signature();
        blueprint
            .create(
                Context::new(
                    agent_receptor,
                    self.messenger_factory.clone(),
                    self.available_components.clone(),
                    self.resource_monitor_actor.clone(),
                )
                .await,
            )
            .await;

        self.agent_signatures.push(signature);
    }

    /// Invokes each registered agent for a given lifespan. If sequential is true,
    /// invocations will only proceed to the next agent once the current
    /// invocation has been successfully acknowledged. When sequential is false,
    /// agents will receive their invocations without waiting. However, the
    /// overall completion (signaled through the receiver returned by the method),
    /// will not return until all invocations have been acknowledged.
    pub async fn execute_lifespan(
        &self,
        lifespan: Lifespan,
        service_context: Arc<ServiceContext>,
        sequential: bool,
    ) -> Result<(), Error> {
        let mut pending_receptors = Vec::new();

        for &signature in &self.agent_signatures {
            let mut receptor = self
                .messenger
                .message(
                    Payload::Invocation(Invocation {
                        lifespan: lifespan.clone(),
                        service_context: Arc::clone(&service_context),
                    })
                    .into(),
                    Audience::Messenger(signature),
                )
                .send();

            if sequential {
                let result = process_payload(receptor.next_payload().await);
                if result.is_err() {
                    return result;
                }
            } else {
                pending_receptors.push(receptor);
            }
        }

        // Pending acks should only be present for non sequential execution. In
        // this case wait for each to complete.
        for mut receptor in pending_receptors {
            let result = process_payload(receptor.next_payload().await);
            if result.is_err() {
                return result;
            }
        }

        Ok(())
    }
}

fn process_payload(
    payload: Result<(service::Payload, service::message::MessageClient), Error>,
) -> Result<(), Error> {
    match payload {
        Ok((service::Payload::Agent(Payload::Complete(Ok(_))), _)) => Ok(()),
        Ok((service::Payload::Agent(Payload::Complete(Err(AgentError::UnhandledLifespan))), _)) => {
            Ok(())
        }
        _ => Err(format_err!("invocation failed")),
    }
}
