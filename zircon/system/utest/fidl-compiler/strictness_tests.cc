// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/flat_ast.h>
#include <fidl/lexer.h>
#include <fidl/parser.h>
#include <fidl/source_file.h>
#include <zxtest/zxtest.h>

#include "error_test.h"
#include "test_library.h"

namespace {

TEST(StrictnessTests, BadDuplicateModifier) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;

type One = strict union { 1: b bool; };
type Two = strict strict union { 1: b bool; };          // line 5
type Three = strict strict strict union { 1: b bool; }; // line 6
  )FIDL",
                      std::move(experimental_flags));
  ASSERT_FALSE(library.Compile());

  const auto& errors = library.errors();
  ASSERT_EQ(errors.size(), 3);
  ASSERT_ERR(errors[0], fidl::ErrDuplicateModifier);
  EXPECT_EQ(errors[0]->span->position().line, 5);
  ASSERT_SUBSTR(errors[0]->msg.c_str(), "strict");
  ASSERT_ERR(errors[1], fidl::ErrDuplicateModifier);
  EXPECT_EQ(errors[1]->span->position().line, 6);
  ASSERT_SUBSTR(errors[1]->msg.c_str(), "strict");
  ASSERT_ERR(errors[2], fidl::ErrDuplicateModifier);
  EXPECT_EQ(errors[2]->span->position().line, 6);
  ASSERT_SUBSTR(errors[2]->msg.c_str(), "strict");
}

TEST(StrictnessTests, BadConflictingModifiers) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;

type SF = strict flexible union { 1: b bool; }; // line 4
type FS = flexible strict union { 1: b bool; }; // line 5
  )FIDL",
                      std::move(experimental_flags));
  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrConflictingModifier,
                                      fidl::ErrConflictingModifier);
  EXPECT_EQ(library.errors()[0]->span->position().line, 4);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "strict");
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "flexible");
  EXPECT_EQ(library.errors()[1]->span->position().line, 5);
  ASSERT_SUBSTR(library.errors()[1]->msg.c_str(), "strict");
  ASSERT_SUBSTR(library.errors()[1]->msg.c_str(), "flexible");
}

TEST(StrictnessTests, GoodBitsStrictness) {
  TestLibrary library(
      R"FIDL(
library example;

bits DefaultStrictFoo {
    BAR = 0x1;
};

strict bits StrictFoo {
    BAR = 0x1;
};

flexible bits FlexibleFoo {
    BAR = 0x1;
};

)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
  EXPECT_EQ(library.LookupBits("FlexibleFoo")->strictness, fidl::types::Strictness::kFlexible);
  EXPECT_EQ(library.LookupBits("StrictFoo")->strictness, fidl::types::Strictness::kStrict);
  EXPECT_EQ(library.LookupBits("DefaultStrictFoo")->strictness, fidl::types::Strictness::kStrict);
}

TEST(StrictnessTests, GoodEnumStrictness) {
  TestLibrary library(
      R"FIDL(
library example;

enum DefaultStrictFoo {
    BAR = 1;
};

strict enum StrictFoo {
    BAR = 1;
};

flexible enum FlexibleFoo {
    BAR = 1;
};

)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
  EXPECT_EQ(library.LookupEnum("FlexibleFoo")->strictness, fidl::types::Strictness::kFlexible);
  EXPECT_EQ(library.LookupEnum("StrictFoo")->strictness, fidl::types::Strictness::kStrict);
  EXPECT_EQ(library.LookupEnum("DefaultStrictFoo")->strictness, fidl::types::Strictness::kStrict);
}

TEST(StrictnessTests, GoodFlexibleEnum) {
  TestLibrary library(R"FIDL(
library example;

flexible enum Foo {
  BAR = 1;
};
)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(StrictnessTests, GoodFlexibleBitsRedundant) {
  TestLibrary library(R"FIDL(
library example;

flexible bits Foo {
  BAR = 0x1;
};
)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(StrictnessTests, BadStrictnessStruct) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;

type Foo = strict struct {
    i int32;
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrCannotSpecifyModifier);
}

TEST(StrictnessTests, BadStrictnessTable) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library("table", R"FIDL(
library example;

type StrictFoo = strict table {};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrCannotSpecifyModifier);
}

TEST(StrictnessTests, GoodUnionStrictness) {
  TestLibrary library(R"FIDL(
library example;

union Foo {
    1: int32 i;
};

flexible union FlexibleFoo {
    1: int32 i;
};

strict union StrictFoo {
    1: int32 i;
};

)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
  EXPECT_EQ(library.LookupUnion("Foo")->strictness, fidl::types::Strictness::kStrict);
  EXPECT_EQ(library.LookupUnion("FlexibleFoo")->strictness, fidl::types::Strictness::kFlexible);
  EXPECT_EQ(library.LookupUnion("StrictFoo")->strictness, fidl::types::Strictness::kStrict);
}

TEST(StrictnessTests, GoodStrictUnionRedundant) {
  TestLibrary library(R"FIDL(
library example;

strict union Foo {
  1: int32 i;
};

)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
  ASSERT_EQ(library.LookupUnion("Foo")->strictness, fidl::types::Strictness::kStrict);
}

}  // namespace
