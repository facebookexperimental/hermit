/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Shared assertion path for the isolation integration test and matrix runner.
//! Physical identities are compared as relations; descriptor numbers, errors,
//! mount rows, permissions, and guest-visible runtime material remain values.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

pub const NAMESPACES: [&str; 7] = ["user", "pid", "uts", "mnt", "net", "ipc", "cgroup"];
pub const BACKENDS: [&str; 7] = [
    "ptrace",
    "dbt",
    "sabre",
    "liteinst",
    "in-guest-trap",
    "kvm",
    "e9patch",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Match,
    Mismatch,
    NoResult,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::NoResult => "no_result",
        }
    }
}

#[derive(Debug)]
pub struct Assertion {
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
}

impl Assertion {
    fn check(name: impl Into<String>, condition: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            verdict: if condition {
                Verdict::Match
            } else {
                Verdict::Mismatch
            },
            detail: detail.into(),
        }
    }

    fn unavailable(name: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            verdict: Verdict::NoResult,
            detail: error.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespacePolicy {
    Private,
    HostNetwork,
    None,
}

impl NamespacePolicy {
    fn shares(self, namespace: &str) -> bool {
        self == Self::None || (self == Self::HostNetwork && namespace == "net")
    }
}

#[derive(Clone, Debug)]
struct Record {
    kind: String,
    tag: String,
    fields: BTreeMap<String, String>,
}

impl Record {
    fn field(&self, name: &str) -> Result<&str, String> {
        self.fields
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("{} {} is missing {name}", self.kind, self.tag))
    }

    fn number(&self, name: &str) -> Result<i128, String> {
        self.field(name)?
            .parse()
            .map_err(|_| format!("{} {} has invalid {name}", self.kind, self.tag))
    }

    fn object(&self) -> Result<(i128, i128), String> {
        if self.fields.contains_key("rc") && self.number("rc")? < 0 {
            return Err(format!(
                "{} {} failed with errno={}",
                self.kind,
                self.tag,
                self.number("errno")?
            ));
        }
        Ok((self.number("dev")?, self.number("ino")?))
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        if self.number("rc")? < 0 || self.number("complete")? != 1 {
            return Err(format!(
                "{} {} failed/incomplete (errno={})",
                self.kind,
                self.tag,
                self.number("errno")?
            ));
        }
        let bytes = decode_hex(self.field("bytes")?)?;
        if self.number("rc")? != bytes.len() as i128 {
            return Err(format!(
                "{} {} byte count is incomplete",
                self.kind, self.tag
            ));
        }
        Ok(bytes)
    }

    fn marker_object(&self) -> Result<Option<(i128, i128)>, String> {
        let reachable = self.number("reachable")?;
        let error = self.number("errno")?;
        if reachable == 1 && error == 0 {
            if self.number("stat_rc")? != 0 || self.number("stat_errno")? != 0 {
                return Err(format!(
                    "{} {} opened marker identity is unavailable",
                    self.kind, self.tag
                ));
            }
            return Ok(Some(self.object()?));
        }
        if reachable == 0 && matches!(error, 2 | 20) {
            return Ok(None);
        }
        Err(format!(
            "{} {} unexpected observation reachable={reachable}, errno={error}",
            self.kind, self.tag
        ))
    }
}

