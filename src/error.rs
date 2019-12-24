use crossbeam::crossbeam_channel::{SendError as SE, TrySendError as TSE};
use std::fmt;

#[derive(Debug)]
pub struct SendError;

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum SendTimeoutError<T> {
    /// The message could not be sent because the channel is full and the operation timed out.
    ///
    /// If this is a zero-capacity channel, then the error indicates that there was no receiver
    /// available to receive the message and the operation timed out.
    Timeout(T),

    /// The message could not be sent because the channel is disconnected.
    Disconnected(T),
}

impl<T> fmt::Debug for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        "SendTimeoutError(...)".fmt(f)
    }
}

impl<T> fmt::Display for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            SendTimeoutError::Timeout(..) => "timed out waiting on send operation".fmt(f),
            SendTimeoutError::Disconnected(..) => "sending on a disconnected channel".fmt(f),
        }
    }
}

impl<T: Send> std::error::Error for SendTimeoutError<T> {
    fn description(&self) -> &str {
        "sending on an empty and disconnected channel"
    }
    fn cause(&self) -> Option<&dyn std::error::Error> {
        None
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

#[derive(Debug)]
pub struct TrySendError<T> {
    pub(crate) kind: ErrorKind,
    pub(crate) value: T,
}

#[derive(Debug)]
pub enum ErrorKind {
    Closed,
    NoCapacity,
}

impl fmt::Display for SendError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "channel closed")
    }
}
impl std::error::Error for SendError {}

impl<T> TrySendError<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.value
    }
    #[inline]
    pub fn is_closed(&self) -> bool {
        if let ErrorKind::Closed = self.kind {
            true
        } else {
            false
        }
    }
    #[inline]
    pub fn is_full(&self) -> bool {
        if let ErrorKind::NoCapacity = self.kind {
            true
        } else {
            false
        }
    }
}

impl<T: fmt::Debug> fmt::Display for TrySendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let descr = match self.kind {
            ErrorKind::Closed => "channel closed",
            ErrorKind::NoCapacity => "no available capacity",
        };
        write!(fmt, "{}", descr)
    }
}
impl<T: fmt::Debug> std::error::Error for TrySendError<T> {}

impl<T> From<TSE<T>> for TrySendError<T> {
    fn from(err: TSE<T>) -> TrySendError<T> {
        match err {
            TSE::Full(i) => TrySendError {
                value: i,
                kind: ErrorKind::NoCapacity,
            },
            TSE::Disconnected(i) => TrySendError {
                value: i,
                kind: ErrorKind::Closed,
            },
        }
    }
}

impl<T> From<SE<T>> for TrySendError<T> {
    fn from(err: SE<T>) -> TrySendError<T> {
        TrySendError {
            kind: ErrorKind::Closed,
            value: err.into_inner(),
        }
    }
}
