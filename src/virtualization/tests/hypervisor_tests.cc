// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <fuchsia/kernel/cpp/fidl.h>
#include <fuchsia/sysinfo/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>
#include <lib/zx/guest.h>
#include <lib/zx/port.h>
#include <lib/zx/resource.h>
#include <lib/zx/status.h>
#include <lib/zx/vcpu.h>
#include <lib/zx/vmar.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/process.h>
#include <zircon/status.h>
#include <zircon/syscalls/hypervisor.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <string>
#include <thread>

#include <fbl/auto_lock.h>
#include <fbl/mutex.h>
#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "hypervisor_tests_constants.h"
#include "src/lib/fxl/test/test_settings.h"

namespace {

constexpr uint32_t kGuestMapFlags =
    ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_PERM_EXECUTE | ZX_VM_SPECIFIC;
constexpr uint32_t kHostMapFlags = ZX_VM_PERM_READ | ZX_VM_PERM_WRITE;
// Inject an interrupt with vector 32, the first user defined interrupt vector.
constexpr uint32_t kInterruptVector = 32u;
constexpr uint64_t kTrapKey = 0x1234;

#ifdef __x86_64__
constexpr uint32_t kNmiVector = 2u;
constexpr uint32_t kGpFaultVector = 13u;
constexpr uint32_t kExceptionVector = 16u;
#endif

#define DECLARE_TEST_FUNCTION(name)     \
  extern "C" const char name##_start[]; \
  extern "C" const char name##_end[];

DECLARE_TEST_FUNCTION(vcpu_resume)
DECLARE_TEST_FUNCTION(vcpu_read_write_state)
DECLARE_TEST_FUNCTION(vcpu_interrupt)
DECLARE_TEST_FUNCTION(guest_set_trap)
#ifdef __aarch64__
DECLARE_TEST_FUNCTION(vcpu_wfi)
DECLARE_TEST_FUNCTION(vcpu_wfi_pending_interrupt_gicv2)
DECLARE_TEST_FUNCTION(vcpu_wfi_pending_interrupt_gicv3)
DECLARE_TEST_FUNCTION(vcpu_wfi_aarch32)
DECLARE_TEST_FUNCTION(vcpu_fp)
DECLARE_TEST_FUNCTION(vcpu_fp_aarch32)
DECLARE_TEST_FUNCTION(vcpu_psci_system_off)
#elif __x86_64__
DECLARE_TEST_FUNCTION(vcpu_hlt)
DECLARE_TEST_FUNCTION(vcpu_pause)
DECLARE_TEST_FUNCTION(vcpu_write_cr0)
DECLARE_TEST_FUNCTION(vcpu_write_invalid_cr0)
DECLARE_TEST_FUNCTION(vcpu_compat_mode)
DECLARE_TEST_FUNCTION(vcpu_syscall)
DECLARE_TEST_FUNCTION(vcpu_sysenter)
DECLARE_TEST_FUNCTION(vcpu_sysenter_compat)
DECLARE_TEST_FUNCTION(vcpu_vmcall_invalid_number)
DECLARE_TEST_FUNCTION(vcpu_vmcall_invalid_cpl)
DECLARE_TEST_FUNCTION(vcpu_extended_registers)
DECLARE_TEST_FUNCTION(guest_set_trap_with_io)
#endif
#undef DECLARE_TEST_FUNCTION

enum {
  X86_PTE_P = 0x01,   // P    Valid
  X86_PTE_RW = 0x02,  // R/W  Read/Write
  X86_PTE_U = 0x04,   // U    Page is user accessible
  X86_PTE_PS = 0x80,  // PS   Page size
};

struct TestCase {
  bool interrupts_enabled = false;
  uintptr_t host_addr = 0;

  zx::vmo vmo;
  zx::guest guest;
  zx::vmar vmar;
  zx::vcpu vcpu;

