# FIDL lexicon

This document defines general terms that have a specific meaning in a FIDL
context. To learn more about specific FIDL topics, refer to the [FIDL
traihead][trailhead]

## Member, field, variant {#member-terms}

A **member** of a declaration is an individual element belonging to a
declaration, i.e. a declaration is comprised of zero, one, or many members.

For instance, consider the `Mode` bits declaration:

```fidl
{%includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/misc.test.fidl" region_tag="mode" %}
```

Both `READ` and `WRITE` are members.

When referring to members of structs or tables, we can more specifically refer
to these members as **fields**.

When referring to members of a union, we can more specifically refer to these
members as **variants**.

For example, consider the `Command` union declaration:

```fidl
{%includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/misc.test.fidl" region_tag="command" %}
```

The two variants are `create_resource` and `release_resource`.

Furthermore, the **selected variant** of an instance of a union is the current
value held by the union at that moment.

## Tag, and ordinal {#union-terms}

The **tag** is the target language variant discriminator, i.e. the specific
construct in a target language that is used to indicate the selected variant of
a union. For example, consider the following TypeScript representation of the
`Command` union:

```typescript
enum CommandTag {
    Create,
    Release,
}

interface Command = {
    tag: CommandTag,
    data: CreateResource | ReleaseResource,
}
```

The tag of `Command` is `Command.tag` and has type `CommandTag`. The actual
values and type representing each variant of `Command` are up to the
implementation.

Note that some languages will not require a tag. For example, some languages use
pattern matching to branch on the variant of a union instead of having an
explicit tag value.

The **ordinal** is the on the wire variant discriminator, i.e. the value used to
indicate the variant of a union in the [FIDL wire format][wire-format]. The
ordinals are explicitly specified in the FIDL definition (in this example, 1 for
`create_resource` and 2 for `release_resource`).

## Encode {#encode}

Encoding refers to the process of serializing values from a target language into
the FIDL wire format.

For the C family of bindings (HLCPP, LLCPP), encode can have a more specific
meaning of taking bytes matching the layout of the FIDL wire format and patching
pointers and handles by replacing them with
`FIDL_ALLOC_PRESENT`/`FIDL_ALLOC_ABSENT` or
`FIDL_HANDLE_PRESENT`/`FIDL_HANDLE_ABSENT` in-place, moving handles into an
out-of-band handle table.

## Decode {#decode}

Decoding refers to the process of deserializing values from raw bytes in the
FIDL wire format into a value in a target language.

For the C family of bindings (HLCPP, LLCPP), decode can have a more specific
meaning of taking bytes matching the layout of the FIDL wire format and patching
pointers and handles by replacing `FIDL_ALLOC_PRESENT`/`FIDL_ALLOC_ABSENT` or
`FIDL_HANDLE_PRESENT`/`FIDL_HANDLE_ABSENT` with the "real" pointer/handle
values in-place, moving handles out of an out-of-band handle table.

## Validate {#validate}

Validation is the process of checking if constraints from the FIDL definition
are satisfied for a given value. Validation occurs both when encoding a value
before being sent, or when decoding a value after receiving it. Example
constraints are vector bounds, handle constraints, and the valid encoding of a
string as UTF-8.

When validation fails, the bindings surface the error to user code, either by
returning it directly or via an error callback.

## Result/error type {#result}

For methods with error types specified:

```fidl
DoWork() -> (Data result) error uint32
```

The **result type** refers to the entire message that would be received by a
server for this method, i.e. the union that consists of either a result of
`Data` or an error of `uint32`. The error type in this case is `uint32`, whereas
`Data` can be referred to as either the response type or the success type.

<!-- xrefs -->
[trailhead]: /docs/development/languages/fidl/README.md
[wire-format]: /docs/reference/fidl/language/wire-format
