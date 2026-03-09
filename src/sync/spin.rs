use core::sync::atomic::{AtomicBool, Ordering::*};

use crate::sync::{mutex::Mutex, parker::Parker};

pub struct SpinMutex(AtomicBool);

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

pub struct SpinParker;

impl Parker for SpinParker {
    const INIT: Self = Self;
    #[inline]
    unsafe fn park(&self) {}
    #[inline]
    fn unpark(&self) {}
}
