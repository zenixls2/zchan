# zchan
rewrite crossbeam-channel to support futures

This project aims to combine tokio (0.1.22) with crossbeam (0.7.3) to provide a faster channel.  
Benchmark result shown below:

### Transfering 200,000 i32 numbers, using unbounded channel
```
zchan:          85ns/iter, total 17075698ns
tokio-channel: 219ns/iter, total 43885005ns
```

### Transfering 200,000 i32 numbers using bounded(1000) channel
```
zchan:         121ns/iter, total 24294765ns
tokio-channel: 388ns/iter, total 77659237ns
```

### Transfering 200,000 i32 numbers using zero-capacity channel
```
zchan:         749ns/iter, total 149971960ns
```

to run the benchmark, you need to turn on the `nocapture` to see the messages.  

### Usage:
An example using unbounded channel:  
```rust
use futures::future;
use tokio::prelude::*;
use tokio::runtime::Runtime;
use zchan::*;

fn main() {
    let mut rt = Runtime::new().unwrap();
    let (tx, rx) = unbounded();
    rt.spawn(future::lazy(move || {
        for i in 0..2000_i32 {
            tx.try_send(i).unwrap();
        }
        Ok(())
    }));
    let task = rx.take(2000).collect().then(|v| {
        assert_eq!(v.unwrap().len(), 2000);
        Ok(())
    });
    rt.spawn(task);
    rt.shutdown_on_idle().wait().unwrap();
}
```

you could get more examples from the `benches` folder.

### Features:
- cloneable `Receiver/Sender`
- faster `Waker` that supports FIFO.

