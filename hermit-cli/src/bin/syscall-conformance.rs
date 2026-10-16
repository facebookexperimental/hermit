/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run a program under one pass-through Reverie tool, on a chosen backend,
//! and print every syscall it made with the result it got, in a form that
//! compares across backends and with native Linux (step 0 of the
//! backend-agnostic architecture work, ai_docs/2026-10-15-kvm-backend-architecture-audit.md
//! section 8 in the dev-hermit parent).
//!
//! The tool, [`Recorder`], injects every syscall unchanged and records its
//! number and result. A backend only intercepts and injects, so every
//! backend running it should give the program the results native Linux
//! gives it. `--backend native` records the same with plain ptrace syscall
//! stops, with no Reverie at all.
//!
//! The output is text: `exit <code>`, then for each thread, in the order the
//! kernel numbered them (thread IDs are increasing), a `thread <n>` line and
//! one `name=outcome` line per syscall, in the thread's order. The outcome is
//! `ok`, an errno name, `?` for a call that does not return (`exit`,
//! `exit_group`; a successful `execve`, which never returns to the caller, is
//! `ok`), or, for the calls in [`EXACT`], the exact return value, which does
//! not depend on addresses, process IDs or time. The calls the vDSO answers
//! ([`VDSO`]) are left out.
//!
//! Backends: `native`, `ptrace` and `kvm`. The in-guest backends (DBT,
//! LiteInst, in-guest-trap, SaBRe) compile their tool into their runtime and
//! can load only Detcore today, so they are refused by name.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use clap::ValueEnum;
use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

/// The syscalls whose exact return value is compared: byte counts, offsets
/// and descriptor numbers, which native Linux gives the same in every run.
const EXACT: &[&str] = &[
    "read",
    "write",
    "pread64",
    "pwrite64",
    "readv",
    "writev",
    "lseek",
    "open",
    "openat",
    "creat",
    "dup",
    "dup2",
    "dup3",
    "readlink",
    "readlinkat",
    "getdents64",
    "fcntl",
];

/// The calls the vDSO answers in the process, which natively mostly never
/// enter the kernel, while reverie-ptrace turns them into syscalls (it
/// disables the vDSO): not compared.
const VDSO: &[&str] = &[
    "clock_gettime",
    "clock_getres",
    "gettimeofday",
    "time",
    "getcpu",
];

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Native,
    Ptrace,
    Kvm,
    Dbt,
    Liteinst,
    InGuestTrap,
    Sabre,
    E9patch,
}

/// Run PROGRAM under the pass-through recorder on BACKEND and write its
/// syscalls and their results to OUTPUT.
#[derive(Debug, Parser)]
struct Opts {
    #[clap(long, value_enum)]
    backend: Backend,
    /// Where to write the record (stdout is the program's).
    #[clap(long)]
    output: PathBuf,
    #[clap(value_name = "PROGRAM")]
    program: PathBuf,
    #[clap(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    args: Vec<OsString>,
}

/// What a syscall returned.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Outcome {
    Value(i64),
    Errno(i32),
    /// It does not return (`exit`, `exit_group`).
    NoReturn,
    /// Recorded before the call (`execve`); replaced if the call returns.
    Pending,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Event {
    tid: i32,
    sysno: i32,
    outcome: Outcome,
}

#[derive(Debug, Default)]
struct Recorded {
    events: Mutex<Vec<Event>>,
}

#[reverie::global_tool]
impl GlobalTool for Recorded {
    type Request = Event;
    type Response = ();
    type Config = ();

    async fn receive_rpc(&self, _from: Pid, event: Event) {
        let mut events = self.events.lock().unwrap();
        // A returning `execve` replaces its pending record, the thread's last.
        if event.outcome != Outcome::Pending
            && let Some(pending) = events
                .iter_mut()
                .rev()
                .find(|e| e.tid == event.tid)
                .filter(|e| e.outcome == Outcome::Pending && e.sysno == event.sysno)
        {
            pending.outcome = event.outcome;
            return;
        }
        events.push(event);
    }
}

