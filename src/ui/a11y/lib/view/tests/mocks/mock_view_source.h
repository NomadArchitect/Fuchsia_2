// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_A11Y_LIB_VIEW_TESTS_MOCKS_MOCK_VIEW_SOURCE_H_
#define SRC_UI_A11Y_LIB_VIEW_TESTS_MOCKS_MOCK_VIEW_SOURCE_H_

#include <fuchsia/ui/views/cpp/fidl.h>

#include <unordered_map>

#include "src/ui/a11y/lib/testing/view_ref_helper.h"
#include "src/ui/a11y/lib/view/view_source.h"
#include "src/ui/a11y/lib/view/view_wrapper.h"

namespace accessibility_test {

class MockViewSource : public a11y::ViewSource {
 public:
  MockViewSource() = default;
  ~MockViewSource() override = default;

  // |ViewSource|
  fxl::WeakPtr<a11y::ViewWrapper> GetViewWrapper(zx_koid_t koid) override;

  // Creates a wrapper for the supplied view_ref, with mock functional
  // interfaces.
  void CreateView(const ViewRefHelper& view_ref);

 private:
  std::unordered_map<zx_koid_t, std::unique_ptr<a11y::ViewWrapper>> views_;
};

}  // namespace accessibility_test

#endif  // SRC_UI_A11Y_LIB_VIEW_TESTS_MOCKS_MOCK_VIEW_SOURCE_H_
