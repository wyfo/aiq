#[cfg(feature = "nightly")]
use core::pin::UnsafePinned;
use core::{
    cell::Cell,
    hint,
    hint::unreachable_unchecked,
    marker::{PhantomData, PhantomPinned},
    mem,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering::*},
};

#[cfg(feature = "queue-state")]
use crate::queue::state::*;
#[cfg(not(feature = "nightly"))]
use crate::unsafe_pinned::UnsafePinned;
use crate::{
    queue::{LockedQueue, Queue},
    sync::{parker::Parker, DefaultSyncPrimitives, SyncPrimitives},
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
pub(super) struct NodeLink<S: SyncPrimitives> {
    pub(super) prev: Cell<NonNull<NodeLink<S>>>,
    pub(super) next: AtomicPtr<NodeLink<S>>,
    pub(super) state: AtomicUsize,
    pub(super) parker: S::Parker,
}

impl<S: SyncPrimitives> NodeLink<S> {
    pub(super) const fn new() -> Self {
        Self {
            prev: Cell::new(NonNull::dangling()),
            next: AtomicPtr::new(ptr::null_mut()),
            state: AtomicUsize::new(NodeLinkState::Unqueued as _),
            parker: S::Parker::INIT,
        }
    }

    pub(super) fn prev(&self) -> &NodeLink<S> {
        unsafe { self.prev.get().as_ref() }
    }

    fn next(&self) -> Option<NonNull<NodeLink<S>>> {
        NonNull::new(self.next.load(SeqCst))
    }

    pub(super) fn find_next(
        &self,
        tail: impl FnOnce() -> Option<NonNull<NodeLink<S>>>,
    ) -> Option<NonNull<NodeLink<S>>> {
        if let Some(next) = self.next() {
            return Some(next);
        }
        self.find_next_backward(tail())
    }

    #[cold]
    #[inline(never)]
    fn find_next_backward(
        &self,
        tail: Option<NonNull<NodeLink<S>>>,
    ) -> Option<NonNull<NodeLink<S>>> {
        let mut next = tail?;
        loop {
            // do not use Self::prev because NodeLink will be cast to NodeWithData
            let prev = unsafe { next.as_ref() }.prev.get();
            if prev == NonNull::from(self) {
                return Some(next);
            }
            if let Some(node) = self.next() {
                return Some(node);
            }
            next = prev
        }
    }

    pub(super) fn state(&self) -> NodeLinkState {
        unsafe { NodeLinkState::from(self.state.load(SeqCst)) }
    }

    #[inline(always)]
    pub(super) fn wait_for_next(&self, reset_state: Option<NodeLinkState>) -> NonNull<NodeLink<S>> {
        for _ in 0..S::SPIN_BEFORE_PARK + 1 {
            if let Some(next) = self.next() {
                return next;
            }
            hint::spin_loop();
        }
        let next = self.park_loop();
        if let Some(reset_state) = reset_state {
            self.state.store(reset_state as _, Relaxed);
        }
        next
    }

    #[cold]
    #[inline(never)]
    fn park_loop(&self) -> NonNull<NodeLink<S>> {
        self.state.store(NodeLinkState::Parked as _, SeqCst);
        loop {
            if let Some(next) = self.next() {
                return next;
            }
            unsafe { self.parker.park() };
        }
    }

    pub(super) fn unlink(&self, next: Option<NonNull<NodeLink<S>>>) -> bool {
        if let Some(next) = next {
            unsafe { next.as_ref() }.prev.set(self.prev.get());
        }
        let next_ptr = next.map_or(ptr::null_mut(), NonNull::as_ptr);
        self.prev().next.store(next_ptr, Relaxed);
        self.state.store(NodeLinkState::Dequeued as _, Release);
        next.is_none()
    }
}

#[repr(C)]
pub(super) struct NodeWithData<T, S: SyncPrimitives> {
    pub(super) link: NodeLink<S>,
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
        debug_assert!(value != NodeLinkState::Parked);
        match value {
            NodeLinkState::Unqueued => RawNodeState::Unqueued,
            NodeLinkState::Queued => RawNodeState::Queued,
            NodeLinkState::Dequeued => RawNodeState::Dequeued,
            _ => unsafe { unreachable_unchecked() },
        }
    }
}

