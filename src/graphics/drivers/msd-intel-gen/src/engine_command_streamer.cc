// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "engine_command_streamer.h"

#include <thread>

#include "cache_config.h"
#include "instructions.h"
#include "magma_util/macros.h"
#include "msd_intel_buffer.h"
#include "msd_intel_connection.h"
#include "platform_logger.h"
#include "platform_trace.h"
#include "registers.h"
#include "render_init_batch.h"
#include "ringbuffer.h"
#include "workarounds.h"

EngineCommandStreamer::EngineCommandStreamer(Owner* owner, EngineCommandStreamerId id,
                                             uint32_t mmio_base)
    : owner_(owner), id_(id), mmio_base_(mmio_base) {
  DASSERT(owner);
}

bool EngineCommandStreamer::InitContext(MsdIntelContext* context) const {
  DASSERT(context);

  uint32_t context_size = GetContextSize();
  DASSERT(context_size > 0 && magma::is_page_aligned(context_size));

  std::unique_ptr<MsdIntelBuffer> context_buffer(
      MsdIntelBuffer::Create(context_size, "context-buffer"));
  if (!context_buffer)
    return DRETF(false, "couldn't create context buffer");

  const uint32_t kRingbufferSize = 32 * magma::page_size();
  auto ringbuffer =
      std::make_unique<Ringbuffer>(MsdIntelBuffer::Create(kRingbufferSize, "ring-buffer"));
  ringbuffer->Reset(kRingbufferSize - magma::page_size());

  if (!InitContextBuffer(context_buffer.get(), ringbuffer.get(),
                         context->exec_address_space().get()))
    return DRETF(false, "InitContextBuffer failed");

  // Transfer ownership of context_buffer
  context->SetEngineState(id(), std::move(context_buffer), std::move(ringbuffer));

  return true;
}

bool EngineCommandStreamer::InitContextWorkarounds(MsdIntelContext* context) {
  auto ringbuffer = context->get_ringbuffer(id());

  if (!ringbuffer->HasSpace(Workarounds::InstructionBytesRequired()))
    return DRETF(false, "insufficient ringbuffer space for workarounds");

  if (!Workarounds::Init(ringbuffer, id()))
    return DRETF(false, "failed to init workarounds");

  return true;
}

bool EngineCommandStreamer::InitContextCacheConfig(MsdIntelContext* context) {
  auto ringbuffer = context->get_ringbuffer(id());

  if (!ringbuffer->HasSpace(CacheConfig::InstructionBytesRequired()))
    return DRETF(false, "insufficient ringbuffer space for cache config");

  if (!CacheConfig::InitCacheConfig(ringbuffer, id()))
    return DRETF(false, "failed to init cache config buffer");

  return true;
}

void EngineCommandStreamer::InitHardware() {
  Reset();

  HardwareStatusPage* status_page = hardware_status_page(id());

  uint32_t gtt_addr = magma::to_uint32(status_page->gpu_addr());
  registers::HardwareStatusPageAddress::write(register_io(), mmio_base_, gtt_addr);

  // TODO(fxbug.dev/80908) - switch to engine specific sequence numbers?
  uint32_t initial_sequence_number = sequencer()->next_sequence_number();
  status_page->write_sequence_number(initial_sequence_number);

  DLOG("initialized engine sequence number: 0x%x", initial_sequence_number);

  registers::GraphicsMode::write(register_io(), mmio_base_,
                                 registers::GraphicsMode::kExeclistEnable,
                                 registers::GraphicsMode::kExeclistEnable);

  registers::HardwareStatusMask::write(register_io(), mmio_base_,
                                       registers::InterruptRegisterBase::USER,
                                       registers::InterruptRegisterBase::UNMASK);

  registers::HardwareStatusMask::write(register_io(), mmio_base_,
                                       registers::InterruptRegisterBase::CONTEXT_SWITCH,
                                       registers::InterruptRegisterBase::UNMASK);
}

