# Message Buffer Message Queue (MBMQ) model for IPC

This is an outline of a set of primitives for IPC (interprocess
communication) proposed for use in Fuchsia.

## Overview of object types

The MBMQ model is based around four types of object:

*   Channel endpoint
*   MsgQueue: message queue
*   MBO: message buffer object
*   CMH: callee MBO holder

A channel endpoint is something that you can send messages to.  A
channel may be redirected to a MsgQueue so that when a message is sent
to the channel, the message is enqueued onto the MsgQueue.

A MsgQueue is a queue of messages, potentially from multiple sources
-- a MsgQueue may be set as the destination for multiple channels.

MBOs have multiple roles:

*   An MBO stores a message.  A message consists of an array of bytes
    (data) and an array of Zircon handles (object-capabilities).  This
    message can be a request or a reply.

*   MBOs are passed back and forth between caller and callee
    processes, which will read or write the MBO.  The caller writes a
    request message into the MBO, which the callee reads.  The callee
    replaces the request with a reply message, which the caller reads.
    Ownership of the MBO is transferred so that, at any given point in
    time, only the caller or the callee can read or write the MBO's
    contents.

*   An MBO acts as a reply path for a request, by which the reply is
    returned from a callee to a caller.  Each MBO may have an
    associated reply-path MsgQueue, which is the queue that the MBO
    will be enqueued on when a callee returns it via
    `zx_cmh_send_reply()`.  This means a callee does not have to
    specify a channel for returning a reply message on.

A CMH is a callee process's limited-access reference to an MBO.  A
callee process can use a CMH to read and write an MBO until it returns
ownership of the MBO back to the caller.  The callee's access to the
MBO is revoked when it sends the reply, so the caller can then reuse
the MBO with a different callee.

## Overview of the request-reply lifecycle

There are four possible states for an MBO, listed here in the order in
which they are typically used:

*   `owned_by_caller`: Owned by the caller: contents accessible through
    the MBO handle
*   `enqueued_as_request`: Enqueued on a MsgQueue (or channel) as a
    request
*   `owned_by_callee`: Owned by a callee via a CMH: contents accessible
    through the CMH handle
*   `enqueued_as_reply`: Enqueued on a MsgQueue as a reply

An MBO switches between these states as it is sent to a callee,
received, and sent back to the caller.

An MBO starts off in the `owned_by_caller` state.

To send a request, the caller process writes the request message into
the MBO using `zx_mbo_write()` and then sends the MBO on a channel
using `zx_channel_write_mbo()`.  This enqueues the MBO onto the
channel's associated MsgQueue and switches the MBO's state to
`enqueued_as_request`.  In that state, MBO's handle can no longer be
used to read or write the MBO, so the caller cannot modify the message
after it has been sent.

The callee process can read the MBO from its MsgQueue using
`zx_msgqueue_read()`, supplying a CMH object.  This removes the MBO
from the message queue, sets the CMH to point to the MBO, and sets the
MBO's state to `owned_by_callee`.  This state gives the callee the
ability to read and write the MBO's contents using the CMH handle.
The caller can read the request message out of the MBO by passing the
CMH handle to `zx_mbo_read()`.  The caller can write a reply message
into the MBO (overwriting its contents) by passing the CMH handle to
`zx_mbo_write()`.

Once the callee has written a reply into the MBO, it can send the
reply to the caller by passing the CMH handle to
`zx_cmh_send_reply()`.  This enqueues the MBO on its associated
MsgQueue, drops the CMH's reference to the MBO (putting the CMH back
in the "unused" state), and sets the MBO's state to
`enqueued_as_reply`.  The callee can now reuse this CMH object in
later calls to `zx_msgqueue_read()`.