  ~TestCase() {
    if (host_addr != 0) {
      zx::vmar::root_self()->unmap(host_addr, VMO_SIZE);
    }
  }
};

zx_status_t GetVmexResource(zx::resource* resource) {
  fuchsia::kernel::VmexResourceSyncPtr vmex_resource;
  auto path = std::string("/svc/") + fuchsia::kernel::VmexResource::Name_;
  zx_status_t status =
      fdio_service_connect(path.data(), vmex_resource.NewRequest().TakeChannel().release());
  if (status != ZX_OK) {
    return status;
  }
  return vmex_resource->Get(resource);
}

zx_status_t GetHypervisorResource(zx::resource* resource) {
  fuchsia::kernel::HypervisorResourceSyncPtr hypervisor_rsrc;
  auto path = std::string("/svc/") + fuchsia::kernel::HypervisorResource::Name_;
  zx_status_t status =
      fdio_service_connect(path.data(), hypervisor_rsrc.NewRequest().TakeChannel().release());
  if (status != ZX_OK) {
    return status;
  }

  return hypervisor_rsrc->Get(resource);
}

#ifdef __aarch64__

zx_status_t GetSysinfo(fuchsia::sysinfo::SysInfoSyncPtr* sysinfo) {
  auto path = std::string("/svc/") + fuchsia::sysinfo::SysInfo::Name_;
  return fdio_service_connect(path.data(), sysinfo->NewRequest().TakeChannel().release());
}

zx_status_t GetInterruptControllerInfo(fuchsia::sysinfo::InterruptControllerInfoPtr* info) {
  fuchsia::sysinfo::SysInfoSyncPtr sysinfo;
  zx_status_t status = GetSysinfo(&sysinfo);
  if (status != ZX_OK) {
    return status;
  }

  zx_status_t fidl_status;
  status = sysinfo->GetInterruptControllerInfo(&fidl_status, info);
  return status != ZX_OK ? status : fidl_status;
}

#endif  // __aarch64__

// Return true if the platform we are running on supports running guests.
bool PlatformSupportsGuests() {
  // Get hypervisor permissions.
  zx::resource hypervisor_resource;
  zx_status_t status = GetHypervisorResource(&hypervisor_resource);
  FX_CHECK(status == ZX_OK) << "Could not get hypervisor resource.";

  // Try create a guest.
  zx::guest guest;
  zx::vmar vmar;
  status = zx::guest::create(hypervisor_resource, 0, &guest, &vmar);
  if (status != ZX_OK) {
    FX_CHECK(status == ZX_ERR_NOT_SUPPORTED)
        << "Unexpected error attempting to create Zircon guest object: "
        << zx_status_get_string(status);
    return false;
  }

  // Create a single VCPU.
  zx::vcpu vcpu;
  status = zx::vcpu::create(guest, /*options=*/0, /*entry=*/0, &vcpu);
  if (status != ZX_OK) {
    FX_CHECK(status == ZX_ERR_NOT_SUPPORTED)
        << "Unexpected error attempting to create VCPU: " << zx_status_get_string(status);
    return false;
  }

  return true;
}

// Setup a guest in fixture |test|.
//
// |start| and |end| point to the start and end of the code that will be copied into the guest for
// execution. If |start| and |end| are null, no code is copied.
void SetupGuest(TestCase* test, const char* start, const char* end) {
  ASSERT_EQ(zx::vmo::create(VMO_SIZE, 0, &test->vmo), ZX_OK);
  ASSERT_EQ(zx::vmar::root_self()->map(kHostMapFlags, 0, test->vmo, 0, VMO_SIZE, &test->host_addr),
            ZX_OK);

  // Add ZX_RIGHT_EXECUTABLE so we can map into guest address space.
  zx::resource vmex_resource;
  ASSERT_EQ(GetVmexResource(&vmex_resource), ZX_OK);
  ASSERT_EQ(test->vmo.replace_as_executable(vmex_resource, &test->vmo), ZX_OK);

  zx::resource hypervisor_resource;
  ASSERT_EQ(GetHypervisorResource(&hypervisor_resource), ZX_OK);
  zx_status_t status = zx::guest::create(hypervisor_resource, 0, &test->guest, &test->vmar);
  ASSERT_EQ(status, ZX_OK);

  zx_gpaddr_t guest_addr;
  ASSERT_EQ(test->vmar.map(kGuestMapFlags, 0, test->vmo, 0, VMO_SIZE, &guest_addr), ZX_OK);
  ASSERT_EQ(test->guest.set_trap(ZX_GUEST_TRAP_MEM, EXIT_TEST_ADDR, PAGE_SIZE, zx::port(), 0),
            ZX_OK);

  // Setup the guest.
  uintptr_t entry = 0;
#if __x86_64__
  // PML4 entry pointing to (addr + 0x1000)
  uint64_t* pte_off = reinterpret_cast<uint64_t*>(test->host_addr);
  *pte_off = PAGE_SIZE | X86_PTE_P | X86_PTE_U | X86_PTE_RW;
  // PDP entry with 1GB page.
  pte_off = reinterpret_cast<uint64_t*>(test->host_addr + PAGE_SIZE);
  *pte_off = X86_PTE_PS | X86_PTE_P | X86_PTE_U | X86_PTE_RW;
  entry = GUEST_ENTRY;
#endif  // __x86_64__

  if (start != nullptr && end != nullptr) {
    memcpy(reinterpret_cast<void*>(test->host_addr + entry), start, end - start);
  }

  status = zx::vcpu::create(test->guest, 0, entry, &test->vcpu);
  ASSERT_EQ(status, ZX_OK);
}

#if __x86_64__
void SetupAndInterrupt(TestCase* test, const char* start, const char* end) {
  ASSERT_NO_FATAL_FAILURE(SetupGuest(test, start, end));
  test->interrupts_enabled = true;

  thrd_t thread;
  int ret = thrd_create(
      &thread,
      [](void* ctx) -> int {
        TestCase* test = static_cast<TestCase*>(ctx);
        return test->vcpu.interrupt(kInterruptVector) == ZX_OK ? thrd_success : thrd_error;
      },
      test);
  ASSERT_EQ(ret, thrd_success);
}
#endif

bool ExceptionThrown(const zx_packet_guest_mem_t& guest_mem, const zx::vcpu& vcpu) {
#if __x86_64__
  if (guest_mem.inst_len != 12) {
    // Not the expected mov imm, (EXIT_TEST_ADDR) size.
    return true;
  }
  if (guest_mem.inst_buf[8] == 0 && guest_mem.inst_buf[9] == 0 && guest_mem.inst_buf[10] == 0 &&
      guest_mem.inst_buf[11] == 0) {
    return false;
  }
  zx_vcpu_state_t vcpu_state;
  if (vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)) != ZX_OK) {
    return true;
  }
  // Print out debug values from the exception handler.
  fprintf(stderr, "Unexpected exception in guest\n");
  fprintf(stderr, "vector = %lu\n", vcpu_state.rax);
  fprintf(stderr, "error code = %lu\n", vcpu_state.rbx);
  fprintf(stderr, "rip = 0x%lx\n", vcpu_state.rcx);
  return true;
