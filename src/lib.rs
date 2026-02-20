#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]
#![cfg_attr(nightly, feature(unsafe_pinned))]

extern crate alloc;

pub mod queue;
pub mod sync;
#[cfg(not(nightly))]
mod unsafe_pinned;

pub use queue::{Node, NodeState, Queue};
