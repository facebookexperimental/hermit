/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's decision on a lookup of another guest process's `cmdline`,
//! descriptor links, `fdinfo` and descriptor directories where the guest's
//! threads are not host tasks (`syscalls::files::other_process_procfs`): the
//! backend declares `provides_process_signal_control`, as KVM does, the
//! guest's requests reach a real global tool whose process table holds the
//! tasks, and the kernel beneath answers the one call it is given with
//! `reply`, after the transient page that call's respelled path is put in.

use std::ffi::CStr;
use std::ffi::CString;

use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use super::*;

/// The caller's process, whose leader makes every call.
const LEADER: DetTid = DetTid::from_raw(17);
/// Another thread of the caller's process.
const WORKER: DetTid = DetTid::from_raw(19);
/// Another guest process.
const OTHER: DetTid = DetTid::from_raw(21);
/// A thread of the other process.
const OTHER_WORKER: DetTid = DetTid::from_raw(23);
/// A number no task of the table holds.
const GONE: i32 = 25;
/// The caller's descriptor on `/proc`, served on a placeholder.
const PROC: i32 = 40;

/// The leader of process `LEADER`, whose requests reach `global`. The kernel
/// beneath gives each transient page Detcore maps (`map_transient`) real
/// memory of this test process, and answers every other call with `reply`,
/// recording it.
struct ProcfsGuest<'a> {
    config: &'a Config,
    global: &'a GlobalState,
    thread: crate::ThreadState<()>,
    reply: Result<i64, Errno>,
    injected: Vec<(Sysno, SyscallArgs)>,
}

