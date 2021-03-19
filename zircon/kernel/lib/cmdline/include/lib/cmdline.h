// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_CMDLINE_INCLUDE_LIB_CMDLINE_H_
#define ZIRCON_KERNEL_LIB_CMDLINE_INCLUDE_LIB_CMDLINE_H_

#include <stdint.h>
#include <sys/types.h>
#include <zircon/compiler.h>

#include <optional>
#include <string_view>

#include <fbl/function.h>

// Cmdline is used to build and access the kernel command line.
//
// The underlying data is stored as a sequence of zero or more C strings followed by a final \0
// (i.e. an empty string).
//
// It can be accessed using the Get* methods or via data() and size().
//
// The Get* methods treat later values as overrides for earlier ones.
//
// For example, an empty command line is [\0], and a command line containing "a=b" is [a=b\0\0].
class Cmdline {
 public:
  static constexpr uint32_t kCmdlineMax = 4096;

  // Append |str| to the command line.
  //
  // |str| should contain "key=value" elements, separated by spaces.  Repeated spaces in |str| will
  // be combined.  Invalid characters will be replaced with '.'.
  //
  // For example:
  //
  //   Append("key=value  red foo=bar\n");
  //
  // will result in [key=value\0red=\0foo=bar.\0\0]
  //
  // Append may be called repeatedly. If |kCmdlineMax| is exceeded, will panic.
  //
  // The command line will always be properly terminated.
  void Append(const char* str);

  // Return the last value for |key| or nullptr if not found.
  //
  // When |key| is nullptr, the entire command line is returned.
  const char* GetString(const char* key) const;

  // Return the last value for |key| or |default_value| if not found.
  //
  // "0", "false", and "off" are considered false.  All other values are considered true.
  bool GetBool(const char* key, bool default_value) const;

  // Return the last value for |key| or |default_value| if not found.
  uint32_t GetUInt32(const char* key, uint32_t default_value) const;

  // Return the last value for |key| or |default_value| if not found.
  uint64_t GetUInt64(const char* key, uint64_t default_value) const;

  // Process and issue callbacks for the reserved RAM entries of the kernel
  // command line, fixing up the entries in response to the results of the
  // callback.
  //
  // A kernel command line may include commands to reserve sections of
  // contiguous physical RAM, usually for testing purposes.  Reserved sections
  // will be contiguous in physical RAM, off limits to the PMM allocator, and
  // accessible by usermode software with access to the root resource or an MMIO
  // resource with appropriate range.  The commands take the following form.
  //
  // kernel.ram.reserve.<name>=<size>,0xXXXXXXXXXXXXXXXX
  //
  // Note the "0xXXXXXXXXXXXXXXXX".  This is a placeholder for a dynamically
  // allocated address and needs to be replicated exactly so that the kernel has
  // a place to publish the physical address of the reservation to usermode.
  //
  // To assist in processing these regions, the Cmdline class provides the
  // ProcessRamReservions method.  This method will attempt to find all of the
  // requested reservation pairs in the system and call the user supplied
  // callback for each.  If the reservation fails for any reason, the method
  // will erase the entry in the cmd line image, replacing it with
  // "." characters instead.  If the reservation is successful, the method will
  // update the base address placeholder with physical address which was reserved.
  //
  // Users must supply a callback function/lambda to the ProcessRamReservations
  // call.  The size and name of each valid reservation will be supplied to the
  // callback, which must return the physical address of the successful
  // reservation, or std::nullopt in the case that the reservation fails for any
  // reason.
  using ProcessRamReservationsCbk =
      fbl::InlineFunction<std::optional<uintptr_t>(size_t size, std::string_view name),
                          sizeof(void*)>;
  void ProcessRamReservations(const ProcessRamReservationsCbk& cbk);

  // read-only  access to the underlying data
  const char* data() const { return data_; }

  // Return the size of data() including the final \0.
  //
  // Guaranteed to be >= 1;
  size_t size() const;

 protected:
  // Adds the given character to data_ and updates length_. If the character would cause the buffer
  // to exceed kCmdlineMax, panic.
  void AddOrAbort(char c);

  // Find the key of the explicitly specified length in our list of command line
  // arguments.
  const char* FindKey(const char* key, size_t key_len) const;

  // Zero-initialize to ensure the |gCmdline| instance of this class lives in the BSS rather than
  // DATA segment so we don't bloat the kernel.
  char data_[kCmdlineMax]{};
  // Does not include the final \0.
  size_t length_{};
};

extern Cmdline gCmdline;

