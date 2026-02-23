use core::{
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr,
    ptr::NonNull,
    sync::atomic::Ordering::*,
};

#[cfg(feature = "queue-state")]
use crate::queue::state::*;
use crate::{
    Queue,
    queue::{LockedQueue, NodeLink, node::NodeWithData},
    sync::{DefaultSyncPrimitives, SyncPrimitives},
};

pub struct Drain<'a, T, S: SyncPrimitives + 'a = DefaultSyncPrimitives> {
    sentinel_node: NodeLink,
    queue: &'a Queue<T, S>,
    locked: Option<LockedQueue<'a, T, S>>,
}

impl<'a, T, S: SyncPrimitives> Drain<'a, T, S> {
    pub(super) fn new(mut locked: LockedQueue<'a, T, S>, new_tail: *mut NodeLink) -> Self {
        let mut sentinel_node = NodeLink::new();
        if locked.tail().is_some() {
            *sentinel_node.next.get_mut() = locked.get_next(&locked.queue.head_sentinel).as_ptr();
            let tail = locked.tail.swap(new_tail, SeqCst);
            #[cfg(feature = "queue-state")]
            let tail = match tail.into() {
                StateOrTail::Tail(t) => t.as_ptr(),
                _ => unsafe { core::hint::unreachable_unchecked() },
            };
            sentinel_node.prev.store(tail, Relaxed);
        }
        Self {
            sentinel_node,
            queue: locked.queue,
            locked: Some(locked),
        }
    }

    #[inline]
    pub fn next(self: Pin<&mut Self>) -> Option<NodeDrained<'a, '_, T, S>> {
        (unsafe { self.get_unchecked_mut() }).next_impl()
    }

    fn next_impl(&mut self) -> Option<NodeDrained<'a, '_, T, S>> {
        Some(NodeDrained {
            node: NonNull::new(*self.sentinel_node.next.get_mut())?,
            drain: self,
        })
    }

    pub fn execute_unlocked<F: FnOnce() -> R, R>(self: Pin<&mut Self>, f: F) -> R {
        let this = unsafe { self.get_unchecked_mut() };
        let sentinel_ptr = ptr::from_ref(&this.sentinel_node).cast_mut();
        if let Some(next) = NonNull::new(*this.sentinel_node.next.get_mut()) {
            unsafe { next.as_ref().prev.store(sentinel_ptr, Relaxed) }
            (this.sentinel_node.prev().next).store(sentinel_ptr, Relaxed);
        }
        drop(unsafe { this.locked.take().unwrap_unchecked() });
        let res = f();
        this.locked = Some(this.queue.lock());
        if *this.sentinel_node.next.get_mut() == sentinel_ptr {
            debug_assert_eq!(
                *this.sentinel_node.next.get_mut(),
                *this.sentinel_node.prev.get_mut(),
            );
            *this.sentinel_node.next.get_mut() = ptr::null_mut();
        }
        res
    }
}

impl<'a, T, S: SyncPrimitives> Drop for Drain<'a, T, S> {
    fn drop(&mut self) {
        self.locked.get_or_insert_with(|| self.queue.lock());
        while self.next_impl().is_some() {}
    }
}

pub struct NodeDrained<'drain, 'a, T, S: SyncPrimitives = DefaultSyncPrimitives> {
    node: NonNull<NodeLink>,
    drain: &'a mut Drain<'drain, T, S>,
}

impl<T, S: SyncPrimitives> Drop for NodeDrained<'_, '_, T, S> {
    fn drop(&mut self) {
        let next = if self.node == self.drain.sentinel_node.prev().into() {
            ptr::null_mut()
        } else {
            let locked = unsafe { self.drain.locked.as_mut().unwrap_unchecked() };
            unsafe { locked.get_next(self.node.as_ref()).as_ptr() }
        };
        *self.drain.sentinel_node.next.get_mut() = next;
        unsafe { self.node.as_ref().dequeue() }
    }
}

impl<T, S: SyncPrimitives> NodeDrained<'_, '_, T, S> {
    pub fn data_pinned(&mut self) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(&mut (*self.node.cast::<NodeWithData<T>>().as_ptr()).data) }
    }
}

impl<T, S: SyncPrimitives> Deref for NodeDrained<'_, '_, T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.node.cast::<NodeWithData<T>>().as_ptr()).data }
    }
}
impl<T: Unpin, S: SyncPrimitives> DerefMut for NodeDrained<'_, '_, T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut (*self.node.cast::<NodeWithData<T>>().as_ptr()).data }
    }
}
