use crate::atomic_serial_waker::*;
use crate::context::Context;
use crate::error::*;
use crate::select::{Operation, Selected};
use crate::utils::Spinlock;
use crate::waker::Waker;
use crate::Shutdown;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use tokio::prelude::*;

/// A pointer to a packet
pub type Zero = usize;

/// A slot for passing one message from a sender to a receiver.
struct Packet<T> {
    /// Equals `true` if the packet is allocated on the stack.
    on_stack: bool,

    /// Equals `true` once the packet is ready for reading or writing.
    ready: AtomicBool,

    /// The message.
    msg: UnsafeCell<Option<T>>,
}

impl<T> Packet<T> {
    /// Creates an empty packet on the stack.
    fn empty_on_stack() -> Self {
        Self {
            on_stack: true,
            ready: AtomicBool::new(false),
            msg: UnsafeCell::new(None),
        }
    }

    /// Creates an empty packet on the heap.
    #[allow(dead_code)]
    fn empty_on_heap() -> Box<Self> {
        Box::new(Self {
            on_stack: false,
            ready: AtomicBool::new(false),
            msg: UnsafeCell::new(None),
        })
    }

    /// Creates a packet on the stack, containing a message.
    fn message_on_stack(msg: T) -> Self {
        Self {
            on_stack: true,
            ready: AtomicBool::new(false),
            msg: UnsafeCell::new(Some(msg)),
        }
    }

    /// Waits until the packet becomes ready for reading or writing
    fn wait_ready(&self) {
        while !self.ready.load(Acquire) {}
    }
}

/// Inner representation of a zero-capacity channel.
struct Inner {
    /// Senders waiting to pair up with a receive operation.
    senders: Waker,

    /// Receivers waiting to pair up with a send operation.
    receivers: Waker,

    /// Equals `true` when channel is disconnected.
    is_disconnected: bool,
}

/// Zero-capacity channel.
pub struct Channel<T> {
    /// Inner representation of the channel.
    inner: Spinlock<Inner>,

    senders: AtomicSerialWaker,
    #[allow(dead_code)]
    to_me: Arc<AtomicUsize>,
    receivers: AtomicSerialWaker,
    from_me: Arc<AtomicUsize>,
    first: bool,

    /// Indicates that dropping a `Channel<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    /// Constructs a new zero-capacity channel.
    pub fn new() -> Self {
        let senders = AtomicSerialWaker::new();
        let to_me = senders.from_me.clone();
        let receivers = AtomicSerialWaker::new();
        let from_me = receivers.from_me.clone();
        Self {
            inner: Spinlock::new(Inner {
                senders: Waker::new(),
                receivers: Waker::new(),
                is_disconnected: false,
            }),
            senders,
            to_me,
            receivers,
            from_me,
            first: true,
            _marker: PhantomData,
        }
    }

    /// Writes a message into the packet.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn write(&self, zero: &mut Zero, msg: T) -> Result<(), T> {
        // If there is no packet, the channel is disconnected.
        if *zero == 0 {
            return Err(msg);
        }

