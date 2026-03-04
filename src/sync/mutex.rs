#[cfg(feature = "std")]
extern crate std;

#[cfg(not(loom))]
use core::sync::atomic::{AtomicBool, Ordering::*};

#[cfg(not(loom))]
#[cfg(feature = "pthread")]
pub use crate::sync::pthread::PthreadMutex;

/// # Safety
///
/// Implementations of this trait must ensure that the mutex is actually
/// exclusive: a lock can't be acquired while the mutex is already locked.
///
/// Calls to [`unlock`](Self::unlock) must *synchronize-with* calls to [`lock`](Self::lock).
pub unsafe trait Mutex {
    #[cfg(not(loom))]
    const INIT: Self;
    #[cfg(loom)]
    fn new() -> Self;
    type Guard<'a>
    where
        Self: 'a;
    fn lock(&self) -> Self::Guard<'_>;
    /// # Safety
    ///
    /// The guard must have been returned from [`lock`](Self::lock), and must be used only once.
    unsafe fn unlock<'a>(&'a self, guard: Self::Guard<'a>);
}

#[cfg(not(loom))]
cfg_if::cfg_if! {
    if #[cfg(feature = "parking_lot")] {
        pub type DefaultMutex = parking_lot::RawMutex;
    } else if #[cfg(feature = "std")] {
        pub type DefaultMutex = StdMutex;
    } else if #[cfg(feature = "pthread")] {
        pub type DefaultMutex = PthreadMutex;
    } else {
        pub type DefaultMutex = SpinMutex;
    }
}
#[cfg(loom)]
pub type DefaultMutex = StdMutex;

#[cfg(not(loom))]
pub struct SpinMutex(AtomicBool);
#[cfg(not(loom))]
unsafe impl Mutex for SpinMutex {
    const INIT: Self = Self(AtomicBool::new(false));
    type Guard<'a>
        = ()
    where
        Self: 'a;
    #[inline]
    fn lock(&self) -> Self::Guard<'_> {
        while self.0.swap(true, Acquire) {
            while !self.0.load(Relaxed) {
                core::hint::spin_loop();
            }
        }
    }
    #[inline]
    unsafe fn unlock<'a>(&'a self, _guard: Self::Guard<'a>) {
        self.0.store(false, Release);
    }
}

#[cfg(not(loom))]
#[cfg(feature = "lock_api")]
unsafe impl<M: lock_api::RawMutex> Mutex for M {
    const INIT: Self = <Self as lock_api::RawMutex>::INIT;
    type Guard<'a>
        = ()
    where
        Self: 'a;

    #[inline]
    fn lock(&self) -> Self::Guard<'_> {
        lock_api::RawMutex::lock(self);
    }
    #[inline]
    unsafe fn unlock<'a>(&'a self, _guard: Self::Guard<'a>) {
        // SAFETY: same contract
        unsafe { self.unlock() }
    }
}

#[cfg(feature = "std")]
#[derive(Debug)]
pub struct StdMutex(
    #[cfg(not(loom))] std::sync::Mutex<()>,
    #[cfg(loom)] loom::sync::Mutex<()>,
);

#[cfg(feature = "std")]
unsafe impl Mutex for StdMutex {
    #[cfg(not(loom))]
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(std::sync::Mutex::new(()));
    #[cfg(loom)]
    fn new() -> Self {
        Self(loom::sync::Mutex::new(()))
    }
    #[cfg(not(loom))]
    type Guard<'a>
        = std::sync::MutexGuard<'a, ()>
    where
        Self: 'a;
    #[cfg(loom)]
    type Guard<'a>
        = loom::sync::MutexGuard<'a, ()>
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