#else
  return false;
#endif
}

void ResumeAndCleanExit(TestCase* test) {
  zx_port_packet_t packet = {};
  ASSERT_EQ(test->vcpu.resume(&packet), ZX_OK);
  EXPECT_EQ(packet.type, ZX_PKT_TYPE_GUEST_MEM);
  EXPECT_EQ(packet.guest_mem.addr, static_cast<zx_gpaddr_t>(EXIT_TEST_ADDR));
#if __x86_64__
  EXPECT_EQ(packet.guest_mem.default_operand_size, 4u);
#endif
  if (test->interrupts_enabled) {
    ASSERT_FALSE(ExceptionThrown(packet.guest_mem, test->vcpu));
  }
}

TEST(Guest, VcpuResume) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_resume_start, vcpu_resume_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuInvalidThreadReuse) {
  {
    TestCase test;
    ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_resume_start, vcpu_resume_end));

    zx::vcpu vcpu;
    zx_status_t status = zx::vcpu::create(test.guest, 0, 0, &vcpu);
    ASSERT_EQ(status, ZX_ERR_BAD_STATE);
  }

  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_resume_start, vcpu_resume_end));
}

TEST(Guest, VcpuReadWriteState) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(
      SetupGuest(&test, vcpu_read_write_state_start, vcpu_read_write_state_end));

  zx_vcpu_state_t vcpu_state = {
#if __aarch64__
    // clang-format off
        .x = {
             0u,  1u,  2u,  3u,  4u,  5u,  6u,  7u,  8u,  9u,
            10u, 11u, 12u, 13u, 14u, 15u, 16u, 17u, 18u, 19u,
            20u, 21u, 22u, 23u, 24u, 25u, 26u, 27u, 28u, 29u,
            30u,
        },
    // clang-format on
    .sp = 64u,
    .cpsr = 0,
    .padding1 = {},
#elif __x86_64__
    .rax = 1u,
    .rcx = 2u,
    .rdx = 3u,
    .rbx = 4u,
    .rsp = 5u,
    .rbp = 6u,
    .rsi = 7u,
    .rdi = 8u,
    .r8 = 9u,
    .r9 = 10u,
    .r10 = 11u,
    .r11 = 12u,
    .r12 = 13u,
    .r13 = 14u,
    .r14 = 15u,
    .r15 = 16u,
    .rflags = 0,
#endif
  };

  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);

