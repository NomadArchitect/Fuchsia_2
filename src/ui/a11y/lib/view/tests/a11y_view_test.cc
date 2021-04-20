// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/a11y/lib/view/a11y_view.h"

#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl_test_base.h>
#include <lib/gtest/test_loop_fixture.h>
#include <lib/sys/cpp/testing/component_context_provider.h>
#include <lib/sys/cpp/testing/fake_component.h>

#include <set>
#include <unordered_map>
#include <vector>

#include <gtest/gtest.h>

#include "src/ui/a11y/lib/util/util.h"
#include "src/ui/a11y/lib/view/tests/mocks/scenic_mocks.h"

namespace accessibility_test {
namespace {

class FakeAccessibilityViewRegistry : public fuchsia::ui::accessibility::view::Registry {
 public:
  FakeAccessibilityViewRegistry(fuchsia::ui::views::ViewHolderToken client_view_holder_token)
      : client_view_holder_token_(std::move(client_view_holder_token)) {}
  ~FakeAccessibilityViewRegistry() override = default;

  // |fuchsia::ui::accessibility::view::Registry|
  void CreateAccessibilityViewHolder(fuchsia::ui::views::ViewRef a11y_view_ref,
                                     fuchsia::ui::views::ViewHolderToken a11y_view_holder_token,
                                     CreateAccessibilityViewHolderCallback callback) override {
    a11y_view_ref_ = std::move(a11y_view_ref);
    callback(std::move(client_view_holder_token_));
  }

  fidl::InterfaceRequestHandler<fuchsia::ui::accessibility::view::Registry> GetHandler(
      async_dispatcher_t* dispatcher = nullptr) {
    return [this, dispatcher](
               fidl::InterfaceRequest<fuchsia::ui::accessibility::view::Registry> request) {
      bindings_.AddBinding(this, std::move(request), dispatcher);
    };
  }

  const fuchsia::ui::views::ViewRef& a11y_view_ref() { return a11y_view_ref_; }

 private:
  fuchsia::ui::views::ViewHolderToken client_view_holder_token_;
  fuchsia::ui::views::ViewRef a11y_view_ref_;
  fidl::BindingSet<fuchsia::ui::accessibility::view::Registry> bindings_;
};

class AccessibilityViewTest : public gtest::TestLoopFixture {
 public:
  AccessibilityViewTest() = default;
  ~AccessibilityViewTest() override = default;

  void SetUp() override {
    gtest::TestLoopFixture::SetUp();

    mock_session_ = std::make_unique<MockSession>();
    mock_scenic_ = std::make_unique<MockScenic>(mock_session_.get());

    auto [client_view_token, client_view_holder_token] = scenic::ViewTokenPair::New();
    fidl::Clone(client_view_holder_token, &client_view_holder_token_);
    fake_accessibility_view_registry_ =
        std::make_unique<FakeAccessibilityViewRegistry>(std::move(client_view_holder_token));

    context_provider_.service_directory_provider()->AddService(mock_scenic_->GetHandler());
    context_provider_.service_directory_provider()->AddService(
        fake_accessibility_view_registry_->GetHandler());

    RunLoopUntilIdle();
  }

 protected:
  sys::testing::ComponentContextProvider context_provider_;
  std::unique_ptr<MockSession> mock_session_;
  std::unique_ptr<MockScenic> mock_scenic_;
  std::unique_ptr<FakeAccessibilityViewRegistry> fake_accessibility_view_registry_;
  fuchsia::ui::views::ViewHolderToken client_view_holder_token_;
};

TEST_F(AccessibilityViewTest, TestConstruction) {
  fuchsia::ui::scenic::ScenicPtr scenic =
      context_provider_.context()->svc()->Connect<fuchsia::ui::scenic::Scenic>();

  a11y::AccessibilityView a11y_view(context_provider_.context(), std::move(scenic));

  RunLoopUntilIdle();

  EXPECT_TRUE(mock_scenic_->create_session_called());

  // Verify that a11y view was created.
  const auto& views = mock_session_->views();
  EXPECT_EQ(views.size(), 1u);

  // Verify that a11y view ref was passed to accessibility view registry.
  EXPECT_EQ(a11y::GetKoid(views.begin()->second.view_ref),
            a11y::GetKoid(fake_accessibility_view_registry_->a11y_view_ref()));

  // Verify that client view holder was created as a child of the a11y view.
  const auto& view_holders = mock_session_->view_holders();
  EXPECT_EQ(view_holders.size(), 1u);
  EXPECT_EQ(view_holders.begin()->second.parent_id, views.begin()->second.id);
}

}  // namespace
}  // namespace accessibility_test
