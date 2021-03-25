// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/media/codec_impl/codec_adapter.h>
#include <zircon/assert.h>

#include <limits>
#include <memory>

namespace {
constexpr uint64_t kInputBufferConstraintsVersionOrdinal = 1;
constexpr uint64_t kInputDefaultBufferConstraintsVersionOrdinal =
    kInputBufferConstraintsVersionOrdinal;

// No particular reason to demand more than 1 input packet to camp on, since by default we'll likely
// only be decoding from 1 at a time.  If a particular decoder really does camp on more than 1 at a
// time for whatever reason for any significant duration, it should override this default.
constexpr uint32_t kInputPacketCountForCodecMin = 1;
// This is fairly arbitrary, but roughly speaking, 1 to be decoding, 1 to be in
// flight back to the client.  The one in-flight from the client to the codec is the client's
// business, to avoid double-counting (or vice versa if you like - the counting doesn't care which
// is counted as long as we're not double-counting).  Particular CodecAdapter(s) may want to
// override this upward if we find it's needed to keep the HW busy when there's any backlog.
constexpr uint32_t kInputPacketCountForCodecRecommended = 2;
constexpr uint32_t kInputPacketCountForCodecRecommendedMax = 16;
constexpr uint32_t kInputPacketCountForCodecMax = 64;

constexpr uint32_t kInputDefaultPacketCountForCodec = kInputPacketCountForCodecRecommended;

constexpr uint32_t kInputPacketCountForClientMin = 1;
constexpr uint32_t kInputPacketCountForClientMax = std::numeric_limits<uint32_t>::max();

// Just 1 buffer to be in flight back to the client, filling, or in flight back to the codec.  Along
// with the 1 buffer that'll be requested by the codec, this is just barely enough to keep the
// codec busy assuming codec processing is slower than returning an input buffer to the client,
// filling that buffer, and returning that buffer back to the codec server.
//
// This doesn't intend to be large enough to ride out any hypothetical codec performance variability
// vs. needed processing rate.
constexpr uint32_t kInputDefaultPacketCountForClient = 1;

// TODO(dustingreen): Implement and permit single-buffer mode.  (The default
// will probably remain buffer per packet mode though.)
constexpr bool kInputSingleBufferModeAllowed = false;
constexpr bool kInputDefaultSingleBufferMode = false;

// These fields should soon be ignored by clients as these fields are being deprecated, so it's not
// particularly important that they don't match what each CodecAdapter will tell sysmem via
// SetConstraints().
//
// TODO(fxbug.dev/61424): Remove these when possible.
//
// A client using the min shouldn't necessarily expect performance to be
// acceptable when running higher bit-rates.
constexpr uint32_t kInputPerPacketBufferBytesMin = 8 * 1024;
// This is fairly arbitrary, but roughly speaking, ~266 KiB for an average frame
// at 50 Mbps for 4k video, rounded up to 512 KiB buffer space per packet to
// allow most but not all frames to fit in one packet.  It could be equally
// reasonable to say the average-size compressed from should barely fit in one
// packet's buffer space, or the average-size compressed frame should split to
// ~1.5 packets, but we don't want an excessive number of packets required per
// frame (not even for I frames).
constexpr uint32_t kInputPerPacketBufferBytesRecommended = 512 * 1024;
// This is an arbitrary cap for now.  The only reason it's larger than
// recommended is to allow some room to profile whether larger buffer space per
// packet might be useful for performance.
constexpr uint32_t kInputPerPacketBufferBytesMax = 4 * 1024 * 1024;
constexpr uint32_t kInputDefaultPerPacketBufferBytes = kInputPerPacketBufferBytesRecommended;

}  // namespace

CodecAdapter::CodecAdapter(std::mutex& lock, CodecAdapterEvents* codec_adapter_events)
    : lock_(lock),
      events_(codec_adapter_events),
      random_device_(),
      not_for_security_prng_(random_device_()) {
  ZX_DEBUG_ASSERT(events_);
  // nothing else to do here
}

CodecAdapter::~CodecAdapter() {
  // nothing to do here
}

void SetCodecMetrics(CodecMetrics* codec_metrics);

std::optional<media_metrics::StreamProcessorEvents2MetricDimensionImplementation>
CodecAdapter::CoreCodecMetricsImplementation() {
  // This will cause a ZX_PANIC() if LogEvent() is being used by a sub-class, in which case the
  // sub-class must override CoreCodecMetricsImplementation().
  return std::nullopt;
}

void CodecAdapter::CoreCodecSetSecureMemoryMode(
    CodecPort port, fuchsia::mediacodec::SecureMemoryMode secure_memory_mode) {
  if (secure_memory_mode != fuchsia::mediacodec::SecureMemoryMode::OFF) {
    events_->onCoreCodecFailCodec(
        "In CodecAdapter::CoreCodecSetSecureMemoryMode(), secure_memory_mode != OFF");
    return;
  }
  // CodecImpl will enforce that BufferCollection constraints and BufferCollectionInfo_2 are
  // consistent with OFF.
  return;
}

std::unique_ptr<const fuchsia::media::StreamBufferConstraints>
CodecAdapter::CoreCodecBuildNewInputConstraints() {
  auto constraints = std::make_unique<fuchsia::media::StreamBufferConstraints>();
  constraints->set_buffer_constraints_version_ordinal(kInputBufferConstraintsVersionOrdinal)
      .set_packet_count_for_server_recommended_max(kInputPacketCountForCodecRecommendedMax)
      .set_per_packet_buffer_bytes_min(kInputPerPacketBufferBytesMin)
      .set_per_packet_buffer_bytes_recommended(kInputPerPacketBufferBytesRecommended)
      .set_per_packet_buffer_bytes_max(kInputPerPacketBufferBytesMax)
      .set_packet_count_for_server_min(kInputPacketCountForCodecMin)
      .set_packet_count_for_server_recommended(kInputPacketCountForCodecRecommended)
      .set_packet_count_for_server_max(kInputPacketCountForCodecMax)
      .set_packet_count_for_client_min(kInputPacketCountForClientMin)
      .set_packet_count_for_client_max(kInputPacketCountForClientMax)
      .set_single_buffer_mode_allowed(kInputSingleBufferModeAllowed);

  constraints->mutable_default_settings()
      ->set_buffer_lifetime_ordinal(0)
      .set_buffer_constraints_version_ordinal(kInputDefaultBufferConstraintsVersionOrdinal)
      .set_packet_count_for_server(kInputDefaultPacketCountForCodec)
      .set_packet_count_for_client(kInputDefaultPacketCountForClient)
      .set_per_packet_buffer_bytes(kInputDefaultPerPacketBufferBytes)
      .set_single_buffer_mode(kInputDefaultSingleBufferMode);

  return constraints;
}

void CodecAdapter::CoreCodecResetStreamAfterCurrentFrame() {
  ZX_PANIC(
      "onCoreCodecResetStreamAfterCurrentFrame() triggered by a CodecAdapter that doesn't override "
      "CoreCodecResetStreamAfterCurrentFrame()\n");
}