/// The pass-through tool: every syscall goes to the kernel unchanged.
#[derive(Debug, Default, Clone)]
struct Recorder;

#[reverie::tool]
impl Tool for Recorder {
    type GlobalState = Recorded;
    type ThreadState = ();

    async fn handle_syscall_event<T: Guest<Self>>(
        &self,
        guest: &mut T,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        let tid = guest.tid().as_raw();
        let sysno = syscall.number() as i32;
        let event = |outcome| Event {
            tid,
            sysno,
            outcome,
        };
        match syscall {
            Syscall::Exit(_) | Syscall::ExitGroup(_) => {
                guest.send_rpc(event(Outcome::NoReturn)).await;
                guest.tail_inject(syscall).await
            }
            Syscall::Execve(_) | Syscall::Execveat(_) => {
                // A successful `execve` does not come back here.
                guest.send_rpc(event(Outcome::Pending)).await;
                let errno = guest.inject(syscall).await.unwrap_err();
                guest
                    .send_rpc(event(Outcome::Errno(errno.into_raw())))
                    .await;
                Err(errno.into())
            }
            _ => {
                let result = guest.inject(syscall).await;
                let outcome = match result {
                    Ok(value) => Outcome::Value(value),
                    Err(errno) => Outcome::Errno(errno.into_raw()),
                };
                guest.send_rpc(event(outcome)).await;
                Ok(result?)
            }
        }
    }
}

/// The comparable form of a run.
#[derive(Debug)]
struct Report {
    exit_code: i32,
    threads: Vec<Vec<String>>,
}

impl Report {
    fn text(&self) -> String {
        let mut text = format!("exit {}\n", self.exit_code);
        for (number, calls) in self.threads.iter().enumerate() {
            text.push_str(&format!("thread {number}\n"));
            for call in calls {
                text.push_str(call);
                text.push('\n');
            }
        }
        text
    }
}

fn errno_name(errno: i32) -> String {
    reverie::syscalls::Errno::new(errno)
        .name()
        .map_or_else(|| format!("E{errno}"), str::to_owned)
}

fn outcome_text(name: &str, outcome: Outcome) -> String {
    match outcome {
        Outcome::Value(value) if EXACT.contains(&name) => value.to_string(),
        Outcome::Value(_) | Outcome::Pending => "ok".to_owned(),
        Outcome::Errno(errno) => errno_name(errno),
        Outcome::NoReturn => "?".to_owned(),
    }
}

/// Group `(tid, name, outcome)` by thread, threads in increasing ID order.
fn report(exit_code: i32, events: Vec<(i32, String, Outcome)>) -> Report {
    let mut threads: BTreeMap<i32, Vec<String>> = BTreeMap::new();
    for (tid, name, outcome) in events {
        if VDSO.contains(&name.as_str()) {
            continue;
        }
        let text = format!("{name}={}", outcome_text(&name, outcome));
        threads.entry(tid).or_default().push(text);
    }
    Report {
        exit_code,
        threads: threads.into_values().collect(),
    }
}

fn reverie_events(events: Vec<Event>) -> Vec<(i32, String, Outcome)> {
    events
        .into_iter()
        .map(|event| {
            let name = Sysno::from(event.sysno).name().to_owned();
            (event.tid, name, event.outcome)
        })
        .collect()
}

fn exit_code(status: ExitStatus) -> i32 {
    match status {
        ExitStatus::Exited(code) => code,
        ExitStatus::Signaled(signal, _) => 128 + signal as i32,
        ExitStatus::SignaledRaw(signal, _) => 128 + signal,
    }
}

async fn run_ptrace(program: &Path, args: &[OsString]) -> anyhow::Result<Report> {
    let mut command = reverie::process::Command::new(program);
    command.args(args);
    let tracer = reverie_ptrace::TracerBuilder::<Recorder>::new(command)
        .spawn()
        .await?;
    let (status, recorded) = tracer.wait().await?;
    let events = recorded.events.into_inner().unwrap();
    Ok(report(exit_code(status), reverie_events(events)))
}

