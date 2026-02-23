#[cfg(feature = "std")]
extern crate std;
#[cfg(feature = "atomic-wait")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicBool, Ordering::*};
#[cfg(feature = "std")]
use std::sync::{Condvar, Mutex, atomic::AtomicU8};

/// # Safety
///
/// [`park`] must block until [`unpark`] is called; there cannot be spurious wakeup.
/// If [`unpark`] has been called before [`park`], then [`park`] must return immediately.
/// Calls to [`unpark`] must *synchronize-with* calls to [`park`].
///
/// [`park`]: Self::park
/// [`unpark`]: Self::unpark
pub unsafe trait Parker {
    const INIT: Self;
    /// # Safety
    ///
    /// `park` can only be called by a single thread at a time.
    unsafe fn park(&self);
    fn unpark(&self);
}

cfg_if::cfg_if! {
    if #[cfg(feature = "atomic-wait")] {
        pub type DefaultParker = AtomicParker;
    } else if #[cfg(feature = "std")] {
        pub type DefaultParker = StdParker;
    } else {
        pub type DefaultParker = SpinParker;
    }
}

pub struct SpinParker(AtomicBool);
unsafe impl Parker for SpinParker {
    const INIT: Self = Self(AtomicBool::new(false));
    #[inline]
    unsafe fn park(&self) {
        while !self.0.load(Acquire) {
            core::hint::spin_loop();
        }
    }
    #[inline]
    fn unpark(&self) {
        self.0.store(true, Release);
    }
}

#[cfg(feature = "atomic-wait")]
#[derive(Debug)]
pub struct AtomicParker(AtomicU32);

#[cfg(feature = "atomic-wait")]
impl AtomicParker {
    const EMPTY: u32 = 0;
    const NOTIFIED: u32 = 1;
    const PARKED: u32 = u32::MAX;
}

#[cfg(feature = "atomic-wait")]
// SAFETY: implementation taken for std Parker futex implementation
unsafe impl Parker for AtomicParker {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(AtomicU32::new(0));

    #[inline]
    unsafe fn park(&self) {
        if self.0.fetch_sub(1, Acquire) == Self::NOTIFIED {
            return;
        }
        loop {
            atomic_wait::wait(&self.0, Self::PARKED);
            if self
                .0
                .compare_exchange(Self::NOTIFIED, Self::EMPTY, Acquire, Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    #[inline]
    fn unpark(&self) {
        if self.0.swap(Self::NOTIFIED, Release) == Self::PARKED {
            atomic_wait::wake_one(&self.0);
        }
    }
}

#[cfg(feature = "std")]
#[derive(Debug)]
pub struct StdParker {
    condvar: Condvar,
    mutex: Mutex<()>,
    state: AtomicU8,
}

#[cfg(feature = "std")]
impl StdParker {
    const EMPTY: u8 = 0;
    const NOTIFIED: u8 = 1;
    const PARKED: u8 = u8::MAX;
}

#[cfg(feature = "std")]
// SAFETY: implementation inspired for std Parker futex/pthread implementation
unsafe impl Parker for StdParker {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self {
        condvar: Condvar::new(),
        mutex: Mutex::new(()),
        state: AtomicU8::new(Self::EMPTY),
    };

    #[inline]
    unsafe fn park(&self) {
        if self.state.fetch_sub(1, Acquire) == Self::NOTIFIED {
            return;
        }
        let guard = self.mutex.lock().unwrap();
        *self
            .condvar
            .wait_while(guard, |_| {
                self.state
                    .compare_exchange(Self::NOTIFIED, Self::EMPTY, Acquire, Relaxed)
                    .is_err()
            })
            .unwrap();
    }

    #[inline]
    fn unpark(&self) {
        if self.state.swap(Self::NOTIFIED, Release) == Self::PARKED {
            drop(self.mutex.lock().unwrap());
            self.condvar.notify_one();
        }
    }
}
