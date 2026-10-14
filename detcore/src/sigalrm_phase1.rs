/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Signal phase 1's table of the syscalls a process may make while its
//! SIGALRM disposition is a guest handler (in-guest LiteInst).
//!
//! Design: dev-hermit `ai_docs/transient/liteinst-inguest-signal-handlers-design-20261007.md`,
//! section 5 ("Unmodelled sleeping operations are refused", "Temporary masks",
//! "Other consumers of a pending SIGALRM") and closure 1, under
//! <https://github.com/rrnewton/hermit/issues/3520>.
//!
//! A guest SIGALRM is delivered only at points the scheduler controls. Three
//! kinds of call would break that, so in a handling process every syscall is
//! classified here before it runs, and anything this table does not place is
//! refused with EOPNOTSUPP:
//! - a wait that may sleep in the kernel interruptibly (Linux would end it for
//!   a signal) without first filing a scheduler request that admits it as a
//!   wait: a due SIGALRM would be stranded with no loss recorded;
//! - a temporary signal mask, which the published "SIGALRM virtually blocked"
//!   bit does not describe;
//! - a consumer that would take a pending SIGALRM outside the ledger.
//!
//! The table is deliberately the phase's measured need, not a survey of
//! everything that might be safe, so that every entry can be checked. The 6
//! target cells (`iostat -d -x 1 1`, `mpstat -I SCPU 1 1`,
//! `pidstat -d -p 1 1 1`, each also in a strict recipe) make these syscalls
//! after installing their SIGALRM handler, traced natively with `strace -f`:
//! `write` (to fd 1 only), `read`, `openat`, `close`, `fstat`, `lseek`,
//! `newfstatat`, `access`, `getdents64`, `brk`, `munmap`, `rt_sigaction`,
//! `rt_sigreturn`, `alarm`, `pause` and `exit_group`. The table adds what a
//! handler's run needs and that never sleeps: the remaining memory calls, the
//! signal mask, `exit`, identity, and the time calls that the runtime's
//! trapping vDSO stubs turn into syscalls when site patching is off.
//!
//! - [`ALWAYS_ALLOWED`]: none of these sleeps interruptibly; `pause` sleeps
//!   only through its admitted, tagged scheduler request. Stated limitation:
//!   on a file system whose operations wait for a user-space or network
//!   server (FUSE, Coda, 9p, NFS, CIFS), a path lookup, metadata call or open
//!   can wait interruptibly for that server; phase 1 does not model those
//!   file systems, and the target cells do not use them.
//! - [`DESCRIPTOR_IO`]: `read` and `write`, and `lseek` by the same rule
//!   ([`descriptor_class`]): a memfd, or a procfs or sysfs file Detcore
//!   serves from a snapshot of a kind whose capture cannot wait
//!   (`ProcfsFile::capture_cannot_wait`: exactly the kinds the cells read),
//!   where the kernel itself confirms the kind ([`kernel_provenance`]: the
//!   descriptor's resolved path and its file system, recorded at the open).
//!   Detcore's own path is a lexical spelling: `/proc/self/fd/N/../../x`
//!   normalizes to a `/proc/self` path whatever the kernel resolved. Nothing
//!   else is admitted: a regular mode is no proof (`/proc/kmsg` is `S_IFREG`
//!   and waits), and a cached `O_NONBLOCK` state can be changed by another
//!   process after a fork. A write to the container's inherited stdout or
//!   stderr runs, and `handle_write` records a loss if a SIGALRM is due once
//!   its grant is taken (design closure 1; the cells print to captured
//!   stdout).
//! - [`OPENS`]: `open`, `openat`, `creat`. `handle_openat` refuses, after its
//!   path grant (when no other guest can change the path), opening a FIFO
//!   without `O_NONBLOCK` (it waits for the other end), and opening any
//!   character or block device (its open may take an interruptible lock or
//!   wait for a carrier), unless the open has `O_PATH` or `O_DIRECTORY`, which
//!   never reach the file's own open. A lookup that fails for a reason the
//!   open would not share is refused too ([`open_refused`]). Lease breaks
//!   cannot block an open: `F_SETLEASE` is refused while any process handles
//!   SIGALRM and recorded otherwise (`SigalrmControl::ArmProducer`).
//! - `close`: only a memfd, or a descriptor the kernel places on procfs or
//!   sysfs ([`kernel_provenance`]), whose release callbacks never wait for a
//!   server or a peer. A socket's last close may linger, a terminal's may
//!   drain, and a network file system's may wait for its server.
//!
//! Every other syscall is refused, including `rt_sigsuspend` (until phase 1's
//! emulated `sigsuspend` exists), `rt_sigtimedwait`, `signalfd`, the poll and
//! select families, `epoll_pwait2`, `nanosleep`, `futex`, sockets, `fcntl`,
//! `sendfile`, `ioctl` (the cells' `ioctl` calls all precede their handler),
//! `fork`, `execve` and `flock`. A refusal is reported with the
//! syscall's name, so a phase-1 run that needs another entry says which; an
//! entry is added only with the check that places it.

