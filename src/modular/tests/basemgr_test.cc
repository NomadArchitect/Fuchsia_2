// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/testing/modular/cpp/fidl.h>
#include <lib/inspect/contrib/cpp/archive_reader.h>
#include <lib/modular/testing/cpp/fake_component.h>
#include <lib/stdcompat/optional.h>

#include <gmock/gmock.h>
#include <sdk/lib/modular/testing/cpp/fake_agent.h>

#include "src/lib/fsl/vmo/strings.h"
#include "src/modular/lib/fidl/clone.h"
#include "src/modular/lib/modular_config/modular_config.h"
#include "src/modular/lib/modular_config/modular_config_constants.h"
#include "src/modular/lib/modular_test_harness/cpp/fake_session_launcher_component.h"
#include "src/modular/lib/modular_test_harness/cpp/fake_session_shell.h"
#include "src/modular/lib/modular_test_harness/cpp/test_harness_fixture.h"
#include "src/modular/lib/pseudo_dir/pseudo_dir_server.h"

namespace {

using ::testing::HasSubstr;

constexpr char kBasemgrSelector[] = "*_inspect/basemgr.cmx:root";
constexpr char kBasemgrComponentName[] = "basemgr.cmx";

class BasemgrTest : public modular_testing::TestHarnessFixture {
 public:
  BasemgrTest() : executor_(dispatcher()) {}

  fpromise::result<inspect::contrib::DiagnosticsData> GetInspectDiagnosticsData() {
    auto archive = real_services()->Connect<fuchsia::diagnostics::ArchiveAccessor>();

    inspect::contrib::ArchiveReader reader(std::move(archive), {kBasemgrSelector});
    fpromise::result<std::vector<inspect::contrib::DiagnosticsData>, std::string> result;
    executor_.schedule_task(
        reader.SnapshotInspectUntilPresent({kBasemgrComponentName})
            .then([&](fpromise::result<std::vector<inspect::contrib::DiagnosticsData>, std::string>&
                          snapshot_result) { result = std::move(snapshot_result); }));
    RunLoopUntil([&] { return result.is_ok() || result.is_error(); });

    if (result.is_error()) {
      EXPECT_FALSE(result.is_error()) << "Error was " << result.error();
      return fpromise::error();
    }

    if (result.value().size() != 1) {
      EXPECT_EQ(1u, result.value().size()) << "Expected only one component";
      return fpromise::error();
    }

    return fpromise::ok(std::move(result.value()[0]));
  }

  async::Executor executor_;
};

// Tests that when multiple session shell are provided the first is picked
TEST_F(BasemgrTest, StartFirstShellWhenMultiple) {
  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  // Session shells used in list
  auto session_shell = modular_testing::FakeSessionShell::CreateWithDefaultOptions();
  auto session_shell2 = modular_testing::FakeSessionShell::CreateWithDefaultOptions();

  // Create session shell list (appended in order)
  builder.InterceptSessionShell(session_shell->BuildInterceptOptions());
  builder.InterceptSessionShell(session_shell2->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  // Run until one is started
  RunLoopUntil([&] { return session_shell->is_running() || session_shell2->is_running(); });

  // Assert only first one is started
  EXPECT_TRUE(session_shell->is_running());
  EXPECT_FALSE(session_shell2->is_running());
}

// Tests that basemgr starts the configured session launcher component when basemgr starts.
TEST_F(BasemgrTest, StartsSessionComponent) {
  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  auto session_launcher_component =
      modular_testing::FakeSessionLauncherComponent::CreateWithDefaultOptions();

  builder.InterceptSessionLauncherComponent(session_launcher_component->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_launcher_component->is_running(); });

  EXPECT_TRUE(session_launcher_component->is_running());
}

// Tests that basemgr starts the configured session launcher component with the given args.
TEST_F(BasemgrTest, StartsSessionComponentWithArgs) {
  static const std::string kTestArg = "--foo";

  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  fidl::VectorPtr<std::string> startup_args = std::nullopt;
  builder.InterceptSessionLauncherComponent(
      {.url = modular_testing::TestHarnessBuilder::GenerateFakeUrl(),
       .launch_handler = [&startup_args](
                             fuchsia::sys::StartupInfo startup_info,
                             fidl::InterfaceHandle<fuchsia::modular::testing::InterceptedComponent>
                             /* unused */) { startup_args = startup_info.launch_info.arguments; }},
      /*args=*/cpp17::make_optional<std::vector<std::string>>({kTestArg}));
  builder.BuildAndRun(test_harness());

  // Run until the session launcher component is started.
  RunLoopUntil([&] { return startup_args.has_value(); });

  ASSERT_TRUE(startup_args.has_value());
  ASSERT_EQ(1u, startup_args->size());
  EXPECT_EQ(kTestArg, startup_args->at(0));
}

// Tests that basemgr starts a session with the given configuration when instructed by
// the session launcher component.
TEST_F(BasemgrTest, StartsSessionWithConfig) {
  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  auto session_launcher_component =
      modular_testing::FakeSessionLauncherComponent::CreateWithDefaultOptions();
  auto session_shell = modular_testing::FakeSessionShell::CreateWithDefaultOptions();

  builder.InterceptSessionLauncherComponent(session_launcher_component->BuildInterceptOptions());
  // The session shell is specified in the configuration generated by the session launcher
  // component, so avoid InterceptSessionShell(), which adds it to the configuration in |builder|.
  builder.InterceptComponent(session_shell->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_launcher_component->is_running(); });

