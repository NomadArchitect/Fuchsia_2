// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_FIDL_DRIVER_INCLUDE_LIB_FIDL_DRIVER_CPP_UNIFIED_MESSAGING_DECLARATIONS_H_
#define LIB_FIDL_DRIVER_INCLUDE_LIB_FIDL_DRIVER_CPP_UNIFIED_MESSAGING_DECLARATIONS_H_

namespace fdf {

// |Result| represents the result of calling a two-way FIDL method |Method|.
//
// It inherits from different `fitx::result` types depending on |Method|:
//
// - When the method does not use the error syntax:
//     - When the method response has no body:
//
//           fitx::result<fidl::Error>
//
//     - When the method response has a body:
//
//           fitx::result<fidl::Error, MethodPayload>
//
//       where `fidl::Error` is a type representing any transport error or
//       protocol level terminal errors such as epitaphs, and |MethodPayload|
//       is the response type.
//
// - When the method uses the error syntax:
//     - When the method response payload is an empty struct:
//
//           fitx::result<fidl::AnyErrorIn<Method>>
//
//     - When the method response payload is not an empty struct:
//
//           fitx::result<fidl::AnyErrorIn<Method>, MethodPayload>
//
//       where |MethodPayload| is the success type.
//
// See also |fidl::AnyErrorIn|.
template <typename FidlMethod>
class Result;

// |AsyncEventHandler| is used by asynchronous clients to handle events using
// natural types. It also adds a callback for handling errors.
template <typename Protocol>
class AsyncEventHandler;

// |Server| is a pure-virtual interface to be implemented by a server, receiving
// natural types.
template <typename Protocol>
class Server;

}  // namespace fdf

#endif  // LIB_FIDL_DRIVER_INCLUDE_LIB_FIDL_DRIVER_CPP_UNIFIED_MESSAGING_DECLARATIONS_H_
