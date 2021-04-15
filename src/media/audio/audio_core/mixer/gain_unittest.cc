// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/audio_core/mixer/gain.h"

#include <iterator>

#include <fbl/algorithm.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "lib/syslog/cpp/macros.h"

using testing::Each;
using testing::FloatEq;
using testing::Not;
using testing::Pointwise;

namespace media::audio::test {

TEST(StaticGainTest, CombineGains) {
  static_assert(-90.0 < Gain::kMinGainDb / 2);
  static_assert(15.0 > Gain::kMaxGainDb / 2);

  EXPECT_EQ(Gain::CombineGains(-90, -90), Gain::kMinGainDb);
  EXPECT_EQ(Gain::CombineGains(15, 15), Gain::kMaxGainDb);
  EXPECT_EQ(Gain::CombineGains(-20, 5), -15);
}

// Test the internally-used inline func that converts AScale gain to dB.
TEST(StaticGainTest, GainScaleToDb) {
  // Unity scale is 0.0dB (no change).
  EXPECT_FLOAT_EQ(Gain::ScaleToDb(Gain::kUnityScale), Gain::kUnityGainDb);

  // 10x scale-up in amplitude (by definition) is exactly +20.0dB.
  EXPECT_FLOAT_EQ(Gain::ScaleToDb(Gain::kUnityScale * 10.0f), 20.0f);

  // 1/100x scale-down in amplitude (by definition) is exactly -40.0dB.
  EXPECT_FLOAT_EQ(Gain::ScaleToDb(Gain::kUnityScale * 0.01f), -40.0f);

  // 1/2x scale-down by calculation: -6.020600... dB.
  const float half_scale = -6.0206001f;
  EXPECT_FLOAT_EQ(half_scale, Gain::ScaleToDb(Gain::kUnityScale * 0.5f));
}

// Test the inline function that converts a numerical value to dB.
TEST(StaticGainTest, DoubleToDb) {
  EXPECT_DOUBLE_EQ(Gain::DoubleToDb(Gain::kUnityScale), 0.0);  // Unity: 0 dB
  EXPECT_DOUBLE_EQ(Gain::DoubleToDb(Gain::kUnityScale * 100.0),
                   40.0);                                              // 100x: 40 dB
  EXPECT_DOUBLE_EQ(Gain::DoubleToDb(Gain::kUnityScale * 0.1), -20.0);  // 10%: -20 dB

  EXPECT_GE(Gain::DoubleToDb(Gain::kUnityScale * 0.5),
            -6.0206 * 1.000001);  // 50%: approx -6.0206 dB
  EXPECT_LE(Gain::DoubleToDb(Gain::kUnityScale * 0.5),
            -6.0206 * 0.999999);  // FP representation => 2 comps
}

// Gain tests - how does the Gain object respond when given values close to its
// maximum or minimum; does it correctly cache; do values combine to form Unity
// gain. Is data scaling accurately performed, and is it adequately linear? Do
// our gains and accumulators behave as expected when they overflow?
//
// Gain tests using AScale and the Gain object only
//
class GainBase : public testing::Test {
 protected:
  void SetUp() override {
    testing::Test::SetUp();
    rate_1khz_output_ = TimelineRate(1000, ZX_SEC(1));
  }

  // Used for debugging purposes.
  static void DisplayScaleVals(const Gain::AScale* scale_arr, int64_t buf_size) {
    printf("\n    ********************************************************");
    printf("\n **************************************************************");
    printf("\n ***    Displaying raw scale array data for length %5ld    ***", buf_size);
    printf("\n **************************************************************");
    for (auto idx = 0; idx < buf_size; ++idx) {
      if (idx % 10 == 0) {
        printf("\n [%d]  ", idx);
      }
      printf("%.7f   ", scale_arr[idx]);
    }
    printf("\n **************************************************************");
    printf("\n    ********************************************************");
    printf("\n");
  }

  // Overridden by SourceGainControl and DestGainControl
  virtual void SetGain(float gain_db) = 0;
  virtual void SetOtherGain(float gain_db) = 0;
  virtual void SetGainWithRamp(float gain_db, zx::duration duration,
                               fuchsia::media::audio::RampType ramp_type =
                                   fuchsia::media::audio::RampType::SCALE_LINEAR) = 0;
  virtual void SetOtherGainWithRamp(float gain_db, zx::duration duration,
                                    fuchsia::media::audio::RampType ramp_type =
                                        fuchsia::media::audio::RampType::SCALE_LINEAR) = 0;
  virtual void CompleteRamp() = 0;

  // Used by SourceGainTest and DestGainTest
  void TestUnityGain(float source_gain_db, float dest_gain_db);
  void UnityChecks();
  void GainCachingChecks();
  void VerifyMinGain(float source_gain_db, float dest_gain_db);
  void MinGainChecks();
  void VerifyMaxGain(float source_gain_db, float dest_gain_db);
  void MaxGainChecks();
  void SourceMuteChecks();

  // Used by SourceGainRampTest and DestGainRampTest
  void TestRampWithNoDuration();
  void TestRampWithDuration();
  void TestRampIntoSilence();
  void TestRampOutOfSilence();
  void TestRampFromSilenceToSilence();
  void TestRampsCombineForSilence();
  void TestRampUnity();
  void TestFlatRamp();
  void TestRampingBelowMinGain();
  void TestRampWithMute();
  void TestAdvance();
  void TestSetGainCancelsRamp();
  void TestRampsForSilence();
  void TestRampsForNonSilence();

