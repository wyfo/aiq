#[cfg(feature = "alloc")]
extern crate alloc;

use core::{
    hint,
    marker::PhantomData,
    mem,
    mem::ManuallyDrop,
    ops::{Deref, Not},
    ptr,
    ptr::NonNull,
};

use crate::{
    loom::{
        AtomicPtrExt,
        sync::{
            atomic,
            atomic::{AtomicPtr, Ordering::*},
        },
    },
    node::{NodeLink, RawNodeState, node_getters},
    sync::{DefaultSyncPrimitives, SyncPrimitives, mutex::Mutex, parker::Parker},
};

mod drain;
pub(crate) mod state;

pub use drain::*;
pub use state::*;

type MutexGuard<'a, SP> = <<SP as SyncPrimitives>::Mutex as Mutex>::Guard<'a>;

/// # Safety
///
/// For a given instance, [`Self::queue`] must always return a reference to the same queue.
pub unsafe trait QueueRef {
    type NodeData;
    type State: QueueState;
    type SyncPrimitives: SyncPrimitives;

    fn queue(&self) -> &Queue<Self::NodeData, Self::State, Self::SyncPrimitives>;
}

unsafe impl<T, S: QueueState, SP: SyncPrimitives> QueueRef for &Queue<T, S, SP> {
    type NodeData = T;
    type State = S;
    type SyncPrimitives = SP;
    fn queue(&self) -> &Queue<Self::NodeData, Self::State, Self::SyncPrimitives> {
        self
    }
}

#[cfg(feature = "alloc")]
unsafe impl<T, S: QueueState, SP: SyncPrimitives> QueueRef for alloc::sync::Arc<Queue<T, S, SP>> {
    type NodeData = T;
    type State = S;
    type SyncPrimitives = SP;
    fn queue(&self) -> &Queue<Self::NodeData, Self::State, Self::SyncPrimitives> {
        self
    }
}

