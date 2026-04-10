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
    pub fn wait_if<P: FnOnce() -> bool>(&self, predicate: P) -> WaitIf<&Self, SP, P> {
        WaitIf {
            node: WaitQueueRef::node(self),
            predicate: Some(predicate),
        }
    }

    #[cfg(feature = "alloc")]
    #[inline]
    pub fn wait_if_owned<P: FnOnce() -> bool>(
        self: Arc<Self>,
        predicate: P,
    ) -> WaitIf<Arc<Self>, SP, P> {
        WaitIf {
            node: WaitQueueRef::node(self),
            predicate: Some(predicate),
        }
    }

    #[inline]
    pub fn wait_until<P: FnMut() -> Option<T>, T>(
        &self,
        predicate: P,
    ) -> WaitUntil<&Self, SP, P, T> {
        WaitUntil {
            node: WaitQueueRef::node(self),
            predicate,
        }
    }

    #[cfg(feature = "alloc")]
    #[inline]
    pub fn wait_until_owned<P: FnMut() -> Option<T>, T>(
        self: Arc<Self>,
        predicate: P,
    ) -> WaitUntil<Arc<Self>, SP, P, T> {
        WaitUntil {
            node: WaitQueueRef::node(self),
            predicate,
        }
    }
}

struct WaitQueueRef<Q, SP> {
    wait_queue: Q,
    _sync_primitives: PhantomData<SP>,
}

unsafe impl<Q: Send, SP> Send for WaitQueueRef<Q, SP> {}
unsafe impl<Q: Sync, SP> Sync for WaitQueueRef<Q, SP> {}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives> WaitQueueRef<Q, SP> {
    fn node(queue: Q) -> Node<Self> {
        Node::new(WaitQueueRef {
            wait_queue: queue,
            _sync_primitives: PhantomData,
        })
    }
}

queue_ref!(WaitQueueRef<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>, NodeData = Option<Waker>, SyncPrimitives = SP, &self.wait_queue.queue);

pub struct WaitIf<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives, P: FnOnce() -> bool> {
    node: Node<WaitQueueRef<Q, SP>>,
    predicate: Option<P>,
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives, P: FnOnce() -> bool> Future
    for WaitIf<Q, SP, P>
{
    type Output = ();

    #[cold]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match unsafe { Pin::new_unchecked(&mut this.node) }.state() {
            NodeState::Unqueued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    *waiter = Some(cx.waker().clone());
                });
                waiter.enqueue();
                if unsafe { this.predicate.take().unwrap_unchecked() }() {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
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
}

pub struct WaitUntil<
    Q: Deref<Target = WaitQueue<SP>>,
    SP: SyncPrimitives,
    P: FnMut() -> Option<T>,
    T,
> {
    node: Node<WaitQueueRef<Q, SP>>,
    predicate: P,
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives, P: FnMut() -> Option<T>, T>
    WaitUntil<Q, SP, P, T>
{
    #[cold]
    unsafe fn poll_cold(&mut self, cx: &mut Context<'_>) -> Poll<T> {
        match unsafe { Pin::new_unchecked(&mut self.node) }.state() {
            NodeState::Unqueued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    *waiter = Some(cx.waker().clone());
                });
                waiter.enqueue();
            }
            NodeState::Queued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    if (*waiter).as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                        *waiter = Some(cx.waker().clone());
                    }
                });
            }
            NodeState::Dequeued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    *waiter = Some(cx.waker().clone());
                });
                waiter.reset().enqueue();
            }
        }
        match (self.predicate)() {
            Some(res) => Poll::Ready(res),
            None => Poll::Pending,
        }
    }
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives, P: FnMut() -> Option<T>, T> Future
    for WaitUntil<Q, SP, P, T>
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match (this.predicate)() {
            Some(res) => Poll::Ready(res),
            None => unsafe { this.poll_cold(cx) },
        }
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