  // Used by SourceGainScaleArrayTest and DestGainScaleArrayTest
  void TestGetScaleArrayNoRamp();
  void TestGetScaleArray();
  void TestScaleArrayLongRamp();
  void TestScaleArrayShortRamp();
  void TestScaleArrayWithoutAdvance();
  void TestScaleArrayBigAdvance();
  void TestRampCompletion();
  void TestAdvanceHalfwayThroughRamp();
  void TestSuccessiveRamps();
  void TestCombinedRamps();
  void TestCrossFades();

  Gain gain_;

  // All tests use a 1 kHz frame rate, for easy 1-frame-per-msec observation.
  TimelineRate rate_1khz_output_;
};

// Used so that identical testing is done on the source-gain and dest-gain portions of Gain.
class SourceGainControl : public GainBase {
 protected:
  void SetGain(float gain_db) override { gain_.SetSourceGain(gain_db); }
  void SetOtherGain(float gain_db) override { gain_.SetDestGain(gain_db); }
  void SetGainWithRamp(float gain_db, zx::duration duration,
                       fuchsia::media::audio::RampType ramp_type =
                           fuchsia::media::audio::RampType::SCALE_LINEAR) override {
    gain_.SetSourceGainWithRamp(gain_db, duration, ramp_type);
  }
  void SetOtherGainWithRamp(float gain_db, zx::duration duration,
                            fuchsia::media::audio::RampType ramp_type =
                                fuchsia::media::audio::RampType::SCALE_LINEAR) override {
    gain_.SetDestGainWithRamp(gain_db, duration, ramp_type);
  }
  void CompleteRamp() override { gain_.CompleteSourceRamp(); }
};
class DestGainControl : public GainBase {
 protected:
  void SetGain(float gain_db) override { gain_.SetDestGain(gain_db); }
  void SetOtherGain(float gain_db) override { gain_.SetSourceGain(gain_db); }
  void SetGainWithRamp(float gain_db, zx::duration duration,
                       fuchsia::media::audio::RampType ramp_type =
                           fuchsia::media::audio::RampType::SCALE_LINEAR) override {
    gain_.SetDestGainWithRamp(gain_db, duration, ramp_type);
  }
  void SetOtherGainWithRamp(float gain_db, zx::duration duration,
                            fuchsia::media::audio::RampType ramp_type =
                                fuchsia::media::audio::RampType::SCALE_LINEAR) override {
    gain_.SetSourceGainWithRamp(gain_db, duration, ramp_type);
  }
  void CompleteRamp() override { gain_.CompleteDestRamp(); }
};

// General (non-specific to source or dest) gain checks
class GainTest : public SourceGainControl {};

// Gain checks that can be source/dest inverted, and thus are run both ways
class SourceGainTest : public SourceGainControl {};
class DestGainTest : public DestGainControl {};

// Checks of gain-ramping behavior
class SourceGainRampTest : public SourceGainControl {};
class DestGainRampTest : public DestGainControl {};

// Precise calculation checks, of GetScaleArray with gain-ramping
class SourceGainScaleArrayTest : public SourceGainControl {};
class DestGainScaleArrayTest : public DestGainControl {};

// Test the defaults upon construction
TEST_F(GainTest, Defaults) {
  EXPECT_FLOAT_EQ(gain_.GetGainScale(), Gain::kUnityScale);
  EXPECT_TRUE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());
}

void GainBase::TestUnityGain(float source_gain_db, float dest_gain_db) {
  SetGain(source_gain_db);
  SetOtherGain(dest_gain_db);
  EXPECT_FLOAT_EQ(Gain::kUnityScale, gain_.GetGainScale());

  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsUnity());
}
void GainBase::UnityChecks() {
  TestUnityGain(Gain::kUnityGainDb, Gain::kUnityGainDb);

  // These positive/negative values should sum to 0.0: UNITY
  TestUnityGain(Gain::kMaxGainDb / 2, -Gain::kMaxGainDb / 2);
  TestUnityGain(-Gain::kMaxGainDb, Gain::kMaxGainDb);
}
// Do source and destination gains correctly combine to produce unity scaling?
TEST_F(SourceGainTest, Unity) { UnityChecks(); }
TEST_F(DestGainTest, Unity) { UnityChecks(); }

void GainBase::GainCachingChecks() {
  Gain expect_gain;
  Gain::AScale amplitude_scale, expect_amplitude_scale;

  // Set expect_amplitude_scale to a value that represents -6.0 dB.
  expect_gain.SetSourceGain(-6.0f);
  expect_amplitude_scale = expect_gain.GetGainScale();

  // If Render gain defaults to 0.0, this represents -6.0 dB too.
  SetGain(0.0f);
  SetOtherGain(-6.0f);
  amplitude_scale = gain_.GetGainScale();
  EXPECT_FLOAT_EQ(expect_amplitude_scale, amplitude_scale);

  // Now set a different renderer gain that will be cached (+3.0).
  SetGain(3.0f);
  SetOtherGain(-3.0f);
  amplitude_scale = gain_.GetGainScale();
  EXPECT_FLOAT_EQ(Gain::kUnityScale, amplitude_scale);

  // If Render gain is cached val of +3, then combo should be Unity.
  SetOtherGain(-3.0f);
  amplitude_scale = gain_.GetGainScale();
  EXPECT_FLOAT_EQ(Gain::kUnityScale, amplitude_scale);

  // Try another Output gain; with cached +3 this should equate to -6dB.
  SetOtherGain(-9.0f);
  EXPECT_FLOAT_EQ(expect_amplitude_scale, gain_.GetGainScale());

  // Render gain cached +3 and Output gain non-cached -3 should lead to Unity.
  SetOtherGain(-3.0f);
  EXPECT_FLOAT_EQ(Gain::kUnityScale, gain_.GetGainScale());
}
// Gain caches any previously set source gain, using it if needed.
// This verifies the default and caching behavior of the Gain object
TEST_F(SourceGainTest, GainCaching) { GainCachingChecks(); }
TEST_F(DestGainTest, GainCaching) { GainCachingChecks(); }

