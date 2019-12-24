#![feature(test)]
extern crate test;
use futures::future;
use std::time::Instant;
use test::Bencher;
use tokio::prelude::*;
use tokio::runtime::Runtime;
use zchan::*;

#[bench]
fn test_unbounded(_b: &mut Bencher) {
    let mut rt = Runtime::new().unwrap();
    let (tx, rx) = unbounded();
    rt.spawn(future::lazy(move || {
        for i in 0..200000_i32 {
            tx.try_send(i).unwrap();
        }
        Ok(())
    }));
    let now = Instant::now();
    let task = rx.for_each(|_i| Ok(()));
    rt.spawn(task);

    rt.shutdown_on_idle().wait().unwrap();
    let elapsed = now.elapsed().as_nanos();
    println!(
        "unbounded new: {}ns/iter, total {}ns",
        elapsed / 200000,
        elapsed
    );
}
