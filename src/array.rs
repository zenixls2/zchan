use crate::atomic_serial_waker::*;
use crate::error::*;
use crate::Shutdown;
use crossbeam::atomic::AtomicConsume;
use crossbeam::utils::CachePadded;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::atomic::{self, AtomicUsize, Ordering::*};
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::time::Instant;
use tokio::prelude::*;

/// A slot in a channel.
struct Slot<T> {
    /// The current stamp.
    stamp: AtomicUsize,

    /// The message in this slot
    msg: UnsafeCell<T>,
}

/// The token type for the array
#[derive(Debug)]
pub struct Array {
    /// Slot to read from or write to.
    slot: *const u8,

    /// Stamp to store into the slot after reading or writing.
    stamp: usize,
}

impl Default for Array {
    #[inline]
    fn default() -> Self {
        Self {
            slot: ptr::null(),
            stamp: 0,
        }
    }
}

struct Ptr<T> {
    p: UnsafeCell<*mut T>,
}

impl<T> Ptr<T> {
    #[inline]
    pub fn new(t: *mut T) -> Self {
        Self {
            p: UnsafeCell::new(t),
        }
    }
    #[inline]
    pub unsafe fn get(&self) -> *mut T {
        *self.p.get()
    }
}

unsafe impl<T> Send for Ptr<T> {}
unsafe impl<T> Sync for Ptr<T> {}

/// Bounded channel based on a preallocated array.
pub struct Channel<T> {
    /// The head of the channel.
    ///
    /// This value is a "stamp" consisting of an index into the buffer, a mark bit, and a lap, but
    /// packed into a single `usize`. The lower bits represent the index, while the upper bits
    /// represent the lap. The mark bit in the head is always zero.
    ///
    /// Messages are popped from the head of the channel.
    head: CachePadded<AtomicUsize>,

    /// The tail of the channel
    ///
    /// This value is a "stamp" consisting of an index into the buffer, a mark bit, and a lap, but
    /// packed into a single `usize`. The lower bits represent the index, while the upper bits
    /// represent the lap. The mark bit indicates that the channel is disconnected.
    ///
    /// Messages are pushed into the tail of the channel.
    tail: CachePadded<AtomicUsize>,

    /// The buffer holding slots.
    buffer: Ptr<Slot<T>>,

    /// The channel capacity
    cap: usize,

    /// A stamp with the value of `{lap: 1, mark: 0, index: 0}`.
    one_lap: usize,

    /// If this bit is set in the tail, that means the channel is disconnected.
    mark_bit: usize,

    /// Senders waiting while the channel is full.
    senders: AtomicSerialWaker,

    /// Receivers waiting while the channel is empty and not disconnected.
    receivers: AtomicSerialWaker,

    from_me: Arc<AtomicUsize>,

    #[allow(dead_code)]
    to_me: Arc<AtomicUsize>,

    first: bool,

    /// Indicates that dropping a `Channel<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    /// Creates a bounded channel of capacity `cap`.
    pub fn with_capacity(cap: usize) -> Self {
        assert!(cap > 0, "capacity must be positive. use zero() instead");

        // Compute constants `mark_bit` and `one_lap`.
        let mark_bit = (cap + 1).next_power_of_two();
        let one_lap = mark_bit * 2;

        // Head is initialized to `{lap: 0, mark: 0, index: 0}`.
        let head = 0;
        // Tail is initialized to `{lap: 0, mark: 0, index: 0}`.
        let tail = 0;

        // Allocate a buffer of `cap` slots.
        let buffer = {
            let mut v = Vec::<Slot<T>>::with_capacity(cap);
            let ptr = v.as_mut_ptr();
            mem::forget(v);
            ptr
        };

        for i in 0..cap {
            unsafe {
                // Set the stamp to `{lap: 0, mark: 0, index: i}`.
                let slot = buffer.add(i);
                ptr::write(&mut (*slot).stamp, AtomicUsize::new(i));
            }
        }
        let senders = AtomicSerialWaker::new();
        let to_me = senders.from_me.clone();
        let receivers = AtomicSerialWaker::new();
        let from_me = receivers.from_me.clone();

        Self {
            buffer: Ptr::new(buffer),
            cap,
            one_lap,
            mark_bit,
            head: CachePadded::new(AtomicUsize::new(head)),
            tail: CachePadded::new(AtomicUsize::new(tail)),
            senders,
            receivers,
            to_me,
            from_me,
            first: true,
            _marker: PhantomData,
        }
    }

