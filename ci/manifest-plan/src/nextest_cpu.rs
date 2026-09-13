use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

pub const ATTEMPT_RECORD_SCHEMA: u64 = 1;
pub const BINARY_MAP_SCHEMA: u64 = 1;
pub const CPU_REPORT_SCHEMA: u64 = 1;
pub const CPU_BINARY_MAP_ENV: &str = "HERMIT_NEXTEST_CPU_BINARY_MAP";
pub const CPU_RECORD_DIR_ENV: &str = "HERMIT_NEXTEST_CPU_RECORD_DIR";
pub const CPU_REPORT_PATH_ENV: &str = "HERMIT_NEXTEST_CPU_REPORT_PATH";
pub const CPU_SOURCE: &str = dagrun::proccpu::CPU_SOURCE_PROCFS;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryMapEntry {
    pub executable: String,
    pub package: String,
    pub binary: String,
    pub binary_name: String,
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryMap {
    pub schema: u64,
    pub entries: Vec<BinaryMapEntry>,
}

#[derive(Deserialize)]
struct NextestInventory {
    #[serde(rename = "rust-suites")]
    rust_suites: BTreeMap<String, NextestInventorySuite>,
}

#[derive(Deserialize)]
struct NextestInventorySuite {
    #[serde(rename = "package-name")]
    package: String,
    #[serde(rename = "binary-id")]
    binary: String,
    #[serde(rename = "binary-name")]
    binary_name: String,
    kind: String,
    #[serde(rename = "binary-path")]
    executable: String,
}

impl BinaryMap {
    pub fn from_nextest_inventory(bytes: &[u8]) -> Result<Self, String> {
        let inventory: NextestInventory = serde_json::from_slice(bytes)
            .map_err(|error| format!("malformed nextest inventory: {error}"))?;
        let mut entries = Vec::with_capacity(inventory.rust_suites.len());
        for (inventory_key, suite) in inventory.rust_suites {
            if inventory_key != suite.binary {
                return Err(format!(
                    "nextest inventory key {inventory_key:?} disagrees with binary-id {:?}",
                    suite.binary
                ));
            }
            entries.push(BinaryMapEntry {
                executable: suite.executable,
                package: suite.package,
                binary: suite.binary,
                binary_name: suite.binary_name,
                kind: suite.kind,
            });
        }
        entries.sort_by(|left, right| left.executable.cmp(&right.executable));
        let map = Self {
            schema: BINARY_MAP_SCHEMA,
            entries,
        };
        map.validate()?;
        Ok(map)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != BINARY_MAP_SCHEMA {
            return Err(format!(
                "unsupported binary-map schema {}, expected {BINARY_MAP_SCHEMA}",
                self.schema
            ));
        }
        let mut executables = BTreeMap::new();
        let mut binary_ids = BTreeMap::new();
        let mut suites = BTreeMap::new();
        for entry in &self.entries {
            if entry.executable.is_empty()
                || entry.package.is_empty()
                || entry.binary.is_empty()
                || entry.binary_name.is_empty()
                || entry.kind.is_empty()
            {
                return Err("nextest binary-map fields must be nonempty".into());
            }
            if !Path::new(&entry.executable).is_absolute() {
                return Err(format!(
                    "nextest binary-map executable is not absolute: {:?}",
                    entry.executable
                ));
            }
            if let Some(previous) = executables.insert(&entry.executable, entry) {
                return Err(format!(
                    "nextest binary-map executable path {:?} is ambiguous between binary ids {:?} and {:?}",
                    entry.executable, previous.binary, entry.binary
                ));
            }
            let binary_id = (&entry.package, &entry.binary);
            if let Some(previous) = binary_ids.insert(binary_id, entry) {
                return Err(format!(
                    "nextest binary-map identity ({:?}, {:?}) is ambiguous between executable paths {:?} and {:?}",
                    entry.package, entry.binary, previous.executable, entry.executable
                ));
            }
            let suite = (&entry.package, &entry.binary_name, &entry.kind);
            if let Some(previous) = suites.insert(suite, entry) {
                return Err(format!(
                    "nextest binary-map suite ({:?}, {:?}, {:?}) is ambiguous between executable paths {:?} and {:?}",
                    entry.package,
                    entry.binary_name,
                    entry.kind,
                    previous.executable,
                    entry.executable
                ));
            }
        }
        Ok(())
    }

    pub fn identity_for_executable(&self, executable: &Path) -> Result<(&str, &str), String> {
        let executable = executable
            .to_str()
            .ok_or_else(|| "nextest executable path is not valid UTF-8".to_string())?;
        self.entries
            .iter()
            .find(|entry| entry.executable == executable)
            .map(|entry| (entry.package.as_str(), entry.binary.as_str()))
            .ok_or_else(|| {
                format!("nextest executable path {executable:?} is absent from the typed inventory")
            })
    }

    pub fn identity_for_suite(
        &self,
        package: &str,
        binary_name: &str,
        kind: &str,
    ) -> Result<(&str, &str), String> {
        self.entries
            .iter()
            .find(|entry| {
                entry.package == package
                    && entry.binary_name == binary_name
                    && entry.kind == kind
            })
            .map(|entry| (entry.package.as_str(), entry.binary.as_str()))
            .ok_or_else(|| {
                format!(
                    "typed nextest suite ({package:?}, {binary_name:?}, {kind:?}) is absent from the binary map"
                )
            })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptIdentity {
    pub package: String,
    pub binary: String,
    pub test: String,
    pub attempt: u64,
}

impl AttemptIdentity {
    pub fn validate(&self) -> Result<(), String> {
        if self.package.is_empty() || self.binary.is_empty() || self.test.is_empty() {
            return Err("attempt identity fields must be nonempty".into());
        }
        if self.attempt == 0 {
            return Err("attempt number must be positive".into());
        }
        Ok(())
    }

    pub fn key(&self) -> String {
        let mut hasher = Sha256::new();
        for value in [&self.package, &self.binary, &self.test] {
            hasher.update(value.len().to_be_bytes());
            hasher.update(value.as_bytes());
        }
        hasher.update(self.attempt.to_be_bytes());
        format!("{:x}", hasher.finalize())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptCompletion {
    Exit { code: i32 },
    Signal { signal: i32 },
    SupervisorSignal { signal: i32 },
}

impl AttemptCompletion {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Exit { code } if (0..=255).contains(code) => Ok(()),
            Self::Exit { code } => Err(format!("exit code {code} is outside 0..=255")),
            Self::Signal { signal } | Self::SupervisorSignal { signal } if *signal > 0 => Ok(()),
            Self::Signal { signal } | Self::SupervisorSignal { signal } => {
                Err(format!("signal {signal} is not positive"))
            }
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Exit { code: 0 })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    pub schema: u64,
    pub key: String,
    pub run_id: String,
    pub identity: AttemptIdentity,
    pub cpu_source: String,
    pub cpu_usage_usec: u64,
    pub wall_time_ms: u64,
    pub completion: AttemptCompletion,
}

impl AttemptRecord {
    pub fn new(
        run_id: String,
        identity: AttemptIdentity,
        cpu_usage_usec: u64,
        wall_time_ms: u64,
        completion: AttemptCompletion,
    ) -> Self {
        let key = identity.key();
        Self {
            schema: ATTEMPT_RECORD_SCHEMA,
            key,
            run_id,
            identity,
            cpu_source: CPU_SOURCE.into(),
            cpu_usage_usec,
            wall_time_ms,
            completion,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != ATTEMPT_RECORD_SCHEMA {
            return Err(format!(
                "unsupported attempt-record schema {}, expected {ATTEMPT_RECORD_SCHEMA}",
                self.schema
            ));
        }
        if self.run_id.is_empty() {
            return Err("attempt record run_id must be nonempty".into());
        }
        self.identity.validate()?;
        if self.key != self.identity.key() {
            return Err(
                "attempt record key does not match its binary/test/attempt identity".into(),
            );
        }
        if self.cpu_source != CPU_SOURCE {
            return Err(format!(
                "attempt record cpu_source {:?} is not {CPU_SOURCE:?}",
                self.cpu_source
            ));
        }
        self.completion.validate()
    }

    pub fn file_name(&self) -> String {
        format!("{}.json", self.key)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CpuReport {
    pub schema: u64,
    pub run_id: Option<String>,
    pub attempts: Vec<AttemptRecord>,
}

impl CpuReport {
    pub fn new(run_id: Option<String>, attempts: Vec<AttemptRecord>) -> Self {
        Self {
            schema: CPU_REPORT_SCHEMA,
            run_id,
            attempts,
        }
    }
}

fn serialized_line<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| format!("cannot serialize JSON record: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", path.display()))
}

pub fn write_attempt_atomic(directory: &Path, record: &AttemptRecord) -> Result<PathBuf, String> {
    record.validate()?;
    let metadata = fs::metadata(directory)
        .map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "attempt-record path {} is not a directory",
            directory.display()
        ));
    }
    let final_path = directory.join(record.file_name());
    let temporary = directory.join(format!(".{}.{}.tmp", record.key, std::process::id()));
    let bytes = serialized_line(record)?;
    write_new_file(&temporary, &bytes)?;
    let publish = fs::hard_link(&temporary, &final_path).map_err(|error| {
        format!(
            "cannot publish attempt record {} without replacing an existing record: {error}",
            final_path.display()
        )
    });
    let _ = fs::remove_file(&temporary);
    publish?;
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync {}: {error}", directory.display()))?;
    Ok(final_path)
}

pub fn read_attempt_records(directory: &Path) -> Result<Vec<AttemptRecord>, String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "cannot read attempt-record directory {}: {error}",
            directory.display()
        )
    })?;
    let mut entries = entries.collect::<Result<Vec<_>, _>>().map_err(|error| {
        format!(
            "cannot enumerate attempt-record directory {}: {error}",
            directory.display()
        )
    })?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut by_identity = BTreeMap::new();
    for entry in entries {
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "cannot inspect attempt record {}: {error}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            return Err(format!(
                "attempt-record directory contains non-file {}",
                entry.path().display()
            ));
        }
        let bytes = fs::read(entry.path())
            .map_err(|error| format!("cannot read {}: {error}", entry.path().display()))?;
        let record: AttemptRecord = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "malformed attempt record {}: {error}",
                entry.path().display()
            )
        })?;
        record.validate().map_err(|error| {
            format!("invalid attempt record {}: {error}", entry.path().display())
        })?;
        if by_identity.contains_key(&record.identity) {
            return Err("duplicate attempt record for one binary/test/attempt identity".into());
        }
        let expected_name = record.file_name();
        if entry.file_name() != std::ffi::OsStr::new(&expected_name) {
            return Err(format!(
                "attempt record {} is substituted: its content requires file name {expected_name}",
                entry.path().display()
            ));
        }
        by_identity.insert(record.identity.clone(), record);
    }
    Ok(by_identity.into_values().collect())
}

pub fn read_binary_map(path: &Path) -> Result<BinaryMap, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read nextest binary map {}: {error}", path.display()))?;
    let map: BinaryMap = serde_json::from_slice(&bytes)
        .map_err(|error| format!("malformed nextest binary map {}: {error}", path.display()))?;
    map.validate()
        .map_err(|error| format!("invalid nextest binary map {}: {error}", path.display()))?;
    Ok(map)
}

fn write_replace_atomic<T: Serialize>(path: &Path, value: &T, label: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(label),
        std::process::id()
    ));
    let bytes = serialized_line(value)?;
    write_new_file(&temporary, &bytes)?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("cannot publish {label} {}: {error}", path.display())
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync {}: {error}", parent.display()))
}

pub fn write_binary_map_atomic(path: &Path, map: &BinaryMap) -> Result<(), String> {
    map.validate()?;
    write_replace_atomic(path, map, "nextest binary map")
}

pub fn write_report_atomic(path: &Path, report: &CpuReport) -> Result<(), String> {
    write_replace_atomic(path, report, "nextest CPU report")
}
