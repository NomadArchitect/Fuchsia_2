// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <locale.h>

#include <fidl/lexer.h>
#include <fidl/parser.h>
#include <fidl/raw_ast.h>
#include <fidl/source_file.h>
#include <zxtest/zxtest.h>

#include "error_test.h"
#include "test_library.h"

namespace {

// Test that an invalid compound identifier fails parsing. Regression
// test for fxbug.dev/7600.
TEST(ParsingTests, BadCompoundIdentifierTest) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  // The leading 0 in the library name causes parsing an Identifier
  // to fail, and then parsing a CompoundIdentifier to fail.
  TestLibrary library(R"FIDL(
library 0fidl.test.badcompoundidentifier;
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ParsingTests, BadCompoundIdentifierTestOld) {
  // The leading 0 in the library name causes parsing an Identifier
  // to fail, and then parsing a CompoundIdentifier to fail.
  TestLibrary library(R"FIDL(
library 0fidl.test.badcompoundidentifier;
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

// Test that library name formatting checks are done in the parser
TEST(ParsingTests, BadLibraryNameTest) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library a_b;
)FIDL",
                      experimental_flags);

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidLibraryNameComponent);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "a_b");
}

TEST(ParsingTests, BadLibraryNameTestOld) {
  TestLibrary library(R"FIDL(
library a_b;
)FIDL");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidLibraryNameComponent);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "a_b");
}