async fn run_kvm(program: &Path, args: &[OsString]) -> anyhow::Result<Report> {
    let image = std::fs::read(program).with_context(|| format!("reading {program:?}"))?;
    let cwd = std::env::current_dir()?;
    let mut argv = vec![program.to_str().context("non-UTF-8 program")?.to_owned()];
    for arg in args {
        argv.push(arg.to_str().context("non-UTF-8 argument")?.to_owned());
    }
    // In name order, as for the other backends (see `run_native`).
    let mut envp = std::env::vars()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    envp.sort();
    let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let envp = envp.iter().map(String::as_str).collect::<Vec<_>>();
    // The guest gets this process's standard input as its descriptor 0, as
    // a native or ptrace guest does.
    // SAFETY: duplicating a descriptor; the result is owned below or -1.
    let stdin = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    // SAFETY: a descriptor fcntl just returned, owned by nothing else.
    let stdin = (stdin >= 0).then(|| unsafe { std::fs::File::from_raw_fd(stdin) });
    let mut backend = reverie_kvm::KvmBackend::new_with_stdin(1 << 30, stdin)
        .map_err(|error| anyhow::anyhow!("reverie-kvm: {error}"))?;
    backend
        .install_static_elf_with_exec_context(&image, program.as_os_str(), &argv, &envp, &cwd)
        .map_err(|error| anyhow::anyhow!("loading {program:?} into KVM: {error}"))?;
    drop(image);
    let (recorded, exit_code, _stdout, _stderr) = backend
        .run_static_elf_with_tool::<Recorder>((), false)
        .await
        .map_err(|error| anyhow::anyhow!("KVM run: {error}"))?;
    let events = recorded.events.into_inner().unwrap();
    Ok(report(exit_code, reverie_events(events)))
}

