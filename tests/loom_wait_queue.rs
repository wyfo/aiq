#![cfg(loom)]

use std::sync::Arc;

use aiq::wait_queue::WaitQueue;
use loom::thread;
use tokio_test::{assert_pending, assert_ready, task};

#[test]
fn notify_one() {
    loom::model(|| {
        let rx = Arc::new(<WaitQueue>::new());
        let mut wait = task::spawn(rx.wait());
        assert_pending!(wait.poll());

        let tx = rx.clone();
        let th = thread::spawn(move || tx.notify_one());
        let _ = wait.poll();
        th.join().unwrap();
        assert_ready!(wait.poll());
    });
}

#[test]
fn notify_all() {
    loom::model(|| {
        let rx = Arc::new(<WaitQueue>::new());
        let mut wait = task::spawn(rx.wait());
        assert_pending!(wait.poll());

        let tx = rx.clone();
        let th = thread::spawn(move || tx.notify_all());
        let _ = wait.poll();
        th.join().unwrap();
        assert_ready!(wait.poll());
    });
}
