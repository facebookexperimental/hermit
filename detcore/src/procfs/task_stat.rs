/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A process's own `/proc/<pid>/stat` as Detcore renders it (package K slice
//! D4c-1 of <https://github.com/rrnewton/hermit/issues/4213>), and the minimal
//! record of task names it falls back on.
//!
//! Detcore renders the caller's own process's `stat`, by every spelling, from
//! its process table, in Linux's 52 fields ([`render_stat`]). Its second field,
//! the task's name (Linux's `task_struct::comm`), comes from one of two
//! sources, chosen by the backend's declared capability alone
//! ([`CommSource::of_backend`]):
//!
//! - [`CommSource::Kernel`] where guest threads are host tasks the tool can
//!   name (`procfs_inode::tool_can_name_guest_descriptors`: ptrace, DBT,
//!   SaBRe, LiteInst): the kernel beneath's own name, which the tool reads
//!   from the described task's `/proc/<pid>[/task/<tid>]/comm` in the
//!   reader's turn (`Detcore::kernel_task_comm`). It follows every way Linux
//!   renames a task, a write to a `comm` file by any spelling included, and
//!   the host kernel's own rule for an exec by descriptor. The file must be
//!   on a procfs. Where the tool shares the guest's root (DBT, SaBRe,
//!   LiteInst) and the guest has changed it (`chroot`), nothing is read: the
//!   path would go through a root the guest controls, where no check on the
//!   opened file authenticates the task. Where the read is refused or fails
//!   (that root change, the tool's own descriptors exhausted, a guest
//!   seccomp filter), the read of `stat` is the failure policy's named
//!   refusal, never the record. A tracer (ptrace) reads its own `/proc`,
//!   which no guest `chroot` changes.
//! - [`CommSource::Record`] where they are not (KVM): Detcore's record of the
//!   name ([`TaskComm`], `ThreadTree::task_comms`), set at each exec from the
//!   new image's `AT_EXECFN` ([`TaskComm::of_exec_filename`]), inherited at
//!   clone, replaced by `prctl(PR_SET_NAME)`.
//!
//! The record would differ from Linux's name after a write to a `comm` file
//! (by the task itself or another; glibc's `pthread_setname_np` of another
//! thread writes `/proc/self/task/<tid>/comm`), and after
//! `execveat(fd, "", AT_EMPTY_PATH)` on Linux 6.14 and later (named by the
//! file's dentry, the record by `/dev/fd/<fd>`). Neither can happen where the
//! record is used: Detcore refuses an open of the reader's own `comm` for
//! writing (EACCES, below), and the KVM executor refuses that exec
//! (ENOTSUP).
//!
//! The `comm` files (package K slice A+): where the name's source is the
//! record (KVM), Detcore also renders the reader's own process's
//! `/proc/<pid>/comm`, by every spelling (`self`, its pid, `thread-self`,
//! `task/<tid>`), from the same record ([`render_comm`],
//! [`renders_task_comm`]), with the `stat` file's seq_file positioning and
//! failure policy. Detcore does not write a name: it refuses an open of a
//! rendered `comm` for writing itself, with EACCES and no injected call
//! (`syscalls::files::rendered_procfs::asks_to_write_comm`), by its path,
//! through a descriptor link, and through a `..` spelling, which Detcore
//! walks itself where it models every component, checking each task it
//! passes (`walk_comm_alias`), and refuses by name for writing where it does
//! not (<https://github.com/rrnewton/hermit/issues/4263>), so the record
//! cannot fall behind a write and the answer depends on neither the host nor
//! the kernel beneath.
//! DECLARED: Linux renames the task (`comm_write`). Another process's
//! `comm` stays the kernel beneath's (the executor's). Where the source is
//! the kernel (ptrace, DBT, SaBRe, LiteInst), every `comm` file stays the
//! kernel's, as before, unclassified.
//!
//! DECLARED, KVM: for the run's root image, the record (from `AT_EXECFN`,
//! the path the root was launched by) names the task as Linux does, while
//! the KVM executor, which still answers `PR_GET_NAME` and
//! `/proc/<pid>/status` there, names it by its resolved, canonical
//! `argv[0]` (reverie-kvm `elf.rs`, `initial_thread_name`): for a `#!`
//! script root, which Hermit's launcher rewrites to the interpreter, `stat`
//! says `t.sh` where `status` says `sh`; for an ELF root launched through a
//! symbolic link `/path/alias -> /path/actual`, `stat` says `alias` where
//! `status` says `actual`. The record is the Linux-correct side and is kept;
//! the executor's naming is the executor's
//! (<https://github.com/rrnewton/reverie/issues/1106>; cells
//! `system-utils/stat-script-root` and `system-utils/stat-alias-root` record
//! it). On ptrace and DBT all three are the kernel's. The rendered `comm`
//! file follows the record, so on KVM it agrees with `stat` and disagrees
//! with `PR_GET_NAME` and `status` for those two roots in the same way, and
//! in every descendant that has not renamed itself or exec'd since. The same
//! split can follow a guest exec by a spelling the executor canonicalizes: a
//! slashless `execve("alias", ...)` of a symbolic link `alias -> actual` can
//! be named `actual` by the executor while the record, `stat` and `comm` say
//! `alias`, as Linux does. After `PR_SET_NAME`, both follow the same rule.
//!
//! The rules are Linux 6.17's (fs/proc/array.c `do_task_stat`, fs/exec.c
//! `begin_new_exec`, kernel/sys.c `PR_SET_NAME`, fs/proc/base.c
//! `comm_show`).

