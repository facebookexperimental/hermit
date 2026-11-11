//! Starting a program inside an invocation cgroup without copying this
//! process (dh-7n47nt).
//!
//! `std::process::Command` with a `pre_exec` closure, which the cgroup join
//! needed, starts every child with fork(). fork() copies this process's
//! address space while it holds the memory-map lock, and every other thread
//! that page-faults meanwhile waits. In the library's test process (about 330
//! MB, 80-140 threads) each fork took a median 114-135 ms and up to 3 s, and
//! CPU readers and census walks in the same process stalled behind them.
//!
//! Here the child is created with clone3(CLONE_VM | CLONE_VFORK |
//! CLONE_INTO_CGROUP), as glibc's posix_spawn creates its children
//! (`CLONE_VM | CLONE_VFORK` on a stack of their own), with the kernel placing
//! it in the invocation cgroup as it is created. The child shares this
//! process's memory until it execs, and this thread waits until then. So the
//! child runs only [`vfork_child`]: raw system calls on storage this thread
//! prepared beforehand, no allocation, no locks and no libc call, on a stack
//! of its own, never on this thread's.
//!
//! clone3 needs Linux 5.3 and CLONE_INTO_CGROUP 5.7. Where clone3 is refused
//! as a call (ENOSYS, E2BIG or EINVAL, as on older kernels or under a seccomp
//! filter, or EPERM from a filter, which a probe tells apart from a cgroup
//! that refused the child), [`Started::Unusable`] tells the caller to start
//! the program the old way, and the refusal is remembered for the rest of the
//! process, with one line on stderr. A refusal that is about the cgroup itself
//! is a failed join.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::ffi::OsString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

/// What to start, and where.
pub(crate) struct Program<'a> {
    /// The program, then its arguments. A program named without a `/` is
    /// looked up in the `PATH` of its environment, as execvp looks it up.
    pub(crate) argv: &'a [String],
    /// This process's environment with these variables set over it.
    pub(crate) env_overrides: &'a BTreeMap<String, String>,
    pub(crate) cwd: &'a Path,
    /// Descriptors that become the program's stdin, stdout and stderr.
    pub(crate) stdio: [RawFd; 3],
    /// The invocation cgroup's directory, which the program starts in.
    pub(crate) cgroup: RawFd,
}

/// How [`start`] ended.
#[derive(Debug)]
pub(crate) enum Started {
    /// The program runs as process `pid`, in the cgroup.
    Running { pid: u32 },
    /// clone3 cannot start programs in a cgroup here, for `reason`; start the
    /// program another way.
    Unusable { reason: String },
    /// The kernel would not create the child in the cgroup.
    JoinFailed(io::Error),
    /// No child was created, or the child could not set itself up or execute
    /// the program, which then never ran.
    Failed(io::Error),
}

/// Whether clone3 has been refused as a call in this process.
pub(crate) struct Clone3Support {
    unusable: OnceLock<String>,
    /// For a test: answer the first `inject_limit` clone3 calls with this
    /// errno instead of making them.
    #[cfg(test)]
    pub(crate) inject: Option<i32>,
    #[cfg(test)]
    inject_limit: usize,
    /// For a test: answer the EPERM probe with this errno instead of making
    /// it.
    #[cfg(test)]
    inject_probe: Option<i32>,
    /// For a test: the child kills itself before exec.
    #[cfg(test)]
    kill_child_before_exec: bool,
    /// For a test: how many times clone3 was called, or answered for.
    #[cfg(test)]
    pub(crate) attempts: std::sync::atomic::AtomicUsize,
    /// For a test: how many EPERM probes were made, or answered for.
    #[cfg(test)]
    pub(crate) probes: std::sync::atomic::AtomicUsize,
    /// For a test: how many refusal notices went to stderr.
    #[cfg(test)]
    pub(crate) notices: std::sync::atomic::AtomicUsize,
}

