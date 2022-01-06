// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/hardware/wlan/phyinfo/c/banjo.h>
#include <fuchsia/wlan/common/c/banjo.h>
#include <zircon/assert.h>

#include <wlan/common/band.h>
#include <wlan/common/channel.h>

namespace wlan {
namespace common {

namespace wlan_common = ::fuchsia::wlan::common;

wlan_info_band_t GetBand(const wlan_channel_t& channel) {
  return Is2Ghz(channel) ? WLAN_INFO_BAND_TWO_GHZ : WLAN_INFO_BAND_FIVE_GHZ;
}

std::string BandStr(uint8_t band) {
  if (band > WLAN_INFO_BAND_COUNT) {
    band = WLAN_INFO_BAND_COUNT;
  }
  switch (band) {
    case WLAN_INFO_BAND_TWO_GHZ:
      return "2 GHz";
    case WLAN_INFO_BAND_FIVE_GHZ:
      return "5 GHz";
    default:
      return "BAND_INV";
  }
}

std::string BandStr(wlan_info_band_t band) { return BandStr(static_cast<uint8_t>(band)); }

std::string BandStr(const wlan_channel_t& channel) { return BandStr(GetBand(channel)); }

wlan_common::Band BandToFidl(uint8_t band) {
  return BandToFidl(static_cast<wlan_info_band_t>(band));
}

wlan_common::Band BandToFidl(wlan_info_band_t band) {
  switch (band) {
    case WLAN_INFO_BAND_TWO_GHZ:
      return wlan_common::Band::WLAN_BAND_2GHZ;
    case WLAN_INFO_BAND_FIVE_GHZ:
      return wlan_common::Band::WLAN_BAND_5GHZ;
    default:
      return wlan_common::Band::WLAN_BAND_COUNT;
  }
}

wlan_info_band_t BandFromFidl(wlan_common::Band band) {
  switch (band) {
    case wlan_common::Band::WLAN_BAND_2GHZ:
      return WLAN_INFO_BAND_TWO_GHZ;
    case wlan_common::Band::WLAN_BAND_5GHZ:
      return WLAN_INFO_BAND_FIVE_GHZ;
    default:
      return WLAN_INFO_BAND_COUNT;
  }
}

}  // namespace common
}  // namespace wlan