use serde::Deserialize;
use serde::Serialize;

use crate::syscalls::sysinfo::clock_ticks;
use crate::types::LogicalTime;

/// Linux's `TASK_COMM_LEN`: a task's name and its terminating NUL.
pub const TASK_COMM_LEN: usize = 16;

/// `PF_EXITING` (include/linux/sched.h): the task is exiting, which a zombie's
/// `stat` shows in its flags (`do_exit` sets it in `exit_signals`).
const PF_EXITING: u64 = 0x0000_0004;

/// `PF_FORKNOEXEC`: the task was forked and has not exec'd. `copy_process`
/// sets it on every new task, threads included (kernel/fork.c:2061 in 6.17),
/// and only the task that execs loses it (`begin_new_exec`).
const PF_FORKNOEXEC: u64 = 0x0000_0040;

/// `RLIM_INFINITY` as `stat`'s `rsslim` prints it.
pub(crate) const RLIM_INFINITY: u64 = u64::MAX;

/// A task's name, Linux's `task_struct::comm`: at most 15 bytes, none of them
/// NUL, padded with NULs to [`TASK_COMM_LEN`], as `__set_task_comm` stores it
/// (`strscpy_pad`).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize
)]
pub struct TaskComm([u8; TASK_COMM_LEN]);

impl TaskComm {
    /// The name Linux stores from `bytes`: the bytes before the first NUL, at
    /// most 15 (`strscpy_pad` into `TASK_COMM_LEN`). `PR_SET_NAME` reads at
    /// most 15 bytes of the caller's string through its NUL
    /// (`strncpy_from_user(comm, arg2, sizeof(me->comm) - 1)`), and the
    /// kernel's `comm` file holds the name and a newline.
    pub fn new(bytes: &[u8]) -> Self {
        let length = bytes
            .iter()
            .take(TASK_COMM_LEN - 1)
            .position(|byte| *byte == 0)
            .unwrap_or_else(|| bytes.len().min(TASK_COMM_LEN - 1));
        let mut name = [0; TASK_COMM_LEN];
        name[..length].copy_from_slice(&bytes[..length]);
        Self(name)
    }

    /// The name an exec of `filename` stores: its last component
    /// (`kbasename(bprm->filename)`, fs/exec.c:1246 in 6.17), cut as
    /// [`Self::new`] cuts it. A new image's `AT_EXECFN` points at a copy of
    /// `bprm->filename` (`create_elf_tables`), so its last component is the
    /// name the exec gave: the path as given for a direct, symbolic-link or
    /// script exec. Only `execveat(fd, "", AT_EMPTY_PATH)` is named otherwise
    /// in Linux 6.14 and later (by the file's dentry, `comm_from_dentry`),
    /// and the backends that use this record do not support that exec.
    pub fn of_exec_filename(filename: &[u8]) -> Self {
        let start = filename
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(0, |slash| slash + 1);
        Self::new(&filename[start..])
    }

    /// The name as the kernel's `/proc/<pid>/comm` holds it: the bytes before
    /// its final newline (`comm_show` prints the name and `\n`).
    pub fn from_comm_file(bytes: &[u8]) -> Self {
        Self::new(bytes.strip_suffix(b"\n").unwrap_or(bytes))
    }

    /// The name's bytes, without the terminating NUL.
    pub fn as_bytes(&self) -> &[u8] {
        let length = self
            .0
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(TASK_COMM_LEN);
        &self.0[..length]
    }
}

