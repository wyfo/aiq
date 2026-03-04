use core::fmt::Debug;

use crate::sync::{
    mutex::{DefaultMutex, Mutex},
    parker::{DefaultParker, Parker},
};

pub mod mutex;
pub mod parker;
#[cfg(not(loom))]
#[cfg(feature = "pthread")]
mod pthread;

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
