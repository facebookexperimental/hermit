/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;
use reverie::RdtscResult;
use reverie::syscalls::PollFd;
use reverie::syscalls::StatBuf;
use reverie::syscalls::StatxBuf;
use reverie::syscalls::Timespec;
use reverie::syscalls::Timeval;
use reverie::syscalls::Timezone;
use reverie::syscalls::ioctl;
use serde::Deserialize;
use serde::Serialize;

const SIOCETHTOOL: usize = 0x8946;

/// Returns the stable error used for legacy ioctls whose nested output cannot
/// be represented by the currently pinned Reverie decoder.
pub(crate) fn deterministic_ioctl_error(request: &ioctl::Request<'_>) -> Option<Errno> {
    match request {
        // SIOCETHTOOL stores its output behind the data pointer nested in an
        // ifreq. Treating it as an opaque request would lose guest-visible
        // memory updates, so reject it identically during record and replay.
        ioctl::Request::SIOCETHTOOL(_) | ioctl::Request::Other(SIOCETHTOOL, _) => {
            Some(Errno::ENODEV)
        }
        _ => None,
    }
}

/// An event. This contains everything needed to verify and reproduce the
/// execution of a syscall.
#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    /// The event that we use to reconstruct the outputs of the original syscall.
    /// This is `Some` if need to record this syscall. If the syscall is already
    /// deterministic, then this is `None`.
    ///
    /// If a recorded syscall failed, then this is `Some(Err(Errno))`. That is,
    /// the failure should be reproduced during replay.
    pub event: Result<SyscallEvent, Errno>,
}

/// A `SyscallEvent` contains all the necessary information to replay a system
/// call.
///
/// Note that we only need a small amount of information to replay a syscall. The
/// only side effects observable by the user are:
///  1. Mutable pointers
///  2. Return values.
///
/// No registers are modified by the kernel except for `rax` (the return value).
/// Therefore, registers themselves do not need to be recorded since they are
/// strictly inputs. However, any arguments that are pointers that point to
/// mutable data expected to be modified by the kernel need to be recorded. If
/// this rule is applied uniformly for all syscalls, then we should be able to
/// implement full record and replay.
#[derive(Debug, Serialize, Deserialize)]
pub enum SyscallEvent {
    Bytes(Vec<u8>),
    /// The flattened output bytes of a vectored read (`readv`/`preadv`/
    /// `preadv2`). The bytes are stored contiguously in read order; on replay
    /// they are scattered back across the guest's `iovec` buffers. The length of
    /// the vector is exactly the return value of the syscall.
    Readv(Vec<u8>),
    Write(i64),
    Mmap(MmapEvent),
    Recvmsg(RecvmsgEvent),
    /// A syscall whose only value we care about is the return value. For many
    /// syscalls, this is often the only output of the syscall and thus it is the
    /// only piece of information that needs to be recorded.
    Return(i64),
    Stat(StatEvent),
    Statfs(Vec<u8>),
    Statx(StatxBuf),
    Rdtsc(RdtscResult),
    Ioctl(ioctl::Output),
    Timespec(TimespecEvent),
    Timeofday((Timeval, Timezone)),
    Poll(PollEvent),
    SockOpt(SockOptEvent),
    EpollWait(EpollWaitEvent),
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit the replay filesystem event schema.
    Open(OpenEvent),
    /// Result of `mkdir`/`mkdirat` plus the physical side effect replay must
    /// reconstruct when the recorded call found an existing directory.
    Mkdir(MkdirEvent),
    // TODO-HUMAN-REVIEW(#557): Audit the V2 record/replay event schema.
    WriteV2(WriteEvent),
    ReadV2(ReadEvent),
    ReadvV2(ReadEvent),
    FtruncateV2(FtruncateEvent),
    /// Destination length after a successful clone ioctl.
    FileClone(FileCloneEvent),
    /// A successful exec and the executable images required immediately before
    /// replaying it. Failed execs use the enclosing [`Event::event`] error.
    Exec(ExecEvent),
    /// The result and mutable output fields of a raw `ppoll` call.
    Ppoll(PpollEvent),
}

