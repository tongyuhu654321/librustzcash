//! This is an interface for dealing with the kinds of
//! parallel computations involved in bellman. It's
//! currently just a thin wrapper around CpuPool and
//! crossbeam but may be extended in the future to
//! allow for various parallelism strategies.

#[cfg(feature = "multicore")]
mod implementation {
    use crossbeam::{self, Scope};
    use futures::{Future, IntoFuture, Poll};
    use futures_cpupool::{CpuFuture, CpuPool};
    use num_cpus;

    #[derive(Clone)]
    pub struct Worker {
        cpus: usize,
        pool: CpuPool,
    }

    impl Worker {
        // We don't expose this outside the library so that
        // all `Worker` instances have the same number of
        // CPUs configured.
        pub(crate) fn new_with_cpus(cpus: usize) -> Worker {
            Worker {
                cpus: cpus,
                pool: CpuPool::new(cpus),
            }
        }

        pub fn new() -> Worker {
            Self::new_with_cpus(num_cpus::get())
        }

        pub fn log_num_cpus(&self) -> u32 {
            log2_floor(self.cpus)
        }

        pub fn compute<F, R>(&self, f: F) -> WorkerFuture<R::Item, R::Error>
        where
            F: FnOnce() -> R + Send + 'static,
            R: IntoFuture + 'static,
            R::Future: Send + 'static,
            R::Item: Send + 'static,
            R::Error: Send + 'static,
        {
            WorkerFuture {
                future: self.pool.spawn_fn(f),
            }
        }

        pub fn scope<'a, F, R>(&self, elements: usize, f: F) -> R
        where
            F: FnOnce(&Scope<'a>, usize) -> R,
        {
            let chunk_size = if elements < self.cpus {
                1
            } else {
                elements / self.cpus
            };

            crossbeam::scope(|scope| f(scope, chunk_size))
        }
    }

    pub struct WorkerFuture<T, E> {
        future: CpuFuture<T, E>,
    }

    impl<T: Send + 'static, E: Send + 'static> Future for WorkerFuture<T, E> {
        type Item = T;
        type Error = E;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            self.future.poll()
        }
    }

    fn log2_floor(num: usize) -> u32 {
        assert!(num > 0);

        let mut pow = 0;

        while (1 << (pow + 1)) <= num {
            pow += 1;
        }

        pow
    }

    #[test]
    fn test_log2_floor() {
        assert_eq!(log2_floor(1), 0);
        assert_eq!(log2_floor(2), 1);
        assert_eq!(log2_floor(3), 1);
        assert_eq!(log2_floor(4), 2);
        assert_eq!(log2_floor(5), 2);
        assert_eq!(log2_floor(6), 2);
        assert_eq!(log2_floor(7), 2);
        assert_eq!(log2_floor(8), 3);
    }
}

#[cfg(not(feature = "multicore"))]
mod implementation {
    use futures::{future, Future, IntoFuture, Poll};

    #[derive(Clone)]
    pub struct Worker;

    impl Worker {
        pub fn new() -> Worker {
            Worker
        }

        pub fn log_num_cpus(&self) -> u32 {
            0
        }

        pub fn compute<F, R>(&self, f: F) -> R::Future
        where
            F: FnOnce() -> R + Send + 'static,
            R: IntoFuture + 'static,
            R::Future: Send + 'static,
            R::Item: Send + 'static,
            R::Error: Send + 'static,
        {
            f().into_future()
        }

        pub fn scope<F, R>(&self, elements: usize, f: F) -> R
        where
            F: FnOnce(&DummyScope, usize) -> R,
        {
            f(&DummyScope, elements)
        }
    }

    pub struct WorkerFuture<T, E> {
        future: future::FutureResult<T, E>,
    }

    impl<T: Send + 'static, E: Send + 'static> Future for WorkerFuture<T, E> {
        type Item = T;
        type Error = E;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            self.future.poll()
        }
    }

    pub struct DummyScope;

    impl DummyScope {
        pub fn spawn<F: FnOnce()>(&self, f: F) {
            f();
        }
    }
}

pub use self::implementation::*;
