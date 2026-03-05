use core::fmt::Debug;

use crate::sync::{
    mutex::{DefaultMutex, Mutex},
    parker::{DefaultParker, Parker},
};

#[cfg(feature = "atomic-wait")]
mod atomic_wait;
pub mod mutex;
pub mod parker;
#[cfg(feature = "pthread")]
mod pthread;
mod spin;
#[cfg(feature = "std")]
mod std;

pub trait SyncPrimitives: Debug {
    type Mutex: Mutex;
    type Parker: Parker;
    const SPIN_BEFORE_PARK: usize;
}

#[derive(Debug)]
pub struct DefaultSyncPrimitives;

impl SyncPrimitives for DefaultSyncPrimitives {
    type Mutex = DefaultMutex;
    type Parker = DefaultParker;
    #[cfg(not(any(miri, loom)))]
    const SPIN_BEFORE_PARK: usize = 100; // same as `std::sys::sync::mutex::futex`
    #[cfg(any(miri, loom))]
    const SPIN_BEFORE_PARK: usize = 0;
}
