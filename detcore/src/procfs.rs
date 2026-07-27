/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic snapshots for volatile procfs and sysfs files.

use std::path::Path;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ProcfsKind {
    Stat,
    Status,
    Cpuinfo,
    Diskstats,
    Loadavg,
    ProcessIo,
    Uptime,
    BlockStat,
    ScalingCurFreq,
    Sockstat,
    PtyNr,
    SelfSched,
    Fdinfo,
    AioNr,
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

/// State for a procfs file whose volatile fields require normalization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProcfsFile {
    kind: ProcfsKind,
    contents: Option<Vec<u8>>,
    offset: usize,
}

impl ProcfsFile {
    /// Recognizes procfs files that contain observed volatile fields.
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let kind = match path.to_str()? {
            "/proc/self/stat" => ProcfsKind::Stat,
            "/proc/self/status" => ProcfsKind::Status,
            "/proc/cpuinfo" => ProcfsKind::Cpuinfo,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-861): Review deterministic kernel I/O accounting.
            "/proc/diskstats" => ProcfsKind::Diskstats,
            "/proc/loadavg" => ProcfsKind::Loadavg,
            "/proc/uptime" => ProcfsKind::Uptime,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-914): Review host-global inode counter normalization.
            "/proc/sys/fs/inode-nr" => ProcfsKind::InodeNr,
            "/proc/sys/fs/inode-state" => ProcfsKind::InodeState,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-918): Review host-global dentry counter normalization.
            "/proc/sys/fs/dentry-state" => ProcfsKind::DentryState,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-933): Review host-global AIO count normalization.
            "/proc/sys/fs/aio-nr" => ProcfsKind::AioNr,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-927): Review host-global PTY count normalization.
            "/proc/sys/kernel/pty/nr" => ProcfsKind::PtyNr,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-866): Review host-global socket counter normalization.
            "/proc/net/sockstat" => ProcfsKind::Sockstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-939): Review NUMA node VM accounting normalization.
            other if is_node_vmstat_path(other) => ProcfsKind::NodeVmstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-928): Review per-process host scheduler normalization.
            "/proc/self/sched" => ProcfsKind::SelfSched,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-931): Review fdinfo backing-identity normalization.
            other
                if other.strip_prefix("/proc/self/fdinfo/").is_some_and(|fd| {
                    !fd.is_empty() && fd.bytes().all(|byte| byte.is_ascii_digit())
                }) =>
            {
                ProcfsKind::Fdinfo
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-934): Review host NUMA observation normalization.
            "/proc/self/numa_maps" => ProcfsKind::NumaMaps,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-937): Review smaps rollup accounting normalization.
            "/proc/self/smaps_rollup" => ProcfsKind::SmapsRollup,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-944): Review AVX-512 elapsed-time normalization.
            "/proc/self/arch_status" => ProcfsKind::ArchStatus,
            "/proc/swaps" => ProcfsKind::Swaps,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-949): Review per-mapping memory accounting normalization.
            "/proc/self/smaps" => ProcfsKind::Smaps,
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
            // A cpufreq `*_cur_freq` file reports the instantaneous core clock,
            // a live hardware reading that differs run-to-run and breaks tools
            // like `lscpu` under `--verify`. These are opened relative to a
            // `/sys/devices/system/cpu` directory fd, so match on the suffix
            // rather than an absolute path.
            other
                if other.ends_with("cpufreq/scaling_cur_freq")
                    || other.ends_with("cpufreq/cpuinfo_cur_freq") =>
            {
                ProcfsKind::ScalingCurFreq
            }
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
            _ => return None,
        };
        Some(Self {
            kind,
            contents: None,
            offset: 0,
        })
    }

    /// Returns true until the underlying procfs content has been captured.
    pub(crate) fn needs_snapshot(&self) -> bool {
        self.contents.is_none()
    }

    /// Normalizes and stores a complete snapshot captured from the kernel.
    // TODO-HUMAN-REVIEW(PR-723): Review procfs snapshot identity normalization.
    pub(crate) fn initialize(
        &mut self,
        contents: Vec<u8>,
        virtual_uptime_seconds: u64,
        virtual_realtime_seconds: i64,
        virtual_pid: i32,
        virtual_ppid: i32,
    ) {
        self.contents = Some(match self.kind {
            ProcfsKind::Stat => sanitize_stat(&contents, virtual_pid, virtual_ppid),
            ProcfsKind::Status => sanitize_status(&contents, virtual_pid, virtual_ppid),
            ProcfsKind::Cpuinfo => sanitize_cpuinfo(&contents),
            ProcfsKind::Diskstats => sanitize_diskstats(&contents),
            ProcfsKind::Loadavg => sanitize_loadavg(&contents),
            ProcfsKind::ProcessIo => sanitize_process_io(&contents),
            ProcfsKind::Uptime => sanitize_uptime(&contents, virtual_uptime_seconds),
            ProcfsKind::BlockStat => sanitize_block_stat(&contents),
            ProcfsKind::ScalingCurFreq => sanitize_scaling_cur_freq(&contents),
            ProcfsKind::Sockstat => sanitize_sockstat(&contents),
            ProcfsKind::PtyNr => sanitize_pty_nr(&contents),
            ProcfsKind::SelfSched => sanitize_self_sched(&contents),
            ProcfsKind::Fdinfo => sanitize_fdinfo(&contents),
            ProcfsKind::AioNr => sanitize_aio_nr(&contents),
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
            ProcfsKind::Locks => sanitize_locks(&contents),
            ProcfsKind::NodeVmstat => sanitize_node_vmstat(&contents),
            ProcfsKind::ThpCounter => sanitize_thp_counter(&contents),
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

// TODO-HUMAN-REVIEW(PR-723): Review /proc stat identity field normalization.
fn sanitize_stat(contents: &[u8], virtual_pid: i32, virtual_ppid: i32) -> Vec<u8> {
    const VOLATILE_FIELDS: &[usize] = &[10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 24, 39, 42, 43, 44];

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
    fields[4 - 3] = virtual_ppid.to_string();
    fields[5 - 3] = "0".to_owned();
    fields[6 - 3] = "0".to_owned();
    for field in VOLATILE_FIELDS {
        fields[*field - 3] = "0".to_owned();
    }
    format!("{virtual_pid}{comm} {}\n", fields.join(" ")).into_bytes()
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#553)
// TODO-HUMAN-REVIEW(PR-723): Review /proc status identity field normalization.
fn sanitize_status(contents: &[u8], virtual_pid: i32, virtual_ppid: i32) -> Vec<u8> {
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

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(TGID)
            || body.starts_with(PID)
            || body.starts_with(NS_TGID)
            || body.starts_with(NS_PID)
        {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(format!(":\t{virtual_pid}").as_bytes());
        } else if body.starts_with(PPID) {
            normalized.extend_from_slice(PPID);
            normalized.extend_from_slice(format!("\t{virtual_ppid}").as_bytes());
        } else if body.starts_with(TRACER_PID) {
            normalized.extend_from_slice(TRACER_PID);
            normalized.extend_from_slice(b"\t1");
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
            normalized.extend_from_slice(b"cpu MHz\t\t: 0.000");
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-764)
/// Normalizes a cpufreq `scaling_cur_freq` / `cpuinfo_cur_freq` snapshot. The
/// instantaneous core frequency is a live hardware reading that varies between
/// otherwise identical runs, so replace it with a fixed value. This mirrors the
/// `cpu MHz` zeroing already done for `/proc/cpuinfo` in [`sanitize_cpuinfo`],
/// and keeps the static `cpuinfo_max_freq`/`scaling_max_freq` files untouched.
fn sanitize_scaling_cur_freq(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\n".to_vec()
    }
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
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\n".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-927): Review the /proc/sys/kernel/pty/nr policy.
fn sanitize_pty_nr(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\n".to_vec()
    }
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

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let mut normalized = Vec::with_capacity(contents.len());
    let mut core_fields_seen = [false; 3];

    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let Some((left, right)) = body.split_once(':') else {
            normalized.extend_from_slice(body.as_bytes());
            if has_newline {
                normalized.push(b'\n');
            }
            continue;
        };
        let label = left.trim();
        let replacement = if let Some(index) = FLOAT_FIELDS.iter().position(|field| *field == label)
        {
            let Ok(value) = right.trim().parse::<f64>() else {
                return contents.to_vec();
            };
            if !value.is_finite() || value.is_sign_negative() {
                return contents.to_vec();
            }
            core_fields_seen[index] = true;
            Some("0.000000")
        } else if INTEGER_FIELDS.contains(&label) {
            if right.trim().parse::<u128>().is_err() {
                return contents.to_vec();
            }
            Some("0")
        } else {
            None
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

    if core_fields_seen.iter().all(|seen| *seen) {
        normalized
    } else {
        contents.to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-931): Review the /proc/self/fdinfo field policy.
fn sanitize_fdinfo(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };

    let mut normalized = Vec::with_capacity(contents.len());
    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.starts_with("mnt_id:") {
            normalized.extend_from_slice(b"mnt_id:\t0");
        } else if body.starts_with("ino:") {
            normalized.extend_from_slice(b"ino:\t0");
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
        if let Some(label) = accounting_label {
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
        "Private_Dirty",
        "Referenced",
        "Anonymous",
        "KSM",
        "LazyFree",
        "AnonHugePages",
        "ShmemPmdMapped",
        "FilePmdMapped",
        "Shared_Hugetlb",
        "Private_Hugetlb",
        "Swap",
        "SwapPss",
        "Locked",
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
                    "Size" | "KernelPageSize" | "MMUPageSize" => is_smaps_kilobyte_value(value),
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
// Rather than collapse every identity to a single constant (which would make
// two distinct locked files indistinguishable and break holder/waiter
// grouping), remap each *distinct* raw sequence, PID, and device:inode to a
// dense synthetic value. Equal raw values map to equal synthetic values, so
// same-object, same-owner, and holder/waiter equivalences survive while the
// host-specific magnitudes do not. Rows are grouped by their (virtual)
// sequence with the granted holder ahead of its waiters, giving a byte-stable
// snapshot without scattering a group across a global sort. A row we cannot
// classify is redacted to a sentinel rather than passed through, so an
// unrecognized kernel extension fails safe instead of leaking raw identities.
fn sanitize_locks(contents: &[u8]) -> Vec<u8> {
    use std::collections::BTreeMap;

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    if text.is_empty() {
        return Vec::new();
    }

    struct LockRow {
        details: usize,
        seq: String,
        fields: Vec<String>,
    }

    let mut rows = Vec::new();
    let mut redactions = 0usize;
    // Distinct raw values, kept sorted so the dense assignment is reproducible.
    let mut seqs = BTreeMap::new();
    let mut pids = BTreeMap::new();
    let mut objs = BTreeMap::new();
    for line in text.lines() {
        let fields = line
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let details = usize::from(fields.get(1).is_some_and(|field| field == "->")) + 1;
        let well_formed = fields.len() >= details + 7
            && fields.first().is_some_and(|field| field.ends_with(':'))
            && fields[details + 4].split(':').count() == 3;
        if !well_formed {
            redactions += 1;
            continue;
        }

        let seq = fields[0].trim_end_matches(':').to_owned();
        seqs.insert(seq.clone(), 0usize);
        pids.insert(fields[details + 3].clone(), 0usize);
        objs.insert(fields[details + 4].clone(), 0usize);
        rows.push(LockRow {
            details,
            seq,
            fields,
        });
    }

    // Assign dense identities in sorted order of the distinct raw values.
    for (index, value) in seqs.values_mut().enumerate() {
        *value = index;
    }
    for (index, value) in pids.values_mut().enumerate() {
        *value = index + 1;
    }
    for (index, value) in objs.values_mut().enumerate() {
        *value = index + 1;
    }

    for row in &mut rows {
        let details = row.details;
        row.fields[0] = format!("{}:", seqs[&row.seq]);
        row.fields[details + 3] = pids[&row.fields[details + 3]].to_string();
        row.fields[details + 4] = format!("00:00:{}", objs[&row.fields[details + 4]]);
    }

    // Group by virtual sequence, holder (details == 1) before its waiters, so a
    // lock and the tasks blocked on it stay adjacent and in a stable order.
    rows.sort_by(|a, b| {
        (seqs[&a.seq], a.details, &a.fields).cmp(&(seqs[&b.seq], b.details, &b.fields))
    });

    let mut normalized: Vec<String> = rows.iter().map(|row| row.fields.join(" ")).collect();
    normalized.extend(std::iter::repeat_n("REDACTED".to_owned(), redactions));

    let mut normalized = normalized.join("\n").into_bytes();
    if text.ends_with('\n') {
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
            ProcfsFile::from_path(Path::new("/proc/net/sockstat"))
                .unwrap()
                .kind,
            ProcfsKind::Sockstat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/kernel/pty/nr"))
                .unwrap()
                .kind,
            ProcfsKind::PtyNr
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/sched"))
                .unwrap()
                .kind,
            ProcfsKind::SelfSched
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/fdinfo/17"))
                .unwrap()
                .kind,
            ProcfsKind::Fdinfo
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/self/fdinfo/")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/proc/self/fdinfo/stdin")).is_none());
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/sys/fs/aio-nr"))
                .unwrap()
                .kind,
            ProcfsKind::AioNr
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
    fn recognizes_cpufreq_current_frequency_by_suffix() {
        // Opened relative to a `/sys/devices/system/cpu` directory fd.
        assert_eq!(
            ProcfsFile::from_path(Path::new("cpu0/cpufreq/scaling_cur_freq"))
                .unwrap()
                .kind,
            ProcfsKind::ScalingCurFreq
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new(
                "/sys/devices/system/cpu/cpu3/cpufreq/cpuinfo_cur_freq"
            ))
            .unwrap()
            .kind,
            ProcfsKind::ScalingCurFreq
        );
        // The static min/max limits are deterministic and must not be rewritten.
        assert!(ProcfsFile::from_path(Path::new("cpu0/cpufreq/cpuinfo_max_freq")).is_none());
        assert!(ProcfsFile::from_path(Path::new("cpu0/cpufreq/scaling_max_freq")).is_none());
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
            "/sys/fs/btrfs/004b7924-9df8-4ec2-aea0-d9775554e1ba/allocation/data/bytes_may_use",
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
        assert!(ProcfsFile::from_path(Path::new("/proc/vmstat")).is_none());
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
    fn scaling_cur_freq_is_fixed() {
        assert_eq!(sanitize_scaling_cur_freq(b"2483951\n"), b"0\n");
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
    fn stat_normalizes_runtime_counters() {
        let input = b"3 (name with spaces) R 1 0 0 0 -1 0 89 0 1 2 3 4 5 6 20 0 1 7 520343512 2879488 123 18446744073709551615 100 200 300 0 0 0 0 3145728 0 0 0 0 17 114 0 0 9 10 11 400 500 600 700 800 900 1000 0\n";
        let output = String::from_utf8(sanitize_stat(input, 3, 1)).unwrap();
        let comm_end = output.rfind(") ").unwrap();
        let fields = output[comm_end + 2..]
            .split_whitespace()
            .collect::<Vec<_>>();
        for field in [10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 24, 39, 42, 43, 44] {
            assert_eq!(fields[field - 3], "0", "field {field} was not normalized");
        }
        assert!(output.starts_with("3 (name with spaces) R 1 0 0 "));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#553)
    #[test]
    fn status_normalizes_affinity_and_context_switches() {
        let input = b"Name:\tcat\nTgid:\t1234\nPid:\t1234\nPPid:\t1200\nTracerPid:\t0\nNStgid:\t1234\nNSpid:\t1234\nNSpgid:\t1200\nNSsid:\t1190\nSigQ:\t426/2042342\nCpus_allowed:\tffffffff,ffffffff\nCpus_allowed_list:\t0-63\nvoluntary_ctxt_switches:\t120\nnonvoluntary_ctxt_switches:\t3\n";
        assert_eq!(
            sanitize_status(input, 3, 1),
            b"Name:\tcat\nTgid:\t3\nPid:\t3\nPPid:\t1\nTracerPid:\t1\nNStgid:\t3\nNSpid:\t3\nNSpgid:\t0\nNSsid:\t0\nSigQ:\t0/0\nCpus_allowed:\t00000000,00000000,00000000,00000001\nCpus_allowed_list:\t0\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n"
        );
    }

    #[test]
    fn cpuinfo_normalizes_frequency() {
        let input = b"processor\t: 0\ncpu MHz\t\t: 2994.183\ncache size\t: 1024 KB\n";
        assert_eq!(
            sanitize_cpuinfo(input),
            b"processor\t: 0\ncpu MHz\t\t: 0.000\ncache size\t: 1024 KB\n"
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
        assert_eq!(sanitize_pty_nr(b"107\n"), b"0\n");
        assert!(sanitize_pty_nr(b"").is_empty());
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
Private_Dirty:\t0 kB\n\
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
THPeligible:    0\n";

        assert_eq!(
            sanitize_smaps_rollup(contents),
            b"71000000-7ffffffff000 ---p 00000000 00:00 0 [rollup]\n\
Rss:\t0 kB\n\
Pss:\t0 kB\n\
Pss_File:\t0 kB\n\
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
            sanitize_fdinfo(contents),
            b"pos:\t1\n\
flags:\t0100002\n\
mnt_id:\t0\n\
ino:\t0\n\
eventfd-count: 0000000000000007\n"
        );
    }

    #[test]
    fn self_sched_hides_host_scheduler_accounting() {
        let contents = b"cat (3, #threads: 1)\n\
se.exec_start : 377650149.445644\n\
se.vruntime : 133948666.432951\n\
se.sum_exec_runtime : 3.637972\n\
nr_switches : 149\n\
se.avg.load_avg : 749\n\
policy : 0\n";

        assert_eq!(
            sanitize_self_sched(contents),
            b"cat (3, #threads: 1)\n\
se.exec_start : 0.000000\n\
se.vruntime : 0.000000\n\
se.sum_exec_runtime : 0.000000\n\
nr_switches : 0\n\
se.avg.load_avg : 0\n\
policy : 0\n"
        );
    }

    #[test]
    fn self_sched_leaves_unknown_formats_untouched() {
        let missing_core_field = b"se.exec_start : 1.0\nse.vruntime : 2.0\n";
        assert_eq!(sanitize_self_sched(missing_core_field), missing_core_field);

        let invalid_counter = b"se.exec_start : NaN\n\
se.vruntime : 2.0\n\
se.sum_exec_runtime : 3.0\n";
        assert_eq!(sanitize_self_sched(invalid_counter), invalid_counter);

        let negative_counter = b"se.exec_start : 1.0\n\
se.vruntime : 2.0\n\
se.sum_exec_runtime : 3.0\n\
nr_switches : -1\n";
        assert_eq!(sanitize_self_sched(negative_counter), negative_counter);
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

        // Fail safe, not open: an unclassifiable row is redacted rather than
        // passed through with its raw kernel identities.
        assert_eq!(sanitize_locks(b"malformed row\n"), b"REDACTED\n");
        assert_eq!(
            sanitize_locks(b"74: POSIX ADVISORY WRITE 480 08:02:1111 0 EOF\nmalformed\n"),
            b"0: POSIX ADVISORY WRITE 1 00:00:1 0 EOF\nREDACTED\n"
        );
        assert!(sanitize_locks(b"").is_empty());
    }

    #[test]
    fn snapshot_supports_partial_reads() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/self/status")).unwrap();
        file.initialize(b"voluntary_ctxt_switches:\t12\n".to_vec(), 120, 0, 3, 1);
        assert_eq!(file.take(5).unwrap(), b"volun");
        assert_eq!(file.take(128).unwrap(), b"tary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());
    }

    #[test]
    fn snapshot_supports_positional_reads_and_rewinds() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/sys/fs/file-nr")).unwrap();
        file.initialize(b"245853\t0\t1048576\n".to_vec(), 0, 0, 1, 0);

        assert_eq!(file.take(2).unwrap(), b"0\t");
        assert_eq!(file.take_at(4, 1).unwrap(), b"9");
        assert_eq!(file.position().0, 2, "pread must not move the cursor");
        file.set_offset(0);
        assert_eq!(file.take(128).unwrap(), b"0\t0\t9223372036854775807\n");
    }
}
