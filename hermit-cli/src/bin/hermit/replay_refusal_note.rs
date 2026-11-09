/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! The discriminator for a container child whose scheduler refuses a replayed
//! schedule after a late precise timer event and exits it 122 instead of
//! finishing the run.
//!
//! A run that finishes after a late precise timer event is refused by name as
//! a skid overshoot: the child takes Reverie's late-event count at the end of
//! the run and reports it through its result channel (`SkidOvershootReport`
//! in hermit-cli's lib.rs, `FailureKind::SkidOvershoot`), and test harnesses
//! retry that refusal. A replay (`--replay-preemptions-from`,
//! `--replay-schedule-from`) that the late event pushes past a recorded
//! callback checkpoint never gets there: the scheduler refuses the checkpoint
//! and exits the child 122 at once (`detcore::scheduler::exit_on_replay_refusal`),
//! and 122 alone is every refusal's status. So just before that exit the
//! child writes the late-event count here, through the scheduler's replay
//! refusal hook (`detcore::set_replay_refusal_exit_hook`), and the parent
//! reads it when it classifies the 122 (`classify_container_result`): the
//! refusal is then the late event's skid-overshoot refusal, as on a run that
//! finished.
//!
//! The note is a `MAP_SHARED` cell, as the virtual-timeout note is
//! (`terminal_outcome_note`). A run that replays a schedule arms it
//! (`RunOpts::launch_in_container`) before it clones the container child,
//! which inherits the mapping and the hook; a guest loses both at its
//! `execve`. Every container launch clears a note already mapped
//! (`owned_container::run`), so a note always belongs to the latest child.

use std::io;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use hermit::Context;
use hermit::Error;

/// One shared note: the late precise timer events a child recorded before its
/// replay refusal, or zero for none.
struct Note {
    late_events: NonNull<AtomicU64>,
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
        let late_events = NonNull::new(address.cast::<AtomicU64>())
            .expect("mmap returned a non-null non-failure address");
        // SAFETY: the fresh mapping is writable and suitably page-aligned.
        unsafe { late_events.as_ptr().write(AtomicU64::new(0)) };
        Ok(Self { late_events })
    }

    fn cell(&self) -> &AtomicU64 {
        // SAFETY: initialized in `map` and never unmapped.
        unsafe { self.late_events.as_ref() }
    }

    fn record(&self, late_events: u64) {
        self.cell().store(late_events, Ordering::Release);
    }

    fn take(&self) -> Option<u64> {
        match self.cell().swap(0, Ordering::Acquire) {
            0 => None,
            late_events => Some(late_events),
        }
    }
}

static NOTE: OnceLock<Note> = OnceLock::new();

/// The scheduler's replay refusal hook in a container child: leave the count
/// for the parent. An atomic store, so it is safe wherever the scheduler
/// exits from.
fn record_in_shared_note(late_events: u64) {
    if let Some(note) = NOTE.get() {
        note.record(late_events);
    }
}

/// Before a container child of a run that replays a schedule is cloned: map
/// the note once and install the hook that writes it.
pub(super) fn arm() -> Result<(), Error> {
    if NOTE.get().is_none() {
        let note = Note::map().context("mapping the replay refusal note")?;
        let _ = NOTE.set(note);
        detcore::set_replay_refusal_exit_hook(Some(record_in_shared_note));
    }
    clear();
    Ok(())
}

/// Before any container child is cloned: forget what an earlier child left,
/// so a refusal it never reported cannot be read as this child's. Maps
/// nothing.
pub(super) fn clear() {
    if let Some(note) = NOTE.get() {
        note.take();
    }
}

/// The late precise timer events the latest container child noted before its
/// replay refusal, if any; reading it clears it. `None` when no child was
/// armed in this process.
pub(super) fn take() -> Option<u64> {
    NOTE.get()?.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A count written by a forked child just before it exits 122, as a
    /// scheduler refusing a replay after a late timer event exits, reaches the
    /// parent once. A private note keeps this away from the process's own,
    /// which other tests' classifications read.
    #[test]
    fn a_late_event_count_noted_by_an_exiting_child_reaches_its_parent_once() {
        let note = Note::map().unwrap();
        // SAFETY: the child does only an atomic store and `_exit`, both
        // async-signal-safe, so forking a multi-threaded test process is sound.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", io::Error::last_os_error());
        if child == 0 {
            note.record(3);
            // SAFETY: async-signal-safe; runs no destructors in the child.
            unsafe { libc::_exit(detcore_model::HERMIT_POLICY_REFUSAL_EXIT) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(
            libc::WEXITSTATUS(status),
            detcore_model::HERMIT_POLICY_REFUSAL_EXIT
        );
        assert_eq!(note.take(), Some(3));
        assert_eq!(note.take(), None, "read once");
    }
}
