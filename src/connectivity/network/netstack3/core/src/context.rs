// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Execution contexts.
//!
//! This module defines "context" traits, which allow code in this crate to be
//! written agnostic to their execution context.
//!
//! All of the code in this crate operates in terms of "events". When an event
//! occurs (for example, a packet is received, an application makes a request,
//! or a timer fires), a function is called to handle that event. In response to
//! that event, the code may wish to emit new events (for example, to send a
//! packet, to respond to an application request, or to install a new timer).
//! The traits in this module provide the ability to emit new events. For
//! example, if, in order to handle some event, we need the ability to install
//! new timers, then the function to handle that event would take a
//! [`TimerContext`] parameter, which it could use to install new timers.
//!
//! Structuring code this way allows us to write code which is agnostic to
//! execution context - a test mock or any number of possible "real-world"
//! implementations of these traits all appear as indistinguishable, opaque
//! trait implementations to our code.
//!
//! The benefits are deeper than this, though. Large units of code can be
//! subdivided into smaller units that view each other as "contexts". For
//! example, the ARP implementation in the [`crate::device::arp`] module defines
//! the [`ArpContext`] trait, which is an execution context for ARP operations.
//! It is implemented both by the test mocks in that module, and also by the
//! Ethernet device implementation in the [`crate::device::ethernet`] module.
//!
//! This subdivision of code into small units in turn enables modularity. If,
//! for example, the IP code sees transport layer protocols as execution
//! contexts, then customizing which transport layer protocols are supported is
//! just a matter of providing a different implementation of the transport layer
//! context traits (this isn't what we do today, but we may in the future).
//!
//! # Synchronized vs Non-Synchronized Contexts
//!
//! Since Netstack3 aspires to be multi-threaded in the future, some resources
//! need to be shared between threads, including resources which are accessed
//! via context traits. Sometimes, this resource sharing has implications for
//! how the context's behavior is exposed via its API, and thus has implications
//! for how the consuming code needs to interact with that API.
//!
//! For this reason, some modules require two different contexts - a
//! "synchronized" context and a "non-synchronized" context. Traits implementing
//! a synchronized context are named `FooSyncContext` while traits implementing
//! a non-synchronized context are named `FooContext`. Note that the
//! implementation of a non-synchronized context trait may still provide access
//! to shared resources - the distinction is simply that the consumer doesn't
//! need to be aware that that's what's happening under the hood. As a result,
//! this shared access is not assumed by the context trait itself. Its API is
//! designed just as it would be if exclusive access were assumed. For example,
//! even though a multi-threaded implementation of [`TimerContext`] would need
//! to synchronize on a shared set of timers, the `TimerContext` trait is
//! designed as though it were providing a vanilla, unsynchronized container.
//!
//! When synchronized contexts provide access to state, they do it via a
//! `with`-style API:
//!
//! ```rust
//! trait FooSyncContext<S> {
//!     fn with_state<O, F: FnOnce(&mut S) -> O>(&mut self, f: F) -> O;
//! }
//! ```
//!
//! This style is easy to implement when state is shared and mutated via
//! interior mutability (e.g., using mutexes), and is also easy to implement
//! when state is accessed exclusively (e.g., when writing a test mock). It also
//! makes it clear that a critical section is starting and ending, and thus
//! makes it clear to the programmer that they're performing a potentially
//! expensive operation, and hopefully encourages them to minimize the duration
//! of the critical section.
//!
//! Since the `with_xxx` method operates on `&mut self`, it prevents other
//! operations on the context from happening concurrently. This prevents
//! deadlocks which occur as a result of a single mutex being locked while it is
//! held by the locking thread - in other words, it prevents lock reentrance.
//!
//! [`ArpContext`]: crate::device::arp::ArpContext

use core::time::Duration;

use packet::{BufferMut, Serializer};
use rand::{CryptoRng, RngCore};

use crate::{BlanketCoreContext, Ctx, EventDispatcher, Instant, TimerId};

/// A context that provides access to a monotonic clock.
pub trait InstantContext {
    /// The type of an instant in time.
    ///
    /// All time is measured using `Instant`s, including scheduling timers
    /// through [`TimerContext`]. This type may represent some sort of
    /// real-world time (e.g., [`std::time::Instant`]), or may be mocked in
    /// testing using a fake clock.
    type Instant: Instant;

    /// Returns the current instant.
    ///
    /// `now` guarantees that two subsequent calls to `now` will return
    /// monotonically non-decreasing values.
    fn now(&self) -> Self::Instant;
}

// Temporary blanket impl until we switch over entirely to the traits defined in
// this module.
impl<D: EventDispatcher, C: BlanketCoreContext> InstantContext for Ctx<D, C> {
    type Instant = C::Instant;

    fn now(&self) -> Self::Instant {
        self.ctx.now()
    }
}

/// An [`InstantContext`] which stores a cached value for the current time.
///
/// `CachedInstantCtx`s are constructed via [`new_cached_instant_context`].
pub(crate) struct CachedInstantCtx<I>(I);

impl<I: Instant> InstantContext for CachedInstantCtx<I> {
    type Instant = I;
    fn now(&self) -> I {
        self.0.clone()
    }
}

/// Construct a new `CachedInstantCtx` from the current time.
///
/// This is a hack until we figure out a strategy for splitting context objects.
/// Currently, since most context methods take a `&mut self` argument, lifetimes
/// which don't need to conflict in principle - such as the lifetime of state
/// obtained mutably from [`StateContext`] and the lifetime required to call the
/// [`InstantContext::now`] method on the same object - do conflict, and thus
/// cannot overlap. Until we figure out an approach to deal with that problem,
/// this exists as a workaround.
pub(crate) fn new_cached_instant_context<I: InstantContext + ?Sized>(
    ctx: &I,
) -> CachedInstantCtx<I::Instant> {
    CachedInstantCtx(ctx.now())
}

/// A context that supports scheduling timers.
pub trait TimerContext<Id>: InstantContext {
    /// Schedule a timer to fire after some duration.
    ///
    /// `schedule_timer` schedules the given timer to be fired after `duration`
    /// has elapsed, overwriting any previous timer with the same ID.
    ///
    /// If there was previously a timer with that ID, return the time at which
    /// is was scheduled to fire.
    ///
    /// # Panics
    ///
    /// `schedule_timer` may panic if `duration` is large enough that
    /// `self.now() + duration` overflows.
    fn schedule_timer(&mut self, duration: Duration, id: Id) -> Option<Self::Instant> {
        self.schedule_timer_instant(self.now().checked_add(duration).unwrap(), id)
    }

    /// Schedule a timer to fire at some point in the future.
    ///
    /// `schedule_timer` schedules the given timer to be fired at `time`,
    /// overwriting any previous timer with the same ID.
    ///
    /// If there was previously a timer with that ID, return the time at which
    /// is was scheduled to fire.
    fn schedule_timer_instant(&mut self, time: Self::Instant, id: Id) -> Option<Self::Instant>;

    /// Cancel a timer.
    ///
    /// If a timer with the given ID exists, it is canceled and the instant at
    /// which it was scheduled to fire is returned.
    fn cancel_timer(&mut self, id: Id) -> Option<Self::Instant>;

    /// Cancel all timers which satisfy a predicate.
    ///
    /// `cancel_timers_with` calls `f` on each scheduled timer, and cancels any
    /// timer for which `f` returns true.
    fn cancel_timers_with<F: FnMut(&Id) -> bool>(&mut self, f: F);

    /// Get the instant a timer will fire, if one is scheduled.
    ///
    /// Returns the [`Instant`] a timer with ID `id` will be invoked. If no
    /// timer with the given ID exists, `scheduled_instant` will return `None`.
    fn scheduled_instant(&self, id: Id) -> Option<Self::Instant>;
}

impl<D: EventDispatcher, C: BlanketCoreContext> TimerContext<TimerId> for Ctx<D, C> {
    fn schedule_timer_instant(
        &mut self,
        time: Self::Instant,
        id: TimerId,
    ) -> Option<Self::Instant> {
        self.ctx.schedule_timer_instant(time, id)
    }

    fn cancel_timer(&mut self, id: TimerId) -> Option<Self::Instant> {
        self.ctx.cancel_timer(id)
    }

    fn cancel_timers_with<F: FnMut(&TimerId) -> bool>(&mut self, f: F) {
        self.ctx.cancel_timers_with(f)
    }

    fn scheduled_instant(&self, id: TimerId) -> Option<Self::Instant> {
        self.ctx.scheduled_instant(id)
    }
}

