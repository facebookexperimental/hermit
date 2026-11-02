/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! This module just aggregates submodules.

mod accept_in_turn;
mod files;
/// The `SigBlk` a `/proc` status snapshot reports in place of the kernel's,
/// for the guest-mask publication tests (`initialize_procfs_snapshot` calls it
/// directly).
#[cfg(test)]
pub(crate) use files::status_published_sigblk;
pub(crate) mod guest_siginfo;
pub mod helpers;

/// Re-exported so `procfs` can report the SAME ceiling that `F_SETPIPE_SZ`
/// enforces. Two constants that must agree are one constant.
pub(crate) use files::DETERMINISTIC_PIPE_CAPACITY_BYTES;
pub(crate) use files::SyscallCwd;
pub(crate) use files::descriptor_directory::SendInspection;
pub(crate) use files::may_name_file_or_descriptor;
pub(crate) use files::procfs_levels::take_proc_device;
pub(crate) use files::read_guest_exact;
pub(crate) use files::read_guest_prefix;
pub(crate) use files::rendered_procfs;
pub(crate) use threads::VIRTUAL_CPUSET_BYTES;
mod io;
pub(crate) mod memory;
mod misc;
mod namespace;
pub(crate) use namespace::anonymous_object_devices;
pub(crate) use namespace::guest_task_procfs;
mod network_trace;
pub(crate) mod robust_list;
pub(crate) mod scm_rights;
/// A buffer for an injected syscall, in the stack scratch or a transient page.
pub(crate) mod scratch;
mod signal;
pub(crate) mod signal_shadow;
/// The guest's virtual SIGALRM blocked bit (signal phase 1).
pub(crate) use signal::ClaimFault;
/// A send's `siginfo`, for the in-guest delivery gate's tests (A5 step
/// 3c.1c).
#[cfg(test)]
pub(crate) use signal::ReservedSendSiginfo;
/// A send's target, for the in-guest delivery gate's tests (A5 step 3c.1c).
#[cfg(test)]
pub(crate) use signal::ReservedSendTarget;
/// An emulated sleep's own check, for the in-guest delivery gate's tests (A5
/// step 3c.1c).
#[cfg(test)]
pub(crate) use signal::SleepCheck;
/// An emulated sleep's own verdict, for the in-guest delivery gate's tests (A5
/// step 3c.1c).
#[cfg(test)]
pub(crate) use signal::SleepSignal;
pub(crate) use signal::claim_still_pending;
/// An emulated sleep's own check, for the in-guest delivery gate's tests (A5
/// step 3c.1c).
#[cfg(test)]
pub(crate) use signal::sleep_signal;
pub(crate) use signal::take_claim_fault;
pub(crate) use signal::take_pending_signal;
pub(crate) use signal::virtual_sigalrm_blocked;
/// Rename cookies in inotify events, renumbered by first observation.
pub(crate) mod inotify;
pub(crate) mod socket_timestamp_ioctl;
pub(crate) mod sysinfo;
mod threads;
/// Where a futex wait takes its deadline from; a `restart_syscall` resumes an
/// interrupted wait with the deadline it kept.
pub(crate) use threads::FutexDeadline;
/// The kernel's view of a guest thread's signals, which the scheduler reads when
/// it commits a wake for a parked precise-mode futex waiter or an emulated sleep.
pub(crate) use threads::KernelSignalState;
/// One of a thread's two queues of pending signals.
pub(crate) use threads::SignalQueue;
pub(crate) use threads::default_action_ends_process;
pub(crate) use threads::forget_thread_status;
pub(crate) use threads::host_signal_state_reads_refused;
pub(crate) use threads::ignored_at_generation;
pub(crate) use threads::keep_status_files_open;
pub(crate) use threads::kernel_sigset_bit;
pub(crate) use threads::read_kernel_signal_state;
pub(crate) use threads::read_thread_signal_state;
pub(crate) use threads::real_uid;
pub(crate) use threads::refuse_host_signal_state_reads;
#[cfg(test)]
pub(crate) use threads::signal_state_read_seam;
pub(crate) use threads::thread_is_gone;
pub(crate) mod time;
pub(crate) mod timerfd;

use crate::consts::DET_SPECIAL_INODE_OFFSET;
use crate::resources::Device;
use crate::resources::ResourceID;
use crate::types::DetInode;
use crate::types::RawFd;

/// Give inherited standard streams identities that do not depend on backend
/// loader activity observed before the guest reaches its entry point.
fn deterministic_stdio_inode(fd: RawFd) -> Option<DetInode> {
    (libc::STDIN_FILENO..=libc::STDERR_FILENO)
        .contains(&fd)
        .then_some(DetInode::mint(
            DET_SPECIAL_INODE_OFFSET.as_raw() + fd as u64,
        ))
}

/// Preserve the existing inherited-stdio slot identities, but do not assign one
/// to an ordinary file or pipe that has replaced that slot. Aliases above fd 2
/// retain their existing pooled identity; extending the fixed namespace to
/// aliases requires consistent path-stat behavior too.
pub(crate) fn deterministic_stdio_inode_for_resource(
    fd: RawFd,
    resource: Option<ResourceID>,
) -> Option<DetInode> {
    match resource {
        Some(ResourceID::Device(
            Device::ContainerStdin | Device::ContainerStdout | Device::ContainerStderr,
        )) => deterministic_stdio_inode(fd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_inode_namespace_is_fixed() {
        assert_eq!(
            deterministic_stdio_inode(libc::STDIN_FILENO),
            Some(DetInode::mint(1000))
        );
        assert_eq!(
            deterministic_stdio_inode(libc::STDOUT_FILENO),
            Some(DetInode::mint(1001))
        );
        assert_eq!(
            deterministic_stdio_inode(libc::STDERR_FILENO),
            Some(DetInode::mint(1002))
        );
        assert_eq!(deterministic_stdio_inode(3), None);

        for resource in [
            ResourceID::Device(Device::ContainerStdin),
            ResourceID::Device(Device::ContainerStdout),
            ResourceID::Device(Device::ContainerStderr),
        ] {
            for fd in 0..=2 {
                assert_eq!(
                    deterministic_stdio_inode_for_resource(fd, Some(resource.clone())),
                    Some(DetInode::mint(1000 + fd as u64))
                );
            }
            assert_eq!(
                deterministic_stdio_inode_for_resource(3, Some(resource)),
                None
            );
        }
        for fd in 0..=2 {
            assert_eq!(deterministic_stdio_inode_for_resource(fd, None), None);
            assert_eq!(
                deterministic_stdio_inode_for_resource(
                    fd,
                    Some(ResourceID::FileContents(DetInode::mint(1000))),
                ),
                None
            );
        }
    }
}
