#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]
#![cfg_attr(nightly, feature(unsafe_pinned))]

mod loom;
pub mod node;
pub mod queue;
pub mod sync;
#[cfg(not(nightly))]
mod unsafe_pinned;
#[cfg(feature = "wait-queue")]
pub mod wait_queue;

pub use node::{Node, NodeState};
pub use queue::Queue;
#[cfg(feature = "wait-queue")]
pub use wait_queue::WaitQueue;
