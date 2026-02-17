use core::{marker::PhantomData, mem::ManuallyDrop, pin::Pin, ptr::NonNull};

use crate::{
    queue::{MutexGuard, NodeLink, Queue},
    sync::{mutex::Mutex, SyncPrimitives},
};

pub struct Drain<'a, T, S: SyncPrimitives + 'a> {
    sentinel_node: NodeLink<S>,
    queue: &'a Queue<T, S>,
    guard: ManuallyDrop<MutexGuard<'a, S>>,
    _node_data: PhantomData<T>,
}

impl<'a, T, S: SyncPrimitives> Drain<'a, T, S> {
    pub fn next_data(self: Pin<&mut Self>) -> NonNull<T> {
        let _ = &self.sentinel_node;
        todo!()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(self: Pin<&mut Self>) -> &T
    where
        T: Sync,
    {
        unsafe { self.next_data().as_ref() }
    }

    pub fn execute_unlocked<F: FnOnce() -> R, R>(self: Pin<&mut Self>, f: F) -> R {
        let this = unsafe { self.get_unchecked_mut() };
        // TODO chain to the sentinel
        unsafe { ManuallyDrop::drop(&mut this.guard) };
        let res = f();
        this.guard = ManuallyDrop::new(this.queue.mutex.lock());
        res
    }
}

impl<'a, T, S: SyncPrimitives> Drop for Drain<'a, T, S> {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.guard) };
    }
}
