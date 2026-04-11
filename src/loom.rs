pub(crate) mod sync {
    cfg_if::cfg_if! {
        if #[cfg(loom)] {
            pub(crate) use loom::sync::*;
        } else if #[cfg(feature = "std")] {
            extern crate std;
            pub(crate) use std::sync::*;
        }
    }
    pub(crate) mod atomic {
        #[cfg(not(loom))]
        pub(crate) use core::sync::atomic::*;

        #[cfg(loom)]
        pub(crate) use loom::sync::atomic::*;

        #[cfg(loom)]
        fn seqcst_fence(order: Ordering) {
            if order == loom::sync::atomic::Ordering::SeqCst {
                loom::sync::atomic::fence(order);
            }
        }

        #[cfg(loom)]
        #[derive(Debug)]
        pub(crate) struct AtomicPtr<T>(loom::sync::atomic::AtomicPtr<T>);

        #[cfg(loom)]
        impl<T> AtomicPtr<T> {
            pub(crate) fn new(ptr: *mut T) -> Self {
                Self(loom::sync::atomic::AtomicPtr::new(ptr))
            }

            pub(crate) fn with_mut<R>(&mut self, f: impl FnOnce(&mut *mut T) -> R) -> R {
                self.0.with_mut(|x| f(x))
            }

            pub(crate) fn load(&self, order: Ordering) -> *mut T {
                seqcst_fence(order);
                self.0.load(order)
            }

            pub(crate) fn store(&self, x: *mut T, order: Ordering) {
                self.0.store(x, order);
                seqcst_fence(order);
            }

            pub(crate) fn swap(&self, x: *mut T, order: Ordering) -> *mut T {
                seqcst_fence(order);
                let res = self.0.swap(x, order);
                seqcst_fence(order);
                res
            }

            pub(crate) fn compare_exchange(
                &self,
                current: *mut T,
                new: *mut T,
                success: Ordering,
                failure: Ordering,
            ) -> Result<*mut T, *mut T> {
                seqcst_fence(success);
                let res = self.0.compare_exchange(current, new, success, failure);
                if res.is_ok() {
                    seqcst_fence(success);
                }
                res
            }

            pub(crate) fn compare_exchange_weak(
                &self,
                current: *mut T,
                new: *mut T,
                success: Ordering,
                failure: Ordering,
            ) -> Result<*mut T, *mut T> {
                seqcst_fence(success);
                let res = self.0.compare_exchange_weak(current, new, success, failure);
                if res.is_ok() {
                    seqcst_fence(success);
                }
                res
            }
        }
    }
}

pub(crate) trait AtomicPtrExt<T> {
    fn load_mut(&mut self) -> *mut T;
    fn store_mut(&mut self, ptr: *mut T);
}

impl<T> AtomicPtrExt<T> for sync::atomic::AtomicPtr<T> {
    fn load_mut(&mut self) -> *mut T {
        #[cfg(not(loom))]
        return *self.get_mut();
        #[cfg(loom)]
        return self.with_mut(|ptr| *ptr);
    }

    fn store_mut(&mut self, ptr: *mut T) {
        #[cfg(not(loom))]
        let () = *self.get_mut() = ptr;
        #[cfg(loom)]
        return self.with_mut(|p| *p = ptr);
    }
}
