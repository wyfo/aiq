# Algorithm

`aiq` stands for Atomic Intrusive Queue. It is an intrusive doubly-linked list of pinned nodes, which supports concurrent tail-insertion and serialized removal.

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
    parked_next: AtomicPtr<AtomicPtr<NodeLink>>,
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

where `NodeLink` is the [linking part](https://www.youtube.com/watch?v=eVTXPUF4Oz4) of a bigger `repr(C)` node struct covered in a later [section](#node-data-and-aliasing). `T` parameter of `Queue` represents arbitrary data carried by the node; it has no importance for the algorithm itself, but matters when the queue is used.

`Tail<S>` is just a marker type to indicate the tail pointer may contain a [queue state](#queue-state) when there is no node. `*mut Tail<S>` can be considered as a `union { state: S, ptr: NonNull<NodeLink> }`.

`NodeLink::prev` also encodes the node's state via special values when it is not actively pointing to a predecessor:
- 0: the node is unqueued (never inserted)
- 1: the node is dequeued (was inserted and has since been removed)
- 2: the node is queued and is the queue's head
- address of a previous node: the node is queued

Because the queue stores raw pointers to nodes, those pointers must remain valid as long as the nodes are queued. This is achieved by requiring nodes to be pinned before insertion. It also means that nodes must be removed from the queue before being dropped, which is done in the node's [destructor](#node-data-and-aliasing).

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
            match self.tail.compare_exchange_weak(tail, node, SeqCst, Relaxed) {
                Ok(_) => break,
                Err(t) => tail = t,
            }
        }
        let prev_next = if tail.is_null() { &self.head } else { &(*tail).next };
        // Set previous tail's next, unparking the thread removing the node if needed
        #[cfg(not(target_arch = "x86_64"))]
        prev_next.store(node, SeqCst);
        #[cfg(not(target_arch = "x86_64"))]
        if self.parked_next.load(SeqCst) == prev_next.as_ptr() {
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

Once a node is enqueued (i.e. the tail pointer has been updated), every subsequent operation on that node (including removal) must be serialized through the queue mutex. This is done safely through a `LockedQueue` guard returned by `Queue::lock`.

## Parking algorithm

Node insertion is two-phased: the queue's tail pointer is first updated atomically (phase 1), then the predecessor node's `next` (or the queue's head) pointer is written (phase 2). This means the predecessor node must remain valid until phase 2 completes. As a consequence, removing a non-tail node requires waiting for phase 2 to finish, i.e. waiting for the node's `next` pointer to be written. This is enforced using the queue's parker with a platform-dependent algorithm:

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
        self.parked_next.store(ptr::from_ref(next).cast_mut(), SeqCst);
        #[cfg(target_arch = "x86_64")]
        const PARKED: *mut NodeLink = ptr::without_provenance_mut(1);
        #[cfg(target_arch = "x86_64")]
        if let Err(next) = next.compare_exchange(ptr::null_mut(), PARKED, Relaxed, Acquire) {
            return unsafe { NonNull::new_unchecked(next) };
        }
        loop {
            #[cfg(not(target_arch = "x86_64"))]
            if let Some(next) = NonNull::new(next.load(SeqCst)) {
                self.parked_next.store(ptr::null_mut(), SeqCst);
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

Notice `get_next` is a method of `LockedQueue`, meaning the queue's lock must be held. This guarantees that only one thread at a time can be waiting in `park`, so there is no data race on the queue's parker.

### x86_64

On the removal side, the next pointer is replaced with a `PARKED` sentinel via a CAS. If the CAS succeeds, the removal thread parks. On the insertion side, the next pointer is overwritten via an atomic swap. If the swap returns the `PARKED` sentinel — meaning the removal thread had set it — the insertion thread calls `unpark`.

### Other platforms

On other platforms, `SeqCst` stores are less expensive than a CAS[^1]. The algorithm is optimized for this and becomes:
- insertion: `SeqCst` store to the predecessor's `next` pointer, then `SeqCst` load of `parked_next` → unpark if `parked_next` equals the predecessor's `next` pointer
- removal: `SeqCst` store to `parked_next`, then `SeqCst` load of the node's `next` pointer → park if not yet set

This is a variation of the classical "store X; load Y || store Y; load X" pattern, which guarantees that at least one side sees the other's write. It relies on the `Parker` implementation delivering an `unpark` that was issued before `park` as an immediate return, so the order of park vs. unpark does not matter.

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
        // reset prev_next (the predecessor's next pointer, or queue's head) and
        // update the tail to point to the predecessor (or null).
        // prev_next must be cleared before the tail CAS: otherwise a concurrent
        // enqueuer whose phase 1 CAS succeeds between the tail CAS and the clear
        // could have its phase 2 write overwritten.
        if next.is_none() {
            prev_next.store(ptr::null_mut(), Relaxed);
            let new_tail = if is_head { ptr::null_mut() } else { prev };
            if ((self.tail).compare_exchange(node, new_tail, SeqCst, Relaxed)).is_err() {
                // The tail CAS failed, meaning a concurrent insertion (phase 1) appended
                // a new node after this one since we read next. Wait for that insertion's
                // phase 2 to write the next pointer.
                next = Some(self.get_next(&node.next));
            }
        }
        // Unlink the node if it has a next pointer
        if let Some(next) = next {
            unsafe { next.as_ref().prev.store(prev, Relaxed) };
            prev_next.store(next.as_ptr(), Relaxed);
        }
        // Set the prev pointer to the special dequeued state value.
        // Use `Release` ordering so synchronize with future accesses of the state.
        node.prev.store(RawNodeState::Dequeued.into_ptr(), Release);
    }
}
```

There are three ways to remove a node from the queue:

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

The node is removed in the `Dequeue` destructor. Forgetting the `Dequeue` value (e.g. via `Dequeue::requeue()`) skips removal, causing the next `dequeue` call to return the same node.

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

The node is removed in the `Pop` destructor with `wait_enqueued=true`, because the tail node may still be mid-phase-2 of its own insertion when `pop` is called.

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
            // Swap the tail to reset it and keep the current enqueued nodes
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
    
    /// Every operation on `Drain` requires it to be pinned.
    pub fn next(self: Pin<&mut Self>) -> Option<NodeDrained<'a, '_, T, S, SP>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.locked.as_ref().unwrap();
        NonNull::new(this.sentinel_node.next.load(Relaxed)).map(|node| NodeDrained { node, drain: this })
    }
}
```

Circular chaining is not done at `Drain` creation, because the sentinel node cannot be pinned before being returned from the constructor. Instead, it is deferred to `fn execute_unlocked<F: FnOnce() -> R, R>(self: Pin<&mut Self>, f: F) -> R`, which is called when the lock must be temporarily released. When the lock is released, nodes may remove themselves; because the remaining chain is circular through the sentinel, such self-removals never interfere with the queue's head or tail.

## Node data and aliasing

Concurrent intrusive queues break[^2] the Rust aliasing model, as a thread can hold a mutable reference to its node while another thread dequeues the node and accesses its data.

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

`repr(C)` allows casting the `NodeLink` pointer used in the algorithm to a `NodeInner` pointer to access the node's data.
The node's data is not considered thread-safe, so accesses to it must be serialized; this is done through the queue's mutex while the node is in the queued state.

The `Node` struct also embeds a queue reference to enforce node removal before the `Node` is dropped. Because the node must be pinned to be enqueued, Rust's [drop guarantee](https://doc.rust-lang.org/std/pin/index.html#drop-guarantee) ensures the destructor runs before the memory is freed, so the queue cannot contain dangling nodes.

## Node state and access

A node has three states stored in its `prev` pointer: unqueued, queued, and dequeued. This is exposed through the following API:

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
Each state wrapper has appropriate methods: `enqueue` for `NodeUnqueued`, `dequeue` for `NodeQueued`, etc. All of them have a data accessor, but since accessing the data of a queued node requires the queue's mutex to be held, a mutex guard is embedded in `NodeQueued`.

Regarding the `Dequeue`/`Pop`/`NodeDrained` types seen earlier, because they all borrow the queue's mutex guard, they also safely provide access to node data.

## Queue state

A key advantage of `aiq` over mutex-protected queues is that the atomic tail pointer can store arbitrary integer data when the queue is empty, using pointer tagging. The `S: QueueState` parameter can take two values: `()` (no state) and `usize`. With the latter, a whole class of algorithms becomes possible, the most immediate being a semaphore.

In a semaphore, the counter reaches zero when waiters start to enqueue, so the queue state can store the number of available permits while the queue stores the waiting nodes. When there are no enqueued waiters, semaphore operations reduce to simple CAS loops on the queue's tail pointer via `Queue::fetch_update_state<F: FnMut(S) -> Option<S>>(&self, mut f: F) -> Result<S, Option<S>>`. Removal methods also have a counterpart that sets a new queue state when removing the last node.

## `loom` support

[`loom`](http://crates.io/crates/loom) support is enabled through `#[cfg(loom)]`.

`loom`'s biggest limitation is its lack of support for `SeqCst` ordering. However, `SeqCst` is used both for tail pointer manipulation and for the parking algorithm. Supporting `loom` therefore requires some adaptations:
 - the x86_64 parking algorithm is always used (it relies on modification order rather than `SeqCst` cross-object ordering)
 - `SeqCst` loads of the tail pointer are replaced with a CAS, so correctness relies on the modification order of the tail atomic rather than the `SeqCst` total order.

Moreover, `loom` doesn't support `UnsafePinned` or pointer-based workflows, requiring the use of `loom::sync::UnsafeCell` to check access correctness. So `NodeInner` becomes:
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
As a result, node data accesses by reference are disabled and methods `with_data`/`with_data_mut` must be used. These methods are also available without `#[cfg(loom)]` (but hidden), making it possible to write loom-compatible code directly. This is for example used in `aiq` examples.

[^1]: On x86_64, `SeqCst` atomic stores are compiled into `xchg`, the same assembly instruction as atomic swap.
[^2]: there is literally a temporary hack in the compiler to handle it.