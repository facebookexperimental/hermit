/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

//! The thread that runs a library launch's ptrace session.
//!
//! The public launch functions run the tracer, Detcore's tools and their
//! runtime on this thread rather than on the caller's, so that a launch needs
//! no particular stack from its caller. A debug build of Detcore keeps every
//! syscall arm's temporaries in one frame, and the session needed about 2 MiB,
//! which is the whole of Rust's default for a spawned thread or a test
//! (<https://github.com/rrnewton/hermit/issues/4389>).
//!
//! Each calling thread gets one tracer thread, kept for that caller's
//! lifetime, so that an operation retained at a cleanup checkpoint stays with
//! the thread that owns it and the caller can resume it there. Ptrace binds a
//! tracee to the thread that attached it, so a whole session (spawn, waits,
//! teardown) stays on one tracer thread. The tracer thread exits with its
//! caller; a retained operation then cannot recover, as one whose original
//! thread exited could not before.

use std::cell::Cell;
use std::cell::RefCell;
use std::panic;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::ThreadId;

/// The tracer thread's stack: the 8 MiB a Linux main thread usually has
/// (the default `ulimit -s`), on which the CLI runs the same session.
pub(crate) const TRACER_STACK_BYTES: usize = 8 << 20;

type Job = Box<dyn FnOnce() + Send>;

// Set by [`launch_on_calling_thread`] for the whole process.
static ON_CALLER: AtomicBool = AtomicBool::new(false);

/// Makes every later launch in this process run its ptrace session on the
/// calling thread, as launches did before the tracer thread, and leaves the
/// caller's stack to the caller. For a process that must not gain threads: the
/// `hermit` CLI runs its launch inside a container child made with a raw
/// `clone`, whose entry point ends with a thread exit, so a tracer thread
/// still waiting for work would keep that child alive. The CLI launches from
/// its main thread or from the child's own stack, which have room.
pub fn launch_on_calling_thread() {
    ON_CALLER.store(true, Ordering::Relaxed);
}

fn in_place() -> bool {
    ON_TRACER.get() || ON_CALLER.load(Ordering::Relaxed)
}

struct TracerThread {
    jobs: mpsc::Sender<Job>,
    id: ThreadId,
}

thread_local! {
    // The calling thread's tracer thread. Dropping it with the caller closes
    // the channel, and the tracer thread returns.
    static TRACER: RefCell<Option<TracerThread>> = const { RefCell::new(None) };
    static ON_TRACER: Cell<bool> = const { Cell::new(false) };
}

fn spawn() -> std::io::Result<TracerThread> {
    let (jobs, receive) = mpsc::channel::<Job>();
    let handle = std::thread::Builder::new()
        .name("hermit-tracer".to_owned())
        .stack_size(TRACER_STACK_BYTES)
        .spawn(move || {
            ON_TRACER.set(true);
            for job in receive {
                job();
            }
        })?;
    Ok(TracerThread {
        jobs,
        id: handle.thread().id(),
    })
}

/// Runs `job` on the calling thread's tracer thread and returns its result.
/// A panic in `job` resumes in the caller. Called on a tracer thread, or after
/// [`launch_on_calling_thread`], it runs `job` on the calling thread.
pub(crate) fn execute<R: Send + 'static>(
    job: impl FnOnce() -> R + Send + 'static,
) -> std::io::Result<R> {
    if in_place() {
        return Ok(job());
    }
    let (send, receive) = mpsc::channel();
    let job: Job = Box::new(move || {
        let _ = send.send(panic::catch_unwind(AssertUnwindSafe(job)));
    });
    TRACER.with(|tracer| {
        let mut tracer = tracer.borrow_mut();
        if tracer.is_none() {
            *tracer = Some(spawn()?);
        }
        tracer
            .as_ref()
            .expect("the tracer thread was just set")
            .jobs
            .send(job)
            .map_err(|_| std::io::Error::other("Hermit's tracer thread has exited"))
    })?;
    match receive.recv() {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(payload)) => panic::resume_unwind(payload),
        Err(_) => Err(std::io::Error::other(
            "Hermit's tracer thread exited without finishing the launch",
        )),
    }
}

/// Runs `job` with [`execute`] if `owner` is the calling thread's tracer
/// thread, which is where that owner's operations live. `None` otherwise.
pub(crate) fn execute_on<R: Send + 'static>(
    owner: ThreadId,
    job: impl FnOnce() -> R + Send + 'static,
) -> Option<std::io::Result<R>> {
    if in_place() {
        return None;
    }
    let mine = TRACER.with(|tracer| tracer.borrow().as_ref().map(|tracer| tracer.id));
    (mine == Some(owner)).then(|| execute(job))
}

/// A [`reverie::process::Command`] on its way to the tracer thread.
pub(crate) struct SendCommand(reverie::process::Command);

// SAFETY: `Command`'s only part that is not `Send` is
// `reverie_process::util::CStringArray`, whose raw pointers point into the heap
// buffers of the `CString`s it owns. Moving it to another thread moves those
// owners with it, and nothing is shared with the sending thread. Every other
// field is `Send` (its `pre_exec` callbacks are `Send + Sync`).
unsafe impl Send for SendCommand {}

impl SendCommand {
    pub(crate) fn new(command: reverie::process::Command) -> Self {
        Self(command)
    }

    /// Called inside the closure, so that the closure captures the whole
    /// wrapper rather than the command field alone.
    pub(crate) fn into_inner(self) -> reverie::process::Command {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tracer thread serves its caller: every job runs on the same named
    /// thread, a job's panic resumes in the caller and leaves that thread
    /// serving, and `execute_on` runs only for that thread.
    #[test]
    fn one_tracer_thread_serves_its_caller() {
        let first = execute(|| std::thread::current().id()).unwrap();
        assert_ne!(first, std::thread::current().id());
        assert_eq!(
            execute(|| std::thread::current().name().map(str::to_owned)).unwrap(),
            Some("hermit-tracer".to_owned())
        );
        let panicked = std::panic::catch_unwind(|| execute(|| panic!("in the job")));
        let payload = panicked.expect_err("the panic resumes in the caller");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"in the job"));
        assert_eq!(execute(|| std::thread::current().id()).unwrap(), first);
        assert!(execute_on(std::thread::current().id(), || ()).is_none());
        assert_eq!(
            execute_on(first, || std::thread::current().id()).map(Result::unwrap),
            Some(first)
        );
    }
}
