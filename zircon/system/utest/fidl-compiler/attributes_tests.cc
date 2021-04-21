// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/diagnostics.h>
#include <fidl/flat_ast.h>
#include <fidl/lexer.h>
#include <fidl/parser.h>
#include <fidl/reporter.h>
#include <fidl/source_file.h>
#include <zxtest/zxtest.h>

#include "error_test.h"
#include "test_library.h"

namespace {

TEST(AttributesTests, GoodPlacementOfAttributes) {
  SharedAmongstLibraries shared;
  TestLibrary dependency("exampleusing.fidl", R"FIDL(
library exampleusing;

struct Empty {};

)FIDL",
                         &shared);
  ASSERT_TRUE(dependency.Compile());

  TestLibrary library("example.fidl", R"FIDL(
[OnLibrary]
library example;

using exampleusing;

[OnBits]
bits ExampleBits {
    [OnBitsMember]
    MEMBER = 1;
};

[OnConst]
const uint32 EXAMPLE_CONST = 0;

[OnEnum]
enum ExampleEnum {
    [OnEnumMember]
    MEMBER = 1;
};

[OnProtocol]
protocol ExampleProtocol {
    [OnMethod]
    Method([OnParameter] exampleusing.Empty arg);
};

[OnService]
service ExampleService {
    [OnServiceMember]
    ExampleProtocol member;
};

[OnStruct]
struct ExampleStruct {
    [OnStructMember]
    uint32 member;
};

[OnTable]
table ExampleTable {
    [OnTableMember]
    1: uint32 member;
};

[OnTypeAlias]
alias ExampleTypeAlias = uint32;

[OnUnion]
union ExampleUnion {
    [OnUnionMember]
    1: uint32 variant;
};

)FIDL",
                      &shared);
  ASSERT_TRUE(library.AddDependentLibrary(std::move(dependency)));
  ASSERT_TRUE(library.Compile());

  EXPECT_TRUE(library.library()->HasAttribute("OnLibrary"));

  auto example_bits = library.LookupBits("ExampleBits");
  ASSERT_NOT_NULL(example_bits);
  EXPECT_TRUE(example_bits->attributes->HasAttribute("OnBits"));
  EXPECT_TRUE(example_bits->members.front().attributes->HasAttribute("OnBitsMember"));

  auto example_const = library.LookupConstant("EXAMPLE_CONST");
  ASSERT_NOT_NULL(example_const);
  EXPECT_TRUE(example_const->attributes->HasAttribute("OnConst"));

  auto example_enum = library.LookupEnum("ExampleEnum");
  ASSERT_NOT_NULL(example_enum);
  EXPECT_TRUE(example_enum->attributes->HasAttribute("OnEnum"));
  EXPECT_TRUE(example_enum->members.front().attributes->HasAttribute("OnEnumMember"));

  auto example_protocol = library.LookupProtocol("ExampleProtocol");
  ASSERT_NOT_NULL(example_protocol);
  EXPECT_TRUE(example_protocol->attributes->HasAttribute("OnProtocol"));
  EXPECT_TRUE(example_protocol->methods.front().attributes->HasAttribute("OnMethod"));
  ASSERT_NOT_NULL(example_protocol->methods.front().maybe_request);
  EXPECT_TRUE(
      example_protocol->methods.front().maybe_request->members.front().attributes->HasAttribute(
          "OnParameter"));

  auto example_service = library.LookupService("ExampleService");
  ASSERT_NOT_NULL(example_service);
  EXPECT_TRUE(example_service->attributes->HasAttribute("OnService"));
  EXPECT_TRUE(example_service->members.front().attributes->HasAttribute("OnServiceMember"));

  auto example_struct = library.LookupStruct("ExampleStruct");
  ASSERT_NOT_NULL(example_struct);
  EXPECT_TRUE(example_struct->attributes->HasAttribute("OnStruct"));
  EXPECT_TRUE(example_struct->members.front().attributes->HasAttribute("OnStructMember"));

  auto example_table = library.LookupTable("ExampleTable");
  ASSERT_NOT_NULL(example_table);
  EXPECT_TRUE(example_table->attributes->HasAttribute("OnTable"));
  EXPECT_TRUE(example_table->members.front().maybe_used->attributes->HasAttribute("OnTableMember"));

  auto example_type_alias = library.LookupTypeAlias("ExampleTypeAlias");
  ASSERT_NOT_NULL(example_type_alias);
  EXPECT_TRUE(example_type_alias->attributes->HasAttribute("OnTypeAlias"));

  auto example_union = library.LookupUnion("ExampleUnion");
  ASSERT_NOT_NULL(example_union);
  EXPECT_TRUE(example_union->attributes->HasAttribute("OnUnion"));
  EXPECT_TRUE(example_union->members.front().maybe_used->attributes->HasAttribute("OnUnionMember"));
}

