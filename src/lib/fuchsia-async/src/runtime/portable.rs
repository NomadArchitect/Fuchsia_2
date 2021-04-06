// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod task {
    use core::task::{Context, Poll};
    use std::future::Future;
    use std::pin::Pin;

    /// A handle to a task.
    ///
    /// A task can be polled for the output of the future it is executing. A
    /// dropped task will be cancelled after dropping. To immediately cancel a
    /// task, call the cancel() method. To run a task to completion without
    /// retaining the Task handle, call the detach() method.
    #[derive(Debug)]
    pub struct Task<T>(async_executor::Task<T>);

    impl<T: 'static> Task<T> {
        /// Poll the given future on a thread dedicated to blocking tasks.
        ///
        /// Blocking tasks should ideally be constrained to only blocking regions
        /// of code, such as the system call invocation that is being made that
        /// needs to avoid blocking the reactor. For such a use case, using
        /// blocking::unblock() directly may be more efficient.
        pub fn blocking(fut: impl Future<Output = T> + Send + 'static) -> Self
        where
            T: Send,
        {
            Self::spawn(super::executor::blocking(fut))
        }

        /// spawn a new `Send` task onto the executor.
        pub fn spawn(fut: impl Future<Output = T> + Send + 'static) -> Self
        where
            T: Send,
        {
            Self(super::executor::spawn(fut))
        }

        /// spawn a new non-`Send` task onto the single threaded executor.
        pub fn local<'a>(fut: impl Future<Output = T> + 'static) -> Self {
            Self(super::executor::local(fut))
        }

        /// detach the Task handle. The contained future will be polled until completion.
        pub fn detach(self) {
            self.0.detach()
        }

        /// cancel a task and wait for cancellation to complete.
        pub async fn cancel(self) -> Option<T> {
            self.0.cancel().await
        }
    }

    impl<T> Future for Task<T> {
        type Output = T;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            use futures_lite::FutureExt;
            self.0.poll(cx)
        }
    }
}

pub mod executor {
    use crate::runtime::WakeupTime;
    use easy_parallel::Parallel;
    use fuchsia_zircon_status as zx_status;
    use std::future::Future;

    /// A time relative to the executor's clock.
    pub use std::time::Instant as Time;

    impl WakeupTime for Time {
        fn into_time(self) -> Time {
            self
        }
    }

    pub(crate) fn blocking<T: Send + 'static>(
        fut: impl Future<Output = T> + Send + 'static,
    ) -> impl Future<Output = T> {
        blocking::unblock(|| LOCAL.with(|local| async_io::block_on(GLOBAL.run(local.run(fut)))))
    }

    pub(crate) fn spawn<T: 'static>(
        fut: impl Future<Output = T> + Send + 'static,
    ) -> async_executor::Task<T>
    where
        T: Send,
    {
        GLOBAL.spawn(fut)
    }

    pub(crate) fn local<T>(fut: impl Future<Output = T> + 'static) -> async_executor::Task<T>
    where
        T: 'static,
    {
        LOCAL.with(|local| local.spawn(fut))
    }

    thread_local! {
        static LOCAL: async_executor::LocalExecutor<'static> = async_executor::LocalExecutor::new();
    }

    static GLOBAL: async_executor::Executor<'_> = async_executor::Executor::new();

    /// An executor.
    /// Mostly API-compatible with the Fuchsia variant (without the run_until_stalled or
    /// fake time pieces).
    /// The current implementation of Executor does not isolate work
    /// (as the underlying executor is not yet capable of this).
    pub struct Executor;

    impl Executor {
        /// Create a new executor running with actual time.
        pub fn new() -> Result<Self, zx_status::Status> {
            Ok(Self {})
        }

        /// Run a single future to completion using multiple threads.
        // Takes `&mut self` to ensure that only one thread-manager is running at a time.
        pub fn run<T>(&mut self, main_future: impl Future<Output = T>, num_threads: usize) -> T {
            let (signal, shutdown) = async_channel::unbounded::<()>();

            let (_, res) = Parallel::new()
                .each(0..num_threads, |_| {
                    LOCAL.with(|local| {
                        let _ = async_io::block_on(local.run(GLOBAL.run(shutdown.recv())));
                    })
                })
                .finish(|| {
                    LOCAL.with(|local| {
                        async_io::block_on(local.run(GLOBAL.run(async {
                            let res = main_future.await;
                            drop(signal);
                            res
                        })))
                    })
                });
            res
        }

        /// Run a single future to completion on a single thread.
        // Takes `&mut self` to ensure that only one thread-manager is running at a time.
        pub fn run_singlethreaded<T>(&mut self, main_future: impl Future<Output = T>) -> T {
            LOCAL.with(|local| async_io::block_on(GLOBAL.run(local.run(main_future))))
        }
    }
}

pub mod timer {
    use crate::runtime::WakeupTime;
    use futures::prelude::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// An asynchronous timer.
    #[derive(Debug)]
    #[must_use = "futures do nothing unless polled"]
    pub struct Timer(async_io::Timer);

    impl Timer {
        /// Create a new timer scheduled to fire at `time`.
        pub fn new<WT>(time: WT) -> Self
        where
            WT: WakeupTime,
        {
            Timer(async_io::Timer::at(time.into_time()))
        }
    }

    impl Future for Timer {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.poll_unpin(cx).map(drop)
        }
    }
}
