/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System-wide procfs files whose contents are fixed (owner decision, Oct
//! 13; <https://github.com/rrnewton/hermit/issues/4266>): the files that
//! describe the kernel and its limits, each one value from the
//! [`MachineDescription`], and the kernel's tables (interrupts, memory zones,
//! modules and others), rendered for the description's CPUs and memory with
//! every counter zero. `/proc/diskstats` is not one yet: its disks must be
//! those `/sys/block` lists, which still come from the host. Detcore renders them with no kernel open,
//! the same on every backend and host.
//!
//! Before, the host-process backends passed the kernel files through
//! unchanged and kept each table's host shape (its interrupt names, zones
//! and one column per host CPU) with the counters zeroed, while the
//! KVM executor refused most of them and made up the rest.
//!
//! The tables follow the formats of Linux 7.1, the kernel of the reference
//! host, where `uname` reports release 5.2.0; the few format changes between
//! the two are not worth a second copy of each table.

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::machine::MachineDescription;

/// A procfs file whose contents are a fact of the [`MachineDescription`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum FixedProcfsFile {
    /// `/proc/version`: the kernel's banner.
    Version,
    /// `/proc/cmdline`: the kernel's command line.
    Cmdline,
    Osrelease,
    Ostype,
    /// `kernel/version`.
    KernelVersion,
    BootId,
    EntropyAvail,
    OverflowUid,
    OverflowGid,
    PidMax,
    CapLastCap,
    OvercommitMemory,
    MaxMapCount,
    MmapMinAddr,
    /// `/proc/modules`: no modules.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-883): Review module reference-count normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/issues/3815): Review the
    // empty module table.
    Modules,
    /// `/proc/filesystems`: the filesystem types the reference kernel
    /// registers.
    Filesystems,
    /// `/proc/interrupts`: the timer's and the clock's interrupt lines and the
    /// architecture's interrupts, one column per CPU.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-883): Review interrupt and module accounting snapshots.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-883): Review synthetic interrupt and module accounting values.
    Interrupts,
    /// `/proc/softirqs`: Linux's ten softirqs, one column per CPU.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-883): Review interrupt and module accounting snapshots.
    Softirqs,
    /// `/proc/swaps`: no swap areas.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-945): Review host swap-usage normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-945): Review the zeroed /proc/swaps Used column policy.
    Swaps,
    /// `/proc/buddyinfo`: node 0's zones.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-905): Review buddy allocator normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-905): Review the /proc/buddyinfo field policy.
    Buddyinfo,
    /// `/proc/zoneinfo`: node 0's zones, a page set per CPU.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-913): Review host memory-zone accounting normalization.
    Zoneinfo,
    /// `/proc/schedstat`: a line per CPU, no scheduling domains.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-907): Review host scheduler accounting normalization.
    Schedstat,
    /// `/proc/key-users`: root's key quota.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-951): Review key-user resource normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-951): Review the /proc/key-users quota accounting policy.
    KeyUsers,
    /// `/proc/vmstat`: the reference kernel's counters.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-938): Review host VM accounting normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-938): Review host VM accounting normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-938): Review the /proc/vmstat field policy.
    Vmstat,
    /// `/proc/cgroups`: the controllers, all on the unified hierarchy.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3996): Review host cgroup count normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3996): Review the /proc/cgroups field policy.
    // HotSpot reads this at JVM start to detect its container
    // (https://github.com/rrnewton/hermit/issues/3987).
    Cgroups,
    /// `/proc/tty/drivers`: the terminal drivers of a container's `/dev`.
    TtyDrivers,
    /// `/proc/meminfo`: the description's memory, all of it free.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-863): Review canonical guest memory accounting.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-863): Review canonical guest memory accounting.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-863): Review canonical /proc/meminfo guest accounting.
    // TODO-HUMAN-REVIEW(PR-863): Use configured guest memory for meminfo.
    Meminfo,
    /// `fs/file-nr`.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-910): Review host file-table normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-910): Review the /proc/sys/fs/file-nr policy.
    FileNr,
    /// `fs/file-max`.
    FileMax,
    /// `fs/inode-nr`.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-914): Review host-global inode counter normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-914): Review the /proc/sys/fs/inode-nr field policy.
    InodeNr,
    /// `fs/inode-state`.
    InodeState,
    /// `fs/dentry-state`.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-918): Review host-global dentry counter normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-918): Review the /proc/sys/fs/dentry-state field policy.
    DentryState,
    /// `fs/aio-nr`.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-933): Review host-global AIO count normalization.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-933): Review the /proc/sys/fs/aio-nr policy.
    AioNr,
    /// `fs/aio-max-nr`.
    AioMaxNr,
    /// `fs/pipe-max-size`.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-2568): Review determinizing the pipe ceiling read.
    // The ceiling that decides whether a guest may raise a pipe above
    // the pinned capacity. Reported raw it is the HOST's sysctl, so a
    // guest that reads it and acts on it branches on the machine.
    // Reported as the pinned capacity, it agrees with what
    // `F_SETPIPE_SZ` actually allows.
    PipeMaxSize,
}