struct NoStack;
struct NoStackGuard;
impl Drop for NoStackGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for NoStack {
    type StackGuard = NoStackGuard;
    fn size(&self) -> usize {
        panic!("these lookups use no guest stack")
    }
    fn capacity(&self) -> usize {
        panic!("these lookups use no guest stack")
    }
    fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
        panic!("these lookups use no guest stack")
    }
    fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
        panic!("these lookups use no guest stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("these lookups use no guest stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for ProcfsGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.global
            .receive_rpc(Tid::from_raw(LEADER.as_raw()), message)
            .await
    }
    fn config(&self) -> &Config {
        self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for ProcfsGuest<'_> {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = NoStack;

    fn tid(&self) -> Tid {
        Tid::from_raw(LEADER.as_raw())
    }
    fn pid(&self) -> Tid {
        Tid::from_raw(LEADER.as_raw())
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        reverie::syscalls::LocalMemory::new()
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("these lookups read no registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("these lookups use no guest stack")
    }
    async fn daemonize(&mut self) {
        panic!("these lookups do not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        let (number, args) = call.into_parts();
        match number {
            Sysno::mmap => {
                assert!(args.arg1 <= 4096, "a respelled path fits a page");
                Ok(Box::leak(vec![0u8; 4096].into_boxed_slice()).as_ptr() as i64)
            }
            Sysno::munmap => Ok(0),
            _ => {
                self.injected.push((number, args));
                self.reply
            }
        }
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("these lookups do not retire the guest")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("these lookups set no timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("these lookups set no timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("these lookups read no clock")
    }
}

/// A fail-closed configuration whose policy returns the typed error, on a
/// backend whose guest threads are not host tasks (or are, with `kvm`
/// false), which serves `/proc` directories on placeholders.
fn config(kvm: bool) -> Config {
    Config {
        sequentialize_threads: false,
        panic_on_unsupported_syscalls: true,
        exit_on_unsupported_syscall: true,
        shutdown_on_unsupported_syscall: false,
        directory_placeholder: Some(std::path::PathBuf::from("/placeholder")),
        ..Config::default()
    }
    .with_backend(|backend| backend.provides_process_signal_control = kvm)
}

/// A global tool whose table holds the caller's process (`LEADER` and
/// `WORKER`) and another process (`OTHER` and `OTHER_WORKER`), and no task
/// numbered `GONE` or 1: pid 1 is Hermit's own init.
fn state(config: &Config) -> GlobalState {
    let state = GlobalState::initialize(config, false);
    {
        let mut sched = state.sched.lock().unwrap();
        sched.thread_tree.add_child(LEADER, LEADER, true);
        sched.thread_tree.add_child(LEADER, WORKER, false);
        sched.thread_tree.add_child(LEADER, OTHER, true);
        sched.thread_tree.add_child(OTHER, OTHER_WORKER, false);
    }
    for tid in [LEADER, WORKER, OTHER, OTHER_WORKER] {
        install_test_registration(&state, tid, Ivar::new());
    }
    state
}

/// What Detcore answers `sysno`, whose path is `path` at argument
/// `path_argument` and whose other arguments are `args` (the path's place
/// filled in here), on the backend `kvm` names, with the kernel answering the
/// call with `reply`: the answer, and the paths of the calls that reached the
/// kernel beneath.
async fn decide(
    kvm: bool,
    sysno: Sysno,
    path: &str,
    path_argument: usize,
    args: [usize; 6],
    reply: Result<i64, Errno>,
) -> (Option<Result<i64, Error>>, Vec<(Sysno, String)>) {
    decide_with_descriptor(kvm, sysno, path, path_argument, args, reply, None).await
}

/// [`decide`], with the caller also holding `descriptor`, a number and the
/// path Detcore recorded for it, when one is given.
async fn decide_with_descriptor(
    kvm: bool,
    sysno: Sysno,
    path: &str,
    path_argument: usize,
    mut args: [usize; 6],
    reply: Result<i64, Errno>,
    descriptor: Option<(i32, &str)>,
) -> (Option<Result<i64, Error>>, Vec<(Sysno, String)>) {
    let config = config(kvm);
    let global = state(&config);
    let tool = <Detcore as reverie::Tool>::new(Tid::from_raw(LEADER.as_raw()), &config);
    let mut thread = crate::ThreadState::new(LEADER, &config, ());
    thread.detpid = Some(LEADER);
    thread
        .add_fd(
            PROC,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            crate::fd::FdType::Regular,
            None,
        )
        .unwrap();
    thread
        .with_detfd(PROC, |detfd| {
            detfd.set_path(std::path::PathBuf::from("/proc"));
            detfd.set_placeholder(crate::fd::PlaceholderState {
                directory: crate::procfs::fd_directory::PlaceholderDirectory::Root,
                tag: None,
                position: 0,
                path_only: false,
            });
        })
        .unwrap();
    if let Some((fd, recorded)) = descriptor {
        thread
            .add_fd(fd, OFlag::O_RDONLY, crate::fd::FdType::Regular, None)
            .unwrap();
        thread
            .with_detfd(fd, |detfd| {
                detfd.set_path(std::path::PathBuf::from(recorded))
            })
            .unwrap();
    }
    let mut guest = ProcfsGuest {
        config: &config,
        global: &global,
        thread,
        reply,
        injected: Vec::new(),
    };
    let spelled = CString::new(path).unwrap();
    args[path_argument] = spelled.as_ptr() as usize;
    let call = Syscall::from_raw(
        sysno,
        SyscallArgs::new(args[0], args[1], args[2], args[3], args[4], args[5]),
    );
    let answer = tool.other_process_procfs_answer(&mut guest, &call).await;
    let reached = guest
        .injected
        .iter()
        .map(|(number, args)| {
            let values = [
                args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
            ];
            // `utime` and `utimes` reach the kernel as `utimensat`, whose
            // path is argument 1.
            let index = if *number == Sysno::utimensat && sysno != Sysno::utimensat {
                1
            } else {
                path_argument
            };
            // SAFETY: Detcore points the path argument at a NUL-terminated
            // spelling in a page this guest leaked for it.
            let respelled = unsafe { CStr::from_ptr(values[index] as *const libc::c_char) };
            (*number, respelled.to_string_lossy().into_owned())
        })
        .collect();
    (answer, reached)
}

/// `open(path, O_RDONLY)` as Detcore decides it, the kernel answering the
/// respelled call with `reply`.
async fn open(
    path: &str,
    reply: Result<i64, Errno>,
) -> (Option<Result<i64, Error>>, Vec<(Sysno, String)>) {
    decide(
        true,
        Sysno::openat,
        path,
        1,
        [libc::AT_FDCWD as usize, 0, libc::O_RDONLY as usize, 0, 0, 0],
        reply,
    )
    .await
}

/// The answer is the fail-closed policy's typed refusal of `sysno`.
fn assert_refused(answer: &Option<Result<i64, Error>>, sysno: Sysno, path: &str) {
    match answer {
        Some(Err(Error::Tool(error))) => assert_eq!(
            error
                .downcast_ref::<crate::UnsupportedSyscallError>()
                .expect("the policy's named refusal")
                .0,
            sysno,
            "{path}"
        ),
        other => panic!("{path}: expected the named refusal, got {other:?}"),
    }
}

/// Another guest process's `cmdline`, descriptor link (and a path below
/// one), `fdinfo` and descriptor directories, by its number, by a thread's
/// number, through `task/<tid>`, through `fdinfo/..`, and through the
/// caller's own descriptor on `/proc` (`/proc/self/fd/40`): the kernel is
/// given the call with the process's directory spelled as a name procfs does
/// not have, and once it answers ENOENT the call is refused by name.
#[tokio::test]
async fn another_guest_process_files_are_refused_once_the_kernel_reaches_the_walk() {
    let other = OTHER.as_raw();
    for (path, respelled) in [
        (format!("/proc/{other}/cmdline"), "/proc/--/cmdline"),
        (format!("/proc/{other}/fd/1"), "/proc/--/fd/1"),
        (format!("/proc/{other}/fd/1/below"), "/proc/--/fd/1/below"),
        (format!("/proc/{other}/fdinfo/1"), "/proc/--/fdinfo/1"),
        (format!("/proc/{other}/fd"), "/proc/--/fd"),
        (format!("/proc/{other}/fdinfo/"), "/proc/--/fdinfo/"),
        (
            format!("/proc/{other}/task/{}/cmdline", OTHER_WORKER.as_raw()),
            "/proc/--/task/23/cmdline",
        ),
        (
            format!("/proc/{}/cmdline", OTHER_WORKER.as_raw()),
            "/proc/--/cmdline",
        ),
        (
            format!("/proc/{other}/fdinfo/../cmdline"),
            "/proc/--/cmdline",
        ),
        (
            format!("/proc/self/fd/{PROC}/{other}/cmdline"),
            "/proc/--/cmdline",
        ),
    ] {
        let (answer, reached) = open(&path, Err(Errno::ENOENT)).await;
        assert_refused(&answer, Sysno::openat, &path);
        assert_eq!(
            reached,
            vec![(Sysno::openat, respelled.to_owned())],
            "{path}"
        );
    }
}

/// A path that goes on past another guest process's `cmdline`, a regular
/// file, gets Linux's ENOTDIR once the kernel reaches the walk: through a
/// further component for every call with one path (and as the first path of
/// a call with two, below), and after slashes alone for a call that looks
/// its path up whole. A call that creates or removes that last name, and
/// every path past another process's `fdinfo/<N>`, whose answer depends on
/// the descriptors that process holds, stay refused by name; past a task the
/// table does not hold, ENOENT. Before, `cmdline/` was refused and
/// `cmdline/x` reached the backend, whose answer depended on whether a host
/// task had that number.
#[tokio::test]
async fn a_path_past_another_process_cmdline_is_not_a_directory() {
    let other = OTHER.as_raw();
    let worker = OTHER_WORKER.as_raw();
    let mkdirat = [libc::AT_FDCWD as usize, 0, 0o700, 0, 0, 0];
    let creat = [
        libc::AT_FDCWD as usize,
        0,
        (libc::O_WRONLY | libc::O_CREAT) as usize,
        0o600,
        0,
        0,
    ];
    for (sysno, args, path) in [
        (Sysno::openat, None, format!("/proc/{other}/cmdline/x")),
        (Sysno::openat, None, format!("/proc/{other}/cmdline/.")),
        (Sysno::openat, None, format!("/proc/{other}/cmdline/..")),
        (Sysno::openat, None, format!("/proc/{other}/cmdline/")),
        (Sysno::openat, None, format!("/proc/{other}/cmdline//")),
        (
            Sysno::openat,
            None,
            format!("/proc/{other}/task/{worker}/cmdline/"),
        ),
        (Sysno::openat, None, format!("/proc/{other}/fd/../cmdline/")),
        (
            Sysno::openat,
            Some(creat),
            format!("/proc/{other}/cmdline/x"),
        ),
        (
            Sysno::mkdirat,
            Some(mkdirat),
            format!("/proc/{other}/cmdline/."),
        ),
    ] {
        let args = args.unwrap_or([libc::AT_FDCWD as usize, 0, libc::O_RDONLY as usize, 0, 0, 0]);
        let (answer, reached) = decide(true, sysno, &path, 1, args, Err(Errno::ENOENT)).await;
        assert!(
            matches!(answer, Some(Err(Error::Errno(Errno::ENOTDIR)))),
            "{sysno} {path}: {answer:?}"
        );
        assert_eq!(reached.len(), 1, "{path}: {reached:?}");
        assert!(reached[0].1.starts_with("/proc/--/"), "{path}: {reached:?}");
    }
    for (sysno, args, path) in [
        (Sysno::openat, creat, format!("/proc/{other}/cmdline/")),
        (Sysno::mkdirat, mkdirat, format!("/proc/{other}/cmdline/")),
        (
            Sysno::openat,
            [libc::AT_FDCWD as usize, 0, libc::O_RDONLY as usize, 0, 0, 0],
            format!("/proc/{other}/fdinfo/1/x"),
        ),
        (
            Sysno::openat,
            [libc::AT_FDCWD as usize, 0, libc::O_RDONLY as usize, 0, 0, 0],
            format!("/proc/{other}/fdinfo/1/"),
        ),
    ] {
        let (answer, reached) = decide(true, sysno, &path, 1, args, Err(Errno::ENOENT)).await;
        assert_refused(&answer, sysno, &path);
        assert_eq!(reached.len(), 1, "{path}: {reached:?}");
    }
    let path = format!("/proc/{GONE}/cmdline/x");
    let (answer, reached) = open(&path, Err(Errno::ENOENT)).await;
    assert!(
        matches!(answer, Some(Err(Error::Errno(Errno::ENOENT)))),
        "{path}: {answer:?}"
    );
    assert_eq!(reached.len(), 1, "{path}: {reached:?}");
}

/// The second path of a call with two (`link`, `linkat`, `rename`,
/// `renameat`, `renameat2`), past another guest process's `cmdline` or past
/// a task the table does not hold, is refused by name once the kernel
/// answers ENOENT, or a permission refusal Detcore takes for one: Linux walks
/// the first path before it (`do_linkat`, `do_renameat2`), and that ENOENT
/// may be the first path's, whose own answer Linux would give. Any other
/// answer is the kernel's, as before: here the first path's ENOTDIR. As the
/// call's first path, it gets Linux's ENOTDIR or ENOENT, as for a call with
/// one path. Before, the second path got ENOTDIR or ENOENT whatever had
/// failed: `link` of a missing source to `/proc/<pid>/cmdline/x` answered
/// ENOTDIR where Linux answers ENOENT.
#[tokio::test]
async fn a_second_path_past_another_process_cmdline_is_refused_by_name() {
    let past = format!("/proc/{}/cmdline/x", OTHER.as_raw());
    let gone = format!("/proc/{GONE}/cmdline/x");
    let at = [libc::AT_FDCWD as usize, 0, libc::AT_FDCWD as usize, 0, 0, 0];
    // Each call, the argument of its first path, of its second, and its
    // other arguments.
    for (sysno, first, second, args) in [
        (Sysno::link, 0, 1, [0; 6]),
        (Sysno::linkat, 1, 3, at),
        (Sysno::rename, 0, 1, [0; 6]),
        (Sysno::renameat, 1, 3, at),
        (Sysno::renameat2, 1, 3, at),
    ] {
        for path in [&past, &gone] {
            for reply in [Errno::ENOENT, Errno::EACCES] {
                let (answer, reached) = decide(true, sysno, path, second, args, Err(reply)).await;
                assert_refused(&answer, sysno, path);
                assert_eq!(reached.len(), 1, "{sysno} {path}: {reached:?}");
                assert!(
                    reached[0].1.starts_with("/proc/--/"),
                    "{sysno} {path}: {reached:?}"
                );
            }
            let (answer, _) = decide(true, sysno, path, second, args, Err(Errno::ENOTDIR)).await;
            assert!(
                matches!(answer, Some(Err(Error::Errno(Errno::ENOTDIR)))),
                "{sysno} {path}: {answer:?}"
            );
        }
        let (answer, _) = decide(true, sysno, &past, first, args, Err(Errno::ENOENT)).await;
        assert!(
            matches!(answer, Some(Err(Error::Errno(Errno::ENOTDIR)))),
            "{sysno} {past} first: {answer:?}"
        );
        let (answer, _) = decide(true, sysno, &gone, first, args, Err(Errno::ENOENT)).await;
        assert!(
            matches!(answer, Some(Err(Error::Errno(Errno::ENOENT)))),
            "{sysno} {gone} first: {answer:?}"
        );
    }
}

/// Linux checks a call's own arguments before its walk: the kernel's refusal
/// of them is the answer, for another process's file and for a task the
/// table does not hold alike. Before,
/// `access(.., 8)` and `readlink(.., 0)` of an unheld task's file answered
/// ENOENT.
#[tokio::test]
async fn an_argument_the_kernel_refuses_before_the_walk_is_refused_as_linux_refuses_it() {
    for path in [
        format!("/proc/{}/cmdline", OTHER.as_raw()),
        format!("/proc/{GONE}/cmdline"),
    ] {
        let (answer, reached) = decide(
            true,
            Sysno::access,
            &path,
            0,
            [0, 8, 0, 0, 0, 0],
            Err(Errno::EINVAL),
        )
        .await;
        assert!(
            matches!(answer, Some(Err(Error::Errno(Errno::EINVAL)))),
            "{path}: {answer:?}"
        );
        assert_eq!(reached.len(), 1, "{path}: {reached:?}");
        let (answer, _) = decide(
            true,
            Sysno::readlink,
            &path.replace("cmdline", "fd/1"),
            0,
            [0, 0, 0, 0, 0, 0],
            Err(Errno::EINVAL),
        )
        .await;
        assert!(
            matches!(answer, Some(Err(Error::Errno(Errno::EINVAL)))),
            "{path}: {answer:?}"
        );
    }
}

/// The errors Linux decides from a call's own arguments before any walk are
/// decided from them, before the respelled call, whatever the kernel beneath
/// would answer it: a mknod of a directory is EPERM, which the kernel's own
/// EPERM, taken for its refusal of the respelled name, used to turn into
/// ENOTDIR past another process's cmdline and ENOENT for a task the table
/// does not hold; renameat2's RENAME_EXCHANGE with RENAME_NOREPLACE is EINVAL
/// with the path first or second, where the second used to be refused by
/// name; an empty symlink target is ENOENT; and utimensat's two UTIME_OMIT
/// answer 0. A mknod of a device, which Linux checks only after its walk,
/// still gets the walk's answer, the kernel's EPERM taken for its refusal.
#[tokio::test]
async fn an_argument_linux_decides_before_the_walk_is_decided_from_the_arguments() {
    let past = format!("/proc/{}/cmdline/x", OTHER.as_raw());
    let gone = format!("/proc/{GONE}/cmdline");
    let at = libc::AT_FDCWD as usize;
    let directory = (libc::S_IFDIR | 0o600) as usize;
    let exchange_noreplace = (libc::RENAME_EXCHANGE | libc::RENAME_NOREPLACE) as usize;
    let empty = CString::new("").unwrap();
    let omit = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    ];
    let decided = |answer: &Option<Result<i64, Error>>, expected: Result<i64, Errno>| match (
        answer, expected,
    ) {
        (Some(Ok(value)), Ok(wanted)) => *value == wanted,
        (Some(Err(Error::Errno(errno))), Err(wanted)) => *errno == wanted,
        _ => false,
    };
    for path in [&past, &gone] {
        for reply in [Err(Errno::EPERM), Err(Errno::ENOENT), Ok(0)] {
            let mut cases = vec![
                (
                    Sysno::mknodat,
                    1,
                    [at, 0, directory, 0, 0, 0],
                    Err(Errno::EPERM),
                ),
                (
                    Sysno::renameat2,
                    1,
                    [at, 0, at, 0, exchange_noreplace, 0],
                    Err(Errno::EINVAL),
                ),
                (
                    Sysno::renameat2,
                    3,
                    [at, 0, at, 0, exchange_noreplace, 0],
                    Err(Errno::EINVAL),
                ),
                (
                    Sysno::symlinkat,
                    2,
                    [empty.as_ptr() as usize, at, 0, 0, 0, 0],
                    Err(Errno::ENOENT),
                ),
                (
                    Sysno::utimensat,
                    1,
                    [at, 0, omit.as_ptr() as usize, 0, 0, 0],
                    Ok(0),
                ),
            ];
            #[cfg(target_arch = "x86_64")]
            cases.push((
                Sysno::mknod,
                0,
                [0, directory, 0, 0, 0, 0],
                Err(Errno::EPERM),
            ));
            for (sysno, argument, args, expected) in cases {
                let (answer, reached) = decide(true, sysno, path, argument, args, reply).await;
                assert!(
                    decided(&answer, expected),
                    "{sysno} {path} at {argument}, the kernel answering {reply:?}: {answer:?}"
                );
                assert!(
                    reached.is_empty(),
                    "{sysno} {path}: the call reached the kernel: {reached:?}"
                );
            }
        }
    }
    let device = (libc::S_IFCHR | 0o600) as usize;
    for (path, expected) in [(&past, Errno::ENOTDIR), (&gone, Errno::ENOENT)] {
        let (answer, reached) = decide(
            true,
            Sysno::mknodat,
            path,
            1,
            [at, 0, device, 0, 0, 0],
            Err(Errno::EPERM),
        )
        .await;
        assert!(
            matches!(answer, Some(Err(Error::Errno(errno))) if errno == expected),
            "a device at {path}: {answer:?}"
        );
        assert_eq!(reached.len(), 1, "a device at {path}: {reached:?}");
    }
}

/// An empty pathname on a descriptor open on another process's `/proc` file,
/// or one of a task the table no longer holds, is the descriptor itself
/// (with AT_EMPTY_PATH) or nothing (without it), never a lookup of the path
/// Detcore recorded for the descriptor. execveat gets Linux's answer without
/// reaching the kernel beneath: EINVAL for an unknown flag first, ENOENT for
/// the empty path without AT_EMPTY_PATH, and with it EACCES, since a procfs
/// file or directory cannot be executed; a descriptor opened through another
/// process's descriptor link is refused by name. statx's reserved mask bit
/// is still EINVAL, and the descriptor's other operations are left to its
/// handlers and the backend. Before, the recorded path, with a slash after
/// it, was looked up: ENOENT for every execveat, ENOTDIR for statx.
#[tokio::test]
async fn an_empty_pathname_is_the_descriptor_not_a_lookup() {
    const DESCRIPTOR: i32 = 41;
    let fd = DESCRIPTOR as usize;
    let other = OTHER.as_raw();
    let cmdline = format!("/proc/{other}/cmdline");
    let gone = format!("/proc/{GONE}/cmdline");
    let directory = format!("/proc/{other}/fd");
    let empty = libc::AT_EMPTY_PATH as usize;
    let on = |recorded: String, sysno: Sysno, argument: usize, args: [usize; 6]| async move {
        decide_with_descriptor(
            true,
            sysno,
            "",
            argument,
            args,
            Err(Errno::ENOENT),
            Some((DESCRIPTOR, recorded.as_str())),
        )
        .await
    };
    let errno = |answer: &Option<Result<i64, Error>>| match answer {
        Some(Err(Error::Errno(errno))) => Some(*errno),
        _ => None,
    };
    for recorded in [&cmdline, &gone, &directory] {
        for (flags, expected) in [
            (empty | 1, Errno::EINVAL),
            (1, Errno::EINVAL),
            (0, Errno::ENOENT),
            (empty, Errno::EACCES),
        ] {
            let (answer, reached) = on(
                recorded.clone(),
                Sysno::execveat,
                1,
                [fd, 0, 0, 0, flags, 0],
            )
            .await;
            assert_eq!(
                errno(&answer),
                Some(expected),
                "execveat on {recorded}, flags {flags:#x}: {answer:?}"
            );
            assert!(reached.is_empty(), "{recorded}: {reached:?}");
        }
        let (answer, reached) = on(
            recorded.clone(),
            Sysno::statx,
            1,
            [fd, 0, empty, 0x8000_0000, 0, 0],
        )
        .await;
        assert_eq!(
            errno(&answer),
            Some(Errno::EINVAL),
            "{recorded}: {answer:?}"
        );
        assert!(reached.is_empty(), "{recorded}: {reached:?}");
        for (sysno, args) in [
            (
                Sysno::statx,
                [fd, 0, empty, libc::STATX_BASIC_STATS as usize, 0, 0],
            ),
            (
                Sysno::statx,
                [fd, 0, 0, libc::STATX_BASIC_STATS as usize, 0, 0],
            ),
            (Sysno::newfstatat, [fd, 0, 0, empty, 0, 0]),
            (Sysno::fchownat, [fd, 0, usize::MAX, usize::MAX, empty, 0]),
        ] {
            let (answer, reached) = on(recorded.clone(), sysno, 1, args).await;
            assert!(answer.is_none(), "{sysno} on {recorded}: {answer:?}");
            assert!(reached.is_empty(), "{sysno} on {recorded}: {reached:?}");
        }
    }
    let link = format!("/proc/{other}/fd/1");
    let (answer, reached) = on(link.clone(), Sysno::execveat, 1, [fd, 0, 0, 0, empty, 0]).await;
    assert_refused(&answer, Sysno::execveat, &link);
    assert!(reached.is_empty(), "{link}: {reached:?}");
}

/// `utime` and `utimes`, which KVM's backend does not implement, have their
/// times checked from the arguments (EFAULT for times that cannot be read,
/// EINVAL for `utimes`' microseconds out of range) without reaching the
/// kernel beneath, and otherwise reach it as `utimensat`, as Detcore's own
/// handlers issue them: past another process's cmdline the walk's ENOTDIR,
/// for a task the table does not hold ENOENT, and for another process's
/// cmdline the named refusal. Before, the kernel was given the raw call, and
/// KVM's ENOSYS reached the guest as if it were Linux's answer.
#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn utime_and_utimes_reach_the_kernel_as_utimensat() {
    let past = format!("/proc/{}/cmdline/x", OTHER.as_raw());
    let gone = format!("/proc/{GONE}/cmdline");
    let cmdline = format!("/proc/{}/cmdline", OTHER.as_raw());
    let utimbuf = [0i64, 0];
    let timevals = [0i64, 0, 0, 0];
    let out_of_range = [0i64, 1_000_000, 0, 0];
    let unreadable = 8usize;
    let errno = |answer: &Option<Result<i64, Error>>| match answer {
        Some(Err(Error::Errno(errno))) => Some(*errno),
        _ => None,
    };
    for (sysno, times) in [
        (Sysno::utime, utimbuf.as_ptr() as usize),
        (Sysno::utimes, timevals.as_ptr() as usize),
        (Sysno::utime, 0),
        (Sysno::utimes, 0),
    ] {
        for (path, expected) in [(&past, Errno::ENOTDIR), (&gone, Errno::ENOENT)] {
            let (answer, reached) = decide(
                true,
                sysno,
                path,
                0,
                [0, times, 0, 0, 0, 0],
                Err(Errno::ENOENT),
            )
            .await;
            assert_eq!(errno(&answer), Some(expected), "{sysno} {path}: {answer:?}");
            assert_eq!(reached.len(), 1, "{sysno} {path}: {reached:?}");
            assert_eq!(
                reached[0].0,
                Sysno::utimensat,
                "{sysno} {path}: {reached:?}"
            );
            assert!(reached[0].1.starts_with("/proc/--/"), "{reached:?}");
        }
        let (answer, _) = decide(
            true,
            sysno,
            &cmdline,
            0,
            [0, times, 0, 0, 0, 0],
            Err(Errno::ENOENT),
        )
        .await;
        assert_refused(&answer, sysno, &cmdline);
    }
    for (sysno, times, expected) in [
        (Sysno::utime, unreadable, Errno::EFAULT),
        (Sysno::utimes, unreadable, Errno::EFAULT),
        (Sysno::utimes, out_of_range.as_ptr() as usize, Errno::EINVAL),
    ] {
        for path in [&past, &gone, &cmdline] {
            let (answer, reached) = decide(
                true,
                sysno,
                path,
                0,
                [0, times, 0, 0, 0, 0],
                Err(Errno::ENOENT),
            )
            .await;
            assert_eq!(errno(&answer), Some(expected), "{sysno} {path}: {answer:?}");
            assert!(reached.is_empty(), "{sysno} {path}: {reached:?}");
        }
    }
}

/// A task the process table does not hold has no `/proc` directory, as
/// Linux's lookup finds none: ENOENT once the kernel reaches the walk, for
/// the task named, a thread of another process named below it, and a task a
/// `..` walk passes.
#[tokio::test]
async fn a_task_the_table_does_not_hold_has_no_directory() {
    for path in [
        format!("/proc/{GONE}/cmdline"),
        format!("/proc/{GONE}/fd/1"),
        format!("/proc/{}/task/{GONE}/cmdline", OTHER.as_raw()),
        format!("/proc/{GONE}/../{}/cmdline", OTHER.as_raw()),
    ] {
        let (answer, reached) = open(&path, Err(Errno::ENOENT)).await;
        assert!(
            matches!(answer, Some(Err(Error::Errno(Errno::ENOENT)))),
            "{path}: {answer:?}"
        );
        assert_eq!(reached.len(), 1, "{path}: {reached:?}");
    }
}

/// The caller's own process, by its number, a sibling thread's number and
/// `task/<tid>`, and every other spelling Detcore does not decide here,
/// stay with their handlers: nothing is decided and nothing injected.
#[tokio::test]
async fn the_callers_own_process_and_other_files_are_left_to_their_handlers() {
    for path in [
        format!("/proc/{}/cmdline", LEADER.as_raw()),
        format!("/proc/{}/cmdline", WORKER.as_raw()),
        format!("/proc/{}/task/{}/fd/1", LEADER.as_raw(), WORKER.as_raw()),
        format!("/proc/self/fd/{PROC}/{}/cmdline", LEADER.as_raw()),
        "/proc/self/cmdline".to_owned(),
        format!("/proc/{}/status", OTHER.as_raw()),
        "/proc/1/cmdline".to_owned(),
    ] {
        let (answer, reached) = open(&path, Err(Errno::ENOENT)).await;
        assert!(answer.is_none(), "{path}: {answer:?}");
        assert!(reached.is_empty(), "{path}: {reached:?}");
    }
}

/// Hermit's own init is left to the backend, its cmdline, its descriptor
/// links and its descriptor directories alike: nothing is decided and
/// nothing injected. On KVM which of its descriptor entries exist changes
/// with host timing, a known limitation owned by D-PID1
/// (https://github.com/rrnewton/hermit/issues/4071). A descriptor's fdinfo
/// is the exception (`hermits_inits_fdinfo_is_refused_by_name`).
#[tokio::test]
async fn hermits_init_is_left_to_the_backend() {
    for path in [
        "/proc/1/cmdline",
        "/proc/1/fd/3",
        "/proc/1/fd",
        "/proc/1/fdinfo",
        "/proc/1/task/1/fd/3",
    ] {
        let (answer, reached) = open(path, Err(Errno::ENOENT)).await;
        assert!(answer.is_none(), "{path}: {answer:?}");
        assert!(reached.is_empty(), "{path}: {reached:?}");
    }
}

/// A descriptor's fdinfo of Hermit's own init, by pid 1, through `task/1`,
/// past the file, and through `..`: the backend has no fdinfo of its own,
/// and its procfs would answer from the supervisor's descriptor table, host
/// state. So, as for another guest process's, the kernel is given the call
/// with the process's directory spelled as a name procfs does not have, and
/// once it answers ENOENT the call is refused by name.
#[tokio::test]
async fn hermits_inits_fdinfo_is_refused_by_name() {
    for (path, respelled) in [
        ("/proc/1/fdinfo/0", "/proc/-/fdinfo/0"),
        ("/proc/1/task/1/fdinfo/0", "/proc/-/task/1/fdinfo/0"),
        ("/proc/1/fdinfo/0/..", "/proc/-/fdinfo/0/.."),
        ("/proc/self/../1/fdinfo/0", "/proc/-/fdinfo/0"),
    ] {
        let (answer, reached) = open(path, Err(Errno::ENOENT)).await;
        assert_refused(&answer, Sysno::openat, path);
        assert_eq!(
            reached,
            vec![(Sysno::openat, respelled.to_owned())],
            "{path}"
        );
    }
}

/// Where the guest's threads are host tasks, the kernel beneath answers for
/// every process: nothing is decided.
#[tokio::test]
async fn nothing_is_decided_where_guest_threads_are_host_tasks() {
    let path = format!("/proc/{}/cmdline", OTHER.as_raw());
    let (answer, reached) = decide(
        false,
        Sysno::openat,
        &path,
        1,
        [libc::AT_FDCWD as usize, 0, libc::O_RDONLY as usize, 0, 0, 0],
        Err(Errno::ENOENT),
    )
    .await;
    assert!(answer.is_none(), "{answer:?}");
    assert!(reached.is_empty(), "{reached:?}");
}