/// What Detcore's process table says about the task a per-process `stat`
/// file describes (`Scheduler::task_stat_facts`): the thread below `task/`,
/// else the task the top-level number names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatFacts {
    /// The task's recorded name, `None` where Detcore recorded none (a task
    /// of a process that has not exec'd an image Detcore saw).
    pub comm: Option<TaskComm>,
    /// The CPUs the task may run on and its home (`Scheduler::task_cpus`),
    /// which place it on a CPU (`TaskStat::processor`).
    pub cpus: crate::cpu_set::TaskCpus,
    /// The task has exited and is not reaped: a zombie, whose state is `Z`.
    pub exited: bool,
    /// The task leads its thread group.
    pub leads: bool,
    /// The thread group leader's exit signal (`task_struct::exit_signal`), as
    /// Detcore's wait model keeps it (`ProcessWaitMetadata::exit_signal`).
    /// A thread that does not lead has -1 (`copy_process` for
    /// `CLONE_THREAD`, kernel/fork.c:2328), whatever this is.
    pub process_exit_signal: i32,
    /// The thread group leader was forked and has not exec'd
    /// (`ProcessWaitMetadata::forked_without_exec`).
    pub forked_without_exec: bool,
    /// The process's group and session, as `getpgid` and `getsid` answer
    /// (`Scheduler::group_or_session_of`).
    pub process_group: i32,
    pub session: i32,
    /// The tasks Linux counts in the process (`signal_struct::nr_threads`):
    /// its live threads and its leader until the group is reaped, the tasks
    /// its `task/` directory lists (`Scheduler::procfs_listing`).
    pub num_threads: i32,
    /// The process the task belongs to, by its leader's number; `None` for a
    /// number no process of the table holds.
    pub process: Option<i32>,
    /// The process's parent as the guest sees it (`getppid`'s answer), from
    /// Detcore's model (`Scheduler::guest_visible_parent`): its real parent's
    /// process, the namespace's init (1) for an orphan given to it, and for a
    /// process whose real parent is outside the guest's tree (the run's root,
    /// and its `CLONE_PARENT` children) the parent its own `getppid` answered
    /// when it first opened its rendered `stat`
    /// (`Scheduler::record_external_parent`). `None` before that answer is
    /// recorded.
    pub parent: Option<i32>,
    /// The process's virtual soft `RLIMIT_RSS` (`ThreadTree::rss_limits`),
    /// `RLIM_INFINITY` unless it set another.
    pub rsslim: u64,
}

/// Where a rendered `stat`'s name (field 2) comes from: decided by the
/// backend's declared capability, never by which backend it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommSource {
    /// The kernel beneath's own name (`Detcore::kernel_task_comm`).
    Kernel,
    /// Detcore's record of the name (`ThreadTree::task_comms`).
    Record,
}

impl CommSource {
    /// The source for a backend: the kernel's name where its guest threads
    /// are host tasks the tool can name
    /// (`procfs_inode::tool_can_name_guest_descriptors`, the capability
    /// `namespace::guest_task_procfs` tests first, so the one
    /// `Detcore::exec_image_at` and `mark_past_first_execve`'s `exe` read
    /// depend on), Detcore's record elsewhere. The other conditions of
    /// `guest_task_procfs` (an unchanged root, the tool's mount namespace)
    /// decide whether a path through the guest's working directory or
    /// descriptor links resolves, not whether a task's name can be read by
    /// its number; after a guest `chroot` the source stays the kernel, whose
    /// read succeeds through a tracer's own `/proc`, and where the tool
    /// shares the guest's root, not at all: the read of `stat` is refused by
    /// name (`Detcore::kernel_task_comm`).
    ///
    /// NOTE: `tool_can_name_guest_descriptors` is today the inverse of
    /// `provides_process_signal_control`, which only KVM declares, so the
    /// record is the source exactly where the backend owns signal delivery.
    /// That capability describes signal ownership, not the absence of a host
    /// task name: a future backend that owns signals but runs guest threads
    /// as host tasks would need its own capability here. Writes do not depend
    /// on this choice: Detcore refuses a write open of every `comm` it
    /// renders.
    pub(crate) fn of_backend(tool_can_name_guest_descriptors: bool) -> Self {
        if tool_can_name_guest_descriptors {
            Self::Kernel
        } else {
            Self::Record
        }
    }
}

