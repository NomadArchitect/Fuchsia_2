// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_LLCPP_FIDL_ALLOCATOR_H_
#define LIB_FIDL_LLCPP_FIDL_ALLOCATOR_H_

#include <lib/fidl/llcpp/traits.h>
#include <zircon/assert.h>

#include <functional>
#include <type_traits>

namespace fidl {

// Class common to all the FidlAllocator classes. This one is independent of the initial buffer
// size.
// All the implementation is done here. The only specialization available is FidlAllocator which
// defines the initial buffer size.
// The allocator owns all the data which are allocated. That means that the allocated data can be
// used by pure views.
// The allocated data are freed when the allocator is freed.
// The methods of the allocator are not directly called by the user. Instead ObjectView, StringView
// and VectorView use them.
// The allocation is done within the initial buffer. When the initial buffer is full (or, at least,
// the next allocation doesn't fit in the remaining space), the allocator allocates extra buffers
// on the heap. If one allocation is bigger than the capacity of a standard extra buffer, a
// tailored buffer is allocated which only contains the allocation.
//
// Allocations are put one after the other in the buffers. When a buffer can't fit the next
// allocation, the remaining space is lost an another buffer is allocated on the heap.
// Each allocation respects FIDL_ALIGNMENT.
// For allocations which don't need a destructor, we only allocate the requested size within the
// buffer.
// For allocations with a non trivial destructor, we also allocate some space for a
// |struct Destructor| which is stored before the requested data.
class AnyAllocator {
 private:
  AnyAllocator(uint8_t* next_data_available, size_t available_size)
      : next_data_available_(next_data_available), available_size_(available_size) {}
  ~AnyAllocator();

  // Struct used to store the data needed to deallocate an allocation (to call the destructor).
  struct Destructor {
    Destructor(Destructor* next, size_t count, void (*destructor)(uint8_t*, size_t))
        : next(next), count(count), destructor(destructor) {}

    Destructor* const next;
    const size_t count;
    void (*const destructor)(uint8_t*, size_t);
  };

  // Struct used to have more allocation buffers on the heap (when the initial buffer is full).
  struct ExtraBlock {
   public:
    // In most cases, the size in big enough to only need an extra allocation. It's also small
    // enough to not use too much heap memory.
    // The actual allocated size for the ExtraBlock struct will be 16Kb.
    static constexpr size_t kExtraSize = 16 * 1024 - FIDL_ALIGN(sizeof(ExtraBlock*));

    explicit ExtraBlock(ExtraBlock* next_block) : next_block_(next_block) {}

    ExtraBlock* next_block() const { return next_block_; }
    uint8_t* data() { return data_; }

   private:
    // Next block to deallocate (block allocated before this one).
    ExtraBlock* next_block_;
    // The usable data.
    alignas(FIDL_ALIGNMENT) uint8_t data_[kExtraSize];
  };

  // Deallocate anything allocated by the allocator. Any data previously allocated must not be
  // accessed anymore.
  void Clean();

  // Deallocate anything allocated by the allocator. After this call, the allocator is in the
  // extact same state it was after the construction. Any data previously allocated must not be
  // accessed anymore.
  void Reset(uint8_t* next_data_available, size_t available_size) {
    Clean();
    next_data_available_ = next_data_available;
    available_size_ = available_size;
  }

  // Allocates and default constructs an instance of T. Used by fidl::ObjectView.
  template <typename T, typename... Args>
  T* Allocate(Args&&... args) {
    return new (Allocate(sizeof(T), 1,
                         std::is_trivially_destructible<T>::value ? nullptr : ObjectDestructor<T>))
        T(std::forward<Args>(args)...);
  }

  // Allocates and default constructs a vector of T. Used by fidl::VectorView and StringView.
  // All the |count| vector elements are constructed.
  template <typename T>
  T* AllocateVector(size_t count) {
    return new (Allocate(sizeof(T), count,
                         std::is_trivially_destructible<T>::value ? nullptr : VectorDestructor<T>))
        typename std::remove_const<T>::type[count];
  }

  // Method which can deallocate an instance of T.
  template <typename T>
  static void ObjectDestructor(uint8_t* data, size_t count) {
    T* object = reinterpret_cast<T*>(data);
    object->~T();
  }

  // Method which can deallocate a vector of T.
  template <typename T>
  static void VectorDestructor(uint8_t* data, size_t count) {
    T* object = reinterpret_cast<T*>(data);
    T* end = object + count;
    while (object < end) {
      object->~T();
      ++object;
    }
  }

  // The actual allocation.
  // |Allocate| allocates the requested elements and eventually records the destructor to call
  // during the allocator destruction if |destructor_function| is not null.
  // The allocated data is not initialized (it will be initialized by the caller).
  uint8_t* Allocate(size_t item_size, size_t count,
                    void (*destructor_function)(uint8_t* data, size_t count));

  // Pointer to the next available data.
  uint8_t* next_data_available_;
  // Size of the data available at next_data_available_.
  size_t available_size_;
  // Linked list of the destructors to call starting with the last allocation.
  Destructor* last_destructor_ = nullptr;
  // Linked list of the extra blocks used for the allocation.
  ExtraBlock* last_extra_block_ = nullptr;

  template <typename T>
  friend class ObjectView;

  template <typename T>
  friend class VectorView;

  template <size_t>
  friend class FidlAllocator;
};

// Class which allows the allocation of data for the views (ObjectView, StringView, VectorView).
template <size_t initial_capacity = 512>
class FidlAllocator : public AnyAllocator {
 public:
  // Can't move because destructor pointers can point within initial_buffer_.
  FidlAllocator(FidlAllocator&& to_move) = delete;
  // Copying an allocator doesn't make sense.
  FidlAllocator(FidlAllocator& to_copy) = delete;

  FidlAllocator() : AnyAllocator(initial_buffer_, initial_capacity) {}

  // Deallocate anything allocated by the allocator. After this call, the allocator is in the
  // exact same state it was after the construction. Any data previously allocated must not be
  // accessed anymore.
  void Reset() { AnyAllocator::Reset(initial_buffer_, initial_capacity); }

 private:
  alignas(FIDL_ALIGNMENT) uint8_t initial_buffer_[initial_capacity];
};

}  // namespace fidl

#endif  // LIB_FIDL_LLCPP_FIDL_ALLOCATOR_H_
