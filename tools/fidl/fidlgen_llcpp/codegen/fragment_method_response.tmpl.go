// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package codegen

// fragmentMethodResponseTmpl contains the definition for
// fidl::WireResponse<Method>.
const fragmentMethodResponseTmpl = `
{{- define "MethodResponseDeclaration" }}
{{- EnsureNamespace "" }}
{{- if .Response.IsResource }}
{{- IfdefFuchsia -}}
{{- end }}
template<>
struct {{ .WireResponse }} final {
  FIDL_ALIGNDECL
    {{- /* Add underscore to prevent name collision */}}
  fidl_message_header_t _hdr;
    {{- range $index, $param := .ResponseArgs }}
  {{ $param.Type }} {{ $param.Name }};
    {{- end }}

  {{- if .ResponseArgs }}
  explicit {{ .WireResponse.Self }}({{ RenderCalleeParams .ResponseArgs }})
  {{ RenderInitMessage .ResponseArgs }} {
  _InitHeader();
  }
  {{- end }}
  {{ .WireResponse.Self }}() {
  _InitHeader();
  }

  static constexpr const fidl_type_t* Type =
  {{- if .ResponseArgs }}
  &{{ .Response.WireCodingTable }};
  {{- else }}
  &::fidl::_llcpp_coding_AnyZeroArgMessageTable;
  {{- end }}
  static constexpr uint32_t MaxNumHandles = {{ .Response.MaxHandles }};
  static constexpr uint32_t PrimarySize = {{ .Response.InlineSize }};
  static constexpr uint32_t MaxOutOfLine = {{ .Response.MaxOutOfLine }};
  static constexpr bool HasFlexibleEnvelope = {{ .Response.HasFlexibleEnvelope }};
  static constexpr bool HasPointer = {{ .Response.HasPointer }};
  static constexpr ::fidl::internal::TransactionalMessageKind MessageKind =
    ::fidl::internal::TransactionalMessageKind::kResponse;

  {{- if .Response.IsResource }}
  void _CloseHandles();
  {{- end }}

  class UnownedEncodedMessage final {
   public:
  UnownedEncodedMessage({{- RenderCalleeParams "uint8_t* _backing_buffer" "uint32_t _backing_buffer_size" .ResponseArgs }})
    : message_(::fidl::OutgoingMessage::ConstructorArgs{
        .iovecs = iovecs_,
        .iovec_capacity = ::fidl::internal::IovecBufferSize,
  {{- if gt .Response.MaxHandles 0 }}
        .handles = handles_,
        .handle_capacity = std::min(ZX_CHANNEL_MAX_MSG_HANDLES, MaxNumHandles),
  {{- end }}
        .backing_buffer = _backing_buffer,
        .backing_buffer_capacity = _backing_buffer_size,
      }) {
    FIDL_ALIGNDECL {{ .WireResponse.Self }} _response{
    {{- RenderForwardParams .ResponseArgs -}}
    };
    message_.Encode<{{ .WireResponse }}>(&_response);
  }
  UnownedEncodedMessage(uint8_t* _backing_buffer, uint32_t _backing_buffer_size,
                        {{ .WireResponse.Self }}* response)
    : message_(::fidl::OutgoingMessage::ConstructorArgs{
        .iovecs = iovecs_,
        .iovec_capacity = ::fidl::internal::IovecBufferSize,
  {{- if gt .Response.MaxHandles 0 }}
        .handles = handles_,
        .handle_capacity = std::min(ZX_CHANNEL_MAX_MSG_HANDLES, MaxNumHandles),
  {{- end }}
        .backing_buffer = _backing_buffer,
        .backing_buffer_capacity = _backing_buffer_size,
      }) {
    message_.Encode<{{ .WireResponse }}>(response);
  }
  UnownedEncodedMessage(const UnownedEncodedMessage&) = delete;
  UnownedEncodedMessage(UnownedEncodedMessage&&) = delete;
  UnownedEncodedMessage* operator=(const UnownedEncodedMessage&) = delete;
  UnownedEncodedMessage* operator=(UnownedEncodedMessage&&) = delete;

  zx_status_t status() const { return message_.status(); }
{{- IfdefFuchsia -}}
  const char* status_string() const { return message_.status_string(); }
{{- EndifFuchsia -}}
  bool ok() const { return message_.status() == ZX_OK; }
  const char* error_message() const { return message_.error_message(); }

  ::fidl::OutgoingMessage& GetOutgoingMessage() { return message_; }

{{- IfdefFuchsia -}}
  template <typename ChannelLike>
  void Write(ChannelLike&& client) { message_.Write(std::forward<ChannelLike>(client)); }
{{- EndifFuchsia -}}

   private:
  ::fidl::internal::IovecBuffer iovecs_;
  {{- if gt .Response.MaxHandles 0 }}
    zx_handle_disposition_t handles_[std::min(ZX_CHANNEL_MAX_MSG_HANDLES, MaxNumHandles)];
  {{- end }}
  ::fidl::OutgoingMessage message_;
  };

  class OwnedEncodedMessage final {
   public:
  explicit OwnedEncodedMessage({{ RenderCalleeParams .ResponseArgs }})
    : message_({{ RenderForwardParams "backing_buffer_.data()" "backing_buffer_.size()" .ResponseArgs }}) {}
  explicit OwnedEncodedMessage({{ .WireResponse }}* response)
    : message_(backing_buffer_.data(), backing_buffer_.size(), response) {}
  OwnedEncodedMessage(const OwnedEncodedMessage&) = delete;
  OwnedEncodedMessage(OwnedEncodedMessage&&) = delete;
  OwnedEncodedMessage* operator=(const OwnedEncodedMessage&) = delete;
  OwnedEncodedMessage* operator=(OwnedEncodedMessage&&) = delete;

  zx_status_t status() const { return message_.status(); }
{{- IfdefFuchsia -}}
  const char* status_string() const { return message_.status_string(); }
{{- EndifFuchsia -}}
  bool ok() const { return message_.ok(); }
  const char* error_message() const { return message_.error_message(); }

  ::fidl::OutgoingMessage& GetOutgoingMessage() { return message_.GetOutgoingMessage(); }

{{- IfdefFuchsia -}}
  template <typename ChannelLike>
  void Write(ChannelLike&& client) { message_.Write(std::forward<ChannelLike>(client)); }
{{- EndifFuchsia -}}

   private:
  {{ .Response.ServerAllocation.BackingBufferType }} backing_buffer_;
  UnownedEncodedMessage message_;
  };

 public:
  class DecodedMessage final : public ::fidl::internal::DecodedMessageBase<{{ .WireResponse }}> {
   public:
    using DecodedMessageBase<{{ .WireResponse }}>::DecodedMessageBase;

    DecodedMessage(uint8_t* bytes, uint32_t byte_actual, zx_handle_info_t* handles = nullptr,
                   uint32_t handle_actual = 0)
        : DecodedMessageBase(
            ::fidl::IncomingMessage(bytes, byte_actual, handles, handle_actual)) {}

    {{- if .Response.IsResource }}
    ~DecodedMessage() {
      if (ok() && (PrimaryObject() != nullptr)) {
        PrimaryObject()->_CloseHandles();
      }
    }
    {{- end }}

    {{ .WireResponse }}* PrimaryObject() {
      ZX_DEBUG_ASSERT(ok());
      return reinterpret_cast<{{ .WireResponse }}*>(bytes());
    }

    // Release the ownership of the decoded message. That means that the handles won't be closed
    // When the object is destroyed.
    // After calling this method, the |DecodedMessage| object should not be used anymore.
    void ReleasePrimaryObject() { ResetBytes(); }
  };

 private:
  void _InitHeader();
};
{{- if .Response.IsResource }}
{{- EndifFuchsia -}}
{{- end }}
{{- end }}




{{- define "MethodResponseDefinition" }}
  {{- EnsureNamespace "" }}
{{- if .Response.IsResource }}
{{- IfdefFuchsia -}}
{{- end }}
  void {{ .WireResponse }}::_InitHeader() {
    fidl_init_txn_header(&_hdr, 0, {{ .OrdinalName }});
  }

  {{ if .Response.IsResource }}
    void {{ .WireResponse }}::_CloseHandles() {
      {{- range .ResponseArgs }}
        {{- CloseHandles . false false }}
      {{- end }}
    }
  {{- end }}
{{- if .Response.IsResource }}
{{- EndifFuchsia -}}
{{- end }}
{{- end }}
`
