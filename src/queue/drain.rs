use core::{hint::unreachable_unchecked, pin::Pin, ptr, ptr::NonNull};

use crate::{
    Queue,
    loom::{
        AtomicPtrExt,
        sync::atomic::{AtomicPtr, Ordering::*},
    },
    node::{RawNodeState, node_getters},
    queue::{
        LockedQueue, NodeLink, QueueState,
        state::{StateOrPtr, Tail},
    },
    sync::{DefaultSyncPrimitives, SyncPrimitives},
};

pub struct Drain<'a, T, S: QueueState = (), SP: SyncPrimitives + 'a = DefaultSyncPrimitives> {
    sentinel_node: NodeLink,
    queue: &'a Queue<T, S, SP>,
    locked: Option<LockedQueue<'a, T, S, SP>>,
}

impl<'a, T, S: QueueState, SP: SyncPrimitives> Drain<'a, T, S, SP> {
    pub(super) fn new(locked: LockedQueue<'a, T, S, SP>, new_tail: *mut Tail<S>) -> Self {
        let mut head = ptr::null_mut();
        let mut tail = ptr::null_mut();
        if locked.tail().is_some() {
            head = locked.get_next(&locked.queue.head).as_ptr();
            locked.head.store(ptr::null_mut(), Relaxed);
            match locked.tail.swap(new_tail, SeqCst).into() {
                StateOrPtr::Ptr(ptr) => tail = ptr.as_ptr(),
                _ => unsafe { unreachable_unchecked() },
            }
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
        NonNull::new(self.sentinel_node.next.load_mut())
    }

    fn set_head(&mut self, head: *mut NodeLink) {
        self.sentinel_node.next.store_mut(head);
    }

    pub fn is_empty(&self) -> bool {
        self.sentinel_node.next.load(Relaxed).is_null()
    }

    #[inline]
    pub fn next(self: Pin<&mut Self>) -> Option<NodeDrained<'a, '_, T, S, SP>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.locked.as_ref().unwrap();
        this.head().map(|node| NodeDrained { node, drain: this })
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
            let tail = NonNull::new(this.sentinel_node.prev.load_mut());
            debug_assert_eq!(this.head(), tail);
            this.set_head(ptr::null_mut());
        }
        res
    }

    pub fn for_each<H, N: FnMut(&mut H, Pin<&mut T>) -> bool, U: FnMut(&mut H)>(
        mut self,
        helper: &mut H,
        mut on_next: N,
        mut on_unlock: U,
    ) {
        let mut this = unsafe { Pin::new_unchecked(&mut self) };
        loop {
            let Some(mut node) = this.as_mut().next() else {
                break;
            };
            if node.with_data_mut(|data| on_next(helper, data)) {
                drop(node);
                if unsafe { this.as_mut().get_unchecked_mut().head().is_none() } {
                    break;
                }
                this.as_mut().execute_unlocked(|| on_unlock(helper));
            }
        }
        drop(self);
        on_unlock(helper);
    }

    #[cold]
    #[inline(never)]
    fn drain(&mut self) {
        self.locked.get_or_insert_with(|| self.queue.lock());
        while let Some(node) = self.head() {
            drop(NodeDrained { node, drain: self });
        }
    }
}

impl<'a, T, S: QueueState, SP: SyncPrimitives> Drop for Drain<'a, T, S, SP> {
    #[inline]
    fn drop(&mut self) {
        if !self.sentinel_node.next.load(Relaxed).is_null() {
            self.drain();
        }
    }
}

pub struct NodeDrained<
    'drain,
    'a,
    T,
    S: QueueState = (),
    SP: SyncPrimitives = DefaultSyncPrimitives,
> {
    node: NonNull<NodeLink>,
    drain: &'a mut Drain<'drain, T, S, SP>,
}

unsafe impl<'drain, T: Send, S: QueueState, SP: SyncPrimitives> Send
    for NodeDrained<'drain, '_, T, S, SP>
where
    LockedQueue<'drain, T, S, SP>: Sync,
{
}
unsafe impl<'drain, T: Sync, S: QueueState, SP: SyncPrimitives> Sync
    for NodeDrained<'drain, '_, T, S, SP>
where
    LockedQueue<'drain, T, S, SP>: Sync,
{
}

node_getters!(
    NodeDrained<'drain, 'a, T, S: QueueState, SP: SyncPrimitives>,
    T
);

impl<T, S: QueueState, SP: SyncPrimitives> Drop for NodeDrained<'_, '_, T, S, SP> {
    fn drop(&mut self) {
        let node = unsafe { self.node.as_ref() };
        let next = if self.node == self.drain.sentinel_node.prev().into() {
            ptr::null_mut()
        } else {
            let locked = unsafe { self.drain.locked.as_mut().unwrap_unchecked() };
            locked.get_next(&node.next).as_ptr()
        };
        self.drain.set_head(next);
        node.prev.store(RawNodeState::Dequeued.into_ptr(), Release);
    }
}