        self.receivers.wake();
        let packet = &*(*zero as *const Packet<T>);
        packet.msg.get().write(Some(msg));
        packet.ready.store(true, Release);
        Ok(())
    }

    /// Reads a message from the packet.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn read(&self, zero: &mut Zero) -> Result<T, ()> {
        // If there is no packet, the channel is disconnected.
        if *zero == 0 {
            return Err(());
        }

        let packet = &*(*zero as *const Packet<T>);
        self.senders.wake();
        if packet.on_stack {
            // The message has been in the packet from the beginning, so there is no need to wait
            // for it. However, after reading the message, we need to set `ready` to true in
            // order to signal that the packet can be destroyed.
            let msg = packet.msg.get().replace(None).unwrap();
            packet.ready.store(true, Release);
            Ok(msg)
        } else {
            // Wait until the message becomes available, then read it and destroy the
            // heap-allocated packet.
            packet.wait_ready();
            let msg = packet.msg.get().replace(None).unwrap();
            drop(Box::from_raw(packet as *const Packet<T> as *mut Packet<T>));
            Ok(msg)
        }
    }

    /// Attempts to send a message into the channel.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        let zero = &mut Zero::default();
        let mut inner = self.inner.lock();

        // If there's a waiting receiver, pair up with it.
        if let Some(operation) = inner.receivers.try_select() {
            *zero = operation.packet;
            drop(inner);
            unsafe {
                self.write(zero, msg).ok().unwrap();
            }
            Ok(())
        } else if inner.is_disconnected {
            Err(TrySendError {
                kind: ErrorKind::Closed,
                value: msg,
            })
        } else {
            Err(TrySendError {
                kind: ErrorKind::NoCapacity,
                value: msg,
            })
        }
    }

    /// Send a message into the channel.
    pub fn _send(&self, msg: T) -> Result<(), SendTimeoutError<T>> {
        let zero = &mut Zero::default();
        let mut inner = self.inner.lock();

        // If there's a waiting receiver, pair up with it.
        if let Some(operation) = inner.receivers.try_select() {
            *zero = operation.packet;
            drop(inner);
            unsafe {
                self.write(zero, msg).ok().unwrap();
            }
            return Ok(());
        }

        if inner.is_disconnected {
            return Err(SendTimeoutError::Disconnected(msg));
        }

        Context::with(|cx| {
            // Prepare for blocking until a receiver wakes us up.
            let oper = Operation::hook(zero);
            let packet = Packet::<T>::message_on_stack(msg);
            inner
                .senders
                .register_with_packet(oper, &packet as *const Packet<T> as usize, cx);
            self.receivers.wake();
            drop(inner);

            // Block the current thread.
            let sel = cx.wait();

            match sel {
                Selected::Waiting | Selected::Aborted => unreachable!(),
                Selected::Disconnected => {
                    self.inner.lock().senders.unregister(oper).unwrap();
                    let msg = unsafe { packet.msg.get().replace(None).unwrap() };
                    Err(SendTimeoutError::Disconnected(msg))
                }
                Selected::Operation(_) => {
                    // Wait until the message is read, then drop the packet.
                    packet.wait_ready();
                    Ok(())
                }
            }
        })
    }

    /// Attempts to receive a message without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let zero = &mut Zero::default();
        let mut inner = self.inner.lock();

        // If there's a waiting sender, pair up with it.
        if let Some(operation) = inner.senders.try_select() {
            *zero = operation.packet;
            drop(inner);
            unsafe { self.read(zero).map_err(|_| TryRecvError::Disconnected) }
        } else if inner.is_disconnected {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn recv(&self) -> Result<T, RecvTimeoutError> {
        let zero = &mut Zero::default();
        let mut inner = self.inner.lock();

        // If there's a waiting sender, pair up with it.
        if let Some(operation) = inner.senders.try_select() {
            *zero = operation.packet;
            drop(inner);
            unsafe {
                return self.read(zero).map_err(|_| RecvTimeoutError::Disconnected);
            }
        }

        if inner.is_disconnected {
            return Err(RecvTimeoutError::Disconnected);
        }
        Context::with(|cx| {
            // Prepare for blocking until a sender wakes us up.
            let oper = Operation::hook(zero);
            let packet = Packet::<T>::empty_on_stack();
            inner
                .receivers
                .register_with_packet(oper, &packet as *const Packet<T> as usize, cx);
            self.senders.wake();
            drop(inner);

            // Block the current thread.
            let sel = cx.wait();

            match sel {
                Selected::Aborted | Selected::Waiting => unreachable!(),
                Selected::Disconnected => {
                    self.inner.lock().receivers.unregister(oper).unwrap();
                    Err(RecvTimeoutError::Disconnected)
                }
                Selected::Operation(_) => {
                    // Wait until the message is provided, then read it.
                    packet.wait_ready();
                    unsafe { Ok(packet.msg.get().replace(None).unwrap()) }
                }
            }
        })
    }

    pub fn len(&self) -> usize {
        0
    }

    pub fn capacity(&self) -> Option<usize> {
        Some(0)
    }

    pub fn is_empty(&self) -> bool {
        true
    }

    pub fn is_full(&self) -> bool {
        true
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Shutdown for Channel<T> {
    fn shutdown(&self) -> bool {
        let mut inner = self.inner.lock();
        if !inner.is_disconnected {
            inner.is_disconnected = true;
            inner.senders.disconnect();
            inner.receivers.disconnect();
            true
        } else {
            false
        }
    }

    fn is_shutdown(&self) -> bool {
        let inner = self.inner.lock();
        inner.is_disconnected
    }
}

impl<T> Stream for Channel<T> {
    type Item = T;
    type Error = ();
    fn poll(&mut self) -> Poll<Option<T>, ()> {
        match self.try_recv() {
            Ok(t) => return Ok(Async::Ready(Some(t))),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(Async::Ready(None)),
        };
        if self.first {
            self.first = false;
            self.receivers.register();
            match self.recv() {
                Ok(t) => Ok(Async::Ready(Some(t))),
                Err(RecvTimeoutError::Timeout) => Ok(Async::NotReady),
                Err(_) => Ok(Async::Ready(None)),
            }
        } else if positive_update(&self.from_me) {
            self.receivers.register();
            match self.recv() {
                Ok(t) => Ok(Async::Ready(Some(t))),
                Err(RecvTimeoutError::Timeout) => Ok(Async::NotReady),
                Err(_) => Ok(Async::Ready(None)),
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
            Err(e) if e.is_full() => {
                self.senders.register();
                match self._send(e.into_inner()) {
                    Ok(()) => Ok(AsyncSink::Ready),
                    Err(_) => Err(SendError),
                }
            }
            Err(_) => unreachable!(),
        }
    }

    #[inline]
    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}