/// A handler for timer firing events.
///
/// A `TimerHandler` is a type capable of handling the event of a timer firing.
pub(crate) trait TimerHandler<Id> {
    /// Handle a timer firing.
    fn handle_timer(&mut self, id: Id);
}

// NOTE:
// - Code in this crate is required to only obtain random values through an
//   `RngContext`. This allows a deterministic RNG to be provided when useful
//   (for example, in tests).
// - The CSPRNG requirement exists so that random values produced within the
//   network stack are not predictable by outside observers. This helps prevent
//   certain kinds of fingerprinting and denial of service attacks.

/// A context that provides a random number generator (RNG).
pub trait RngContext {
    // TODO(joshlf): If the CSPRNG requirement becomes a performance problem,
    // introduce a second, non-cryptographically secure, RNG.

    /// The random number generator (RNG) provided by this `RngContext`.
    ///
    /// The provided RNG must be cryptographically secure, and users may rely on
    /// that property for their correctness and security.
    type Rng: RngCore + CryptoRng;

    /// Gets the random number generator (RNG).
    fn rng(&self) -> &Self::Rng;

    /// Gets the random number generator (RNG) mutably.
    fn rng_mut(&mut self) -> &mut Self::Rng;
}

/// A context that provides access to a random number generator (RNG) and a
/// state at the same time.
///
/// `RngStateContext<State, Id>` is more powerful than `C: RngContext +
/// StateContext<State, Id>` because the latter only allows accessing either the
/// RNG or the state at a time, but not both due to lifetime restrictions.
pub trait RngStateContext<State, Id = ()>:
    RngContext + DualStateContext<State, <Self as RngContext>::Rng, Id, ()>
{
    /// Gets the state and the random number generator (RNG).
    fn get_state_rng_with(&mut self, id: Id) -> (&mut State, &mut Self::Rng) {
        self.get_states_mut_with(id, ())
    }
}

impl<State, Id, C: RngContext + DualStateContext<State, <Self as RngContext>::Rng, Id, ()>>
    RngStateContext<State, Id> for C
{
}

/// An extension trait for [`RngStateContext`] where `Id = ()`.
pub trait RngStateContextExt<State>: RngStateContext<State> {
    /// Gets the state and the random number generator (RNG).
    fn get_state_rng(&mut self) -> (&mut State, &mut Self::Rng) {
        self.get_state_rng_with(())
    }
}

impl<State, C: RngStateContext<State>> RngStateContextExt<State> for C {}

// Temporary blanket impl until we switch over entirely to the traits defined in
// this module.
impl<D: EventDispatcher, C: BlanketCoreContext> RngContext for Ctx<D, C> {
    type Rng = C::Rng;

    fn rng(&self) -> &C::Rng {
        self.ctx.rng()
    }

    fn rng_mut(&mut self) -> &mut C::Rng {
        self.ctx.rng_mut()
    }
}

/// A context that provides access to state.
///
/// `StateContext` stores instances of `State` keyed by `Id`, and provides
/// getters for this state. If `Id` is `()`, then `StateContext` represents a
/// single instance of `State`.
pub trait StateContext<State, Id = ()> {
    /// Get the state immutably.
    ///
    /// # Panics
    ///
    /// `get_state_with` panics if `id` is not a valid identifier. (e.g., an
    /// out-of-bounds index, a reference to an object that has been removed from
    /// a map, etc).
    fn get_state_with(&self, id: Id) -> &State;

    /// Get the state mutably.
    ///
    /// # Panics
    ///
    /// `get_state_mut_with` panics if `id` is not a valid identifier. (e.g., an
    /// out-of-bounds index, a reference to an object that has been removed from
    /// a map, etc).
    fn get_state_mut_with(&mut self, id: Id) -> &mut State;

    // TODO(joshlf): Once equality `where` bounds are supported, use those
    // instead of these `where Self: StateContext<...>` bounds
    // (https://github.com/rust-lang/rust/issues/20041).

    /// Get the state immutably when the `Id` type is `()`.
    ///
    /// `x.get_state()` is shorthand for `x.get_state_with(())`.
    fn get_state(&self) -> &State
    where
        Self: StateContext<State>,
    {
        self.get_state_with(())
    }

    /// Get the state mutably when the `Id` type is `()`.
    ///
    /// `x.get_state_mut()` is shorthand for `x.get_state_mut_with(())`.
    fn get_state_mut(&mut self) -> &mut State
    where
        Self: StateContext<State>,
    {
        self.get_state_mut_with(())
    }
}

// NOTE(joshlf): I experimented with a generic `MultiStateContext` trait which
// could be invoked as `MultiStateContext<(T,)>`, `MultiStateContext<(T, U)>`,
// etc. It proved difficult to use in practice, as implementations often
// required a lot of boilerplate. See this issue for detail:
// https://users.rust-lang.org/t/why-doesnt-rust-know-the-concrete-type-in-this-trait-impl/39498.
// In practice, having a `DualStateContext` trait which only supports two state
// types results in a much simpler and easier to use API.

/// A context that provides access to two states at once.
///
/// Unlike [`StateContext`], `DualStateContext` provides access to two different
/// states at once. `C: DualStateContext<T, U>` is more powerful than `C:
/// StateContext<T> + StateContext<U>` because the latter only allows accessing
/// either `T` or `U` at a time, but not both due to lifetime restrictions.
pub trait DualStateContext<State0, State1, Id0 = (), Id1 = ()> {
    /// Gets the states immutably.
    ///
    /// # Panics
    ///
    /// `get_states_with` panics if `id0` or `id1` are not valid identifiers.
    /// (e.g., an out-of-bounds index, a reference to an object that has been
    /// removed from a map, etc).
    fn get_states_with(&self, id0: Id0, id1: Id1) -> (&State0, &State1);

    /// Gets the states mutably.
    ///
    /// # Panics
    ///
    /// `get_states_mut_with` panics if `id0` or `id1` are not valid
    /// identifiers. (e.g., an out-of-bounds index, a reference to an object
    /// that has been removed from a map, etc).
    fn get_states_mut_with(&mut self, id0: Id0, id1: Id1) -> (&mut State0, &mut State1);

    // TODO(joshlf): Once equality `where` bounds are supported, use those
    // instead of these `where Self: DualStateContext<...>` bounds
    // (https://github.com/rust-lang/rust/issues/20041).

    /// Get the first state (`State0`) immutably when the `Id1` type is `()`.
    ///
    /// `x.get_state_with(id)` is shorthand for `x.get_states_with(id, ()).0`.
    ///
    /// # Panics
    ///
    /// `get_state_with` panics if `id0` is not a valid identifier. (e.g., an
    /// out-of-bounds index, a reference to an object that has been removed from
    /// a map, etc).
    fn get_state_with<'a>(&'a self, id: Id0) -> &'a State0
    where
        Self: DualStateContext<State0, State1, Id0>,
        State1: 'a,
    {
        let (state0, _state1) = self.get_states_with(id, ());
        state0
    }

    /// Get the first state (`State0`) mutably when the `Id1` type is `()`.
    ///
    /// `x.get_state_mut_with(id)` is shorthand for `x.get_states_mut_with(id,
    /// ()).0`.
    ///
    /// # Panics
    ///
    /// `get_state_mut_with` panics if `id0` is not a valid identifier. (e.g.,
    /// an out-of-bounds index, a reference to an object that has been removed
    /// from a map, etc).
    fn get_state_mut_with<'a>(&'a mut self, id: Id0) -> &'a mut State0
    where
        Self: DualStateContext<State0, State1, Id0>,
        State1: 'a,
    {
        let (state0, _state1) = self.get_states_mut_with(id, ());
        state0
    }

    /// Get the states immutably when both ID types are `()`.
    ///
    /// `x.get_states()` is shorthand for `x.get_states_with((), ())`.
    fn get_states(&self) -> (&State0, &State1)
    where
        Self: DualStateContext<State0, State1>,
    {
        self.get_states_with((), ())
    }

    /// Get the state mutably when both ID types are `()`.
    ///
    /// `x.get_states_mut()` is shorthand for `x.get_states_mut_with((), ())`.
    fn get_states_mut(&mut self) -> (&mut State0, &mut State1)
    where
        Self: DualStateContext<State0, State1>,
    {
        self.get_states_mut_with((), ())
    }

    /// Get the first state (`State0`) immutably when both ID types are `()`.
    ///
    /// `x.get_first_state()` is shorthand for `x.get_states().0`.
    fn get_first_state<'a>(&'a self) -> &'a State0
    where
        Self: DualStateContext<State0, State1>,
        State1: 'a,
    {
        let (state0, _state1) = self.get_states_with((), ());
        state0
    }

    /// Get the first state (`State0`) mutably when both ID types are `()`.
    ///
    /// `x.get_first_state_mut()` is shorthand for `x.get_states_mut().0`.
    fn get_first_state_mut<'a>(&'a mut self) -> &'a mut State0
    where
        Self: DualStateContext<State0, State1>,
        State1: 'a,
    {
        let (state0, _state1) = self.get_states_mut_with((), ());
        state0
    }
}

