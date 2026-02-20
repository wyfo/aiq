use core::{pin::Pin, ptr, ptr::NonNull, sync::atomic::Ordering::*};

#[cfg(feature = "queue-state")]
use crate::queue::state::*;
use crate::{
    Queue,
    queue::{
        LockedQueue, NodeLink,
        node::{NodeLinkState, NodeWithData},
    },
    sync::SyncPrimitives,
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
        let node = if let Some(tail) = self.tail {
            let locked = unsafe { self.locked.as_mut().unwrap_unchecked() };
            let node = unsafe { locked.dequeue().unwrap_unchecked() };
            if node.node == tail.cast() {
                self.tail = None;
            }
            node.node
        } else {
            let sentinel = self.sentinel_node.as_ref()?;
            if sentinel.prev.get() == NonNull::from(sentinel) {
                self.sentinel_node = None;
                return None;
            }
            let node = sentinel.find_next(|| Some(sentinel.prev.get())).unwrap();
            let node_ref = unsafe { node.as_ref() };
            node_ref.unlink(node_ref.wait_for_next(None));
            node
        };
        Some(unsafe { Pin::new_unchecked(&mut (*node.cast::<NodeWithData<T, S>>().as_ptr()).data) })
    }

    pub fn execute_unlocked<F: FnOnce() -> R, R>(self: Pin<&mut Self>, f: F) -> R {
        let this = unsafe { self.get_unchecked_mut() };
        let locked = unsafe { this.locked.take().unwrap_unchecked() };
        if let Some(tail) = this.tail {
            let sentinel = this.sentinel_node.insert(NodeLink::new());
            sentinel.prev.set(tail);
            *sentinel.next.get_mut() = (locked.head_sentinel)
                .wait_for_next(Some(NodeLinkState::Queued))
                .as_ptr();
            if let Some(next) = unsafe { NonNull::new(tail.as_ref().next.load(Acquire)) } {
                locked.head_sentinel.next.store(next.as_ptr(), Release);
            } else {
                locked.head_sentinel.next.store(ptr::null_mut(), Relaxed);
                #[cfg(not(feature = "queue-state"))]
                let tail_ptr = tail.as_ptr();
                #[cfg(feature = "queue-state")]
                let tail_ptr = StateOrTail::Tail(tail).into();
                if (locked.tail)
                    .compare_exchange(tail_ptr, ptr::null_mut(), SeqCst, Acquire)
                    .is_err()
                {
                    let next = unsafe { tail.as_ref().wait_for_next(Some(NodeLinkState::Queued)) };
                    locked.head_sentinel.next.store(next.as_ptr(), Release);
                }
            }
            this.tail = None;
        }
        drop(locked);
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
