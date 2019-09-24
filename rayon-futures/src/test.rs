#![cfg(test)]

use super::ScopeFutureExt;
//use futures::executor::Notify;

use futures::executor::block_on;
use futures::future::select;
use futures::{Future, FutureExt};

//use futures::sync::oneshot;

use futures;
use rayon_core::{scope, ThreadPool, ThreadPoolBuilder};

/// Basic test of using futures to data on the stack frame.
#[test]
fn future_test() {
    let data = &[0, 1];

    // Here we call `block_on` on a select future, which will block at
    // least one thread. So we need a second thread to ensure no
    // deadlock.
    ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap()
        .install(|| {
            scope(|s| {
                let a = s.spawn_future(futures::future::ok::<_, ()>(&data[0]));
                let b = s.spawn_future(futures::future::ok::<_, ()>(&data[1]));
                let (item1, next) = block_on(select(a, b)).factor_first();
                let item1 = item1.unwrap();
                let item2 = block_on(next).unwrap();
                assert!(*item1 == 0 || *item1 == 1);
                assert!(*item2 == 1 - *item1);
            });
        });
}

/// Test using `map` on a Rayon future. The `map` closure is executed
/// for side-effects, and modifies the `data` variable that is owned
/// by enclosing stack frame.
#[test]
fn future_map() {
    let data = &mut ["Hello, ".to_string()];

    let data_as_mut = async { &mut data[0] };

    let mut future = None;
    scope(|s| {
        let a = s.spawn_future(data_as_mut);
        future = Some(s.spawn_future(a.map(|s| s.push_str("world!"))));
    });

    // future must have executed for the scope to have ended, even
    // though we never invoked `wait` to observe its result
    assert_eq!(data[0], "Hello, world!");
    assert!(future.is_some());
}

/// Test that we can create a future that returns an `&mut` to data,
/// so long as it outlives the scope.
#[test]
fn future_escape_ref() {
    let data = &mut ["Hello, ".to_string()];

    {
        let mut future = None;
        scope(|s| {
            future = Some(s.spawn_future(async { &mut data[0] }));
        });
        let s = block_on(future.unwrap());
        s.push_str("world!");
    }

    assert_eq!(data[0], "Hello, world!");
}

#[test]
#[should_panic(expected = "Hello, world!")]
fn future_panic_prop() {
    scope(|s| {
        let future = s.spawn_future(async {
            panic!("Hello, world!");
        });
        block_on(future);
    });
}

/// Test that, even if we have only one thread, invoke `rayon_wait`
/// will not panic.
/* #[test]
fn future_rayon_wait_1_thread() {
    unimplemented!();
    /*
    // run with only 1 worker thread; this would deadlock if we couldn't make progress
    let mut result = None;
    ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
        .install(|| {
            scope(|s| {
                use std::sync::mpsc::channel;
                let (tx, rx) = channel();
                let a = s.spawn_future(lazy(move || Ok::<usize, ()>(rx.recv().unwrap())));
                //                          ^^^^ FIXME: why is this needed?
                let b = s.spawn_future(a.map(|v| v + 1));
                let c = s.spawn_future(b.map(|v| v + 1));
                s.spawn(move |_| tx.send(20).unwrap());
                result = Some(c.rayon_wait().unwrap());
            });
        });
    assert_eq!(result, Some(22));
    */
} */

/// Test that invoking `wait` on a `RayonFuture` will panic, if it is inside
/// a Rayon worker thread.
/* #[test]
#[should_panic]
fn future_wait_panics_inside_rayon_thread() {
    unimplemented!();
    /*
    scope(|s| {
        let future = s.spawn_future(lazy(move || Ok::<(), ()>(())));
        let _ = future.wait(); // should panic, not return a value
    });
    */
} */

/// Test that invoking `wait` on a `RayonFuture` will not panic if we
/// are outside a Rayon worker thread.
/* #[test]
fn future_wait_works_outside_rayon_threads() {
    unimplemented!();
    /*
    let mut future = None;
    scope(|s| {
        future = Some(s.spawn_future(lazy(move || Ok::<(), ()>(()))));
    });
    assert_eq!(Ok(()), future.unwrap().wait());
    */
} */

