// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_TESTS_INTEGRATION_FLUTTER_TESTS_EMBEDDER_FLUTTER_EMBEDDER_TEST_H_
#define SRC_UI_TESTS_INTEGRATION_FLUTTER_TESTS_EMBEDDER_FLUTTER_EMBEDDER_TEST_H_

#include <fuchsia/ui/policy/cpp/fidl.h>
#include <fuchsia/ui/scenic/cpp/fidl.h>
#include <fuchsia/ui/views/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/testing/test_with_environment.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_token_pair.h>
#include <lib/zx/clock.h>
#include <zircon/status.h>
#include <zircon/time.h>

#include "src/lib/ui/base_view/embedded_view_utils.h"
#include "src/ui/testing/views/color.h"
#include "src/ui/testing/views/embedder_view.h"

namespace flutter_embedder_test {

/// Defines a list of services that are injected into the test environment. Unlike the
/// injected-services in CMX which are injected per test package, these are injected per test and
/// result in a more hermetic test environment.
constexpr std::size_t NUM_SERVICES = 14;
constexpr std::array<std::pair<const char*, const char*>, NUM_SERVICES> kInjectedServices = {{
    // clang-format off
    {
      "fuchsia.accessibility.semantics.SemanticsManager",
      "fuchsia-pkg://fuchsia.com/a11y-manager#meta/a11y-manager.cmx"
    },{
      "fuchsia.deprecatedtimezone.Timezone",
      "fuchsia-pkg://fuchsia.com/timezone#meta/timezone.cmx"
    },{
      "fuchsia.fonts.Provider",
      "fuchsia-pkg://fuchsia.com/fonts#meta/fonts.cmx"
    },{
      "fuchsia.hardware.display.Provider",
      "fuchsia-pkg://fuchsia.com/fake-hardware-display-controller-provider#meta/hdcp.cmx"
    },{
      "fuchsia.intl.PropertyProvider",
      "fuchsia-pkg://fuchsia.com/intl_property_manager#meta/intl_property_manager.cmx"
    },{
      "fuchsia.netstack.Netstack",
      "fuchsia-pkg://fuchsia.com/netstack#meta/netstack.cmx"
    },{
      "fuchsia.posix.socket.Provider",
      "fuchsia-pkg://fuchsia.com/netstack#meta/netstack.cmx"
    },{
      "fuchsia.tracing.provider.Registry",
      "fuchsia-pkg://fuchsia.com/trace_manager#meta/trace_manager.cmx"
    },{
      "fuchsia.ui.input.ImeService",
      "fuchsia-pkg://fuchsia.com/ime_service#meta/ime_service.cmx"
    },{
      "fuchsia.ui.input.ImeVisibilityService",
      "fuchsia-pkg://fuchsia.com/ime_service#meta/ime_service.cmx"
    },{
      "fuchsia.ui.scenic.Scenic",
      "fuchsia-pkg://fuchsia.com/scenic#meta/scenic.cmx"
    },{
      "fuchsia.ui.pointerinjector.Registry",
      "fuchsia-pkg://fuchsia.com/scenic#meta/scenic.cmx"
    },{
      "fuchsia.ui.policy.Presenter",
      "fuchsia-pkg://fuchsia.com/root_presenter#meta/root_presenter.cmx"
    },{
      "fuchsia.ui.input.InputDeviceRegistry",
      "fuchsia-pkg://fuchsia.com/root_presenter#meta/root_presenter.cmx"
    },
    // clang-format on
}};

// Timeout when waiting on Scenic API calls like |GetDisplayInfo|.
constexpr zx::duration kCallTimeout = zx::sec(5);
// Timeout for Scenic's |TakeScreenshot| FIDL call.
constexpr zx::duration kScreenshotTimeout = zx::sec(10);
// Timeout to fail the test if it goes beyond this duration.
constexpr zx::duration kTestTimeout = zx::min(1);

class FlutterEmbedderTestsBase : public sys::testing::TestWithEnvironment {
 public:
  explicit FlutterEmbedderTestsBase(
      const std::array<std::pair<const char*, const char*>, NUM_SERVICES> injected_services)
      : injected_services_(injected_services) {}