void GainBase::VerifyMinGain(float source_gain_db, float dest_gain_db) {
  SetGain(source_gain_db);
  SetOtherGain(dest_gain_db);

  EXPECT_FLOAT_EQ(Gain::kMuteScale, gain_.GetGainScale());

  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsSilent());
}
void GainBase::MinGainChecks() {
  // First, test for source/dest interactions.
  // if OutputGain <= kMinGainDb, scale must be 0, regardless of renderer gain.
  VerifyMinGain(-2 * Gain::kMinGainDb, Gain::kMinGainDb);

  // if renderer gain <= kMinGainDb, scale must be 0, regardless of Output gain.
  VerifyMinGain(Gain::kMinGainDb, Gain::kMaxGainDb * 1.2);

  // if sum of renderer gain and Output gain <= kMinGainDb, scale should be 0.
  // Output gain is just slightly above MinGain; renderer takes us below it.
  VerifyMinGain(-2.0f, Gain::kMinGainDb + 1.0f);

  // Next, test for source/dest interactions.
  // Check if source alone mutes.
  VerifyMinGain(Gain::kMinGainDb, Gain::kUnityGainDb);
  VerifyMinGain(Gain::kMinGainDb, Gain::kUnityGainDb + 1);
  // Check if dest alone mutes.
  VerifyMinGain(Gain::kUnityGainDb + 1, Gain::kMinGainDb);
  VerifyMinGain(Gain::kUnityGainDb, Gain::kMinGainDb);

  // Check if the combination mutes.
  VerifyMinGain(Gain::kMinGainDb / 2, Gain::kMinGainDb / 2);
}
// System independently limits stream and master/device Gains to kMinGainDb
// (-160dB). Assert scale is zero, if either (or combo) are kMinGainDb or less.
TEST_F(SourceGainTest, GainIsLimitedToMin) { MinGainChecks(); }
TEST_F(DestGainTest, GainIsLimitedToMin) { MinGainChecks(); }

void GainBase::VerifyMaxGain(float source_gain_db, float dest_gain_db) {
  SetGain(source_gain_db);
  SetOtherGain(dest_gain_db);

  EXPECT_FLOAT_EQ(Gain::kMaxScale, gain_.GetGainScale());
  EXPECT_FLOAT_EQ(Gain::kMaxGainDb, gain_.GetGainDb());

  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsSilent());
}
void GainBase::MaxGainChecks() {
  // Check if source or dest alone mutes.
  VerifyMaxGain(Gain::kMaxGainDb, Gain::kUnityGainDb);

  // Check if the combination mutes.
  VerifyMinGain(Gain::kMinGainDb / 2, Gain::kMinGainDb / 2);

  // One gain is just slightly below MaxGain; the other will take us above it.
  VerifyMaxGain(Gain::kMaxGainDb - 1.0f, 2.0f);

  // Stages are not clamped until they are combined
  VerifyMaxGain(Gain::kMaxGainDb + 1.0f, -1.0f);
}
// System independently limits stream and master/device Gains to kMinGainDb
// (-160dB). Assert scale is zero, if either (or combo) are kMinGainDb or less.
TEST_F(SourceGainTest, GainIsLimitedToMax) { MaxGainChecks(); }
TEST_F(DestGainTest, GainIsLimitedToMax) { MaxGainChecks(); }

void GainBase::SourceMuteChecks() {
  SetGain(0.0f);
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_EQ(gain_.GetGainScale(), Gain::kUnityScale);
  EXPECT_EQ(gain_.GetGainDb(), Gain::kUnityGainDb);

  gain_.SetSourceMute(false);
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_EQ(gain_.GetGainScale(), Gain::kUnityScale);
  EXPECT_EQ(gain_.GetGainDb(), Gain::kUnityGainDb);

  gain_.SetSourceMute(true);
  EXPECT_TRUE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_EQ(gain_.GetGainScale(), Gain::kMuteScale);
  EXPECT_LE(gain_.GetGainDb(), Gain::kMinGainDb);

  gain_.SetSourceMute(false);
  SetGainWithRamp(-10.0, zx::msec(25));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_EQ(gain_.GetGainScale(), Gain::kUnityScale);
  EXPECT_EQ(gain_.GetGainDb(), Gain::kUnityGainDb);

  gain_.SetSourceMute(true);
  EXPECT_TRUE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_EQ(gain_.GetGainScale(), Gain::kMuteScale);
  EXPECT_LE(gain_.GetGainDb(), Gain::kMinGainDb);
}
// source_mute control should affect IsSilent, IsUnity, IsRamping and GetGainScale appropriately.
TEST_F(SourceGainTest, SourceMuteOverridesGainAndRamp) { SourceMuteChecks(); }
TEST_F(DestGainTest, SourceMuteOverridesGainAndRamp) { SourceMuteChecks(); }