#if __aarch64__
  EXPECT_EQ(vcpu_state.x[0], static_cast<uint64_t>(EXIT_TEST_ADDR));
  EXPECT_EQ(vcpu_state.x[1], 2u);
  EXPECT_EQ(vcpu_state.x[2], 4u);
  EXPECT_EQ(vcpu_state.x[3], 6u);
  EXPECT_EQ(vcpu_state.x[4], 8u);
  EXPECT_EQ(vcpu_state.x[5], 10u);
  EXPECT_EQ(vcpu_state.x[6], 12u);
  EXPECT_EQ(vcpu_state.x[7], 14u);
  EXPECT_EQ(vcpu_state.x[8], 16u);
  EXPECT_EQ(vcpu_state.x[9], 18u);
  EXPECT_EQ(vcpu_state.x[10], 20u);
  EXPECT_EQ(vcpu_state.x[11], 22u);
  EXPECT_EQ(vcpu_state.x[12], 24u);
  EXPECT_EQ(vcpu_state.x[13], 26u);
  EXPECT_EQ(vcpu_state.x[14], 28u);
  EXPECT_EQ(vcpu_state.x[15], 30u);
  EXPECT_EQ(vcpu_state.x[16], 32u);
  EXPECT_EQ(vcpu_state.x[17], 34u);
  EXPECT_EQ(vcpu_state.x[18], 36u);
  EXPECT_EQ(vcpu_state.x[19], 38u);
  EXPECT_EQ(vcpu_state.x[20], 40u);
  EXPECT_EQ(vcpu_state.x[21], 42u);
  EXPECT_EQ(vcpu_state.x[22], 44u);
  EXPECT_EQ(vcpu_state.x[23], 46u);
  EXPECT_EQ(vcpu_state.x[24], 48u);
  EXPECT_EQ(vcpu_state.x[25], 50u);
  EXPECT_EQ(vcpu_state.x[26], 52u);
  EXPECT_EQ(vcpu_state.x[27], 54u);
  EXPECT_EQ(vcpu_state.x[28], 56u);
  EXPECT_EQ(vcpu_state.x[29], 58u);
  EXPECT_EQ(vcpu_state.x[30], 60u);
  EXPECT_EQ(vcpu_state.sp, 128u);
  EXPECT_EQ(vcpu_state.cpsr, 0b0110u << 28);
#elif __x86_64__
  EXPECT_EQ(vcpu_state.rax, 2u);
  EXPECT_EQ(vcpu_state.rcx, 4u);
  EXPECT_EQ(vcpu_state.rdx, 6u);
  EXPECT_EQ(vcpu_state.rbx, 8u);
  EXPECT_EQ(vcpu_state.rsp, 10u);
  EXPECT_EQ(vcpu_state.rbp, 12u);
  EXPECT_EQ(vcpu_state.rsi, 14u);
  EXPECT_EQ(vcpu_state.rdi, 16u);
  EXPECT_EQ(vcpu_state.r8, 18u);
  EXPECT_EQ(vcpu_state.r9, 20u);
  EXPECT_EQ(vcpu_state.r10, 22u);
  EXPECT_EQ(vcpu_state.r11, 24u);
  EXPECT_EQ(vcpu_state.r12, 26u);
  EXPECT_EQ(vcpu_state.r13, 28u);
  EXPECT_EQ(vcpu_state.r14, 30u);
  EXPECT_EQ(vcpu_state.r15, 32u);
  EXPECT_EQ(vcpu_state.rflags, (1u << 0) | (1u << 18));
