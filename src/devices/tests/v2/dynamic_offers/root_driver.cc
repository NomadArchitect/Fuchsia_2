// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.offers.test/cpp/fidl.h>
#include <lib/driver2/driver2_cpp.h>

#include <bind/fuchsia/test/cpp/bind.h>

namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf

namespace fcd = fuchsia_component_decl;
namespace ft = fuchsia_offers_test;

namespace {

const std::string_view kChildName = "leaf";

class RootDriver : public driver::DriverBase, public fidl::Server<ft::Handshake> {
 public:
  RootDriver(driver::DriverStartArgs start_args, fdf::UnownedDispatcher dispatcher)
      : driver::DriverBase("root", std::move(start_args), std::move(dispatcher)) {}

  zx::status<> Start() override {
    node_.Bind(std::move(node()), async_dispatcher());
    // Setup the outgoing directory.
    {
      driver::ServiceInstanceHandler handler;
      ft::Service::Handler service(&handler);

      auto device = [this](fidl::ServerEnd<ft::Handshake> server_end) mutable -> void {
        fidl::BindServer<fidl::Server<ft::Handshake>>(async_dispatcher(), std::move(server_end),
                                                      this);
      };
      zx::status<> status = service.add_device(std::move(device));
      if (status.is_error()) {
        FDF_LOG(ERROR, "Failed to add device %s", status.status_string());
      }
      status = context().outgoing()->AddService<ft::Service>(std::move(handler), kChildName);
      if (status.is_error()) {
        FDF_LOG(ERROR, "Failed to add service %s", status.status_string());
      }
    }

    // Start the driver.
    auto result = AddChild();
    if (result.is_error()) {
      return result.take_error();
    }

    return zx::ok();
  }

 private:
  zx::status<> AddChild() {
    fidl::Arena arena;

    // Set the offers of the node.
    auto service = fcd::OfferService{{
        .source_name = ft::Service::Name,
        .target_name = ft::Service::Name,
    }};

    auto mapping = fcd::NameMapping{{
        .source_name = std::string(kChildName),
        .target_name = "default",
    }};
    service.renamed_instances() = std::vector{std::move(mapping)};

    auto instance_filter = std::string("default");
    service.source_instance_filter() = std::vector{std::move(instance_filter)};

    auto offer = fcd::Offer::WithService(service);

    // Set the properties of the node that a driver will bind to.
    fdf::NodeProperty property =
        driver::MakeProperty(1 /* BIND_PROTOCOL */, bind_fuchsia_test::BIND_PROTOCOL_DEVICE);

    auto args = fdf::NodeAddArgs{{
        .name = std::string(kChildName),
        .offers = std::vector{std::move(offer)},
        .properties = std::vector{std::move(property)},
    }};

    // Create endpoints of the `NodeController` for the node.
    auto endpoints = fidl::CreateEndpoints<fdf::NodeController>();
    if (endpoints.is_error()) {
      return zx::error(ZX_ERR_INTERNAL);
    }

    node_->AddChild({std::move(args), std::move(endpoints->server), {}})
        .Then([&, client = std::move(endpoints->client)](
                  fidl::Result<fdf::Node::AddChild>& add_result) mutable {
          if (add_result.is_error()) {
            FDF_LOG(ERROR, "Failed to AddChild: %s",
                    add_result.error_value().FormatDescription().c_str());
            node_.AsyncTeardown();
            return;
          }

          controller_.Bind(std::move(client), async_dispatcher());
        });

    return zx::ok();
  }

  // fidl::Server<ft::Handshake>
  void Do(DoRequest& request, DoCompleter::Sync& completer) override { completer.Reply(); }

  fidl::SharedClient<fdf::Node> node_;
  fidl::SharedClient<fdf::NodeController> controller_;
};

}  // namespace

FUCHSIA_DRIVER_RECORD_CPP_V2(driver::Record<RootDriver>);
