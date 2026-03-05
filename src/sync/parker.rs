#[cfg(feature = "atomic-wait")]
pub use super::atomic_wait::AtomicParker;
#[cfg(feature = "pthread")]
pub use super::pthread::PthreadParker;
pub use super::spin::SpinParker;
#[cfg(feature = "std")]
pub use super::std::StdParker;

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
    #[doc(hidden)]
    fn new() -> Self
    where
        Self: Sized,
    {
        Self::INIT
    }
    /// # Safety
    ///
    /// `park` can only be called by a single thread at a time.
    unsafe fn park(&self);
    fn unpark(&self);
}

cfg_if::cfg_if! {
    if #[cfg(loom)] {
      pub type DefaultParker = StdParker;
    } else if #[cfg(feature = "atomic-wait")] {
        pub type DefaultParker = AtomicParker;
    } else if #[cfg(feature = "std")] {
        pub type DefaultParker = StdParker;
    } else if #[cfg(feature = "pthread")] {
        pub type DefaultParker = PthreadParker;
    } else {
        pub type DefaultParker = SpinParker;
    }
}
