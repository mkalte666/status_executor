#![warn(missing_docs)]
//!
//! Execute long running computational tasks, optionally transmitting a status back
//!
//! # Example usage
//! ```
//!     use status_executor::{StatusExecutor, StatusSender, StdContext};
//!     // status data need to be Send + 'static and also implement Clone
//!     #[derive(Debug,Clone)]
//!     struct MyStatus {
//!         progress: i32,
//!         msg: String,
//!     }
//!     // lets some heavy work instead of this
//!     let e = StatusExecutor::new(StdContext::default(), |s| {
//!         let mut p = 0;
//!         while p < 100 {
//!             std::thread::sleep(std::time::Duration::from_secs_f32(0.4));
//!             s.send(MyStatus {
//!                 progress: p,
//!                 msg: "Working...".to_string(),
//!             });
//!             p += 15;
//!         }
//!         // post that we are done!
//!         s.send(MyStatus {
//!             progress: p,
//!             msg: "Done!".to_string(),
//!         });
//!         p
//!     });
//!     // your gui or whatever might grab the most recent (or none, if none is available) status
//!     while !e.is_done() {
//!         match e.status() {
//!             Some(s) => println!("{} - currently at {}", s.msg, s.progress),
//!             None => std::thread::yield_now(),
//!         }
//!     }
//!     // the most recent status is also saved even if nothing new is produced.
//!     match e.latest_status() {
//!          Some(s) => println!("{} - last at {}", s.msg, s.progress),
//!          None => {
//!              assert!(false, "We produced *at least* one status, this should ont happen");
//!          }
//!     }
//!     // result is an Option<Arc<T>> because otherwise you would have to either move it out of the internal state
//!     // or track the locking used for the internal state
//!     //let res = e.result().expect("function returned is_done() == true, this should exist now");
//!     // you can, however just take the result as well, consuming the executor.
//!     let res = e.take_result();
//!     assert_eq!(res,Some(105));
//!
//!   ```
//! # Motivation
//! Sometimes the need arises to offload computation tasks.
//! Async does not handle this well, and tokio etc. give you specific contexts and thread pools to run computation tasks on instead.
//! And, as of writing this note, outside of async contexts, there isn't really anything to just ease these type of fire-off-once takes-forever tasks.
//!
//! status_executor provides two Structs - `Executor` and `StatusExecutor` for this task.
//! Executor itself is nothing but a overblown join handle. Using it becomes interesting if you enable the rayon feature.
//! Then you can call `Executor::new` using `RayonContext` instead, which makes the work spwan in rayons global pool instead.
//!
//! `StatusExecutor` is the name giving part. Say, you have a gui application, and you are doing *heavy* lifting in the background.
//! Users might be interested in the current state (it might, after all, take 20 minutes or so).
//! The obvious Solution is a channel. However, it becomes a bit tedious to write this rapidly.
//! So `StatusExecutor` is a little helper for that.
//!
//! # Panic behaviour
//! Threads running in an executor might panic
//! If the rayon feature is used, rayon does have a panic handler that will usually lead to an abort (unless you modify the thread pools panic handler)
//! Mirroring this, the `StdContext` will abort the process
//! Using `RayonGlobalContext` or `RayonContext` will instead just use the set panic handler - which might be a panic as well.
//!
use std::{
    panic,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex, MutexGuard,
    },
};

use internal_context::InternalData;

/// An `Executor` runs a function in some `Context` (this will usually be a thread)
///
/// This is similar to `std::thread::JoinHandle`, though it always will yield an `Arc<T>` instead
/// This allows the `Executor` to be stored itself, and accesses via `Executor::result()` are a cheap arc clone instead of requiring moves.
/// Of course, using `Executor::take()`, you can still grab the inner data if its available.
///
pub struct Executor<T> {
    state: Arc<Mutex<InternalData<T>>>,
}

impl<T> Executor<T>
where
    T: Send + Sync + 'static,
{
    /// Run `f` in the Execution context `_e`, creating an `Executor`
    ///
    /// # Arguments
    ///   * `e`: The context to run on. For `StdContext` or `RayonGlobalContext` you can just use `::default()`, they are stateless.
    ///   * `f`: The function to run. `f` must be `FnOnce + Send + 'static` - as if you would pass it to `std::thread::spawn`
    pub fn new<E, F>(e: E, f: F) -> Self
    where
        F: FnOnce() -> T + Send + 'static,
        E: Context,
    {
        let state = Arc::new(Mutex::new(InternalData { res: None }));

        e.execute(state.clone(), f);

        Self { state }
    }

    /// internal locking function for the stared state
    fn lock_state(&self) -> MutexGuard<'_, InternalData<T>> {
        match self.state.lock() {
            Ok(g) => g,
            Err(e) => {
                panic!("Cannot lock internal state of Executor. This means that the lock is poisoned, and *that*, in turn, means that the thread has paniced. Lock result: {}",e);
            }
        }
    }

    /// Returns true if the function passed into `Executor::new()` has completed
    pub fn is_done(&self) -> bool {
        self.lock_state().res.is_some()
    }

    /// Returns `Some(Arc<T>)` if the function passed into `Execute::new()` has completed, or `None` otherwise
    pub fn result(&self) -> Option<Arc<T>> {
        self.lock_state().res.clone()
    }

    /// Consumes this `Executor`, returning either `Some(T)` if the executor has completed, or `None` if it has not.
    /// <div class="warning">
    ///     There is no way for the work function if you you unsuccessfully tried to take the data. It will run to its end.
    ///     There is no way to retrieve this data as well.
    ///     So you probably only want to only `Executor::take()` if `Executor::is_done()` returnd true before.
    /// </div>
    pub fn take_result(self) -> Option<T> {
        match Arc::into_inner(self.state) {
            None => None,
            Some(mtx) => {
                let state = mtx
                    .into_inner()
                    .expect("Executor state was poisoned, probably due to panic in work thread.");
                state.res.map(|x| Arc::into_inner(x)).unwrap_or(None)
            }
        }
    }
}