impl FixedProcfsFile {
    /// Every fixed file.
    pub(crate) const ALL: [Self; 35] = [
        Self::Version,
        Self::Cmdline,
        Self::Osrelease,
        Self::Ostype,
        Self::KernelVersion,
        Self::BootId,
        Self::EntropyAvail,
        Self::OverflowUid,
        Self::OverflowGid,
        Self::PidMax,
        Self::CapLastCap,
        Self::OvercommitMemory,
        Self::MaxMapCount,
        Self::MmapMinAddr,
        Self::Modules,
        Self::Filesystems,
        Self::Interrupts,
        Self::Softirqs,
        Self::Swaps,
        Self::Buddyinfo,
        Self::Zoneinfo,
        Self::Schedstat,
        Self::KeyUsers,
        Self::Vmstat,
        Self::Cgroups,
        Self::TtyDrivers,
        Self::Meminfo,
        Self::FileNr,
        Self::FileMax,
        Self::InodeNr,
        Self::InodeState,
        Self::DentryState,
        Self::AioNr,
        Self::AioMaxNr,
        Self::PipeMaxSize,
    ];

    /// The file's path, NUL-terminated.
    pub(crate) fn c_path(self) -> &'static std::ffi::CStr {
        match self {
            Self::Version => c"/proc/version",
            Self::Cmdline => c"/proc/cmdline",
            Self::Osrelease => c"/proc/sys/kernel/osrelease",
            Self::Ostype => c"/proc/sys/kernel/ostype",
            Self::KernelVersion => c"/proc/sys/kernel/version",
            Self::BootId => c"/proc/sys/kernel/random/boot_id",
            Self::EntropyAvail => c"/proc/sys/kernel/random/entropy_avail",
            Self::OverflowUid => c"/proc/sys/kernel/overflowuid",
            Self::OverflowGid => c"/proc/sys/kernel/overflowgid",
            Self::PidMax => c"/proc/sys/kernel/pid_max",
            Self::CapLastCap => c"/proc/sys/kernel/cap_last_cap",
            Self::OvercommitMemory => c"/proc/sys/vm/overcommit_memory",
            Self::MaxMapCount => c"/proc/sys/vm/max_map_count",
            Self::MmapMinAddr => c"/proc/sys/vm/mmap_min_addr",
            Self::Modules => c"/proc/modules",
            Self::Filesystems => c"/proc/filesystems",
            Self::Interrupts => c"/proc/interrupts",
            Self::Softirqs => c"/proc/softirqs",
            Self::Swaps => c"/proc/swaps",
            Self::Buddyinfo => c"/proc/buddyinfo",
            Self::Zoneinfo => c"/proc/zoneinfo",
            Self::Schedstat => c"/proc/schedstat",
            Self::KeyUsers => c"/proc/key-users",
            Self::Vmstat => c"/proc/vmstat",
            Self::Cgroups => c"/proc/cgroups",
            Self::TtyDrivers => c"/proc/tty/drivers",
            Self::Meminfo => c"/proc/meminfo",
            Self::FileNr => c"/proc/sys/fs/file-nr",
            Self::FileMax => c"/proc/sys/fs/file-max",
            Self::InodeNr => c"/proc/sys/fs/inode-nr",
            Self::InodeState => c"/proc/sys/fs/inode-state",
            Self::DentryState => c"/proc/sys/fs/dentry-state",
            Self::AioNr => c"/proc/sys/fs/aio-nr",
            Self::AioMaxNr => c"/proc/sys/fs/aio-max-nr",
            Self::PipeMaxSize => c"/proc/sys/fs/pipe-max-size",
        }
    }

    /// The file's path.
    pub(crate) fn path(self) -> &'static Path {
        Path::new(
            self.c_path()
                .to_str()
                .expect("fixed procfs paths are ASCII"),
        )
    }

    pub(crate) fn from_path(path: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|file| file.c_path().to_bytes() == path.as_bytes())
    }

    /// The permission bits Linux gives the file: read-only for the kernel's
    /// descriptions, writable by its owner (global root) for the tunables.
    /// Detcore refuses every write, as Linux refuses a process that is not
    /// global root. A guest that is global root would otherwise change the
    /// host's setting, so it is refused too: a known difference.
    pub(crate) fn permissions(self) -> u32 {
        match self {
            Self::OverflowUid
            | Self::OverflowGid
            | Self::PidMax
            | Self::OvercommitMemory
            | Self::MaxMapCount
            | Self::MmapMinAddr
            | Self::FileMax
            | Self::AioMaxNr
            | Self::PipeMaxSize => 0o644,
            _ => 0o444,
        }
    }

    /// Whether the file is an integer sysctl (`proc_dointvec`,
    /// `proc_doulongvec_minmax` and the `fs` counters built on it), whose
    /// reads at a nonzero position Linux answers with end of file. The others
    /// are strings (`proc_dostring`, `proc_do_uuid`) or `seq_file` files,
    /// which serve the rest of their contents.
    pub(crate) fn is_integer(self) -> bool {
        !self.is_seq_file()
            && !matches!(
                self,
                Self::Osrelease | Self::Ostype | Self::KernelVersion | Self::BootId
            )
    }

    /// Whether the file is a `seq_file` (every file outside `/proc/sys`),
    /// which seeks only from its start or position (`seq_lseek`), rather
    /// than a sysctl file.
    pub(crate) fn is_seq_file(self) -> bool {
        !self.c_path().to_bytes().starts_with(b"/proc/sys/")
    }

    /// Whether Linux reads the file through `read_iter`, as every file here
    /// does except `/proc/modules` and `/proc/swaps`, whose `seq_file`
    /// operations have only `read`. A vectored read of one of those two goes
    /// through `do_loop_readv_writev`, which refuses every `RWF_` flag but
    /// `RWF_HIPRI` (measured on Linux 7.1).
    pub(crate) fn has_read_iter(self) -> bool {
        !matches!(self, Self::Modules | Self::Swaps)
    }

    /// The file's contents on `machine`.
    pub(crate) fn contents(self, machine: &MachineDescription) -> Vec<u8> {
        let kernel = &machine.kernel;
        let limits = &machine.limits;
        let cpus = machine.cpus;
        let table = match self {
            Self::Modules => Some(String::new()),
            Self::Filesystems => Some(FILESYSTEMS.to_owned()),
            Self::Interrupts => Some(interrupts(cpus)),
            Self::Softirqs => Some(softirqs(cpus)),
            Self::Swaps => Some("Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n".to_owned()),
            Self::Buddyinfo => Some(buddyinfo()),
            Self::Zoneinfo => Some(zoneinfo(cpus)),
            Self::Schedstat => Some(schedstat(cpus)),
            Self::KeyUsers => Some(KEY_USERS.to_owned()),
            Self::Vmstat => Some(vmstat()),
            Self::Cgroups => Some(cgroups()),
            Self::TtyDrivers => Some(TTY_DRIVERS.to_owned()),
            Self::Meminfo => Some(meminfo(machine.memory)),
            _ => None,
        };
        if let Some(table) = table {
            return table.into_bytes();
        }
        let text = match self {
            Self::Version => kernel.banner(),
            Self::Cmdline => kernel.command_line(),
            Self::Osrelease => kernel.release.clone(),
            Self::Ostype => kernel.sysname.clone(),
            Self::KernelVersion => kernel.version(),
            Self::BootId => kernel.boot_id.clone(),
            Self::EntropyAvail => limits.entropy_avail.to_string(),
            Self::OverflowUid => limits.overflow_uid.to_string(),
            Self::OverflowGid => limits.overflow_gid.to_string(),
            Self::PidMax => limits.pid_max.to_string(),
            Self::CapLastCap => limits.cap_last_cap.to_string(),
            Self::OvercommitMemory => limits.overcommit_memory.to_string(),
            Self::MaxMapCount => limits.max_map_count.to_string(),
            Self::MmapMinAddr => limits.mmap_min_addr.to_string(),
            // Nothing is counted: no open files, inodes or dentries in use,
            // no asynchronous I/O requests.
            Self::FileNr => format!("0\t0\t{}", limits.file_max),
            Self::FileMax => limits.file_max.to_string(),
            Self::InodeNr => "0\t0".to_owned(),
            Self::InodeState => "0\t0\t0\t0\t0\t0\t0".to_owned(),
            // The fourth field is the age limit, 45 seconds.
            Self::DentryState => "0\t0\t45\t0\t0\t0".to_owned(),
            Self::AioNr => "0".to_owned(),
            Self::AioMaxNr => limits.aio_max_nr.to_string(),
            Self::PipeMaxSize => limits.pipe_max_size.to_string(),
            _ => unreachable!("{self:?} is a table"),
        };
        format!("{text}\n").into_bytes()
    }
}

