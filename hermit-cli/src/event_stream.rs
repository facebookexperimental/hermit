/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use reverie::Tid;
use reverie::syscalls::Displayable;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::event::Event;

/// An event to help with debugging, but is not actually necessary for the
/// functionality of record/replay.
#[derive(Debug, Serialize, Deserialize)]
pub struct DebugEvent {
    /// The raw syscall.
    syscall: (Sysno, SyscallArgs),

    /// The pretty, displayable version of the syscall.
    pretty: String,
}

impl DebugEvent {
    /// Constructs a new `DebugEvent`.
    pub fn new<M: MemoryAccess>(syscall: Syscall, memory: &M) -> Self {
        Self {
            syscall: syscall.into_parts(),
            pretty: format!("{}", syscall.display(memory)),
        }
    }

    /// Returns the syscall associated with this debug event.
    pub fn syscall(&self) -> Syscall {
        Syscall::from_raw(self.syscall.0, self.syscall.1)
    }
}

impl fmt::Display for DebugEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.pretty)
    }
}

/// The number of argument registers the x86-64 kernel actually reads for
/// `sysno`. Registers beyond this are not part of the syscall's ABI and can hold
/// arbitrary leftover values in the guest, so they must be excluded when
/// comparing a replayed syscall against its recorded counterpart.
///
/// Reverie stores all six raw registers in every typed syscall and derives
/// `PartialEq` over them, so a naive `Syscall == Syscall` compares unused
/// registers too. That produces false desyncs for any syscall with fewer than
/// six arguments (e.g. `statfs`, which uses two).
///
/// Returns `None` for syscalls without an entry; callers then fall back to
/// comparing all six registers (the conservative pre-existing behavior). Only
/// syscalls that record/replay subscribes to (see `recorder::subscriptions`)
/// flow through the comparator, so those are the ones covered here.
///
/// These are true kernel arities, which for a few syscalls exceed the number of
/// typed fields reverie exposes (`open`, `openat`, `ioctl`, `socket`, and
/// `fcntl` fold meaningful arguments into typed enums). Using the kernel arity
/// -- not the typed field count -- guarantees we never zero a meaningful
/// argument and therefore never mask a genuine divergence.
fn kernel_arg_count(sysno: Sysno) -> Option<u8> {
    use reverie::syscalls::Sysno::*;
    Some(match sysno {
        close | fchdir | dup | time | unlink => 1,
        access | stat | fstat | lstat | dup2 | clock_gettime | gettimeofday | settimeofday
        | mkdir | statfs | fstatfs => 2,
        mprotect | read | readv | write | writev | lseek | getdents | getdents64 | dup3 | ioctl
        | socket | fcntl | connect | sendmsg | poll | getpeername | getsockname | getrandom
        | readlink | unlinkat | open | execve => 3,
        pread64 | pwrite64 | newfstatat | fadvise64 | openat => 4,
        statx | pwritev | preadv | ppoll | setsockopt | getsockopt | execveat => 5,
        recvfrom | sendto | pwritev2 | preadv2 | mmap => 6,
        _ => return None,
    })
}

/// Returns `syscall` with any argument registers beyond its kernel arity zeroed.
/// This makes the record/replay desync comparison ignore unused registers, which
/// are not part of the syscall and may legitimately differ between record and
/// replay. Syscalls without a known arity are returned unchanged (all six
/// registers still compared).
pub(crate) fn normalize_unused_args(syscall: Syscall) -> Syscall {
    let (sysno, args) = syscall.into_parts();
    let Some(used) = kernel_arg_count(sysno) else {
        return syscall;
    };
    let mut raw = [
        args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
    ];
    for reg in raw.iter_mut().skip(usize::from(used)) {
        *reg = 0;
    }
    Syscall::from_raw(
        sysno,
        SyscallArgs::new(raw[0], raw[1], raw[2], raw[3], raw[4], raw[5]),
    )
}

/// A stream of syscall events.
#[derive(Serialize, Deserialize)]
pub struct EventReader {
    // The file where events are stored.
    //
    // NOTE: This field isn't serializable/deserializable, so we have to skip it
    // for now. With an in-guest backend, we'd need to implement this manually
    // to support state migration.
    #[serde(skip, default = "default_reader")]
    reader: io::BufReader<fs::File>,

    // The file where raw syscalls are stored. This is used for detecting
    // desynchronization bugs. This is stored in a separate file so that we can
    // easily turn this on or off to shift the balance on debuggability and
    // performance.
    #[serde(skip, default = "default_reader")]
    debug_events: io::BufReader<fs::File>,

    // The number of events read so far. Useful for debugging purposes.
    pub count: u64,
}

fn default_reader() -> io::BufReader<fs::File> {
    unimplemented!("Serialization is not yet implemented")
}

