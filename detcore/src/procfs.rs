/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic snapshots for volatile procfs and sysfs files.

use std::collections::BTreeMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ProcfsKind {
    Stat,
    Status,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-964): Review thread-self procfs identity normalization.
    ThreadStat,
    ThreadStatus,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-843): Review process and system accounting snapshots.
    ProcessStat,
    Statm,
    ProcessStatus,
    SystemStat,
    Cpuinfo,
    Diskstats,
    Loadavg,
    ProcessIo,
    Uptime,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-863): Review canonical guest memory accounting.
    Meminfo,
    BlockStat,
    NodeMeminfo,
    NodeNumastat,
    HwmonInput,
    ScalingCurFreq,
    Sockstat,
    PtyNr,
    SelfSched,
    Fdinfo,
    AioNr,
    AioMaxNr,
    NumaMaps,
    SmapsRollup,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-944): Review AVX-512 elapsed-time normalization.
    ArchStatus,
    CpuidleCounter,
    Smaps,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-951): Review key-user resource normalization.
    KeyUsers,
    Pressure,
    Buddyinfo,
    Schedstat,
    SelfSchedstat,
    SoftnetStat,
    FileNr,
    FileMax,
    Zoneinfo,
    InodeNr,
    InodeState,
    Protocols,
    BtrfsBytesReserved,
    BtrfsBytesPinned,
    Rtc,
    DentryState,
    Mountinfo,
    RandomUuid,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-945): Review host swap-usage normalization.
    Swaps,
    Locks,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-941): Review transparent-hugepage counter normalization.
    ThpCounter,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-939): Review NUMA node VM accounting normalization.
    NodeVmstat,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-950): Review ACPI CPPC feedback normalization.
    CppcFeedback,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-967): Review Unix socket identity normalization.
    UnixSockets,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-966): Review Btrfs commit telemetry normalization.
    BtrfsCommitStats,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-963): Review sysfs RTC clock normalization.
    SysfsRtcDate,
    SysfsRtcTime,
    SysfsRtcEpoch,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-961): Review proc netlink identity normalization.
    NetlinkSockets,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-960): Review per-CPU host interrupt normalization.
    IrqPerCpuCount,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-883): Review interrupt and module accounting snapshots.
    InterruptCounters,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-883): Review module reference-count normalization.
    Modules,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-958): Review host-global uevent sequence normalization.
    UeventSeqnum,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-957): Review Btrfs reservation normalization.
    BtrfsBytesMayUse,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-956): Review host block queue-depth normalization.
    BlockInflight,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-938): Review host VM accounting normalization.
    Vmstat,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-950): Review ACPI CPPC feedback path recognition.
fn is_cppc_feedback_path(path: &Path) -> bool {
    let mut components = path.iter().rev();
    let Some("feedback_ctrs") = components.next().and_then(|part| part.to_str()) else {
        return false;
    };
    let Some("acpi_cppc") = components.next().and_then(|part| part.to_str()) else {
        return false;
    };
    let Some(cpu) = components.next().and_then(|part| part.to_str()) else {
        return false;
    };
    let Some(cpu_number) = cpu.strip_prefix("cpu") else {
        return false;
    };

    !cpu_number.is_empty() && cpu_number.bytes().all(|byte| byte.is_ascii_digit())
}

const THP_COUNTERS: &[&str] = &[
    "anon_fault_alloc",
    "anon_fault_fallback",
    "anon_fault_fallback_charge",
    "nr_anon",
    "nr_anon_partially_mapped",
    "shmem_alloc",
    "shmem_fallback",
    "shmem_fallback_charge",
    "split",
    "split_deferred",
    "split_failed",
    "swpin",
    "swpin_fallback",
    "swpin_fallback_charge",
    "swpout",
    "swpout_fallback",
    "zswpout",
];

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-941): Review transparent-hugepage counter path recognition.
fn is_thp_counter_path(path: &Path) -> bool {
    let mut components = path.iter().rev();
    let Some(counter) = components.next().and_then(|part| part.to_str()) else {
        return false;
    };
    let Some(stats) = components.next().and_then(|part| part.to_str()) else {
        return false;
    };
    let Some(size_dir) = components.next().and_then(|part| part.to_str()) else {
        return false;
    };

    let Some(size_kb) = size_dir
        .strip_prefix("hugepages-")
        .and_then(|value| value.strip_suffix("kB"))
    else {
        return false;
    };
    stats == "stats"
        && !size_kb.is_empty()
        && size_kb.bytes().all(|byte| byte.is_ascii_digit())
        && THP_COUNTERS.contains(&counter)
}

fn is_btrfs_bytes_reserved_path(path: &Path) -> bool {
    is_btrfs_allocation_gauge_path(path, "bytes_reserved")
}

// TODO-HUMAN-REVIEW(PR-971): Review Btrfs pinned-space path recognition.
fn is_btrfs_bytes_pinned_path(path: &Path) -> bool {
    is_btrfs_allocation_gauge_path(path, "bytes_pinned")
}

fn is_btrfs_allocation_gauge_path(path: &Path, gauge: &str) -> bool {
    let Ok(relative) = path.strip_prefix("/sys/fs/btrfs") else {
        return false;
    };
    let mut components = relative.iter();
    let (Some(uuid), Some("allocation"), Some(class), Some(candidate_gauge), None) = (
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next(),
    ) else {
        return false;
    };

    candidate_gauge == gauge
        && is_btrfs_uuid(uuid)
        && matches!(class, "data" | "metadata" | "system")
}

fn is_btrfs_bytes_may_use_path(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix("/sys/fs/btrfs") else {
        return false;
    };
    let mut components = relative.iter();
    let (Some(uuid), Some("allocation"), Some(class), Some("bytes_may_use"), None) = (
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next(),
    ) else {
        return false;
    };

    is_btrfs_uuid(uuid) && matches!(class, "data" | "metadata" | "system")
}

fn is_btrfs_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn is_cpuidle_counter_path(path: &Path) -> bool {
    let mut components = path.iter().rev();
    let Some(counter) = components.next().and_then(|part| part.to_str()) else {
        return false;
    };
    let Some(state) = components.next().and_then(|part| part.to_str()) else {
        return false;
    };
    let Some(cpuidle) = components.next().and_then(|part| part.to_str()) else {
        return false;
    };

    let Some(state_index) = state.strip_prefix("state") else {
        return false;
    };
    cpuidle == "cpuidle"
        && !state_index.is_empty()
        && state_index.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(counter, "time" | "usage" | "above" | "below" | "rejected")
}

fn sysfs_rtc_kind(path: &Path) -> Option<ProcfsKind> {
    let relative = path.strip_prefix("/sys/class/rtc").ok()?;
    let mut components = relative.iter();
    let rtc = components.next()?.to_str()?;
    let leaf = components.next()?.to_str()?;
    if components.next().is_some() {
        return None;
    }
    let rtc_index = rtc.strip_prefix("rtc")?;
    if rtc_index.is_empty() || !rtc_index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    match leaf {
        "date" => Some(ProcfsKind::SysfsRtcDate),
        "time" => Some(ProcfsKind::SysfsRtcTime),
        "since_epoch" => Some(ProcfsKind::SysfsRtcEpoch),
        _ => None,
    }
}

/// State for a procfs file whose volatile fields require normalization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProcfsFile {
    kind: ProcfsKind,
    target_fd: Option<i32>,
    bound_thread_identity: Option<(i32, i32, i32)>,
    contents: Option<Vec<u8>>,
    offset: usize,
}

/// Guest-visible values used to normalize one procfs snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcfsSnapshotContext {
    pub(crate) virtual_uptime_seconds: u64,
    pub(crate) virtual_realtime_seconds: i64,
    pub(crate) virtual_memory_kb: u64,
    pub(crate) virtual_pid: i32,
    pub(crate) virtual_ppid: i32,
    pub(crate) virtual_pty_count: usize,
    pub(crate) fdinfo_identity: Option<(u64, i32, u64)>,
    pub(crate) random_uuid: Option<[u8; 16]>,
}

impl ProcfsFile {
    /// Recognizes procfs files that contain observed volatile fields.
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let path = normalize_observed_path(path)?;
        let path_text = path.to_str()?;
        let target_fd = parse_fdinfo_target(path_text);
        let kind = match path_text {
            "/proc/self/stat" => ProcfsKind::Stat,
            "/proc/self/status" => ProcfsKind::Status,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-964): Review the thread-self aliases.
            "/proc/thread-self/stat" => ProcfsKind::ThreadStat,
            "/proc/thread-self/status" => ProcfsKind::ThreadStatus,
            "/proc/self/statm" | "/proc/thread-self/statm" => ProcfsKind::Statm,
            other if is_process_file_path(other, "stat") => ProcfsKind::ProcessStat,
            other if is_process_file_path(other, "statm") => ProcfsKind::Statm,
            other if is_process_file_path(other, "status") => ProcfsKind::ProcessStatus,
            "/proc/cpuinfo" => ProcfsKind::Cpuinfo,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-861): Review deterministic kernel I/O accounting.
            "/proc/diskstats" => ProcfsKind::Diskstats,
            "/proc/loadavg" => ProcfsKind::Loadavg,
            "/proc/uptime" => ProcfsKind::Uptime,
            "/proc/stat" => ProcfsKind::SystemStat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-863): Review canonical guest memory accounting.
            "/proc/meminfo" => ProcfsKind::Meminfo,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-914): Review host-global inode counter normalization.
            "/proc/sys/fs/inode-nr" => ProcfsKind::InodeNr,
            "/proc/sys/fs/inode-state" => ProcfsKind::InodeState,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-918): Review host-global dentry counter normalization.
            "/proc/sys/fs/dentry-state" => ProcfsKind::DentryState,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-873): Review deterministic kernel pseudo-file snapshots.
            "/proc/self/mountinfo" => ProcfsKind::Mountinfo,
            "/proc/sys/kernel/random/uuid" => ProcfsKind::RandomUuid,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-933): Review host-global AIO count normalization.
            "/proc/sys/fs/aio-nr" => ProcfsKind::AioNr,
            "/proc/sys/fs/aio-max-nr" => ProcfsKind::AioMaxNr,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-927): Review host-global PTY count normalization.
            "/proc/sys/kernel/pty/nr" => ProcfsKind::PtyNr,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-866): Review host-global socket counter normalization.
            "/proc/net/sockstat" => ProcfsKind::Sockstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-938): Review host VM accounting normalization.
            "/proc/vmstat" => ProcfsKind::Vmstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-958): Review host-global uevent sequence normalization.
            "/sys/kernel/uevent_seqnum" => ProcfsKind::UeventSeqnum,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-939): Review NUMA node VM accounting normalization.
            other if is_node_vmstat_path(other) => ProcfsKind::NodeVmstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-928): Review per-process host scheduler normalization.
            other if is_process_file_path(other, "sched") => ProcfsKind::SelfSched,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-931): Review fdinfo backing-identity normalization.
            _ if target_fd.is_some() => ProcfsKind::Fdinfo,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-934): Review host NUMA observation normalization.
            other if is_process_file_path(other, "numa_maps") => ProcfsKind::NumaMaps,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-937): Review smaps rollup accounting normalization.
            other if is_process_file_path(other, "smaps_rollup") => ProcfsKind::SmapsRollup,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-944): Review AVX-512 elapsed-time normalization.
            other if is_process_file_path(other, "arch_status") => ProcfsKind::ArchStatus,
            "/proc/swaps" => ProcfsKind::Swaps,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-949): Review per-mapping memory accounting normalization.
            other if is_process_file_path(other, "smaps") => ProcfsKind::Smaps,
            "/proc/key-users" => ProcfsKind::KeyUsers,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-903): Review host pressure accounting normalization.
            "/proc/pressure/cpu" | "/proc/pressure/io" | "/proc/pressure/memory" => {
                ProcfsKind::Pressure
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-905): Review buddy allocator normalization.
            "/proc/buddyinfo" => ProcfsKind::Buddyinfo,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-907): Review host scheduler accounting normalization.
            "/proc/schedstat" => ProcfsKind::Schedstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-922): Review per-process host scheduler normalization.
            other if is_process_schedstat_path(other) => ProcfsKind::SelfSchedstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-909): Review softnet counter normalization.
            "/proc/net/softnet_stat" => ProcfsKind::SoftnetStat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-910): Review host file-table normalization.
            "/proc/sys/fs/file-nr" => ProcfsKind::FileNr,
            "/proc/sys/fs/file-max" => ProcfsKind::FileMax,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-913): Review host memory-zone accounting normalization.
            "/proc/zoneinfo" => ProcfsKind::Zoneinfo,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-865): Review deterministic NUMA and hwmon snapshots.
            other if is_numa_node_file(other, "meminfo") => ProcfsKind::NodeMeminfo,
            other if is_numa_node_file(other, "numastat") => ProcfsKind::NodeNumastat,
            other if is_hwmon_input_file(other) => ProcfsKind::HwmonInput,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-916): Review live protocol allocation counter normalization.
            "/proc/net/protocols" => ProcfsKind::Protocols,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-926): Review kernel lock identity normalization.
            "/proc/locks" => ProcfsKind::Locks,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-969): Review Btrfs reserved-byte normalization.
            other if is_btrfs_bytes_reserved_path(Path::new(other)) => {
                ProcfsKind::BtrfsBytesReserved
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-917): Review host RTC normalization.
            "/proc/driver/rtc" => ProcfsKind::Rtc,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-961): Review proc netlink identity normalization.
            "/proc/net/netlink" => ProcfsKind::NetlinkSockets,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-966): Review Btrfs commit telemetry normalization.
            _ if is_btrfs_commit_stats_path(&path) => ProcfsKind::BtrfsCommitStats,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-967): Review Unix socket identity normalization.
            "/proc/net/unix" => ProcfsKind::UnixSockets,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-883): Review interrupt and module accounting snapshots.
            "/proc/interrupts" | "/proc/softirqs" => ProcfsKind::InterruptCounters,
            "/proc/modules" => ProcfsKind::Modules,
            // AUTONOMOUS-BOT-IMPLEMENTED
            other if is_cpufreq_policy_value_path(Path::new(other)) => ProcfsKind::ScalingCurFreq,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-950): Review ACPI CPPC feedback normalization.
            other if is_cppc_feedback_path(Path::new(other)) => ProcfsKind::CppcFeedback,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-932): Review average-frequency snapshot normalization.
            other if is_process_io_path(other) => ProcfsKind::ProcessIo,
            other if is_block_stat_path(other) => ProcfsKind::BlockStat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-971): Review Btrfs pinned-space normalization.
            other if is_btrfs_bytes_pinned_path(Path::new(other)) => ProcfsKind::BtrfsBytesPinned,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-935): Review cpuidle counter normalization.
            other if is_cpuidle_counter_path(Path::new(other)) => ProcfsKind::CpuidleCounter,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-941): Review transparent-hugepage counter normalization.
            other if is_thp_counter_path(Path::new(other)) => ProcfsKind::ThpCounter,
            // TODO-HUMAN-REVIEW(PR-963): Review sysfs RTC clock normalization.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-960): Review per-CPU host interrupt normalization.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-957): Review Btrfs reservation normalization.
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-956): Review host block queue-depth normalization.
            _ if is_block_inflight_path(&path) => ProcfsKind::BlockInflight,
            _ if is_btrfs_bytes_may_use_path(&path) => ProcfsKind::BtrfsBytesMayUse,
            _ if is_irq_per_cpu_count_path(&path) => ProcfsKind::IrqPerCpuCount,
            _ => sysfs_rtc_kind(&path)?,
        };
        Some(Self {
            kind,
            target_fd,
            bound_thread_identity: None,
            contents: None,
            offset: 0,
        })
    }

    /// Returns true until the underlying procfs content has been captured.
    pub(crate) fn needs_snapshot(&self) -> bool {
        self.contents.is_none()
    }

    /// Returns true when the procfs inode binds to the thread that opened it.
    pub(crate) fn needs_bound_thread_identity(&self) -> bool {
        matches!(self.kind, ProcfsKind::ThreadStat | ProcfsKind::ThreadStatus)
    }

    /// Binds a thread-self procfs inode to its opener's process identity.
    // TODO-HUMAN-REVIEW(PR-964): Review open-time procfs thread binding.
    pub(crate) fn bind_thread_identity(&mut self, tgid: i32, tid: i32, ppid: i32) {
        assert!(
            self.needs_bound_thread_identity(),
            "only thread-self procfs files bind an opener identity"
        );
        self.bound_thread_identity = Some((tgid, tid, ppid));
    }

    /// Returns true when this snapshot consumes deterministic random bytes.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic kernel UUID generation.
    pub(crate) fn needs_random_uuid(&self) -> bool {
        self.kind == ProcfsKind::RandomUuid
    }

    /// Normalizes and stores a complete snapshot captured from the kernel.
    // TODO-HUMAN-REVIEW(PR-723): Review procfs snapshot identity normalization.
    // TODO-HUMAN-REVIEW(PR-955): Review deterministic UUID snapshot input.
    pub(crate) fn initialize(&mut self, contents: Vec<u8>, context: ProcfsSnapshotContext) {
        let ProcfsSnapshotContext {
            virtual_uptime_seconds,
            virtual_realtime_seconds,
            virtual_memory_kb,
            virtual_pid,
            virtual_ppid,
            virtual_pty_count,
            fdinfo_identity,
            random_uuid,
        } = context;
        self.contents = Some(match self.kind {
            ProcfsKind::Stat => sanitize_stat(&contents, Some((virtual_pid, virtual_ppid))),
            ProcfsKind::Status => {
                sanitize_status(&contents, Some((virtual_pid, virtual_pid, virtual_ppid)))
            }
            ProcfsKind::ThreadStat => {
                let (_, tid, ppid) = self
                    .bound_thread_identity
                    .expect("thread-self stat was not bound when opened");
                sanitize_stat(&contents, Some((tid, ppid)))
            }
            ProcfsKind::ThreadStatus => {
                let (tgid, tid, ppid) = self
                    .bound_thread_identity
                    .expect("thread-self status was not bound when opened");
                sanitize_status(&contents, Some((tgid, tid, ppid)))
            }
            ProcfsKind::ProcessStat => sanitize_stat(&contents, None),
            ProcfsKind::Statm => sanitize_statm(&contents),
            ProcfsKind::ProcessStatus => sanitize_status(&contents, None),
            ProcfsKind::SystemStat => sanitize_system_stat(
                &contents,
                virtual_uptime_seconds,
                virtual_realtime_seconds
                    .saturating_sub(i64::try_from(virtual_uptime_seconds).unwrap_or(i64::MAX)),
            ),
            ProcfsKind::Cpuinfo => sanitize_cpuinfo(&contents),
            ProcfsKind::Diskstats => sanitize_diskstats(&contents),
            ProcfsKind::Loadavg => sanitize_loadavg(&contents),
            ProcfsKind::ProcessIo => sanitize_process_io(&contents),
            ProcfsKind::Uptime => sanitize_uptime(&contents, virtual_uptime_seconds),
            ProcfsKind::Meminfo => sanitize_meminfo(&contents, virtual_memory_kb),
            ProcfsKind::BlockStat => sanitize_block_stat(&contents),
            ProcfsKind::NodeMeminfo => sanitize_node_meminfo(&contents),
            ProcfsKind::NodeNumastat => sanitize_node_numastat(&contents),
            ProcfsKind::HwmonInput => sanitize_numeric_scalar(&contents),
            ProcfsKind::ScalingCurFreq => sanitize_scaling_cur_freq(&contents),
            ProcfsKind::Sockstat => sanitize_sockstat(&contents),
            ProcfsKind::Vmstat => sanitize_vmstat(&contents),
            ProcfsKind::UeventSeqnum => sanitize_uevent_seqnum(&contents),
            ProcfsKind::BtrfsBytesMayUse => sanitize_btrfs_bytes_may_use(&contents),
            ProcfsKind::BlockInflight => sanitize_block_inflight(&contents),
            ProcfsKind::IrqPerCpuCount => sanitize_irq_per_cpu_count(&contents),
            ProcfsKind::PtyNr => sanitize_pty_nr(&contents, virtual_pty_count),
            ProcfsKind::SelfSched => sanitize_self_sched(&contents),
            ProcfsKind::Fdinfo => sanitize_fdinfo(&contents, fdinfo_identity),
            ProcfsKind::AioNr => sanitize_aio_nr(&contents),
            ProcfsKind::AioMaxNr => sanitize_aio_nr(&contents),
            ProcfsKind::NumaMaps => sanitize_numa_maps(&contents),
            ProcfsKind::SmapsRollup => sanitize_smaps_rollup(&contents),
            ProcfsKind::ArchStatus => sanitize_arch_status(&contents),
            ProcfsKind::Swaps => sanitize_swaps(&contents),
            ProcfsKind::CpuidleCounter => sanitize_cpuidle_counter(&contents),
            ProcfsKind::Smaps => sanitize_smaps(&contents),
            ProcfsKind::KeyUsers => sanitize_key_users(&contents),
            ProcfsKind::Pressure => sanitize_pressure(&contents),
            ProcfsKind::Buddyinfo => sanitize_buddyinfo(&contents),
            ProcfsKind::Schedstat => sanitize_schedstat(&contents),
            ProcfsKind::SelfSchedstat => sanitize_self_schedstat(&contents),
            ProcfsKind::SoftnetStat => sanitize_softnet_stat(&contents),
            ProcfsKind::FileNr => sanitize_file_nr(&contents),
            ProcfsKind::FileMax => sanitize_file_max(&contents),
            ProcfsKind::Zoneinfo => sanitize_zoneinfo(&contents),
            ProcfsKind::InodeNr => sanitize_inode_nr(&contents),
            ProcfsKind::InodeState => sanitize_inode_state(&contents),
            ProcfsKind::Protocols => sanitize_protocols(&contents),
            ProcfsKind::BtrfsBytesReserved => sanitize_btrfs_bytes_reserved(&contents),
            ProcfsKind::BtrfsBytesPinned => sanitize_btrfs_bytes_pinned(&contents),
            ProcfsKind::Rtc => sanitize_rtc(&contents, virtual_realtime_seconds),
            ProcfsKind::DentryState => sanitize_dentry_state(&contents),
            ProcfsKind::NetlinkSockets => sanitize_netlink_sockets(&contents),
            ProcfsKind::Locks => sanitize_locks(&contents),
            ProcfsKind::NodeVmstat => sanitize_node_vmstat(&contents),
            ProcfsKind::CppcFeedback => sanitize_cppc_feedback(&contents),
            ProcfsKind::UnixSockets => sanitize_unix_sockets(&contents),
            ProcfsKind::BtrfsCommitStats => sanitize_btrfs_commit_stats(&contents),
            ProcfsKind::SysfsRtcDate | ProcfsKind::SysfsRtcTime | ProcfsKind::SysfsRtcEpoch => {
                sanitize_sysfs_rtc_attribute(&contents, self.kind, virtual_realtime_seconds)
            }
            ProcfsKind::ThpCounter => sanitize_thp_counter(&contents),
            ProcfsKind::InterruptCounters => sanitize_interrupt_counters(&contents),
            ProcfsKind::Modules => sanitize_modules(&contents),
            ProcfsKind::Mountinfo => sanitize_mountinfo(&contents),
            ProcfsKind::RandomUuid => sanitize_random_uuid(
                &contents,
                random_uuid.expect("random UUID snapshot omitted deterministic bytes"),
            ),
        });
    }

    /// Returns the next bytes from the normalized snapshot.
    pub(crate) fn take(&mut self, maximum: usize) -> Option<Vec<u8>> {
        let bytes = self.take_at(self.offset, maximum)?;
        self.offset = self.offset.saturating_add(bytes.len());
        Some(bytes)
    }

    /// Returns bytes at an explicit offset without changing the shared cursor.
    pub(crate) fn take_at(&self, offset: usize, maximum: usize) -> Option<Vec<u8>> {
        let contents = self.contents.as_ref()?;
        let start = offset.min(contents.len());
        let end = start.saturating_add(maximum).min(contents.len());
        Some(contents[start..end].to_vec())
    }

    /// Returns the shared cursor and initialized snapshot length.
    pub(crate) fn position(&self) -> (usize, Option<usize>) {
        (self.offset, self.contents.as_ref().map(Vec::len))
    }

    /// Updates the shared cursor used by all aliases of this open file.
    pub(crate) fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    pub(crate) fn target_fd(&self) -> Option<i32> {
        self.target_fd
    }
}

