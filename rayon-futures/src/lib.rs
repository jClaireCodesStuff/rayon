//! Future support in Rayon.
//!
//! See `README.md` for details.
#![deny(missing_debug_implementations)]
#![doc(html_root_url = "https://docs.rs/rayon-futures/0.1")]

// TODO: bare trait object
#![allow(bare_trait_objects)]

extern crate futures;
extern crate rayon_core;

use futures::future::CatchUnwind;
use futures::task::{Context, Waker, ArcWake};
use futures::{Future, Poll};
//use rayon_core::internal::worker; // May need `RUSTFLAGS='--cfg rayon_unstable'` to compile

//use futures::executor;
use rayon_core::internal::task::{ScopeHandle, Task as RayonTask, ToScopeHandle};
use std::any::Any;
use std::pin::Pin;
use std::fmt;
//use std::marker::PhantomData;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
//use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::atomic::Ordering::*;
use std::sync::Arc;
use std::sync::Mutex;

const STATE_PARKED: usize = 0;
const STATE_UNPARKED: usize = 1;
const STATE_EXECUTING: usize = 2;
const STATE_EXECUTING_UNPARKED: usize = 3;
const STATE_COMPLETE: usize = 4;

/// Attempt to change an `AtomicUsize` to a different state.
///
/// Loads the current state into `*local` and performs one of the following:
///
///  - only if the current state is equal to `*local` and `xchg_ordering` is satisfied
///    - atomically sets the state to `next`
///    - returns `true`
///
///  - otherwise only `load_ordering` is satisfied
///    - returns `false`
///
/// Panics if `load_ordering` is stronger than `xchg_ordering`
fn change_state(
    atom: &AtomicUsize,
    local: &mut usize,
    next: usize,
    xchg_ordering: Ordering,
    load_ordering: Ordering
) -> bool
{
    match atom.compare_exchange_weak(
        {*local},
        next,
        xchg_ordering,
        load_ordering
    ) {
        Ok(x) => { *local = x; true },
        Err(x) => { *local = x; false }
    }
}

pub trait ScopeFutureExt<'scope> {
    fn spawn_future<F>(&self, future: F) -> RayonFuture<F::Output>
    where
        F: Future + Send + 'scope;
}

impl<'scope, T> ScopeFutureExt<'scope> for T
where
    T: ToScopeHandle<'scope>,
{
    fn spawn_future<F>(&self, future: F) -> RayonFuture<F::Output>
    where
        F: Future + Send + 'scope,
    {
        let scope_future = ScopeFuture::spawn(future, self.to_scope_handle());
        let scope_future = erase_lifetime(scope_future);

        return RayonFuture { scope_future };

        // Because `ScopeFutureEscapeSafe<T>` contains the lifetimes of `T` and
        // can escape any other lifetime `'l`, that lifetime may be erased.
        fn erase_lifetime<'l, T>(
            x: Arc<ScopeFutureEscapeSafe<T> + 'l>,
        ) -> Arc<ScopeFutureEscapeSafe<T>> {
            unsafe {
                mem::transmute(x)
            }
        }
    }
}

/// Represents the result of a future that has been spawned in the
/// Rayon threadpool.
///
/// # Panic behavior
///
/// Any panics that occur while computing the spawned future will be
/// propagated when this future is polled.
pub struct RayonFuture<T> {
    scope_future: Arc<ScopeFutureEscapeSafe<Result<T, Box<Any + Send + 'static>>>>,
}

impl<T> Future for RayonFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<T> {
        use crate::Poll::*;
        if !self.scope_future.probe() {
            self.scope_future.set_waker_by_ref(cx.waker());
        }
        // Setting the `waker` doesn't race against other invocations of `poll`
        // because `&mut Self` is required.  It races against polling the inner
        // future, but this race is serialized by the contents mutex.
        //
        // Case A:
        //    `waker` is set before the inner future completes.  It can then be
        //    woken before `get_poll` takes the mutex.  A spurious wakeup can
        //    occur, but spurious wakeups are allowed.
        //
        // Case B:
        //    `waker` is set after the inner future completes.  A wakeup is
        //    missed.  Result is recorded (happens-before) waker is set
        //    (happens-before) `get_poll` below. The result is visible and no
        //    deadlock occurs.
        //
        // Case C:
        //    `probe` observes and synchronizes with `STATE_COMPLETE`.
        //    `complete` locks mutex (happens-before) setting `STATE_COMPLETE`
        //    (happens-before) `probe` (happens-before) `get_poll`
        match self.scope_future.get_poll() {
            Pending => Pending,
            Ready(Ok(x)) => Ready(x),
            Ready(Err(p)) => panic::resume_unwind(p)
        }
    }
}

