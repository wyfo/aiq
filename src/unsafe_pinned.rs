use core::{cell::UnsafeCell, marker::PhantomPinned};

#[derive(Debug, Default)]
pub struct UnsafePinned<T: ?Sized> {
    _phantom: PhantomPinned,
    inner: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Sync> Sync for UnsafePinned<T> {}

impl<T> UnsafePinned<T> {
    pub const fn new(inner: T) -> Self {
        Self {
            _phantom: PhantomPinned,
            inner: UnsafeCell::new(inner),
        }
    }

    #[allow(dead_code)]
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}
impl<T: ?Sized> UnsafePinned<T> {
    pub const fn get(&self) -> *mut T {
        self.inner.get()
    }
}
