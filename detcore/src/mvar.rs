/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Provides Mvars. Mvars are multiple-write variables. MVars begin in an
//! "empty" state and can be filled at runtime with a value. They are similar,
//! but not identical, to a channel of length 1.
//!
//! See "IStructures - Data Structures for Parallel Computing", Nikhil, 1989
//!
//! See also the [Haskell implementation of MVars][haskell-mvars], which is
//! directly analogous. The functions in this module share (roughly) the same
//! names.
//!
//! [haskell-mvars]: http://hackage.haskell.org/package/base/docs/Control-Concurrent-MVar.html

use std::fmt::Debug;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use futures::future::Future;
use futures::task::Context;
use futures::task::Poll;
use futures::task::Waker;

/// A Mvar is a synchronized variable that may be empty, or contain one value.
///
/// It's basically a concurrent Option<T> supporting blocking reads and writes.
/// It is also similar to a one-element synchronous channel, but without a
/// separation between the send and receive ends, and with "peek" as well as pop
/// operations.
///
/// Each new Mvar starts in a None state. Upon filling the Mvar with `put`, any
/// blocked readers will wake up and receive a value. Conversely, upon emptying
/// the Mvar with `take`, one blocked writers will wake up and fill the value.
#[derive(Debug, Clone)]
pub struct Mvar<T> {
    // TODO: replace this with a lock-free implementation.
    // This is an initial, primitive implementation with a coarse-grain lock.
    inner: Arc<Mutex<Shared<T>>>,
}

#[derive(Debug)]
struct Shared<T> {
    contents: Option<T>,
    // These will nondestructively read, and can all run together.
    read_waiters: Vec<Waker>,
    // These are waiting to destructively read, so only one can go at at time.
    take_waiters: Vec<Waker>,
    put_waiters: Vec<Waker>,
}

/// Internal struct for implementing distinct Futures instances.
struct ReadResult<T>(Mvar<T>);
struct TakeResult<T>(Mvar<T>);
struct PutResult<T>(Mvar<T>, T);

/// Helper function. Wake up the appropriate waiting tasks when a put occurs.
fn wake_on_put<T>(shared: &mut Shared<T>) {
    let towake = mem::take(&mut shared.read_waiters);
    for w in towake {
        w.wake();
    }
    match shared.take_waiters.pop() {
        None => {}
        Some(w) => w.wake(),
    }
}

impl<T: Debug> Default for Mvar<T> {
    fn default() -> Self {
        Mvar::new()
    }
}

impl<T: Debug> Mvar<T> {
    /// Allocate a new, empty Mvar.
    pub fn new() -> Mvar<T> {
        Mvar {
            inner: Arc::new(Mutex::new(Shared {
                contents: None,
                read_waiters: Vec::new(),
                take_waiters: Vec::new(),
                put_waiters: Vec::new(),
            })),
        }
    }

    /// A non-blocking version of take, which returns `None` if the MVar is
    /// empty.
    #[allow(unused)]
    pub fn try_take(&self) -> Option<T> {
        let mut shared = self.inner.lock().expect("Mvar try_read could not lock.");
        let contents = mem::take(&mut shared.contents);
        match contents {
            None => None,
            Some(v) => {
                // Wake ONE pending put task
                match shared.put_waiters.pop() {
                    None => {}
                    Some(w) => w.wake(),
                };
                Some(v)
            }
        }
    }

    /// A non-blocking version of the put operation. It will succeed if it finds
    /// the Mvar in an empty state, and it will fail and return `false` if the
    /// Mvar is already full.
    pub fn try_put(&self, val: T) -> bool {
        let mut shared = self.inner.lock().expect("Mvar try_put could not lock.");
        match &shared.contents {
            None => {
                // TODO: can we get rid of the clone here?:
                shared.contents = Some(val);
                wake_on_put(&mut shared);
                true
            }
            Some(_) => false,
        }
    }
    /*
    /// Wait until the MVar is full, then atomically swap a new value for the old
    /// one, returning the old value.
    ///
    /// (NB: this is different than Haskell's `swapMVar`, which is non-atomic.)
    pub async fn swap(&self, _val: T) -> T {
        panic!("FINISHME")
    }

    /// Non-blocking: Attempt to swap, and return `Ok(old)` with the old value if
    /// the swap succeeds. If the swap fails (due to the Mvar being empty), then
    /// `Err(new)` is returned, transferring owner of the input "new" value back
    /// to the caller.
    pub async fn try_swap(&self, _val: T) -> Result<T, T> {
        panic!("FINISHME")
    }
    */
}