/// A `StatusExecutor` runs a function in some `Context` (this will usually be a thread)
///
/// For the general behaviour, see `Executor`.
///
/// `StatusExecutor` additionally takes a Status `S` (which has to be `Send + 'static` for the underlying channel).
///  This status can then be updated from within the work function.
pub struct StatusExecutor<T, S> {
    state: Arc<Mutex<InternalData<T>>>,
    rx: Receiver<S>,
    last_status: Mutex<Option<S>>,
}

impl<T, S> StatusExecutor<T, S>
where
    T: Send + Sync + 'static,
    S: Send + Clone + 'static,
{
    /// Run `f` in the Execution context `_e`, creating an `Executor`
    ///
    /// # Arguments
    ///   * `e`: The context to run on. For `StdContext` or `RayonGlobalContext` you can just use `::default()`, they are stateless.
    ///   * `f`: The function to run. `f` must be `FnOnce + Send + 'static` - as if you would pass it to `std::thread::spawn`
    ///
    /// Additionally, `f` takes a `StatusSender<S>` that can be use to update the status.
    pub fn new<E, F>(e: E, f: F) -> Self
    where
        F: FnOnce(StatusSender<S>) -> T + Send + 'static,
        E: Context,
    {
        let state = Arc::new(Mutex::new(InternalData { res: None }));

        let (tx, rx) = channel();
        let sender = StatusSender { tx };

        e.execute(state.clone(), move || f(sender));

        Self {
            state,
            rx,
            last_status: Mutex::new(None),
        }
    }

    /// internal locking for the state
    fn lock_state(&self) -> MutexGuard<'_, InternalData<T>> {
        self.state.lock().expect("Cannot lock internal state of StatusExecutor. This means that the lock is poisoned, and *that*, in turn, means that the thread has panicked.")
    }

    /// internal locking for the persistant
    fn lock_last_status(&self) -> MutexGuard<'_, Option<S>> {
        self.last_status.lock().expect("Cannot lock internal last status of StatusExecutor. This means that the lock is poisoned, and *that*, in turn, means that the only owning thread has panicked by this point. I am unsure how one could get to this point.")
    }

    /// Returns true if the function passed into `StatusExecutor::new()` has completed
    pub fn is_done(&self) -> bool {
        self.lock_state().res.is_some()
    }

    /// Returns `Some(Arc<T>)` if the function passed into `StatusExecutor::new()` has completed, or `None` otherwise
    pub fn result(&self) -> Option<Arc<T>> {
        self.lock_state().res.clone()
    }

    /// Consumes this `Executor`, returning either `Some(T)` if the executor has completed, or `None` if it has not.
    /// <div class="warning">
    ///     There is no way to retrieve the data if you call this before the work function is done.
    ///     So you probably only want to only `Executor::take()` if `Executor::is_done()` returnd true before.
    /// </div>
    ///
    /// `StatusSender::send` will begin to return false if you take before the execution is done, so you could abort processing that way.
    pub fn take_result(self) -> Option<T> {
        match Arc::into_inner(self.state) {
            None => None,
            Some(mtx) => {
                let state = mtx.into_inner().expect(
                    "StatusExecutor state was poisoned, probably due to panic in work thread.",
                );
                state.res.map(|x| Arc::into_inner(x)).unwrap_or(None)
            }
        }
    }

    /// Returns a status `Some(S)` if there are any status infos pending, or `None` if there isn't anything new.
    pub fn status(&self) -> Option<S> {
        match self.rx.try_recv() {
            Ok(s) => {
                *self.lock_last_status() = Some(s.clone());
                Some(s)
            }
            _ => None,
        }
    }

    /// Checks for a new status and returns the latest one it has seen.
    /// Similar to `StatusExecutor::status`, but will always return something.
    ///
    /// Technically, since this is using try_iter, you could produce status faster than consuming them with this function
    /// Then it would block. However, for latest_status to be slower than a producer, the producer would basically have to call send() in an infinite loop
    /// That's not an intended use case, so its deemed an acceptable issue.
    pub fn latest_status(&self) -> Option<S> {
        let latest = self.rx.try_iter().last();
        match latest {
            Some(s) => {
                *self.lock_last_status() = Some(s.clone());
                Some(s)
            }
            None => self.lock_last_status().clone(),
        }
    }
}