#endif  // __x86_64__
}

TEST(Guest, VcpuInterrupt) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_interrupt_start, vcpu_interrupt_end));
  test.interrupts_enabled = true;

#if __x86_64__
  // Resume once and wait for the guest to set up an IDT.
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
#endif

  ASSERT_EQ(test.vcpu.interrupt(kInterruptVector), ZX_OK);
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

#if __x86_64__
  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
  EXPECT_EQ(vcpu_state.rax, kInterruptVector);
#endif
}

TEST(Guest, GuestSetTrapWithMem) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, guest_set_trap_start, guest_set_trap_end));

  // Trap on access of TRAP_ADDR.
  ASSERT_EQ(test.guest.set_trap(ZX_GUEST_TRAP_MEM, TRAP_ADDR, PAGE_SIZE, zx::port(), kTrapKey),
            ZX_OK);

  zx_port_packet_t packet = {};
  ASSERT_EQ(test.vcpu.resume(&packet), ZX_OK);
  EXPECT_EQ(packet.key, kTrapKey);
  EXPECT_EQ(packet.type, ZX_PKT_TYPE_GUEST_MEM);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, GuestSetTrapWithBell) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, guest_set_trap_start, guest_set_trap_end));

  zx::port port;
  ASSERT_EQ(zx::port::create(0, &port), ZX_OK);

  // Trap on access of TRAP_ADDR.
  ASSERT_EQ(test.guest.set_trap(ZX_GUEST_TRAP_BELL, TRAP_ADDR, PAGE_SIZE, port, kTrapKey), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_port_packet_t packet = {};
  ASSERT_EQ(port.wait(zx::time::infinite(), &packet), ZX_OK);
  EXPECT_EQ(packet.key, kTrapKey);
  EXPECT_EQ(packet.type, ZX_PKT_TYPE_GUEST_BELL);
  EXPECT_EQ(packet.guest_bell.addr, static_cast<zx_gpaddr_t>(TRAP_ADDR));
}

// TestCase for fxbug.dev/33986.
TEST(Guest, GuestSetTrapWithBellDrop) {
  // Build the port before test so test is destructed first.
  zx::port port;
  ASSERT_EQ(zx::port::create(0, &port), ZX_OK);

  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, guest_set_trap_start, guest_set_trap_end));

  // Trap on access of TRAP_ADDR.
  ASSERT_EQ(test.guest.set_trap(ZX_GUEST_TRAP_BELL, TRAP_ADDR, PAGE_SIZE, port, kTrapKey), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  // The guest in test is destructed with one packet still queued on the
  // port. This should work correctly.
}

// TestCase for fxbug.dev/34001.
TEST(Guest, GuestSetTrapWithBellAndUser) {
  zx::port port;
  ASSERT_EQ(zx::port::create(0, &port), ZX_OK);

  // Queue a packet with the same key as the trap.
  zx_port_packet packet = {};
  packet.key = kTrapKey;
  packet.type = ZX_PKT_TYPE_USER;
  ASSERT_EQ(port.queue(&packet), ZX_OK);

  // Force guest to be released and cancel all packets associated with traps.
  {
    TestCase test;
    ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, guest_set_trap_start, guest_set_trap_end));

    // Trap on access of TRAP_ADDR.
    ASSERT_EQ(test.guest.set_trap(ZX_GUEST_TRAP_BELL, TRAP_ADDR, PAGE_SIZE, port, kTrapKey), ZX_OK);

    ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
  }

  ASSERT_EQ(port.wait(zx::time::infinite(), &packet), ZX_OK);
  EXPECT_EQ(packet.key, kTrapKey);
  EXPECT_EQ(packet.type, ZX_PKT_TYPE_USER);
}