  // |testing::Test|
  void SetUp() override {
    TestWithEnvironment::SetUp();
    // This is done in |SetUp| as opposed to the constructor to allow subclasses the opportunity to
    // override |CreateServices()|.
    auto services = TestWithEnvironment::CreateServices();
    CreateServices(services);

    // Add test-specific launchable services.
    for (const auto& service_info : injected_services_) {
      zx_status_t status =
          services->AddServiceWithLaunchInfo({.url = service_info.second}, service_info.first);
      FX_CHECK(status == ZX_OK) << "Failed to add service " << service_info.first;
    }

    environment_ = CreateNewEnclosingEnvironment("flutter-embedder-tests", std::move(services),
                                                 {.inherit_parent_services = true});
    WaitForEnclosingEnvToStart(environment());

    FX_VLOGS(1) << "Created test environment.";

    // Post a "just in case" quit task, if the test hangs.
    async::PostDelayedTask(
        dispatcher(),
        [] { FX_LOGS(FATAL) << "\n\n>> Test did not complete in time, terminating.  <<\n\n"; },
        kTestTimeout);
  }

  // Configures services available to the test environment. This method is called by |SetUp()|. It
  // shadows but calls |TestWithEnvironment::CreateServices()|.
  virtual void CreateServices(std::unique_ptr<sys::testing::EnvironmentServices>& services) {}

  sys::testing::EnclosingEnvironment* environment() { return environment_.get(); }

  fuchsia::ui::views::ViewToken CreatePresentationViewToken() {
    auto [view_token, view_holder_token] = scenic::ViewTokenPair::New();

    auto presenter = environment()->ConnectToService<fuchsia::ui::policy::Presenter>();
    presenter.set_error_handler(
        [](zx_status_t status) { FAIL() << "presenter: " << zx_status_get_string(status); });
    presenter->PresentView(std::move(view_holder_token), nullptr);

    return std::move(view_token);
  }

 private:
  const std::array<std::pair<const char*, const char*>, NUM_SERVICES> injected_services_;
  std::unique_ptr<sys::ComponentContext> const component_context_;
  std::unique_ptr<sys::testing::EnclosingEnvironment> environment_;
};

class FlutterEmbedderTests : public FlutterEmbedderTestsBase {
 public:
  FlutterEmbedderTests() : FlutterEmbedderTestsBase(kInjectedServices) {}

  // |testing::Test|
  void SetUp() override {
    ASSERT_NO_FATAL_FAILURE(FlutterEmbedderTestsBase::SetUp());

    // Connect to scenic to ensure it is up and running.
    auto scenic = environment()->ConnectToService<fuchsia::ui::scenic::Scenic>();
    scenic->GetDisplayInfo([this](fuchsia::ui::gfx::DisplayInfo info) { QuitLoop(); });
    RunLoopWithTimeout(kCallTimeout);
  }

  void RunAppWithArgs(const std::string& component_url,
                      const std::vector<std::string>& component_args = {}) {
    fuchsia::ui::scenic::ScenicPtr scenic;
    environment()->ConnectToService(scenic.NewRequest());
    scenic.set_error_handler([](zx_status_t status) {
      FAIL() << "Lost connection to Scenic: " << zx_status_get_string(status);
    });

    scenic::EmbeddedViewInfo flutter_runner = scenic::LaunchComponentAndCreateView(
        environment()->launcher_ptr(), component_url, component_args);
    flutter_runner.controller.events().OnTerminated = [](auto...) { FAIL(); };

    // Present the view.
    embedder_view_.emplace(scenic::ViewContext{
        .session_and_listener_request =
            scenic::CreateScenicSessionPtrAndListenerRequest(scenic.get()),
        .view_token = CreatePresentationViewToken(),
    });

    // Embed the view.
    bool is_rendering = false;
    embedder_view_->EmbedView(std::move(flutter_runner),
                              [&is_rendering](fuchsia::ui::gfx::ViewState view_state) {
                                is_rendering = view_state.is_rendering;
                              });
    RunLoopUntil([&is_rendering] { return is_rendering; });
    FX_LOGS(INFO) << "Launched component: " << component_url;
  }

