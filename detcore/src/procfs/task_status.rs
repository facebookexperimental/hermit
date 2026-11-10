/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A task's `/proc/<pid>/status` as Detcore renders it (package K item 2b
//! slice 2b-2 of <https://github.com/rrnewton/hermit/issues/5063>).
//!
//! Linux prints a task's `status` from the task itself (`proc_pid_status`,
//! fs/proc/array.c:439-465 in 6.17), world-readable, with no
//! `ptrace_may_access` check on any line. Detcore renders it from the same
//! records its `stat` renders from (`task_stat::TaskStat`), and adds the
//! lines only `status` has, each from the source that answers the guest's
//! own matching query in the same run, on that backend:
//!
//! | line | source |
//! | --- | --- |
//! | `Name` | the task's name, as its `stat` shows it, escaped as Linux escapes it |
//! | `State` | as `stat`'s state letter: `R (running)` for the reader, `Z (zombie)`, else `S (sleeping)` |
//! | `Tgid`, `Pid`, `PPid`, `NStgid`, `NSpid` | the process table, numbered as the reader's `getpid` and `getppid` answer |
//! | `Ngid`, `TracerPid`, `Kthread` | 0: the one node of a kernel without NUMA, Detcore refuses `ptrace` to every guest, and no guest task is a kernel thread |
//! | `Uid`, `Gid` | Detcore's virtual root, which `getuid`, `geteuid`, `getgid` and `getegid` answer on every backend (0) |
//! | `Groups` | the reader's own `getgroups` answer: Detcore makes `setgroups` a no-op, so every guest task keeps the groups the run started with |
//! | `NSpgid`, `NSsid` | Detcore's model, as `getpgid` and `getsid` answer |
//! | the `Vm*` block, `HugetlbPages` | `0 kB` for a task with memory, absent for a zombie, as Linux prints it only with an mm |
//! | `Threads` | the process's tasks, as `stat`'s `num_threads` |
//! | `SigPnd` .. `SigCgt` | the backend's signal sets of the task (`Guest::signal_sets`), with the guest mask a thread published in place of Detcore's private one |
//! | `CapInh`, `CapPrm`, `CapEff` | `capget` of the task, where the backend answers it |
//! | `CapBnd`, `CapAmb` | the reader's own `PR_CAPBSET_READ` and `PR_CAP_AMBIENT_IS_SET` answers, for the reader's own task only: no call answers them for another |
//! | `Cpus_allowed`, `Cpus_allowed_list` | Detcore's affinity, as `sched_getaffinity` answers |
//! | `Mems_allowed`, `Mems_allowed_list` | node 0 alone, as a kernel built without NUMA prints them: `1` and `0`, a mask of `MAX_NUMNODES` (1) bits; `get_mempolicy` itself is ENOSYS there |
//! | `voluntary_ctxt_switches`, `nonvoluntary_ctxt_switches` | 0, as `getrusage` answers |
//!
//! A line whose source does not answer for the task is left out: the signal
//! sets of a zombie (the backends report none for an exited thread), the
//! capability sets where `capget` of the task fails (KVM's executor answers
//! only the caller's own), the bounding and ambient sets of any task but the
//! reader. The lines no source answers for any task are named in
//! [`OMITTED_STATUS_LINES`], each with its reason, and never rendered.
//! procps' `libproc2` reads `status` by line name and keeps a process whose
//! file lacks a line.

use crate::cpu_set::CpuSet;
use crate::procfs::task_stat::TaskStat;

/// The lines of Linux's `status` that Detcore never renders, because no
/// query of the guest's is answered for them in a way Detcore can repeat for
/// any task (the coordinator's 2b-2 rule: a line is rendered only from the
/// source that answers its matching syscall in the same run). One list, so
/// that a ruling can move a line out of it once it has a source.
pub(crate) const OMITTED_STATUS_LINES: [(&str, &str); 11] = [
    (
        "Umask",
        "umask(2) is the only query and it replaces the mask, and it answers for the caller \
         alone; Detcore keeps no record of any task's mask",
    ),
    (
        "FDSize",
        "no call answers the size of a task's descriptor table, and Detcore keeps no record of it",
    ),
    (
        "CoreDumping",
        "no call answers whether a task is dumping core",
    ),
    (
        "THP_enabled",
        "PR_GET_THP_DISABLE answers for the caller alone, and PR_SET_THP_DISABLE reaches the \
         backend, so another task's value has no source",
    ),
    (
        "untag_mask",
        "Detcore answers ARCH_GET_UNTAG_MASK with EINVAL, as a kernel without linear address \
         masking does, which prints no untag_mask line",
    ),
    ("SigQ", "no call answers how many signals a user has queued"),
    (
        "NoNewPrivs",
        "Detcore answers PR_GET_NO_NEW_PRIVS with ENOSYS",
    ),
    ("Seccomp", "Detcore answers PR_GET_SECCOMP with ENOSYS"),
    (
        "Seccomp_filters",
        "Detcore answers PR_GET_SECCOMP with ENOSYS, and refuses every seccomp filter",
    ),
    (
        "Speculation_Store_Bypass",
        "Detcore answers PR_GET_SPECULATION_CTRL with ENOSYS; the value is the host CPU's",
    ),
    (
        "SpeculationIndirectBranch",
        "Detcore answers PR_GET_SPECULATION_CTRL with ENOSYS; the value is the host CPU's",
    ),
];

