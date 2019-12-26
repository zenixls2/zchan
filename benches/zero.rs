#![feature(test)]
extern crate test;
use futures::future;
use std::time::Instant;
use test::Bencher;
use tokio::prelude::*;
use tokio::runtime::Runtime;
use zchan::*;

#[bench]
fn test_zero(_b: &mut Bencher) {
    let mut rt = Runtime::new().unwrap();
    let (mut tx, rx) = zero();
    rt.spawn(future::lazy(move || {
        for i in 0..200000_i32 {
            tx = tx.send(i).wait().unwrap();
        }
        Ok(())
    }));
    let now = Instant::now();
    let task = rx.for_each(|_| Ok(())).map_err(|_| ());
    rt.block_on(task).unwrap();
    let elapsed = now.elapsed().as_nanos();
    println!("zero new: {}ns/iter, total {}ns", elapsed / 200000, elapsed);
}