fn normalize_observed_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => return None,
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

fn is_process_file_path(path: &str, filename: &str) -> bool {
    let Some(relative) = path.strip_prefix("/proc/") else {
        return false;
    };
    let components = relative.split('/').collect::<Vec<_>>();
    match components.as_slice() {
        [task, candidate] => is_proc_task_name(task) && *candidate == filename,
        [process, "task", thread, candidate] => {
            is_proc_process_name(process) && is_numeric_id(thread) && *candidate == filename
        }
        _ => false,
    }
}

fn parse_fdinfo_target(path: &str) -> Option<i32> {
    let relative = path.strip_prefix("/proc/")?;
    let components = relative.split('/').collect::<Vec<_>>();
    let fd = match components.as_slice() {
        [task, "fdinfo", fd] if is_proc_task_name(task) => *fd,
        [process, "task", thread, "fdinfo", fd]
            if is_proc_process_name(process) && is_numeric_id(thread) =>
        {
            *fd
        }
        _ => return None,
    };
    fd.parse().ok()
}

const VIRTUAL_CPU_FREQUENCY_KHZ: u64 = 1_000_000;

fn is_cpufreq_policy_value_path(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix("/sys/devices/system/cpu") else {
        return false;
    };
    let components = relative
        .iter()
        .filter_map(|component| component.to_str())
        .collect::<Vec<_>>();
    let attribute = match components.as_slice() {
        [cpu, "cpufreq", attribute]
            if cpu.strip_prefix("cpu").is_some_and(|id| {
                !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
            }) =>
        {
            *attribute
        }
        ["cpufreq", policy, attribute]
            if policy.strip_prefix("policy").is_some_and(|id| {
                !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
            }) =>
        {
            *attribute
        }
        _ => return false,
    };
    matches!(
        attribute,
        "scaling_cur_freq"
            | "cpuinfo_cur_freq"
            | "cpuinfo_avg_freq"
            | "cpuinfo_min_freq"
            | "cpuinfo_max_freq"
            | "scaling_min_freq"
            | "scaling_max_freq"
    )
}

