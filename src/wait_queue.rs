#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
use alloc::sync::Arc;
use core::{
    future::Future,
    hint::assert_unchecked,
    marker::PhantomData,
    mem::MaybeUninit,
    ops::Deref,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::{
    Node, NodeState, Queue, queue_ref,
    sync::{DefaultSyncPrimitives, SyncPrimitives},
};

pub struct WaitQueue<SP: SyncPrimitives = DefaultSyncPrimitives> {
    queue: Queue<Option<Waker>, (), SP>,
}

impl<SP: SyncPrimitives> Default for WaitQueue<SP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<SP: SyncPrimitives> WaitQueue<SP> {
    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    #[inline]
    pub const fn new() -> Self {
        Self {
            queue: Queue::new(),
        }
    }

    #[inline]
    pub fn notify_one(&self) {
        self.queue.with_lock(|mut locked| {
            let waker = locked
                .dequeue()
                .and_then(|mut w| w.with_data_mut(|mut w| w.take()));
            drop(locked);
            if let Some(waker) = waker {
                waker.wake();
            }
        });
    }

    #[inline]
    pub fn notify_last(&self) {
        self.queue.with_lock(|mut locked| {
            let waker = locked
                .pop()
                .and_then(|mut w| w.with_data_mut(|mut w| w.take()));
            drop(locked);
            if let Some(waker) = waker {
                waker.wake();
            }
        });
    }

    #[inline]
    pub fn notify_all(&self) {
        self.queue.with_lock(|locked| {
            let mut wakers = WakerList::new();
            locked.drain().for_each(
                &mut wakers,
                |wakers, mut waker| {
                    if let Some(w) = waker.take() {
                        wakers.push(w);
                    }
                    wakers.is_full()
                },
                |wakers| wakers.drain().for_each(Waker::wake),
            );
            wakers.drain().for_each(Waker::wake);
        });
    }

    #[inline]
    pub fn wait(&self) -> Wait<'_, SP> {
        Wait(Node::new(WaitQueueRef {
            wait_queue: self,
            _sync_primitives: PhantomData,
        }))
    }

    #[cfg(feature = "alloc")]
    #[inline]
    pub fn wait_owned(self: Arc<Self>) -> OwnedWait<SP> {
        OwnedWait(Node::new(WaitQueueRef {
            wait_queue: self,
            _sync_primitives: PhantomData,
        }))
    }
}

struct WaitQueueRef<Q, SP: SyncPrimitives> {
    wait_queue: Q,
    _sync_primitives: PhantomData<SP>,
}

queue_ref!(WaitQueueRef<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>, NodeData = Option<Waker>, SyncPrimitives = SP, &self.wait_queue.queue);

fn poll_wait<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>(
    node: Pin<&mut Node<WaitQueueRef<Q, SP>>>,
    cx: &mut Context<'_>,
) -> Poll<()> {
    match node.state() {
        NodeState::Unqueued(mut waiter) => {
            waiter.with_data_mut(|mut waiter| {
                waiter.get_or_insert_with(|| cx.waker().clone());
            });
            waiter.enqueue();
            Poll::Pending
        }
        NodeState::Queued(mut waiter) => {
            waiter.with_data_mut(|mut waiter| {
                if (*waiter).as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                    *waiter = Some(cx.waker().clone());
                }
            });
            Poll::Pending
        }
        NodeState::Dequeued(_) => Poll::Ready(()),
    }
}

pub struct Wait<'a, SP: SyncPrimitives>(Node<WaitQueueRef<&'a WaitQueue<SP>, SP>>);

impl<SP: SyncPrimitives> Future for Wait<'_, SP> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        poll_wait(unsafe { self.map_unchecked_mut(|this| &mut this.0) }, cx)
    }
}

#[cfg(feature = "alloc")]
pub struct OwnedWait<SP: SyncPrimitives>(Node<WaitQueueRef<Arc<WaitQueue<SP>>, SP>>);

#[cfg(feature = "alloc")]
impl<SP: SyncPrimitives> Future for OwnedWait<SP> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        poll_wait(unsafe { self.map_unchecked_mut(|this| &mut this.0) }, cx)
    }
}

struct WakerList {
    wakers: [MaybeUninit<Waker>; 32],
    len: usize,
}

impl WakerList {
    fn new() -> Self {
        Self {
            wakers: unsafe { MaybeUninit::uninit().assume_init() },
            len: 0,
        }
    }
    fn push(&mut self, waker: Waker) {
        self.wakers[self.len].write(waker);
        self.len += 1;
    }
    fn is_full(&self) -> bool {
        self.len == self.wakers.len()
    }
    fn drain(&mut self) -> impl Iterator<Item = Waker> {
        let len = self.len;
        self.len = 0;
        unsafe { assert_unchecked(len <= self.wakers.len()) };
        self.wakers[..len]
            .iter()
            .map(|w| unsafe { w.assume_init_read() })
    }
}

impl Drop for WakerList {
    fn drop(&mut self) {
        self.drain().for_each(drop);
    }
}
