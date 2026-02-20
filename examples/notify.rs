use std::{
    pin::{Pin, pin},
    sync::atomic::{AtomicU64, Ordering::SeqCst},
    task::{Context, Poll, Waker},
};

use aiq::{Node, NodeState, Queue};
use arrayvec::ArrayVec;
use pin_project_lite::pin_project;

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
            queue: Queue::new(),
            notify_waiters_count: AtomicU64::new(0),
        }
    }

    fn notify_single(&self, notification: Notification) {
        if let Err(mut locked) = self.queue.fetch_update_state_or_lock(|_| Some(1)) {
            let mut waiter = match notification {
                Notification::One => locked.dequeue().unwrap(),
                Notification::Last => locked.pop().unwrap(),
                _ => unreachable!(),
            };
            waiter.notification = Some(notification);
            let waker = waiter.waker.take();
            drop(waiter);
            drop(locked);
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    pub fn notify_one(&self) {
        self.notify_single(Notification::One);
    }

    pub fn notify_last(&self) {
        self.notify_single(Notification::Last);
    }

    pub fn notify_waiters(&self) {
        self.notify_waiters_count.fetch_add(1, SeqCst);
        if let Some(locked) = self.queue.is_empty_or_lock() {
            let mut wakers = ArrayVec::<Waker, 32>::new();
            {
                let mut drain = pin!(locked.drain());
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
                        drain.as_mut().execute_unlocked(|| {
                            for waker in wakers.drain(..) {
                                waker.wake();
                            }
                        });
                    }
                }
            }
            for waker in wakers {
                waker.wake();
            }
        }
    }

    pub fn notified(&self) -> Notified<'_> {
        Notified {
            node: Node::new(NotifyRef(self), Waiter::default()),
            notify_waiters_count: self.notify_waiters_count.load(SeqCst),
            polled_ready: false,
        }
    }
}

struct NotifyRef<'a>(&'a Notify);

impl AsRef<Queue<Waiter>> for NotifyRef<'_> {
    fn as_ref(&self) -> &Queue<Waiter> {
        &self.0.queue
    }
}

pin_project! {
    pub struct Notified<'a> {
        #[pin]
        node: Node<NotifyRef<'a>, Waiter>,
        notify_waiters_count: u64,
        polled_ready: bool,
    }

    impl PinnedDrop for Notified<'_> {
        fn drop(this: Pin<&mut Self>) {
            let mut this = this.project();
            if !*this.polled_ready {
                let (state, queue) = this.node.as_mut().state_and_queue();
                if let NodeState::Dequeued(waiter) = state {
                    match waiter.notification.unwrap() {
                        Notification::One => queue.0.notify_one(),
                        Notification::Last => queue.0.notify_last(),
                        _ => {}
                    }
                }
            }
        }
    }
}

impl Notified<'_> {
    fn poll_notified(self: Pin<&mut Self>, cx: Option<&mut Context<'_>>) -> Poll<()> {
        let this = self.project();
        let (state, notify) = this.node.state_and_queue();
        match state {
            NodeState::Unqueued(mut waiter) => {
                if notify.0.notify_waiters_count.load(SeqCst) == *this.notify_waiters_count
                    && waiter
                        .fetch_update_queue_state_or_enqueue(
                            |state| (state == 1).then_some(0),
                            |_, mut waiter| {
                                if let Some(cx) = cx.as_ref() {
                                    waiter.waker.get_or_insert_with(|| cx.waker().clone());
                                }
                                true
                            },
                        )
                        .is_err()
                {
                    return Poll::Pending;
                }
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
        *this.polled_ready = true;
        Poll::Ready(())
    }

    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.poll_notified(None).is_ready()
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_notified(Some(cx))
    }
}

fn main() {}