impl Clone3Support {
    pub(crate) const fn new() -> Self {
        Self {
            unusable: OnceLock::new(),
            #[cfg(test)]
            inject: None,
            #[cfg(test)]
            inject_limit: 0,
            #[cfg(test)]
            inject_probe: None,
            #[cfg(test)]
            kill_child_before_exec: false,
            #[cfg(test)]
            attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            probes: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            notices: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// For a test: a [`Clone3Support`] with these injections.
    #[cfg(test)]
    const fn injecting(
        inject: Option<i32>,
        inject_limit: usize,
        inject_probe: Option<i32>,
        kill_child_before_exec: bool,
    ) -> Self {
        Self {
            unusable: OnceLock::new(),
            inject,
            inject_limit,
            inject_probe,
            kill_child_before_exec,
            attempts: std::sync::atomic::AtomicUsize::new(0),
            probes: std::sync::atomic::AtomicUsize::new(0),
            notices: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// For a test: a [`Clone3Support`] whose clone3 always fails with
    /// `errno`, without being called, as under a filter that refuses every
    /// clone3: the EPERM probe gets `errno` too.
    #[cfg(test)]
    pub(crate) const fn refusing(errno: i32) -> Self {
        Self::injecting(Some(errno), usize::MAX, Some(errno), false)
    }

    /// For a test: a [`Clone3Support`] whose first clone3 fails with `errno`,
    /// without being called, as a cgroup that refuses one child would make it
    /// fail. The EPERM probe and every later clone3 are real.
    #[cfg(test)]
    pub(crate) const fn refusing_once(errno: i32) -> Self {
        Self::injecting(Some(errno), 1, None, false)
    }

    /// For a test: [`Clone3Support::refusing_once`], with the EPERM probe
    /// answered `probe` instead of made.
    #[cfg(test)]
    pub(crate) const fn refusing_once_with_probe(errno: i32, probe: i32) -> Self {
        Self::injecting(Some(errno), 1, Some(probe), false)
    }

    /// For a test: a [`Clone3Support`] whose children kill themselves with
    /// SIGKILL before they exec.
    #[cfg(test)]
    pub(crate) const fn killing_children_before_exec() -> Self {
        Self::injecting(None, 0, None, true)
    }
}

/// This process's [`Clone3Support`].
pub(crate) static CLONE3: Clone3Support = Clone3Support::new();

/// What an errno from clone3 means. The cgroup errnos are those of Linux
/// v6.12's `cgroup_css_set_fork` (kernel/cgroup/cgroup.c) and what it calls;
/// the others are clone3's own argument checks (kernel/fork.c).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// clone3, or one of its flags, is not available here: an older kernel
    /// (ENOSYS before 5.3; E2BIG or EINVAL for arguments or a flag it does not
    /// know, before 5.7) or a seccomp filter (ENOSYS).
    Unusable,
    /// Either a seccomp filter refused clone3 or the cgroup's write
    /// permission check did (`inode_permission`, for an immutable file or a
    /// security module); [`probe_refuses_clone3`] tells which.
    FilterOrJoin,
    /// The cgroup refused the child: not a cgroup directory (EBADF), not
    /// allowed (EACCES, ENOENT), on a read-only mount (EROFS), removed
    /// (ENODEV), a cgroup that may not hold processes (EBUSY, EOPNOTSUPP).
    Join,
    /// Anything else, such as EAGAIN or ENOMEM: no child was created.
    Failed,
}

fn refusal(errno: i32) -> Refusal {
    match errno {
        libc::ENOSYS | libc::E2BIG | libc::EINVAL => Refusal::Unusable,
        libc::EPERM => Refusal::FilterOrJoin,
        libc::EBADF
        | libc::EACCES
        | libc::ENOENT
        | libc::EROFS
        | libc::ENODEV
        | libc::EBUSY
        | libc::EOPNOTSUPP => Refusal::Join,
        _ => Refusal::Failed,
    }
}

/// Whether clone3 itself is refused here, asked by a call that cannot create
/// a child: clone3 with no arguments, which Linux refuses with EINVAL before
/// it reads anything (`copy_clone_args_from_user`: a size below
/// `CLONE_ARGS_SIZE_VER0`), unless a seccomp filter refuses the call first
/// (EPERM or ENOSYS).
fn probe_refuses_clone3(support: &Clone3Support) -> bool {
    matches!(probe_clone3(support), libc::EPERM | libc::ENOSYS)
}

/// The errno of clone3 with no arguments: EINVAL where clone3 is allowed.
fn probe_clone3(support: &Clone3Support) -> i32 {
    #[cfg(test)]
    {
        support.probes.fetch_add(1, Ordering::Relaxed);
        if let Some(errno) = support.inject_probe {
            return errno;
        }
    }
    #[cfg(not(test))]
    let _ = support;
    let result = unsafe { syscall6(libc::SYS_clone3, [0, 0, 0, 0, 0, 0]) };
    -result as i32
}

/// Remembers that clone3 is unusable here for `reason`, and says so on
/// stderr the first time: every later start in this process forks.
fn remember_unusable(support: &Clone3Support, reason: &str) {
    if support.unusable.set(reason.to_string()).is_ok() {
        #[cfg(test)]
        support.notices.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "manifest-plan: {reason}; invocations in a cgroup start with fork() from now on (dh-7n47nt)"
        );
    }
}

/// Start `program` in its cgroup, unless clone3 is unusable here.
pub(crate) fn start(support: &Clone3Support, program: &Program) -> Started {
    if let Some(reason) = support.unusable.get() {
        return Started::Unusable {
            reason: reason.clone(),
        };
    }
    let prepared = match Prepared::new(support, program) {
        Ok(prepared) => prepared,
        Err(error) => return Started::Failed(error),
    };
    let result = CHILD_STACK.with_borrow_mut(|stack| {
        if stack.is_none() {
            *stack = Some(ChildStack::new()?);
        }
        let stack = stack.as_ref().expect("set above");
        Ok::<i64, io::Error>(clone_into_cgroup(support, &prepared, stack, program.cgroup))
    });
    let result = match result {
        Ok(result) => result,
        Err(error) => return Started::Failed(error),
    };
    if result < 0 {
        let errno = (-result) as i32;
        let error = io::Error::from_raw_os_error(errno);
        let unusable = |error: io::Error| {
            let reason = format!("clone3 with CLONE_INTO_CGROUP was refused: {error}");
            remember_unusable(support, &reason);
            Started::Unusable { reason }
        };
        return match refusal(errno) {
            Refusal::Unusable => unusable(error),
            Refusal::FilterOrJoin if probe_refuses_clone3(support) => unusable(error),
            Refusal::FilterOrJoin | Refusal::Join => Started::JoinFailed(error),
            Refusal::Failed => Started::Failed(error),
        };
    }
    let pid = result as u32;
    // CLONE_VFORK returned only after the child execed or exited, and the
    // kernel's completion of that wait orders the child's store before this
    // load. A child killed by a signal before it execed stored nothing, so it
    // reads as a started program that a signal killed, which its wait
    // reports.
    let failure = prepared.state.failure.load(Ordering::Relaxed);
    if failure != 0 {
        // The child stored why it could not exec and exited; reap it, retrying
        // a wait that a signal interrupts so no zombie is left.
        let mut status = 0;
        while unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) } == -1
            && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        return Started::Failed(io::Error::from_raw_os_error(failure));
    }
    Started::Running { pid }
}

