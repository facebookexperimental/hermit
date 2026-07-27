/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic snapshots for volatile procfs and sysfs files.

use std::path::Path;

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
    Smaps,
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-951): Review key-user resource normalization.
    KeyUsers,
    Pressure,
    Buddyinfo,
    Schedstat,
    SoftnetStat,
    FileNr,
    FileMax,
    Zoneinfo,
    InodeNr,
    InodeState,
    Protocols,
    BtrfsBytesReserved,
}

fn is_btrfs_bytes_reserved_path(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix("/sys/fs/btrfs") else {
        return false;
    };
    let mut components = relative.iter();
    let (Some(uuid), Some("allocation"), Some(class), Some("bytes_reserved"), None) = (
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next().and_then(|part| part.to_str()),
        components.next(),
    ) else {
        return false;
    };

    let canonical_uuid = uuid.len() == 36
        && uuid.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        });
    canonical_uuid && matches!(class, "data" | "metadata" | "system")
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
            // TODO-HUMAN-REVIEW(PR-866): Review host-global socket counter normalization.
            "/proc/net/sockstat" => ProcfsKind::Sockstat,
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
            // TODO-HUMAN-REVIEW(PR-969): Review Btrfs reserved-byte normalization.
            other if is_btrfs_bytes_reserved_path(Path::new(other)) => {
                ProcfsKind::BtrfsBytesReserved
            }
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
            ProcfsKind::Smaps => sanitize_smaps(&contents),
            ProcfsKind::KeyUsers => sanitize_key_users(&contents),
            ProcfsKind::Pressure => sanitize_pressure(&contents),
            ProcfsKind::Buddyinfo => sanitize_buddyinfo(&contents),
            ProcfsKind::Schedstat => sanitize_schedstat(&contents),
            ProcfsKind::SoftnetStat => sanitize_softnet_stat(&contents),
            ProcfsKind::FileNr => sanitize_file_nr(&contents),
            ProcfsKind::FileMax => sanitize_file_max(&contents),
            ProcfsKind::Zoneinfo => sanitize_zoneinfo(&contents),
            ProcfsKind::InodeNr => sanitize_inode_nr(&contents),
            ProcfsKind::InodeState => sanitize_inode_state(&contents),
            ProcfsKind::Protocols => sanitize_protocols(&contents),
            ProcfsKind::BtrfsBytesReserved => sanitize_btrfs_bytes_reserved(&contents),
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
            ProcfsFile::from_path(Path::new("/proc/self/smaps"))
                .unwrap()
                .kind,
            ProcfsKind::Smaps
        );
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
    fn scaling_cur_freq_is_fixed() {
        assert_eq!(sanitize_scaling_cur_freq(b"2483951\n"), b"0\n");
        assert!(sanitize_scaling_cur_freq(b"").is_empty());
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
    fn snapshot_supports_partial_reads() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/self/status")).unwrap();
        file.initialize(b"voluntary_ctxt_switches:\t12\n".to_vec(), 120, 3, 1);
        assert_eq!(file.take(5).unwrap(), b"volun");
        assert_eq!(file.take(128).unwrap(), b"tary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());
    }

    #[test]
    fn snapshot_supports_positional_reads_and_rewinds() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/sys/fs/file-nr")).unwrap();
        file.initialize(b"245853\t0\t1048576\n".to_vec(), 0, 1, 0);

        assert_eq!(file.take(2).unwrap(), b"0\t");
        assert_eq!(file.take_at(4, 1).unwrap(), b"9");
        assert_eq!(file.position().0, 2, "pread must not move the cursor");
        file.set_offset(0);
        assert_eq!(file.take(128).unwrap(), b"0\t0\t9223372036854775807\n");
    }
}