impl<T> Drop for RayonFuture<T> {
    fn drop(&mut self) {
        self.scope_future.cancel();
    }
}

impl<T> fmt::Debug for RayonFuture<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("RayonFuture").finish()
    }
}
/// ////////////////////////////////////////////////////////////////////////
#[derive(Debug)]
struct ScopeFuture<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
    state: AtomicUsize,
    contents: Mutex<ScopeFutureContents<'scope, F, S>>,
}

type CU<F> = CatchUnwind<AssertUnwindSafe<F>>;
type CUOutput<F> = <CU<F> as Future>::Output;

struct ScopeFutureContents<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
    /* spawn: Option<Spawn<CU<F>>>, */
    //spawn: (), // TODO: equivalent
    inner_future: Option<CU<F>>,

    // Pointer to ourselves. We `None` this out when we are finished
    // executing, but it's convenient to keep around normally.
    this: Option<Arc<ScopeFuture<'scope, F, S>>>,

    // the counter in the scope; since the scope doesn't terminate until
    // counter reaches zero, and we hold a ref in this counter, we are
    // assured that this pointer remains valid
    scope: Option<S>,

    // waker to wake the outer task when the inner future completes.
    waker: Option<Waker>,

    result: Poll<CUOutput<F>>,

    canceled: bool,
}

impl<'scope, F, S> fmt::Debug for ScopeFutureContents<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("ScopeFutureContents").finish()
    }
}

// TODO: auto-impl Send + Sync for ScopeFuture
// Assert that the `*const` is safe to transmit between threads:
unsafe impl<'scope, F, S> Send for ScopeFuture<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
}
unsafe impl<'scope, F, S> Sync for ScopeFuture<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
}

impl<'scope, F, S> ScopeFuture<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
    fn spawn(future: F, scope: S) -> Arc<Self> {
        // Using `AssertUnwindSafe` is valid here because (a) the data
        // is `Send + Sync`, which is our usual boundary and (b)
        // panics will be propagated when the `RayonFuture` is polled.
        use futures::FutureExt;
        let inner_future = AssertUnwindSafe(future).catch_unwind();

        let outer_future: Arc<Self> = Arc::new(ScopeFuture::<F, S> {
            state: AtomicUsize::new(STATE_PARKED),
            contents: Mutex::new(ScopeFutureContents {
                inner_future: Some(inner_future),
                this: None,
                scope: Some(scope),
                waker: None,
                result: Poll::Pending,
                canceled: false,
            }),
        });

        // Become self-referential
        {
            let mut contents = outer_future.contents.try_lock().unwrap();
            contents.this = Some(outer_future.clone());
        }

        outer_future.unpark_inherent();

        outer_future
    }

    /// "Unpark" this `ScopeFuture`.  It enters the appropriate unparked state
    /// and schedules itself if needed.
    fn unpark_inherent(&self) {
        let mut loaded_state = self.state.load(Relaxed);
        loop {
            match loaded_state {
                STATE_PARKED => {
                    if {
                        change_state(
                            &self.state,
                            &mut loaded_state,
                            STATE_UNPARKED,
                            Release, Relaxed
                        )
                    } {
                        // Contention here is unlikely but possible: a
                        // previous execution might have moved us to the
                        // PARKED state but not yet released the lock.
                        let contents = self.contents.lock().unwrap();
                        let task_ref = contents.this.clone().expect("this-ref already dropped");

                        // We assert that `contents.scope` will be not
                        // be dropped until the task is executed. This
                        // is true because we only drop
                        // `contents.scope` from within `RayonTask::execute()`.
                        unsafe {
                            contents
                                .scope
                                .as_ref()
                                .expect("scope already dropped")
                                .spawn_task(task_ref);
                        }
                        return;
                    }
                }

                STATE_EXECUTING => {
                    if {
                        change_state(
                            &self.state,
                            &mut loaded_state,
                            STATE_EXECUTING_UNPARKED,
                            Release,
                            Relaxed,
                        )
                    } {
                        return;
                    }
                }

                state => {
                    debug_assert!(
                        state == STATE_UNPARKED
                            || state == STATE_EXECUTING_UNPARKED
                            || state == STATE_COMPLETE
                    );
                    return;
                }
            }
        }
    }

    fn begin_execute_state(&self) {
        // When we are put into the unparked state, we are enqueued in
        // a worker thread. We should then be executed exactly once,
        // at which point we transiition to STATE_EXECUTING. Nobody
        // should be contending with us to change the state here.
        //
        // TODO: determine the required ordering here.
        let prev = self.state.compare_exchange(STATE_UNPARKED, STATE_EXECUTING, AcqRel, Relaxed);
        debug_assert_eq!(prev, Ok(STATE_UNPARKED));
    }

    /// Attempt to exit STATE_EXECUTING.  Returns `false` if the inner future
    /// has been unparked and should be polled again.
    fn end_execute_state(&self) -> bool {
        let mut loaded_state = self.state.load(Relaxed);
        loop {
            match loaded_state {
                STATE_EXECUTING => {
                    if {
                        change_state(
                            &self.state,
                            &mut loaded_state,
                            STATE_PARKED,
                            Release, Relaxed
                        )
                    } {
                        // We put ourselves into parked state, no need to
                        // re-execute until woken.
                        return true;
                    }
                }

                state => {
                    debug_assert_eq!(state, STATE_EXECUTING_UNPARKED);
                    if {
                        change_state(
                            &self.state,
                            &mut loaded_state,
                            STATE_EXECUTING,
                            Release, Relaxed
                        )
                    } {
                        // We finished executing, but an unpark request
                        // came in the meantime.  We need to execute
                        // again. Return false as we failed to end the
                        // execution phase.
                        return false;
                    }
                }
            }
        }
    }
}

