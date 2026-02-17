use core::{
    cell::Cell,
    mem,
    mem::ManuallyDrop,
    ops::{Deref, Not},
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering::*},
};

use crate::sync::{mutex::Mutex, DefaultSyncPrimitives, SyncPrimitives};

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
        Self::new_impl(state_to_ptr(state))
    }

    #[cfg(feature = "queue-state")]
    #[inline]
    pub fn state(&self) -> Option<usize> {
        ptr_to_state(self.tail.load(SeqCst))
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
        while let Some(state) = ptr_to_state(tail) {
            match (self.tail).compare_exchange(tail, state_to_ptr(f(state)), SeqCst, Acquire) {
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
            head: self.head.get(),
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

    unsafe fn enqueue(&self, node: &NodeLink<S>) -> bool {
        let _ = node;
        todo!()
    }

    unsafe fn remove(&self, node: &NodeLink<S>) {
        let _ = node;
        todo!()
    }
}

impl<T, S: SyncPrimitives> Default for Queue<T, S> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct LockedQueue<'a, T, S: SyncPrimitives> {
    queue: &'a Queue<T, S>,
    head: Option<NonNull<NodeWithData<T, S>>>,
    guard: ManuallyDrop<MutexGuard<'a, S>>,
}

impl<'a, T, S: SyncPrimitives> LockedQueue<'a, T, S> {
    pub fn dequeue(&mut self) -> Option<Dequeue<'_, T, S>> {
        todo!()
    }

    pub fn pop(&mut self) -> Option<Pop<'_, T, S>> {
        todo!()
    }

    pub fn drain(self) -> Drain<'a, T, S> {
        todo!()
    }
}

impl<T, S: SyncPrimitives> Drop for LockedQueue<'_, T, S> {
    fn drop(&mut self) {
        self.queue.head.set(self.head);
        unsafe { self.queue.mutex.unlock(ManuallyDrop::take(&mut self.guard)) };
    }
}

pub struct Dequeue<'a, T, S: SyncPrimitives> {
    head: NonNull<NodeWithData<T, S>>,
    _guard: &'a mut LockedQueue<'a, T, S>,
}

impl<T, S: SyncPrimitives> Dequeue<'_, T, S> {
    pub fn data(&self) -> *mut T {
        unsafe { &raw mut (*self.head.as_ptr()).data }
    }

    pub fn requeue(self) {
        mem::forget(self);
    }

    #[cfg(feature = "queue-state")]
    pub fn set_queue_state<F>(self, state: F) -> bool {
        todo!()
    }
}

impl<T, S: SyncPrimitives> Drop for Dequeue<'_, T, S> {
    fn drop(&mut self) {
        todo!()
    }
}

impl<T: Sync, S: SyncPrimitives> Deref for Dequeue<'_, T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &unsafe { self.head.as_ref() }.data
    }
}

pub struct Pop<'a, T, S: SyncPrimitives> {
    head: NonNull<NodeWithData<T, S>>,
    _guard: &'a mut LockedQueue<'a, T, S>,
}

impl<T, S: SyncPrimitives> Pop<'_, T, S> {
    pub fn data(&self) -> *mut T {
        unsafe { &raw mut (*self.head.as_ptr()).data }
    }

    pub fn requeue(self) {
        mem::forget(self);
    }

    #[cfg(feature = "queue-state")]
    pub fn set_queue_state<F>(self, state: F) -> bool {
        todo!()
    }
}

impl<T, S: SyncPrimitives> Drop for Pop<'_, T, S> {
    fn drop(&mut self) {
        todo!()
    }
}

impl<T: Sync, S: SyncPrimitives> Deref for Pop<'_, T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &unsafe { self.head.as_ref() }.data
    }
}
