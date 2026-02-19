use core::{pin::Pin, ptr::NonNull, sync::atomic::Ordering::*};

use crate::{
    queue::{LockedQueue, NodeLink},
    sync::SyncPrimitives,
    Queue,
};

pub struct Drain<'a, T, S: SyncPrimitives + 'a> {
    tail: Option<NonNull<NodeLink<S>>>,
    sentinel_node: Option<NodeLink<S>>,
    queue: &'a Queue<T, S>,
    locked: Option<LockedQueue<'a, T, S>>,
}

impl<'a, T, S: SyncPrimitives> Drain<'a, T, S> {
    pub(super) fn new(locked: LockedQueue<'a, T, S>) -> Self {
        Self {
            tail: NonNull::new(locked.queue.tail.load(SeqCst)),
            sentinel_node: None,
            queue: locked.queue,
            locked: Some(locked),
        }
    }

    pub fn next(self: Pin<&mut Self>) -> Option<Pin<&mut T>> {
        (unsafe { self.get_unchecked_mut() }).next_impl()
    }

    fn next_impl(&mut self) -> Option<Pin<&mut T>> {
        let mut node = if let Some(tail) = self.tail {
            let locked = unsafe { self.locked.as_mut().unwrap_unchecked() };
            let node = unsafe { locked.dequeue().unwrap_unchecked() };
            if node.node == tail.cast() {
                self.tail = None;
            }
            node.node
        } else {
            let sentinel = self.sentinel_node.as_ref()?;
            let node = sentinel.find_next(|| {
                Some(sentinel.prev.get()).filter(|n| *n != NonNull::from(sentinel))
            })?;
            let node_ref = unsafe { node.as_ref() };
            node_ref.unlink(Some(node_ref.wait_for_next(None)));
            node.cast()
        };
        Some(unsafe { Pin::new_unchecked(&mut node.as_mut().data) })
    }

    pub fn execute_unlocked<F: FnOnce() -> R, R>(self: Pin<&mut Self>, f: F) -> R {
        let this = unsafe { self.get_unchecked_mut() };
        // TODO chain to the sentinel
        drop(unsafe { this.locked.take().unwrap_unchecked() });
        let res = f();
        this.locked = Some(this.queue.lock());
        res
    }
}

impl<'a, T, S: SyncPrimitives> Drop for Drain<'a, T, S> {
    fn drop(&mut self) {
        self.locked.get_or_insert_with(|| self.queue.lock());
        while self.next_impl().is_some() {}
    }
}
