#[cfg(nightly)]
use core::pin::UnsafePinned;
use core::{
    cell::Cell,
    hint::unreachable_unchecked,
    marker::PhantomData,
    mem,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering::*},
};

#[cfg(feature = "queue-state")]
use crate::queue::state::*;
#[cfg(not(nightly))]
use crate::unsafe_pinned::UnsafePinned;
use crate::{
    queue::{LockedQueue, Queue, QueueRef},
    sync::SyncPrimitives,
};

#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(usize)]
pub(super) enum NodeLinkState {
    Unqueued,
    Queued,
    Dequeued,
    Parked,
}

impl NodeLinkState {
    #[inline(always)]
    pub(super) unsafe fn from(value: usize) -> Self {
        unsafe { mem::transmute::<usize, Self>(value) }
    }
}

#[repr(C)]
pub(super) struct NodeLink {
    pub(super) prev: Cell<NonNull<NodeLink>>,
    pub(super) next: AtomicPtr<NodeLink>,
    pub(super) state: AtomicUsize,
}

impl NodeLink {
    pub(super) const fn new() -> Self {
        Self {
            prev: Cell::new(NonNull::dangling()),
            next: AtomicPtr::new(ptr::null_mut()),
            state: AtomicUsize::new(NodeLinkState::Unqueued as _),
        }
    }

    #[inline(always)]
    pub(super) fn prev(&self) -> &NodeLink {
        unsafe { self.prev.get().as_ref() }
    }

    #[inline(always)]
    pub(super) fn next(&self) -> Option<NonNull<NodeLink>> {
        NonNull::new(self.next.load(SeqCst))
    }

    #[inline(always)]
    pub(super) fn state(&self) -> NodeLinkState {
        unsafe { NodeLinkState::from(self.state.load(SeqCst)) }
    }

    #[inline(always)]
    pub(super) fn unlink(&self, next: NonNull<NodeLink>) {
        unsafe { next.as_ref() }.prev.set(self.prev.get());
        self.prev().next.store(next.as_ptr(), Relaxed);
        self.state.store(NodeLinkState::Dequeued as _, Release);
    }
}

#[repr(C)]
pub(super) struct NodeWithData<T> {
    pub(super) link: NodeLink,
    pub(super) data: T,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RawNodeState {
    Unqueued,
    Queued,
    Dequeued,
}

impl From<NodeLinkState> for RawNodeState {
    #[inline(always)]
    fn from(value: NodeLinkState) -> Self {
        match value {
            NodeLinkState::Unqueued => RawNodeState::Unqueued,
            NodeLinkState::Queued | NodeLinkState::Parked => RawNodeState::Queued,
            NodeLinkState::Dequeued => RawNodeState::Dequeued,
        }
    }
}

pub enum NodeState<'a, T, S: SyncPrimitives> {
    Unqueued(NodeUnqueued<'a, T, S>),
    Queued(NodeQueued<'a, T, S>),
    Dequeued(NodeDequeued<'a, T, S>),
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
        unsafe { self.node.get().as_ref().unwrap().link.state().into() }
    }

    #[inline(always)]
    pub fn state(self: Pin<&mut Self>) -> NodeState<'_, Q::NodeData, Q::SyncPrimitives> {
        unsafe { self.get_unchecked_mut().state_and_queue_impl().0 }
    }

    #[inline(always)]
    fn state_and_queue_impl(&self) -> (NodeState<'_, Q::NodeData, Q::SyncPrimitives>, &Q) {
        let node = NonNull::from(&self.node);
        let state = match self.raw_state() {
            RawNodeState::Unqueued => NodeState::Unqueued(NodeUnqueued {
                node,
                queue: self.queue.queue(),
            }),
            RawNodeState::Queued => {
                let locked = self.queue.queue().lock();
                match self.raw_state() {
                    RawNodeState::Unqueued => unsafe { unreachable_unchecked() },
                    RawNodeState::Queued => NodeState::Queued(NodeQueued { node, locked }),
                    RawNodeState::Dequeued => NodeState::Dequeued(NodeDequeued {
                        node,
                        _queue: PhantomData,
                    }),
                }
            }
            RawNodeState::Dequeued => NodeState::Dequeued(NodeDequeued {
                node,
                _queue: PhantomData,
            }),
        };
        (state, &self.queue)
    }

    pub fn state_and_queue(
        self: Pin<&mut Self>,
    ) -> (NodeState<'_, Q::NodeData, Q::SyncPrimitives>, &Q) {
        unsafe { self.get_unchecked_mut().state_and_queue_impl() }
    }
}

impl<Q: QueueRef> Drop for Node<Q> {
    fn drop(&mut self) {
        if let NodeState::Queued(queued) = self.state_and_queue_impl().0 {
            queued.dequeue()
        }
    }
}

macro_rules! node_data {
    ($node:ident) => {
        impl<T, S: SyncPrimitives> $node<'_, T, S> {
            #[allow(dead_code)]
            fn link(&self) -> NonNull<NodeLink> {
                NonNull::new(UnsafePinned::raw_get(self.node.as_ptr()))
                    .unwrap()
                    .cast()
            }

            #[allow(clippy::mut_from_ref)]
            fn data_pinned_const(&self) -> Pin<&mut T> {
                unsafe {
                    Pin::new_unchecked(&mut (*UnsafePinned::raw_get(self.node.as_ptr())).data)
                }
            }

            pub fn data_pinned(&mut self) -> Pin<&mut T> {
                self.data_pinned_const()
            }
        }

        impl<T, S: SyncPrimitives> Deref for $node<'_, T, S> {
            type Target = T;

            fn deref(&self) -> &Self::Target {
                unsafe { &(*UnsafePinned::raw_get(self.node.as_ptr())).data }
            }
        }