use std::ffi::CString;
use std::path::Path;

use nix::errno::Errno;
use reverie::Guest;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
use tracing::info;

use crate::fd::FdType;
use crate::procfs::ProcfsFile;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Device;
use crate::resources::ResourceID;
use crate::tool_local::Detcore;

/// What Detcore knows of a descriptor, for [`descriptor_class`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FdFacts {
    /// Detcore's kind for the descriptor.
    pub ty: FdType,
    /// Whether it is the container's inherited stdin.
    pub container_stdin: bool,
    /// Whether it is the container's inherited stdout or stderr.
    pub container_output: bool,
    /// The file mode Detcore recorded for it, if any (`st_mode`).
    pub mode: Option<u32>,
    /// Whether Detcore serves its reads from a procfs snapshot of a kind
    /// whose capture cannot wait, and the kernel confirms the kind.
    pub safe_snapshot: bool,
    /// Whether the kernel places it on procfs or sysfs.
    pub pseudo_fs: bool,
}

/// What the kernel says a descriptor opened by a process that handles
/// SIGALRM is, recorded by `handle_openat` right after the open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Phase1Provenance {
    /// Its file system is procfs or sysfs.
    pub pseudo_fs: bool,
    /// Its resolved path names a snapshot kind whose capture cannot wait.
    pub safe_snapshot: bool,
}

/// The kernel's account of `fd` in process `pid`, none of it opening the
/// file: its file system and resolved path (`statfs` and `readlink` of
/// `/proc/<pid>/fd/<fd>`), and its mount (`fdinfo` and `mountinfo`). The
/// resolved path names a procfs or sysfs object only on that file system's
/// whole mount at `/proc` or `/sys`: through a bind mount of one object over
/// another's name, `readlink` reports the mount point's name.
pub(crate) fn kernel_provenance(pid: i32, fd: i32) -> Phase1Provenance {
    let link = format!("/proc/{pid}/fd/{fd}");
    let resolved = std::fs::read_link(&link)
        .ok()
        .filter(|path| path.is_absolute());
    let mountinfo = std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).ok();
    let mount = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}"))
        .ok()
        .and_then(|fdinfo| fdinfo_mount_id(&fdinfo))
        .and_then(|mount_id| mount_root_and_point(mountinfo.as_deref()?, mount_id));
    provenance_from(statfs_type(&link), mount, resolved.as_deref())
}

/// Whether a `/proc/<pid>/fd/<fd>` link target names a signalfd.
pub(crate) fn link_is_signalfd(target: &Path) -> bool {
    target.as_os_str() == "anon_inode:[signalfd]"
}

/// Whether process `pid` holds a signalfd, from the kernel's own descriptor
/// table (`/proc/<pid>/fd`), so an inherited one counts whatever Detcore's
/// model calls it. A table that cannot be read counts as holding one. Listed
/// with [`crate::util::find_in_directory`], which keeps the guest's heap out
/// of it under in-guest LiteInst.
pub(crate) fn process_holds_signalfd(pid: i32) -> bool {
    let directory = std::path::PathBuf::from(format!("/proc/{pid}/fd"));
    crate::util::find_in_directory(&directory, |name| {
        std::fs::read_link(directory.join(name))
            .is_ok_and(|target| link_is_signalfd(&target))
            .then_some(())
    })
    .map_or(true, |found| found.is_some())
}

/// [`kernel_provenance`] from its kernel facts: the file system type, the
/// mount's root and mount point, and the resolved path.
fn provenance_from(
    fs_type: Option<libc::c_long>,
    mount: Option<(&str, &str)>,
    resolved: Option<&Path>,
) -> Phase1Provenance {
    let sysfs = fs_type == Some(libc::SYSFS_MAGIC);
    let pseudo_fs = sysfs || fs_type == Some(libc::PROC_SUPER_MAGIC);
    let whole_mount = mount == Some(("/", if sysfs { "/sys" } else { "/proc" }));
    let safe_snapshot = pseudo_fs
        && whole_mount
        && resolved.is_some_and(|path| {
            ProcfsFile::from_path(path).is_some_and(|file| file.capture_cannot_wait())
                || (sysfs && is_canonical_block_stat(path))
        });
    Phase1Provenance {
        pseudo_fs,
        safe_snapshot,
    }
}

