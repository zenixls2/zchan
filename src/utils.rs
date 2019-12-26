use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering::*};

/// A simple spinlock
pub struct Spinlock<T> {
    flag: AtomicBool,
    value: UnsafeCell<T>,
}

impl<T> Spinlock<T> {
    /// Returns a new spinlock initialized with `value`.
    pub fn new(value: T) -> Self {
        Self {
            flag: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    // Locks the spinlock.
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        while self.flag.swap(true, Acquire) {}
        SpinlockGuard { parent: self }
    }
}

/// A guard holding a spinlock locked.
pub struct SpinlockGuard<'a, T: 'a> {
    parent: &'a Spinlock<T>,
}

impl<'a, T> Drop for SpinlockGuard<'a, T> {
    fn drop(&mut self) {
        self.parent.flag.store(false, Release);
    }
}

impl<'a, T> Deref for SpinlockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.parent.value.get() }
    }
}

impl<'a, T> DerefMut for SpinlockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.parent.value.get() }
    }
}
