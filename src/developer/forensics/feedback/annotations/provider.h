// Copyright 2022 The Fuchsia Authors.All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_PROVIDER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_PROVIDER_H_

#include <lib/fit/function.h>
#include <lib/fpromise/promise.h>

#include <set>

#include "src/developer/forensics/feedback/annotations/types.h"

namespace forensics::feedback {

// Collects safe-to-cache annotations asynchronously.
class StaticAsyncAnnotationProvider {
 public:
  // Returns the annotation keys a provider will collect.
  virtual std::set<std::string> GetKeys() const = 0;

  // Returns the annotations this provider collects via |callback|.
  //
  // Note: this method will be called once.
  virtual void GetOnce(::fit::callback<void(Annotations)> callback) = 0;
};

// Collects unsafe-to-cache annotations synchronously.
//
// Note: synchronous calls must be low-cost and return quickly, e.g. not IPC.
class DynamicSyncAnnotationProvider {
 public:
  // Returns the Annotations from this provider.
  virtual Annotations Get() = 0;
};

// Collects annotations not set by the platform.
class NonPlatformAnnotationProvider : public DynamicSyncAnnotationProvider {
 public:
  // Returns true if non-platform annotations are missing.
  virtual bool IsMissingAnnotations() const = 0;
};

// Collects unsafe-to-cache annotations asynchronously.
class DynamicAsyncAnnotationProvider {
 public:
  // Returns the annotation keys a provider will collect.
  virtual std::set<std::string> GetKeys() const = 0;

  // Returns the annotations this provider collects via |callback|.
  virtual void Get(::fit::callback<void(Annotations)> callback) = 0;
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_ANNOTATIONS_PROVIDER_H_