// Test that otherwise reserved words can be appropriately parsed when context
// is clear.
TEST(ParsingTests, GoodParsingReservedWordsInStructTest) {
  TestLibrary library(R"FIDL(
library example;

struct struct {
    bool field;
};

struct flexible {};
struct strict {};
struct resource {};

struct InStruct {
    struct foo;
    flexible bar;
    strict baz;
    resource qux;

    bool as;
    bool library;
    bool using;

    bool array;
    bool handle;
    bool request;
    bool string;
    bool vector;

    bool bool;
    bool int8;
    bool int16;
    bool int32;
    bool int64;
    bool uint8;
    bool uint16;
    bool uint32;
    bool uint64;
    bool float32;
    bool float64;

    bool true;
    bool false;

    bool reserved;
};
)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(ParsingTests, GoodParsingHandlesInStructTest) {
  TestLibrary library(R"FIDL(
library example;

enum obj_type : uint32 {
    NONE = 0;
    PROCESS = 1;
    THREAD = 2;
    VMO = 3;
    CHANNEL = 4;
    EVENT = 5;
    PORT = 6;
    INTERRUPT = 9;
    PCI_DEVICE = 11;
    LOG = 12;
    SOCKET = 14;
    RESOURCE = 15;
    EVENTPAIR = 16;
    JOB = 17;
    VMAR = 18;
    FIFO = 19;
    GUEST = 20;
    VCPU = 21;
    TIMER = 22;
    IOMMU = 23;
    BTI = 24;
    PROFILE = 25;
    PMT = 26;
    SUSPEND_TOKEN = 27;
    PAGER = 28;
    EXCEPTION = 29;
    CLOCK = 30;
};

resource_definition handle : uint32 {
    properties {
        obj_type subtype;
    };
};

resource struct Handles {
    handle plain_handle;

    handle:BTI bti_handle;
    handle:CHANNEL channel_handle;
    handle:CLOCK clock_handle;
    handle:LOG debuglog_handle;
    handle:EVENT event_handle;
    handle:EVENTPAIR eventpair_handle;
    handle:EXCEPTION exception_handle;
    handle:FIFO fifo_handle;
    handle:GUEST guest_handle;
    handle:INTERRUPT interrupt_handle;
    handle:IOMMU iommu_handle;
    handle:JOB job_handle;
    handle:PAGER pager_handle;
    handle:PCI_DEVICE pcidevice_handle;
    handle:PMT pmt_handle;
    handle:PORT port_handle;
    handle:PROCESS process_handle;
    handle:PROFILE profile_handle;
    handle:RESOURCE resource_handle;
    handle:SOCKET socket_handle;
    handle:SUSPEND_TOKEN suspendtoken_handle;
    handle:THREAD thread_handle;
    handle:TIMER timer_handle;
    handle:VCPU vcpu_handle;
    handle:VMAR vmar_handle;
    handle:VMO vmo_handle;
};
)FIDL");

  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(ParsingTests, GoodParsingHandleConstraintTest) {
  fidl::ExperimentalFlags experimental_flags;

  TestLibrary library(R"FIDL(
library example;

enum obj_type : uint32 {
    NONE = 0;
    VMO = 3;
};

bits rights : uint32 {
  TRANSFER = 1;
};

resource_definition handle : uint32 {
    properties {
        obj_type subtype;
        rights rights;
    };
};

resource struct Handles {
    handle plain_handle;
    handle:VMO subtype_handle;
    handle:<VMO, rights.TRANSFER> rights_handle;
};
)FIDL",
                      std::move(experimental_flags));

  ASSERT_COMPILED_AND_CONVERT(library);
}

// Test that otherwise reserved words can be appropriarely parsed when context
// is clear.
TEST(ParsingTests, GoodParsingReservedWordsInUnionTest) {
  TestLibrary library(R"FIDL(
library example;

struct struct {
    bool field;
};

union InUnion {
    1:  struct foo;

    2:  bool as;
    3:  bool library;
    4:  bool using;

    5:  bool array;
    6:  bool handle;
    7:  bool request;
    8:  bool string;
    9:  bool vector;

    10: bool bool;
    11: bool int8;
    12: bool int16;
    13: bool int32;
    14: bool int64;
    15: bool uint8;
    16: bool uint16;
    17: bool uint32;
    18: bool uint64;
    19: bool float32;
    20: bool float64;

    21: bool true;
    22: bool false;

    23: bool reserved;
};
)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
}

// Test that otherwise reserved words can be appropriately parsed when context
// is clear.
TEST(ParsingTests, GoodParsingReservedWordsInProtocolTest) {
  TestLibrary library(R"FIDL(
library example;

struct struct {
    bool field;
};

protocol InProtocol {
    as(bool as);
    library(bool library);
    using(bool using);

    array(bool array);
    handle(bool handle);
    request(bool request);
    string(bool string);
    vector(bool vector);

    bool(bool bool);
    int8(bool int8);
    int16(bool int16);
    int32(bool int32);
    int64(bool int64);
    uint8(bool uint8);
    uint16(bool uint16);
    uint32(bool uint32);
    uint64(bool uint64);
    float32(bool float32);
    float64(bool float64);

    true(bool true);
    false(bool false);

    reserved(bool reserved);

    foo(struct arg, int32 arg2, struct arg3);
};
)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(ParsingTests, BadCharPoundSignTest) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library test;

type Test = struct {
    #uint8 uint8;
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidCharacter);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "#");
}

TEST(ParsingTests, BadCharPoundSignTestOld) {
  TestLibrary library(R"FIDL(
library test;

struct Test {
    uint8 #uint8;
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidCharacter);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "#");
}

TEST(ParsingTests, BadCharSlashTest) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library test;

type Test = struct / {
    uint8 uint8;
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidCharacter);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "/");
}

TEST(ParsingTests, BadCharSlashTestOld) {
  TestLibrary library(R"FIDL(
library test;

struct Test / {
    uint8 uint8;
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidCharacter);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "/");
}

TEST(ParsingTests, BadIdentifierTest) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library test;

type test_ = struct {
    uint8 uint8;
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidIdentifier);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "test_");
}

TEST(ParsingTests, BadIdentifierTestOld) {
  TestLibrary library(R"FIDL(
library test;

struct test_ {
    uint8 uint8;
};
)FIDL");
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidIdentifier);
  ASSERT_SUBSTR(library.errors()[0]->msg.c_str(), "test_");
}

class LocaleSwapper {
 public:
  explicit LocaleSwapper(const char* new_locale) { old_locale_ = setlocale(LC_ALL, new_locale); }
  ~LocaleSwapper() { setlocale(LC_ALL, old_locale_); }