    /// Attempts to reserve a slot for sending a message.
    fn start_send(&self, array: &mut Array) -> bool {
        let mut tail = self.tail.load_consume();
        loop {
            // Check if the channel is disconnected.
            if tail & self.mark_bit != 0 {
                array.slot = ptr::null();
                array.stamp = 0;
                return true;
            }

            // Deconstruct the tail
            let index = tail & (self.mark_bit - 1);
            let lap = tail & !(self.one_lap - 1);

            // Inspect the corresponding slot.
            let slot = unsafe { &*self.buffer.get().add(index) };
            let stamp = slot.stamp.load_consume();

            if tail == stamp {
                let new_tail = if index + 1 < self.cap {
                    // Same lap, incremented index.
                    // Set to `{lap: lap, mark: 0, index: index + 1}`.
                    tail + 1
                } else {
                    // One lap forward, index wraps around to zero.
                    // Set to `{lap: lap.wrapping_add(1), mark: 0, index: 0}`.
                    lap.wrapping_add(self.one_lap)
                };

                // Try moving the tail.
                match self
                    .tail
                    .compare_exchange_weak(tail, new_tail, SeqCst, Relaxed)
                {
                    Ok(_) => {
                        // Prepare the token for the follow-up call to `write`.
                        array.slot = slot as *const Slot<T> as *const u8;
                        array.stamp = tail + 1;
                        return true;
                    }
                    Err(t) => {
                        tail = t;
                    }
                }
            } else if stamp.wrapping_add(self.one_lap) == tail + 1 {
                atomic::fence(SeqCst);
                let head = self.head.load_consume();

                // If the head lags one lap behind the tail as well...
                if head.wrapping_add(self.one_lap) == tail {
                    return false;
                }

                tail = self.tail.load_consume();
            } else {
                tail = self.tail.load_consume();
            }
        }
    }

    /// Writes a message into the channel.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn write(&self, array: &mut Array, msg: T) -> Result<(), T> {
        // If there is no slot, the channel is disconnected.
        if array.slot.is_null() {
            return Err(msg);
        }

        let slot: &Slot<T> = &*(array.slot as *const Slot<T>);

        // Write the message into the slot and update the stamp.
        slot.msg.get().write(msg);
        slot.stamp.store(array.stamp, Release);

        // Wake a sleeping receiver.
        self.receivers.wake();
        Ok(())
    }

    /// Attempts to reserve a slot for receiving a message.
    fn start_recv(&self, array: &mut Array) -> bool {
        let mut head = self.head.load_consume();

        loop {
            // Deconstruct the head.
            let index = head & (self.mark_bit - 1);
            let lap = head & !(self.one_lap - 1);

            // Inspect the corresponding slot.
            let slot = unsafe { &*self.buffer.get().add(index) };
            let stamp = slot.stamp.load_consume();

            if head + 1 == stamp {
                let new = if index + 1 < self.cap {
                    // Same lap, incremented index.
                    // Set to `{lap: lap, mark: 0, index: index + 1}`.
                    head + 1
                } else {
                    // One lap forward, index wraps around to zero.
                    // Set to `{lap: lap.wrapping_add(1), mark: 0, index: 0}`.
                    lap.wrapping_add(self.one_lap)
                };

                // Try moving the head.
                match self.head.compare_exchange_weak(head, new, SeqCst, Relaxed) {
                    Ok(_) => {
                        // Prepare the token for the follow-up call to `read`.
                        array.slot = slot as *const Slot<T> as *const u8;
                        array.stamp = head.wrapping_add(self.one_lap);
                        return true;
                    }
                    Err(h) => {
                        head = h;
                    }
                }
            } else if stamp == head {
                atomic::fence(SeqCst);
                let tail = self.tail.load_consume();

                // If the tail equals the head, that means the channel is empty.
                if (tail & !self.mark_bit) == head {
                    // If the channel is disconnected...
                    if tail & self.mark_bit != 0 {
                        // ...hen receive an error
                        array.slot = ptr::null();
                        array.stamp = 0;
                        return true;
                    } else {
                        // Otherwrise, the receive operation is not ready.
                        return false;
                    }
                }
                head = self.head.load_consume();
            } else {
                head = self.head.load_consume();
            }
        }
    }

    unsafe fn poll_read(&self, array: &mut Array) -> Poll<Option<T>, ()> {
        if array.slot.is_null() {
            return Ok(Async::Ready(None));
        }
        let slot: &Slot<T> = &*(array.slot as *const Slot<T>);

        // Read the message from the slot and update the stamp.
        let msg = slot.msg.get().read();
        slot.stamp.store(array.stamp, Release);

        self.senders.wake();
        Ok(Async::Ready(Some(msg)))
    }

    /// Reads a message from the channel.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn read(&self, array: &mut Array) -> Result<T, ()> {
        if array.slot.is_null() {
            // The channel is disconnected.
            return Err(());
        }

        let slot: &Slot<T> = &*(array.slot as *const Slot<T>);

        // Read the message from the slot and update the stamp.
        let msg = slot.msg.get().read();
        slot.stamp.store(array.stamp, Release);

        // Wake a sleeping sender.
        self.senders.wake();
        Ok(msg)
    }

    /// Attempts to send a message into the channel.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        let array = &mut Array::default();
        if self.start_send(array) {
            unsafe {
                self.write(array, msg).map_err(|msg| TrySendError {
                    kind: ErrorKind::Closed,
                    value: msg,
                })
            }
        } else {
            self.senders.register();
            Err(TrySendError {
                kind: ErrorKind::NoCapacity,
                value: msg,
            })
        }
    }

    /// Sends a message into the channel.
    pub fn send(&self, msg: T, deadline: Option<Instant>) -> Result<(), SendTimeoutError<T>> {
        let array = &mut Array::default();
        loop {
            // Try sending a message several times.
            if self.start_send(array) {
                let res = unsafe { self.write(array, msg) };
                return res.map_err(SendTimeoutError::Disconnected);
            }
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    return Err(SendTimeoutError::Timeout(msg));
                }
            }
        }
    }

    /// Attempts to receive a message without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let array = &mut Array::default();

        if self.start_recv(array) {
            unsafe { self.read(array).map_err(|_| TryRecvError::Disconnected) }
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Returns the current number of messages inside the channel.
    pub fn len(&self) -> usize {
        loop {
            // Load the tail, then load the head.
            let tail = self.tail.load_consume();
            let head = self.head.load_consume();

            // If the tail didn't change, we've got consistent values to work with.
            if self.tail.load_consume() == tail {
                let hix = head & (self.mark_bit - 1);
                let tix = tail & (self.mark_bit - 1);

                return if hix < tix {
                    tix - hix
                } else if hix > tix {
                    self.cap - hix + tix
                } else if (tail & !self.mark_bit) == head {
                    0
                } else {
                    self.cap
                };
            }
        }
    }

    /// Returns the capacity of the channel.
    pub fn capacity(&self) -> Option<usize> {
        Some(self.cap)
    }

    pub fn is_empty(&self) -> bool {
        let head = self.head.load_consume();
        let tail = self.tail.load_consume();

        // Is the tail equal to the head?
        //
        // Note: If the head changes just before we load the tail, that means there was a moment
        // when the channel was not empty, so it is safe to just return `false`.
        (tail & self.mark_bit) == head
    }

    /// Returns `true` if the channel is full.
    pub fn is_full(&self) -> bool {
        let tail = self.tail.load_consume();
        let head = self.head.load_consume();

        // Is the head lagging one lap behind tail?
        //
        // Note: If the tail changes just before we load the head, that means there was a moment
        // when the channel was not full, so it is safe just return `false`.
        head.wrapping_add(self.one_lap) == tail & !self.mark_bit
    }
}