/// A context for receiving frames.
pub trait RecvFrameContext<B: BufferMut, Meta> {
    /// Receive a frame.
    ///
    /// `receive_frame` receives a frame with the given metadata.
    fn receive_frame(&mut self, metadata: Meta, frame: B);
}

// TODO(joshlf): Rename `FrameContext` to `SendFrameContext`

/// A context for sending frames.
pub trait FrameContext<B: BufferMut, Meta> {
    // TODO(joshlf): Add an error type parameter or associated type once we need
    // different kinds of errors.

    /// Send a frame.
    ///
    /// `send_frame` sends a frame with the given metadata. The frame itself is
    /// passed as a [`Serializer`] which `send_frame` is responsible for
    /// serializing. If serialization fails for any reason, the original,
    /// unmodified `Serializer` is returned.
    ///
    /// [`Serializer`]: packet::Serializer
    fn send_frame<S: Serializer<Buffer = B>>(&mut self, metadata: Meta, frame: S) -> Result<(), S>;
}

/// A handler for frame events.
///
/// A `FrameHandler` is a type capable of handling the event of a frame being
/// received.
pub(crate) trait FrameHandler<Ctx, Meta, B> {
    /// Handle a frame being received.
    fn handle_frame(ctx: &mut Ctx, meta: Meta, buffer: B);
}

/// A context that stores performance counters.
///
/// `CounterContext` allows counters keyed by string names to be incremented for
/// testing and debugging purposes. It is assumed that, if a no-op
/// implementation of [`increment_counter`] is provided, then calls will be
/// optimized out entirely by the compiler.
pub trait CounterContext {
    /// Increment the counter with the given key.
    fn increment_counter(&self, key: &'static str);
}

// Temporary blanket impl until we switch over entirely to the traits defined in
// this module.
impl<D: EventDispatcher, C: BlanketCoreContext> CounterContext for Ctx<D, C> {
    // TODO(rheacock): This is tricky because it's used in test only macro
    // code so the compiler thinks `key` is unused. Remove this when this is
    // no longer a problem.
    #[allow(unused)]
    fn increment_counter(&self, key: &'static str) {
        increment_counter!(self, key);
    }
}

/// A context for emitting events.
///
/// `EventContext` encodes the common pattern for emitting atomic events of type
/// `T` from core. An implementation of `EventContext` must guarantee that
/// events are processed in the order they are emitted.
pub trait EventContext<T> {
    /// Handles `event`.
    fn on_event(&mut self, event: T);
}

/// Mock implementations of context traits.
///
/// Each trait `Xxx` has a mock called `DummyXxx`. `DummyXxx` implements `Xxx`,
/// and `impl<T> DummyXxx for T` where either `T: AsRef<DummyXxx>` or `T:
/// AsMut<DummyXxx>` or both (depending on the trait). This allows dummy
/// implementations to be composed easily - any container type need only provide
/// the appropriate `AsRef` and/or `AsMut` implementations, and the blanket impl
/// will take care of the rest.
#[cfg(test)]
pub(crate) mod testutil {
    use alloc::{
        boxed::Box,
        collections::{BinaryHeap, HashMap},
        format,
        string::String,
        vec,
        vec::Vec,
    };
    use core::{
        fmt::{self, Debug, Formatter},
        hash::Hash,
        iter::FromIterator,
        ops::{self, RangeBounds},
    };

    use assert_matches::assert_matches;
    use packet::Buf;
    use rand_xorshift::XorShiftRng;

    use super::*;
    use crate::{
        data_structures::ref_counted_hash_map::{RefCountedHashSet, RemoveResult},
        device::DeviceId,
        testutil::FakeCryptoRng,
        Instant,
    };

    /// A dummy implementation of `Instant` for use in testing.
    #[derive(Default, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
    pub struct DummyInstant {
        // A DummyInstant is just an offset from some arbitrary epoch.
        offset: Duration,
    }

    impl DummyInstant {
        pub(crate) const LATEST: DummyInstant = DummyInstant { offset: Duration::MAX };

        fn saturating_add(self, dur: Duration) -> DummyInstant {
            DummyInstant { offset: self.offset.saturating_add(dur) }
        }
    }

    impl From<Duration> for DummyInstant {
        fn from(offset: Duration) -> DummyInstant {
            DummyInstant { offset }
        }
    }

    impl Instant for DummyInstant {
        fn duration_since(&self, earlier: DummyInstant) -> Duration {
            self.offset.checked_sub(earlier.offset).unwrap()
        }

        fn checked_add(&self, duration: Duration) -> Option<DummyInstant> {
            self.offset.checked_add(duration).map(|offset| DummyInstant { offset })
        }

        fn checked_sub(&self, duration: Duration) -> Option<DummyInstant> {
            self.offset.checked_sub(duration).map(|offset| DummyInstant { offset })
        }
    }

    impl ops::Add<Duration> for DummyInstant {
        type Output = DummyInstant;

        fn add(self, dur: Duration) -> DummyInstant {
            DummyInstant { offset: self.offset + dur }
        }
    }

    impl ops::Sub<DummyInstant> for DummyInstant {
        type Output = Duration;

        fn sub(self, other: DummyInstant) -> Duration {
            self.offset - other.offset
        }
    }

    impl ops::Sub<Duration> for DummyInstant {
        type Output = DummyInstant;

        fn sub(self, dur: Duration) -> DummyInstant {
            DummyInstant { offset: self.offset - dur }
        }
    }

    impl Debug for DummyInstant {
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            write!(f, "{:?}", self.offset)
        }
    }

    /// A dummy [`InstantContext`] which stores the current time as a
    /// [`DummyInstant`].
    #[derive(Default)]
    pub struct DummyInstantCtx {
        time: DummyInstant,
    }

    impl DummyInstantCtx {
        /// Advance the current time by the given duration.
        pub(crate) fn sleep(&mut self, dur: Duration) {
            self.time.offset += dur;
        }
    }

    impl InstantContext for DummyInstantCtx {
        type Instant = DummyInstant;
        fn now(&self) -> DummyInstant {
            self.time
        }
    }

    impl<T: AsRef<DummyInstantCtx>> InstantContext for T {
        type Instant = DummyInstant;
        fn now(&self) -> DummyInstant {
            self.as_ref().now()
        }
    }

    /// Arbitrary data of type `D` attached to a `DummyInstant`.
    ///
    /// `InstantAndData` implements `Ord` and `Eq` to be used in a `BinaryHeap`
    /// and ordered by `DummyInstant`.
    #[derive(Clone, Debug)]
    pub(crate) struct InstantAndData<D>(pub(crate) DummyInstant, pub(crate) D);

    impl<D> InstantAndData<D> {
        pub(crate) fn new(time: DummyInstant, data: D) -> Self {
            Self(time, data)
        }
    }

    impl<D> Eq for InstantAndData<D> {}

    impl<D> PartialEq for InstantAndData<D> {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }

    impl<D> Ord for InstantAndData<D> {
        fn cmp(&self, other: &Self) -> core::cmp::Ordering {
            other.0.cmp(&self.0)
        }
    }

    impl<D> PartialOrd for InstantAndData<D> {
        fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    /// A dummy [`TimerContext`] which stores time as a [`DummyInstantCtx`].
    pub(crate) struct DummyTimerCtx<Id> {
        instant: DummyInstantCtx,
        timers: BinaryHeap<InstantAndData<Id>>,
    }

    impl<Id> Default for DummyTimerCtx<Id> {
        fn default() -> DummyTimerCtx<Id> {
            DummyTimerCtx { instant: DummyInstantCtx::default(), timers: BinaryHeap::default() }
        }
    }

    impl<Id> AsMut<DummyTimerCtx<Id>> for DummyTimerCtx<Id> {
        fn as_mut(&mut self) -> &mut DummyTimerCtx<Id> {
            self
        }
    }

    impl<Id: Clone> DummyTimerCtx<Id> {
        /// Get an ordered list of all currently-scheduled timers.
        pub(crate) fn timers(&self) -> Vec<(DummyInstant, Id)> {
            self.timers
                .clone()
                .into_sorted_vec()
                .into_iter()
                .map(|InstantAndData(i, id)| (i, id))
                .collect()
        }
    }