impl<T: Debug + Clone> Mvar<T> {
    /// Blocking read that waits until the MVar is full, but clones rather than
    /// taking the value.
    #[allow(unused)]
    pub async fn read(&self) -> T {
        ReadResult(self.clone()).await
    }

    /// Nonblocking peek at the state of the Mvar right now (empty/filled).
    ///
    /// This is, in general, nondeterministic, because it may be racing with a
    /// put operation. Use with care.
    pub fn try_read(&self) -> Option<T> {
        let shared = self.inner.lock().expect("Mvar try_read could not lock.");
        shared.contents.clone()
    }

    /// Write a value into an (empty) Mvar. This is threadsafe, but it will block
    /// the current future if the Mvar is already full.
    #[allow(unused)]
    pub async fn put(&self, val: T) {
        PutResult(self.clone(), val).await
    }

    /// Block until the Mvar becomes full, and then remove and return its
    /// contents, leaving it in an empty stat again.
    #[allow(unused)]
    pub async fn take(&self) -> T {
        TakeResult(self.clone()).await
    }
}

impl<T: Debug + Clone> Future for ReadResult<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let inner = &self.0.inner;
        let mut shared = inner.lock().expect("Mvar ReadResult poll could not lock.");
        match &shared.contents {
            None => {
                shared.read_waiters.push(cx.waker().clone());
                Poll::Pending
            }
            Some(x) => Poll::Ready(x.clone()),
        }
    }
}

impl<T: Debug + Clone> Future for TakeResult<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let inner = &self.0.inner;
        let mut shared = inner.lock().expect("Mvar TakeResult poll could not lock.");
        let contents = mem::take(&mut shared.contents);
        match contents {
            None => {
                // Block until an item is available:
                shared.take_waiters.push(cx.waker().clone());
                Poll::Pending
            }
            Some(x) => {
                // Wake up ONE pending put. Waking more than one should work,
                // but would be wasteful.
                match shared.put_waiters.pop() {
                    None => {}
                    Some(w) => w.wake(),
                }
                Poll::Ready(x)
            }
        }
    }
}

