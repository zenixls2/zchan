use crate::atomic_serial_waker::*;
use crate::error::*;
use crate::Shutdown;
use crossbeam::atomic::AtomicConsume;
use crossbeam::utils::CachePadded;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::ptr;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::time::Instant;
use tokio::prelude::*;

// Bits indicating the state of a slot
// * If a message has been written into the slot, `WRITE` is set.
// * If a message has been read from the slot, `READ` is set.
// * If the block is being destroyed, `DESTROY` is set.
const WRITE: usize = 1;
const READ: usize = 2;
const DESTROY: usize = 4;

// Each block covers one "lap" of indices.
const LAP: usize = 32;
// The maximum number of messages a block can hold.
const BLOCK_CAP: usize = LAP - 1;
// How many lower bits are reserved for metadata.
const SHIFT: usize = 1;
// Has two different purposes:
// * If set in head, indicates that the block is not the last one.
// * If set in tail, indicates the channel is disconnected.
const MARK_BIT: usize = 1;

/// A slot in a block
struct Slot<T> {
    /// the message
    msg: UnsafeCell<ManuallyDrop<T>>,
    /// The state of the slot
    state: AtomicUsize,
}

impl<T> Slot<T> {
    /// Waits until a message been written into the slot
    #[inline]
    fn wait_write(&self) {
        while self.state.load_consume() & WRITE == 0 {}
    }
}

/// A block in a linked list.
///
/// Each block in the list can hold up to `BLOCK_CAP` messages.
struct Block<T> {
    /// The next block in the linked list.
    next: AtomicPtr<Block<T>>,

    /// Slots for messages.
    slots: [Slot<T>; BLOCK_CAP],
}

impl<T> Block<T> {
    /// Creates an empty block
    fn new() -> Block<T> {
        unsafe { mem::zeroed() }
    }

    /// Waits until the next pointer is set.
    #[inline]
    fn wait_next(&self) -> *mut Block<T> {
        loop {
            let next = self.next.load(Acquire);
            if !next.is_null() {
                return next;
            }
        }
    }

    /// Sets the `DESTROY` bit in slots starting from `start` and destroys the block.
    #[inline]
    unsafe fn destroy(this: *mut Block<T>, start: usize) {
        // It is not necessary to set the `DESTROY` bit in the last slot because that
        // slot has begun destruction of the block.
        for i in start..BLOCK_CAP - 1 {
            let slot = (*this).slots.get_unchecked(i);

            // Mark the `DESTROY` bit if a thread is still using the slot.
            if slot.state.load_consume() & READ == 0
                && slot.state.fetch_or(DESTROY, AcqRel) & READ == 0
            {
                // If a thread is still using the slot, it will continue destruction of the block
                return;
            }
        }

        // No thread is using the block, now it is safe to destroy it.
        drop(Box::from_raw(this));
    }
}

/// A position in a channel.
#[derive(Debug)]
struct Position<T> {
    /// The index in the channel.
    index: AtomicUsize,

    /// The clock in the linked list.
    block: AtomicPtr<Block<T>>,
}

/// The token type for the list flavor.
#[derive(Debug)]
pub struct List {
    /// The block of slots
    block: *const u8,

    /// The ofset into the block.
    offset: usize,
}

impl Default for List {
    #[inline]
    fn default() -> Self {
        Self {
            block: ptr::null(),
            offset: 0,
        }
    }
}

/// Unbounded channel implemented as a linked list.
///
/// Each message sent into the channel is assigned a sequence number, i.e. an index.
/// Indices are represented as numbers of type `usize` and wrap on overflow.
///
/// Consecutive messages are grouped into blocks in order to put less pressure on the allocator
/// and improve cache efficiency.
pub struct Channel<T> {
    /// The head of the channel.
    head: CachePadded<Position<T>>,

    /// The tail of the channel.
    tail: CachePadded<Position<T>>,

    /// receivers waiting while the channel is empty and not disconnected.
    receivers: AtomicSerialWaker,

    from_me: Arc<AtomicUsize>,

    first: bool,

    /// Indicates that dropping a `Channel<T>` may drop messages of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    /// Creates a new unbounded channel.
    pub fn new() -> Self {
        let waker = AtomicSerialWaker::new();
        let from_me = waker.from_me.clone();
        Channel {
            head: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            tail: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            receivers: waker,
            from_me,
            first: true,
            _marker: PhantomData,
        }
    }