    pub(crate) trait DummyInstantRange: Debug {
        fn contains(&self, i: DummyInstant) -> bool;

        /// Converts `&self` to a type-erased trait object reference.
        ///
        /// This makes it more ergonomic to construct a `&dyn
        /// DummyInstantRange`, which is necessary in order to use different
        /// range types in a context in which a single concrete type is
        /// expected.
        fn as_dyn(&self) -> &dyn DummyInstantRange
        where
            Self: Sized,
        {
            self
        }
    }

    impl DummyInstantRange for DummyInstant {
        fn contains(&self, i: DummyInstant) -> bool {
            self == &i
        }
    }

    impl<B: RangeBounds<DummyInstant> + Debug> DummyInstantRange for B {
        fn contains(&self, i: DummyInstant) -> bool {
            RangeBounds::contains(self, &i)
        }
    }

    // This impl is necessary in order to allow passing different range types
    // to `assert_timers_installed` and friends in a single call.
    impl<'a> DummyInstantRange for &'a dyn DummyInstantRange {
        fn contains(&self, i: DummyInstant) -> bool {
            <dyn DummyInstantRange as DummyInstantRange>::contains(*self, i)
        }
    }

    impl<Id: Debug + Clone + Hash + Eq> DummyTimerCtx<Id> {
        /// Asserts that `self` contains exactly the timers in `timers`.
        ///
        /// Each timer must be present, and its deadline must fall into the
        /// specified range. Ranges may be specified either as a specific
        /// [`DummyInstant`] or as any [`RangeBounds<DummyInstant>`].
        ///
        /// # Panics
        ///
        /// Panics if `timers` contains the same ID more than once or if `self`
        /// does not contain exactly the timers in `timers`.
        ///
        /// [`RangeBounds<DummyInstant>`]: core::ops::RangeBounds
        #[track_caller]
        pub(crate) fn assert_timers_installed<
            R: DummyInstantRange,
            I: IntoIterator<Item = (Id, R)>,
        >(
            &self,
            timers: I,
        ) {
            self.assert_timers_installed_inner(timers, true);
        }

        /// Asserts that `self` contains at least the timers in `timers`.
        ///
        /// Like [`assert_timers_installed`], but only asserts that `timers` is
        /// a subset of the timers installed; other timers may be installed in
        /// addition to those in `timers`.
        #[track_caller]
        pub(crate) fn assert_some_timers_installed<
            R: DummyInstantRange,
            I: IntoIterator<Item = (Id, R)>,
        >(
            &self,
            timers: I,
        ) {
            self.assert_timers_installed_inner(timers, false);
        }

        /// Asserts that no timers are installed.
        ///
        /// # Panics
        ///
        /// Panics if any timers are installed.
        #[track_caller]
        pub(crate) fn assert_no_timers_installed(&self) {
            self.assert_timers_installed::<DummyInstant, _>([]);
        }

        #[track_caller]
        fn assert_timers_installed_inner<R: DummyInstantRange, I: IntoIterator<Item = (Id, R)>>(
            &self,
            timers: I,
            exact: bool,
        ) {
            let mut timers = timers.into_iter().fold(HashMap::new(), |mut timers, (id, range)| {
                assert_matches!(timers.insert(id, range), None);
                timers
            });

            enum Error<Id, R: DummyInstantRange> {
                ExpectedButMissing { id: Id, range: R },
                UnexpectedButPresent { id: Id, instant: DummyInstant },
                UnexpectedInstant { id: Id, range: R, instant: DummyInstant },
            }

            let mut errors = Vec::new();

            // Make sure that all installed timers were expected (present in
            // `timers`).
            for InstantAndData(instant, id) in self.timers.iter().cloned() {
                match timers.remove(&id) {
                    None => {
                        if exact {
                            errors.push(Error::UnexpectedButPresent { id, instant })
                        }
                    }
                    Some(range) => {
                        if !range.contains(instant) {
                            errors.push(Error::UnexpectedInstant { id, range, instant })
                        }
                    }
                }
            }

            // Make sure that all expected timers were already found in
            // `self.timers` (and removed from `timers`).
            errors
                .extend(timers.drain().map(|(id, range)| Error::ExpectedButMissing { id, range }));

            if errors.len() > 0 {
                let mut s = String::from("Unexpected timer contents:");
                for err in errors {
                    s += &match err {
                        Error::ExpectedButMissing { id, range } => {
                            format!("\n\tMissing timer {:?} with deadline {:?}", id, range)
                        }
                        Error::UnexpectedButPresent { id, instant } => {
                            format!("\n\tUnexpected timer {:?} with deadline {:?}", id, instant)
                        }
                        Error::UnexpectedInstant { id, range, instant } => format!(
                            "\n\tTimer {:?} has unexpected deadline {:?} (wanted {:?})",
                            id, instant, range
                        ),
                    };
                }
                panic!("{}", s);
            }
        }
    }

    impl<Id> AsRef<DummyInstantCtx> for DummyTimerCtx<Id> {
        fn as_ref(&self) -> &DummyInstantCtx {
            &self.instant
        }
    }

    impl<Id: PartialEq> DummyTimerCtx<Id> {
        // Just like `TimerContext::cancel_timer`, but takes a reference to `Id`
        // rather than a value. This allows us to implement
        // `schedule_timer_instant`, which needs to retain ownership of the
        // `Id`.
        fn cancel_timer_inner(&mut self, id: &Id) -> Option<DummyInstant> {
            let mut r: Option<DummyInstant> = None;
            // NOTE(brunodalbo): Cancelling timers can be made a faster than
            // this if we keep two data structures and require that `Id: Hash`.
            self.timers = self
                .timers
                .drain()
                .filter(|t| {
                    if &t.1 == id {
                        r = Some(t.0);
                        false
                    } else {
                        true
                    }
                })
                .collect::<Vec<_>>()
                .into();
            r
        }
    }

    impl<Id: PartialEq> TimerContext<Id> for DummyTimerCtx<Id> {
        fn schedule_timer_instant(&mut self, time: DummyInstant, id: Id) -> Option<DummyInstant> {
            let ret = self.cancel_timer_inner(&id);
            self.timers.push(InstantAndData::new(time, id));
            ret
        }

        fn cancel_timer(&mut self, id: Id) -> Option<DummyInstant> {
            self.cancel_timer_inner(&id)
        }

        fn cancel_timers_with<F: FnMut(&Id) -> bool>(&mut self, mut f: F) {
            self.timers = self.timers.drain().filter(|t| !f(&t.1)).collect::<Vec<_>>().into();
        }

        fn scheduled_instant(&self, id: Id) -> Option<DummyInstant> {
            self.timers.iter().find_map(|x| if x.1 == id { Some(x.0) } else { None })
        }
    }

    pub(crate) trait DummyTimerCtxExt<Id: Clone>: AsMut<DummyTimerCtx<Id>> + Sized {
        /// Triggers the next timer, if any, by calling `f` on it.
        ///
        /// `trigger_next_timer` triggers the next timer, if any, advances the
        /// internal clock to the timer's scheduled time, and returns its ID.
        fn trigger_next_timer<F: FnMut(&mut Self, Id)>(&mut self, mut f: F) -> Option<Id> {
            self.as_mut().timers.pop().map(|InstantAndData(t, id)| {
                self.as_mut().instant.time = t;
                f(self, id.clone());
                id
            })
        }

        /// Skips the current time forward until `instant`, triggering all
        /// timers until then, inclusive, by calling `f` on them.
        ///
        /// Returns the timers which were triggered.
        ///
        /// # Panics
        ///
        /// Panics if `instant` is in the past.
        fn trigger_timers_until_instant<F: FnMut(&mut Self, Id)>(
            &mut self,
            instant: DummyInstant,
            mut f: F,
        ) -> Vec<Id> {
            assert!(instant >= self.as_mut().now());
            let mut timers = Vec::new();

            while self
                .as_mut()
                .timers
                .peek()
                .map(|InstantAndData(i, _id)| i <= &instant)
                .unwrap_or(false)
            {
                timers.push(self.trigger_next_timer(&mut f).unwrap())
            }

            assert!(self.as_mut().now() <= instant);
            self.as_mut().instant.time = instant;

            timers
        }

        /// Skips the current time forward by `duration`, triggering all timers
        /// until then, inclusive, by calling `f` on them.
        ///
        /// Returns the timers which were triggered.
        fn trigger_timers_for<F: FnMut(&mut Self, Id)>(
            &mut self,
            duration: Duration,
            f: F,
        ) -> Vec<Id> {
            let instant = self.as_mut().now().saturating_add(duration);
            // We know the call to `self.trigger_timers_until_instant` will not
            // panic because we provide an instant that is greater than or equal
            // to the current time.
            self.trigger_timers_until_instant(instant, f)
        }

