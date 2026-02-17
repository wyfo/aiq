#[cfg(feature = "std")]
extern crate std;

use core::sync::atomic::{AtomicBool, Ordering::*};

/// # Safety
///
/// Implementations of this trait must ensure that the mutex is actually
/// exclusive: a lock can't be acquired while the mutex is already locked.
///
/// Calls to [`unlock`](Self::unlock) must *synchronize-with* calls to [`lock`](Self::lock).
pub unsafe trait Mutex {
    const INIT: Self;
    type Guard<'a>
    where
        Self: 'a;
    #[must_use]
    fn lock(&self) -> Self::Guard<'_>;
    /// # Safety
    ///
    /// The guard must have been returned from [`lock`](Self::lock), and must be used only once.
    unsafe fn unlock<'a>(&'a self, guard: Self::Guard<'a>);
}

cfg_if::cfg_if! {
    if #[cfg(feature = "parking_lot")] {
        pub type DefaultMutex = parking_lot::RawMutex;
    } else if #[cfg(feature = "std")] {
        pub type DefaultMutex = StdMutex;
    } else {
        pub type DefaultMutex = SpinMutex;
    }
}

pub struct SpinMutex(AtomicBool);
unsafe impl Mutex for SpinMutex {
    const INIT: Self = SpinMutex(AtomicBool::new(false));
    type Guard<'a>
        = ()
    where
        Self: 'a;
    fn lock(&self) -> Self::Guard<'_> {
        while self.0.swap(true, Acquire) {
            while self.0.load(Relaxed) {
                core::hint::spin_loop();
            }
        }
    }
    unsafe fn unlock<'a>(&'a self, _guard: Self::Guard<'a>) {
        self.0.store(false, Release)
    }
}

#[cfg(feature = "lock_api")]
unsafe impl<M: lock_api::RawMutex> Mutex for M {
    const INIT: Self = <Self as lock_api::RawMutex>::INIT;
    type Guard<'a>
        = ()
    where
        Self: 'a;

    fn lock(&self) -> Self::Guard<'_> {
        lock_api::RawMutex::lock(self);
    }
    unsafe fn unlock<'a>(&'a self, _guard: Self::Guard<'a>) {
        // SAFETY: same contract
        unsafe { self.unlock() }
    }
}

#[cfg(feature = "std")]
#[derive(Debug)]
pub struct StdMutex(std::sync::Mutex<()>);

#[cfg(feature = "std")]
unsafe impl Mutex for StdMutex {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(std::sync::Mutex::new(()));
    type Guard<'a>
        = std::sync::MutexGuard<'a, ()>
    where
        Self: 'a;
    fn lock(&self) -> Self::Guard<'_> {
        self.0.lock().unwrap()
    }
    unsafe fn unlock<'a>(&'a self, guard: Self::Guard<'a>) {
        drop(guard);
    }
}