// TODO(53594): migrate these to BootOptions.
namespace kernel_option {
static constexpr const char kBufferchainReservePages[] = "kernel.bufferchain.reserve-pages";
static constexpr const char kBypassDebuglog[] = "kernel.bypass-debuglog";
static constexpr const char kDebugUartPoll[] = "kernel.debug_uart_poll";
static constexpr const char kEnableDebuggingSyscalls[] = "kernel.enable-debugging-syscalls";
static constexpr const char kEnableSerialSysaclls[] = "kernel.enable-serial-syscalls";
static constexpr const char kEntropyTestLen[] = "kernel.entropy-test.len";
static constexpr const char kEntropyTestSrc[] = "kernel.entropy-test.src";
static constexpr const char kForceWatchdogDisabled[] = "kernel.force-watchdog-disabled";
static constexpr const char kGfxConsoleEarly[] = "gfxconsole.early";
static constexpr const char kGfxConsoleFont[] = "gfxconsole.font";
static constexpr const char kHaltOnPanic[] = "kernel.halt-on-panic";
static constexpr const char kKtraceBufSize[] = "ktrace.bufsize";
static constexpr const char kKtraceGrpMask[] = "ktrace.grpmask";
static constexpr const char kLockupDetectorCriticalSectionFatalThresholdMs[] =
    "kernel.lockup-detector.critical-section-fatal-threshold-ms";
static constexpr const char kLockupDetectorCriticalSectionThresholdMs[] =
    "kernel.lockup-detector.critical-section-threshold-ms";
static constexpr const char kLockupDetectorHeartbeatAgeFatalThresholdMs[] =
    "kernel.lockup-detector.heartbeat-age-fatal-threshold-ms";
static constexpr const char kLockupDetectorHeartbeatAgeThresholdMs[] =
    "kernel.lockup-detector.heartbeat-age-threshold-ms";
static constexpr const char kLockupDetectorHeartbeatPeriodMs[] =
    "kernel.lockup-detector.heartbeat-period-ms";
static constexpr const char kMemoryLimitDbg[] = "kernel.memory-limit-dbg";
static constexpr const char kMemoryLimitMb[] = "kernel.memory-limit-mb";
static constexpr const char kMexecForceHighRamdisk[] = "kernel.mexec-force-high-ramdisk";
static constexpr const char kMexecPciShutdown[] = "kernel.mexec-pci-shutdown";
static constexpr const char kPageScannerEnableEviction[] = "kernel.page-scanner.enable-eviction";
static constexpr const char kPageScannerDiscardableEvictionsPercent[] =
    "kernel.page-scanner.discardable-evictions-percent";
static constexpr const char kPageScannerPageTableEvictionPolicy[] =
    "kernel.page-scanner.page-table-eviction-policy";
static constexpr const char kPageScannerPromoteNoClones[] = "kernel.page-scanner.promote-no-clones";
static constexpr const char kPageScannerStartAtBoot[] = "kernel.page-scanner.start-at-boot";
static constexpr const char kPageScannerZeroPageScansPerSecond[] =
    "kernel.page-scanner.zero-page-scans-per-second";
static constexpr const char kPmmCheckerAction[] = "kernel.pmm-checker.action";
static constexpr const char kPmmCheckerEnable[] = "kernel.pmm-checker.enable";
static constexpr const char kPmmCheckerFillSize[] = "kernel.pmm-checker.fill-size";
static constexpr const char kPortobserverReservePages[] = "kernel.portobserver.reserve-pages";
static constexpr const char kPortPacketReservePages[] = "kernel.portpacket.reserve-pages";
static constexpr const char kRootJobBehavior[] = "kernel.root-job.behavior";
static constexpr const char kRootJobNotice[] = "kernel.root-job.notice";
static constexpr const char kSerial[] = "kernel.serial";
static constexpr const char kShell[] = "kernel.shell";
static constexpr const char kSmpHt[] = "kernel.smp.ht";
static constexpr const char kSmpMaxCpus[] = "kernel.smp.maxcpus";
static constexpr const char kUserpagerOverTimeTimeoutSeconds[] =
    "kernel.userpager.overtime_timeout_seconds";
static constexpr const char kUserpagerOverTimeWaitSeconds[] =
    "kernel.userpager.overtime_wait_seconds";
static constexpr const char kVdsoClockGetMonotonicForceSyscall[] =
    "vdso.clock_get_monotonic_force_syscall";
static constexpr const char kVdsoTicksGetForceSyscall[] = "vdso.ticks_get_force_syscall";
static constexpr const char kWallclock[] = "kernel.wallclock";
}  // namespace kernel_option

#endif  // ZIRCON_KERNEL_LIB_CMDLINE_INCLUDE_LIB_CMDLINE_H_