/// `/proc/filesystems` of the reference kernel.
const FILESYSTEMS: &str = include_str!("tables/filesystems.txt");

/// `/proc/key-users` with root's quota alone, at Linux's root limits.
const KEY_USERS: &str = "    0:     0 0/0 0/1000000 0/25000000\n";

/// `/proc/tty/drivers` for a container's `/dev`: the console, the
/// controlling terminal, the pseudo-terminals and the serial ports.
const TTY_DRIVERS: &str = "\
/dev/tty             /dev/tty        5       0 system:/dev/tty
/dev/console         /dev/console    5       1 system:console
/dev/ptmx            /dev/ptmx       5       2 system
/dev/vc/0            /dev/vc/0       4       0 system:vtmaster
serial_8250          /dev/ttyS       4 64-95 serial
pty_slave            /dev/pts      136 0-1048575 pty:slave
pty_master           /dev/ptm      128 0-1048575 pty:master
unknown              /dev/tty        4 1-63 console
";

/// The interrupt lines `/proc/interrupts` lists: the timer and the real-time
/// clock, on the I/O APIC, edge-triggered.
const INTERRUPT_LINES: [(u32, u32, &str); 2] = [(0, 2, "timer"), (8, 8, "rtc0")];

/// The x86 interrupts after the lines, with their descriptions
/// (`arch_show_interrupts`).
const ARCH_INTERRUPTS: [(&str, &str); 15] = [
    ("NMI", "Non-maskable interrupts"),
    ("LOC", "Local timer interrupts"),
    ("PMI", "Performance monitoring interrupts"),
    ("IWI", "IRQ work interrupts"),
    ("RES", "Rescheduling interrupts"),
    ("CAL", "Function call interrupts"),
    ("TLB", "TLB shootdowns"),
    ("TRM", "Thermal event interrupt"),
    ("THR", "Threshold APIC interrupts"),
    ("DFR", "Deferred Error APIC interrupts"),
    ("MCE", "Machine check exceptions"),
    ("MCP", "Machine check polls"),
    ("PIN", "Posted-interrupt notification event"),
    ("NPI", "Nested posted-interrupt event"),
    ("PIW", "Posted-interrupt wakeup event"),
];

