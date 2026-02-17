use core::ptr;

use crate::{queue::node::NodeLink, sync::SyncPrimitives};

pub(super) const TAIL_FLAG: usize = 1;
pub(super) const STATE_SHIFT: usize = 1;

pub type QueueState = usize;
pub const INTRUSIVE_QUEUE_MAX_STATE: QueueState = usize::MAX >> STATE_SHIFT;

pub(super) fn state_to_ptr<S: SyncPrimitives>(state: QueueState) -> *mut NodeLink<S> {
    assert!(
        state <= usize::MAX >> STATE_SHIFT,
        "queue state must be lesser than or equal to {INTRUSIVE_QUEUE_MAX_STATE}"
    );
    ptr::without_provenance_mut(state << STATE_SHIFT)
}

pub(super) fn ptr_to_state<S: SyncPrimitives>(ptr: *mut NodeLink<S>) -> Option<QueueState> {
    (ptr.addr() & TAIL_FLAG == 0).then(|| ptr.addr() >> STATE_SHIFT)
}
