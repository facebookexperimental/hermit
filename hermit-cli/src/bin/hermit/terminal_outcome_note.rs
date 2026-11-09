/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! The discriminator for a container child whose scheduler exits it 124 for a
//! `--virtual-timeout` instead of reporting the outcome through its result
//! channel.
//!
//! On ptrace the scheduler hands its outcome to the run, which finishes and
//! reports it through the child's result channel (`FailureKind::TerminalOutcome`).
//! A backend that cannot unwind the guest has the scheduler exit the child
//! with the outcome's status, and a status is all that crosses that exit. A
//! virtual timeout's 124 is also the status of `--timeout`'s fallback, which
//! `classify_container_result` has always read as `class=run-timeout`. So the
//! child writes the bound here, through the scheduler's exit hook
//! (`detcore::set_virtual_timeout_exit_hook`), just before it exits, and the
//! parent reads it when it classifies the 124. A guest deadlock's 121 is its
//! own and needs no note.
//!
//! The note is a `MAP_SHARED` cell, as `--max-log-bytes`'s budget is
//! (`tracing::LogBudget`). A run with `--virtual-timeout` arms it
//! (`RunOpts::launch_in_container`) before it clones the container child,
//! which inherits the mapping and the hook; a guest loses both at its
//! `execve`. Every container launch clears a note already mapped
//! (`owned_container::run`), so a note always belongs to the latest child,
//! even in a process that launches a run without the bound after one with
//! it; a process that never launches a run with the bound never maps it.

use std::io;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use detcore::TerminalOutcome;
use hermit::Context;
use hermit::Error;

/// One shared note: the bound of a virtual timeout in virtual nanoseconds, or
/// zero for none (the bound is never zero).
struct Note {
    bound_ns: NonNull<AtomicU64>,
}

// SAFETY: an initialized atomic in a MAP_SHARED mapping that is never
// unmapped; the atomic supplies synchronization.
unsafe impl Send for Note {}
unsafe impl Sync for Note {}

impl Note {
    fn map() -> io::Result<Self> {
        // SAFETY: anonymous shared mapping, no fixed address and no backing fd.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<AtomicU64>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let bound_ns = NonNull::new(address.cast::<AtomicU64>())
            .expect("mmap returned a non-null non-failure address");
        // SAFETY: the fresh mapping is writable and suitably page-aligned.
        unsafe { bound_ns.as_ptr().write(AtomicU64::new(0)) };
        Ok(Self { bound_ns })
    }

    fn cell(&self) -> &AtomicU64 {
        // SAFETY: initialized in `map` and never unmapped.
        unsafe { self.bound_ns.as_ref() }
    }

    fn record(&self, bound_ns: u64) {
        self.cell().store(bound_ns, Ordering::Release);
    }

    fn take(&self) -> Option<TerminalOutcome> {
        match self.cell().swap(0, Ordering::Acquire) {
            0 => None,
            bound_ns => Some(TerminalOutcome::VirtualTimeout { bound_ns }),
        }
    }
}

static NOTE: OnceLock<Note> = OnceLock::new();

/// The scheduler's exit hook in a container child: leave the bound for the
/// parent. An atomic store, so it is safe wherever the scheduler exits from.
fn record_in_shared_note(bound_ns: u64) {
    if let Some(note) = NOTE.get() {
        note.record(bound_ns);
    }
}

/// Before a container child of a run with `--virtual-timeout` is cloned: map
/// the note once and install the exit hook that writes it.
pub(super) fn arm() -> Result<(), Error> {
    if NOTE.get().is_none() {
        let note = Note::map().context("mapping the --virtual-timeout exit note")?;
        let _ = NOTE.set(note);
        detcore::set_virtual_timeout_exit_hook(Some(record_in_shared_note));
    }
    clear();
    Ok(())
}

/// Before any container child is cloned: forget what an earlier child left,
/// so a 124 it never reported cannot be read as this child's. Maps nothing.
pub(super) fn clear() {
    if let Some(note) = NOTE.get() {
        note.take();
    }
}

/// The virtual timeout the latest container child left before exiting, if
/// any; reading it clears it. `None` when no child was armed in this process.
pub(super) fn take() -> Option<TerminalOutcome> {
    NOTE.get()?.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A note written by a forked child just before it exits 124, as a
    /// scheduler that cannot unwind its guest exits on a virtual timeout,
    /// reaches the parent once, with the bound. A private note keeps this away
    /// from the process's own, which other tests' classifications read.
    #[test]
    fn a_virtual_timeout_noted_by_an_exiting_child_reaches_its_parent_once() {
        let note = Note::map().unwrap();
        let bound_ns = 3_500_000_000;
        // SAFETY: the child does only an atomic store and `_exit`, both
        // async-signal-safe, so forking a multi-threaded test process is sound.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", io::Error::last_os_error());
        if child == 0 {
            note.record(bound_ns);
            // SAFETY: async-signal-safe; runs no destructors in the child.
            unsafe { libc::_exit(hermit::HERMIT_DEADLINE_EXIT) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), hermit::HERMIT_DEADLINE_EXIT);
        assert_eq!(
            note.take(),
            Some(TerminalOutcome::VirtualTimeout { bound_ns })
        );
        assert_eq!(note.take(), None, "read once");
    }
}
