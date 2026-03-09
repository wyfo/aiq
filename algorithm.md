# Algorithm

`aiq` stands for Atomic Intrusive Queue. It is a doubly intrusive linked-list of pinned nodes, which support concurrent tail-insertion and serialized removal.

Code fragments included in the explanation are simplified for clarity.

## Sync primitives

The algorithm uses two synchronization primitives, gathered in a `SyncPrimitives` trait:

```rust
pub trait SyncPrimitives {
    /// used for serializing nodes removal operations
    type Mutex: Mutex;
    /// used to synchronize nodes insertion and removal
    type Parker: Parker;
    /// spin loop bound before parking thread
    const SPIN_BEFORE_PARK: usize;
}
```

Mutexes and parkers are common primitives which don't need to be detailed here. The crate provides several implementations (std-based, pthread-based, etc.) depending on compilation features.

## Queue and node definitions

The algorithm uses the following structs:

```rust
pub struct Queue<T, S: QueueState, SP: SyncPrimitives = DefaultSyncPrimitives> {
    tail: AtomicPtr<Tail<S>>,
    head: AtomicPtr<NodeLink>,
    #[cfg(not(target_arch = "x86_64"))]
    parked_node: AtomicPtr<AtomicPtr<NodeLink>>,
    mutex: SP::Mutex,
    parker: SP::Parker,
    _node_data: PhantomData<T>,
}

#[repr(align(4))]
struct NodeLink {
    pub(crate) prev: AtomicPtr<NodeLink>,
    pub(crate) next: AtomicPtr<NodeLink>,
}
```

