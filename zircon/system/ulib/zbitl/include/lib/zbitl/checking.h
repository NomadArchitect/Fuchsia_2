// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_ZBITL_CHECKING_H_
#define LIB_ZBITL_CHECKING_H_

#include <lib/fitx/result.h>
#include <zircon/assert.h>
#include <zircon/boot/image.h>

#include <string_view>

namespace zbitl {

// Modify a header so that it passes checks.  This can be used to mint new
// items from a designated initializer that omits uninteresting bits.
inline constexpr zbi_header_t SanitizeHeader(zbi_header_t header) {
  header.magic = ZBI_ITEM_MAGIC;
  header.flags |= ZBI_FLAG_VERSION;
  if (!(header.flags & ZBI_FLAG_CRC32)) {
    header.crc32 = ZBI_ITEM_NO_CRC32;
  }
  return header;
}

/// Returns empty if and only if the ZBI is complete (bootable), otherwise an
/// error string.  This takes any zbitl::View type or any type that acts like
/// it.  Note this does not check for errors from zbi.take_error() so if Zbi is
/// zbitl::View then the caller must use zbi.take_error() afterwards.  This
/// function always scans every item so all errors Zbi::iterator detects will
/// be found.  But this function's return value only indicates if the items
/// that were scanned before any errors were encountered added up to a complete
/// ZBI (regardless of whether there were additional items with errors).
template <typename Zbi>
fitx::result<std::string_view> CheckComplete(Zbi&& zbi,
                                             uint32_t kernel_type
#ifdef __aarch64__
                                             = ZBI_TYPE_KERNEL_ARM64
#elif defined(__x86_64__)
                                             = ZBI_TYPE_KERNEL_X64

#endif
                                             ,
                                             uint32_t bootfs_type = ZBI_TYPE_STORAGE_BOOTFS) {
  enum {
    kKernelAbsent,
    kKernelFirst,
    kKernelLater,
  } kernel = kKernelAbsent;
  bool bootfs = false;
  bool empty = true;
  for (auto [header, payload] : zbi) {
    if (header->type == kernel_type) {
      kernel = (empty && kernel == kKernelAbsent) ? kKernelFirst : kKernelLater;
    } else if (header->type == bootfs_type) {
      bootfs = true;
    }
    empty = false;
  }

  if (empty) {
    return fitx::error("empty ZBI");
  }
  switch (kernel) {
    case kKernelAbsent:
      return fitx::error("no kernel item found");
    case kKernelLater:
      return fitx::error("kernel item out of order: must be first");
    case kKernelFirst:
      if (bootfs) {  // It's complete.
        return fitx::ok();
      }
      return fitx::error("missing BOOTFS");
  }
  ZX_ASSERT_MSG(false, "unreachable");
}

}  // namespace zbitl

#endif  // LIB_ZBITL_CHECKING_H_