pub enum NodeState<'a, T, S: SyncPrimitives> {
    Unqueued(NodeUnqueued<'a, T, S>),
    Queued(NodeQueued<'a, T, S>),
    Dequeued(NodeDequeued<'a, T, S>),
}

pub struct Node<Q: AsRef<Queue<T, S>>, T, S: SyncPrimitives = DefaultSyncPrimitives> {
    queue: Q,
    node: UnsafePinned<NodeWithData<T, S>>,
    _pinned: PhantomPinned,
}

impl<Q: AsRef<Queue<T, S>>, T, S: SyncPrimitives> Node<Q, T, S> {
    pub const fn new(queue: Q, data: T) -> Self {
        Self {
            queue,
            node: UnsafePinned::new(NodeWithData {
                link: NodeLink::new(),
                data,
            }),
            _pinned: PhantomPinned,
        }
    }

    pub const fn queue(&self) -> &Q {
        &self.queue
    }

    pub fn raw_state(&self) -> RawNodeState {
        unsafe { self.node.get().as_ref().unwrap().link.state().into() }
    }

    pub fn state(self: Pin<&mut Self>) -> NodeState<'_, T, S> {
        unsafe { self.get_unchecked_mut().state_impl() }
    }

    fn state_impl(&mut self) -> NodeState<'_, T, S> {
        let node = unsafe { NonNull::new_unchecked(&mut *self.node.get()) };
        match self.raw_state() {
            RawNodeState::Unqueued => NodeState::Unqueued(NodeUnqueued {
                node,
                queue: self.queue.as_ref(),
            }),
            RawNodeState::Queued => {
                let locked = self.queue.as_ref().lock();
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
        }
    }
}

impl<Q: AsRef<Queue<T, S>>, T, S: SyncPrimitives> Drop for Node<Q, T, S> {
    fn drop(&mut self) {
        if let NodeState::Queued(queued) = self.state_impl() {
            queued.dequeue()
        }
    }
}

macro_rules! node_data {
    ($node:ident $(, $lf:lifetime)?) => {
        impl<T, S: SyncPrimitives> $node<'_, $($lf,)? T, S> {
            pub fn data_pinned(&mut self) -> Pin<&mut T> {
                unsafe { Pin::new_unchecked(&mut self.node.as_mut().data) }
            }
        }

        impl<T, S: SyncPrimitives> Deref for $node<'_, $($lf,)? T, S> {
            type Target = T;

            fn deref(&self) -> &Self::Target {
                unsafe { &self.node.as_ref().data }
            }
        }

        impl<T: Unpin, S: SyncPrimitives> DerefMut for $node<'_, $($lf,)? T, S> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                unsafe { &mut self.node.as_mut().data }
            }
        }
    };
}
pub(crate) use node_data;

pub struct NodeUnqueued<'a, T, S: SyncPrimitives> {
    node: NonNull<NodeWithData<T, S>>,
    queue: &'a Queue<T, S>,
}

impl<'a, T, S: SyncPrimitives> NodeUnqueued<'a, T, S> {
    #[inline]
    pub fn enqueue<I: FnOnce(Pin<&mut T>)>(mut self: Pin<&mut Self>, init: I) {
        init(self.data_pinned());
        let node = self.node.cast();
        let new_tail = |tail| (node.as_ptr(), Some(tail));
        unsafe { self.queue.as_ref().enqueue(node, new_tail) };
    }

    #[cfg(feature = "queue-state")]
    #[inline]
    pub fn fetch_update_queue_state_or_enqueue<
        F: FnMut(QueueState) -> Option<QueueState>,
        I: FnMut(Option<QueueState>, Pin<&mut T>),
    >(
        self: Pin<&mut Self>,
        mut f: F,
        mut init: I,
    ) -> bool {
        let node = self.node.cast();
        let data = || unsafe { Pin::new_unchecked(&mut (*self.node.as_ptr()).data) };
        let new_tail = |tail| match StateOrTail::from(tail) {
            StateOrTail::State(state) => match f(state) {
                Some(new_state) => (StateOrTail::State(new_state).into(), None),
                None => {
                    init(Some(state), data());
                    (node.as_ptr(), Some(ptr::null_mut()))
                }
            },
            StateOrTail::Tail(tail) => {
                init(None, data());
                (node.as_ptr(), Some(tail.as_ptr()))
            }
        };
        !unsafe { self.queue.as_ref().enqueue(node, new_tail) }
    }
}

node_data!(NodeUnqueued);

pub struct NodeQueued<'a, T, S: SyncPrimitives> {
    node: NonNull<NodeWithData<T, S>>,
    locked: LockedQueue<'a, T, S>,
}

impl<T, S: SyncPrimitives> NodeQueued<'_, T, S> {
    pub fn dequeue(mut self) {
        unsafe { self.locked.remove(&self.node.as_ref().link, ptr::null_mut) };
    }

    #[cfg(feature = "queue-state")]
    pub fn dequeue_and_try_set_queue_state<F: FnOnce() -> QueueState>(mut self, state: F) -> bool {
        let reset_tail = || StateOrTail::State(state()).into();
        unsafe { self.locked.remove(&self.node.as_ref().link, reset_tail) }
    }
}

node_data!(NodeQueued);

pub struct NodeDequeued<'a, T, S: SyncPrimitives> {
    node: NonNull<NodeWithData<T, S>>,
    _queue: PhantomData<&'a Queue<T, S>>,
}

node_data!(NodeDequeued);