  scenic::Screenshot TakeScreenshot() {
    fuchsia::ui::scenic::ScenicPtr scenic;
    environment()->ConnectToService(scenic.NewRequest());
    scenic.set_error_handler([](zx_status_t status) {
      FAIL() << "Lost connection to Scenic: " << zx_status_get_string(status);
    });

    FX_LOGS(INFO) << "Taking screenshot... ";
    fuchsia::ui::scenic::ScreenshotData screenshot_out;
    scenic->TakeScreenshot(
        [this, &screenshot_out](fuchsia::ui::scenic::ScreenshotData screenshot, bool status) {
          EXPECT_TRUE(status) << "Failed to take screenshot";
          screenshot_out = std::move(screenshot);
          QuitLoop();
        });
    EXPECT_FALSE(RunLoopWithTimeout(kScreenshotTimeout)) << "Timed out waiting for screenshot.";
    FX_LOGS(INFO) << "Screenshot captured.";

    return scenic::Screenshot(screenshot_out);
  }

  bool TakeScreenshotUntil(scenic::Color color,
                           fit::function<void(std::map<scenic::Color, size_t>)> callback = nullptr,
                           zx::duration timeout = kTestTimeout) {
    return RunLoopWithTimeoutOrUntil(
        [this, &callback, &color] {
          auto screenshot = TakeScreenshot();
          auto histogram = screenshot.Histogram();

          bool color_found = histogram[color] > 0;
          if (color_found && callback != nullptr) {
            callback(std::move(histogram));
          }
          return color_found;
        },
        timeout);
  }

  // Inject directly into Root Presenter, using fuchsia.ui.input FIDLs.
  void InjectInput() {
    using fuchsia::ui::input::InputReport;
    // Device parameters
    auto parameters = fuchsia::ui::input::TouchscreenDescriptor::New();
    *parameters = {.x = {.range = {.min = -1000, .max = 1000}},
                   .y = {.range = {.min = -1000, .max = 1000}},
                   .max_finger_id = 10};

    FX_LOGS(INFO) << "Injecting input... ";
    // Register it against Root Presenter.
    fuchsia::ui::input::DeviceDescriptor device{.touchscreen = std::move(parameters)};
    auto registry = environment()->ConnectToService<fuchsia::ui::input::InputDeviceRegistry>();
    fuchsia::ui::input::InputDevicePtr connection;
    registry->RegisterDevice(std::move(device), connection.NewRequest());

    {
      // Inject one input report, then a conclusion (empty) report.
      auto touch = fuchsia::ui::input::TouchscreenReport::New();
      *touch = {.touches = {{.finger_id = 1, .x = 0, .y = 0}}};  // center of display
      InputReport report{.event_time = TimeToUint(MonotonicNow()), .touchscreen = std::move(touch)};
      connection->DispatchReport(std::move(report));
    }

    {
      auto touch = fuchsia::ui::input::TouchscreenReport::New();
      InputReport report{.event_time = TimeToUint(MonotonicNow()), .touchscreen = std::move(touch)};
      connection->DispatchReport(std::move(report));
    }
    FX_LOGS(INFO) << "Input dispatched.";
  }

 private:
  zx::time MonotonicNow() { return zx::clock::get_monotonic(); };

  template <typename TimeT>
  uint64_t TimeToUint(const TimeT& time) {
    FX_CHECK(time.get() >= 0);
    return static_cast<uint64_t>(time.get());
  };
  // Wrapped in optional since the view is not created until the middle of SetUp
  std::optional<scenic::EmbedderView> embedder_view_;
};

}  // namespace flutter_embedder_test

#endif  // SRC_UI_TESTS_INTEGRATION_FLUTTER_TESTS_EMBEDDER_FLUTTER_EMBEDDER_TEST_H_
