// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::netstack::types::NetstackMethod;
use crate::server::Facade;
use anyhow::Error;
use async_trait::async_trait;
use serde_json::{to_value, Value};

use crate::common_utils::common::parse_u64_identifier;
use crate::netstack::facade::NetstackFacade;

#[async_trait(?Send)]
impl Facade for NetstackFacade {
    async fn handle_request(&self, method: String, args: Value) -> Result<Value, Error> {
        match NetstackMethod::from_str(&method) {
            NetstackMethod::InitNetstack => Ok(to_value(())?),
            NetstackMethod::ListInterfaces => {
                let result = self.list_interfaces().await?;
                Ok(to_value(result)?)
            }
            NetstackMethod::GetIpv6Addresses => {
                let result = self.get_ipv6_addresses().await?;
                Ok(to_value(result)?)
            }
            NetstackMethod::GetLinkLocalIpv6Addresses => {
                let result = self.get_link_local_ipv6_addresses().await?;
                Ok(to_value(result)?)
            }
            NetstackMethod::EnableInterface => {
                let identifier = parse_u64_identifier(args)?;
                let result = self.enable_interface(identifier).await?;
                Ok(to_value(result)?)
            }
            NetstackMethod::DisableInterface => {
                let identifier = parse_u64_identifier(args)?;
                let result = self.disable_interface(identifier).await?;
                Ok(to_value(result)?)
            }
            _ => return Err(format_err!("Invalid Netstack FIDL method: {:?}", method)),
        }
    }
}