The caller process can then read the MBO from its MsgQueue using
`zx_msgqueue_read()`.  The caller supplies a CMH object but in this
case the CMH is not used.  The `zx_msgqueue_read()` syscall removes
the MBO from the MsgQueue and sets the MBO's state back to
`owned_by_callee`.  The caller can use the key value returned by
`zx_msgqueue_read()` to determine which MBO was returned, if
necessary.  The caller can now read the reply message from the MBO
using `zx_mbo_read()`.

The cycle can now repeat.  The caller can now write a new request
message into the MBO and send it on a channel as above, potentially to
a different callee.

## Core operations

*   `zx_mbo_create() -> mbo`

    Creates a new MBO.  The MBO starts off in the `owned_by_caller` state.

*   `zx_mbo_read(mbo_or_cmh) -> (data, handles)`

    Reads an MBO.  This reads the entire contents of the MBO.
    (Operations for partial reads will be defined later.)  This takes
    either an MBO handle (in the `owned_by_caller` state) or a CMH
    handle (for a CMH pointing to an MBO in the `owned_by_callee`
    state).

    If the message is too large to fit into the buffer passed to the
    syscall, the syscall returns the size of the message.  This allows
    the process to allocate a larger buffer and call the syscall
    again.

*   `zx_mbo_write(mbo_or_cmh, data, handles)`

    Writes an MBO.  This replaces the MBO's existing contents.
    (Operations for partial/incremental writes will be defined later.)
    This takes either an MBO handle (in the `owned_by_caller` state)
    or a CMH handle (for a CMH pointing to an MBO in the
    `owned_by_callee` state).

*   `zx_channel_write_mbo(channel, mbo)`

    Sends an MBO as a request on a channel.  The MBO must be in the
    `owned_by_caller` state.  Its state will be changed to
    `enqueued_as_request`.

    If the channel has an associated destination MsgQueue, as set by
    `zx_channel_redirect()`, the MBO will be enqueued onto that
    MsgQueue and its `key` field will be set to the key value that was
    set by `zx_channel_redirect()`.  Otherwise, the MBO will be
    enqueued on the channel so that it is readable from the channel's
    opposite endpoint.

*   `zx_msgqueue_create() -> msgqueue`

    Creates a new MsgQueue.

*   `zx_msgqueue_read(msgqueue, cmh) -> key`

    Reads an MBO from a MsgQueue.  If the MsgQueue is empty, this
    first blocks until the MsgQueue is non-empty.

    This takes a CMH as an argument.  The CMH must not currently hold
    a reference to an MBO, otherwise an error is returned.

    This removes the first MBO from the message queue.  If the MBO was
    in the `enqueued_as_request` state, this sets the CMH to point to
    the MBO, and changes the MBO's state to `owned_by_callee`.

    If the MBO was in the `enqueued_as_reply` state, this changes the
    MBO's state to `owned_by_caller` and does not modify the CMH.

    This returns the `key` field from the MBO, which allows the
    process to determine which channel the message was sent on (for
    requests) or which request this is a reply to (for replies).

*   `zx_channel_redirect(channel_or_mbo, msgqueue, key)`

    Sets the associated destination MsgQueue for a channel or an MBO.

    For channels: When given channel endpoint 1 of a pair, this sets
    the associated MsgQueue for endpoint 2 of the pair so that calls
    to `zx_channel_write_mbo()` on endpoint 2 will enqueue messages
    onto the given MsgQueue with the given key value.  If the channel
    had any existing messages queued on it (previously written to
    endpoint 2 and currently readable from endpoint 1), they are moved
    onto the given MsgQueue.

    For MBOs: When given an MBO, this sets the associated MsgQueue for
    the MBO, onto which `zx_cmh_send_reply()` will enqueue the MBO
    with the given key value.  The MBO must be in the
    `owned_by_caller` state.

*   `zx_cmh_create() -> cmh`

    Creates a new CMH.  The CMH starts off not holding a reference to
    any MBO.