impl<'scope, F, S> ArcWake for ScopeFuture<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let this = &*arc_self;
        this.unpark_inherent()
    }
}

impl<'scope, F, S> RayonTask for ScopeFuture<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{

    fn execute(this: Arc<Self>) {
        use crate::Poll::*;
        // Futures-0.3 requires creating a `Context` before polling async code.
        let waker = futures::task::waker(this.clone());
        let mut cx = Context::from_waker(&waker);

        let mut parked = true;
        loop {
            // Taking the contents mutex serializes with other operations that
            // access it such as cancellation or the outer future being polled.
            let mut contents = this.contents.lock().unwrap();
            if parked {
                this.begin_execute_state();
                parked = false;
            }

            // Then check for cancellation.
            if contents.canceled {
                contents.complete(Pending);
                return
            }

            match contents.poll_inner(&mut cx) {
                Pending => {
                    if this.end_execute_state() {
                        return;
                    }
                },
                polled_complete => {
                    contents.complete(polled_complete);
                    return;
                }
            }
        }
    }
    /* fn execute(this: Arc<Self>) {
        unimplemented!();
        /*
        // *generally speaking* there should be no contention for the
        // lock, but it is possible -- we can end execution, get re-enqeueud,
        // and re-executed, before we have time to return from this fn
        let mut contents = this.contents.lock().unwrap();

        this.begin_execute_state();
        loop {
            if contents.canceled {
                return contents.complete(Ok(Async::NotReady));
            } else {
                match contents.poll() {
                    Ok(Async::Ready(v)) => {
                        return contents.complete(Ok(Async::Ready(v)));
                    }
                    Ok(Async::NotReady) => {
                        if this.end_execute_state() {
                            return;
                        }
                    }
                    Err(err) => {
                        return contents.complete(Err(err));
                    }
                }
            }
        }
        */
    }*/
}

impl<'scope, F, S> ScopeFutureContents<'scope, F, S>
where
    F: Future + Send + 'scope,
    S: ScopeHandle<'scope>,
{
    fn poll_inner(&mut self, cx: &mut Context) -> Poll<CUOutput<F>> {
        // UNSAFE: `ScopeFutureContents` is Arc-boxed, which
        // satisfies `Pin` even if the inner future is `!Unpin`.
        let inner_future = unsafe {
            Pin::new_unchecked(self.inner_future.as_mut().expect("inner future already dropped"))
        };
        inner_future.poll(cx)
        //unimplemented!();
        /*
        let notify = self.this.as_ref().unwrap();
        self.spawn.as_mut().unwrap().poll_future_notify(notify, 0)
        */
    }

    fn complete(&mut self, value: Poll<CUOutput<F>>) {
        // UNSAFE: Consuming `self.scope` allows the lifetime `'scope` to end,
        // and would allow `inner_future` to dangle, so it is dropped first.
        drop(self.inner_future.take().unwrap());
        self.result = value;
        let this = self.this.take().unwrap();

        if cfg!(debug_assertions) {
            let state = this.state.load(Relaxed);
            debug_assert!(
                state == STATE_EXECUTING || state == STATE_EXECUTING_UNPARKED,
                "cannot complete when not executing (state = {})",
                state
            );
        }
        this.state.store(STATE_COMPLETE, Release);

        // PANIC: The outer waker is arbitrary user code
        use crate::panic::AssertUnwindSafe as AUS;
        use crate::panic::catch_unwind;
        let waker = self.waker.take();
        let waker_catch = waker.and_then(|w| {
            catch_unwind(AUS(|| w.wake())).err()
        } );

        // TODO: panic propagation
        // Previous implementation propagates a waker panic to the Scope,
        // inner future panic to the outer future.
        let scope = self.scope.take().expect("scope already dropped");
        if let Some(p) = waker_catch {
            scope.panicked(p)
        } else {
            scope.ok()
        }

        /*
        // So, this is subtle. We know that the type `F` may have some
        // data which is only valid until the end of the scope, and we
        // also know that the scope doesn't end until `self.counter`
        // is decremented below. So we want to be sure to drop
        // `self.future` first, lest its dtor try to access some of
        // that state or something!
        mem::drop(self.spawn.take().unwrap());

        self.result = value;
        let this = self.this.take().unwrap();
        if cfg!(debug_assertions) {
            let state = this.0.state.load(Relaxed);
            debug_assert!(
                state == STATE_EXECUTING || state == STATE_EXECUTING_UNPARKED,
                "cannot complete when not executing (state = {})",
                state
            );
        }
        this.0.state.store(STATE_COMPLETE, Release);

        // `notify()` here is arbitrary user-code, so it may well
        // panic. We try to capture that panic and forward it
        // somewhere useful if we can.
        let mut err = None;
        if let Some(waiting_task) = self.waiting_task.take() {
            match panic::catch_unwind(AssertUnwindSafe(|| waiting_task.notify())) {
                Ok(()) => {}
                Err(e) => {
                    err = Some(e);
                }
            }
        }

        // Allow the enclosing scope to end. Asserts that
        // `self.counter` is still valid, which we know because caller
        // to `new_rayon_future()` ensures it for us.
        let scope = self.scope.take().unwrap();
        if let Some(err) = err {
            scope.panicked(err);
        } else {
            scope.ok();
        }
        */
    }
}

