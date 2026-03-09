use core::sync::atomic::{AtomicU32, Ordering::*};

use crate::sync::parker::Parker;

#[derive(Debug)]
pub struct AtomicParker(AtomicU32);

impl AtomicParker {
    const EMPTY: u32 = 0;
    const NOTIFIED: u32 = 1;
    const PARKED: u32 = u32::MAX;
}

// implementation taken for std Parker futex implementation
impl Parker for AtomicParker {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(AtomicU32::new(0));

    #[inline]
    unsafe fn park(&self) {
        if self.0.fetch_sub(1, Acquire) == Self::NOTIFIED {
            return;
        }
        loop {
            atomic_wait::wait(&self.0, Self::PARKED);
            if ((self.0).compare_exchange(Self::NOTIFIED, Self::EMPTY, Acquire, Relaxed)).is_ok() {
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
