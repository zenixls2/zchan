//! Waking mechanism for threads blocked on channel operations.

use std::thread::{self, ThreadId};

use crate::context::Context;
use crate::select::{Operation, Selected};

/// Represents a thread blocked on a specific channel operation.
pub struct Entry {
    /// The operation.
    pub oper: Operation,

    /// Optional packet.
    pub packet: usize,

    /// Context associated with the thread owning this operation.
    pub cx: Context,
}

/// A queue of threads blocked on channel operations.
///
/// This data structure is used by threads to register blocking operations and get woken up once
/// an operation becomes ready.
pub struct Waker {
    /// A list of select operations.
    selectors: Vec<Entry>,
}

impl Waker {
    /// Creates a new `Waker`.
    #[inline]
    pub fn new() -> Self {
        Waker {
            selectors: Vec::new(),
        }
    }

    /// Registers a select operation.
    #[inline]
    pub fn register(&mut self, oper: Operation, cx: &Context) {
        self.register_with_packet(oper, 0, cx);
    }

    /// Registers a select operation and a packet.
    #[inline]
    pub fn register_with_packet(&mut self, oper: Operation, packet: usize, cx: &Context) {
        self.selectors.push(Entry {
            oper,
            packet,
            cx: cx.clone(),
        });
    }

    /// Unregisters a select operation.
    #[inline]
    pub fn unregister(&mut self, oper: Operation) -> Option<Entry> {
        for i in 0..self.selectors.len() {
            if self.selectors[i].oper == oper {
                return Some(self.selectors.remove(i));
            }
        }
        None
    }

    /// Attempts to find another thread's entry, select the operation, and wake it up.
    #[inline]
    pub fn try_select(&mut self) -> Option<Entry> {
        let mut entry = None;

        if !self.selectors.is_empty() {
            let thread_id = current_thread_id();

            for i in 0..self.selectors.len() {
                // Does the entry belong to a different thread?
                if self.selectors[i].cx.thread_id() != thread_id {
                    // Try selecting this operation.
                    let sel = Selected::Operation(self.selectors[i].oper);
                    let res = self.selectors[i].cx.try_select(sel);

                    if res.is_ok() {
                        // Provide the packet.
                        self.selectors[i].cx.store_packet(self.selectors[i].packet);
                        // Wake the thread up.
                        self.selectors[i].cx.unpark();

                        // Remove the entry from the queue to keep it clean and improve
                        // performance.
                        entry = Some(self.selectors.remove(i));
                        break;
                    }
                }
            }
        }

        entry
    }

    /// Returns `true` if there is an entry which can be selected by the current thread.
    #[inline]
    pub fn can_select(&self) -> bool {
        if self.selectors.is_empty() {
            false
        } else {
            let thread_id = current_thread_id();

            self.selectors.iter().any(|entry| {
                entry.cx.thread_id() != thread_id && entry.cx.selected() == Selected::Waiting
            })
        }
    }

    /// Notifies all registered operations that the channel is disconnected.
    #[inline]
    pub fn disconnect(&mut self) {
        for entry in self.selectors.iter() {
            if entry.cx.try_select(Selected::Disconnected).is_ok() {
                // Wake the thread up.
                //
                // Here we don't remove the entry from the queue. Registered threads must
                // unregister from the waker by themselves. They might also want to recover the
                // packet value and destroy it, if necessary.
                entry.cx.unpark();
            }
        }
    }
}

impl Drop for Waker {
    #[inline]
    fn drop(&mut self) {
        debug_assert_eq!(self.selectors.len(), 0);
    }
}

#[inline]
fn current_thread_id() -> ThreadId {
    thread_local! {
        /// Cached thread-local id.
        static THREAD_ID: ThreadId = thread::current().id();
    }

    THREAD_ID
        .try_with(|id| *id)
        .unwrap_or_else(|_| thread::current().id())
}
