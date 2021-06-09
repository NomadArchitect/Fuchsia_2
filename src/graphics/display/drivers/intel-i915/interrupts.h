// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_I915_INTERRUPTS_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_I915_INTERRUPTS_H_

#include <fuchsia/hardware/intelgpucore/c/banjo.h>
#include <fuchsia/hardware/pci/cpp/banjo.h>
#include <lib/zx/interrupt.h>
#include <threads.h>
#include <zircon/types.h>

#include <optional>

#include <fbl/macros.h>

#include "registers-pipe.h"

namespace i915 {

class Controller;

class Interrupts {
 public:
  Interrupts(Controller* controller);
  ~Interrupts();

  zx_status_t Init();
  void FinishInit();
  void Resume();
  void Destroy();

  void EnablePipeVsync(registers::Pipe pipe, bool enable);
  zx_status_t SetInterruptCallback(const intel_gpu_core_interrupt_t* callback,
                                   uint32_t interrupt_mask);

  int IrqLoop();

 private:
  void EnableHotplugInterrupts();
  void HandlePipeInterrupt(registers::Pipe pipe, zx_time_t timestamp);

  // Initialized by constructor.
  Controller* controller_;  // Assume that controller callbacks are threadsafe
  mtx_t lock_;

  // Initialized by |Init|.
  zx::interrupt irq_;
  pci_irq_mode_t irq_mode_;
  std::optional<thrd_t> irq_thread_;  // Valid while irq_ is valid.

  intel_gpu_core_interrupt_t interrupt_cb_ __TA_GUARDED(lock_) = {};
  uint32_t interrupt_mask_ __TA_GUARDED(lock_) = 0;

  DISALLOW_COPY_ASSIGN_AND_MOVE(Interrupts);
};

}  // namespace i915

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_I915_INTERRUPTS_H_
