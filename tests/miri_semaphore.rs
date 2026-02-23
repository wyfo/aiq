#[allow(dead_code)]
#[path = "../examples/semaphore.rs"]
mod semaphore;

use std::{
    future::poll_fn,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    task::Poll,
    thread,
};

use futures::executor::block_on;
use semaphore::Semaphore;

#[test]
fn basic_usage() {
    const NUM: usize = 2;

    struct Shared {
        semaphore: Semaphore,
        active: AtomicUsize,
    }

    async fn actor(shared: Arc<Shared>) {
        let _permit = shared.semaphore.acquire().await.unwrap();
        let actual = shared.active.fetch_add(1, SeqCst);
        assert!(actual < NUM);

        let actual = shared.active.fetch_sub(1, SeqCst);
        assert!(actual <= NUM);
    }

    let shared = Arc::new(Shared {
        semaphore: Semaphore::new(NUM),
        active: AtomicUsize::new(0),
    });

    for _ in 0..NUM {
        let shared = shared.clone();

        thread::spawn(move || {
            block_on(actor(shared));
        });
    }

    block_on(actor(shared));
}

#[test]
fn release() {
    let semaphore = Arc::new(Semaphore::new(1));

    {
        let semaphore = semaphore.clone();
        thread::spawn(move || {
            block_on(semaphore.acquire()).unwrap();
        });
    }

    block_on(semaphore.acquire()).unwrap();
}

#[test]
fn basic_closing() {
    const NUM: usize = 2;

    let semaphore = Arc::new(Semaphore::new(1));

    for _ in 0..NUM {
        let semaphore = semaphore.clone();

        thread::spawn(move || {
            for _ in 0..2 {
                block_on(semaphore.acquire()).map_err(|_| ())?;
            }

            Ok::<(), ()>(())
        });
    }

    semaphore.close();
}

#[test]
fn concurrent_close() {
    const NUM: usize = 3;

    let semaphore = Arc::new(Semaphore::new(1));

    for _ in 0..NUM {
        let semaphore = semaphore.clone();

        thread::spawn(move || {
            block_on(semaphore.acquire()).map_err(|_| ())?;
            semaphore.close();

            Ok::<(), ()>(())
        });
    }
}

#[test]
fn concurrent_cancel() {
    async fn poll_and_cancel(semaphore: Arc<Semaphore>) {
        let mut acquire1 = Some(semaphore.acquire());
        let mut acquire2 = Some(semaphore.acquire());
        poll_fn(|cx| {
            // poll the acquire future once, and then immediately throw
            // it away. this simulates a situation where a future is
            // polled and then cancelled, such as by a timeout.
            if let Some(acquire) = acquire1.take() {
                tokio::pin!(acquire);
                let _ = acquire.poll(cx);
            }
            if let Some(acquire) = acquire2.take() {
                tokio::pin!(acquire);
                let _ = acquire.poll(cx);
            }
            Poll::Ready(())
        })
        .await
    }

    let semaphore = Arc::new(Semaphore::new(0));
    let t1 = {
        let semaphore = semaphore.clone();
        thread::spawn(move || block_on(poll_and_cancel(semaphore)))
    };
    let t2 = {
        let semaphore = semaphore.clone();
        thread::spawn(move || block_on(poll_and_cancel(semaphore)))
    };
    let t3 = {
        let semaphore = semaphore.clone();
        thread::spawn(move || block_on(poll_and_cancel(semaphore)))
    };

    t1.join().unwrap();
    semaphore.add_permits(10);
    t2.join().unwrap();
    t3.join().unwrap();
}

#[test]
fn batch() {
    let semaphore = Arc::new(Semaphore::new(10));
    let active = Arc::new(AtomicUsize::new(0));
    let mut threads = vec![];

    for _ in 0..2 {
        let semaphore = semaphore.clone();
        let active = active.clone();

        threads.push(thread::spawn(move || {
            for n in &[4, 10, 8] {
                let _permits = block_on(semaphore.acquire_many(*n)).unwrap();

                active.fetch_add(*n as usize, SeqCst);

                let num_active = active.load(SeqCst);
                assert!(num_active <= 10);

                thread::yield_now();

                active.fetch_sub(*n as usize, SeqCst);
            }
        }));
    }

    for thread in threads.into_iter() {
        thread.join().unwrap();
    }

    assert_eq!(10, semaphore.available_permits());
}

#[test]
fn release_during_acquire() {
    let semaphore = Arc::new(Semaphore::new(10));
    let permits1 = semaphore
        .try_acquire_many(8)
        .expect("try_acquire should succeed; semaphore uncontended");
    let semaphore2 = semaphore.clone();
    let thread = thread::spawn(move || block_on(semaphore2.acquire_many(4)).unwrap().forget());

    drop(permits1);
    thread.join().unwrap();
    semaphore.add_permits(4);
    assert_eq!(10, semaphore.available_permits());
}

#[test]
fn concurrent_permit_updates() {
    let semaphore = Arc::new(Semaphore::new(5));
    let t1 = {
        let semaphore = semaphore.clone();
        thread::spawn(move || semaphore.add_permits(3))
    };
    let t2 = {
        let semaphore = semaphore.clone();
        thread::spawn(move || {
            semaphore
                .try_acquire()
                .expect("try_acquire should succeed")
                .forget()
        })
    };
    let t3 = {
        let semaphore = semaphore.clone();
        thread::spawn(move || semaphore.forget_permits(2))
    };

    t1.join().unwrap();
    t2.join().unwrap();
    t3.join().unwrap();
    assert_eq!(semaphore.available_permits(), 5);
}
