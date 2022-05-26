// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/attachments/attachment_manager.h"

#include <lib/async/cpp/executor.h>
#include <lib/async/cpp/task.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <cstddef>
#include <memory>
#include <optional>
#include <string>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback_data/attachments/types.h"
#include "src/developer/forensics/testing/gmatchers.h"
#include "src/developer/forensics/testing/gpretty_printers.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace forensics {
namespace feedback_data {
namespace {

using ::testing::Contains;
using ::testing::ElementsAreArray;
using ::testing::Eq;
using ::testing::HasSubstr;
using ::testing::IsEmpty;
using ::testing::Not;
using ::testing::Pair;
using ::testing::UnorderedElementsAreArray;

class SimpleAttachmentProvider : public AttachmentProvider {
 public:
  SimpleAttachmentProvider(async_dispatcher_t* dispatcher, zx::duration delay, AttachmentValue data)
      : dispatcher_(dispatcher), delay_(delay), data_(std::move(data)) {}

  ::fpromise::promise<AttachmentValue> Get(zx::duration timeout) override {
    ::fpromise::bridge<AttachmentValue> bridge;

    async::PostDelayedTask(
        dispatcher_,
        [this, timeout, completer = std::move(bridge.completer)]() mutable {
          if (delay_ <= timeout) {
            completer.complete_ok(data_);
          } else {
            completer.complete_ok(AttachmentValue(Error::kTimeout));
          }
        },
        (delay_ <= timeout) ? delay_ : timeout);

    return bridge.consumer.promise_or(::fpromise::error());
  }

 private:
  async_dispatcher_t* dispatcher_;
  zx::duration delay_;
  AttachmentValue data_;
};

using AttachmentManagerTest = UnitTestFixture;

TEST_F(AttachmentManagerTest, Static) {
  async::Executor executor(dispatcher());
  AttachmentManager manager({"static"}, {{"static", AttachmentValue("value")}});

  Attachments attachments;
  executor.schedule_task(
      manager.GetAttachments(zx::duration::infinite())
          .and_then([&attachments](Attachments& result) { attachments = std::move(result); })
          .or_else([] { FX_LOGS(FATAL) << "Unreachable branch"; }));

  RunLoopUntilIdle();
  EXPECT_THAT(attachments, ElementsAreArray({Pair("static", AttachmentValue("value"))}));
}

TEST_F(AttachmentManagerTest, DropStatic) {
  async::Executor executor(dispatcher());
  AttachmentManager manager({"static"}, {{"static", AttachmentValue("value")}});

  manager.DropStaticAttachment("static", Error::kConnectionError);
  manager.DropStaticAttachment("unused", Error::kConnectionError);

  Attachments attachments;
  executor.schedule_task(
      manager.GetAttachments(zx::duration::infinite())
          .and_then([&attachments](Attachments& result) { attachments = std::move(result); })
          .or_else([] { FX_LOGS(FATAL) << "Unreachable branch"; }));

  RunLoopUntilIdle();
  EXPECT_THAT(attachments,
              ElementsAreArray({Pair("static", AttachmentValue(Error::kConnectionError))}));
}

TEST_F(AttachmentManagerTest, Dynamic) {
  async::Executor executor(dispatcher());

  SimpleAttachmentProvider provider1(dispatcher(), zx::sec(1), AttachmentValue("value1"));
  SimpleAttachmentProvider provider2(dispatcher(), zx::sec(3), AttachmentValue("value2"));

  AttachmentManager manager({"dynamic1", "dynamic2"}, {},
                            {
                                {"dynamic1", &provider1},
                                {"dynamic2", &provider2},
                            });

  Attachments attachments;
  executor.schedule_task(
      manager.GetAttachments(zx::sec(1))
          .and_then([&attachments](Attachments& result) { attachments = std::move(result); })
          .or_else([] { FX_LOGS(FATAL) << "Unreachable branch"; }));

  RunLoopFor(zx::sec(1));
  EXPECT_THAT(attachments, ElementsAreArray({
                               Pair("dynamic1", AttachmentValue("value1")),
                               Pair("dynamic2", AttachmentValue(Error::kTimeout)),
                           }));

  attachments.clear();

  executor.schedule_task(
      manager.GetAttachments(zx::duration::infinite())
          .and_then([&attachments](Attachments& result) { attachments = std::move(result); })
          .or_else([] { FX_LOGS(FATAL) << "Unreachable branch"; }));

  RunLoopFor(zx::sec(3));
  EXPECT_THAT(attachments, ElementsAreArray({
                               Pair("dynamic1", AttachmentValue("value1")),
                               Pair("dynamic2", AttachmentValue("value2")),
                           }));
}

TEST_F(AttachmentManagerTest, NoProvider) {
  ASSERT_DEATH({ AttachmentManager manager({"unknown.attachment"}); },
               HasSubstr("Attachment \"unknown.attachment\" collected by 0 providers"));
}

}  // namespace
}  // namespace feedback_data
}  // namespace forensics