/// `scope` should panic if `Waker::wake` panics.
#[test]
#[should_panic(expected = "Hello, world!")]
fn panicy_waker() {
    use crate::futures::channel::oneshot;
    use crate::futures::task;
    use std::pin::Pin;
    use std::sync::Arc;

    // should reach end of scope but panic when scope returns.
    use std::panic;
    let mut reached_end_of_scope = false;

    struct PanicWake;
    impl task::ArcWake for PanicWake {
        fn wake_by_ref(_arc_self: &Arc<Self>) {
            panic!("Hello, world!");
        }
    }

    let arc_pw = Arc::new(PanicWake);
    let waker_pw = task::waker(arc_pw);

    use panic::AssertUnwindSafe;
    let scope_res = panic::catch_unwind(AssertUnwindSafe(|| {
        scope(|s| {
            let (tx, rx) = oneshot::channel::<i32>();
            let inner_future = async {
                let x = rx.await;
                assert_eq!(x.unwrap(), 22);
            };
            let mut outer_future = s.spawn_future(inner_future);
            // `spawn_future` executes eagerly.  Register the pancy waker as
            // waiting on the outer future.
            {
                let mut cx = task::Context::from_waker(&waker_pw);
                let p = (Pin::new(&mut outer_future)).poll(&mut cx);
                assert!(p.is_pending());
            }
            // Now wake the inner future
            tx.send(22).unwrap();
            // Should not panic before the end of the scope
            reached_end_of_scope = true;
        })
    }));
    assert!(reached_end_of_scope);
    panic::resume_unwind(scope_res.unwrap_err());
}

/* #[test]
fn double_unpark() {
    unimplemented!();
    /*
    let unpark0 = Arc::new(TrackUnpark {
        value: AtomicUsize::new(0),
    });
    let unpark1 = Arc::new(TrackUnpark {
        value: AtomicUsize::new(0),
    });
    let mut _tag = None;
    scope(|s| {
        let (a_tx, a_rx) = oneshot::channel::<u32>();
        let rf = s.spawn_future(a_rx);

        let mut spawn = task::spawn(rf);

        // test that we don't panic if people try to install a task many times;
        // even if they are different tasks
        for i in 0..22 {
            let u = if i % 2 == 0 {
                unpark0.clone()
            } else {
                unpark1.clone()
            };
            match spawn.poll_future_notify(&u, 0) {
                Ok(Async::NotReady) => {
                    // good, we expect not to be ready yet
                }
                r => panic!("spawn poll returned: {:?}", r),
            }
        }

        a_tx.send(22).unwrap();

        // just hold onto `rf` to ensure that nothing is cancelled
        _tag = Some(spawn.into_inner());
    });

    // Since scope is done, our spawned future must have completed. It
    // should have signalled the unpark value we gave it -- but
    // exactly once, even though we called `poll` many times.
    assert_eq!(unpark1.value.load(Ordering::SeqCst), 1);

    // unpark0 was not the last unpark supplied, so it will never be signalled
    assert_eq!(unpark0.value.load(Ordering::SeqCst), 0);

    struct TrackUnpark {
        value: AtomicUsize,
    }

    impl Notify for TrackUnpark {
        fn notify(&self, _: usize) {
            self.value.fetch_add(1, Ordering::SeqCst);
        }
    }
    */
} */

// Test two global-pool futures, one needing data from the other.
#[test]
fn global_future_map() {
    use std::sync::{Arc, Mutex};

    let data = Arc::new(Mutex::new("Hello, ".to_string()));

    let pool = ThreadPool::global();

    let outer_future_a;
    {
        let data = data.clone();
        outer_future_a = pool.spawn_future(async move { data });
    }

    let outer_future_b = pool.spawn_future(async move {
        let data = outer_future_a.await;
        let mut v = data.lock().unwrap();
        v.push_str("world!");
    });

    block_on(outer_future_b);

    assert_eq!(data.lock().unwrap().as_str(), "Hello, world!");
}

#[test]
#[should_panic(expected = "Hello, world!")]
fn async_future_panic_prop() {
    use std::panic::{self, AssertUnwindSafe};
    let inner_future = async {
        panic!("Hello, world!");
    };

    let pool = ThreadPool::global();
    let outer_future = pool.spawn_future(inner_future);

    let res = panic::catch_unwind(AssertUnwindSafe(|| {
        block_on(outer_future);
    }));

    panic::resume_unwind(res.err().unwrap());
}

#[test]
fn async_future_scope_interact() {
    let pool = ThreadPool::global();
    let future = pool.spawn_future(async { 22 });

    let mut vec = vec![];
    scope(|s| {
        let future = s.spawn_future(future.map(|x| x * 2));
        s.spawn(|_| {
            vec.push(block_on(future));
        }); // just because
    });

    assert_eq!(vec![44], vec);
}
