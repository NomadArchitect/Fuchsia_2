// Copyright 2022 The Fuchsia Authors.All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/annotations/annotation_manager.h"

#include <lib/async/cpp/task.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/syslog/cpp/macros.h>

#include <map>
#include <memory>

namespace forensics::feedback {
namespace {

void InsertUnique(const Annotations& annotations, const std::set<std::string>& allowlist,
                  Annotations* out) {
  for (const auto& [k, v] : annotations) {
    if (allowlist.count(k) != 0) {
      FX_CHECK(out->count(k) == 0) << "Attempting to re-insert " << k;
      out->insert({k, v});
    }
  }
}

void InsertUnique(const Annotations& annotations, Annotations* out) {
  for (const auto& [k, v] : annotations) {
    FX_CHECK(out->count(k) == 0) << "Attempting to re-insert " << k;
    out->insert({k, v});
  }
}

// Inserts all keys in |keys| with a value of |error| into |out|, if they don't already have a
// value.
void InsertMissing(const std::set<std::string>& keys, const Error error,
                   const std::set<std::string>& allowlist, Annotations* out) {
  for (const auto& key : keys) {
    if (allowlist.count(key) == 0 || out->count(key) != 0) {
      continue;
    }

    out->insert({key, error});
  }
}

template <typename Container, typename T>
void Remove(Container& c, T v) {
  c.erase(std::remove(c.begin(), c.end(), v), c.end());
}

// Creates a callable object that can be used to complete an asynchronous flow and an object to
// consume its results.
auto CompleteAndConsume() {
  ::fpromise::bridge<> bridge;

  auto completer = std::make_shared<::fpromise::completer<>>(std::move(bridge.completer));
  auto complete = [completer] {
    if ((*completer)) {
      completer->complete_ok();
    }
  };

  return std::make_pair(std::move(complete), std::move(bridge.consumer));
}

}  // namespace

AnnotationManager::AnnotationManager(
    async_dispatcher_t* dispatcher, std::set<std::string> allowlist,
    const Annotations static_annotations, NonPlatformAnnotationProvider* non_platform_provider,
    std::vector<DynamicSyncAnnotationProvider*> dynamic_sync_providers,
    std::vector<StaticAsyncAnnotationProvider*> static_async_providers,
    std::vector<DynamicAsyncAnnotationProvider*> dynamic_async_providers)
    : dispatcher_(dispatcher),
      allowlist_(std::move(allowlist)),
      static_annotations_(),
      non_platform_provider_(non_platform_provider),
      dynamic_sync_providers_(std::move(dynamic_sync_providers)),
      static_async_providers_(std::move(static_async_providers)),
      dynamic_async_providers_(std::move(dynamic_async_providers)) {
  InsertUnique(static_annotations, allowlist_, &static_annotations_);

  // Create a weak pointer because |this| isn't guaranteed to outlive providers.
  auto self = ptr_factory_.GetWeakPtr();
  for (auto* provider : static_async_providers_) {
    provider->GetOnce([self, provider](const Annotations annotations) {
      if (!self) {
        return;
      }

      InsertUnique(annotations, self->allowlist_, &(self->static_annotations_));

      // Remove the reference to |provider| once it has returned its annotations.
      Remove(self->static_async_providers_, provider);
      if (!self->static_async_providers_.empty()) {
        return;
      }

      // No static async providers remain so complete all calls to WaitForStaticAsync.
      for (auto& waiting : self->waiting_for_static_) {
        waiting();
        waiting = nullptr;
      }

      Remove(self->waiting_for_static_, nullptr);
    });
  }
}

void AnnotationManager::InsertStatic(const Annotations& annotations) {
  InsertUnique(annotations, allowlist_, &static_annotations_);
}

::fpromise::promise<Annotations> AnnotationManager::GetAll(const zx::duration timeout) {
  // Create a weak pointer because |this| isn't guaranteed to outlive providers.
  auto self = ptr_factory_.GetWeakPtr();

  return ::fpromise::join_promises(WaitForStaticAsync(timeout), WaitForDynamicAsync(timeout))
      .and_then([self](std::tuple<::fpromise::result<>, ::fpromise::result<Annotations>>& results) {
        Annotations annotations = self->ImmediatelyAvailable();

        // Add the dynamic async annotations.
        InsertUnique(std::get<1>(results).value(), &annotations);

        // Any async annotations not collected timed out.
        for (const auto& p : self->static_async_providers_) {
          InsertMissing(p->GetKeys(), Error::kTimeout, self->allowlist_, &annotations);
        }

        for (const auto& p : self->dynamic_async_providers_) {
          InsertMissing(p->GetKeys(), Error::kTimeout, self->allowlist_, &annotations);
        }

        return ::fpromise::ok(std::move(annotations));
      });
}

Annotations AnnotationManager::ImmediatelyAvailable() const {
  Annotations annotations(static_annotations_);
  for (auto* provider : dynamic_sync_providers_) {
    InsertUnique(provider->Get(), allowlist_, &annotations);
  }

  if (non_platform_provider_ != nullptr) {
    InsertUnique(non_platform_provider_->Get(), &annotations);
  }

  return annotations;
}

bool AnnotationManager::IsMissingNonPlatformAnnotations() const {
  return (non_platform_provider_ == nullptr) ? false
                                             : non_platform_provider_->IsMissingAnnotations();
}

::fpromise::promise<> AnnotationManager::WaitForStaticAsync(const zx::duration timeout) {
  // All static async annotations have been collected.
  if (static_async_providers_.empty()) {
    return ::fpromise::make_ok_promise();
  }

  auto [complete, consume] = CompleteAndConsume();

  async::PostDelayedTask(dispatcher_, complete, timeout);
  waiting_for_static_.push_back(complete);

  return consume.promise_or(::fpromise::error()).or_else([] {
    FX_LOGS(FATAL) << "Promise for waiting on static annotations was incorrectly dropped";
    return ::fpromise::error();
  });
}

namespace {

// Stores state needed to join the result of dynamic async annotation flows.
struct AsyncAnnotations {
  Annotations annotations;
  std::vector<DynamicAsyncAnnotationProvider*> providers;
  ::std::function<void()> complete;
};

}  // namespace

::fpromise::promise<Annotations> AnnotationManager::WaitForDynamicAsync(
    const zx::duration timeout) {
  // No need to collect dynamic async annotations.
  if (dynamic_async_providers_.empty()) {
    return ::fpromise::make_ok_promise(Annotations{});
  }

  auto [complete, consume] = CompleteAndConsume();

  auto async_state = std::make_shared<AsyncAnnotations>(AsyncAnnotations{
      .annotations = {},
      .providers = dynamic_async_providers_,
      .complete = complete,
  });

  // Create a weak pointer because |this| isn't guaranteed to outlive providers.
  auto self = ptr_factory_.GetWeakPtr();
  for (auto* provider : async_state->providers) {
    provider->Get([self, provider, async_state](const Annotations annotations) {
      if (!self) {
        return;
      }

      InsertUnique(annotations, self->allowlist_, &(async_state->annotations));

      // Remove the reference to |provider| once it has returned its annotations.
      Remove(async_state->providers, provider);
      if (!async_state->providers.empty()) {
        return;
      }

      // No dynamic async providers remain so complete the call to WaitForDynamicAsync.
      async_state->complete();
    });
  }

  async::PostDelayedTask(dispatcher_, async_state->complete, timeout);

  return consume.promise_or(::fpromise::error())
      .and_then([async_state] { return ::fpromise::ok(async_state->annotations); })
      .or_else([] {
        FX_LOGS(FATAL) << "Promise for waiting on dynamic annotations was incorrectly dropped";
        return ::fpromise::error();
      });
}

}  // namespace forensics::feedback
