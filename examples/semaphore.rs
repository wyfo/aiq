use std::{
    cmp::min,
    mem,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use aiq::{Node, NodeState, Queue, queue::LockedQueue, queue_ref};
use arrayvec::ArrayVec;
use pin_project_lite::pin_project;

const CLOSED: usize = 1;
const PERMIT_SHIFT: usize = 1;

#[derive(Default)]
struct Waiter {
    waker: Option<Waker>,
    permits_remaining: u32,
}

#[derive(Default)]
pub struct Semaphore(Queue<Waiter, usize>);

impl Semaphore {
    pub const MAX_PERMITS: usize = usize::MAX >> 3;

    #[inline(always)]
    const fn check_add_permits(state: usize, add: usize) -> usize {
        let current = state >> PERMIT_SHIFT;
        assert!(add <= Self::MAX_PERMITS - current, "permits overflow");
        state + (add << PERMIT_SHIFT)
    }

    #[inline(always)]
    const fn check_acquire_permits(state: usize, acquire: u32) -> Option<usize> {
        if state & CLOSED != 0 {
            return None;
        }
        state.checked_sub((acquire as usize) << PERMIT_SHIFT)
    }

    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    #[inline]
    pub const fn new(permits: usize) -> Self {
        Self(Queue::with_state(Self::check_add_permits(0, permits)))
    }

    #[inline]
    pub fn available_permits(&self) -> usize {
        self.0.state().unwrap_or(0) >> PERMIT_SHIFT
    }

    #[inline]
    pub fn add_permits(&self, permits: usize) {
        if permits == 0 {
            return;
        }
        self.0.fetch_update_state_with_lock(
            |state| Self::check_add_permits(state, permits),
            |locked| self.add_permits_locked(permits, locked),
        );
    }

    fn add_permits_locked<'a>(
        &'a self,
        mut permits: usize,
        mut locked: LockedQueue<'a, Waiter, usize>,
    ) {
        let mut wakers = ArrayVec::<Waker, 32>::new();
        'outer: loop {
            loop {
                let mut waiter = locked.dequeue().unwrap();
                let requeue = waiter.with_data_mut(|mut waiter| {
                    if waiter.permits_remaining as usize > permits {
                        waiter.permits_remaining -= permits as u32;
                        return true;
                    }
                    permits -= waiter.permits_remaining as usize;
                    wakers.push(waiter.waker.take().unwrap());
                    false
                });
                if requeue {
                    waiter.requeue();
                    break 'outer;
                } else if waiter.try_set_queue_state(permits << PERMIT_SHIFT) || permits == 0 {
                    break 'outer;
                } else if wakers.is_full() {
                    break;
                }
            }
            drop(locked);
            wakers.drain(..).for_each(Waker::wake);
            match (self.0)
                .fetch_update_state_or_lock(|state| Self::check_add_permits(state, permits))
            {
                Some(l) => locked = l,
                None => return,
            }
        }
        drop(locked);
        wakers.into_iter().for_each(Waker::wake);
    }

    #[inline]
    pub fn forget_permits(&self, permits: usize) -> usize {
        if permits == 0 {
            return 0;
        }
        let state = (self.0)
            .fetch_update_state(|state| Some(state.wrapping_sub(permits << PERMIT_SHIFT)))
            .unwrap_or(0);
        min(permits, state >> PERMIT_SHIFT)
    }

    #[inline]
    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.acquire_many(1).await
    }

    #[inline]
    pub async fn acquire_many(&self, permits: u32) -> Result<SemaphorePermit<'_>, AcquireError> {
        let acquire = |state| Self::check_acquire_permits(state, permits);
        if self.0.fetch_update_state(acquire).is_err() {
            let node = Node::new(SemaphoreRef(self));
            Acquire { node, permits }.await?;
        }
        Ok(SemaphorePermit { sem: self, permits })
    }

    #[inline]
    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        self.try_acquire_many(1)
    }

    #[inline]
    pub fn try_acquire_many(&self, permits: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        let acquire = |state| Self::check_acquire_permits(state, permits);
        match self.0.fetch_update_state(acquire) {
            Ok(_) => Ok(SemaphorePermit { sem: self, permits }),
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
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        mem::forget(self.acquire().await?);
        Ok(OwnedSemaphorePermit { sem: self, permits })
    }

    #[inline]
    pub fn try_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.try_acquire_many_owned(1)
    }

    #[inline]
    pub fn try_acquire_many_owned(
        self: Arc<Self>,
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        mem::forget(self.try_acquire_many(permits)?);
        Ok(OwnedSemaphorePermit { sem: self, permits })
    }

    pub fn close(&self) {
        if let Some(locked) = self.0.fetch_update_state_or_lock(|state| state | CLOSED) {
            let mut wakers = ArrayVec::<Waker, 32>::new();
            locked.drain_try_set_state(CLOSED).for_each(
                &mut wakers,
                |wakers, mut waiter| {
                    wakers.push(waiter.waker.take().unwrap());
                    wakers.is_full()
                },
                |wakers| wakers.drain(..).for_each(Waker::wake),
            );
            wakers.into_iter().for_each(Waker::wake);
        }
    }

    #[inline]
    pub fn is_closed(&self) -> bool {
        self.0.state().is_some_and(|state| state & CLOSED != 0)
    }
}

struct SemaphoreRef<'a>(&'a Semaphore);
queue_ref!(
    SemaphoreRef<'a>,
    NodeData = Waiter,
    State = usize,
    &self.0.0
);

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
        match this.node.state() {
            NodeState::Unqueued(mut waiter) => loop {
                let Err(state) = waiter.queue().0.0.fetch_update_state(|state| {
                    Semaphore::check_acquire_permits(state, *this.permits)
                }) else {
                    break Poll::Ready(Ok(()));
                };
                if state.is_some_and(|s| s & CLOSED != 0) {
                    break Poll::Ready(Err(AcquireError(())));
                }
                waiter.with_data_mut(|mut waiter| {
                    waiter.permits_remaining =
                        *this.permits - (state.unwrap_or(0) as u32 >> PERMIT_SHIFT);
                    waiter.waker.get_or_insert_with(|| cx.waker().clone());
                });
                match waiter.try_enqueue_with_queue_state(state) {
                    Ok(_) => break Poll::Pending,
                    Err(w) => waiter = w,
                }
            },
            NodeState::Queued(waiter) if waiter.queue().0.is_closed() => {
                Poll::Ready(Err(AcquireError(())))
            }
            NodeState::Queued(mut waiter) => {
                waiter.with_data_mut(|mut waiter| {
                    if !waiter.waker.as_ref().unwrap().will_wake(cx.waker()) {
                        waiter.waker = Some(cx.waker().clone());
                    }
                });
                Poll::Pending
            }
            NodeState::Dequeued(waiter) if waiter.queue().0.is_closed() => {
                Poll::Ready(Err(AcquireError(())))
            }
            NodeState::Dequeued(_) => Poll::Ready(Ok(())),
        }
    }

    #[cold]
    fn cancel(self: Pin<&mut Self>) {
        let this = self.project();
        match this.node.state() {
            NodeState::Unqueued(_) => {}
            NodeState::Queued(mut waiter) => {
                let acquired = (*this.permits - waiter.with_data_mut(|w| w.permits_remaining)) as _;
                if let Err((sem, locked)) = waiter.dequeue_try_set_queue_state(acquired) {
                    sem.0.add_permits_locked(acquired, locked);
                }
            }
            NodeState::Dequeued(waiter) => waiter.queue().0.add_permits(*this.permits as _),
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
