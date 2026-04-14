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
    task::{Context, Poll, Waker, ready},
};

use crate::{
    Node, NodeState, Queue,
    queue::LockedQueue,
    queue_ref,
    sync::{DefaultSyncPrimitives, SyncPrimitives},
};

const EMPTY: usize = 0;
const CLOSED: usize = 1;

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
    pub fn wait(&self) -> Wait<&Self, SP> {
        Wait {
            node: Node::new(WaitQueueRef {
                wait_queue: self,
                _sync_primitives: PhantomData,
            }),
        }
    }

    #[cfg(feature = "alloc")]
    #[inline]
    pub fn wait_owned(self: Arc<Self>) -> Wait<Arc<Self>, SP> {
        Wait {
            node: Node::new(WaitQueueRef {
                wait_queue: self,
                _sync_primitives: PhantomData,
            }),
        }
    }

    #[inline]
    pub fn wait_if<P: FnOnce() -> bool>(&self, predicate: P) -> WaitIf<&Self, SP, P> {
        WaitIf {
            wait: self.wait(),
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
            wait: self.wait_owned(),
            predicate: Some(predicate),
        }
    }

    #[inline]
    pub fn wait_until<P: FnMut() -> Option<T>, T>(
        &self,
        predicate: P,
    ) -> WaitUntil<&Self, SP, P, T> {
        WaitUntil {
            wait: self.wait(),
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
            wait: self.wait_owned(),
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

queue_ref!(WaitQueueRef<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>, NodeData = Option<Waker>, State = usize, SyncPrimitives = SP, &self.wait_queue.queue);

pub struct Wait<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives> {
    node: Node<WaitQueueRef<Q, SP>>,
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives> Wait<Q, SP> {
    fn poll_wait(self: Pin<&mut Self>, cx: &mut Context<'_>, requeue: bool) -> Poll<()> {
        let mut waiter = match unsafe { self.map_unchecked_mut(|this| &mut this.node) }.state() {
            NodeState::Unqueued(waiter) => waiter,
            NodeState::Queued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    if (*waiter).as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                        *waiter = Some(cx.waker().clone());
                    }
                });
                return Poll::Pending;
            }
            NodeState::Dequeued(waiter) if requeue => waiter.reset(),
            NodeState::Dequeued(_) => return Poll::Ready(()),
        };
        waiter.with_data_mut(|mut waiter| {
            *waiter = Some(cx.waker().clone());
        });
        match waiter.try_enqueue_with_queue_state(|s| s.is_none_or(|s| s == EMPTY)) {
            Ok(_) => Poll::Pending,
            Err(_) => Poll::Ready(()),
        }
    }
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives> Future for Wait<Q, SP> {
    type Output = ();

    #[cold]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_wait(cx, false)
    }
}

pub struct WaitIf<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives, P: FnOnce() -> bool> {
    wait: Wait<Q, SP>,
    predicate: Option<P>,
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives, P: FnOnce() -> bool> Future
    for WaitIf<Q, SP, P>
{
    type Output = ();

    #[cold]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        ready!(unsafe { Pin::new_unchecked(&mut this.wait) }.poll_wait(cx, false));
        if this.predicate.take().is_some_and(|p| p()) {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

pub struct WaitUntil<
    Q: Deref<Target = WaitQueue<SP>>,
    SP: SyncPrimitives,
    P: FnMut() -> Option<T>,
    T,
> {
    wait: Wait<Q, SP>,
    predicate: P,
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives, P: FnMut() -> Option<T>, T>
    WaitUntil<Q, SP, P, T>
{
    #[cold]
    unsafe fn poll_cold(&mut self, cx: &mut Context<'_>) -> Poll<T> {
        let is_closed = unsafe { Pin::new_unchecked(&mut self.wait) }
            .poll_wait(cx, true)
            .is_ready();
        match (self.predicate)() {
            Some(res) => Poll::Ready(res),
            None if is_closed => panic!("wait queue is closed but predicate didn't return `Some`"),
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
