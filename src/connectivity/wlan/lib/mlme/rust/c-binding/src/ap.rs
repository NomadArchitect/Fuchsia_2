// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    banjo_fuchsia_hardware_wlan_mac as banjo_wlan_mac, fidl_fuchsia_wlan_mlme as fidl_mlme,
    fuchsia_zircon as zx,
    ieee80211::Bssid,
    log::error,
    wlan_mlme::{ap::Ap, buffer::BufferProvider, device::Device, error::ResultExt, timer::*},
    wlan_span::CSpan,
};

#[no_mangle]
pub extern "C" fn ap_sta_new(
    device: Device,
    buf_provider: BufferProvider,
    scheduler: Scheduler,
    bssid: &[u8; 6],
) -> *mut Ap {
    Box::into_raw(Box::new(Ap::new(device, buf_provider, scheduler, Bssid(*bssid))))
}

#[no_mangle]
pub extern "C" fn ap_sta_delete(sta: *mut Ap) {
    if !sta.is_null() {
        unsafe { Box::from_raw(sta) };
    }
}

#[no_mangle]
pub extern "C" fn ap_sta_timeout_fired(sta: &mut Ap, event_id: EventId) {
    sta.handle_timed_event(event_id);
}

#[no_mangle]
pub extern "C" fn ap_sta_handle_mlme_msg(sta: &mut Ap, bytes: CSpan<'_>) -> i32 {
    #[allow(deprecated)] // Allow until main message loop is in Rust.
    match fidl_mlme::MlmeRequestMessage::decode(bytes.into(), &mut []) {
        Ok(msg) => sta.handle_mlme_msg(msg).into_raw_zx_status(),
        Err(e) => {
            error!("error decoding MLME message: {}", e);
            zx::Status::IO.into_raw()
        }
    }
}

#[no_mangle]
pub extern "C" fn ap_sta_handle_mac_frame(
    sta: &mut Ap,
    frame: CSpan<'_>,
    rx_info: *const banjo_wlan_mac::WlanRxInfo,
) -> i32 {
    // unsafe is ok because we checked rx_info is not a nullptr.
    let rx_info = if !rx_info.is_null() { Some(unsafe { *rx_info }) } else { None };
    sta.handle_mac_frame::<&[u8]>(frame.into(), rx_info);
    zx::sys::ZX_OK
}

#[no_mangle]
pub extern "C" fn ap_sta_handle_eth_frame(sta: &mut Ap, frame: CSpan<'_>) -> i32 {
    sta.handle_eth_frame(frame.into());
    zx::sys::ZX_OK
}

#[no_mangle]
pub extern "C" fn ap_sta_handle_hw_indication(
    sta: &mut Ap,
    ind: banjo_wlan_mac::WlanIndication,
) -> i32 {
    sta.handle_hw_indication(ind);
    zx::sys::ZX_OK
}
