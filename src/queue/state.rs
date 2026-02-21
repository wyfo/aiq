use core::{ptr, ptr::NonNull};

use crate::queue::node::NodeLink;

pub(super) const TAIL_FLAG: usize = 1;
pub(super) const STATE_SHIFT: usize = 1;

pub type QueueState = usize;
pub const INTRUSIVE_QUEUE_MAX_STATE: QueueState = usize::MAX >> STATE_SHIFT;

#[derive(Debug)]
pub(super) enum StateOrTail {
    State(QueueState),
    Tail(NonNull<NodeLink>),
}

#[inline(always)]
pub(super) const fn state_to_ptr(state: QueueState) -> *mut NodeLink {
    #[cold]
    #[inline(never)]
    const fn panic_queue_state_overflow() -> ! {
        panic!("queue state overflow")
    }
    if state > usize::MAX >> STATE_SHIFT {
        panic_queue_state_overflow()
    }
    ptr::without_provenance_mut(state << STATE_SHIFT)
}

impl From<*mut NodeLink> for StateOrTail {
    #[inline(always)]
    fn from(value: *mut NodeLink) -> Self {
        if value.addr() & TAIL_FLAG != 0 {
            Self::Tail(unsafe { NonNull::new_unchecked(value.map_addr(|addr| addr & !TAIL_FLAG)) })
        } else {
            Self::State(value.addr() >> STATE_SHIFT)
        }
    }
}

impl From<StateOrTail> for *mut NodeLink {
    #[inline(always)]
    fn from(value: StateOrTail) -> Self {
        match value {
            StateOrTail::State(state) => state_to_ptr(state),
            StateOrTail::Tail(tail) => tail.as_ptr().map_addr(|addr| addr | TAIL_FLAG),
        }
    }
}
