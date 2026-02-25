#[allow(dead_code)]
#[path = "semaphore.rs"]
mod semaphore;

use std::{
    cell::UnsafeCell,
    fmt, mem,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use semaphore::Semaphore;

#[derive(Debug)]
pub struct TryLockError(());

pub struct Mutex<T: ?Sized> {
    semaphore: Semaphore,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(data: T) -> Self {
        Self {
            semaphore: Semaphore::new(1),
            data: UnsafeCell::new(data),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        mem::forget(self.semaphore.try_acquire().map_err(|_| TryLockError(()))?);
        Ok(MutexGuard { mutex: self })
    }

    pub async fn lock(&self) -> MutexGuard<'_, T> {
        if let Ok(guard) = self.try_lock() {
            return guard;
        }
        mem::forget(self.semaphore.acquire().await.unwrap());
        MutexGuard { mutex: self }
    }

    pub fn try_lock_owned(self: Arc<Self>) -> Result<OwnedMutexGuard<T>, TryLockError> {
        self.try_lock()
            .map(mem::forget)
            .map(|_| OwnedMutexGuard { mutex: self })
    }

    pub async fn lock_owned(self: Arc<Self>) -> OwnedMutexGuard<T> {
        mem::forget(self.lock().await);
        OwnedMutexGuard { mutex: self }
    }

    fn unlock(&self) {
        self.semaphore.add_permits(1);
    }
}

pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MutexGuard").field(&&**self).finish()
    }
}

pub struct OwnedMutexGuard<T: ?Sized> {
    mutex: Arc<Mutex<T>>,
}

impl<T: ?Sized> Deref for OwnedMutexGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for OwnedMutexGuard<T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for OwnedMutexGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MutexGuard").field(&&**self).finish()
    }
}

fn main() {}