        /// Triggers timers and expects them to be the given timers.
        ///
        /// The number of timers to be triggered is taken to be the number of
        /// timers produced by `timers`. Timers may be triggered in any order.
        ///
        /// # Panics
        ///
        /// Panics under the following conditions:
        /// - Fewer timers could be triggered than expected
        /// - Timers were triggered that were not expected
        /// - Timers that were expected were not triggered
        #[track_caller]
        fn trigger_timers_and_expect_unordered<
            I: IntoIterator<Item = Id>,
            F: FnMut(&mut Self, Id),
        >(
            &mut self,
            timers: I,
            mut f: F,
        ) where
            Id: Debug + Hash + Eq,
        {
            let mut timers = RefCountedHashSet::from_iter(timers);

            for _ in 0..timers.len() {
                let id = self.trigger_next_timer(&mut f).expect("ran out of timers to trigger");
                match timers.remove(id.clone()) {
                    RemoveResult::Removed(()) | RemoveResult::StillPresent => {}
                    RemoveResult::NotPresent => panic!("triggered unexpected timer: {:?}", id),
                }
            }

            if timers.len() > 0 {
                let mut s = String::from("Expected timers did not trigger:");
                for (id, count) in timers.iter_counts() {
                    s += &format!("\n\t{count}x {id:?}");
                }
                panic!("{}", s);
            }
        }

        /// Triggers timers until `instant` and expects them to be the given
        /// timers.
        ///
        /// Like `trigger_timers_and_expect_unordered`, except that timers will
        /// only be triggered until `instant` (inclusive).
        fn trigger_timers_until_and_expect_unordered<
            I: IntoIterator<Item = Id>,
            F: FnMut(&mut Self, Id),
        >(
            &mut self,
            instant: DummyInstant,
            timers: I,
            f: F,
        ) where
            Id: Debug + Hash + Eq,
        {
            let mut timers = RefCountedHashSet::from_iter(timers);

            let triggered_timers = self.trigger_timers_until_instant(instant, f);

            for id in triggered_timers {
                match timers.remove(id.clone()) {
                    RemoveResult::Removed(()) | RemoveResult::StillPresent => {}
                    RemoveResult::NotPresent => panic!("triggered unexpected timer: {:?}", id),
                }
            }

            if timers.len() > 0 {
                let mut s = String::from("Expected timers did not trigger:");
                for (id, count) in timers.iter_counts() {
                    s += &format!("\n\t{count}x {id:?}");
                }
                panic!("{}", s);
            }
        }

        /// Triggers timers for `duration` and expects them to be the given
        /// timers.
        ///
        /// Like `trigger_timers_and_expect_unordered`, except that timers will
        /// only be triggered for `duration` (inclusive).
        fn trigger_timers_for_and_expect<I: IntoIterator<Item = Id>, F: FnMut(&mut Self, Id)>(
            &mut self,
            duration: Duration,
            timers: I,
            f: F,
        ) where
            Id: Debug + Hash + Eq,
        {
            let instant = self.as_mut().now().saturating_add(duration);
            self.trigger_timers_until_and_expect_unordered(instant, timers, f);
        }
    }

    impl<Id: Clone, T: AsMut<DummyTimerCtx<Id>>> DummyTimerCtxExt<Id> for T {}

    /// A dummy [`FrameContext`].
    pub struct DummyFrameCtx<Meta> {
        frames: Vec<(Meta, Vec<u8>)>,
        should_error_for_frame: Option<Box<dyn Fn(&Meta) -> bool>>,
    }

    impl<Meta> DummyFrameCtx<Meta> {
        /// Closure which can decide to cause an error to be thrown when
        /// handling a frame, based on the metadata.
        pub fn set_should_error_for_frame<F: Fn(&Meta) -> bool + 'static>(&mut self, f: F) {
            self.should_error_for_frame = Some(Box::new(f));
        }
    }

    impl<Meta> Default for DummyFrameCtx<Meta> {
        fn default() -> DummyFrameCtx<Meta> {
            DummyFrameCtx { frames: Vec::new(), should_error_for_frame: None }
        }
    }

    impl<Meta> DummyFrameCtx<Meta> {
        /// Get the frames sent so far.
        pub(crate) fn frames(&self) -> &[(Meta, Vec<u8>)] {
            self.frames.as_slice()
        }
    }

    impl<B: BufferMut, Meta> FrameContext<B, Meta> for DummyFrameCtx<Meta> {
        fn send_frame<S: Serializer<Buffer = B>>(
            &mut self,
            metadata: Meta,
            frame: S,
        ) -> Result<(), S> {
            if let Some(should_error_for_frame) = &self.should_error_for_frame {
                if should_error_for_frame(&metadata) {
                    return Err(frame);
                }
            }

            let buffer = frame.serialize_vec_outer().map_err(|(_err, s)| s)?;
            self.frames.push((metadata, buffer.as_ref().to_vec()));
            Ok(())
        }
    }

    /// A dummy [`EventContext`].
    pub struct DummyEventCtx<E: Debug> {
        events: Vec<E>,
    }

    impl<E, Event: Debug + From<E>> EventContext<E> for DummyEventCtx<Event> {
        fn on_event(&mut self, event: E) {
            self.events.push(event.into())
        }
    }

    impl<E: Debug> Drop for DummyEventCtx<E> {
        fn drop(&mut self) {
            assert!(
                self.events.is_empty(),
                "dropped context with unacknowledged events: {:?}",
                self.events
            );
        }
    }

    impl<E: Debug> Default for DummyEventCtx<E> {
        fn default() -> Self {
            Self { events: Default::default() }
        }
    }

    impl<E: Debug> DummyEventCtx<E> {
        pub fn take(&mut self) -> Vec<E> {
            core::mem::take(&mut self.events)
        }
    }

    /// A dummy [`CounterContext`].
    #[derive(Default)]
    pub struct DummyCounterCtx {
        counters: core::cell::RefCell<HashMap<&'static str, usize>>,
    }

    impl CounterContext for DummyCounterCtx {
        fn increment_counter(&self, key: &'static str) {
            let mut counters = self.counters.borrow_mut();
            let val = counters.entry(key).or_insert(0);
            *val += 1;
        }
    }

    impl<T: AsRef<DummyCounterCtx>> CounterContext for T {
        fn increment_counter(&self, key: &'static str) {
            self.as_ref().increment_counter(key);
        }
    }

    /// A wrapper for a [`DummyTimerCtx`] and some other state.
    ///
    /// `DummyCtx` pairs some arbitrary state, `S`, with a `DummyTimerCtx`, a
    /// `DummyFrameCtx`, and a `DummyCounterCtx`. It implements
    /// [`InstantContext`], [`TimerContext`], [`FrameContext`], and
    /// [`CounterContext`]. It also provides getters for `S`. If the type, `S`,
    /// is meant to implement some other trait, then the caller is advised to
    /// instead implement that trait for `DummyCtx<S, Id, Meta, Event>`. This
    /// allows for full test mocks to be written with a minimum of boilerplate
    /// code.
    pub(crate) struct DummyCtx<S, Id, Meta, Event: Debug> {
        state: S,
        timers: DummyTimerCtx<Id>,
        frames: DummyFrameCtx<Meta>,
        counters: DummyCounterCtx,
        rng: FakeCryptoRng<XorShiftRng>,
        events: DummyEventCtx<Event>,
    }

    impl<S: Default, Id, Meta, Event: Debug> Default for DummyCtx<S, Id, Meta, Event> {
        fn default() -> DummyCtx<S, Id, Meta, Event> {
            DummyCtx::with_state(S::default())
        }
    }

    impl<S, Id, Meta, Event: Debug> DummyNetworkContext for DummyCtx<S, Id, Meta, Event> {
        type TimerId = Id;
        type SendMeta = Meta;
    }

    impl<S, Id, Meta, Event: Debug> DummyCtx<S, Id, Meta, Event> {
        /// Constructs a `DummyCtx` with the given state and default
        /// `DummyTimerCtx`, `DummyFrameCtx`, and `DummyCounterCtx`.
        pub(crate) fn with_state(state: S) -> DummyCtx<S, Id, Meta, Event> {
            DummyCtx {
                state,
                timers: DummyTimerCtx::default(),
                frames: DummyFrameCtx::default(),
                counters: DummyCounterCtx::default(),
                rng: FakeCryptoRng::new_xorshift(0),
                events: DummyEventCtx::default(),
            }
        }

