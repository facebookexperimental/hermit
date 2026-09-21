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
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reverie::Errno;
use reverie::syscalls::Displayable;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::event::Event;

/// Stable event-stream identity derived from the guest process tree.
///
/// Each component after the root is the parent's deterministic child ordinal.
/// Unlike a process-local counter, this remains collision-free when Reverie
/// forks the tool itself and each process receives its own tool instance.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct EventStreamId(Vec<u64>);

impl EventStreamId {
    pub(crate) fn root() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn child(&self, ordinal: u64) -> Self {
        let mut path = self.0.clone();
        path.push(ordinal);
        Self(path)
    }

    fn file_name(&self) -> String {
        let mut identity = b"hermit-event-stream-v1\0".to_vec();
        for ordinal in &self.0 {
            identity.extend_from_slice(&ordinal.to_be_bytes());
        }
        format!("stream-{}", detcore::Digest::new(&identity))
    }
}

impl fmt::Display for EventStreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", detcore::ROOT_DETPID.as_raw())?;
        for ordinal in &self.0 {
            write!(f, ".{ordinal}")?;
        }
        Ok(())
    }
}

/// Allocates child ordinals within one deterministic parent stream.
///
/// The counter belongs to the parent thread state rather than the Recorder or
/// Replayer process instance. When Reverie forks a tool process, the child
/// receives its already-assigned stream identity and a fresh descendant
/// counter, while the continuing parent retains the incremented counter.
#[derive(Default)]
pub(crate) struct ChildEventStreamIds(AtomicU64);

impl ChildEventStreamIds {
    pub(crate) fn next(&self, parent: &EventStreamId) -> EventStreamId {
        let ordinal = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("record/replay child stream ordinal overflow"));
        parent.child(ordinal)
    }
}

#[derive(Serialize, Deserialize)]
struct EventStreamHeader {
    stream_id: EventStreamId,
}

fn decode_stream_header(
    reader: &mut io::BufReader<fs::File>,
    expected: &EventStreamId,
) -> io::Result<()> {
    let header: EventStreamHeader =
        bincode::serde::decode_from_std_read(reader, bincode::config::legacy()).map_err(
            |error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid event-stream identity header: {error}"),
                )
            },
        )?;
    if header.stream_id != *expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "event-stream identity mismatch: expected {expected}, found {}",
                header.stream_id
            ),
        ));
    }
    Ok(())
}

fn encode_stream_header(
    writer: &mut io::BufWriter<fs::File>,
    stream_id: &EventStreamId,
) -> io::Result<()> {
    bincode::serde::encode_into_std_write(
        EventStreamHeader {
            stream_id: stream_id.clone(),
        },
        writer,
        bincode::config::legacy(),
    )
    .map(|_| ())
    .map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to encode event-stream identity header: {error}"),
        )
    })
}

impl Serialize for ChildEventStreamIds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0.load(Ordering::Relaxed))
    }
}

impl<'de> Deserialize<'de> for ChildEventStreamIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self(AtomicU64::new(u64::deserialize(deserializer)?)))
    }
}

/// An event to help with debugging, but is not actually necessary for the
/// functionality of record/replay.
#[derive(Debug, Serialize, Deserialize)]
pub struct DebugEvent {
    /// The raw syscall.
    syscall: (Sysno, SyscallArgs),

    /// The pretty, displayable version of the syscall.
    pretty: String,

    /// Exec pathname bytes and lookup context. Raw syscall registers only hold
    /// a pointer, so this snapshot prevents a changed failed exec request from
    /// silently consuming an event that happened to return the same errno.
    exec_request: Option<DebugExecRequest>,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DebugExecRequest {
    dirfd: libc::c_int,
    path: Result<Vec<u8>, Errno>,
    flags: libc::c_int,
}

impl DebugEvent {
    /// Constructs a new `DebugEvent`.
    pub fn new<M: MemoryAccess>(syscall: Syscall, memory: &M) -> Self {
        let exec_request = match syscall {
            Syscall::Execve(call) => {
                let call: reverie::syscalls::Execveat = call.into();
                Some(DebugExecRequest {
                    dirfd: call.dirfd(),
                    path: call
                        .path()
                        .ok_or(Errno::EFAULT)
                        .and_then(|path| path.read(memory))
                        .map(|path| path.as_os_str().as_bytes().to_vec()),
                    flags: call.flags(),
                })
            }
            Syscall::Execveat(call) => Some(DebugExecRequest {
                dirfd: call.dirfd(),
                path: call
                    .path()
                    .ok_or(Errno::EFAULT)
                    .and_then(|path| path.read(memory))
                    .map(|path| path.as_os_str().as_bytes().to_vec()),
                flags: call.flags(),
            }),
            _ => None,
        };
        Self {
            syscall: syscall.into_parts(),
            pretty: format!("{}", syscall.display(memory)),
            exec_request,
        }
    }

    /// Returns the syscall associated with this debug event.
    pub fn syscall(&self) -> Syscall {
        Syscall::from_raw(self.syscall.0, self.syscall.1)
    }

    pub(crate) fn exec_request_matches(&self, other: &Self) -> bool {
        self.exec_request == other.exec_request
    }