// See that zx::vcpu::resume returns ZX_ERR_BAD_STATE if the port has been closed.
TEST(Guest, GuestSetTrapClosePort) {
  zx::port port;
  ASSERT_EQ(zx::port::create(0, &port), ZX_OK);

  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, guest_set_trap_start, guest_set_trap_end));

  ASSERT_EQ(test.guest.set_trap(ZX_GUEST_TRAP_BELL, TRAP_ADDR, PAGE_SIZE, port, kTrapKey), ZX_OK);

  port.reset();

  zx_port_packet_t packet = {};
  ASSERT_EQ(test.vcpu.resume(&packet), ZX_ERR_BAD_STATE);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

#ifdef __aarch64__

TEST(Guest, VcpuWfi) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_wfi_start, vcpu_wfi_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuWfiPendingInterrupt) {
  fuchsia::sysinfo::InterruptControllerInfoPtr info;
  ASSERT_EQ(ZX_OK, GetInterruptControllerInfo(&info));

  TestCase test;
  switch (info->type) {
    case fuchsia::sysinfo::InterruptControllerType::GIC_V2:
      ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_wfi_pending_interrupt_gicv2_start,
                                         vcpu_wfi_pending_interrupt_gicv2_end));
      break;
    case fuchsia::sysinfo::InterruptControllerType::GIC_V3:
      ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_wfi_pending_interrupt_gicv3_start,
                                         vcpu_wfi_pending_interrupt_gicv3_end));
      break;
    default:
      ASSERT_TRUE(false) << "Unsupported GIC version";
  }

  // Inject two interrupts so that there will be one pending when the guest exits on wfi.
  ASSERT_EQ(test.vcpu.interrupt(kInterruptVector), ZX_OK);
  ASSERT_EQ(test.vcpu.interrupt(kInterruptVector + 1), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuWfiAarch32) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_wfi_aarch32_start, vcpu_wfi_aarch32_end));

  zx_port_packet_t packet = {};
  ASSERT_EQ(test.vcpu.resume(&packet), ZX_OK);
  EXPECT_EQ(packet.type, ZX_PKT_TYPE_GUEST_MEM);
  EXPECT_EQ(packet.guest_mem.addr, static_cast<zx_gpaddr_t>(EXIT_TEST_ADDR));
  EXPECT_EQ(packet.guest_mem.read, false);
  EXPECT_EQ(packet.guest_mem.data, 0u);
}

TEST(Guest, VcpuFp) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_fp_start, vcpu_fp_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuFpAarch32) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_fp_aarch32_start, vcpu_fp_aarch32_end));

  zx_port_packet_t packet = {};
  ASSERT_EQ(test.vcpu.resume(&packet), ZX_OK);
  EXPECT_EQ(packet.type, ZX_PKT_TYPE_GUEST_MEM);
  EXPECT_EQ(packet.guest_mem.addr, static_cast<zx_gpaddr_t>(EXIT_TEST_ADDR));
  EXPECT_EQ(packet.guest_mem.read, false);
  EXPECT_EQ(packet.guest_mem.data, 0u);
}

TEST(Guest, VcpuPsciSystemOff) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_psci_system_off_start, vcpu_psci_system_off_end));

  zx_port_packet_t packet = {};
  ASSERT_EQ(test.vcpu.resume(&packet), ZX_ERR_UNAVAILABLE);
}

TEST(Guest, VcpuWriteStateIoAarch32) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, nullptr, nullptr));

  // ZX_VCPU_IO is not supported on arm64.
  zx_vcpu_io_t io{};
  io.access_size = 1;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_ERR_INVALID_ARGS);
}

#elif __x86_64__

TEST(Guest, VcpuInterruptPriority) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_interrupt_start, vcpu_interrupt_end));
  test.interrupts_enabled = true;

  // Resume once and wait for the guest to set up an IDT.
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  // Check that interrupts have higher priority than exceptions.
  ASSERT_EQ(test.vcpu.interrupt(kExceptionVector), ZX_OK);
  ASSERT_EQ(test.vcpu.interrupt(kInterruptVector), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
  EXPECT_EQ(vcpu_state.rax, kInterruptVector);

  // TODO(fxbug.dev/12585): Check that the exception is cleared.
}