#[macro_export]
macro_rules! queue_ref {
    (
        $ty:ident$(<$($lf:lifetime)? $(,)? $($arg:ident $(: $bound:path)?)? $(,)?>)?,
        NodeData = $data:ty,
        $(State = $state:ty,)?
        $(SyncPrimitives = $sync:ty,)?
        &self $(.$field:tt)+
    ) => {
        unsafe impl $(<
            $($lf,)?
            $($arg $(: $bound)?)?
        >)? $crate::queue::QueueRef for $ty $(<$($lf,)? $($arg,)?>)? {
            type NodeData = $data;
            type State = $crate::__private_queue_ref!(@state $($state)?);
            type SyncPrimitives = $crate::__private_queue_ref!(@sync $($sync)?);
            fn queue(&self) -> &$crate::Queue<Self::NodeData, Self::State, Self::SyncPrimitives> {
                &self $(.$field)+
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __private_queue_ref {
    (@lifetime '_,) => {};
    (@lifetime $lf:lifetime,) => { $lf, };
    (@sync) => { $crate::sync::DefaultSyncPrimitives };
    (@sync $ty:ty) => { $ty };
    (@state) => { () };
    (@state $ty:ty) => { $ty };
}

const HEAD_MARKER: *mut NodeLink = RawNodeState::Queued.into_ptr();

pub struct Queue<T, S: QueueState = (), SP: SyncPrimitives = DefaultSyncPrimitives> {
    tail: AtomicPtr<Tail<S>>,
    head: AtomicPtr<NodeLink>,
    #[cfg(not(any(target_arch = "x86_64", loom)))]
    parked_next: AtomicPtr<AtomicPtr<NodeLink>>,
    mutex: SP::Mutex,
    parker: SP::Parker,
    _node_data: PhantomData<T>,
}

unsafe impl<T: Send, S: QueueState + Send, SP: SyncPrimitives> Send for Queue<T, S, SP>
where
    SP::Mutex: Send,
    SP::Parker: Send,
{
}
unsafe impl<T: Send, S: QueueState + Send, SP: SyncPrimitives> Sync for Queue<T, S, SP>
where
    SP::Mutex: Sync,
    SP::Parker: Sync,
{
}

impl<T, S: QueueState, SP: SyncPrimitives> Queue<T, S, SP> {
    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    const fn new_impl(tail: *mut Tail<S>) -> Self {
        Self {
            tail: AtomicPtr::new(tail),
            head: AtomicPtr::new(ptr::null_mut()),
            #[cfg(not(any(target_arch = "x86_64", loom)))]
            parked_next: AtomicPtr::new(ptr::null_mut()),
            #[cfg(not(loom))]
            mutex: SP::Mutex::INIT,
            #[cfg(loom)]
            mutex: SP::Mutex::new(),
            #[cfg(not(loom))]
            parker: SP::Parker::INIT,
            #[cfg(loom)]
            parker: SP::Parker::new(),
            _node_data: PhantomData,
        }
    }

    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    #[inline]
    pub const fn new() -> Self {
        Self::new_impl(ptr::null_mut())
    }

    #[inline(always)]
    fn load_tail(&self) -> *mut Tail<S> {
        #[cfg(not(loom))]
        return self.tail.load(SeqCst);
        #[cfg(loom)]
        return (self.tail).compare_and_swap(ptr::null_mut(), ptr::null_mut(), SeqCst);
    }

    #[inline(always)]
    fn tail(&self) -> Option<NonNull<NodeLink>> {
        StateOrPtr::from(self.load_tail()).tail()
    }

    #[inline]
    pub fn state(&self) -> Option<S> {
        StateOrPtr::from(self.load_tail()).state()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.state().is_some()
    }

    #[inline]
    pub fn try_update_state(&self, current: S, new: S) -> Result<S, Option<S>> {
        let cur = StateOrPtr::State(current).into();
        let new = StateOrPtr::State(new).into();
        match self.tail.compare_exchange_weak(cur, new, SeqCst, Acquire) {
            Ok(_) => Ok(current),
            Err(ptr) => Err(StateOrPtr::from(ptr).state()),
        }
    }

    #[inline]
    pub fn fetch_update_state<F: FnMut(S) -> Option<S>>(&self, mut f: F) -> Result<S, Option<S>> {
        let mut tail = self.tail.load(Relaxed);
        while let StateOrPtr::State(state) = tail.into() {
            let Some(new_state) = f(state) else {
                return Err(Some(state));
            };
            let new_tail = StateOrPtr::State(new_state).into();
            match (self.tail).compare_exchange_weak(tail, new_tail, SeqCst, Acquire) {
                Ok(_) => return Ok(state),
                Err(ptr) => tail = ptr,
            }
        }
        Err(None)
    }

    #[inline]
    pub fn lock(&self) -> LockedQueue<'_, T, S, SP> {
        LockedQueue {
            queue: self,
            guard: ManuallyDrop::new(self.mutex.lock()),
        }
    }

    #[inline]
    pub fn is_empty_or_lock(&self) -> Option<LockedQueue<'_, T, S, SP>> {
        self.is_empty().not().then(|| self.lock())
    }

    #[inline]
    pub fn fetch_update_state_or_lock<F: FnMut(S) -> S>(
        &self,
        mut f: F,
    ) -> Option<LockedQueue<'_, T, S, SP>> {
        self.fetch_update_state(|s| Some(f(s))).err()?;
        let lock = self.lock();
        self.fetch_update_state(|s| Some(f(s))).err()?;
        Some(lock)
    }

    #[inline]
    pub fn fetch_update_state_with_lock<
        'a,
        F: FnMut(S) -> S,
        L: FnOnce(LockedQueue<'a, T, S, SP>) -> R,
        R,
    >(
        &'a self,
        mut f: F,
        locked_fallback: L,
    ) {
        if self.fetch_update_state(|s| Some(f(s))).is_err() {
            self.fetch_update_state_locked(f, locked_fallback);
        }
    }

    #[cold]
    #[inline(never)]
    pub fn fetch_update_state_locked<
        'a,
        F: FnMut(S) -> S,
        L: FnOnce(LockedQueue<'a, T, S, SP>) -> R,
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
        mut node: NonNull<NodeLink>,
        mut check_tail: impl FnMut(*mut Tail<S>) -> bool,
    ) -> bool {
        let mut tail = self.tail.load(Relaxed);
        let prev = loop {
            if !check_tail(tail) {
                atomic::fence(Acquire);
                unsafe { node.as_mut().prev.store_mut(ptr::null_mut()) };
                return false;
            }
            let prev = StateOrPtr::from(tail).tail();
            let prev_ptr = prev.map_or(HEAD_MARKER, NonNull::as_ptr);
            unsafe { node.as_mut().prev.store_mut(prev_ptr) };
            let new_tail = StateOrPtr::Ptr(node).into();
            match (self.tail).compare_exchange_weak(tail, new_tail, SeqCst, Relaxed) {
                Ok(_) => break prev,
                Err(t) => tail = t,
            }
        };
        let prev_next = NonNull::from(prev.map_or(&self.head, |p| unsafe { &p.as_ref().next }));
        if SP::Parker::NEVER_BLOCKS {
            unsafe { prev_next.as_ref() }.store(node.as_ptr(), Release);
        } else {
            #[cfg(not(any(target_arch = "x86_64", loom)))]
            unsafe { prev_next.as_ref() }.store(node.as_ptr(), SeqCst);
            #[cfg(not(any(target_arch = "x86_64", loom)))]
            if self.parked_next.load(SeqCst) == prev_next.as_ptr() {
                self.unpark();
            }
            #[cfg(any(target_arch = "x86_64", loom))]
            if unsafe { !(prev_next.as_ref().swap(node.as_ptr().cast(), Release)).is_null() } {
                self.unpark();
            }
        }
        true
    }

    #[cold]
    #[inline(never)]
    fn unpark(&self) {
        self.parker.unpark();
    }
}

impl<T, SP: SyncPrimitives> Queue<T, usize, SP> {
    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    #[inline]
    pub const fn with_state(state: usize) -> Self {
        Self::new_impl(state_to_ptr(state))
    }
}

impl<T, S: QueueState, SP: SyncPrimitives> Default for Queue<T, S, SP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, S: QueueState, SP: SyncPrimitives> AsRef<Self> for Queue<T, S, SP> {
    fn as_ref(&self) -> &Self {
        self
    }
}

pub struct LockedQueue<'a, T, S: QueueState = (), SP: SyncPrimitives = DefaultSyncPrimitives> {
    queue: &'a Queue<T, S, SP>,
    guard: ManuallyDrop<MutexGuard<'a, SP>>,
}

impl<'a, T, S: QueueState, SP: SyncPrimitives> LockedQueue<'a, T, S, SP> {
    #[inline(always)]
    fn get_next(&self, next: &AtomicPtr<NodeLink>) -> NonNull<NodeLink> {
        if let Some(next) = NonNull::new(next.load(Acquire)) {
            return next;
        }
        self.wait_for_next(next)
    }

    #[cold]
    #[inline(never)]
    fn wait_for_next(&self, next: &AtomicPtr<NodeLink>) -> NonNull<NodeLink> {
        if SP::Parker::NEVER_BLOCKS {
            loop {
                unsafe { self.parker.park() };
                if let Some(next) = NonNull::new(next.load(Acquire)) {
                    return next;
                }
            }
        }
        for _ in 0..SP::SPIN_BEFORE_PARK {
            hint::spin_loop();
            if let Some(next) = NonNull::new(next.load(Acquire)) {
                return next;
            }
        }
        #[cfg(not(any(target_arch = "x86_64", loom)))]
        (self.parked_next).store(ptr::from_ref(next).cast_mut(), SeqCst);
        #[cfg(any(target_arch = "x86_64", loom))]
        const PARKED: *mut NodeLink = ptr::without_provenance_mut(1);
        #[cfg(any(target_arch = "x86_64", loom))]
        if let Err(next) = next.compare_exchange(ptr::null_mut(), PARKED, Relaxed, Acquire) {
            return unsafe { NonNull::new_unchecked(next) };
        }
        loop {
            #[cfg(not(any(target_arch = "x86_64", loom)))]
            if let Some(next) = NonNull::new(next.load(SeqCst)) {
                self.parked_next.store(ptr::null_mut(), SeqCst);
                return next;
            }
            unsafe { self.parker.park() };
            #[cfg(any(target_arch = "x86_64", loom))]
            let next = next.load(Acquire);
            #[cfg(any(target_arch = "x86_64", loom))]
            if next != PARKED {
                return unsafe { NonNull::new_unchecked(next) };
            }
        }
    }

    #[inline]
    pub fn dequeue(&mut self) -> Option<Dequeue<'a, '_, T, S, SP>> {
        self.tail()?;
        let node = self.get_next(&self.queue.head);
        Some(Dequeue { node, locked: self })
    }

    #[inline]
    pub fn pop(&mut self) -> Option<Pop<'a, '_, T, S, SP>> {
        let node = self.tail()?;
        Some(Pop { node, locked: self })
    }

    #[inline]
    pub fn drain(self) -> Drain<'a, T, S, SP> {
        Drain::new(self, ptr::null_mut())
    }

    pub fn drain_try_set_state(self, state: S) -> Drain<'a, T, S, SP> {
        Drain::new(self, StateOrPtr::State(state).into())
    }

    pub(crate) unsafe fn remove(
        &mut self,
        node: &NodeLink,
        new_tail: *mut Tail<S>,
        wait_enqueued: bool,
    ) -> bool {
        unsafe { self.remove_with_prev(node, new_tail, wait_enqueued, node.prev.load(Relaxed)) }
    }

    #[inline]
    unsafe fn remove_with_prev(
        &mut self,
        node: &NodeLink,
        mut new_tail: *mut Tail<S>,
        wait_enqueued: bool,
        prev: *mut NodeLink,
    ) -> bool {
        let is_head = prev == HEAD_MARKER;
        let prev_next = if is_head {
            &self.head
        } else {
            unsafe { &(*prev).next }
        };
        if wait_enqueued {
            let prev_next = self.get_next(prev_next);
            debug_assert_eq!(prev_next, node.into());
        }
        let mut next = node.next();
        if next.is_none() {
            prev_next.store(ptr::null_mut(), Relaxed);
            if !is_head {
                new_tail = StateOrPtr::Ptr(unsafe { NonNull::new_unchecked(prev) }).into();
            }
            let node_ptr = StateOrPtr::Ptr(NonNull::from(node)).into();
            if ((self.tail).compare_exchange(node_ptr, new_tail, SeqCst, Relaxed)).is_err() {
                next = Some(self.get_next(&node.next));
            }
        }
        if let Some(next) = next {
            unsafe { next.as_ref().prev.store(prev, Relaxed) };
            prev_next.store(next.as_ptr(), Relaxed);
        }
        node.prev.store(RawNodeState::Dequeued.into_ptr(), Release);
        is_head
    }
}

