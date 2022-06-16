// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Result,
    errors::{ffx_bail, ffx_error},
    fidl::endpoints::create_proxy,
    fidl_fuchsia_developer_remotecontrol as rc, fidl_fuchsia_sys2 as fsys,
    fuchsia_url::AbsoluteComponentUrl,
    fuchsia_zircon_status::Status,
};

pub mod storage;

pub const SELECTOR_FORMAT_HELP: &str =
    "Selector format: <component moniker>:(in|out|exposed)[:<service name>].
Wildcards may be used anywhere in the selector.

Example: 'remote-control:out:*' would return all services in 'out' for
the component remote-control.

Note that moniker wildcards are not recursive: 'a/*/c' will only match
components named 'c' running in some sub-realm directly below 'a', and
no further.";

pub async fn connect_to_lifecycle_controller(
    rcs_proxy: &rc::RemoteControlProxy,
) -> Result<fsys::LifecycleControllerProxy> {
    let (lifecycle_controller, server_end) = create_proxy::<fsys::LifecycleControllerMarker>()?;
    rcs_proxy
        .root_lifecycle_controller(server_end)
        .await?
        .map_err(|i| ffx_error!("Could not open LifecycleController: {}", Status::from_raw(i)))?;
    Ok(lifecycle_controller)
}

/// Verifies that `url` can be parsed as a fuchsia-pkg CM URL
/// Returns the name of the component manifest, if the parsing was successful.
pub fn verify_fuchsia_pkg_cm_url(url: &str) -> Result<String> {
    let url = match AbsoluteComponentUrl::parse(url) {
        Ok(url) => url,
        Err(e) => ffx_bail!("URL parsing error: {:?}", e),
    };

    let manifest = url
        .resource()
        .split('/')
        .last()
        .ok_or(ffx_error!("Could not extract manifest filename from URL"))?;

    if let Some(name) = manifest.strip_suffix(".cm") {
        Ok(name.to_string())
    } else if manifest.ends_with(".cmx") {
        ffx_bail!(
            "{} is a legacy component manifest. Run it using `ffx component run-legacy`",
            manifest
        )
    } else {
        ffx_bail!(
            "{} is not a component manifest! Component manifests must end in the `cm` extension.",
            manifest
        )
    }
}