/// `/proc/interrupts` (`show_interrupts`) in Linux 7.1's layout, with
/// three-digit line numbers: a count per CPU, each `" %10u"`; for a line,
/// the chip left-aligned in 14 columns, the pin right-aligned in 15, and the
/// trigger and action.
fn interrupts(cpus: u32) -> String {
    let counts = || "          0".repeat(cpus as usize);
    let mut out = " ".repeat(11);
    for cpu in 0..cpus {
        out += &format!("CPU{cpu:<8}");
    }
    out.push('\n');
    for (line, pin, action) in INTERRUPT_LINES {
        out += &format!(
            "{line:>3}:{}  {:<14} {pin:>14}-{:<8}  {action}\n",
            counts(),
            "IO-APIC",
            "edge"
        );
    }
    for (name, description) in ARCH_INTERRUPTS {
        out += &format!("{name:>3}:{}  {description}\n", counts());
    }
    out
}

/// Linux's softirqs, in `softirq_to_name` order.
const SOFTIRQS: [&str; 10] = [
    "HI", "TIMER", "NET_TX", "NET_RX", "BLOCK", "IRQ_POLL", "TASKLET", "SCHED", "HRTIMER", "RCU",
];

/// `/proc/softirqs` (`show_softirqs`).
fn softirqs(cpus: u32) -> String {
    let mut out = " ".repeat(20);
    for cpu in 0..cpus {
        out += &format!("CPU{cpu:<8}");
    }
    out.push('\n');
    for name in SOFTIRQS {
        out += &format!("{name:>12}:{}\n", "          0".repeat(cpus as usize));
    }
    out
}