// Ramp-related tests
//
void GainBase::TestRampWithNoDuration() {
  SetGain(-11.0f);
  SetOtherGain(-1.0f);
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());

  SetGainWithRamp(+1.0f, zx::nsec(0));
  EXPECT_TRUE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsSilent());
}
// Setting a ramp with zero duration is the same as an immediate gain change.
TEST_F(SourceGainRampTest, SetRampWithNoDurationChangesCurrentGain) { TestRampWithNoDuration(); }
TEST_F(DestGainRampTest, SetRampWithNoDurationChangesCurrentGain) { TestRampWithNoDuration(); }

// Setting a ramp with non-zero duration does not take effect until Advance.
void GainBase::TestRampWithDuration() {
  SetGain(24.0f);
  SetOtherGain(-24.0f);
  EXPECT_TRUE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());

  SetGainWithRamp(Gain::kMinGainDb, zx::nsec(1));
  EXPECT_TRUE(gain_.GetGainScale() == Gain::kUnityScale);
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
}
// Setting a ramp with non-zero duration does not take effect until Advance.
TEST_F(SourceGainRampTest, SetRampWithDurationDoesntChangeCurrentGain) { TestRampWithDuration(); }
TEST_F(DestGainRampTest, SetRampWithDurationDoesntChangeCurrentGain) { TestRampWithDuration(); }

void GainBase::TestRampIntoSilence() {
  SetGain(0.0f);
  SetOtherGain(Gain::kMinGainDb + 1.0f);
  SetGainWithRamp(Gain::kMinGainDb + 1.0f, zx::sec(1));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsUnity());

  SetOtherGain(0.0f);
  SetGainWithRamp(Gain::kMinGainDb * 2, zx::sec(1));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsUnity());
}
// If we are ramping-down and already silent, IsSilent should remain true.
TEST_F(SourceGainRampTest, RampFromNonSilenceToSilenceIsNotSilent) { TestRampIntoSilence(); }
TEST_F(DestGainRampTest, RampFromNonSilenceToSilenceIsNotSilent) { TestRampIntoSilence(); }

void GainBase::TestRampOutOfSilence() {
  // Combined, we start in silence...
  SetGain(Gain::kMinGainDb + 10.f);
  SetOtherGain(-22.0f);
  EXPECT_TRUE(gain_.IsSilent());
  // ... and ramp out of it
  SetGainWithRamp(+22.0f, zx::sec(1));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());

  // The first stage, on its own, makes us silent...
  SetGain(Gain::kMinGainDb - 5.0f);
  SetOtherGain(0.0f);
  EXPECT_TRUE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());
  // ... but it ramps out of it.
  SetGainWithRamp(Gain::kMinGainDb + 1.0f, zx::sec(1));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsUnity());
}
// If we are ramping-down and already silent, IsSilent should remain true.
TEST_F(SourceGainRampTest, RampFromSilenceToNonSilenceIsNotSilent) { TestRampOutOfSilence(); }
TEST_F(DestGainRampTest, RampFromSilenceToNonSilenceIsNotSilent) { TestRampOutOfSilence(); }

void GainBase::TestRampFromSilenceToSilence() {
  // Both start and end are at/below kMinGainDb -- ramping up
  SetGain(Gain::kMinGainDb - 1.0f);
  SetGainWithRamp(Gain::kMinGainDb, zx::sec(1));
  EXPECT_TRUE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());

  // Both start and end are at/below kMinGainDb -- ramping down
  SetGainWithRamp(Gain::kMinGainDb - 2.0f, zx::sec(1));
  EXPECT_TRUE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());
}
// If the beginning and end of a ramp are both at/below min gain, it isn't ramping.
TEST_F(SourceGainRampTest, RampFromSilenceToSilenceIsNotRamping) { TestRampFromSilenceToSilence(); }
TEST_F(DestGainRampTest, RampFromSilenceToSilenceIsNotRamping) { TestRampFromSilenceToSilence(); }

void GainBase::TestRampsCombineForSilence() {
  // Both start and end are at/below kMinGainDb -- ramping up
  SetGain(Gain::kMinGainDb);
  SetOtherGain(Gain::kUnityGainDb);
  EXPECT_TRUE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());

  // Because our scalelinear ramps are not equal-power, we "bulge" at the midpoint of fades, thus
  // combined ramps may not be silent just because their endpoints are.
  SetGainWithRamp(Gain::kUnityGainDb, zx::sec(1));
  SetOtherGainWithRamp(Gain::kMinGainDb, zx::sec(1));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsRamping());
}
// If the beginning and end of a ramp are both at/below min gain, it isn't ramping.
TEST_F(SourceGainRampTest, RampsCombineForSilenceIsNotSilent) { TestRampsCombineForSilence(); }
TEST_F(DestGainRampTest, RampsCombineForSilenceIsNotSilent) { TestRampsCombineForSilence(); }

