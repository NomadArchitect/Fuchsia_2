// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/symbols/visit_scopes.h"

#include "src/developer/debug/zxdb/symbols/code_block.h"
#include "src/developer/debug/zxdb/symbols/collection.h"
#include "src/developer/debug/zxdb/symbols/inheritance_path.h"
#include "src/developer/debug/zxdb/symbols/inherited_from.h"

namespace zxdb {

namespace {

VisitResult DoVisitClassHierarchy(InheritancePath* path,
                                  fit::function<VisitResult(const InheritancePath& path)>& cb) {
  if (VisitResult result = cb(*path); result != VisitResult::kContinue)
    return result;

  // Iterate through base classes.
  for (const auto& lazy_from : path->path().back().collection->inherited_from()) {
    const InheritedFrom* inherited_from = lazy_from.Get()->As<InheritedFrom>();
    if (!inherited_from)
      continue;

    const Collection* from_coll = inherited_from->from().Get()->As<Collection>();
    if (!from_coll)
      continue;

    path->path().emplace_back(RefPtrTo(inherited_from), RefPtrTo(from_coll));
    if (VisitResult result = DoVisitClassHierarchy(path, cb); result != VisitResult::kContinue)
      return result;
    path->path().pop_back();
  }

  return VisitResult::kContinue;
}

}  // namespace

VisitResult VisitLocalBlocks(const CodeBlock* starting,
                             fit::function<VisitResult(const CodeBlock*)> cb) {
  // Need to hold references when walking up the symbol hierarchy.
  fxl::RefPtr<CodeBlock> cur_block = RefPtrTo(starting);
  while (cur_block) {
    VisitResult result = cb(cur_block.get());
    if (result != VisitResult::kContinue)
      return result;

    if (cur_block->As<Function>() || !cur_block->parent())
      break;  // Don't iterate above functions.
    auto parent_ref = cur_block->parent().Get();
    cur_block = RefPtrTo(parent_ref->As<CodeBlock>());
  }
  return VisitResult::kContinue;
}

VisitResult VisitClassHierarchy(const Collection* starting,
                                fit::function<VisitResult(const InheritancePath& path)> cb) {
  InheritancePath path(RefPtrTo(starting));
  return DoVisitClassHierarchy(&path, cb);
}

}  // namespace zxdb
