#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
use alloc::sync::Arc;
use core::{
    future::Future,
    hint::{assert_unchecked, unreachable_unchecked},
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ops::Deref,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::{
    Node, NodeState, Queue,
    node::{NodeDequeued, RawNodeState},
    queue::LockedQueue,
    queue_ref,
    sync::{DefaultSyncPrimitives, SyncPrimitives},
};

const EMPTY: usize = 0;
const CLOSED: usize = 1;

#[derive(Debug, Default)]
struct Waiter {
    waker: Option<Waker>,
    notification: Option<Notification>,
}

#[derive(Debug, Clone, Copy)]
enum Notification {
    One,
    Last,
}

pub struct WaitQueue<SP: SyncPrimitives = DefaultSyncPrimitives> {
    queue: Queue<Waiter, usize, SP>,
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
            queue: Queue::with_state_const(EMPTY),
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
            waiter.with_data_mut(|mut w| w.notification = Some(Notification::One));
            let waker = waiter.with_data_mut(|mut w| unsafe { w.waker.take().unwrap_unchecked() });
            drop(waiter);
            drop(locked);
            waker.wake();
        });
    }

    #[inline]
    pub fn notify_last(&self) {
        self.queue.is_empty_or_locked(|mut locked| {
            let mut waiter = unsafe { locked.pop().unwrap_unchecked() };
            waiter.with_data_mut(|mut w| w.notification = Some(Notification::Last));
            let waker = waiter.with_data_mut(|mut w| unsafe { w.waker.take().unwrap_unchecked() });
            drop(waiter);
            drop(locked);
            waker.wake();
        });
    }

    #[inline]
    pub fn notify_many(&self, count: usize) {
        self.queue
            .is_empty_or_locked(|locked| notify_many(locked, count));
    }

    #[inline]
    pub fn notify_many_const<const COUNT: usize>(&self) {
        self.queue
            .is_empty_or_locked(|locked| notify_many(locked, COUNT));
    }

    #[inline]
    pub fn notify_all(&self) {
        self.queue.is_empty_or_locked(drain_queue::<EMPTY, SP>);
    }

    #[inline]
    pub fn wait(&self) -> Wait<&Self, SP> {
        Wait {
            node: ManuallyDrop::new(Node::new(WaitQueueRef {
                wait_queue: self,
                _sync_primitives: PhantomData,
            })),
        }
    }

    #[cfg(feature = "alloc")]
    #[inline]
    pub fn wait_owned(self: Arc<Self>) -> Wait<Arc<Self>, SP> {
        Wait {
            node: ManuallyDrop::new(Node::new(WaitQueueRef {
                wait_queue: self,
                _sync_primitives: PhantomData,
            })),
        }
    }

    #[inline]
    pub fn wait_if<P: FnOnce() -> bool>(&self, predicate: P) -> WaitIf<&Self, P, SP> {
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
    ) -> WaitIf<Arc<Self>, P, SP> {
        WaitIf {
            wait: self.wait_owned(),
            predicate: Some(predicate),
        }
    }

    #[inline]
    pub fn wait_until<P: FnMut() -> Option<T>, T>(
        &self,
        predicate: P,
    ) -> WaitUntil<&Self, P, T, SP> {
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
    ) -> WaitUntil<Arc<Self>, P, T, SP> {
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

queue_ref!(WaitQueueRef<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>, NodeData = Waiter, State = usize, SyncPrimitives = SP, &self.wait_queue.queue);

pub struct Wait<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives = DefaultSyncPrimitives> {
    node: ManuallyDrop<Node<WaitQueueRef<Q, SP>>>,
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives> Wait<Q, SP> {
    fn poll_wait(self: Pin<&mut Self>, cx: &mut Context<'_>, requeue: bool) -> Poll<()> {
        let mut waiter = match unsafe { self.map_unchecked_mut(|this| &mut *this.node) }.state() {
            NodeState::Unqueued(waiter) => waiter,
            NodeState::Queued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    if (waiter.waker.as_ref()).is_none_or(|w| !w.will_wake(cx.waker())) {
                        waiter.waker = Some(cx.waker().clone());
                    }
                });
                return Poll::Pending;
            }
            NodeState::Dequeued(waiter) if requeue => waiter.reset(),
            NodeState::Dequeued(_) => return Poll::Ready(()),
        };
        waiter.with_data_mut(|mut waiter| {
            waiter.waker = Some(cx.waker().clone());
        });
        match waiter.try_enqueue_with_queue_state(|s| s.is_none_or(|s| s == EMPTY)) {
            Ok(_) => Poll::Pending,
            Err(_) => Poll::Ready(()),
        }
    }
}

