use crate::loom::sync::{
    Condvar, Mutex, MutexGuard,
    atomic::{AtomicUsize, Ordering::*},
};

#[derive(Debug)]
pub struct StdMutex(Mutex<()>);

unsafe impl super::Mutex for StdMutex {
    #[cfg(not(loom))]
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(Mutex::new(()));
    #[cfg(loom)]
    const INIT: Self = unimplemented!();
    fn new() -> Self {
        Self(Mutex::new(()))
    }
    type Guard<'a>
        = MutexGuard<'a, ()>
    where
        Self: 'a;
    #[inline]
    fn lock(&self) -> Self::Guard<'_> {
        #[cold]
        #[inline(never)]
        fn panic_lock() -> ! {
            panic!("poisoned lock: another task failed inside");
        }
        match self.0.lock() {
            Ok(guard) => guard,
            Err(_) => panic_lock(),
        }
    }
    #[inline]
    unsafe fn unlock<'a>(&'a self, guard: Self::Guard<'a>) {
        drop(guard);
    }
}

#[derive(Debug)]
pub struct StdParker {
    state: AtomicUsize,
    mutex: Mutex<()>,
    condvar: Condvar,
}

impl StdParker {
    const EMPTY: usize = 0;
    const NOTIFIED: usize = 1;
    const PARKED: usize = usize::MAX;
}

// implementation inspired for std Parker futex/pthread implementation
impl super::Parker for StdParker {
    #[cfg(not(loom))]
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self {
        state: AtomicUsize::new(Self::EMPTY),
        mutex: Mutex::new(()),
        condvar: Condvar::new(),
    };
    #[cfg(loom)]
    const INIT: Self = unimplemented!();
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(Self::EMPTY),
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
        }
    }

    #[inline]
    unsafe fn park(&self) {
        if self.state.fetch_sub(1, Acquire) == Self::NOTIFIED {
            return;
        }
        let guard = self.mutex.lock().unwrap();
        let is_not_notified = |_: &mut _| {
            ((self.state).compare_exchange(Self::NOTIFIED, Self::EMPTY, Acquire, Relaxed)).is_err()
        };
        #[cfg(not(loom))]
        *self.condvar.wait_while(guard, is_not_notified).unwrap();
        #[cfg(loom)]
        let mut guard = guard;
        #[cfg(loom)]
        while is_not_notified(&mut *guard) {
            guard = self.condvar.wait(guard).unwrap();
        }
    }

    #[inline]
    fn unpark(&self) {
        if self.state.swap(Self::NOTIFIED, Release) == Self::PARKED {
            drop(self.mutex.lock().unwrap());
            self.condvar.notify_one();
        }
    }
}
