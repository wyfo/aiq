use crate::sync::{
    mutex::{DefaultMutex, Mutex},
    parker::{DefaultParker, Parker},
};

pub mod mutex;
pub mod parker;

pub trait SyncPrimitives {
    type Mutex: Mutex;
    type Parker: Parker;
    const SPIN_BEFORE_PARK: usize;
}

pub struct DefaultSyncPrimitives;

impl SyncPrimitives for DefaultSyncPrimitives {
    type Mutex = DefaultMutex;
    type Parker = DefaultParker;
    const SPIN_BEFORE_PARK: usize = 100; // same as `std::sys::sync::mutex::futex`
}