/// Recorded output and signal side effects of a read syscall.
// TODO-HUMAN-REVIEW(#557): Audit the recorded signalfd side-effect API.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReadEvent {
    /// Bytes returned to the guest.
    pub bytes: Vec<u8>,
    /// Number of pending SIGPIPE instances consumed by this signalfd read.
    pub consumed_sigpipe_count: u64,
    /// Kernel object whose state must advance during replay.
    pub replay_fd_kind: ReplayFdKind,
}

/// Recorded result and side effects of a write-family syscall.
// TODO-HUMAN-REVIEW(#557): Audit the recorded output and SIGPIPE API.
#[derive(Debug, Serialize, Deserialize)]
pub struct WriteEvent {
    pub result: Result<i64, Errno>,
    /// Original captured output stream aliased by the descriptor, if any.
    pub output_fd: Option<i32>,
    /// Byte offset used when the captured output endpoint is a regular file.
    pub output_offset: Option<i64>,
    /// Whether the write advances the captured open-file description offset.
    pub advances_output_offset: bool,
    /// Whether the kernel generated SIGPIPE together with EPIPE.
    pub generated_sigpipe: bool,
    /// Kernel object whose state must advance during replay.
    pub replay_fd_kind: ReplayFdKind,
    /// Offset at which a regular-file write began.
    pub replay_file_offset: Option<i64>,
    /// Whether the regular-file write advanced its open-file description.
    pub replay_file_advances_offset: bool,
}

/// Recorded result and captured-output side effect of ftruncate.
// TODO-HUMAN-REVIEW(#557): Audit the recorded ftruncate side-effect API.
#[derive(Debug, Serialize, Deserialize)]
pub struct FtruncateEvent {
    /// Recorded syscall result.
    pub result: Result<i64, Errno>,
    /// Original captured output stream aliased by the descriptor, if any.
    pub output_fd: Option<i32>,
    /// Requested file length.
    pub length: libc::off_t,
    /// Whether the target was a regular file during recording.
    pub replay_regular_file: bool,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#662): Audit descriptor-kind replay side effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReplayFdKind {
    None,
    Eventfd,
    RegularFile,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#662): Audit physical-open replay policy.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenEvent {
    /// Recorded syscall result.
    pub result: Result<i64, Errno>,
    /// Which filesystem object must exist physically during replay.
    pub materialize: OpenMaterialization,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OpenMaterialization {
    None,
    RegularFile,
    Directory,
}

/// Recorded result and replay-side materialization policy for `mkdir` and
/// `mkdirat`.
#[derive(Debug, Serialize, Deserialize)]
pub struct MkdirEvent {
    /// Recorded syscall result returned unchanged to the replayed guest.
    pub result: Result<i64, Errno>,
    /// True only when an `EEXIST` result was caused by a directory at the final
    /// path component (not by a regular file or symlink).
    pub existing_directory: bool,
}

/// A successful `execve`/`execveat` attempt. The event is written only from the
/// post-exec callback, so its presence is proof that Linux replaced the image.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecEvent {
    pub(crate) request: ExecRequest,
    pub(crate) executable: ExecImage,
    pub(crate) target: ExecTarget,
    pub(crate) dependencies: Vec<ExecDependency>,
}

/// Raw pathname bytes and lookup context supplied to `execveat`. Detcore
/// canonicalizes `execve` into this form before the recorder sees it.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ExecRequest {
    pub(crate) dirfd: libc::c_int,
    pub(crate) path: Vec<u8>,
    pub(crate) flags: libc::c_int,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ExecImage {
    pub(crate) digest: detcore::Digest,
    pub(crate) mode: u32,
}

/// Guest-visible state of a descriptor used as an executable object. Replay
/// restores this descriptor from the recorded image before asking Linux to
/// resolve an `AT_EMPTY_PATH` or procfs-fd exec request.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ExecDescriptor {
    /// Descriptor named by the exec request.
    pub(crate) target_fd: libc::c_int,
    pub(crate) status_flags: libc::c_int,
    pub(crate) offset: Option<libc::off_t>,
    /// Every descriptor that shared the target's open-file description before
    /// exec. `FD_CLOEXEC` belongs to each descriptor rather than the shared
    /// open-file description, so it is captured per alias.
    pub(crate) aliases: Vec<ExecDescriptorAlias>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ExecDescriptorAlias {
    pub(crate) fd: libc::c_int,
    pub(crate) descriptor_flags: libc::c_int,
}

