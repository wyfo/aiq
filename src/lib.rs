#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]
#![cfg_attr(feature = "nightly", feature(unsafe_pinned))]

extern crate alloc;

pub mod queue;
pub mod sync;
#[cfg(not(feature = "nightly"))]
mod unsafe_pinned;

pub use queue::{Node, Queue};