  EXPECT_TRUE(!session_shell->is_running());

  // Create the configuration that the session launcher component passes to basemgr.
  fuchsia::modular::session::SessionShellMapEntry entry;
  entry.mutable_config()->mutable_app_config()->set_url(session_shell->url());
  fuchsia::modular::session::ModularConfig config;
  config.mutable_basemgr_config()->mutable_session_shell_map()->push_back(std::move(entry));

  fuchsia::mem::Buffer config_buf;
  ASSERT_TRUE(fsl::VmoFromString(modular::ConfigToJsonString(config), &config_buf));

  // Launch the session.
  session_launcher_component->launcher()->LaunchSessionmgr(std::move(config_buf));

  // The configured session shell should start.
  RunLoopUntil([&] { return session_shell->is_running(); });

  EXPECT_TRUE(session_shell->is_running());
}

// Tests that the session launcher component can also provide services to sessionmgr's children.
TEST_F(BasemgrTest, SessionLauncherCanOfferServices) {
  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  auto session_launcher_component =
      modular_testing::FakeSessionLauncherComponent::CreateWithDefaultOptions();
  auto session_shell = modular_testing::FakeSessionShell::CreateWithDefaultOptions();
  auto agent = modular_testing::FakeAgent::CreateWithDefaultOptions();

  builder.InterceptSessionLauncherComponent(session_launcher_component->BuildInterceptOptions());
  // The following components are specified in the configuration generated by the session launcher.
  builder.InterceptComponent(session_shell->BuildInterceptOptions());
  auto agent_options = agent->BuildInterceptOptions();
  agent_options.sandbox_services.push_back(fuchsia::testing::modular::TestProtocol::Name_);
  builder.InterceptComponent(std::move(agent_options));
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_launcher_component->is_running(); });

  EXPECT_TRUE(!session_shell->is_running());

  // Create the configuration that the session launcher component passes to basemgr.
  fuchsia::modular::session::SessionShellMapEntry entry;
  entry.mutable_config()->mutable_app_config()->set_url(session_shell->url());
  fuchsia::modular::session::ModularConfig config;
  config.mutable_basemgr_config()->mutable_session_shell_map()->push_back(std::move(entry));
  config.mutable_sessionmgr_config()->mutable_session_agents()->push_back(agent->url());

  fuchsia::mem::Buffer config_buf;
  ASSERT_TRUE(fsl::VmoFromString(modular::ConfigToJsonString(config), &config_buf));

  // Build a directory to serve services from the session launcher component.
  int connect_count = 0;
  auto dir = std::make_unique<vfs::PseudoDir>();
  dir->AddEntry(
      fuchsia::testing::modular::TestProtocol::Name_,
      std::make_unique<vfs::Service>([&](zx::channel, async_dispatcher_t*) { ++connect_count; }));
  auto dir_server = std::make_unique<modular::PseudoDirServer>(std::move(dir));

  // Construct a ServiceList with the above dir server.
  fuchsia::sys::ServiceList service_list;
  service_list.names.push_back(fuchsia::testing::modular::TestProtocol::Name_);
  service_list.host_directory = dir_server->Serve().Unbind().TakeChannel();

  // Launch the session.
  session_launcher_component->launcher()->LaunchSessionmgrWithServices(std::move(config_buf),
                                                                       std::move(service_list));

  // The configured session shell and agent should start.
  RunLoopUntil([&] { return session_shell->is_running() && agent->is_running(); });

  // Connect to the provided service from the agent.
  auto test_ptr =
      agent->component_context()->svc()->Connect<fuchsia::testing::modular::TestProtocol>();
  RunLoopUntil([&] { return connect_count > 0; });
  EXPECT_EQ(1, connect_count);

  // Test that the provided services can still be reached if the session is restarted
  // (fxbug.dev/61680).
  session_shell->Exit(1);
  RunLoopUntil([&] { return !session_shell->is_running() && !agent->is_running(); });
  RunLoopUntil([&] { return session_shell->is_running() && agent->is_running(); });
  auto test_ptr2 =
      agent->component_context()->svc()->Connect<fuchsia::testing::modular::TestProtocol>();
  RunLoopUntil([&] { return connect_count > 1; });
  EXPECT_EQ(2, connect_count);
}

