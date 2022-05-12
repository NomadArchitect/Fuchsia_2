// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/gtest/real_loop_fixture.h>

#include <memory>

#include <gtest/gtest.h>

#include "fuchsia/examples/diagnostics/cpp/fidl.h"
#include "lib/async-loop/cpp/loop.h"
#include "lib/async-loop/loop.h"
#include "lib/async/dispatcher.h"
#include "lib/fidl/cpp/interface_ptr.h"
#include "profile.h"

class ProfileTests : public gtest::RealLoopFixture {
 public:
  ProfileTests() : loop_(&kAsyncLoopConfigNeverAttachToThread) { loop_.StartThread(); }

  ~ProfileTests() override { loop_.Shutdown(); }

  std::shared_ptr<Profile> CreateNewProfile() {
    auto p = std::make_shared<Profile>(loop_.dispatcher());
    // profile will run on its own thread, so save it so that it doesn't die before loop variable.
    profiles_.push_back(p);
    return p;
  }

  fidl::InterfacePtr<fuchsia::examples::diagnostics::Profile> GetHandler(
      const std::shared_ptr<Profile>& profile) {
    fidl::InterfacePtr<fuchsia::examples::diagnostics::Profile> ptr;
    profile->AddBinding(ptr.NewRequest(dispatcher()));
    return ptr;
  }

  fidl::InterfacePtr<fuchsia::examples::diagnostics::ProfileReader> GetReaderHandler(
      const std::shared_ptr<Profile>& profile) {
    fidl::InterfacePtr<fuchsia::examples::diagnostics::ProfileReader> ptr;
    profile->AddReaderBinding(ptr.NewRequest(dispatcher()));
    return ptr;
  }

 private:
  std::vector<std::shared_ptr<Profile>> profiles_;
  async::Loop loop_;
};

TEST_F(ProfileTests, Name) {
  auto profile = CreateNewProfile();
  auto client = GetHandler(profile);
  std::string name = "placeholder";
  client->GetName([&](std::string n) { name = std::move(n); });
  RunLoopUntil([&]() { return name != "placeholder"; });
  EXPECT_EQ(name, "");

  std::string set_name = "my_name";
  client->SetName(set_name);
  client->GetName([&](std::string n) { name = std::move(n); });
  RunLoopUntil([&]() { return !name.empty(); });
  EXPECT_EQ(name, set_name);
}

TEST_F(ProfileTests, Balance) {
  auto profile = CreateNewProfile();
  auto client = GetHandler(profile);

  int64_t balance = -1;
  int64_t old_balance = balance;
  client->GetBalance([&](int64_t b) { balance = b; });
  RunLoopUntil([&]() { return balance != old_balance; });
  EXPECT_EQ(balance, 0);
  old_balance = balance;

  // Add balance
  client->AddBalance(4);
  client->WithdrawBalance(2, [](bool status) { ASSERT_TRUE(status); });
  client->AddBalance(10);
  client->WithdrawBalance(13, [](bool status) {
    // balance cannot go into negative so we should get false.
    ASSERT_FALSE(status);
  });
  client->GetBalance([&](int64_t b) { balance = b; });
  RunLoopUntil([&]() { return balance != old_balance; });
  EXPECT_EQ(balance, 12);

  // make sure we can withdraw all balance
  old_balance = balance;
  client->WithdrawBalance(12, [](bool status) { ASSERT_TRUE(status); });
  client->GetBalance([&](int64_t b) { balance = b; });
  RunLoopUntil([&]() { return balance != old_balance; });
  EXPECT_EQ(balance, 0);
}

// Test that reader can read latest changes to profile.
// Disabled because it is flaky.
TEST_F(ProfileTests, DISABLED_NameWithReader) {
  auto profile = CreateNewProfile();
  auto client = GetHandler(profile);
  auto reader = GetReaderHandler(profile);

  std::string name = "placeholder";

  std::string set_name = "my_name";

  client->SetName(set_name);
  reader->GetName([&](std::string n) { name = std::move(n); });

  RunLoopUntil([&]() { return name != "placeholder"; });
  //  Flakes sometimes, this will flake much more in an integration tests.
  EXPECT_EQ(name, set_name);
}

// Test that reader can read latest changes to profile.
// Disabled because it is flaky.
TEST_F(ProfileTests, DISABLED_BalanceWithReader) {
  auto profile = CreateNewProfile();
  auto client = GetHandler(profile);
  auto reader = GetReaderHandler(profile);
  int64_t balance = -1;
  int64_t old_balance = balance;

  // Add balance
  client->AddBalance(4);
  client->WithdrawBalance(2, [](bool status) { ASSERT_TRUE(status); });
  reader->GetBalance([&](int64_t b) { balance = b; });
  RunLoopUntil([&]() { return balance != old_balance; });

  // flaky test and fails more often. Server has a race condition even though
  // it is single threaded.
  EXPECT_EQ(balance, 2);
}
