#[cfg(nightly)]
use core::pin::UnsafePinned;
#[cfg(not(loom))]
use core::sync::atomic::{AtomicPtr, Ordering::*};
use core::{hint::unreachable_unchecked, pin::Pin, ptr, ptr::NonNull};

#[cfg(loom)]
use loom::sync::atomic::{AtomicPtr, Ordering::*};

#[cfg(feature = "queue-state")]
use crate::queue::state::*;
use crate::queue::{LockedQueue, QueueRef};
#[cfg(not(nightly))]
use crate::unsafe_pinned::UnsafePinned;

#[repr(align(4))]
pub(crate) struct NodeLink {
    pub(crate) prev: AtomicPtr<NodeLink>,
    pub(crate) next: AtomicPtr<NodeLink>,
}

impl NodeLink {
    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    pub(crate) const fn new() -> Self {
        Self {
            prev: AtomicPtr::new(ptr::null_mut()),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[inline(always)]
    pub(crate) fn prev(&self) -> &NodeLink {
        unsafe { self.prev.load(Relaxed).as_ref().unwrap_unchecked() }
    }

    pub(crate) fn dequeue(&self) {
        let dequeued = ptr::without_provenance_mut(RawNodeState::Dequeued as _);
        self.prev.store(dequeued, Release);
    }

    #[inline(always)]
    pub(crate) fn next(&self) -> Option<NonNull<NodeLink>> {
        NonNull::new(self.next.load(SeqCst))
    }

    #[inline(always)]
    pub(crate) fn state(&self) -> RawNodeState {
        match self.prev.load(Acquire).addr().min(2) {
            0 => RawNodeState::Unqueued,
            1 => RawNodeState::Dequeued,
            2 => RawNodeState::Queued,
            _ => unreachable!(),
        }
    }
}

#[repr(C)]
pub(crate) struct NodeInner<T> {
    pub(crate) link: NodeLink,
    #[cfg(not(loom))]
    pub(crate) data: T,
    #[cfg(loom)]
    pub(crate) data: loom::cell::UnsafeCell<T>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RawNodeState {
    Unqueued = 0,
    Queued = 2,
    Dequeued = 1,
}

pub enum NodeState<'a, Q: QueueRef> {
    Unqueued(NodeUnqueued<'a, Q>),
    Queued(NodeQueued<'a, Q>),
    Dequeued(NodeDequeued<'a, Q>),
}

pub struct Node<Q: QueueRef> {
    queue: Q,
    node: UnsafePinned<NodeInner<Q::NodeData>>,
}

unsafe impl<Q: QueueRef> Send for Node<Q> {}
unsafe impl<Q: QueueRef> Sync for Node<Q> {}

impl<Q: QueueRef> Node<Q> {
    pub fn new(queue: Q) -> Self
    where
        Q::NodeData: Default,
    {
        Self::with_data(queue, Default::default())
    }

    #[cfg_attr(loom, const_fn::const_fn(cfg(false)))]
    pub const fn with_data(queue: Q, data: Q::NodeData) -> Self {
        Self {
            queue,
            node: UnsafePinned::new(NodeInner {
                link: NodeLink::new(),
                #[cfg(not(loom))]
                data,
                #[cfg(loom)]
                data: loom::cell::UnsafeCell::new(data),
            }),
        }
    }

    #[inline(always)]
    pub const fn queue(&self) -> &Q {
        &self.queue
    }

    #[inline(always)]
    pub fn raw_state(&self) -> RawNodeState {
        unsafe { (*self.node.get()).link.state() }
    }

    #[inline(always)]
    pub fn state(self: Pin<&mut Self>) -> NodeState<'_, Q> {
        unsafe { self.get_unchecked_mut().state_impl() }
    }

    #[inline(always)]
    fn state_impl(&self) -> NodeState<'_, Q> {
        let node = NonNull::new(self.node.get()).unwrap();
        let queue = &self.queue;
        match self.raw_state() {
            RawNodeState::Unqueued => NodeState::Unqueued(NodeUnqueued { node, queue }),
            RawNodeState::Queued => {
                let locked = self.queue.queue().lock();
                match self.raw_state() {
                    RawNodeState::Unqueued => unsafe { unreachable_unchecked() },
                    RawNodeState::Queued => NodeState::Queued(NodeQueued {
                        node,
                        queue,
                        locked,
                    }),
                    RawNodeState::Dequeued => NodeState::Dequeued(NodeDequeued { node, queue }),
                }
            }
            RawNodeState::Dequeued => NodeState::Dequeued(NodeDequeued { node, queue }),
        }
    }

    #[cold]
    #[inline(never)]
    fn dequeue(&mut self) {
        let mut locked = self.queue.queue().lock();
        if self.raw_state() == RawNodeState::Queued {
            unsafe { locked.remove(&(*self.node.get()).link, ptr::null_mut()) };
        }
    }
}

impl<Q: QueueRef> Drop for Node<Q> {
    #[inline(always)]
    fn drop(&mut self) {
        if self.raw_state() == RawNodeState::Queued {
            self.dequeue();
        }
    }
}

pub struct NodeUnqueued<'a, Q: QueueRef> {
    node: NonNull<NodeInner<Q::NodeData>>,
    queue: &'a Q,
}

node_getters!(NodeUnqueued<'a, Q: QueueRef>, Q::NodeData);

impl<'a, Q: QueueRef> NodeUnqueued<'a, Q> {
    pub fn queue(&self) -> &'a Q {
        self.queue
    }

    #[inline]
    pub fn enqueue(self) {
        #[cfg(feature = "queue-state")]
        let new_tail = |tail| {
            let prev = match StateOrTail::from(tail) {
                StateOrTail::Tail(tail) => tail.as_ptr(),
                _ => ptr::null_mut(),
            };
            Some((StateOrTail::Tail(self.node.cast()).into(), Some(prev)))
        };
        #[cfg(not(feature = "queue-state"))]
        let new_tail = |tail| Some((self.node.as_ptr().cast(), Some(tail)));
        unsafe { self.queue.queue().enqueue(self.node.cast(), new_tail) };
    }

    #[cfg(feature = "queue-state")]
    pub fn fetch_update_queue_state_or_enqueue<
        F: FnMut(QueueState) -> Option<QueueState>,
        I: FnMut(Option<QueueState>, Pin<&mut Q::NodeData>) -> bool,
    >(
        mut self,
        mut f: F,
        mut init: I,
    ) -> Result<(), Option<QueueState>> {
        let Self { node, queue } = self;
        let new_tail = |tail| match StateOrTail::from(tail) {
            StateOrTail::State(state) => match f(state) {
                Some(new_state) => Some((StateOrTail::State(new_state).into(), None)),
                None if self.with_data_mut(|data| init(Some(state), data)) => Some((
                    StateOrTail::Tail(self.node.cast()).into(),
                    Some(ptr::null_mut()),
                )),
                None => None,
            },
            StateOrTail::Tail(tail) if self.with_data_mut(|data| init(None, data)) => Some((
                StateOrTail::Tail(self.node.cast()).into(),
                Some(tail.as_ptr()),
            )),
            StateOrTail::Tail(_) => None,
        };
        match unsafe { queue.queue().enqueue(node.cast(), new_tail) } {
            Some(tail) => Err(match tail.into() {
                StateOrTail::State(state) => Some(state),
                _ => None,
            }),
            None => Ok(()),
        }
    }
}

pub struct NodeQueued<'a, Q: QueueRef> {
    node: NonNull<NodeInner<Q::NodeData>>,
    queue: &'a Q,
    locked: LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>,
}

node_getters!(NodeQueued<'a, Q: QueueRef>, Q::NodeData);

impl<'a, Q: QueueRef> NodeQueued<'a, Q> {
    pub fn queue(&self) -> &'a Q {
        self.queue
    }

    pub fn dequeue(mut self) -> (&'a Q, LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>) {
        let node = unsafe { self.node.cast().as_ref() };
        unsafe { self.locked.remove(node, ptr::null_mut()) };
        (self.queue, self.locked)
    }

    #[cfg(feature = "queue-state")]
    #[allow(clippy::type_complexity)]
    pub fn dequeue_try_set_queue_state(
        mut self,
        state: QueueState,
    ) -> Result<
        (&'a Q, LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>),
        (&'a Q, LockedQueue<'a, Q::NodeData, Q::SyncPrimitives>),
    > {
        let node = unsafe { self.node.cast().as_ref() };
        if unsafe { (self.locked).remove(node, StateOrTail::State(state).into()) } {
            Ok((self.queue, self.locked))
        } else {
            Err((self.queue, self.locked))
        }
    }
}

pub struct NodeDequeued<'a, Q: QueueRef> {
    node: NonNull<NodeInner<Q::NodeData>>,
    queue: &'a Q,
}

node_getters!(NodeDequeued<'a, Q: QueueRef>, Q::NodeData);

impl<'a, Q: QueueRef> NodeDequeued<'a, Q> {
    pub fn queue(&self) -> &'a Q {
        self.queue
    }
}

macro_rules! node_getters {
    ($node:ident<$($lf:lifetime,)* $($arg:ident $(:$bound:path)?),*>, $data:ty) => {
        impl<$($lf,)* $($arg $(:$bound)?),*> $node<$($lf,)* $($arg),*> {
            #[cfg(not(loom))]
            fn data_ptr(&self) -> *mut $data {
                unsafe { &raw mut (*self.node.as_ptr().cast::<crate::node::NodeInner<$data>>()).data }
            }

            #[cfg(loom)]
            fn data_ptr(&self) -> *mut loom::cell::UnsafeCell<$data> {
                unsafe { &raw mut (*self.node.as_ptr().cast::<crate::node::NodeInner<$data>>()).data }
            }

            #[cfg(not(loom))]
            #[inline]
            pub fn data(&self) -> &$data {
                unsafe { &*self.data_ptr() }
            }

            #[cfg(not(loom))]
            #[inline]
            pub fn data_mut(&mut self) -> core::pin::Pin<&mut $data> {
                unsafe { Pin::new_unchecked(&mut *self.data_ptr()) }
            }

            #[inline]
            #[doc(hidden)]
            pub fn with_data<F: FnOnce(&$data) -> R, R>(&self, f: F) -> R {
                #[cfg(not(loom))]
                return f(self.data());
                #[cfg(loom)]
                return unsafe { (*self.data_ptr()).with(|data| f(&*data)) }
            }

            #[inline]
            #[doc(hidden)]
            pub fn with_data_mut<F: FnOnce(core::pin::Pin<&mut $data>) -> R, R>(&mut self, f: F) -> R {
                #[cfg(not(loom))]
                return f(self.data_mut());
                #[cfg(loom)]
                return unsafe { (*self.data_ptr()).with_mut(|data| f(core::pin::Pin::new_unchecked(&mut *data))) }
            }
        }


        #[cfg(not(loom))]
        impl<$($lf,)* $($arg $(:$bound)?),*> core::ops::Deref for $node<$($lf,)* $($arg),*> {
            type Target = $data;
            #[inline]
            fn deref(&self) -> &Self::Target {
                self.data()
            }
        }

        #[cfg(not(loom))]
        impl<$($lf,)* $($arg $(:$bound)?),*> core::ops::DerefMut for $node<$($lf,)* $($arg),*>
        where
            Self::Target: Unpin
        {
            #[inline]
            fn deref_mut(&mut self) -> &mut Self::Target {
                self.data_mut().get_mut()
            }
        }
    };
}
pub(crate) use node_getters;