fn is_process_io_path(path: &str) -> bool {
    path == "/proc/self/io"
        || path
            .strip_prefix("/proc/")
            .and_then(|path| path.strip_suffix("/io"))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_process_schedstat_path(path: &str) -> bool {
    let Some(relative) = path.strip_prefix("/proc/") else {
        return false;
    };
    let components = relative.split('/').collect::<Vec<_>>();
    match components.as_slice() {
        [task, "schedstat"] => is_proc_task_name(task),
        [process, "task", thread, "schedstat"] => {
            is_proc_process_name(process) && is_numeric_id(thread)
        }
        _ => false,
    }
}

fn is_proc_task_name(name: &str) -> bool {
    matches!(name, "self" | "thread-self") || is_numeric_id(name)
}

fn is_proc_process_name(name: &str) -> bool {
    name == "self" || is_numeric_id(name)
}

fn is_numeric_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_block_stat_path(path: &str) -> bool {
    path.strip_prefix("/sys/block/")
        .and_then(|path| path.strip_suffix("/stat"))
        .is_some_and(|device| !device.is_empty() && !device.contains('/'))
}

fn is_numa_node_file(path: &str, filename: &str) -> bool {
    path.strip_prefix("/sys/devices/system/node/node")
        .and_then(|path| path.split_once('/'))
        .is_some_and(|(node, leaf)| {
            !node.is_empty() && node.bytes().all(|byte| byte.is_ascii_digit()) && leaf == filename
        })
}

fn is_hwmon_input_file(path: &str) -> bool {
    path.strip_prefix("/sys/class/hwmon/hwmon")
        .and_then(|path| path.split_once('/'))
        .is_some_and(|(instance, attribute)| {
            !instance.is_empty()
                && instance.bytes().all(|byte| byte.is_ascii_digit())
                && !attribute.contains('/')
                && attribute.ends_with("_input")
        })
}

// TODO-HUMAN-REVIEW(PR-723): Review /proc stat identity field normalization.
// TODO-HUMAN-REVIEW(PR-843): Review process memory and runtime normalization.
fn sanitize_stat(contents: &[u8], virtual_identity: Option<(i32, i32)>) -> Vec<u8> {
    const VOLATILE_FIELDS: &[usize] = &[
        10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 23, 24, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36,
        37, 39, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
    ];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let Some(comm_start) = text.find(" (") else {
        return contents.to_vec();
    };
    let Some(comm_end) = text.rfind(") ") else {
        return contents.to_vec();
    };
    let comm = &text[comm_start..=comm_end];
    let mut fields = text[comm_end + 2..]
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if fields.len() < 50 {
        return contents.to_vec();
    }

    // `fields` starts with proc stat field 3 (state).
    let pid = if let Some((virtual_pid, virtual_ppid)) = virtual_identity {
        fields[4 - 3] = virtual_ppid.to_string();
        fields[5 - 3] = "0".to_owned();
        fields[6 - 3] = "0".to_owned();
        virtual_pid.to_string()
    } else {
        fields[0] = "S".to_owned();
        text[..comm_start].to_owned()
    };
    for field in VOLATILE_FIELDS {
        fields[*field - 3] = "0".to_owned();
    }
    format!("{pid}{comm} {}\n", fields.join(" ")).into_bytes()
}

fn sanitize_statm(contents: &[u8]) -> Vec<u8> {
    let fields = contents
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() != 7
        || fields
            .iter()
            .any(|field| !field.iter().all(u8::is_ascii_digit))
    {
        return contents.to_vec();
    }
    b"0 0 0 0 0 0 0\n".to_vec()
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#553)
// TODO-HUMAN-REVIEW(PR-723): Review /proc status identity field normalization.
fn sanitize_status(contents: &[u8], virtual_identity: Option<(i32, i32, i32)>) -> Vec<u8> {
    const STATE: &[u8] = b"State:";
    const TGID: &[u8] = b"Tgid:";
    const PID: &[u8] = b"Pid:";
    const PPID: &[u8] = b"PPid:";
    const TRACER_PID: &[u8] = b"TracerPid:";
    const NS_TGID: &[u8] = b"NStgid:";
    const NS_PID: &[u8] = b"NSpid:";
    const NS_PGID: &[u8] = b"NSpgid:";
    const NS_SID: &[u8] = b"NSsid:";
    const SIGQ: &[u8] = b"SigQ:";
    const CPUS_ALLOWED: &[u8] = b"Cpus_allowed:";
    const CPUS_ALLOWED_LIST: &[u8] = b"Cpus_allowed_list:";
    const VOLUNTARY: &[u8] = b"voluntary_ctxt_switches:";
    const NONVOLUNTARY: &[u8] = b"nonvoluntary_ctxt_switches:";
    const MEMORY_FIELDS: &[&[u8]] = &[
        b"VmPeak",
        b"VmSize",
        b"VmLck",
        b"VmPin",
        b"VmHWM",
        b"VmRSS",
        b"RssAnon",
        b"RssFile",
        b"RssShmem",
        b"VmData",
        b"VmStk",
        b"VmExe",
        b"VmLib",
        b"VmPTE",
        b"VmSwap",
        b"HugetlbPages",
    ];

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(STATE) {
            normalized.extend_from_slice(b"State:\tS (sleeping)");
        } else if let Some((virtual_tgid, _, _)) = virtual_identity
            && (body.starts_with(TGID) || body.starts_with(NS_TGID))
        {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(format!(":\t{virtual_tgid}").as_bytes());
        } else if let Some((_, virtual_pid, _)) = virtual_identity
            && (body.starts_with(PID) || body.starts_with(NS_PID))
        {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(format!(":\t{virtual_pid}").as_bytes());
        } else if let Some((_, _, virtual_ppid)) = virtual_identity
            && body.starts_with(PPID)
        {
            normalized.extend_from_slice(PPID);
            normalized.extend_from_slice(format!("\t{virtual_ppid}").as_bytes());
        } else if body.starts_with(TRACER_PID) {
            normalized.extend_from_slice(TRACER_PID);
            normalized.extend_from_slice(if virtual_identity.is_some() {
                b"\t1"
            } else {
                b"\t0"
            });
        } else if body.starts_with(NS_PGID) || body.starts_with(NS_SID) {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(b":\t0");
        } else if body.starts_with(SIGQ) {
            normalized.extend_from_slice(SIGQ);
            normalized.extend_from_slice(b"\t0/0");
        } else if body.starts_with(CPUS_ALLOWED) {
            normalized.extend_from_slice(CPUS_ALLOWED);
            normalized.extend_from_slice(b"\t00000000,00000000,00000000,00000001");
        } else if body.starts_with(CPUS_ALLOWED_LIST) {
            normalized.extend_from_slice(CPUS_ALLOWED_LIST);
            normalized.extend_from_slice(b"\t0");
        } else if body.starts_with(VOLUNTARY) {
            normalized.extend_from_slice(VOLUNTARY);
            normalized.extend_from_slice(b"\t0");
        } else if body.starts_with(NONVOLUNTARY) {
            normalized.extend_from_slice(NONVOLUNTARY);
            normalized.extend_from_slice(b"\t0");
        } else if let Some(name_end) = body.iter().position(|byte| *byte == b':')
            && MEMORY_FIELDS.contains(&&body[..name_end])
        {
            normalized.extend_from_slice(&body[..name_end]);
            normalized.extend_from_slice(b":\t0 kB");
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_cpuinfo(contents: &[u8]) -> Vec<u8> {
    const CPU_MHZ: &[u8] = b"cpu MHz";

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(CPU_MHZ) {
            normalized.extend_from_slice(b"cpu MHz\t\t: 1000.000");
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-861): Review deterministic kernel I/O accounting values.
fn sanitize_diskstats(contents: &[u8]) -> Vec<u8> {
    sanitize_numeric_lines(contents, 3)
}

fn sanitize_block_stat(contents: &[u8]) -> Vec<u8> {
    sanitize_numeric_lines(contents, 0)
}

fn sanitize_numeric_lines(contents: &[u8], stable_fields: usize) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let fields = body
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let counters = fields.get(stable_fields..).unwrap_or_default();
        if !counters.is_empty()
            && counters
                .iter()
                .all(|field| field.iter().all(u8::is_ascii_digit))
        {
            for (index, field) in fields.iter().take(stable_fields).enumerate() {
                if index > 0 {
                    normalized.push(b' ');
                }
                normalized.extend_from_slice(field);
            }
            for (index, _) in counters.iter().enumerate() {
                if !normalized.is_empty() && normalized.last() != Some(&b'\n') {
                    normalized.push(b' ');
                }
                let value = match index {
                    0 | 4 => 1,
                    2 | 6 => 8,
                    _ => 0,
                };
                normalized.extend_from_slice(value.to_string().as_bytes());
            }
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-865): Review synthetic NUMA and hwmon accounting values.
fn sanitize_node_numastat(contents: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let fields = body
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if fields.len() == 2 && fields[1].iter().all(u8::is_ascii_digit) {
            normalized.extend_from_slice(fields[0]);
            normalized.extend_from_slice(b" 0");
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_process_io(contents: &[u8]) -> Vec<u8> {
    const COUNTERS: &[&[u8]] = &[
        b"rchar",
        b"wchar",
        b"syscr",
        b"syscw",
        b"read_bytes",
        b"write_bytes",
        b"cancelled_write_bytes",
    ];

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let name_end = body.iter().position(|byte| *byte == b':');
        let name = name_end.map_or(body, |end| &body[..end]);
        if COUNTERS.contains(&name) {
            normalized.extend_from_slice(name);
            normalized.extend_from_slice(b": 0");
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_node_meminfo(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let mut normalized = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let Some((label, value)) = body.split_once(':') else {
            normalized.push_str(body);
            if has_newline {
                normalized.push('\n');
            }
            continue;
        };
        let field = label.split_whitespace().last().unwrap_or_default();
        let mut value_fields = value.split_whitespace();
        let numeric = value_fields.next();
        let unit = value_fields.next();
        if numeric.is_some_and(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
            && value_fields.next().is_none()
        {
            // Keep the node online and internally consistent while hiding live usage.
            let synthetic = match field {
                "MemTotal" | "MemFree" => 1_048_576,
                _ => 0,
            };
            normalized.push_str(label);
            normalized.push_str(": ");
            normalized.push_str(&synthetic.to_string());
            if let Some(unit) = unit {
                normalized.push(' ');
                normalized.push_str(unit);
            }
        } else {
            normalized.push_str(body);
        }
        if has_newline {
            normalized.push('\n');
        }
    }
    normalized.into_bytes()
}

fn sanitize_numeric_scalar(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    if text.trim().parse::<i64>().is_err() {
        return contents.to_vec();
    }
    if text.ends_with('\n') {
        b"0\n".to_vec()
    } else {
        b"0".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-764)
// TODO-HUMAN-REVIEW(PR-932): Review average-frequency snapshot normalization.
/// Expose one coherent virtual cpufreq policy across current, average, and
/// min/max attributes.
fn sanitize_scaling_cur_freq(contents: &[u8]) -> Vec<u8> {
    let Ok(value) = std::str::from_utf8(contents) else {
        return Vec::new();
    };
    if value.trim().parse::<u64>().is_err() {
        return Vec::new();
    }
    format!("{VIRTUAL_CPU_FREQUENCY_KHZ}\n").into_bytes()
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-969): Review Btrfs bytes_reserved normalization.
fn sanitize_btrfs_bytes_reserved(contents: &[u8]) -> Vec<u8> {
    let has_newline = contents.ends_with(b"\n");
    let value = contents.strip_suffix(b"\n").unwrap_or(contents);
    let valid_value = std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .is_some();
    if !valid_value {
        return contents.to_vec();
    }

    if has_newline {
        b"0\n".to_vec()
    } else {
        b"0".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-971): Review Btrfs bytes_pinned normalization.
fn sanitize_btrfs_bytes_pinned(contents: &[u8]) -> Vec<u8> {
    let has_newline = contents.ends_with(b"\n");
    let value = contents.strip_suffix(b"\n").unwrap_or(contents);
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return contents.to_vec();
    }

    if has_newline {
        b"0\n".to_vec()
    } else {
        b"0".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-935): Review cpuidle counter normalization.
fn sanitize_cpuidle_counter(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\n".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-941): Review transparent-hugepage counter normalization.
fn sanitize_thp_counter(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\n".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-950): Review ACPI CPPC feedback counter values.
fn sanitize_cppc_feedback(contents: &[u8]) -> Vec<u8> {
    let has_newline = contents.ends_with(b"\n");
    let body = contents.strip_suffix(b"\n").unwrap_or(contents);
    let Ok(text) = std::str::from_utf8(body) else {
        return contents.to_vec();
    };
    let mut fields = text.split_whitespace();
    let (Some(reference), Some(delivered), None) = (fields.next(), fields.next(), fields.next())
    else {
        return contents.to_vec();
    };
    let valid_reference = reference
        .strip_prefix("ref:")
        .is_some_and(|value| value.parse::<u64>().is_ok());
    let valid_delivered = delivered
        .strip_prefix("del:")
        .is_some_and(|value| value.parse::<u64>().is_ok());
    if !valid_reference || !valid_delivered {
        return contents.to_vec();
    }

    let mut normalized = b"ref:0 del:0".to_vec();
    if has_newline {
        normalized.push(b'\n');
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-963): Review virtual RTC attribute values.
fn sanitize_sysfs_rtc_attribute(
    contents: &[u8],
    kind: ProcfsKind,
    virtual_realtime_seconds: i64,
) -> Vec<u8> {
    let has_newline = contents.ends_with(b"\n");
    let value = contents.strip_suffix(b"\n").unwrap_or(contents);
    let valid = match kind {
        ProcfsKind::SysfsRtcDate => matches_digit_separated(value, 10, &[(4, b'-'), (7, b'-')]),
        ProcfsKind::SysfsRtcTime => matches_digit_separated(value, 8, &[(2, b':'), (5, b':')]),
        ProcfsKind::SysfsRtcEpoch => !value.is_empty() && value.iter().all(u8::is_ascii_digit),
        _ => return contents.to_vec(),
    };
    if !valid {
        return contents.to_vec();
    }
    let Some(now) = DateTime::<Utc>::from_timestamp(virtual_realtime_seconds, 0) else {
        return contents.to_vec();
    };
    let fixed = match kind {
        ProcfsKind::SysfsRtcDate => now.format("%Y-%m-%d").to_string(),
        ProcfsKind::SysfsRtcTime => now.format("%H:%M:%S").to_string(),
        ProcfsKind::SysfsRtcEpoch => virtual_realtime_seconds.to_string(),
        _ => unreachable!("validated sysfs RTC kind changed"),
    };

    let mut normalized = fixed.into_bytes();
    if has_newline {
        normalized.push(b'\n');
    }
    normalized
}

fn matches_digit_separated(value: &[u8], expected_len: usize, separators: &[(usize, u8)]) -> bool {
    value.len() == expected_len
        && value.iter().enumerate().all(|(index, byte)| {
            separators
                .iter()
                .find_map(|(position, separator)| (*position == index).then_some(*separator))
                .map_or_else(|| byte.is_ascii_digit(), |separator| *byte == separator)
        })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-957): Review Btrfs bytes_may_use normalization.
fn sanitize_btrfs_bytes_may_use(contents: &[u8]) -> Vec<u8> {
    let has_newline = contents.ends_with(b"\n");
    let value = contents.strip_suffix(b"\n").unwrap_or(contents);
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return contents.to_vec();
    }

    if has_newline {
        b"0\n".to_vec()
    } else {
        b"0".to_vec()
    }
}

fn sanitize_loadavg(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0.00 0.00 0.00 1/1 1\n".to_vec()
    }
}

fn sanitize_uptime(contents: &[u8], virtual_uptime_seconds: u64) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        format!("{virtual_uptime_seconds}.00 0.00\n").into_bytes()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-843): Review deterministic procfs system accounting.
fn sanitize_system_stat(
    contents: &[u8],
    virtual_uptime_seconds: u64,
    virtual_boot_time_seconds: i64,
) -> Vec<u8> {
    const VOLATILE_FIELDS: &[&[u8]] = &[
        b"intr",
        b"ctxt",
        b"processes",
        b"procs_running",
        b"procs_blocked",
        b"softirq",
    ];

    let cpu_count = contents
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.split(|byte| byte.is_ascii_whitespace()).next())
        .filter(|name| name.starts_with(b"cpu") && *name != b"cpu")
        .count() as u64;
    let per_cpu_idle_ticks = virtual_uptime_seconds.saturating_mul(100);
    let counters = sanitize_named_counters(
        contents,
        |name| name.starts_with(b"cpu") || VOLATILE_FIELDS.contains(&name),
        |name, index| {
            if index == 0 && name == b"cpu" {
                per_cpu_idle_ticks.saturating_mul(cpu_count)
            } else if index == 0 && name.starts_with(b"cpu") {
                per_cpu_idle_ticks
            } else {
                0
            }
        },
    );

    let mut normalized = Vec::with_capacity(counters.len());
    for line in counters.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(b"btime ") {
            normalized.extend_from_slice(format!("btime {virtual_boot_time_seconds}").as_bytes());
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_named_counters(
    contents: &[u8],
    should_normalize: impl Fn(&[u8]) -> bool,
    counter_value: impl Fn(&[u8], usize) -> u64,
) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let mut fields = body
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let name = fields.next().unwrap_or_default();
        if should_normalize(name) {
            normalized.extend_from_slice(name);
            for (index, _) in fields.enumerate() {
                normalized.push(b' ');
                normalized.extend_from_slice(counter_value(name, index).to_string().as_bytes());
            }
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-863): Review canonical /proc/meminfo guest accounting.
/// Replaces host-global memory pressure with Hermit's configured guest memory.
fn sanitize_meminfo(contents: &[u8], virtual_memory_kb: u64) -> Vec<u8> {
    if contents.is_empty() {
        return Vec::new();
    }
    format!(
        "MemTotal:       {virtual_memory_kb} kB\n\
         MemFree:        {virtual_memory_kb} kB\n\
         MemAvailable:   {virtual_memory_kb} kB\n\
         Buffers:        0 kB\n\
         Cached:         0 kB\n\
         SwapCached:     0 kB\n\
         Active:         0 kB\n\
         Inactive:       0 kB\n\
         Shmem:          0 kB\n\
         SReclaimable:   0 kB\n\
         SwapTotal:      0 kB\n\
         SwapFree:       0 kB\n"
    )
    .into_bytes()
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-951): Review the /proc/key-users quota accounting policy.
fn sanitize_key_users(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let fields = body.split_whitespace().collect::<Vec<_>>();
        let [uid, usage, key_counts, key_quota, byte_quota] = fields.as_slice() else {
            return contents.to_vec();
        };
        let Some(uid) = uid
            .strip_suffix(':')
            .filter(|uid| uid.parse::<u32>().is_ok())
        else {
            return contents.to_vec();
        };
        if usage.parse::<u64>().is_err() || parse_key_user_pair(key_counts).is_none() {
            return contents.to_vec();
        }
        let Some((_, max_keys)) = parse_key_user_pair(key_quota) else {
            return contents.to_vec();
        };
        let Some((_, max_bytes)) = parse_key_user_pair(byte_quota) else {
            return contents.to_vec();
        };

        normalized.extend_from_slice(format!("{uid}: 0 0/0 0/{max_keys} 0/{max_bytes}").as_bytes());
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn parse_key_user_pair(field: &str) -> Option<(u64, u64)> {
    let (current, maximum) = field.split_once('/')?;
    Some((current.parse().ok()?, maximum.parse().ok()?))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-905): Review the /proc/buddyinfo field policy.
fn sanitize_buddyinfo(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let fields = body.split_whitespace().collect::<Vec<_>>();
        let is_buddy_row = fields.len() >= 5
            && fields[0] == "Node"
            && fields[1].ends_with(',')
            && fields[2] == "zone"
            && fields[4..].iter().all(|field| field.parse::<u64>().is_ok());

        if is_buddy_row {
            normalized.extend_from_slice(fields[..4].join(" ").as_bytes());
            for _ in &fields[4..] {
                normalized.extend_from_slice(b" 0");
            }
        } else {
            normalized.extend_from_slice(body.as_bytes());
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-910): Review the /proc/sys/fs/file-nr policy.
const VIRTUAL_FILE_MAX: u64 = i64::MAX as u64;

fn sanitize_file_nr(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        format!("0\t0\t{VIRTUAL_FILE_MAX}\n").into_bytes()
    }
}

fn sanitize_file_max(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        format!("{VIRTUAL_FILE_MAX}\n").into_bytes()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-914): Review the /proc/sys/fs/inode-nr field policy.
fn sanitize_inode_nr(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\t0\n".to_vec()
    }
}

fn sanitize_inode_state(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\t0\t0\t0\t0\t0\t0\n".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-918): Review the /proc/sys/fs/dentry-state field policy.
fn sanitize_dentry_state(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\t0\t45\t0\t0\t0\n".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-945): Review the zeroed /proc/swaps Used column policy.
fn sanitize_swaps(contents: &[u8]) -> Vec<u8> {
    const HEADER: [&str; 5] = ["Filename", "Type", "Size", "Used", "Priority"];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let mut lines = text.split_inclusive('\n');
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    if !header.split_whitespace().eq(HEADER) {
        return contents.to_vec();
    }

    let mut normalized = Vec::with_capacity(contents.len());
    normalized.extend_from_slice(header.as_bytes());
    for line in lines {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let fields = body.split_whitespace().collect::<Vec<_>>();
        let [filename, swap_type, size, used, priority] = fields.as_slice() else {
            return contents.to_vec();
        };
        if size.parse::<u64>().is_err()
            || used.parse::<u64>().is_err()
            || priority.parse::<i32>().is_err()
        {
            return contents.to_vec();
        }

        normalized.extend_from_slice(
            format!("{filename}\t{swap_type}\t{size}\t0\t{priority}").as_bytes(),
        );
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-933): Review the /proc/sys/fs/aio-nr policy.
fn sanitize_aio_nr(contents: &[u8]) -> Vec<u8> {
    let Ok(value) = std::str::from_utf8(contents) else {
        return Vec::new();
    };
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map_or_else(Vec::new, |_| b"0\n".to_vec())
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-927): Review the /proc/sys/kernel/pty/nr policy.
fn sanitize_pty_nr(contents: &[u8], virtual_count: usize) -> Vec<u8> {
    let Ok(value) = std::str::from_utf8(contents) else {
        return Vec::new();
    };
    if value.trim().parse::<u64>().is_err() {
        return Vec::new();
    }
    format!("{virtual_count}\n").into_bytes()
}

fn is_node_vmstat_path(path: &str) -> bool {
    let relative = path
        .strip_prefix("/sys/devices/system/node/")
        .unwrap_or(path);
    let Some(node) = relative.strip_suffix("/vmstat") else {
        return false;
    };
    let Some(index) = node.strip_prefix("node") else {
        return false;
    };
    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-939): Review the zeroed node VM accounting policy.
fn sanitize_node_vmstat(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = body.split_whitespace();
        let (Some(name), Some(value), None) = (fields.next(), fields.next(), fields.next()) else {
            return contents.to_vec();
        };
        if value.parse::<u64>().is_err() {
            return contents.to_vec();
        }

        normalized.extend_from_slice(name.as_bytes());
        normalized.extend_from_slice(b" 0");
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-958): Review the synthetic /sys/kernel/uevent_seqnum value.
fn sanitize_uevent_seqnum(contents: &[u8]) -> Vec<u8> {
    let Some(value) = contents.strip_suffix(b"\n") else {
        return contents.to_vec();
    };
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        contents.to_vec()
    } else {
        b"0\n".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-866): Review the /proc/net/sockstat field policy.
fn sanitize_sockstat(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = body
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        match fields.first().map(String::as_str) {
            Some("TCP:") => {
                let inuse = sockstat_field(&fields, "inuse").unwrap_or_else(|| "0".to_owned());
                replace_sockstat_field(&mut fields, "orphan", "0");
                replace_sockstat_field(&mut fields, "alloc", &inuse);
                replace_sockstat_field(&mut fields, "mem", "0");
            }
            Some("UDP:") => replace_sockstat_field(&mut fields, "mem", "0"),
            _ => {}
        }

        normalized.extend_from_slice(fields.join(" ").as_bytes());
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-967): Review Unix socket identity normalization.
fn sanitize_unix_sockets(contents: &[u8]) -> Vec<u8> {
    const HEADER: [&str; 8] = [
        "Num", "RefCount", "Protocol", "Flags", "Type", "St", "Inode", "Path",
    ];
    const ZERO_NUM: &str = "0000000000000000:";

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let Some(body) = text.strip_suffix('\n') else {
        return contents.to_vec();
    };
    let mut lines = body.split('\n');
    let Some(header) = lines.next() else {
        return contents.to_vec();
    };
    if !header.split_whitespace().eq(HEADER) {
        return contents.to_vec();
    }

    let mut rows = Vec::new();
    for line in lines {
        let Some((mut fields, path)) = unix_socket_fields(line) else {
            return contents.to_vec();
        };
        let Some(num) = fields[0].strip_suffix(':') else {
            return contents.to_vec();
        };
        if !is_fixed_lower_hex(num, 16)
            || !is_fixed_lower_hex(fields[1], 8)
            || !is_fixed_lower_hex(fields[2], 8)
            || !is_fixed_lower_hex(fields[3], 8)
            || !is_fixed_lower_hex(fields[4], 4)
            || !is_fixed_lower_hex(fields[5], 2)
            || !is_decimal(fields[6])
        {
            return contents.to_vec();
        }

        fields[0] = ZERO_NUM;
        fields[6] = "0";
        let mut row = fields.join(" ");
        if !path.is_empty() {
            row.push(' ');
            row.push_str(path);
        }
        rows.push(row);
    }
    rows.sort_unstable();

    let mut normalized = String::with_capacity(text.len());
    normalized.push_str(header);
    normalized.push('\n');
    for row in rows {
        normalized.push_str(&row);
        normalized.push('\n');
    }
    normalized.into_bytes()
}

fn unix_socket_fields(line: &str) -> Option<(Vec<&str>, &str)> {
    const FIELD_COUNT: usize = 7;

    let mut fields = Vec::with_capacity(FIELD_COUNT);
    let mut remainder = line;
    while fields.len() < FIELD_COUNT {
        remainder = remainder.trim_start_matches(|character: char| character.is_ascii_whitespace());
        if remainder.is_empty() {
            return None;
        }
        let end = remainder
            .find(|character: char| character.is_ascii_whitespace())
            .unwrap_or(remainder.len());
        fields.push(&remainder[..end]);
        remainder = &remainder[end..];
    }

    Some((
        fields,
        remainder.trim_start_matches(|character: char| character.is_ascii_whitespace()),
    ))
}

fn is_fixed_lower_hex(field: &str, width: usize) -> bool {
    field.len() == width
        && field
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_decimal(field: &str) -> bool {
    !field.is_empty()
        && field.bytes().all(|byte| byte.is_ascii_digit())
        && field.parse::<u64>().is_ok()
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-961): Review proc netlink identity normalization.
fn sanitize_netlink_sockets(contents: &[u8]) -> Vec<u8> {
    const HEADER: [&str; 10] = [
        "sk", "Eth", "Pid", "Groups", "Rmem", "Wmem", "Dump", "Locks", "Drops", "Inode",
    ];
    const ZERO_POINTER: &str = "0000000000000000";

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let Some(body) = text.strip_suffix('\n') else {
        return contents.to_vec();
    };
    let mut lines = body.split('\n');
    let Some(header) = lines.next() else {
        return contents.to_vec();
    };
    if !header.split_whitespace().eq(HEADER) {
        return contents.to_vec();
    }

    let mut rows = Vec::new();
    for line in lines {
        let mut fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != HEADER.len()
            || !is_lower_hex(fields[0], 16)
            || !is_lower_hex(fields[3], 8)
        {
            return contents.to_vec();
        }

        let Some(key) = [
            parse_decimal(fields[1]),
            parse_decimal(fields[2]),
            parse_lower_hex(fields[3]),
            parse_decimal(fields[4]),
            parse_decimal(fields[5]),
            parse_decimal(fields[6]),
            parse_decimal(fields[7]),
            parse_decimal(fields[8]),
        ]
        .into_iter()
        .collect::<Option<Vec<_>>>() else {
            return contents.to_vec();
        };
        if parse_decimal(fields[9]).is_none() {
            return contents.to_vec();
        }

        fields[0] = ZERO_POINTER;
        fields[9] = "0";
        rows.push((key, fields.join(" ")));
    }
    rows.sort_unstable();

    let mut normalized = String::with_capacity(text.len());
    normalized.push_str(header);
    normalized.push('\n');
    for (_, row) in rows {
        normalized.push_str(&row);
        normalized.push('\n');
    }
    normalized.into_bytes()
}

fn is_lower_hex(field: &str, width: usize) -> bool {
    field.len() == width
        && field
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_lower_hex(field: &str) -> Option<u64> {
    u64::from_str_radix(field, 16).ok()
}

fn parse_decimal(field: &str) -> Option<u64> {
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    field.parse().ok()
}

fn is_irq_per_cpu_count_path(path: &Path) -> bool {
    if path.file_name().and_then(|leaf| leaf.to_str()) != Some("per_cpu_count") {
        return false;
    }
    let Some(irq_directory) = path.parent() else {
        return false;
    };
    let Some(irq) = irq_directory.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !irq.is_empty()
        && irq.bytes().all(|byte| byte.is_ascii_digit())
        && irq_directory.parent() == Some(Path::new("/sys/kernel/irq"))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-960): Review the /sys/kernel/irq/<IRQ>/per_cpu_count field policy.
fn sanitize_irq_per_cpu_count(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let has_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.contains('\n') {
        return contents.to_vec();
    }

    let fields = body.split(',').collect::<Vec<_>>();
    if fields.is_empty()
        || fields
            .iter()
            .any(|field| field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return contents.to_vec();
    }

    let mut normalized = vec!["0"; fields.len()].join(",").into_bytes();
    if has_newline {
        normalized.push(b'\n');
    }
    normalized
}

fn is_block_inflight_path(path: &Path) -> bool {
    path.file_name().and_then(|leaf| leaf.to_str()) == Some("inflight")
        && path.parent().and_then(Path::parent) == Some(Path::new("/sys/block"))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-956): Review the /sys/block/<device>/inflight field policy.
fn sanitize_block_inflight(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let has_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.contains('\n') {
        return contents.to_vec();
    }

    let mut fields = body.split_whitespace();
    let valid = matches!(
        (fields.next(), fields.next(), fields.next()),
        (Some(reads), Some(writes), None)
            if reads.parse::<u64>().is_ok() && writes.parse::<u64>().is_ok()
    );
    if !valid {
        return contents.to_vec();
    }

    if has_newline {
        b"0 0\n".to_vec()
    } else {
        b"0 0".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-938): Review the /proc/vmstat field policy.
fn sanitize_vmstat(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let mut normalized = Vec::with_capacity(contents.len());
    let mut row_count = 0;

    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let fields = body.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[0].is_empty() || fields[1].parse::<u64>().is_err() {
            return contents.to_vec();
        }

        normalized.extend_from_slice(fields[0].as_bytes());
        normalized.extend_from_slice(b" 0");
        if has_newline {
            normalized.push(b'\n');
        }
        row_count += 1;
    }

    if row_count == 0 {
        contents.to_vec()
    } else {
        normalized
    }
}

fn sockstat_field(fields: &[String], name: &str) -> Option<String> {
    let index = fields.iter().position(|field| field == name)?;
    fields.get(index + 1).cloned()
}

fn replace_sockstat_field(fields: &mut [String], name: &str, value: &str) {
    let Some(index) = fields.iter().position(|field| field == name) else {
        return;
    };
    let Some(field_value) = fields.get_mut(index + 1) else {
        return;
    };
    *field_value = value.to_owned();
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-928): Review the /proc/self/sched field policy.
fn sanitize_self_sched(contents: &[u8]) -> Vec<u8> {
    const FLOAT_FIELDS: &[&str] = &["se.exec_start", "se.vruntime", "se.sum_exec_runtime"];
    const INTEGER_FIELDS: &[&str] = &[
        "se.nr_migrations",
        "nr_switches",
        "nr_voluntary_switches",
        "nr_involuntary_switches",
        "se.avg.load_sum",
        "se.avg.runnable_sum",
        "se.avg.util_sum",
        "se.avg.load_avg",
        "se.avg.runnable_avg",
        "se.avg.util_avg",
        "se.avg.last_update_time",
        "se.avg.util_est",
        "clock-delta",
        "mm->numa_scan_seq",
        "numa_pages_migrated",
        "total_numa_faults",
    ];
    const STABLE_INTEGER_FIELDS: &[&str] = &[
        "se.load.weight",
        "policy",
        "prio",
        "se.slice",
        "ext.enabled",
        "numa_preferred_nid",
        "uclamp.min",
        "uclamp.max",
        "effective uclamp.min",
        "effective uclamp.max",
    ];
    const UCLAMP_FIELDS: &[(&str, &str)] = &[
        ("uclamp.min", "0"),
        ("uclamp.max", "1024"),
        ("effective uclamp.min", "0"),
        ("effective uclamp.max", "1024"),
    ];

    let Ok(text) = std::str::from_utf8(contents) else {
        return Vec::new();
    };
    let mut normalized = Vec::with_capacity(contents.len());
    let mut core_fields_seen = [false; 3];
    let mut header_seen = false;

    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        if line_index == 0 {
            let Some((name, details)) = body.rsplit_once(" (") else {
                return Vec::new();
            };
            let Some(details) = details.strip_suffix(')') else {
                return Vec::new();
            };
            let Some((pid, threads)) = details.split_once(", #threads: ") else {
                return Vec::new();
            };
            if pid.parse::<i32>().is_err() || threads.parse::<u64>().is_err() {
                return Vec::new();
            }
            normalized.extend_from_slice(format!("{name} (0, #threads: 1)").as_bytes());
            if has_newline {
                normalized.push(b'\n');
            }
            header_seen = true;
            continue;
        }
        let Some((left, right)) = body.split_once(':') else {
            if !body.is_empty() && body.bytes().all(|byte| byte == b'-') {
                normalized.extend_from_slice(body.as_bytes());
            } else if body.starts_with("current_node=") {
                let fields = body
                    .replace(',', "")
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if fields.len() != 2
                    || !fields.iter().all(|field| {
                        field.split_once('=').is_some_and(|(name, value)| {
                            matches!(name, "current_node" | "numa_group_id")
                                && value.parse::<i64>().is_ok()
                        })
                    })
                {
                    return Vec::new();
                }
                normalized.extend_from_slice(b"current_node=0, numa_group_id=0");
            } else if body.starts_with("numa_faults ") {
                let fields = body.split_whitespace().skip(1).collect::<Vec<_>>();
                let expected = [
                    "node",
                    "task_private",
                    "task_shared",
                    "group_private",
                    "group_shared",
                ];
                if fields.len() != expected.len()
                    || !fields.iter().zip(expected).all(|(field, expected)| {
                        field.split_once('=').is_some_and(|(name, value)| {
                            name == expected && value.parse::<u64>().is_ok()
                        })
                    })
                {
                    return Vec::new();
                }
                normalized.extend_from_slice(
                    b"numa_faults node=0 task_private=0 task_shared=0 group_private=0 group_shared=0",
                );
            } else {
                return Vec::new();
            }
            if has_newline {
                normalized.push(b'\n');
            }
            continue;
        };
        let label = left.trim();
        let replacement = if let Some(index) = FLOAT_FIELDS.iter().position(|field| *field == label)
        {
            let Ok(value) = right.trim().parse::<f64>() else {
                return Vec::new();
            };
            if !value.is_finite() || value.is_sign_negative() {
                return Vec::new();
            }
            core_fields_seen[index] = true;
            Some("0.000000")
        } else if let Some((_, replacement)) =
            UCLAMP_FIELDS.iter().find(|(field, _)| *field == label)
        {
            if right.trim().parse::<u128>().is_err() {
                return Vec::new();
            }
            Some(*replacement)
        } else if INTEGER_FIELDS.contains(&label) {
            if right.trim().parse::<u128>().is_err() {
                return Vec::new();
            }
            Some("0")
        } else if STABLE_INTEGER_FIELDS.contains(&label) {
            if right.trim().parse::<i128>().is_err() {
                return Vec::new();
            }
            None
        } else if right.trim().parse::<u128>().is_ok() {
            // New kernels may append observational scheduler counters. Keep
            // forward-compatible numeric telemetry deterministic without
            // accepting malformed or negative unknown fields.
            Some("0")
        } else {
            return Vec::new();
        };

        if let Some(value) = replacement {
            normalized.extend_from_slice(left.as_bytes());
            normalized.extend_from_slice(b": ");
            normalized.extend_from_slice(value.as_bytes());
        } else {
            normalized.extend_from_slice(body.as_bytes());
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }

    if header_seen && core_fields_seen.iter().all(|seen| *seen) {
        normalized
    } else {
        Vec::new()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-931): Review the /proc/self/fdinfo field policy.
fn sanitize_fdinfo(contents: &[u8], identity: Option<(u64, i32, u64)>) -> Vec<u8> {
    let Some((virtual_inode, logical_flags, virtual_open_file)) = identity else {
        return Vec::new();
    };
    let Ok(text) = std::str::from_utf8(contents) else {
        return Vec::new();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.starts_with("mnt_id:") {
            normalized.extend_from_slice(b"mnt_id:\t1");
        } else if body.starts_with("ino:") {
            normalized.extend_from_slice(format!("ino:\t{virtual_inode}").as_bytes());
        } else if body.starts_with("flags:") {
            normalized.extend_from_slice(format!("flags:\t{logical_flags:07o}").as_bytes());
        } else if body.starts_with("eventfd-id:") {
            normalized.extend_from_slice(format!("eventfd-id: {virtual_open_file}").as_bytes());
        } else if body.starts_with("Pid:") {
            normalized.extend_from_slice(b"Pid:\t1");
        } else if body.starts_with("NSpid:") {
            normalized.extend_from_slice(b"NSpid:\t1");
        } else if body.starts_with("tfd:")
            || body.starts_with("inotify ")
            || body.starts_with("lock:")
        {
            return Vec::new();
        } else {
            let allowed = body.split_once(':').is_some_and(|(label, _)| {
                matches!(
                    label,
                    "pos"
                        | "eventfd-count"
                        | "eventfd-semaphore"
                        | "sigmask"
                        | "clockid"
                        | "ticks"
                        | "settime flags"
                        | "it_value"
                        | "it_interval"
                        | "seals"
                )
            });
            if !allowed {
                return Vec::new();
            }
            normalized.extend_from_slice(body.as_bytes());
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-934): Review the /proc/self/numa_maps field policy.
fn sanitize_numa_maps(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let mut normalized = Vec::with_capacity(contents.len());
    let mut row_count = 0;

    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let fields = body.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 2 || u64::from_str_radix(fields[0], 16).is_err() {
            return contents.to_vec();
        }

        let mut kept = Vec::with_capacity(fields.len());
        for field in fields {
            if let Some(value) = field
                .strip_prefix("active=")
                .or_else(|| field.strip_prefix("mapmax="))
            {
                if value.parse::<u64>().is_err() {
                    return contents.to_vec();
                }
            } else {
                kept.push(field);
            }
        }
        normalized.extend_from_slice(kept.join(" ").as_bytes());
        if has_newline {
            normalized.push(b'\n');
        }
        row_count += 1;
    }

    if row_count == 0 {
        contents.to_vec()
    } else {
        normalized
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-937): Review the /proc/self/smaps_rollup field policy.
fn sanitize_smaps_rollup(contents: &[u8]) -> Vec<u8> {
    const HOST_ACCOUNTING_FIELDS: &[&str] = &[
        "Rss",
        "Pss",
        "Pss_Dirty",
        "Pss_Anon",
        "Pss_File",
        "Pss_Shmem",
        "Shared_Clean",
        "Shared_Dirty",
        "Private_Clean",
        "Referenced",
        "KSM",
        "SwapPss",
    ];
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let accounting_label = body.split_once(':').and_then(|(label, value)| {
            let mut fields = value.split_whitespace();
            let amount = fields.next()?;
            (amount.parse::<u64>().is_ok()
                && fields.next() == Some("kB")
                && fields.next().is_none())
            .then_some(label)
        });
        if let Some(label) = accounting_label.filter(|label| HOST_ACCOUNTING_FIELDS.contains(label))
        {
            normalized.extend_from_slice(label.as_bytes());
            normalized.extend_from_slice(b":\t0 kB");
        } else {
            normalized.extend_from_slice(body.as_bytes());
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-944): Review the /proc/self/arch_status field policy.
fn sanitize_arch_status(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let elapsed = body
            .strip_prefix("AVX512_elapsed_ms:")
            .map(str::trim)
            .and_then(|value| value.parse::<u64>().ok());
        if elapsed.is_some() {
            normalized.extend_from_slice(b"AVX512_elapsed_ms:\t0");
        } else {
            normalized.extend_from_slice(body.as_bytes());
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-949): Review the /proc/self/smaps accounting field policy.
fn sanitize_smaps(contents: &[u8]) -> Vec<u8> {
    const ACCOUNTING_FIELDS: &[&str] = &[
        "Rss",
        "Pss",
        "Pss_Dirty",
        "Pss_Anon",
        "Pss_File",
        "Pss_Shmem",
        "Shared_Clean",
        "Shared_Dirty",
        "Private_Clean",
        "Referenced",
        "KSM",
        "SwapPss",
    ];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let mut normalized = Vec::with_capacity(contents.len());
    let mut mapping_count = 0;
    let mut accounting_count = 0;

    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);

        if is_smaps_mapping_header(body) {
            mapping_count += 1;
            normalized.extend_from_slice(body.as_bytes());
        } else {
            if mapping_count == 0 {
                return contents.to_vec();
            }
            let Some((label, value)) = body.split_once(':') else {
                return contents.to_vec();
            };

            if ACCOUNTING_FIELDS.contains(&label) {
                if !is_smaps_kilobyte_value(value) {
                    return contents.to_vec();
                }
                normalized.extend_from_slice(label.as_bytes());
                normalized.extend_from_slice(b":\t0 kB");
                accounting_count += 1;
            } else {
                let valid_static_field = match label {
                    "Size" | "KernelPageSize" | "MMUPageSize" | "Private_Dirty" | "Anonymous"
                    | "LazyFree" | "AnonHugePages" | "ShmemPmdMapped" | "FilePmdMapped"
                    | "Shared_Hugetlb" | "Private_Hugetlb" | "Swap" | "Locked" => {
                        is_smaps_kilobyte_value(value)
                    }
                    "THPeligible" | "ProtectionKey" => is_smaps_integer_value(value),
                    "VmFlags" => value
                        .split_whitespace()
                        .all(|flag| flag.bytes().all(|byte| byte.is_ascii_alphanumeric())),
                    _ => false,
                };
                if !valid_static_field {
                    return contents.to_vec();
                }
                normalized.extend_from_slice(body.as_bytes());
            }
        }

        if has_newline {
            normalized.push(b'\n');
        }
    }

    if mapping_count == 0 || accounting_count == 0 {
        contents.to_vec()
    } else {
        normalized
    }
}

fn is_smaps_mapping_header(line: &str) -> bool {
    let mut fields = line.split_whitespace();
    let Some((start, end)) = fields.next().and_then(|range| range.split_once('-')) else {
        return false;
    };
    let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
    else {
        return false;
    };
    let Some(permissions) = fields.next() else {
        return false;
    };
    let permissions = permissions.as_bytes();
    let valid_permissions = permissions.len() == 4
        && matches!(permissions[0], b'r' | b'-')
        && matches!(permissions[1], b'w' | b'-')
        && matches!(permissions[2], b'x' | b'-')
        && matches!(permissions[3], b'p' | b's');
    let valid_offset = fields
        .next()
        .is_some_and(|offset| u64::from_str_radix(offset, 16).is_ok());
    let valid_device = fields.next().is_some_and(|device| {
        device.split_once(':').is_some_and(|(major, minor)| {
            u64::from_str_radix(major, 16).is_ok() && u64::from_str_radix(minor, 16).is_ok()
        })
    });
    let valid_inode = fields
        .next()
        .is_some_and(|inode| inode.parse::<u64>().is_ok());

    start < end && valid_permissions && valid_offset && valid_device && valid_inode
}

fn is_smaps_kilobyte_value(value: &str) -> bool {
    let mut fields = value.split_whitespace();
    matches!(
        (fields.next(), fields.next(), fields.next()),
        (Some(number), Some("kB"), None) if number.parse::<u64>().is_ok()
    )
}

fn is_smaps_integer_value(value: &str) -> bool {
    let mut fields = value.split_whitespace();
    matches!(
        (fields.next(), fields.next()),
        (Some(number), None) if number.parse::<u64>().is_ok()
    )
}

fn sanitize_pressure(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let fields = body
            .split_whitespace()
            .map(|field| {
                let Some((name, _)) = field.split_once('=') else {
                    return field.to_owned();
                };
                match name {
                    "avg10" | "avg60" | "avg300" => format!("{name}=0.00"),
                    "total" => "total=0".to_owned(),
                    _ => field.to_owned(),
                }
            })
            .collect::<Vec<_>>();

        normalized.extend_from_slice(fields.join(" ").as_bytes());
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_zoneinfo(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        if trimmed.starts_with("Node ") || trimmed.starts_with("cpu: ") {
            normalized.extend_from_slice(body.as_bytes());
        } else {
            normalized.extend_from_slice(zero_decimal_runs(body).as_bytes());
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-883): Review synthetic interrupt and module accounting values.
fn sanitize_interrupt_counters(contents: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let Some(colon) = body.iter().position(|byte| *byte == b':') else {
            normalized.extend_from_slice(line);
            continue;
        };
        let fields = body[colon + 1..]
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let counter_count = fields
            .iter()
            .take_while(|field| field.iter().all(u8::is_ascii_digit))
            .count();
        if counter_count == 0 {
            normalized.extend_from_slice(line);
            continue;
        }

        normalized.extend_from_slice(&body[..=colon]);
        for (index, field) in fields.iter().enumerate() {
            normalized.push(b' ');
            normalized.extend_from_slice(if index < counter_count { b"0" } else { field });
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-883): Review synthetic module reference counts.
fn sanitize_modules(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = body
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if fields.len() >= 4 {
            let holders = if fields[3] == "-" {
                0
            } else {
                fields[3]
                    .split(',')
                    .filter(|holder| !holder.is_empty())
                    .count()
            };
            fields[2] = holders.to_string();
        }
        normalized.extend_from_slice(fields.join(" ").as_bytes());
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_schedstat(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = body
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let preserved_fields = match fields.first().map(String::as_str) {
            Some("timestamp") => 1,
            Some(label) if numbered_label(label, "cpu") => 1,
            Some(label) if numbered_label(label, "domain") => 3,
            _ => fields.len(),
        };
        for field in fields.iter_mut().skip(preserved_fields) {
            if field.parse::<u128>().is_ok() {
                *field = "0".to_owned();
            }
        }

        normalized.extend_from_slice(fields.join(" ").as_bytes());
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-873): Review private mount-root normalization.
fn sanitize_mountinfo(contents: &[u8]) -> Vec<u8> {
    const TEMP_ROOT_PREFIXES: &[&[u8]] = &[b"/tmpvol/.tmp", b"/tmp/.tmp"];

    fn is_private_temp_root(root: &[u8]) -> bool {
        let Some(suffix) = TEMP_ROOT_PREFIXES
            .iter()
            .find_map(|prefix| root.strip_prefix(*prefix))
        else {
            return false;
        };
        suffix.len() == 6 && suffix.iter().all(u8::is_ascii_alphanumeric)
    }

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let fields = body.split(|byte| *byte == b' ').collect::<Vec<_>>();

        if fields.len() >= 5 && is_private_temp_root(fields[3]) {
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    normalized.push(b' ');
                }
                if index == 3 {
                    normalized.extend_from_slice(b"/tmpvol/.hermit");
                    normalized.extend_from_slice(fields[4]);
                } else {
                    normalized.extend_from_slice(field);
                }
            }
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-955): Review deterministic kernel UUID generation.
fn sanitize_random_uuid(contents: &[u8], mut random: [u8; 16]) -> Vec<u8> {
    const HYPHENS: &[usize] = &[8, 13, 18, 23];

    if contents.len() != 37 || contents[36] != b'\n' {
        return contents.to_vec();
    }
    for (index, byte) in contents[..36].iter().copied().enumerate() {
        if HYPHENS.contains(&index) {
            if byte != b'-' {
                return contents.to_vec();
            }
        } else if !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte) {
            return contents.to_vec();
        }
    }
    if contents[14] != b'4' || !matches!(contents[19], b'8' | b'9' | b'a' | b'b') {
        return contents.to_vec();
    }

    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\n",
        random[0],
        random[1],
        random[2],
        random[3],
        random[4],
        random[5],
        random[6],
        random[7],
        random[8],
        random[9],
        random[10],
        random[11],
        random[12],
        random[13],
        random[14],
        random[15]
    )
    .into_bytes()
}

fn numbered_label(label: &str, prefix: &str) -> bool {
    label.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-922): Review the /proc/self/schedstat field policy.
fn sanitize_self_schedstat(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let fields = text.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 || fields.iter().any(|field| field.parse::<u64>().is_err()) {
        return contents.to_vec();
    }

    if text.ends_with('\n') {
        b"0 0 0\n".to_vec()
    } else {
        b"0 0 0".to_vec()
    }
}

// TODO-HUMAN-REVIEW(PR-909): Review the single-CPU softnet policy.
/// Exposes Hermit's single virtual CPU and hides host CPU count, hotplug state,
/// CPU identifiers, and live network backlog counters.
fn sanitize_softnet_stat(_contents: &[u8]) -> Vec<u8> {
    const VIRTUAL_SOFTNET_STAT: &[u8] =
        b"00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000\n";
    VIRTUAL_SOFTNET_STAT.to_vec()
}

fn zero_decimal_runs(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut in_digits = false;
    for character in text.chars() {
        if character.is_ascii_digit() {
            if !in_digits {
                normalized.push('0');
                in_digits = true;
            }
        } else {
            in_digits = false;
            normalized.push(character);
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-916): Review the /proc/net/protocols field policy.
fn sanitize_protocols(contents: &[u8]) -> Vec<u8> {
    const HEADER: &[&str] = &[
        "protocol", "size", "sockets", "memory", "press", "maxhdr", "slab", "module",
    ];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return contents.to_vec();
    };
    let header_fields = header.split_whitespace().collect::<Vec<_>>();
    if !header_fields.starts_with(HEADER) {
        return contents.to_vec();
    }

    let mut normalized = Vec::new();
    normalized.push(header.to_owned());
    for line in lines {
        let mut fields = line
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if fields.len() != header_fields.len()
            || fields[1].parse::<u64>().is_err()
            || fields[2].parse::<u64>().is_err()
            || (fields[3] != "-1" && fields[3].parse::<u64>().is_err())
        {
            return contents.to_vec();
        }

        fields[2] = "0".to_owned();
        if fields[3] != "-1" {
            fields[3] = "0".to_owned();
        }
        normalized.push(fields.join(" "));
    }
    if normalized.len() == 1 {
        return contents.to_vec();
    }

    let mut output = normalized.join("\n").into_bytes();
    if text.ends_with('\n') {
        output.push(b'\n');
    }
    output
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-917): Review virtual RTC clock and alarm policy.
fn sanitize_rtc(contents: &[u8], virtual_realtime_seconds: i64) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let Some(now) = DateTime::<Utc>::from_timestamp(virtual_realtime_seconds, 0) else {
        return contents.to_vec();
    };
    let rtc_time = now.format("%H:%M:%S").to_string();
    let rtc_date = now.format("%Y-%m-%d").to_string();

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let replacement = body.split_once(':').and_then(|(key, _)| {
            let value = match key.trim() {
                "rtc_time" => rtc_time.as_str(),
                "rtc_date" => rtc_date.as_str(),
                "alrm_time" => "00:00:00",
                "alrm_date" => rtc_date.as_str(),
                "alarm_IRQ"
                | "alrm_pending"
                | "update IRQ enabled"
                | "periodic IRQ enabled"
                | "periodic_IRQ"
                | "update_IRQ" => "no",
                "periodic IRQ frequency" | "max user IRQ frequency" | "periodic_freq" => "0",
                _ => return None,
            };
            Some(format!("{key}: {value}"))
        });
        if let Some(replacement) = replacement {
            normalized.extend_from_slice(replacement.as_bytes());
        } else {
            normalized.extend_from_slice(body.as_bytes());
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-926): Review the /proc/locks identity-virtualization policy.
//
// `/proc/locks` rows have the shape
//     `SEQ: [->] CLASS MODE RW PID MAJOR:MINOR:INODE START END`
// where the leading `SEQ:` is a kernel-global lock counter, `PID` is the owner
// task, and `MAJOR:MINOR:INODE` identifies the backing object. All three are
// host-specific and vary run-to-run, and the kernel emits rows in an internal
// order. A granted lock and the blocked waiters queued behind it share one
// `SEQ`, and a waiter row is marked with `->`.
//
// Rather than collapse every identity to a single constant, derive dense IDs
// from stable lock semantics. Equal raw values remain equal, distinct objects
// stay distinct, holder/waiter groups stay adjacent, and raw ID renumbering or
// row reordering does not change the resulting snapshot. An unknown format
// fails the complete snapshot closed instead of leaking any raw identities.
fn sanitize_locks(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return Vec::new();
    };
    if text.is_empty() {
        return Vec::new();
    }

    #[derive(Clone)]
    struct LockRow {
        sequence: String,
        waiter: bool,
        fields: Vec<String>,
        owner_index: usize,
        object_index: usize,
    }

    let mut rows: Vec<LockRow> = Vec::new();
    for line in text.lines() {
        let fields = line
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let waiter = fields.get(1).is_some_and(|field| field == "->");
        let details = usize::from(waiter) + 1;
        let owner_index = details + 3;
        let object_index = details + 4;
        let Some(sequence) = fields
            .first()
            .and_then(|field| field.strip_suffix(':'))
            .filter(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            return Vec::new();
        };
        if fields.len() != details + 7
            || fields[owner_index].parse::<i64>().is_err()
            || fields[object_index].split(':').count() != 3
        {
            return Vec::new();
        }
        rows.push(LockRow {
            sequence: sequence.to_owned(),
            waiter,
            fields,
            owner_index,
            object_index,
        });
    }

    let mut object_signatures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &rows {
        let mut signature = row.fields.clone();
        signature[0] = if row.waiter { "waiter" } else { "holder" }.to_owned();
        signature[row.owner_index] = "owner".to_owned();
        signature[row.object_index] = "object".to_owned();
        object_signatures
            .entry(row.fields[row.object_index].clone())
            .or_default()
            .push(signature.join(" "));
    }
    let mut objects = object_signatures.into_iter().collect::<Vec<_>>();
    for (_, signatures) in &mut objects {
        signatures.sort_unstable();
    }
    objects.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    let object_ids = objects
        .into_iter()
        .enumerate()
        .map(|(index, (object, _))| (object, index + 1))
        .collect::<BTreeMap<_, _>>();
    for row in &mut rows {
        let object_id = object_ids[&row.fields[row.object_index]];
        row.fields[row.object_index] = format!("00:00:{object_id}");
    }

    let mut owner_signatures: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for row in &rows {
        let owner = row.fields[row.owner_index]
            .parse::<i64>()
            .expect("owner was validated above");
        if owner == -1 {
            continue;
        }
        let mut signature = row.fields.clone();
        signature[0] = if row.waiter { "waiter" } else { "holder" }.to_owned();
        signature[row.owner_index] = "owner".to_owned();
        owner_signatures
            .entry(owner)
            .or_default()
            .push(signature.join(" "));
    }
    let mut owners = owner_signatures.into_iter().collect::<Vec<_>>();
    for (_, signatures) in &mut owners {
        signatures.sort_unstable();
    }
    owners.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    let owner_ids = owners
        .into_iter()
        .enumerate()
        .map(|(index, (owner, _))| (owner, index + 1))
        .collect::<BTreeMap<_, _>>();
    for row in &mut rows {
        let owner = row.fields[row.owner_index]
            .parse::<i64>()
            .expect("owner was validated above");
        if owner != -1 {
            row.fields[row.owner_index] = owner_ids[&owner].to_string();
        }
    }

    let mut groups: BTreeMap<String, Vec<LockRow>> = BTreeMap::new();
    for row in rows {
        groups.entry(row.sequence.clone()).or_default().push(row);
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    for rows in &mut groups {
        rows.sort_by(|left, right| {
            left.waiter
                .cmp(&right.waiter)
                .then_with(|| left.fields.cmp(&right.fields))
        });
    }
    groups.sort_by_key(|rows| {
        rows.iter()
            .map(|row| {
                let mut fields = row.fields.clone();
                fields[0] = "sequence:".to_owned();
                fields.join(" ")
            })
            .collect::<Vec<_>>()
    });

    let normalized_rows = groups
        .into_iter()
        .enumerate()
        .flat_map(|(sequence, rows)| {
            rows.into_iter().map(move |mut row| {
                row.fields[0] = format!("{}:", sequence + 1);
                row.fields.join(" ")
            })
        })
        .collect::<Vec<_>>();
    let mut normalized = normalized_rows.join("\n").into_bytes();
    if text.ends_with('\n') {
        normalized.push(b'\n');
    }
    normalized
}

fn is_btrfs_commit_stats_path(path: &Path) -> bool {
    if path.file_name().and_then(|leaf| leaf.to_str()) != Some("commit_stats") {
        return false;
    }
    let Some(filesystem_directory) = path.parent() else {
        return false;
    };
    let Some(uuid) = filesystem_directory
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    is_lowercase_uuid(uuid) && filesystem_directory.parent() == Some(Path::new("/sys/fs/btrfs"))
}

fn is_lowercase_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-966): Review the Btrfs commit_stats field policy.
fn sanitize_btrfs_commit_stats(contents: &[u8]) -> Vec<u8> {
    const LABELS: &[&str] = &[
        "commits",
        "cur_commit_ms",
        "last_commit_ms",
        "max_commit_ms",
        "total_commit_ms",
    ];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let has_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines = body.split('\n').collect::<Vec<_>>();
    if lines.len() != LABELS.len() {
        return contents.to_vec();
    }

    for (line, expected_label) in lines.iter().zip(LABELS) {
        let mut fields = line.split_whitespace();
        let (Some(label), Some(value), None) = (fields.next(), fields.next(), fields.next()) else {
            return contents.to_vec();
        };
        if label != *expected_label || value.parse::<u64>().is_err() {
            return contents.to_vec();
        }
    }

    let mut normalized = LABELS
        .iter()
        .map(|label| format!("{label} 0"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    if has_newline {
        normalized.push(b'\n');
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_normalized_procfs_paths() {
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/stat"))
                .unwrap()
                .kind,
            ProcfsKind::Stat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/status"))
                .unwrap()
                .kind,
            ProcfsKind::Status
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/thread-self/stat"))
                .unwrap()
                .kind,
            ProcfsKind::ThreadStat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/thread-self/status"))
                .unwrap()
                .kind,
            ProcfsKind::ThreadStatus
        );
        for (path, kind) in [
            ("/proc/123/stat", ProcfsKind::ProcessStat),
            ("/proc/self/statm", ProcfsKind::Statm),
            ("/proc/123/statm", ProcfsKind::Statm),
            ("/proc/123/status", ProcfsKind::ProcessStatus),
            ("/proc/stat", ProcfsKind::SystemStat),
        ] {
            assert_eq!(ProcfsFile::from_path(Path::new(path)).unwrap().kind, kind);
        }
        assert!(ProcfsFile::from_path(Path::new("/proc/not-a-pid/stat")).is_none());
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/cpuinfo"))
                .unwrap()
                .kind,
            ProcfsKind::Cpuinfo
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/diskstats"))
                .unwrap()
                .kind,
            ProcfsKind::Diskstats
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/loadavg"))
                .unwrap()
                .kind,
            ProcfsKind::Loadavg
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/uptime"))
                .unwrap()
                .kind,
            ProcfsKind::Uptime
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/meminfo"))
                .unwrap()
                .kind,
            ProcfsKind::Meminfo
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/net/sockstat"))
                .unwrap()
                .kind,
            ProcfsKind::Sockstat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/vmstat"))
                .unwrap()
                .kind,
            ProcfsKind::Vmstat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/block/md0/inflight"))
                .unwrap()
                .kind,
            ProcfsKind::BlockInflight
        );
        assert!(ProcfsFile::from_path(Path::new("/sys/block/md0/size")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/sys/class/block/md0/inflight")).is_none());
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/kernel/uevent_seqnum"))
                .unwrap()
                .kind,
            ProcfsKind::UeventSeqnum
        );
        assert!(ProcfsFile::from_path(Path::new("/sys/kernel/uevent_helper")).is_none());
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/kernel/irq/254/per_cpu_count"))
                .unwrap()
                .kind,
            ProcfsKind::IrqPerCpuCount
        );
        assert!(ProcfsFile::from_path(Path::new("/sys/kernel/irq/irq254/per_cpu_count")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/sys/kernel/irq/254/actions")).is_none());
        assert!(
            ProcfsFile::from_path(Path::new("/sys/kernel/irq/254/device/per_cpu_count")).is_none()
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/kernel/pty/nr"))
                .unwrap()
                .kind,
            ProcfsKind::PtyNr
        );
        for path in [
            "/proc/self/sched",
            "/proc/thread-self/sched",
            "/proc/123/sched",
            "/proc/self/task/456/sched",
        ] {
            assert_eq!(
                ProcfsFile::from_path(Path::new(path)).unwrap().kind,
                ProcfsKind::SelfSched
            );
        }
        for path in [
            "/proc/self/fdinfo/17",
            "/proc/thread-self/fdinfo/17",
            "/proc/123/fdinfo/17",
            "/proc/self/task/456/fdinfo/17",
        ] {
            let procfs = ProcfsFile::from_path(Path::new(path)).unwrap();
            assert_eq!(procfs.kind, ProcfsKind::Fdinfo);
            assert_eq!(procfs.target_fd(), Some(17));
        }
        assert!(ProcfsFile::from_path(Path::new("/proc/self/fdinfo/")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/proc/self/fdinfo/stdin")).is_none());
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/aio-nr"))
                .unwrap()
                .kind,
            ProcfsKind::AioNr
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/aio-max-nr"))
                .unwrap()
                .kind,
            ProcfsKind::AioMaxNr
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/numa_maps"))
                .unwrap()
                .kind,
            ProcfsKind::NumaMaps
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/smaps_rollup"))
                .unwrap()
                .kind,
            ProcfsKind::SmapsRollup
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/arch_status"))
                .unwrap()
                .kind,
            ProcfsKind::ArchStatus
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/swaps"))
                .unwrap()
                .kind,
            ProcfsKind::Swaps
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/key-users"))
                .unwrap()
                .kind,
            ProcfsKind::KeyUsers
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/123/io"))
                .unwrap()
                .kind,
            ProcfsKind::ProcessIo
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/block/nvme0n1/stat"))
                .unwrap()
                .kind,
            ProcfsKind::BlockStat
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/not-a-pid/io")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/sys/block/nvme0n1/size")).is_none());
        for path in [
            "/proc/pressure/cpu",
            "/proc/pressure/io",
            "/proc/pressure/memory",
        ] {
            assert_eq!(
                ProcfsFile::from_path(Path::new(path)).unwrap().kind,
                ProcfsKind::Pressure
            );
        }
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/buddyinfo"))
                .unwrap()
                .kind,
            ProcfsKind::Buddyinfo
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/schedstat"))
                .unwrap()
                .kind,
            ProcfsKind::Schedstat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/net/softnet_stat"))
                .unwrap()
                .kind,
            ProcfsKind::SoftnetStat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/file-nr"))
                .unwrap()
                .kind,
            ProcfsKind::FileNr
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/file-max"))
                .unwrap()
                .kind,
            ProcfsKind::FileMax
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/zoneinfo"))
                .unwrap()
                .kind,
            ProcfsKind::Zoneinfo
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/inode-nr"))
                .unwrap()
                .kind,
            ProcfsKind::InodeNr
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/inode-state"))
                .unwrap()
                .kind,
            ProcfsKind::InodeState
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/net/protocols"))
                .unwrap()
                .kind,
            ProcfsKind::Protocols
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/locks"))
                .unwrap()
                .kind,
            ProcfsKind::Locks
        );
        for alias in ["/proc/./locks", "/proc/self/../locks"] {
            assert_eq!(
                ProcfsFile::from_path(Path::new(alias)).unwrap().kind,
                ProcfsKind::Locks
            );
        }
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/smaps"))
                .unwrap()
                .kind,
            ProcfsKind::Smaps
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/driver/rtc"))
                .unwrap()
                .kind,
            ProcfsKind::Rtc
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/dentry-state"))
                .unwrap()
                .kind,
            ProcfsKind::DentryState
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/net/netlink"))
                .unwrap()
                .kind,
            ProcfsKind::NetlinkSockets
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/net/packet")).is_none());
        assert_eq!(
            ProcfsFile::from_path(Path::new(
                "/sys/fs/btrfs/004b7924-9df8-4ec2-aea0-d9775554e1ba/commit_stats"
            ))
            .unwrap()
            .kind,
            ProcfsKind::BtrfsCommitStats
        );
        assert!(
            ProcfsFile::from_path(Path::new(
                "/sys/fs/btrfs/004B7924-9DF8-4EC2-AEA0-D9775554E1BA/commit_stats"
            ))
            .is_none()
        );
        assert!(
            ProcfsFile::from_path(Path::new(
                "/sys/fs/btrfs/004b7924-9df8-4ec2-aea0-d9775554e1ba/generation"
            ))
            .is_none()
        );
        assert!(
            ProcfsFile::from_path(Path::new(
                "/sys/fs/btrfs/004b7924-9df8-4ec2-aea0-d9775554e1ba/nested/commit_stats"
            ))
            .is_none()
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/net/unix"))
                .unwrap()
                .kind,
            ProcfsKind::UnixSockets
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/net/packet")).is_none());
        for path in [
            "/proc/self/schedstat",
            "/proc/thread-self/schedstat",
            "/proc/123/schedstat",
            "/proc/self/task/456/schedstat",
            "/proc/123/task/456/schedstat",
        ] {
            assert_eq!(
                ProcfsFile::from_path(Path::new(path)).unwrap().kind,
                ProcfsKind::SelfSchedstat
            );
        }
        assert!(ProcfsFile::from_path(Path::new("/proc/self/task/nope/schedstat")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/proc/self/maps")).is_none());
    }

    #[test]
    fn recognizes_coherent_cpufreq_policy_paths() {
        for path in [
            "/sys/devices/system/cpu/cpu3/cpufreq/cpuinfo_cur_freq",
            "/sys/devices/system/cpu/cpu3/cpufreq/cpuinfo_avg_freq",
            "/sys/devices/system/cpu/cpu3/cpufreq/scaling_min_freq",
            "/sys/devices/system/cpu/cpufreq/policy3/cpuinfo_avg_freq",
            "/sys/devices/system/cpu/cpufreq/policy3/cpuinfo_max_freq",
        ] {
            assert_eq!(
                ProcfsFile::from_path(Path::new(path)).unwrap().kind,
                ProcfsKind::ScalingCurFreq
            );
        }
        assert!(
            ProcfsFile::from_path(Path::new("/tmp/cpufreq/policy3/cpuinfo_avg_freq")).is_none()
        );
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    #[test]
    fn recognizes_mount_and_random_uuid_paths() {
        for (path, kind) in [
            ("/proc/self/mountinfo", ProcfsKind::Mountinfo),
            ("/proc/sys/kernel/random/uuid", ProcfsKind::RandomUuid),
        ] {
            assert_eq!(ProcfsFile::from_path(Path::new(path)).unwrap().kind, kind);
        }
    }

    #[test]
    fn private_mount_roots_are_guest_stable() {
        let input = b"37 29 0:31 /tmpvol/.tmpAb12Z9 /tmp rw - btrfs /dev/md0 rw\n38 29 0:31 /host/data /data ro - btrfs /dev/md0 ro\n39 29 0:31 /tmp/.tmp654321 /etc/group ro - btrfs /dev/md0 ro\n";
        assert_eq!(
            sanitize_mountinfo(input),
            b"37 29 0:31 /tmpvol/.hermit/tmp /tmp rw - btrfs /dev/md0 rw\n38 29 0:31 /host/data /data ro - btrfs /dev/md0 ro\n39 29 0:31 /tmpvol/.hermit/etc/group /etc/group ro - btrfs /dev/md0 ro\n"
        );
    }

    #[test]
    fn random_uuid_uses_deterministic_v4_bytes_or_fails_open() {
        let kernel_uuid = b"24e63f35-232a-43e2-8799-b151e9833f45\n";
        assert_eq!(
            sanitize_random_uuid(kernel_uuid, [0; 16]),
            b"00000000-0000-4000-8000-000000000000\n"
        );
        assert_eq!(
            sanitize_random_uuid(kernel_uuid, [0xff; 16]),
            b"ffffffff-ffff-4fff-bfff-ffffffffffff\n"
        );

        for malformed in [
            b"24e63f35-232a-1e32-8799-b151e9833f45\n".as_slice(),
            b"24e63f35-232a-4e32-c799-b151e9833f45\n",
            b"24E63F35-232A-4E32-8799-B151E9833F45\n",
            b"24e63f35-232a-4e32-8799-b151e9833f45",
            b"not-a-uuid\n",
        ] {
            assert_eq!(sanitize_random_uuid(malformed, [0; 16]), malformed);
        }
    }

    #[test]
    fn recognizes_only_btrfs_reserved_byte_gauges() {
        const UUID: &str = "004b7924-9df8-4ec2-aea0-d9775554e1ba";
        for class in ["data", "metadata", "system"] {
            let path = format!("/sys/fs/btrfs/{UUID}/allocation/{class}/bytes_reserved");
            assert_eq!(
                ProcfsFile::from_path(Path::new(&path)).unwrap().kind,
                ProcfsKind::BtrfsBytesReserved
            );
        }

        for path in [
            "/sys/fs/btrfs/004B7924-9df8-4ec2-aea0-d9775554e1ba/allocation/data/bytes_reserved",
            "/sys/fs/btrfs/not-a-uuid/allocation/data/bytes_reserved",
            "/sys/fs/btrfs/004b7924-9df8-4ec2-aea0-d9775554e1ba/allocation/global/bytes_reserved",
            // bytes_may_use is now recognized as BtrfsBytesMayUse (PR-957).
            "/sys/fs/btrfs/004b7924-9df8-4ec2-aea0-d9775554e1ba/allocation/data/bytes_used",
            "/sys/fs/btrfs/004b7924-9df8-4ec2-aea0-d9775554e1ba/allocation/data/nested/bytes_reserved",
            "/tmp/004b7924-9df8-4ec2-aea0-d9775554e1ba/allocation/data/bytes_reserved",
        ] {
            assert!(ProcfsFile::from_path(Path::new(path)).is_none(), "{path}");
        }
    }

    #[test]
    fn recognizes_only_numa_node_vmstat_paths() {
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/devices/system/node/node0/vmstat"))
                .unwrap()
                .kind,
            ProcfsKind::NodeVmstat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("node12/vmstat"))
                .unwrap()
                .kind,
            ProcfsKind::NodeVmstat
        );
        assert!(ProcfsFile::from_path(Path::new("node/vmstat")).is_none());
        assert!(ProcfsFile::from_path(Path::new("node0/meminfo")).is_none());
        // /proc/vmstat is now recognized as ProcfsKind::Vmstat (PR-938).
    }

    #[test]
    fn node_vmstat_preserves_fields_and_zeros_accounting() {
        let contents = b"nr_free_pages 3859604\nnuma_hit 197122344154\nnr_writeback 38\n";
        assert_eq!(
            sanitize_node_vmstat(contents),
            b"nr_free_pages 0\nnuma_hit 0\nnr_writeback 0\n"
        );
        assert!(sanitize_node_vmstat(b"").is_empty());
        assert_eq!(
            sanitize_node_vmstat(b"nr_free_pages unknown\n"),
            b"nr_free_pages unknown\n"
        );
        assert_eq!(
            sanitize_node_vmstat(b"nr_free_pages 1 extra\n"),
            b"nr_free_pages 1 extra\n"
        );
    }

    #[test]
    fn recognizes_numa_and_hwmon_accounting_paths() {
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/devices/system/node/node0/numastat"))
                .unwrap()
                .kind,
            ProcfsKind::NodeNumastat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/devices/system/node/node12/meminfo"))
                .unwrap()
                .kind,
            ProcfsKind::NodeMeminfo
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/sys/class/hwmon/hwmon4/power1_input"))
                .unwrap()
                .kind,
            ProcfsKind::HwmonInput
        );
        assert!(
            ProcfsFile::from_path(Path::new("/sys/devices/system/node/nodeX/numastat")).is_none()
        );
        assert!(ProcfsFile::from_path(Path::new("/sys/class/hwmon/hwmon4/power1_cap")).is_none());
    }

    #[test]
    fn scaling_cur_freq_is_fixed() {
        assert_eq!(sanitize_scaling_cur_freq(b"2483951\n"), b"1000000\n");
        assert!(sanitize_scaling_cur_freq(b"").is_empty());
    }

    #[test]
    fn swaps_preserves_configuration_and_zeros_usage() {
        let contents = b"Filename\tType\tSize\tUsed\tPriority\n\
/dev/nvme1n1p3 partition 2000892 0 5\n\
/data/swapvol/swapfile file 134217724 69308912 -2\n";
        assert_eq!(
            sanitize_swaps(contents),
            b"Filename\tType\tSize\tUsed\tPriority\n\
/dev/nvme1n1p3\tpartition\t2000892\t0\t5\n\
/data/swapvol/swapfile\tfile\t134217724\t0\t-2\n"
        );
        assert!(sanitize_swaps(b"").is_empty());
        assert_eq!(
            sanitize_swaps(b"Filename Type Size Used Priority\n/swap file 12 3 1 extra\n"),
            b"Filename Type Size Used Priority\n/swap file 12 3 1 extra\n"
        );
        assert_eq!(
            sanitize_swaps(b"Filename Type Size Used Priority\n/swap file 12 unknown\n"),
            b"Filename Type Size Used Priority\n/swap file 12 unknown\n"
        );
    }

    #[test]
    fn key_users_preserves_uids_and_quota_limits() {
        let contents = b"0: 15 15/15 15/1000000 2499/25000000\n\
1000: 2 1/2 1/200 77/20000\n";
        assert_eq!(
            sanitize_key_users(contents),
            b"0: 0 0/0 0/1000000 0/25000000\n\
1000: 0 0/0 0/200 0/20000\n"
        );
        assert!(sanitize_key_users(b"").is_empty());
        assert_eq!(
            sanitize_key_users(b"0: 15 15/15 invalid 2499/25000000\n"),
            b"0: 15 15/15 invalid 2499/25000000\n"
        );
        assert_eq!(
            sanitize_key_users(b"0: 15 15/15 15/1000000 2499/25000000 extra\n"),
            b"0: 15 15/15 15/1000000 2499/25000000 extra\n"
        );
    }

    #[test]
    fn softnet_stat_synthesizes_one_virtual_cpu_row() {
        let input = b"00908c97 00000002 0000009e 00000004 00000005 00000006 00000007 00000008 00000009 0000000a 0000000b 0000000c 00000003 0000000e 0000000f\n\
00123456 00000012 00000034 00000056 00000078 0000009a 000000bc 000000de 000000f0 00000011 00000022 00000033 00000007 00000044 00000055\n";
        let expected = b"00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000\n";
        assert_eq!(sanitize_softnet_stat(input), expected);
        assert_eq!(sanitize_softnet_stat(b"not a softnet row\n"), expected);
    }

    #[test]
    fn btrfs_reserved_bytes_are_fixed() {
        assert_eq!(sanitize_btrfs_bytes_reserved(b"164425728\n"), b"0\n");
        assert_eq!(sanitize_btrfs_bytes_reserved(b"164425728"), b"0");
        assert_eq!(
            sanitize_btrfs_bytes_reserved(b"18446744073709551615\n"),
            b"0\n"
        );
        for malformed in [
            b"".as_slice(),
            b"18446744073709551616\n",
            b"-1\n",
            b"123 456\n",
            b"unknown\n",
        ] {
            assert_eq!(sanitize_btrfs_bytes_reserved(malformed), malformed);
        }
    }

    #[test]
    fn recognizes_only_btrfs_pinned_space_gauges() {
        const UUID: &str = "63152d54-3f28-408a-80a2-46e53b5c0bda";
        for class in ["data", "metadata", "system"] {
            let path = format!("/sys/fs/btrfs/{UUID}/allocation/{class}/bytes_pinned");
            assert_eq!(
                ProcfsFile::from_path(Path::new(&path)).unwrap().kind,
                ProcfsKind::BtrfsBytesPinned
            );
        }

        let reserved_path = format!("/sys/fs/btrfs/{UUID}/allocation/data/bytes_reserved");
        assert_eq!(
            ProcfsFile::from_path(Path::new(&reserved_path))
                .unwrap()
                .kind,
            ProcfsKind::BtrfsBytesReserved
        );

        for path in [
            "/sys/fs/btrfs/63152D54-3f28-408a-80a2-46e53b5c0bda/allocation/data/bytes_pinned",
            "/sys/fs/btrfs/not-a-uuid/allocation/data/bytes_pinned",
            "/sys/fs/btrfs/63152d54-3f28-408a-80a2-46e53b5c0bda/allocation/global/bytes_pinned",
            "/sys/fs/btrfs/63152d54-3f28-408a-80a2-46e53b5c0bda/allocation/data/bytes_pinned/extra",
            "/tmp/63152d54-3f28-408a-80a2-46e53b5c0bda/allocation/data/bytes_pinned",
        ] {
            assert!(ProcfsFile::from_path(Path::new(path)).is_none());
        }
    }

    #[test]
    fn btrfs_pinned_space_gauge_is_fixed() {
        assert_eq!(sanitize_btrfs_bytes_pinned(b"66535424\n"), b"0\n");
        assert_eq!(sanitize_btrfs_bytes_pinned(b"66535424"), b"0");
        for malformed in [
            b"".as_slice(),
            b"-1\n",
            b"123 456\n",
            b"123\n\n",
            b"unknown\n",
        ] {
            assert_eq!(sanitize_btrfs_bytes_pinned(malformed), malformed);
        }
    }

    #[test]
    fn recognizes_only_dynamic_cpuidle_counters() {
        for path in [
            "cpu0/cpuidle/state0/time",
            "/sys/devices/system/cpu/cpu3/cpuidle/state12/usage",
            "cpu0/cpuidle/state0/above",
            "cpu0/cpuidle/state0/below",
            "cpu0/cpuidle/state0/rejected",
        ] {
            assert_eq!(
                ProcfsFile::from_path(Path::new(path)).unwrap().kind,
                ProcfsKind::CpuidleCounter
            );
        }

        for path in [
            "cpu0/cpuidle/state0/name",
            "cpu0/cpuidle/state0/latency",
            "cpu0/cpuidle/state0/residency",
            "cpu0/cpuidle/state/usage",
            "cpu0/cpuidle/statex/time",
        ] {
            assert!(ProcfsFile::from_path(Path::new(path)).is_none());
        }
    }

    #[test]
    fn cpuidle_counter_is_fixed() {
        assert_eq!(sanitize_cpuidle_counter(b"42496983978\n"), b"0\n");
        assert!(sanitize_cpuidle_counter(b"").is_empty());
    }

    #[test]
    fn recognizes_only_per_size_thp_counters() {
        for counter in THP_COUNTERS {
            let path = format!("hugepages-2048kB/stats/{counter}");
            assert_eq!(
                ProcfsFile::from_path(Path::new(&path)).unwrap().kind,
                ProcfsKind::ThpCounter
            );
        }

        for path in [
            "hugepages-2048kB/enabled",
            "hugepages-2048kB/shmem_enabled",
            "hugepages-2048kB/stats/unknown",
            "hugepages-kB/stats/nr_anon",
            "hugepages-2MB/stats/nr_anon",
        ] {
            assert!(ProcfsFile::from_path(Path::new(path)).is_none());
        }
    }

    #[test]
    fn thp_counter_is_fixed() {
        assert_eq!(sanitize_thp_counter(b"37515411\n"), b"0\n");
        assert!(sanitize_thp_counter(b"").is_empty());
    }

    #[test]
    fn recognizes_only_numbered_cpu_cppc_feedback() {
        for path in [
            "cpu0/acpi_cppc/feedback_ctrs",
            "/sys/devices/system/cpu/cpu315/acpi_cppc/feedback_ctrs",
        ] {
            assert_eq!(
                ProcfsFile::from_path(Path::new(path)).unwrap().kind,
                ProcfsKind::CppcFeedback
            );
        }

        for path in [
            "cpu/acpi_cppc/feedback_ctrs",
            "gpu0/acpi_cppc/feedback_ctrs",
            "cpu0/acpi_cppc/highest_perf",
            "cpu0/feedback_ctrs",
        ] {
            assert!(ProcfsFile::from_path(Path::new(path)).is_none());
        }
    }

    #[test]
    fn cppc_feedback_counters_are_fixed() {
        assert_eq!(
            sanitize_cppc_feedback(b"ref:222494767542210 del:411574706774324\n"),
            b"ref:0 del:0\n"
        );
        assert_eq!(sanitize_cppc_feedback(b"ref:123 del:456"), b"ref:0 del:0");
        for malformed in [
            b"ref:abc del:456\n".as_slice(),
            b"ref:123 delivered:456\n",
            b"ref:123\n",
            b"ref:123 del:456 extra\n",
        ] {
            assert_eq!(sanitize_cppc_feedback(malformed), malformed);
        }
    }

    #[test]
    fn recognizes_only_numbered_sysfs_rtc_clock_attributes() {
        for (path, kind) in [
            ("/sys/class/rtc/rtc0/date", ProcfsKind::SysfsRtcDate),
            ("/sys/class/rtc/rtc12/time", ProcfsKind::SysfsRtcTime),
            (
                "/sys/class/rtc/rtc315/since_epoch",
                ProcfsKind::SysfsRtcEpoch,
            ),
        ] {
            assert_eq!(ProcfsFile::from_path(Path::new(path)).unwrap().kind, kind);
        }

        for path in [
            "/sys/class/rtc/rtc/date",
            "/sys/class/rtc/rtcX/time",
            "/sys/class/rtc/rtc0/hctosys",
            "/sys/class/rtc/rtc0/device/time",
            "/tmp/rtc0/since_epoch",
        ] {
            assert!(ProcfsFile::from_path(Path::new(path)).is_none());
        }
    }

    #[test]
    fn sysfs_rtc_uses_the_virtual_realtime() {
        assert_eq!(
            sanitize_sysfs_rtc_attribute(b"2026-07-27\n", ProcfsKind::SysfsRtcDate, 1_767_225_600,),
            b"2026-01-01\n"
        );
        assert_eq!(
            sanitize_sysfs_rtc_attribute(b"12:24:03\n", ProcfsKind::SysfsRtcTime, 1_767_229_261,),
            b"01:01:01\n"
        );
        assert_eq!(
            sanitize_sysfs_rtc_attribute(b"1785155071", ProcfsKind::SysfsRtcEpoch, 1_735_689_600,),
            b"1735689600"
        );
        for (malformed, kind) in [
            (b"2026/07/27\n".as_slice(), ProcfsKind::SysfsRtcDate),
            (b"12:24\n".as_slice(), ProcfsKind::SysfsRtcTime),
            (b"-1\n".as_slice(), ProcfsKind::SysfsRtcEpoch),
            (b"\n".as_slice(), ProcfsKind::SysfsRtcEpoch),
        ] {
            assert_eq!(
                sanitize_sysfs_rtc_attribute(malformed, kind, 1_767_225_600),
                malformed
            );
        }
    }

    #[test]
    fn uevent_seqnum_is_fixed_after_strict_validation() {
        assert_eq!(sanitize_uevent_seqnum(b"1282733\n"), b"0\n");
        assert_eq!(sanitize_uevent_seqnum(b"1282733"), b"1282733");
        assert_eq!(sanitize_uevent_seqnum(b"unknown\n"), b"unknown\n");
        assert_eq!(sanitize_uevent_seqnum(b"\n"), b"\n");
    }

    #[test]
    fn recognizes_only_btrfs_reservation_gauges() {
        const UUID: &str = "63152d54-3f28-408a-80a2-46e53b5c0bda";
        for class in ["data", "metadata", "system"] {
            let path = format!("/sys/fs/btrfs/{UUID}/allocation/{class}/bytes_may_use");
            assert_eq!(
                ProcfsFile::from_path(Path::new(&path)).unwrap().kind,
                ProcfsKind::BtrfsBytesMayUse
            );
        }

        for path in [
            "/sys/fs/btrfs/63152D54-3f28-408a-80a2-46e53b5c0bda/allocation/data/bytes_may_use",
            "/sys/fs/btrfs/not-a-uuid/allocation/data/bytes_may_use",
            "/sys/fs/btrfs/63152d54-3f28-408a-80a2-46e53b5c0bda/allocation/global/bytes_may_use",
            "/sys/fs/btrfs/63152d54-3f28-408a-80a2-46e53b5c0bda/allocation/data/bytes_used",
            "/tmp/63152d54-3f28-408a-80a2-46e53b5c0bda/allocation/data/bytes_may_use",
        ] {
            assert!(ProcfsFile::from_path(Path::new(path)).is_none());
        }
    }

    #[test]
    fn btrfs_reservation_gauge_is_fixed() {
        assert_eq!(sanitize_btrfs_bytes_may_use(b"58974208\n"), b"0\n");
        assert_eq!(sanitize_btrfs_bytes_may_use(b"58974208"), b"0");
        for malformed in [b"".as_slice(), b"-1\n", b"123 456\n", b"unknown\n"] {
            assert_eq!(sanitize_btrfs_bytes_may_use(malformed), malformed);
        }
    }

    #[test]
    fn numa_and_hwmon_values_are_synthetic() {
        assert_eq!(
            sanitize_node_numastat(b"numa_hit 123\nnuma_miss 7\n"),
            b"numa_hit 0\nnuma_miss 0\n"
        );
        assert_eq!(
            sanitize_node_meminfo(
                b"Node 0 MemTotal: 791462432 kB\nNode 0 MemFree: 24654068 kB\nNode 0 Active: 100 kB\nNode 0 HugePages_Total: 2\n"
            ),
            b"Node 0 MemTotal: 1048576 kB\nNode 0 MemFree: 1048576 kB\nNode 0 Active: 0 kB\nNode 0 HugePages_Total: 0\n"
        );
        assert_eq!(sanitize_numeric_scalar(b"193723000\n"), b"0\n");
        assert_eq!(sanitize_numeric_scalar(b"-12000"), b"0");
        assert_eq!(
            sanitize_numeric_scalar(b"not-a-number\n"),
            b"not-a-number\n"
        );
    }

    #[test]
    fn stat_normalizes_runtime_counters() {
        let input = b"3 (name with spaces) R 1 0 0 0 -1 0 89 0 1 2 3 4 5 6 20 0 1 7 520343512 2879488 123 18446744073709551615 100 200 300 0 0 0 0 3145728 0 0 0 0 17 114 0 0 9 10 11 400 500 600 700 800 900 1000 0\n";
        let output = String::from_utf8(sanitize_stat(input, Some((3, 1)))).unwrap();
        let comm_end = output.rfind(") ").unwrap();
        let fields = output[comm_end + 2..]
            .split_whitespace()
            .collect::<Vec<_>>();
        for field in [
            10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 23, 24, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35,
            36, 37, 39, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
        ] {
            assert_eq!(fields[field - 3], "0", "field {field} was not normalized");
        }
        assert!(output.starts_with("3 (name with spaces) R 1 0 0 "));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#553)
    #[test]
    fn status_normalizes_affinity_and_context_switches() {
        let input = b"Name:\tcat\nTgid:\t1234\nPid:\t1234\nPPid:\t1200\nTracerPid:\t0\nNStgid:\t1234\nNSpid:\t1234\nNSpgid:\t1200\nNSsid:\t1190\nSigQ:\t426/2042342\nVmHWM:\t1572 kB\nVmRSS:\t1568 kB\nRssFile:\t1452 kB\nCpus_allowed:\tffffffff,ffffffff\nCpus_allowed_list:\t0-63\nvoluntary_ctxt_switches:\t120\nnonvoluntary_ctxt_switches:\t3\n";
        assert_eq!(
            sanitize_status(input, Some((3, 3, 1))),
            b"Name:\tcat\nTgid:\t3\nPid:\t3\nPPid:\t1\nTracerPid:\t1\nNStgid:\t3\nNSpid:\t3\nNSpgid:\t0\nNSsid:\t0\nSigQ:\t0/0\nVmHWM:\t0 kB\nVmRSS:\t0 kB\nRssFile:\t0 kB\nCpus_allowed:\t00000000,00000000,00000000,00000001\nCpus_allowed_list:\t0\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n"
        );
    }

    #[test]
    fn thread_status_uses_the_identity_bound_when_opened() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/thread-self/status")).unwrap();
        assert!(file.needs_bound_thread_identity());
        file.bind_thread_identity(3, 4, 1);
        file.initialize(
            b"Tgid:\t1234\nPid:\t1235\nPPid:\t1200\nNStgid:\t1234\nNSpid:\t1235\n".to_vec(),
            ProcfsSnapshotContext {
                virtual_pid: 99,
                virtual_ppid: 98,
                ..ProcfsSnapshotContext::default()
            },
        );
        assert_eq!(
            file.take(usize::MAX).unwrap(),
            b"Tgid:\t3\nPid:\t4\nPPid:\t1\nNStgid:\t3\nNSpid:\t4\n"
        );
    }

    #[test]
    fn process_accounting_preserves_identity_and_hides_live_state() {
        let stat = b"42 (worker) R 1 7 8 0 -1 0 89 0 1 2 3 4 5 6 20 0 1 7 520343512 2879488 123 18446744073709551615 100 200 300 0 0 0 0 3145728 0 0 0 0 17 114 0 0 9 10 11 400 500 600 700 800 900 1000 0\n";
        let output = String::from_utf8(sanitize_stat(stat, None)).unwrap();
        assert!(output.starts_with("42 (worker) S 1 7 8 "));
        let comm_end = output.rfind(") ").unwrap();
        let fields = output[comm_end + 2..]
            .split_whitespace()
            .collect::<Vec<_>>();
        assert_eq!(fields[23 - 3], "0");
        assert_eq!(fields[24 - 3], "0");

        assert_eq!(
            sanitize_statm(b"62203 7952 5707 4033 0 3255 0\n"),
            b"0 0 0 0 0 0 0\n"
        );
        let status = b"Name:\thermit\nState:\tR (running)\nPid:\t1\nPPid:\t0\nVmSize:\t249000 kB\nVmRSS:\t30000 kB\n";
        assert_eq!(
            sanitize_status(status, None),
            b"Name:\thermit\nState:\tS (sleeping)\nPid:\t1\nPPid:\t0\nVmSize:\t0 kB\nVmRSS:\t0 kB\n"
        );
    }

    #[test]
    fn system_stat_uses_virtual_uptime_and_boot_time() {
        assert_eq!(
            sanitize_system_stat(
                b"cpu  1 2 3 4 5 6 7 8 9 10\ncpu0 1 2 3 4 5 6 7 8 9 10\nintr 9 8 7\nbtime 1234\nprocesses 55\n",
                120,
                1_767_225_480,
            ),
            b"cpu 12000 0 0 0 0 0 0 0 0 0\ncpu0 12000 0 0 0 0 0 0 0 0 0\nintr 0 0 0\nbtime 1767225480\nprocesses 0\n"
        );
    }

    #[test]
    fn cpuinfo_normalizes_frequency() {
        let input = b"processor\t: 0\ncpu MHz\t\t: 2994.183\ncache size\t: 1024 KB\n";
        assert_eq!(
            sanitize_cpuinfo(input),
            b"processor\t: 0\ncpu MHz\t\t: 1000.000\ncache size\t: 1024 KB\n"
        );
    }

    #[test]
    fn io_accounting_counters_use_synthetic_values() {
        assert_eq!(
            sanitize_diskstats(b"259 0 nvme0n1 100 2 300 4 500 6 700 8 9 10 11\n"),
            b"259 0 nvme0n1 1 0 8 0 1 0 8 0 0 0 0\n"
        );
        assert_eq!(
            sanitize_block_stat(b"100 2 300 4 500 6 700 8 9 10 11\n"),
            b"1 0 8 0 1 0 8 0 0 0 0\n"
        );
        assert_eq!(
            sanitize_process_io(
                b"rchar: 100\nwchar: 200\nsyscr: 3\nsyscw: 4\nread_bytes: 5\nwrite_bytes: 6\ncancelled_write_bytes: 7\n"
            ),
            b"rchar: 0\nwchar: 0\nsyscr: 0\nsyscw: 0\nread_bytes: 0\nwrite_bytes: 0\ncancelled_write_bytes: 0\n"
        );
    }

    #[test]
    fn loadavg_and_uptime_use_virtual_values() {
        assert_eq!(
            sanitize_loadavg(b"344.01 369.71 375.04 526/107858 512196\n"),
            b"0.00 0.00 0.00 1/1 1\n"
        );
        assert_eq!(
            sanitize_uptime(b"156980.56 37990755.08\n", 120),
            b"120.00 0.00\n"
        );
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-914): Review inode counter fixture coverage.
    #[test]
    fn inode_nr_hides_host_global_allocation_counters() {
        assert_eq!(sanitize_inode_nr(b"13929543\t1109179\n"), b"0\t0\n");
        assert_eq!(
            sanitize_inode_state(b"13929543\t1109179\t0\t0\t0\t0\t0\n"),
            b"0\t0\t0\t0\t0\t0\t0\n"
        );
        assert!(sanitize_inode_nr(b"").is_empty());
        assert!(sanitize_inode_state(b"").is_empty());
    }

    #[test]
    fn buddyinfo_preserves_topology_and_zeros_free_lists() {
        let contents = b"Node 0, zone DMA 0 1 2 3\n\
Node 1, zone Normal 42 17 5 1\n\
malformed buddy row\n";
        assert_eq!(
            sanitize_buddyinfo(contents),
            b"Node 0, zone DMA 0 0 0 0\n\
Node 1, zone Normal 0 0 0 0\n\
malformed buddy row\n"
        );
    }

    #[test]
    fn file_nr_hides_host_global_allocations() {
        assert_eq!(
            sanitize_file_nr(b"245853\t0\t9223372036854775807\n"),
            b"0\t0\t9223372036854775807\n"
        );
        assert_eq!(sanitize_file_max(b"1048576\n"), b"9223372036854775807\n");
        assert!(sanitize_file_nr(b"").is_empty());
        assert!(sanitize_file_max(b"").is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-918): Review dentry counter fixture coverage.
    #[test]
    fn dentry_state_hides_host_global_cache_counters() {
        assert_eq!(
            sanitize_dentry_state(b"1888773\t1374220\t45\t0\t212904\t0\n"),
            b"0\t0\t45\t0\t0\t0\n"
        );
        assert!(sanitize_dentry_state(b"").is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-933): Review AIO count fixture coverage.
    #[test]
    fn aio_nr_hides_host_global_reservations() {
        assert_eq!(sanitize_aio_nr(b"3040\n"), b"0\n");
        assert!(sanitize_aio_nr(b"").is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-927): Review PTY count fixture coverage.
    #[test]
    fn pty_nr_hides_host_global_allocations() {
        assert_eq!(sanitize_pty_nr(b"107\n", 2), b"2\n");
        assert!(sanitize_pty_nr(b"", 2).is_empty());
    }

    #[test]
    fn sockstat_hides_host_global_allocation_and_memory_counters() {
        let contents = b"sockets: used 41\n\
TCP: inuse 3 orphan 2 tw 7 alloc 100 mem 200\n\
UDP: inuse 4 mem 99\n\
RAW: inuse 5\n";

        assert_eq!(
            sanitize_sockstat(contents),
            b"sockets: used 41\n\
TCP: inuse 3 orphan 0 tw 7 alloc 3 mem 0\n\
UDP: inuse 4 mem 0\n\
RAW: inuse 5\n"
        );
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    #[test]
    fn recognizes_interrupt_and_module_accounting_paths() {
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/interrupts"))
                .unwrap()
                .kind,
            ProcfsKind::InterruptCounters
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/softirqs"))
                .unwrap()
                .kind,
            ProcfsKind::InterruptCounters
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/modules"))
                .unwrap()
                .kind,
            ProcfsKind::Modules
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/devices")).is_none());
    }

    #[test]
    fn interrupt_and_module_accounting_is_synthetic() {
        let interrupts = b"           CPU0       CPU1\n  9:        123          4 IR-PCI-MSI 0-edge acpi\nNMI:          8          9 Non-maskable interrupts\nERR:          5\n";
        assert_eq!(
            sanitize_interrupt_counters(interrupts),
            b"           CPU0       CPU1\n  9: 0 0 IR-PCI-MSI 0-edge acpi\nNMI: 0 0 Non-maskable interrupts\nERR: 0\n"
        );
        assert_eq!(
            sanitize_interrupt_counters(b"  9: 9 99 IR-PCI-MSI 0-edge acpi\nERR: 999999\n"),
            sanitize_interrupt_counters(b"  9: 123456789 4 IR-PCI-MSI 0-edge acpi\nERR: 5\n"),
            "native counter width and padding must not affect the snapshot"
        );

        let modules = b"kvm_amd 212992 95 - Live 0x0\nkvm 1200128 1 kvm_amd, Live 0x0\nllc 20480 2 bridge,stp, Live 0x0\n";
        assert_eq!(
            sanitize_modules(modules),
            b"kvm_amd 212992 0 - Live 0x0\nkvm 1200128 1 kvm_amd, Live 0x0\nllc 20480 2 bridge,stp, Live 0x0\n"
        );
    }

    #[test]
    fn pressure_hides_host_stall_averages_and_totals() {
        let contents = b"some avg10=12.34 avg60=23.45 avg300=34.56 total=123456\n\
full avg10=1.23 avg60=2.34 avg300=3.45 total=654321\n";

        assert_eq!(
            sanitize_pressure(contents),
            b"some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n\
full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
        );
    }

    #[test]
    fn rtc_uses_the_virtual_clock_and_hides_alarm_state() {
        let contents = b"rtc_time\t: 10:07:42\n\
rtc_date\t: 2026-07-27\n\
alrm_time\t: 00:50:12\n\
alrm_date\t: 2026-07-28\n\
alarm_IRQ\t: yes\n\
periodic IRQ enabled\t: yes\n\
periodic IRQ frequency\t: 1024\n\
24hr\t\t: yes\n";

        assert_eq!(
            sanitize_rtc(contents, 978_307_199),
            b"rtc_time\t: 23:59:59\n\
rtc_date\t: 2000-12-31\n\
alrm_time\t: 00:00:00\n\
alrm_date\t: 2000-12-31\n\
alarm_IRQ\t: no\n\
periodic IRQ enabled\t: no\n\
periodic IRQ frequency\t: 0\n\
24hr\t\t: yes\n"
        );
    }

    #[test]
    fn protocols_hides_live_socket_and_memory_counters() {
        let contents = b"protocol size sockets memory press maxhdr slab module cl co\n\
TCP 2304 17 12309 no 256 yes kernel y y\n\
RAW 1008 4 -1 NI 0 yes kernel y y\n";

        assert_eq!(
            sanitize_protocols(contents),
            b"protocol size sockets memory press maxhdr slab module cl co\n\
TCP 2304 0 0 no 256 yes kernel y y\n\
RAW 1008 0 -1 NI 0 yes kernel y y\n"
        );
    }

    #[test]
    fn schedstat_hides_host_scheduler_accounting() {
        let contents = b"version 17\n\
timestamp 4671819092\n\
cpu0 1 2 3 4 5 6 129488086714063 39956207684532 545893933\n\
domain0 SMT 00000003 1 2 3\n";

        assert_eq!(
            sanitize_schedstat(contents),
            b"version 17\n\
timestamp 0\n\
cpu0 0 0 0 0 0 0 0 0 0\n\
domain0 SMT 00000003 0 0 0\n"
        );
    }

    #[test]
    fn self_schedstat_hides_host_scheduler_accounting() {
        assert_eq!(
            sanitize_self_schedstat(b"3029609 1559338 150\n"),
            b"0 0 0\n"
        );
        assert_eq!(sanitize_self_schedstat(b"3029609 1559338 150"), b"0 0 0");
    }

    #[test]
    fn self_schedstat_leaves_unknown_formats_untouched() {
        let extra_field = b"3029609 1559338 150 4\n";
        assert_eq!(sanitize_self_schedstat(extra_field), extra_field);

        let invalid_counter = b"3029609 waiting 150\n";
        assert_eq!(sanitize_self_schedstat(invalid_counter), invalid_counter);
    }

    #[test]
    fn zoneinfo_hides_host_memory_accounting() {
        let contents = b"Node 3, zone    DMA32\n\
  pages free     2816\n\
      nr_inactive_anon 39937459\n\
        protection: (0, 2117, 772897)\n\
    cpu: 7\n\
              count:    12\n";

        assert_eq!(
            sanitize_zoneinfo(contents),
            b"Node 3, zone    DMA32\n\
  pages free     0\n\
      nr_inactive_anon 0\n\
        protection: (0, 0, 0)\n\
    cpu: 7\n\
              count:    0\n"
        );
    }

    #[test]
    fn protocols_leaves_unknown_formats_untouched() {
        let missing_column = b"protocol size sockets press\nTCP 2304 17 no\n";
        assert_eq!(sanitize_protocols(missing_column), missing_column);

        let invalid_counter = b"protocol size sockets memory press maxhdr slab module\n\
TCP 2304 many 12309 no 256 yes kernel\n";
        assert_eq!(sanitize_protocols(invalid_counter), invalid_counter);
    }

    #[test]
    fn smaps_hides_host_memory_accounting_but_preserves_mapping_metadata() {
        let contents = b"71000000-71001000 r-xp 00000000 00:00 0\n\
Size:                  4 kB\n\
KernelPageSize:        4 kB\n\
MMUPageSize:           4 kB\n\
Rss:                   4 kB\n\
Pss:                   3 kB\n\
Shared_Clean:          4 kB\n\
Private_Dirty:         0 kB\n\
THPeligible:           0\n\
ProtectionKey:         0\n\
VmFlags: rd ex mr mw me ac\n";

        assert_eq!(
            sanitize_smaps(contents),
            b"71000000-71001000 r-xp 00000000 00:00 0\n\
Size:                  4 kB\n\
KernelPageSize:        4 kB\n\
MMUPageSize:           4 kB\n\
Rss:\t0 kB\n\
Pss:\t0 kB\n\
Shared_Clean:\t0 kB\n\
Private_Dirty:         0 kB\n\
THPeligible:           0\n\
ProtectionKey:         0\n\
VmFlags: rd ex mr mw me ac\n"
        );
    }

    #[test]
    fn smaps_leaves_unknown_or_malformed_formats_untouched() {
        let invalid_counter = b"71000000-71001000 r-xp 00000000 00:00 0\nPss: many kB\n";
        assert_eq!(sanitize_smaps(invalid_counter), invalid_counter);

        let unknown_field = b"71000000-71001000 r-xp 00000000 00:00 0\nMystery: 1 kB\n";
        assert_eq!(sanitize_smaps(unknown_field), unknown_field);

        let invalid_header = b"not-a-range r-xp 00000000 00:00 0\nPss: 3 kB\n";
        assert_eq!(sanitize_smaps(invalid_header), invalid_header);
    }

    #[test]
    fn arch_status_uses_logical_avx512_elapsed_time() {
        let contents = b"AVX512_elapsed_ms:\t48\n\
x86_Thread_features:\t\tshstk\n\
x86_Thread_features_locked:\t\n";

        assert_eq!(
            sanitize_arch_status(contents),
            b"AVX512_elapsed_ms:\t0\n\
x86_Thread_features:\t\tshstk\n\
x86_Thread_features_locked:\t\n"
        );
        assert_eq!(
            sanitize_arch_status(b"AVX512_elapsed_ms:\tunknown\n"),
            b"AVX512_elapsed_ms:\tunknown\n"
        );
    }

    #[test]
    fn smaps_rollup_hides_physical_page_accounting() {
        let contents = b"71000000-7ffffffff000 ---p 00000000 00:00 0 [rollup]\n\
Rss:                2216 kB\n\
Pss:                 311 kB\n\
Pss_File:            131 kB\n\
Locked:                12 kB\n\
Swap:                   8 kB\n\
THPeligible:    0\n";

        assert_eq!(
            sanitize_smaps_rollup(contents),
            b"71000000-7ffffffff000 ---p 00000000 00:00 0 [rollup]\n\
Rss:\t0 kB\n\
Pss:\t0 kB\n\
Pss_File:\t0 kB\n\
Locked:                12 kB\n\
Swap:                   8 kB\n\
THPeligible:    0\n"
        );
    }

    #[test]
    fn numa_maps_hides_host_page_aging_and_sharing_maxima() {
        let contents = b"71000000 default anon=1 dirty=1 active=0 N0=1 kernelpagesize_kB=4\n\
7ffff7c00000 default file=/usr/lib64/libc.so.6 mapped=41 mapmax=443 N0=41 kernelpagesize_kB=4\n";

        assert_eq!(
            sanitize_numa_maps(contents),
            b"71000000 default anon=1 dirty=1 N0=1 kernelpagesize_kB=4\n\
7ffff7c00000 default file=/usr/lib64/libc.so.6 mapped=41 N0=41 kernelpagesize_kB=4\n"
        );
    }

    #[test]
    fn numa_maps_leaves_unknown_formats_untouched() {
        let invalid_address = b"address default anon=1\n";
        assert_eq!(sanitize_numa_maps(invalid_address), invalid_address);

        let invalid_counter = b"71000000 default active=recent N0=1\n";
        assert_eq!(sanitize_numa_maps(invalid_counter), invalid_counter);
    }

    #[test]
    fn fdinfo_hides_backing_identity_only() {
        let contents = b"pos:\t1\n\
flags:\t0100002\n\
mnt_id:\t16368\n\
ino:\t47761541\n\
eventfd-count: 0000000000000007\n";

        assert_eq!(
            sanitize_fdinfo(contents, Some((9007, 0o100002, 42))),
            b"pos:\t1\n\
flags:\t0100002\n\
mnt_id:\t1\n\
ino:\t9007\n\
eventfd-count: 0000000000000007\n"
        );
    }

    #[test]
    fn self_sched_hides_host_scheduler_accounting() {
        let contents = b"cat (3, #threads: 7)\n\
se.exec_start : 377650149.445644\n\
se.vruntime : 133948666.432951\n\
se.sum_exec_runtime : 3.637972\n\
nr_switches : 149\n\
se.avg.load_avg : 749\n\
uclamp.min : 128\n\
uclamp.max : 768\n\
effective uclamp.min : 256\n\
effective uclamp.max : 512\n\
policy : 0\n\
uclamp.max : 1024\n\
new.kernel.counter : 55\n\
current_node=7, numa_group_id=91\n\
numa_faults node=7 task_private=8 task_shared=9 group_private=10 group_shared=11\n";

        assert_eq!(
            sanitize_self_sched(contents),
            b"cat (0, #threads: 1)\n\
se.exec_start : 0.000000\n\
se.vruntime : 0.000000\n\
se.sum_exec_runtime : 0.000000\n\
nr_switches : 0\n\
se.avg.load_avg : 0\n\
uclamp.min : 0\n\
uclamp.max : 1024\n\
effective uclamp.min : 0\n\
effective uclamp.max : 1024\n\
policy : 0\n\
uclamp.max : 1024\n\
new.kernel.counter : 0\n\
current_node=0, numa_group_id=0\n\
numa_faults node=0 task_private=0 task_shared=0 group_private=0 group_shared=0\n"
        );
    }

    #[test]
    fn self_sched_fails_closed_on_unknown_formats() {
        let missing_core_field = b"se.exec_start : 1.0\nse.vruntime : 2.0\n";
        assert!(sanitize_self_sched(missing_core_field).is_empty());

        let invalid_counter = b"se.exec_start : NaN\n\
se.vruntime : 2.0\n\
se.sum_exec_runtime : 3.0\n";
        assert!(sanitize_self_sched(invalid_counter).is_empty());

        let negative_counter = b"se.exec_start : 1.0\n\
se.vruntime : 2.0\n\
se.sum_exec_runtime : 3.0\n\
nr_switches : -1\n";
        assert!(sanitize_self_sched(negative_counter).is_empty());
    }

    #[test]
    fn locks_virtualize_identities_but_preserve_equivalences() {
        // A holder (seq 74) with a waiter blocked behind it on the same object,
        // a second lock by the same owner (PID 480) on a *different* object,
        // and a third lock by a different owner on the *first* object.
        let contents = b"74: POSIX  ADVISORY  WRITE 480 08:02:1111 0 EOF\n\
74: -> POSIX  ADVISORY  WRITE 481 08:02:1111 0 EOF\n\
9: OFDLCK ADVISORY  WRITE 480 08:02:2222 0 EOF\n\
21: FLOCK ADVISORY  WRITE 999 08:02:1111 0 EOF\n";

        let out = String::from_utf8(sanitize_locks(contents)).unwrap();
        let lines: Vec<&str> = out.lines().collect();

        // No host-specific sequence, PID, or device:inode magnitude survives.
        assert!(!out.contains("74"), "raw sequence leaked: {out}");
        assert!(!out.contains("480") && !out.contains("481") && !out.contains("999"));
        assert!(!out.contains("1111") && !out.contains("2222"));

        // The holder and its `->` waiter keep a shared virtual sequence and stay
        // adjacent (grouping preserved rather than scattered by a global sort).
        let holder = lines
            .iter()
            .position(|l| l.contains("POSIX") && !l.contains("->"));
        let waiter = lines.iter().position(|l| l.contains("->"));
        let (holder, waiter) = (holder.unwrap(), waiter.unwrap());
        assert_eq!(waiter, holder + 1, "waiter must immediately follow holder");
        let seq_of = |l: &str| l.split(':').next().unwrap().to_owned();
        assert_eq!(seq_of(lines[holder]), seq_of(lines[waiter]));

        // The two locks on object 08:02:1111 (holder + FLOCK) share one virtual
        // object id; the lock on 08:02:2222 keeps a *distinct* one.
        let obj_of = |l: &str| l.split_whitespace().nth_back(2).unwrap().to_owned();
        let flock = lines.iter().find(|l| l.contains("FLOCK")).unwrap();
        let ofd = lines.iter().find(|l| l.contains("OFDLCK")).unwrap();
        assert_eq!(
            obj_of(lines[holder]),
            obj_of(flock),
            "same object must match"
        );
        assert_ne!(
            obj_of(lines[holder]),
            obj_of(ofd),
            "distinct objects must differ"
        );

        // Same raw owner (PID 480) on two different objects keeps one virtual
        // owner; the other owners stay distinct.
        let pid_of = |l: &str| l.split_whitespace().nth_back(3).unwrap().to_owned();
        assert_eq!(pid_of(lines[holder]), pid_of(ofd), "same owner must match");
        assert_ne!(pid_of(lines[holder]), pid_of(lines[waiter]));
        assert_ne!(pid_of(lines[holder]), pid_of(flock));

        let renumbered_and_reordered = b"800: FLOCK ADVISORY WRITE 9000 00:fe:9001 0 EOF\n\
700: OFDLCK ADVISORY WRITE 8000 00:fe:9002 0 EOF\n\
900: -> POSIX ADVISORY WRITE 7000 00:fe:9001 0 EOF\n\
900: POSIX ADVISORY WRITE 8000 00:fe:9001 0 EOF\n";
        assert_eq!(
            sanitize_locks(contents),
            sanitize_locks(renumbered_and_reordered),
            "raw ID renumbering and row order changed the virtual graph"
        );

        // Fail closed, not open: one unclassifiable row suppresses the entire
        // snapshot instead of mixing a sentinel with partially trusted data.
        assert!(sanitize_locks(b"malformed row\n").is_empty());
        assert!(
            sanitize_locks(b"74: POSIX ADVISORY WRITE 480 08:02:1111 0 EOF\nmalformed\n")
                .is_empty()
        );
        assert!(sanitize_locks(&[0xff, 0xfe]).is_empty());
        assert!(sanitize_locks(b"").is_empty());
    }

    #[test]
    fn unix_sockets_hide_kernel_identities_and_sort_semantic_rows() {
        let contents = b"Num RefCount Protocol Flags Type St Inode Path\n\
00000000fedcba98: 00000003 00000000 00010000 0002 01 12346 /run/socket two\n\
000000001234abcd: 00000002 00000000 00000000 0001 03 12345\n";

        assert_eq!(
            sanitize_unix_sockets(contents),
            b"Num RefCount Protocol Flags Type St Inode Path\n\
0000000000000000: 00000002 00000000 00000000 0001 03 0\n\
0000000000000000: 00000003 00000000 00010000 0002 01 0 /run/socket two\n"
        );
    }

    #[test]
    fn unix_sockets_fail_open_on_unknown_schemas() {
        for malformed in [
            b"".as_slice(),
            b"Num RefCount Protocol Flags Type St Inode Path",
            b"Num RefCount Protocol Flags Type St Inode Path Extra\n",
            b"Num RefCount Protocol Flags Type St Inode Path\n1234: 00000003 00000000 00000000 0001 03 12345\n",
            b"Num RefCount Protocol Flags Type St Inode Path\n000000001234abcd: 0000000G 00000000 00000000 0001 03 12345\n",
            b"Num RefCount Protocol Flags Type St Inode Path\n000000001234abcd: 00000003 00000000 00000000 0001 03 inode\n",
            b"Num RefCount Protocol Flags Type St Inode Path\n000000001234abcd: 00000003 00000000 00000000 0001 03 18446744073709551616\n",
            b"Num RefCount Protocol Flags Type St Inode Path\n000000001234abcd: 00000003 00000000 00000000 0001\n",
        ] {
            assert_eq!(sanitize_unix_sockets(malformed), malformed);
        }
    }

    #[test]
    fn btrfs_commit_stats_hides_host_commit_telemetry() {
        let contents = b"commits 14545\n\
cur_commit_ms 3\n\
last_commit_ms 211\n\
max_commit_ms 1977796\n\
total_commit_ms 11281713\n";
        assert_eq!(
            sanitize_btrfs_commit_stats(contents),
            b"commits 0\n\
cur_commit_ms 0\n\
last_commit_ms 0\n\
max_commit_ms 0\n\
total_commit_ms 0\n"
        );
    }

    #[test]
    fn btrfs_commit_stats_leaves_unknown_formats_untouched() {
        let wrong_order = b"cur_commit_ms 3\ncommits 14545\n";
        assert_eq!(sanitize_btrfs_commit_stats(wrong_order), wrong_order);

        let invalid_value = b"commits many\ncur_commit_ms 3\nlast_commit_ms 211\nmax_commit_ms 7\ntotal_commit_ms 9\n";
        assert_eq!(sanitize_btrfs_commit_stats(invalid_value), invalid_value);

        let extra_field = b"commits 1 transactions\ncur_commit_ms 3\nlast_commit_ms 2\nmax_commit_ms 7\ntotal_commit_ms 9\n";
        assert_eq!(sanitize_btrfs_commit_stats(extra_field), extra_field);
    }

    #[test]
    fn netlink_sockets_hide_kernel_identities_and_sort_semantic_rows() {
        let contents = b"sk Eth Pid Groups Rmem Wmem Dump Locks Drops Inode\n\
00000000fedcba98 10 20 00000002 3 4 5 6 7 12346\n\
000000001234abcd 4 10 00000001 0 0 0 2 0 12345\n";

        assert_eq!(
            sanitize_netlink_sockets(contents),
            b"sk Eth Pid Groups Rmem Wmem Dump Locks Drops Inode\n\
0000000000000000 4 10 00000001 0 0 0 2 0 0\n\
0000000000000000 10 20 00000002 3 4 5 6 7 0\n"
        );
    }

    #[test]
    fn netlink_sockets_fail_open_on_unknown_schemas() {
        for malformed in [
            b"".as_slice(),
            b"sk Eth Pid Groups Rmem Wmem Dump Locks Drops Inode",
            b"sk Eth Pid Groups Rmem Wmem Dump Locks Drops Inode Extra\n",
            b"sk Eth Pid Groups Rmem Wmem Dump Locks Drops Inode\n1234 4 10 00000001 0 0 0 2 0 12345\n",
            b"sk Eth Pid Groups Rmem Wmem Dump Locks Drops Inode\n000000001234abcd 4 10 00000001 0 0 0 2 zero 12345\n",
            b"sk Eth Pid Groups Rmem Wmem Dump Locks Drops Inode\n000000001234abcd 4 10 00000001 0 0 0 2 0 12345 extra\n",
        ] {
            assert_eq!(sanitize_netlink_sockets(malformed), malformed);
        }
    }

    #[test]
    fn irq_per_cpu_count_hides_host_interrupt_totals() {
        assert_eq!(
            sanitize_irq_per_cpu_count(b"0,17,0,983421,0\n"),
            b"0,0,0,0,0\n"
        );
        assert_eq!(sanitize_irq_per_cpu_count(b"42,0"), b"0,0");
    }

    #[test]
    fn irq_per_cpu_count_leaves_unknown_formats_untouched() {
        for contents in [
            b"".as_slice(),
            b"1,,2\n".as_slice(),
            b"1,2,three\n".as_slice(),
            b"1,2\n3,4\n".as_slice(),
        ] {
            assert_eq!(sanitize_irq_per_cpu_count(contents), contents);
        }
    }

    #[test]
    fn block_inflight_hides_host_queue_depths() {
        assert_eq!(sanitize_block_inflight(b"      24        3\n"), b"0 0\n");
        assert_eq!(sanitize_block_inflight(b"0 7"), b"0 0");
    }

    #[test]
    fn block_inflight_leaves_unknown_formats_untouched() {
        let malformed = b"reads writes\n";
        assert_eq!(sanitize_block_inflight(malformed), malformed);

        let extra_field = b"1 2 3\n";
        assert_eq!(sanitize_block_inflight(extra_field), extra_field);

        let extra_row = b"1 2\n3 4\n";
        assert_eq!(sanitize_block_inflight(extra_row), extra_row);
    }

    #[test]
    fn vmstat_hides_host_vm_accounting() {
        let contents = b"nr_free_pages 4587515\npgfault 175926829665\noom_kill 30\n";
        assert_eq!(
            sanitize_vmstat(contents),
            b"nr_free_pages 0\npgfault 0\noom_kill 0\n"
        );
        assert_eq!(
            sanitize_vmstat(b"nr_free_pages 4587515"),
            b"nr_free_pages 0"
        );
    }

    #[test]
    fn vmstat_leaves_unknown_formats_untouched() {
        let extra_field = b"nr_free_pages 4587515 pages\n";
        assert_eq!(sanitize_vmstat(extra_field), extra_field);

        let invalid_counter = b"nr_free_pages many\n";
        assert_eq!(sanitize_vmstat(invalid_counter), invalid_counter);
    }

    #[test]
    fn meminfo_uses_configured_guest_memory() {
        assert_eq!(
            sanitize_meminfo(b"MemTotal: 791462432 kB\n", 1_048_576),
            b"MemTotal:       1048576 kB\n\
              MemFree:        1048576 kB\n\
              MemAvailable:   1048576 kB\n\
              Buffers:        0 kB\n\
              Cached:         0 kB\n\
              SwapCached:     0 kB\n\
              Active:         0 kB\n\
              Inactive:       0 kB\n\
              Shmem:          0 kB\n\
              SReclaimable:   0 kB\n\
              SwapTotal:      0 kB\n\
              SwapFree:       0 kB\n"
        );
        assert!(sanitize_meminfo(b"", 1_048_576).is_empty());
    }

    #[test]
    fn snapshot_supports_partial_reads() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/self/status")).unwrap();
        file.initialize(
            b"voluntary_ctxt_switches:\t12\n".to_vec(),
            ProcfsSnapshotContext {
                virtual_uptime_seconds: 120,
                virtual_pid: 3,
                virtual_ppid: 1,
                ..ProcfsSnapshotContext::default()
            },
        );
        assert_eq!(file.take(5).unwrap(), b"volun");
        assert_eq!(file.take(128).unwrap(), b"tary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());
    }

    #[test]
    fn snapshot_supports_positional_reads_and_rewinds() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/sys/fs/file-nr")).unwrap();
        file.initialize(
            b"245853\t0\t1048576\n".to_vec(),
            ProcfsSnapshotContext {
                virtual_pid: 1,
                ..ProcfsSnapshotContext::default()
            },
        );

        assert_eq!(file.take(2).unwrap(), b"0\t");
        assert_eq!(file.take_at(4, 1).unwrap(), b"9");
        assert_eq!(file.position().0, 2, "pread must not move the cursor");
        file.set_offset(0);
        assert_eq!(file.take(128).unwrap(), b"0\t0\t9223372036854775807\n");
    }
}