/// `clone_args` as Linux 5.7 defines it (`CLONE_ARGS_SIZE_VER2`).
#[repr(C)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// Calls clone3 for `prepared` on `stack`, with every signal blocked on this
/// thread so that no handler of this process can run in the child before it
/// resets them. Returns the child's PID or the negative errno.
fn clone_into_cgroup(
    support: &Clone3Support,
    prepared: &Prepared,
    stack: &ChildStack,
    cgroup: RawFd,
) -> i64 {
    #[cfg(test)]
    {
        let attempt = support.attempts.fetch_add(1, Ordering::Relaxed);
        if let Some(errno) = support.inject {
            if attempt < support.inject_limit {
                return -i64::from(errno);
            }
        }
    }
    #[cfg(not(test))]
    let _ = support;
    let args = CloneArgs {
        flags: (libc::CLONE_VM | libc::CLONE_VFORK) as u64 | CLONE_INTO_CGROUP,
        pidfd: 0,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: libc::SIGCHLD as u64,
        stack: stack.low as u64,
        stack_size: CHILD_STACK_BYTES as u64,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: cgroup as u64,
    };
    // Block every signal, glibc's internal ones too, which pthread_sigmask
    // leaves unblocked; glibc's posix_spawn blocks them the same way.
    let all: u64 = !0;
    let mut previous: u64 = 0;
    unsafe {
        syscall6(
            libc::SYS_rt_sigprocmask,
            [
                libc::SIG_SETMASK as usize,
                &all as *const u64 as usize,
                &mut previous as *mut u64 as usize,
                8,
                0,
                0,
            ],
        );
    }
    let result = unsafe { arch::clone3_then_child(&args, &prepared.state) };
    unsafe {
        syscall6(
            libc::SYS_rt_sigprocmask,
            [
                libc::SIG_SETMASK as usize,
                &previous as *const u64 as usize,
                0,
                8,
                0,
                0,
            ],
        );
    }
    result
}

