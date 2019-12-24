use crate::Shutdown;
use std::ops;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};
use std::usize;
use tokio::prelude::*;

/// Reference counter internals
struct Counter<C> {
    /// The number of senders associated with the channel.
    senders: AtomicUsize,

    /// The number of receivers associated with the channel.
    receivers: AtomicUsize,

    /// Set to true if the last sender or the last receiver reference deallocates the channel.
    destroy: AtomicBool,

    /// The internal channel
    chan: C,
}

/// Wraps a channel into the reference counter.
pub fn new<C: Shutdown>(chan: C) -> (Sender<C>, Receiver<C>) {
    let counter = Box::into_raw(Box::new(Counter {
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
        destroy: AtomicBool::new(false),
        chan,
    }));
    let s = Sender { counter };
    let r = Receiver { counter };
    (s, r)
}

/// The sending side.
pub struct Sender<C: Shutdown> {
    counter: *mut Counter<C>,
}

impl<C: Shutdown> Sender<C> {
    /// Returns the internal `Counter`.
    fn counter(&self) -> &Counter<C> {
        unsafe { &*self.counter }
    }

    fn counter_mut(&self) -> &mut Counter<C> {
        unsafe { &mut *self.counter }
    }

    /// Acquires another sender reference.
    pub fn acquire(&self) -> Sender<C> {
        let counter = self.counter().senders.fetch_add(1, Relaxed);

        // Cloning senders and calling `mem::forget` on the clones could potentially overflow the
        // counter. It's very difficult to recover sensibly from such degenerate scenarios so we
        // just abort when the counter becomes very large.
        if counter == usize::MAX {
            process::abort();
        }

        Sender {
            counter: self.counter,
        }
    }

    /// Releases the sender reference.
    ///
    /// Function `disconnect` will be called if this is the last sender reference.
    pub unsafe fn release<F: FnOnce(&C) -> bool>(&self, disconnect: F) {
        if self.counter().senders.fetch_sub(1, AcqRel) == 1 {
            disconnect(&self.counter().chan);
            if self.counter().destroy.swap(true, AcqRel) {
                drop(Box::from_raw(self.counter));
            }
        }
    }
}

impl<C: Shutdown> ops::Deref for Sender<C> {
    type Target = C;

    fn deref(&self) -> &C {
        &self.counter().chan
    }
}

impl<C: Shutdown> ops::DerefMut for Sender<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.counter_mut().chan
    }
}

impl<C: Shutdown> PartialEq for Sender<C> {
    fn eq(&self, other: &Sender<C>) -> bool {
        self.counter == other.counter
    }
}

impl<C: Shutdown> Drop for Sender<C> {
    fn drop(&mut self) {
        unsafe {
            self.release(|c| c.shutdown());
        };
    }
}

impl<C: Shutdown> Clone for Sender<C> {
    fn clone(&self) -> Self {
        self.acquire()
    }
}

impl<C: Sink + Shutdown> Sink for Sender<C> {
    type SinkItem = <C as Sink>::SinkItem;
    type SinkError = <C as Sink>::SinkError;
    fn start_send(
        &mut self,
        item: Self::SinkItem,
    ) -> Result<AsyncSink<Self::SinkItem>, Self::SinkError> {
        self.counter_mut().chan.start_send(item)
    }
    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}

unsafe impl<T: Send + Shutdown> Send for Sender<T> {}
unsafe impl<T: Send + Shutdown> Sync for Sender<T> {}

/// The receiving side.
pub struct Receiver<C: Shutdown> {
    counter: *mut Counter<C>,
}

impl<C: Shutdown> Receiver<C> {
    /// Returns the internal `Counter`.
    fn counter(&self) -> &Counter<C> {
        unsafe { &*self.counter }
    }

    fn counter_mut(&self) -> &mut Counter<C> {
        unsafe { &mut *self.counter }
    }

    /// Acquires another receiver reference.
    pub fn acquire(&self) -> Receiver<C> {
        let counter = self.counter().receivers.fetch_add(1, Relaxed);

        // Cloning receivers and calling `mem::forget` one the clones could potentially overflow the
        // counter. It's very difficult to recover sensibly from such degenerate scenarios so we
        // just abort when the counter becomes very large.
        if counter >= usize::MAX {
            process::abort();
        }
        Receiver {
            counter: self.counter,
        }
    }

    /// Releases the receiver reference.
    ///
    /// Function `disconnect` will be called if this is the last receiver reference.
    pub unsafe fn release<F: FnOnce(&C) -> bool>(&self, disconnect: F) {
        if self.counter().receivers.fetch_sub(1, AcqRel) == 1 {
            disconnect(&self.counter().chan);

            if self.counter().destroy.swap(true, AcqRel) {
                drop(Box::from_raw(self.counter));
            }
        }
    }
}

impl<C: Shutdown> ops::Deref for Receiver<C> {
    type Target = C;
    fn deref(&self) -> &C {
        &self.counter().chan
    }
}

impl<C: Shutdown> ops::DerefMut for Receiver<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.counter_mut().chan
    }
}

impl<C: Shutdown> PartialEq for Receiver<C> {
    fn eq(&self, other: &Receiver<C>) -> bool {
        self.counter == other.counter
    }
}

impl<C: Shutdown> Drop for Receiver<C> {
    fn drop(&mut self) {
        unsafe {
            self.release(|c| c.shutdown());
        }
    }
}

impl<C: Shutdown> Clone for Receiver<C> {
    fn clone(&self) -> Self {
        self.acquire()
    }
}

impl<C: Shutdown + Stream> Stream for Receiver<C> {
    type Item = <C as Stream>::Item;
    type Error = <C as Stream>::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.counter_mut().chan.poll()
    }
}

unsafe impl<T: Send + Shutdown> Send for Receiver<T> {}
unsafe impl<T: Send + Shutdown> Sync for Receiver<T> {}