/// The name a rendered `stat` shows, from its `source`: the kernel's name
/// the tool read (`kernel`, `Err` naming why there is none), or Detcore's
/// record (`record`). `Err` names why there is none, which the caller turns
/// into the failure policy's named refusal: a failed kernel read is never
/// answered with the record, which a host resource (the tool's descriptors)
/// would then choose, and which can be stale where the kernel is the source.
pub(crate) fn stat_comm(
    source: CommSource,
    kernel: Result<TaskComm, &'static str>,
    record: Option<TaskComm>,
) -> Result<TaskComm, &'static str> {
    match source {
        CommSource::Kernel => kernel,
        CommSource::Record => record.ok_or("Detcore recorded no name for the task"),
    }
}

/// Whether Detcore renders the `stat` of the task `facts` describe for a
/// reader in process `own`, whose name comes from `source`: only the
/// reader's own process's (package K slice D4c-1), and where the source is
/// Detcore's record, only once a name is recorded (a process that has not
/// exec'd an image Detcore saw has none). Another process's file is the
/// kernel beneath's: the host kernel's on ptrace and DBT, the provider's
/// EACCES on KVM.
pub(crate) fn renders_task_stat(facts: &TaskStatFacts, own: i32, source: CommSource) -> bool {
    facts.process == Some(own) && (source == CommSource::Kernel || facts.comm.is_some())
}

/// Whether Detcore renders the `comm` of the task `facts` describe for a
/// reader in process `own`, whose name comes from `source` (package K slice
/// A+): only where the source is Detcore's record (the same capability that
/// chooses `stat`'s name, never a backend's identity), only the reader's own
/// process's, and only once a name is recorded. Elsewhere the file is the
/// kernel beneath's: the host kernel's own name on ptrace and DBT, which
/// Detcore leaves unclassified, and on KVM another process's, or an unnamed
/// task's, the executor's EACCES.
pub(crate) fn renders_task_comm(facts: &TaskStatFacts, own: i32, source: CommSource) -> bool {
    source == CommSource::Record && facts.process == Some(own) && facts.comm.is_some()
}

/// A task's `comm` file (fs/proc/base.c:1750-1765 `comm_show` in 6.17): its
/// name as `proc_task_name(m, p, false)` prints it, raw, through its first
/// NUL (fs/proc/array.c:99-118, `seq_printf(m, "%.64s", tcomm)`), and a
/// newline.
pub(crate) fn render_comm(comm: &TaskComm) -> Vec<u8> {
    let mut line = comm.as_bytes().to_vec();
    line.push(b'\n');
    line
}

/// The CPU time a `stat` shows (fields 14-17, `do_task_stat`,
/// fs/proc/array.c:549-550 and 571-575 at v6.16): the user and system time of
/// the whole process for a process's file and of the one thread for a
/// thread's (`thread_group_cputime_adjusted`, `task_cputime_adjusted`), and
/// the process's reaped children's (`signal_struct::cutime`, `cstime`) in
/// both. Detcore's logical CPU accounting, the one `times(2)`,
/// `getrusage(2)` and the CPU-time clocks report (`crate::cpu_clock`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TaskCpuTimes {
    pub(crate) user: LogicalTime,
    pub(crate) system: LogicalTime,
    pub(crate) children_user: LogicalTime,
    pub(crate) children_system: LogicalTime,
}

/// One rendering of a `stat` file, with what its reader adds to the
/// process table's facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TaskStat {
    /// The number the file's path names its task by: field 1
    /// (`pid_nr_ns(pid, ns)` of the inode's pid, the thread's number below
    /// `task/`).
    pub(crate) pid: i32,
    /// The task's name: the kernel's, or Detcore's record ([`stat_comm`]).
    pub(crate) comm: TaskComm,
    pub(crate) facts: TaskStatFacts,
    /// The reading thread is the task the file describes, which is running.
    pub(crate) reader_is_task: bool,
    /// The process's parent: as `getppid` answers its reader for its own
    /// process, or for another process's file read by this one,
    /// [`TaskStatFacts::parent`], numbered as that process's `getppid`
    /// answers it (`Detcore::getppid_numbered_parent`).
    pub(crate) ppid: i32,
    /// The process's `RLIMIT_RSS` soft limit ([`TaskStatFacts::rsslim`]),
    /// whoever reads it.
    pub(crate) rsslim: u64,
    /// The CPU the task runs on, field 39: its place on the run's virtual
    /// CPUs ([`TaskStatFacts::cpus`], `TaskCpus::current`), 0 on the single
    /// CPU without `--virtual-cpus`.
    pub(crate) processor: u32,
    /// The task's CPU time as of this read, its reader's own process's;
    /// zero for another process's file, whose accounting Detcore keeps in
    /// that process's own threads.
    pub(crate) cpu: TaskCpuTimes,
}