        /// Seed the testing RNG with a specific value.
        pub(crate) fn seed_rng(&mut self, seed: u128) {
            self.rng = FakeCryptoRng::new_xorshift(seed);
        }

        /// Move the clock forward by the given duration without firing any
        /// timers.
        ///
        /// If any timers are scheduled to fire in the given duration, future
        /// use of this `DummyCtx` may have surprising or buggy behavior.
        pub(crate) fn sleep_skip_timers(&mut self, duration: Duration) {
            self.timers.instant.sleep(duration);
        }

        /// Get an immutable reference to the inner state.
        ///
        /// This method is provided instead of an [`AsRef`] impl to avoid
        /// conflicting with user-provided implementations of `AsRef<T> for
        /// DummyCtx<S, Id, Meta, Event>` for other types, `T`. It is named
        /// `get_ref` instead of `as_ref` so that programmer doesn't need to
        /// specify which `as_ref` method is intended.
        pub(crate) fn get_ref(&self) -> &S {
            &self.state
        }

        /// Get a mutable reference to the inner state.
        ///
        /// `get_mut` is like `get_ref`, but it returns a mutable reference.
        pub(crate) fn get_mut(&mut self) -> &mut S {
            &mut self.state
        }

        /// Get the list of frames sent so far.
        pub(crate) fn frames(&self) -> &[(Meta, Vec<u8>)] {
            self.frames.frames()
        }

        /// Get the value of the named counter.
        pub(crate) fn get_counter(&self, ctr: &str) -> usize {
            self.counters.counters.borrow().get(ctr).cloned().unwrap_or(0)
        }

        pub(crate) fn timer_ctx(&self) -> &DummyTimerCtx<Id> {
            &self.timers
        }

        pub(crate) fn timer_ctx_mut(&mut self) -> &mut DummyTimerCtx<Id> {
            &mut self.timers
        }

        pub(crate) fn take_events(&mut self) -> Vec<Event> {
            self.events.take()
        }
    }

    impl<S, Id, Meta, Event: Debug> AsRef<DummyInstantCtx> for DummyCtx<S, Id, Meta, Event> {
        fn as_ref(&self) -> &DummyInstantCtx {
            self.timers.as_ref()
        }
    }

    impl<S, Id, Meta, Event: Debug> AsRef<DummyTimerCtx<Id>> for DummyCtx<S, Id, Meta, Event> {
        fn as_ref(&self) -> &DummyTimerCtx<Id> {
            &self.timers
        }
    }

    impl<S, Id, Meta, Event: Debug> AsMut<DummyTimerCtx<Id>> for DummyCtx<S, Id, Meta, Event> {
        fn as_mut(&mut self) -> &mut DummyTimerCtx<Id> {
            &mut self.timers
        }
    }

    impl<S, Id, Meta, Event: Debug> AsMut<DummyFrameCtx<Meta>> for DummyCtx<S, Id, Meta, Event> {
        fn as_mut(&mut self) -> &mut DummyFrameCtx<Meta> {
            &mut self.frames
        }
    }

    impl<S, Id, Meta, Event: Debug> AsRef<DummyCounterCtx> for DummyCtx<S, Id, Meta, Event> {
        fn as_ref(&self) -> &DummyCounterCtx {
            &self.counters
        }
    }

    impl<S, Id: Debug + PartialEq, Meta, Event: Debug> TimerContext<Id>
        for DummyCtx<S, Id, Meta, Event>
    {
        fn schedule_timer_instant(&mut self, time: DummyInstant, id: Id) -> Option<DummyInstant> {
            self.timers.schedule_timer_instant(time, id)
        }

        fn cancel_timer(&mut self, id: Id) -> Option<DummyInstant> {
            self.timers.cancel_timer(id)
        }

        fn cancel_timers_with<F: FnMut(&Id) -> bool>(&mut self, f: F) {
            self.timers.cancel_timers_with(f);
        }

        fn scheduled_instant(&self, id: Id) -> Option<DummyInstant> {
            self.timers.scheduled_instant(id)
        }
    }

    impl<S, Id, Meta, Event: Debug> EventContext<Event> for DummyCtx<S, Id, Meta, Event> {
        fn on_event(&mut self, event: Event) {
            self.events.on_event(event)
        }
    }

    impl<B: BufferMut, S, Id, Meta, Event: Debug> FrameContext<B, Meta>
        for DummyCtx<S, Id, Meta, Event>
    {
        fn send_frame<SS: Serializer<Buffer = B>>(
            &mut self,
            metadata: Meta,
            frame: SS,
        ) -> Result<(), SS> {
            self.frames.send_frame(metadata, frame)
        }
    }

    impl<S, Id, Meta, Event: Debug> RngContext for DummyCtx<S, Id, Meta, Event> {
        type Rng = FakeCryptoRng<XorShiftRng>;

        fn rng(&self) -> &Self::Rng {
            &self.rng
        }

        fn rng_mut(&mut self) -> &mut Self::Rng {
            &mut self.rng
        }
    }

    impl<S, Id, Meta, Event: Debug> DualStateContext<S, FakeCryptoRng<XorShiftRng>>
        for DummyCtx<S, Id, Meta, Event>
    {
        fn get_states_with(&self, _id0: (), _id1: ()) -> (&S, &FakeCryptoRng<XorShiftRng>) {
            (&self.state, &self.rng)
        }

        fn get_states_mut_with(
            &mut self,
            _id0: (),
            _id1: (),
        ) -> (&mut S, &mut FakeCryptoRng<XorShiftRng>) {
            (&mut self.state, &mut self.rng)
        }
    }

    #[derive(Debug)]
    pub(crate) struct PendingFrameData<CtxId, Meta> {
        pub(crate) dst_context: CtxId,
        pub(crate) meta: Meta,
        pub(crate) frame: Vec<u8>,
    }

    pub(crate) type PendingFrame<CtxId, Meta> = InstantAndData<PendingFrameData<CtxId, Meta>>;

    /// A dummy network, composed of many `DummyCtx`s.
    ///
    /// Provides a utility to have many contexts keyed by `CtxId` that can
    /// exchange frames.
    pub(crate) struct DummyNetwork<CtxId, RecvMeta, Ctx: DummyNetworkContext, Links>
    where
        Links: DummyNetworkLinks<Ctx::SendMeta, RecvMeta, CtxId>,
    {
        contexts: HashMap<CtxId, Ctx>,
        current_time: DummyInstant,
        pending_frames: BinaryHeap<PendingFrame<CtxId, RecvMeta>>,
        links: Links,
    }

    /// A context which can be used with a [`DummyNetwork`].
    pub(crate) trait DummyNetworkContext {
        /// The type of timer IDs installed by this context.
        type TimerId;
        /// The type of metadata associated with frames sent by this context.
        type SendMeta;
    }

    /// A set of links in a `DummyNetwork`.
    ///
    /// A `DummyNetworkLinks` represents the set of links in a `DummyNetwork`.
    /// It exposes the link information by providing the ability to map from a
    /// frame's sending metadata - including its context, local state, and
    /// `SendMeta` - to the set of appropriate receivers, each represented by
    /// a context ID, receive metadata, and latency.
    pub(crate) trait DummyNetworkLinks<SendMeta, RecvMeta, CtxId> {
        fn map_link(&self, ctx: CtxId, meta: SendMeta) -> Vec<(CtxId, RecvMeta, Option<Duration>)>;
    }

    impl<
            SendMeta,
            RecvMeta,
            CtxId,
            F: Fn(CtxId, SendMeta) -> Vec<(CtxId, RecvMeta, Option<Duration>)>,
        > DummyNetworkLinks<SendMeta, RecvMeta, CtxId> for F
    {
        fn map_link(&self, ctx: CtxId, meta: SendMeta) -> Vec<(CtxId, RecvMeta, Option<Duration>)> {
            (self)(ctx, meta)
        }
    }

    /// The result of a single step in a `DummyNetwork`
    #[derive(Debug)]
    pub(crate) struct StepResult {
        pub(crate) timers_fired: usize,
        pub(crate) frames_sent: usize,
    }

    impl StepResult {
        fn new(timers_fired: usize, frames_sent: usize) -> Self {
            Self { timers_fired, frames_sent }
        }

        fn new_idle() -> Self {
            Self::new(0, 0)
        }

        /// Returns `true` if the last step did not perform any operations.
        pub(crate) fn is_idle(&self) -> bool {
            return self.timers_fired == 0 && self.frames_sent == 0;
        }
    }

