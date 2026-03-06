# aiq — Atomic Intrusive Queue

A concurrent intrusive queue for building async primitives.

## Features

- 100%[^1] safe API
- `#[no_std]`, with optional `alloc`/`std` features
- Lockless enqueuing: multiple nodes can be enqueued concurrently while another is being dequeued; dequeuing requires locking
- Generic `Mutex`/`Parker` traits, with default implementations for std, pthread, spin, and `atomic-wait`/`parking_lot`
- Optional atomic state embedded in the queue when empty (`queue-state` feature)

## Usage

```rust
use aiq::{Node, NodeState, Queue};
use std::pin::pin;

let queue: Queue<usize> = Queue::new();

// Nodes carry user data and an intrusive link into the queue
let mut node = pin!(Node::with_data(&queue, 42));

// Enqueue by matching on node state
match node.state() {
    NodeState::Unqueued(node) => node.enqueue(),
    _ => unreachable!(),
}

// Dequeue requires locking
let mut locked = queue.is_empty_or_lock().unwrap();
let item = locked.dequeue().unwrap();
assert_eq!(*item, 42);
```

See [examples](examples) for full implementations of `tokio::sync::Notify` and `tokio::sync::Semaphore` built with `aiq`, with fully identical API and behavior.

## Cargo Features

| Feature | Description |
|---------|-------------|
| `std` *(default)* | `std::sync`-based mutex and condvar parker; implies `alloc` |
| `alloc` | Enables `Arc<Queue<T, S>>` as a `QueueRef` |
| `atomic-wait` | Futex-based parker via the `atomic-wait` crate |
| `lock_api` | `lock_api::RawMutex` trait implementation |
| `parking_lot` | `parking_lot` mutex; implies `lock_api` |
| `pthread` | Raw pthread mutex and condition variable; implies `alloc` |
| `queue-state` | Provides a `usize` state with CAS updates when the queue is empty |

Without any features enabled, the library falls back to spin-based mutex and parker.

## Performance

Benchmark results for `tokio` benchmarks, run with both tokio native primitives and their `aiq` counterparts from [examples](examples) on an Apple M3:

*benchmarks prefixed by `contention`/`uncontented` measure `Semaphore` performance*

| Benchmark | aiq | tokio | aiq speedup |
|-----------|----:|------:|------------:|
| `notify_one/10` | 116.42 µs | 109.91 µs | 0.94 |
| `notify_one/50` | 90.67 µs | 134.49 µs | 1.48 |
| `notify_one/100` | 87.32 µs | 134.30 µs | 1.54 |
| `notify_one/200` | 82.55 µs | 123.66 µs | 1.50 |
| `notify_one/500` | 86.43 µs | 113.21 µs | 1.31 |
| | | | |
| `notify_waiters/10` | 257.66 µs | 397.64 µs | 1.54 |
| `notify_waiters/50` | 121.76 µs | 232.06 µs | 1.91 |
| `notify_waiters/100` | 98.14 µs | 157.64 µs | 1.61 |
| `notify_waiters/200` | 94.02 µs | 152.86 µs | 1.63 |
| `notify_waiters/500` | 103.53 µs | 157.85 µs | 1.52 |
| | | | |
| `contention/concurrent_multi` | 6.87 µs | 6.91 µs | 1.01 |
| `contention/concurrent_single` | 136.16 ns | 163.61 ns | 1.20 |
| | | | |
| `uncontented/concurrent_multi` | 6.92 µs | 6.97 µs | 1.01 |
| `uncontented/concurrent_single` | 134.84 ns | 163.25 ns | 1.21 |
| `uncontented/multi` | 52.72 ns | 92.52 ns | 1.75 |
| | | | |

`aiq`-based reimplementations seem to give a consistent speedup compared to tokio native ones.

Only `notify_one/10` gives a worse result, but it seems to be a side effect of the benchmark implementation itself. In fact, because `aiq` enqueuing operation is more parallelizable than tokio's mutex-protected one, waiter tasks have been measured to be 2x more often blocked on a pending future, resulting in the tokio worker thread being parked (because there are only 1-2 tasks per thread with only 10 waiter tasks).

## Acknowledgements

`aiq::queue::Drain` algorithm reuses the idea originally introduced to tokio by [Tymoteusz Wiśniewski](https://github.com/satakuma) in [tokio-rs/tokio#5458](https://github.com/tokio-rs/tokio/pull/5458): make the draining atomic by moving the list nodes into a temporary circular list.

A small improvement, motivated by API ergonomics, has been made: the circular chaining is deferred until the queue lock actually needs to be released mid-drain.

[^1]: `QueueRef` trait is actually unsafe to implement, but it comes with `queue_ref!` macro to do it without unsafe code.
