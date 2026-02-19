use core::{
    cell::Cell,
    mem,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut, Not},
    pin::Pin,
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering::*},
};

use crate::sync::{DefaultSyncPrimitives, SyncPrimitives, mutex::Mutex, parker::Parker};

mod drain;
mod node;
#[cfg(feature = "queue-state")]
mod state;

pub use drain::*;
pub use node::*;
#[cfg(feature = "queue-state")]
pub use state::*;

type MutexGuard<'a, S> = <<S as SyncPrimitives>::Mutex as Mutex>::Guard<'a>;

#[repr(C)]
pub struct Queue<T, S: SyncPrimitives = DefaultSyncPrimitives> {
    tail: AtomicPtr<NodeLink<S>>,
    head_sentinel: NodeLink<S>,
    _padding: crossbeam_utils::CachePadded<()>,
    mutex: S::Mutex,
    head: Cell<Option<NonNull<NodeWithData<T, S>>>>,
}

impl<T, S: SyncPrimitives> Queue<T, S> {
    fn new_impl(tail: *mut NodeLink<S>) -> Self {
        Self {
            tail: AtomicPtr::new(tail),
            head_sentinel: NodeLink::new(),
            _padding: crossbeam_utils::CachePadded::new(()),
            mutex: S::Mutex::INIT,
            head: Cell::new(None),
        }
    }

    #[inline]
    pub fn new() -> Self {
        Self::new_impl(ptr::null_mut())
    }

    #[cfg(feature = "queue-state")]
    #[inline]
    pub fn with_state(state: QueueState) -> Self {
        Self::new_impl(StateOrTail::State(state).into())
    }

    #[cfg(feature = "queue-state")]
    #[inline]
    pub fn state(&self) -> Option<usize> {
        match self.tail.load(SeqCst).into() {
            StateOrTail::State(state) => Some(state),
            _ => None,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "queue-state")]
        return self.state().is_none();
        #[cfg(not(feature = "queue-state"))]
        return self.tail.load(SeqCst).is_null();
    }

    #[cfg(feature = "queue-state")]
    pub fn fetch_update_state<F: FnMut(QueueState) -> QueueState>(&self, mut f: F) -> bool {
        let mut tail = self.tail.load(Relaxed);
        while let StateOrTail::State(state) = tail.into() {
            let new = StateOrTail::State(f(state)).into();
            match self.tail.compare_exchange(tail, new, SeqCst, Acquire) {
                Ok(_) => return true,
                Err(ptr) => tail = ptr,
            }
        }
        false
    }

    #[inline]
    pub fn lock(&self) -> LockedQueue<'_, T, S> {
        LockedQueue {
            queue: self,
            guard: ManuallyDrop::new(self.mutex.lock()),
        }
    }

    #[inline]
    pub fn is_empty_or_lock(&self) -> Option<LockedQueue<'_, T, S>> {
        self.is_empty().not().then(|| self.lock())
    }

    #[cfg(feature = "queue-state")]
    pub fn fetch_update_state_or_lock<F: FnMut(QueueState) -> QueueState>(
        &self,
        mut f: F,
    ) -> Option<LockedQueue<'_, T, S>> {
        if self.fetch_update_state(&mut f) {
            return None;
        }
        let lock = self.lock();
        self.fetch_update_state(&mut f).not().then_some(lock)
    }

    unsafe fn enqueue(
        &self,
        node: NonNull<NodeLink<S>>,
        mut new_tail: impl FnMut(*mut NodeLink<S>) -> (*mut NodeLink<S>, Option<*mut NodeLink<S>>),
    ) -> bool {
        let mut tail = self.tail.load(Relaxed);
        let prev = loop {
            let (new_tail, prev) = new_tail(tail);
            let prev =
                prev.map(|p| NonNull::new(p).unwrap_or_else(|| NonNull::from(&self.head_sentinel)));
            if let Some(prev) = prev {
                unsafe { node.as_ref() }.prev.set(prev);
            }
            match (self.tail).compare_exchange_weak(tail, new_tail, SeqCst, Relaxed) {
                Ok(_) if prev.is_none() => return false,
                Ok(_) => break unsafe { prev.unwrap().as_ref() },
                Err(ptr) => tail = ptr,
            }
        };
        (unsafe { node.as_ref() }.state).store(NodeLinkState::Queued as _, Relaxed);
        prev.next.store(node.as_ptr(), SeqCst);
        if prev.state() == NodeLinkState::Parked {
            #[cold]
            fn unpark<S: SyncPrimitives>(prev: &NodeLink<S>) {
                prev.parker.unpark();
            }
            unpark(prev);
        }
        true
    }
}