TEST(Guest, VcpuNmi) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_interrupt_start, vcpu_interrupt_end));
  test.interrupts_enabled = true;

  // Resume once and wait for the guest to set up an IDT.
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  // Check that NMIs are handled.
  ASSERT_EQ(test.vcpu.interrupt(kNmiVector), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
  EXPECT_EQ(vcpu_state.rax, kNmiVector);
}

TEST(Guest, VcpuNmiPriority) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_interrupt_start, vcpu_interrupt_end));
  test.interrupts_enabled = true;

  // Resume once and wait for the guest to set up an IDT.
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  // Check that NMIs have higher priority than interrupts.
  ASSERT_EQ(test.vcpu.interrupt(kInterruptVector), ZX_OK);
  ASSERT_EQ(test.vcpu.interrupt(kNmiVector), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
  EXPECT_EQ(vcpu_state.rax, kNmiVector);

  // TODO(fxbug.dev/12585): Check that the interrupt is queued.
}

TEST(Guest, VcpuException) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_interrupt_start, vcpu_interrupt_end));
  test.interrupts_enabled = true;

  // Resume once and wait for the guest to set up an IDT.
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  // Check that exceptions are handled.
  ASSERT_EQ(test.vcpu.interrupt(kExceptionVector), ZX_OK);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
  EXPECT_EQ(vcpu_state.rax, kExceptionVector);
}

TEST(Guest, VcpuHlt) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupAndInterrupt(&test, vcpu_hlt_start, vcpu_hlt_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuPause) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_pause_start, vcpu_pause_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuWriteCr0) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_write_cr0_start, vcpu_write_cr0_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);

  // Check that the initial value of cr0, which was read into rbx, has the
  // correct initial values for the bits in the guest/host mask.
  EXPECT_EQ(vcpu_state.rbx & (X86_CR0_NE | X86_CR0_NW | X86_CR0_CD),
            static_cast<uint64_t>(X86_CR0_CD));

  // Check that the updated value of cr0, which was read into rax, correctly shadows the values in
  // the guest/host mask.
  EXPECT_EQ(vcpu_state.rax & (X86_CR0_NE | X86_CR0_CD), static_cast<uint64_t>(X86_CR0_NE));
}

TEST(Guest, VcpuWriteInvalidCr0) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(
      SetupGuest(&test, vcpu_write_invalid_cr0_start, vcpu_write_invalid_cr0_end));

  test.interrupts_enabled = true;

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
  EXPECT_EQ(vcpu_state.rax, kGpFaultVector);
}

TEST(Guest, VcpuCompatMode) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_compat_mode_start, vcpu_compat_mode_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
#if __x86_64__
  EXPECT_EQ(vcpu_state.rbx, 1u);
  EXPECT_EQ(vcpu_state.rcx, 2u);
#endif
}

TEST(Guest, VcpuSyscall) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_syscall_start, vcpu_syscall_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuSysenter) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_sysenter_start, vcpu_sysenter_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuSysenterCompat) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_sysenter_compat_start, vcpu_sysenter_compat_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

TEST(Guest, VcpuVmcallInvalidNumber) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(
      SetupGuest(&test, vcpu_vmcall_invalid_number_start, vcpu_vmcall_invalid_number_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);

  const uint64_t kUnknownHypercall = -1000;
  EXPECT_EQ(vcpu_state.rax, kUnknownHypercall);
}

TEST(Guest, VcpuVmcallInvalidCpl) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(
      SetupGuest(&test, vcpu_vmcall_invalid_cpl_start, vcpu_vmcall_invalid_cpl_end));

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);

  const uint64_t kNotPermitted = -1;
  EXPECT_EQ(vcpu_state.rax, kNotPermitted);
}

