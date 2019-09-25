/*! ```compile_fail,0382,0501,0503,0596,0597

extern crate futures;
extern crate rayon_core;
extern crate rayon_futures;

use futures::future::lazy;
use rayon_futures::ScopeFutureExt;

fn a() {
    let data = &mut [format!("Hello, ")];

    let mut future = None;
    rayon_core::scope(|s| {
        let data = &mut *data;
        // E0597 `data` does not live long enough.  It is captured by the
        // future but does not outlive the invocation of `scope`
        future = Some(s.spawn_future(async { &mut data[0] })); //~ ERROR;
    });

    // E0503 `*data` is mutably borrowed.  It is visible in the output type of
    // the outer future, which will be dropped at the end of the function.
    //
    // E0501 `data[_]` was borrowed uniquely by closure passed to scope.
    assert_eq!(data[0], "Hello, world!"); //~ ERROR
}

fn b() {
    let data = &mut [format!("Hello, ")];

    let mut future = None;
    rayon_core::scope(|s| {
        future = Some(s.spawn_future(async move { &mut data[0] } ));
    });

    // E0382 borrow of moved value.  `data` was moved to satisfy the `async
    // move` declaration
    assert_eq!(data[0], "Hello, world!"); //~ ERROR
}

fn c() {
    let mut future = None;
    // borrowed value does not live long enough
    let data_store = [format!("Hello, ")];
    let data = &mut data_store;
    rayon_core::scope(|s| {
        // E0597 `data` does not live long enough`.  `data_store` is dropped
        // before `future` (LIFO) but `future` may hold a reference to
        // `data_store`.  This appears to be propagated to `data`, thus the
        // error.
        future = Some(s.spawn_future(async { &mut data[0] } )); //~ ERROR
    });
}

fn main() { }

``` */
