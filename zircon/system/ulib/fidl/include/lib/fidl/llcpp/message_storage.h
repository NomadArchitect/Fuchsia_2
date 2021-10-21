// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_LLCPP_MESSAGE_STORAGE_H_
#define LIB_FIDL_LLCPP_MESSAGE_STORAGE_H_

#include <lib/fidl/llcpp/traits.h>
#include <lib/fit/function.h>

#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <type_traits>

#if defined(__clang__) && __has_attribute(uninitialized)
// Attribute "uninitialized" disables -ftrivial-auto-var-init=pattern
// (automatic variable initialization) for the specified variable.
// This is a security measure to better reveal memory corruptions and
// reduce leaking sensitive bits, but FIDL generated code/runtime can
// sometimes prove that a buffer is always overwritten. In those cases
// we can use this attribute to disable the compiler-inserted initialization
// and avoid the performance hit of writing to a large buffer.
#define FIDL_INTERNAL_DISABLE_AUTO_VAR_INIT __attribute__((uninitialized))
#else
#define FIDL_INTERNAL_DISABLE_AUTO_VAR_INIT
#endif

namespace fidl {

// Holds a reference to any storage buffer. This is independent of the allocation.
struct BufferSpan {
  BufferSpan() = default;
  BufferSpan(uint8_t* data, uint32_t capacity) : data(data), capacity(capacity) {}

  uint8_t* data = nullptr;
  uint32_t capacity = 0;
};

namespace internal {

// A stack allocated uninitialized array of |kSize| bytes, guaranteed to follow
// FIDL alignment.
//
// To properly ensure uninitialization, always declare objects of this type with
// FIDL_INTERNAL_DISABLE_AUTO_VAR_INIT.
template <size_t kSize>
struct InlineMessageBuffer {
  static_assert(kSize % FIDL_ALIGNMENT == 0, "kSize must be FIDL-aligned");

  // NOLINTNEXTLINE
  InlineMessageBuffer() {}
  InlineMessageBuffer(InlineMessageBuffer&&) = delete;
  InlineMessageBuffer(const InlineMessageBuffer&) = delete;
  InlineMessageBuffer& operator=(InlineMessageBuffer&&) = delete;
  InlineMessageBuffer& operator=(const InlineMessageBuffer&) = delete;

  BufferSpan view() { return BufferSpan(data(), kSize); }
  uint8_t* data() { return data_; }
  const uint8_t* data() const { return data_; }
  constexpr size_t size() const { return kSize; }

 private:
  FIDL_ALIGNDECL uint8_t data_[kSize];
};

static_assert(sizeof(InlineMessageBuffer<40>) == 40);

static_assert(alignof(std::max_align_t) % FIDL_ALIGNMENT == 0,
              "BoxedMessageBuffer should follow FIDL alignment when allocated on the heap.");

// A heap allocated uninitialized array of |kSize| bytes, guaranteed to follow
// FIDL alignment.
template <size_t kSize>
struct BoxedMessageBuffer {
  static_assert(kSize % FIDL_ALIGNMENT == 0, "kSize must be FIDL-aligned");

  BoxedMessageBuffer() { ZX_DEBUG_ASSERT(FidlIsAligned(bytes_)); }
  ~BoxedMessageBuffer() { delete[] bytes_; }
  BoxedMessageBuffer(BoxedMessageBuffer&&) = delete;
  BoxedMessageBuffer(const BoxedMessageBuffer&) = delete;
  BoxedMessageBuffer& operator=(BoxedMessageBuffer&&) = delete;
  BoxedMessageBuffer& operator=(const BoxedMessageBuffer&) = delete;

  BufferSpan view() { return BufferSpan(data(), kSize); }
  uint8_t* data() { return bytes_; }
  const uint8_t* data() const { return bytes_; }
  constexpr size_t size() const { return kSize; }

 private:
  uint8_t* bytes_ = new uint8_t[kSize];
};

// |AnyBufferAllocator| is a type-erasing buffer allocator. Its main purpose is
// to extend the caller-allocating call/reply flavors to work with a flexible
// range of buffer-like types ("upstream allocators").
//
// This class is similar in spirit to a |std::pmr::polymorphic_allocator|,
// except that it is specialized to allocating buffers (ranges of bytes).
//
// This class is compact (4 machine words), such that it may be efficiently
// moved around as a temporary value.
//
// If initialized with a |BufferSpan|, allocates in that buffer span. If
// initialized with a reference to some arena, allocates in that arena.
//
// To extend |AnyBufferAllocator| to work with future buffer-like types,
// declare a function overload for a user type |U| in the |::fidl::internal|
// namespace:
//
//     AnyBufferAllocator MakeAnyBufferAllocator(U upstream_allocator);
//
class AnyBufferAllocator {
 public:
  // An upstream allocator is an object that responds to allocation commands and
  // updates the state of the underlying memory resource referenced by the
  // function. It is similar to a reducer in functional-reactive programming.
  //
  // If the allocator cannot satisfy the allocation, it should return nullptr,
  // and preserve its original state before the allocation.
  //
  // Using |inline_function| ensures that there is no heap allocation, which
  // would otherwise defeat the purpose of caller-allocating flavors.
  //
  // |num_bytes| represents the size of the allocation request.
  using UpstreamAllocator = fit::inline_function<uint8_t*(uint32_t num_bytes)>;

  // This constructor should only be used by |MakeAnyBufferAllocator|.
  explicit AnyBufferAllocator(UpstreamAllocator&& upstream_allocator)
      : resource_(std::move(upstream_allocator)) {}

  // Allocates a buffer of size |num_bytes|.
  uint8_t* Allocate(uint32_t num_bytes) { return resource_(num_bytes); }

 private:
  UpstreamAllocator resource_;
};

static_assert(sizeof(AnyBufferAllocator) <= 4 * sizeof(void*),
              "AnyBufferAllocator should be reasonably small");

// Type erasing adaptor from |BufferSpan| to |AnyBufferAllocator|.
// See |AnyBufferAllocator|.
AnyBufferAllocator MakeAnyBufferAllocator(fidl::BufferSpan buffer_span);

}  // namespace internal
}  // namespace fidl

#endif  // LIB_FIDL_LLCPP_MESSAGE_STORAGE_H_
