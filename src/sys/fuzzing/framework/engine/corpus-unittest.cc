// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/sys/fuzzing/framework/engine/corpus.h"

#include <thread>
#include <unordered_set>

#include <gtest/gtest.h>

namespace fuzzing {
namespace {

// Test fixtures.

Input input0() { return Input(); }
Input input1() { return Input({0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48}); }
Input input2() { return Input({0x21, 0x22}); }
Input input3() { return Input({0x31, 0x32, 0x33, 0x34, 0x35, 0x36}); }
Input input4() { return Input({0x41, 0x42, 0x43, 0x44}); }

std::shared_ptr<Options> DefaultOptions() {
  auto options = std::make_shared<Options>();
  Corpus::AddDefaults(options.get());
  return options;
}

void AddAllToCorpus(Corpus *corpus) {
  ASSERT_EQ(corpus->Add(input1()), ZX_OK);
  ASSERT_EQ(corpus->Add(input2()), ZX_OK);
  ASSERT_EQ(corpus->Add(input3()), ZX_OK);
  ASSERT_EQ(corpus->Add(input4()), ZX_OK);
}

// Unit tests.

TEST(CorpusTest, AddDefaults) {
  Options options;
  Corpus::AddDefaults(&options);
  EXPECT_EQ(options.seed(), kDefaultSeed);
  EXPECT_EQ(options.max_input_size(), kDefaultMaxInputSize);
}

TEST(CorpusTest, AddInputs) {
  Corpus corpus;
  auto options = DefaultOptions();
  options->set_max_input_size(8);
  corpus.Configure(options);

  // Empty input is implicitly included.
  EXPECT_EQ(corpus.num_inputs(), 1U);
  EXPECT_EQ(corpus.total_size(), 0U);

  size_t expected = input1().size() + input2().size();
  EXPECT_EQ(corpus.Add(input1()), ZX_OK);
  EXPECT_EQ(corpus.Add(input2()), ZX_OK);

  EXPECT_EQ(corpus.num_inputs(), 3U);
  EXPECT_EQ(corpus.total_size(), expected);

  // Empty inputs are not added.
  EXPECT_EQ(corpus.Add(input0()), ZX_OK);
  EXPECT_EQ(corpus.num_inputs(), 3U);
  EXPECT_EQ(corpus.total_size(), expected);

  // Over-large inputs return an error.
  Input large_input;
  large_input.Resize(9);
  EXPECT_EQ(corpus.Add(std::move(large_input)), ZX_ERR_BUFFER_TOO_SMALL);
  EXPECT_EQ(corpus.num_inputs(), 3U);
  EXPECT_EQ(corpus.total_size(), expected);

  // Adding an existing input doesn't change the  number of inputs or total size.
  EXPECT_EQ(corpus.Add(input1()), ZX_OK);
  EXPECT_EQ(corpus.Add(input2()), ZX_OK);
  EXPECT_EQ(corpus.num_inputs(), 3U);
  EXPECT_EQ(corpus.total_size(), expected);
}

TEST(CorpusTest, At) {
  Corpus corpus;
  corpus.Configure(DefaultOptions());

  // Empty input is always present.
  Input input;
  EXPECT_TRUE(corpus.At(0, &input));
  EXPECT_EQ(input.ToHex(), input0().ToHex());

  // Add some elements.
  AddAllToCorpus(&corpus);

  // Corpus should been in sorted order: shortest to longest.
  EXPECT_TRUE(corpus.At(0, &input));
  EXPECT_EQ(input.ToHex(), input0().ToHex());

  EXPECT_TRUE(corpus.At(1, &input));
  EXPECT_EQ(input.ToHex(), input2().ToHex());

  EXPECT_TRUE(corpus.At(2, &input));
  EXPECT_EQ(input.ToHex(), input4().ToHex());

  EXPECT_TRUE(corpus.At(3, &input));
  EXPECT_EQ(input.ToHex(), input3().ToHex());

  EXPECT_TRUE(corpus.At(4, &input));
  EXPECT_EQ(input.ToHex(), input1().ToHex());

  // Out-of-bounds returns empty input.
  EXPECT_FALSE(corpus.At(5, &input));
  EXPECT_EQ(input.ToHex(), input0().ToHex());
}

TEST(CorpusTest, Pick) {
  Corpus corpus;

  // Set the seed explicitly. In the real system, omitting the seed option will cause the engine to
  // derive one from the current time.
  auto options = DefaultOptions();
  options->set_seed(100);
  corpus.Configure(options);

  // Corpus always has an empty input.
  Input input;
  corpus.Pick(&input);
  EXPECT_EQ(input.ToHex(), input0().ToHex());

  // |Pick| doesn't exhaust, but does shuffle.
  AddAllToCorpus(&corpus);
  std::vector<std::string> ordered_a;
  for (size_t i = 0; i < 100; ++i) {
    corpus.Pick(&input);
    ordered_a.push_back(input.ToHex());
  }
  std::vector<std::string> ordered_b;
  for (size_t i = 0; i < 100; ++i) {
    corpus.Pick(&input);
    ordered_b.push_back(input.ToHex());
  }
  std::unordered_set<std::string> unique_a(ordered_a.begin(), ordered_a.end());
  std::unordered_set<std::string> unique_b(ordered_b.begin(), ordered_b.end());

  // The loop above should pick all inputs, but in different order. These assertions are very likely
  // but not guaranteed for an arbitrary seed. For the given seed, they work.
  EXPECT_EQ(unique_a.size(), corpus.num_inputs());
  EXPECT_EQ(unique_b.size(), corpus.num_inputs());
  EXPECT_NE(ordered_a, ordered_b);
}

TEST(CorpusTest, PickIsDeterministic) {
  Corpus corpus1;
  Corpus corpus2;

  // Set the seed explicitly. In the real system, omitting the seed option will cause the engine to
  // derive one from the current time.
  auto options = DefaultOptions();
  options->set_seed(100);
  corpus1.Configure(options);
  corpus2.Configure(options);

  // Same seed and inputs should produce same order.
  AddAllToCorpus(&corpus1);
  AddAllToCorpus(&corpus2);

  Input input1;
  Input input2;
  for (size_t i = 0; i < 100; ++i) {
    corpus1.Pick(&input1);
    corpus2.Pick(&input2);
    EXPECT_EQ(input1.ToHex(), input2.ToHex());
  }
}

}  // namespace
}  // namespace fuzzing