/// Methods of `ScopeFuture` which remain safe to call even if `'scope` is escaped.
///
/// UNSAFE: Implementations of these methods may not access `F`, the inner future.
unsafe trait ScopeFutureEscapeSafe<T>: Send + Sync {
    /// Returns true if the future has entered the COMPLETE state.
    ///
    /// Synchronizes with that state transition.
    fn probe(&self) -> bool;

    /// Get the most recent `Poll` result of the inner future F.  The result of
    /// polling after this method has returned `Ready` is unspecified but safe.
    fn get_poll(&self) -> Poll<T>;

    /// Advises that the outer future has been dropped.
    fn cancel(&self);

    /// Set the Waker that will be activated when the inner future completes or
    /// panics.
    fn set_waker_by_ref(&self, _: &Waker);
}

unsafe impl<'scope, F, S> ScopeFutureEscapeSafe<CUOutput<F>> for ScopeFuture<'scope, F, S>
where
    F: Future + Send,
    S: ScopeHandle<'scope>,
{
    fn probe(&self) -> bool {
        self.state.load(Acquire) == STATE_COMPLETE
    }

    fn get_poll(&self) -> Poll<CUOutput<F>> {
        // UNSAFE: the Future `F` must not be polled or otherwise borrowed here
        // because this method may be called from outside `'scope`.
        let mut contents = self.contents.lock().unwrap();
        let poll_result = mem::replace(&mut contents.result, Poll::Pending);

        // Detect being called after the end.  Acquiring the contents mutex
        // ensures that STATE_COMPLETE will be observable because it is stored
        // with that mutex held.  However, returning `PollPending` is the behavior
        // of `futures::future::Fuse`, so this doesn't seem necessary.
        //
        // if poll_result == Poll::Pending && self.state.load(Relaxed) == STATE_COMPLETE {
        //    panic!("`RayonFuture` was polled after finishing.");
        // }

        poll_result
    }

    fn cancel(&self) {
        // Fast-path: check if this is already complete and return if
        // so. A relaxed load suffices since we are not going to
        // access any data as a result of this action.
        if self.state.load(Relaxed) == STATE_COMPLETE {
            return;
        }

        // Slow-path. Get the lock and set the canceled flag to
        // true.

        let mut contents = self.contents.lock().unwrap();
        contents.canceled = true;

        // Unpark if `contents.this` is still available.

        if let Some(ref u) = contents.this {
            u.unpark_inherent();
        }

        /*
        // If the `this` we grabbed was not `None`, then notify it.
        // This will schedule the future.
        if let Some(ref u) = contents.this {
            
        }*/
        
    }

    fn set_waker_by_ref(&self, w: &Waker) {
        let mut contents = self.contents.lock().unwrap();
        // Don't clone if `Waker::will_wake`
        let old = contents.waker.as_ref();
        if old.map(|old| old.will_wake(w)) == Some(true) {
            return;
        }
        // else replace
        let new = w.clone();
        contents.waker = Some(new);
    }
}

mod compile_fail;
mod test;
