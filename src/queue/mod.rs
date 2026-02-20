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

unsafe impl<T, S: SyncPrimitives> Send for Queue<T, S> {}
unsafe impl<T, S: SyncPrimitives> Sync for Queue<T, S> {}

impl<T, S: SyncPrimitives> Queue<T, S> {
    const fn new_impl(tail: *mut NodeLink<S>) -> Self {
        Self {
            tail: AtomicPtr::new(tail),
            head_sentinel: NodeLink::new(),
            _padding: crossbeam_utils::CachePadded::new(()),
            mutex: S::Mutex::INIT,
            head: Cell::new(None),
        }
    }

    #[inline]
    pub const fn new() -> Self {
        Self::new_impl(ptr::null_mut())
    }

    #[cfg(feature = "queue-state")]
    #[inline]
    pub const fn with_state(state: QueueState) -> Self {
        Self::new_impl(state_to_ptr(state))
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
    pub fn fetch_update_state<F: FnMut(QueueState) -> Option<QueueState>>(
        &self,
        mut f: F,
    ) -> Result<QueueState, Option<QueueState>> {
        let mut tail = self.tail.load(Relaxed);
        while let StateOrTail::State(state) = tail.into() {
            let Some(new_state) = f(state) else {
                return Err(Some(state));
            };
            let new_tail = StateOrTail::State(new_state).into();
            match self.tail.compare_exchange(tail, new_tail, SeqCst, Acquire) {
                Ok(_) => return Ok(state),
                Err(ptr) => tail = ptr,
            }
        }
        Err(None)
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
    pub fn fetch_update_state_or_lock<F: FnMut(QueueState) -> Option<QueueState>>(
        &self,
        mut f: F,
    ) -> Result<QueueState, LockedQueue<'_, T, S>> {
        if let Ok(state) = self.fetch_update_state(&mut f) {
            return Ok(state);
        }
        let lock = self.lock();
        self.fetch_update_state(&mut f).map_err(|_| lock)
    }

    unsafe fn enqueue(
        &self,
        node: NonNull<NodeLink<S>>,
        mut new_tail: impl FnMut(
            *mut NodeLink<S>,
        ) -> Option<(*mut NodeLink<S>, Option<*mut NodeLink<S>>)>,
    ) -> Option<*mut NodeLink<S>> {
        let mut tail = self.tail.load(Relaxed);
        let prev = loop {
            let Some((new_tail, prev)) = new_tail(tail) else {
                return Some(tail);
            };
            let prev =
                prev.map(|p| NonNull::new(p).unwrap_or_else(|| NonNull::from(&self.head_sentinel)));
            if let Some(prev) = prev {
                unsafe { node.as_ref() }.prev.set(prev);
            }
            match (self.tail).compare_exchange_weak(tail, new_tail, SeqCst, Relaxed) {
                Ok(_) if prev.is_none() => return None,
                Ok(_) => break unsafe { prev.unwrap().as_ref() },
                Err(ptr) => tail = ptr,
            }
        };
        (unsafe { node.as_ref() }.state).store(NodeLinkState::Queued as _, Relaxed);
        prev.next.store(node.as_ptr().cast(), SeqCst);
        if prev.state() == NodeLinkState::Parked {
            #[cold]
            fn unpark<S: SyncPrimitives>(prev: &NodeLink<S>) {
                prev.parker.unpark();
            }
            unpark(prev);
        }
        Some(tail)
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
    #[cfg(not(feature = "queue-state"))]
    fn tail(&self) -> Option<NonNull<NodeLink<S>>> {
        NonNull::new(self.queue.tail.load(SeqCst))
    }

    #[cfg(feature = "queue-state")]
    fn tail(&self) -> Option<NonNull<NodeLink<S>>> {
        match self.queue.tail.load(SeqCst).into() {
            StateOrTail::Tail(tail) => Some(tail),
            _ => None,
        }
    }

    pub fn dequeue(&mut self) -> Option<NodeDequeuing<'a, '_, T, S>> {
        let node = (self.queue.head_sentinel).wait_for_next();
        Some(NodeDequeuing { node, locked: self })
    }

    pub fn pop(&mut self) -> Option<NodeDequeuing<'a, '_, T, S>> {
        let node = self.tail()?.cast();
        Some(NodeDequeuing { node, locked: self })
    }

    pub fn drain(self) -> Drain<'a, T, S> {
        Drain::new(self, ptr::null_mut())
    }

    #[cfg(feature = "queue-state")]
    pub fn drain_set_state(self, state: QueueState) -> Drain<'a, T, S> {
        Drain::new(self, StateOrTail::State(state).into())
    }