void EngineCommandStreamer::InvalidateTlbs() {
  // Should only be called when gpu is idle.
  switch (id()) {
    case RENDER_COMMAND_STREAMER: {
      auto reg = registers::RenderEngineTlbControl::Get().FromValue(0);
      reg.invalidate().set(1);
      reg.WriteTo(register_io());
      break;
    }
    default:
      DASSERT(false);
      break;
  }
}

// Register definitions from BSpec BXML Reference.
// Register State Context definition from public BSpec.
// Render command streamer:
// https://01.org/sites/default/files/documentation/intel-gfx-prm-osrc-kbl-vol07-3d_media_gpgpu.pdf
// pp.25 Video command streamer:
// https://01.org/sites/default/files/documentation/intel-gfx-prm-osrc-kbl-vol03-gpu_overview.pdf
// pp.15
class RegisterStateHelper {
 public:
  RegisterStateHelper(EngineCommandStreamerId id, uint32_t mmio_base, uint32_t* state)
      : id_(id), mmio_base_(mmio_base), state_(state) {}

  void write_load_register_immediate_headers() {
    state_[0x1] = 0x1100101B;
    state_[0x21] = 0x11001011;
    switch (id_) {
      case RENDER_COMMAND_STREAMER:
        state_[0x41] = 0x11000001;
        break;
      case VIDEO_COMMAND_STREAMER:
        break;
      default:
        DASSERT(false);
    }
  }

  // CTXT_SR_CTL - Context Save/Restore Control Register
  void write_context_save_restore_control() {
    constexpr uint32_t kInhibitSyncContextSwitchBit = 1 << 3;
    constexpr uint32_t kRenderContextRestoreInhibitBit = 1;

    state_[0x2] = mmio_base_ + 0x244;

    uint32_t bits = kInhibitSyncContextSwitchBit;
    if (id_ == RENDER_COMMAND_STREAMER) {
      bits |= kRenderContextRestoreInhibitBit;
    }
    state_[0x3] = (bits << 16) | bits;
  }

  // RING_BUFFER_HEAD - Ring Buffer Head
  void write_ring_head_pointer(uint32_t head) {
    state_[0x4] = mmio_base_ + 0x34;
    state_[0x5] = head;
  }

  // RING_BUFFER_TAIL - Ring Buffer Tail
  void write_ring_tail_pointer(uint32_t tail) {
    state_[0x6] = mmio_base_ + 0x30;
    state_[0x7] = tail;
  }

  // RING_BUFFER_START - Ring Buffer Start
  void write_ring_buffer_start(uint32_t gtt_ring_buffer_start) {
    DASSERT(magma::is_page_aligned(gtt_ring_buffer_start));
    state_[0x8] = mmio_base_ + 0x38;
    state_[0x9] = gtt_ring_buffer_start;
  }

  // RING_BUFFER_CTL - Ring Buffer Control
  void write_ring_buffer_control(uint32_t ringbuffer_size) {
    constexpr uint32_t kRingValid = 1;
    DASSERT(ringbuffer_size >= PAGE_SIZE && ringbuffer_size <= 512 * PAGE_SIZE);
    DASSERT(magma::is_page_aligned(ringbuffer_size));
    state_[0xA] = mmio_base_ + 0x3C;
    // This register assumes 4k pages
    DASSERT(PAGE_SIZE == 4096);
    state_[0xB] = (ringbuffer_size - PAGE_SIZE) | kRingValid;
  }

  // BB_ADDR_UDW - Batch Buffer Upper Head Pointer Register
  void write_batch_buffer_upper_head_pointer() {
    state_[0xC] = mmio_base_ + 0x168;
    state_[0xD] = 0;
  }

  // BB_ADDR - Batch Buffer Head Pointer Register
  void write_batch_buffer_head_pointer() {
    state_[0xE] = mmio_base_ + 0x140;
    state_[0xF] = 0;
  }

  // BB_STATE - Batch Buffer State Register
  void write_batch_buffer_state() {
    constexpr uint32_t kAddressSpacePpgtt = 1 << 5;
    state_[0x10] = mmio_base_ + 0x110;
    state_[0x11] = kAddressSpacePpgtt;
  }

