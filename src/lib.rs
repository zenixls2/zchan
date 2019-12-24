mod counter;
mod error;
pub use error::*;
mod atomic_serial_waker;
#[allow(dead_code)]
pub mod array;
pub mod list;
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
