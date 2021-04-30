// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_cobalt::{
    SoftwareDistributionInfo, SystemDataUpdaterMarker, SystemDataUpdaterProxy,
};
use fuchsia_component::client::connect_to_protocol;
use log::{error, info};
use omaha_client::common::AppSet;

pub async fn notify_cobalt_current_software_distribution(app_set: AppSet) {
    info!("Notifying Cobalt about the current channel and app id.");
    let proxy = match connect_to_protocol::<SystemDataUpdaterMarker>() {
        Ok(proxy) => proxy,
        Err(e) => {
            error!("Failed to connect to cobalt: {}", e);
            return;
        }
    };
    notify_cobalt_current_software_distribution_impl(proxy, app_set).await;
}

async fn notify_cobalt_current_software_distribution_impl(
    proxy: SystemDataUpdaterProxy,
    app_set: AppSet,
) {
    let channel = app_set.get_current_channel().await;
    let app_id = app_set.get_current_app_id().await;

    let distribution_info = SoftwareDistributionInfo {
        current_channel: Some(channel),
        current_realm: Some(app_id),
        ..SoftwareDistributionInfo::EMPTY
    };
    match proxy.set_software_distribution_info(distribution_info).await {
        Ok(fidl_fuchsia_cobalt::Status::Ok) => {}
        error => {
            error!("SystemDataUpdater.SetSoftwareDistributionInfo failed: {:?}", error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_cobalt::SystemDataUpdaterRequest;
    use fuchsia_async as fasync;
    use futures::prelude::*;
    use omaha_client::{common::App, protocol::Cohort};

    #[fasync::run_singlethreaded(test)]
    async fn test_notify_cobalt() {
        let app_set = AppSet::new(vec![App::builder("id", [1, 2])
            .with_cohort(Cohort { name: Some("current-channel".to_string()), ..Cohort::default() })
            .build()]);

        let (proxy, mut stream) = create_proxy_and_stream::<SystemDataUpdaterMarker>().unwrap();
        let stream_fut = async move {
            match stream.next().await {
                Some(Ok(SystemDataUpdaterRequest::SetSoftwareDistributionInfo {
                    info,
                    responder,
                })) => {
                    assert_eq!(info.current_channel.unwrap(), "current-channel");
                    assert_eq!(info.current_realm.unwrap(), "id");
                    responder.send(fidl_fuchsia_cobalt::Status::Ok).unwrap();
                }
                err => panic!("Err in request handler: {:?}", err),
            }
            assert!(stream.next().await.is_none());
        };
        future::join(notify_cobalt_current_software_distribution_impl(proxy, app_set), stream_fut)
            .await;
    }
}
