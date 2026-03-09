# aiq — Atomic Intrusive Queue

A concurrent intrusive queue for building async primitives.

See [here](algorithm.md) for a detailed explanation of the algorithm.

## Features

- 100%[^1] safe API
- `#[no_std]`, with optional `alloc`/`std` features
- Lockless enqueuing: multiple nodes can be enqueued concurrently while another is being dequeued; dequeuing requires locking
- Generic `Mutex`/`Parker` traits, with default implementations for std, pthread, spin, and `atomic-wait`/`parking_lot`
- Optional atomic state embedded in the queue when empty

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

Without any features enabled, the library falls back to spin-based mutex and parker.

## Performance

Benchmark results for `tokio` benchmarks, run with both tokio native primitives and their `aiq` counterparts from [examples](examples) on an Apple M3:

*benchmarks prefixed by `contention`/`uncontented` measure `Semaphore` performance*

| Benchmark | aiq | tokio | aiq speedup |
|-----------|----:|------:|------------:|
| `notify_one/10` | 160.12 µs | 104.80 µs | 0.65 |
| `notify_one/50` | 90.61 µs | 132.19 µs | 1.46 |
| `notify_one/100` | 83.83 µs | 136.12 µs | 1.62 |
| `notify_one/200` | 84.63 µs | 128.59 µs | 1.52 |
| `notify_one/500` | 84.88 µs | 121.12 µs | 1.43 |
| | | | |
| `notify_waiters/10` | 243.29 µs | 259.03 µs | 1.06 |
| `notify_waiters/50` | 114.68 µs | 169.27 µs | 1.48 |
| `notify_waiters/100` | 95.52 µs | 154.03 µs | 1.61 |
| `notify_waiters/200` | 93.07 µs | 140.76 µs | 1.51 |
| `notify_waiters/500` | 106.41 µs | 153.51 µs | 1.44 |
| | | | |
| `contention/concurrent_multi` | 6.95 µs | 6.94 µs | 1.00 |
| `contention/concurrent_single` | 129.82 ns | 158.25 ns | 1.22 |
| | | | |
| `uncontented/concurrent_multi` | 6.97 µs | 7.00 µs | 1.00 |
| `uncontented/concurrent_single` | 130.40 ns | 157.94 ns | 1.21 |
| `uncontented/multi` | 52.47 ns | 92.17 ns | 1.76 |
| | | | |

`aiq`-based reimplementations seem to give a consistent speedup compared to tokio native ones.

Only `notify_one/10` gives a worse (and quite random) result, but it seems to be a side effect of the benchmark implementation itself. In fact, because `aiq` enqueuing operation is more parallelizable than tokio's mutex-protected one, waiter tasks have been measured to be 2x more often blocked on a pending future, resulting in the tokio worker thread being parked (because there are only 1-2 tasks per thread with only 10 waiter tasks).

## Testing

Reimplementations of `tokio::sync::Notify` and `tokio::sync::Semaphore` are tested on the full tokio test suite with both [`miri`](https://github.com/rust-lang/miri/) and [`loom`](https://github.com/tokio-rs/loom).

## Acknowledgements

`aiq::queue::Drain` algorithm reuses the idea originally introduced to tokio by [Tymoteusz Wiśniewski](https://github.com/satakuma) in [tokio-rs/tokio#5458](https://github.com/tokio-rs/tokio/pull/5458): make the draining atomic by moving the list nodes into a temporary circular list.

A small improvement, motivated by API ergonomics, has been made: the circular chaining is deferred until the queue lock actually needs to be released mid-drain.

[^1]: `QueueRef` trait is actually unsafe to implement, but it comes with `queue_ref!` macro to do it without unsafe code.
