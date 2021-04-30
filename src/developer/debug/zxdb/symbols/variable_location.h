// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_VARIABLE_LOCATION_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_VARIABLE_LOCATION_H_

#include <inttypes.h>

#include <vector>

#include "src/developer/debug/zxdb/symbols/dwarf_expr.h"

namespace zxdb {

class SymbolContext;

// Describes the location of a value. A value can be in different locations depending on what the
// value of the IP is at which is represented as a series of ranges. The location for the value
// within those ranges is described as an opaque array of bytes (this is the DWARF expression which
// will evaluate to the value).
//
// In DWARF, simple variables that are always valid look like this:
//   DW_AT_location (DW_OP_reg5 RDI)
//
// Complicated ones with ranges look like this:
//   DW_AT_location:
//     [0x00000000000ad6be,  0x00000000000ad6c8): DW_OP_reg2 RCX
//     [0x00000000000ad6c8,  0x00000000000ad780): DW_OP_reg14 R14
class VariableLocation {
 public:
  struct Entry {
    // These addresses are relative to the module that generated the symbol. A symbol context is
    // required to compare to physical addresses.
    //
    // These will be 0,0 for a range that's always valid.
    uint64_t begin = 0;  // First address.
    uint64_t end = 0;    // First address past end.

    // Returns whether this entry matches the given physical IP.
    bool InRange(const SymbolContext& symbol_context, uint64_t ip) const;

    // The DWARF expression that evaluates to the result. Evaluate with the DwarfExprEval object.
    DwarfExpr expression;
  };

  VariableLocation();

  // Constructs a Location with a single location valid for all address ranges, with the program
  // contained in the given buffer.
  VariableLocation(const uint8_t* data, size_t size, const UncachedLazySymbol& source);

  // Constructs with an extracted array of Entries.
  VariableLocation(std::vector<Entry> locations);

  ~VariableLocation();

  // Returns whether this location lacks any actual locations.
  bool is_null() const { return locations_.empty(); }

  const std::vector<Entry>& locations() const { return locations_; }

  // Returns the Entry that corresponds to the given IP, or nullptr if none matched.
  const Entry* EntryForIP(const SymbolContext& symbol_context, uint64_t ip) const;

 private:
  // The location list. The DWARF spec explicitly allows for ranges to overlap which means the value
  // can be retrieved from either location.
  std::vector<Entry> locations_;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_VARIABLE_LOCATION_H_
