/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The syscall-number prefilter on `Detcore::handle_syscall_event`'s file and
//! descriptor checks ([`may_name_file_or_descriptor`]).

use reverie::syscalls::LocalMemory;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

use super::may_name_file_or_descriptor;
use super::procfs_levels;
use super::removed_names;
use super::removes_a_name;
use super::rendered_procfs;
use crate::record_or_replay::NoopTool;
use crate::syscall_classification::all_pinned_syscalls;
use crate::syscall_hot_path_tests::InjectGuest;
use crate::syscall_hot_path_tests::detcore;

fn zero_args() -> SyscallArgs {
    SyscallArgs::new(0, 0, 0, 0, 0, 0)
}

/// The prefilter is exactly the union of each check's own syscalls, for
/// every syscall number in the pinned table, and the calls on the hot path
/// that name no file or descriptor are outside it.
#[test]
fn the_prefilter_is_the_union_of_each_checks_own_syscalls() {
    let mut skipped = 0;
    for sysno in all_pinned_syscalls() {
        let union = removes_a_name(sysno)
            || rendered_procfs::names_a_descriptor(sysno)
            || procfs_levels::looks_up_names(sysno)
            || rendered_procfs::may_answer_operation(sysno)
            || rendered_procfs::may_answer_mapping(sysno)
            || rendered_procfs::may_answer_path_call(sysno);
        assert_eq!(may_name_file_or_descriptor(sysno), union, "{sysno}");
        skipped += usize::from(!union);
    }
    eprintln!(
        "prefilter: {skipped} of {} syscall numbers skip the file checks",
        all_pinned_syscalls().count()
    );
    for sysno in [
        Sysno::getppid,
        Sysno::getpid,
        Sysno::gettid,
        Sysno::clock_gettime,
        Sysno::futex,
        Sysno::sched_yield,
        Sysno::brk,
        Sysno::mprotect,
        Sysno::munmap,
        Sysno::rt_sigprocmask,
        Sysno::rt_sigaction,
        Sysno::nanosleep,
        Sysno::exit,
        Sysno::exit_group,
        Sysno::set_tid_address,
        Sysno::getrandom,
        Sysno::close,
    ] {
        assert!(!may_name_file_or_descriptor(sysno), "{sysno}");
    }
}

/// Each check's syscalls, as the prefilter takes them, are exactly the ones
/// its own table acts on, for every syscall number: a call added to a
/// table and not to its predicate (or the reverse) fails here.
#[test]
fn each_checks_syscalls_are_the_ones_its_own_table_acts_on() {
    let memory = LocalMemory::new();
    for sysno in all_pinned_syscalls() {
        let call = Syscall::from_raw(sysno, zero_args());
        // `last_names_removed_by` and `guest_made_fifos_removed_by`; zero
        // flags, so `renameat2` is not an exchange.
        assert_eq!(
            removed_names(&call, true).is_some(),
            removes_a_name(sysno),
            "{sysno}"
        );
        assert!(
            removed_names(&call, false).is_none() || removes_a_name(sysno),
            "{sysno}"
        );
        // `adopt_received_placeholders`: a NULL path names its directory
        // descriptor itself, so every listed call names a descriptor.
        assert_eq!(
            !rendered_procfs::descriptor_arguments(&memory, sysno, &zero_args()).is_empty(),
            rendered_procfs::names_a_descriptor(sysno),
            "{sysno}"
        );
        // `refuse_procfs_name_outside_level`.
        assert_eq!(
            procfs_levels::looked_up(sysno).is_some(),
            procfs_levels::looks_up_names(sysno),
            "{sysno}"
        );
        // `answer_rendered_procfs_mapping` answers `Syscall::Mmap` only.
        assert_eq!(
            matches!(call, Syscall::Mmap(_)),
            rendered_procfs::may_answer_mapping(sysno),
            "{sysno}"
        );
    }
}

/// For every syscall number outside the prefilter, each check answers its
/// trivial value with no injection, no register read and no scratch stack
/// (the guest refuses both), with the configuration gates that let the
/// checks act open: skipping them changes nothing but the work.
#[test]
fn every_prefiltered_call_is_one_each_check_answers_trivially() {
    futures::executor::block_on(async {
        let mut tool = detcore::<NoopTool>();
        tool.cfg.recordreplay_modes = false;
        for sysno in all_pinned_syscalls().filter(|sysno| !may_name_file_or_descriptor(*sysno)) {
            let call = Syscall::from_raw(sysno, zero_args());
            let mut guest = InjectGuest::<NoopTool>::new(Ok(0));
            guest.config.virtualize_metadata = true;
            guest.config.sequentialize_threads = true;
            assert!(
                tool.last_names_removed_by(&mut guest, &call)
                    .await
                    .is_empty(),
                "{sysno}"
            );
            assert!(
                tool.guest_made_fifos_removed_by(&mut guest, &call)
                    .await
                    .is_empty(),
                "{sysno}"
            );
            assert!(
                tool.adopt_received_placeholders(&mut guest, &call)
                    .await
                    .is_ok(),
                "{sysno}"
            );
            assert!(
                matches!(
                    tool.refuse_procfs_name_outside_level(&mut guest, &call)
                        .await,
                    Ok(None)
                ),
                "{sysno}"
            );
            assert!(
                tool.answer_rendered_procfs_operation(&mut guest, &call)
                    .await
                    .is_none(),
                "{sysno}"
            );
            assert!(
                tool.answer_rendered_procfs_mapping(&mut guest, &call)
                    .await
                    .is_none(),
                "{sysno}"
            );
            assert!(
                matches!(
                    tool.answer_rendered_procfs_path_call(&mut guest, &call)
                        .await,
                    Ok(None)
                ),
                "{sysno}"
            );
            assert!(guest.injected().is_empty(), "{sysno}");
        }
    })
}