  // SBB_ADDR_UDW - Second Level Batch Buffer Upper Head Pointer Register
  void write_second_level_batch_buffer_upper_head_pointer() {
    state_[0x12] = mmio_base_ + 0x11C;
    state_[0x13] = 0;
  }

  // SBB_ADDR - Second Level Batch Buffer Head Pointer Register
  void write_second_level_batch_buffer_head_pointer() {
    state_[0x14] = mmio_base_ + 0x114;
    state_[0x15] = 0;
  }

  // SBB_STATE - Second Level Batch Buffer State Register
  void write_second_level_batch_buffer_state() {
    state_[0x16] = mmio_base_ + 0x118;
    state_[0x17] = 0;
  }

  // BB_PER_CTX_PTR - Batch Buffer Per Context Pointer
  void write_batch_buffer_per_context_pointer() {
    state_[0x18] = mmio_base_ + 0x1C0;
    state_[0x19] = 0;
  }

  // INDIRECT_CTX - Indirect Context Pointer
  void write_indirect_context_pointer() {
    state_[0x1A] = mmio_base_ + 0x1C4;
    state_[0x1B] = 0;
  }

  // INDIRECT_CTX_OFFSET - Indirect Context Offset Pointer
  void write_indirect_context_offset_pointer() {
    state_[0x1C] = mmio_base_ + 0x1C8;
    state_[0x1D] = 0;
  }

  // CS_CTX_TIMESTAMP - CS Context Timestamp Count
  void write_context_timestamp() {
    state_[0x22] = mmio_base_ + 0x3A8;
    state_[0x23] = 0;
  }

  void write_pdp3_upper(uint64_t pdp_bus_addr) {
    state_[0x24] = mmio_base_ + 0x28C;
    state_[0x25] = magma::upper_32_bits(pdp_bus_addr);
  }

  void write_pdp3_lower(uint64_t pdp_bus_addr) {
    state_[0x26] = mmio_base_ + 0x288;
    state_[0x27] = magma::lower_32_bits(pdp_bus_addr);
  }

  void write_pdp2_upper(uint64_t pdp_bus_addr) {
    state_[0x28] = mmio_base_ + 0x284;
    state_[0x29] = magma::upper_32_bits(pdp_bus_addr);
  }

  void write_pdp2_lower(uint64_t pdp_bus_addr) {
    state_[0x2A] = mmio_base_ + 0x280;
    state_[0x2B] = magma::lower_32_bits(pdp_bus_addr);
  }

  void write_pdp1_upper(uint64_t pdp_bus_addr) {
    state_[0x2C] = mmio_base_ + 0x27C;
    state_[0x2D] = magma::upper_32_bits(pdp_bus_addr);
  }

  void write_pdp1_lower(uint64_t pdp_bus_addr) {
    state_[0x2E] = mmio_base_ + 0x278;
    state_[0x2F] = magma::lower_32_bits(pdp_bus_addr);
  }

  void write_pdp0_upper(uint64_t pdp_bus_addr) {
    state_[0x30] = mmio_base_ + 0x274;
    state_[0x31] = magma::upper_32_bits(pdp_bus_addr);
  }

  void write_pdp0_lower(uint64_t pdp_bus_addr) {
    state_[0x32] = mmio_base_ + 0x270;
    state_[0x33] = magma::lower_32_bits(pdp_bus_addr);
  }

  // R_PWR_CLK_STATE - Render Power Clock State Register
  void write_render_power_clock_state() {
    DASSERT(id_ == RENDER_COMMAND_STREAMER);
    state_[0x42] = mmio_base_ + 0x0C8;
    state_[0x43] = 0;
  }

 private:
  EngineCommandStreamerId id_;
  uint32_t mmio_base_;
  uint32_t* state_;
};

