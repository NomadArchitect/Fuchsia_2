// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/a11y/lib/screen_reader/screen_reader_message_generator.h"

#include <fuchsia/accessibility/semantics/cpp/fidl.h>

#include <memory>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/ui/a11y/lib/screen_reader/i18n/tests/mocks/mock_message_formatter.h"

// This header file has been generated from the strings library fuchsia.intl.l10n.
#include "fuchsia/intl/l10n/cpp/fidl.h"

namespace accessibility_test {
namespace {

using fuchsia::accessibility::semantics::Node;
using fuchsia::accessibility::semantics::Role;
using fuchsia::accessibility::semantics::States;
using fuchsia::intl::l10n::MessageIds;

class ScreenReaderMessageGeneratorTest : public gtest::RealLoopFixture {
 public:
  void SetUp() override {
    auto mock_message_formatter = std::make_unique<MockMessageFormatter>();
    mock_message_formatter_ptr_ = mock_message_formatter.get();
    screen_reader_message_generator_ =
        std::make_unique<a11y::ScreenReaderMessageGenerator>(std::move(mock_message_formatter));
  }

 protected:
  std::unique_ptr<a11y::ScreenReaderMessageGenerator> screen_reader_message_generator_;
  MockMessageFormatter* mock_message_formatter_ptr_;
};

TEST_F(ScreenReaderMessageGeneratorTest, BasicNode) {
  Node node;
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 0u);
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeWithALabel) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeButton) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::BUTTON);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_BUTTON),
                                               "button");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 2u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "button");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeButtonNoLabel) {
  Node node;
  node.set_role(Role::BUTTON);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_BUTTON),
                                               "button");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "button");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeHeader) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::HEADER);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_HEADER),
                                               "header");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 2u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "header");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeImage) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::IMAGE);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_IMAGE),
                                               "image");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 2u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "image");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeSlider) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::SLIDER);
  node.set_states(States());
  node.mutable_states()->set_range_value(10.0);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_SLIDER),
                                               "slider");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 2u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo, 10");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "slider");
}

TEST_F(ScreenReaderMessageGeneratorTest, GenerateByMessageId) {
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_SLIDER),
                                               "slider");
  auto result =
      screen_reader_message_generator_->GenerateUtteranceByMessageId(MessageIds::ROLE_SLIDER);
  ASSERT_TRUE(result.utterance.has_message());
  ASSERT_EQ(result.utterance.message(), "slider");
}

