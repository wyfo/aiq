use core::{marker::PhantomData, ptr, ptr::NonNull};

use crate::node::NodeLink;

pub(crate) struct Tail<S>(PhantomData<S>);

#[expect(private_bounds)]
pub trait QueueState: QueueStatePrivate {}

/// # Safety
///
/// Implementation must be bijective.
pub(super) unsafe trait QueueStatePrivate: Sized + Copy + PartialEq {
    fn tail_to_enum(tail: *mut Tail<Self>) -> StateOrPtr<Self>;
    fn enum_to_tail(state_or_ptr: StateOrPtr<Self>) -> *mut Tail<Self>;
}

pub(crate) enum StateOrPtr<State> {
    State(State),
    Ptr(NonNull<NodeLink>),
}

impl<State: QueueState> StateOrPtr<State> {
    #[inline(always)]
    pub(crate) fn state(self) -> Option<State> {
        match self {
            Self::State(state) => Some(state),
            _ => None,
        }
    }

    #[inline(always)]
    pub(crate) fn tail(self) -> Option<NonNull<NodeLink>> {
        match self {
            Self::Ptr(tail) => Some(tail),
            _ => None,
        }
    }
}

unsafe impl QueueStatePrivate for () {
    #[inline(always)]
    fn tail_to_enum(tail: *mut Tail<Self>) -> StateOrPtr<Self> {
        NonNull::new(tail)
            .map(NonNull::cast)
            .map_or(StateOrPtr::State(()), StateOrPtr::Ptr)
    }
    #[inline(always)]
    fn enum_to_tail(state_or_ptr: StateOrPtr<Self>) -> *mut Tail<Self> {
        match state_or_ptr {
            StateOrPtr::State(_) => ptr::null_mut(),
            StateOrPtr::Ptr(ptr) => ptr.as_ptr().cast(),
        }
    }
}
impl QueueState for () {}

const TAIL_FLAG: usize = 1;
const STATE_SHIFT: usize = 1;

pub const INTRUSIVE_QUEUE_MAX_STATE: usize = usize::MAX >> STATE_SHIFT;
#[inline(always)]
pub(super) const fn state_to_ptr(state: usize) -> *mut Tail<usize> {
    #[cold]
    #[inline(never)]
    const fn panic_queue_state_overflow() -> ! {
        panic!("queue state overflow")
    }
    if state > INTRUSIVE_QUEUE_MAX_STATE {
        panic_queue_state_overflow()
    }
    ptr::without_provenance_mut(state << STATE_SHIFT)
}

unsafe impl QueueStatePrivate for usize {
    #[inline(always)]
    fn tail_to_enum(tail: *mut Tail<Self>) -> StateOrPtr<Self> {
        if tail.addr() & TAIL_FLAG != 0 {
            let ptr = tail.map_addr(|addr| addr & !TAIL_FLAG).cast();
            StateOrPtr::Ptr(unsafe { NonNull::new_unchecked(ptr) })
        } else {
            StateOrPtr::State(tail.addr() >> STATE_SHIFT)
        }
    }
    #[inline(always)]
    fn enum_to_tail(state_or_ptr: StateOrPtr<Self>) -> *mut Tail<Self> {
        match state_or_ptr {
            StateOrPtr::State(state) => state_to_ptr(state),
            StateOrPtr::Ptr(ptr) => ptr.as_ptr().map_addr(|addr| addr | TAIL_FLAG).cast(),
        }
    }
}
impl QueueState for usize {}

pub(crate) fn check_alignment<T>(ptr: *mut T) -> *mut Tail<*mut T> {
    const { assert!(align_of::<T>() > 1) };
    ptr.map_addr(|addr| addr & !TAIL_FLAG).cast()
}

unsafe impl<T> QueueStatePrivate for *mut T {
    #[inline(always)]
    fn tail_to_enum(tail: *mut Tail<Self>) -> StateOrPtr<Self> {
        if tail.addr() & TAIL_FLAG != 0 {
            let ptr = tail.map_addr(|addr| addr & !TAIL_FLAG).cast();
            StateOrPtr::Ptr(unsafe { NonNull::new_unchecked(ptr) })
        } else {
            StateOrPtr::State(tail.cast())
        }
    }
    #[inline(always)]
    fn enum_to_tail(state_or_ptr: StateOrPtr<Self>) -> *mut Tail<Self> {
        match state_or_ptr {
            StateOrPtr::State(state) => check_alignment(state),
            StateOrPtr::Ptr(ptr) => ptr.as_ptr().map_addr(|addr| addr | TAIL_FLAG).cast(),
        }
    }
}
impl<T> QueueState for *mut T {}

impl<S: QueueState> From<*mut Tail<S>> for StateOrPtr<S> {
    #[inline(always)]
    fn from(value: *mut Tail<S>) -> Self {
        S::tail_to_enum(value)
    }
}

impl<S: QueueState> From<StateOrPtr<S>> for *mut Tail<S> {
    #[inline(always)]
    fn from(value: StateOrPtr<S>) -> Self {
        S::enum_to_tail(value)
    }
}

#[cfg(test)]
mod tests {
    use core::ptr;

    use crate::queue::{StateOrPtr, Tail};

    #[test]
    fn null_is_state() {
        assert!(matches!(
            ptr::null_mut::<Tail<()>>().into(),
            StateOrPtr::State(())
        ));
        assert!(matches!(
            ptr::null_mut::<Tail<usize>>().into(),
            StateOrPtr::State(0)
        ));
    }
}