#[derive(Clone, Debug)]
pub struct Observation {
    records: Vec<Record>,
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("odd hex-encoded byte string".to_owned());
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err("invalid hex-encoded byte string".to_owned()),
            };
            Ok((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}

impl Observation {
    pub fn cli_exit(&self) -> Result<(i128, i128), String> {
        let exit = self.one("CLI_EXIT", "child")?;
        if exit.number("exited")? != 1 {
            return Err(format!(
                "CLI did not exit normally; signal={}",
                exit.number("signal")?
            ));
        }
        Ok((exit.number("code")?, exit.number("signal")?))
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "report is not UTF-8 ASCII")?;
        if !text.ends_with('\n') {
            return Err("report ends in a partial record".to_owned());
        }
        let mut records = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let mut words = line.split_ascii_whitespace();
            let kind = words
                .next()
                .ok_or_else(|| format!("empty record at line {}", index + 1))?;
            let tag = words
                .next()
                .ok_or_else(|| format!("missing record tag at line {}", index + 1))?;
            let mut fields = BTreeMap::new();
            for word in words {
                let (key, value) = word
                    .split_once('=')
                    .ok_or_else(|| format!("untyped record field at line {}", index + 1))?;
                if fields.insert(key.to_owned(), value.to_owned()).is_some() {
                    return Err(format!("duplicate record field at line {}", index + 1));
                }
            }
            records.push(Record {
                kind: kind.to_owned(),
                tag: tag.to_owned(),
                fields,
            });
        }
        Ok(Self { records })
    }

    pub fn read(path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path).map_err(|error| format!("report metadata: {error}"))?;
        if metadata.len() > 64 * 1024 * 1024 {
            return Err("report exceeds the complete-observation limit".to_owned());
        }
        Self::parse(&fs::read(path).map_err(|error| format!("report read: {error}"))?)
    }

    fn one(&self, kind: &str, tag: &str) -> Result<&Record, String> {
        let mut records = self
            .records
            .iter()
            .filter(|record| record.kind == kind && record.tag == tag);
        let first = records
            .next()
            .ok_or_else(|| format!("missing {kind} {tag}"))?;
        if records.next().is_some() {
            return Err(format!("ambiguous {kind} {tag}"));
        }
        Ok(first)
    }

    fn object(&self, kind: &str, tag: &str) -> Result<(i128, i128), String> {
        self.one(kind, tag)?.object()
    }

    fn file(&self, tag: &str) -> Result<Vec<u8>, String> {
        self.one("FILE", tag)?.bytes()
    }

    fn directory(&self, tag: &str) -> Result<Vec<Vec<u8>>, String> {
        let directory = self.one("DIRECTORY", tag)?;
        let count = directory.number("rc")?;
        if count < 0 {
            return Err(format!(
                "directory {tag} failed with errno={}",
                directory.number("errno")?
            ));
        }
        let entries: Vec<_> = self
            .records
            .iter()
            .filter(|record| record.kind == "ENTRY" && record.tag == tag)
            .map(|record| decode_hex(record.field("bytes")?))
            .collect::<Result<_, String>>()?;
        if count != entries.len() as i128 + 2 {
            return Err(format!("directory {tag} enumeration count is incomplete"));
        }
        if entries.iter().collect::<BTreeSet<_>>().len() != entries.len() {
            return Err(format!("directory {tag} contains duplicate names"));
        }
        Ok(entries)
    }

    fn descriptor_numbers(&self, process: &str) -> Result<BTreeSet<i128>, String> {
        let directory = self.one("FD_DIRECTORY", process)?;
        let count = directory.number("rc")?;
        if count < 0 {
            return Err(format!(
                "descriptor directory {process} failed errno={}",
                directory.number("errno")?
            ));
        }
        let mut descriptors = BTreeSet::new();
        for entry in self
            .records
            .iter()
            .filter(|record| record.kind == "FD_ENTRY" && record.tag == process)
        {
            let number = entry.number("fd")?;
            if number < 0 || !descriptors.insert(number) {
                return Err(format!(
                    "descriptor directory {process} contains an invalid or repeated descriptor"
                ));
            }
        }
        if count != descriptors.len() as i128 + 2 {
            return Err(format!(
                "descriptor directory {process} count={count} does not bind every numeric entry"
            ));
        }
        let complete = self.one("FD_SCAN_COMPLETE", process)?;
        if complete.number("rc")? != 0 {
            return Err(format!(
                "descriptor scan {process} did not complete errno={}",
                complete.number("errno")?
            ));
        }
        Ok(descriptors)
    }

    fn descriptor(&self, process: &str, fd: i128) -> Result<(i128, i128), String> {
        let tag = format!("{process}/{fd}");
        let object = self.object("STAT", &tag)?;
        self.one("LINK", &tag)?.bytes()?;
        let info = self.one("FILE", &tag)?.bytes()?;
        for field in ["pos", "flags", "mnt_id", "ino"] {
            status_field(&info, field)?;
        }
        let capability = self.one("CAPABILITY", &tag)?;
        capability.marker_object()?;
        if process == "self" {
            let flags = self.one("FD_FLAGS", &tag)?;
            for field in ["flags", "mode"] {
                if flags.number(field)? < 0 || flags.number(&format!("{field}_errno"))? != 0 {
                    return Err(format!("descriptor {tag} {field} failed"));
                }
            }
            let direct = self.one("FD_OPENAT", &tag)?;
            direct.marker_object()?;
        }
        Ok(object)
    }
}

fn native_logical_app<'a>(
    guest: &Observation,
    native: &'a Observation,
) -> Result<&'a Record, String> {
    // Legacy ROLE rows are comm hints. They cannot authenticate the logical
    // application or a DBT carrier, even when one namespace happens to match.
    let role = native.one("BOUND_ROLE", "logical_app")?;
    if role.field("proof")? != "exec-object" {
        return Err("logical-app carrier proof requires the separately reviewed mapped-ELF/report-object observation; a role label cannot establish it".to_owned());
    }
    let process_tag = role.field("process")?;
    let process = native.one("PROCESS", process_tag)?;
    if process.number("pid")? != role.number("pid")?
        || process.number("start")? != role.number("start")?
    {
        return Err(
            "logical-app PID/starttime does not match its independently captured process"
                .to_owned(),
        );
    }
    native.one("PROCESS_COMPLETE", process_tag)?;
    let expected = native.object("EXPECTED_OBJECT", "logical_app")?;
    let executable = native.object("STAT", &format!("{process_tag}/exe"))?;
    if expected != executable {
        return Err(format!(
            "logical-app executable object mismatch: expected={expected:?}, observed={executable:?}"
        ));
    }
    let status = native.file(&format!("{process_tag}/status"))?;
    let native_pid = status_field(&status, "NSpid")?
        .split_ascii_whitespace()
        .last()
        .ok_or("empty native NSpid")?;
    let guest_status = guest.file("self/status")?;
    if native_pid != status_field(&guest_status, "Pid")? {
        return Err(
            "logical-app namespace PID does not match the guest's real proc status".to_owned(),
        );
    }
    Ok(process)
}