bool EngineCommandStreamer::InitContextBuffer(MsdIntelBuffer* buffer, Ringbuffer* ringbuffer,
                                              AddressSpace* address_space) const {
  auto platform_buf = buffer->platform_buffer();
  void* addr;
  if (!platform_buf->MapCpu(&addr))
    return DRETF(false, "Couldn't map context buffer");

  uint32_t* state = reinterpret_cast<uint32_t*>(reinterpret_cast<uint8_t*>(addr) + PAGE_SIZE);
  RegisterStateHelper helper(id(), mmio_base_, state);

  helper.write_load_register_immediate_headers();
  helper.write_context_save_restore_control();
  helper.write_ring_head_pointer(ringbuffer->head());
  // Ring buffer tail and start is patched in later (see UpdateContext).
  helper.write_ring_tail_pointer(0);
  helper.write_ring_buffer_start(0);
  helper.write_ring_buffer_control(ringbuffer->size());
  helper.write_batch_buffer_upper_head_pointer();
  helper.write_batch_buffer_head_pointer();
  helper.write_batch_buffer_state();
  helper.write_second_level_batch_buffer_upper_head_pointer();
  helper.write_second_level_batch_buffer_head_pointer();
  helper.write_second_level_batch_buffer_state();
  helper.write_batch_buffer_per_context_pointer();
  helper.write_indirect_context_pointer();
  helper.write_indirect_context_offset_pointer();
  helper.write_context_timestamp();
  helper.write_pdp3_upper(0);
  helper.write_pdp3_lower(0);
  helper.write_pdp2_upper(0);
  helper.write_pdp2_lower(0);
  helper.write_pdp1_upper(0);
  helper.write_pdp1_lower(0);
  helper.write_pdp0_upper(0);
  helper.write_pdp0_lower(0);
  if (address_space->type() == ADDRESS_SPACE_PPGTT) {
    auto ppgtt = static_cast<PerProcessGtt*>(address_space);
    uint64_t pml4_addr = ppgtt->get_pml4_bus_addr();
    helper.write_pdp0_upper(pml4_addr);
    helper.write_pdp0_lower(pml4_addr);
  }

  if (id() == RENDER_COMMAND_STREAMER) {
    helper.write_render_power_clock_state();
  }

  if (!platform_buf->UnmapCpu())
    return DRETF(false, "Couldn't unmap context buffer");

  return true;
}

bool EngineCommandStreamer::SubmitContext(MsdIntelContext* context, uint32_t tail) {
  TRACE_DURATION("magma", "SubmitContext");
  if (!UpdateContext(context, tail))
    return DRETF(false, "UpdateContext failed");

  SubmitExeclists(context);
  return true;
}

bool EngineCommandStreamer::UpdateContext(MsdIntelContext* context, uint32_t tail) {
  gpu_addr_t gpu_addr;
  if (!context->GetRingbufferGpuAddress(id(), &gpu_addr))
    return DRETF(false, "failed to get ringbuffer gpu address");

  uint8_t* cpu_addr = reinterpret_cast<uint8_t*>(context->GetCachedContextBufferCpuAddr(id()));
  if (!cpu_addr)
    return DRETF(false, "failed to get cached context buffer cpu address");

  RegisterStateHelper helper(id(), mmio_base_, reinterpret_cast<uint32_t*>(cpu_addr + PAGE_SIZE));

  DLOG("UpdateContext ringbuffer gpu_addr 0x%lx tail 0x%x", gpu_addr, tail);

  uint32_t gtt_addr = magma::to_uint32(gpu_addr);
  helper.write_ring_buffer_start(gtt_addr);
  helper.write_ring_tail_pointer(tail);

  return true;
}

