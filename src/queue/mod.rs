#[cfg(feature = "std")]
extern crate std;

use core::{
    hint,
    marker::PhantomData,
    mem,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut, Not},
    pin::Pin,
    ptr,
    ptr::NonNull,
    sync::{
        atomic,
        atomic::{AtomicPtr, Ordering::*},
    },
};

use crate::sync::{DefaultSyncPrimitives, SyncPrimitives, mutex::Mutex, parker::Parker};

mod drain;
#[cfg(feature = "queue-state")]
pub(crate) mod state;

pub use drain::*;
#[cfg(feature = "queue-state")]
pub use state::*;

use crate::node::{NodeLink, NodeWithData};

type MutexGuard<'a, S> = <<S as SyncPrimitives>::Mutex as Mutex>::Guard<'a>;

pub trait QueueRef {
    type NodeData;
    type SyncPrimitives: SyncPrimitives;

    fn queue(&self) -> &Queue<Self::NodeData, Self::SyncPrimitives>;
}

impl<T, S: SyncPrimitives> QueueRef for &Queue<T, S> {
    type NodeData = T;
    type SyncPrimitives = S;
    fn queue(&self) -> &Queue<Self::NodeData, Self::SyncPrimitives> {
        self
    }
}

#[cfg(feature = "std")]
impl<T, S: SyncPrimitives> QueueRef for std::sync::Arc<Queue<T, S>> {
    type NodeData = T;
    type SyncPrimitives = S;
    fn queue(&self) -> &Queue<Self::NodeData, Self::SyncPrimitives> {
        self
    }
}

#[repr(C)]
pub struct Queue<T, S: SyncPrimitives = DefaultSyncPrimitives> {
    sentinel: NodeLink,
    #[cfg(not(target_arch = "x86_64"))]
    parked_node: AtomicPtr<NodeLink>,
    mutex: S::Mutex,
    parker: S::Parker,
    _phantom: PhantomData<T>,
}

unsafe impl<T, S: SyncPrimitives> Send for Queue<T, S> {}
unsafe impl<T, S: SyncPrimitives> Sync for Queue<T, S> {}