        impl<T: Unpin, S: SyncPrimitives> DerefMut for $node<'_, T, S> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                unsafe { &mut (*UnsafePinned::raw_get(self.node.as_ptr())).data }
            }
        }
    };
}

pub struct NodeUnqueued<'a, T, S: SyncPrimitives> {
    node: NonNull<UnsafePinned<NodeWithData<T>>>,
    queue: &'a Queue<T, S>,
}

impl<'a, T, S: SyncPrimitives> NodeUnqueued<'a, T, S> {
    #[inline]
    pub fn enqueue<I: FnOnce(Pin<&mut T>)>(&mut self, init: I) {
        init(self.data_pinned());
        #[cfg(feature = "queue-state")]
        let new_tail = |tail| Some((StateOrTail::Tail(self.node.cast()).into(), Some(tail)));
        #[cfg(not(feature = "queue-state"))]
        let new_tail = |tail| Some((self.node.as_ptr().cast(), Some(tail)));
        unsafe { self.queue.as_ref().enqueue(self.link(), new_tail) };
    }

    #[cfg(feature = "queue-state")]
    pub fn fetch_update_queue_state_or_enqueue<
        F: FnMut(QueueState) -> Option<QueueState>,
        I: FnMut(Option<QueueState>, Pin<&mut T>) -> bool,
    >(
        &mut self,
        mut f: F,
        mut init: I,
    ) -> Result<(), Option<QueueState>> {
        let new_tail = |tail| match StateOrTail::from(tail) {
            StateOrTail::State(state) => match f(state) {
                Some(new_state) => Some((StateOrTail::State(new_state).into(), None)),
                None if init(Some(state), self.data_pinned_const()) => Some((
                    StateOrTail::Tail(self.node.cast()).into(),
                    Some(ptr::null_mut()),
                )),
                None => None,
            },
            StateOrTail::Tail(tail) if init(None, self.data_pinned_const()) => Some((
                StateOrTail::Tail(self.node.cast()).into(),
                Some(tail.as_ptr()),
            )),
            StateOrTail::Tail(_) => None,
        };
        match unsafe { self.queue.as_ref().enqueue(self.link(), new_tail) } {
            Some(tail) => Err(match tail.into() {
                StateOrTail::State(state) => Some(state),
                _ => None,
            }),
            None => Ok(()),
        }
    }
}

node_data!(NodeUnqueued);

pub struct NodeQueued<'a, T, S: SyncPrimitives> {
    node: NonNull<UnsafePinned<NodeWithData<T>>>,
    locked: LockedQueue<'a, T, S>,
}

impl<T, S: SyncPrimitives> NodeQueued<'_, T, S> {
    pub fn dequeue(mut self) {
        unsafe { self.locked.remove(self.link().as_ref(), ptr::null_mut) };
    }

    #[cfg(feature = "queue-state")]
    pub fn dequeue_and_try_set_queue_state<F: FnOnce() -> QueueState>(mut self, state: F) -> bool {
        let reset_tail = || StateOrTail::State(state()).into();
        unsafe { self.locked.remove(self.link().as_ref(), reset_tail) }
    }
}

node_data!(NodeQueued);

pub struct NodeDequeued<'a, T, S: SyncPrimitives> {
    node: NonNull<UnsafePinned<NodeWithData<T>>>,
    _queue: PhantomData<&'a Queue<T, S>>,
}

node_data!(NodeDequeued);