TEST_F(ScreenReaderMessageGeneratorTest, ClickableNode) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.mutable_actions()->push_back(fuchsia::accessibility::semantics::Action::DEFAULT);
  node.set_role(Role::BUTTON);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_BUTTON),
                                               "button");
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::DOUBLE_TAP_HINT),
                                               "double tap to activate");

  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 3u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "button");
  ASSERT_TRUE(result[2].utterance.has_message());
  ASSERT_EQ(result[2].utterance.message(), "double tap to activate");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeRadioButtonSelected) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::RADIO_BUTTON);
  node.mutable_states()->set_selected(true);
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::RADIO_BUTTON_SELECTED), "foo radio button selected");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo radio button selected");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeRadioButtonUnselected) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::RADIO_BUTTON);
  node.mutable_states()->set_selected(false);
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::RADIO_BUTTON_UNSELECTED), "foo radio button unselected");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo radio button unselected");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeRadioButtonEmptyLabel) {
  Node node;
  node.mutable_attributes()->set_label("");
  node.set_role(Role::RADIO_BUTTON);
  node.mutable_states()->set_selected(false);
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::RADIO_BUTTON_UNSELECTED), "radio button unselected");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "radio button unselected");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeRadioButtonMessageFormatterReturnNullopt) {
  Node node;
  node.mutable_attributes()->set_label("");
  node.set_role(Role::RADIO_BUTTON);
  node.mutable_states()->set_selected(false);
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_FALSE(result[0].utterance.has_message());
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeLink) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::LINK);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_LINK),
                                               "link");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 2u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "link");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeLinkEmptyLabel) {
  Node node;
  node.mutable_attributes()->set_label("");
  node.set_role(Role::LINK);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_LINK),
                                               "link");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "link");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeCheckBoxWithoutStates) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::CHECK_BOX);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_CHECKBOX),
                                               "check box");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 2u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "check box");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeCheckBoxWithStates) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::CHECK_BOX);
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ROLE_CHECKBOX),
                                               "check box");
  mock_message_formatter_ptr_->SetMessageForId(static_cast<uint64_t>(MessageIds::ELEMENT_CHECKED),
                                               "checked");
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::ELEMENT_NOT_CHECKED), "not checked");
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::ELEMENT_PARTIALLY_CHECKED), "partially checked");
  node.mutable_states()->set_checked_state(
      fuchsia::accessibility::semantics::CheckedState::CHECKED);

  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 3u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "check box");
  ASSERT_TRUE(result[2].utterance.has_message());
  ASSERT_EQ(result[2].utterance.message(), "checked");

  node.mutable_states()->set_checked_state(
      fuchsia::accessibility::semantics::CheckedState::UNCHECKED);

  result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 3u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "check box");
  ASSERT_TRUE(result[2].utterance.has_message());
  ASSERT_EQ(result[2].utterance.message(), "not checked");

  node.mutable_states()->set_checked_state(fuchsia::accessibility::semantics::CheckedState::MIXED);

  result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 3u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "check box");
  ASSERT_TRUE(result[2].utterance.has_message());
  ASSERT_EQ(result[2].utterance.message(), "partially checked");

  node.mutable_states()->set_checked_state(fuchsia::accessibility::semantics::CheckedState::NONE);

  result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 2u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo");
  ASSERT_TRUE(result[1].utterance.has_message());
  ASSERT_EQ(result[1].utterance.message(), "check box");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeToggleSwitchOn) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::TOGGLE_SWITCH);
  node.mutable_states()->set_toggled_state(fuchsia::accessibility::semantics::ToggledState::ON);
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::ELEMENT_TOGGLED_ON), "foo switch on");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo switch on");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeToggleSwitchOff) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::TOGGLE_SWITCH);
  node.mutable_states()->set_toggled_state(fuchsia::accessibility::semantics::ToggledState::OFF);
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::ELEMENT_TOGGLED_OFF), "foo switch off");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo switch off");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeToggleSwitchIndeterminate) {
  Node node;
  node.mutable_attributes()->set_label("foo");
  node.set_role(Role::TOGGLE_SWITCH);
  node.mutable_states()->set_toggled_state(
      fuchsia::accessibility::semantics::ToggledState::INDETERMINATE);
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::ELEMENT_TOGGLED_OFF), "foo switch off");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "foo switch off");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeToggleSwitchEmptyLabel) {
  Node node;
  node.mutable_attributes()->set_label("");
  node.set_role(Role::TOGGLE_SWITCH);
  node.mutable_states()->set_toggled_state(
      fuchsia::accessibility::semantics::ToggledState::INDETERMINATE);
  mock_message_formatter_ptr_->SetMessageForId(
      static_cast<uint64_t>(MessageIds::ELEMENT_TOGGLED_OFF), "switch off");
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_TRUE(result[0].utterance.has_message());
  ASSERT_EQ(result[0].utterance.message(), "switch off");
}

TEST_F(ScreenReaderMessageGeneratorTest, NodeToggleSwitchMessageFormatterReturnsNullopt) {
  Node node;
  node.mutable_attributes()->set_label("");
  node.set_role(Role::TOGGLE_SWITCH);
  node.mutable_states()->set_toggled_state(
      fuchsia::accessibility::semantics::ToggledState::INDETERMINATE);
  auto result = screen_reader_message_generator_->DescribeNode(&node);
  ASSERT_EQ(result.size(), 1u);
  ASSERT_FALSE(result[0].utterance.has_message());
}

}  // namespace
}  // namespace accessibility_test
