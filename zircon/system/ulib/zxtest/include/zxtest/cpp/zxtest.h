// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZXTEST_CPP_ZXTEST_H_
#define ZXTEST_CPP_ZXTEST_H_

#ifndef ZXTEST_INCLUDE_INTERNAL_HEADERS
#error This header is not intended for direct inclusion. Include zxtest/zxtest.h instead.
#endif

#include <cstdlib>
#include <type_traits>

#include <fbl/string.h>
#include <fbl/string_printf.h>
#include <zxtest/base/assertion.h>
#include <zxtest/base/parameterized-value-impl.h>
#include <zxtest/base/runner.h>
#include <zxtest/base/test.h>
#include <zxtest/base/values.h>

#ifdef __Fuchsia__
#include <zircon/status.h>

#include <zxtest/base/death-statement.h>
#endif

// Pre-processor magic to allow EXPECT_ macros not enforce a return type on helper functions.
#define _RETURN_IF_FATAL_true     \
  do {                            \
    unittest_fails();             \
    if (_ZXTEST_ABORT_IF_ERROR) { \
      return;                     \
    }                             \
  } while (0)

#define _RETURN_IF_FATAL_false \
  do {                         \
    unittest_fails();          \
  } while (0)

#define _RETURN_IF_FATAL(fatal) _RETURN_IF_FATAL_##fatal

#ifdef ZXTEST_USE_STREAMABLE_MACROS
#include <zxtest/cpp/assert_streams.h>
#else
#include <zxtest/cpp/assert.h>
#endif

// Macro definitions for usage within CPP.
#define _ZXTEST_EXPAND_(arg) arg
#define _ZXTEST_GET_FIRST_(first, ...) first
#define _ZXTEST_GET_SECOND_(first, second, ...) second

#define RUN_ALL_TESTS(argc, argv) zxtest::RunAllTests(argc, argv)

#define _ZXTEST_TEST_REF_N(TestCase, Test, Tag) TestCase##_##Test##_##Tag##_Ref
#define _ZXTEST_TEST_REF(TestCase, Test) _ZXTEST_TEST_REF_N(TestCase, Test, 0)

#define _ZXTEST_DEFAULT_FIXTURE ::zxtest::Test
#define _ZXTEST_PARAM_FIXTURE ::zxtest::TestWithParam

#define _ZXTEST_TEST_CLASS(TestCase, Test) TestCase##_##Test##_Class

#define _ZXTEST_TEST_CLASS_DECL(Fixture, TestClass) \
  class TestClass final : public Fixture {          \
   public:                                          \
    TestClass() = default;                          \
    ~TestClass() final = default;                   \
                                                    \
   private:                                         \
    void TestBody() final;                          \
  }

#define _ZXTEST_BEGIN_TEST_BODY(TestClass) void TestClass::TestBody()

#define _ZXTEST_REGISTER_FN(TestCase, Test) TestCase##_##Test##_register_fn

