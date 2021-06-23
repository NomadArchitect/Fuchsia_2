// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_LIB_TEST_HERMETIC_FIDELITY_TEST_H_
#define SRC_MEDIA_AUDIO_LIB_TEST_HERMETIC_FIDELITY_TEST_H_

#include <fuchsia/media/cpp/fidl.h>

#include <set>
#include <string>

#include "src/media/audio/lib/test/hermetic_pipeline_test.h"

namespace media::audio::test {

// These tests feed a series of individual sinusoidal signals (across the
// frequency spectrum) into the pipeline, validating that the output level is
// (1) high at the expected frequency, and (2) low at all other frequencies
// (respectively, frequency response and signal-to-noise-and-distortion).
class HermeticFidelityTest : public HermeticPipelineTest {
 public:
  static constexpr int64_t kNumReferenceFreqs = 42;
  // The (approximate) frequencies represented by limit-threshold arrays freq_resp_lower_limits_db
  // and sinad_lower_limits_db, and corresponding actual results arrays gathered during the tests.
  // The actual frequencies (within a buffer of kFreqTestBufSize) are found in kRefFreqsInternal.
  static const std::array<int32_t, kNumReferenceFreqs> kReferenceFrequencies;

  // Test the three render paths present in today's effects configuration.
  enum class RenderPath {
    Media = 0,
    Communications = 1,
    Ultrasound = 2,
  };
  // Specify an output channel to measure, and thresholds against which to compare it.
  struct ChannelMeasurement {
    int32_t channel;
    const std::array<double, kNumReferenceFreqs> freq_resp_lower_limits_db;
    const std::array<double, kNumReferenceFreqs> sinad_lower_limits_db;

    ChannelMeasurement(int32_t chan, std::array<double, kNumReferenceFreqs> freqs,
                       std::array<double, kNumReferenceFreqs> sinads)
        : channel(chan), freq_resp_lower_limits_db(freqs), sinad_lower_limits_db(sinads) {}
  };

  // This struct includes all the configuration info for this full-spectrum test.
  template <fuchsia::media::AudioSampleFormat InputFormat,
            fuchsia::media::AudioSampleFormat OutputFormat>
  struct TestCase {
    std::string test_name;

    TypedFormat<InputFormat> input_format;
    RenderPath path;
    const std::set<int32_t> channels_to_play;

    PipelineConstants pipeline;
    int32_t low_cut_frequency = 0;
    int32_t low_pass_frequency = fuchsia::media::MAX_PCM_FRAMES_PER_SECOND;
    std::optional<uint32_t> thermal_state = std::nullopt;

    TypedFormat<OutputFormat> output_format;
    const std::set<ChannelMeasurement> channels_to_measure;
  };

  void SetUp() override;

  template <fuchsia::media::AudioSampleFormat InputFormat,
            fuchsia::media::AudioSampleFormat OutputFormat>
  void Run(const TestCase<InputFormat, OutputFormat>& tc);

 protected:
  // Custom build-time flags
  //
  // These could become cmdline flags. For normal CQ operation, all should be false.
  //
  // Debug positioning and values of the renderer's input buffer, by showing certain locations.
  static constexpr bool kDebugInputBuffer = false;

  // Debug positioning and values of the output ring buffer snapshot, by showing certain locations.
  static constexpr bool kDebugOutputBuffer = false;

  // Show a frequency's result immediately. Helps correlate UNDERFLOW with affected frequency.
  static constexpr bool kSuppressInProgressResults = false;

  // Retain and display the worst-case results in a multi-repeat run. Helpful for updating limits.
  static constexpr bool kRetainWorstCaseResults = false;

  // Show results at test-end in tabular form, for copy/compare to hermetic_fidelity_result.cc.
  static constexpr bool kDisplaySummaryResults = false;

  // Consts related to fidelity testing
  //
  // When testing fidelity, we compare the actual measured value to an expected value. These tests
  // are designed so that they pass if 'actual' is greater than or equal to 'expected' -- or if
  // 'actual' is less than 'expected' by (at most) the following tolerance. This tolerance also
  // determines the number of digits of precision for 'expected' values, when stored or displayed.
  static constexpr double kFidelityDbTolerance = 0.001;

  // The power-of-two size of our spectrum analysis buffer.
  static constexpr int64_t kFreqTestBufSize = 65536;

  // Saving all input|output files (if --save-input-and-output specified) consumes too much
  // on-device storage. These tests save only the input|output files for this specified frequency.
  static constexpr int32_t kFrequencyForSavedWavFiles = 1000;

  static inline double DoubleToDb(double val) { return std::log10(val) * 20.0; }

  static std::array<double, HermeticFidelityTest::kNumReferenceFreqs>& level_results(
      RenderPath path, int32_t channel, uint32_t thermal_state);
  static std::array<double, HermeticFidelityTest::kNumReferenceFreqs>& sinad_results(
      RenderPath path, int32_t channel, uint32_t thermal_state);

  void TranslateReferenceFrequencies(int32_t device_frame_rate);

  // Create a renderer for this path, submit and play the input buffer, retrieve the ring buffer.
  template <fuchsia::media::AudioSampleFormat InputFormat,
            fuchsia::media::AudioSampleFormat OutputFormat>
  AudioBuffer<OutputFormat> GetRendererOutput(TypedFormat<InputFormat> input_format,
                                              int64_t input_buffer_frames, RenderPath path,
                                              AudioBuffer<InputFormat> input,
                                              VirtualOutput<OutputFormat>* device);

  // Display results for this path, in tabular form for each compare/copy to existing limits.
  template <fuchsia::media::AudioSampleFormat InputFormat,
            fuchsia::media::AudioSampleFormat OutputFormat>
  void DisplaySummaryResults(const TestCase<InputFormat, OutputFormat>& test_case);

  // Validate results for the given channel set, against channel-mapped results arrays.
  template <fuchsia::media::AudioSampleFormat InputFormat,
            fuchsia::media::AudioSampleFormat OutputFormat>
  void VerifyResults(const TestCase<InputFormat, OutputFormat>& test_case);

  // Change the output pipeline's thermal state, blocking until the state change completes.
  zx_status_t ConfigurePipelineForThermal(uint32_t thermal_state);

 private:
  // Ref frequencies, internally translated to values corresponding to a buffer[kFreqTestBufSize].
  std::array<int32_t, kNumReferenceFreqs> translated_ref_freqs_;

  bool save_fidelity_wav_files_;
};

inline bool operator<(const HermeticFidelityTest::ChannelMeasurement& lhs,
                      const HermeticFidelityTest::ChannelMeasurement& rhs) {
  return (lhs.channel < rhs.channel);
}

}  // namespace media::audio::test

#endif  // SRC_MEDIA_AUDIO_LIB_TEST_HERMETIC_FIDELITY_TEST_H_