// Tests that basemgr starts a new session with a new configuration, and stops the existing one
// when instructed to launch a new session by the session launcher component.
TEST_F(BasemgrTest, LaunchSessionmgrReplacesExistingSession) {
  // Instructs the session launcher component to launches a session with a configuration with the
  // given session shell URL, and waits until it's launched.
  auto launch_session_with_session_shell =
      [](modular_testing::FakeSessionLauncherComponent& session_launcher_component,
         std::string session_shell_url) {
        fuchsia::modular::session::SessionShellMapEntry entry;
        entry.mutable_config()->mutable_app_config()->set_url(std::move(session_shell_url));
        fuchsia::modular::session::ModularConfig config;
        config.mutable_basemgr_config()->mutable_session_shell_map()->push_back(std::move(entry));

        fuchsia::mem::Buffer config_buf;
        ASSERT_TRUE(fsl::VmoFromString(modular::ConfigToJsonString(config), &config_buf));

        session_launcher_component.launcher()->LaunchSessionmgr(std::move(config_buf));
      };

  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  auto session_launcher_component =
      modular_testing::FakeSessionLauncherComponent::CreateWithDefaultOptions();
  auto session_shell = modular_testing::FakeSessionShell::CreateWithDefaultOptions();
  auto session_shell_2 = modular_testing::FakeSessionShell::CreateWithDefaultOptions();

  builder.InterceptSessionLauncherComponent(session_launcher_component->BuildInterceptOptions());
  builder.InterceptComponent(session_shell->BuildInterceptOptions());
  builder.InterceptComponent(session_shell_2->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_launcher_component->is_running(); });
  EXPECT_TRUE(!session_shell->is_running());

  // Launch the first session.
  launch_session_with_session_shell(*session_launcher_component, session_shell->url());

  // The first session shell should start.
  RunLoopUntil([&] { return session_shell->is_running(); });
  EXPECT_TRUE(session_shell->is_running());

  // Launch the second session.
  launch_session_with_session_shell(*session_launcher_component, session_shell_2->url());

  // The second session shell should start, and the first shell should stop.
  RunLoopUntil([&] { return !session_shell->is_running() && session_shell_2->is_running(); });

  EXPECT_FALSE(session_shell->is_running());
  EXPECT_TRUE(session_shell_2->is_running());
}

// Tests that LaunchSessionmgr closes the channel with an ZX_ERR_INVALID_ARGS epitaph if the
// config buffer is not readable.
TEST_F(BasemgrTest, LaunchSessionmgrFailsGivenUnreadableBuffer) {
  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  auto session_launcher_component =
      modular_testing::FakeSessionLauncherComponent::CreateWithDefaultOptions();

  builder.InterceptSessionLauncherComponent(session_launcher_component->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_launcher_component->is_running(); });

  // Launch the session with a configuration Buffer that has an incorrect size.
  fuchsia::mem::Buffer config_buf;
  ASSERT_TRUE(fsl::VmoFromString("", &config_buf));
  config_buf.size = 1;

  // Connect to Launcher with a handler that lets us capture the error.
  fuchsia::modular::session::LauncherPtr launcher;
  session_launcher_component->component_context()->svc()->Connect(launcher.NewRequest());

  bool error_handler_called{false};
  zx_status_t error_status{ZX_OK};
  launcher.set_error_handler([&](zx_status_t status) {
    error_handler_called = true;
    error_status = status;
  });

  launcher->LaunchSessionmgr(std::move(config_buf));

  RunLoopUntil([&] { return error_handler_called; });

  EXPECT_EQ(ZX_ERR_INVALID_ARGS, error_status);
}