fn native_complete(native: &Observation) -> Result<(bool, String), String> {
    native.one("NATIVE_SCHEMA", "backend-isolation-physical-v1")?;
    let complete = native.one("NATIVE_COMPLETE", "backend-isolation-physical-v1")?;
    let errors = complete.number("errors")?;
    let live = complete.number("live")?;
    if errors != 0
        || live != 0
        || native
            .records
            .iter()
            .any(|record| matches!(record.kind.as_str(), "CHILDREN_ERROR" | "OBSERVER_ERROR"))
    {
        return Err(format!(
            "incomplete native descendant proof: errors={errors}, retained live descendants={live}; discovery/probe errors are retained"
        ));
    }
    // A final footer and some process rows do not prove a held target was
    // captured. The observer must bind every required physical generation.
    let expected = complete.number("expected_generations")?;
    let observed = complete.number("observed_generations")?;
    if expected <= 0 || observed != expected || complete.number("live_boundary_complete")? != 1 {
        return Err(format!(
            "physical generation coverage incomplete: expected={expected}, observed={observed}"
        ));
    }
    let generation_rows = native
        .records
        .iter()
        .filter(|record| record.kind == "GENERATION_COMPLETE")
        .map(|record| record.number("generation"))
        .collect::<Result<Vec<_>, _>>()?;
    let generations: BTreeSet<_> = generation_rows.iter().copied().collect();
    if generations.len() as i128 != expected || generations.len() != generation_rows.len() {
        return Err("missing or duplicate physical generation boundary receipts".to_owned());
    }
    Ok((true, "complete declared live boundaries for every required physical generation; no historical lifecycle guarantee".to_owned()))
}

fn assertion(
    rows: &mut Vec<Assertion>,
    name: impl Into<String>,
    result: Result<(bool, String), String>,
) {
    let name = name.into();
    rows.push(match result {
        Ok((condition, detail)) => Assertion::check(name, condition, detail),
        Err(error) => Assertion::unavailable(name, error),
    });
}

fn status_field<'a>(bytes: &'a [u8], field: &str) -> Result<&'a str, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "status is not UTF-8")?;
    text.lines()
        .find_map(|line| {
            line.strip_prefix(field)
                .and_then(|line| line.strip_prefix(':'))
        })
        .map(str::trim)
        .ok_or_else(|| format!("status lacks {field}"))
}

