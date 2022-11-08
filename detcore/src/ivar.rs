// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Ivars are a write-once variable cell with blocking read.  Ivars begin in an "empty" state and
//! can be filled at runtime with a value.
//!
//! See "IStructures: Data Structures for Parallel Computing", Nikhil, 1989

use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Result;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use futures::future::Future;
use futures::task::Context;
use futures::task::Poll;
use futures::task::Waker;

/// An Ivar (or I-structure) is a monotonic, write-once data structure.
///
/// It's basically a concurrent Option<T> with blocking reads. Each new Ivar starts in a None state,
/// and can only be filled once.  Upon filling the Ivar with `put`, any blocked readers will wake up
/// and receive a value.
#[derive(Debug, Clone)]
pub struct Ivar<T> {
    // TODO: replace this with a lock-free implementation.
    // This is an initial, primitive implementation with a coarse-grain lock.
    inner: Arc<Mutex<Shared<T>>>,
}

#[derive(Debug)]
struct Shared<T> {
    contents: Option<T>,
    waiter: Option<Waker>,
}

/// Helper function. Wake up the appropriate waiting tasks when a put occurs.
fn wake_on_put<T>(shared: &mut Shared<T>) {
    let towake = mem::take(&mut shared.waiter);
    if let Some(w) = towake {
        w.wake();
    }
}

// #[allow(dead_code)]
impl<T: Debug> Ivar<T> {
    /// Allocate a new, empty Ivar.
    pub fn new() -> Ivar<T> {
        Ivar {
            inner: Arc::new(Mutex::new(Shared {
                contents: None,
                waiter: None,
            })),
        }
    }

    /// Allocate a new Ivar which is already filled.
    pub fn full(val: T) -> Ivar<T> {
        Ivar {
            inner: Arc::new(Mutex::new(Shared {
                contents: Some(val),
                waiter: None,
            })),
        }
    }

    /// Write a value into an (empty) Ivar.  This is threadsafe.  However, any attempt to write two
    /// values, either on the same thread or different ones, will result in a panic.
    ///
    /// Unlike puts, reads (with await) are idempotent and can happen as many times as desired on
    /// the same IVar.
    pub fn put(&self, val: T) {
        {
            let mut shared = self.inner.lock().expect("Ivar put could not lock.");
            if shared.contents.is_none() {
                // Normally this would be the linearization point for the put operation, i.e., CAS:
                shared.contents = Some(val);
                wake_on_put(&mut shared);
                return;
            }
        }
        panic!(
            "Ivar multiple put exception! Attempted to write {:?} to {}.",
            val, &self
        );
    }

    /// A non-blocking version of the put operation.  Returns `None` if the value was
    /// written, and gives back the value otherwise.
    ///
    /// This is generally RISKY if the intent is to use IVars in a deterministic paralel
    /// computation.  It is likely to violate the usual invariant with IVars that they are
    /// filled by a unique writing with a single, deterministic value.  Two racing
    /// `try_put` operations should thus only attempt to write the same value, if
    /// determinism is the intent.
    pub fn try_put(&self, val: T) -> Option<T> {
        let mut shared = self.inner.lock().expect("Ivar try_put could not lock.");
        match &shared.contents {
            None => {
                shared.contents = Some(val);
                wake_on_put(&mut shared);
                None
            }
            Some(_) => Some(val),
        }
    }
}

impl<T: Debug> Default for Ivar<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Equality on Ivars is pointer equality.
impl<T: Debug> PartialEq for Ivar<T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(Arc::as_ptr(&self.inner), Arc::as_ptr(&other.inner))
    }
}

impl<T: Debug> Display for Ivar<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.write_fmt(format_args!("<ivar {:p} ", Arc::as_ptr(&self.inner)))?;
        let shared = self.inner.lock().unwrap();
        match &shared.contents {
            Some(val) => {
                f.write_fmt(format_args!("{:?}", val))?;
            }
            None => {
                if shared.waiter.is_none() {
                    f.write_str("NoWaiter")?;
                } else {
                    f.write_str("HasWaiter")?;
                }
            }
        }
        f.write_str(">")
    }
}

impl<T: Debug + Clone> Ivar<T> {
    /// Nonblocking peek at the state of the Ivar right now (empty/filled).
    ///
    /// This is, in general, nondeterministic, because it may be racing with a put operation.
    /// Use with care.
    ///
    pub fn try_read(&self) -> Option<T> {
        let shared = self.inner.lock().expect("Ivar try_read could not lock.");
        shared.contents.clone()
    }
    /// This has the same effect as calling .await directly on the ivar, however it is provided for
    /// two reasons:
    ///
    ///   (1) symmetry with put,
    ///   (2) taking self by reference instead of consuming it
    ///
    pub async fn get(&self) -> T {
        let v: Ivar<T> = self.clone();
        v.await
    }
}

/// Await an Ivar to read its value.
impl<T: Debug + Clone> Future for Ivar<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut shared = self.inner.lock().expect("Ivar poll could not lock.");
        match &shared.contents {
            None => {
                // Store the most recent Waker only:
                shared.waiter = Some(cx.waker().clone());
                Poll::Pending
            }
            Some(x) => Poll::Ready(x.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ivar_simple1() {
        let iv = Ivar::new();
        let e = iv.try_read();
        assert_eq!(e, None);
        iv.put(33);
        let x = iv.clone().await;
        let z = iv.try_read();
        let y = iv.await;
        assert_eq!(x, 33);
        assert_eq!(y, 33);
        assert_eq!(z, Some(33));
    }

    #[tokio::test]
    #[should_panic]
    async fn test_ivar_multi_put() {
        let iv = Ivar::new();
        iv.put(33);
        iv.put(44);
    }

    #[tokio::test]
    async fn test_ivar_concurrent_put_get1() {
        let iv = Ivar::new();
        let i2 = iv.clone();
        let jh = tokio::spawn(async move {
            println!("Spawned child, reading ivar:");
            let x: u64 = i2.await;
            println!("Spawned child, read {}", x);
            assert_eq!(x, 33);
        });
        println!("Parent about to put to ivar");
        iv.put(33);
        println!("Parent put complete");
        jh.await.expect("Join failed");
        println!("Parent: child joined");
    }

    #[tokio::test]
    async fn test_ivar_concurrent_put_get2() {
        let iv = Ivar::new();
        let i2 = iv.clone();
        let jh = tokio::spawn(async move {
            println!("Child: putting ivar:");
            iv.put(33);
            println!("Child: put complete");
        });
        println!("Parent: about to read ivar");
        let x: u64 = i2.await;
        println!("Parent: read {} from ivar", x);
        jh.await.expect("Join failed");
        println!("Parent: child joined");
        assert_eq!(x, 33);
    }
}