impl<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives> Drop for Wait<Q, SP> {
    fn drop(&mut self) {
        #[inline(always)]
        fn renotify<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>(
            waiter: NodeDequeued<WaitQueueRef<Q, SP>>,
        ) {
            match waiter.with_data(|w| w.notification) {
                Some(Notification::One) => waiter.queue().wait_queue.notify_one(),
                Some(Notification::Last) => waiter.queue().wait_queue.notify_last(),
                None => {}
            }
        }
        match self.node.raw_state() {
            RawNodeState::Queued => {
                #[cold]
                unsafe fn dequeue<Q: Deref<Target = WaitQueue<SP>>, SP: SyncPrimitives>(
                    node: &mut Node<WaitQueueRef<Q, SP>>,
                ) {
                    match unsafe { Pin::new_unchecked(node).state_from_raw(RawNodeState::Queued) } {
                        NodeState::Queued(waiter) => {
                            waiter.dequeue();
                        }
                        NodeState::Dequeued(waiter) => renotify(waiter),
                        _ => {}
                    }
                }
                unsafe { dequeue(&mut self.node) };
            }
            RawNodeState::Dequeued => unsafe {
                let NodeState::Dequeued(waiter) =
                    Pin::new_unchecked(&mut *self.node).state_from_raw(RawNodeState::Queued)
                else {
                    unreachable_unchecked()
                };
                renotify(waiter);
            },
            _ => {}
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

pub struct WaitIf<
    Q: Deref<Target = WaitQueue<SP>>,
    P: FnOnce() -> bool,
    SP: SyncPrimitives = DefaultSyncPrimitives,
> {
    wait: Wait<Q, SP>,
    predicate: Option<P>,
}

impl<Q: Deref<Target = WaitQueue<SP>>, P: FnOnce() -> bool, SP: SyncPrimitives> Future
    for WaitIf<Q, P, SP>
{
    type Output = ();

    #[cold]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match unsafe { Pin::new_unchecked(&mut this.wait) }.poll_wait(cx, false) {
            Poll::Pending if this.predicate.take().is_some_and(|p| !p()) => Poll::Ready(()),
            poll => poll,
        }
    }
}

pub struct WaitUntil<
    Q: Deref<Target = WaitQueue<SP>>,
    P: FnMut() -> Option<T>,
    T,
    SP: SyncPrimitives = DefaultSyncPrimitives,
> {
    wait: Wait<Q, SP>,
    predicate: P,
}

impl<Q: Deref<Target = WaitQueue<SP>>, P: FnMut() -> Option<T>, T, SP: SyncPrimitives>
    WaitUntil<Q, P, T, SP>
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

impl<Q: Deref<Target = WaitQueue<SP>>, P: FnMut() -> Option<T>, T, SP: SyncPrimitives> Future
    for WaitUntil<Q, P, T, SP>
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

fn notify_many<SP: SyncPrimitives>(mut locked: LockedQueue<Waiter, usize, SP>, count: usize) {
    let mut wakers = WakerList::new();
    for _ in 0..count {
        let Some(mut waiter) = locked.dequeue() else {
            drop(locked);
            break;
        };
        waiter.with_data_mut(|mut w| w.notification = Some(Notification::One));
        wakers.push(waiter.with_data_mut(|mut w| unsafe { w.waker.take().unwrap_unchecked() }));
        drop(waiter);
        if wakers.is_full() {
            let queue = locked.unlock();
            wakers.drain().for_each(Waker::wake);
            match queue.is_empty_or_lock() {
                Some(l) => locked = l,
                None => break,
            };
        }
    }
    wakers.drain().for_each(Waker::wake);
}

fn drain_queue<const STATE: usize, SP: SyncPrimitives>(locked: LockedQueue<Waiter, usize, SP>) {
    let mut wakers = WakerList::new();
    locked.drain_try_set_state(STATE).for_each(
        &mut wakers,
        |wakers, mut waker| {
            wakers.push(unsafe { waker.waker.take().unwrap_unchecked() });
            wakers.is_full()
        },
        |wakers| wakers.drain().for_each(Waker::wake),
    );
}

#[unsafe(no_mangle)]
fn plop(q: &WaitQueue) {
    q.notify_many_const::<32>();
}