 private:
  const char* old_locale_;
};

TEST(ParsingTests, BadInvalidCharacterTest) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  LocaleSwapper swapper("de_DE.iso88591");
  TestLibrary library("invalid.character.fidl", R"FIDL(
library fidl.test.maxbytes;

// This is all alphanumeric in the appropriate locale, but not a valid
// identifier.
type ß = struct {
    x int32;
};

)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrInvalidCharacter,
                                      fidl::ErrInvalidCharacter);
}

TEST(ParsingTests, BadInvalidCharacterTestOld) {
  LocaleSwapper swapper("de_DE.iso88591");
  TestLibrary library("invalid.character.fidl", R"FIDL(
library fidl.test.maxbytes;

// This is all alphanumeric in the appropriate locale, but not a valid
// identifier.
struct ß {
    int32 x;
};

)FIDL");
  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrInvalidCharacter,
                                      fidl::ErrInvalidCharacter);
}

TEST(ParsingTests, GoodEmptyStructTest) {
  TestLibrary library("empty_struct.fidl", R"FIDL(
library fidl.test.emptystruct;

struct Empty {
};

)FIDL");
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(ParsingTests, BadErrorOnTypeAliasBeforeImports) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  SharedAmongstLibraries shared;
  TestLibrary dependency("dependent.fidl", R"FIDL(
library dependent;

struct Something {};
)FIDL",
                         &shared);
  TestLibrary converted_dependency;
  ASSERT_COMPILED_AND_CONVERT_INTO(dependency, converted_dependency);

  TestLibrary library("example.fidl", R"FIDL(
library example;

alias foo = int16;
using dependent;

type UseDependent = struct {
    field dependent.Something;
};
)FIDL",
                      &shared, experimental_flags);
  ASSERT_TRUE(library.AddDependentLibrary(std::move(dependency)));
  ASSERT_ERRORED_DURING_COMPILE_WITH_DEP(library, converted_dependency,
                                         fidl::ErrLibraryImportsMustBeGroupedAtTopOfFile);
}

TEST(ParsingTests, BadErrorOnTypeAliasBeforeImportsWithOldDep) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  SharedAmongstLibraries shared;
  TestLibrary dependency("dependent.fidl", R"FIDL(
library dependent;

struct Something {};
)FIDL",
                         &shared);
  TestLibrary cloned_dependency;
  ASSERT_COMPILED_AND_CLONE_INTO(dependency, cloned_dependency);

  TestLibrary library("example.fidl", R"FIDL(
library example;

alias foo = int16;
using dependent;

type UseDependent = struct {
    field dependent.Something;
};
)FIDL",
                      &shared, experimental_flags);
  ASSERT_TRUE(library.AddDependentLibrary(std::move(dependency)));
  ASSERT_ERRORED_DURING_COMPILE_WITH_DEP(library, cloned_dependency,
                                         fidl::ErrLibraryImportsMustBeGroupedAtTopOfFile);
}

TEST(ParsingTests, BadErrorOnTypeAliasBeforeImportsOld) {
  SharedAmongstLibraries shared;
  TestLibrary dependency("dependent.fidl", R"FIDL(
library dependent;

struct Something {};
)FIDL",
                         &shared);
  ASSERT_TRUE(dependency.Compile());

  TestLibrary library("example.fidl", R"FIDL(
library example;

alias foo = int16;
using dependent;

struct UseDependent {
    dependent.Something field;
};
)FIDL",
                      &shared);
  ASSERT_TRUE(library.AddDependentLibrary(std::move(dependency)));
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrLibraryImportsMustBeGroupedAtTopOfFile);
}