impl<T, S: SyncPrimitives> Queue<T, S> {
    const fn new_impl(tail: *mut NodeLink) -> Self {
        Self {
            sentinel: NodeLink {
                prev: AtomicPtr::new(tail),
                next: AtomicPtr::new(ptr::null_mut()),
            },
            #[cfg(not(target_arch = "x86_64"))]
            parked_node: AtomicPtr::new(ptr::null_mut()),
            mutex: S::Mutex::INIT,
            parker: S::Parker::INIT,
            _phantom: PhantomData,
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
    pub fn state(&self) -> Option<QueueState> {
        match self.sentinel.prev.load(SeqCst).into() {
            StateOrTail::State(state) => Some(state),
            _ => None,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "queue-state")]
        return self.state().is_some();
        #[cfg(not(feature = "queue-state"))]
        return self.sentinel.prev.load(SeqCst).is_null();
    }

    #[cfg(feature = "queue-state")]
    #[inline]
    pub fn fetch_update_state<F: FnMut(QueueState) -> Option<QueueState>>(
        &self,
        mut f: F,
    ) -> Result<QueueState, Option<QueueState>> {
        let mut tail = self.sentinel.prev.load(Relaxed);
        while let StateOrTail::State(state) = tail.into() {
            let Some(new_state) = f(state) else {
                return Err(Some(state));
            };
            let new_tail = StateOrTail::State(new_state).into();
            match (self.sentinel.prev).compare_exchange_weak(tail, new_tail, SeqCst, Acquire) {
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
    #[inline]
    pub fn fetch_update_state_or_lock<F: FnMut(QueueState) -> QueueState>(
        &self,
        mut f: F,
    ) -> Option<LockedQueue<'_, T, S>> {
        self.fetch_update_state(|s| Some(f(s))).err()?;
        let lock = self.lock();
        self.fetch_update_state(|s| Some(f(s))).err()?;
        Some(lock)
    }

    #[cfg(feature = "queue-state")]
    #[inline]
    pub fn fetch_update_state_with_lock<
        'a,
        F: FnMut(QueueState) -> QueueState,
        L: FnOnce(LockedQueue<'a, T, S>) -> R,
        R,
    >(
        &'a self,
        mut f: F,
        locked_fallback: L,
    ) {
        if self.fetch_update_state(|s| Some(f(s))).is_err() {
            self.fetch_update_state_locked(f, locked_fallback)
        }
    }

    #[cfg(feature = "queue-state")]
    #[cold]
    #[inline(never)]
    pub fn fetch_update_state_locked<
        'a,
        F: FnMut(QueueState) -> QueueState,
        L: FnOnce(LockedQueue<'a, T, S>) -> R,
        R,
    >(
        &'a self,
        mut f: F,
        locked_fallback: L,
    ) {
        let lock = self.lock();
        if self.fetch_update_state(|s| Some(f(s))).is_err() {
            locked_fallback(lock);
        }
    }

    pub(crate) unsafe fn enqueue(
        &self,
        node: NonNull<NodeLink>,
        mut new_tail: impl FnMut(*mut NodeLink) -> Option<(*mut NodeLink, Option<*mut NodeLink>)>,
    ) -> Option<*mut NodeLink> {
        let mut backoff = 0;
        #[cold]
        #[inline(never)]
        fn spin(backoff: usize) -> usize {
            for _ in 0..1 << backoff {
                hint::spin_loop();
            }
            backoff + if backoff < 6 { 1 } else { 0 }
        }
        let mut tail = self.sentinel.prev.load(Relaxed);
        let prev = loop {
            let Some((new_tail, prev)) = new_tail(tail) else {
                atomic::fence(Acquire);
                unsafe { node.as_ref().prev.store(ptr::null_mut(), Relaxed) }
                return Some(tail);
            };
            let prev = prev.map(|p| NonNull::new(p).unwrap_or_else(|| (&self.sentinel).into()));
            let prev_ptr = prev.map_or(ptr::null_mut(), NonNull::as_ptr);
            unsafe { node.as_ref().prev.store(prev_ptr, Relaxed) }
            if ((self.sentinel.prev).compare_exchange_weak(tail, new_tail, SeqCst, Relaxed)).is_ok()
            {
                break prev?;
            }
            backoff = spin(backoff);
            tail = self.sentinel.prev.load(Relaxed);
        };
        #[cfg(not(target_arch = "x86_64"))]
        (unsafe { prev.as_ref() }.next).store(node.as_ptr().cast(), SeqCst);
        #[cfg(not(target_arch = "x86_64"))]
        if self.parked_node.load(SeqCst) == prev.as_ptr() {
            self.unpark();
        }
        #[cfg(target_arch = "x86_64")]
        if unsafe { !(prev.as_ref().next.swap(node.as_ptr().cast(), SeqCst)).is_null() } {
            self.unpark()
        }
        Some(tail)
    }

    #[cold]
    #[inline(never)]
    fn unpark(&self) {
        self.parker.unpark();
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

pub struct LockedQueue<'a, T, S: SyncPrimitives = DefaultSyncPrimitives> {
    queue: &'a Queue<T, S>,
    guard: ManuallyDrop<MutexGuard<'a, S>>,
}

impl<'a, T, S: SyncPrimitives> LockedQueue<'a, T, S> {
    #[inline(always)]
    fn tail(&self) -> Option<NonNull<NodeLink>> {
        let tail = self.sentinel.prev.load(SeqCst);
        #[cfg(not(feature = "queue-state"))]
        return NonNull::new(tail);
        #[cfg(feature = "queue-state")]
        return match tail.into() {
            StateOrTail::Tail(tail) => Some(tail),
            _ => None,
        };
    }

    #[inline(always)]
    fn get_next(&mut self, node: &NodeLink) -> NonNull<NodeLink> {
        if let Some(next) = node.next() {
            return next;
        }
        self.wait_for_next(node)
    }

    #[cold]
    #[inline(never)]
    fn wait_for_next(&mut self, node: &NodeLink) -> NonNull<NodeLink> {
        for _ in 0..S::SPIN_BEFORE_PARK {
            hint::spin_loop();
            if let Some(next) = node.next() {
                return next;
            }
        }
        let node_ptr = ptr::from_ref(node).cast_mut();
        #[cfg(target_arch = "x86_64")]
        if let Err(next) = (node.next).compare_exchange(
            ptr::null_mut(),
            ptr::without_provenance_mut(1),
            Relaxed,
            SeqCst,
        ) {
            return unsafe { NonNull::new_unchecked(next) };
        }
        #[cfg(not(target_arch = "x86_64"))]
        self.parked_node.store(node_ptr, SeqCst);
        loop {
            if let Some(next) = node.next() {
                #[cfg(not(target_arch = "x86_64"))]
                self.parked_node.store(ptr::null_mut(), SeqCst);
                return next;
            }
            unsafe { self.parker.park() };
        }
    }

    #[inline]
    pub fn dequeue(&mut self) -> Option<NodeDequeuing<'a, '_, T, S>> {
        self.tail()?;
        let node = self.get_next(&self.queue.sentinel);
        Some(NodeDequeuing { node, locked: self })
    }

    #[inline]
    pub fn pop(&mut self) -> Option<NodeDequeuing<'a, '_, T, S>> {
        let node = self.tail()?;
        let prev_next = self.get_next(unsafe { node.as_ref() }.prev());
        debug_assert_eq!(prev_next, node);
        Some(NodeDequeuing { node, locked: self })
    }

    #[inline]
    pub fn drain(self) -> Drain<'a, T, S> {
        Drain::new(self, ptr::null_mut())
    }

    #[cfg(feature = "queue-state")]
    pub fn drain_try_set_state(self, state: QueueState) -> Drain<'a, T, S> {
        Drain::new(self, StateOrTail::State(state).into())
    }

    pub(crate) unsafe fn remove(&mut self, node: &NodeLink, new_tail: *mut NodeLink) -> bool {
        if let Some(next) = node.next() {
            node.unlink(next);
            return false;
        }
        let prev = unsafe { NonNull::new_unchecked(node.prev.load(Relaxed)) };
        unsafe { *prev.as_ref().next.as_ptr() = ptr::null_mut() };
        let is_head = prev == (&self.sentinel).into();
        #[cfg(not(feature = "queue-state"))]
        let new_tail = if is_head { new_tail } else { prev.as_ptr() };
        #[cfg(feature = "queue-state")]
        let new_tail = if is_head {
            new_tail
        } else {
            StateOrTail::Tail(prev).into()
        };
        #[cfg(not(feature = "queue-state"))]
        let node_ptr = ptr::from_ref(node).cast_mut();
        #[cfg(feature = "queue-state")]
        let node_ptr = StateOrTail::Tail(NonNull::from(node)).into();
        if ((self.sentinel.prev).compare_exchange(node_ptr, new_tail, SeqCst, Relaxed)).is_err() {
            node.unlink(self.get_next(node));
            return false;
        }
        node.dequeue();
        is_head
    }
}

impl<T, S: SyncPrimitives> Drop for LockedQueue<'_, T, S> {
    #[inline]
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

pub struct NodeDequeuing<'locked, 'a, T, S: SyncPrimitives = DefaultSyncPrimitives> {
    node: NonNull<NodeLink>,
    locked: &'a mut LockedQueue<'locked, T, S>,
}

impl<T, S: SyncPrimitives> NodeDequeuing<'_, '_, T, S> {
    pub fn data_pinned(&mut self) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(&mut (*self.node.cast::<NodeWithData<T>>().as_ptr()).data) }
    }

    pub fn requeue(self) {
        mem::forget(self);
    }

    #[cfg(feature = "queue-state")]
    pub fn try_set_queue_state(self, state: QueueState) -> bool {
        let this = &mut *ManuallyDrop::new(self);
        unsafe { (this.locked).remove(this.node.as_ref(), StateOrTail::State(state).into()) }
    }
}

impl<T, S: SyncPrimitives> Drop for NodeDequeuing<'_, '_, T, S> {
    fn drop(&mut self) {
        unsafe { self.locked.remove(self.node.as_ref(), ptr::null_mut()) };
    }
}

impl<T, S: SyncPrimitives> Deref for NodeDequeuing<'_, '_, T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.node.cast::<NodeWithData<T>>().as_ptr()).data }
    }
}
impl<T: Unpin, S: SyncPrimitives> DerefMut for NodeDequeuing<'_, '_, T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut (*self.node.cast::<NodeWithData<T>>().as_ptr()).data }
    }
}