/// Run `program` natively, observing its syscalls with plain ptrace (no
/// Reverie): a syscall-stop at each entry and exit only reads registers, so
/// the program gets the results Linux gives it.
fn run_native(program: &Path, args: &[OsString]) -> anyhow::Result<Report> {
    use std::collections::BTreeSet;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::ffi::OsStringExt;

    use nix::sys::ptrace;
    use nix::sys::signal::Signal;
    use nix::sys::wait::WaitPidFlag;
    use nix::sys::wait::WaitStatus;
    use nix::sys::wait::waitpid;
    use nix::unistd::ForkResult;
    use nix::unistd::Pid as NixPid;

    let argv = std::iter::once(program.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .map(|arg| CString::new(arg.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    let path = CString::new(program.as_os_str().as_bytes())?;
    // The environment in name order, as reverie-process's `Command` passes
    // it to a ptrace or KVM guest, so that the order, which a program can
    // see, is not a difference between the launchers.
    let mut environment = std::env::vars_os().collect::<Vec<_>>();
    environment.sort();
    let envp = environment
        .into_iter()
        .map(|(key, value)| {
            let mut entry = key.into_vec();
            entry.push(b'=');
            entry.extend(value.into_vec());
            CString::new(entry)
        })
        .collect::<Result<Vec<_>, _>>()?;
    // SAFETY: the child only calls async-signal-safe functions before exec.
    let child = match unsafe { nix::unistd::fork() }? {
        ForkResult::Child => {
            // The Rust runtime ignores SIGPIPE; a program starts with the
            // default, as reverie-ptrace's and the shell's children do.
            // SAFETY: resetting a disposition in the forked child.
            let _ = unsafe {
                nix::sys::signal::signal(Signal::SIGPIPE, nix::sys::signal::SigHandler::SigDfl)
            };
            let _ = ptrace::traceme();
            let _ = nix::sys::signal::raise(Signal::SIGSTOP);
            let _ = nix::unistd::execve(&path, &argv, &envp);
            // SAFETY: exiting the forked child without unwinding.
            unsafe { libc::_exit(127) };
        }
        ForkResult::Parent { child } => child,
    };
    waitpid(child, None)?;
    ptrace::setoptions(
        child,
        ptrace::Options::PTRACE_O_TRACESYSGOOD
            | ptrace::Options::PTRACE_O_TRACEFORK
            | ptrace::Options::PTRACE_O_TRACEVFORK
            | ptrace::Options::PTRACE_O_TRACECLONE
            | ptrace::Options::PTRACE_O_TRACEEXEC
            | ptrace::Options::PTRACE_O_EXITKILL,
    )?;
    ptrace::syscall(child, None)?;

    let mut events = Vec::new();
    // The call each thread is in, from its entry stop to its exit stop.
    let mut entered: BTreeMap<i32, i64> = BTreeMap::new();
    // New threads, whose first stop is the SIGSTOP that starts them traced.
    let mut starting: BTreeSet<i32> = BTreeSet::new();
    let mut exit_code = -1;
    loop {
        let status = match waitpid(NixPid::from_raw(-1), Some(WaitPidFlag::__WALL)) {
            Ok(status) => status,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(error) => return Err(error.into()),
        };
        match status {
            WaitStatus::PtraceSyscall(pid) => {
                let tid = pid.as_raw();
                let regs = ptrace::getregs(pid)?;
                match entered.remove(&tid) {
                    None => {
                        let sysno = regs.orig_rax as i64;
                        if sysno == libc::SYS_exit || sysno == libc::SYS_exit_group {
                            events.push((tid, sysno, Outcome::NoReturn));
                        } else {
                            entered.insert(tid, sysno);
                        }
                    }
                    Some(sysno) => {
                        let value = regs.rax as i64;
                        let outcome = if (-4095..0).contains(&value) {
                            Outcome::Errno(-value as i32)
                        } else {
                            Outcome::Value(value)
                        };
                        events.push((tid, sysno, outcome));
                    }
                }
                let _ = ptrace::syscall(pid, None);
            }
            WaitStatus::PtraceEvent(pid, _, event) => {
                if (event == libc::PTRACE_EVENT_FORK
                    || event == libc::PTRACE_EVENT_VFORK
                    || event == libc::PTRACE_EVENT_CLONE)
                    && let Ok(new) = ptrace::getevent(pid)
                {
                    starting.insert(new as i32);
                }
                let _ = ptrace::syscall(pid, None);
            }
            WaitStatus::Stopped(pid, signal) => {
                let tid = pid.as_raw();
                let deliver = if signal == Signal::SIGSTOP && starting.remove(&tid) {
                    None
                } else {
                    Some(signal)
                };
                let _ = ptrace::syscall(pid, deliver);
            }
            WaitStatus::Exited(pid, code) => {
                if let Some(sysno) = entered.remove(&pid.as_raw()) {
                    events.push((pid.as_raw(), sysno, Outcome::NoReturn));
                }
                if pid == child {
                    exit_code = code;
                }
            }
            WaitStatus::Signaled(pid, signal, _) if pid == child => {
                exit_code = 128 + signal as i32;
            }
            _ => {}
        }
    }
    let events = events
        .into_iter()
        .map(|(tid, sysno, outcome)| {
            let name = Sysno::from(sysno as i32).name().to_owned();
            (tid, name, outcome)
        })
        .collect();
    Ok(report(exit_code, events))
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    let report = match opts.backend {
        Backend::Native => run_native(&opts.program, &opts.args)?,
        Backend::Ptrace | Backend::Kvm => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                match opts.backend {
                    Backend::Ptrace => run_ptrace(&opts.program, &opts.args).await,
                    _ => run_kvm(&opts.program, &opts.args).await,
                }
            })?
        }
        Backend::E9patch => bail!(
            "REFUSED: e9patch preprocessing rewrites the program to call an in-process runtime \
             built with Detcore; the pass-through recorder has no such runtime yet"
        ),
        backend => bail!(
            "REFUSED: the {backend:?} backend compiles its tool into its in-guest runtime and \
             can load only Detcore today; the pass-through recorder cannot run on it yet"
        ),
    };
    std::fs::write(&opts.output, report.text())?;
    Ok(())
}