TEST(ParsingTests, GoodAttributeValueHasCorrectContents) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library("example.fidl", R"FIDL(
  library example;

  @foo("Bar")
  type Empty = struct{};
)FIDL",
                      experimental_flags);

  std::unique_ptr<fidl::raw::File> ast;
  ASSERT_TRUE(library.Parse(&ast));

  fidl::raw::AttributeNew attribute =
      std::move(ast->type_decls.front()->attributes->attributes.front());
  ASSERT_STR_EQ(attribute.name.c_str(), "foo");
  ASSERT_TRUE(attribute.args.size() == 1);

  fidl::raw::AttributeArg arg = std::move(attribute.args[0]);
  ASSERT_STR_EQ(static_cast<fidl::raw::StringLiteral*>(arg.value.get())->MakeContents(), "Bar");
}

// TODO(fxbug.dev/70247): this "Good" test is copied because it cannot use the
//  full ASSERT_CONVERTED_AND_COMPILE macro, since the condition we are testing
//  is a valid parse tree generation.
TEST(ParsingTests, GoodAttributeValueHasCorrectContentsOld) {
  TestLibrary library("example.fidl", R"FIDL(
  library example;

  [Foo="Bar"]
  struct Empty{};
  )FIDL");

  std::unique_ptr<fidl::raw::File> ast;
  ASSERT_TRUE(library.Parse(&ast));

  fidl::raw::AttributeOld attribute =
      std::move(ast->struct_declaration_list.front()->attributes->attributes.front());
  ASSERT_STR_EQ(attribute.name.c_str(), "Foo");
  ASSERT_STR_EQ(static_cast<fidl::raw::StringLiteral*>(attribute.value.get())->MakeContents(),
                "Bar");
}

TEST(ParsingTests, GoodMultilineCommentHasCorrectContents) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library("example.fidl", R"FIDL(
  library example;

  /// A
  /// multiline
  /// comment!
  type Empty = struct {};
)FIDL",
                      experimental_flags);

  std::unique_ptr<fidl::raw::File> ast;
  ASSERT_TRUE(library.Parse(&ast));

  fidl::raw::AttributeNew attribute =
      std::move(ast->type_decls.front()->attributes->attributes.front());
  ASSERT_STR_EQ(attribute.name.c_str(), "doc");
  ASSERT_TRUE(attribute.args.size() == 1);

  fidl::raw::AttributeArg arg = std::move(attribute.args[0]);
  ASSERT_STR_EQ(static_cast<fidl::raw::DocCommentLiteral*>(arg.value.get())->MakeContents(),
                " A\n multiline\n comment!\n");
}

// TODO(fxbug.dev/70247): this "Good" test is copied because it cannot use the
//  full ASSERT_CONVERTED_AND_COMPILE macro, since the condition we are testing
//  is a valid parse tree generation.
TEST(ParsingTests, GoodMultilineCommentHasCorrectContentsOld) {
  TestLibrary library("example.fidl", R"FIDL(
  library example;

  /// A
  /// multiline
  /// comment!
  struct Empty{};
  )FIDL");

  std::unique_ptr<fidl::raw::File> ast;
  ASSERT_TRUE(library.Parse(&ast));

  fidl::raw::AttributeOld attribute =
      std::move(ast->struct_declaration_list.front()->attributes->attributes.front());
  ASSERT_STR_EQ(attribute.name.c_str(), "Doc");
  ASSERT_STR_EQ(static_cast<fidl::raw::DocCommentLiteral*>(attribute.value.get())->MakeContents(),
                " A\n multiline\n comment!\n");
}

TEST(ParsingTests, WarnDocCommentBlankLineTest) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

/// start

/// end
struct Empty{};
)FIDL");

  ASSERT_COMPILED_AND_CONVERT(library);
  const auto& warnings = library.warnings();
  // TODO(fxbug.dev/70247): The number of warnings has doubled, as we are going
  //  to collect every warning twice: once for the original compilation, and
  //  once again for the converted one.  This number will need to be halved
  //  during cleanup.
  ASSERT_EQ(warnings.size(), 2);
  ASSERT_ERR(warnings[0], fidl::WarnBlankLinesWithinDocCommentBlock);
  ASSERT_ERR(warnings[1], fidl::WarnBlankLinesWithinDocCommentBlock);
}

