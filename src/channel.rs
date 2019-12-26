use crate::array;
use crate::counter;
use crate::list;
use crate::zero as _zero;
pub type UnboundedSender<T> = counter::Sender<list::Channel<T>>;
pub type UnboundedReceiver<T> = counter::Receiver<list::Channel<T>>;

pub fn unbounded<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    counter::new(list::Channel::new())
}

pub type Sender<T> = counter::Sender<array::Channel<T>>;
pub type Receiver<T> = counter::Receiver<array::Channel<T>>;

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    counter::new(array::Channel::with_capacity(cap))
}

pub type ZeroSender<T> = counter::Sender<_zero::Channel<T>>;
pub type ZeroReceiver<T> = counter::Receiver<_zero::Channel<T>>;

pub fn zero<T>() -> (ZeroSender<T>, ZeroReceiver<T>) {
    counter::new(_zero::Channel::new())
}