*   `zx_cmh_send_reply(cmh)`

    Returns a reply message to the caller.  The given CMH must have a
    reference to an MBO (which will be in the state
    `owned_by_callee`).  This operation drops the CMH's reference to
    the MBO, and enqueues the MBO onto its associated MsgQueue,
    setting the MBO's `key` field to its reply key.  The MBO's state
    is set to `enqueued_as_reply`.

*   `zx_object_wait_async_mbo(handle, mbo, signals, options)`

    This is a replacement for `zx_object_wait_async()`.  Like that
    syscall, it waits until one or more of the given signals is
    asserted on the object specified by `handle`.  The difference is
    that rather than returning the notification by sending a port
    packet to a port, the new syscall returns the notification as a
    reply on the given MBO.  The notification is returned as if by an
    invocation of `zx_cmd_send_reply()`, enqueuing the MBO onto its
    associated reply queue.

    This means that waiting for a signal on an object is like making a
    call to the object.

    The given MBO must be in the `owned_by_caller` state.

    Note that `zx_object_wait_async_mbo()` does not need to allocate
    memory.  We can ensure that every MBO preallocates enough memory
    for the bookkeeping for waiting for a signal.  In contrast,
    `zx_object_wait_async()` must allocate memory each time it is
    called.

    Note that while `zx_object_wait_async()` is commonly used for
    waiting for messages on channels in Fuchsia today, this is not
    necessary in the MBMQ model where messages (MBOs) are enqueued
    directly onto MsgQueues.

    We might need to define an equivalent to `zx_port_cancel()`.

Handle-closing: operations when all references or handles to an object
are dropped:

*   MBO: An MBO is freed when all references to it have been dropped.

    Closing all the handles to an MBO only causes the MBO to be freed
    if there are no other references to the MBO from a MsgQueue or a
    CMH.  If the handles to an MBO are closed while the MBO is
    enqueued as a request on a MsgQueue, the MBO remains in the
    MsgQueue, and it can still be read into a CMH and sent as a reply,
    but it will be freed when `zx_msgqueue_read()` returns the MBO to
    the `owned_by_caller` state.

    This means it is possible to do a "fire-and-forget" send with an
    MBO: that is, send the MBO as a request message, but close the MBO
    handle and ignore any replies.

*   Automatic replies: An MBO receives an automatic reply message if
    it was sent as a request but there is no way a callee could send a
    reply.  There are two cases for this:

    *   Closed CMH: If a CMH is closed while it holds a reference to
        an MBO, the system will send an automatic reply on the MBO.
        The system will replace the MBO's contents with a default
        reply message and send the MBO as a reply (as if
        `zx_cmh_send_reply()` was called).

    *   Closed MsgQueue: If all the handles to a MsgQueue are closed
        while its queue contains MBOs in the state
        `enqueued_as_request`, or if MBOs are enqueued onto the
        MsgQueue after all handles to the MsgQueue were closed, then
        the system will send automatic replies on those MBOs.  This
        does not apply to MBOs in the state `enqueued_as_reply`
        because these are already replies.

    This means that if a callee process crashes in the middle of
    processing a request from a caller, or before unqueuing the
    request, the caller will not be left waiting for a reply message
    indefinitely.

## State for each object type

This section gives a summary of the state that is stored by each of
the object types.

MBO:

*   Message contents.  This consists of two resizable arrays:
    *   An array of bytes (data).
    *   An array of Zircon handles.
*   `key`: 64-bit integer.  This is set when the MBO is enqueued onto
    a MsgQueue by either `zx_channel_write_mbo()` or
    `zx_cmh_send_reply()`.  Its value is returned by
    `zx_msgqueue_read()`.
*   `reply_queue`: This is the MsgQueue that `zx_cmh_send_reply()`
    will enqueue the MBO onto when it is sent as a reply.
*   `reply_key`: 64-bit integer.  `zx_cmh_send_reply()` will set the
    MBO's `key` field to this value when the MBO is sent as a reply.