/// `stat`'s state letter for a task: `R` for the reader itself, which runs
/// while it reads; `Z` for a zombie; `S` for every other task. With threads
/// sequentialized no other task runs while the file is read, and Detcore
/// does not model a task's kernel wait states (`D`, `t`, `T`): a fixed `S`.
fn state(stat: &TaskStat) -> char {
    if stat.facts.exited {
        'Z'
    } else if stat.reader_is_task {
        'R'
    } else {
        'S'
    }
}

/// `stat`'s `flags` (field 9): only the bits Detcore models.
/// `PF_FORKNOEXEC` for a task forked without an exec since (every thread
/// that does not lead, and a leader by Detcore's wait model), and
/// `PF_EXITING` for a zombie. Never `PF_RANDOMIZE`: every launcher Hermit
/// execs a guest through sets `ADDR_NO_RANDOMIZE`, and KVM's loader does not
/// randomize. Never `PF_SUPERPRIV` or the kernel's other bits, which
/// describe host privilege use and kernel state.
fn flags(stat: &TaskStat) -> u64 {
    let mut flags = 0;
    if !stat.facts.leads || stat.facts.forked_without_exec {
        flags |= PF_FORKNOEXEC;
    }
    if stat.facts.exited {
        flags |= PF_EXITING;
    }
    flags
}

