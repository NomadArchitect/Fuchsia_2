// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_FIDL_CPP_INCLUDE_LIB_FIDL_CPP_NATURAL_CODING_ERRORS_H_
#define SRC_LIB_FIDL_CPP_INCLUDE_LIB_FIDL_CPP_NATURAL_CODING_ERRORS_H_

namespace fidl::internal {

// Use extern definitions of errors to avoid a copy for each .cc file including
// this .h file.
extern const char* const kCodingErrorInvalidBoolean;
extern const char* const kCodingErrorVectorLimitExceeded;
extern const char* const kCodingErrorNullDataReceivedForNonNullableVector;
extern const char* const kCodingErrorNullVectorMustHaveSizeZero;
extern const char* const kCodingErrorStringLimitExceeded;
extern const char* const kCodingErrorNullDataReceivedForNonNullableString;
extern const char* const kCodingErrorNullStringMustHaveSizeZero;
extern const char* const kCodingErrorStringNotValidUtf8;
extern const char* const kCodingErrorNullTableMustHaveSizeZero;
extern const char* const kCodingErrorInvalidNumBytesSpecifiedInEnvelope;
extern const char* const kCodingErrorInvalidNumHandlesSpecifiedInEnvelope;
extern const char* const kCodingErrorNonEmptyByteCountInNullEnvelope;
extern const char* const kCodingErrorNonEmptyHandleCountInNullEnvelope;
extern const char* const kCodingErrorInvalidInlineBit;
extern const char* const kCodingErrorUnknownBitSetInBitsValue;
extern const char* const kCodingErrorUnknownEnumValue;
extern const char* const kCodingErrorUnknownUnionTag;
extern const char* const kCodingErrorInvalidPaddingBytes;
extern const char* const kCodingErrorRecursionDepthExceeded;
extern const char* const kCodingErrorInvalidPresenceIndicator;
extern const char* const kCodingErrorNotAllBytesConsumed;
extern const char* const kCodingErrorNotAllHandlesConsumed;
extern const char* const kCodingErrorAllocationSizeExceeds32Bits;
extern const char* const kCodingErrorOutOfLineObjectExceedsMessageBounds;
extern const char* const kCodingErrorTooManyHandlesConsumed;
extern const char* const kCodingErrorAbsentNonNullableHandle;

}  // namespace fidl::internal

#endif  // SRC_LIB_FIDL_CPP_INCLUDE_LIB_FIDL_CPP_NATURAL_CODING_ERRORS_H_
