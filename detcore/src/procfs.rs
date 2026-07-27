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
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-951): Review key-user resource normalization.
    KeyUsers,
    Pressure,
    Buddyinfo,
    Schedstat,
    SoftnetStat,
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
            // TODO-HUMAN-REVIEW(PR-866): Review host-global socket counter normalization.
            "/proc/net/sockstat" => ProcfsKind::Sockstat,
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
            ProcfsKind::KeyUsers => sanitize_key_users(&contents),
            ProcfsKind::Pressure => sanitize_pressure(&contents),
            ProcfsKind::Buddyinfo => sanitize_buddyinfo(&contents),
            ProcfsKind::Schedstat => sanitize_schedstat(&contents),
            ProcfsKind::SoftnetStat => sanitize_softnet_stat(&contents),
        });
        self.offset = 0;
    }

    /// Returns the next bytes from the normalized snapshot.
    pub(crate) fn take(&mut self, maximum: usize) -> Option<Vec<u8>> {
        let contents = self.contents.as_ref()?;
        let end = self.offset.saturating_add(maximum).min(contents.len());
        let bytes = contents[self.offset..end].to_vec();
        self.offset = end;
        Some(bytes)
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

// TODO-HUMAN-REVIEW(PR-909): Review the preserved CPU-index column.
/// Preserves the softnet table shape and per-row CPU index while hiding live
/// network backlog counters maintained by the host kernel.
fn sanitize_softnet_stat(contents: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let rows = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    if rows.is_empty()
        || rows.iter().any(|fields| {
            fields.len() < 13
                || fields.iter().any(|field| {
                    field.len() != 8 || !field.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        })
    {
        return contents.to_vec();
    }

    let mut normalized = String::with_capacity(text.len());
    for fields in rows {
        for (index, field) in fields.into_iter().enumerate() {
            if index != 0 {
                normalized.push(' ');
            }
            normalized.push_str(if index == 12 { field } else { "00000000" });
        }
        normalized.push('\n');
    }
    if !text.ends_with('\n') {
        normalized.pop();
    }
    normalized.into_bytes()
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
    fn softnet_stat_preserves_cpu_indices_and_zeros_counters() {
        let input = b"00908c97 00000002 0000009e 00000004 00000005 00000006 00000007 00000008 00000009 0000000a 0000000b 0000000c 00000003 0000000e 0000000f\n";
        assert_eq!(
            sanitize_softnet_stat(input),
            b"00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000003 00000000 00000000\n"
        );
        assert_eq!(
            sanitize_softnet_stat(b"not a softnet row\n"),
            b"not a softnet row\n"
        );
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
    fn snapshot_supports_partial_reads() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/self/status")).unwrap();
        file.initialize(b"voluntary_ctxt_switches:\t12\n".to_vec(), 120, 3, 1);
        assert_eq!(file.take(5).unwrap(), b"volun");
        assert_eq!(file.take(128).unwrap(), b"tary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());
    }
}