void EngineCommandStreamer::SubmitExeclists(MsdIntelContext* context) {
  TRACE_DURATION("magma", "SubmitExeclists");
  gpu_addr_t gpu_addr;
  if (!context->GetGpuAddress(id(), &gpu_addr)) {
    // Shouldn't happen.
    DASSERT(false);
    gpu_addr = kInvalidGpuAddr;
  }

  auto start = std::chrono::high_resolution_clock::now();

  for (bool busy = true; busy;) {
    constexpr uint32_t kTimeoutUs = 100;
    uint64_t status = registers::ExeclistStatus::read(register_io(), mmio_base());

    busy = registers::ExeclistStatus::execlist_write_pointer(status) ==
               registers::ExeclistStatus::execlist_current_pointer(status) &&
           registers::ExeclistStatus::execlist_queue_full(status);
    if (busy) {
      if (std::chrono::duration<double, std::micro>(std::chrono::high_resolution_clock::now() -
                                                    start)
              .count() > kTimeoutUs) {
        MAGMA_LOG(WARNING, "Timeout waiting for execlist port");
        break;
      }
    }
  }

  DLOG("SubmitExeclists context descriptor id 0x%lx", gpu_addr >> 12);

  // Use most significant bits of context gpu_addr as globally unique context id
  DASSERT(PAGE_SIZE == 4096);
  uint64_t descriptor0 = registers::ExeclistSubmitPort::context_descriptor(
      gpu_addr, magma::to_uint32(gpu_addr >> 12),
      context->exec_address_space()->type() == ADDRESS_SPACE_PPGTT);
  uint64_t descriptor1 = 0;

  registers::ExeclistSubmitPort::write(register_io(), mmio_base_, descriptor1, descriptor0);
}

uint64_t EngineCommandStreamer::GetActiveHeadPointer() {
  return registers::ActiveHeadPointer::read(register_io(), mmio_base_);
}

bool EngineCommandStreamer::Reset() {
  registers::GraphicsDeviceResetControl::Engine engine;

  switch (id()) {
    case RENDER_COMMAND_STREAMER:
      engine = registers::GraphicsDeviceResetControl::RENDER_ENGINE;
      break;
    default:
      // TODO:(fxbug.dev/80909) - reset for video CS
      return DRETF(false, "Reset for engine id %d not implemented", id());
  }

  registers::ResetControl::request(register_io(), mmio_base());

  constexpr uint32_t kRetryMs = 10;
  constexpr uint32_t kRetryTimeoutMs = 100;

  auto start = std::chrono::high_resolution_clock::now();
  std::chrono::duration<double, std::milli> elapsed;

  bool ready_for_reset = false;
  do {
    ready_for_reset = registers::ResetControl::ready_for_reset(register_io(), mmio_base());
    if (ready_for_reset) {
      break;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(kRetryMs));
    elapsed = std::chrono::high_resolution_clock::now() - start;
  } while (elapsed.count() < kRetryTimeoutMs);

  bool reset_complete = false;
  if (ready_for_reset) {
    registers::GraphicsDeviceResetControl::initiate_reset(register_io(), engine);
    start = std::chrono::high_resolution_clock::now();

    do {
      reset_complete =
          registers::GraphicsDeviceResetControl::is_reset_complete(register_io(), engine);
      if (reset_complete) {
        break;
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(kRetryMs));
      elapsed = std::chrono::high_resolution_clock::now() - start;
    } while (elapsed.count() < kRetryTimeoutMs);
  }

  // Always invalidate tlbs, otherwise risk memory corruption.
  InvalidateTlbs();

  DLOG("ready_for_reset %d reset_complete %d", ready_for_reset, reset_complete);

  return DRETF(reset_complete, "Reset did not complete");
}

bool EngineCommandStreamer::StartBatchBuffer(MsdIntelContext* context, gpu_addr_t gpu_addr,
                                             AddressSpaceType address_space_type) {
  auto ringbuffer = context->get_ringbuffer(id());

  uint32_t dword_count = MiBatchBufferStart::kDwordCount + MiNoop::kDwordCount;

  if (!ringbuffer->HasSpace(dword_count * sizeof(uint32_t)))
    return DRETF(false, "ringbuffer has insufficient space");

  MiBatchBufferStart::write(ringbuffer, gpu_addr, address_space_type);
  MiNoop::write(ringbuffer);

  DLOG("started batch buffer 0x%lx address_space_type %d", gpu_addr, address_space_type);

  return true;
}
