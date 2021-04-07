// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zxtest/zxtest.h>

#include "error_test.h"
#include "fidl/diagnostics.h"
#include "test_library.h"

namespace {

TEST(RecoverableCompilationTests, BadRecoverInLibraryConsume) {
  TestLibrary library(R"FIDL(
library example;

protocol P {};
protocol P {};      // Error: name collision

table Table {
    1: string? s;   // Error: nullable table member
};

union Union {
    1: string? s;   // Error: nullable union member
};
)FIDL");
  EXPECT_FALSE(library.Compile());
  const auto& errors = library.errors();
  ASSERT_EQ(errors.size(), 3);
  ASSERT_ERR(errors[0], fidl::ErrNameCollision);
  ASSERT_ERR(errors[1], fidl::ErrNullableTableMember);
  ASSERT_ERR(errors[2], fidl::ErrNullableUnionMember);
}

TEST(RecoverableCompilationTests, BadRecoverInLibraryCompile) {
  TestLibrary library(R"FIDL(
library example;

union Union {
    1: string string_value;
    2: UnknownType unknown_value; // Error: unknown type
};

enum Enum {
    ZERO = 0;
    ONE = 1;
    TWO = 1;                      // Error: duplicate value
    THREE = 3;
};

enum OtherEnum {
    NONE = 0;
    ONE = 1;
    ONE = 2;                      // Error: duplicate name
};

table NonDenseTable {
    1: string s;
    3: uint8 b;                   // Error: non-dense ordinals
};
)FIDL");
  EXPECT_FALSE(library.Compile());
  const auto& errors = library.errors();
  ASSERT_EQ(errors.size(), 4);
  ASSERT_ERR(errors[0], fidl::ErrUnknownType);
  ASSERT_ERR(errors[1], fidl::ErrDuplicateMemberName);
  ASSERT_ERR(errors[2], fidl::ErrNonDenseOrdinal);
  ASSERT_ERR(errors[3], fidl::ErrDuplicateMemberValue);
}

TEST(RecoverableCompilationTests, BadRecoverInLibraryVerifyAttributes) {
  TestLibrary library(R"FIDL(
library example;

[ForDeprecatedCBindings = "True"]  // Error: invalid placement & value
union Union {
    1: string foo;
};

[Transitional]        // Error: invalid placement
table Table {
    1: string foo;
};

[MaxBytes = "1"]      // Error: too large
struct Struct {
    uint16 foo;
};

)FIDL");
  EXPECT_FALSE(library.Compile());
  const auto& errors = library.errors();
  ASSERT_EQ(errors.size(), 4);
  ASSERT_ERR(errors[0], fidl::ErrInvalidAttributePlacement);
  ASSERT_ERR(errors[1], fidl::ErrInvalidAttributeValue);
  ASSERT_ERR(errors[2], fidl::ErrInvalidAttributePlacement);
  ASSERT_ERR(errors[3], fidl::ErrTooManyBytes);
}

}  // namespace