TEST(AttributesTests, BadNoAttributeOnUsingNotEventDoc) {
  TestLibrary library(R"FIDL(
library example;

/// nope
[NoAttributeOnUsing, EvenDoc]
using we.should.not.care;

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrAttributesNotAllowedOnLibraryImport);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Doc");
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "NoAttributeOnUsing");
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "EvenDoc");
}

// Test that a duplicate attribute is caught, and nicely reported.
TEST(AttributesTests, BadNoTwoSameAttributeTest) {
  TestLibrary library("dup_attributes.fidl", R"FIDL(
library fidl.test.dupattributes;

[dup = "first", dup = "second"]
protocol A {
    MethodA();
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrDuplicateAttribute);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "dup");
}

// Test that doc comments and doc attributes clash are properly checked.
TEST(AttributesTests, BadNoTwoSameDocAttributeTest) {
  TestLibrary library("dup_attributes.fidl", R"FIDL(
library fidl.test.dupattributes;

/// first
[Doc = "second"]
protocol A {
    MethodA();
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrDuplicateAttribute);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Doc");
}

// Test that TODO
TEST(AttributesTests, BadNoTwoSameAttributeOnLibraryTest) {
  TestLibrary library;
  library.AddSource("dup_attributes.fidl", R"FIDL(
[dup = "first"]
library fidl.test.dupattributes;

)FIDL");
  library.AddSource("dup_attributes_second.fidl", R"FIDL(
[dup = "second"]
library fidl.test.dupattributes;

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrDuplicateAttribute);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "dup");
}

// Test that a close attribute is caught.
TEST(AttributesTests, BadWarnOnCloseAttributeTest) {
  TestLibrary library("dup_attributes.fidl", R"FIDL(
library fidl.test.dupattributes;

[Duc = "should be Doc"]
protocol A {
    MethodA();
};

)FIDL");
  EXPECT_TRUE(library.Compile());
  const auto& warnings = library.warnings();
  ASSERT_EQ(warnings.size(), 1);
  ASSERT_ERR(warnings[0], fidl::WarnAttributeTypo);
  ASSERT_SUBSTR(warnings[0]->msg.c_str(), "Duc");
  ASSERT_SUBSTR(warnings[0]->msg.c_str(), "Doc");
}

// This tests our ability to treat warnings as errors.  It is here because this
// is the most convenient warning.
TEST(AttributesTests, BadWarningsAsErrorsTest) {
  TestLibrary library("dup_attributes.fidl", R"FIDL(
library fidl.test.dupattributes;

[Duc = "should be Doc"]
protocol A {
    MethodA();
};

)FIDL");
  library.set_warnings_as_errors(true);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::WarnAttributeTypo);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Duc");
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Doc");
  ASSERT_EQ(library.warnings().size(), 0);
}

TEST(AttributesTests, BadEmptyTransport) {
  TestLibrary library("transport_attribuets.fidl", R"FIDL(
library fidl.test.transportattributes;

[Transport]
protocol A {
    MethodA();
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidTransportType);
}

TEST(AttributesTests, BogusTransport) {
  TestLibrary library("transport_attribuets.fidl", R"FIDL(
library fidl.test.transportattributes;

[Transport = "Bogus"]
protocol A {
    MethodA();
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidTransportType);
}

TEST(AttributesTests, GoodChannelTransport) {
  TestLibrary library("transport_attribuets.fidl", R"FIDL(
library fidl.test.transportattributes;

[Transport = "Channel"]
protocol A {
    MethodA();
};

)FIDL");
  EXPECT_TRUE(library.Compile());
  ASSERT_EQ(library.errors().size(), 0);
  ASSERT_EQ(library.warnings().size(), 0);
}

TEST(AttributesTests, GoodSyscallTransport) {
  TestLibrary library("transport_attributes.fidl", R"FIDL(
library fidl.test.transportattributes;

[Transport = "Syscall"]
protocol A {
    MethodA();
};

)FIDL");
  EXPECT_TRUE(library.Compile());
  ASSERT_EQ(library.errors().size(), 0);
  ASSERT_EQ(library.warnings().size(), 0);
}

TEST(AttributesTests, GoodMultipleTransports) {
  TestLibrary library("transport_attribuets.fidl", R"FIDL(
library fidl.test.transportattributes;

[Transport = "Channel, Syscall"]
protocol A {
    MethodA();
};

)FIDL");
  EXPECT_TRUE(library.Compile());
  ASSERT_EQ(library.errors().size(), 0);
  ASSERT_EQ(library.warnings().size(), 0);
}

TEST(AttributesTests, BadMultipleTransportsWithBogus) {
  TestLibrary library("transport_attribuets.fidl", R"FIDL(
library fidl.test.transportattributes;

[Transport = "Channel, Bogus, Syscall"]
protocol A {
    MethodA();
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidTransportType);
}

TEST(AttributesTests, BadTransitionalInvalidPlacement) {
  TestLibrary library(R"FIDL(
library fidl.test;

[Transitional]
protocol MyProtocol {
  MyMethod();
};
  )FIDL");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidAttributePlacement);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Transitional");
}

TEST(AttributesTests, BadUnknownInvalidPlacementOnUnion) {
  TestLibrary library("library fidl.test; [Unknown] flexible union U { 1: int32 a; };");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidAttributePlacement);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Unknown");
}

TEST(AttributesTests, BadUnknownInvalidPlacementOnBitsMember) {
  TestLibrary library("library fidl.test; flexible bits B : uint32 { [Unknown] A = 0x1; };");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidAttributePlacement);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Unknown");
}