*   State: one of the four MBO states listed above (`owned_by_caller`,
    `owned_by_callee`, `enqueued_as_request`, `enqueued_as_reply`).
    Note that in practice we do not need to distinguish between
    `enqueued_as_request` and `owned_by_callee`.  Operations on MBO
    handles need to check for `owned_by_caller`, whereas
    `zx_msgqueue_read()` needs to check for `enqueued_as_request`
    versus `enqueued_as_reply`.

CMH:

*   Reference to an MBO.  This reference may be null.  If the
    reference is non-null, the MBO is in the `owned_by_callee` state.

MsgQueue:

*   List of MBOs, all of which will be in the state
    `enqueued_as_request` or `enqueued_as_reply`.

Channel endpoint:

*   Reference to a MsgQueue.  This reference may be null.
*   `channel_key`: 64-bit integer.  When an MBO is sent through this
    channel endpoint, its `key` field will be set to this
    `channel_key` value.
*   List of MBOs, all of which will be in the state
    `enqueued_as_request`.  This will be empty if the endpoint has an
    associated MsgQueue.

## Combined send+wait operation

The core IPC operations described above can all be invoked via
separate syscall invocations.  In addition to those syscalls, we
provide a combined send+wait syscall that allows a specific sequence
of those core IPC operations to be done in a single syscall
invocation.  This allows a process to send an outgoing message and
then wait for an incoming message.

Using this combined syscall reduces the overhead associated with
syscall invocations that comes from entering and leaving kernel mode.
More importantly, it allows the kernel to optimise the cases where it
is possible to do a direct context switch to the receiver process.  If
the "send message" step wakes a thread, and if the
`zx_msgqueue_read()` step would block, the kernel can switch directly
to the thread that was woken.

Furthermore, if the message being sent fits into the buffer provided
by the receiver, the kernel can potentially copy the message directly
to the receiver's buffer without making an intermediate copy in the
MBO's buffer.  This is termed the "direct-copy optimisation".  (Note,
however, that this is not entirely straightforward to implement,
because the sender and receiver's address spaces will usually not be
mapped at the same time.)

### Definition

```c
struct zx_mbmq_multiop {
  // Inputs for write+send:
  bool is_req;           // true if sending a request, false if sending a reply
  zx_handle_t mbo;       // for zx_mbo_write() + zx_channel_write_mbo()/zx_cmh_send_reply()
  zx_handle_t channel;   // for zx_channel_write_mbo() (if is_req is true)

  // Inputs for wait+read:
  zx_handle_t msgqueue;  // for zx_msgqueue_read()
  zx_handle_t cmh;       // for zx_msgqueue_read()

  buffer_info buf;       // for zx_mbo_write() and zx_mbo_read()

  // Output:
  uint64_t key;          // from zx_msgqueue_read()
};

zx_status_t zx_mbmq_multiop(zx_mbmq_multiop* args);
```

`zx_mbmq_multiop()` does the following:

*   Do `zx_mbo_write()` to write the message specified by `buf` into
    the MBO specified by `mbo` (which may be an MBO handle or a CMH
    handle).
*   Send message:
    *   If `is_req` is true, do `zx_channel_write_mbo()` to send `mbo`
        on `channel`.
    *   If `is_req` is false, do `zx_cmh_send_reply()` on `mbo` to
        send the message as a reply.
*   Do `zx_msgqueue_read()` on `msgqueue` and `cmh`.  Returns the
    resulting key value in `key`.
*   Do `zx_mbo_read()` to read the message from the MBO that was
    unqueued by `zx_msgqueue_read()` into `buffer`.
    *   If the message was fully read into the buffer, the MBO is
        truncated (i.e. its copy of the message is dropped).  This is
        to allow the direct-copy optimisation.
    *   If the message that was unqueued was a request, this is
        equivalent to `zx_mbo_read()` on `cmh`.  Otherwise, if the
        unqueued message was a reply, then if userland were to do an
        equivalent `zx_mbo_read()` call it would involve looking up
        the MBO handle based on the `key` value.

## Properties of CMHs