/// How the main target is reconstructed. Ordinary paths are materialized using
/// their recorded symlink topology. Procfs/fd magic links and `AT_EMPTY_PATH`
/// must already resolve to the recorded live object and are verified instead.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ExecTarget {
    Materialize(ExecMaterialization),
    VerifyLive,
    RestoreDescriptor(ExecDescriptor),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ExecMaterialization {
    pub(crate) base: ExecMaterializationBase,
    pub(crate) path: Vec<u8>,
    pub(crate) symlinks: Vec<crate::record_replay_path::ResolvedSymlink>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum ExecMaterializationBase {
    Root,
    Cwd,
    DirectoryFd(libc::c_int),
}

/// A shebang or ELF interpreter required by the recorded image.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ExecDependency {
    pub(crate) base: ExecMaterializationBase,
    pub(crate) path: Vec<u8>,
    pub(crate) image: ExecImage,
    pub(crate) symlinks: Vec<crate::record_replay_path::ResolvedSymlink>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileCloneEvent {
    /// Final logical destination length.
    pub length: u64,
    /// Destination offset where the recorded source image begins.
    pub destination_offset: u64,
    /// Logical length of the cloned source range.
    pub replacement_length: u64,
    /// Whether the clone replaces the complete destination image.
    pub truncate_destination: bool,
    /// Recorded representation of the final destination image.
    pub image: FileCloneImage,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum FileCloneImage {
    /// Allocated data extents stored directly in the event stream.
    Extents(Vec<FileExtent>),
    /// Path relative to the recording data directory for a streamed snapshot.
    Sidecar(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileExtent {
    /// Byte offset of this extent.
    pub offset: u64,
    /// Extent contents.
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MmapEvent {
    /// The address where the memory shall be mapped.
    pub addr: usize,
    /// The contents of the memory map. Note that this may be less than the
    /// requested `length`.
    pub buf: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecvmsgEvent {
    pub result: i64,
    pub iovs: Vec<Vec<u8>>,
    pub name: Vec<u8>,
    pub name_len: libc::socklen_t,
    pub control: Vec<u8>,
    pub control_len: usize,
    pub flags: libc::c_int,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StatEvent {
    #[serde(with = "StatBuf")]
    pub statbuf: libc::stat,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TimespecEvent {
    pub timespec: Timespec,
}

/// Records every guest-visible output of a raw `poll` call.
#[derive(Serialize, Deserialize, Debug)]
pub struct PollEvent {
    /// The exact return value or errno observed while recording.
    pub result: Result<i64, Errno>,

    /// Whether the guest supplied a non-null pollfd pointer.
    pub fds_pointer_present: bool,

    /// The post-kernel pollfd output. Successful calls contain the complete
    /// array. An EFAULT may contain only the readable prefix because Linux can
    /// fault after writing earlier `revents` entries.
    pub fds: Option<Vec<PollFd>>,
}

/// Records every guest-visible output of a raw `ppoll` call.
///
/// Unlike `poll`, Linux treats the timeout as an in-out parameter. The pollfd
/// and timeout copyouts are independent of the return value. Linux writes
/// `revents` before attempting the remaining-time copyout and preserves the
/// syscall result when that later write faults. Keep both output snapshots
/// alongside the nested result so replay can restore the writes in kernel order
/// before returning either success or the error.
#[derive(Serialize, Deserialize, Debug)]
pub struct PpollEvent {
    /// The exact return value or errno observed while recording.
    pub result: Result<i64, Errno>,

    /// Whether the guest supplied a non-null pollfd pointer.
    pub fds_pointer_present: bool,

    /// Complete output on success or the readable output prefix on EFAULT.
    pub fds: Option<Vec<PollFd>>,

    /// Whether the guest supplied a non-null raw timeout pointer.
    pub timeout_pointer_present: bool,

    /// The exact remaining timeout after the kernel call. Raw `ppoll` mutates
    /// this continuously; replay must restore the captured value without
    /// rounding, freezing, or synthesizing it.
    pub timeout: Option<Timespec>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EpollWaitEvent {
    /// Raw initialized epoll_event bytes returned by the kernel.
    pub events: Vec<u8>,
    /// The number of initialized events in the buffer.
    pub updated: usize,
    /// Whether replay must wait on the guest-created kernel epoll set.
    pub replay_kernel_side_effect: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SockOptEvent {
    /// The (possibly truncated) value.
    pub value: Vec<u8>,

    /// The length of the value. If this is the same as `value.len()`, then
    /// no truncation of the value occurred.
    pub length: libc::socklen_t,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siocethtool_has_a_deterministic_error() {
        let request = ioctl::Request::SIOCETHTOOL(None);

        assert_eq!(deterministic_ioctl_error(&request), Some(Errno::ENODEV));

        let legacy_request = ioctl::Request::Other(SIOCETHTOOL, 0x1234);
        assert_eq!(
            deterministic_ioctl_error(&legacy_request),
            Some(Errno::ENODEV)
        );
    }

    #[test]
    fn neighboring_unknown_ioctl_is_not_rejected() {
        let request = ioctl::Request::Other(SIOCETHTOOL - 1, 0x1234);

        assert_eq!(deterministic_ioctl_error(&request), None);
    }

    #[test]
    fn mkdir_event_schema_preserves_directory_classification() {
        let events = vec![
            Event {
                event: Ok(SyscallEvent::Mkdir(MkdirEvent {
                    result: Err(Errno::EEXIST),
                    existing_directory: true,
                })),
            },
            Event {
                event: Ok(SyscallEvent::Mkdir(MkdirEvent {
                    result: Err(Errno::ENOENT),
                    existing_directory: false,
                })),
            },
        ];
        let encoded = bincode::serde::encode_to_vec(&events, bincode::config::legacy()).unwrap();
        let (decoded, consumed): (Vec<Event>, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::legacy()).unwrap();

        assert_eq!(consumed, encoded.len());
        let [first, second] = decoded.as_slice() else {
            panic!("unexpected mkdir event count")
        };
        assert!(matches!(
            &first.event,
            Ok(SyscallEvent::Mkdir(MkdirEvent {
                result: Err(Errno::EEXIST),
                existing_directory: true,
            }))
        ));
        assert!(matches!(
            &second.event,
            Ok(SyscallEvent::Mkdir(MkdirEvent {
                result: Err(Errno::ENOENT),
                existing_directory: false,
            }))
        ));
    }

    #[test]
    fn exec_event_schema_preserves_non_utf8_paths_and_topology() {
        let event = Event {
            event: Ok(SyscallEvent::Exec(ExecEvent {
                request: ExecRequest {
                    dirfd: libc::AT_FDCWD,
                    path: b"relative/\xff-program".to_vec(),
                    flags: 0,
                },
                executable: ExecImage {
                    digest: detcore::Digest::new(b"program"),
                    mode: 0o751,
                },
                target: ExecTarget::Materialize(ExecMaterialization {
                    base: ExecMaterializationBase::Cwd,
                    path: b"relative/\xff-program".to_vec(),
                    symlinks: vec![crate::record_replay_path::ResolvedSymlink {
                        lookup_index: 1,
                        target: b"target-\xfe".to_vec(),
                    }],
                }),
                dependencies: Vec::new(),
            })),
        };
        let encoded = bincode::serde::encode_to_vec(&event, bincode::config::legacy()).unwrap();
        let (decoded, consumed): (Event, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::legacy()).unwrap();

        assert_eq!(consumed, encoded.len());
        let Ok(SyscallEvent::Exec(decoded)) = decoded.event else {
            panic!("unexpected exec event")
        };
        assert_eq!(decoded.request.path, b"relative/\xff-program");
        let ExecTarget::Materialize(materialization) = decoded.target else {
            panic!("unexpected exec target policy")
        };
        assert_eq!(materialization.path, b"relative/\xff-program");
        assert_eq!(materialization.symlinks[0].target, b"target-\xfe");
    }
}