/// The `mnt_id` line of a descriptor's `/proc/<pid>/fdinfo/<fd>`.
fn fdinfo_mount_id(fdinfo: &str) -> Option<u64> {
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|id| id.trim().parse().ok())
}

/// The root (within its file system) and mount point of mount `mount_id` in
/// a `/proc/<pid>/mountinfo` text.
fn mount_root_and_point(mountinfo: &str, mount_id: u64) -> Option<(&str, &str)> {
    mountinfo.lines().find_map(|line| {
        let mut fields = line.split(' ');
        let id: u64 = fields.next()?.parse().ok()?;
        let _parent = fields.next()?;
        let _device = fields.next()?;
        let root = fields.next()?;
        let point = fields.next()?;
        (id == mount_id).then_some((root, point))
    })
}

/// `statfs(path).f_type`, if the lookup succeeds.
fn statfs_type(path: &str) -> Option<libc::c_long> {
    let path = CString::new(path).ok()?;
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: a valid C string and a buffer of the right size.
    let rc = unsafe { libc::statfs(path.as_ptr(), buf.as_mut_ptr()) };
    // SAFETY: statfs filled the buffer when it returned 0.
    (rc == 0).then(|| unsafe { buf.assume_init() }.f_type)
}

/// A whole disk's statistics file under its canonical sysfs path,
/// `/sys/devices/.../block/<disk>/stat`, which `/sys/block/<disk>/stat`
/// resolves to. Sysfs names are the kernel's own.
fn is_canonical_block_stat(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    path.starts_with("/sys/devices")
        && matches!(
            components.as_slice(),
            [.., block, _, stat] if block.as_os_str() == "block" && stat.as_os_str() == "stat"
        )
}

/// Syscalls allowed whatever their arguments.
pub(crate) const ALWAYS_ALLOWED: &[Sysno] = &[
    // Measured in the target cells after their handler is installed.
    Sysno::fstat,
    Sysno::newfstatat,
    #[cfg(not(target_arch = "aarch64"))]
    Sysno::access,
    Sysno::getdents64,
    Sysno::brk,
    Sysno::munmap,
    Sysno::rt_sigaction,
    Sysno::rt_sigreturn,
    #[cfg(not(target_arch = "aarch64"))]
    Sysno::alarm,
    #[cfg(not(target_arch = "aarch64"))]
    Sysno::pause,
    Sysno::exit_group,
    // A handler's run, and the same calls by their other names.
    Sysno::faccessat,
    Sysno::mmap,
    Sysno::mprotect,
    Sysno::rt_sigprocmask,
    Sysno::setitimer,
    Sysno::getitimer,
    Sysno::exit,
    Sysno::getpid,
    Sysno::gettid,
    // Existing fixed virtual-root scalar returns: these dispatch arms neither
    // inject credential queries nor access a guest result buffer or wait.
    // Shared signal eligibility, scheduling and accounting are unchanged.
    Sysno::getuid,
    Sysno::geteuid,
    Sysno::getgid,
    Sysno::getegid,
    // The trapping vDSO stubs of a run with site patching off.
    Sysno::clock_gettime,
    Sysno::clock_getres,
    Sysno::gettimeofday,
    #[cfg(not(target_arch = "aarch64"))]
    Sysno::time,
];

/// Descriptor I/O, allowed by the descriptor (its first argument).
pub(crate) const DESCRIPTOR_IO: &[Sysno] = &[Sysno::read, Sysno::write];

/// Opens: `handle_openat` refuses a blocking FIFO open after its grant.
pub(crate) const OPENS: &[Sysno] = &[
    #[cfg(not(target_arch = "aarch64"))]
    Sysno::open,
    Sysno::openat,
    #[cfg(not(target_arch = "aarch64"))]
    Sysno::creat,
];

/// Syscalls allowed for some arguments or descriptors only ([`classify`]).
pub(crate) const CONDITIONAL: &[Sysno] = &[Sysno::close, Sysno::lseek];

/// How phase 1 treats one syscall in a process that handles SIGALRM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase1Class {
    /// It runs, with any check its handler makes after its grant.
    Allowed,
    /// Refused with EOPNOTSUPP, for the reason given.
    Refused(&'static str),
}

/// Classifies one syscall made by a process that handles SIGALRM. `facts`
/// reports what Detcore knows of a descriptor, if it models it.
pub(crate) fn classify(
    sysno: Sysno,
    args: &SyscallArgs,
    facts: impl FnOnce(i32) -> Option<FdFacts>,
) -> Phase1Class {
    if ALWAYS_ALLOWED.contains(&sysno) || OPENS.contains(&sysno) {
        return Phase1Class::Allowed;
    }
    if DESCRIPTOR_IO.contains(&sysno) {
        return descriptor_class(sysno == Sysno::write, facts(args.arg0 as i32));
    }
    if !CONDITIONAL.contains(&sysno) {
        return Phase1Class::Refused("a syscall phase 1 has not placed");
    }
    match sysno {
        Sysno::close => close_class(facts(args.arg0 as i32)),
        // A seek on a live seq_file runs its show callback, as a read does.
        Sysno::lseek => descriptor_class(false, facts(args.arg0 as i32)),
        _ => unreachable!("{sysno} is not in CONDITIONAL"),
    }
}