/// A relative path the guest names, in this process's own memory
/// (`LocalMemory`); nothing opens it.
const PATH: &std::ffi::CStr = c"hermit-prefilter-test-name";

/// Registers whose stack pointer leaves no scratch, so a lookup stages its
/// buffer in a transient page, whose `mmap` is the first injection.
fn no_scratch() -> libc::user_regs_struct {
    // SAFETY: the registers are plain integers; all zero is a valid value.
    unsafe { std::mem::zeroed() }
}

/// A guest-made FIFO set of one, isolated from the run's registry.
struct OneFifo((u64, u64));

impl crate::fifo_peer::GuestMadeFifos for OneFifo {
    fn any(&self) -> bool {
        true
    }

    fn contains(&self, fifo: (u64, u64)) -> bool {
        fifo == self.0
    }
}

/// A representative call of each check is inside the prefilter and still
/// reaches the check's work: `unlink` reaches `last_names_removed_by`'s
/// lookup and, with a guest-made FIFO in the set it consults (the test's
/// own), `guest_made_fifos_removed_by`'s;
/// an invalid `setxattr` flag is answered by
/// `answer_rendered_procfs_path_call`. For the other checks the
/// representative is in its own table, which the check reads first: `read`
/// names a descriptor, `openat` and `connect` look a name up, `write` and
/// `fcntl` are answered for a rendered descriptor and `mmap` maps one.
#[test]
fn a_representative_call_of_each_check_still_reaches_it() {
    let path = PATH.as_ptr() as usize;
    futures::executor::block_on(async {
        let mut tool = detcore::<NoopTool>();
        tool.cfg.recordreplay_modes = false;
        // aarch64 has unlinkat, not unlink.
        #[cfg(target_arch = "x86_64")]
        let (removal, args) = (Sysno::unlink, SyscallArgs::new(path, 0, 0, 0, 0, 0));
        #[cfg(not(target_arch = "x86_64"))]
        let (removal, args) = (
            Sysno::unlinkat,
            SyscallArgs::new(libc::AT_FDCWD as usize, path, 0, 0, 0, 0),
        );
        let unlink = Syscall::from_raw(removal, args);
        assert!(may_name_file_or_descriptor(removal));

        let mut guest =
            InjectGuest::<NoopTool>::new(Err(reverie::Errno::ENOMEM)).with_registers(no_scratch());
        guest.config.virtualize_metadata = true;
        guest.config.sequentialize_threads = true;
        assert!(
            tool.last_names_removed_by(&mut guest, &unlink)
                .await
                .is_empty()
        );
        let injected = guest.injected();
        assert_eq!(injected.first().map(|(sysno, _)| *sysno), Some(Sysno::mmap));

        // A set of this test's own with one FIFO in it: the process-wide
        // registry stays untouched, so concurrent tests see no change.
        let mut guest =
            InjectGuest::<NoopTool>::new(Err(reverie::Errno::ENOMEM)).with_registers(no_scratch());
        let removed = tool
            .guest_made_fifos_removed_from(&mut guest, &unlink, &OneFifo((u64::MAX, u64::MAX)))
            .await;
        assert!(removed.is_empty());
        assert_eq!(
            guest.injected().first().map(|(sysno, _)| *sysno),
            Some(Sysno::mmap)
        );

        // Invalid flags (0x100) for an attribute change: EINVAL before the
        // name, the value or the path, for every file.
        let setxattr = Syscall::from_raw(
            Sysno::setxattr,
            SyscallArgs::new(path, path, 0, 0, 0x100, 0),
        );
        assert!(may_name_file_or_descriptor(Sysno::setxattr));
        let mut guest = InjectGuest::<NoopTool>::new(Ok(0));
        assert_eq!(
            tool.answer_rendered_procfs_path_call(&mut guest, &setxattr)
                .await
                .map_err(|error| error.into_errno().unwrap()),
            Err(reverie::Errno::EINVAL)
        );
        assert!(guest.injected().is_empty());
    });
    for (sysno, own) in [
        (
            Sysno::read,
            rendered_procfs::names_a_descriptor(Sysno::read),
        ),
        (Sysno::openat, procfs_levels::looks_up_names(Sysno::openat)),
        (
            Sysno::connect,
            procfs_levels::looks_up_names(Sysno::connect),
        ),
        (
            Sysno::write,
            rendered_procfs::may_answer_operation(Sysno::write),
        ),
        (
            Sysno::fcntl,
            rendered_procfs::may_answer_operation(Sysno::fcntl),
        ),
        (
            Sysno::mmap,
            rendered_procfs::may_answer_mapping(Sysno::mmap),
        ),
        // aarch64 has faccessat, not access.
        #[cfg(target_arch = "x86_64")]
        (
            Sysno::access,
            rendered_procfs::may_answer_path_call(Sysno::access),
        ),
        #[cfg(not(target_arch = "x86_64"))]
        (
            Sysno::faccessat,
            rendered_procfs::may_answer_path_call(Sysno::faccessat),
        ),
    ] {
        assert!(own, "{sysno}");
        assert!(may_name_file_or_descriptor(sysno), "{sysno}");
    }
}
