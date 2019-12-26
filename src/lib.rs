mod counter;
mod error;
pub use error::*;
pub mod array;
mod atomic_serial_waker;
#[allow(dead_code)]
mod context;
pub mod list;
mod select;
#[allow(dead_code)]
mod utils;
#[allow(dead_code)]
mod waker;
pub mod zero;
pub use atomic_serial_waker::*;
mod channel;
pub use channel::*;

pub trait Shutdown {
    fn shutdown(&self) -> bool;
    fn is_shutdown(&self) -> bool;
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