where `NodeLink` is the [linking part](https://www.youtube.com/watch?v=eVTXPUF4Oz4) of a bigger `repr(C)` node struct covered in a next [section](#node-data-and-aliasing). `T` parameter of `Queue` represents arbitrary data carried by the node; it has no importance for the algorithm itself, but matters when queue is used.

`Tail<S>` is just a marker type to indicate the tail pointer may contain a [queue state](#queue-state) when there is no node. `*mut Tail<S>` can be considered as a `union { state: S, ptr: NonNull<NodeLink> }`.

`NodeLink::prev` is also used to carry the node state using special values:
- 0: the node is unqueued
- 1: the node is dequeued
- 2: the node is queued and is the queue's head
- address of a previous node: the node is queued

Because nodes' pointers are stored in the queue, the pointers must remain valid as long as the node are queued. This is achieved by requiring the node to be pinned to be inserted. It also means that nodes must be removed from the queue before being dropped. This is done in node's [destructor](#node-data-and-aliasing).

Synchronization of node insertion and removal using parker is done differently depending on the platform, as detailed in the dedicated [section](#parking-algorithm).

## Node insertion

Here is the insertion simplified code:
```rust
const HEAD_MARKER: *mut NodeLink = ptr::without_provenance_mut(2);
impl<T, S: QueueState, SP: SyncPrimitives> Queue<T, S, SP> {
    unsafe fn enqueue(&self, node: *mut NodeLink) {
        // CAS loop to set the node as the new tail
        // while setting the previous tail as node's prev pointer
        let mut tail = self.tail.load(Relaxed);
        loop {
            // prev must be set to mark the node as enqueued, with a special value if it is the head. 
            // `Relaxed` order is ok, as the state is only accessed by the thread/task inserting
            // the node.
            let prev = if tail.is_null() { HEAD_MARKER } else { tail };
            (*node).prev.store(prev, Relaxed);
            // Updating queue's tail pointer with `SeqCst` ordering is required to be 
            // able to load the tail with `SeqCst` and not miss any enqueued node. 
            // Contrary to mutex-protected queues which rely on the total modification order 
            // of the mutex's atomic state, this queue relies on the `SeqCst` total order, 
            // as a `SeqCst` load is a lot less expensive than an atomic RMW operation.
            match self.tail.compare_exchange_weak(tail, new_tail, SeqCst, Relaxed) {
                Ok(_) => break,
                Err(t) => tail = t,
            }
        }
        let prev_next = if tail.is_null() { &self.head } else { &(*tail).next };
        // Set previous tail's next, unparking the thread removing the node if needed
        #[cfg(not(target_arch = "x86_64"))]
        prev_next.store(node, SeqCst);
        #[cfg(not(target_arch = "x86_64"))]
        if self.parked_node.load(SeqCst) == prev_next.as_ptr() {
            self.parker.unpark();
        }
        #[cfg(target_arch = "x86_64")]
        if unsafe { !(prev_next.as_ref().swap(node.as_ptr().cast(), Release)).is_null() } {
            self.parker.unpark();
        }
    }
}
```

## Locked queue

Once a node is enqueued, i.e. the tail pointer has been updated, every access to it (including removal) must be serialized through the queue mutex. This is done safely through a `LockedQueue` guard returned by `Queue::lock`.

## Parking algorithm

Node insertion is not atomic, queue's tail is first updated, then the next pointer of the previous node/tail is set. It means that the previous node must remain valid (not be dropped) until the insertion ends. As a consequence, when a node is removed, if it is not the tail, it must wait for its next pointer to be set. It is enforced using queue's parker with a platform-dependent algorithm:

```rust
impl<T, S: QueueState, SP: SyncPrimitives> LockedQueue<T, S, SP> {
    // This function must be called with queue's mutex acquired.
    // It must not be called if the node is at the queue's tail.
    fn get_next(&self, next: &AtomicPtr<NodeLink>) -> NonNull<NodeLink> {
        // Spin a bit before parking in case next is already set.
        for _ in 0..1 + S::SPIN_BEFORE_PARK {
            if let Some(next) = NonNull::new(next.load(Acquire)) {
                return next;
            }
            hint::spin_loop();
        }
        #[cfg(not(target_arch = "x86_64"))]
        (self.parked_node).store(ptr::from_ref(next).cast_mut(), SeqCst);
        #[cfg(target_arch = "x86_64")]
        const PARKED: *mut NodeLink = ptr::without_provenance_mut(1);
        #[cfg(target_arch = "x86_64")]
        if let Err(next) = next.compare_exchange(ptr::null_mut(), PARKED, Relaxed, Acquire) {
            return unsafe { NonNull::new_unchecked(next) };
        }
        loop {
            #[cfg(not(target_arch = "x86_64"))]
            if let Some(next) = NonNull::new(next.load(SeqCst)) {
                self.parked_node.store(ptr::null_mut(), SeqCst);
                return next;
            }
            unsafe { self.parker.park() };
            #[cfg(target_arch = "x86_64")]
            let next = next.load(Acquire);
            #[cfg(target_arch = "x86_64")]
            if next != PARKED {
                return unsafe { NonNull::new_unchecked(next) };
            }
        }
    }
}
```

Notice `get_next` is a method of `LockedQueue`, which means queue's lock must be acquired, so there is no data race in queue's parker use.

### x86_64

On removal side, next pointer updated with a CAS to a special value meaning the thread is parked, while insertion side write next pointer with CAS too. If removal CAS succeeds, the thread is parked. On the other side, if insertion CAS fails, it means the removal thread has been parked and must be unparked.

### Other platforms

On other platforms, `SeqCst` stores are less expensive than a CAS[^1]. The algorithm is optimized for it and becomes:
- insertion: seqcst store of previous node's next pointer + seqcst load of queue's `parked_next` → unpark if `parked_next` is previous node's next pointer
- removal: seqcst store of queue's `parked_next` + seqcst load of node's next pointer → park if next pointer is not set

This is a variation of the classical "store X; load Y || store Y; load X". It leverages [parker's token system](https://doc.rust-lang.org/std/thread/fn.park.html#park-and-unpark), so the order of park vs. unpark doesn't matter. 

## Node removal

Removing a node from the list is done with the following simplified code:
```rust
use std::ptr;

impl<T, S: QueueState, SP: SyncPrimitives> LockedQueue<T, S, SP> {
    // mutex must be acquired
    unsafe fn remove(&self, node: *mut NodeLink, wait_enqueued: bool) {
        let prev = (*node).prev.load(Relaxed);
        let is_head = prev == HEAD_MARKER;
        let prev_next_ptr = if is_head { &self.head } else { &(*prev).next };
        // In some case, node is removed concurrently to its enqueuing,
        // so it is needed to wait for the enqueuing to end.
        if wait_enqueued {
            let prev_next = self.get_next(prev_next);
            debug_assert_eq!(prev_next.as_ptr(), node);
        }
        let mut next = node.next();
        // If next pointer is not set, node is assumed to be the tail;
        // reset the next pointer of the previous node/queue's head and set 
        // the tail to the previous node/null.
        // Next pointer must be set before the tail is updated, so there will be no
        // race with the next inserted node.
        if next.is_none() {
            prev_next.store(ptr::null_mut(), Relaxed);
            let new_tail = if is_head { ptr::null_mut() } else { prev };
            if ((self.tail).compare_exchange(node, new_tail, SeqCst, Relaxed)).is_err() {
                // If tail update fails, it means that another node has been enqueued since
                // (as the lock is acquired, so no other operation can be done on the queue).
                // Removal must then wait for the enqueuing to be completed.
                next = Some(self.get_next(&node.next));
            }
        }
        // Unlink the node if it has a next pointer
        if let Some(next) = next {
            unsafe { next.as_ref().prev.store(prev, Relaxed) };
            prev_next.store(next.as_ptr(), Relaxed);
        }
        // Set the prev pointer to the special dequeued state value.
        // Use `Release` ordering so synchronize with future access of the state.
        node.prev.store(RawNodeState::Dequeued.into_ptr(), Release);
        let unlink = |next: NonNull<NodeLink>| {
            next.as_ref().prev.store(prev, Relaxed);
            prev_next_ptr.store(next.as_ptr(), Relaxed);
            // Set the prev pointer to the special dequeued state value.
            // Use `Release` ordering so synchronize with future access of the state.
            (*node).prev.store(ptr::without_provenance_mut(1), Release);
        };
        // If node's next is set, then it can safely be unlinked.
        if let Some(next) = NonNull::new((*node).next.load(SeqCst)) {
            return unlink(next);
        }
        // Otherwise, assuming the node is the tail, reset the next pointer of
        // the previous node/queue's head and set the tail to the previous node/null.
        // Next pointer must be set before the tail is updated, so there will be no
        // race with the next inserted node.
        prev_next_ptr.store(ptr::null_mut(), Relaxed);
        let new_tail = prev.map_addr(|addr| addr & !2);
        match self.tail.compare_exchange(node, new_tail, SeqCst, Relaxed) {
            // The tail has been successfully updated, previous node's next/queue's head
            // has already been reset, so just update the node state to dequeued
            Ok(_) => (*node).prev.store(ptr::without_provenance_mut(1), Release),
            // Another node has been enqueued since, so wait for the next pointer to be set
            // and unlink the node.
            Err(_) => unlink(self.get_next(&(*node).next)),
        }
    }
}
```

There are three ways to remove a node from the queue

#### Node removing itself

This is especially done in node's [destructor](#node-data-and-aliasing). The node has already been enqueued.

#### Dequeuing a node (FIFO)

```rust
impl<'a, T, S: QueueState, SP: SyncPrimitives> LockedQueue<'a, T, S, SP> {
    pub fn dequeue(&mut self) -> Option<Dequeue<'a, '_, T, S, SP>> {
        // Check the queue is not empty, relying on `SeqCst` ordering as in insertion
        NonNull::new(self.queue.tail.load(SeqCst))?;
        // Wait for the head to be written so the node insertion is complete
        let node = self.get_next(&self.queue.head);
        Some(Dequeue { node, locked: self })
    }
}
```

Node is removed in `Dequeue` destructor — not running just make the next `dequeue` call to return the same node.

#### Popping a node (LIFO)

```rust
impl<'a, T, S: QueueState, SP: SyncPrimitives> LockedQueue<'a, T, S, SP> {
    pub fn pop(&mut self) -> Option<Pop<'a, '_, T, S, SP>> {
        // Just load the tail
        let node = NonNull::new(self.queue.tail.load(SeqCst))?;
        Some(Pop { node, locked: self })
    }
}
```

Node is removed in `Pop` destructor, but with `wait_enqueued=true`.

## Queue draining

Another way to remove nodes from the queue is to drain it, i.e. remove all nodes at once. This is done using the circular temporary list [trick](README.md#acknowledgements) used in tokio:
```rust
pub struct Drain<'a, T, S: QueueState = (), SP: SyncPrimitives + 'a = DefaultSyncPrimitives> {
    // Sentinel node used as the base of circular chaining
    sentinel_node: NodeLink,
    queue: &'a Queue<T, S, SP>,
    locked: Option<LockedQueue<'a, T, S, SP>>,
}

impl<'a, T, S: QueueState, SP: SyncPrimitives> Drain<'a, T, S, SP> {
    fn new(locked: LockedQueue<'a, T, S, SP>) -> Self {
        let mut head = ptr::null_mut();
        let mut tail = ptr::null_mut();
        // Check if the queue is not empty.
        if !locked.queue.tail.load(SeqCst).is_null() {
            // In this case, wait for the head (the queue is locked 
            // so the head cannot be removed in the meantime)
            head = locked.get_next(&locked.queue.head).as_ptr();
            // Reset the head
            locked.head.store(ptr::null_mut(), Relaxed);
            // Swap the head
            let tail = locked.tail.swap(ptr::null_mut(), SeqCst);
        }
        Self {
            sentinel_node: NodeLink {
                prev: AtomicPtr::new(tail),
                next: AtomicPtr::new(head),
            },
            queue: locked.queue,
            locked: Some(locked),
        }
    }
    
    /// Every operation on `Drain` requires to have it pinned.
    pub fn next(self: Pin<&mut Self>) -> Option<NodeDrained<'a, '_, T, S, SP>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.locked.as_ref().unwrap();
        NonNull::new(this.sentinel_node.next.load(Relaxed)).map(|node| NodeDrained { node, drain: this })
    }
}
```

Circular chaining is not done at `Drain` creation. Indeed, it would require the sentinel node to be pinned, but it cannot be the case in a function that returns it. The additional trick is to do the chaining only if the lock must be temporarily released. There is a `fn execute_unlocked<F: FnOnce() -> R, R>(self: Pin<&mut Self>, f: F) -> R` that does exactly that. When the lock is released, nodes can be removed, and because the list is circular, they will never interfere with queue's head of tail. 

## Node data and aliasing

Concurrent intrusive queues are breaking[^2] the Rust aliasing model, as a thread can have a mutable reference on its node while another may dequeue the node and access its data.

[RFC 3467](https://rust-lang.github.io/rfcs/3467-unsafe-pinned.html) introduces a new `UnsafePinned` wrapper for this purpose, with a polyfill on stable toolchain. The full node definition is then:

```rust
pub struct Node<Q: QueueRef> {
    queue: Q,
    node: UnsafePinned<NodeInner<Q::NodeData>>,
}

#[repr(C)]
struct NodeInner<T> {
    pub(crate) link: NodeLink,
    pub(crate) data: T,
}
```

`repr(C)` allows casting `NodeLink` pointer manipulated in the algorithm to `NodeInner` pointer and access node's data. 
Node's data is not considered thread-safe, so its accesses must be serialized; this is done through queue's mutex while the node is in queued state.

`Node` struct also embed a queue reference to enforce the removal of the node from the queue before dropping it. Because the node must be pinned to be enqueued, its removal is [guaranteed](https://doc.rust-lang.org/std/pin/index.html#drop-guarantee), so the queue cannot contain dangling nodes.

## Node state and access

A node has three states stored in its `prev` pointer: unqueued, queued and dequeued. This is materialized in the following API:

```rust
impl<Q: QueueRef> Node<Q> {
    pub fn state(self: Pin<&mut Self>) -> NodeState<'_, Q> { /* ... */ }
}

pub enum NodeState<'a, Q: QueueRef> {
    Unqueued(NodeUnqueued<'a, Q>),
    Queued(NodeQueued<'a, Q>),
    Dequeued(NodeDequeued<'a, Q>),
}
``` 
Each state wrapper has proper methods: `enqueue` for `NodeUnqueued`, `dequeue` for `NodeQueued`, etc. All of them has data accessor, but as accessing the data of a queued node required to have the queue's mutex acquired, a mutex guard is embedded in `NodeQueued`.

Regarding `Dequeue`/`Pop`/`NodeDrained` types seen earlier, because they all borrow queue's mutex guard, they also give safely access to node data.

## Queue state

A big advantage to `aiq` regarding mutex-protected queue is that the atomic tail can be used to store arbitrary integer data when the queue is empty (thanks to pointer tagging technique). The `S: QueueState` parameter can accept two values: `()`, i.e. no state, and `usize`. With the latter a whole class of algorithms becomes possible, the most immediate being a semaphore.

In fact, semaphore counter goes to zero when waiters starts to enqueue, so queue state can be used to store the available permits or the waiter nodes. When there are no enqueued waiters, semaphore operation are reduced to simple CAS loops on queue's tail pointer with `Queue::fetch_update_state<F: FnMut(S) -> Option<S>>(&self, mut f: F) -> Result<S, Option<S>>`. Removal methods also has a twin method to set a new state when removing the last node.

## `loom` support

[`loom`](http://crates.io/crates/loom) support is enabled through `#[cfg(loom)]`.

`loom`'s biggest limitation is the non-support of `SeqCst` ordering. However, `SeqCst` is used both for the tail pointer manipulation and for the parking algorithm. Supporting `loom` thus requires some adaptations:
 - the parking algorithm is always x86_64's one
 - `SeqCst` loads of the tail pointer are replaced with a CAS in order to rely on the modification total order instead of the `SeqCst` total order.

Moreover, `loom` doesn't support `UnsafePinned` and pointer based workflows, it requires using `loom::sync::UnsafeCell` to check accesses correctness. So `NodeInner` becomes:
```rust
#[repr(C)]
pub(crate) struct NodeInner<T> {
    pub(crate) link: NodeLink,
    #[cfg(not(loom))]
    pub(crate) data: T,
    #[cfg(loom)]
    pub(crate) data: loom::cell::UnsafeCell<T>,
}
```
As a result, node's data accesses by reference are disabled and methods `with_data`/`with_data_mut` must be used. These methods are also available without `#[cfg(loom)]` (but hidden), making possible to write loom compatible code directly. This is for example used in `aiq` examples.

[^1]: On x86_64, `SeqCst` atomic stores are compiled into `xchg`, the same assembly instruction as atomic swap.
[^2]: there is literally a temporary hack in the compiler to handle it.