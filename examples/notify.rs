// #![forbid(unsafe_code)]
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::{
    ops::Deref,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker, ready},
};

use aiq::{Node, NodeState, Queue, queue::LockedQueue, queue_ref};
use arrayvec::ArrayVec;
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering::Acquire, Ordering::SeqCst, fence};
use pin_project_lite::pin_project;

const STATE_UNNOTIFIED: usize = 0;
const STATE_NOTIFIED: usize = 1;

#[derive(Default)]
struct Waiter {
    waker: Option<Waker>,
    notification: Option<Notification>,
}

#[derive(Clone, Copy)]
enum Notification {
    One,
    Last,
    All,
}

#[derive(Default)]
pub struct Notify {
    queue: Queue<Waiter, usize>,
    generation: AtomicU64,
}

impl Notify {
    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    #[inline]
    pub const fn new() -> Self {
        Self {
            queue: Queue::with_state(STATE_UNNOTIFIED),
            generation: AtomicU64::new(0),
        }
    }

    #[inline(always)]
    fn notify_single(&self, notification: Notification) {
        self.queue.fetch_update_state_or_locked(
            |_| STATE_NOTIFIED,
            |locked| self.wake_single(notification, locked),
        );
    }

    fn wake_single<'a>(
        &'a self,
        notification: Notification,
        mut locked: LockedQueue<'a, Waiter, usize>,
    ) {
        let waker;
        match notification {
            Notification::One => {
                let mut waiter = locked.dequeue().unwrap();
                waker = waiter.with_data_mut(|mut waiter| {
                    waiter.notification = Some(notification);
                    waiter.waker.take()
                });
                waiter.try_set_queue_state(STATE_UNNOTIFIED);
            }
            Notification::Last => {
                let mut waiter = locked.pop().unwrap();
                waker = waiter.with_data_mut(|mut waiter| {
                    waiter.notification = Some(notification);
                    waiter.waker.take()
                });
                waiter.try_set_queue_state(STATE_UNNOTIFIED);
            }
            _ => unreachable!(),
        }
        drop(locked);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    #[inline]
    pub fn notify_one(&self) {
        self.notify_single(Notification::One);
    }

    #[inline]
    pub fn notify_last(&self) {
        self.notify_single(Notification::Last);
    }

    pub fn notify_waiters(&self) {
        let locked = self.queue.lock();
        #[cfg(loom)]
        fence(SeqCst);
        self.generation.fetch_add(1, SeqCst);
        #[cfg(loom)]
        fence(SeqCst);
        let mut wakers = ArrayVec::<Waker, 32>::new();
        locked.drain_try_set_state(STATE_UNNOTIFIED).for_each(
            &mut wakers,
            |wakers, mut waiter| {
                waiter.notification = Some(Notification::All);
                if let Some(waker) = waiter.waker.take() {
                    wakers.push(waker);
                }
                wakers.is_full()
            },
            |wakers| wakers.drain(..).for_each(Waker::wake),
        );
        wakers.into_iter().for_each(Waker::wake);
    }

    #[inline]
    pub fn notified(&self) -> Notified<'_> {
        #[cfg(loom)]
        fence(SeqCst);
        Notified {
            inner: NotifiedInner {
                node: Node::new(NotifyRef(self)),
                generation: self.generation.load(SeqCst),
                completed: false,
            },
        }
    }

    #[inline]
    pub fn notified_owned(self: Arc<Self>) -> OwnedNotified {
        #[cfg(loom)]
        fence(SeqCst);
        OwnedNotified {
            inner: NotifiedInner {
                generation: self.generation.load(SeqCst),
                node: Node::new(NotifyRef(self)),
                completed: false,
            },
        }
    }
}

struct NotifyRef<N>(N);
queue_ref!(NotifyRef<N: Deref<Target = Notify>>, NodeData = Waiter, State = usize, &self.0.queue);

