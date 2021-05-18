// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package codegen

const fragmentClientAsyncMethodsTmpl = `
{{- define "AsyncClientAllocationComment" -}}
{{- $alloc := .Request.ClientAllocation }}
{{- if $alloc.IsStack -}}
Allocates {{ $alloc.Size }} bytes of request buffer on the stack. The callback is stored on the heap.
{{- else -}}
The request and callback are allocated on the heap.
{{- end }}
{{- end }}

{{- define "ClientAsyncRequestManagedMethodDefinition" }}
{{- IfdefFuchsia -}}

::fidl::Result {{ .Protocol.WireClientImpl.NoLeading }}::{{ .Name }}(
        {{ RenderParams .RequestArgs
                        (printf "::fit::callback<void (%s* response)> _cb" .WireResponse) }}) {
  class ResponseContext final : public {{ .WireResponseContext }} {
   public:
    ResponseContext(::fit::callback<void ({{ .WireResponse }}* response)> cb)
        : cb_(std::move(cb)) {}

    void OnReply({{ .WireResponse }}* response) override {
      cb_(response);
      delete this;
    }

    void OnError() override {
      delete this;
    }

   private:
    ::fit::callback<void ({{ .WireResponse }}* response)> cb_;
  };

  auto* _context = new ResponseContext(std::move(_cb));
  ::fidl::internal::ClientBase::PrepareAsyncTxn(_context);
  {{ .WireRequest }}::OwnedEncodedMessage _request(
    {{- RenderForwardParams "::fidl::internal::AllowUnownedInputRef{}" "_context->Txid()" .RequestArgs -}}
  );
  return _request.GetOutgoingMessage().Write(this, _context);
}

::fidl::Result {{ .Protocol.WireClientImpl.NoLeading }}::{{ .Name }}(
        {{- if .RequestArgs }}
          {{ RenderParams "::fidl::BufferSpan _request_buffer"
                          .RequestArgs
                          (printf "%s* _context" .WireResponseContext) }}
        {{- else }}
          {{ .WireResponseContext }}* _context
        {{- end -}}
    ) {
  ::fidl::internal::ClientBase::PrepareAsyncTxn(_context);
  {{ if .RequestArgs }}
    {{ .WireRequest }}::UnownedEncodedMessage _request(
      {{ RenderForwardParams "_request_buffer.data"
                             "_request_buffer.capacity"
                             "_context->Txid()"
                             .RequestArgs }});
  {{- else }}
    {{ .WireRequest }}::OwnedEncodedMessage _request(
      {{ RenderForwardParams "::fidl::internal::AllowUnownedInputRef{}" "_context->Txid()" .RequestArgs }});
  {{- end }}
  return _request.GetOutgoingMessage().Write(this, _context);
}
{{- EndifFuchsia -}}
{{- end }}
`