/// A task's `stat` line in Linux's 52 fields (`do_task_stat`,
/// fs/proc/array.c:467-669 in 6.17), from Detcore's state:
///
/// | field | name | value |
/// | --- | --- | --- |
/// | 1 | pid | the number the path names the task by |
/// | 2 | comm | the task's name ([`TaskStat::comm`]), raw bytes, in parentheses |
/// | 3 | state | `R` for the reader, `Z` for a zombie, else `S` ([`state`]) |
/// | 4 | ppid | the process's parent |
/// | 5, 6 | pgrp, session | Detcore's model, as `getpgid` and `getsid` answer |
/// | 7, 8 | tty_nr, tpgid | 0, -1: no controlling terminal |
/// | 9 | flags | [`flags`] |
/// | 14, 15 | utime, stime | the task's user and system time ([`TaskCpuTimes`]), in clock ticks; 0 for another process's file |
/// | 16, 17 | cutime, cstime | its process's reaped children's, in clock ticks; 0 for another process's file |
/// | 18, 19 | priority, nice | 20, 0: nice is inert under Detcore |
/// | 20 | num_threads | the process's tasks |
/// | 25 | rsslim | the process's `RLIMIT_RSS` soft limit |
/// | 38 | exit_signal | the leader's, -1 for another thread |
/// | 39 | processor | the task's virtual CPU, [`TaskStat::processor`] |
/// | 40, 41 | rt_priority, policy | 0, 0 (`SCHED_OTHER`) |
/// | 52 | exit_code | 0 |
///
/// Every other field is 0: the fault counts (10-13), the guest time (43, 44),
/// `itrealvalue` (21, always 0 in Linux), the start time (22), the memory
/// sizes and addresses (23, 24, 26-30, 45-51), the four obsolete signal
/// words (31-34, which the sanitizer zeroed on every backend before this),
/// `wchan` (35), `nswap` and `cnswap` (36, 37) and the block-I/O delay
/// (42). These are the fields `sanitize_stat` zeroed in the kernel's file
/// before Detcore rendered it, with the last CPU (39), which is 0 on the
/// single virtual CPU.
pub(crate) fn render_stat(stat: &TaskStat) -> Vec<u8> {
    let facts = &stat.facts;
    let mut line = format!("{} (", stat.pid).into_bytes();
    line.extend_from_slice(stat.comm.as_bytes());
    let exit_signal = if facts.leads {
        facts.process_exit_signal
    } else {
        -1
    };
    let fields: [String; 50] = [
        // 3 state
        state(stat).to_string(),
        // 4 ppid, 5 pgrp, 6 session, 7 tty_nr, 8 tpgid
        stat.ppid.to_string(),
        facts.process_group.to_string(),
        facts.session.to_string(),
        "0".to_owned(),
        "-1".to_owned(),
        // 9 flags
        flags(stat).to_string(),
        // 10 minflt .. 13 cmajflt
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        // 14 utime .. 17 cstime
        clock_ticks(stat.cpu.user).to_string(),
        clock_ticks(stat.cpu.system).to_string(),
        clock_ticks(stat.cpu.children_user).to_string(),
        clock_ticks(stat.cpu.children_system).to_string(),
        // 18 priority, 19 nice, 20 num_threads
        "20".to_owned(),
        "0".to_owned(),
        facts.num_threads.to_string(),
        // 21 itrealvalue, 22 starttime, 23 vsize, 24 rss
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        // 25 rsslim
        stat.rsslim.to_string(),
        // 26 startcode .. 37 cnswap
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        // 38 exit_signal, 39 processor, 40 rt_priority, 41 policy
        exit_signal.to_string(),
        stat.processor.to_string(),
        "0".to_owned(),
        "0".to_owned(),
        // 42 delayacct_blkio_ticks .. 51 env_end
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        "0".to_owned(),
        // 52 exit_code
        "0".to_owned(),
    ];
    line.extend_from_slice(b") ");
    line.extend_from_slice(fields.join(" ").as_bytes());
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> TaskStatFacts {
        TaskStatFacts {
            comm: Some(TaskComm::new(b"probe")),
            cpus: Default::default(),
            exited: false,
            leads: true,
            process_exit_signal: libc::SIGCHLD,
            forked_without_exec: false,
            process_group: 0,
            session: 0,
            num_threads: 1,
            process: Some(7),
            parent: None,
            rsslim: RLIM_INFINITY,
        }
    }

    fn fields(line: &[u8]) -> (String, Vec<String>) {
        let text = String::from_utf8_lossy(line).into_owned();
        assert!(text.ends_with('\n'), "{text:?}");
        let open = text.find(" (").unwrap();
        let close = text.rfind(") ").unwrap();
        let comm = text[open + 2..close].to_owned();
        let mut fields = vec![text[..open].to_owned(), format!("({comm})")];
        fields.extend(text[close + 2..].split_whitespace().map(str::to_owned));
        (comm, fields)
    }

    /// The rendered line has Linux's 52 fields, in `do_task_stat`'s order,
    /// with Detcore's values where it owns them and the documented fixed
    /// values elsewhere.
    #[test]
    fn stat_has_linuxs_52_fields_in_order() {
        let stat = TaskStat {
            pid: 7,
            comm: TaskComm::new(b"probe"),
            facts: TaskStatFacts {
                process_group: 7,
                session: 3,
                num_threads: 2,
                ..facts()
            },
            reader_is_task: true,
            ppid: 3,
            rsslim: RLIM_INFINITY,
            processor: 3,
            cpu: TaskCpuTimes::default(),
        };
        let line = render_stat(&stat);
        let (comm, fields) = fields(&line);
        assert_eq!(comm, "probe");
        assert_eq!(fields.len(), 52, "{fields:?}");
        let expected: [(usize, &str); 17] = [
            (1, "7"),
            (2, "(probe)"),
            (3, "R"),
            (4, "3"),
            (5, "7"),
            (6, "3"),
            (7, "0"),
            (8, "-1"),
            (9, "0"),
            (18, "20"),
            (19, "0"),
            (20, "2"),
            (25, "18446744073709551615"),
            (38, "17"),
            (39, "3"),
            (41, "0"),
            (52, "0"),
        ];
        for (field, value) in expected {
            assert_eq!(fields[field - 1], value, "field {field}");
        }
        let fixed: Vec<usize> = expected.iter().map(|(field, _)| *field).collect();
        for (index, value) in fields.iter().enumerate() {
            if !fixed.contains(&(index + 1)) {
                assert_eq!(value, "0", "field {} of {fields:?}", index + 1);
            }
        }
        assert_eq!(
            String::from_utf8(line).unwrap(),
            "7 (probe) R 3 7 3 0 -1 0 0 0 0 0 0 0 0 0 20 0 2 0 0 0 0 18446744073709551615 \
             0 0 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0 0 0 0 0 0 0 0 0\n"
        );
    }

    /// The CPU times are the task's and its process's reaped children's,
    /// truncated to clock ticks (`nsec_to_clock_t`): 10 ms each.
    #[test]
    fn stat_shows_the_cpu_times_in_clock_ticks() {
        let ms = |ms: u64| LogicalTime::from_nanos(ms * 1_000_000);
        let stat = TaskStat {
            pid: 7,
            comm: TaskComm::new(b"probe"),
            facts: facts(),
            reader_is_task: true,
            ppid: 3,
            rsslim: RLIM_INFINITY,
            processor: 0,
            cpu: TaskCpuTimes {
                user: ms(1234),
                system: ms(9),
                children_user: ms(20),
                children_system: ms(10) + LogicalTime::from_nanos(9_999_999),
            },
        };
        let (_, fields) = fields(&render_stat(&stat));
        assert_eq!(fields[13..17], ["123", "0", "2", "1"], "{fields:?}");
    }

    /// A thread that does not lead shows exit signal -1 and `PF_FORKNOEXEC`
    /// (it was cloned and never exec'd), and a sibling's state is `S`; a
    /// zombie's is `Z`, with `PF_EXITING`.
    #[test]
    fn stat_of_a_sibling_thread_and_a_zombie() {
        let sibling = TaskStat {
            pid: 8,
            comm: TaskComm::new(b"worker"),
            facts: TaskStatFacts {
                leads: false,
                num_threads: 2,
                ..facts()
            },
            reader_is_task: false,
            ppid: 3,
            rsslim: 4096,
            processor: 0,
            cpu: TaskCpuTimes::default(),
        };
        let (_, sibling_fields) = fields(&render_stat(&sibling));
        assert_eq!(sibling_fields.len(), 52);
        assert_eq!(sibling_fields[1], "(worker)");
        assert_eq!(sibling_fields[2], "S");
        assert_eq!(sibling_fields[8], PF_FORKNOEXEC.to_string());
        assert_eq!(sibling_fields[24], "4096");
        assert_eq!(sibling_fields[37], "-1");
        let zombie = TaskStat {
            facts: TaskStatFacts {
                exited: true,
                forked_without_exec: true,
                ..facts()
            },
            ..sibling
        };
        let (_, zombie_fields) = fields(&render_stat(&zombie));
        assert_eq!(zombie_fields[2], "Z");
        assert_eq!(zombie_fields[8], (PF_FORKNOEXEC | PF_EXITING).to_string());
        // It leads (facts()), so its exit signal is the process's.
        assert_eq!(zombie_fields[37], "17");
    }

    /// The name is kept raw: a newline, a backslash, a byte that is not
    /// UTF-8 and a closing parenthesis are printed as they are
    /// (`proc_task_name(m, p, false)`), and a parser finds it between the
    /// first ` (` and the last `) `. The kernel's `comm` file's newline is
    /// not part of the name.
    #[test]
    fn comm_is_raw_in_stat() {
        let comm = TaskComm::from_comm_file(b"a)b\n\\\xff c\n");
        assert_eq!(comm.as_bytes(), b"a)b\n\\\xff c");
        let stat = TaskStat {
            pid: 1,
            comm,
            facts: facts(),
            reader_is_task: true,
            ppid: 0,
            rsslim: RLIM_INFINITY,
            processor: 0,
            cpu: TaskCpuTimes::default(),
        };
        let line = render_stat(&stat);
        assert!(line.starts_with(b"1 (a)b\n\\\xff c) R 0 "), "{line:?}");
        assert_eq!(TaskComm::from_comm_file(b"\n").as_bytes(), b"");
        assert_eq!(TaskComm::from_comm_file(b"x").as_bytes(), b"x");
    }

    /// Linux stores at most 15 bytes and stops at the first NUL
    /// (`strscpy_pad`).
    #[test]
    fn comm_is_cut_at_15_bytes_and_the_first_nul() {
        assert_eq!(
            TaskComm::new(b"0123456789abcdefghij").as_bytes(),
            b"0123456789abcde"
        );
        assert_eq!(TaskComm::new(b"w1\n").as_bytes(), b"w1\n");
        assert_eq!(TaskComm::new(b"ab\0cd").as_bytes(), b"ab");
        assert_eq!(TaskComm::new(b"").as_bytes(), b"");
        // A NUL at the 16th byte or later is past the cut.
        assert_eq!(
            TaskComm::new(b"0123456789abcdef\0").as_bytes(),
            b"0123456789abcde"
        );
    }

    /// An exec names the task by the last component of the path it was
    /// given (`AT_EXECFN`), cut to 15 bytes, as Linux does for a direct, a
    /// symbolic-link and a script exec (measured on Linux 7.1: an exec
    /// through `a-very-long-symlink-name-xyz` names the task
    /// `a-very-long-sym`, `/dev/fd/3` names it `3`).
    #[test]
    fn exec_names_the_task_by_the_paths_last_component() {
        let name = |path: &[u8]| TaskComm::of_exec_filename(path).as_bytes().to_vec();
        assert_eq!(name(b"/usr/bin/true"), b"true");
        assert_eq!(name(b"true"), b"true");
        assert_eq!(name(b"/proc/self/exe"), b"exe");
        assert_eq!(name(b"/dev/fd/3"), b"3");
        assert_eq!(name(b"sub/execat"), b"execat");
        assert_eq!(
            name(b"/tmp/a-very-long-symlink-name-xyz"),
            b"a-very-long-sym"
        );
    }

    /// Only the reader's own process's file is rendered, and where the
    /// name's source is Detcore's record, only once a name is recorded.
    #[test]
    fn only_the_readers_own_named_tasks_are_rendered() {
        use CommSource::Kernel;
        use CommSource::Record;
        let own = facts();
        assert!(renders_task_stat(&own, 7, Record));
        assert!(renders_task_stat(&own, 7, Kernel));
        assert!(!renders_task_stat(&own, 8, Kernel), "another process's");
        let unnamed = TaskStatFacts { comm: None, ..own };
        assert!(!renders_task_stat(&unnamed, 7, Record), "no name at all");
        assert!(renders_task_stat(&unnamed, 7, Kernel), "the kernel's name");
        let gone = TaskStatFacts {
            process: None,
            ..own
        };
        assert!(!renders_task_stat(&gone, 7, Kernel), "no such process");
    }

    /// A `comm` file is the name, raw, and a newline (`comm_show`): the
    /// bytes a `PR_SET_NAME` of 15 bytes, of raw bytes, or of nothing left,
    /// as the kernel's file shows them (measured on Linux 7.1: `a)b\n\c`
    /// reads back as `a)b\n\c\n`, 7 bytes; an empty name as `\n`).
    #[test]
    fn a_comm_file_is_the_name_and_a_newline() {
        assert_eq!(render_comm(&TaskComm::new(b"probe")), b"probe\n");
        assert_eq!(
            render_comm(&TaskComm::new(b"0123456789abcdefghij")),
            b"0123456789abcde\n"
        );
        assert_eq!(render_comm(&TaskComm::new(b"a)b\n\\c")), b"a)b\n\\c\n");
        assert_eq!(render_comm(&TaskComm::new(b"")), b"\n");
        // What the kernel's file holds is the name back.
        let comm = TaskComm::new(b"w1\n");
        assert_eq!(TaskComm::from_comm_file(&render_comm(&comm)), comm);
    }

    /// Detcore renders a `comm` file only where the name's source is its
    /// record, only the reader's own process's, and only once a name is
    /// recorded; where the source is the kernel, never (the kernel's file,
    /// unclassified, as before).
    #[test]
    fn only_the_readers_own_named_comms_are_rendered_and_only_from_the_record() {
        use CommSource::Kernel;
        use CommSource::Record;
        let own = facts();
        assert!(renders_task_comm(&own, 7, Record));
        assert!(!renders_task_comm(&own, 7, Kernel), "the kernel's file");
        assert!(!renders_task_comm(&own, 8, Record), "another process's");
        let unnamed = TaskStatFacts { comm: None, ..own };
        assert!(!renders_task_comm(&unnamed, 7, Record), "no name at all");
        let gone = TaskStatFacts {
            process: None,
            ..own
        };
        assert!(!renders_task_comm(&gone, 7, Record), "no such process");
    }

    /// The source is the backend's declared capability, and a source that
    /// has no name is a refusal: a failed kernel read is never answered with
    /// the record, which can be stale there (a write to a `comm` file
    /// changes the kernel's name and not the record).
    #[test]
    fn a_failed_kernel_comm_read_is_refused_not_answered_with_the_record() {
        assert_eq!(CommSource::of_backend(true), CommSource::Kernel);
        assert_eq!(CommSource::of_backend(false), CommSource::Record);
        let kernel = TaskComm::new(b"renamed");
        let record = TaskComm::new(b"stale");
        assert_eq!(
            stat_comm(CommSource::Kernel, Ok(kernel), Some(record)),
            Ok(kernel)
        );
        assert!(stat_comm(CommSource::Kernel, Err("unreadable"), Some(record)).is_err());
        assert!(stat_comm(CommSource::Kernel, Err("unreadable"), None).is_err());
        assert_eq!(
            stat_comm(CommSource::Record, Err("none"), Some(record)),
            Ok(record)
        );
        assert_eq!(
            stat_comm(CommSource::Record, Ok(kernel), Some(record)),
            Ok(record),
            "the record's backends read no kernel name"
        );
        assert!(stat_comm(CommSource::Record, Err("none"), None).is_err());
    }
}