pub fn check_isolation(
    guest: Result<&Observation, String>,
    native: Result<&Observation, String>,
    policy: NamespacePolicy,
    expected_cwd: &[u8],
) -> Vec<Assertion> {
    let mut rows = Vec::new();
    let mut required_processes: BTreeSet<String> = ["self", "pid1", "pid1_task/1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assertion(
        &mut rows,
        "processes.pid1_tasks.complete",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let tasks = guest.directory("pid1_tasks")?;
            if tasks.is_empty() {
                return Err("PID1 has no observed task".to_owned());
            }
            for task in tasks {
                let task = std::str::from_utf8(&task).map_err(|_| "non-ASCII PID1 task entry")?;
                let number: u64 = task.parse().map_err(|_| "nonnumeric PID1 task entry")?;
                if number == 0 || task != number.to_string() {
                    return Err("invalid PID1 task identity".to_owned());
                }
                required_processes.insert(format!("pid1_task/{number}"));
            }
            Ok((
                true,
                "every numeric PID1 task remains a required namespace and descriptor view"
                    .to_owned(),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "guest.complete",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            guest.one("SCHEMA", "backend-isolation-v1")?;
            guest.one("COMPLETE", "backend-isolation-v1")?;
            Ok((true, "complete typed guest report".to_owned()))
        })(),
    );
    assertion(
        &mut rows,
        "native.complete",
        (|| {
            let native = native.as_ref().map_err(Clone::clone)?;
            native_complete(native)
        })(),
    );
    for namespace in NAMESPACES {
        assertion(
            &mut rows,
            format!("namespace.{namespace}.host_separation"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let host = guest.object("HOST_NS", namespace)?;
                let inside = guest.object("STAT", &format!("self/ns/{namespace}"))?;
                let shared = inside == host;
                Ok((
                    shared == policy.shares(namespace),
                    format!(
                        "shared_with_immediate_host={shared}, raw_host={host:?}, raw_guest={inside:?}"
                    ),
                ))
            })(),
        );
        for process in ["pid1", "pid1_task/1"] {
            assertion(
                &mut rows,
                format!("namespace.{namespace}.{process}_alias"),
                (|| {
                    let guest = guest.as_ref().map_err(Clone::clone)?;
                    let own = guest.object("STAT", &format!("self/ns/{namespace}"))?;
                    let other = guest.object("STAT", &format!("{process}/ns/{namespace}"))?;
                    Ok((
                        own == other,
                        format!("raw_self={own:?}, raw_{process}={other:?}"),
                    ))
                })(),
            );
        }
        for process in required_processes.iter().filter(|process| {
            process.starts_with("pid1_task/") && process.as_str() != "pid1_task/1"
        }) {
            assertion(
                &mut rows,
                format!("namespace.{namespace}.{process}_alias"),
                (|| {
                    let guest = guest.as_ref().map_err(Clone::clone)?;
                    let own = guest.object("STAT", &format!("self/ns/{namespace}"))?;
                    let other = guest.object("STAT", &format!("{process}/ns/{namespace}"))?;
                    Ok((
                        own == other,
                        format!("raw_self={own:?}, raw_task={other:?}"),
                    ))
                })(),
            );
        }
        assertion(
            &mut rows,
            format!("namespace.{namespace}.native_guest"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let native = native.as_ref().map_err(Clone::clone)?;
                let inside = guest.object("STAT", &format!("self/ns/{namespace}"))?;
                let process = native_logical_app(guest, native)?;
                let observed = native.object("STAT", &format!("{}/ns/{namespace}", process.tag))?;
                Ok((
                    inside == observed,
                    format!("raw_guest={inside:?}, raw_bound_logical_app={observed:?}"),
                ))
            })(),
        );
    }
    assertion(
        &mut rows,
        "pid.private_init",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let status = guest.file("pid1/status")?;
            let pid = status_field(&status, "Pid")?;
            let parent = status_field(&status, "PPid")?;
            let name = status_field(&status, "Name")?;
            let isolated = pid == "1" && parent == "0" && name == "hermit";
            Ok((
                policy == NamespacePolicy::None || isolated,
                format!("PID1 name={name}, pid={pid}, parent={parent}"),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "pid.detcore_identity",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let identity = guest.one("IDENTITY", "self")?;
            let pid = identity.number("getpid")?;
            Ok((pid == 3, format!("Detcore getpid={pid}")))
        })(),
    );
    assertion(
        &mut rows,
        "hostname.kernel",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let hostname = guest.file("kernel_hostname")?;
            let expected = if policy == NamespacePolicy::None {
                native
                    .as_ref()
                    .map_err(Clone::clone)?
                    .file("host/kernel_hostname")?
            } else {
                b"hermetic-container.local\n".to_vec()
            };
            Ok((
                hostname == expected,
                "kernel hostname matches selected UTS policy".to_owned(),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "scratch.host_separation",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let tmp = guest.object("STAT", "tmp")?;
            let host = guest.object("HOST_TMP", "tmp")?;
            let shared = tmp == host;
            Ok((
                shared == (policy == NamespacePolicy::None),
                format!("shared_with_immediate_host={shared}, raw_host={host:?}, raw_tmp={tmp:?}"),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "scratch.complete_contents",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let entries = guest.directory("tmp")?;
            Ok((
                policy == NamespacePolicy::None || entries.is_empty(),
                format!("complete sorted entry count={}", entries.len()),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "cwd.logical_path",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let cwd = guest.one("CWD", "self")?;
            if cwd.number("rc")? != 0 {
                return Err(format!("getcwd failed errno={}", cwd.number("errno")?));
            }
            let path = decode_hex(cwd.field("bytes")?)?;
            Ok((
                path == expected_cwd,
                "logical working directory is the declared input".to_owned(),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "cwd.complete_contents",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let entries = guest.directory("cwd")?;
            Ok((
                entries.is_empty(),
                format!("complete sorted entry count={}", entries.len()),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "network.interfaces",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let interfaces = guest.directory("interfaces")?;
            if policy != NamespacePolicy::Private {
                let host = native
                    .as_ref()
                    .map_err(Clone::clone)?
                    .directory("host/interfaces")?;
                return Ok((
                    interfaces == host,
                    "complete interface names match explicit host network".to_owned(),
                ));
            }
            Ok((
                interfaces == [b"lo".to_vec()],
                format!("complete interface count={}", interfaces.len()),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "network.loopback_up",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let bytes = guest.file("lo_flags")?;
            let text = std::str::from_utf8(&bytes).map_err(|_| "loopback flags are not UTF-8")?;
            let flags = u64::from_str_radix(text.trim().trim_start_matches("0x"), 16)
                .map_err(|_| "invalid loopback flags")?;
            Ok((flags & 1 != 0, format!("loopback flags=0x{flags:x}")))
        })(),
    );
    for name in [
        "root", "tmp", "proc", "sys", "dev", "cwd", "shm", "devpts", "cgroup",
    ] {
        assertion(
            &mut rows,
            format!("filesystem.{name}.visible"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                guest.object("STAT", name)?;
                let statfs = guest.one("STATFS", name)?;
                Ok((
                    statfs.number("rc")? == 0,
                    format!("statfs errno={}", statfs.number("errno")?),
                ))
            })(),
        );
    }
    for name in ["sys", "cgroup"] {
        assertion(
            &mut rows,
            format!("filesystem.{name}.readonly"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let statfs = guest.one("STATFS", name)?;
                if statfs.number("rc")? < 0 {
                    return Err(format!("statfs {name} failed"));
                }
                let flags = u64::from_str_radix(statfs.field("flags")?, 16)
                    .map_err(|_| "invalid statfs flags")?;
                Ok((
                    policy == NamespacePolicy::None || flags & 15 == 15,
                    format!("statfs flags=0x{flags:x}"),
                ))
            })(),
        );
    }
    for name in ["shm", "cgroup"] {
        assertion(
            &mut rows,
            format!("filesystem.{name}.empty"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let entries = guest.directory(name)?;
                Ok((
                    policy == NamespacePolicy::None || entries.is_empty(),
                    format!("complete sorted entry count={}", entries.len()),
                ))
            })(),
        );
    }
    assertion(
        &mut rows,
        "filesystem.cgroup.tmpfs_mask",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let statfs = guest.one("STATFS", "cgroup")?;
            Ok((
                policy == NamespacePolicy::None || statfs.field("type")? == "1021994",
                format!("filesystem type={}", statfs.field("type")?),
            ))
        })(),
    );
    for name in ["mountinfo", "mounts", "net_dev", "net_route", "cgroup"] {
        assertion(
            &mut rows,
            format!("view.{name}.complete"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let bytes = guest.file(name)?;
                Ok((!bytes.is_empty(), format!("complete bytes={}", bytes.len())))
            })(),
        );
    }
    for name in ["ipc", "socket"] {
        assertion(
            &mut rows,
            format!("sentinel.{name}.reachability"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let record = guest.one("SENTINEL", name)?;
                let reachable = record.number("reachable")? == 1;
                if record.number("errno")? == 38 {
                    return Err(format!(
                        "{name} sentinel observation syscall returned ENOSYS; refusal is not namespace separation"
                    ));
                }
                let allowed = if name == "socket" {
                    policy != NamespacePolicy::Private
                } else {
                    policy == NamespacePolicy::None
                };
                Ok((
                    reachable == allowed,
                    format!("reachable={reachable}, errno={}", record.number("errno")?),
                ))
            })(),
        );
    }
    assertion(
        &mut rows,
        "descriptors.inherited_objects",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let mut leaked = Vec::new();
            let mut unavailable = Vec::new();
            let mut forbidden = Vec::new();
            for kind in ["HOST_DIRECTORY", "HOST_FILE", "HOST_SOCKET"] {
                match guest.object(kind, "sentinel") {
                    Ok(object) => forbidden.push(object),
                    Err(error) => unavailable.push(error),
                }
            }
            for process in &required_processes {
                let descriptors = match guest.descriptor_numbers(process) {
                    Ok(descriptors) => descriptors,
                    Err(error) => {
                        unavailable.push(error);
                        BTreeSet::new()
                    }
                };
                for fd in descriptors {
                    if let Err(error) = guest.descriptor(process, fd) {
                        unavailable.push(error);
                    }
                }
                let prefix = format!("{process}/");
                for record in guest.records.iter().filter(|record| record.kind == "STAT") {
                    let Some(suffix) = record.tag.strip_prefix(&prefix) else {
                        continue;
                    };
                    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                        continue;
                    }
                    match record.object() {
                        Ok(object) => {
                            if forbidden.contains(&object) {
                                leaked.push(record.tag.clone());
                            }
                        }
                        Err(error) => unavailable.push(error),
                    }
                }
            }
            if !leaked.is_empty() {
                return Ok((
                    false,
                    format!(
                        "original unsolicited objects reachable at {leaked:?}; unavailable={unavailable:?}"
                    ),
                ));
            }
            if !unavailable.is_empty() {
                return Err(format!(
                    "inherited-object inventory incomplete: {unavailable:?}"
                ));
            }
            Ok((
                true,
                "every enumerated descriptor object differs from the original unsolicited objects"
                    .to_owned(),
            ))
        })(),
    );
    assertion(
        &mut rows,
        "descriptors.proc_and_direct_capabilities",
        (|| {
            let guest = guest.as_ref().map_err(Clone::clone)?;
            let expected_marker = guest.object("HOST_FILE", "sentinel")?;
            let mut required = BTreeSet::new();
            let mut unavailable = Vec::new();
            for tag in ["self_fd42", "pid1_fd42", "pid1_thread_fd42"] {
                required.insert(("CAPABILITY", tag.to_owned()));
            }
            for process in &required_processes {
                let descriptors = match guest.descriptor_numbers(process) {
                    Ok(descriptors) => descriptors,
                    Err(error) => {
                        unavailable.push(error);
                        BTreeSet::new()
                    }
                };
                for fd in descriptors {
                    let tag = format!("{process}/{fd}");
                    required.insert(("CAPABILITY", tag.clone()));
                    if process == "self" {
                        required.insert(("FD_OPENAT", tag));
                    }
                }
            }
            let mut leaked = Vec::new();
            for (kind, tag) in &required {
                if let Err(error) = guest.one(kind, tag) {
                    unavailable.push(error);
                }
            }
            for record in &guest.records {
                if matches!(record.kind.as_str(), "CAPABILITY" | "FD_OPENAT") {
                    match record.marker_object() {
                        Ok(Some(object)) => {
                            if object == expected_marker {
                                leaked.push(format!("{} {}", record.kind, record.tag));
                            }
                        }
                        Ok(None) => {}
                        Err(error) => unavailable.push(error),
                    }
                }
            }
            if !leaked.is_empty() {
                return Ok((
                    false,
                    format!(
                        "controlled marker capability reachable at {leaked:?}; unavailable={unavailable:?}"
                    ),
                ));
            }
            if !unavailable.is_empty() {
                return Err(format!(
                    "capability observations incomplete: {unavailable:?}"
                ));
            }
            Ok((
                true,
                "every required direct/proc controlled-marker operation observed unreachable"
                    .to_owned(),
            ))
        })(),
    );
    if let Ok(guest) = guest.as_ref() {
        let mut capabilities: BTreeSet<_> = ["self_fd42", "pid1_fd42", "pid1_thread_fd42"]
            .into_iter()
            .map(|tag| ("CAPABILITY".to_owned(), tag.to_owned()))
            .collect();
        for record in &guest.records {
            if matches!(record.kind.as_str(), "CAPABILITY" | "FD_OPENAT") {
                capabilities.insert((record.kind.clone(), record.tag.clone()));
            }
        }
        for (kind, tag) in capabilities {
            assertion(
                &mut rows,
                format!("descriptors.capability.{kind}.{tag}"),
                (|| {
                    let expected = guest.object("HOST_FILE", "sentinel")?;
                    let object = guest.one(&kind, &tag)?.marker_object()?;
                    Ok((
                        object != Some(expected),
                        format!("opened_marker={object:?}, original_marker={expected:?}"),
                    ))
                })(),
            );
        }
    }
    for process in &required_processes {
        assertion(
            &mut rows,
            format!("descriptors.{process}.complete"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let descriptors = guest.descriptor_numbers(process)?;
                for fd in &descriptors {
                    guest.descriptor(process, *fd)?;
                }
                Ok((
                    true,
                    format!(
                        "complete numeric descriptor count={} with stat, link, full fdinfo, and capability rows",
                        descriptors.len()
                    ),
                ))
            })(),
        );
        for alias in ["cwd", "root"] {
            assertion(
                &mut rows,
                format!("descriptors.{process}.{alias}_alias"),
                (|| {
                    let guest = guest.as_ref().map_err(Clone::clone)?;
                    let object = guest.object("STAT", &format!("{process}/{alias}"))?;
                    let expected =
                        guest.object("STAT", if alias == "cwd" { "cwd" } else { "root" })?;
                    Ok((
                        object == expected,
                        format!("raw_alias={object:?}, raw_view={expected:?}"),
                    ))
                })(),
            );
        }
    }
    if let Ok(guest) = guest.as_ref() {
        for process in &required_processes {
            let numbers = guest
                .records
                .iter()
                .filter(|record| record.kind == "FD_ENTRY" && record.tag == *process)
                .map(|record| record.number("fd"))
                .collect::<Result<BTreeSet<_>, _>>();
            match numbers {
                Ok(numbers) => {
                    for fd in numbers {
                        assertion(
                            &mut rows,
                            format!("descriptors.{process}.fd{fd}.complete"),
                            (|| {
                                let object = guest.descriptor(process, fd)?;
                                Ok((
                                    true,
                                    format!(
                                        "literal fd={fd}, complete object={object:?}; flags, target, fdinfo and capability retained"
                                    ),
                                ))
                            })(),
                        );
                    }
                }
                Err(error) => rows.push(Assertion::unavailable(
                    format!("descriptors.{process}.numeric_entries"),
                    error,
                )),
            }
        }
    }
    rows
}