    /// Attempts to reserve a slot for sending a message.
    pub fn start_send(&self, list: &mut List) -> bool {
        let mut tail = self.tail.index.load_consume();
        let mut block = self.tail.block.load(Acquire);
        let mut next_block = None;

        loop {
            // Check if the channel is disconnected
            if tail & MARK_BIT != 0 {
                list.block = ptr::null();
                return true;
            }

            // Calculate the offset of the index into the block.
            let offset = (tail >> SHIFT) % LAP;

            // If we reached the end of the block, wait until the next one is installed.
            if offset == BLOCK_CAP {
                tail = self.tail.index.load_consume();
                block = self.tail.block.load(Acquire);
                continue;
            }

            // If we're going to have to install the next block, allocate it in advance
            // in order to make the wait for other threads as short as possible.
            if offset + 1 == BLOCK_CAP && next_block.is_none() {
                next_block = Some(Box::new(Block::<T>::new()));
            }

            // If this is the first message to be sent into the channel, we need to allocate
            // the first block and install it.
            if block.is_null() {
                let new = Box::into_raw(Box::new(Block::<T>::new()));
                if self.tail.block.compare_and_swap(block, new, Release) == block {
                    self.head.block.store(new, Release);
                    block = new;
                } else {
                    next_block = unsafe { Some(Box::from_raw(new)) };
                    tail = self.tail.index.load_consume();
                    block = self.tail.block.load(Acquire);
                    continue;
                }
            }

            let new_tail = tail + (1 << SHIFT);

            // Try advancing the tail forward
            match self
                .tail
                .index
                .compare_exchange_weak(tail, new_tail, SeqCst, Acquire)
            {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, install the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next_block = Box::into_raw(next_block.unwrap());
                        self.tail.block.store(next_block, Release);
                        self.tail.index.fetch_add(1 << SHIFT, Release);
                        (*block).next.store(next_block, Release);
                    }
                    list.block = block as *const u8;
                    list.offset = offset;
                    return true;
                },
                Err(t) => {
                    tail = t;
                    block = self.tail.block.load(Acquire);
                }
            }
        }
    }

    /// Writes a message into the channel.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn write(&self, list: &mut List, msg: T) -> Result<(), T> {
        // If there is no slot, the channel is disconnected.
        if list.block.is_null() {
            return Err(msg);
        }

        // Write the message into the slot.
        let block = list.block as *mut Block<T>;
        let offset = list.offset;
        let slot = (*block).slots.get_unchecked(offset);
        slot.msg.get().write(ManuallyDrop::new(msg));
        slot.state.fetch_or(WRITE, Release);

        // Wake a sleeping receiver.
        self.receivers.wake();
        Ok(())
    }

    /// Attempts to reserve a slot for receiving a message.
    pub fn start_recv(&self, list: &mut List) -> bool {
        let mut head = self.head.index.load_consume();
        let mut block = self.head.block.load(Acquire);

        loop {
            // Calculate the offset of the index into the block.
            let offset = (head >> SHIFT) % LAP;

            // If we reached the end of the block, wait until the next one is installed.
            if offset == BLOCK_CAP {
                head = self.head.index.load_consume();
                block = self.head.block.load(Acquire);
                continue;
            }
            let mut new_head = head + (1 << SHIFT);

            if new_head & MARK_BIT == 0 {
                atomic::fence(SeqCst);
                let tail = self.tail.index.load_consume();

                // If the tail equals the head, that means the channel is empty.
                if head >> SHIFT == tail >> SHIFT {
                    if tail & MARK_BIT != 0 {
                        // ...then receive an error.
                        list.block = ptr::null();
                        return true;
                    } else {
                        // Otherwise, the receive operation is not ready.
                        return false;
                    }
                }

                // If head and tail are not in the same block, set `MARK_BIT` in head.
                if (head >> SHIFT) / LAP != (tail >> SHIFT) / LAP {
                    new_head |= MARK_BIT;
                }
            }

            // The block can be null here only if the first message is being sent into the channel.
            // In that case, just wait until it gets initialized.
            if block.is_null() {
                head = self.head.index.load_consume();
                block = self.head.block.load(Acquire);
                continue;
            }

            // Try moving the head index forward.
            match self
                .head
                .index
                .compare_exchange_weak(head, new_head, SeqCst, Acquire)
            {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, move to the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next = (*block).wait_next();
                        let mut next_index = (new_head & !MARK_BIT).wrapping_add(1 << SHIFT);
                        if !(*next).next.load(Relaxed).is_null() {
                            next_index |= MARK_BIT;
                        }
                        self.head.block.store(next, Release);
                        self.head.index.store(next_index, Release);
                    }
                    list.block = block as *const u8;
                    list.offset = offset;
                    return true;
                },
                Err(h) => {
                    head = h;
                    block = self.head.block.load(Acquire);
                }
            }
        }
    }

    /// Reads a message from the channel.
    unsafe fn poll_read(&self, list: &mut List) -> Poll<Option<T>, ()> {
        if list.block.is_null() {
            return Ok(Async::Ready(None));
        }
        let block = list.block as *mut Block<T>;
        let offset = list.offset;
        let slot = (*block).slots.get_unchecked(offset);
        slot.wait_write();
        let m = slot.msg.get().read();
        let msg = ManuallyDrop::into_inner(m);
        // Destroy the block if we've reached the end, or if another thread wanted to destroy
        // but couldn't because we're busy reading from the slot.
        if offset + 1 == BLOCK_CAP {
            Block::destroy(block, 0);
        } else if slot.state.fetch_or(READ, AcqRel) & DESTROY != 0 {
            Block::destroy(block, offset + 1);
        }
        Ok(Async::Ready(Some(msg)))
    }

    /// Reads a message from the channel.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn read(&self, list: &mut List) -> Result<T, ()> {
        if list.block.is_null() {
            // The channel is disconnected.
            return Err(());
        }

        // Read the message.
        let block = list.block as *mut Block<T>;
        let offset = list.offset;
        let slot = (*block).slots.get_unchecked(offset);
        slot.wait_write();
        let m = slot.msg.get().read();
        let msg = ManuallyDrop::into_inner(m);

        // Destroy the block if we've reached the end, or if another thread wanted to destroy
        // but couldn't because we're busy reading from the slot.
        if offset + 1 == BLOCK_CAP {
            Block::destroy(block, 0);
        } else if slot.state.fetch_or(READ, AcqRel) & DESTROY != 0 {
            Block::destroy(block, offset + 1);
        }
        Ok(msg)
    }

    /// Attempts to send a message into the channel.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.send(msg, None).map_err(|err| match err {
            SendTimeoutError::Disconnected(msg) => TrySendError {
                kind: ErrorKind::Closed,
                value: msg,
            },
            SendTimeoutError::Timeout(_) => unreachable!(),
        })
    }

    /// Sends a message into the channel
    pub fn send(&self, msg: T, _deadline: Option<Instant>) -> Result<(), SendTimeoutError<T>> {
        let list = &mut List::default();
        assert!(self.start_send(list));
        unsafe {
            self.write(list, msg)
                .map_err(SendTimeoutError::Disconnected)
        }
    }
    /// Attempts to receive a message without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let list = &mut List::default();

        if self.start_recv(list) {
            unsafe { self.read(list).map_err(|_| TryRecvError::Disconnected) }
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Returns the current number of messages inside the channel.
    pub fn len(&self) -> usize {
        loop {
            // Load the tail index, then load the head index.
            let mut tail = self.tail.index.load_consume();
            let mut head = self.head.index.load_consume();

            // If the tail index didn't change, we've got consistent indices to work with.
            if self.tail.index.load_consume() == tail {
                // Erase the lower bits.
                tail &= !((1 << SHIFT) - 1);
                head &= !((1 << SHIFT) - 1);

                // Rotate indices so that head falls into the frist block.
                let lap = (head >> SHIFT) / LAP;
                tail = tail.wrapping_sub((lap * LAP) << SHIFT);
                head = head.wrapping_sub((lap * LAP) << SHIFT);

                // Remove the lower bits.
                tail >>= SHIFT;
                head >>= SHIFT;

                // Fix up indecies if they fall onto block ends.
                if head == BLOCK_CAP {
                    head = 0;
                    tail -= LAP;
                }
                if tail == BLOCK_CAP {
                    tail += 1;
                }

                // Return the difference minus the number of blocks between tail and head.
                return tail - head - tail / LAP;
            }
        }
    }

    /// Returns the capacity of the channel.
    pub fn capacity(&self) -> Option<usize> {
        None
    }

    pub fn is_empty(&self) -> bool {
        let head = self.head.index.load_consume();
        let tail = self.tail.index.load_consume();
        head >> SHIFT == tail >> SHIFT
    }

    pub fn is_full(&self) -> bool {
        false
    }
}

