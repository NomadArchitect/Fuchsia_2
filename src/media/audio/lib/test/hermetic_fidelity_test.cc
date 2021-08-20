// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/lib/test/hermetic_fidelity_test.h"

#include <fuchsia/media/cpp/fidl.h>
#include <fuchsia/thermal/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/types.h>

#include <array>
#include <cmath>
#include <iomanip>
#include <map>
#include <set>
#include <string>

#include <test/thermal/cpp/fidl.h>

#include "lib/zx/time.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/media/audio/lib/analysis/analysis.h"
#include "src/media/audio/lib/analysis/generators.h"
#include "src/media/audio/lib/format/audio_buffer.h"
#include "src/media/audio/lib/test/renderer_shim.h"

using ASF = fuchsia::media::AudioSampleFormat;

namespace media::audio::test {

namespace {
struct ResultsIndex {
  HermeticFidelityTest::RenderPath path;
  int32_t channel;
  uint32_t thermal_state;

  bool operator<(const ResultsIndex& rhs) const {
    return std::tie(path, channel, thermal_state) <
           std::tie(rhs.path, rhs.channel, rhs.thermal_state);
  }
};

};  // namespace

// For each path|channel|thermal_state, we maintain two results arrays: Frequency Response and
// Signal-to-Noise-and-Distortion (sinad). A map of array results is saved as a function-local
// static variable. If kRetainWorstCaseResults is set, we persist results across repeated test runs.
//
// Note: two test cases must not collide on the same path/channel/thermal_state. Thus, this must be
// refactored if two test cases need to specify the same path|output_channels|thermal_state (an
// example would be Dynamic Range testing -- the same measurements, but at different volumes).

// static
// Retrieve (initially allocating, if necessary) the array of level results for this path|channel.
std::array<double, HermeticFidelityTest::kNumReferenceFreqs>& HermeticFidelityTest::level_results(
    RenderPath path, int32_t channel, uint32_t thermal_state) {
  // Allocated only when first needed, and automatically cleaned up when process exits.
  static auto results_level_db =
      new std::map<ResultsIndex, std::array<double, HermeticFidelityTest::kNumReferenceFreqs>>();

  ResultsIndex index{
      .path = path,
      .channel = channel,
      .thermal_state = thermal_state,
  };
  if (results_level_db->find(index) == results_level_db->end()) {
    auto& results = (*results_level_db)[index];
    std::fill(results.begin(), results.end(), std::numeric_limits<double>::infinity());
  }

  return results_level_db->find(index)->second;
}

// static
// Retrieve (initially allocating, if necessary) the array of sinad results for this path|channel.
// A map of these array results is saved as a function-local static variable.
std::array<double, HermeticFidelityTest::kNumReferenceFreqs>& HermeticFidelityTest::sinad_results(
    RenderPath path, int32_t channel, uint32_t thermal_state) {
  // Allocated only when first needed, and automatically cleaned up when process exits.
  static auto results_sinad_db =
      new std::map<ResultsIndex, std::array<double, HermeticFidelityTest::kNumReferenceFreqs>>();

  ResultsIndex index{
      .path = path,
      .channel = channel,
      .thermal_state = thermal_state,
  };
  if (results_sinad_db->find(index) == results_sinad_db->end()) {
    auto& results = (*results_sinad_db)[index];
    std::fill(results.begin(), results.end(), std::numeric_limits<double>::infinity());
  }

  return results_sinad_db->find(index)->second;
}

void HermeticFidelityTest::SetUp() {
  HermeticPipelineTest::SetUp();

  // We save input|output files if requested. Ensure the requested frequency is one we measure.
  save_fidelity_wav_files_ = HermeticPipelineTest::save_input_and_output_files_;
  if (save_fidelity_wav_files_) {
    bool requested_frequency_found = false;
    for (auto freq : kReferenceFrequencies) {
      if (freq == kFrequencyForSavedWavFiles) {
        requested_frequency_found = true;
        break;
      }
    }

    if (!requested_frequency_found) {
      FX_LOGS(WARNING) << kFrequencyForSavedWavFiles
                       << " is not in the frequency list, a WAV file cannot be saved";
      save_fidelity_wav_files_ = false;
    }
  }
}

// Translate real-world frequencies to frequencies that fit perfectly into our signal buffer.
// Internal frequencies must be integers, so we don't need to Window the output before frequency
// analysis. We use buffer size and frame rate. Thus, when measuring real-world frequency 2000 Hz
// with buffer size 65536 at frame rate 96 kHz, we use the internal frequency 1365, rather than
// 1365.333... -- translating to a real-world frequency of 1999.5 Hz (this is not a problem).
//
// We also want these internal frequencies to have fewer common factors with our buffer size and
// frame rates, as this can mask problems where previous buffer sections are erroneously repeated.
// So if a computed internal frequency is not integral, we use the odd neighbor, rather than round.
void HermeticFidelityTest::TranslateReferenceFrequencies(int32_t device_frame_rate) {
  for (auto freq_idx = 0u; freq_idx < kReferenceFrequencies.size(); ++freq_idx) {
    double internal_freq =
        static_cast<double>(kReferenceFrequencies[freq_idx] * kFreqTestBufSize) / device_frame_rate;
    int32_t floor_freq = std::floor(internal_freq);
    int32_t ceil_freq = std::ceil(internal_freq);
    translated_ref_freqs_[freq_idx] = (floor_freq % 2) ? floor_freq : ceil_freq;
  }
}

// Retrieve the number of thermal subscribers, and set them all to the specified thermal_state.
// thermal_test_control is synchronous: when SetThermalState returns, a change is committed.
zx_status_t HermeticFidelityTest::ConfigurePipelineForThermal(uint32_t thermal_state) {
  constexpr size_t kMaxRetries = 100;
  constexpr zx::duration kRetryPeriod = zx::msec(10);

  std::optional<size_t> audio_subscriber;

  std::vector<::test::thermal::SubscriberInfo> subscriber_data;
  // We might query thermal::test::Control before AudioCore has subscribed, so wait for it.
  for (size_t retries = 0u; retries < kMaxRetries; ++retries) {
    auto status = thermal_test_control()->GetSubscriberInfo(&subscriber_data);
    if (status != ZX_OK) {
      ADD_FAILURE() << "GetSubscriberInfo failed: " << status;
      return status;
    }

    // There is only one thermal subscriber for audio; there might be others of non-audio types.
    for (auto subscriber_num = 0u; subscriber_num < subscriber_data.size(); ++subscriber_num) {
      if (subscriber_data[subscriber_num].actor_type == fuchsia::thermal::ActorType::AUDIO) {
        audio_subscriber = subscriber_num;
        break;
      }
    }
    if (audio_subscriber.has_value()) {
      break;
    }
    zx::nanosleep(zx::deadline_after(kRetryPeriod));
  }

  if (!audio_subscriber.has_value()) {
    ADD_FAILURE() << "No audio-related thermal subscribers. "
                     "Don't set thermal_state if a pipeline has no thermal support";
    return ZX_ERR_TIMED_OUT;
  }

  auto max_thermal_state = subscriber_data[audio_subscriber.value()].num_thermal_states - 1;
  if (thermal_state > max_thermal_state) {
    ADD_FAILURE() << "Subscriber cannot be put into thermal_state " << thermal_state << " (max "
                  << max_thermal_state << ")";
    return ZX_ERR_NOT_SUPPORTED;
  }

  auto status =
      this->thermal_test_control()->SetThermalState(audio_subscriber.value(), thermal_state);
  if (status != ZX_OK) {
    ADD_FAILURE() << "SetThermalState failed: " << status;
    return status;
  }

  return ZX_OK;
}

template <ASF InputFormat, ASF OutputFormat>
AudioBuffer<OutputFormat> HermeticFidelityTest::GetRendererOutput(
    TypedFormat<InputFormat> input_format, int64_t input_buffer_frames, RenderPath path,
    AudioBuffer<InputFormat> input, VirtualOutput<OutputFormat>* device) {
  FX_CHECK(input_format.frames_per_second() == 96000);

  fuchsia::media::AudioRenderUsage usage = fuchsia::media::AudioRenderUsage::MEDIA;

  if (path == RenderPath::Communications) {
    usage = fuchsia::media::AudioRenderUsage::COMMUNICATION;
  }

  // Render input such that first input frame will be rendered into first ring buffer frame.
  if (path == RenderPath::Ultrasound) {
    auto renderer = CreateUltrasoundRenderer(input_format, input_buffer_frames, true);
    auto packets = renderer->AppendPackets({&input});

    renderer->PlaySynchronized(this, device, 0);
    renderer->WaitForPackets(this, packets);
  } else {
    auto renderer = CreateAudioRenderer(input_format, input_buffer_frames, usage);
    auto packets = renderer->AppendPackets({&input});

    renderer->PlaySynchronized(this, device, 0);
    renderer->WaitForPackets(this, packets);
  }

  // Extract it from the VAD ring-buffer.
  return device->SnapshotRingBuffer();
}

template <ASF InputFormat, ASF OutputFormat>
void HermeticFidelityTest::DisplaySummaryResults(
    const TestCase<InputFormat, OutputFormat>& test_case) {
  // Loop by channel, displaying summary results, in a separate loop from checking each result.
  for (const auto& channel_spec : test_case.channels_to_measure) {
    // Show results in tabular forms, for easy copy into hermetic_fidelity_results.cc.
    const auto& chan_level_results_db =
        level_results(test_case.path, channel_spec.channel, test_case.thermal_state.value_or(0));
    printf("\n\tFull-spectrum Frequency Response - %s - output channel %d",
           test_case.test_name.c_str(), channel_spec.channel);
    for (auto freq_idx = 0u; freq_idx < kNumReferenceFreqs; ++freq_idx) {
      printf(" %s%8.3f,", (freq_idx % 10 == 0 ? "\n" : ""),
             floor(chan_level_results_db[freq_idx] / kFidelityDbTolerance) * kFidelityDbTolerance);
    }
    printf("\n");

    const auto& chan_sinad_results_db =
        sinad_results(test_case.path, channel_spec.channel, test_case.thermal_state.value_or(0));
    printf("\n\tSignal-to-Noise and Distortion -   %s - output channel %d",
           test_case.test_name.c_str(), channel_spec.channel);
    for (auto freq_idx = 0u; freq_idx < kNumReferenceFreqs; ++freq_idx) {
      printf(" %s%8.3f,", (freq_idx % 10 == 0 ? "\n" : ""),
             floor(chan_sinad_results_db[freq_idx] / kFidelityDbTolerance) * kFidelityDbTolerance);
    }
    printf("\n\n");
  }
}

template <ASF InputFormat, ASF OutputFormat>
void HermeticFidelityTest::VerifyResults(const TestCase<InputFormat, OutputFormat>& test_case) {
  // Loop by channel_to_measure
  for (const auto& channel_spec : test_case.channels_to_measure) {
    const auto& chan_level_results_db =
        level_results(test_case.path, channel_spec.channel, test_case.thermal_state.value_or(0));
    for (auto freq_idx = 0u; freq_idx < kNumReferenceFreqs; ++freq_idx) {
      EXPECT_GE(chan_level_results_db[freq_idx],
                channel_spec.freq_resp_lower_limits_db[freq_idx] - kFidelityDbTolerance)
          << "  Channel " << channel_spec.channel << ", FreqResp [" << std::setw(2) << freq_idx
          << "]  (" << std::setw(5) << kReferenceFrequencies[freq_idx]
          << " Hz):  " << std::setprecision(7)
          << floor(chan_level_results_db[freq_idx] / kFidelityDbTolerance) * kFidelityDbTolerance;
    }

    const auto& chan_sinad_results_db =
        sinad_results(test_case.path, channel_spec.channel, test_case.thermal_state.value_or(0));
    for (auto freq_idx = 0u; freq_idx < kNumReferenceFreqs; ++freq_idx) {
      EXPECT_GE(chan_sinad_results_db[freq_idx],
                channel_spec.sinad_lower_limits_db[freq_idx] - kFidelityDbTolerance)
          << "  Channel " << channel_spec.channel << ", SINAD    [" << std::setw(2) << freq_idx
          << "]  (" << std::setw(5) << kReferenceFrequencies[freq_idx]
          << " Hz):  " << std::setprecision(7)
          << floor(chan_sinad_results_db[freq_idx] / kFidelityDbTolerance) * kFidelityDbTolerance;
    }
  }
}

template <ASF OutputFormat>
bool HermeticFidelityTest::DeviceHasUnderflows(VirtualOutput<OutputFormat>* device) {
  auto root = environment()->ReadInspect(HermeticAudioEnvironment::kAudioCoreComponent);
  for (auto kind : {"device underflows", "pipeline underflows"}) {
    std::vector<std::string> path = {
        "output devices",
        fxl::StringPrintf("%03lu", device->inspect_id()),
        kind,
    };
    auto path_string = fxl::JoinStrings(path, "/");
    auto h = root.GetByPath(path);
    if (!h) {
      ADD_FAILURE() << "Missing inspect hierarchy for " << path_string;
      continue;
    }
    auto p = h->node().template get_property<inspect::UintPropertyValue>("count");
    if (!p) {
      ADD_FAILURE() << "Missing property: " << path_string << "[count]";
      continue;
    }
    if (p->value() > 0) {
      FX_LOGS(WARNING) << "Found underflow at " << path_string;
      return true;
    }
  }
  return false;
}

// Additional fidelity assessments, potentially added in the future:
// (1) Dynamic range (1kHz input at -30/60/90 db: measure level, sinad. Overall gain sensitivity)
//     This should clearly show the impact of dynamic compression in the effects chain.
// (2) Assess the e2e input data path (from device to capturer)
//     Included for completeness: we apply no capture effects; should equal audio_fidelity_tests.
template <ASF InputFormat, ASF OutputFormat>
void HermeticFidelityTest::Run(
    const HermeticFidelityTest::TestCase<InputFormat, OutputFormat>& tc) {
  // TODO(mpuryear): support source frequencies other than 96k, when necessary
  FX_CHECK(tc.input_format.frames_per_second() == 96000)
      << "For now, non-96k renderer frame rates are disallowed in this test";
  FX_CHECK(tc.output_format.frames_per_second() == 96000)
      << "For now, non-96k device frame rates are disallowed in this test";
  // Translate from input frame number to output frame number.
  auto input_frame_to_output_frame = [](int64_t input_frame) { return input_frame; };

  //
  // Compute input signal length: it should first include time to ramp in, then the number of frames
  // that we actually analyze, and then time to ramp out.
  int64_t input_signal_frames_to_measure =
      std::ceil(static_cast<double>(kFreqTestBufSize * tc.input_format.frames_per_second()) /
                tc.output_format.frames_per_second());
  auto input_signal_frames =
      tc.pipeline.neg_filter_width + input_signal_frames_to_measure + tc.pipeline.pos_filter_width;

  //
  // Compute the renderer payload buffer size (including pre-signal silence).
  // TODO(mpuryear): revisit, once pipeline automatically handles filter_width by feeding silence.
  auto input_signal_start = tc.pipeline.pos_filter_width;
  auto total_input_frames = input_signal_start + input_signal_frames;
  if constexpr (kDebugInputBuffer) {
    FX_LOGS(INFO) << "input_signal_start " << input_signal_start
                  << ", input_signal_frames_to_measure " << input_signal_frames_to_measure
                  << ", total_input_frames " << total_input_frames;
  }

  auto input_type_mono =
      Format::Create<InputFormat>(1, tc.input_format.frames_per_second()).take_value();
  auto bookend_silence = GenerateSilentAudio(input_type_mono, input_signal_start);

  // We create the AudioBuffer later. Ensure no out-of-range channels are requested to play.
  for (const auto& channel : tc.channels_to_play) {
    ASSERT_LT(static_cast<int32_t>(channel), tc.input_format.channels())
        << "Cannot play out-of-range input channel";
  }

  //
  // Then, calculate the length of the output signal and set up the VAD, with a 1-sec ring-buffer.
  auto output_buffer_frames_needed =
      static_cast<int64_t>(input_frame_to_output_frame(total_input_frames));
  auto output_buffer_size = tc.output_format.frames_per_second();
  FX_CHECK(output_buffer_frames_needed <= output_buffer_size)
      << "output_buffer_frames_needed (" << output_buffer_frames_needed
      << ") must not exceed output_buffer_size (" << output_buffer_size << ")";

  auto device =
      CreateOutput(AUDIO_STREAM_UNIQUE_ID_BUILTIN_SPEAKERS, tc.output_format,
                   output_buffer_frames_needed, std::nullopt, tc.pipeline.output_device_gain_db);

  if (tc.thermal_state.has_value()) {
    if (ConfigurePipelineForThermal(tc.thermal_state.value()) != ZX_OK) {
      return;
    }
  }

  for (auto ec : tc.effect_configs) {
    fuchsia::media::audio::EffectsController_UpdateEffect_Result result;
    auto status = effects_controller()->UpdateEffect(ec.name, ec.config, &result);
    ASSERT_EQ(status, ZX_OK);
  }

  // Generate rate-specific internal frequency values for our power-of-two-sized analysis buffer.
  TranslateReferenceFrequencies(tc.output_format.frames_per_second());

  //
  // Now iterate through the spectrum, completely processing one frequency at a time.
  for (auto freq_idx = 0u; freq_idx < kNumReferenceFreqs; ++freq_idx) {
    auto freq = translated_ref_freqs_[freq_idx];  // The frequency within our power-of-two buffer
    auto freq_for_display = kReferenceFrequencies[freq_idx];

    // Write input signal to input buffer. Start with silence for pre-ramping, which aligns the
    // input and output WAV files (if enabled). Prepend / append signal to account for ramp-in/out.
    // We could include trailing silence to flush out any cached values and show decay, but there is
    // no need to do so for these tests.
    auto signal_section =
        GenerateCosineAudio(input_type_mono, input_signal_frames_to_measure, freq);
    auto input_mono = bookend_silence;
    input_mono.Append(AudioBufferSlice(
        &signal_section, input_signal_frames_to_measure - tc.pipeline.neg_filter_width,
        input_signal_frames_to_measure));
    input_mono.Append(AudioBufferSlice(&signal_section));
    input_mono.Append(AudioBufferSlice(&signal_section, 0, tc.pipeline.pos_filter_width));
    FX_CHECK(input_mono.NumFrames() == static_cast<int64_t>(total_input_frames))
        << "Miscalculated input_mono length: testcode error";

    auto silence_mono = GenerateSilentAudio(input_type_mono, total_input_frames);

    std::vector<AudioBufferSlice<InputFormat>> channels;
    for (auto play_channel = 0; play_channel < tc.input_format.channels(); ++play_channel) {
      if (tc.channels_to_play.find(play_channel) != tc.channels_to_play.end()) {
        channels.push_back(AudioBufferSlice(&input_mono));
      } else {
        channels.push_back(AudioBufferSlice(&silence_mono));
      }
    }
    auto input = AudioBuffer<InputFormat>::Interleave(channels);
    FX_CHECK(input.NumFrames() == static_cast<int64_t>(total_input_frames))
        << "Miscalculated input length: testcode error";

    if constexpr (kDebugInputBuffer) {
      if (kDebugBuffersAtAllFrequencies || freq_for_display == kFrequencyForBufferDebugging) {
        // We construct the input buffer in pieces. If signals don't align at these seams, it causes
        // distortion. For debugging, show these "seam" locations in the input buffer we created.
        std::string tag = "\nInput buffer for " + std::to_string(freq_for_display) + " Hz [" +
                          std::to_string(freq_idx) + "]";
        input.Display(0, 16, tag);
        input.Display(input_signal_start - 16, input_signal_start + 16, "Start of input signal");
        input.Display(input_signal_start + tc.pipeline.neg_filter_width - 16,
                      input_signal_start + tc.pipeline.neg_filter_width + 16,
                      "End of initial ramp-in of input signal");
        input.Display(
            input_signal_start + tc.pipeline.neg_filter_width + input_signal_frames_to_measure - 16,
            input_signal_start + tc.pipeline.neg_filter_width + input_signal_frames_to_measure + 16,
            "End of input signal; start of additional ramp-out");
        input.Display(input_signal_start + input_signal_frames - 16,
                      input_signal_start + input_signal_frames + 16, "End of additional ramp-out");
        input.Display(input.NumFrames() - 16, input.NumFrames(), "End of input buffer");
      }
    }

    // Save off the input file, if requested.
    if (save_fidelity_wav_files_) {
      // We shouldn't save files for ALL frequencies -- just save the files for this frequency.
      if (freq_for_display == kFrequencyForSavedWavFiles) {
        std::string test_name = tc.test_name + "_" + std::to_string(freq_for_display) + "hz";
        HermeticPipelineTest::WriteWavFile<InputFormat>(test_name, "input",
                                                        AudioBufferSlice(&input));
      }
    }

    // Set up the renderer, run it and retrieve the output.
    auto ring_buffer =
        GetRendererOutput(tc.input_format, total_input_frames, tc.path, input, device);

    // Loop here on each channel to measure...
    for (const auto& channel_spec : tc.channels_to_measure) {
      auto ring_buffer_chan = AudioBufferSlice(&ring_buffer).GetChannel(channel_spec.channel);

      // Analyze the results
      auto output_analysis_start =
          input_frame_to_output_frame(input_signal_start + tc.pipeline.neg_filter_width);
      auto output = AudioBufferSlice(&ring_buffer_chan, output_analysis_start,
                                     output_analysis_start + kFreqTestBufSize);

      if constexpr (kDebugOutputBuffer) {
        if (kDebugBuffersAtAllFrequencies || freq_for_display == kFrequencyForBufferDebugging) {
          std::string tag = "\nOutput buffer for " + std::to_string(freq_for_display) + " Hz [" +
                            std::to_string(freq_idx) + "], channel " +
                            std::to_string(channel_spec.channel);
          // For debugging, show critical locations in the output buffer we retrieved.
          ring_buffer_chan.Display(0, 16, tag);
          ring_buffer_chan.Display(output_analysis_start - 16, output_analysis_start + 16,
                                   "Startof output analysis section");
          ring_buffer_chan.Display(output_analysis_start + kFreqTestBufSize - 16,
                                   output_analysis_start + kFreqTestBufSize + 16,
                                   "End of output analysis section");
          ring_buffer_chan.Display(ring_buffer_chan.NumFrames() - 16, ring_buffer_chan.NumFrames(),
                                   "End of output buffer");
        }
      }

      auto channel_is_out_of_band = (channel_spec.freq_resp_lower_limits_db[0] == -INFINITY);
      auto out_of_band = (freq_for_display < tc.low_cut_frequency ||
                          freq_for_display > tc.low_pass_frequency || channel_is_out_of_band);

      double sinad_db, level_db = 0.0;
      if (out_of_band) {
        // For out-of-band frequencies, we use the sinad array to store Out-of-Band Rejection,
        // which is measured as the sinad(all frequencies), assuming a full-scale input.
        sinad_db = DoubleToDb(1.0 / MeasureAudioFreqs(output, {}).total_magn_other);

        if constexpr (!kSuppressInProgressResults) {
          FX_LOGS(INFO) << "Channel " << channel_spec.channel << ": " << std::setw(5)
                        << freq_for_display << " Hz [" << std::setw(2) << freq_idx
                        << "] --       out-of-band rejection " << std::fixed << std::setprecision(4)
                        << std::setw(8) << sinad_db << " db";
        }
      } else {
        auto result = MeasureAudioFreqs(output, {static_cast<int32_t>(freq)});
        level_db = DoubleToDb(result.magnitudes[freq]);
        if (isinf(level_db) && level_db < 0) {
          // If an expected signal was truly absent (silence), we probably underflowed. This
          // [level_db, sinad_db] pair is meaningless, so set sinad_db to -INFINITY as well.
          sinad_db = -INFINITY;
        } else {
          sinad_db = DoubleToDb(result.magnitudes[freq] / result.total_magn_other);
        }

        if constexpr (!kSuppressInProgressResults) {
          FX_LOGS(INFO) << "Channel " << channel_spec.channel << ": " << std::setw(5)
                        << freq_for_display << " Hz [" << std::setw(2) << freq_idx << "] --  level "
                        << std::fixed << std::setprecision(4) << std::setw(9) << level_db
                        << " db,  sinad " << std::setw(8) << sinad_db << " db";
        }
      }

      if (save_fidelity_wav_files_) {
        // We shouldn't save files for the full frequency set -- just save files for this frequency.
        if (freq_for_display == kFrequencyForSavedWavFiles) {
          std::string test_name = tc.test_name + "_chan" + std::to_string(channel_spec.channel) +
                                  "_" + std::to_string(freq_for_display) + "hz";
          HermeticPipelineTest::WriteWavFile<OutputFormat>(test_name, "output", output);
        }
      }

      // Retrieve the arrays of measurements for this path and channel
      auto& curr_level_db =
          level_results(tc.path, channel_spec.channel, tc.thermal_state.value_or(0));
      auto& curr_sinad_db =
          sinad_results(tc.path, channel_spec.channel, tc.thermal_state.value_or(0));
      if constexpr (kRetainWorstCaseResults) {
        curr_level_db[freq_idx] = std::min(curr_level_db[freq_idx], level_db);
        curr_sinad_db[freq_idx] = std::min(curr_sinad_db[freq_idx], sinad_db);
      } else {
        curr_level_db[freq_idx] = level_db;
        curr_sinad_db[freq_idx] = sinad_db;
      }
    }
  }

  if constexpr (kDisplaySummaryResults) {
    DisplaySummaryResults(tc);
  }

  // TODO(fxbug.dev/80003): Skipping checks until underflows are fixed.
  if (DeviceHasUnderflows(device)) {
    FX_LOGS(WARNING) << "Skipping threshold checks due to underflows";
  } else {
    VerifyResults(tc);
  }
}

// We only run the pipeline fidelity tests with FLOAT inputs/outputs, for full data precision.
template void HermeticFidelityTest::Run<ASF::FLOAT, ASF::FLOAT>(
    const TestCase<ASF::FLOAT, ASF::FLOAT>& tc);

}  // namespace media::audio::test