// Tests that LaunchSessionmgr closes the channel with an ZX_ERR_INVALID_ARGS epitaph if the
// config buffer does not contain valid Modular configuration JSON.
TEST_F(BasemgrTest, LaunchSessionmgrFailsGivenInvalidConfigJson) {
  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  auto session_launcher_component =
      modular_testing::FakeSessionLauncherComponent::CreateWithDefaultOptions();

  builder.InterceptSessionLauncherComponent(session_launcher_component->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_launcher_component->is_running(); });

  // Launch the session with a configuration that is not valid JSON.
  fuchsia::mem::Buffer config_buf;
  ASSERT_TRUE(fsl::VmoFromString("this is not valid json", &config_buf));

  // Connect to Launcher with a handler that lets us capture the error.
  fuchsia::modular::session::LauncherPtr launcher;
  session_launcher_component->component_context()->svc()->Connect(launcher.NewRequest());

  bool error_handler_called{false};
  zx_status_t error_status{ZX_OK};
  launcher.set_error_handler([&](zx_status_t status) {
    error_handler_called = true;
    error_status = status;
  });

  launcher->LaunchSessionmgr(std::move(config_buf));

  RunLoopUntil([&] { return error_handler_called; });

  EXPECT_EQ(ZX_ERR_INVALID_ARGS, error_status);
}

// Tests that LaunchSessionmgr closes the channel with an ZX_ERR_INVALID_ARGS epitaph if the
// config includes a session launcher component.
TEST_F(BasemgrTest, LaunchSessionmgrFailsGivenConfigWithSessionLauncher) {
  static constexpr auto kTestSessionLauncherUrl =
      "fuchsia-pkg://fuchsia.com/test_session_launcher#meta/test_session_launcher.cmx";

  fuchsia::modular::testing::TestHarnessSpec spec;
  modular_testing::TestHarnessBuilder builder(std::move(spec));

  auto session_launcher_component =
      modular_testing::FakeSessionLauncherComponent::CreateWithDefaultOptions();

  builder.InterceptSessionLauncherComponent(session_launcher_component->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_launcher_component->is_running(); });

  // Launch the session with a valid configuration that has `session_launcher` set.
  fuchsia::modular::session::ModularConfig config;
  config.mutable_basemgr_config()->mutable_session_launcher()->set_url(kTestSessionLauncherUrl);

  fuchsia::mem::Buffer config_buf;
  ASSERT_TRUE(fsl::VmoFromString(modular::ConfigToJsonString(config), &config_buf));

  // Connect to Launcher with a handler that lets us capture the error.
  fuchsia::modular::session::LauncherPtr launcher;
  session_launcher_component->component_context()->svc()->Connect(launcher.NewRequest());

  bool error_handler_called{false};
  zx_status_t error_status{ZX_OK};
  launcher.set_error_handler([&](zx_status_t status) {
    error_handler_called = true;
    error_status = status;
  });

  launcher->LaunchSessionmgr(std::move(config_buf));

  RunLoopUntil([&] { return error_handler_called; });

  EXPECT_EQ(ZX_ERR_INVALID_ARGS, error_status);
}

// Tests that basemgr exposes its configuration in Inspect.
TEST_F(BasemgrTest, ExposesConfigInInspect) {
  auto session_shell = modular_testing::FakeSessionShell::CreateWithDefaultOptions();

  fuchsia::modular::testing::TestHarnessSpec spec;
  spec.set_environment_suffix("inspect");

  modular_testing::TestHarnessBuilder builder(std::move(spec));
  builder.InterceptSessionShell(session_shell->BuildInterceptOptions());
  builder.BuildAndRun(test_harness());

  RunLoopUntil([&] { return session_shell->is_running(); });

  auto inspect_result = GetInspectDiagnosticsData();
  ASSERT_TRUE(inspect_result.is_ok());
  auto inspect_data = inspect_result.take_value();

  // The inspect property should contain configuration that uses |session_shell|.
  const auto& config_value = inspect_data.GetByPath({"root", modular_config::kInspectConfig});
  ASSERT_TRUE(config_value.IsString());
  EXPECT_THAT(config_value.GetString(), HasSubstr(session_shell->url()));
}

}  // namespace
