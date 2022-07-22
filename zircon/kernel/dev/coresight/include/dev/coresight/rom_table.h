// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_
#define ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_

#include <lib/fitx/result.h>
#include <zircon/assert.h>

#include <optional>
#include <string_view>
#include <type_traits>
#include <utility>

#include <dev/coresight/component.h>
#include <hwreg/bitfields.h>

namespace coresight {

// [CS] D6.4.4
struct Class0x1RomEntry : public hwreg::RegisterBase<Class0x1RomEntry, uint32_t> {
  DEF_FIELD(31, 12, offset);
  DEF_RSVDZ_FIELD(11, 9);
  DEF_FIELD(8, 4, powerid);
  DEF_RSVDZ_BIT(3);
  DEF_BIT(2, powerid_valid);
  DEF_BIT(1, format);
  DEF_BIT(0, present);

  static auto GetAt(uint32_t offset, uint32_t N) {
    return hwreg::RegisterAddr<Class0x1RomEntry>(offset +
                                                 N * static_cast<uint32_t>(sizeof(uint32_t)));
  }
};

// [CS] D7.5.17
struct Class0x9Rom32BitEntry : public hwreg::RegisterBase<Class0x9Rom32BitEntry, uint32_t> {
  DEF_FIELD(31, 12, offset);
  DEF_RSVDZ_FIELD(11, 9);
  DEF_FIELD(8, 4, powerid);
  DEF_RSVDZ_BIT(3);
  DEF_BIT(2, powerid_valid);
  DEF_FIELD(1, 0, present);

  static auto GetAt(uint32_t offset, uint32_t N) {
    return hwreg::RegisterAddr<Class0x9Rom32BitEntry>(offset +
                                                      N * static_cast<uint32_t>(sizeof(uint32_t)));
  }
};

// [CS] D7.5.17
struct Class0x9Rom64BitEntry : public hwreg::RegisterBase<Class0x9Rom64BitEntry, uint64_t> {
  DEF_FIELD(63, 12, offset);
  DEF_RSVDZ_FIELD(11, 9);
  DEF_FIELD(8, 4, powerid);
  DEF_RSVDZ_BIT(3);
  DEF_BIT(2, powerid_valid);
  DEF_FIELD(1, 0, present);

  static auto GetAt(uint32_t offset, uint32_t N) {
    return hwreg::RegisterAddr<Class0x9Rom64BitEntry>(offset +
                                                      N * static_cast<uint32_t>(sizeof(uint64_t)));
  }
};

// [CS] D7.5.10
struct Class0x9RomDeviceIdRegister
    : public hwreg::RegisterBase<Class0x9RomDeviceIdRegister, uint32_t> {
  enum class Format : uint8_t {
    k32Bit = 0,
    k64Bit = 1,
  };

  DEF_RSVDZ_FIELD(31, 6);
  DEF_BIT(5, prr);
  DEF_BIT(4, sysmem);
  DEF_ENUM_FIELD(Format, 3, 0, format);

  static auto GetAt(uint32_t offset) {
    return hwreg::RegisterAddr<Class0x9RomDeviceIdRegister>(offset + 0xfcc);
  }
};

// [CS] D5
// A ROM table is a basic CoreSight component that provides pointers to other
// components (including other ROM tables) in its lower registers via offsets
// from its base address. It is an organizational structure that can be used to
// find all CoreSight components - possibly as well as legacy or
// vendor-specific ones - on an SoC. Thought of as a tree, the leaves are the
// system's CoreSight components and the root is typically referred to as the
// "base ROM table" (or, more plainly, "the ROM table").
class RomTable {
 public:
  // Represents an error occurred while walking the table.
  struct WalkError {
    std::string_view reason;
    uint32_t offset{};
  };

  // Walks the underlying tree of components with no dynamic allocation,
  // calling `callback` on the offset from the table's base address (implicitly
  // encoded in `io`) of each component found. The (`io`, `max_offset`)
  // together implicitly give the aperture to walk.
  //
  // The walk will visit and access the first page of memory of each found
  // component. Unfortunately, however, there is no canonical means to
  // determine how large a region of memory this entails. The determination of
  // the maximum visited offset - or at least something deemed large enough -
  // is left to the caller. The offset must at least be
  // `kMininumComponentSize`, which is the size of the base table proper.
  template <typename IoProvider, typename ComponentCallback>
  static fitx::result<WalkError> Walk(IoProvider io, uint32_t max_offset,
                                      ComponentCallback&& callback) {
    static_assert(std::is_invocable_v<ComponentCallback, uint32_t>);
    ZX_ASSERT(max_offset >= kMinimumComponentSize);
    return WalkFrom(io, max_offset, callback, 0);
  }

 private:
  using ClassId = ComponentIdRegister::Class;
  using Class0x9EntryFormat = Class0x9RomDeviceIdRegister::Format;

  // [CS] D6.2.1, D7.2.1
  // The maximum number of ROM table entries, for various types.
  static constexpr uint32_t kMax0x1RomEntries = 960u;
  static constexpr uint32_t kMax0x9Rom32BitEntries = 512u;
  static constexpr uint32_t kMax0x9Rom64BitEntries = 256u;