/// Whether Detcore renders the `status` of the task `facts` describe, whose
/// name comes from `source`, for a reader of the task's own process (`own`)
/// or another, on a backend where it renders any (`Detcore::renders_any_status`):
/// any task of the guest's process table, as Linux serves every task's
/// `status` to every reader in its PID namespace, except, where the name's
/// source is the kernel (ptrace), a task of the reader's own process, whose
/// file stays the kernel's, sanitized, as before; and where the source is
/// Detcore's record (KVM), only once a name is recorded, as for `stat`.
/// A number no process of the table holds is the kernel beneath's to answer.
///
/// Before slice 2b-2 another process's file was the kernel beneath's: on KVM
/// the provider's EACCES for a live process and ENOENT for a zombie, which
/// host timing decided (`other_process_procfs`); on ptrace the kernel's,
/// sanitized, which showed a zombie as `S (sleeping)`. The reader's own
/// process's on KVM was the provider's 13 lines (no memory, signal,
/// capability or CPU lines).
pub(crate) fn renders_task_status(
    facts: &crate::procfs::task_stat::TaskStatFacts,
    source: crate::procfs::task_stat::CommSource,
    own: bool,
) -> bool {
    use crate::procfs::task_stat::CommSource;
    facts.process.is_some()
        && match source {
            CommSource::Kernel => !own,
            CommSource::Record => facts.comm.is_some(),
        }
}

/// Whether `line`, a `status` line's name, is one Detcore never renders
/// ([`OMITTED_STATUS_LINES`]).
pub(crate) fn omits(line: &str) -> bool {
    OMITTED_STATUS_LINES.iter().any(|(name, _)| *name == line)
}

/// The memory lines Linux prints for a task with an mm (`task_mem` and
/// `hugetlb_report_usage`, fs/proc/task_mmu.c and mm/hugetlb.c in 6.17),
/// in its order.
const MEMORY_LINES: [&str; 16] = [
    "VmPeak",
    "VmSize",
    "VmLck",
    "VmPin",
    "VmHWM",
    "VmRSS",
    "RssAnon",
    "RssFile",
    "RssShmem",
    "VmData",
    "VmStk",
    "VmExe",
    "VmLib",
    "VmPTE",
    "VmSwap",
    "HugetlbPages",
];

/// A task's five signal sets as its `status` shows them (`task_sig`,
/// fs/proc/array.c:264-301 in 6.17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StatusSignals {
    pub(crate) pending: u64,
    pub(crate) shared_pending: u64,
    pub(crate) blocked: u64,
    pub(crate) ignored: u64,
    pub(crate) caught: u64,
}

/// A task's capability sets as its `status` shows them (`task_cap`,
/// fs/proc/array.c:311-331 in 6.17), each where its source answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StatusCapabilities {
    /// `CapInh`, `CapPrm` and `CapEff`, from `capget` of the task.
    pub(crate) inheritable_permitted_effective: Option<(u64, u64, u64)>,
    /// `CapBnd`, from the reader's own `PR_CAPBSET_READ`.
    pub(crate) bounding: Option<u64>,
    /// `CapAmb`, from the reader's own `PR_CAP_AMBIENT_IS_SET`.
    pub(crate) ambient: Option<u64>,
}

/// One rendering of a `status` file: what its `stat` renders from, and what
/// only `status` shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskStatus {
    /// The task's `stat` facts, name and numbers (`task_stat::TaskStat`).
    pub(crate) stat: TaskStat,
    /// The reader's supplementary groups, as its `getgroups` answered.
    pub(crate) groups: Vec<u32>,
    /// The task's signal sets, `None` where the backend reports none for it
    /// (a zombie).
    pub(crate) signals: Option<StatusSignals>,
    pub(crate) capabilities: StatusCapabilities,
    /// The CPUs the task may run on and the machine's CPU count, under
    /// `--virtual-cpus`; `None` reports Hermit's single CPU in a 128-bit
    /// mask, as `sched_getaffinity` answers without it.
    pub(crate) cpus_allowed: Option<(CpuSet, u32)>,
}

