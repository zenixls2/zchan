use core::fmt;
use crossbeam::atomic::AtomicConsume;
use crossbeam::queue::{ArrayQueue, PushError};
use futures::task::{current, Task};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[inline]
pub fn positive_update(a: &AtomicUsize) -> bool {
    let mut prev = a.load_consume();
    while prev > 0 {
        match a.compare_exchange_weak(prev, prev - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(e) => prev = e,
        }
    }
    false
}

pub struct AtomicSerialWaker {
    waker: ArrayQueue<Task>,
    // increase when wake() or wk() fails to find one task to notify()
    // decrease when register() and no other thread is waking
    waiting: AtomicUsize,
    // flag shared witch Receiver/UnboundedReceiver
    // please refer to the explanation in UnboundedReceiver/Receiver
    pub(crate) from_me: Arc<AtomicUsize>,
}

impl Default for AtomicSerialWaker {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicSerialWaker {
    pub fn new() -> Self {
        trait AssertSync: Sync {}
        impl AssertSync for Task {}
        Self {
            waker: ArrayQueue::new(4),
            waiting: AtomicUsize::new(0),
            from_me: Arc::new(AtomicUsize::new(0)),
        }
    }
    #[inline]
    pub fn register(&self) {
        match self.waker.push(current()) {
            Ok(()) => {
                self.wk();
            }
            Err(PushError(w)) => {
                self.from_me.fetch_add(1, Ordering::Release);
                w.notify();
            }
        };
    }

    #[inline]
    fn wk(&self) {
        if let Ok(task) = self.waker.pop() {
            if positive_update(&self.waiting) {
                self.from_me.fetch_add(1, Ordering::Release);
                task.notify();
            } else {
                match self.waker.push(task) {
                    Ok(()) => {}
                    Err(PushError(w)) => {
                        self.from_me.fetch_add(1, Ordering::Release);
                        w.notify();
                    }
                }
            }
        }
    }

    #[inline]
    pub fn wake(&self) {
        let task = self.waker.pop();
        if let Ok(task) = task {
            self.from_me.fetch_add(1, Ordering::Release);
            task.notify();
        } else {
            self.waiting.fetch_add(1, Ordering::Release);
        }
    }

    #[inline]
    pub fn wake_all(&self) {
        while let Ok(task) = self.waker.pop() {
            self.from_me.fetch_add(1, Ordering::Release);
            task.notify();
        }
    }
}

impl fmt::Debug for AtomicSerialWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AtomicSerialWaker")
    }
}

unsafe impl Send for AtomicSerialWaker {}
unsafe impl Sync for AtomicSerialWaker {}
