use std::{
    cmp::min,
    mem,
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll, Waker},
};

use aiq::{
    Node, NodeState, Queue,
    queue::{LockedQueue, QueueRef},
    sync::DefaultSyncPrimitives,
};
use arrayvec::ArrayVec;
use pin_project_lite::pin_project;

const CLOSED: usize = 1;
const PERMIT_SHIFT: usize = 1;

#[derive(Default)]
struct Waiter {
    waker: Option<Waker>,
    permits: u32,
}

#[derive(Default)]
pub struct Semaphore(Queue<Waiter>);

impl Semaphore {
    pub const MAX_PERMITS: usize = usize::MAX >> 3;

    #[inline]
    pub const fn new(permits: usize) -> Self {
        assert!(permits <= Self::MAX_PERMITS);
        Self(Queue::with_state(permits << PERMIT_SHIFT))
    }

    #[inline]
    pub fn available_permits(&self) -> usize {
        self.0.state().unwrap_or(0) >> PERMIT_SHIFT
    }

    #[inline]
    pub fn add_permits(&self, n: usize) {
        if !self.try_add_permits(n) {
            self.add_permits_with_lock(n);
        }
    }

    #[inline(always)]
    fn try_add_permits(&self, n: usize) -> bool {
        self.0
            .fetch_update_state(|state| {
                let new_state = state.wrapping_add(n << PERMIT_SHIFT);
                assert!(new_state >> PERMIT_SHIFT <= Self::MAX_PERMITS);
                Some(new_state)
            })
            .is_ok()
    }

    #[cold]
    pub fn add_permits_with_lock(&self, n: usize) {
        let locked = self.0.lock();
        if self.try_add_permits(n) {
            return;
        }
        self.add_permits_locked(n, locked);
    }

    fn add_permits_locked<'a>(&'a self, mut n: usize, mut locked: LockedQueue<'a, Waiter>) {
        let mut wakers = ArrayVec::<Waker, 32>::new();
        loop {
            while let Some(mut waiter) = locked.dequeue() {
                if waiter.permits as usize > n {
                    waiter.permits -= n as u32;
                    waiter.requeue();
                    break;
                }
                n -= waiter.permits as usize;
                wakers.push(waiter.waker.take().unwrap());
                if waiter.try_set_queue_state(|| n << PERMIT_SHIFT) || wakers.is_full() {
                    break;
                }
            }
            drop(locked);
            let is_full = wakers.is_full();
            for waker in wakers.drain(..) {
                waker.wake();
            }
            if !is_full {
                break;
            }
            locked = self.0.lock();
        }
    }

    #[inline]
    pub fn forget_permits(&self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let state = (self.0)
            .fetch_update_state(|state| Some(state.wrapping_sub(n << PERMIT_SHIFT)))
            .unwrap_or(0);
        min(n, state >> PERMIT_SHIFT)
    }

    #[inline]
    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.acquire_many(1).await
    }

    #[inline]
    pub async fn acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, AcquireError> {
        match (self.0).fetch_update_state(|state| {
            if state & CLOSED != 0 {
                return None;
            }
            state.checked_sub((n as usize) << PERMIT_SHIFT)
        }) {
            Ok(_) => {}
            Err(Some(state)) if state & CLOSED != 0 => return Err(AcquireError(())),
            Err(_) => {
                Acquire {
                    node: Node::new(SemaphoreRef(self), Waiter::default()),
                    permits: n,
                }
                .await?;
            }
        }
        Ok(SemaphorePermit {
            sem: self,
            permits: n,
        })
    }

    #[inline]
    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        self.try_acquire_many(1)
    }

    #[inline]
    pub fn try_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        let n = n as usize;
        match (self.0).fetch_update_state(|state| state.checked_sub(n << PERMIT_SHIFT)) {
            Ok(_) => Ok(SemaphorePermit {
                sem: self,
                permits: n as _,
            }),
            Err(Some(state)) if state & CLOSED != 0 => Err(TryAcquireError::Closed),
            Err(_) => Err(TryAcquireError::NoPermits),
        }
    }

    #[inline]
    pub async fn acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.acquire_many_owned(1).await
    }

    #[inline]
    pub async fn acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        mem::forget(self.acquire().await?);
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: n,
        })
    }

    #[inline]
    pub fn try_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.try_acquire_many_owned(1)
    }

    #[inline]
    pub fn try_acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        mem::forget(self.try_acquire_many(n)?);
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: n as _,
        })
    }

    pub fn close(&self) {
        if let Err(locked) = (self.0).fetch_update_state_or_lock(|state| Some(state | CLOSED)) {
            let mut wakers = ArrayVec::<Waker, 32>::new();
            {
                let mut drain = pin!(locked.drain_set_state(CLOSED));
                loop {
                    let Some(mut waiter) = drain.as_mut().next() else {
                        break;
                    };
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

    #[inline]
    pub fn is_closed(&self) -> bool {
        self.0.state().is_some_and(|state| state & CLOSED != 0)
    }
}

struct SemaphoreRef<'a>(&'a Semaphore);

impl QueueRef for SemaphoreRef<'_> {
    type NodeData = Waiter;
    type SyncPrimitives = DefaultSyncPrimitives;
    fn queue(&self) -> &Queue<Self::NodeData, Self::SyncPrimitives> {
        &self.0.0
    }
}

#[derive(Debug)]
pub struct AcquireError(());
#[derive(Debug)]
pub enum TryAcquireError {
    Closed,
    NoPermits,
}

pin_project! {
    struct Acquire<'a> {
        #[pin]
        node: Node<SemaphoreRef<'a>>,
        permits: u32,
    }

    impl PinnedDrop for Acquire<'_> {
        #[inline(always)]
        fn drop(this: Pin<&mut Self>) {
            if this.permits > 0 {
                this.cancel();
            }
        }
    }
}

