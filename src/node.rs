#[cfg(nightly)]
use core::pin::UnsafePinned;
use core::{
    hint::unreachable_unchecked,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering::*},
};

#[cfg(feature = "queue-state")]
use crate::queue::state::*;
use crate::queue::{LockedQueue, QueueRef};
#[cfg(not(nightly))]
use crate::unsafe_pinned::UnsafePinned;

#[repr(C)]
pub(crate) struct NodeLink {
    pub(crate) prev: AtomicPtr<NodeLink>,
    pub(crate) next: AtomicPtr<NodeLink>,
}

impl NodeLink {
    pub(crate) const fn new() -> Self {
        Self {
            prev: AtomicPtr::new(ptr::null_mut()),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[inline(always)]
    pub(crate) fn prev(&self) -> &NodeLink {
        unsafe { self.prev.load(Relaxed).as_ref().unwrap_unchecked() }
    }

    pub(crate) fn dequeue(&self) {
        let dequeued = ptr::without_provenance_mut(RawNodeState::Dequeued as _);
        self.prev.store(dequeued, Release);
    }

    #[inline(always)]
    pub(crate) fn next(&self) -> Option<NonNull<NodeLink>> {
        NonNull::new(self.next.load(SeqCst))
    }

    #[inline(always)]
    pub(crate) fn state(&self) -> RawNodeState {
        match self.prev.load(Acquire).addr().min(2) {
            0 => RawNodeState::Unqueued,
            1 => RawNodeState::Dequeued,
            2 => RawNodeState::Queued,
            _ => unreachable!(),
        }
    }
}

#[repr(C)]
pub(crate) struct NodeWithData<T> {
    pub(crate) link: NodeLink,
    pub(crate) data: T,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RawNodeState {
    Unqueued = 0,
    Queued = 2,
    Dequeued = 1,
}

pub enum NodeState<'a, Q: QueueRef> {
    Unqueued(NodeUnqueued<'a, Q>),
    Queued(NodeQueued<'a, Q>),
    Dequeued(NodeDequeued<'a, Q>),
}

pub struct Node<Q: QueueRef> {
    queue: Q,
    node: UnsafePinned<NodeWithData<Q::NodeData>>,
}

unsafe impl<Q: QueueRef> Send for Node<Q> {}
unsafe impl<Q: QueueRef> Sync for Node<Q> {}

impl<Q: QueueRef> Node<Q> {
    pub const fn new(queue: Q, data: Q::NodeData) -> Self {
        Self {
            queue,
            node: UnsafePinned::new(NodeWithData {
                link: NodeLink::new(),
                data,
            }),
        }
    }

    #[inline(always)]
    pub const fn queue(&self) -> &Q {
        &self.queue
    }

    #[inline(always)]
    pub fn raw_state(&self) -> RawNodeState {
        unsafe { (*self.node.get()).link.state() }
    }

    #[inline(always)]
    pub fn state(self: Pin<&mut Self>) -> NodeState<'_, Q> {
        unsafe { self.get_unchecked_mut().state_impl() }
    }

    #[inline(always)]
    fn state_impl(&self) -> NodeState<'_, Q> {
        let node = NonNull::new(self.node.get()).unwrap();
        let queue = &self.queue;
        match self.raw_state() {
            RawNodeState::Unqueued => NodeState::Unqueued(NodeUnqueued { node, queue }),
            RawNodeState::Queued => {
                let locked = self.queue.queue().lock();
                match self.raw_state() {
                    RawNodeState::Unqueued => unsafe { unreachable_unchecked() },
                    RawNodeState::Queued => NodeState::Queued(NodeQueued {
                        node,
                        queue,
                        locked,
                    }),
                    RawNodeState::Dequeued => NodeState::Dequeued(NodeDequeued { node, queue }),
                }
            }
            RawNodeState::Dequeued => NodeState::Dequeued(NodeDequeued { node, queue }),
        }
    }

    #[cold]
    #[inline(never)]
    fn dequeue(&mut self) {
        let mut locked = self.queue.queue().lock();
        if self.raw_state() == RawNodeState::Queued {
            unsafe { locked.remove(&(*self.node.get()).link, ptr::null_mut()) };
        }
    }
}

impl<Q: QueueRef> Drop for Node<Q> {
    #[inline(always)]
    fn drop(&mut self) {
        if self.raw_state() == RawNodeState::Queued {
            self.dequeue()
        }
    }
}

macro_rules! node_getters {
    ($node:ident) => {
        impl<'a, Q: QueueRef> $node<'a, Q> {
            pub fn queue(&self) -> &'a Q {
                self.queue
            }

            pub fn data_pinned(&mut self) -> Pin<&mut Q::NodeData> {
                unsafe { Pin::new_unchecked(&mut (*self.node.as_ptr()).data) }
            }
        }

        impl<Q: QueueRef> Deref for $node<'_, Q> {
            type Target = Q::NodeData;

            fn deref(&self) -> &Self::Target {
                unsafe { &(*self.node.as_ptr()).data }
            }
        }

        impl<Q: QueueRef> DerefMut for $node<'_, Q> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                unsafe { &mut (*self.node.as_ptr()).data }
            }
        }
    };
}