void GainBase::TestRampUnity() {
  SetGain(Gain::kUnityGainDb);
  SetOtherGain(Gain::kUnityGainDb);
  EXPECT_TRUE(gain_.IsUnity());

  SetGainWithRamp(-1.0f, zx::sec(1));

  // Expect pre-ramp conditions
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
}
// If a ramp is active/pending, then IsUnity should never be true.
TEST_F(SourceGainRampTest, RampIsNeverUnity) { TestRampUnity(); }
TEST_F(DestGainRampTest, RampIsNeverUnity) { TestRampUnity(); }

void GainBase::TestFlatRamp() {
  SetGain(Gain::kUnityGainDb);
  SetOtherGain(-20.0f);

  SetGainWithRamp(0.0f, zx::sec(1));

  // Expect pre-ramp conditions
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());

  // ... and a flat ramp should combine with the other side to equal Unity.
  SetOtherGain(0.0f);
  EXPECT_TRUE(gain_.IsUnity());
}
// If the beginning and end of a ramp are the same, it isn't ramping.
TEST_F(SourceGainRampTest, FlatIsntRamping) { TestFlatRamp(); }
TEST_F(DestGainRampTest, FlatIsntRamping) { TestFlatRamp(); }

void GainBase::TestRampWithMute() {
  SetGain(0.0f);
  SetGainWithRamp(-10.0, zx::msec(25));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsRamping());

  gain_.SetSourceMute(true);
  EXPECT_TRUE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());

  gain_.SetSourceMute(false);
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsRamping());
}
// If the beginning and end of a ramp are the same, it isn't ramping.
TEST_F(SourceGainRampTest, MuteOverridesRamp) { TestRampWithMute(); }
TEST_F(DestGainRampTest, MuteOverridesRamp) { TestRampWithMute(); }

void GainBase::TestAdvance() {
  SetGain(-150.0f);
  SetOtherGain(-13.0f);

  SetGainWithRamp(+13.0f, zx::nsec(1));

  // Advance far beyond end of ramp -- 10 msec (10 frames@1kHz) vs. 1 nsec.
  gain_.Advance(10, rate_1khz_output_);

  // Expect post-ramp conditions
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_TRUE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());
}
// Upon Advance, we should see a change in the instantaneous GetGainScale().
TEST_F(SourceGainRampTest, AdvanceChangesGain) { TestAdvance(); }
TEST_F(DestGainRampTest, AdvanceChangesGain) { TestAdvance(); }

void GainBase::TestSetGainCancelsRamp() {
  SetGain(-60.0f);
  SetOtherGain(-20.0f);
  EXPECT_FLOAT_EQ(gain_.GetGainDb(), -80.0f);

  SetGainWithRamp(-20.0f, zx::sec(1));
  EXPECT_TRUE(gain_.IsRamping());
  // Advance halfway through the ramp (500 frames, which at 1kHz is 500 ms).
  gain_.Advance(500, rate_1khz_output_);
  EXPECT_TRUE(gain_.IsRamping());

  SetGain(0.0f);
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_FLOAT_EQ(gain_.GetGainDb(), -20.0f);
}
// Setting a static gain during ramping should cancel the ramp
TEST_F(SourceGainRampTest, SetSourceGainCancelsRamp) { TestSetGainCancelsRamp(); }
TEST_F(DestGainRampTest, SetSourceGainCancelsRamp) { TestSetGainCancelsRamp(); }

void GainBase::TestRampsForSilence() {
  SetGain(-80.0f);
  SetOtherGain(-80.0f);
  SetGainWithRamp(-80.0f, zx::sec(1));
  // Flat ramp reverts to static gain combination
  EXPECT_TRUE(gain_.IsSilent());

  SetGainWithRamp(-90.0f, zx::sec(1));
  // Already below the silence threshold and ramping downward
  EXPECT_TRUE(gain_.IsSilent());

  SetGain(10.0f);
  SetOtherGain(Gain::kMinGainDb);
  SetGainWithRamp(12.0f, zx::sec(1));
  // Ramping upward, but other stage is below mute threshold
  EXPECT_TRUE(gain_.IsSilent());

  SetGain(Gain::kMinGainDb - 5.0f);
  SetOtherGain(10.0f);
  SetGainWithRamp(Gain::kMinGainDb, zx::sec(1));
  // Ramping upward, but to a target below mute threshold
  EXPECT_TRUE(gain_.IsSilent());
}
// Setting a static gain during ramping should cancel the ramp
TEST_F(SourceGainRampTest, WhenIsSilentShouldBeTrue) { TestRampsForSilence(); }
TEST_F(DestGainRampTest, WhenIsSilentShouldBeTrue) { TestRampsForSilence(); }

void GainBase::TestRampsForNonSilence() {
  SetGain(-79.0f);
  SetOtherGain(-80.0f);
  SetGainWithRamp(-90.0f, zx::sec(1));
  // Above the silence threshold but ramping downward
  EXPECT_FALSE(gain_.IsSilent());

  SetGain(-100.0f);
  SetOtherGain(-65.0f);
  SetGainWithRamp(-90.0f, zx::sec(1));
  // Below the silence threshold but ramping upward
  EXPECT_FALSE(gain_.IsSilent());

  SetGain(Gain::kMinGainDb - 5.0f);
  SetOtherGain(10.0f);
  SetGainWithRamp(Gain::kMinGainDb + 1.0f, zx::sec(1));
  // Ramping from below to above mute threshold
  EXPECT_FALSE(gain_.IsSilent());

  // The following case is not considered silence, but could be:
  //
  SetGain(-100.0f);
  SetOtherGain(-120.0f);
  SetGainWithRamp(-60.0f, zx::sec(1));
  EXPECT_FALSE(gain_.IsSilent());
}
TEST_F(SourceGainRampTest, WhenIsSilentShouldBeFalse) { TestRampsForNonSilence(); }
TEST_F(DestGainRampTest, WhenIsSilentShouldBeFalse) { TestRampsForNonSilence(); }

