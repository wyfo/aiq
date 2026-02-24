use std::{
    ops::Deref,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::SeqCst},
    },
    task::{Context, Poll, Waker, ready},
};

use aiq::{
    Node, NodeState, Queue,
    queue::{LockedQueue, QueueRef, QueueState},
    sync::DefaultSyncPrimitives,
};
use arrayvec::ArrayVec;
use pin_project_lite::pin_project;

const STATE_UNNOTIFIED: QueueState = 0;
const STATE_NOTIFIED: QueueState = 1;

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
    queue: Queue<Waiter>,
    generation: AtomicU64,
}

impl Notify {
    pub const fn new() -> Self {
        Self {
            queue: Queue::with_state(STATE_UNNOTIFIED),
            generation: AtomicU64::new(0),
        }
    }

    #[inline(always)]
    fn notify_single(&self, notification: Notification) {
        self.queue.fetch_update_state_with_lock(
            |_| STATE_NOTIFIED,
            |locked| self.wake_single(notification, locked),
        );
    }

    fn wake_single<'a>(&'a self, notification: Notification, mut locked: LockedQueue<'a, Waiter>) {
        let mut waiter = match notification {
            Notification::One => locked.dequeue().unwrap(),
            Notification::Last => locked.pop().unwrap(),
            _ => unreachable!(),
        };
        waiter.notification = Some(notification);
        let waker = waiter.waker.take();
        waiter.try_set_queue_state(STATE_UNNOTIFIED);
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
        self.generation.fetch_add(1, SeqCst);
        let mut wakers = ArrayVec::<Waker, 32>::new();
        {
            let mut drain = pin!(locked.drain_try_set_state(STATE_UNNOTIFIED));
            loop {
                let Some(mut waiter) = drain.as_mut().next() else {
                    break;
                };
                waiter.notification = Some(Notification::All);
                if let Some(waker) = waiter.waker.take() {
                    wakers.push(waker);
                }
                drop(waiter);
                if wakers.is_full() {
                    (drain.as_mut()).execute_unlocked(|| wakers.drain(..).for_each(Waker::wake));
                }
            }
        }
        wakers.into_iter().for_each(Waker::wake);
    }

    #[inline]
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            inner: NotifiedInner {
                node: Node::new(NotifyRef(self), Waiter::default()),
                generation: self.generation.load(SeqCst),
                completed: false,
            },
        }
    }

    #[inline]
    pub fn notified_owned(self: Arc<Self>) -> OwnedNotified {
        OwnedNotified {
            inner: NotifiedInner {
                generation: self.generation.load(SeqCst),
                node: Node::new(NotifyRef(self), Waiter::default()),
                completed: false,
            },
        }
    }
}

struct NotifyRef<N>(N);

impl<N: Deref<Target = Notify>> QueueRef for NotifyRef<N> {
    type NodeData = Waiter;
    type SyncPrimitives = DefaultSyncPrimitives;
    fn queue(&self) -> &Queue<Self::NodeData, Self::SyncPrimitives> {
        &self.0.queue
    }
}

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
            NodeState::Unqueued(waiter) => {
                let notify = waiter.queue();
                let mut enqueued = true;
                match waiter.fetch_update_queue_state_or_enqueue(
                    |state| (state == STATE_NOTIFIED).then_some(STATE_UNNOTIFIED),
                    |_, mut waiter| {
                        if notify.0.generation.load(SeqCst) != *this.generation {
                            enqueued = false;
                            return false;
                        }
                        if let Some(cx) = cx.as_ref() {
                            waiter.waker.get_or_insert_with(|| cx.waker().clone());
                        }
                        true
                    },
                ) {
                    Ok(_) => Poll::Ready(false),
                    Err(_) if !enqueued || notify.0.generation.load(SeqCst) != *this.generation => {
                        Poll::Ready(enqueued)
                    }
                    Err(_) => Poll::Pending,
                }
            }
            NodeState::Queued(waiter)
                if waiter.queue().0.generation.load(SeqCst) != *this.generation =>
            {
                waiter.dequeue_try_set_queue_state(STATE_UNNOTIFIED).ok();
                Poll::Ready(false)
            }
            NodeState::Queued(mut waiter) => {
                if let Some(cx) = cx
                    && (waiter.waker.as_ref()).is_none_or(|waker| !waker.will_wake(cx.waker()))
                {
                    waiter.waker = Some(cx.waker().clone());
                }
                Poll::Pending
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
            NodeState::Dequeued(waiter) => match waiter.notification.unwrap() {
                Notification::One => waiter.queue().0.notify_one(),
                Notification::Last => waiter.queue().0.notify_last(),
                _ => {}
            },
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
