#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]
#![cfg_attr(nightly, feature(unsafe_pinned))]

mod loom;
pub mod node;
pub mod queue;
pub mod sync;
#[cfg(not(nightly))]
mod unsafe_pinned;
pub mod wait_queue;

pub use node::{Node, NodeState};
pub use queue::Queue;
pub use wait_queue::WaitQueue;
