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
    Node, NodeState, Queue,
    queue::LockedQueue,
    queue_ref,
    sync::{DefaultSyncPrimitives, SyncPrimitives},
};

const EMPTY: usize = 0;
const CLOSED: usize = 1;

fn not_closed(state: Option<usize>) -> bool {
    state.is_none_or(|s| s == EMPTY)
}

pub struct WaitQueue<SP: SyncPrimitives = DefaultSyncPrimitives> {
    queue: Queue<Option<Waker>, usize, SP>,
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

    pub fn is_closed(&self) -> bool {
        self.queue.state().is_some_and(|s| s != EMPTY)
    }

    pub fn close(&self) {
        if let Some(locked) = self.queue.fetch_update_state_or_lock(|_| CLOSED) {
            drain_queue::<CLOSED, SP>(locked);
        }
    }

    #[inline]
    pub fn notify_one(&self) {
        self.queue.is_empty_or_locked(|mut locked| {
            let mut waiter = unsafe { locked.dequeue().unwrap_unchecked() };
            let waker = waiter.with_data_mut(|mut w| w.take());
            drop(waiter);
            drop(locked);
            if let Some(waker) = waker {
                waker.wake();
            }
        });
    }

    #[inline]
    pub fn notify_last(&self) {
        self.queue.is_empty_or_locked(|mut locked| {
            let mut waiter = unsafe { locked.pop().unwrap_unchecked() };
            let waker = waiter.with_data_mut(|mut w| w.take());
            drop(waiter);
            drop(locked);
            if let Some(waker) = waker {
                waker.wake();
            }
        });
    }

    #[inline]
    pub fn notify_all(&self) {
        self.queue.is_empty_or_locked(drain_queue::<EMPTY, SP>);
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

queue_ref!(WaitQueueRef<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>, NodeData = Option<Waker>, State = usize, SyncPrimitives = SP, &self.wait_queue.queue);

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
                match waiter.try_enqueue_with_queue_state(not_closed) {
                    Ok(_) => Poll::Ready(()),
                    Err(_) if unsafe { this.predicate.take().unwrap_unchecked() }() => {
                        Poll::Pending
                    }
                    Err(_) => Poll::Ready(()),
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
        let mut closed = false;
        match unsafe { Pin::new_unchecked(&mut self.node) }.state() {
            NodeState::Unqueued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    *waiter = Some(cx.waker().clone());
                });
                if waiter.try_enqueue_with_queue_state(not_closed).is_err() {
                    closed = true;
                }
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
                let waiter = waiter.reset();
                if waiter.try_enqueue_with_queue_state(not_closed).is_err() {
                    closed = true;
                }
            }
        }
        match (self.predicate)() {
            Some(res) => Poll::Ready(res),
            None if closed => panic!("wait queue is closed but predicate didn't return `Some(_)`"),
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

fn drain_queue<const STATE: usize, SP: SyncPrimitives>(
    locked: LockedQueue<Option<Waker>, usize, SP>,
) {
    let mut wakers = WakerList::new();
    locked.drain_try_set_state(STATE).for_each(
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
}
