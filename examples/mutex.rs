use std::{
    cell::UnsafeCell,
    fmt, hint, mem,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use aiq::{
    Node, NodeState, Queue,
    queue::{LockedQueue, QueueState},
};
use pin_project_lite::pin_project;

const STATE_UNLOCKED: usize = 0;
const STATE_LOCKED: usize = 1;
const SPIN: usize = 100; // same as `std::sys::sync::mutex::futex`

fn lock_state(state: QueueState) -> Option<QueueState> {
    (state == STATE_UNLOCKED).then_some(STATE_LOCKED)
}

#[derive(Debug)]
pub struct TryLockError(());

pub struct Mutex<T: ?Sized> {
    queue: Queue<Option<Waker>>,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(data: T) -> Self {
        Self {
            queue: Queue::new(),
            data: UnsafeCell::new(data),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        match self.queue.fetch_update_state(lock_state) {
            Ok(_) => Ok(MutexGuard { mutex: self }),
            Err(_) => Err(TryLockError(())),
        }
    }

    pub async fn lock(&self) -> MutexGuard<'_, T> {
        if let Ok(guard) = self.try_lock() {
            return guard;
        }
        Lock {
            node: Node::new(&self.queue, None),
            completed: false,
        }
        .await;
        MutexGuard { mutex: self }
    }

    pub fn try_lock_owned(self: Arc<Self>) -> Result<OwnedMutexGuard<T>, TryLockError> {
        self.try_lock()
            .map(mem::forget)
            .map(|_| OwnedMutexGuard { mutex: self })
    }

    pub async fn lock_owned(self: Arc<Self>) -> OwnedMutexGuard<T> {
        mem::forget(self.lock().await);
        OwnedMutexGuard { mutex: self }
    }

    fn unlock(&self) {
        if self.queue.fetch_update_state(|_| Some(0)).is_err() {
            self.unlock_contended();
        }
    }

    #[cold]
    fn unlock_contended(&self) {
        wake_next(self.queue.lock());
    }
}

fn wake_next(mut locked: LockedQueue<'_, Option<Waker>>) {
    let mut waker = None;
    if let Some(mut waiter) = locked.dequeue() {
        waker = waiter.take();
        waiter.try_set_queue_state(|| STATE_LOCKED);
    }
    drop(locked);
    if let Some(waker) = waker {
        waker.wake();
    }
}

pin_project! {
    pub struct Lock<'a> {
        #[pin]
        node: Node<&'a Queue<Option<Waker>>>,
        completed: bool,
    }

    impl PinnedDrop for Lock<'_> {
        #[inline]
        fn drop(this: Pin<&mut Self>) {
            if !this.completed {
                this.cancel();
            }
        }
    }
}

impl Lock<'_> {
    fn poll_lock(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.project();
        let (state, queue) = this.node.state_and_queue();
        match state {
            NodeState::Unqueued(mut waiter) => {
                for _ in 0..SPIN {
                    if queue.state() == Some(STATE_UNLOCKED) {
                        break;
                    }
                    hint::spin_loop();
                }
                match waiter.fetch_update_queue_state_or_enqueue(lock_state, |_, mut waker| {
                    waker.get_or_insert_with(|| cx.waker().clone());
                    true
                }) {
                    Ok(_) => Poll::Ready(()),
                    Err(_) => Poll::Pending,
                }
            }
            NodeState::Queued(mut waiter) => {
                if !waiter.as_ref().unwrap().will_wake(cx.waker()) {
                    *waiter = Some(cx.waker().clone());
                }
                Poll::Pending
            }
            NodeState::Dequeued(_) => Poll::Ready(()),
        }
    }

    #[cold]
    fn cancel(self: Pin<&mut Self>) {
        let (state, queue) = self.project().node.state_and_queue();
        match state {
            NodeState::Unqueued(_) => unreachable!(),
            NodeState::Queued(waiter) => {
                let _ = waiter.dequeue_try_set_queue_state(|| STATE_LOCKED);
            }
            NodeState::Dequeued(_) => {
                if let Some(locked) = queue.is_empty_or_lock() {
                    wake_next(locked);
                }
            }
        }
    }
}

impl Future for Lock<'_> {
    type Output = ();

    #[cold]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = self.as_mut().poll_lock(cx);
        if res.is_ready() {
            *self.project().completed = true;
        }
        res
    }
}

pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MutexGuard").field(&&**self).finish()
    }
}

pub struct OwnedMutexGuard<T: ?Sized> {
    mutex: Arc<Mutex<T>>,
}

impl<T: ?Sized> Deref for OwnedMutexGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for OwnedMutexGuard<T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for OwnedMutexGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MutexGuard").field(&&**self).finish()
    }
}

fn main() {}