    unsafe fn remove(
        &mut self,
        node: &NodeLink<S>,
        reset_tail: impl FnOnce() -> *mut NodeLink<S>,
    ) -> bool {
        if let Some(next) = NonNull::new(node.next.load(Acquire)) {
            node.unlink(next);
            return false;
        }
        let prev = node.prev();
        prev.next.store(ptr::null_mut(), Relaxed);
        let (new_tail, empty) = if ptr::from_ref(prev) == ptr::from_ref(&self.queue.head_sentinel) {
            (reset_tail(), true)
        } else {
            #[cfg(not(feature = "queue-state"))]
            let new_tail = node.prev.get().as_ptr();
            #[cfg(feature = "queue-state")]
            let new_tail = StateOrTail::Tail(node.prev.get()).into();
            (new_tail, false)
        };
        #[cfg(not(feature = "queue-state"))]
        let node_ptr = ptr::from_ref(node).cast_mut();
        #[cfg(feature = "queue-state")]
        let node_ptr = StateOrTail::Tail(NonNull::from(node)).into();
        if (self.queue.tail)
            .compare_exchange(node_ptr, new_tail, SeqCst, Relaxed)
            .is_err()
        {
            node.unlink(node.wait_for_next());
            return false;
        }
        node.state.store(NodeLinkState::Dequeued as _, Release);
        empty
    }
}

impl<T, S: SyncPrimitives> Drop for LockedQueue<'_, T, S> {
    fn drop(&mut self) {
        unsafe { self.queue.mutex.unlock(ManuallyDrop::take(&mut self.guard)) };
    }
}

impl<T, S: SyncPrimitives> Deref for LockedQueue<'_, T, S> {
    type Target = Queue<T, S>;

    fn deref(&self) -> &Self::Target {
        self.queue
    }
}

pub struct NodeDequeuing<'locked, 'a, T, S: SyncPrimitives> {
    node: NonNull<NodeLink<S>>,
    locked: &'a mut LockedQueue<'locked, T, S>,
}

impl<T, S: SyncPrimitives> NodeDequeuing<'_, '_, T, S> {
    pub fn data_pinned(&mut self) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(&mut (*self.node.cast::<NodeWithData<T, S>>().as_ptr()).data) }
    }

    pub fn requeue(self) {
        mem::forget(self);
    }

    #[cfg(feature = "queue-state")]
    pub fn try_set_queue_state<F: FnOnce() -> QueueState>(self, state: F) -> bool {
        let reset_tail = || StateOrTail::State(state()).into();
        unsafe { self.locked.remove(self.node.as_ref(), reset_tail) }
    }
}

impl<T, S: SyncPrimitives> Drop for NodeDequeuing<'_, '_, T, S> {
    fn drop(&mut self) {
        unsafe { self.locked.remove(self.node.as_ref(), ptr::null_mut) };
    }
}

impl<T, S: SyncPrimitives> Deref for NodeDequeuing<'_, '_, T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.node.cast::<NodeWithData<T, S>>().as_ptr()).data }
    }
}
impl<T: Unpin, S: SyncPrimitives> DerefMut for NodeDequeuing<'_, '_, T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut (*self.node.cast::<NodeWithData<T, S>>().as_ptr()).data }
    }
}