/// `/proc/buddyinfo` (`frag_show_print`): node 0's populated zones, eleven
/// orders each, none free.
fn buddyinfo() -> String {
    ["DMA", "DMA32", "Normal"]
        .map(|zone| format!("Node 0, zone {zone:>8} {}\n", "     0 ".repeat(11)))
        .concat()
}

/// `/proc/zoneinfo` of the reference kernel's node 0, every number zero, with
/// a page set per CPU.
fn zoneinfo(cpus: u32) -> String {
    let pagesets = (0..cpus)
        .map(|cpu| ZONEINFO_PAGESET.replace("@CPU@", &cpu.to_string()))
        .collect::<String>();
    ZONEINFO.replace("@PAGESETS@\n", &pagesets)
}

/// `/proc/zoneinfo` of the reference kernel's node 0, each zone's page sets
/// marked `@PAGESETS@`.
const ZONEINFO: &str = include_str!("tables/zoneinfo.txt");

/// One CPU's page set in `/proc/zoneinfo`, its number marked `@CPU@`.
const ZONEINFO_PAGESET: &str = include_str!("tables/zoneinfo_pageset.txt");

/// `/proc/schedstat` (`show_schedstat`), version 17: a line per CPU. One
/// CPU has no scheduling domains.
fn schedstat(cpus: u32) -> String {
    let mut out = "version 17\ntimestamp 0\n".to_owned();
    for cpu in 0..cpus {
        out += &format!("cpu{cpu} 0 0 0 0 0 0 0 0 0\n");
    }
    out
}

/// `/proc/vmstat` (`vmstat_show`): the reference kernel's counters, each 0.
fn vmstat() -> String {
    include_str!("tables/vmstat_names.txt")
        .lines()
        .map(|name| format!("{name} 0\n"))
        .collect()
}

/// The controllers `/proc/cgroups` lists, as the reference kernel does.
const CGROUP_CONTROLLERS: [&str; 10] = [
    "cpu",
    "cpuacct",
    "blkio",
    "devices",
    "freezer",
    "net_cls",
    "perf_event",
    "net_prio",
    "hugetlb",
    "pids",
];

/// `/proc/cgroups` (`proc_cgroupstats_show`): every controller on the
/// unified hierarchy (hierarchy 0), enabled, in one cgroup.
fn cgroups() -> String {
    let mut out = "#subsys_name\thierarchy\tnum_cgroups\tenabled\n".to_owned();
    for controller in CGROUP_CONTROLLERS {
        out += &format!("{controller}\t0\t1\t1\n");
    }
    out
}