impl<T> Shutdown for Channel<T> {
    fn shutdown(&self) -> bool {
        let tail = self.tail.fetch_or(self.mark_bit, SeqCst);
        if tail & self.mark_bit == 0 {
            self.senders.wake_all();
            self.receivers.wake_all();
            true
        } else {
            false
        }
    }

    fn is_shutdown(&self) -> bool {
        self.tail.load_consume() & self.mark_bit != 0
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        // Get the index of the head
        let hix = self.head.load_consume() & (self.mark_bit - 1);

        // Loop over all slots that hold a message and drop them.
        for i in 0..self.len() {
            // Compute the index of the next slot holding a message.
            let index = if hix + i < self.cap {
                hix + i
            } else {
                hix + i - self.cap
            };

            unsafe {
                self.buffer.get().add(index).drop_in_place();
            }
        }

        // Finally, deallocate the buffer, but don't run any destructors.
        unsafe {
            Vec::from_raw_parts(self.buffer.get(), 0, self.cap);
        }
    }
}

impl<T> Stream for Channel<T> {
    type Item = T;
    type Error = ();
    fn poll(&mut self) -> Poll<Option<T>, ()> {
        let array = &mut Array::default();
        unsafe {
            if self.start_recv(array) {
                self.poll_read(array)
            } else if self.first {
                self.first = false;
                self.receivers.register();
                if self.start_recv(array) {
                    self.poll_read(array)
                } else {
                    Ok(Async::NotReady)
                }
            } else if positive_update(&self.from_me) {
                self.receivers.register();
                if self.start_recv(array) {
                    self.poll_read(array)
                } else {
                    Ok(Async::NotReady)
                }
            } else {
                Ok(Async::NotReady)
            }
        }
    }
}

impl<T> Sink for Channel<T> {
    type SinkItem = T;
    type SinkError = SendError;

    #[inline]
    fn start_send(&mut self, item: T) -> Result<AsyncSink<Self::SinkItem>, Self::SinkError> {
        match self.try_send(item) {
            Ok(()) => Ok(AsyncSink::Ready),
            Err(e) if e.is_closed() => Err(SendError),
            Err(e) if e.is_full() => Ok(AsyncSink::NotReady(e.into_inner())),
            Err(_) => unreachable!(),
        }
    }

    #[inline]
    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}