pin_project! {
    struct NotifiedInner<N: Deref<Target = Notify>> {
        #[pin]
        node: Node<NotifyRef<N>>,
        generation: u64,
        completed: bool,
    }

    impl<N: Deref<Target = Notify>> PinnedDrop for NotifiedInner<N> {
        fn drop(mut this: Pin<&mut Self>) {
            if !this.completed {
               this.cancel();
            }
        }
    }
}

impl<N: Deref<Target = Notify>> NotifiedInner<N> {
    fn poll_notified(mut self: Pin<&mut Self>, cx: Option<&mut Context<'_>>) -> Poll<()> {
        if ready!(self.as_mut().poll_notified_impl(cx)) {
            self.as_mut().cancel();
        }
        *self.project().completed = true;
        Poll::Ready(())
    }

    #[inline(always)]
    fn poll_notified_impl(self: Pin<&mut Self>, cx: Option<&mut Context<'_>>) -> Poll<bool> {
        let mut this = self.project();
        match this.node.as_mut().state() {
            NodeState::Unqueued(mut waiter) => loop {
                #[cfg(loom)]
                fence(SeqCst);
                if waiter.queue().0.generation.load(SeqCst) != *this.generation {
                    break Poll::Ready(false);
                }
                let Err(state) = waiter.queue().0.queue.fetch_update_state(|state| {
                    (state == STATE_NOTIFIED).then_some(STATE_UNNOTIFIED)
                }) else {
                    break Poll::Ready(false);
                };
                if let Some(cx) = cx.as_ref() {
                    waiter.with_data_mut(|mut waiter| {
                        waiter.waker.get_or_insert_with(|| cx.waker().clone());
                    });
                }
                let notify = waiter.queue();
                match waiter.try_enqueue_with_queue_state(|s| s == state) {
                    Ok(_) => {
                        #[cfg(loom)]
                        fence(SeqCst);
                        break if notify.0.generation.load(SeqCst) != *this.generation {
                            Poll::Ready(true)
                        } else {
                            Poll::Pending
                        };
                    }
                    Err(w) => waiter = w,
                }
            },
            NodeState::Queued(mut waiter) => {
                #[cfg(loom)]
                fence(SeqCst);
                if waiter.queue().0.generation.load(SeqCst) != *this.generation {
                    waiter.dequeue_try_set_queue_state(STATE_UNNOTIFIED).ok();
                    Poll::Ready(false)
                } else {
                    if let Some(cx) = cx {
                        waiter.with_data_mut(|mut waiter| {
                            if (waiter.waker.as_ref())
                                .is_none_or(|waker| !waker.will_wake(cx.waker()))
                            {
                                waiter.waker = Some(cx.waker().clone());
                            }
                        });
                    }
                    Poll::Pending
                }
            }
            NodeState::Dequeued(_) => Poll::Ready(false),
        }
    }

    #[cold]
    fn cancel(self: Pin<&mut Self>) {
        match self.project().node.state() {
            NodeState::Unqueued(_) => {}
            NodeState::Queued(waiter) => {
                waiter.dequeue_try_set_queue_state(STATE_UNNOTIFIED).ok();
            }
            NodeState::Dequeued(mut waiter) => {
                match waiter.with_data_mut(|w| w.notification.unwrap()) {
                    Notification::One => waiter.queue().0.notify_one(),
                    Notification::Last => waiter.queue().0.notify_last(),
                    _ => {}
                }
            }
        }
    }
}

pin_project! {
    pub struct Notified<'a> {
        #[pin]
        inner: NotifiedInner<&'a Notify>
    }
}

impl Notified<'_> {
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.project().inner.poll_notified(None).is_ready()
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll_notified(Some(cx))
    }
}

pin_project! {
    pub struct OwnedNotified {
        #[pin]
        inner: NotifiedInner<Arc<Notify>>
    }
}

impl OwnedNotified {
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.project().inner.poll_notified(None).is_ready()
    }
}

impl Future for OwnedNotified {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll_notified(Some(cx))
    }
}

fn main() {}
