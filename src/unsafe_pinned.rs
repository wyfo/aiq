use core::{cell::UnsafeCell, marker::PhantomPinned};

#[derive(Debug, Default)]
pub struct UnsafePinned<T: ?Sized> {
    _phantom: PhantomPinned,
    value: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Sync> Sync for UnsafePinned<T> {}

impl<T> UnsafePinned<T> {
    pub const fn new(inner: T) -> Self {
        Self {
            _phantom: PhantomPinned,
            value: UnsafeCell::new(inner),
        }
    }

    #[allow(dead_code)]
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}
impl<T: ?Sized> UnsafePinned<T> {
    #[inline(always)]
    pub const fn get(&self) -> *mut T {
        self.value.get()
    }

    #[inline(always)]
    pub const fn raw_get(this: *const Self) -> *mut T {
        UnsafeCell::raw_get(this as _)
    }
}
