#![feature(test)]
extern crate test;
use futures::future;
use std::time::Instant;
use test::Bencher;
use tokio::prelude::*;
use tokio::runtime::Runtime;
use zchan::*;

#[bench]
fn test_bounded(_b: &mut Bencher) {
    let mut rt = Runtime::new().unwrap();
    let (mut tx, rx) = bounded(1000);
    rt.spawn(future::lazy(move || {
        for i in 0..200000_i32 {
            tx = tx.send(i).wait().unwrap();
        }
        Ok(())
    }));
    let now = Instant::now();
    let task = rx.take(200000).collect().then(|v| {
        assert_eq!(v.unwrap().len(), 200000);
        Ok(())
    });
    rt.spawn(task);

    rt.shutdown_on_idle().wait().unwrap();
    let elapsed = now.elapsed().as_nanos();
    println!(
        "bounded(1000) new: {}ns/iter, total {}ns",
        elapsed / 200000,
        elapsed
    );
}