  // There are several types of ROM table entry registers; this struct serves
  // as unified front-end for accessing their contents.
  struct EntryContents {
    uint64_t value;
    uint32_t offset;
    bool present;
  };

  template <typename IoProvider, typename ComponentCallback>
  static fitx::result<WalkError> WalkFrom(IoProvider io, uint32_t max_offset,
                                          ComponentCallback&& callback, uint32_t offset) {
    const ClassId classid = ComponentIdRegister::GetAt(offset).ReadFrom(&io).classid();
    const DeviceArchRegister arch_reg = DeviceArchRegister::GetAt(offset).ReadFrom(&io);
    const auto architect = static_cast<uint16_t>(arch_reg.architect());
    const auto archid = static_cast<uint16_t>(arch_reg.archid());
    if (IsTable(classid, architect, archid)) {
      uint32_t max_entries = 0;
      std::optional<Class0x9EntryFormat> format;
      if (classid == ClassId::k0x1RomTable) {
        max_entries = kMax0x1RomEntries;
      } else {
        // If not a class 0x1 table, then a class 0x9.
        ZX_DEBUG_ASSERT(classid == ClassId::kCoreSight);
        format = Class0x9RomDeviceIdRegister::GetAt(offset).ReadFrom(&io).format();
        switch (*format) {
          case Class0x9EntryFormat::k32Bit:
            max_entries = kMax0x9Rom32BitEntries;
            break;
          case Class0x9EntryFormat::k64Bit:
            max_entries = kMax0x9Rom64BitEntries;
            break;
          default:
            return fitx::error(WalkError{"bad format value", offset});
        }
      }

      for (uint32_t i = 0; i < max_entries; ++i) {
        fitx::result<std::string_view, EntryContents> read_entry_result =
            ReadEntryAt(io, offset, i, classid, format);
        if (read_entry_result.is_error()) {
          return fitx::error(WalkError{read_entry_result.error_value(), offset});
        }
        EntryContents contents = read_entry_result.value();
        if (contents.value == 0) {
          break;  // Signals that the walk is over if identically zero.
        }
        if (!contents.present) {
          continue;
        }
        // [CS] D5.4
        // the offset provided by the ROM table entry requires a shift of 12 bits.
        uint32_t new_offset = offset + (contents.offset << 12);
        if (max_offset - kMinimumComponentSize < new_offset) {
          printf("does not fit: (view size, offset) = (%u, %u)\n", max_offset, new_offset);
          return fitx::error(WalkError{"component exceeds aperture", new_offset});
        }
        if (fitx::result<WalkError> walk_result = WalkFrom(io, max_offset, callback, new_offset);
            walk_result.is_error()) {
          return walk_result;
        }
      }
      return fitx::ok();
    }

    // There should be a ROM table at offset zero.
    if (offset == 0) {
      return fitx::error{WalkError{"not a ROM table", 0}};
    }

    std::forward<ComponentCallback>(callback)(offset);
    return fitx::ok();
  }

  static bool IsTable(ClassId classid, uint16_t architect, uint16_t archid);

  template <typename IoProvider>
  static fitx::result<std::string_view, EntryContents> ReadEntryAt(
      IoProvider io, uint32_t offset, uint32_t N, ClassId classid,
      std::optional<Class0x9EntryFormat> format) {
    if (classid == ClassId::k0x1RomTable) {
      auto entry = Class0x1RomEntry::GetAt(offset, N).ReadFrom(&io);
      return fitx::ok(EntryContents{
          .value = entry.reg_value(),
          .offset = static_cast<uint32_t>(entry.offset()),
          .present = static_cast<bool>(entry.present()),
      });
    }

    // If not a class 0x1 table, then a class 0x9.
    ZX_DEBUG_ASSERT(classid == ClassId::kCoreSight);
    ZX_DEBUG_ASSERT(format);

    switch (*format) {
      case Class0x9EntryFormat::k32Bit: {
        auto entry = Class0x9Rom32BitEntry::GetAt(offset, N).ReadFrom(&io);
        return fitx::ok(EntryContents{
            .value = entry.reg_value(),
            .offset = static_cast<uint32_t>(entry.offset()),
            // [CS] D7.5.17: only a value of 0b11 for present() signifies presence.
            .present = static_cast<bool>(entry.present() & 0b11),
        });
      }
      case Class0x9EntryFormat::k64Bit: {
        auto entry = Class0x9Rom64BitEntry::GetAt(offset, N).ReadFrom(&io);
        uint64_t u32_offset = entry.offset() & 0xffffffff;
        if (entry.offset() != u32_offset) {
          return fitx::error(
              "a simplifying assumption is made that a ROM table entry's offset only contains 32 "
              "bits of information. If this is no longer true, please file a bug.");
        }
        return fitx::ok(EntryContents{
            .value = entry.reg_value(),
            .offset = static_cast<uint32_t>(u32_offset),
            .present = static_cast<bool>(entry.present() & 0b11),
        });
      }
    }
    return fitx::error("bad format value");
  }
};

}  // namespace coresight

#endif  // ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_