TEST(Guest, VcpuExtendedRegisters) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(
      SetupGuest(&test, vcpu_extended_registers_start, vcpu_extended_registers_end));

  // Guest sets xmm0.
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  // Clear host xmm0.
  __asm__("xorps %%xmm0, %%xmm0" ::: "xmm0");

  // Guest reads xmm0 into rax:rbx.
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));

  // Check that the host xmm0 is restored to zero.
  bool xmm0_is_zero;
  __asm__(
      "ptest %%xmm0, %%xmm0\n"
      "sete %0"
      : "=q"(xmm0_is_zero));
  EXPECT_TRUE(xmm0_is_zero);

  zx_vcpu_state_t vcpu_state;
  ASSERT_EQ(test.vcpu.read_state(ZX_VCPU_STATE, &vcpu_state, sizeof(vcpu_state)), ZX_OK);
  EXPECT_EQ(vcpu_state.rax, 0x89abcdef01234567u);
  EXPECT_EQ(vcpu_state.rbx, 0x76543210fedcba98u);

  // Guest disables SSE
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
  // Guest successfully runs again
  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

// Verify that write_state with ZX_VCPU_IO only accepts valid access sizes.
TEST(Guest, VcpuWriteStateIoInvalidSize) {
  TestCase test;
  // Passing nullptr for start and end since we don't need to actually run the guest for this test.
  ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, nullptr, nullptr));

  // valid access sizes
  zx_vcpu_io_t io{};
  io.access_size = 1;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_OK);
  io.access_size = 2;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_OK);
  io.access_size = 4;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_OK);

  // invalid access sizes
  io.access_size = 0;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_ERR_INVALID_ARGS);
  io.access_size = 3;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_ERR_INVALID_ARGS);
  io.access_size = 5;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_ERR_INVALID_ARGS);
  io.access_size = 255;
  ASSERT_EQ(test.vcpu.write_state(ZX_VCPU_IO, &io, sizeof(io)), ZX_ERR_INVALID_ARGS);
}

TEST(Guest, GuestSetTrapWithIo) {
  TestCase test;
  ASSERT_NO_FATAL_FAILURE(
      SetupGuest(&test, guest_set_trap_with_io_start, guest_set_trap_with_io_end));

  // Trap on writes to TRAP_PORT.
  ASSERT_EQ(test.guest.set_trap(ZX_GUEST_TRAP_IO, TRAP_PORT, 1, zx::port(), kTrapKey), ZX_OK);

  zx_port_packet_t packet = {};
  ASSERT_EQ(test.vcpu.resume(&packet), ZX_OK);
  EXPECT_EQ(packet.key, kTrapKey);
  EXPECT_EQ(packet.type, ZX_PKT_TYPE_GUEST_IO);
  EXPECT_EQ(packet.guest_io.port, TRAP_PORT);

  ASSERT_NO_FATAL_FAILURE(ResumeAndCleanExit(&test));
}

#endif  // __x86_64__

TEST(Guest, VcpuUseAfterThreadExits) {
  TestCase test;
  zx_status_t status = ZX_ERR_NOT_SUPPORTED;
  // Do the setup on another thread so that the VCPU attaches to the other thread.
  std::thread t([&]() {
    ASSERT_NO_FATAL_FAILURE(SetupGuest(&test, vcpu_resume_start, vcpu_resume_end));
    status = ZX_OK;
  });
  t.join();

  ASSERT_EQ(status, ZX_OK);
  // Send an interrupt to the VCPU after the thread has been shutdown.
  test.vcpu.interrupt(kInterruptVector);
  // Shutdown the VCPU after the thread has been shutdown.
  test.vcpu.reset();
}

}  // namespace

// Provide our own main so that we can abort testing if no guest support is detected.
int main(int argc, char** argv) {
  fxl::CommandLine cl = fxl::CommandLineFromArgcArgv(argc, argv);
  if (!fxl::SetTestSettings(cl)) {
    return EXIT_FAILURE;
  }

  // Ensure the platform supports running guests.
  if (!PlatformSupportsGuests()) {
    fprintf(stderr, "No support for running guests on current platform. Aborting tests.\n");
    return EXIT_FAILURE;
  }

  // Run tests.
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