// TODO(fxbug.dev/70247): This test cannot be run by converting old code, and so
//  must maintain a manual copy here until conversion is complete.  See the test
//  with below for more info.
TEST(NewSyntaxTests, WarnCommentInsideDocCommentTestNew) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library("example.fidl", R"FIDL(
library example;

/// start
// middle
/// end
type Empty = struct {};
)FIDL",
                      experimental_flags);

  ASSERT_TRUE(library.Compile());
  const auto& warnings = library.warnings();
  ASSERT_GE(warnings.size(), 1);
  ASSERT_ERR(warnings[0], fidl::WarnCommentWithinDocCommentBlock);
}

// TODO(fxbug.dev/70247): The converter moves the errant comment into the proper
//  location, so this test no longer produces warnings after conversion.  A
//  manual copy of the test has been added to above - once conversion is
//  completed and ASSERT_COMPILED_AND_CONVERT is removed, that test should be
//  copied in place of this one.
TEST(ParsingTests, WarnCommentInsideDocCommentTest) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

/// start
// middle
/// end
struct Empty{};
)FIDL");

  ASSERT_COMPILED_AND_CONVERT(library);
  const auto& warnings = library.warnings();
  ASSERT_GE(warnings.size(), 1);
  ASSERT_ERR(warnings[0], fidl::WarnCommentWithinDocCommentBlock);
}

TEST(ParsingTests, WarnDocCommentWithCommentBlankLineTest) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

/// start
// middle

/// end
struct Empty{};
)FIDL");

  ASSERT_COMPILED_AND_CONVERT(library);
  const auto& warnings = library.warnings();
  // TODO(fxbug.dev/70247): The number of warnings has doubled, as we are going
  //  to collect every warning twice: once for the original compilation, and
  //  once again for the converted one.  This number will need to be halved
  //  during cleanup.
  ASSERT_EQ(warnings.size(), 4);
  ASSERT_ERR(warnings[0], fidl::WarnCommentWithinDocCommentBlock);
  ASSERT_ERR(warnings[1], fidl::WarnBlankLinesWithinDocCommentBlock);
  ASSERT_ERR(warnings[2], fidl::WarnCommentWithinDocCommentBlock);
  ASSERT_ERR(warnings[3], fidl::WarnBlankLinesWithinDocCommentBlock);
}

TEST(ParsingTests, BadDocCommentNotAllowedOnParams) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library("example.fidl", R"FIDL(
library example;

protocol Example {
  Method(/// Doc comment
         struct { b bool; });
};
)FIDL",
                      experimental_flags);

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrDocCommentOnParameters);
}

TEST(ParsingTests, BadDocCommentNotAllowedOnParamsOld) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

protocol Example {
  Method(/// Doc comment
         Bool b);
};
)FIDL");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrDocCommentOnParameters);
}

TEST(ParsingTests, GoodCommentsSurroundingDocCommentTest) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

// some comments above,
// maybe about the doc comment
/// A
/// multiline
/// comment!
// another comment about the struct
struct Empty{};
)FIDL");

  library.set_warnings_as_errors(true);
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(ParsingTests, GoodBlankLinesAfterDocCommentTest) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

/// doc comment



struct Empty{};
)FIDL");

  library.set_warnings_as_errors(true);
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(ParsingTests, GoodBlankLinesAfterDocCommentWithCommentTest) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

/// doc comment


// regular comment

struct Empty{};
)FIDL");

  library.set_warnings_as_errors(true);
  ASSERT_COMPILED_AND_CONVERT(library);
}

TEST(ParsingTests, WarnTrailingDocCommentTest) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

struct Empty{};
/// bad
)FIDL");

  ASSERT_COMPILED_AND_CONVERT(library);
  const auto& warnings = library.warnings();
  // TODO(fxbug.dev/70247): The number of warnings has doubled, as we are going
  //  to collect every warning twice: once for the original compilation, and
  //  once again for the converted one.  This number will need to be halved
  //  during cleanup.
  ASSERT_EQ(warnings.size(), 2);
  ASSERT_ERR(warnings[0], fidl::WarnDocCommentMustBeFollowedByDeclaration);
  ASSERT_ERR(warnings[1], fidl::WarnDocCommentMustBeFollowedByDeclaration);
}