/// `/proc/meminfo` (`meminfo_proc_show`): `memory` bytes, all free, nothing
/// cached or swapped, each value right-aligned in 8 columns after a
/// 16-column label, as Linux prints them.
fn meminfo(memory: u64) -> String {
    let kb = memory / 1024;
    [
        ("MemTotal:", kb),
        ("MemFree:", kb),
        ("MemAvailable:", kb),
        ("Buffers:", 0),
        ("Cached:", 0),
        ("SwapCached:", 0),
        ("Active:", 0),
        ("Inactive:", 0),
        ("Shmem:", 0),
        ("SReclaimable:", 0),
        ("SwapTotal:", 0),
        ("SwapFree:", 0),
    ]
    .map(|(label, value)| format!("{label:<16}{value:>8} kB\n"))
    .concat()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;

    use super::*;

    fn machine() -> MachineDescription {
        MachineDescription::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
    }

    fn contents(file: FixedProcfsFile) -> String {
        String::from_utf8(file.contents(&machine())).unwrap()
    }

    /// The table, byte for byte: changing a value is a decision, recorded
    /// here and in `MachineDescription`.
    #[test]
    fn fixed_procfs_files_have_their_documented_values() {
        use FixedProcfsFile::*;
        let expected = [
            (
                Version,
                "Linux version 5.2.0 #1 SMP Thu Jan 01 00:00:00 UTC 2026\n",
            ),
            (Cmdline, "BOOT_IMAGE=/vmlinuz-5.2.0 quiet\n"),
            (Osrelease, "5.2.0\n"),
            (Ostype, "Linux\n"),
            (KernelVersion, "#1 SMP Thu Jan 01 00:00:00 UTC 2026\n"),
            (BootId, "6865726d-6974-4000-8000-000000000000\n"),
            (EntropyAvail, "256\n"),
            (OverflowUid, "65534\n"),
            (OverflowGid, "65534\n"),
            (PidMax, "4194304\n"),
            (CapLastCap, "40\n"),
            (OvercommitMemory, "0\n"),
            (MaxMapCount, "65530\n"),
            (MmapMinAddr, "65536\n"),
            (Modules, ""),
            (Swaps, "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n"),
            (KeyUsers, "    0:     0 0/0 0/1000000 0/25000000\n"),
            (
                Interrupts,
                "\x20          CPU0       \n\
                 \x20 0:          0  IO-APIC                     2-edge      timer\n\
                 \x20 8:          0  IO-APIC                     8-edge      rtc0\n\
                 NMI:          0  Non-maskable interrupts\n\
                 LOC:          0  Local timer interrupts\n\
                 PMI:          0  Performance monitoring interrupts\n\
                 IWI:          0  IRQ work interrupts\n\
                 RES:          0  Rescheduling interrupts\n\
                 CAL:          0  Function call interrupts\n\
                 TLB:          0  TLB shootdowns\n\
                 TRM:          0  Thermal event interrupt\n\
                 THR:          0  Threshold APIC interrupts\n\
                 DFR:          0  Deferred Error APIC interrupts\n\
                 MCE:          0  Machine check exceptions\n\
                 MCP:          0  Machine check polls\n\
                 PIN:          0  Posted-interrupt notification event\n\
                 NPI:          0  Nested posted-interrupt event\n\
                 PIW:          0  Posted-interrupt wakeup event\n",
            ),
            (
                Softirqs,
                "                    CPU0       \n\
                 \x20         HI:          0\n\
                 \x20      TIMER:          0\n\
                 \x20     NET_TX:          0\n\
                 \x20     NET_RX:          0\n\
                 \x20      BLOCK:          0\n\
                 \x20   IRQ_POLL:          0\n\
                 \x20    TASKLET:          0\n\
                 \x20      SCHED:          0\n\
                 \x20    HRTIMER:          0\n\
                 \x20        RCU:          0\n",
            ),
            (
                Buddyinfo,
                "Node 0, zone      DMA      0      0      0      0      0      0      0      0      0      0      0 \n\
                 Node 0, zone    DMA32      0      0      0      0      0      0      0      0      0      0      0 \n\
                 Node 0, zone   Normal      0      0      0      0      0      0      0      0      0      0      0 \n",
            ),
            (
                Schedstat,
                "version 17\ntimestamp 0\ncpu0 0 0 0 0 0 0 0 0 0\n",
            ),
            (
                Cgroups,
                "#subsys_name\thierarchy\tnum_cgroups\tenabled\n\
                 cpu\t0\t1\t1\ncpuacct\t0\t1\t1\nblkio\t0\t1\t1\ndevices\t0\t1\t1\n\
                 freezer\t0\t1\t1\nnet_cls\t0\t1\t1\nperf_event\t0\t1\t1\n\
                 net_prio\t0\t1\t1\nhugetlb\t0\t1\t1\npids\t0\t1\t1\n",
            ),
            (
                Meminfo,
                "MemTotal:         976562 kB\n\
                 MemFree:          976562 kB\n\
                 MemAvailable:     976562 kB\n\
                 Buffers:               0 kB\n\
                 Cached:                0 kB\n\
                 SwapCached:            0 kB\n\
                 Active:                0 kB\n\
                 Inactive:              0 kB\n\
                 Shmem:                 0 kB\n\
                 SReclaimable:          0 kB\n\
                 SwapTotal:             0 kB\n\
                 SwapFree:              0 kB\n",
            ),
            (FileNr, "0\t0\t9223372036854775807\n"),
            (FileMax, "9223372036854775807\n"),
            (InodeNr, "0\t0\n"),
            (InodeState, "0\t0\t0\t0\t0\t0\t0\n"),
            (DentryState, "0\t0\t45\t0\t0\t0\n"),
            (AioNr, "0\n"),
            (AioMaxNr, "65536\n"),
            (PipeMaxSize, "8192\n"),
        ];
        for (file, text) in expected {
            assert_eq!(contents(file), text, "{file:?}");
        }
        // The three tables too long to write out here have their own tests.
        let long = [Filesystems, Zoneinfo, Vmstat, TtyDrivers];
        assert_eq!(expected.len() + long.len(), FixedProcfsFile::ALL.len());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-914): Review inode counter fixture coverage.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-918): Review dentry counter fixture coverage.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-933): Review AIO count fixture coverage.
    /// The long tables: well formed, every counter zero, and the same for any
    /// host, since nothing in them is read from one.
    #[test]
    fn the_long_tables_are_well_formed_and_count_nothing() {
        let filesystems = contents(FixedProcfsFile::Filesystems);
        for line in filesystems.lines() {
            let (flag, name) = line.split_once('\t').unwrap();
            assert!(matches!(flag, "" | "nodev") && !name.is_empty(), "{line}");
        }
        for name in [
            "\n\text4\n",
            "\nnodev\tproc\n",
            "\nnodev\tsysfs\n",
            "\nnodev\ttmpfs\n",
        ] {
            assert!(format!("\n{filesystems}").contains(name), "{name}");
        }

        let vmstat = contents(FixedProcfsFile::Vmstat);
        assert_eq!(vmstat.lines().count(), 206);
        assert!(
            vmstat
                .lines()
                .all(|line| line.ends_with(" 0") && line.split(' ').count() == 2)
        );
        assert!(vmstat.lines().any(|line| line == "nr_free_pages 0"));

        let tty = contents(FixedProcfsFile::TtyDrivers);
        assert!(
            tty.lines()
                .any(|line| line.starts_with("pty_slave            /dev/pts      136 "))
        );
        assert_eq!(tty.lines().count(), 8);

        // Apart from the node and CPU numbers, every number in
        // `/proc/zoneinfo` is 0.
        let zoneinfo = contents(FixedProcfsFile::Zoneinfo);
        for line in zoneinfo.lines() {
            if line.starts_with("Node 0, zone ") || line.starts_with("    cpu: ") {
                continue;
            }
            let numbers = line
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|word| word.chars().all(|c| c.is_ascii_digit()) && !word.is_empty());
            for number in numbers {
                assert_eq!(number, "0", "{line}");
            }
        }
        assert!(!zoneinfo.contains('@'), "an unexpanded marker");
    }

    /// The tables agree with each other and with the description: a column,
    /// line or page set per CPU, and the same populated zones.
    #[test]
    fn the_tables_describe_the_machine_consistently() {
        for cpus in [1, 2, 3] {
            let machine = MachineDescription { cpus, ..machine() };
            let table = |file: FixedProcfsFile| String::from_utf8(file.contents(&machine)).unwrap();
            let columns = |text: &str| text.lines().next().unwrap().split_whitespace().count();
            assert_eq!(columns(&table(FixedProcfsFile::Interrupts)), cpus as usize);
            assert_eq!(columns(&table(FixedProcfsFile::Softirqs)), cpus as usize);
            for line in table(FixedProcfsFile::Interrupts).lines().skip(1) {
                let counts = line.split_once(": ").unwrap().1;
                assert_eq!(
                    counts
                        .split_whitespace()
                        .take_while(|word| *word == "0")
                        .count(),
                    cpus as usize,
                    "{line}"
                );
            }
            assert_eq!(
                table(FixedProcfsFile::Schedstat)
                    .lines()
                    .filter(|line| line.starts_with("cpu"))
                    .count(),
                cpus as usize
            );
            let zoneinfo = table(FixedProcfsFile::Zoneinfo);
            let populated = zoneinfo.matches("  pagesets\n").count();
            assert_eq!(populated, 3);
            assert_eq!(
                zoneinfo.matches("    cpu: ").count(),
                populated * cpus as usize
            );
        }
        // Buddyinfo lists the zones whose page sets zoneinfo shows.
        let zoneinfo = contents(FixedProcfsFile::Zoneinfo);
        let populated = zoneinfo
            .split("Node 0, zone ")
            .filter(|zone| zone.contains("  pagesets\n"))
            .map(|zone| zone.lines().next().unwrap().trim().to_owned())
            .collect::<Vec<_>>();
        let buddyinfo = contents(FixedProcfsFile::Buddyinfo)
            .lines()
            .map(|line| line.split_whitespace().nth(3).unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(populated, buddyinfo);
        // The configured memory is what meminfo reports.
        let machine = MachineDescription {
            memory: 2 << 30,
            ..machine()
        };
        let meminfo = String::from_utf8(FixedProcfsFile::Meminfo.contents(&machine)).unwrap();
        assert!(meminfo.starts_with("MemTotal:        2097152 kB\n"));
    }

    /// `/proc/version`, `kernel/osrelease`, `kernel/ostype` and
    /// `kernel/version` describe the kernel `uname` reports, from the same
    /// description.
    #[test]
    fn procfs_agrees_with_uname() {
        let machine = machine();
        let kernel = &machine.kernel;
        let version = contents(FixedProcfsFile::Version);
        assert!(version.starts_with(&format!("{} version {} ", kernel.sysname, kernel.release)));
        assert!(version.trim_end().ends_with(&kernel.version()));
        assert_eq!(
            contents(FixedProcfsFile::Osrelease).trim_end(),
            kernel.release
        );
        assert_eq!(contents(FixedProcfsFile::Ostype).trim_end(), kernel.sysname);
        assert_eq!(
            contents(FixedProcfsFile::KernelVersion).trim_end(),
            kernel.version()
        );
    }

    /// Opened by Detcore, a fixed file reads its contents on the run's
    /// machine, whatever the template it starts from.
    #[test]
    fn a_rendered_fixed_file_reads_its_contents() {
        for fixed in FixedProcfsFile::ALL {
            let mut file = super::super::ProcfsFile::from_path(fixed.path())
                .unwrap()
                .into_rendered();
            let template = file.rendering_template().to_vec();
            file.initialize(
                template,
                super::super::ProcfsSnapshotContext {
                    machine: machine(),
                    ..Default::default()
                },
            );
            assert_eq!(
                file.take(usize::MAX),
                Some(fixed.contents(&machine())),
                "{fixed:?}"
            );
        }
    }

    /// Linux formats an integer sysctl only for a read from position 0, so a
    /// read that copied part of it is followed by end of file; a string
    /// sysctl and a `seq_file` file serve the rest.
    #[test]
    fn integer_files_end_after_a_partial_read() {
        let open = |fixed: FixedProcfsFile| {
            let mut file = super::super::ProcfsFile::from_path(fixed.path())
                .unwrap()
                .into_rendered();
            let template = file.rendering_template().to_vec();
            file.initialize(
                template,
                super::super::ProcfsSnapshotContext {
                    machine: machine(),
                    ..Default::default()
                },
            );
            file
        };
        let mut pid_max = open(FixedProcfsFile::PidMax);
        assert_eq!(pid_max.take(2), Some(b"41".to_vec()));
        assert_eq!(pid_max.take(64), Some(Vec::new()));
        assert_eq!(pid_max.take_at(0, 64), Some(b"4194304\n".to_vec()));

        let mut osrelease = open(FixedProcfsFile::Osrelease);
        assert_eq!(osrelease.take(2), Some(b"5.".to_vec()));
        assert_eq!(osrelease.take(64), Some(b"2.0\n".to_vec()));

        let mut version = open(FixedProcfsFile::Version);
        assert_eq!(version.take(6), Some(b"Linux ".to_vec()));
        assert_eq!(version.take(7), Some(b"version".to_vec()));

        for fixed in FixedProcfsFile::ALL {
            let text = contents(fixed);
            let integer = fixed.c_path().to_bytes().starts_with(b"/proc/sys/")
                && text
                    .split_whitespace()
                    .all(|word| word.parse::<u64>().is_ok());
            assert_eq!(fixed.is_integer(), integer, "{fixed:?}");
        }
    }

    #[test]
    fn fixed_procfs_files_are_found_by_path_alone() {
        for file in FixedProcfsFile::ALL {
            assert_eq!(
                FixedProcfsFile::from_path(file.path().to_str().unwrap()),
                Some(file)
            );
        }
        assert_eq!(
            FixedProcfsFile::from_path("/proc/sys/kernel/hostname"),
            None
        );
        assert_eq!(FixedProcfsFile::from_path("/proc/self/cmdline"), None);
    }
}
