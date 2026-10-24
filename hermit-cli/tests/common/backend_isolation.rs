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

pub const DESCRIPTOR_REQUEST_LIMIT: usize = 4096;
pub const DESCRIPTOR_ARGUMENT_LIMIT: usize = 65_536;

fn descriptor_decimal(value: &str, positive: bool) -> Result<u32, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid descriptor identity {value:?}"));
    }
    let number: u32 = value.parse().map_err(|_| "descriptor identity overflow")?;
    if number > i32::MAX as u32 || (positive && number == 0) || value != number.to_string() {
        return Err(format!("noncanonical descriptor identity {value:?}"));
    }
    Ok(number)
}

fn descriptor_role(role: &str) -> Result<(), String> {
    if matches!(role, "self" | "pid1") {
        return Ok(());
    }
    let tid = role
        .strip_prefix("pid1_task/")
        .ok_or("unknown descriptor role")?;
    descriptor_decimal(tid, true)?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DescriptorKey {
    role: String,
    fd: u32,
}

impl DescriptorKey {
    pub fn parse(value: &str) -> Result<Self, String> {
        let (role, fd) = value.rsplit_once('/').ok_or("descriptor key lacks an FD")?;
        descriptor_role(role)?;
        Ok(Self {
            role: role.to_owned(),
            fd: descriptor_decimal(fd, false)?,
        })
    }

    pub fn role(&self) -> &str {
        &self.role
    }
    pub fn fd(&self) -> u32 {
        self.fd
    }
    pub fn name(&self) -> String {
        format!("{}/{}", self.role, self.fd)
    }
}

/* Host-held requirements come from authenticated complete capture bytes. A
 * captured unavailable view is retained as unavailable, not an empty proof. */
#[derive(Clone, Debug)]
pub struct DescriptorRequirements {
    keys: BTreeSet<DescriptorKey>,
    roles: BTreeSet<String>,
    complete_views: BTreeSet<String>,
    tasks_complete: bool,
    unavailable: Vec<String>,
}

impl DescriptorRequirements {
    pub fn from_prior(prior: &Observation) -> Result<Self, String> {
        let mut roles: BTreeSet<String> = ["self", "pid1", "pid1_task/1"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        for record in &prior.records {
            if matches!(
                record.kind.as_str(),
                "FD_DIRECTORY" | "FD_ENTRY" | "FD_SCAN_COMPLETE"
            ) {
                descriptor_role(&record.tag)?;
                roles.insert(record.tag.clone());
            }
            if record.kind == "ENTRY" && record.tag == "pid1_tasks" {
                let bytes = decode_hex(record.field("bytes")?)?;
                let task = std::str::from_utf8(&bytes).map_err(|_| "non-ASCII prior task")?;
                descriptor_decimal(task, true)?;
                roles.insert(format!("pid1_task/{task}"));
            }
        }
        let mut keys = BTreeSet::new();
        for entry in prior
            .records
            .iter()
            .filter(|record| record.kind == "FD_ENTRY")
        {
            if entry.fields.len() != 1 {
                return Err("prior FD_ENTRY has unknown fields".to_owned());
            }
            let key = DescriptorKey::parse(&format!("{}/{}", entry.tag, entry.field("fd")?))?;
            if !keys.insert(key) {
                return Err("repeated prior FD_ENTRY".to_owned());
            }
        }
        let mut complete_views = BTreeSet::new();
        let mut unavailable = Vec::new();
        let tasks_complete = match descriptor_tasks(prior) {
            Ok(_) => true,
            Err(error) => {
                unavailable.push(error);
                false
            }
        };
        for role in &roles {
            match prior.descriptor_inventory(role) {
                Ok(_) => {
                    complete_views.insert(role.clone());
                }
                Err(error) => unavailable.push(error),
            }
        }
        let requirements = Self {
            keys,
            roles,
            complete_views,
            tasks_complete,
            unavailable,
        };
        requirements.arguments()?;
        Ok(requirements)
    }

    pub fn keys(&self) -> &BTreeSet<DescriptorKey> {
        &self.keys
    }
    pub fn roles(&self) -> &BTreeSet<String> {
        &self.roles
    }
    pub fn tasks_complete(&self) -> bool {
        self.tasks_complete
    }
    pub fn view_complete(&self, role: &str) -> bool {
        self.complete_views.contains(role)
    }

    pub fn arguments(&self) -> Result<Vec<String>, String> {
        let mut keys = self.keys.clone();
        // A controlled key also carries a prior unavailable role with no keys.
        // It is a real operation request, never a fabricated FD_ENTRY.
        for role in &self.roles {
            for fd in 42..=44 {
                keys.insert(DescriptorKey {
                    role: role.clone(),
                    fd,
                });
            }
        }
        let arguments: Vec<_> = keys.into_iter().map(|key| key.name()).collect();
        if arguments.len() > DESCRIPTOR_REQUEST_LIMIT
            || arguments.iter().map(|value| value.len() + 1).sum::<usize>()
                > DESCRIPTOR_ARGUMENT_LIMIT
        {
            return Err("complete descriptor request set exceeds the guest bound".to_owned());
        }
        Ok(arguments)
    }
}

fn descriptor_tasks(guest: &Observation) -> Result<BTreeSet<String>, String> {
    let directory = guest.one("DIRECTORY", "pid1_tasks")?;
    if directory.number("errno")? != 0 {
        return Err("PID1 task directory is unavailable".to_owned());
    }
    let tasks = guest.directory("pid1_tasks")?;
    if tasks.is_empty() {
        return Err("PID1 has no observed task".to_owned());
    }
    let roles: BTreeSet<_> = tasks
        .into_iter()
        .map(|bytes| {
            let value = std::str::from_utf8(&bytes).map_err(|_| "non-ASCII PID1 task")?;
            descriptor_decimal(value, true)?;
            Ok(format!("pid1_task/{value}"))
        })
        .collect::<Result<_, String>>()?;
    if !roles.contains("pid1_task/1") {
        return Err("required PID1 leader task is absent from the current list".to_owned());
    }
    Ok(roles)
}

fn descriptor_metadata_keys(
    guest: Option<&Observation>,
    role: &str,
    requirements: Option<&DescriptorRequirements>,
) -> Result<BTreeSet<i128>, String> {
    let mut keys: BTreeSet<_> = requirements
        .into_iter()
        .flat_map(|required| required.keys.iter())
        .filter(|key| key.role == role)
        .map(|key| i128::from(key.fd))
        .collect();
    if let Some(guest) = guest {
        for record in guest
            .records
            .iter()
            .filter(|record| record.kind == "FD_ENTRY" && record.tag == role)
        {
            keys.insert(record.number("fd")?);
        }
    }
    Ok(keys)
}

fn descriptor_capabilities(
    guest: Option<&Observation>,
    roles: &BTreeSet<String>,
    requirements: Option<&DescriptorRequirements>,
) -> BTreeSet<(String, String)> {
    let mut required: BTreeSet<_> = ["self_fd42", "pid1_fd42", "pid1_thread_fd42"]
        .into_iter()
        .map(|tag| ("CAPABILITY".to_owned(), tag.to_owned()))
        .collect();
    for role in roles {
        let mut keys: BTreeSet<_> = (42..=44).map(i128::from).collect();
        // A malformed current entry cannot erase validated host-held names.
        // Its numeric-view error is still retained by the checker below.
        keys.extend(
            descriptor_metadata_keys(None, role, requirements)
                .expect("host requirements are validated"),
        );
        if let Some(guest) = guest {
            for record in guest
                .records
                .iter()
                .filter(|record| record.kind == "FD_ENTRY" && record.tag == *role)
            {
                if let Ok(fd) = record.number("fd") {
                    keys.insert(fd);
                }
            }
        }
        for fd in keys {
            let tag = format!("{role}/{fd}");
            required.insert(("CAPABILITY".to_owned(), tag.clone()));
            if role == "self" {
                required.insert(("FD_OPENAT".to_owned(), tag));
            }
        }
    }
    if let Some(guest) = guest {
        for record in &guest.records {
            if matches!(record.kind.as_str(), "CAPABILITY" | "FD_OPENAT") {
                required.insert((record.kind.clone(), record.tag.clone()));
            }
        }
    }
    required
}

fn unsigned_token(value: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid unsigned object token {value:?}"));
    }
    value
        .parse()
        .map_err(|_| format!("unsigned object token overflows u64: {value:?}"))
}

fn original_descriptor_objects(guest: &Observation) -> Result<BTreeSet<(u64, u64)>, String> {
    let mut objects = BTreeSet::new();
    for kind in ["HOST_DIRECTORY", "HOST_FILE", "HOST_SOCKET"] {
        let record = guest.one(kind, "sentinel")?;
        if record.fields.len() != 2 {
            return Err(format!("{kind} has unknown or missing identity fields"));
        }
        objects.insert((
            unsigned_token(record.field("dev")?)?,
            unsigned_token(record.field("ino")?)?,
        ));
    }
    Ok(objects)
}

struct DescriptorObject {
    object: Option<(u64, u64)>,
    error: u32,
    mode: u32,
}

fn descriptor_object_record(record: &Record) -> Result<DescriptorObject, String> {
    if record.fields.len() != 5 {
        return Err(format!(
            "{} {} has unknown or missing fields",
            record.kind, record.tag
        ));
    }
    let rc = record.field("rc")?;
    let error = unsigned_token(record.field("errno")?)?;
    let error: u32 = error.try_into().map_err(|_| "object errno exceeds u32")?;
    let object = (
        unsigned_token(record.field("dev")?)?,
        unsigned_token(record.field("ino")?)?,
    );
    let mode = record.field("mode")?;
    if mode.is_empty() || !mode.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
        return Err("object mode is not unsigned octal".to_owned());
    }
    let mode = u32::from_str_radix(mode, 8).map_err(|_| "object mode overflow")?;
    match (rc, error) {
        ("0", 0) => Ok(DescriptorObject {
            object: Some(object),
            error,
            mode,
        }),
        ("-1", error) if error > 0 => Ok(DescriptorObject {
            object: None,
            error,
            mode,
        }),
        _ => Err(format!(
            "{} {} has inconsistent return/errno",
            record.kind, record.tag
        )),
    }
}