/// `status`'s `State` line for `stat`'s state letter (`task_state_array`,
/// fs/proc/array.c:126-140 in 6.17).
fn state_name(letter: char) -> &'static str {
    match letter {
        'R' => "R (running)",
        'Z' => "Z (zombie)",
        _ => "S (sleeping)",
    }
}

/// A task's name as `status` prints it (`proc_task_name(m, p, true)`,
/// fs/proc/array.c:99-118 in 6.17): `seq_escape_str` with only `\n` and `\`
/// escaped, as `\n` and `\\`.
fn escaped_name(name: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(name.len());
    for byte in name {
        match byte {
            b'\n' => escaped.extend_from_slice(b"\\n"),
            b'\\' => escaped.extend_from_slice(b"\\\\"),
            other => escaped.push(*other),
        }
    }
    escaped
}

/// A task's `status` in Linux 6.17's order (`proc_pid_status`,
/// fs/proc/array.c:439-465: `task_state`, `task_mem` with an mm,
/// `task_sig`, `task_cap`, `task_seccomp`, `task_cpus_allowed`,
/// `cpuset_task_status_allowed` and `task_context_switch_counts`), with
/// each line's value from [`TaskStatus`] (the module's table), leaving out a
/// line whose source does not answer for the task and every line of
/// [`OMITTED_STATUS_LINES`].
pub(crate) fn render_status(status: &TaskStatus) -> Vec<u8> {
    let stat = &status.stat;
    let facts = &stat.facts;
    let tgid = facts.process.unwrap_or(stat.pid);
    let mut lines: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut line = |name: &'static str, value: String| lines.push((name, value.into_bytes()));
    line("Umask", String::new());
    line(
        "State",
        state_name(crate::procfs::task_stat::state(stat)).to_owned(),
    );
    line("Tgid", tgid.to_string());
    line("Ngid", "0".to_owned());
    line("Pid", stat.pid.to_string());
    line("PPid", stat.ppid.to_string());
    line("TracerPid", "0".to_owned());
    line("Uid", "0\t0\t0\t0".to_owned());
    line("Gid", "0\t0\t0\t0".to_owned());
    line("FDSize", String::new());
    // `task_state` separates the groups with spaces and ends the list with
    // one more (fs/proc/array.c:199-206 in 6.17), so no groups print as a
    // lone space.
    let mut groups = status
        .groups
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    groups.push(' ');
    line("Groups", groups);
    line("NStgid", tgid.to_string());
    line("NSpid", stat.pid.to_string());
    line("NSpgid", facts.process_group.to_string());
    line("NSsid", facts.session.to_string());
    line("Kthread", "0".to_owned());
    if !facts.exited {
        for name in MEMORY_LINES {
            line(name, format!("{:>8} kB", 0));
        }
        line("CoreDumping", String::new());
        line("THP_enabled", String::new());
        line("untag_mask", String::new());
    }
    line("Threads", facts.num_threads.to_string());
    line("SigQ", String::new());
    if let Some(signals) = status.signals {
        line("SigPnd", format!("{:016x}", signals.pending));
        line("ShdPnd", format!("{:016x}", signals.shared_pending));
        line("SigBlk", format!("{:016x}", signals.blocked));
        line("SigIgn", format!("{:016x}", signals.ignored));
        line("SigCgt", format!("{:016x}", signals.caught));
    }
    let capabilities = status.capabilities;
    if let Some((inheritable, permitted, effective)) = capabilities.inheritable_permitted_effective
    {
        line("CapInh", format!("{inheritable:016x}"));
        line("CapPrm", format!("{permitted:016x}"));
        line("CapEff", format!("{effective:016x}"));
    }
    if let Some(bounding) = capabilities.bounding {
        line("CapBnd", format!("{bounding:016x}"));
    }
    if let Some(ambient) = capabilities.ambient {
        line("CapAmb", format!("{ambient:016x}"));
    }
    line("NoNewPrivs", String::new());
    line("Seccomp", String::new());
    line("Seccomp_filters", String::new());
    line("Speculation_Store_Bypass", String::new());
    line("SpeculationIndirectBranch", String::new());
    match status.cpus_allowed {
        Some((cpus, bits)) => {
            line("Cpus_allowed", cpus.mask(bits));
            line("Cpus_allowed_list", cpus.list());
        }
        None => {
            line(
                "Cpus_allowed",
                "00000000,00000000,00000000,00000001".to_owned(),
            );
            line("Cpus_allowed_list", "0".to_owned());
        }
    }
    line("Mems_allowed", "1".to_owned());
    line("Mems_allowed_list", "0".to_owned());
    line("voluntary_ctxt_switches", "0".to_owned());
    line("nonvoluntary_ctxt_switches", "0".to_owned());

    let mut text = b"Name:\t".to_vec();
    text.extend_from_slice(&escaped_name(stat.comm.as_bytes()));
    text.push(b'\n');
    for (name, value) in lines {
        if omits(name) {
            continue;
        }
        text.extend_from_slice(name.as_bytes());
        text.extend_from_slice(b":\t");
        text.extend_from_slice(&value);
        text.push(b'\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procfs::task_stat::TaskComm;
    use crate::procfs::task_stat::TaskCpuTimes;
    use crate::procfs::task_stat::TaskStatFacts;

    fn status(exited: bool, reader_is_task: bool) -> TaskStatus {
        TaskStatus {
            stat: TaskStat {
                pid: 9,
                comm: TaskComm::new(b"worker"),
                facts: TaskStatFacts {
                    comm: Some(TaskComm::new(b"worker")),
                    exited,
                    leads: false,
                    num_threads: 2,
                    process: Some(7),
                    parent: Some(3),
                    ..TaskStatFacts::default()
                },
                reader_is_task,
                ppid: 3,
                rsslim: crate::procfs::task_stat::RLIM_INFINITY,
                processor: 0,
                cpu: TaskCpuTimes::default(),
                permitted: true,
            },
            groups: vec![0, 65534],
            signals: Some(StatusSignals {
                pending: 0,
                shared_pending: 0x4000,
                blocked: 0x200,
                ignored: 0x8000_0800,
                caught: 0x1_0000_0000,
            }),
            capabilities: StatusCapabilities {
                inheritable_permitted_effective: Some((0, 0x1ff_ffff_ffff, 0x1ff_ffff_ffff)),
                bounding: None,
                ambient: None,
            },
            cpus_allowed: None,
        }
    }

    fn names(text: &[u8]) -> Vec<String> {
        String::from_utf8(text.to_vec())
            .unwrap()
            .lines()
            .map(|line| line.split(':').next().unwrap().to_owned())
            .collect()
    }

    /// A live thread's status has Linux's lines in Linux's order, the memory
    /// block at `0 kB` printed as `task_mem` pads it, and none of the
    /// omitted lines.
    #[test]
    fn a_live_threads_status_has_linuxs_lines_in_its_order() {
        let text = String::from_utf8(render_status(&status(false, false))).unwrap();
        let expected = "Name:\tworker\n\
            State:\tS (sleeping)\n\
            Tgid:\t7\n\
            Ngid:\t0\n\
            Pid:\t9\n\
            PPid:\t3\n\
            TracerPid:\t0\n\
            Uid:\t0\t0\t0\t0\n\
            Gid:\t0\t0\t0\t0\n\
            Groups:\t0 65534 \n\
            NStgid:\t7\n\
            NSpid:\t9\n\
            NSpgid:\t0\n\
            NSsid:\t0\n\
            Kthread:\t0\n\
            VmPeak:\t       0 kB\n\
            VmSize:\t       0 kB\n\
            VmLck:\t       0 kB\n\
            VmPin:\t       0 kB\n\
            VmHWM:\t       0 kB\n\
            VmRSS:\t       0 kB\n\
            RssAnon:\t       0 kB\n\
            RssFile:\t       0 kB\n\
            RssShmem:\t       0 kB\n\
            VmData:\t       0 kB\n\
            VmStk:\t       0 kB\n\
            VmExe:\t       0 kB\n\
            VmLib:\t       0 kB\n\
            VmPTE:\t       0 kB\n\
            VmSwap:\t       0 kB\n\
            HugetlbPages:\t       0 kB\n\
            Threads:\t2\n\
            SigPnd:\t0000000000000000\n\
            ShdPnd:\t0000000000004000\n\
            SigBlk:\t0000000000000200\n\
            SigIgn:\t0000000080000800\n\
            SigCgt:\t0000000100000000\n\
            CapInh:\t0000000000000000\n\
            CapPrm:\t000001ffffffffff\n\
            CapEff:\t000001ffffffffff\n\
            Cpus_allowed:\t00000000,00000000,00000000,00000001\n\
            Cpus_allowed_list:\t0\n\
            Mems_allowed:\t1\n\
            Mems_allowed_list:\t0\n\
            voluntary_ctxt_switches:\t0\n\
            nonvoluntary_ctxt_switches:\t0\n";
        assert_eq!(text, expected);
        for (name, _) in OMITTED_STATUS_LINES {
            assert!(!names(text.as_bytes()).contains(&name.to_owned()), "{name}");
        }
    }

    /// A zombie's status says `Z (zombie)` and has no memory block, as Linux
    /// prints it only with an mm; with no signal sets reported for it, it
    /// has no signal lines either.
    #[test]
    fn a_zombies_status_has_no_memory_block() {
        let mut zombie = status(true, false);
        zombie.signals = None;
        zombie.groups = Vec::new();
        let text = render_status(&zombie);
        // No groups print as Linux's lone trailing space.
        assert!(String::from_utf8_lossy(&text).contains("Groups:\t \n"));
        let names = names(&text);
        assert!(String::from_utf8_lossy(&text).contains("State:\tZ (zombie)\n"));
        assert!(
            names
                .iter()
                .all(|name| !MEMORY_LINES.contains(&name.as_str()))
        );
        assert!(!names.contains(&"SigBlk".to_owned()));
        assert!(names.contains(&"Threads".to_owned()));
        assert!(names.contains(&"CapEff".to_owned()));
    }

    /// The reader's own task is running; its bounding and ambient sets are
    /// shown where the reader answered them, and the capability lines are
    /// left out where `capget` did not answer.
    #[test]
    fn the_readers_own_status_is_running_and_shows_what_its_queries_answered() {
        let mut own = status(false, true);
        own.capabilities = StatusCapabilities {
            inheritable_permitted_effective: None,
            bounding: Some(0x1ff_ffff_ffff),
            ambient: Some(0),
        };
        let text = String::from_utf8(render_status(&own)).unwrap();
        assert!(text.contains("State:\tR (running)\n"), "{text}");
        assert!(text.contains("CapBnd:\t000001ffffffffff\nCapAmb:\t0000000000000000\n"));
        assert!(!text.contains("CapInh"), "{text}");
    }

    /// Linux escapes only a newline and a backslash in `Name`.
    #[test]
    fn a_status_name_escapes_newline_and_backslash() {
        assert_eq!(escaped_name(b"a\nb\\c d\t"), b"a\\nb\\\\c d\t".to_vec());
    }

    /// Under `--virtual-cpus` the affinity is the task's, over the machine's
    /// CPUs, as `sched_getaffinity` answers.
    #[test]
    fn a_status_shows_the_tasks_virtual_affinity() {
        let mut virtual_cpus = status(false, false);
        virtual_cpus.cpus_allowed = Some((CpuSet::first(4), 4));
        let text = String::from_utf8(render_status(&virtual_cpus)).unwrap();
        assert!(
            text.contains("Cpus_allowed:\tf\nCpus_allowed_list:\t0-3\n"),
            "{text}"
        );
    }

    /// Where the name's source is the kernel (ptrace), another process's
    /// status is rendered and the reader's own process's stays the kernel's;
    /// where it is Detcore's record (KVM), every process's is rendered once
    /// a name is recorded; a number no process holds is never rendered.
    #[test]
    fn which_statuses_detcore_renders() {
        use crate::procfs::task_stat::CommSource::Kernel;
        use crate::procfs::task_stat::CommSource::Record;
        let named = status(false, false).stat.facts;
        let unnamed = TaskStatFacts {
            comm: None,
            ..named
        };
        let gone = TaskStatFacts {
            process: None,
            ..named
        };
        assert!(renders_task_status(&named, Kernel, false));
        assert!(!renders_task_status(&named, Kernel, true), "own, ptrace");
        assert!(renders_task_status(&named, Record, true), "own, KVM");
        assert!(renders_task_status(&named, Record, false));
        assert!(renders_task_status(&unnamed, Kernel, false));
        assert!(!renders_task_status(&unnamed, Record, false), "no name");
        assert!(!renders_task_status(&gone, Kernel, false));
        assert!(!renders_task_status(&gone, Record, true));
    }

    /// Every omitted line is one Linux prints, and the list names each once.
    #[test]
    fn the_omitted_lines_are_linux_lines_named_once() {
        let mut seen = std::collections::BTreeSet::new();
        for (name, reason) in OMITTED_STATUS_LINES {
            assert!(seen.insert(name), "{name} twice");
            assert!(!reason.is_empty());
        }
        assert!(omits("Umask"));
        assert!(!omits("CapEff"));
    }
}