impl EventReader {
    /// Opens an existing event stream.
    pub fn open(path: &Path, thread_id: Tid) -> io::Result<Self> {
        Ok(Self {
            reader: io::BufReader::new(fs::File::open(
                path.join("thread").join(thread_id.to_string()),
            )?),
            debug_events: io::BufReader::new(fs::File::open(
                path.join("thread").join(format!("{}.debug", thread_id)),
            )?),
            count: 0,
        })
    }

    /// Reads the next event from the stream. Returns an error if there are no
    /// more events to consume.
    pub fn next_event(&mut self) -> Result<Event, bincode::error::DecodeError> {
        bincode::serde::decode_from_std_read(&mut self.reader, bincode::config::legacy())
    }

    /// Reads the next syscall from the syscall stream.
    pub fn next_debug_event(&mut self) -> Result<DebugEvent, bincode::error::DecodeError> {
        let debug_event = bincode::serde::decode_from_std_read(
            &mut self.debug_events,
            bincode::config::legacy(),
        )?;
        self.count += 1;
        Ok(debug_event)
    }
}

impl Default for EventReader {
    fn default() -> Self {
        panic!("Thread state should be explicitly initialized in init_thread_state")
    }
}

/// A stream of syscall events.
#[derive(Serialize, Deserialize)]
pub struct EventWriter {
    // The file where events are stored.
    //
    // NOTE: This field isn't serializable/deserializable, so we have to skip it
    // for now. With an in-guest backend, we'd need to implement this manually
    // to support state migration.
    #[serde(skip, default = "default_writer")]
    writer: io::BufWriter<fs::File>,

    // The file where syscalls are stored. This is used for debugging purposes.
    #[serde(skip, default = "default_writer")]
    debug_events: io::BufWriter<fs::File>,
}

fn default_writer() -> io::BufWriter<fs::File> {
    unimplemented!("Serialization is not yet implemented")
}

impl EventWriter {
    /// Creates a new event stream.
    pub fn create(path: &Path, thread_id: Tid) -> io::Result<Self> {
        let path = path.join("thread");

        fs::create_dir_all(&path)?;

        Ok(Self {
            writer: io::BufWriter::new(fs::File::create(path.join(thread_id.to_string()))?),
            debug_events: io::BufWriter::new(fs::File::create(
                path.join(format!("{}.debug", thread_id)),
            )?),
        })
    }

    /// Writes an event to the end of the stream.
    pub fn push_event(&mut self, event: Event) -> Result<(), bincode::error::EncodeError> {
        bincode::serde::encode_into_std_write(&event, &mut self.writer, bincode::config::legacy())
            .map(|_| ())
    }

    /// Writes a debug event to the end of the stream.
    pub fn push_debug_event(
        &mut self,
        event: DebugEvent,
    ) -> Result<(), bincode::error::EncodeError> {
        bincode::serde::encode_into_std_write(
            &event,
            &mut self.debug_events,
            bincode::config::legacy(),
        )
        .map(|_| ())
    }
}

impl Default for EventWriter {
    fn default() -> Self {
        panic!("Thread state should be explicitly initialized in init_thread_state")
    }
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::normalize_unused_args;

    fn raw(sysno: Sysno, args: SyscallArgs) -> Syscall {
        Syscall::from_raw(sysno, args)
    }

    #[test]
    fn unused_args_do_not_cause_desync() {
        // statfs(path, buf) uses two arguments; registers 2..6 are unused and may
        // hold arbitrary leftover values. The raw compare (the bug) sees them as
        // different, but the normalized compare must not.
        let clean = raw(Sysno::statfs, SyscallArgs::new(0x1000, 0x2000, 0, 0, 0, 0));
        let garbage = raw(
            Sysno::statfs,
            SyscallArgs::new(0x1000, 0x2000, 0xdead, 0xbeef, 0, 0xcafe),
        );
        assert_ne!(clean, garbage, "raw comparison should differ (the bug)");
        assert_eq!(
            normalize_unused_args(clean),
            normalize_unused_args(garbage),
            "normalized statfs must ignore unused argument registers"
        );
    }

    #[test]
    fn meaningful_args_are_not_masked() {
        // fcntl(fd, cmd, arg) uses three arguments -- reverie exposes only two
        // typed fields, but the third register is real data. A difference there
        // must still be reported (guards against masking real divergences).
        let a = raw(Sysno::fcntl, SyscallArgs::new(3, 4, 0x800, 0, 0, 0));
        let b = raw(Sysno::fcntl, SyscallArgs::new(3, 4, 0x0, 0, 0, 0));
        assert_ne!(
            normalize_unused_args(a),
            normalize_unused_args(b),
            "fcntl's third argument is meaningful and must not be zeroed"
        );
    }

    #[test]
    fn unknown_syscall_compares_all_registers() {
        // A syscall without an arity entry keeps the conservative behavior of
        // comparing every register.
        let a = raw(Sysno::getpid, SyscallArgs::new(0, 0, 0xaa, 0, 0, 0));
        let b = raw(Sysno::getpid, SyscallArgs::new(0, 0, 0xbb, 0, 0, 0));
        assert_ne!(normalize_unused_args(a), normalize_unused_args(b));
    }
}
