#[cfg(feature = "pthread")]
pub use super::pthread::PthreadMutex;
pub use super::spin::SpinMutex;
#[cfg(feature = "std")]
pub use super::std::StdMutex;

/// # Safety
///
/// Implementations of this trait must ensure that the mutex is actually
/// exclusive: a lock can't be acquired while the mutex is already locked.
///
/// Calls to [`unlock`](Self::unlock) must *synchronize-with* calls to [`lock`](Self::lock).
pub unsafe trait Mutex {
    const INIT: Self;
    #[doc(hidden)]
    fn new() -> Self
    where
        Self: Sized,
    {
        Self::INIT
    }
    type Guard<'a>
    where
        Self: 'a;
    fn lock(&self) -> Self::Guard<'_>;
    /// # Safety
    ///
    /// The guard must have been returned from [`lock`](Self::lock), and must be used only once.
    unsafe fn unlock<'a>(&'a self, guard: Self::Guard<'a>);
}

cfg_if::cfg_if! {
    if #[cfg(loom)] {
        pub type DefaultMutex = StdMutex;
    } else if #[cfg(feature = "parking_lot")] {
        pub type DefaultMutex = parking_lot::RawMutex;
    } else if #[cfg(feature = "std")] {
        pub type DefaultMutex = StdMutex;
    } else if #[cfg(feature = "pthread")] {
        pub type DefaultMutex = PthreadMutex;
    } else {
        pub type DefaultMutex = SpinMutex;
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
