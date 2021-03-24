// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package codegen

const tmplDecoderEncoder = `
{{- define "DecoderEncoder" -}}
[](uint8_t* bytes, uint32_t num_bytes, zx_handle_info_t* handles, uint32_t num_handles) ->
  ::std::pair<zx_status_t, zx_status_t> {
  {{ .Wire }}::DecodedMessage decoded(bytes, num_bytes);  // decoder_encoder.

  if (decoded.status() != ZX_OK) {
    return ::std::make_pair<zx_status_t, zx_status_t>(decoded.status(), ZX_ERR_INTERNAL);
  }

  {{ .Wire }}* value = decoded.PrimaryObject();
  {{ .Wire }}::OwnedEncodedMessage encoded(value);

  if (encoded.status() != ZX_OK) {
    return ::std::make_pair<zx_status_t, zx_status_t>(decoded.status(), encoded.status());
  }

  [[maybe_unused]] fidl_outgoing_msg_t* message = encoded.GetOutgoingMessage().message();
  // TODO: Verify re-encoded message matches initial message.
  return ::std::make_pair<zx_status_t, zx_status_t>(decoded.status(), encoded.status());
}
{{- end -}}
`