TEST(AttributesTests, BadUnknownInvalidOnStrictUnionsEnums) {
  {
    TestLibrary library("library fidl.test; strict union U { [Unknown] 1: int32 a; };");
    ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnknownAttributeOnInvalidType);
    ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Unknown");
  }

  {
    TestLibrary library("library fidl.test; strict enum E : uint32 { [Unknown] A = 1; };");
    ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnknownAttributeOnInvalidType);
    ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "Unknown");
  }
}

TEST(AttributesTests, GoodUnknownOkOnFlexibleOrTransitionalEnumsUnionMembers) {
  {
    TestLibrary library("library fidl.test; flexible union U { [Unknown] 1: int32 a; };");
    EXPECT_TRUE(library.Compile());
  }

  {
    TestLibrary library(
        "library fidl.test; [Transitional] strict union U { [Unknown] 1: int32 a; };");
    EXPECT_TRUE(library.Compile());
  }

  {
    TestLibrary library("library fidl.test; flexible enum E : uint32 { [Unknown] A = 1; };");
    EXPECT_TRUE(library.Compile());
  }

  {
    TestLibrary library(
        "library fidl.test; [Transitional] strict enum E : uint32 { [Unknown] A = 1; };");
    EXPECT_TRUE(library.Compile());
  }
}

TEST(AttributesTests, BadIncorrectPlacementLayout) {
  TestLibrary library(R"FIDL(
[ForDeprecatedCBindings]
library fidl.test;

[ForDeprecatedCBindings]
const int32 MyConst = 0;

[ForDeprecatedCBindings]
enum MyEnum {
    [ForDeprecatedCBindings]
    MyMember = 5;
};

struct MyStruct {
    [ForDeprecatedCBindings]
    int32 MyMember;
};

[ForDeprecatedCBindings]
union MyUnion {
    [ForDeprecatedCBindings]
    1: int32 MyMember;
};

[ForDeprecatedCBindings]
table MyTable {
    [ForDeprecatedCBindings]
    1: int32 MyMember;
};

[ForDeprecatedCBindings]
protocol MyProtocol {
    [ForDeprecatedCBindings]
    MyMethod();
};

)FIDL");
  EXPECT_FALSE(library.Compile());
  const auto& errors = library.errors();
  ASSERT_EQ(errors.size(), 10);
  ASSERT_ERR(errors[0], fidl::ErrInvalidAttributePlacement);
  ASSERT_SUBSTR(errors[0]->msg.c_str(), "ForDeprecatedCBindings");
}

TEST(AttributesTests, BadDeprecatedAttributes) {
  TestLibrary library(R"FIDL(
library fidl.test;

[Layout = "Simple"]
struct MyStruct {};

[Layout = "Complex"]
protocol MyOtherProtocol {
  MyMethod();
};

[Layout = "Simple"]
protocol MyProtocol {
  MyMethod();
};
)FIDL");
  EXPECT_FALSE(library.Compile());
  const auto& errors = library.errors();
  ASSERT_EQ(errors.size(), 3);
  for (size_t i = 0; i < errors.size(); i++) {
    ASSERT_ERR(errors[i], fidl::ErrDeprecatedAttribute);
  }
}

TEST(AttributesTests, BadSimpleUnion) {
  TestLibrary library(R"FIDL(
library fidl.test;

union U {
    1: string s;
};

[ForDeprecatedCBindings]
protocol P {
    -> Event(U u);
};
)FIDL");
  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrUnionCannotBeSimple,
                                      fidl::ErrMemberMustBeSimple);
}

bool MustHaveThreeMembers(fidl::Reporter* reporter, const fidl::raw::Attribute& attribute,
                          const fidl::flat::Decl* decl) {
  switch (decl->kind) {
    case fidl::flat::Decl::Kind::kStruct: {
      auto struct_decl = static_cast<const fidl::flat::Struct*>(decl);
      return struct_decl->members.size() == 3;
    }
    default:
      return false;
  }
}

