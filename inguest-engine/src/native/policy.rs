//! Native operation helpers used by the actual concrete engine prelude.
//!
//! These are the maintained bodies from the former native Detcore owner.
//! The ordinary test target includes THIS production source; it loads no
//! engine DSO, allocator/TLS owner, ToolHost or native admission mechanism.

use reverie::Tid;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Errno;

pub(super) const RANDOM_FILL_CHUNK_BYTES: usize = 4096;
pub(super) const GETRANDOM_MAX_BYTES: usize = (i32::MAX as usize) & !4095;
const GETRANDOM_ALLOWED_FLAGS: u32 = libc::GRND_NONBLOCK | libc::GRND_RANDOM | libc::GRND_INSECURE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GetrandomProbe {
    pub(super) requested: usize,
    pub(super) writable: usize,
}

impl GetrandomProbe {
    pub(super) fn consumed(self) -> usize {
        if self.writable == self.requested {
            return self.requested;
        }

        let failed_chunk = self.writable / RANDOM_FILL_CHUNK_BYTES;
        ((failed_chunk + 1) * RANDOM_FILL_CHUNK_BYTES).min(self.requested)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ProcessCloneProperties {
    pub(super) copies_process: bool,
    pub(super) blocks_parent: bool,
    pub(super) shares_files_without_thread: bool,
    pub(super) shares_memory_without_thread: bool,
}

pub(super) fn requires_native_lifecycle(sysnum: i64) -> bool {
    match sysnum {
        // AUTONOMOUS-BOT-IMPLEMENTED
        libc::SYS_fork
        | libc::SYS_vfork
        | libc::SYS_clone
        | libc::SYS_clone3
        | libc::SYS_rt_sigreturn
        | libc::SYS_execve => true,
        _ => false,
    }
}

pub(super) fn prlimit_new_limit_is_readable(
    sysnum: i64,
    args: &[u64],
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    if sysnum != libc::SYS_prlimit64 || args[2] == 0 {
        return true;
    }
    let mut limit = [0_u8; std::mem::size_of::<libc::rlimit64>()];
    read(args[2] as usize, &mut limit)
}

pub(super) fn multiplexed_io_inputs_are_readable(
    sysnum: i64,
    args: &[u64],
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    // AUTONOMOUS-BOT-IMPLEMENTED
    if sysnum == libc::SYS_ppoll {
        if args[2] == 0 {
            return true;
        }
        let mut timeout = [0_u8; std::mem::size_of::<libc::timespec>()];
        return read(args[2] as usize, &mut timeout);
    }
    // AUTONOMOUS-BOT-IMPLEMENTED
    if sysnum != libc::SYS_pselect6 {
        return true;
    }

    let nfds = args[0] as i64;
    if nfds < 0 {
        return true;
    }
    if args[4] != 0 {
        let mut timeout = [0_u8; std::mem::size_of::<libc::timespec>()];
        if !read(args[4] as usize, &mut timeout) {
            return false;
        }
    }

    const INTERNAL_MAX_NFDS: i64 = (std::mem::size_of::<libc::c_ulong>() * 8) as i64;
    if nfds > INTERNAL_MAX_NFDS {
        return true;
    }
    if nfds > 0 {
        let mut fd_set = [0_u8; std::mem::size_of::<libc::c_ulong>()];
        for address in &args[1..=3] {
            if *address != 0 && !read(*address as usize, &mut fd_set) {
                return false;
            }
        }
    }
    if args[5] != 0 {
        let mut sigmask_argument = [0_u8; 2 * std::mem::size_of::<usize>()];
        if !read(args[5] as usize, &mut sigmask_argument) {
            return false;
        }
    }
    true
}

pub(super) fn getrandom_flags_are_valid(flags: u64) -> bool {
    let flags = flags as u32;
    let random = flags & libc::GRND_RANDOM != 0;
    let insecure = flags & libc::GRND_INSECURE != 0;

    flags & !GETRANDOM_ALLOWED_FLAGS == 0 && !(random && insecure)
}

pub(super) fn getrandom_writable_prefix(
    args: &[u64],
    mut write: impl FnMut(usize, &[u8]) -> Result<usize, Errno>,
) -> Option<Result<GetrandomProbe, Errno>> {
    if args[1] == 0 || !getrandom_flags_are_valid(args[2]) {
        return None;
    }

    let requested = (args[1] as usize).min(GETRANDOM_MAX_BYTES);
    let zeros = [0_u8; RANDOM_FILL_CHUNK_BYTES];
    let mut writable = 0;
    while writable < requested {
        let Some(remote) = (args[0] as usize).checked_add(writable) else {
            break;
        };
        let chunk_len = (requested - writable).min(RANDOM_FILL_CHUNK_BYTES);
        let count = match write(remote, &zeros[..chunk_len]) {
            Ok(count) => count.min(chunk_len),
            Err(error) => return Some(Err(error)),
        };
        writable += count;
        if count < chunk_len {
            break;
        }
    }
    Some(Ok(GetrandomProbe {
        requested,
        writable,
    }))
}

pub(super) fn process_clone_properties(
    sysnum: i64,
    args: &[u64],
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> ProcessCloneProperties {
    const CLONE_ARGS_SIZE_VER0: u64 = 64;

    let flags = match sysnum {
        libc::SYS_fork => Some(0),
        libc::SYS_vfork => Some(libc::CLONE_VFORK as u64),
        libc::SYS_clone => args.first().copied(),
        libc::SYS_clone3 => {
            let Some((&address, &size)) = args.first().zip(args.get(1)) else {
                return ProcessCloneProperties::default();
            };
            if address == 0 || size < CLONE_ARGS_SIZE_VER0 {
                return ProcessCloneProperties::default();
            }
            let mut bytes = [0_u8; std::mem::size_of::<u64>()];
            if !read(address as usize, &mut bytes) {
                return ProcessCloneProperties::default();
            }
            Some(u64::from_ne_bytes(bytes))
        }
        _ => None,
    };

    let Some(flags) = flags else {
        return ProcessCloneProperties::default();
    };
    ProcessCloneProperties {
        copies_process: flags & (libc::CLONE_THREAD | libc::CLONE_VFORK) as u64 == 0,
        blocks_parent: flags & libc::CLONE_VFORK as u64 != 0,
        shares_files_without_thread: flags & libc::CLONE_FILES as u64 != 0
            && flags & libc::CLONE_THREAD as u64 == 0,
        shares_memory_without_thread: flags & libc::CLONE_VM as u64 != 0
            && flags & (libc::CLONE_THREAD | libc::CLONE_VFORK) as u64 == 0,
    }
}

pub(super) fn successful_process_clone_result(sysnum: i64, result: i64) -> bool {
    result >= 0
        && matches!(
            sysnum,
            libc::SYS_fork | libc::SYS_vfork | libc::SYS_clone | libc::SYS_clone3
        )
}

pub(super) fn process_child_registration(
    sysnum: i64,
    result: i64,
    virtual_child_tid: i32,
    raw_flags: u64,
    exit_signal: i32,
) -> Option<(Tid, CloneFlags, libc::c_int)> {
    if result <= 0 || virtual_child_tid <= 0 {
        return None;
    }
    let flags = CloneFlags::from_bits_truncate(raw_flags);
    if flags.intersects(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VFORK)
        || !matches!(sysnum, libc::SYS_fork | libc::SYS_clone | libc::SYS_clone3)
    {
        return None;
    }
    Some((Tid::from_raw(virtual_child_tid), flags, exit_signal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_clone_result_invalidates_only_for_successful_clone_family_calls() {
        for sysnum in [
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_clone,
            libc::SYS_clone3,
        ] {
            assert!(successful_process_clone_result(sysnum, 0));
            assert!(successful_process_clone_result(sysnum, 42));
            assert!(!successful_process_clone_result(
                sysnum,
                -libc::EINVAL as i64
            ));
        }

        for sysnum in [libc::SYS_getuid, libc::SYS_read, libc::SYS_execve] {
            assert!(!successful_process_clone_result(sysnum, 0));
            assert!(!successful_process_clone_result(sysnum, 42));
            assert!(!successful_process_clone_result(
                sysnum,
                -libc::EINVAL as i64
            ));
        }
    }

    #[test]
    fn process_child_registration_uses_only_successful_parent_results() {
        let fork = process_child_registration(libc::SYS_fork, 91_001, 4, 0, libc::SIGCHLD)
            .expect("successful parent-side fork must register the virtual child");
        assert_eq!(fork.0, Tid::from_raw(4));
        assert!(fork.1.is_empty());
        assert_eq!(fork.2, libc::SIGCHLD);

        let raw_flags = libc::CLONE_PARENT as u64;
        let clone =
            process_child_registration(libc::SYS_clone, 91_002, 5, raw_flags, libc::SIGUSR1)
                .expect("successful parent-side process clone must be registered");
        assert_eq!(clone.0, Tid::from_raw(5));
        assert!(clone.1.contains(CloneFlags::CLONE_PARENT));
        assert_eq!(clone.2, libc::SIGUSR1);

        assert_eq!(
            process_child_registration(libc::SYS_fork, 0, 4, 0, libc::SIGCHLD),
            None,
            "the child-side result must not register itself"
        );
        assert_eq!(
            process_child_registration(libc::SYS_clone, 91_003, 6, libc::CLONE_THREAD as u64, 0,),
            None,
            "thread clones keep the existing thread-created callback"
        );
        assert_eq!(
            process_child_registration(
                libc::SYS_vfork,
                91_004,
                7,
                libc::CLONE_VFORK as u64,
                libc::SIGCHLD,
            ),
            None,
            "vfork keeps the existing copied-child path"
        );
        assert!(
            process_child_registration(libc::SYS_clone3, 91_005, 8, 0, libc::SIGUSR2).is_some()
        );
    }

    #[test]
    fn process_clone_properties_cover_vfork_and_shared_files_forms() {
        let empty = [0; 6];
        assert_eq!(
            process_clone_properties(libc::SYS_fork, &empty, |_, _| {
                panic!("fork has no clone flags in guest memory")
            }),
            ProcessCloneProperties {
                copies_process: true,
                ..ProcessCloneProperties::default()
            }
        );
        assert_eq!(
            process_clone_properties(libc::SYS_vfork, &empty, |_, _| {
                panic!("vfork has no clone flags in guest memory")
            }),
            ProcessCloneProperties {
                copies_process: false,
                blocks_parent: true,
                shares_files_without_thread: false,
                shares_memory_without_thread: false,
            }
        );

        let mut clone = [0; 6];
        clone[0] = libc::CLONE_FILES as u64;
        assert!(
            process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_files_without_thread
        );
        clone[0] = (libc::CLONE_FILES | libc::CLONE_THREAD) as u64;
        assert!(
            !process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_files_without_thread
        );
        clone[0] = libc::CLONE_VM as u64;
        assert!(
            process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_memory_without_thread
        );
        clone[0] = (libc::CLONE_VM | libc::CLONE_THREAD) as u64;
        assert!(
            !process_clone_properties(libc::SYS_clone, &clone, |_, _| false)
                .shares_memory_without_thread
        );
        clone[0] = libc::CLONE_VFORK as u64;
        let vfork = process_clone_properties(libc::SYS_clone, &clone, |_, _| false);
        assert!(vfork.blocks_parent);
        assert!(!vfork.shares_memory_without_thread);

        let address = 0x2345;
        let mut clone3 = [0; 6];
        clone3[0] = address;
        clone3[1] = 64;
        assert!(
            process_clone_properties(libc::SYS_clone3, &clone3, |observed, bytes| {
                assert_eq!(observed, address as usize);
                bytes.copy_from_slice(&(libc::CLONE_FILES as u64).to_ne_bytes());
                true
            })
            .shares_files_without_thread
        );
        assert!(
            process_clone_properties(libc::SYS_clone3, &clone3, |_, bytes| {
                bytes.copy_from_slice(&(libc::CLONE_VFORK as u64).to_ne_bytes());
                true
            })
            .blocks_parent
        );
        clone3[1] = 63;
        assert_eq!(
            process_clone_properties(libc::SYS_clone3, &clone3, |_, _| {
                panic!("short clone3 input must not be read")
            }),
            ProcessCloneProperties::default()
        );
        clone3[1] = 64;
        assert_eq!(
            process_clone_properties(libc::SYS_clone3, &clone3, |_, _| false),
            ProcessCloneProperties::default()
        );
    }

    #[test]
    fn prlimit_input_preflight_rejects_unreadable_non_null_limits() {
        let null_limit = [0, libc::RLIMIT_NOFILE as u64, 0, 0, 0, 0];
        assert!(prlimit_new_limit_is_readable(
            libc::SYS_prlimit64,
            &null_limit,
            |_, _| false,
        ));

        let limit = [0, libc::RLIMIT_NOFILE as u64, 1, 0, 0, 0];
        assert!(!prlimit_new_limit_is_readable(
            libc::SYS_prlimit64,
            &limit,
            |_, _| false,
        ));
        assert!(prlimit_new_limit_is_readable(
            libc::SYS_prlimit64,
            &limit,
            |address, bytes| address == 1 && bytes.len() == std::mem::size_of::<libc::rlimit64>(),
        ));

        assert!(prlimit_new_limit_is_readable(
            libc::SYS_getrlimit,
            &limit,
            |_, _| false,
        ));
    }

    #[test]
    fn multiplexed_io_input_preflight_rejects_unreadable_inputs() {
        for (sysnum, timeout_index) in [(libc::SYS_ppoll, 2_usize), (libc::SYS_pselect6, 4_usize)] {
            let mut args = [0; 6];
            assert!(multiplexed_io_inputs_are_readable(sysnum, &args, |_, _| {
                false
            }));

            args[timeout_index] = 1;
            assert!(!multiplexed_io_inputs_are_readable(
                sysnum,
                &args,
                |_, _| false
            ));
            assert!(multiplexed_io_inputs_are_readable(
                sysnum,
                &args,
                |address, bytes| {
                    address == 1 && bytes.len() == std::mem::size_of::<libc::timespec>()
                },
            ));
        }

        let pselect_sets = [1, 1, 0, 0, 0, 0];
        assert!(!multiplexed_io_inputs_are_readable(
            libc::SYS_pselect6,
            &pselect_sets,
            |_, _| false,
        ));
        assert!(multiplexed_io_inputs_are_readable(
            libc::SYS_pselect6,
            &pselect_sets,
            |address, bytes| {
                address == 1 && bytes.len() == std::mem::size_of::<libc::c_ulong>()
            },
        ));

        let ignored_negative_sets = [u64::MAX, 1, 1, 1, 1, 1];
        assert!(multiplexed_io_inputs_are_readable(
            libc::SYS_pselect6,
            &ignored_negative_sets,
            |_, _| false,
        ));

        assert!(multiplexed_io_inputs_are_readable(
            libc::SYS_read,
            &[0, 0, 1, 0, 0, 0],
            |_, _| false,
        ));
    }

    #[test]
    fn only_dynamorio_managed_lifecycle_stays_native() {
        for sysnum in [
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_clone,
            libc::SYS_clone3,
            libc::SYS_rt_sigreturn,
            libc::SYS_execve,
        ] {
            assert!(requires_native_lifecycle(sysnum));
        }
        for sysnum in [
            libc::SYS_execveat,
            libc::SYS_wait4,
            libc::SYS_waitid,
            libc::SYS_read,
        ] {
            assert!(!requires_native_lifecycle(sysnum));
        }
    }

    #[test]
    fn getrandom_flag_validation_matches_detcore_policy() {
        for flags in [
            0,
            u64::from(libc::GRND_NONBLOCK),
            u64::from(libc::GRND_RANDOM),
            u64::from(libc::GRND_NONBLOCK | libc::GRND_RANDOM),
            1_u64 << 32,
        ] {
            assert!(getrandom_flags_are_valid(flags), "flags={flags:#x}");
        }
        assert!(!getrandom_flags_are_valid(u64::from(
            libc::GRND_RANDOM | libc::GRND_INSECURE
        )));
        assert!(!getrandom_flags_are_valid(0x8000_0000));
    }

    #[test]
    fn getrandom_probe_uses_zero_writes_and_tracks_shared_consumption() {
        let args = [0x1000, 16, 0, 0, 0, 0];
        let partial = getrandom_writable_prefix(&args, |remote, bytes| {
            assert_eq!(remote, 0x1000);
            assert_eq!(bytes, [0_u8; 16]);
            Ok(8)
        });
        let partial = partial.unwrap().unwrap();
        assert_eq!(
            partial,
            GetrandomProbe {
                requested: 16,
                writable: 8,
            }
        );
        assert_eq!(partial.consumed(), 16);

        let huge = [1, u64::MAX, 0, 0, 0, 0];
        let fault = getrandom_writable_prefix(&huge, |_, _| Ok(0))
            .unwrap()
            .unwrap();
        assert_eq!(fault.writable, 0);
        assert_eq!(fault.requested, GETRANDOM_MAX_BYTES);
        assert_eq!(fault.consumed(), RANDOM_FILL_CHUNK_BYTES);

        let invalid_flags = [0x1000, 16, 0x8000_0000, 0, 0, 0];
        let mut invoked = false;
        assert_eq!(
            getrandom_writable_prefix(&invalid_flags, |_, _| {
                invoked = true;
                Ok(16)
            }),
            None
        );
        assert!(!invoked);
    }
}