TEST(ParsingTests, BadTrailingDocCommentInDeclTest) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library("example.fidl", R"FIDL(
library example;

type Empty = struct {
   a = int8;
   /// bad
};
)FIDL",
                      experimental_flags);

  ASSERT_FALSE(library.Compile());

  const auto& errors = library.errors();
  ASSERT_EQ(errors.size(), 3);
  ASSERT_ERR(errors[0], fidl::ErrUnexpectedTokenOfKind);
  ASSERT_ERR(errors[1], fidl::ErrUnexpectedTokenOfKind);
  ASSERT_ERR(errors[2], fidl::ErrUnexpectedTokenOfKind);
}

TEST(ParsingTests, BadTrailingDocCommentInDeclTestOld) {
  TestLibrary library("example.fidl", R"FIDL(
library example;

struct Empty {
   int8 a;
   /// bad
};
)FIDL");

  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind,
                                      fidl::ErrUnexpectedTokenOfKind);
}

TEST(ParsingTests, BadFinalMemberMissingSemicolon) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;

type Struct = struct {
    uint_value uint8;
    foo string // error: missing semicolon
};
)FIDL",
                      experimental_flags);

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

TEST(ParsingTests, BadFinalMemberMissingSemicolonOld) {
  TestLibrary library(R"FIDL(
library example;

struct Struct {
    uint8 uint_value;
    string foo // error: missing semicolon
};
)FIDL");

  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind);
}

// NOTE(fxbug.dev/72924): this test is slightly different from the old syntax
// one that it replaces, in that the "missing" portion of the struct member is a
// type, not a name.
TEST(ParsingTests, BadFinalMemberMissingTypeAndSemicolon) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);
  TestLibrary library(R"FIDL(
library example;

type Struct = struct {
    uint_value uint8;
    string_value
}; // error: want type, got "}"
   // error: want "}", got EOF
)FIDL",
                      experimental_flags);

  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind,
                                      fidl::ErrUnexpectedTokenOfKind);
}

TEST(ParsingTests, BadFinalMemberMissingNameAndSemicolonOld) {
  TestLibrary library(R"FIDL(
library example;

struct Struct {
    uint8 uint_value;
    string
}; // error: want field name, got "}"
   // error: want "}", got EOF
)FIDL");

  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind,
                                      fidl::ErrUnexpectedTokenOfKind);
}

TEST(ParsingTests, BadMissingConstraintBrackets) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);

  TestLibrary library(R"FIDL(
library example;

type Foo = struct {
    bad_no_brackets vector<uint8>:10,optional;
};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_TWICE_DURING_COMPILE(library, fidl::ErrUnexpectedTokenOfKind,
                                      fidl::ErrUnexpectedTokenOfKind);
}

TEST(ParsingTests, GoodSingleConstraint) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);

  TestLibrary library(R"FIDL(
library example;

type Foo = struct {
  with_brackets vector<int32>:<10>;
  without_brackets vector<int32>:10;
};
)FIDL",
                      experimental_flags);
  ASSERT_COMPILED(library);
}

TEST(ParsingTests, BadSubtypeCtor) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);

  TestLibrary library(R"FIDL(
library example;

type Foo = struct : uint32 {};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrCannotSpecifySubtype);
}

TEST(ParsingTests, BadLayoutClass) {
  fidl::ExperimentalFlags experimental_flags;
  experimental_flags.SetFlag(fidl::ExperimentalFlags::Flag::kAllowNewSyntax);

  TestLibrary library(R"FIDL(
library example;

type Foo = foobar {};
)FIDL",
                      experimental_flags);
  ASSERT_ERRORED_DURING_COMPILE(library, fidl::ErrInvalidLayoutClass);
}

}  // namespace