/// Classifies a `close`: a socket's last close may linger, and a terminal's
/// may drain, interruptibly.
pub(crate) fn close_class(facts: Option<FdFacts>) -> Phase1Class {
    let Some(facts) = facts else {
        return Phase1Class::Refused("closing a descriptor Detcore does not model");
    };
    if facts.container_stdin || facts.container_output {
        return Phase1Class::Refused("closing the container's stdio, which may be a socket");
    }
    if facts.ty == FdType::Memfd || facts.pseudo_fs {
        Phase1Class::Allowed
    } else {
        Phase1Class::Refused("closing a descriptor whose last close may wait")
    }
}

/// Whether `handle_openat`, in a process that handles SIGALRM, refuses an open
/// (without `O_PATH` or `O_DIRECTORY`) given its path lookup, taken after its
/// grant, and whether the open has `O_NONBLOCK`. Opening a FIFO without
/// `O_NONBLOCK` waits for the other end; opening a character or block device
/// may take an interruptible lock or wait for a carrier, `O_NONBLOCK` or not.
/// A lookup that failed for a reason the open would not share leaves the kind
/// unknown: a missing or unresolvable path fails the open the same way, or
/// (with `O_CREAT`) makes it create a regular file, but a lookup denied by
/// permission is refused, since a security module can deny the lookup's
/// `getattr` and still allow the open.
pub(crate) fn open_refused(lookup: Result<libc::stat, reverie::Errno>, nonblocking: bool) -> bool {
    match lookup {
        Ok(stat) => match stat.st_mode & libc::S_IFMT {
            libc::S_IFIFO => !nonblocking,
            libc::S_IFCHR | libc::S_IFBLK => true,
            _ => false,
        },
        Err(
            reverie::Errno::ENOENT
            | reverie::Errno::ENOTDIR
            | reverie::Errno::ELOOP
            | reverie::Errno::ENAMETOOLONG
            | reverie::Errno::EFAULT,
        ) => false,
        Err(_) => true,
    }
}

/// Classifies a `read` or `write` by what Detcore knows of its descriptor.
pub(crate) fn descriptor_class(write: bool, facts: Option<FdFacts>) -> Phase1Class {
    let Some(facts) = facts else {
        return Phase1Class::Refused("I/O on a descriptor Detcore does not model");
    };
    if facts.container_output && write {
        // `handle_write` records a loss if a SIGALRM is due at its grant.
        return Phase1Class::Allowed;
    }
    if facts.container_stdin || facts.container_output {
        return Phase1Class::Refused("I/O on the container's stdio that may wait");
    }
    match facts.ty {
        FdType::Memfd => Phase1Class::Allowed,
        // Served from a snapshot whose capture cannot wait.
        FdType::Regular if facts.safe_snapshot => Phase1Class::Allowed,
        _ => Phase1Class::Refused("I/O on a descriptor that may sleep interruptibly"),
    }
}