// ScaleArray-related tests
//
void GainBase::TestGetScaleArrayNoRamp() {
  Gain::AScale scale_arr[3];
  SetGain(-42.0f);
  SetOtherGain(-68.0f);

  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);
  Gain::AScale expect_scale = gain_.GetGainScale();

  EXPECT_THAT(scale_arr, Each(FloatEq(expect_scale)));

  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsSilent());
}
// If no ramp, all vals returned by GetScaleArray should equal GetGainScale().
TEST_F(SourceGainScaleArrayTest, GetScaleArrayNoRampEqualsGetScale) { TestGetScaleArrayNoRamp(); }
TEST_F(DestGainScaleArrayTest, GetScaleArrayNoRampEqualsGetScale) { TestGetScaleArrayNoRamp(); }

void GainBase::TestGetScaleArray() {
  Gain::AScale scale_arr[6];
  Gain::AScale expect_arr[6] = {1.0, 0.82, 0.64, 0.46, 0.28, 0.1};

  SetGainWithRamp(-20, zx::msec(5));
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Pointwise(FloatEq(), expect_arr));

  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsSilent());
}
// Validate when ramp and GetScaleArray are identical length.
TEST_F(SourceGainScaleArrayTest, GetScaleArrayRamp) { TestGetScaleArray(); }
TEST_F(DestGainScaleArrayTest, GetScaleArrayRamp) { TestGetScaleArray(); }

void GainBase::TestScaleArrayLongRamp() {
  Gain::AScale scale_arr[4];  // At 1kHz this is less than the ramp duration.
  Gain::AScale expect_arr[4] = {1.0, 0.901, 0.802, 0.703};

  SetGainWithRamp(-40, zx::msec(10));
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Pointwise(FloatEq(), expect_arr));

  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsSilent());
}
// Validate when ramp duration is greater than GetScaleArray.
TEST_F(SourceGainScaleArrayTest, GetScaleArrayLongRamp) { TestScaleArrayLongRamp(); }
TEST_F(DestGainScaleArrayTest, GetScaleArrayLongRamp) { TestScaleArrayLongRamp(); }

void GainBase::TestScaleArrayShortRamp() {
  Gain::AScale scale_arr[9];  // At 1kHz this is longer than the ramp duration.
  Gain::AScale expect_arr[9] = {1.0, 0.82, 0.64, 0.46, 0.28, 0.1, 0.1, 0.1, 0.1};

  SetGainWithRamp(-20, zx::msec(5));
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Pointwise(FloatEq(), expect_arr));

  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsSilent());
}
// Validate when ramp duration is shorter than GetScaleArray.
TEST_F(SourceGainScaleArrayTest, GetScaleArrayShortRamp) { TestScaleArrayShortRamp(); }
TEST_F(DestGainScaleArrayTest, GetScaleArrayShortRamp) { TestScaleArrayShortRamp(); }

void GainBase::TestScaleArrayWithoutAdvance() {
  SetGainWithRamp(-123.45678, zx::msec(9));

  Gain::AScale scale_arr[10];
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  Gain::AScale scale_arr2[10];
  gain_.GetScaleArray(scale_arr2, std::size(scale_arr2), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Pointwise(FloatEq(), scale_arr2));
}
// Successive GetScaleArray calls without Advance should return same results.
TEST_F(SourceGainScaleArrayTest, GetScaleArrayWithoutAdvance) { TestScaleArrayWithoutAdvance(); }
TEST_F(DestGainScaleArrayTest, GetScaleArrayWithoutAdvance) { TestScaleArrayWithoutAdvance(); }

void GainBase::TestScaleArrayBigAdvance() {
  Gain::AScale scale_arr[6];
  Gain::AScale expect = Gain::kUnityScale * 2;

  SetGainWithRamp(6.0205999, zx::msec(5));
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Not(Each(FloatEq(expect))));
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsSilent());

  gain_.Advance(rate_1khz_output_.Scale(ZX_SEC(10)), rate_1khz_output_);
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Each(FloatEq(expect)));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsUnity());
}
// Advances that exceed ramp durations should lead to end-to-ramp conditions.
TEST_F(SourceGainScaleArrayTest, GetScaleArrayBigAdvance) { TestScaleArrayBigAdvance(); }
TEST_F(DestGainScaleArrayTest, GetScaleArrayBigAdvance) { TestScaleArrayBigAdvance(); }