/// `CLONE_INTO_CGROUP` (Linux 5.7), not in every libc crate release.
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// The child's stack: no more than raw system calls run on it. Its frames
/// stay far under 1 KiB even unoptimized, and execve runs on the kernel's
/// stack, so 64 KiB leaves more than 64 times what it needs. A guard page
/// below it turns an overflow into the child's own SIGSEGV, never a write
/// into this process's memory; [`start`] then reports a started program that
/// SIGSEGV killed, as for any child a signal kills before it execs.
const CHILD_STACK_BYTES: usize = 64 * 1024;

struct ChildStack {
    /// The mapping: the guard page, then the stack.
    base: *mut libc::c_void,
    len: usize,
    /// The stack's lowest address; it grows down from `low + CHILD_STACK_BYTES`.
    low: *mut u8,
}

impl ChildStack {
    fn new() -> io::Result<Self> {
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .map_err(|_| io::Error::other("cannot read the page size"))?;
        let len = page + CHILD_STACK_BYTES;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::mprotect(base, page, libc::PROT_NONE) } != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::munmap(base, len) };
            return Err(error);
        }
        Ok(Self {
            base,
            len,
            low: unsafe { base.cast::<u8>().add(page) },
        })
    }
}

impl Drop for ChildStack {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base, self.len) };
    }
}

thread_local! {
    /// One child stack per thread that starts programs: CLONE_VFORK keeps
    /// this thread waiting until its child has execed or exited, so a thread
    /// never has two children on it.
    static CHILD_STACK: RefCell<Option<ChildStack>> = const { RefCell::new(None) };
}

/// Everything the child reads, prepared before clone3, and what it writes.
struct Prepared {
    state: ChildState,
    // Owners of the storage `state` points into.
    _strings: Vec<CString>,
    _pointers: Vec<Vec<*const libc::c_char>>,
}

/// What [`vfork_child`] reads through one pointer. Every pointer names
/// storage in [`Prepared`], which outlives the child's use of it: this thread
/// waits in clone3 until the child has execed or exited.
#[repr(C)]
pub(crate) struct ChildState {
    /// Paths to try, in order, to execute the program.
    candidates: *const *const libc::c_char,
    candidate_count: usize,
    /// NULL-terminated argv and envp.
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    /// `/bin/sh`, a slot for the path that failed with ENOEXEC, then argv
    /// from its second entry, NULL-terminated: how execvp runs a file without
    /// a `#!` line. The child writes the slot.
    shell_argv: *mut *const libc::c_char,
    cwd: *const libc::c_char,
    stdio: [RawFd; 3],
    /// The errno that stopped the child, or 0 if it reached the program.
    failure: AtomicI32,
    /// For a test: the child kills itself first.
    #[cfg(test)]
    kill_before_exec: bool,
}