    /// Error type that marks that one of the `run_until` family of functions
    /// reached a maximum number of iterations.
    #[derive(Debug)]
    pub(crate) struct LoopLimitReachedError;

    impl<CtxId, RecvMeta, Ctx, Links> DummyNetwork<CtxId, RecvMeta, Ctx, Links>
    where
        CtxId: Eq + Hash + Copy + Debug,
        Ctx: DummyNetworkContext
            + AsRef<DummyTimerCtx<Ctx::TimerId>>
            + AsMut<DummyTimerCtx<Ctx::TimerId>>
            + AsMut<DummyFrameCtx<Ctx::SendMeta>>,
        Ctx::TimerId: Copy,
        Links: DummyNetworkLinks<Ctx::SendMeta, RecvMeta, CtxId>,
    {
        /// Creates a new `DummyNetwork`.
        ///
        /// Creates a new `DummyNetwork` with the collection of `DummyCtx`s in
        /// `contexts`. `Ctx`s are named by type parameter `CtxId`.
        ///
        /// # Panics
        ///
        /// Calls to `new` will panic if given a `DummyCtx` with timer events.
        /// `DummyCtx`s given to `DummyNetwork` **must not** have any timer
        /// events already attached to them, because `DummyNetwork` maintains
        /// all the internal timers in dispatchers in sync to enable synchronous
        /// simulation steps.
        pub(crate) fn new<I: IntoIterator<Item = (CtxId, Ctx)>>(contexts: I, links: Links) -> Self {
            let mut contexts = contexts.into_iter().collect::<HashMap<_, _>>();
            // Take the current time to be the latest of the times of any of the
            // contexts. This ensures that no context has state which is based
            // on having observed a time in the future, which could cause bugs.
            // For any contexts which have a time further in the past, it will
            // appear as though time has jumped forwards, but that's fine. The
            // only way that this could be a problem would be if a timer were
            // installed which should have fired in the interim (code might
            // become buggy in this case). However, we assert below that no
            // timers are installed.
            let latest_time = contexts
                .iter()
                .map(|(_, ctx)| ctx.as_ref().instant.time)
                .max()
                // If `max` returns `None`, it means that we were called with no
                // contexts. That's kind of silly, but whatever - arbitrarily
                // choose the current time as the epoch.
                .unwrap_or(DummyInstant::default());

            assert!(
                !contexts.iter().any(|(_, ctx)| { !ctx.as_ref().timers.is_empty() }),
                "can't start network with contexts that already have timers set"
            );

            // Synchronize all contexts' current time to the latest time of any
            // of the contexts. See comment above for more details.
            for (_, ctx) in contexts.iter_mut() {
                AsMut::<DummyTimerCtx<_>>::as_mut(ctx).instant.time = latest_time;
            }

            Self { contexts, current_time: latest_time, pending_frames: BinaryHeap::new(), links }
        }

        /// Retrieves a `DummyCtx` named `context`.
        pub(crate) fn context<K: Into<CtxId>>(&mut self, context: K) -> &mut Ctx {
            self.contexts.get_mut(&context.into()).unwrap()
        }

        /// Iterates over pending frames in an arbitrary order.
        pub(crate) fn iter_pending_frames(
            &self,
        ) -> impl Iterator<Item = &PendingFrame<CtxId, RecvMeta>> {
            self.pending_frames.iter()
        }

        /// Drops all pending frames; they will not be delivered.
        pub(crate) fn drop_pending_frames(&mut self) {
            self.pending_frames.clear();
        }

        /// Performs a single step in network simulation.
        ///
        /// `step` performs a single logical step in the collection of `Ctx`s
        /// held by this `DummyNetwork`. A single step consists of the following
        /// operations:
        ///
        /// - All pending frames, kept in each `DummyCtx`, are mapped to their
        ///   destination context/device pairs and moved to an internal
        ///   collection of pending frames.
        /// - The collection of pending timers and scheduled frames is inspected
        ///   and a simulation time step is retrieved, which will cause a next
        ///   event to trigger. The simulation time is updated to the new time.
        /// - All scheduled frames whose deadline is less than or equal to the
        ///   new simulation time are sent to their destinations, handled using
        ///   `handle_frame`.
        /// - All timer events whose deadline is less than or equal to the new
        ///   simulation time are fired, handled using `handle_timer`.
        ///
        /// If any new events are created during the operation of frames or
        /// timers, they **will not** be taken into account in the current
        /// `step`. That is, `step` collects all the pending events before
        /// dispatching them, ensuring that an infinite loop can't be created as
        /// a side effect of calling `step`.
        ///
        /// The return value of `step` indicates which of the operations were
        /// performed.
        ///
        /// # Panics
        ///
        /// If `DummyNetwork` was set up with a bad `links`, calls to `step` may
        /// panic when trying to route frames to their context/device
        /// destinations.
        pub(crate) fn step<
            FH: FnMut(&mut Ctx, RecvMeta, Buf<Vec<u8>>),
            FT: FnMut(&mut Ctx, Ctx::TimerId),
        >(
            &mut self,
            mut handle_frame: FH,
            mut handle_timer: FT,
        ) -> StepResult
        where
            Ctx::TimerId: core::fmt::Debug,
        {
            self.collect_frames();

            let next_step = if let Some(t) = self.next_step() {
                t
            } else {
                return StepResult::new_idle();
            };

            // This assertion holds the contract that `next_step` does not
            // return a time in the past.
            assert!(next_step >= self.current_time);
            let mut ret = StepResult::new(0, 0);
            // Move time forward:
            self.current_time = next_step;
            for (_, ctx) in self.contexts.iter_mut() {
                AsMut::<DummyTimerCtx<_>>::as_mut(ctx).instant.time = next_step;
            }

            // Dispatch all pending frames:
            while let Some(InstantAndData(t, _)) = self.pending_frames.peek() {
                // TODO(https://github.com/rust-lang/rust/issues/53667): Remove
                // this break once let_chains is stable.
                if *t > self.current_time {
                    break;
                }
                // We can unwrap because we just peeked.
                let frame = self.pending_frames.pop().unwrap().1;
                handle_frame(
                    self.context(frame.dst_context),
                    frame.meta,
                    Buf::new(frame.frame, ..),
                );
                ret.frames_sent += 1;
            }

            // Dispatch all pending timers.
            for (_, ctx) in self.contexts.iter_mut() {
                // We have to collect the timers before dispatching them, to
                // avoid an infinite loop in case handle_timer schedules another
                // timer for the same or older DummyInstant.
                let mut timers = Vec::<Ctx::TimerId>::new();
                while let Some(InstantAndData(t, id)) = ctx.as_ref().timers.peek() {
                    // TODO(https://github.com/rust-lang/rust/issues/53667):
                    // Remove this break once let_chains is stable.
                    if *t > ctx.as_ref().now() {
                        break;
                    }
                    timers.push(*id);
                    assert_ne!(AsMut::<DummyTimerCtx<_>>::as_mut(ctx).timers.pop(), None);
                }

                for t in timers {
                    handle_timer(ctx, t);
                    ret.timers_fired += 1;
                }
            }

            ret
        }

        /// Runs the network until it is starved of events.
        ///
        /// # Panics
        ///
        /// Panics if 1,000,000 steps are performed without becoming idle.
        /// Also panics under the same conditions as [`step`].
        pub(crate) fn run_until_idle<
            FH: FnMut(&mut Ctx, RecvMeta, Buf<Vec<u8>>) + Copy,
            FT: FnMut(&mut Ctx, Ctx::TimerId) + Copy,
        >(
            &mut self,
            handle_frame: FH,
            handle_timer: FT,
        ) where
            Ctx::TimerId: core::fmt::Debug,
        {
            for _ in 0..1_000_000 {
                if self.step(handle_frame, handle_timer).is_idle() {
                    return;
                }
            }
            panic!("DummyNetwork seems to have gotten stuck in a loop.");
        }

        /// Collects all queued frames.
        ///
        /// Collects all pending frames and schedules them for delivery to the
        /// destination context/device based on the result of `links`. The
        /// collected frames are queued for dispatching in the `DummyNetwork`,
        /// ordered by their scheduled delivery time given by the latency result
        /// provided by `links`.
        pub(crate) fn collect_frames(&mut self) {
            let all_frames: Vec<(CtxId, Vec<(Ctx::SendMeta, Vec<u8>)>)> = self
                .contexts
                .iter_mut()
                .filter_map(|(n, ctx)| {
                    let ctx: &mut DummyFrameCtx<_> = ctx.as_mut();
                    if ctx.frames.is_empty() {
                        None
                    } else {
                        Some((n.clone(), ctx.frames.drain(..).collect()))
                    }
                })
                .collect();

            for (src_context, frames) in all_frames.into_iter() {
                for (send_meta, frame) in frames.into_iter() {
                    for (dst_context, recv_meta, latency) in
                        self.links.map_link(src_context, send_meta)
                    {
                        self.pending_frames.push(PendingFrame::new(
                            self.current_time + latency.unwrap_or(Duration::from_millis(0)),
                            PendingFrameData { frame: frame.clone(), dst_context, meta: recv_meta },
                        ));
                    }
                }
            }
        }

        /// Calculates the next `DummyInstant` when events are available.
        ///
        /// Returns the smallest `DummyInstant` greater than or equal to the
        /// current time for which an event is available. If no events are
        /// available, returns `None`.
        fn next_step(&self) -> Option<DummyInstant> {
            // Get earliest timer in all contexts.
            let next_timer = self
                .contexts
                .iter()
                .filter_map(|(_, ctx)| match ctx.as_ref().timers.peek() {
                    Some(tmr) => Some(tmr.0),
                    None => None,
                })
                .min();
            // Get the instant for the next packet.
            let next_packet_due = self.pending_frames.peek().map(|t| t.0);

            // Return the earliest of them both, and protect against returning a
            // time in the past.
            match next_timer {
                Some(t) if next_packet_due.is_some() => Some(t).min(next_packet_due),
                Some(t) => Some(t),
                None => next_packet_due,
            }
            .map(|t| t.max(self.current_time))
        }
    }