TEST(AttributesTests, BadConstraintOnlyThreeMembersOnStruct) {
  TestLibrary library(R"FIDL(
library fidl.test;

[MustHaveThreeMembers]
struct MyStruct {
    int64 one;
    int64 two;
    int64 three;
    int64 oh_no_four;
};

)FIDL");
  library.AddAttributeSchema("MustHaveThreeMembers",
                             fidl::flat::AttributeSchema(
                                 {
                                     fidl::flat::AttributeSchema::Placement::kStructDecl,
                                 },
                                 {
                                     "",
                                 },
                                 MustHaveThreeMembers));
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrAttributeConstraintNotSatisfied);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "MustHaveThreeMembers");
}

TEST(AttributesTests, BadConstraintOnlyThreeMembersOnMethod) {
  TestLibrary library(R"FIDL(
library fidl.test;

protocol MyProtocol {
    [MustHaveThreeMembers] MyMethod();
};

)FIDL");
  library.AddAttributeSchema("MustHaveThreeMembers",
                             fidl::flat::AttributeSchema(
                                 {
                                     fidl::flat::AttributeSchema::Placement::kMethod,
                                 },
                                 {
                                     "",
                                 },
                                 MustHaveThreeMembers));
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrAttributeConstraintNotSatisfied);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "MustHaveThreeMembers");
}

TEST(AttributesTests, BadConstraintOnlyThreeMembersOnProtocol) {
  TestLibrary library(R"FIDL(
library fidl.test;

[MustHaveThreeMembers]
protocol MyProtocol {
    MyMethod();
    MySecondMethod();
};

)FIDL");
  library.AddAttributeSchema("MustHaveThreeMembers",
                             fidl::flat::AttributeSchema(
                                 {
                                     fidl::flat::AttributeSchema::Placement::kProtocolDecl,
                                 },
                                 {
                                     "",
                                 },
                                 MustHaveThreeMembers));
  // Twice because there are two methods.
  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrAttributeConstraintNotSatisfied,
                                      fidl::ErrAttributeConstraintNotSatisfied);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "MustHaveThreeMembers");
}

TEST(AttributesTests, BadMaxBytes) {
  TestLibrary library(R"FIDL(
library fidl.test;

[MaxBytes = "27"]
table MyTable {
  1: bool here;
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrTooManyBytes);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "27");  // 27 allowed
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "40");  // 40 found
}

TEST(AttributesTests, BadMaxBytesBoundTooBig) {
  TestLibrary library(R"FIDL(
library fidl.test;

[MaxBytes = "4294967296"] // 2^32
table MyTable {
  1: uint8 u;
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrBoundIsTooBig);
}

TEST(AttributesTests, BadMaxBytesUnableToParseBound) {
  TestLibrary library(R"FIDL(
library fidl.test;

[MaxBytes = "invalid"]
table MyTable {
  1: uint8 u;
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnableToParseBound);
}

TEST(AttributesTests, BadMaxHandles) {
  TestLibrary library(R"FIDL(
library fidl.test;

[MaxHandles = "2"]
resource union MyUnion {
  1: uint8 hello;
  2: array<uint8>:8 world;
  3: vector<handle>:6 foo;
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrTooManyHandles);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "2");  // 2 allowed
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "6");  // 6 found
}

TEST(AttributesTests, badAttributeValue) {
  TestLibrary library(R"FIDL(
library fidl.test;

[ForDeprecatedCBindings = "Complex"]
protocol P {
    Method();
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidAttributeValue);
}

TEST(AttributesTests, BadSelectorIncorrectPlacement) {
  TestLibrary library(R"FIDL(
library fidl.test;

[Selector = "Nonsense"]
union MyUnion {
  1: uint8 hello;
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidAttributePlacement);
}

TEST(AttributesTests, BadNoAttributesOnReserved) {
  {
    TestLibrary library(R"FIDL(
library fidl.test;

union Foo {
  [Foo]
  1: reserved;
};
)FIDL");
    ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrCannotAttachAttributesToReservedOrdinals);
  }

  {
    TestLibrary library(R"FIDL(
  library fidl.test;

  table Foo {
    [Foo]
    1: reserved;
  };
  )FIDL");
    ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrCannotAttachAttributesToReservedOrdinals);
  }
}

TEST(AttributesTests, BadParameterAttributeIncorrectPlacement) {
  TestLibrary library(R"FIDL(
library fidl.test;

protocol ExampleProtocol {
    Method(exampleusing.Empty arg [OnParameter]);
};

)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}
}  // namespace
