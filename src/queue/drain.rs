#[cfg(not(loom))]
use core::sync::atomic::{AtomicPtr, Ordering::*};
use core::{
    pin::{Pin, pin},
    ptr,
    ptr::NonNull,
};

#[cfg(loom)]
use loom::sync::atomic::{AtomicPtr, Ordering::*};

#[cfg(feature = "queue-state")]
use crate::queue::state::*;
use crate::{
    Queue,
    node::node_getters,
    queue::{LockedQueue, NodeLink},
    sync::{DefaultSyncPrimitives, SyncPrimitives},
};

pub struct Drain<'a, T, S: SyncPrimitives + 'a = DefaultSyncPrimitives> {
    sentinel_node: NodeLink,
    queue: &'a Queue<T, S>,
    locked: Option<LockedQueue<'a, T, S>>,
}

impl<'a, T, S: SyncPrimitives> Drain<'a, T, S> {
    pub(super) fn new(locked: LockedQueue<'a, T, S>, new_tail: *mut NodeLink) -> Self {
        let mut head = ptr::null_mut();
        let mut tail = ptr::null_mut();
        if locked.tail().is_some() {
            head = locked.get_next(&locked.queue.head).as_ptr();
            locked.head.store(ptr::null_mut(), Relaxed);
            tail = locked.tail.swap(new_tail, SeqCst);
            #[cfg(feature = "queue-state")]
            match tail.into() {
                StateOrTail::Tail(t) => tail = t.as_ptr(),
                _ => unsafe { core::hint::unreachable_unchecked() },
            };
        }
        Self {
            sentinel_node: NodeLink {
                prev: AtomicPtr::new(tail),
                next: AtomicPtr::new(head),
            },
            queue: locked.queue,
            locked: Some(locked),
        }
    }

    fn head(&mut self) -> Option<NonNull<NodeLink>> {
        #[cfg(not(loom))]
        return NonNull::new(*self.sentinel_node.next.get_mut());
        #[cfg(loom)]
        return self.sentinel_node.next.with_mut(|head| NonNull::new(*head));
    }

    fn set_head(&mut self, head: *mut NodeLink) {
        #[cfg(not(loom))]
        let () = *self.sentinel_node.next.get_mut() = head;
        #[cfg(loom)]
        self.sentinel_node.next.with_mut(|ptr| *ptr = head);
    }

    #[inline]
    pub fn next(self: Pin<&mut Self>) -> Option<NodeDrained<'a, '_, T, S>> {
        (unsafe { self.get_unchecked_mut() }).next_impl()
    }

    fn next_impl(&mut self) -> Option<NodeDrained<'a, '_, T, S>> {
        if self.locked.is_none() {
            self.lock();
        }
        Some(NodeDrained {
            node: self.head()?,
            drain: self,
        })
    }

    #[cold]
    #[inline(never)]
    fn lock(&mut self) {
        self.locked = Some(self.queue.lock());
    }

    pub fn execute_unlocked<F: FnOnce() -> R, R>(self: Pin<&mut Self>, f: F) -> R {
        let this = unsafe { self.get_unchecked_mut() };
        let sentinel_ptr = ptr::from_ref(&this.sentinel_node).cast_mut();
        if let Some(next) = this.head() {
            unsafe { next.as_ref().prev.store(sentinel_ptr, Relaxed) }
            (this.sentinel_node.prev().next).store(sentinel_ptr, Relaxed);
        }
        drop(unsafe { this.locked.take().unwrap_unchecked() });
        let res = f();
        this.locked = Some(this.queue.lock());
        if this.head() == NonNull::new(sentinel_ptr) {
            #[cfg(not(loom))]
            let tail = NonNull::new(*this.sentinel_node.prev.get_mut());
            #[cfg(loom)]
            let tail = this.sentinel_node.next.with_mut(|tail| NonNull::new(*tail));
            debug_assert_eq!(this.head(), tail);
            this.set_head(ptr::null_mut());
        }
        res
    }

    pub fn for_each<H, N: FnMut(&mut H, Pin<&mut T>) -> bool, U: FnMut(&mut H)>(
        self,
        helper: &mut H,
        mut on_next: N,
        mut on_unlock: U,
    ) {
        let mut this = pin!(self);
        loop {
            let Some(mut node) = this.as_mut().next() else {
                break;
            };
            if node.with_data_mut(|data| on_next(helper, data)) {
                drop(node);
                this.as_mut().execute_unlocked(|| on_unlock(helper));
            }
        }
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

node_getters!(NodeDrained<'drain, 'a, T, S: SyncPrimitives>, T);

impl<T, S: SyncPrimitives> Drop for NodeDrained<'_, '_, T, S> {
    fn drop(&mut self) {
        let next = if self.node == self.drain.sentinel_node.prev().into() {
            ptr::null_mut()
        } else {
            let locked = unsafe { self.drain.locked.as_mut().unwrap_unchecked() };
            unsafe { locked.get_next(&self.node.as_ref().next).as_ptr() }
        };
        self.drain.set_head(next);
        unsafe { self.node.as_ref().dequeue() }
    }
}