CMHs have these useful properties:

*   **Acts as a reply capability:** A CMH acts as single-use,
    revokable capability for replying to a request.  When the reply is
    sent, the CMH's reference to the MBO is dropped, revoking the
    callee's ability to use it to modify the MBO or send the MBO as a
    reply again.

*   **Reusable:** A callee can reuse a CMH across multiple requests.
    This means we can avoid doing an allocation and deallocation for
    each request, and we can avoid modifying the handle table for each
    request.  (A CMH's ability to reply to a particular request is
    revoked when the reply is sent, but the CMH itself is not
    revoked.)

*   **Acts as a message handle:** A CMH acts as a handle to a request
    message.  This means that a large request can be read
    incrementally by doing multiple syscall invocations using that
    handle to read parts of the message.  (Note, however, that we have
    not defined the syscalls for doing that yet.)  This means that a
    careful callee can potentially accept arbitrarily large messages
    while avoiding being vulnerable to memory exhaustion DoS.

    In contrast, Zircon's current `zx_channel_read()` syscall requires
    that a message be read fully into memory (or be truncated).  This
    means that if the current 64k message size limit were removed,
    there would be no way for a message receiver to use
    `zx_channel_read()` to receive an arbitrarily large message
    without risking exhausting its own memory.

## "Fire-and-forget" requests: requests without replies

At the FIDL level, some request messages are "fire-and-forget": they
have no corresponding reply message.

In the MBMQ model, each request generally has an associated reply
message, but it may be an empty or automatic reply, and the caller may
choose to ignore it.

For fire-and-forget requests, a caller has a choice of whether it
recycles MBOs across requests or not:

*   Non-recycled MBOs: This is the simplest for a caller to do, so it
    is likely to be the common case.  The caller allocates a new MBO
    for each fire-and-forget message.  The caller closes the MBO
    handle after sending the MBO, without ever setting a `reply_queue`
    on the MBO.  The MBO will get freed automatically after the callee
    has received the message and dropped its reference to the MBO.

*   Recycled MBOs: A callee has the option of detecting when the
    callee has dropped its reference to the MBO.  It can exercise that
    option by setting a `reply_queue` on the MBO in order to receive a
    reply, just as with MBOs where non-trivial replies are expected.
    This may be useful for a resource-conscious caller, which can use
    this ability to recycle MBOs between requests or to implement flow
    control.

For fire-and-forget requests, a well-behaved callee should release the
MBO after it has read or processed the message by writing an empty
reply message into the MBO and calling `zx_cmh_send_reply()` (or,
equivalently, by closing the CMH handle).  Unfortunately that has the
problem of requiring an extra syscall invocation, so we might want to
introduce a way of releasing the callee's MBO reference implicitly
when the message is read.

Note that a badly-behaved callee could hold into the MBO indefinitely,
but that is not very different from behaving badly by never unqueuing
requests.

## Bidirectional channels versus shareable channels

Currently, Zircon channels are bidirectional.  FIDL uses one direction
for request messages and the other direction for reply and event
messages.

Using bidirectional channels this way has the disadvantage that it
causes channel endpoints to be *non-shareable*.  If the client
endpoint of a channel were to be shared between multiple processes (by
duplicating the handle), the processes would run into a problem when
attempting to send requests on that endpoint: There would be no way to
route the reply for a request back to the process that sent the
request.  If the processes attempted to read replies, they would race
to receive each other's replies from the same channel queue.  (This
problem is solved for requests sent using `zx_channel_call()`, but not
for other requests.)

As a result, Zircon disallows duplicating channel handles, to prevent
processes from getting into a situation where replies are misrouted in
that way.

The non-shareability of channels has some disadvantages:

*   Non-shareability complicates matters when we want to share a FIDL
    object between multiple processes.

    One option here is for a FIDL protocol to provide a `Clone` or
    `Duplicate` method for creating a new channel referencing the same
    FIDL object.  Examples of this are `fuchsia.io.Node/Clone`,
    `fuchsia.ldsvc.Loader/Clone` and
    `fuchsia.sysmem.BufferCollectionToken/Duplicate`.  This requires
    the server process to co-operate in making the reference
    shareable.  This approach is somewhat problematic in cases where
    we want strong protections against memory exhaustion DoS, because
    a `Clone` method must allocate data structures in the server
    process.

    Another option is to acquire multiple references to a FIDL object
    from the source that we get the object from, e.g. by doing
    multiple `Open` calls on a `fuchsia.io.Directory`.

    A further option is to create proxy channels that forward requests
    to one FIDL object.  This is clearly undesirable for performance
    reasons.

*   Non-shareability also complicates matters when we want to share a
    FIDL object between multiple threads or other entities in the same
    process.

    If channel handles were shareable, we could make duplicates of a
    channel handle and hand them off to different entities within a
    process, allowing the lifetime of each duplicate handle to be
    managed separately.  When channel handles are non-shareable, this
    can necessitate more complicated protocols for ownership and
    thread safety such as reference counting or locking.

In contrast, the MBMQ model allows channels to be made shareable,
because it provides per-request paths through which reply messages can
be sent.

At the same time, the MBMQ model is compatible with bidirectional
channels.

## Preserving message ordering across channels

The MBMQ model is able to preserve the ordering of messages sent on
different channels that are handled by the same server process.  This
means that, for example, if message M1 is sent on channel C1 and then
message M2 is sent on channel C2, the server process can ensure that
the messages are processed in the order they were sent.  The server
just has to ensure that C1 and C2 are redirected to the same MsgQueue,
which will preserve the message ordering within the MsgQueue.

In contrast, Zircon's current IPC primitives are not able to preserve
message ordering in this case, because channels C1 and C2 have
separate message queues.  As messages are enqueued onto those queues,
the information about their interleaving is lost.  Zircon currently
preserves message ordering only within a channel, not between
channels.

## Notes on terminology

### Role of the "key" values

In this document, the term "key" has essentially the same meaning as
in Zircon's current [`zx_object_wait_async()`][zx_object_wait_async]
and [`zx_port_wait()`][zx_port_wait] syscalls.

[zx_object_wait_async]: <https://fuchsia.googlesource.com/fuchsia/+/dc596b81547a0930c88945bff32c8094a361ba3c/docs/reference/syscalls/object_wait_async.md>
[zx_port_wait]: <https://fuchsia.googlesource.com/fuchsia/+/dc596b81547a0930c88945bff32c8094a361ba3c/docs/reference/syscalls/port_wait.md>

In the MBMQ model, a key value is used by a process to identify which
of its incoming channels a message came from, or which of its earlier
requests an incoming reply correponds to.  A process can use whatever
key values it wants when passing them to `zx_object_wait_async()` or
`zx_channel_redirect()`.  We expect that the typical usage will be for
a process to treat a key as being a pointer to some data structure in
its address space.

### "Caller" and "callee"

We are using the terms "caller" and "callee" to emphasise that these
roles are relative to a particular interaction.  The "caller" is the
process that sends a request and may later receive a reply.  The
"callee" is the process that receives a request and may send a reply.
We can't use the terms "sender" and "receiver" for these two roles
because the caller and callee may both send and receive messages.

An alternative pair of terms would be "client" and "server".  We are
avoiding those terms, for two reasons:

*   Firstly, a process that is a server in one interaction can be the
    client in another interaction.
*   Secondly, a server process may send callback messages to its
    clients (e.g. send-only request messages).  In such cases, we
    choose to say that the server remains a server but acts as a
    caller when sending a message to its clients.

The terms "caller" and "callee" are commonly used in the context of
programming languages and compilers where it is clear that a function
may be a callee in one case and a caller in another case.

## Acknowledgements

The concept of MBOs, with the MBO acting as both a reusable message
buffer and a return path, is due to Corey Tabaka.