impl<T> Shutdown for Channel<T> {
    fn shutdown(&self) -> bool {
        let tail = self.tail.index.fetch_or(MARK_BIT, SeqCst);
        if tail & MARK_BIT == 0 {
            self.receivers.wake_all();
            true
        } else {
            false
        }
    }

    fn is_shutdown(&self) -> bool {
        self.tail.index.load_consume() & MARK_BIT != 0
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        let mut head = self.head.index.load_consume();
        let mut tail = self.tail.index.load_consume();
        let mut block = self.head.block.load(Relaxed);

        // Erase the lower bits
        head &= !((1 << SHIFT) - 1);
        tail &= !((1 << SHIFT) - 1);

        unsafe {
            // Drop all messages between head and tail and deallocate the help-allocated blocks.
            while head != tail {
                let offset = (head >> SHIFT) % LAP;
                if offset < BLOCK_CAP {
                    // Drop the message in the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    ManuallyDrop::drop(&mut *(*slot).msg.get());
                } else {
                    // Deallocate the block and move to the next one.
                    let next = (*block).next.load(Relaxed);
                    drop(Box::from_raw(block));
                    block = next;
                }
                head = head.wrapping_add(1 << SHIFT);
            }

            // Deallocate the last remmaining block.
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
    }
}

impl<T> Stream for Channel<T> {
    type Item = T;
    type Error = ();
    fn poll(&mut self) -> Poll<Option<T>, ()> {
        let list = &mut List::default();
        if self.start_recv(list) {
            unsafe { self.poll_read(list) }
        } else if self.first {
            self.first = false;
            self.receivers.register();
            if self.start_recv(list) {
                unsafe { self.poll_read(list) }
            } else {
                Ok(Async::NotReady)
            }
        } else if positive_update(&self.from_me) {
            self.receivers.register();
            if self.start_recv(list) {
                unsafe { self.poll_read(list) }
            } else {
                Ok(Async::NotReady)
            }
        } else {
            Ok(Async::NotReady)
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
            _ => unreachable!(),
        }
    }

    #[inline]
    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}
