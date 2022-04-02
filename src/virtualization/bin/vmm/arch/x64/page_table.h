// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_BIN_VMM_ARCH_X64_PAGE_TABLE_H_
#define SRC_VIRTUALIZATION_BIN_VMM_ARCH_X64_PAGE_TABLE_H_

#include <lib/zx/status.h>
#include <zircon/syscalls/port.h>

#include "src/virtualization/bin/vmm/device/phys_mem.h"

// Create an identity-mapped page table.
//
// @param addr The mapped address of guest physical memory.
// @param size The size of guest physical memory.
// @param end_off The offset to the end of the page table.
zx_status_t create_page_table(const PhysMem& phys_mem);

// NOTE: x86 instructions are guaranteed to be 15 bytes or fewer.
constexpr uint8_t kMaxInstructionSize = 15;
using InstructionBuffer = std::array<uint8_t, kMaxInstructionSize>;
using InstructionSpan = cpp20::span<uint8_t>;

// Read an instruction from a virtual address.
//
// @param cr3_addr The address of the page table in the guest physical address
//                 space.
// @param rip_addr The address of the instruction in the guest virtual address
//                 space.
// @param span     The location to read the instruction into.
zx::status<> ReadInstruction(const PhysMem& phys_mem, zx_gpaddr_t cr3_addr, zx_vaddr_t rip_addr,
                             InstructionSpan span);

#endif  // SRC_VIRTUALIZATION_BIN_VMM_ARCH_X64_PAGE_TABLE_H_
