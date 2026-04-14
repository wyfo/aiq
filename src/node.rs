#[cfg(nightly)]
use core::pin::UnsafePinned;
use core::{hint::unreachable_unchecked, pin::Pin, ptr, ptr::NonNull};

#[cfg(not(nightly))]
use crate::unsafe_pinned::UnsafePinned;
use crate::{
    loom::{
        AtomicPtrExt,
        sync::atomic::{AtomicPtr, Ordering::*},
    },
    queue::{QueueRef, StateOrPtr},
};

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

impl RawNodeState {
    pub(crate) const fn into_ptr(self) -> *mut NodeLink {
        ptr::without_provenance_mut(self as _)
    }
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

unsafe impl<Q: QueueRef<NodeData: Send> + Send> Send for Node<Q> {}
unsafe impl<Q: QueueRef + Sync> Sync for Node<Q> {}

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
        let raw_state = self.raw_state();
        unsafe { self.state_from_raw(raw_state) }
    }

    /// # Safety
    ///
    /// Raw state must have been obtained from [`raw_state`](Self::raw_state),
    /// (or been [`Unqueued`](RawNodeState::Unqueued) if the node has never been queued).
    /// Node state must not be changed through [`state`](Self::state) in between.
    pub unsafe fn state_from_raw(
        self: Pin<&mut Self>,
        raw_state: RawNodeState,
    ) -> NodeState<'_, Q> {
        let this = unsafe { self.get_unchecked_mut() };
        let node = NonNull::new(this.node.get()).unwrap();
        let queue = &this.queue;
        match raw_state {
            RawNodeState::Unqueued => NodeState::Unqueued(NodeUnqueued { node, queue }),
            RawNodeState::Queued => {
                let locked = this.queue.queue().lock();
                match this.raw_state() {
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
            unsafe { locked.remove(&(*self.node.get()).link, ptr::null_mut(), false) };
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

unsafe impl<Q: QueueRef<NodeData: Send> + Sync> Send for NodeUnqueued<'_, Q> {}
unsafe impl<Q: QueueRef<NodeData: Sync> + Sync> Sync for NodeUnqueued<'_, Q> {}

node_getters!(NodeUnqueued<'a, Q: QueueRef>, Q::NodeData);

impl<'a, Q: QueueRef> NodeUnqueued<'a, Q> {
    #[inline]
    pub fn queue(&self) -> &'a Q {
        self.queue
    }

    #[inline]
    pub fn enqueue(self) {
        unsafe { self.queue.queue().enqueue(self.node.cast(), |_| true) };
    }

    #[inline]
    pub fn try_enqueue_with_queue_state<S: FnMut(Option<Q::State>) -> bool>(
        self,
        mut match_state: S,
    ) -> Result<(), Self> {
        let check_tail = |tail| match_state(StateOrPtr::from(tail).state());
        if unsafe { self.queue.queue().enqueue(self.node.cast(), check_tail) } {
            Ok(())
        } else {
            Err(self)
        }
    }
}

#[expect(type_alias_bounds)]
type LockedQueue<'a, Q: QueueRef> =
    crate::queue::LockedQueue<'a, Q::NodeData, Q::State, Q::SyncPrimitives>;

pub struct NodeQueued<'a, Q: QueueRef> {
    node: NonNull<NodeInner<Q::NodeData>>,
    queue: &'a Q,
    locked: LockedQueue<'a, Q>,
}

unsafe impl<'a, Q: QueueRef<NodeData: Send> + Sync> Send for NodeQueued<'a, Q> where
    LockedQueue<'a, Q>: Send
{
}
unsafe impl<'a, Q: QueueRef<NodeData: Sync> + Sync> Sync for NodeQueued<'a, Q> where
    LockedQueue<'a, Q>: Sync
{
}

node_getters!(NodeQueued<'a, Q: QueueRef>, Q::NodeData);

impl<'a, Q: QueueRef> NodeQueued<'a, Q> {
    #[inline]
    pub fn queue(&self) -> &'a Q {
        self.queue
    }

    #[inline]
    pub fn dequeue(mut self) -> (&'a Q, LockedQueue<'a, Q>) {
        let node = unsafe { self.node.cast().as_ref() };
        unsafe { self.locked.remove(node, ptr::null_mut(), false) };
        (self.queue, self.locked)
    }

    #[inline]
    #[allow(clippy::type_complexity)]
    pub fn dequeue_try_set_queue_state(
        mut self,
        state: Q::State,
    ) -> Result<(&'a Q, LockedQueue<'a, Q>), (&'a Q, LockedQueue<'a, Q>)> {
        let node = unsafe { self.node.cast().as_ref() };
        if unsafe { (self.locked).remove(node, StateOrPtr::State(state).into(), false) } {
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

unsafe impl<Q: QueueRef<NodeData: Send> + Sync> Send for NodeDequeued<'_, Q> {}
unsafe impl<Q: QueueRef<NodeData: Sync> + Sync> Sync for NodeDequeued<'_, Q> {}

node_getters!(NodeDequeued<'a, Q: QueueRef>, Q::NodeData);

impl<'a, Q: QueueRef> NodeDequeued<'a, Q> {
    #[inline]
    pub fn queue(&self) -> &'a Q {
        self.queue
    }

    #[inline]
    pub fn reset(self) -> NodeUnqueued<'a, Q> {
        let node = unsafe { self.node.cast::<NodeLink>().as_mut() };
        node.prev.store_mut(ptr::null_mut());
        node.next.store_mut(ptr::null_mut());
        NodeUnqueued {
            queue: self.queue,
            node: self.node,
        }
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
                unsafe { core::pin::Pin::new_unchecked(&mut *self.data_ptr()) }
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