void GainBase::TestRampCompletion() {
  Gain::AScale scale_arr[6];
  Gain::AScale scale_arr2[6];

  constexpr float target_gain_db = -30.1029995;
  const float target_gain_scale = Gain::DbToScale(target_gain_db);

  // With a 5ms duration and 1 frame per ms, scale_arr will perfectly fit
  // each frame such that scale_arr[5] == target_gain_scale.
  SetGainWithRamp(target_gain_db, zx::msec(5));
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  for (size_t k = 0; k < std::size(scale_arr); k++) {
    const float diff = Gain::kUnityScale - target_gain_scale;
    const float want = Gain::kUnityScale - diff * static_cast<float>(k) / 5.0;
    EXPECT_FLOAT_EQ(want, scale_arr[k]) << "index " << k;
  }
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_EQ(Gain::kUnityGainDb, gain_.GetGainDb());
  EXPECT_EQ(Gain::kUnityScale, gain_.GetGainScale());

  // After clearing the ramp, scale_arr should be constant.
  CompleteRamp();
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Each(FloatEq(target_gain_scale)));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_EQ(target_gain_db, gain_.GetGainDb());
  EXPECT_EQ(target_gain_scale, gain_.GetGainScale());
  EXPECT_FLOAT_EQ(target_gain_db, gain_.GetGainDb());

  // Without a ramp, scale_arr should be constant even after Advance.
  gain_.Advance(10, rate_1khz_output_);
  gain_.GetScaleArray(scale_arr2, std::size(scale_arr2), rate_1khz_output_);

  EXPECT_THAT(scale_arr, Each(FloatEq(target_gain_scale)));
  EXPECT_FALSE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_EQ(target_gain_db, gain_.GetGainDb());
  EXPECT_EQ(target_gain_scale, gain_.GetGainScale());
}

// Completing a ramp should fast-forward any in-process ramps.
TEST_F(SourceGainScaleArrayTest, CompleteSourceRamp) { TestRampCompletion(); }
TEST_F(DestGainScaleArrayTest, CompleteDestRamp) { TestRampCompletion(); }

void GainBase::TestAdvanceHalfwayThroughRamp() {
  Gain::AScale scale_arr[4];  // At 1kHz this is less than the ramp duration.
  Gain::AScale expect_arr[4];

  SetGainWithRamp(-20.0f, zx::msec(9));
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

  Gain::AScale expect_scale = Gain::kUnityScale;
  EXPECT_FLOAT_EQ(gain_.GetGainScale(), expect_scale);

  // When comparing buffers, do it within the tolerance of 32-bit float
  for (auto& val : expect_arr) {
    val = expect_scale;
    expect_scale -= 0.1;
  }
  EXPECT_THAT(scale_arr, Pointwise(FloatEq(), expect_arr));
  EXPECT_FALSE(gain_.IsSilent());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_TRUE(gain_.IsRamping());

  // Advance only partially through the duration of the ramp.
  const auto kFramesToAdvance = 2;
  gain_.Advance(kFramesToAdvance, rate_1khz_output_);
  gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);
  // DisplayScaleVals(scale_arr, std::size(scale_arr));

  expect_scale = expect_arr[kFramesToAdvance];
  EXPECT_FLOAT_EQ(expect_scale, gain_.GetGainScale());

  for (auto& val : expect_arr) {
    val = expect_scale;
    expect_scale -= 0.1;
  }
  EXPECT_THAT(scale_arr, Pointwise(FloatEq(), expect_arr));
  EXPECT_TRUE(gain_.IsRamping());
  EXPECT_FALSE(gain_.IsUnity());
  EXPECT_FALSE(gain_.IsSilent());
}
// After partial Advance through a ramp, instantaneous gain should be accurate.
TEST_F(SourceGainScaleArrayTest, AdvanceHalfwayThroughRamp) { TestAdvanceHalfwayThroughRamp(); }
TEST_F(DestGainScaleArrayTest, AdvanceHalfwayThroughRamp) { TestAdvanceHalfwayThroughRamp(); }

// After partial Advance through a ramp, followed by a second ramp, the second ramp
// ramp should start where the first ramp left off.
void GainBase::TestSuccessiveRamps() {
  SetGainWithRamp(-20.0f, zx::msec(10));

  auto scale_start = Gain::kUnityScale;
  EXPECT_FLOAT_EQ(scale_start, gain_.GetGainScale());
  EXPECT_TRUE(gain_.IsRamping());

  // Advance only partially through the duration of the ramp.
  gain_.Advance(2, rate_1khz_output_);  // 1 frame == 1ms

  auto expect_scale = scale_start + (Gain::DbToScale(-20.f) - scale_start) * 2.0 / 10.0;
  EXPECT_FLOAT_EQ(expect_scale, gain_.GetGainScale());
  EXPECT_TRUE(gain_.IsRamping());

  // A new ramp should start at the same spot.
  SetGainWithRamp(-80.0f, zx::msec(10));

  scale_start = expect_scale;
  EXPECT_FLOAT_EQ(expect_scale, gain_.GetGainScale());
  EXPECT_TRUE(gain_.IsRamping());

  // Advance again.
  gain_.Advance(2, rate_1khz_output_);

  expect_scale = scale_start + (Gain::DbToScale(-80.f) - scale_start) * 2.0 / 10.0;
  EXPECT_FLOAT_EQ(expect_scale, gain_.GetGainScale());
  EXPECT_TRUE(gain_.IsRamping());
}
// After partial Advance through a ramp, followed by a second ramp, the second ramp
// ramp should start where the first ramp left off.
TEST_F(SourceGainScaleArrayTest, TwoRamps) { TestSuccessiveRamps(); }
TEST_F(DestGainScaleArrayTest, TwoRamps) { TestSuccessiveRamps(); }

