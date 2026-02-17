#[cfg(feature = "nightly")]
use core::pin::UnsafePinned;
use core::{
    cell::Cell,
    hint::unreachable_unchecked,
    marker::PhantomPinned,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr,
    ptr::NonNull,
    sync::atomic::{
        AtomicPtr, AtomicUsize,
        Ordering::{Acquire, Release, SeqCst},
    },
};

#[cfg(not(feature = "nightly"))]
use crate::unsafe_pinned::UnsafePinned;
use crate::{
    queue::{MutexGuard, Queue, QueueState},
    sync::{parker::Parker, DefaultSyncPrimitives, SyncPrimitives},
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum NodeLinkState {
    Unqueued = 0b0000,
    Queued = 0b0001,
    Dequeued = 0b0010,
    DequeuedPendingNext = 0b0110,
    DequeuedParked = 0b1110,
}

impl NodeLinkState {
    #[inline(always)]
    unsafe fn from(value: usize) -> Self {
        match value {
            s if s == Self::Unqueued as _ => Self::Unqueued,
            s if s == Self::Queued as _ => Self::Queued,
            s if s == Self::Dequeued as _ => Self::Dequeued,
            s if s == Self::DequeuedPendingNext as _ => Self::DequeuedPendingNext,
            s if s == Self::DequeuedParked as _ => Self::DequeuedParked,
            _ => unsafe { unreachable_unchecked() },
        }
    }
}

#[repr(C)]
pub(crate) struct NodeLink<S: SyncPrimitives> {
    pub(crate) prev: Cell<Option<NonNull<NodeLink<S>>>>,
    pub(crate) next: AtomicPtr<NodeLink<S>>,
    pub(crate) state: AtomicUsize,
    pub(crate) parker: S::Parker,
}

impl<S: SyncPrimitives> NodeLink<S> {
    pub(crate) const fn new() -> Self {
        Self {
            prev: Cell::new(None),
            next: AtomicPtr::new(ptr::null_mut()),
            state: AtomicUsize::new(NodeLinkState::DequeuedPendingNext as _),
            parker: S::Parker::INIT,
        }
    }

    pub(crate) fn wait_for_next(&self) -> NonNull<NodeLink<S>> {
        if let Some(next) = NonNull::new(self.next.load(SeqCst)) {
            self.state.store(NodeLinkState::Dequeued as _, Release);
            return next;
        }
        self.state.store(NodeLinkState::DequeuedParked as _, SeqCst);
        loop {
            if let Some(next) = NonNull::new(self.next.load(SeqCst)) {
                self.state.store(NodeState::Dequeued as _, Release);
                return next;
            }
            unsafe { self.parker.park() };
        }
    }
}

#[repr(C)]
pub(crate) struct NodeWithData<T, S: SyncPrimitives> {
    pub(crate) link: NodeLink<S>,
    pub(crate) data: T,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NodeState {
    Unqueued = 0b00,
    Queued = 0b01,
    Dequeued = 0b10,
}

impl From<NodeLinkState> for NodeState {
    #[inline(always)]
    fn from(value: NodeLinkState) -> Self {
        match value as usize & 0b11 {
            s if s == NodeState::Unqueued as _ => NodeState::Unqueued,
            s if s == NodeState::Queued as _ => NodeState::Queued,
            s if s == NodeState::Dequeued as _ => NodeState::Dequeued,
            _ => unsafe { unreachable_unchecked() },
        }
    }
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

    fn link(&self) -> &NodeLink<S> {
        unsafe { &(*self.node.get()).link }
    }

    pub const fn data(&self) -> &T {
        unsafe { &(*self.node.get()).data }
    }

    pub fn state(&self) -> NodeState {
        unsafe { NodeLinkState::from(self.link().state.load(Acquire)) }.into()
    }

    pub fn enqueue(self: Pin<&mut Self>) -> bool {
        unsafe { self.queue.as_ref().enqueue(self.link()) }
    }

    #[cfg(feature = "queue-state")]
    pub fn fetch_update_queue_state_or_enqueue<F: FnMut(QueueState) -> QueueState>(&self, f: F) {
        todo!()
    }

    pub fn update(self: Pin<&mut Self>) -> Option<Update<'_, T, S>> {
        todo!()
    }
}

impl<Q: AsRef<Queue<T, S>>, T, S: SyncPrimitives> Drop for Node<Q, T, S> {
    fn drop(&mut self) {
        unsafe { self.queue.as_ref().remove(self.link()) };
    }
}

pub struct Update<'a, T, S: SyncPrimitives + 'a> {
    data: &'a mut T,
    _guard: MutexGuard<'a, S>,
}

impl<'a, T, S: SyncPrimitives + 'a> Deref for Update<'a, T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<'a, T, S: SyncPrimitives + 'a> DerefMut for Update<'a, T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data
    }
}