/// Whether `resource` names the container's inherited stdout or stderr.
pub(crate) fn is_container_output(resource: &ResourceID) -> bool {
    matches!(
        resource,
        ResourceID::Device(Device::ContainerStdout | Device::ContainerStderr)
    )
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Signal phase 1's gate for one syscall of a process that handles
    /// SIGALRM: `Some(EOPNOTSUPP)` when [`classify`] refuses it, `None` when it
    /// may run.
    pub(crate) fn sigalrm_phase1_gate<G: Guest<Self>>(
        &self,
        guest: &G,
        call: Syscall,
    ) -> Option<Errno> {
        let (sysno, args) = call.into_parts();
        let class = classify(sysno, &args, |fd| {
            guest
                .thread_state()
                .with_detfd(fd, |detfd| FdFacts {
                    // A procfs file Detcore renders is, to the kernel, the
                    // empty sealed memfd that holds its place: Detcore serves
                    // its reads, and its last close waits for nothing.
                    ty: if detfd.procfs_rendered_path().is_some() {
                        FdType::Memfd
                    } else {
                        detfd.ty()
                    },
                    container_stdin: matches!(
                        detfd.resource(),
                        Some(ResourceID::Device(Device::ContainerStdin))
                    ),
                    container_output: detfd.resource().as_ref().is_some_and(is_container_output),
                    mode: detfd.stat().map(|stat| stat.mode),
                    safe_snapshot: detfd.procfs_serves_snapshot()
                        && detfd.procfs_capture_cannot_wait()
                        && detfd
                            .sigalrm_phase1_provenance()
                            .is_some_and(|provenance| provenance.safe_snapshot),
                    pseudo_fs: detfd
                        .sigalrm_phase1_provenance()
                        .is_some_and(|provenance| provenance.pseudo_fs),
                })
                .ok()
        });
        let Phase1Class::Refused(why) = class else {
            return None;
        };
        info!(
            "[dtid {}] {} refused (EOPNOTSUPP) in a process that handles SIGALRM: {} (signal phase 1).",
            guest.thread_state().dettid,
            sysno,
            why
        );
        Some(Errno::EOPNOTSUPP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscall_classification::all_pinned_syscalls;

    fn args(values: [usize; 6]) -> SyscallArgs {
        SyscallArgs::new(
            values[0], values[1], values[2], values[3], values[4], values[5],
        )
    }

    fn no_fd(_: i32) -> Option<FdFacts> {
        None
    }

    fn names(list: &[Sysno]) -> Vec<&'static str> {
        let mut names: Vec<_> = list.iter().map(|sysno| sysno.name()).collect();
        names.sort_unstable();
        names
    }

    /// The kernel's descriptor table, not the model, decides whether the
    /// process holds a signalfd: one created here is found by its link.
    #[test]
    fn a_signalfd_in_the_kernels_table_is_found() {
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        assert!(link_is_signalfd(Path::new("anon_inode:[signalfd]")));
        assert!(!link_is_signalfd(Path::new("anon_inode:[eventfd]")));
        assert!(!link_is_signalfd(Path::new("/dev/null")));
        let pid = std::process::id() as i32;
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut mask) };
        unsafe { libc::sigaddset(&mut mask, libc::SIGUSR2) };
        let raw = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC) };
        assert!(raw >= 0);
        let signalfd = unsafe { OwnedFd::from_raw_fd(raw) };
        assert!(process_holds_signalfd(pid));
        drop(signalfd);
        // No longer necessarily false: the test binary may hold another; so
        // check only that a process without a readable table counts as one.
        assert!(process_holds_signalfd(-1));
    }

    /// The table's exact membership is pinned, and every syscall outside it is
    /// refused: an entry added, removed or swapped fails here and needs the
    /// check that places it.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_sigalrm_phase1_table_admits_exactly_its_pinned_members() {
        assert_eq!(
            names(ALWAYS_ALLOWED),
            [
                "access",
                "alarm",
                "brk",
                "clock_getres",
                "clock_gettime",
                "exit",
                "exit_group",
                "faccessat",
                "fstat",
                "getdents64",
                "getegid",
                "geteuid",
                "getgid",
                "getitimer",
                "getpid",
                "gettid",
                "gettimeofday",
                "getuid",
                "mmap",
                "mprotect",
                "munmap",
                "newfstatat",
                "pause",
                "rt_sigaction",
                "rt_sigprocmask",
                "rt_sigreturn",
                "setitimer",
                "time",
            ]
        );
        assert_eq!(names(DESCRIPTOR_IO), ["read", "write"]);
        assert_eq!(names(OPENS), ["creat", "open", "openat"]);
        assert_eq!(names(CONDITIONAL), ["close", "lseek"]);

        let placed: Vec<Sysno> = [ALWAYS_ALLOWED, DESCRIPTOR_IO, OPENS, CONDITIONAL].concat();
        let mut refused = 0;
        for sysno in all_pinned_syscalls() {
            if placed.contains(&sysno) {
                continue;
            }
            assert_eq!(
                classify(sysno, &args([0; 6]), no_fd),
                Phase1Class::Refused("a syscall phase 1 has not placed"),
                "{sysno}"
            );
            refused += 1;
        }
        assert_eq!(refused + placed.len(), all_pinned_syscalls().count());
    }

    /// Only the four fixed scalar credential returns are admitted. Buffered
    /// credentials, native process relations and new path operations keep their
    /// existing refusal; none of these decisions may consult a descriptor.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sigalrm_phase1_scalar_identity_keeps_neighboring_refusals() {
        for sysno in [Sysno::getuid, Sysno::geteuid, Sysno::getgid, Sysno::getegid] {
            assert_eq!(
                classify(sysno, &args([0; 6]), |_| panic!(
                    "no descriptor for {sysno}"
                )),
                Phase1Class::Allowed,
                "{sysno}"
            );
        }
        for sysno in [
            Sysno::getresuid,
            Sysno::getresgid,
            Sysno::getgroups,
            Sysno::getppid,
            Sysno::getpgrp,
            Sysno::getpgid,
            Sysno::getsid,
            Sysno::uname,
            Sysno::getrandom,
            Sysno::stat,
            Sysno::lstat,
            Sysno::statx,
            Sysno::readlink,
            Sysno::readlinkat,
        ] {
            assert_eq!(
                classify(sysno, &args([0; 6]), |_| panic!(
                    "no descriptor for {sysno}"
                )),
                Phase1Class::Refused("a syscall phase 1 has not placed"),
                "{sysno}"
            );
        }
    }

    /// The kernel's account of a descriptor: a procfs file of a safe kind is
    /// a safe snapshot on a pseudo file system; an ordinary file is neither;
    /// a path whose lexical spelling names a safe procfs file but whose
    /// target is another procfs file is on procfs but not a safe snapshot.
    #[test]
    fn sigalrm_phase1_provenance_comes_from_the_kernel() {
        use std::os::fd::AsRawFd;

        let pid = std::process::id() as i32;
        let uptime = std::fs::File::open("/proc/uptime").unwrap();
        assert_eq!(
            kernel_provenance(pid, uptime.as_raw_fd()),
            Phase1Provenance {
                pseudo_fs: true,
                safe_snapshot: true,
            }
        );

        let dir = std::env::temp_dir().join(format!("sigalrm-phase1-{pid}"));
        let inner = dir.join("p/q");
        std::fs::create_dir_all(&inner).unwrap();
        let ordinary = std::fs::File::create(dir.join("ordinary")).unwrap();
        assert_eq!(
            kernel_provenance(pid, ordinary.as_raw_fd()),
            Phase1Provenance::default()
        );

        // `/proc/self/fd/N/../../schedstat` normalizes lexically to
        // `/proc/self/schedstat`, but the kernel resolves it through the
        // directory N names, to a symlink to another procfs file.
        std::os::unix::fs::symlink("/proc/meminfo", dir.join("schedstat")).unwrap();
        let held = std::fs::File::open(&inner).unwrap();
        let alias = format!("/proc/self/fd/{}/../../schedstat", held.as_raw_fd());
        let aliased = std::fs::File::open(&alias).unwrap();
        assert_eq!(
            kernel_provenance(pid, aliased.as_raw_fd()),
            Phase1Provenance {
                pseudo_fs: true,
                safe_snapshot: false,
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();

        // A whole disk's statistics, through its /sys/block alias, where the
        // host has one.
        if let Some(disk) = std::fs::read_dir("/sys/block")
            .ok()
            .and_then(|mut entries| entries.next())
            .and_then(Result::ok)
        {
            let stat = std::fs::File::open(disk.path().join("stat")).unwrap();
            assert!(kernel_provenance(pid, stat.as_raw_fd()).safe_snapshot);
        }
    }

    /// A descriptor's mount comes from its fdinfo `mnt_id` and that mount's
    /// line in mountinfo; a bind mount of one procfs object over another's
    /// name has its own line, whose root is the bound object, not `/`.
    #[test]
    fn sigalrm_phase1_mount_provenance_parses_fdinfo_and_mountinfo() {
        assert_eq!(
            fdinfo_mount_id("pos:\t0\nflags:\t0100000\nmnt_id:\t15570\nino:\t4026\n"),
            Some(15570)
        );
        assert_eq!(fdinfo_mount_id("pos:\t0\n"), None);
        let mountinfo = "\
15522 1 253:1 / / rw,relatime shared:1 - xfs /dev/root rw
15532 15522 0:23 / /sys rw,nosuid,nodev,noexec,relatime shared:5 - sysfs sysfs rw
15570 15522 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
15601 15570 0:22 /driver/rtc /proc/uptime rw,relatime shared:12 - proc proc rw
";
        assert_eq!(mount_root_and_point(mountinfo, 15570), Some(("/", "/proc")));
        assert_eq!(mount_root_and_point(mountinfo, 15532), Some(("/", "/sys")));
        // The bind alias: readlink would report /proc/uptime.
        assert_eq!(
            mount_root_and_point(mountinfo, 15601),
            Some(("/driver/rtc", "/proc/uptime"))
        );
        assert_eq!(mount_root_and_point(mountinfo, 9), None);

        let proc = Some(libc::PROC_SUPER_MAGIC);
        let uptime = Some(Path::new("/proc/uptime"));
        assert!(provenance_from(proc, Some(("/", "/proc")), uptime).safe_snapshot);
        // A bind of /proc/driver/rtc over /proc/uptime: same file system and
        // resolved name, but not the whole mount.
        let bound = provenance_from(proc, Some(("/driver/rtc", "/proc/uptime")), uptime);
        assert!(bound.pseudo_fs && !bound.safe_snapshot);
        // procfs mounted elsewhere, or an unknown mount, is not admitted.
        assert!(!provenance_from(proc, Some(("/", "/mnt/proc")), uptime).safe_snapshot);
        assert!(!provenance_from(proc, None, uptime).safe_snapshot);
        let sysfs = Some(libc::SYSFS_MAGIC);
        let disk = Some(Path::new(
            "/sys/devices/pci0000:00/0000:00:1f.2/block/sda/stat",
        ));
        assert!(provenance_from(sysfs, Some(("/", "/sys")), disk).safe_snapshot);
        assert!(
            !provenance_from(sysfs, Some(("/class/rtc/rtc0", "/sys/devices")), disk).safe_snapshot
        );
    }

    /// The safe snapshot kinds are exactly what the target cells read after
    /// installing their handler; the RTC files, whose capture takes an
    /// interruptible lock, are not, and /proc/kmsg has no snapshot at all.
    #[test]
    fn sigalrm_phase1_safe_snapshots_are_the_cells_files() {
        use std::path::Path;

        use crate::procfs::ProcfsFile;

        let safe = |path: &str| {
            ProcfsFile::from_path(Path::new(path)).map(|file| file.capture_cannot_wait())
        };
        for cells_file in [
            "/proc/uptime",
            "/proc/stat",
            "/proc/softirqs",
            "/proc/1/stat",
            "/proc/1/status",
            "/proc/1/schedstat",
            "/sys/block/sda/stat",
        ] {
            assert_eq!(safe(cells_file), Some(true), "{cells_file}");
        }
        for waits in ["/proc/driver/rtc", "/sys/class/rtc/rtc0/time"] {
            assert_eq!(safe(waits), Some(false), "{waits}");
        }
        assert_eq!(safe("/proc/kmsg"), None);
    }

    /// Descriptor I/O and seeks run only on a memfd or a safe snapshot;
    /// stdout and stderr writes run (with `handle_write`'s due check), other
    /// stdio use does not; `close` admits only a memfd or a recorded regular
    /// file or directory that is not stdio; only the two placed `ioctl`
    /// requests run; and the open verdict follows the looked-up kind.
    #[test]
    fn sigalrm_phase1_descriptor_and_argument_conditions() {
        let facts = |ty, stdio: Option<Device>, mode, safe_snapshot| {
            Some(FdFacts {
                ty,
                container_stdin: matches!(stdio, Some(Device::ContainerStdin)),
                container_output: matches!(
                    stdio,
                    Some(Device::ContainerStdout | Device::ContainerStderr)
                ),
                mode,
                safe_snapshot,
                // A safe snapshot is on procfs or sysfs by its provenance.
                pseudo_fs: safe_snapshot,
            })
        };
        // A descriptor the kernel places on procfs or sysfs, not a snapshot.
        let on_pseudo_fs = |stdio: Option<Device>, mode| {
            Some(FdFacts {
                ty: FdType::Regular,
                container_stdin: matches!(stdio, Some(Device::ContainerStdin)),
                container_output: matches!(
                    stdio,
                    Some(Device::ContainerStdout | Device::ContainerStderr)
                ),
                mode,
                safe_snapshot: false,
                pseudo_fs: true,
            })
        };
        let reg = Some(libc::S_IFREG | 0o644);
        let dir = Some(libc::S_IFDIR | 0o755);
        let chr = Some(libc::S_IFCHR | 0o666);
        for (case, write, allowed) in [
            (facts(FdType::Memfd, None, None, false), true, true),
            // A safe snapshot (every file the cells read) is admitted.
            (facts(FdType::Regular, None, reg, true), false, true),
            // A regular mode is no proof: /proc/kmsg is S_IFREG and waits.
            (facts(FdType::Regular, None, reg, false), false, false),
            (facts(FdType::Regular, None, dir, false), false, false),
            (facts(FdType::Regular, None, chr, false), true, false),
            (facts(FdType::Regular, None, None, false), false, false),
            (facts(FdType::Pipe, None, None, false), false, false),
            (facts(FdType::Socket, None, None, false), true, false),
            (facts(FdType::Signalfd, None, None, false), false, false),
            (facts(FdType::Eventfd, None, None, false), false, false),
            (facts(FdType::Timerfd, None, None, false), false, false),
            (facts(FdType::Inotify, None, None, false), false, false),
            (facts(FdType::Userfaultfd, None, None, false), false, false),
            (facts(FdType::Pidfd, None, None, false), false, false),
            (facts(FdType::Epoll, None, None, false), false, false),
            (
                facts(FdType::Regular, Some(Device::ContainerStdout), None, false),
                true,
                true,
            ),
            (
                facts(FdType::Regular, Some(Device::ContainerStderr), None, false),
                true,
                true,
            ),
            // Stdio's recorded mode is startup metadata, never proof (design
            // closure 1).
            (
                facts(FdType::Regular, Some(Device::ContainerStdout), reg, true),
                false,
                false,
            ),
            (
                facts(FdType::Regular, Some(Device::ContainerStdin), reg, true),
                false,
                false,
            ),
            (None, true, false),
        ] {
            let class = descriptor_class(write, case);
            assert_eq!(
                class == Phase1Class::Allowed,
                allowed,
                "{case:?} write={write}: {class:?}"
            );
        }

        let class = |sysno, values, fd| classify(sysno, &args(values), move |_| fd);
        assert!(matches!(
            class(
                Sysno::read,
                [7, 0, 0, 0, 0, 0],
                facts(FdType::Signalfd, None, None, false)
            ),
            Phase1Class::Refused(_)
        ));
        // lseek follows the read rule: a live seq_file seek runs its show.
        assert_eq!(
            class(
                Sysno::lseek,
                [3, 1, 0, 0, 0, 0],
                facts(FdType::Regular, None, reg, true)
            ),
            Phase1Class::Allowed
        );
        assert!(matches!(
            class(
                Sysno::lseek,
                [3, 1, 0, 0, 0, 0],
                facts(FdType::Regular, None, reg, false)
            ),
            Phase1Class::Refused(_)
        ));

        for (case, allowed) in [
            (facts(FdType::Memfd, None, None, false), true),
            // procfs and sysfs release callbacks never wait.
            (on_pseudo_fs(None, reg), true),
            (on_pseudo_fs(None, dir), true),
            (facts(FdType::Regular, None, reg, true), true),
            // A regular file elsewhere may be on a file system whose last
            // close waits for its server (Coda, NFS).
            (facts(FdType::Regular, None, reg, false), false),
            (facts(FdType::Regular, None, dir, false), false),
            (facts(FdType::Regular, None, chr, false), false),
            (facts(FdType::Regular, None, None, false), false),
            (on_pseudo_fs(Some(Device::ContainerStdout), reg), false),
            (on_pseudo_fs(Some(Device::ContainerStdin), reg), false),
            (facts(FdType::Socket, None, None, false), false),
            (facts(FdType::Pipe, None, None, false), false),
            (None, false),
        ] {
            assert_eq!(
                close_class(case) == Phase1Class::Allowed,
                allowed,
                "{case:?}"
            );
        }

        // No ioctl is placed: the cells' ioctl calls precede their handler,
        // and a device's ioctl may take an interruptible lock first.
        for request in [
            libc::TCGETS,
            libc::TIOCGWINSZ,
            libc::TCSETSW,
            libc::TCSETSF,
            libc::TCSBRK,
            libc::FIONREAD,
        ] {
            assert_eq!(
                class(Sysno::ioctl, [1, request as usize, 0, 0, 0, 0], None),
                Phase1Class::Refused("a syscall phase 1 has not placed"),
                "{request:#x}"
            );
        }

        let stat_with = |mode| {
            // SAFETY: an all-zero `stat` is a valid value.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            stat.st_mode = mode;
            stat
        };
        for nonblocking in [false, true] {
            for device in [libc::S_IFCHR, libc::S_IFBLK] {
                assert!(open_refused(Ok(stat_with(device | 0o666)), nonblocking));
            }
            for never in [libc::S_IFREG, libc::S_IFDIR, libc::S_IFSOCK] {
                assert!(!open_refused(Ok(stat_with(never | 0o644)), nonblocking));
            }
        }
        assert!(open_refused(Ok(stat_with(libc::S_IFIFO | 0o644)), false));
        assert!(!open_refused(Ok(stat_with(libc::S_IFIFO | 0o644)), true));
        for passes in [
            reverie::Errno::ENOENT,
            reverie::Errno::ENOTDIR,
            reverie::Errno::ELOOP,
            reverie::Errno::ENAMETOOLONG,
            reverie::Errno::EFAULT,
        ] {
            assert!(!open_refused(Err(passes), false), "{passes}");
        }
        for unknown in [
            reverie::Errno::EACCES,
            reverie::Errno::ENOMEM,
            reverie::Errno::EINTR,
            reverie::Errno::EIO,
            reverie::Errno::EOVERFLOW,
        ] {
            assert!(open_refused(Err(unknown), false), "{unknown}");
        }
        assert!(is_container_output(&ResourceID::Device(
            Device::ContainerStdout
        )));
        assert!(is_container_output(&ResourceID::Device(
            Device::ContainerStderr
        )));
        assert!(!is_container_output(&ResourceID::Device(
            Device::ContainerStdin
        )));
    }
}