impl<'a> Acquire<'a> {
    fn poll_acquire(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), AcquireError>> {
        let this = self.project();
        let (state, semaphore) = this.node.state_and_queue();
        match state {
            NodeState::Unqueued(mut waiter) => {
                match waiter.fetch_update_queue_state_or_enqueue(
                    |state| {
                        if state & CLOSED != 0 {
                            return None;
                        }
                        state.checked_sub((*this.permits as usize) << PERMIT_SHIFT)
                    },
                    |state, mut waiter| {
                        waiter.permits = match state {
                            Some(s) if s & CLOSED != 0 => return false,
                            Some(s) => *this.permits - (s as u32 >> PERMIT_SHIFT),
                            None => *this.permits,
                        };
                        waiter.waker.get_or_insert_with(|| cx.waker().clone());
                        true
                    },
                ) {
                    Ok(_) => Poll::Ready(Ok(())),
                    Err(Some(state)) if state & CLOSED != 0 => Poll::Ready(Err(AcquireError(()))),
                    Err(_) => Poll::Pending,
                }
            }
            _ if semaphore.0.is_closed() => Poll::Ready(Err(AcquireError(()))),
            NodeState::Queued(mut waiter) => {
                if !waiter.waker.as_ref().unwrap().will_wake(cx.waker()) {
                    waiter.waker = Some(cx.waker().clone());
                }
                Poll::Pending
            }
            NodeState::Dequeued(_) => Poll::Ready(Ok(())),
        }
    }

    #[cold]
    fn cancel(self: Pin<&mut Self>) {
        let this = self.project();
        let (state, semaphore) = this.node.state_and_queue();
        match state {
            NodeState::Unqueued(_) => {}
            NodeState::Queued(waiter) => {
                let acquired = (*this.permits - waiter.permits) as _;
                if let Err(locked) = waiter.dequeue_try_set_queue_state(|| acquired) {
                    semaphore.0.add_permits_locked(acquired, locked);
                }
            }
            NodeState::Dequeued(_) => semaphore.0.add_permits(*this.permits as _),
        }
    }
}

impl<'a> Future for Acquire<'a> {
    type Output = Result<(), AcquireError>;

    #[cold]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = self.as_mut().poll_acquire(cx);
        if res.is_ready() {
            *self.project().permits = 0;
        }
        res
    }
}

pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: u32,
}

impl SemaphorePermit<'_> {
    pub fn forget(mut self) {
        self.permits = 0;
    }

    pub fn merge(&mut self, mut other: Self) {
        assert!(
            std::ptr::eq(self.sem, other.sem),
            "merging permits from different semaphore instances"
        );
        self.permits += other.permits;
        other.permits = 0;
    }

    pub fn split(&mut self, n: usize) -> Option<Self> {
        let n = u32::try_from(n).ok()?;

        if n > self.permits {
            return None;
        }

        self.permits -= n;

        Some(Self {
            sem: self.sem,
            permits: n,
        })
    }

    pub fn num_permits(&self) -> usize {
        self.permits as usize
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.sem.add_permits(self.permits as _);
    }
}

pub struct OwnedSemaphorePermit {
    sem: Arc<Semaphore>,
    permits: u32,
}

impl OwnedSemaphorePermit {
    pub fn forget(mut self) {
        self.permits = 0;
    }

    pub fn merge(&mut self, mut other: Self) {
        assert!(
            Arc::ptr_eq(&self.sem, &other.sem),
            "merging permits from different semaphore instances"
        );
        self.permits += other.permits;
        other.permits = 0;
    }

    pub fn split(&mut self, n: usize) -> Option<Self> {
        let n = u32::try_from(n).ok()?;

        if n > self.permits {
            return None;
        }

        self.permits -= n;

        Some(Self {
            sem: self.sem.clone(),
            permits: n,
        })
    }

    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.sem
    }

    pub fn num_permits(&self) -> usize {
        self.permits as usize
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        self.sem.add_permits(self.permits as _);
    }
}

fn main() {}

#[unsafe(no_mangle)]
fn plop(s: &Semaphore, cx: &mut Context) {
    match pin!(s.acquire()).poll(cx) {
        Poll::Ready(permits) => drop(permits.unwrap()),
        Poll::Pending => unreachable!(),
    }
}