pub struct NodeUnqueued<'a, Q: QueueRef> {
    node: NonNull<NodeWithData<Q::NodeData>>,
    queue: &'a Q,
}

impl<'a, Q: QueueRef> NodeUnqueued<'a, Q> {
    #[inline]
    pub fn enqueue(self) {
        #[cfg(feature = "queue-state")]
        let new_tail = |tail| {
            let prev = match StateOrTail::from(tail) {
                StateOrTail::Tail(tail) => tail.as_ptr(),
                _ => ptr::null_mut(),
            };
            Some((StateOrTail::Tail(self.node.cast()).into(), Some(prev)))
        };
        #[cfg(not(feature = "queue-state"))]
        let new_tail = |tail| Some((self.node.as_ptr().cast(), Some(tail)));
        unsafe { self.queue.queue().enqueue(self.node.cast(), new_tail) };
    }

    #[cfg(feature = "queue-state")]
    pub fn fetch_update_queue_state_or_enqueue<
        F: FnMut(QueueState) -> Option<QueueState>,
        I: FnMut(Option<QueueState>, Pin<&mut Q::NodeData>) -> bool,
    >(
        self,
        mut f: F,
        mut init: I,
    ) -> Result<(), Option<QueueState>> {
        let data_pinned = || unsafe { Pin::new_unchecked(&mut (*self.node.as_ptr()).data) };
        let new_tail = |tail| match StateOrTail::from(tail) {
            StateOrTail::State(state) => match f(state) {
                Some(new_state) => Some((StateOrTail::State(new_state).into(), None)),
                None if init(Some(state), data_pinned()) => Some((
                    StateOrTail::Tail(self.node.cast()).into(),
                    Some(ptr::null_mut()),
                )),
                None => None,
            },
            StateOrTail::Tail(tail) if init(None, data_pinned()) => Some((
                StateOrTail::Tail(self.node.cast()).into(),
                Some(tail.as_ptr()),
            )),
            StateOrTail::Tail(_) => None,
        };
        match unsafe { self.queue.queue().enqueue(self.node.cast(), new_tail) } {
            Some(tail) => Err(match tail.into() {
                StateOrTail::State(state) => Some(state),
                _ => None,
            }),
            None => Ok(()),
        }
    }
}

node_getters!(NodeUnqueued);

pub struct NodeQueued<'a, Q: QueueRef> {
    node: NonNull<NodeWithData<Q::NodeData>>,
    queue: &'a Q,
    locked: LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>,
}

impl<'a, Q: QueueRef> NodeQueued<'a, Q> {
    pub fn dequeue(mut self) -> (&'a Q, LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>) {
        let node = unsafe { self.node.cast().as_ref() };
        unsafe { self.locked.remove(node, ptr::null_mut()) };
        (self.queue, self.locked)
    }

    #[cfg(feature = "queue-state")]
    #[allow(clippy::type_complexity)]
    pub fn dequeue_try_set_queue_state(
        mut self,
        state: QueueState,
    ) -> Result<
        (&'a Q, LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>),
        (&'a Q, LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>),
    > {
        let node = unsafe { self.node.cast().as_ref() };
        if unsafe { (self.locked).remove(node, StateOrTail::State(state).into()) } {
            Ok((self.queue, self.locked))
        } else {
            Err((self.queue, self.locked))
        }
    }
}

node_getters!(NodeQueued);

pub struct NodeDequeued<'a, Q: QueueRef> {
    node: NonNull<NodeWithData<Q::NodeData>>,
    queue: &'a Q,
}

node_getters!(NodeDequeued);