impl<T, S: QueueState, SP: SyncPrimitives> Drop for LockedQueue<'_, T, S, SP> {
    #[inline]
    fn drop(&mut self) {
        unsafe { self.queue.mutex.unlock(ManuallyDrop::take(&mut self.guard)) };
    }
}

impl<T, S: QueueState, SP: SyncPrimitives> Deref for LockedQueue<'_, T, S, SP> {
    type Target = Queue<T, S, SP>;

    fn deref(&self) -> &Self::Target {
        self.queue
    }
}

pub struct Dequeue<'locked, 'a, T, S: QueueState = (), SP: SyncPrimitives = DefaultSyncPrimitives> {
    node: NonNull<NodeLink>,
    locked: &'a mut LockedQueue<'locked, T, S, SP>,
}

unsafe impl<'locked, T: Send, S: QueueState, SP: SyncPrimitives> Send
    for Dequeue<'locked, '_, T, S, SP>
where
    LockedQueue<'locked, T, S, SP>: Send,
{
}
unsafe impl<'locked, T: Sync, S: QueueState, SP: SyncPrimitives> Sync
    for Dequeue<'locked, '_, T, S, SP>
where
    LockedQueue<'locked, T, S, SP>: Sync,
{
}

node_getters!(Dequeue<'drain, 'a, T, S: QueueState, SP: SyncPrimitives>, T);

