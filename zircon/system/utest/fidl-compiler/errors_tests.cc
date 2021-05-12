// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zxtest/zxtest.h>

#include "error_test.h"
#include "test_library.h"

namespace {

TEST(ErrorsTests, GoodError) {
  TestLibrary library(R"FIDL(
library example;

protocol Example {
    Method() -> (string foo) error int32;
};

)FIDL");

  ASSERT_TRUE(library.Compile());

  auto methods = &library.LookupProtocol("Example")->methods;
  ASSERT_EQ(methods->size(), 1);
  auto method = &methods->at(0);
  auto response = method->maybe_response_payload;
  ASSERT_NOT_NULL(response);
  ASSERT_EQ(response->members.size(), 1);
  auto response_member = &response->members.at(0);
  ASSERT_EQ(fidl::flat::GetType(response_member->type_ctor)->kind,
            fidl::flat::Type::Kind::kIdentifier);
  auto result_identifier = static_cast<const fidl::flat::IdentifierType*>(
      fidl::flat::GetType(response_member->type_ctor));
  const fidl::flat::Union* result_union =
      library.LookupUnion(std::string(result_identifier->name.decl_name()));
  ASSERT_NOT_NULL(result_union);
  ASSERT_NOT_NULL(result_union->attributes);
  ASSERT_TRUE(result_union->attributes->HasAttribute("result"));
  ASSERT_EQ(result_union->members.size(), 2);

  const auto& success = result_union->members.at(0);
  ASSERT_NOT_NULL(success.maybe_used);
  ASSERT_STR_EQ("response", std::string(success.maybe_used->name.data()).c_str());

  const fidl::flat::Union::Member& error = result_union->members.at(1);
  ASSERT_NOT_NULL(error.maybe_used);
  ASSERT_STR_EQ("err", std::string(error.maybe_used->name.data()).c_str());

  ASSERT_NOT_NULL(fidl::flat::GetType(error.maybe_used->type_ctor));
  ASSERT_EQ(fidl::flat::GetType(error.maybe_used->type_ctor)->kind,
            fidl::flat::Type::Kind::kPrimitive);
  auto primitive_type = static_cast<const fidl::flat::PrimitiveType*>(
      fidl::flat::GetType(error.maybe_used->type_ctor));
  ASSERT_EQ(primitive_type->subtype, fidl::types::PrimitiveSubtype::kInt32);
}

TEST(ErrorsTests, GoodErrorUnsigned) {
  TestLibrary library(R"FIDL(
library example;

protocol Example {
    Method() -> (string foo) error uint32;
};

)FIDL");

  ASSERT_TRUE(library.Compile());
}

TEST(ErrorsTests, GoodErrorEnum) {
  TestLibrary library(R"FIDL(
library example;

enum ErrorType : int32 {
    GOOD = 1;
    BAD = 2;
    UGLY = 3;
};

protocol Example {
    Method() -> (string foo) error ErrorType;
};

)FIDL");

  ASSERT_TRUE(library.Compile());
}

TEST(ErrorsTests, GoodErrorEnumAfter) {
  TestLibrary library(R"FIDL(
library example;

protocol Example {
    Method() -> (string foo) error ErrorType;
};

enum ErrorType : int32 {
    GOOD = 1;
    BAD = 2;
    UGLY = 3;
};

)FIDL");

  ASSERT_TRUE(library.Compile());
}

TEST(ErrorsTests, BadErrorUnknownIdentifier) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;

protocol Example {
    Method() -> (foo string) error ErrorType;
};
)FIDL",
                      experimental_flags);

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnknownType);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "ErrorType");
}

TEST(ErrorsTests, BadErrorUnknownIdentifierOld) {
  TestLibrary library(R"FIDL(
library example;

protocol Example {
    Method() -> (string foo) error ErrorType;
};
)FIDL");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnknownType);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "ErrorType");
}

TEST(ErrorsTests, BadErrorWrongPrimitive) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;

protocol Example {
    Method() -> (foo string) error float32;
};
)FIDL",
                      experimental_flags);

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidErrorType);
}

TEST(ErrorsTests, BadErrorWrongPrimitiveOld) {
  TestLibrary library(R"FIDL(
library example;

protocol Example {
    Method() -> (string foo) error float32;
};
)FIDL");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidErrorType);
}

TEST(ErrorsTests, BadErrorMissingType) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;
protocol Example {
    Method() -> (flub int32) error;
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorMissingTypeOld) {
  TestLibrary library(R"FIDL(
library example;
protocol Example {
    Method() -> (int32 flub) error;
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorNotAType) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;
protocol Example {
    Method() -> (flub int32) error "hello";
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorNotATypeOld) {
  TestLibrary library(R"FIDL(
library example;
protocol Example {
    Method() -> (int32 flub) error "hello";
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorNoResponse) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;
protocol Example {
    Method() -> error int32;
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorNoResponseOld) {
  TestLibrary library(R"FIDL(
library example;
protocol Example {
    Method() -> error int32;
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorUnexpectedEndOfFile) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;
type ForgotTheSemicolon = table {}
)FIDL",
                      experimental_flags);

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorUnexpectedEndOfFileOld) {
  TestLibrary library(R"FIDL(
library example;
table ForgotTheSemicolon {}
)FIDL");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ErrorsTests, BadErrorEmptyFile) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library("", experimental_flags);

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedIdentifier);
}

TEST(ErrorsTests, BadErrorEmptyFileOld) {
  TestLibrary library("");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedIdentifier);
}
}  // namespace
