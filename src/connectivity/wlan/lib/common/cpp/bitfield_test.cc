/*
 * Copyright (c) 2022 The Fuchsia Authors
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
 * SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
 * OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
 * CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include <array>

#include <wlan/common/bitfield.h>

#include "gtest/gtest.h"

namespace wlan::common {

namespace {

class ByteArray : public ByteArrayBitField<3> {
 public:
  explicit ByteArray(std::array<uint8_t, 3> raw) : ByteArrayBitField(raw) {}
  WLAN_BIT_FIELD(head, 0, 4)
  WLAN_BIT_FIELD(middle, 4, 17)
  WLAN_BIT_FIELD(bit, 21, 1)
  WLAN_BIT_FIELD(tail, 22, 2)
};

TEST(ByteArrayBitfield, ReadByteArrayBitfield) {
  ByteArray array({0b01100010, 0b10101111, 0b00000000});
  EXPECT_EQ(array.head(), 0b0000);
  EXPECT_EQ(array.middle(), 0b00010101011110000u);
  EXPECT_EQ(array.bit(), 0b1);
  EXPECT_EQ(array.tail(), 0b01);
}

TEST(ByteArrayBitfield, WriteByteArrayBitfield) {
  ByteArray array({});
  array.set_head(0b0000);
  array.set_middle(0b00010101011110000u);
  array.set_bit(0b1);
  array.set_tail(0b01);
  std::array<uint8_t, 3> expected = {0b01100010, 0b10101111, 0b00000000};

  ASSERT_EQ(array.val(), expected);
}

class ByteArray2 : public ByteArrayBitField<11> {
 public:
  explicit ByteArray2(std::array<uint8_t, 11> raw) : ByteArrayBitField(raw) {}
  WLAN_BIT_FIELD(u64, 8, 64)
  WLAN_BIT_FIELD(u32, 40, 32)
  WLAN_BIT_FIELD(u32_offset, 44, 32)
};

TEST(ByteArrayBitfield, ReadWriteLongOffsetField) {
  ByteArray2 array({});
  array.set_u32(0xffffffff);
  std::array<uint8_t, 11> expected = {0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0};
  EXPECT_EQ(array.val(), expected);
  EXPECT_EQ(array.u32(), 0xffffffff);

  ByteArray2 array_offset({});
  array_offset.set_u32_offset(0xffffffff);
  std::array<uint8_t, 11> expected_offset = {0, 0x0f, 0xff, 0xff, 0xff, 0xf0, 0, 0, 0, 0, 0};
  EXPECT_EQ(array_offset.val(), expected_offset);
  EXPECT_EQ(array_offset.u32_offset(), 0xffffffff);
}

TEST(ByteArrayBitfield, ReadWriteLongField) {
  ByteArray2 array({});
  array.set_u64(0xffffffffffffffff);
  std::array<uint8_t, 11> expected = {0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0};
  EXPECT_EQ(array.val(), expected);
  EXPECT_EQ(array.u64(), 0xffffffffffffffff);
}

}  // namespace

}  // namespace wlan::common