impl<T, S: QueueState, SP: SyncPrimitives> Dequeue<'_, '_, T, S, SP> {
    pub fn requeue(self) {
        mem::forget(self);
    }

    pub fn try_set_queue_state(self, state: S) -> bool {
        let this = &mut *ManuallyDrop::new(self);
        let new_tail = StateOrPtr::State(state).into();
        unsafe { (this.locked).remove_with_prev(this.node.as_ref(), new_tail, false, HEAD_MARKER) }
    }
}

impl<T, S: QueueState, SP: SyncPrimitives> Drop for Dequeue<'_, '_, T, S, SP> {
    fn drop(&mut self) {
        let new_tail = ptr::null_mut();
        unsafe { (self.locked).remove_with_prev(self.node.as_ref(), new_tail, false, HEAD_MARKER) };
    }
}

pub struct Pop<'locked, 'a, T, S: QueueState = (), SP: SyncPrimitives = DefaultSyncPrimitives> {
    node: NonNull<NodeLink>,
    locked: &'a mut LockedQueue<'locked, T, S, SP>,
}

unsafe impl<'locked, T: Send, S: QueueState, SP: SyncPrimitives> Send for Pop<'locked, '_, T, S, SP> where
    LockedQueue<'locked, T, S, SP>: Send
{
}
unsafe impl<'locked, T: Sync, S: QueueState, SP: SyncPrimitives> Sync for Pop<'locked, '_, T, S, SP> where
    LockedQueue<'locked, T, S, SP>: Sync
{
}

node_getters!(Pop<'drain, 'a, T, S: QueueState, SP: SyncPrimitives>, T);

impl<T, S: QueueState, SP: SyncPrimitives> Pop<'_, '_, T, S, SP> {
    pub fn requeue(self) {
        mem::forget(self);
    }

    pub fn try_set_queue_state(self, state: S) -> bool {
        let this = &mut *ManuallyDrop::new(self);
        unsafe { (this.locked).remove(this.node.as_ref(), StateOrPtr::State(state).into(), true) }
    }
}

impl<T, S: QueueState, SP: SyncPrimitives> Drop for Pop<'_, '_, T, S, SP> {
    fn drop(&mut self) {
        unsafe { (self.locked).remove(self.node.as_ref(), ptr::null_mut(), true) };
    }
}