impl Prepared {
    fn new(support: &Clone3Support, program: &Program) -> io::Result<Self> {
        #[cfg(not(test))]
        let _ = support;
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_string());
        let c_string =
            |bytes: Vec<u8>| CString::new(bytes).map_err(|_| invalid("a NUL byte in an argument"));
        let mut strings = Vec::new();
        let mut pointers = Vec::new();
        let name = program
            .argv
            .first()
            .ok_or_else(|| invalid("an empty argv"))?;
        // The environment the program gets: this process's, with the
        // overrides set over it.
        let mut environment: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for (key, value) in program.env_overrides {
            environment.insert(key.into(), value.into());
        }
        let candidates: Vec<Vec<u8>> = if name.contains('/') {
            vec![name.as_bytes().to_vec()]
        } else {
            // execvp's search: each element of PATH, an empty one meaning the
            // working directory, and glibc's default when PATH is unset.
            let path = environment
                .get(std::ffi::OsStr::new("PATH"))
                .map(|path| path.as_bytes().to_vec())
                .unwrap_or_else(|| b"/bin:/usr/bin".to_vec());
            path.split(|byte| *byte == b':')
                .map(|dir| {
                    if dir.is_empty() {
                        name.as_bytes().to_vec()
                    } else {
                        let mut full = dir.to_vec();
                        full.push(b'/');
                        full.extend_from_slice(name.as_bytes());
                        full
                    }
                })
                .collect()
        };
        let pointer_list = |items: Vec<Vec<u8>>, strings: &mut Vec<CString>| {
            let mut list: Vec<*const libc::c_char> = Vec::with_capacity(items.len() + 1);
            for item in items {
                let string = c_string(item)?;
                list.push(string.as_ptr());
                strings.push(string);
            }
            Ok::<_, io::Error>(list)
        };
        let candidate_list = pointer_list(candidates, &mut strings)?;
        let candidate_count = candidate_list.len();
        let mut argv = pointer_list(
            program
                .argv
                .iter()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            &mut strings,
        )?;
        argv.push(std::ptr::null());
        let mut envp = pointer_list(
            environment
                .into_iter()
                .map(|(key, value)| {
                    let mut pair = key.into_vec();
                    pair.push(b'=');
                    pair.extend_from_slice(value.as_bytes());
                    pair
                })
                .collect(),
            &mut strings,
        )?;
        envp.push(std::ptr::null());
        let mut shell_argv = pointer_list(vec![b"/bin/sh".to_vec()], &mut strings)?;
        shell_argv.push(std::ptr::null());
        shell_argv.extend_from_slice(&argv[1..]);
        let cwd = c_string(program.cwd.as_os_str().as_bytes().to_vec())?;
        let cwd_pointer = cwd.as_ptr();
        strings.push(cwd);
        let state = ChildState {
            candidates: candidate_list.as_ptr(),
            candidate_count,
            argv: argv.as_ptr(),
            envp: envp.as_ptr(),
            shell_argv: shell_argv.as_mut_ptr(),
            cwd: cwd_pointer,
            stdio: program.stdio,
            failure: AtomicI32::new(0),
            #[cfg(test)]
            kill_before_exec: support.kill_child_before_exec,
        };
        pointers.push(candidate_list);
        pointers.push(argv);
        pointers.push(envp);
        pointers.push(shell_argv);
        Ok(Self {
            state,
            _strings: strings,
            _pointers: pointers,
        })
    }
}

/// A raw system call. The child cannot call libc: it shares this process's
/// memory and its thread's TLS, where libc keeps errno and locks.
#[inline(always)]
unsafe fn syscall6(number: libc::c_long, args: [usize; 6]) -> isize {
    unsafe { arch::syscall6(number as usize, args) }
}

