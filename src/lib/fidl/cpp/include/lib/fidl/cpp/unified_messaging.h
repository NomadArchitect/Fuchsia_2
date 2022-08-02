// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_FIDL_CPP_INCLUDE_LIB_FIDL_CPP_UNIFIED_MESSAGING_H_
#define SRC_LIB_FIDL_CPP_INCLUDE_LIB_FIDL_CPP_UNIFIED_MESSAGING_H_

#include <lib/fidl/cpp/internal/natural_client_base.h>
#include <lib/fidl/cpp/internal/natural_message_encoder.h>
#include <lib/fidl/cpp/internal/natural_types.h>
#include <lib/fidl/cpp/natural_types.h>
#include <lib/fidl/cpp/unified_messaging_declarations.h>
#include <lib/fidl/cpp/wire/message.h>
#include <lib/fidl/cpp/wire/traits.h>
#include <lib/fidl/cpp/wire/transaction.h>
#include <lib/fidl/cpp/wire/wire_messaging.h>
#include <lib/fitx/result.h>

#include <cstdint>

namespace fidl {

namespace internal {

template <typename FidlMethod>
struct NaturalMethodTypes {
  using Completer = ::fidl::Completer<>;
};

template <typename FidlMethod>
using NaturalCompleter = typename fidl::internal::NaturalMethodTypes<FidlMethod>::Completer;

// Note: application error types used in the error syntax are limited to int32,
// uint32, and enums thereof. Thus the same application error types are shared
// between wire and natural domain objects.
template <typename FidlMethod>
using NaturalApplicationError =
    typename fidl::internal::WireMethodTypes<FidlMethod>::ApplicationError;

// |NaturalMessageConverter| extends transactional message wrappers with the
// ability to convert to and from domain object types. In particular, result
// unions in methods using the error syntax will be converted to
// |fitx::result<ApplicationError, Payload>| when sending.
//
// |Message| is either a |fidl::Request<Foo>|, |fidl::Response<Foo>|, or
// |fidl::Event<Foo>|.
//
// It should only be used when |Message| has a body.
//
// The default implementation passes through the domain object without any
// transformation.
//
// For flexible two-way methods, |FromDomainObject| is not available. This is
// because the result union for flexible methods contains an extra variant
// |transport_err| which gets folded into |fidl::Error| during conversion to
// |fidl::Result<Foo>|, but which cannot be represented as part of
// |fidl::Response<Foo>|.
template <typename Message>
class NaturalMessageConverter {
  using DomainObject = typename MessageTraits<Message>::Payload;

  // Resource type: |DomainObject|
  // Value type: |const DomainObject&|
  using MessageArg =
      std::conditional_t<fidl::IsResource<DomainObject>::value, Message, const Message&>;

 public:
  static Message FromDomainObject(DomainObject&& o) { return static_cast<Message>(std::move(o)); }

  static DomainObject IntoDomainObject(MessageArg m) {
    return DomainObject{std::forward<MessageArg>(m)};
  }
};

// |DecodeTransactionalMessage| decodes a transactional incoming message to an
// instance of |Payload| containing natural types.
//
// To reducing branching in generated code, |payload| may be |std::nullopt|, in
// which case the message will be decoded without a payload (header-only
// messages).
//
// |message| is always consumed.
template <typename Payload = cpp17::nullopt_t>
static auto DecodeTransactionalMessage(::fidl::IncomingMessage&& message)
    -> std::conditional_t<std::is_same_v<Payload, cpp17::nullopt_t>, ::fitx::result<::fidl::Error>,
                          ::fitx::result<::fidl::Error, Payload>> {
  ZX_DEBUG_ASSERT(message.is_transactional());
  constexpr bool kHasPayload = !std::is_same_v<Payload, cpp17::nullopt_t>;
  const fidl_message_header& header = *message.header();
  auto metadata = ::fidl::WireFormatMetadata::FromTransactionalHeader(header);
  fidl::IncomingMessage body_message = message.SkipTransactionHeader();

  if constexpr (kHasPayload) {
    // Delegate into the decode logic of the payload.
    ::fitx::result decode_result = Decode<Payload>(std::move(body_message), metadata);
    if (decode_result.is_error()) {
      return ::fitx::result<::fidl::Error, Payload>(decode_result.take_error());
    }
    return ::fitx::result<::fidl::Error, Payload>(::fitx::ok(std::move(decode_result.value())));
  } else {
    if (body_message.byte_actual() > 0) {
      return ::fitx::result<::fidl::Error>(::fitx::error(
          ::fidl::Status::DecodeError(ZX_ERR_INVALID_ARGS, kCodingErrorNotAllBytesConsumed)));
    }
    if (body_message.handle_actual() > 0) {
      return ::fitx::result<::fidl::Error>(::fitx::error(
          ::fidl::Status::DecodeError(ZX_ERR_INVALID_ARGS, kCodingErrorNotAllHandlesConsumed)));
    }
    return ::fitx::result<::fidl::Error>(::fitx::ok());
  }
}

inline ::fitx::result<::fidl::Error> ToFitxResult(::fidl::Status result) {
  if (result.ok()) {
    return ::fitx::ok();
  }
  return ::fitx::error<::fidl::Error>(result);
}

}  // namespace internal

// |ClientCallback| is the async callback type used in the |fidl::Client| for
// the FIDL method |Method| that propagates errors, that works with natural
// domain objects.
//
// It is of the form:
//
//     void Callback(Result<Method>&);
//
// where |Result| is a result type of the protocol's transport
// (e.g. |fidl::Result| in Zircon channel messaging).
//
template <typename Method>
using ClientCallback = typename internal::NaturalMethodTypes<Method>::ResultCallback;

}  // namespace fidl

#endif  // SRC_LIB_FIDL_CPP_INCLUDE_LIB_FIDL_CPP_UNIFIED_MESSAGING_H_