// Note: We intentionally wrap the assignment in a constructor function, to workaround the issue
// where in certain builds (both debug and production), the compiler would generated a global
// initiatialization function for the runtime, which would push a huge amount of memory into the
// stack. For 2048 tests, it pushed ~270 KB, which caused an overflow.
#define _ZXTEST_REGISTER(TestCase, Test, Fixture)                                                 \
  _ZXTEST_TEST_CLASS_DECL(Fixture, _ZXTEST_TEST_CLASS(TestCase, Test));                           \
  static zxtest::TestRef _ZXTEST_TEST_REF(TestCase, Test) = {};                                   \
  static void _ZXTEST_REGISTER_FN(TestCase, Test)(void) __attribute__((constructor));             \
  void _ZXTEST_REGISTER_FN(TestCase, Test)(void) {                                                \
    _ZXTEST_TEST_REF(TestCase, Test) =                                                            \
        zxtest::Runner::GetInstance()->RegisterTest<Fixture, _ZXTEST_TEST_CLASS(TestCase, Test)>( \
            #TestCase, #Test, __FILE__, __LINE__);                                                \
  }                                                                                               \
  _ZXTEST_BEGIN_TEST_BODY(_ZXTEST_TEST_CLASS(TestCase, Test))

#define _ZXTEST_REGISTER_PARAMETERIZED(TestSuite, Test)                                \
  _ZXTEST_TEST_CLASS_DECL(TestSuite, _ZXTEST_TEST_CLASS(TestSuite, Test));             \
  static void _ZXTEST_REGISTER_FN(TestSuite, Test)(void) __attribute__((constructor)); \
  void _ZXTEST_REGISTER_FN(TestSuite, Test)(void) {                                    \
    zxtest::Runner::GetInstance()->AddParameterizedTest<TestSuite>(                    \
        std::make_unique<zxtest::internal::AddTestDelegateImpl<                        \
            TestSuite, TestSuite::ParamType, _ZXTEST_TEST_CLASS(TestSuite, Test)>>(),  \
        #TestSuite, #Test, {.filename = __FILE__, .line_number = __LINE__});           \
  }                                                                                    \
  _ZXTEST_BEGIN_TEST_BODY(_ZXTEST_TEST_CLASS(TestSuite, Test))

#define TEST(TestCase, Test) _ZXTEST_REGISTER(TestCase, Test, _ZXTEST_DEFAULT_FIXTURE)

#define TEST_F(TestCase, Test) _ZXTEST_REGISTER(TestCase, Test, TestCase)

#define TEST_P(TestSuite, Test) _ZXTEST_REGISTER_PARAMETERIZED(TestSuite, Test)

#define _ZXTEST_NULLPTR nullptr

#define _ZXTEST_ABORT_IF_ERROR zxtest::Runner::GetInstance()->CurrentTestHasFatalFailures()

#define _ZXTEST_STRCMP(actual, expected) zxtest::StrCmp(actual, expected)

#define _ZXTEST_AUTO_VAR_TYPE(var) decltype(var)

#define _ZXTEST_TEST_HAS_ERRORS zxtest::Runner::GetInstance()->CurrentTestHasFailures()

#define _ADD_INSTANTIATION_DEFAULT_NAME(arg1) \
  [](const auto info) -> std::string { return std::to_string(info.index); }

#define _ADD_INSTANTIATION_CUSTOM_NAME(arg1, generator) generator

#define _GET_3RD_ARG(arg1, arg2, arg3, ...) arg3

#define _NAME_GENERATOR_CHOOSER(...) \
  _GET_3RD_ARG(__VA_ARGS__, _ADD_INSTANTIATION_CUSTOM_NAME, _ADD_INSTANTIATION_DEFAULT_NAME)

#define _INSTANTIATION_NAME_FN(...) _NAME_GENERATOR_CHOOSER(__VA_ARGS__)(__VA_ARGS__)

#define INSTANTIATE_TEST_SUITE_P(Prefix, TestSuite, ...)                                        \
  static void _ZXTEST_REGISTER_FN(Prefix, TestSuite)(void) __attribute__((constructor));        \
  void _ZXTEST_REGISTER_FN(Prefix, TestSuite)(void) {                                           \
    static zxtest::internal::ValueProvider<TestSuite::ParamType> provider(                      \
        _ZXTEST_EXPAND_(_ZXTEST_GET_FIRST_(__VA_ARGS__)));                                      \
    zxtest::Runner::GetInstance()->AddInstantiation<TestSuite, TestSuite::ParamType>(           \
        std::make_unique<                                                                       \
            zxtest::internal::AddInstantiationDelegateImpl<TestSuite, TestSuite::ParamType>>(), \
        #Prefix, {.filename = __FILE__, .line_number = __LINE__}, provider,                     \
        _INSTANTIATION_NAME_FN(__VA_ARGS__));                                                   \
  }

// Definition of operations used to evaluate assertion conditions.
#define _EQ(actual, expected) \
  zxtest::internal::Compare(actual, expected, [](const auto& a, const auto& b) { return a == b; })
#define _NE(actual, expected) !_EQ(actual, expected)
#define _BOOL(actual, expected) (static_cast<bool>(actual) == static_cast<bool>(expected))
#define _LT(actual, expected) \
  zxtest::internal::Compare(actual, expected, [](const auto& a, const auto& b) { return a < b; })
#define _LE(actual, expected) \
  zxtest::internal::Compare(actual, expected, [](const auto& a, const auto& b) { return a <= b; })
#define _GT(actual, expected) \
  zxtest::internal::Compare(actual, expected, [](const auto& a, const auto& b) { return a > b; })
#define _GE(actual, expected) \
  zxtest::internal::Compare(actual, expected, [](const auto& a, const auto& b) { return a >= b; })
#define _STREQ(actual, expected) zxtest::StrCmp(actual, expected)
#define _STRNE(actual, expected) !_STREQ(actual, expected)
#define _SUBSTR(str, substr) zxtest::StrContain(str, substr)
#define _BYTEEQ(actual, expected, size) \
  (memcmp(static_cast<const void*>(actual), static_cast<const void*>(expected), size) == 0)
#define _BYTENE(actual, expected, size) !(_BYTEEQ(actual, expected, size))

// Functions used as arguments for EvaluateCondition.
#define _DESC_PROVIDER(desc, ...)                                    \
  [&]() -> fbl::String {                                             \
    fbl::String out_desc;                                            \
    auto format_msg = fbl::StringPrintf(" " __VA_ARGS__);            \
    out_desc = fbl::String::Concat({fbl::String(desc), format_msg}); \
    return out_desc;                                                 \
  }

#define _COMPARE_FN(op) \
  [](const auto& expected_, const auto& actual_) { return op(expected_, actual_); }

#define _COMPARE_3_FN(op, third_param)                        \
  [third_param](const auto& expected_, const auto& actual_) { \
    return op(expected_, actual_, third_param);               \
  }

// Printers for converting values into readable strings.
#define _DEFAULT_PRINTER [](const auto& val) { return zxtest::PrintValue(val); }

#ifdef __Fuchsia__
#define _STATUS_PRINTER [](zx_status_t status) { return zxtest::PrintStatus(status); }
#else
#define _STATUS_PRINTER _DEFAULT_PRINTER
#endif

#define _HEXDUMP_PRINTER(size)                                                 \
  [size](const auto& val) {                                                    \
    return zxtest::internal::ToHex(static_cast<const void*>(val), byte_count); \
  }

#ifdef __Fuchsia__
#define _ZXTEST_DEATH_STATUS_COMPLETE zxtest::internal::DeathStatement::State::kSuccess
#define _ZXTEST_DEATH_STATUS_EXCEPTION zxtest::internal::DeathStatement::State::kException
#define _ZXTEST_DEATH_STATEMENT(statement, expected_result, desc, ...)                     \
  do {                                                                                     \
    _ZXTEST_CHECK_RUNNING();                                                               \
    zxtest::internal::DeathStatement death_statement(statement);                           \
    death_statement.Execute();                                                             \
    if (death_statement.state() != expected_result) {                                      \
      if (death_statement.state() == zxtest::internal::DeathStatement::State::kBadState) { \
        zxtest::Runner::GetInstance()->NotifyFatalError();                                 \
      }                                                                                    \
      if (!death_statement.error_message().empty()) {                                      \
        _ZXTEST_ASSERT_ERROR(true, true, death_statement.error_message().data());          \
      } else {                                                                             \
        _ZXTEST_ASSERT_ERROR(true, true, desc, ##__VA_ARGS__);                             \
      }                                                                                    \
    }                                                                                      \
  } while (0)
#endif  // __Fuchsia__

#endif  // ZXTEST_CPP_ZXTEST_H_