    #[cfg(test)]
    pub(crate) fn for_test(sysno: Sysno, pretty: &str) -> Self {
        Self {
            syscall: (sysno, SyscallArgs::new(0, 0, 0, 0, 0, 0)),
            pretty: pretty.to_owned(),
            exec_request: None,
        }
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
        | mkdir | statfs | fstatfs | ftruncate | kill | listen | rt_sigpending => 2,
        mprotect | read | readv | write | writev | lseek | getdents | getdents64 | dup3 | ioctl
        | socket | fcntl | connect | sendmsg | poll | getpeername | getsockname | getrandom
        | readlink | unlinkat | open | execve | close_range | tgkill => 3,
        pread64 | pwrite64 | newfstatat | fadvise64 | openat => 4,
        statx | pwritev | preadv | ppoll | setsockopt | getsockopt | execveat | prctl => 5,
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
    pub fn open(path: &Path, stream_id: &EventStreamId) -> io::Result<Self> {
        let file_name = stream_id.file_name();
        let mut reader = io::BufReader::new(fs::File::open(path.join("thread").join(&file_name))?);
        let mut debug_events = io::BufReader::new(fs::File::open(
            path.join("thread").join(format!("{file_name}.debug")),
        )?);
        decode_stream_header(&mut reader, stream_id)?;
        decode_stream_header(&mut debug_events, stream_id)?;
        Ok(Self {
            reader,
            debug_events,
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
    pub fn create(path: &Path, stream_id: &EventStreamId) -> io::Result<Self> {
        let path = path.join("thread");

        fs::create_dir_all(&path)?;
        let file_name = stream_id.file_name();

        let create = |path| {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
        };

        let mut writer = io::BufWriter::new(create(path.join(&file_name))?);
        let mut debug_events = io::BufWriter::new(create(path.join(format!("{file_name}.debug")))?);
        encode_stream_header(&mut writer, stream_id)?;
        encode_stream_header(&mut debug_events, stream_id)?;
        Ok(Self {
            writer,
            debug_events,
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
    use std::fs;
    use std::io;

    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::ChildEventStreamIds;
    use super::EventStreamId;
    use super::kernel_arg_count;
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
    fn close_range_ignores_unused_args() {
        let clean = raw(
            Sysno::close_range,
            SyscallArgs::new(3, 9, libc::CLOSE_RANGE_CLOEXEC as usize, 0, 0, 0),
        );
        let garbage = raw(
            Sysno::close_range,
            SyscallArgs::new(
                3,
                9,
                libc::CLOSE_RANGE_CLOEXEC as usize,
                0xdead,
                0xbeef,
                0xcafe,
            ),
        );
        assert_eq!(normalize_unused_args(clean), normalize_unused_args(garbage));
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
    fn delegated_syscalls_have_kernel_arities() {
        assert_eq!(kernel_arg_count(Sysno::kill), Some(2));
        assert_eq!(kernel_arg_count(Sysno::ftruncate), Some(2));
        assert_eq!(kernel_arg_count(Sysno::listen), Some(2));
        assert_eq!(kernel_arg_count(Sysno::rt_sigpending), Some(2));
        assert_eq!(kernel_arg_count(Sysno::tgkill), Some(3));
        assert_eq!(kernel_arg_count(Sysno::prctl), Some(5));
    }

    #[test]
    fn unknown_syscall_compares_all_registers() {
        // A syscall without an arity entry keeps the conservative behavior of
        // comparing every register.
        let a = raw(Sysno::getpid, SyscallArgs::new(0, 0, 0xaa, 0, 0, 0));
        let b = raw(Sysno::getpid, SyscallArgs::new(0, 0, 0xbb, 0, 0, 0));
        assert_ne!(normalize_unused_args(a), normalize_unused_args(b));
    }

    #[test]
    fn pedigree_stream_ids_are_stable_and_collision_free() {
        let root = EventStreamId::root();
        let recording = ChildEventStreamIds::default();
        let replay = ChildEventStreamIds::default();
        let first = recording.next(&root);
        let second = recording.next(&root);
        assert_eq!(first.to_string(), "3.0");
        assert_eq!(second.to_string(), "3.1");
        assert_ne!(first, second);
        assert_eq!(first, replay.next(&root));
        assert_eq!(second, replay.next(&root));

        let serialized = serde_json::to_vec(&recording).expect("serialize child ordinal");
        let restored: ChildEventStreamIds =
            serde_json::from_slice(&serialized).expect("restore child ordinal");
        assert_eq!(restored.next(&root).to_string(), "3.2");

        let grandchildren = ChildEventStreamIds::default();
        assert_eq!(grandchildren.next(&first).to_string(), "3.0.0");
    }

    #[test]
    fn deeply_nested_stream_names_remain_bounded() {
        let mut stream_id = EventStreamId::root();
        for ordinal in 0..125 {
            stream_id = stream_id.child(ordinal);
        }
        assert_eq!(stream_id.file_name().len(), "stream-".len() + 64);
        assert!(stream_id.file_name().len() <= 255);
    }

    #[test]
    fn duplicate_stream_identity_refuses_instead_of_overwriting() {
        let data = tempfile::tempdir().expect("create recording directory");
        let stream_id = EventStreamId::root().child(0);
        let first = super::EventWriter::create(data.path(), &stream_id).expect("create stream");
        let error = super::EventWriter::create(data.path(), &stream_id)
            .err()
            .expect("duplicate stream must refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        drop(first);
    }

    #[test]
    fn mismatched_stream_header_refuses_instead_of_aliasing() {
        let data = tempfile::tempdir().expect("create recording directory");
        let expected = EventStreamId::root().child(0);
        let different = EventStreamId::root().child(1);
        let writer = super::EventWriter::create(data.path(), &expected).expect("create stream");
        drop(writer);

        let file_name = expected.file_name();
        let mut file = io::BufWriter::new(
            fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(data.path().join("thread").join(file_name))
                .expect("open stream for corruption"),
        );
        super::encode_stream_header(&mut file, &different).expect("write mismatched header");
        drop(file);

        let error = super::EventReader::open(data.path(), &expected)
            .err()
            .expect("mismatched stream identity must refuse");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("identity mismatch"));
    }
}
