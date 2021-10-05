// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/stdcompat/span.h>

#include <array>
#include <vector>

#include <gtest/gtest.h>

namespace wlan {

cpp20::span<const int> FuncThatTakesConstSpan(cpp20::span<const int> span) { return span; }

cpp20::span<int> FuncThatTakesSpan(cpp20::span<int> span) { return span; }

TEST(Span, DefaultConstructor) {
  cpp20::span<int> s;
  EXPECT_EQ(0u, s.size());
  EXPECT_TRUE(s.empty());
}

TEST(Span, CopyConstructor) {
  int x;
  cpp20::span<int> input(&x, 1);
  cpp20::span<int> output = FuncThatTakesSpan(input);
  EXPECT_EQ(&x, output.data());
  EXPECT_EQ(1u, output.size());
}

TEST(Span, ConstructFromTwoPointers) {
  int arr[3] = {};
  cpp20::span<int> s(arr, arr + 3);
  EXPECT_EQ(arr, s.data());
  EXPECT_EQ(3u, s.size());
}

TEST(Span, ImplicitConversionFromNonConstSpan) {
  int x;
  cpp20::span<int> input(&x, 1);
  cpp20::span<const int> output = FuncThatTakesConstSpan(input);
  EXPECT_EQ(&x, output.data());
  EXPECT_EQ(1u, output.size());
}

TEST(Span, ImplicitConversionFromArray) {
  int arr[3] = {10, 20, 30};
  {
    auto s = FuncThatTakesConstSpan(arr);
    EXPECT_EQ(arr, s.data());
    EXPECT_EQ(3u, s.size());
  }
  {
    auto s = FuncThatTakesSpan(arr);
    EXPECT_EQ(arr, s.data());
    EXPECT_EQ(3u, s.size());
  }

  const int const_arr[3] = {10, 20, 30};
  {
    auto s = FuncThatTakesConstSpan(const_arr);
    EXPECT_EQ(const_arr, s.data());
    EXPECT_EQ(3u, s.size());
  }
}

TEST(Span, ImplicitConversionFromStdArray) {
  std::array<int, 3> arr = {10, 20, 30};
  {
    auto s = FuncThatTakesConstSpan(arr);
    EXPECT_EQ(arr.data(), s.data());
    EXPECT_EQ(3u, s.size());
  }
  {
    auto s = FuncThatTakesSpan(arr);
    EXPECT_EQ(arr.data(), s.data());
    EXPECT_EQ(3u, s.size());
  }

  const std::array<int, 3> const_arr = {10, 20, 30};
  {
    auto s = FuncThatTakesConstSpan(const_arr);
    EXPECT_EQ(const_arr.data(), s.data());
    EXPECT_EQ(3u, s.size());
  }
}

TEST(Span, ImplicitConversionFromVector) {
  std::vector<int> vec = {10, 20, 30};
  {
    auto s = FuncThatTakesConstSpan(vec);
    EXPECT_EQ(vec.data(), s.data());
    EXPECT_EQ(3u, s.size());
  }
  {
    auto s = FuncThatTakesSpan(vec);
    EXPECT_EQ(vec.data(), s.data());
    EXPECT_EQ(3u, s.size());
  }

  const std::vector<int> const_vec = {10, 20, 30};
  {
    auto s = FuncThatTakesConstSpan(const_vec);
    EXPECT_EQ(const_vec.data(), s.data());
    EXPECT_EQ(3u, s.size());
  }
}

TEST(Span, SizeInBytes) {
  int32_t arr[2];
  cpp20::span<int32_t> s(arr, arr + 2);
  EXPECT_EQ(2u, s.size());
  EXPECT_EQ(8u, s.size_bytes());
}

TEST(Span, IndexOperator) {
  int arr[3] = {};
  cpp20::span<int> s(arr);
  EXPECT_EQ(&s[1], &arr[1]);
}

TEST(Span, RangeBasedFor) {
  const std::vector<int> input = {10, 20, 30};
  cpp20::span<const int> s(input);

  std::vector<int> output;
  for (int x : s) {
    output.push_back(x);
  }
  EXPECT_EQ(input, output);
}

TEST(Span, Subspan) {
  int arr[10] = {};
  cpp20::span<int> s(arr);
  cpp20::span<int> ss = s.subspan(3);
  EXPECT_EQ(arr + 3, ss.data());
  EXPECT_EQ(7u, ss.size());
}

TEST(Span, SubspanWithLength) {
  int arr[10] = {};
  cpp20::span<int> s(arr);
  cpp20::span<int> ss = s.subspan(3, 5);
  EXPECT_EQ(arr + 3, ss.data());
  EXPECT_EQ(5u, ss.size());
}

TEST(Span, AsBytes) {
  int32_t arr[3] = {};
  cpp20::span<int32_t> s(arr);
  cpp20::span<const std::byte> b = as_bytes(s);
  EXPECT_EQ(static_cast<void*>(arr), b.data());
  EXPECT_EQ(12u, b.size());
}

TEST(Span, AsWritableBytes) {
  int32_t arr[3] = {};
  cpp20::span<int32_t> s(arr);
  cpp20::span<std::byte> b = as_writable_bytes(s);
  EXPECT_EQ(static_cast<void*>(arr), b.data());
  EXPECT_EQ(12u, b.size());
}

}  // namespace wlan
