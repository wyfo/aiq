use std::{
    pin::{Pin, pin},
    sync::atomic::{AtomicU64, Ordering::SeqCst},
    task::{Context, Poll, Waker},
};

use aiq::{
    Node, NodeState, Queue,
    queue::{QueueRef, QueueState},
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
    notify_waiters_count: AtomicU64,
}

impl Notify {
    pub const fn new() -> Self {
        Self {
            queue: Queue::with_state(STATE_UNNOTIFIED),
            notify_waiters_count: AtomicU64::new(0),
        }
    }

    #[inline(always)]
    fn notify_single(&self, notification: Notification) {
        if (self.queue.fetch_update_state(|_| Some(STATE_NOTIFIED))).is_err() {
            self.notify_single_cold(notification);
        }
    }

    #[cold]
    fn notify_single_cold(&self, notification: Notification) {
        let mut locked = self.queue.lock();
        if locked.fetch_update_state(|_| Some(STATE_NOTIFIED)).is_ok() {
            return;
        }
        let mut waiter = match notification {
            Notification::One => locked.dequeue().unwrap(),
            Notification::Last => locked.pop().unwrap(),
            _ => unreachable!(),
        };
        waiter.notification = Some(notification);
        let waker = waiter.waker.take();
        waiter.try_set_queue_state(|| STATE_UNNOTIFIED);
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

    #[inline]
    pub fn notify_waiters(&self) {
        if self.queue.is_empty() {
            self.notify_waiters_count.fetch_add(1, SeqCst);
        } else {
            self.notify_waiters_cold();
        }
    }

    #[cold]
    pub fn notify_waiters_cold(&self) {
        let locked = self.queue.lock();
        self.notify_waiters_count.fetch_add(1, SeqCst);
        let mut wakers = ArrayVec::<Waker, 32>::new();
        {
            let mut drain = pin!(locked.drain_set_state(STATE_UNNOTIFIED));
            loop {
                let Some(mut waiter) = drain.as_mut().next() else {
                    break;
                };
                waiter.notification = Some(Notification::All);
                wakers.push(waiter.waker.take().unwrap());
                drop(waiter);
                if wakers.is_full() {
                    (drain.as_mut()).execute_unlocked(|| wakers.drain(..).for_each(Waker::wake));
                }
            }
        }
        wakers.into_iter().for_each(Waker::wake);
    }

    pub fn notified(&self) -> Notified<'_> {
        Notified {
            node: Node::new(NotifyRef(self), Waiter::default()),
            notify_waiters_count: self.notify_waiters_count.load(SeqCst),
            completed: false,
        }
    }
}

struct NotifyRef<'a>(&'a Notify);

impl QueueRef for NotifyRef<'_> {
    type NodeData = Waiter;
    type SyncPrimitives = DefaultSyncPrimitives;
    fn queue(&self) -> &Queue<Self::NodeData, Self::SyncPrimitives> {
        &self.0.queue
    }
}

pin_project! {
    pub struct Notified<'a> {
        #[pin]
        node: Node<NotifyRef<'a>>,
        notify_waiters_count: u64,
        completed: bool,
    }

    impl PinnedDrop for Notified<'_> {
        fn drop(mut this: Pin<&mut Self>) {
            if !this.completed {
               this.cancel();
            }
        }
    }
}

impl Notified<'_> {
    fn poll_notified(self: Pin<&mut Self>, cx: Option<&mut Context<'_>>) -> Poll<()> {
        let mut this = self.project();
        let (state, notify) = this.node.as_mut().state_and_queue();
        match state {
            NodeState::Unqueued(waiter) => {
                if waiter
                    .fetch_update_queue_state_or_enqueue(
                        |state| (state == STATE_NOTIFIED).then_some(STATE_UNNOTIFIED),
                        |_, mut waiter| {
                            if let Some(cx) = cx.as_ref() {
                                waiter.waker.get_or_insert_with(|| cx.waker().clone());
                            }
                            true
                        },
                    )
                    .is_err()
                    && notify.0.notify_waiters_count.load(SeqCst) == *this.notify_waiters_count
                {
                    return Poll::Pending;
                }
            }
            NodeState::Queued(waiter)
                if notify.0.notify_waiters_count.load(SeqCst) != *this.notify_waiters_count =>
            {
                waiter.dequeue_try_set_queue_state(|| STATE_UNNOTIFIED).ok();
            }
            NodeState::Queued(mut waiter) => {
                if let Some(cx) = cx
                    && (waiter.waker.as_ref()).is_none_or(|waker| !waker.will_wake(cx.waker()))
                {
                    waiter.waker = Some(cx.waker().clone());
                }
                return Poll::Pending;
            }
            NodeState::Dequeued(_) => {}
        }
        *this.completed = true;
        Poll::Ready(())
    }

    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.poll_notified(None).is_ready()
    }

    #[cold]
    fn cancel(self: Pin<&mut Self>) {
        let this = self.project();
        let (state, queue) = this.node.state_and_queue();
        match state {
            NodeState::Unqueued(_) => {}
            NodeState::Queued(waiter) => {
                waiter.dequeue_try_set_queue_state(|| STATE_UNNOTIFIED).ok();
            }
            NodeState::Dequeued(waiter) => match waiter.notification.unwrap() {
                Notification::One => queue.0.notify_one(),
                Notification::Last => queue.0.notify_last(),
                _ => {}
            },
        }
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_notified(Some(cx))
    }
}

fn main() {}
