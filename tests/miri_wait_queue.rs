use std::{sync::Arc, thread};

use aiq::wait_queue::WaitQueue;
use tokio_test::{assert_pending, assert_ready, task};

#[test]
fn notify_one() {
    let rx = Arc::new(<WaitQueue>::new());
    let mut wait = task::spawn(rx.wait());
    assert_pending!(wait.poll());

    let tx = rx.clone();
    let th = thread::spawn(move || tx.notify_one());
    let _ = wait.poll();
    th.join().unwrap();
    assert_ready!(wait.poll());
}

#[test]
fn notify_all() {
    let rx = Arc::new(<WaitQueue>::new());
    let mut wait = task::spawn(rx.wait());
    assert_pending!(wait.poll());

    let tx = rx.clone();
    let th = thread::spawn(move || tx.notify_all());
    let _ = wait.poll();
    th.join().unwrap();
    assert_ready!(wait.poll());
}
