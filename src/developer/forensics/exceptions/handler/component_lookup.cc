// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/exceptions/handler/component_lookup.h"

#include <lib/syslog/cpp/macros.h>

#include "src/developer/forensics/utils/errors.h"
#include "src/developer/forensics/utils/fidl/oneshot_ptr.h"
#include "src/developer/forensics/utils/fit/promise.h"
#include "src/lib/fxl/macros.h"

namespace forensics {
namespace exceptions {
namespace handler {
namespace {

using fuchsia::sys::internal::CrashIntrospect_FindComponentByThreadKoid_Result;
using fuchsia::sys::internal::SourceIdentity;

// Wraps around fuchsia::sys::internal::CrashIntrospectPtr to handle establishing the connection,
// losing the connection, waiting for the callback, enforcing a timeout, etc.
//
// GetSourceIdentity() is expected to be called only once.
class ComponentLookup {
 public:
  // fuchsia.sys.internal.CrashIntrospect is expected to be in |services|.
  ComponentLookup(async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services)
      : introspect_(dispatcher, services) {}

  ::fpromise::promise<SourceIdentity> GetSourceIdentity(zx_koid_t process_koid,
                                                        fit::Timeout timeout);

 private:
  fidl::OneShotPtr<fuchsia::sys::internal::CrashIntrospect, SourceIdentity> introspect_;
};

::fpromise::promise<SourceIdentity> ComponentLookup::GetSourceIdentity(zx_koid_t thread_koid,
                                                                       fit::Timeout timeout) {
  introspect_->FindComponentByThreadKoid(
      thread_koid, [this](CrashIntrospect_FindComponentByThreadKoid_Result result) {
        if (introspect_.IsAlreadyDone()) {
          return;
        }

        if (result.is_response()) {
          introspect_.CompleteOk(std::move(result.response().component_info));
        } else {
          // ZX_ERR_NOT_FOUND most likely means a thread from a process outside a component,
          // which is not an error.
          if (result.err() != ZX_ERR_NOT_FOUND) {
            FX_PLOGS(WARNING, result.err()) << "Failed FindComponentByProcessKoid";
          }

          introspect_.CompleteError(Error::kDefault);
        }
      });

  return introspect_.WaitForDone(std::move(timeout)).or_else([](const Error& error) {
    return ::fpromise::error();
  });
}

}  // namespace

::fpromise::promise<SourceIdentity> GetComponentSourceIdentity(
    async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
    fit::Timeout timeout, zx_koid_t thread_koid) {
  auto component_lookup = std::make_unique<ComponentLookup>(dispatcher, services);

  // We must store the promise in a variable due to the fact that the order of evaluation of
  // function parameters is undefined.
  auto component = component_lookup->GetSourceIdentity(thread_koid, std::move(timeout));
  return fit::ExtendArgsLifetimeBeyondPromise(/*promise=*/std::move(component),
                                              /*args=*/std::move(component_lookup));
}

}  // namespace handler
}  // namespace exceptions
}  // namespace forensics
