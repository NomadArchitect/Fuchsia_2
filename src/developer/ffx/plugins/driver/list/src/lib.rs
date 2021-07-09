// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Result},
    ffx_core::ffx_plugin,
    ffx_driver_list_args::DriverListCommand,
    fidl_fuchsia_driver_development::{BindRulesBytecode, DriverDevelopmentProxy},
    fuchsia_zircon_status as zx,
};

#[ffx_plugin(
    "driver_enabled",
    DriverDevelopmentProxy = "bootstrap/driver_manager:expose:fuchsia.driver.development.DriverDevelopment"
)]
pub async fn list(service: DriverDevelopmentProxy, cmd: DriverListCommand) -> Result<()> {
    let driver_info = service
        .get_driver_info(&mut [].iter().map(String::as_str))
        .await
        .map_err(|err| format_err!("FIDL call to get driver info failed: {}", err))?
        .map_err(|err| {
            format_err!(
                "FIDL call to get driver info returned an error: {}",
                zx::Status::from_raw(err)
            )
        })?;

    if cmd.verbose {
        for driver in driver_info {
            println!("{0: <10}: {1}", "Name", driver.name.unwrap_or("".to_string()));
            println!("{0: <10}: {1}", "Driver", driver.libname.unwrap_or("".to_string()));
            match driver.bind_rules {
                Some(BindRulesBytecode::BytecodeV1(bytecode)) => {
                    println!("{0: <10}: {1}", "Bytecode Version", 1);
                    println!("{0: <10}({1} bytes): {2:?}", "Bytecode:", bytecode.len(), bytecode);
                }
                Some(BindRulesBytecode::BytecodeV2(bytecode)) => {
                    println!("{0: <10}: {1}", "Bytecode Version", 2);
                    println!("{0: <10}({1} bytes): {2:?}", "Bytecode:", bytecode.len(), bytecode);
                }
                _ => println!("{0: <10}: {1}", "Bytecode Version", "Unknown"),
            }
            println!();
        }
    } else {
        for driver in driver_info {
            if let Some(libname) = driver.libname {
                println!("{}", libname);
            }
        }
    }
    Ok(())
}