impl<T: Debug + Clone> Future for PutResult<T> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let inner = &self.0.inner;
        let mut shared = inner.lock().expect("Mvar PutResult poll could not lock.");
        match &shared.contents {
            None => {
                // TODO: can we get rid of the clone here?
                shared.contents = Some(self.1.clone());
                wake_on_put(&mut shared);
                Poll::Ready(())
            }
            Some(_) => {
                // Already full, we need to block.
                shared.put_waiters.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::task;

    use super::*;

    #[tokio::test]
    async fn test_mvar_simple1() {
        let mv = Mvar::new();
        let e = mv.try_read();
        assert_eq!(e, None);
        mv.put(33).await;
        let x = mv.try_read();
        let y = mv.take().await;
        let z = mv.try_read();
        assert_eq!(x, Some(33));
        assert_eq!(y, 33);
        assert_eq!(z, None);
    }

    #[tokio::test]
    async fn test_mvar_as_channel() {
        let mv: Mvar<u64> = Mvar::new();
        let mut jhs = Vec::new();
        for i in 0..10u64 {
            let m2 = mv.clone();
            let jh = tokio::spawn(async move {
                println!("Spawned child, write Mvar: {}", i);
                m2.put(i).await;
                println!("Spawned child, write done writing {}.", i);
            });
            jhs.push(jh);
        }
        let mut results = Vec::new();
        for i in 0..10u64 {
            println!("Parent blocking take #{}", i);
            let x = mv.take().await;
            println!("Parent read {}", x);
            results.push(x);
        }
        for jh in jhs {
            jh.await.expect("Join failed");
        }
        results.sort();
        let expected: Vec<u64> = (0..10).collect();
        assert_eq!(results, expected);
    }

    #[tokio::test]
    async fn test_mvar_as_channel_concurrent_readers() {
        let mv: Mvar<u64> = Mvar::new();
        let mut jhs = Vec::new();
        let num_threads: u64 = 10;
        for i in 0..num_threads {
            let m2 = mv.clone();
            let jh = tokio::spawn(async move {
                println!("Spawned child #{}, write Mvar...", i);
                m2.put(i).await;
                println!("Spawned child #{}, done writing.", i);
            });
            jhs.push(jh);
        }

        let mut jhs2 = Vec::new();
        for i in 0..num_threads {
            let m2 = mv.clone();
            let jh = tokio::spawn(async move {
                println!("Spawned child #{}, read Mvar...", i);
                let x = m2.take().await;
                println!("Spawned child #{}, read {}.", i, x);
                x
            });
            jhs2.push(jh);
        }

        let mut results = Vec::new();
        for jh in jhs2 {
            results.push(jh.await.expect("join failed"));
        }
        results.sort();
        let expected: Vec<u64> = (0..10).collect();
        assert_eq!(results, expected);
    }

    /// Use the MVar as a channel to send multiple values between a single
    /// producer and single consumer.
    #[tokio::test]
    async fn test_mvar_as_channel_spsc() {
        let mv: Mvar<u64> = Mvar::new();
        let num_elems: u64 = 10;
        let m2 = mv.clone();
        let jh1 = tokio::spawn(async move {
            for i in 0..num_elems {
                m2.put(i).await;
            }
        });
        let m3 = mv.clone();
        let jh2 = tokio::spawn(async move {
            let mut results = Vec::new();
            for _ in 0..num_elems {
                results.push(m3.take().await);
            }
            results.sort();
            let expected: Vec<u64> = (0..10).collect();
            assert_eq!(results, expected);
        });
        jh1.await.unwrap();
        jh2.await.unwrap();
    }

    /// Non-blocking version that yields until it makes progress on both producer
    /// and consumer side.
    #[tokio::test]
    async fn test_mvar_as_channel_try_spsc() {
        let mv: Mvar<u64> = Mvar::new();
        let num_elems: u64 = 10;
        let m2 = mv.clone();
        let jh1 = tokio::spawn(async move {
            for i in 0..num_elems {
                loop {
                    if m2.try_put(i) {
                        break;
                    } else {
                        eprintln!("!! retrying try_put");
                        task::yield_now().await;
                    }
                }
            }
        });
        let m3 = mv.clone();
        let jh2 = tokio::spawn(async move {
            let mut results = Vec::new();
            for i in 0..num_elems {
                loop {
                    match m3.try_take() {
                        Some(v) => {
                            results.push(v);
                            break;
                        }
                        None => {
                            eprintln!("!! retrying try_take (#{})", i);
                            task::yield_now().await;
                        }
                    }
                }
            }
            results.sort();
            let expected: Vec<u64> = (0..10).collect();
            assert_eq!(results, expected);
        });
        jh1.await.unwrap();
        jh2.await.unwrap();
    }

    #[tokio::test]
    async fn test_mvar_concurrent_put_get1() {
        let mv = Mvar::new();
        let m2 = mv.clone();
        let jh = tokio::spawn(async move {
            println!("Spawned child, reading Mvar:");
            let x: u64 = m2.read().await;
            println!("Spawned child, read {}", x);
            assert_eq!(x, 33);
        });
        println!("Parent about to put to Mvar");
        mv.put(33).await;
        println!("Parent put complete");
        jh.await.expect("Join failed");
        println!("Parent: child joined");
    }

    #[tokio::test]
    async fn test_mvar_concurrent_put_get2() {
        let mv: Mvar<u64> = Mvar::new();
        let m2 = mv.clone();
        eprintln!("Starting test...");
        let jh = tokio::spawn(async move {
            println!("Child: putting Mvar:");
            mv.put(33).await;
            println!("Child: put complete");
        });
        eprintln!("Parent: about to read Mvar");
        let x: u64 = m2.read().await;
        eprintln!("Parent: read {} from Mvar", x);
        assert_eq!(x, 33);
        jh.await.expect("Join failed");
        eprintln!("Parent: child joined");
    }
}
