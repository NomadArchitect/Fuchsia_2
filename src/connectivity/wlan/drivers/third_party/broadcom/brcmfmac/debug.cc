/*
 * Copyright (c) 2012 Broadcom Corporation
 *
 * Permission to use, copy, modify, and/or distribute this software for any purpose with or without
 * fee is hereby granted, provided that the above copyright notice and this permission notice appear
 * in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
 * SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
 * AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT,
 * NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
 * OF THIS SOFTWARE.
 */

#include "debug.h"

#include <algorithm>
#include <cctype>

namespace wlan {
namespace brcmfmac {

// static
void Debug::PrintStringDump(uint32_t flag, const void* data, size_t length) {
  constexpr size_t kValuesPerLine = 64;

  if (zxlog_level_enabled_etc(flag)) {
    zxlogf_etc(flag, nullptr, "%p:", data);

    const char* const bytes = reinterpret_cast<const char*>(data);
    const size_t max_dump_bytes = std::min<size_t>(length, kMaxStringDumpBytes);
    for (size_t i = 0; i < max_dump_bytes; i += kValuesPerLine) {
      char buffer[kValuesPerLine + 1];
      size_t line_width = std::min(kValuesPerLine, max_dump_bytes - i);
      std::transform(bytes + i, bytes + i + line_width, buffer,
                     [](unsigned char c) { return std::isprint(c) ? c : '.'; });
      buffer[line_width] = '\0';
      zxlogf_etc(flag, nullptr, "%04zx: %s", i, buffer);
    }
    if (length > kMaxStringDumpBytes) {
      zxlogf_etc(flag, nullptr, "%04zx: ...", kMaxStringDumpBytes);
    }
  }
}

// static
void Debug::CreateMemoryDump(const void* data, size_t length) {
  // No-op for now.
}

}  // namespace brcmfmac
}  // namespace wlan