fn json_string(value: &str) -> String {
    let mut encoded = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '"' => encoded.push_str("\\\""),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            control if control < ' ' => {
                write!(encoded, "\\u{:04x}", control as u32).expect("String writes succeed");
            }
            other => encoded.push(other),
        }
    }
    encoded.push('"');
    encoded
}

pub fn write_report(path: &Path, cell: &str, rows: &[Assertion]) -> Result<(), String> {
    let mut report = format!(
        "{{\n  \"schema\": \"backend-isolation-v1\",\n  \"cell\": {},\n  \"assertions\": [\n",
        json_string(cell)
    );
    for (index, row) in rows.iter().enumerate() {
        writeln!(
            report,
            "    {{\"name\": {}, \"verdict\": {}, \"detail\": {}}}{}",
            json_string(&row.name),
            json_string(row.verdict.name()),
            json_string(&row.detail),
            if index + 1 == rows.len() { "" } else { "," }
        )
        .expect("String writes succeed");
    }
    report.push_str("  ]\n}\n");
    fs::write(path, report).map_err(|error| format!("typed report write: {error}"))
}

pub fn all_match(rows: &[Assertion]) -> bool {
    !rows.is_empty() && rows.iter().all(|row| row.verdict == Verdict::Match)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_record(kind: &str, tag: &str, bytes: &[u8]) -> String {
        let mut encoded = String::new();
        for byte in bytes {
            write!(encoded, "{byte:02x}").unwrap();
        }
        format!(
            "{kind} {tag} rc={} errno=0 complete=1 bytes={encoded}\n",
            bytes.len()
        )
    }

    fn descriptor_fixture() -> String {
        let mut report = String::from(
            "FD_DIRECTORY self rc=3 errno=0\nFD_ENTRY self fd=42\nFD_SCAN_COMPLETE self rc=0 errno=0\nSTAT self/42 rc=0 errno=0 dev=17 ino=22 mode=100644 nlink=1\nLINK self/42 rc=1 errno=0 complete=1 bytes=78\nFD_FLAGS self/42 flags=0 flags_errno=0 mode=0 mode_errno=0\nFD_OPENAT self/42 reachable=0 errno=20\nCAPABILITY self/42 reachable=0 errno=20\n",
        );
        report.push_str(&bytes_record(
            "FILE",
            "self/42",
            b"pos:\t0\nflags:\t00\nmnt_id:\t7\nino:\t22\n",
        ));
        report
    }

    #[test]
    fn descriptor_inventory_requires_every_numeric_entry() {
        let source = descriptor_fixture();
        let complete = Observation::parse(source.as_bytes()).unwrap();
        assert_eq!(
            complete.descriptor_numbers("self").unwrap(),
            BTreeSet::from([42])
        );
        assert!(complete.descriptor("self", 42).is_ok());
        let missing = source.replace("FD_ENTRY self fd=42\n", "");
        assert!(
            Observation::parse(missing.as_bytes())
                .unwrap()
                .descriptor_numbers("self")
                .is_err()
        );
    }

    #[test]
    fn descriptor_stat_failure_is_unavailable() {
        let source = descriptor_fixture().replace(
            "STAT self/42 rc=0 errno=0 dev=17 ino=22 mode=100644 nlink=1",
            "STAT self/42 rc=-1 errno=13 dev=0 ino=0 mode=0 nlink=0",
        );
        let observation = Observation::parse(source.as_bytes()).unwrap();
        assert!(observation.descriptor("self", 42).is_err());
        let rows = check_isolation(
            Ok(&observation),
            Err("no native proof".to_owned()),
            NamespacePolicy::Private,
            b"/fixture",
        );
        assert!(
            rows.iter()
                .any(|row| row.name == "descriptors.self.fd42.complete"
                    && row.verdict == Verdict::NoResult)
        );
    }

    #[test]
    fn descriptor_inventory_requires_full_fdinfo() {
        let source = descriptor_fixture();
        let source = source
            .lines()
            .filter(|line| !line.starts_with("FILE self/42 "))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert!(
            Observation::parse(source.as_bytes())
                .unwrap()
                .descriptor("self", 42)
                .is_err()
        );
    }

    #[test]
    fn known_inherited_object_leak_survives_incomplete_fdinfo() {
        let mut source = descriptor_fixture()
            .lines()
            .filter(|line| !line.starts_with("FILE self/42 "))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        source.push_str("HOST_DIRECTORY sentinel dev=17 ino=22\nHOST_FILE sentinel dev=17 ino=23\nHOST_SOCKET sentinel dev=17 ino=24\nCAPABILITY self_fd42 reachable=1 errno=0 stat_rc=0 stat_errno=0 dev=17 ino=23\n");
        let guest = Observation::parse(source.as_bytes()).unwrap();
        let rows = check_isolation(
            Ok(&guest),
            Err("no native proof".to_owned()),
            NamespacePolicy::Private,
            b"/fixture",
        );
        assert!(
            rows.iter()
                .any(|row| row.name == "descriptors.inherited_objects"
                    && row.verdict == Verdict::Mismatch)
        );
        assert!(
            rows.iter()
                .any(|row| row.name == "descriptors.proc_and_direct_capabilities"
                    && row.verdict == Verdict::Mismatch)
        );
        assert!(
            rows.iter()
                .any(|row| row.name == "descriptors.self.fd42.complete"
                    && row.verdict == Verdict::NoResult)
        );
    }

    #[test]
    fn known_direct_and_proc_leaks_survive_unavailable_other_inventory() {
        let source = b"HOST_FILE sentinel dev=17 ino=23\nFD_DIRECTORY pid1 rc=-1 errno=13\nCAPABILITY pid1_fd42 reachable=invalid errno=0\nCAPABILITY self_fd42 reachable=1 errno=0 stat_rc=0 stat_errno=0 dev=17 ino=23\nFD_OPENAT self/42 reachable=1 errno=0 stat_rc=0 stat_errno=0 dev=17 ino=23\n";
        let guest = Observation::parse(source).unwrap();
        let rows = check_isolation(
            Ok(&guest),
            Err("no native proof".to_owned()),
            NamespacePolicy::Private,
            b"/fixture",
        );
        assert!(
            rows.iter()
                .any(|row| row.name == "descriptors.proc_and_direct_capabilities"
                    && row.verdict == Verdict::Mismatch)
        );
        assert!(rows.iter().any(
            |row| row.name == "descriptors.capability.CAPABILITY.self_fd42"
                && row.verdict == Verdict::Mismatch
        ));
        assert!(
            rows.iter()
                .any(|row| row.name == "descriptors.capability.FD_OPENAT.self/42"
                    && row.verdict == Verdict::Mismatch)
        );
        assert!(rows.iter().any(
            |row| row.name == "descriptors.capability.CAPABILITY.pid1_fd42"
                && row.verdict == Verdict::NoResult
        ));
        assert!(
            rows.iter()
                .any(|row| row.name == "descriptors.pid1.complete"
                    && row.verdict == Verdict::NoResult)
        );
    }

    #[test]
    fn matching_unbound_role_does_not_authenticate_a_guest() {
        let guest = Observation::parse(b"STAT self/ns/pid rc=0 errno=0 dev=5 ino=77\n").unwrap();
        let native = Observation::parse(b"ROLE unrelated kind=guest comm=6775657374\nSTAT unrelated/ns/pid rc=0 errno=0 dev=5 ino=77\n").unwrap();
        let rows = check_isolation(
            Ok(&guest),
            Ok(&native),
            NamespacePolicy::Private,
            b"/fixture",
        );
        assert!(rows.iter().any(
            |row| row.name == "namespace.pid.native_guest" && row.verdict == Verdict::NoResult
        ));
    }

    #[test]
    fn claimed_role_with_wrong_executable_object_is_unavailable() {
        let guest = Observation::parse(b"SCHEMA backend-isolation-v1 metadata=raw\n").unwrap();
        let native = Observation::parse(b"BOUND_ROLE logical_app proof=exec-object process=p pid=10 start=20\nPROCESS p pid=10 parent=1 start=20 exe=78\nPROCESS_COMPLETE p\nEXPECTED_OBJECT logical_app dev=17 ino=22\nSTAT p/exe rc=0 errno=0 dev=17 ino=99\n").unwrap();
        assert!(native_logical_app(&guest, &native).is_err());
    }

    #[test]
    fn missing_physical_generation_is_unavailable() {
        let native = Observation::parse(b"NATIVE_SCHEMA backend-isolation-physical-v1\nNATIVE_COMPLETE backend-isolation-physical-v1 errors=0 live=0 expected_generations=2 observed_generations=1 live_boundary_complete=1\nGENERATION_COMPLETE one generation=1\n").unwrap();
        assert!(native_complete(&native).is_err());
    }

    #[test]
    fn duplicate_physical_generation_is_unavailable() {
        let native = Observation::parse(b"NATIVE_SCHEMA backend-isolation-physical-v1\nNATIVE_COMPLETE backend-isolation-physical-v1 errors=0 live=0 expected_generations=1 observed_generations=1 live_boundary_complete=1\nGENERATION_COMPLETE one generation=1\nGENERATION_COMPLETE duplicate generation=1\n").unwrap();
        assert!(native_complete(&native).is_err());
    }

    #[test]
    fn truncated_record_or_inconsistent_byte_count_is_unavailable() {
        assert!(Observation::parse(b"NATIVE_COMPLETE backend-isolation-physical-v1").is_err());
        let observation =
            Observation::parse(b"FILE value rc=2 errno=0 complete=1 bytes=78\n").unwrap();
        assert!(observation.file("value").is_err());
    }
}