impl<T, S: SyncPrimitives> Default for Queue<T, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, S: SyncPrimitives> AsRef<Self> for Queue<T, S> {
    fn as_ref(&self) -> &Self {
        self
    }
}

pub struct LockedQueue<'a, T, S: SyncPrimitives> {
    queue: &'a Queue<T, S>,
    guard: ManuallyDrop<MutexGuard<'a, S>>,
}

impl<'a, T, S: SyncPrimitives> LockedQueue<'a, T, S> {
    pub fn dequeue(&mut self) -> Option<NodeDequeuing<'a, '_, T, S>> {
        let tail = || NonNull::new(self.queue.tail.load(SeqCst));
        let node = self.queue.head_sentinel.find_next(tail)?.cast();
        Some(NodeDequeuing { node, locked: self })
    }

    pub fn pop(&mut self) -> Option<NodeDequeuing<'a, '_, T, S>> {
        let node = NonNull::new(self.queue.tail.load(SeqCst))?.cast();
        Some(NodeDequeuing { node, locked: self })
    }

    pub fn drain(self) -> Drain<'a, T, S> {
        Drain::new(self)
    }

    unsafe fn remove(
        &mut self,
        node: &NodeLink<S>,
        reset_tail: impl FnOnce() -> *mut NodeLink<S>,
    ) -> bool {
        if let Some(next) = NonNull::new(node.next.load(Relaxed)) {
            return node.unlink(Some(next));
        }
        node.prev().wait_for_next(Some(NodeLinkState::Queued));
        node.prev().next.store(ptr::null_mut(), Relaxed);
        let node_ptr = ptr::from_ref(node).cast_mut();
        match (self.queue.tail).compare_exchange(node_ptr, reset_tail(), SeqCst, Relaxed) {
            Ok(_) => node.unlink(None),
            Err(_) => node.unlink(Some(node.wait_for_next(None))),
        }
    }
}

impl<T, S: SyncPrimitives> Drop for LockedQueue<'_, T, S> {
    fn drop(&mut self) {
        unsafe { self.queue.mutex.unlock(ManuallyDrop::take(&mut self.guard)) };
    }
}

pub struct NodeDequeuing<'locked, 'a, T, S: SyncPrimitives> {
    node: NonNull<NodeWithData<T, S>>,
    locked: &'a mut LockedQueue<'locked, T, S>,
}

impl<T, S: SyncPrimitives> NodeDequeuing<'_, '_, T, S> {
    pub fn data(&self) -> *mut T {
        unsafe { &raw mut (*self.node.as_ptr()).data }
    }

    pub fn requeue(self) {
        mem::forget(self);
    }

    #[cfg(feature = "queue-state")]
    pub fn try_set_queue_state<F: FnOnce() -> QueueState>(self, state: F) -> bool {
        let node = unsafe { self.node.cast::<NodeLink<S>>().as_ref() };
        let reset_tail = || StateOrTail::State(state()).into();
        unsafe { self.locked.remove(node, reset_tail) }
    }
}

impl<T, S: SyncPrimitives> Drop for NodeDequeuing<'_, '_, T, S> {
    fn drop(&mut self) {
        let node = unsafe { self.node.cast::<NodeLink<S>>().as_ref() };
        unsafe { self.locked.remove(node, ptr::null_mut) };
    }
}

node_data!(NodeDequeuing, '_);