/// A convenience struct around the channel used to send a status `S` from the worker to the `StatusExecutor`
pub struct StatusSender<S> {
    tx: Sender<S>,
}

impl<S> StatusSender<S>
where
    S: Send,
{
    /// Send `S` to the `StatusExecutor`
    /// this will return `true` as long as the underlying channel exists.
    /// You can use the return value to check if the `StatusExecutor` was dropped.
    pub fn send(&self, s: S) -> bool {
        match self.tx.send(s) {
            Ok(_) => true,
            _ => false,
        }
    }
}

/// Private types are wrapped in here for the Trait Sealing to work
mod internal_context {
    use std::sync::{Arc, Mutex};

    /// Tracks internal state
    /// Mostly useful because of the rayon feature - rayon gives no join handles.
    pub struct InternalData<T> {
        pub res: Option<Arc<T>>,
    }

    /// Executors must implement this
    pub trait InternalContext {
        /// run function `f*  on this context, using `state` to return data back to the `Executor` or `StatusExecutor`
        fn execute<T, F>(&self, state: Arc<Mutex<InternalData<T>>>, f: F)
        where
            T: Send + Sync + 'static,
            F: FnOnce() -> T + Send + 'static;
    }
}

/// Trait used for execution contexts
/// <div class="warning"> This is a Sealed trait, do not implement it yourself. </div>
/// Use `StdContext` or `RayonContext` directly instead.
pub trait Context: internal_context::InternalContext {}

/// An executor used to run functions on `std::thread::spawn()` instances.
/// You can just `Default` this one, right now it has no state
pub struct StdContext {
    _tag: (),
}

impl Default for StdContext {
    fn default() -> Self {
        Self { _tag: () }
    }
}

impl Context for StdContext {}

impl internal_context::InternalContext for StdContext {
    fn execute<T, F>(&self, state: Arc<Mutex<InternalData<T>>>, f: F)
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        // we need to hook into the unwind boundary because otherwise this will not abort.
        let res = panic::catch_unwind(panic::AssertUnwindSafe(move || {
            std::thread::spawn(move || {
                let r = f();
                state
                    .lock()
                    .expect("main thread panicked for this executor")
                    .res = Some(Arc::new(r));
            });
        }));
        match res {
            Err(_) => std::process::abort(),
            _ => {}
        }
    }
}

/// With feature "rayon", this module holds the implementations for `RayonGlobalContext` and `RayonContext`
#[cfg(feature = "rayon")]
pub mod rayon_context {
    use std::sync::{Arc, Mutex};

    use rayon::ThreadPool;

    use crate::internal_context::{InternalContext, InternalData};

    /// `RayonGlobalContext` runs things on rayons global executor. use `RayonGlobalContext::default()`.
    pub struct RayonGlobalContext {
        _tag: (),
    }

    impl Default for RayonGlobalContext {
        fn default() -> Self {
            Self { _tag: () }
        }
    }

    impl InternalContext for RayonGlobalContext {
        fn execute<T, F>(&self, state: Arc<Mutex<InternalData<T>>>, f: F)
        where
            T: Send + Sync + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            rayon::spawn(move || {
                let r = f();
                state
                    .lock()
                    .expect("main thread panicked for this executor")
                    .res = Some(Arc::new(r));
            });
        }
    }

    impl crate::Context for RayonGlobalContext {}

    /// `RayonContext` runs things on a rayon thread pool. Thin wrapper around a reference to `ThreadPool`.
    /// Its fine to just use `RayonExecutor::new()` every time.
    pub struct RayonContext<'a> {
        pool: &'a ThreadPool,
    }

    impl<'a> RayonContext<'a> {
        /// Create a
        pub fn new(pool: &'a ThreadPool) -> Self {
            Self { pool }
        }
    }

    impl InternalContext for RayonContext<'_> {
        fn execute<T, F>(&self, state: Arc<Mutex<InternalData<T>>>, f: F)
        where
            T: Send + Sync + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.pool.spawn(move || {
                let r = f();
                state
                    .lock()
                    .expect("main thread panicked for this executor")
                    .res = Some(Arc::new(r));
            });
        }
    }

    impl crate::Context for RayonContext<'_> {}
}

#[cfg(feature = "rayon")]
pub use rayon_context::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_std() {
        let e = Executor::new(StdContext::default(), move || 1234);
        // it must not take long for that one to run.
        while !e.is_done() {
            std::thread::yield_now();
        }

        assert_eq!(e.result().map(|x| *x), Some(1234));
        assert_eq!(e.take_result(), Some(1234));
    }

    #[test]
    fn status_executor_std() {
        let e = StatusExecutor::new(StdContext::default(), move |s| {
            s.send(432);
            s.send(999);
            s.send(4);
            1234
        });
        // it must not take long for that one to run.
        while !e.is_done() {
            std::thread::yield_now();
        }

        assert_eq!(e.status(), Some(432));
        assert_eq!(e.latest_status(), Some(4));

        assert_eq!(e.result().map(|x| *x), Some(1234));
        assert_eq!(e.take_result(), Some(1234));
    }
}