    /// Creates a new [`DummyNetwork`] of legacy [`Ctx`] contexts in a simple
    /// two-host configuration.
    ///
    /// Two hosts are created with the given names. Packets emitted by one
    /// arrive at the other and vice-versa.
    pub(crate) fn new_legacy_simple_dummy_network<CtxId: Copy + Debug + Hash + Eq>(
        a_id: CtxId,
        a: crate::testutil::DummyCtx,
        b_id: CtxId,
        b: crate::testutil::DummyCtx,
    ) -> DummyNetwork<
        CtxId,
        DeviceId,
        crate::testutil::DummyCtx,
        impl DummyNetworkLinks<DeviceId, DeviceId, CtxId>,
    > {
        let contexts = vec![(a_id, a), (b_id, b)].into_iter();
        let device_id = DeviceId::new_ethernet(0);
        DummyNetwork::new(contexts, move |net, _device_id: DeviceId| {
            if net == a_id {
                vec![(b_id, device_id, None)]
            } else {
                vec![(a_id, device_id, None)]
            }
        })
    }

    mod tests {
        use super::*;

        const ONE_SEC: Duration = Duration::from_secs(1);
        const ONE_SEC_INSTANT: DummyInstant = DummyInstant { offset: ONE_SEC };

        #[test]
        fn test_instant_and_data() {
            // Verify implementation of InstantAndData to be used as a complex
            // type in a BinaryHeap.
            let mut heap = BinaryHeap::<InstantAndData<usize>>::new();
            let now = DummyInstant::default();

            fn new_data(time: DummyInstant, id: usize) -> InstantAndData<usize> {
                InstantAndData::new(time, id)
            }

            heap.push(new_data(now + Duration::from_secs(1), 1));
            heap.push(new_data(now + Duration::from_secs(2), 2));

            // Earlier timer is popped first.
            assert_eq!(heap.pop().unwrap().1, 1);
            assert_eq!(heap.pop().unwrap().1, 2);
            assert_eq!(heap.pop(), None);

            heap.push(new_data(now + Duration::from_secs(1), 1));
            heap.push(new_data(now + Duration::from_secs(1), 1));

            // Can pop twice with identical data.
            assert_eq!(heap.pop().unwrap().1, 1);
            assert_eq!(heap.pop().unwrap().1, 1);
            assert_eq!(heap.pop(), None);
        }

        #[test]
        fn test_dummy_timer_context() {
            // An implementation of `TimerContext` that uses `usize` timer IDs
            // and stores every timer in a `Vec`.
            impl<M, E: Debug> TimerHandler<usize> for DummyCtx<Vec<(usize, DummyInstant)>, usize, M, E> {
                fn handle_timer(&mut self, id: usize) {
                    let now = self.now();
                    self.get_mut().push((id, now));
                }
            }

            let mut ctx = DummyCtx::<Vec<(usize, DummyInstant)>, usize, (), ()>::default();

            // When no timers are installed, `trigger_next_timer` should return
            // `false`.
            assert_eq!(ctx.trigger_next_timer(TimerHandler::handle_timer), None);
            assert_eq!(ctx.get_ref().as_slice(), []);

            // When one timer is installed, it should be triggered.
            ctx = Default::default();

            // No timer with id `0` exists yet.
            assert_eq!(ctx.scheduled_instant(0), None);

            assert_eq!(ctx.schedule_timer(ONE_SEC, 0), None);

            // Timer with id `0` scheduled to execute at `ONE_SEC_INSTANT`.
            assert_eq!(ctx.scheduled_instant(0).unwrap(), ONE_SEC_INSTANT);

            assert_eq!(ctx.trigger_next_timer(TimerHandler::handle_timer), Some(0));
            assert_eq!(ctx.get_ref().as_slice(), [(0, ONE_SEC_INSTANT)]);

            // After the timer fires, it should not still be scheduled at some
            // instant.
            assert_eq!(ctx.scheduled_instant(0), None);

            // The time should have been advanced.
            assert_eq!(ctx.now(), ONE_SEC_INSTANT);

            // Once it's been triggered, it should be canceled and not
            // triggerable again.
            ctx = Default::default();
            assert_eq!(ctx.trigger_next_timer(TimerHandler::handle_timer), None);
            assert_eq!(ctx.get_ref().as_slice(), []);

            // If we schedule a timer but then cancel it, it shouldn't fire.
            ctx = Default::default();
            assert_eq!(ctx.schedule_timer(ONE_SEC, 0), None);
            assert_eq!(ctx.cancel_timer(0), Some(ONE_SEC_INSTANT));
            assert_eq!(ctx.trigger_next_timer(TimerHandler::handle_timer), None);
            assert_eq!(ctx.get_ref().as_slice(), []);

            // If we schedule a timer but then schedule the same ID again, the
            // second timer should overwrite the first one.
            ctx = Default::default();
            assert_eq!(ctx.schedule_timer(Duration::from_secs(0), 0), None);
            assert_eq!(ctx.schedule_timer(ONE_SEC, 0), Some(Duration::from_secs(0).into()));
            assert_eq!(ctx.cancel_timer(0), Some(ONE_SEC_INSTANT));

            // If we schedule three timers and then run `trigger_timers_until`
            // with the appropriate value, only two of them should fire.
            ctx = Default::default();
            assert_eq!(ctx.schedule_timer(Duration::from_secs(0), 0), None,);
            assert_eq!(ctx.schedule_timer(Duration::from_secs(1), 1), None,);
            assert_eq!(ctx.schedule_timer(Duration::from_secs(2), 2), None,);
            assert_eq!(
                ctx.trigger_timers_until_instant(ONE_SEC_INSTANT, TimerHandler::handle_timer),
                vec![0, 1],
            );

            // The first two timers should have fired.
            assert_eq!(
                ctx.get_ref().as_slice(),
                [(0, DummyInstant::from(Duration::from_secs(0))), (1, ONE_SEC_INSTANT)]
            );

            // They should be canceled now.
            assert_eq!(ctx.cancel_timer(0), None);
            assert_eq!(ctx.cancel_timer(1), None);

            // The clock should have been updated.
            assert_eq!(ctx.now(), ONE_SEC_INSTANT);

            // The last timer should not have fired.
            assert_eq!(ctx.cancel_timer(2), Some(DummyInstant::from(Duration::from_secs(2))));
        }

        #[test]
        fn test_trigger_timers_until_and_expect_unordered() {
            // If the requested instant does not coincide with a timer trigger
            // point, the time should still be advanced.
            let mut ctx = DummyCtx::<Vec<(usize, DummyInstant)>, usize, (), ()>::default();
            assert_eq!(ctx.schedule_timer(Duration::from_secs(0), 0), None);
            assert_eq!(ctx.schedule_timer(Duration::from_secs(2), 1), None);
            ctx.trigger_timers_until_and_expect_unordered(
                ONE_SEC_INSTANT,
                vec![0],
                TimerHandler::handle_timer,
            );
            assert_eq!(ctx.now(), ONE_SEC_INSTANT);
        }
    }
}