fn controlled_object(
    guest: &Observation,
    kind: &str,
    role: &str,
    fd: u32,
) -> Result<(bool, String), String> {
    let forbidden = original_descriptor_objects(guest)?;
    let tag = format!("{role}/{fd}");
    let observed = descriptor_object_record(guest.one(kind, &tag)?)?;
    if let Some(object) = observed.object {
        return Ok((
            !forbidden.contains(&object),
            format!(
                "raw route/time object={object:?}; forbidden originals={forbidden:?}; a different object is not descriptor closure or whole-view authority"
            ),
        ));
    }
    if kind == "FD_SELF_OBJECT" && observed.error == 9 {
        return Ok((true, "actual direct fstat returned EBADF at this slot/time; old marker and metadata predicates are unchanged".to_owned()));
    }
    if kind == "FD_PROC_OBJECT" && observed.error == 2 {
        if role.starts_with("pid1_task/") && !descriptor_tasks(guest)?.contains(role) {
            return Err(format!("{role} is not in the complete current task list"));
        }
        let parent = descriptor_object_record(guest.one("FD_PARENT", role)?)?;
        if parent.object.is_none() || parent.mode & 0o170000 != 0o040000 {
            return Err(format!(
                "{role} FD parent is unavailable or not a directory"
            ));
        }
        let numbers = guest.descriptor_inventory(role)?;
        if numbers.contains(&i128::from(fd)) {
            return Err(format!(
                "{tag} ENOENT conflicts with the current numeric view"
            ));
        }
        return Ok((
            true,
            format!(
                "actual proc stat ENOENT plus separately observed same-role FD parent and complete numeric exclusion for {tag}; no atomic generation/native custody claim"
            ),
        ));
    }
    Err(format!("{kind} {tag} unavailable errno={}", observed.error))
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

    fn descriptor_inventory(&self, role: &str) -> Result<BTreeSet<i128>, String> {
        descriptor_role(role)?;
        let numbers = self.descriptor_numbers(role)?;
        if self.one("FD_DIRECTORY", role)?.number("errno")? != 0
            || self.one("FD_SCAN_COMPLETE", role)?.number("errno")? != 0
        {
            return Err(format!("{role} inventory/scan has an error"));
        }
        for entry in self
            .records
            .iter()
            .filter(|record| record.kind == "FD_ENTRY" && record.tag == role)
        {
            descriptor_decimal(entry.field("fd")?, false)?;
        }
        Ok(numbers)
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
    check_isolation_with_descriptor_requirements(guest, native, policy, expected_cwd, None)
}

pub fn check_isolation_with_descriptor_requirements(
    guest: Result<&Observation, String>,
    native: Result<&Observation, String>,
    policy: NamespacePolicy,
    expected_cwd: &[u8],
    requirements: Option<&DescriptorRequirements>,
) -> Vec<Assertion> {
    let mut rows = Vec::new();
    let mut required_processes: BTreeSet<String> = ["self", "pid1", "pid1_task/1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if let Some(required) = requirements {
        required_processes.extend(required.roles.iter().cloned());
    }
    // This separate state prevents an incomplete task list from making a
    // subset-wide descriptor aggregate Match; known leaks still win below.
    let current_tasks = guest
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|guest| descriptor_tasks(guest));
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
            if let Err(error) = original_descriptor_objects(guest) {
                unavailable.push(error);
            }
            if let Err(error) = &current_tasks {
                unavailable.push(error.clone());
            }
            if let Some(required) = requirements {
                unavailable.extend(required.unavailable.iter().cloned());
            }
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
                let metadata = match descriptor_metadata_keys(Some(guest), process, requirements) {
                    Ok(keys) => keys,
                    Err(error) => {
                        unavailable.push(error);
                        descriptors
                    }
                };
                for fd in metadata {
                    if let Err(error) = guest.descriptor(process, fd) {
                        unavailable.push(error);
                    }
                }
                for fd in 42..=44 {
                    for kind in if process == "self" {
                        &["FD_PROC_OBJECT", "FD_SELF_OBJECT"][..]
                    } else {
                        &["FD_PROC_OBJECT"][..]
                    } {
                        match controlled_object(guest, kind, process, fd) {
                            Ok((false, _)) => leaked.push(format!("{kind} {process}/{fd}")),
                            Ok((true, _)) => {}
                            Err(error) => unavailable.push(error),
                        }
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
            let required = descriptor_capabilities(Some(guest), &required_processes, requirements);
            let mut unavailable = Vec::new();
            if let Err(error) = &current_tasks {
                unavailable.push(error.clone());
            }
            if let Some(required) = requirements {
                unavailable.extend(required.unavailable.iter().cloned());
            }
            for process in &required_processes {
                if let Err(error) = guest.descriptor_numbers(process) {
                    unavailable.push(error);
                }
                if let Err(error) = descriptor_metadata_keys(Some(guest), process, requirements) {
                    unavailable.push(error);
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
    let capabilities = descriptor_capabilities(
        guest.as_ref().ok().copied(),
        &required_processes,
        requirements,
    );
    for (kind, tag) in capabilities {
        assertion(
            &mut rows,
            format!("descriptors.capability.{kind}.{tag}"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let expected = guest.object("HOST_FILE", "sentinel")?;
                let object = guest.one(&kind, &tag)?.marker_object()?;
                Ok((
                    object != Some(expected),
                    format!("opened_marker={object:?}, original_marker={expected:?}"),
                ))
            })(),
        );
    }
    for process in &required_processes {
        for fd in 42..=44 {
            for kind in if process == "self" {
                &["FD_PROC_OBJECT", "FD_SELF_OBJECT"][..]
            } else {
                &["FD_PROC_OBJECT"][..]
            } {
                assertion(
                    &mut rows,
                    format!("descriptors.original_objects.{kind}.{process}/{fd}"),
                    guest
                        .as_ref()
                        .map_err(Clone::clone)
                        .and_then(|guest| controlled_object(guest, kind, process, fd)),
                );
            }
        }
    }
    for process in &required_processes {
        assertion(
            &mut rows,
            format!("descriptors.{process}.complete"),
            (|| {
                let guest = guest.as_ref().map_err(Clone::clone)?;
                let descriptors = guest.descriptor_numbers(process)?;
                let required = descriptor_metadata_keys(Some(guest), process, requirements)?;
                for fd in &required {
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
    for process in &required_processes {
        let keys = descriptor_metadata_keys(guest.as_ref().ok().copied(), process, requirements);
        let numbers = match keys {
            Ok(keys) => keys,
            Err(error) => {
                rows.push(Assertion::unavailable(
                    format!("descriptors.{process}.numeric_entries"),
                    error,
                ));
                descriptor_metadata_keys(None, process, requirements)
                    .expect("host requirements are validated")
            }
        };
        for fd in numbers {
            assertion(
                &mut rows,
                format!("descriptors.{process}.fd{fd}.complete"),
                (|| {
                    let guest = guest.as_ref().map_err(Clone::clone)?;
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
    fn controlled_fixture(probe: &str) -> String {
        format!(
            "HOST_DIRECTORY sentinel dev=17 ino=22\nHOST_FILE sentinel dev=17 ino=23\nHOST_SOCKET sentinel dev=17 ino=24\n{probe}"
        )
    }

    #[test]
    fn closed_or_reused_controlled_slot_has_only_scoped_object_evidence() {
        for probe in [
            "FD_SELF_OBJECT self/42 rc=-1 errno=9 dev=0 ino=0 mode=0\n",
            "FD_SELF_OBJECT self/42 rc=0 errno=0 dev=19 ino=20 mode=100644\n",
        ] {
            let guest = Observation::parse(controlled_fixture(probe).as_bytes()).unwrap();
            assert!(
                controlled_object(&guest, "FD_SELF_OBJECT", "self", 42)
                    .unwrap()
                    .0
            );
        }
        let old = Observation::parse(
            b"FD_OPENAT self/42 reachable=0 errno=9 stat_rc=-1 stat_errno=9 dev=0 ino=0\n",
        )
        .unwrap();
        assert!(
            old.one("FD_OPENAT", "self/42")
                .unwrap()
                .marker_object()
                .is_err()
        );
    }

    #[test]
    fn controlled_slot_swap_cannot_hide_any_original_object() {
        for ino in [22, 23, 24] {
            let source = controlled_fixture(&format!(
                "FD_SELF_OBJECT self/43 rc=0 errno=0 dev=17 ino={ino} mode=100644\n",
            ));
            let guest = Observation::parse(source.as_bytes()).unwrap();
            assert!(
                !controlled_object(&guest, "FD_SELF_OBJECT", "self", 43)
                    .unwrap()
                    .0
            );
            let rows = check_isolation(
                Ok(&guest),
                Err("no native authority".to_owned()),
                NamespacePolicy::Private,
                b"/fixture",
            );
            assert!(
                rows.iter()
                    .any(|row| row.name == "descriptors.inherited_objects"
                        && row.verdict == Verdict::Mismatch)
            );
        }
    }

    #[test]
    fn controlled_object_requires_unique_unsigned_references_and_actual_probe() {
        let source =
            controlled_fixture("FD_SELF_OBJECT self/42 rc=0 errno=0 dev=19 ino=20 mode=100644\n");
        for changed in [
            source.replace(
                "HOST_DIRECTORY sentinel dev=17",
                "HOST_DIRECTORY sentinel dev=-17",
            ),
            source.replace(
                "HOST_SOCKET sentinel dev=17",
                "HOST_SOCKET sentinel dev=18446744073709551616",
            ),
            format!("{source}HOST_FILE sentinel dev=17 ino=23\n"),
            source.replace(
                "FD_SELF_OBJECT self/42 rc=0",
                "FD_SELF_OBJECT self/42 rc=-1",
            ),
            source.replace(
                "FD_SELF_OBJECT self/42 rc=0 errno=0 dev=19 ino=20 mode=100644\n",
                "",
            ),
            format!("{source}FD_SELF_OBJECT self/42 rc=0 errno=0 dev=19 ino=20 mode=100644\n"),
        ] {
            let guest = Observation::parse(changed.as_bytes()).unwrap();
            assert!(controlled_object(&guest, "FD_SELF_OBJECT", "self", 42).is_err());
        }
        assert!(
            Observation::parse(
                source
                    .replace("dev=19 ino=20", "dev=19 dev=17 ino=20")
                    .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn proc_enoent_requires_the_same_role_parent_and_complete_numeric_view() {
        let source = controlled_fixture(
            "FD_PROC_OBJECT self/42 rc=-1 errno=2 dev=0 ino=0 mode=0\nFD_PARENT self rc=0 errno=0 dev=3 ino=4 mode=40500\nFD_DIRECTORY self rc=3 errno=0\nFD_ENTRY self fd=3\nFD_SCAN_COMPLETE self rc=0 errno=0\n",
        );
        let guest = Observation::parse(source.as_bytes()).unwrap();
        assert!(
            controlled_object(&guest, "FD_PROC_OBJECT", "self", 42)
                .unwrap()
                .0
        );
        for changed in [
            source.replace("FD_PARENT self rc=0 errno=0 dev=3 ino=4 mode=40500\n", ""),
            source.replace(
                "FD_PARENT self rc=0 errno=0",
                "FD_PARENT self rc=-1 errno=13",
            ),
            source.replace("FD_ENTRY self fd=3\n", ""),
            source.replace("FD_ENTRY self fd=3", "FD_ENTRY self fd=42"),
            source.replace(
                "FD_SCAN_COMPLETE self rc=0 errno=0",
                "FD_SCAN_COMPLETE self rc=-1 errno=9",
            ),
        ] {
            let guest = Observation::parse(changed.as_bytes()).unwrap();
            assert!(controlled_object(&guest, "FD_PROC_OBJECT", "self", 42).is_err());
        }
        let task = source.replace("self", "pid1_task/4");
        let guest = Observation::parse(
            format!("{task}DIRECTORY pid1_tasks rc=3 errno=0\nENTRY pid1_tasks bytes=31\n")
                .as_bytes(),
        )
        .unwrap();
        assert!(controlled_object(&guest, "FD_PROC_OBJECT", "pid1_task/4", 42).is_err());
    }

    #[test]
    fn incomplete_or_duplicate_task_list_cannot_shrink_descriptor_aggregate() {
        for tasks in [
            "DIRECTORY pid1_tasks rc=-1 errno=13\n",
            "DIRECTORY pid1_tasks rc=4 errno=0\nENTRY pid1_tasks bytes=31\nENTRY pid1_tasks bytes=31\n",
            "DIRECTORY pid1_tasks rc=4 errno=0\nENTRY pid1_tasks bytes=31\n",
        ] {
            let guest = Observation::parse(tasks.as_bytes()).unwrap();
            let prior = DescriptorRequirements::from_prior(&guest).unwrap();
            assert!(!prior.tasks_complete());
            assert!(prior.roles().contains("pid1_task/1"));
            let rows = check_isolation_with_descriptor_requirements(
                Ok(&guest),
                Err("no native authority".to_owned()),
                NamespacePolicy::Private,
                b"/fixture",
                Some(&prior),
            );
            for name in [
                "descriptors.inherited_objects",
                "descriptors.proc_and_direct_capabilities",
            ] {
                assert!(
                    rows.iter()
                        .any(|row| row.name == name && row.verdict == Verdict::NoResult)
                );
            }
        }
    }

    #[test]
    fn vanished_prior_key_keeps_original_named_operations_and_errors() {
        let before = Observation::parse(descriptor_fixture().as_bytes()).unwrap();
        let required = DescriptorRequirements::from_prior(&before).unwrap();
        let after = controlled_fixture(
            "FD_DIRECTORY self rc=2 errno=0\nFD_SCAN_COMPLETE self rc=0 errno=0\nSTAT self/42 rc=-1 errno=2 dev=0 ino=0 mode=0 nlink=0\nLINK self/42 rc=-1 errno=2 complete=1 bytes=\nFILE self/42 rc=-1 errno=2 complete=0 bytes=\nFD_FLAGS self/42 flags=-1 flags_errno=9 mode=-1 mode_errno=9\nFD_OPENAT self/42 reachable=0 errno=9 stat_rc=-1 stat_errno=9 dev=0 ino=0\nCAPABILITY self/42 reachable=0 errno=2 stat_rc=-1 stat_errno=2 dev=0 ino=0\nFD_SELF_OBJECT self/42 rc=-1 errno=9 dev=0 ino=0 mode=0\n",
        );
        let guest = Observation::parse(after.as_bytes()).unwrap();
        let rows = check_isolation_with_descriptor_requirements(
            Ok(&guest),
            Err("no native authority".to_owned()),
            NamespacePolicy::Private,
            b"/fixture",
            Some(&required),
        );
        for (name, expected) in [
            ("descriptors.self.fd42.complete", Verdict::NoResult),
            ("descriptors.capability.CAPABILITY.self/42", Verdict::Match),
            (
                "descriptors.capability.FD_OPENAT.self/42",
                Verdict::NoResult,
            ),
            (
                "descriptors.original_objects.FD_SELF_OBJECT.self/42",
                Verdict::Match,
            ),
        ] {
            let actual: Vec<_> = rows.iter().filter(|row| row.name == name).collect();
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].verdict, expected);
        }
        assert!(
            required
                .arguments()
                .unwrap()
                .contains(&"pid1_task/1/42".to_owned())
        );
    }

    #[test]
    fn malformed_current_entries_do_not_remove_prior_required_names() {
        let before = Observation::parse(b"FD_DIRECTORY self rc=3 errno=0\nFD_ENTRY self fd=5\nFD_SCAN_COMPLETE self rc=0 errno=0\n").unwrap();
        let required = DescriptorRequirements::from_prior(&before).unwrap();
        let current = Observation::parse(controlled_fixture(
            "FD_DIRECTORY self rc=4 errno=0\nFD_ENTRY self fd=bad\nFD_ENTRY self fd=7\nFD_SCAN_COMPLETE self rc=0 errno=0\n",
        ).as_bytes()).unwrap();
        let current_rows = check_isolation_with_descriptor_requirements(
            Ok(&current),
            Err("no native authority".to_owned()),
            NamespacePolicy::Private,
            b"/fixture",
            Some(&required),
        );
        for name in [
            "descriptors.capability.CAPABILITY.self/7",
            "descriptors.capability.FD_OPENAT.self/7",
            "descriptors.self.numeric_entries",
        ] {
            let matching: Vec<_> = current_rows.iter().filter(|row| row.name == name).collect();
            assert_eq!(matching.len(), 1);
            assert_eq!(matching[0].verdict, Verdict::NoResult);
        }
        let parse_error = Observation::parse(b"FD_ENTRY self fd=5 fd=bad\n")
            .err()
            .unwrap();
        for guest in [Ok(&current), Err(parse_error)] {
            let rows = check_isolation_with_descriptor_requirements(
                guest,
                Err("no native authority".to_owned()),
                NamespacePolicy::Private,
                b"/fixture",
                Some(&required),
            );
            for name in [
                "descriptors.capability.CAPABILITY.self/5",
                "descriptors.capability.FD_OPENAT.self/5",
                "descriptors.self.fd5.complete",
            ] {
                let matching: Vec<_> = rows.iter().filter(|row| row.name == name).collect();
                assert_eq!(matching.len(), 1);
                assert_eq!(matching[0].verdict, Verdict::NoResult);
            }
        }
    }
}
