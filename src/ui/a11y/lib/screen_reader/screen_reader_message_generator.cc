// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>

#include <optional>

#include "fuchsia/accessibility/semantics/cpp/fidl.h"
// This header file has been generated from the strings library fuchsia.intl.l10n.
#include "fuchsia/intl/l10n/cpp/fidl.h"
#include "src/ui/a11y/lib/screen_reader/screen_reader_message_generator.h"
#include "src/ui/a11y/lib/screen_reader/util/util.h"

namespace a11y {
namespace {

using fuchsia::accessibility::semantics::Node;
using fuchsia::accessibility::semantics::Role;
using fuchsia::accessibility::tts::Utterance;
using fuchsia::intl::l10n::MessageIds;

static constexpr zx::duration kDefaultDelay = zx::msec(40);
static constexpr zx::duration kLongDelay = zx::msec(100);

// Returns a message that describes the label and range value of a slider.
std::string GetSliderLabelAndRangeMessage(const fuchsia::accessibility::semantics::Node* node) {
  std::string message;
  if (node->has_attributes() && node->attributes().has_label()) {
    message += node->attributes().label();
  }

  if (node->has_states() && node->states().has_range_value()) {
    message = message + ", " + FormatFloat(node->states().range_value());
  }

  return message;
}

// Returns true if the node is clickable in any way (normal click, long click).
bool NodeIsClickable(const Node* node) {
  if (!node->has_actions()) {
    return false;
  }
  for (const auto& action : node->actions()) {
    if (action == fuchsia::accessibility::semantics::Action::DEFAULT) {
      return true;
    }
  }
  return false;
}

}  // namespace

ScreenReaderMessageGenerator::ScreenReaderMessageGenerator(
    std::unique_ptr<i18n::MessageFormatter> message_formatter)
    : message_formatter_(std::move(message_formatter)) {}

std::vector<ScreenReaderMessageGenerator::UtteranceAndContext>
ScreenReaderMessageGenerator::DescribeNode(const Node* node) {
  std::vector<UtteranceAndContext> description;
  {
    // If this node is a radio button or a toggle switch, the label is part of the whole message
    // that describes it.
    if (node->has_role() && node->role() == fuchsia::accessibility::semantics::Role::RADIO_BUTTON) {
      description.emplace_back(DescribeRadioButton(node));
    } else if (node->has_role() &&
               node->role() == fuchsia::accessibility::semantics::Role::TOGGLE_SWITCH) {
      description.emplace_back(DescribeToggleSwitch(node));
    } else if (node->has_attributes() && node->attributes().has_label() &&
               !node->attributes().label().empty()) {
      Utterance utterance;
      utterance.set_message(node->attributes().label());

      // Note that empty descriptions (no labels), are allowed. It is common for developers forget
      // to add accessible labels to their UI elements, which causes them to not have one. It is
      // desirable still to tell the user what the node is (a button). But because our TTS does not
      // support empty utterances we only send the string for "button" in this case.
      description.emplace_back(UtteranceAndContext{.utterance = std::move(utterance)});
    }
  }
  {
    Utterance utterance;
    if (node->has_role()) {
      if (node->role() == Role::BUTTON) {
        description.emplace_back(GenerateUtteranceByMessageId(MessageIds::ROLE_BUTTON));
      } else if (node->role() == Role::HEADER) {
        description.emplace_back(GenerateUtteranceByMessageId(MessageIds::ROLE_HEADER));
      } else if (node->role() == Role::IMAGE) {
        description.emplace_back(GenerateUtteranceByMessageId(MessageIds::ROLE_IMAGE));
      } else if (node->role() == Role::LINK) {
        description.emplace_back(GenerateUtteranceByMessageId(MessageIds::ROLE_LINK));
      } else if (node->role() == Role::CHECK_BOX) {
        auto check_box_description = DescribeCheckBox(node);
        std::copy(std::make_move_iterator(check_box_description.begin()),
                  std::make_move_iterator(check_box_description.end()),
                  std::back_inserter(description));
      } else if (node->role() == Role::SLIDER) {
        // Add the slider's range value to the label utterance, if specified.
        auto& label_utterance = description.back().utterance;
        label_utterance.set_message(GetSliderLabelAndRangeMessage(node));

        // Add a role description for the slider.
        description.emplace_back(GenerateUtteranceByMessageId(MessageIds::ROLE_SLIDER));
      }
    }
  }
  if (NodeIsClickable(node)) {
    description.emplace_back(GenerateUtteranceByMessageId(MessageIds::DOUBLE_TAP_HINT, kLongDelay));
  }

  return description;
}

ScreenReaderMessageGenerator::UtteranceAndContext
ScreenReaderMessageGenerator::GenerateUtteranceByMessageId(
    MessageIds message_id, zx::duration delay, const std::vector<std::string>& arg_names,
    const std::vector<std::string>& arg_values) {
  UtteranceAndContext utterance;
  auto message = message_formatter_->FormatStringById(static_cast<uint64_t>(message_id), arg_names,
                                                      arg_values);
  if (message != std::nullopt) {
    utterance.utterance.set_message(std::move(*message));
    utterance.delay = delay;
  }
  return utterance;
}

ScreenReaderMessageGenerator::UtteranceAndContext ScreenReaderMessageGenerator::DescribeRadioButton(
    const fuchsia::accessibility::semantics::Node* node) {
  FX_DCHECK(node->has_role() &&
            node->role() == fuchsia::accessibility::semantics::Role::RADIO_BUTTON);
  const auto message_id =
      node->has_states() && node->states().has_selected() && node->states().selected()
          ? MessageIds::RADIO_BUTTON_SELECTED
          : MessageIds::RADIO_BUTTON_UNSELECTED;
  const auto name_value =
      node->has_attributes() && node->attributes().has_label() ? node->attributes().label() : "";
  if (!name_value.empty()) {
    return GenerateUtteranceByMessageId(message_id, zx::duration(zx::msec(0)), {"name"},
                                        {name_value});
  }

  return GenerateUtteranceByMessageId(message_id);
}

std::vector<ScreenReaderMessageGenerator::UtteranceAndContext>
ScreenReaderMessageGenerator::DescribeCheckBox(
    const fuchsia::accessibility::semantics::Node* node) {
  FX_DCHECK(node->has_role() && node->role() == fuchsia::accessibility::semantics::Role::CHECK_BOX);
  std::vector<ScreenReaderMessageGenerator::UtteranceAndContext> description;
  description.emplace_back(GenerateUtteranceByMessageId(MessageIds::ROLE_CHECKBOX, kDefaultDelay));
  if (node->has_states() && node->states().has_checked_state() &&
      node->states().checked_state() != fuchsia::accessibility::semantics::CheckedState::NONE) {
    MessageIds message_id = MessageIds::ELEMENT_NOT_CHECKED;
    switch (node->states().checked_state()) {
      case fuchsia::accessibility::semantics::CheckedState::CHECKED:
        message_id = MessageIds::ELEMENT_CHECKED;
        break;
      case fuchsia::accessibility::semantics::CheckedState::UNCHECKED:
        message_id = MessageIds::ELEMENT_NOT_CHECKED;
        break;
      case fuchsia::accessibility::semantics::CheckedState::MIXED:
        message_id = MessageIds::ELEMENT_PARTIALLY_CHECKED;
        break;
      case fuchsia::accessibility::semantics::CheckedState::NONE:
        // When none is present, return without a description of the state.
        return description;
    }
    description.emplace_back(GenerateUtteranceByMessageId(message_id));
  }
  return description;
}

ScreenReaderMessageGenerator::UtteranceAndContext
ScreenReaderMessageGenerator::DescribeToggleSwitch(
    const fuchsia::accessibility::semantics::Node* node) {
  FX_DCHECK(node->has_role() &&
            node->role() == fuchsia::accessibility::semantics::Role::TOGGLE_SWITCH);
  const auto message_id =
      node->has_states() && node->states().has_toggled_state() &&
              node->states().toggled_state() == fuchsia::accessibility::semantics::ToggledState::ON
          ? MessageIds::ELEMENT_TOGGLED_ON
          : MessageIds::ELEMENT_TOGGLED_OFF;
  const auto name_value =
      node->has_attributes() && node->attributes().has_label() ? node->attributes().label() : "";

  if (!name_value.empty()) {
    return GenerateUtteranceByMessageId(message_id, zx::duration(zx::msec(0)), {"name"},
                                        {name_value});
  }
  return GenerateUtteranceByMessageId(message_id);
}

}  // namespace a11y