void GainBase::TestCombinedRamps() {
  Gain::AScale scale_arr[11];

  {
    // Two arbitrary ramps of the same length, starting at the same time
    SetGainWithRamp(-20, zx::msec(10));
    SetOtherGainWithRamp(+10, zx::msec(10));
    gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

    // Source gain ramps linearly from 0 dB (scale 1.0) to -20 dB (0.1)
    // Dest gain ramps linearly from 0 dB (1.0) to 10 dB (3.16227766)
    //
    // source 1.0 0.91000 0.82000 0.73000 0.64000 0.55000 0.46000 0.37000 0.28000 0.19000 0.10000
    // dest   1.0 1.22623 1.43246 1.64868 1.86491 2.08114 2.29737 2.51359 2.72982 2.94605 3.16228
    //
    // These scale values are multiplied to get the following expect_arr
    Gain::AScale expect_arr[11] = {
        1.0,       1.1067673, 1.1746135, 1.2035388, 1.1935431, 1.1446264,
        1.0567886, 0.9300299, 0.7643502, 0.5597495, 0.3162278,
    };
    EXPECT_THAT(scale_arr, Pointwise(FloatEq(), expect_arr));
  }

  {
    // Now check two ramps of differing lengths and start times
    SetGain(0.0);
    SetOtherGain(-40);
    SetGainWithRamp(-80, zx::msec(10));
    gain_.Advance(5, rate_1khz_output_);

    // At the source-ramp midpoint, source * dest contributions are 0.50005 * 0.01
    EXPECT_FLOAT_EQ(gain_.GetGainScale(), 0.005000501f);
    SetOtherGainWithRamp(15, zx::msec(7));
    gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

    // source ramp continues onward, finalizing at 0.0001 on frame 5. dest ramp ends on frame 7 at
    // 5.6234133. They combine for 0.0005623413 which should be set for the remaining array.
    Gain::AScale expect_arr[11] = {
        0.005000501,   0.32481519,    0.48426268,    0.48334297,    0.32205606,    0.00040195809,
        0.00048214971, 0.00056234133, 0.00056234133, 0.00056234133, 0.00056234133,
    };
    EXPECT_THAT(scale_arr, Pointwise(FloatEq(), expect_arr));
  }
}

// Test that source-ramping and dest-ramping combines correctly
TEST_F(SourceGainScaleArrayTest, CombinedRamps) { TestCombinedRamps(); }
TEST_F(DestGainScaleArrayTest, CombinedRamps) { TestCombinedRamps(); }

void GainBase::TestCrossFades() {
  Gain::AScale scale_arr[11];

  constexpr float kInitialGainDb1 = -20.0f;
  constexpr float kInitialGainDb2 = 0.0f;
  constexpr float kGainChangeDb = 8.0f;
  for (size_t ramp_length = 4; ramp_length <= 8; ramp_length += 2) {
    SCOPED_TRACE("GainBase::TestCrossFades for ramp_length " + std::to_string(ramp_length));

    ASSERT_EQ(ramp_length % 2, 0u) << "Test miscalculation - test assumes ramp_length is even";

    // We set the two ramps with equal duration and offsetting gain-change.
    // Scale-linear crossfading is not equal-power, so although the initial and final gain_db values
    // are equal, the intervening values actually rise to a local max at fade's midpoint.
    SetGain(kInitialGainDb1);
    SetOtherGain(kInitialGainDb2);
    SetGainWithRamp(kInitialGainDb1 + kGainChangeDb, zx::msec(ramp_length));
    SetOtherGainWithRamp(kInitialGainDb2 - kGainChangeDb, zx::msec(ramp_length));
    gain_.GetScaleArray(scale_arr, std::size(scale_arr), rate_1khz_output_);

    // scale values are given below for the ramp_length = 4 case:
    // source 0.10000000  0.13779716  0.17559432  0.21339148  0.25118864  0.25118864 ...
    // dest   1.00000000  0.84952679  0.69905359  0.54858038  0.39810717  0.39810717 ...
    // multiplied to get:
    // expect 0.10000000  0.11706238  0.12274984  0.11706238  0.10000000  0.10000000 ...

    // Rather than comparing strictly, check the logical shape:
    // * At either end of the ramps, the gains are equal
    EXPECT_FLOAT_EQ(scale_arr[0], Gain::DbToScale(kInitialGainDb1 + kInitialGainDb2));
    EXPECT_FLOAT_EQ(scale_arr[ramp_length], scale_arr[0]);

    // * Gain increases monotonically to the midpoint of the ramps
    EXPECT_GT(scale_arr[ramp_length / 2 - 1], scale_arr[ramp_length / 2 - 2]);
    EXPECT_GT(scale_arr[ramp_length / 2], scale_arr[ramp_length / 2 - 1]);

    // * Gain decreases monotonically as we move beyond the midpoint of the ramps
    EXPECT_GT(scale_arr[ramp_length / 2], scale_arr[ramp_length / 2 + 1]);
    EXPECT_GT(scale_arr[ramp_length / 2 + 1], scale_arr[ramp_length / 2 + 2]);

    // * The end-ramp gain holds constant to the end of scale_arr
    EXPECT_FLOAT_EQ(scale_arr[std::size(scale_arr) - 1], scale_arr[ramp_length]);
  }
}
// Check two coincident ramps that offset each other. Because scale-linear ramping is not
// equal-power, the result won't be constant-gain, but it will have a predictable shape.
TEST_F(SourceGainScaleArrayTest, CrossFades) { TestCrossFades(); }
TEST_F(DestGainScaleArrayTest, CrossFades) { TestCrossFades(); }

}  // namespace media::audio::test
