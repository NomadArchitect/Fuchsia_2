// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/cache.h>
#include <lib/arch/x86/boot-cpuid.h>
#include <lib/arch/x86/bug.h>
#include <lib/boot-options/boot-options.h>
#include <lib/code-patching/code-patches.h>
#include <zircon/assert.h>

#include <cstdint>
#include <cstdio>

#include <arch/code-patches/case-id.h>

namespace {

// TODO(68585): While .code-patches is allocated and accessed from directly
// within the kernel, we expect its recorded addresses to be the final,
// link-time ones.
ktl::span<ktl::byte> GetInstructions(uint64_t range_start, size_t range_size) {
  return {reinterpret_cast<ktl::byte*>(range_start), range_size};
}

void PrintCaseInfo(const code_patching::Directive& patch, const char* fmt, ...) {
  printf("code-patching: ");
  va_list args;
  va_start(args, fmt);
  vprintf(fmt, args);
  va_end(args);
  printf(": [%#lx, %#lx)\n", patch.range_start, patch.range_start + patch.range_size);
}

}  // namespace

// Declared in <lib/code-patching/code-patches.h>.
void ArchPatchCode(ktl::span<const code_patching::Directive> patches) {
  arch::BootCpuidIo cpuid;

  // Will effect instruction-data cache consistency on destruction.
  arch::CacheConsistencyContext sync_ctx;

  for (const code_patching::Directive& patch : patches) {
    ktl::span<ktl::byte> insns = GetInstructions(patch.range_start, patch.range_size);
    if (insns.empty()) {
      ZX_PANIC("code-patching: unrecognized address range for patch case ID %u: [%#lx, %#lx)",
               patch.id, patch.range_start, patch.range_start + patch.range_size);
    }

    switch (patch.id) {
      case CASE_ID_SWAPGS_MITIGATION: {
        // `nop` out the mitigation if the bug is not present, if we could not
        // mitigate it even if it was, or if we generally want mitigations off.
        const bool present = arch::HasX86SwapgsBug(cpuid);
        if (!present || gBootOptions->x86_disable_spec_mitigations) {
          code_patching::NopFill(insns);
          ktl::string_view qualifier = !present ? "bug not present" : "all mitigations disabled";
          PrintCaseInfo(patch, "swapgs bug mitigation disabled (%V)", qualifier);
          break;
        }
        PrintCaseInfo(patch, "swapgs bug mitigation enabled");
        continue;  // No patching, so skip past sync'ing.
      }
      default:
        ZX_PANIC("code-patching: unrecognized patch case ID: %u: [%#lx, %#lx)\n", patch.id,
                 patch.range_start, patch.range_start + patch.range_size);
    }
    sync_ctx.SyncRange(patch.range_start, patch.range_size);
  }
}