/// `struct sigaction` as the kernel takes it on x86_64 and aarch64.
#[repr(C)]
struct KernelSigaction {
    handler: usize,
    flags: u64,
    restorer: usize,
    mask: u64,
}

/// The child: set up the program's process state as `std::process::Command`
/// would, then execute it, or store why not and exit.
///
/// # Safety
/// Entered only from [`arch::clone3_then_child`], on the child's own stack,
/// with `state` prepared by [`Prepared::new`]. It never returns, allocates,
/// takes a lock, or calls libc.
unsafe extern "C" fn vfork_child(state: *const ChildState) -> ! {
    unsafe {
        let state = &*state;
        let fail = |errno: isize| -> ! {
            state.failure.store(errno as i32, Ordering::Relaxed);
            // exit_group does not return; the loop only gives the closure
            // its `!` type.
            loop {
                syscall6(libc::SYS_exit_group, [127, 0, 0, 0, 0, 0]);
            }
        };
        let check = |result: isize| {
            if result < 0 {
                fail(-result);
            }
        };
        #[cfg(test)]
        if state.kill_before_exec {
            let pid = syscall6(libc::SYS_getpid, [0; 6]);
            syscall6(
                libc::SYS_kill,
                [pid as usize, libc::SIGKILL as usize, 0, 0, 0, 0],
            );
        }
        // The program leads a process group of its own.
        check(syscall6(libc::SYS_setpgid, [0, 0, 0, 0, 0, 0]));
        // Its stdin, stdout and stderr. The caller passes descriptors above 2,
        // so no target is also a later source; dup3 leaves the copies open
        // across exec.
        let mut target = 0;
        while target < 3 {
            check(syscall6(
                libc::SYS_dup3,
                [state.stdio[target] as usize, target, 0, 0, 0, 0],
            ));
            target += 1;
        }
        check(syscall6(
            libc::SYS_chdir,
            [state.cwd as usize, 0, 0, 0, 0, 0],
        ));
        // Every signal this process handles goes back to its default action
        // before any is unblocked, so no handler of this process can run in
        // the child; SIGPIPE, which Rust programs ignore, too, as
        // `std::process::Command` resets it. exec would reset handled ones
        // anyway; ignored ones stay ignored, as with Command. clone3's
        // CLONE_CLEAR_SIGHAND would reset the handled ones but leaves SIG_IGN
        // alone (`flush_signal_handlers(tsk, 0)`), so it would not reset
        // SIGPIPE as Command does; hence this loop. The child has its own copy
        // of the handlers (no CLONE_SIGHAND), so none of this touches this
        // process's.
        let mut signal = 1;
        while signal < 65 {
            if signal != libc::SIGKILL as usize && signal != libc::SIGSTOP as usize {
                let mut action = KernelSigaction {
                    handler: 0,
                    flags: 0,
                    restorer: 0,
                    mask: 0,
                };
                let read = syscall6(
                    libc::SYS_rt_sigaction,
                    [
                        signal,
                        0,
                        &mut action as *mut KernelSigaction as usize,
                        8,
                        0,
                        0,
                    ],
                );
                let handled = action.handler != libc::SIG_DFL && action.handler != libc::SIG_IGN;
                if read == 0 && (handled || signal == libc::SIGPIPE as usize) {
                    let default = KernelSigaction {
                        handler: libc::SIG_DFL,
                        flags: 0,
                        restorer: 0,
                        mask: 0,
                    };
                    check(syscall6(
                        libc::SYS_rt_sigaction,
                        [
                            signal,
                            &default as *const KernelSigaction as usize,
                            0,
                            8,
                            0,
                            0,
                        ],
                    ));
                }
            }
            signal += 1;
        }
        let empty: u64 = 0;
        check(syscall6(
            libc::SYS_rt_sigprocmask,
            [
                libc::SIG_SETMASK as usize,
                &empty as *const u64 as usize,
                0,
                8,
                0,
                0,
            ],
        ));
        // execvp's search: stop at the first error other than one that means
        // "not here"; report EACCES if any candidate gave it.
        let mut denied = false;
        let mut last = libc::ENOENT as isize;
        let mut index = 0;
        while index < state.candidate_count {
            let path = *state.candidates.add(index);
            let mut errno = -syscall6(
                libc::SYS_execve,
                [
                    path as usize,
                    state.argv as usize,
                    state.envp as usize,
                    0,
                    0,
                    0,
                ],
            );
            if errno as i32 == libc::ENOEXEC {
                // As glibc's execvp (`maybe_script_execute`): run the file
                // under the shell, and judge the shell's error like any other
                // candidate's, so a missing shell continues the search.
                *state.shell_argv.add(1) = path;
                let shell = *state.shell_argv;
                errno = -syscall6(
                    libc::SYS_execve,
                    [
                        shell as usize,
                        state.shell_argv as usize,
                        state.envp as usize,
                        0,
                        0,
                        0,
                    ],
                );
            }
            match errno as i32 {
                libc::EACCES => denied = true,
                libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::ENODEV | libc::ETIMEDOUT => {}
                _ => fail(errno),
            }
            last = errno;
            index += 1;
        }
        fail(if denied { libc::EACCES as isize } else { last })
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::ChildState;
    use super::CloneArgs;

    /// A raw system call (the register convention of Reverie's
    /// `reverie-inguest` exec-stop bridge, `arch::syscall6`): number and
    /// result in rax, arguments in rdi, rsi, rdx, r10, r8, r9; the
    /// instruction clobbers rcx and r11.
    #[inline(always)]
    pub(super) unsafe fn syscall6(number: usize, args: [usize; 6]) -> isize {
        let result: isize;
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") number => result,
                in("rdi") args[0], in("rsi") args[1], in("rdx") args[2],
                in("r10") args[3], in("r8") args[4], in("r9") args[5],
                lateout("rcx") _, lateout("r11") _,
                options(nostack),
            );
        }
        result
    }

    /// clone3, then, in the child, [`super::vfork_child`] on the child's own
    /// stack. Returns, in this thread only, the child's PID or the negative
    /// errno.
    ///
    /// Why the child never touches this thread's stack:
    /// - `args.stack`/`stack_size` name the child's stack, and the kernel
    ///   starts the child with rsp at its top (`stack + stack_size`, 16-byte
    ///   aligned). Every register but rax (0) and rsp is this thread's.
    /// - In the child, the instructions after `syscall` up to `call` read
    ///   only registers (rax, r12) and write only rbp and rdi. `call` pushes
    ///   its return address at the new rsp, on the child's stack, and
    ///   `vfork_child` and everything it calls run there and never return.
    /// - This block is `nostack`, so nothing of this thread lives below its
    ///   rsp that the block could disturb, and the child's rsp never points
    ///   into this thread's stack. This thread waits in the kernel until the
    ///   child execs or exits (CLONE_VFORK), then resumes at the `test` with
    ///   its own rsp, rax = PID or -errno.
    pub(super) unsafe fn clone3_then_child(
        args: *const CloneArgs,
        state: *const ChildState,
    ) -> i64 {
        let result: i64;
        unsafe {
            core::arch::asm!(
                "syscall",
                "test rax, rax",
                "jnz 2f",
                "xor ebp, ebp",
                "mov rdi, r12",
                "call {child}",
                "ud2",
                "2:",
                child = sym super::vfork_child,
                inlateout("rax") libc::SYS_clone3 => result,
                inlateout("rdi") args => _,
                in("rsi") core::mem::size_of::<CloneArgs>(),
                in("r12") state,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        result
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use super::ChildState;
    use super::CloneArgs;

    /// A raw system call (the register convention of Reverie's
    /// `reverie-inguest` exec-stop bridge, `arch::syscall6`): number in x8,
    /// arguments in x0-x5, result in x0, never x8.
    #[inline(always)]
    pub(super) unsafe fn syscall6(number: usize, args: [usize; 6]) -> isize {
        let result: isize;
        unsafe {
            core::arch::asm!(
                "svc #0",
                in("x8") number,
                inlateout("x0") args[0] => result,
                in("x1") args[1], in("x2") args[2], in("x3") args[3],
                in("x4") args[4], in("x5") args[5],
                options(nostack),
            );
        }
        result
    }

    /// clone3, then, in the child, [`super::vfork_child`] on the child's own
    /// stack. Returns, in this thread only, the child's PID or the negative
    /// errno.
    ///
    /// Why the child never touches this thread's stack:
    /// - `args.stack`/`stack_size` name the child's stack, and the kernel
    ///   starts the child with sp at its top (`stack + stack_size`, 16-byte
    ///   aligned). Every register but x0 (0) and sp is this thread's.
    /// - In the child, the instructions after `svc` up to `bl` read only
    ///   registers (x0, x20) and write only x29, x30 and x0. `bl` stores its
    ///   return address in x30, not in memory, and `vfork_child` and
    ///   everything it calls run on the child's stack and never return.
    /// - This block is `nostack`, so nothing of this thread lives below its
    ///   sp that the block could disturb, and the child's sp never points
    ///   into this thread's stack. This thread waits in the kernel until the
    ///   child execs or exits (CLONE_VFORK), then resumes at the `cbnz` with
    ///   its own sp, x0 = PID or -errno.
    pub(super) unsafe fn clone3_then_child(
        args: *const CloneArgs,
        state: *const ChildState,
    ) -> i64 {
        let result: i64;
        unsafe {
            core::arch::asm!(
                "svc #0",
                "cbnz x0, 2f",
                "mov x29, xzr",
                "mov x30, xzr",
                "mov x0, x20",
                "bl {child}",
                "brk #1",
                "2:",
                child = sym super::vfork_child,
                in("x8") libc::SYS_clone3,
                inlateout("x0") args => result,
                in("x1") core::mem::size_of::<CloneArgs>(),
                in("x20") state,
                options(nostack),
            );
        }
        result
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod arch {
    use super::ChildState;
    use super::CloneArgs;

    pub(super) unsafe fn syscall6(_number: usize, _args: [usize; 6]) -> isize {
        -(libc::ENOSYS as isize)
    }

    /// No child code for this architecture: clone3 reads as unavailable, and
    /// the caller starts programs the old way.
    pub(super) unsafe fn clone3_then_child(
        _args: *const CloneArgs,
        _state: *const ChildState,
    ) -> i64 {
        -i64::from(libc::ENOSYS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_clone3_errno_maps_to_its_refusal() {
        for errno in [libc::ENOSYS, libc::E2BIG, libc::EINVAL] {
            assert_eq!(refusal(errno), Refusal::Unusable, "errno {errno}");
        }
        assert_eq!(refusal(libc::EPERM), Refusal::FilterOrJoin);
        for errno in [
            libc::EBADF,
            libc::EACCES,
            libc::ENOENT,
            libc::EROFS,
            libc::ENODEV,
            libc::EBUSY,
            libc::EOPNOTSUPP,
        ] {
            assert_eq!(refusal(errno), Refusal::Join, "errno {errno}");
        }
        for errno in [libc::EAGAIN, libc::ENOMEM] {
            assert_eq!(refusal(errno), Refusal::Failed, "errno {errno}");
        }
    }

    /// The EPERM probe makes a real clone3 call, which creates no child; on
    /// this host, with no filter on clone3, it does not read as refused.
    #[test]
    fn the_eperm_probe_finds_clone3_allowed_here() {
        let support = Clone3Support::new();
        assert_eq!(probe_clone3(&support), libc::EINVAL);
        assert!(!probe_refuses_clone3(&support));
        assert!(probe_refuses_clone3(&Clone3Support::refusing(libc::EPERM)));
        assert!(probe_refuses_clone3(&Clone3Support::refusing(libc::ENOSYS)));
    }
}
