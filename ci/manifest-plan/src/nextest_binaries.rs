// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Prepare and verify the Cargo test executables consumed by validation.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

pub const SELECTION_ENV: &str = "NEXTEST_PREPARED_BUILD_SELECTION";
pub const REQUIRED_ENV: &str = "HERMIT_PREPARED_NEXTEST_REQUIRED";
const RECORD_SCHEMA: u32 = 3;
pub const GUESTS_ENV: &str = "HERMIT_PREPARED_CARGO_GUESTS";
pub const CPU_WRAPPER_ENV: &str = "HERMIT_NEXTEST_CPU_WRAPPER_BIN";
const CPU_WRAPPER_PACKAGE: &str = "hermit-manifest-plan";
const CPU_WRAPPER_TARGET: &str = "nextest-cpu-wrapper";

#[path = "../../cargo-guest-binaries.rs"]
mod cargo_guests;

#[path = "../../record-replay-workloads.rs"]
pub mod record_workloads;

#[derive(Debug, Eq, PartialEq)]
pub struct Arguments {
    pub build: Vec<String>,
    pub runtime: Vec<String>,
}

/// Separate only the Cargo selectors that validation actually supports.
/// Everything following `--` belongs to the test filter, even if it resembles
/// a Cargo selector. Never reconstruct argv by joining it into shell text.
pub fn split_arguments(args: &[String]) -> Result<Arguments, String> {
    let mut result = Arguments {
        build: Vec::new(),
        runtime: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            result.runtime.extend_from_slice(&args[index..]);
            break;
        }
        match arg.as_str() {
            "--workspace"
            | "--all"
            | "--lib"
            | "--bins"
            | "--tests"
            | "--benches"
            | "--examples"
            | "--all-targets"
            | "--all-features"
            | "--no-default-features" => {
                result.build.push(arg.clone());
            }
            "-p" | "--package" | "--exclude" | "-F" | "--features" | "--bin" | "--example"
            | "--test" | "--bench" | "--target" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                    .ok_or_else(|| format!("Cargo selector {arg} requires a value"))?;
                result.build.extend([arg.clone(), value.clone()]);
                index += 1;
            }
            "--release" | "--cargo-profile" | "--manifest-path" | "--target-dir" => {
                return Err(format!("unrecorded Cargo build option {arg}"));
            }
            "--profile"
            | "-j"
            | "--test-threads"
            | "-E"
            | "--filter-expr"
            | "--message-format"
            | "--message-format-version"
            | "--color" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("Nextest runtime option {arg} requires a value"))?;
                result.runtime.extend([arg.clone(), value.clone()]);
                index += 1;
            }
            "--no-capture" | "--nocapture" | "--no-fail-fast" | "--fail-fast" => {
                result.runtime.push(arg.clone());
            }
            _ => {
                if [
                    "--package=",
                    "--exclude=",
                    "--features=",
                    "--bin=",
                    "--example=",
                    "--test=",
                    "--bench=",
                    "--target=",
                ]
                .iter()
                .any(|prefix| arg.starts_with(prefix))
                {
                    let (option, value) = arg.split_once('=').expect("matched equals prefix");
                    if value.is_empty() {
                        return Err(format!("Cargo selector {option} requires a value"));
                    }
                    result.build.extend([option.into(), value.into()]);
                } else if arg.starts_with("--cargo-profile=")
                    || arg.starts_with("--manifest-path=")
                    || arg.starts_with("--target-dir=")
                {
                    return Err(format!("unrecorded Cargo build option {arg}"));
                } else if ["--profile=", "--test-threads=", "--filter-expr="]
                    .iter()
                    .any(|prefix| arg.starts_with(prefix))
                    || arg.strip_prefix("-j").is_some_and(|value| {
                        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
                    })
                {
                    result.runtime.push(arg.clone());
                } else if arg.starts_with('-') {
                    return Err(format!(
                        "unrecognized option before the test-filter separator: {arg}"
                    ));
                } else {
                    result.runtime.push(arg.clone());
                }
            }
        }
        index += 1;
    }
    Ok(result)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn selection_key(args: &[String]) -> String {
    digest(&serde_json::to_vec(args).expect("string vector is serializable"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileIdentity {
    path: PathBuf,
    size: u64,
    mode: u32,
    sha256: String,
}

/// Each identity field that differs between two observations of one path, as
/// `name before -> after`.
///
/// The link count is reported but never compared on its own: adding or
/// removing a hard link also changes ctime, which is compared. Naming it tells
/// a writer that only linked or unlinked the inode apart from one that wrote
/// its bytes. A 2026-10-07 validation refused /src/target/validate/hermit as
/// "changed while hashing" without saying which field moved, which left
/// nothing to identify the writer with.
fn metadata_changes(before: &fs::Metadata, after: &fs::Metadata) -> Vec<String> {
    let time = |seconds: i64, nanoseconds: i64| format!("{seconds}.{nanoseconds:09}");
    let fields = [
        ("dev", before.dev().to_string(), after.dev().to_string()),
        ("ino", before.ino().to_string(), after.ino().to_string()),
        ("size", before.len().to_string(), after.len().to_string()),
        (
            "mode",
            format!("{:o}", before.mode()),
            format!("{:o}", after.mode()),
        ),
        (
            "mtime",
            time(before.mtime(), before.mtime_nsec()),
            time(after.mtime(), after.mtime_nsec()),
        ),
        (
            "ctime",
            time(before.ctime(), before.ctime_nsec()),
            time(after.ctime(), after.ctime_nsec()),
        ),
    ];
    let mut changed: Vec<String> = fields
        .into_iter()
        .filter(|(_, old, new)| old != new)
        .map(|(name, old, new)| format!("{name} {old} -> {new}"))
        .collect();
    if !changed.is_empty() {
        changed.push(format!("nlink {} -> {}", before.nlink(), after.nlink()));
    }
    changed
}

fn file_identity(path: &Path, executable: bool) -> Result<FileIdentity, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect prepared input {}: {error}", path.display()))?;
    if !before.is_file() || executable && before.mode() & 0o111 == 0 {
        return Err(format!(
            "prepared input is not a regular {}file: {}",
            if executable { "executable " } else { "" },
            path.display()
        ));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| format!("cannot open prepared input {}: {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect open input {}: {error}", path.display()))?;
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        return Err(format!(
            "prepared input changed while opening {}",
            path.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash prepared input {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let after = fs::symlink_metadata(path)
        .map_err(|error| format!("prepared input disappeared {}: {error}", path.display()))?;
    let changed = metadata_changes(&before, &after);
    if !changed.is_empty() {
        return Err(format!(
            "prepared input changed while hashing {}: {}",
            path.display(),
            changed.join(", ")
        ));
    }
    Ok(FileIdentity {
        path: path.to_owned(),
        size: before.len(),
        mode: before.mode(),
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// Run git in `root` itself.
///
/// The preparation identity must describe the checkout this tool was pointed
/// at. An inherited `GIT_DIR` would make these reads describe the caller's
/// repository instead, and the scratch fixtures that exercise this function
/// would write into it (https://github.com/rrnewton/hermit/issues/3362).
fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let result = crate::git_environment::git_command()
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot inspect preparation source: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "cannot inspect preparation source with git {args:?}: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    Ok(result.stdout)
}

fn source_identity(root: &Path) -> Result<String, String> {
    let mut hash = Sha256::new();
    for args in [
        &["rev-parse", "HEAD", "HEAD^{tree}"][..],
        &[
            "diff",
            "--binary",
            "--no-ext-diff",
            "--submodule=diff",
            "--no-textconv",
            "HEAD",
            "--",
        ][..],
    ] {
        let bytes = git_bytes(root, args)?;
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    let untracked = git_bytes(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for name in untracked
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::str::from_utf8(name)
            .map_err(|_| "untracked preparation source path is not UTF-8")?;
        let identity = file_identity(&root.join(name), false)?;
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(identity.sha256.as_bytes());
        hash.update(identity.mode.to_le_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read metadata {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid metadata {}: {error}", path.display()))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("metadata has no nonempty {key}"))
}

fn target_path(target: &Path, raw: &str) -> Result<PathBuf, String> {
    let input = Path::new(raw);
    if input
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!("prepared input contains a parent traversal: {raw}"));
    }
    let path = if input.is_absolute() {
        input.to_owned()
    } else {
        target.join(input)
    };
    if !path.starts_with(target) {
        return Err(format!(
            "prepared input is outside target {}: {raw}",
            target.display()
        ));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("missing prepared input {}: {error}", path.display()))?;
    if canonical != path {
        return Err(format!(
            "prepared input uses a symlink or noncanonical path: {raw}"
        ));
    }
    Ok(path)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct BinaryIdentity {
    binary_id: String,
    package_id: String,
    binary_name: String,
    kind: String,
    build_platform: String,
    executable: FileIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SelectionRecord {
    build_args: Vec<String>,
    metadata: FileIdentity,
    binaries: Vec<BinaryIdentity>,
    runtime_files: Vec<FileIdentity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PreparedRecord {
    schema: u32,
    repository: PathBuf,
    target: PathBuf,
    sources: BTreeMap<PathBuf, String>,
    rustc: String,
    cargo_config: BTreeMap<PathBuf, Option<FileIdentity>>,
    build_environment: BTreeMap<String, String>,
    cargo_metadata: FileIdentity,
    selections: BTreeMap<String, SelectionRecord>,
    guests: Vec<FileIdentity>,
    record_workloads: Option<RecordWorkloads>,
    cpu_wrapper: CpuWrapperRecord,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RecordWorkload {
    name: String,
    source: FileIdentity,
    executable: FileIdentity,
    cargo_origin: Option<FileIdentity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RecordWorkloads {
    cargo_events: FileIdentity,
    compiler: FileIdentity,
    c_flags: Vec<String>,
    workloads: Vec<RecordWorkload>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CpuWrapperRecord {
    metadata: FileIdentity,
    executable: FileIdentity,
}

fn cpu_wrapper_artifact(
    events: &str,
    cargo: &Value,
    target: &Path,
) -> Result<FileIdentity, String> {
    let packages = cargo["packages"]
        .as_array()
        .ok_or("missing Cargo packages")?;
    let packages = packages
        .iter()
        .filter(|p| p["name"] == CPU_WRAPPER_PACKAGE)
        .collect::<Vec<_>>();
    if packages.len() != 1 {
        return Err("CPU wrapper package is missing or ambiguous".into());
    }
    let package = packages[0];
    let package_id = string(package, "id")?;
    let targets = package["targets"]
        .as_array()
        .ok_or("missing CPU wrapper targets")?;
    if targets
        .iter()
        .filter(|t| t["name"] == CPU_WRAPPER_TARGET && t["kind"] == serde_json::json!(["bin"]))
        .count()
        != 1
    {
        return Err("CPU wrapper Cargo target is missing or ambiguous".into());
    }
    let mut found = None;
    for line in events.lines() {
        let event: Value = serde_json::from_str(line)
            .map_err(|e| format!("invalid CPU wrapper Cargo event: {e}"))?;
        if event["reason"] != "compiler-artifact" || event["target"]["name"] != CPU_WRAPPER_TARGET {
            continue;
        }
        if event["package_id"] != package_id
            || event["target"]["kind"] != serde_json::json!(["bin"])
            || event["profile"]["test"] != false
        {
            return Err("CPU wrapper artifact has the wrong package, target, or profile".into());
        }
        let path = target_path(target, string(&event, "executable")?)?;
        if found.replace(file_identity(&path, true)?).is_some() {
            return Err("Cargo emitted ambiguous CPU wrapper artifacts".into());
        }
    }
    found.ok_or_else(|| "Cargo did not report the normal CPU wrapper executable".into())
}

fn verify_cpu_wrapper(record: &PreparedRecord, cargo: &Value) -> Result<(), String> {
    check_identity(&record.cpu_wrapper.metadata, false)?;
    let events =
        fs::read_to_string(&record.cpu_wrapper.metadata.path).map_err(|e| e.to_string())?;
    if cpu_wrapper_artifact(&events, cargo, &record.target)? != record.cpu_wrapper.executable {
        return Err("prepared CPU wrapper executable changed".into());
    }
    if let Some(configured) = std::env::var_os(CPU_WRAPPER_ENV) {
        if Path::new(&configured) != record.cpu_wrapper.executable.path {
            return Err("configured CPU wrapper differs from the prepared executable".into());
        }
    }
    Ok(())
}

fn prepare_cpu_wrapper(root: &Path, destination: &Path) -> Result<(), PreparationError> {
    cargo_output(
        root,
        &[
            "build",
            "--locked",
            "--message-format=json-render-diagnostics",
            "-p",
            CPU_WRAPPER_PACKAGE,
            "--bin",
            CPU_WRAPPER_TARGET,
        ]
        .map(String::from),
        destination,
    )
}

/// Explicit standalone preparation. Official test consumers never call this.
pub fn build_cpu_wrapper(root: &Path) -> Result<PathBuf, PreparationError> {
    if std::env::var(REQUIRED_ENV).as_deref() == Ok("1") {
        return Err("an official consumer cannot build a CPU wrapper".into());
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let artifacts = LockedArtifacts::open(&root, true)?;
    let cargo_path = artifacts.root.join("standalone-cargo.json");
    cargo_output(
        &root,
        &["metadata", "--locked", "--format-version", "1"].map(String::from),
        &cargo_path,
    )?;
    let cargo = read_json(&cargo_path)?;
    let events = artifacts.root.join("standalone-cpu-wrapper.jsonl");
    prepare_cpu_wrapper(&root, &events)?;
    Ok(cpu_wrapper_artifact(
        &fs::read_to_string(events).map_err(|e| e.to_string())?,
        &cargo,
        Path::new(string(&cargo, "target_directory")?),
    )?
    .path)
}

pub fn cpu_wrapper(root: &Path) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let artifacts = LockedArtifacts::open(&root, false)?;
    let record = artifacts.current()?;
    verify_record(&root, &record)?;
    Ok(record.cpu_wrapper.executable.path)
}

/// A comparison context for the regular calibration, after the ordinary
/// preparation checks. Other selectors retain their existing timeout path.
pub fn budget_context(
    root: &Path,
    args: &[String],
    launch: Option<&Path>,
) -> Result<String, String> {
    let launch_context = match launch {
        Some(path) => crate::nextest_cohort::verify(path)?,
        None => (None, None),
    };
    let parsed = split_arguments(args)?;
    let regular = [
        "--workspace",
        "--exclude",
        "hermit-detcore",
        "--exclude",
        "hermit",
        "--exclude",
        "hermetic_infra_hermit_flaky-tests",
    ];
    if parsed.build != regular {
        return Ok("null".into());
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let artifacts = LockedArtifacts::open(&root, false)?;
    let record = artifacts.current()?;
    let cargo = verify_record(&root, &record)?;
    verify_selection(&record, &cargo, &parsed.build)?;
    let excluded = format!(":(exclude){}", crate::nextest_cpu::CALIBRATION_PATH);
    let tree = git_bytes(&root, &["ls-tree", "-r", "-z", "HEAD"])?;
    let mut source_hash = Sha256::new();
    for entry in tree
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let path = entry
            .splitn(2, |byte| *byte == b'\t')
            .nth(1)
            .ok_or("invalid Git tree entry")?;
        if path != crate::nextest_cpu::CALIBRATION_PATH.as_bytes() {
            source_hash.update(entry);
            source_hash.update([0]);
        }
    }
    let dirty = git_bytes(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--",
            ".",
            &excluded,
        ],
    )?;
    // Only the derived calibration table is excluded. Its own digest is bound
    // separately in the resolved map. Dirty source never activates a table.
    let source_sha256 = format!("{:x}", source_hash.finalize());
    let mut environment = record.build_environment.clone();
    if environment.get("CARGO_TARGET_DIR").map(Path::new) == Some(record.target.as_path()) {
        environment.insert("CARGO_TARGET_DIR".into(), "${VERIFIED_TARGET}".into());
    }
    // Cargo configuration contents remain exact. Logical placement preserves
    // precedence without equating physical executable paths across checkouts.
    let config = record
        .cargo_config
        .iter()
        .filter_map(|(path, identity)| {
            identity.as_ref().map(|identity| {
                let location = if let Ok(relative) = path.strip_prefix(&root) {
                    format!("source/{}", relative.display())
                } else if let Some(parent) = path.parent().and_then(Path::parent) {
                    match root.ancestors().position(|ancestor| ancestor == parent) {
                        Some(depth) => format!(
                            "ancestor-{depth}/{}",
                            path.file_name().unwrap().to_string_lossy()
                        ),
                        None => path.display().to_string(),
                    }
                } else {
                    path.display().to_string()
                };
                (location, identity.mode, identity.sha256.clone())
            })
        })
        .collect::<BTreeSet<_>>();
    let build = serde_json::to_vec(&(&record.rustc, &parsed.build, environment, config))
        .map_err(|e| e.to_string())?;
    let machine = fs::read_to_string("/proc/cpuinfo")
        .map_err(|e| e.to_string())?
        .lines()
        .find_map(|line| {
            line.strip_prefix("model name")
                .and_then(|value| value.split_once(':'))
                .map(|(_, value)| value.trim().to_string())
        })
        .ok_or("cannot identify calibration CPU model")?;
    let (execution_cohort, launch_proof) = launch_context;
    let context = crate::nextest_cpu::BudgetContext {
        source_sha256,
        source_clean: dirty.is_empty(),
        build_sha256: format!("{:x}", Sha256::digest(&build)),
        machine: format!("{}:{machine}", std::env::consts::ARCH),
        available_cpus: std::thread::available_parallelism()
            .map_err(|e| e.to_string())?
            .get() as u64,
        execution_cohort,
        launch_proof,
    };
    serde_json::to_string(&context).map_err(|e| e.to_string())
}

fn build_environment() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "RUSTFLAGS"
                    | "SOURCE_DATE_EPOCH"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "RUSTDOCFLAGS"
                    | "RUSTC"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
                    | "RUSTUP_TOOLCHAIN"
                    | "CARGO_BUILD_TARGET"
                    | "CARGO_TARGET_DIR"
                    | "CARGO_BUILD_BUILD_DIR"
                    | "CC"
                    | "CXX"
                    | "AR"
                    | "CFLAGS"
                    | "CXXFLAGS"
                    | "LDFLAGS"
                    | "LIBRARY_PATH"
            ) || name.starts_with("CARGO_PROFILE_")
                || (name.starts_with("CARGO_TARGET_") && name != "CARGO_TARGET_TMPDIR")
                || ["CC_", "CXX_", "AR_", "CFLAGS_", "CXXFLAGS_"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
        .collect()
}

fn rustc_identity() -> Result<String, String> {
    let result = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("-vV")
        .output()
        .map_err(|error| format!("cannot identify preparation compiler: {error}"))?;
    if !result.status.success() {
        return Err("cannot identify preparation compiler".into());
    }
    String::from_utf8(result.stdout).map_err(|_| "compiler identity is not UTF-8".into())
}

fn metadata_binaries(
    metadata: &Value,
    cargo: &Value,
    target: &Path,
) -> Result<(Vec<BinaryIdentity>, Vec<FileIdentity>), String> {
    let build = metadata
        .get("rust-build-meta")
        .ok_or("Nextest metadata has no rust-build-meta")?;
    if Path::new(string(build, "target-directory")?) != target {
        return Err("Nextest metadata names a different Cargo target directory".into());
    }
    let packages = cargo
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("Cargo metadata has no package list")?;
    let mut package_ids = BTreeMap::new();
    for package in packages {
        let id = string(package, "id")?;
        if package_ids.insert(id, package).is_some() {
            return Err(format!("Cargo metadata repeats package identity {id}"));
        }
    }
    let binaries = metadata
        .get("rust-binaries")
        .and_then(Value::as_object)
        .filter(|binaries| !binaries.is_empty())
        .ok_or("Nextest metadata has no test executables")?;
    let mut result = Vec::new();
    let mut paths = BTreeSet::new();
    let mut seen_targets = BTreeSet::new();
    for (id, binary) in binaries {
        if string(binary, "binary-id")? != id {
            return Err(format!(
                "Nextest binary key disagrees with its identity: {id}"
            ));
        }
        let package_id = string(binary, "package-id")?;
        let package = package_ids
            .get(package_id)
            .ok_or_else(|| format!("Nextest binary {id} names an unknown package {package_id}"))?;
        let name = string(binary, "binary-name")?;
        let kind = string(binary, "kind")?;
        let targets = package
            .get("targets")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("Cargo package {package_id} has no targets"))?;
        if !targets.iter().any(|target| {
            target.get("name").and_then(Value::as_str) == Some(name)
                && target
                    .get("kind")
                    .and_then(Value::as_array)
                    .is_some_and(|kinds| {
                        kinds.iter().any(|value| value.as_str() == Some(kind))
                            || (kind == "lib"
                                && !kinds.is_empty()
                                && kinds.iter().all(|value| {
                                    matches!(
                                        value.as_str(),
                                        Some(
                                            "rlib"
                                                | "dylib"
                                                | "cdylib"
                                                | "staticlib"
                                                | "proc-macro"
                                        )
                                    )
                                }))
                    })
        }) {
            return Err(format!(
                "Nextest executable {id} is not the declared {kind} target {name} in {package_id}"
            ));
        }
        if !seen_targets.insert((package_id, name, kind, string(binary, "build-platform")?)) {
            return Err(format!(
                "Nextest metadata has ambiguous executables for {package_id} {kind} {name}"
            ));
        }
        let path = target_path(target, string(binary, "binary-path")?)?;
        if !paths.insert(path.clone()) {
            return Err(format!(
                "Nextest identities share one executable path: {}",
                path.display()
            ));
        }
        result.push(BinaryIdentity {
            binary_id: id.clone(),
            package_id: package_id.into(),
            binary_name: name.into(),
            kind: kind.into(),
            build_platform: string(binary, "build-platform")?.into(),
            executable: file_identity(&path, true)?,
        });
    }
    let mut runtime_paths = BTreeSet::new();
    for (package, files) in build
        .get("non-test-binaries")
        .and_then(Value::as_object)
        .ok_or("Nextest metadata has no non-test-binaries map")?
    {
        if !package_ids.contains_key(package.as_str()) {
            return Err(format!(
                "Nextest runtime files name unknown Cargo package {package}"
            ));
        }
        for file in files.as_array().ok_or("invalid non-test-binaries entry")? {
            runtime_paths.insert(target_path(target, string(file, "path")?)?);
        }
    }
    let runtime_files = runtime_paths
        .iter()
        .map(|path| file_identity(path, false))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((result, runtime_files))
}

fn sources(cargo: &Value, root: &Path) -> Result<BTreeMap<PathBuf, String>, String> {
    let mut roots = BTreeSet::from([root.to_owned()]);
    for package in cargo["packages"]
        .as_array()
        .ok_or("Cargo packages are missing")?
    {
        // Registry sources have checksum-bound entries in Cargo.lock. Local
        // and git packages may have mutable checkouts outside this repository.
        if package["source"]
            .as_str()
            .is_some_and(|source| source.starts_with("registry+"))
        {
            continue;
        }
        let manifest = Path::new(string(package, "manifest_path")?);
        let directory = manifest.parent().ok_or("package manifest has no parent")?;
        let git_root = git_bytes(directory, &["rev-parse", "--show-toplevel"])?;
        let git_root = std::str::from_utf8(&git_root)
            .map_err(|_| "non-UTF-8 dependency root")?
            .trim();
        roots.insert(PathBuf::from(git_root));
    }
    roots
        .into_iter()
        .map(|root| source_identity(&root).map(|identity| (root, identity)))
        .collect()
}

fn cargo_config(root: &Path) -> Result<BTreeMap<PathBuf, Option<FileIdentity>>, String> {
    let mut paths = BTreeSet::new();
    for parent in root.ancestors() {
        for name in ["config", "config.toml"] {
            paths.insert(parent.join(".cargo").join(name));
        }
    }
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .ok_or("cannot determine Cargo home")?;
    for name in ["config", "config.toml"] {
        paths.insert(home.join(name));
    }
    paths
        .into_iter()
        .map(|path| {
            let identity = match fs::symlink_metadata(&path) {
                Ok(_) => Some(file_identity(&path, false)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect Cargo configuration {}: {error}",
                        path.display()
                    ));
                }
            };
            Ok((path, identity))
        })
        .collect()
}

/// The preparation population comes from the committed graph, not a second
/// runtime reconstruction of the validation driver.
pub fn profile_selections(
    root: &Path,
    profile: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let graph = fs::read_to_string(root.join("ci/dag/validate.json")).map_err(|e| e.to_string())?;
    let graph = dagrun::io::dag_from_json(&graph).map_err(|e| e.to_string())?;
    config_selections(&graph, profile)
}

pub fn config_selections(
    graph: &dagrun::model::DagConfig,
    profile: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let selected =
        dagrun::select_steps_by_labels(graph, &[profile.into()]).map_err(|e| e.to_string())?;
    let mut selections = BTreeMap::new();
    for step in selected.steps {
        if let Some(args) = step.env.get(SELECTION_ENV) {
            if step.env.get(REQUIRED_ENV).map(String::as_str) != Some("1") {
                return Err(format!(
                    "{} does not require prepared executables",
                    step.tag()
                ));
            }
            let args: Vec<String> =
                serde_json::from_str(args).map_err(|e| format!("{}: {e}", step.tag()))?;
            let parsed = split_arguments(&args)?;
            if parsed.build != args || !parsed.runtime.is_empty() || args.is_empty() {
                return Err(format!(
                    "{} has an invalid prepared Cargo selection",
                    step.tag()
                ));
            }
            selections.insert(selection_key(&args), args);
        }
    }
    if selections.is_empty() {
        return Err(format!(
            "profile {profile} has no prepared Nextest selections"
        ));
    }
    Ok(selections)
}

struct LockedArtifacts {
    root: PathBuf,
    _lock: fs::File,
}

impl LockedArtifacts {
    fn open(root: &Path, exclusive: bool) -> Result<Self, String> {
        let root = root.join("target/ci/nextest-binaries");
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("lock"))
            .map_err(|e| e.to_string())?;
        // Keep this file open across verification and the entire Nextest child.
        // A later producer cannot replace a generation beneath a running test.
        if unsafe {
            libc::flock(
                lock.as_raw_fd(),
                if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                },
            )
        } != 0
        {
            return Err(format!(
                "cannot lock Nextest artifacts: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { root, _lock: lock })
    }

    fn current(&self) -> Result<PreparedRecord, String> {
        let path = self.root.join("current.json");
        let _: FileIdentity = file_identity(&path, false)?;
        serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid prepared record {}: {e}", path.display()))
    }
}

#[derive(Debug)]
pub struct PreparationError {
    pub message: String,
    pub status: u8,
}

impl From<String> for PreparationError {
    fn from(message: String) -> Self {
        Self { message, status: 2 }
    }
}
impl From<&str> for PreparationError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl std::fmt::Display for PreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

/// The Cargo width of a preparation build: the width the scheduler admitted
/// for the preparing node (dagrun delivers it through `CARGO_BUILD_JOBS`, the
/// workspace producers' jobs_env), else 8. Since the one-build change of
/// 2026-09-30 every selection compiles in the optimised `validate` profile,
/// where one large crate's code generation can use up to its 16 codegen units;
/// a fixed width of 8 made each such compile wait on half of them.
fn preparation_build_jobs() -> String {
    std::env::var("CARGO_BUILD_JOBS")
        .ok()
        .filter(|jobs| jobs.parse::<u32>().is_ok_and(|jobs| jobs > 0))
        .unwrap_or_else(|| "8".into())
}

fn cargo_output(root: &Path, args: &[String], destination: &Path) -> Result<(), PreparationError> {
    let output = fs::File::create(destination).map_err(|e| e.to_string())?;
    let status = Command::new(root.join("ci/run-with-reverie-dbt-budget.sh"))
        .arg("cargo")
        .args(args)
        .env("CARGO_BUILD_JOBS", preparation_build_jobs())
        .current_dir(root)
        .stdout(output)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(PreparationError {
            message: format!("Cargo preparation failed ({status}): {args:?}"),
            status: u8::try_from(exit_code(status)).unwrap_or(1),
        });
    }
    Ok(())
}

fn check_identity(expected: &FileIdentity, executable: bool) -> Result<(), String> {
    let actual = file_identity(&expected.path, executable)?;
    if &actual != expected {
        return Err(format!(
            "prepared input changed: {}",
            expected.path.display()
        ));
    }
    Ok(())
}

fn verify_record(root: &Path, record: &PreparedRecord) -> Result<Value, String> {
    if record.schema != RECORD_SCHEMA || record.repository != root {
        return Err("prepared record has the wrong schema or repository".into());
    }
    check_identity(&record.cargo_metadata, false)?;
    let cargo = read_json(&record.cargo_metadata.path)?;
    let target = Path::new(string(&cargo, "target_directory")?);
    if target != record.target || target.canonicalize().map_err(|e| e.to_string())? != target {
        return Err("prepared record has the wrong Cargo target directory".into());
    }
    if record.sources != sources(&cargo, root)? {
        return Err("prepared executables are stale: source identity changed".into());
    }
    if record.rustc != rustc_identity()?
        || record.build_environment != build_environment()
        || record.cargo_config != cargo_config(root)?
    {
        return Err("prepared executables are stale: compiler, Cargo configuration, or build environment changed".into());
    }
    verify_cpu_wrapper(record, &cargo)?;
    Ok(cargo)
}

fn verify_selection(
    record: &PreparedRecord,
    cargo: &Value,
    args: &[String],
) -> Result<SelectionRecord, String> {
    let selected = record
        .selections
        .get(&selection_key(args))
        .ok_or_else(|| format!("no prepared executable selection for {args:?}"))?;
    if selected.build_args != args {
        return Err("prepared Cargo selection does not match its key".into());
    }
    check_identity(&selected.metadata, false)?;
    let metadata = read_json(&selected.metadata.path)?;
    let (binaries, runtime_files) = metadata_binaries(&metadata, cargo, &record.target)?;
    if selected.binaries != binaries || selected.runtime_files != runtime_files {
        return Err("prepared executable, package identity, or runtime file changed".into());
    }
    Ok(selected.clone())
}

fn verify_guests(record: &PreparedRecord) -> Result<BTreeMap<String, String>, String> {
    if !record.selections.values().any(|selection| {
        selection
            .build_args
            .windows(2)
            .any(|args| args == ["--test", "hermit_modes"])
    }) {
        if !record.guests.is_empty() {
            return Err(
                "unexpected Cargo guest records for a selection without hermit_modes".into(),
            );
        }
        return Ok(BTreeMap::new());
    }
    let mut result = BTreeMap::new();
    for guest in &record.guests {
        check_identity(guest, true)?;
        let name = guest
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid guest path")?;
        if !guest.path.starts_with(&record.target)
            || result
                .insert(name.into(), guest.path.to_string_lossy().into())
                .is_some()
        {
            return Err("prepared guest identities are outside the target or ambiguous".into());
        }
    }
    if result.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != cargo_guests::CARGO_GUEST_BINARIES.into_iter().collect()
    {
        return Err("prepared Cargo guest population differs from hermit_modes fixtures".into());
    }
    Ok(result)
}

fn guest_executables(
    events: &str,
    cargo: &Value,
    target: &Path,
) -> Result<Vec<FileIdentity>, String> {
    let package = cargo["packages"]
        .as_array()
        .ok_or("missing Cargo packages")?
        .iter()
        .filter(|package| package["name"].as_str() == Some("hermetic_infra_hermit_tests"))
        .collect::<Vec<_>>();
    if package.len() != 1 {
        return Err("Cargo guest package is missing or ambiguous".into());
    }
    let package = string(package[0], "id")?;
    let mut by_name = BTreeMap::<String, PathBuf>::new();
    for line in events.lines() {
        let event: Value =
            serde_json::from_str(line).map_err(|e| format!("invalid Cargo guest event: {e}"))?;
        if event["reason"] != "compiler-artifact"
            || event["package_id"] != package
            || event["executable"].is_null()
        {
            continue;
        }
        let name = string(&event["target"], "name")?;
        if !cargo_guests::CARGO_GUEST_BINARIES.contains(&name) {
            continue;
        }
        if event["target"]["kind"] != serde_json::json!(["bin"])
            || event["profile"]["test"] != false
        {
            return Err(format!("Cargo guest {name} has the wrong target/profile"));
        }
        let path = target_path(target, string(&event, "executable")?)?;
        if by_name.insert(name.into(), path).is_some() {
            return Err(format!("Cargo emitted ambiguous guest {name}"));
        }
    }
    if by_name.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != cargo_guests::CARGO_GUEST_BINARIES.into_iter().collect()
    {
        return Err("Cargo did not report all 21 hermit_modes guest executables".into());
    }
    by_name
        .values()
        .map(|path| file_identity(path, true))
        .collect()
}

fn has_test_selection<'a>(selections: impl Iterator<Item = &'a Vec<String>>, name: &str) -> bool {
    selections
        .into_iter()
        .any(|args| args.windows(2).any(|args| args == ["--test", name]))
}

fn prepare_record_workloads(
    root: &Path,
    generation: &Path,
    cargo: &Value,
    target: &Path,
) -> Result<RecordWorkloads, PreparationError> {
    let directory = generation.join("record-workloads");
    fs::create_dir(&directory).map_err(|e| e.to_string())?;
    let compiler = file_identity(&record_workloads::c_compiler()?, true)?;
    let cargo_events = file_identity(&generation.join("guests.jsonl"), false)?;
    let rust = record_workloads::cargo_executables(
        &fs::read_to_string(&cargo_events.path).map_err(|e| e.to_string())?,
        cargo,
        root,
        target,
    )?;
    let origins = rust
        .iter()
        .map(|(name, path)| Ok((name.clone(), file_identity(path, true)?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let mut paths = record_workloads::compile_c_workloads(root, &directory, &compiler.path)
        .map_err(|error| PreparationError {
            message: error.message,
            status: error.status,
        })?;
    paths.extend(record_workloads::copy_cargo_workloads(rust, &directory)?);
    let workloads = paths
        .into_iter()
        .map(|(name, path)| {
            let source =
                record_workloads::source(&name).ok_or("unexpected record workload alias")?;
            Ok(RecordWorkload {
                source: file_identity(&root.join(source), false)?,
                executable: file_identity(&path, true)?,
                cargo_origin: origins.get(&name).cloned(),
                name,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(RecordWorkloads {
        compiler,
        cargo_events,
        workloads,
        c_flags: record_workloads::C_FLAGS.map(String::from).into(),
    })
}

fn verify_record_workloads(
    record: &PreparedRecord,
    cargo: &Value,
) -> Result<Option<String>, String> {
    let selected = has_test_selection(
        record.selections.values().map(|s| &s.build_args),
        "record_replay",
    );
    let Some(workloads) = &record.record_workloads else {
        return if selected {
            Err("selected record_replay has no prepared workload population".into())
        } else {
            Ok(None)
        };
    };
    if !selected {
        return Err("unexpected record workloads for a selection without record_replay".into());
    }
    let generation = record
        .cargo_metadata
        .path
        .parent()
        .ok_or("missing prepared generation")?;
    let directory = generation.join("record-workloads");
    if workloads.cargo_events.path != generation.join("guests.jsonl") {
        return Err("record workload Cargo events are outside the selected generation".into());
    }
    check_identity(&workloads.cargo_events, false)?;
    let rust = record_workloads::cargo_executables(
        &fs::read_to_string(&workloads.cargo_events.path).map_err(|e| e.to_string())?,
        cargo,
        &record.repository,
        &record.target,
    )?;
    if workloads.compiler.path != record_workloads::c_compiler()?
        || workloads.c_flags != record_workloads::C_FLAGS
    {
        return Err("record workload C compiler or recipe changed".into());
    }
    check_identity(&workloads.compiler, true)?;
    let mut paths = BTreeMap::new();
    for workload in &workloads.workloads {
        let source =
            record_workloads::source(&workload.name).ok_or("unknown record workload alias")?;
        if workload.source.path != record.repository.join(source)
            || workload.executable.path != directory.join(&workload.name)
        {
            return Err(
                "record workload source or retained path differs from its declared alias".into(),
            );
        }
        record_workloads::executable(&workload.executable.path, &directory)?;
        check_identity(&workload.source, false)?;
        check_identity(&workload.executable, true)?;
        match (rust.get(&workload.name), &workload.cargo_origin) {
            (Some(path), Some(origin)) if path == &origin.path => {
                check_identity(origin, true)?;
                if origin.sha256 != workload.executable.sha256
                    || origin.size != workload.executable.size
                    || origin.mode != workload.executable.mode
                {
                    return Err(
                        "retained record workload differs from Cargo's actual artifact".into(),
                    );
                }
            }
            (None, None) => {}
            _ => return Err("record workload Cargo origin differs from compiler metadata".into()),
        }
        if paths
            .insert(workload.name.clone(), workload.executable.path.clone())
            .is_some()
        {
            return Err("duplicate prepared record workload alias".into());
        }
    }
    record_workloads::prepared_envelope(&paths).map(Some)
}

/// The Cargo profile that compiles a graph profile's prepared test executables.
///
/// The full and portable validations, and the hosted-portable twins of their
/// nodes, build every test executable in the `validate` profile (Cargo.toml),
/// the same profile as their one Hermit binary, so the executables share its
/// compilation. The focused graph profiles keep their own builds in Cargo's
/// default test profile.
pub fn cargo_profile_for(graph_profile: &str) -> Option<&'static str> {
    match graph_profile {
        "full" | "hosted-portable" => Some("validate"),
        _ => None,
    }
}

/// Whether `prepare` compiles this profile's selections as one unified listing.
///
/// Only the validate-profile graph profiles do: their producers build the whole
/// workspace with the unified features first, so the listing compiles nothing.
/// The other profiles' producers build far less (a dev `-p hermit --bin hermit`
/// in the privileged lane, under a 120-second cap), and a whole-workspace
/// listing there would compile every test target of every member; they keep
/// one listing per selection.
pub fn unifies(graph_profile: &str) -> bool {
    cargo_profile_for(graph_profile).is_some()
}

/// One Cargo listing `prepare` runs: its selectors, and the keys of the
/// selections whose metadata is taken from it.
type Listing = (Vec<String>, Vec<String>);

/// The Cargo listings `prepare` runs for a profile.
fn listings(
    graph_profile: &str,
    selections: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<Listing>, String> {
    if unifies(graph_profile) {
        return Ok(vec![(
            unified_selection(selections)?,
            selections.keys().cloned().collect(),
        )]);
    }
    Ok(selections
        .iter()
        .map(|(key, args)| (args.clone(), vec![key.clone()]))
        .collect())
}

/// The one Cargo selection that compiles every prepared selection of a profile.
///
/// Separate Cargo invocations resolve dependency features separately, so each
/// one rebuilt the shared crates it resolved differently and relinked the
/// unhashed `hermit` executable, and every later selection whose tests depend on
/// that executable then rebuilt too. At d44bbbb79acd the sixteen `full`
/// selections took about 520 of the producer's 830 seconds that way. One
/// selection is its own union. Several are compiled as the whole workspace with
/// all targets and the union of their package-qualified features, which is also
/// exactly the workspace build the validate-profile producers run first (see
/// [`unifies`]), so preparation then compiles nothing new.
pub fn unified_selection(
    selections: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    let mut values = selections.values();
    match (values.next(), values.next()) {
        (None, _) => return Err("there is no prepared selection to unify".into()),
        (Some(only), None) => return Ok(only.clone()),
        _ => {}
    }
    let mut features = BTreeSet::new();
    for selection in selections.values() {
        let parsed = CargoSelection::parse(selection)?;
        if parsed.unqualified_features.is_empty() {
            continue;
        }
        let [package] = parsed.packages.as_slice() else {
            return Err(format!(
                "{selection:?} names features without naming exactly one package, so the union cannot qualify them"
            ));
        };
        features.extend(
            parsed
                .unqualified_features
                .iter()
                .map(|feature| format!("{package}/{feature}")),
        );
    }
    let mut union = vec!["--workspace".to_string(), "--all-targets".to_string()];
    if !features.is_empty() {
        union.push("--features".into());
        union.push(features.into_iter().collect::<Vec<_>>().join(","));
    }
    Ok(union)
}

/// The workspace build a producer of a unifying profile runs immediately before
/// preparing it: exactly the profile's unified selection, so preparation
/// compiles nothing it did not already build (see [`unified_selection`]).
pub fn unified_prebuild_command(
    cfg: &dagrun::model::DagConfig,
    profile: &str,
) -> Result<String, String> {
    if !unifies(profile) {
        return Err(format!(
            "profile {profile} lists each selection separately and has no unified workspace build"
        ));
    }
    let unified = unified_selection(&config_selections(cfg, profile)?)?;
    let cargo_profile = cargo_profile_for(profile)
        .map(|name| format!(" --profile {name}"))
        .unwrap_or_default();
    Ok(format!(
        "cargo build --locked{cargo_profile} {} && ./ci/nextest-binaries.rs prepare {profile}",
        unified.join(" ")
    ))
}

/// The Cargo package and target selectors of one prepared selection. Only the
/// selectors the committed graph uses are accepted; anything else is refused
/// rather than approximated, because a wrong membership would run a different
/// set of test executables under an unchanged name.
#[derive(Debug, Default, Eq, PartialEq)]
struct CargoSelection {
    workspace: bool,
    packages: Vec<String>,
    excluded: Vec<String>,
    unqualified_features: Vec<String>,
    lib: bool,
    bins: bool,
    tests: bool,
    all_targets: bool,
    named_tests: Vec<String>,
}

impl CargoSelection {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut index = 0;
        while index < args.len() {
            let value = || {
                args.get(index + 1)
                    .cloned()
                    .ok_or_else(|| format!("{args:?}: {} has no value", args[index]))
            };
            match args[index].as_str() {
                "--workspace" | "--all" => parsed.workspace = true,
                "--lib" => parsed.lib = true,
                "--bins" => parsed.bins = true,
                "--tests" => parsed.tests = true,
                "--all-targets" => parsed.all_targets = true,
                "-p" | "--package" => {
                    parsed.packages.push(value()?);
                    index += 1;
                }
                "--exclude" => {
                    parsed.excluded.push(value()?);
                    index += 1;
                }
                "--test" => {
                    parsed.named_tests.push(value()?);
                    index += 1;
                }
                "-F" | "--features" => {
                    for feature in value()?.split([',', ' ']).filter(|f| !f.is_empty()) {
                        if feature.contains('/') {
                            return Err(format!(
                                "{args:?}: package-qualified feature {feature} is not a supported prepared selector"
                            ));
                        }
                        parsed.unqualified_features.push(feature.into());
                    }
                    index += 1;
                }
                other => {
                    return Err(format!(
                        "{args:?}: {other} is not a supported prepared Cargo selector"
                    ));
                }
            }
            index += 1;
        }
        if parsed.workspace == !parsed.packages.is_empty() {
            return Err(format!(
                "{args:?}: a prepared selection names either --workspace or packages, not both or neither"
            ));
        }
        if !parsed.workspace && !parsed.excluded.is_empty() {
            return Err(format!("{args:?}: --exclude requires --workspace"));
        }
        Ok(parsed)
    }

    fn selects_package(&self, name: &str) -> bool {
        if self.workspace {
            !self.excluded.iter().any(|excluded| excluded == name)
        } else {
            self.packages.iter().any(|package| package == name)
        }
    }

    /// Cargo's target selection for `cargo test`: with no target selector,
    /// every target whose manifest `test` flag is set; `--tests` means the same
    /// set explicitly; `--lib`, `--bins` and `--test NAME` add their kinds.
    fn selects_target(&self, kind: &str, name: &str, tested_by_default: bool) -> bool {
        let explicit =
            self.lib || self.bins || self.tests || self.all_targets || !self.named_tests.is_empty();
        if (!explicit || self.tests) && tested_by_default {
            return true;
        }
        self.all_targets
            || (self.lib && matches!(kind, "lib" | "proc-macro"))
            || (self.bins && kind == "bin")
            || (kind == "test" && self.named_tests.iter().any(|test| test == name))
    }
}

/// A listing of exactly one selection must hand that selection every
/// executable Cargo listed for it; a smaller subset means the matcher and
/// Cargo disagree about the selection's targets.
fn require_listing_kept(
    listing: &[String],
    selection: &[String],
    listed: &Value,
    metadata: &Value,
) -> Result<(), String> {
    let count = |value: &Value| value["rust-binaries"].as_object().map(|b| b.len());
    if selection == listing && count(metadata) != count(listed) {
        return Err(format!(
            "the prepared subset of {listing:?} drops executables Cargo listed for it"
        ));
    }
    Ok(())
}

/// The nextest kind of a Cargo target: every library crate type is `lib`.
fn target_kind(target: &Value) -> Result<&'static str, String> {
    let kinds = target
        .get("kind")
        .and_then(Value::as_array)
        .ok_or("Cargo target has no kind")?;
    let has = |wanted: &str| kinds.iter().any(|kind| kind.as_str() == Some(wanted));
    Ok(if has("bin") {
        "bin"
    } else if has("test") {
        "test"
    } else if has("bench") {
        "bench"
    } else if has("example") {
        "example"
    } else if has("custom-build") {
        "custom-build"
    } else {
        "lib"
    })
}

/// The binaries-metadata of one selection, taken from the unified build's.
///
/// Every executable of the unified listing whose package and target the
/// selection names is kept; runtime files are kept for the selected packages.
/// A selection that would receive no executable, or a named test target that no
/// selected package has, is refused: Cargo refuses the second, and a prepared
/// record never holds an empty selection.
fn selection_metadata(unified: &Value, cargo: &Value, args: &[String]) -> Result<Value, String> {
    let selection = CargoSelection::parse(args)?;
    let members = cargo
        .get("workspace_members")
        .and_then(Value::as_array)
        .ok_or("Cargo metadata has no workspace members")?
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let mut selected_packages = BTreeMap::new();
    for package in cargo
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("Cargo metadata has no package list")?
    {
        let id = string(package, "id")?;
        if members.contains(id) && selection.selects_package(string(package, "name")?) {
            selected_packages.insert(id.to_string(), package);
        }
    }
    let member_names = cargo["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|package| {
            package["id"]
                .as_str()
                .is_some_and(|id| members.contains(id))
        })
        .map(|package| string(package, "name"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    // Cargo refuses an unknown `-p` package; an unknown `--exclude` is only a
    // warning there, so it is accepted here too.
    if let Some(name) = selection
        .packages
        .iter()
        .find(|name| !member_names.contains(name.as_str()))
    {
        return Err(format!(
            "{args:?}: package {name} is not a workspace member"
        ));
    }
    let mut result = unified.clone();
    let binaries = result
        .get_mut("rust-binaries")
        .and_then(Value::as_object_mut)
        .ok_or("unified Nextest metadata has no rust-binaries")?;
    let mut matched_tests = BTreeSet::new();
    let mut kept = serde_json::Map::new();
    for (id, binary) in std::mem::take(binaries) {
        let Some(package) = selected_packages.get(string(&binary, "package-id")?) else {
            continue;
        };
        let name = string(&binary, "binary-name")?;
        let kind = string(&binary, "kind")?;
        let target = package
            .get("targets")
            .and_then(Value::as_array)
            .ok_or("Cargo package has no targets")?
            .iter()
            .find(|target| {
                target.get("name").and_then(Value::as_str) == Some(name)
                    && target_kind(target).is_ok_and(|candidate| {
                        candidate == kind || (candidate == "lib" && kind == "proc-macro")
                    })
            })
            .ok_or_else(|| format!("unified executable {id} has no Cargo target"))?;
        let tested = target.get("test").and_then(Value::as_bool).unwrap_or(false);
        if selection.selects_target(kind, name, tested) {
            if kind == "test" {
                matched_tests.insert(name.to_string());
            }
            kept.insert(id, binary);
        }
    }
    if let Some(missing) = selection
        .named_tests
        .iter()
        .find(|test| !matched_tests.contains(test.as_str()))
    {
        return Err(format!(
            "{args:?}: no selected package has test target {missing}"
        ));
    }
    if kept.is_empty() {
        return Err(format!(
            "{args:?} selects no executable of the unified build"
        ));
    }
    *binaries = kept;
    let runtime = result
        .get_mut("rust-build-meta")
        .and_then(|build| build.get_mut("non-test-binaries"))
        .and_then(Value::as_object_mut)
        .ok_or("unified Nextest metadata has no non-test-binaries map")?;
    runtime.retain(|package, _| selected_packages.contains_key(package));
    Ok(result)
}

pub fn prepare(root: &Path, profile: &str) -> Result<(), PreparationError> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let selections = profile_selections(&root, profile)?;
    let cargo_profile = cargo_profile_for(profile);
    let artifacts = LockedArtifacts::open(&root, true)?;
    let generation = artifacts.root.join(format!(
        "generation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    ));
    fs::create_dir(&generation).map_err(|e| e.to_string())?;
    let cargo_path = generation.join("cargo.json");
    cargo_output(
        &root,
        &[
            "metadata".into(),
            "--format-version".into(),
            "1".into(),
            "--locked".into(),
        ],
        &cargo_path,
    )?;
    let cargo = read_json(&cargo_path)?;
    let before_sources = sources(&cargo, &root)?;
    let before_rustc = rustc_identity()?;
    let before_environment = build_environment();
    let before_config = cargo_config(&root)?;
    let target = PathBuf::from(string(&cargo, "target_directory")?);
    fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    if !target.is_absolute() || target.canonicalize().map_err(|e| e.to_string())? != target {
        return Err("Cargo reported a noncanonical target directory".into());
    }
    // A validate-profile profile compiles every selection in ONE listing; the
    // others list each selection by itself. Either way each selection's
    // binaries-metadata is the part of its listing that its Cargo selectors
    // name, and a listing of one selection must keep every executable Cargo
    // listed for it, which checks the matcher against Cargo on every run.
    for (index, (listing, keys)) in listings(profile, &selections)?.into_iter().enumerate() {
        let mut args = vec![
            "nextest".into(),
            "list".into(),
            "--locked".into(),
            "--message-format".into(),
            "json".into(),
            "--list-type".into(),
            "binaries-only".into(),
        ];
        if let Some(cargo_profile) = cargo_profile {
            args.extend(["--cargo-profile".into(), cargo_profile.into()]);
        }
        args.extend(listing.iter().cloned());
        let listing_path = generation.join(format!("listing-{index}.json"));
        cargo_output(&root, &args, &listing_path)?;
        let listed = read_json(&listing_path)?;
        for key in keys {
            let metadata = selection_metadata(&listed, &cargo, &selections[&key])?;
            require_listing_kept(&listing, &selections[&key], &listed, &metadata)?;
            fs::write(
                generation.join(format!("{key}.json")),
                serde_json::to_vec(&metadata).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    let needs_guests = has_test_selection(selections.values(), "hermit_modes");
    let needs_record_workloads = has_test_selection(selections.values(), "record_replay");
    let guest_path = generation.join("guests.jsonl");
    if needs_guests || needs_record_workloads {
        let mut args = ["build", "--locked"].map(String::from).to_vec();
        if let Some(cargo_profile) = cargo_profile {
            args.extend(["--profile".into(), cargo_profile.into()]);
        }
        args.extend(
            [
                "-p",
                "hermetic_infra_hermit_tests",
                "--bins",
                "--message-format=json",
            ]
            .map(String::from),
        );
        cargo_output(&root, &args, &guest_path)?;
    }
    let cpu_wrapper_path = generation.join("cpu-wrapper.jsonl");
    prepare_cpu_wrapper(&root, &cpu_wrapper_path)?;
    // Hash only after every Cargo selection has completed. Shared paths may be
    // rebuilt by another selection; every published record names the final files.
    let mut recorded_selections = BTreeMap::new();
    for (key, build_args) in selections {
        let path = generation.join(format!("{key}.json"));
        let (binaries, runtime_files) = metadata_binaries(&read_json(&path)?, &cargo, &target)?;
        let metadata = file_identity(&path, false)?;
        let args = [
            "nextest",
            "list",
            "--cargo-metadata",
            cargo_path.to_str().ok_or("non-UTF-8 Cargo metadata path")?,
            "--binaries-metadata",
            path.to_str().ok_or("non-UTF-8 Nextest metadata path")?,
            "--message-format",
            "json",
        ]
        .map(String::from);
        // Nextest itself must accept every complete metadata pair after the
        // final build; this enumerates tests and never invokes the compiler.
        cargo_output(&root, &args, &generation.join(format!("{key}.tests.json")))?;
        recorded_selections.insert(
            key,
            SelectionRecord {
                build_args,
                metadata,
                binaries,
                runtime_files,
            },
        );
    }
    let guests = if needs_guests {
        guest_executables(
            &fs::read_to_string(&guest_path).map_err(|e| e.to_string())?,
            &cargo,
            &target,
        )?
    } else {
        vec![]
    };
    let record_workloads = if needs_record_workloads {
        Some(prepare_record_workloads(
            &root,
            &generation,
            &cargo,
            &target,
        )?)
    } else {
        None
    };
    let record = PreparedRecord {
        schema: RECORD_SCHEMA,
        repository: root.clone(),
        target: target.clone(),
        sources: before_sources,
        rustc: before_rustc,
        cargo_config: before_config,
        build_environment: before_environment,
        cargo_metadata: file_identity(&cargo_path, false)?,
        selections: recorded_selections,
        guests,
        record_workloads,
        cpu_wrapper: CpuWrapperRecord {
            executable: cpu_wrapper_artifact(
                &fs::read_to_string(&cpu_wrapper_path).map_err(|e| e.to_string())?,
                &cargo,
                &target,
            )?,
            metadata: file_identity(&cpu_wrapper_path, false)?,
        },
    };
    let checked_cargo = verify_record(&root, &record)?;
    for selection in record.selections.values() {
        verify_selection(&record, &checked_cargo, &selection.build_args)?;
    }
    verify_guests(&record)?;
    verify_record_workloads(&record, &checked_cargo)?;
    let staging = generation.join("record.json");
    fs::write(
        &staging,
        serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    // A single rename publishes all selections together. Failure leaves the
    // previous record in place; it remains usable only while its recorded
    // inputs still match, since earlier Cargo commands may have changed them.
    fs::rename(staging, artifacts.root.join("current.json")).map_err(|e| e.to_string())?;
    eprintln!(
        "prepared-nextest: published {} selections, {} hermit_modes Cargo guests and {} record workloads for {profile}",
        record.selections.len(),
        record.guests.len(),
        record
            .record_workloads
            .as_ref()
            .map_or(0, |w| w.workloads.len()),
    );
    Ok(())
}

pub fn run(
    root: &Path,
    operation: &str,
    config: Option<&Path>,
    args: &[String],
    launch: Option<&Path>,
    budget_map: Option<(&Path, &str)>,
) -> Result<i32, String> {
    if !matches!(operation, "run" | "list") {
        return Err(format!("unsupported Nextest operation {operation}"));
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let parsed = split_arguments(args)?;
    if let Ok(declared) = std::env::var(SELECTION_ENV) {
        let declared: Vec<String> =
            serde_json::from_str(&declared).map_err(|e| format!("invalid {SELECTION_ENV}: {e}"))?;
        if declared != parsed.build {
            return Err("actual Cargo selectors differ from the committed node's selection".into());
        }
    } else if std::env::var(REQUIRED_ENV).as_deref() == Ok("1") {
        return Err(format!("official Nextest consumer has no {SELECTION_ENV}"));
    }
    let artifacts = LockedArtifacts::open(&root, false)?;
    let record = artifacts.current()?;
    let cargo = verify_record(&root, &record)?;
    let selection = verify_selection(&record, &cargo, &parsed.build)?;
    let guests = verify_guests(&record)?;
    let record_workloads = verify_record_workloads(&record, &cargo)?;
    if budget_map.is_none() {
        if let Some(path) = launch {
            crate::nextest_cohort::verify(path)?;
        }
    }
    if let Some((path, digest)) = budget_map {
        if operation != "run" {
            return Err("resolved budgets bind a run, not an inventory query".into());
        }
        let file =
            fs::File::open(path).map_err(|e| format!("cannot read resolved run budgets: {e}"))?;
        let mut bytes = Vec::new();
        file.take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("resolved run budget map exceeds its size bound".into());
        }
        if format!("{:x}", Sha256::digest(&bytes)) != digest {
            return Err("resolved Nextest budget map changed before execution".into());
        }
        let map: crate::nextest_cpu::ResolvedBudgets =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        map.validate()?;
        let actual: Option<crate::nextest_cpu::BudgetContext> =
            serde_json::from_str(&budget_context(&root, args, launch)?)
                .map_err(|e| e.to_string())?;
        if actual.as_ref() != Some(&map.context) {
            return Err("Nextest source/build/launch context changed after budget resolution; resolve again before running".into());
        }
    }
    let mut command = Command::new("cargo");
    command.arg("nextest");
    if let Some(config) = config {
        command.arg("--config-file").arg(config);
    }
    command
        .arg(operation)
        .arg("--cargo-metadata")
        .arg(&record.cargo_metadata.path)
        .arg("--binaries-metadata")
        .arg(&selection.metadata.path)
        .args(&parsed.runtime)
        .env(
            GUESTS_ENV,
            serde_json::to_string(&guests).map_err(|e| e.to_string())?,
        )
        .current_dir(&root);
    // Never inherit an unverified population from the caller's environment.
    command.env_remove(record_workloads::PREPARED_ENV);
    if let Some(workloads) = record_workloads {
        command.env(record_workloads::PREPARED_ENV, workloads);
    }
    let status = command
        .status()
        .map_err(|e| format!("cannot run prepared Nextest: {e}"))?;
    // Keep the shared lock until the child has joined. Preserve Nextest's
    // status; a refusal never becomes an empty or successful test report.
    drop(artifacts);
    Ok(exit_code(status))
}

pub fn assert_profile(root: &Path, profile: &str) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let artifacts = LockedArtifacts::open(&root, false)?;
    let record = artifacts.current()?;
    let cargo = verify_record(&root, &record)?;
    for args in profile_selections(&root, profile)?.values() {
        verify_selection(&record, &cargo, args)?;
    }
    verify_guests(&record)?;
    verify_record_workloads(&record, &cargo)?;
    Ok(())
}

pub fn executable(root: &Path, package: &str, name: &str) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let artifacts = LockedArtifacts::open(&root, false)?;
    let record = artifacts.current()?;
    let cargo = verify_record(&root, &record)?;
    let ids = cargo["packages"]
        .as_array()
        .ok_or("missing Cargo packages")?
        .iter()
        .filter(|item| item["name"].as_str() == Some(package))
        .map(|item| string(item, "id"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if ids.len() != 1 {
        return Err(format!(
            "prepared package {package} is missing or ambiguous"
        ));
    }
    let required_selection = if std::env::var(REQUIRED_ENV).as_deref() == Ok("1") {
        let raw = std::env::var(SELECTION_ENV)
            .map_err(|_| "prepared executable lookup has no declared build selection")?;
        let args: Vec<String> = serde_json::from_str(&raw)
            .map_err(|e| format!("invalid executable build selection: {e}"))?;
        Some(verify_selection(&record, &cargo, &args)?)
    } else {
        None
    };
    let selections = match required_selection.as_ref() {
        Some(selection) => vec![selection],
        None => record.selections.values().collect(),
    };
    let mut matches = BTreeSet::new();
    for selection in selections {
        for binary in &selection.binaries {
            if ids.contains(binary.package_id.as_str())
                && binary.binary_name == name
                && binary.kind == "test"
            {
                verify_selection(&record, &cargo, &selection.build_args)?;
                matches.insert(binary.executable.path.clone());
            }
        }
    }
    if matches.len() != 1 {
        return Err(format!(
            "expected one prepared {package} test target {name}, found {}",
            matches.len()
        ));
    }
    Ok(matches.into_iter().next().unwrap())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(String::from).collect()
    }

    fn selections(texts: &[&str]) -> BTreeMap<String, Vec<String>> {
        texts
            .iter()
            .map(|text| (selection_key(&args(text)), args(text)))
            .collect()
    }

    #[test]
    fn a_changed_prepared_input_names_each_field_that_moved() {
        let root = std::env::temp_dir().join(format!(
            "hermit-prepared-input-change-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("one"), "1").unwrap();
        fs::write(root.join("two"), "22").unwrap();
        let one = fs::symlink_metadata(root.join("one")).unwrap();
        let two = fs::symlink_metadata(root.join("two")).unwrap();

        assert!(metadata_changes(&one, &one).is_empty());
        let changed = metadata_changes(&one, &two);
        assert_eq!(
            changed[..2],
            [
                format!("ino {} -> {}", one.ino(), two.ino()),
                "size 1 -> 2".to_string(),
            ]
        );
        assert!(!changed.iter().any(|field| field.starts_with("mode ")));
        assert_eq!(changed.last().unwrap(), "nlink 1 -> 1");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn one_selection_is_its_own_union() {
        let only = selections(&["-p hermit-detcore --lib"]);
        assert_eq!(
            unified_selection(&only).unwrap(),
            args("-p hermit-detcore --lib")
        );
    }

    #[test]
    fn several_selections_unify_to_the_workspace_with_qualified_features() {
        let several = selections(&[
            "-p hermit --features third-party-backends,kvm-native-test-support --lib --bins",
            "-p hermit-detcore --test tests_misc",
            "-p hermit --features third-party-backends,kvm-execution-tests --lib --test cli",
            "--workspace --exclude hermit",
        ]);
        assert_eq!(
            unified_selection(&several).unwrap(),
            args(
                "--workspace --all-targets --features hermit/kvm-execution-tests,hermit/kvm-native-test-support,hermit/third-party-backends"
            )
        );
        let unfeatured = selections(&["-p a --lib", "-p b --test t"]);
        assert_eq!(
            unified_selection(&unfeatured).unwrap(),
            args("--workspace --all-targets")
        );
    }

    #[test]
    fn only_validate_profile_profiles_unify_their_listing() {
        let several = selections(&[
            "-p hermit-detcore --test tests_misc",
            "-p hermit --features third-party-backends --test hermit_modes",
        ]);
        for profile in ["full", "hosted-portable"] {
            let plan = listings(profile, &several).unwrap();
            assert_eq!(plan.len(), 1, "{profile}");
            assert_eq!(plan[0].0, unified_selection(&several).unwrap());
            assert_eq!(plan[0].1.len(), 2);
        }
        // The dev-profile producers build far less than the workspace, so
        // each of their selections keeps a listing of its own.
        for profile in ["privileged", "super", "quick"] {
            let plan = listings(profile, &several).unwrap();
            assert_eq!(plan.len(), 2, "{profile}");
            for (listing, keys) in plan {
                assert_eq!(keys, [selection_key(&listing)]);
            }
            assert!(!unifies(profile));
        }
    }

    #[test]
    fn a_union_refuses_what_it_cannot_reproduce_exactly() {
        for refused in [
            "--workspace --features third-party-backends",
            "-p a -p b --features f",
            "-p a --features b/f",
            "-p a --release",
            "-p a --bin one",
            "-p a --all-features",
            "--lib",
            "-p a --workspace",
            "-p a --exclude b",
        ] {
            let pair = selections(&[refused, "-p z --lib"]);
            assert!(unified_selection(&pair).is_err(), "{refused} was unified");
        }
        assert!(unified_selection(&BTreeMap::new()).is_err());
    }

    /// Two members: `a` has a library, a tested and an untested binary and two
    /// integration tests; `b` has a library and an integration test named like
    /// one of `a`'s. The unified listing names every one of them.
    fn membership_fixture() -> (Value, Value) {
        let target = |name: &str, kind: &str, test: bool| serde_json::json!({"name": name, "kind": [kind], "test": test});
        let cargo = serde_json::json!({
            "workspace_members": ["id-a", "id-b", "id-pm"],
            "packages": [
                {"name": "a", "id": "id-a", "targets": [
                    target("a", "lib", true), target("tool", "bin", true),
                    target("guest", "bin", false), target("t1", "test", true),
                    target("t2", "test", true), target("build-script-build", "custom-build", false)
                ]},
                {"name": "b", "id": "id-b", "targets": [
                    target("b", "lib", true), target("t1", "test", true)
                ]},
                {"name": "pm", "id": "id-pm", "targets": [target("pm", "proc-macro", true)]},
                {"name": "dep", "id": "id-dep", "targets": [target("dep", "lib", true)]}
            ]
        });
        let binary = |id: &str, package: &str, name: &str, kind: &str| {
            (
                id.to_string(),
                serde_json::json!({"binary-id": id, "package-id": package,
                    "binary-name": name, "kind": kind, "build-platform": "target",
                    "binary-path": format!("/target/{id}")}),
            )
        };
        let binaries = [
            binary("a", "id-a", "a", "lib"),
            binary("a::bin/tool", "id-a", "tool", "bin"),
            binary("a::bin/guest", "id-a", "guest", "bin"),
            binary("a::t1", "id-a", "t1", "test"),
            binary("a::t2", "id-a", "t2", "test"),
            binary("b", "id-b", "b", "lib"),
            binary("b::t1", "id-b", "t1", "test"),
            binary("pm", "id-pm", "pm", "proc-macro"),
        ]
        .into_iter()
        .collect::<serde_json::Map<_, _>>();
        let unified = serde_json::json!({
            "rust-build-meta": {"target-directory": "/target",
                "non-test-binaries": {"id-a": [{"name": "tool", "path": "/target/tool"}],
                                      "id-b": [{"name": "bb", "path": "/target/bb"}]}},
            "rust-binaries": binaries
        });
        (unified, cargo)
    }

    fn members(unified: &Value, cargo: &Value, selection: &str) -> Result<Vec<String>, String> {
        let metadata = selection_metadata(unified, cargo, &args(selection))?;
        Ok(metadata["rust-binaries"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect())
    }

    #[test]
    fn selection_membership_follows_cargo_target_selection() {
        let (unified, cargo) = membership_fixture();
        let cases: &[(&str, &[&str])] = &[
            ("-p a --lib", &["a"]),
            ("-p pm --lib", &["pm"]),
            ("--workspace --exclude a --exclude b", &["pm"]),
            ("-p a --test t1", &["a::t1"]),
            ("-p a -p b --test t1", &["a::t1", "b::t1"]),
            ("-p a --bins", &["a::bin/guest", "a::bin/tool"]),
            ("-p a --lib --bins", &["a", "a::bin/guest", "a::bin/tool"]),
            // No target selector: every target whose manifest `test` flag is
            // set, so the untested guest binary is not a member.
            (
                "--workspace --exclude b --exclude pm",
                &["a", "a::bin/tool", "a::t1", "a::t2"],
            ),
            ("-p a --tests", &["a", "a::bin/tool", "a::t1", "a::t2"]),
            (
                "--workspace --all-targets --exclude pm",
                &[
                    "a",
                    "a::bin/guest",
                    "a::bin/tool",
                    "a::t1",
                    "a::t2",
                    "b",
                    "b::t1",
                ],
            ),
        ];
        for (selection, expected) in cases {
            assert_eq!(
                members(&unified, &cargo, selection).unwrap(),
                expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "{selection}"
            );
        }
        let runtime = selection_metadata(&unified, &cargo, &args("-p a --lib")).unwrap();
        assert_eq!(
            runtime["rust-build-meta"]["non-test-binaries"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["id-a"]
        );
    }

    #[test]
    fn a_one_selection_listing_must_keep_every_executable_cargo_listed() {
        let (unified, cargo) = membership_fixture();
        let all = args("--workspace --all-targets");
        let kept = selection_metadata(&unified, &cargo, &all).unwrap();
        require_listing_kept(&all, &all, &unified, &kept).unwrap();
        // The matcher keeping less than Cargo listed is a disagreement.
        let lib = args("-p a --lib");
        let fewer = selection_metadata(&unified, &cargo, &lib).unwrap();
        let error = require_listing_kept(&lib, &lib, &unified, &fewer).unwrap_err();
        assert!(error.contains("drops executables Cargo listed"), "{error}");
        // A subset of a unified listing is expected and not checked here.
        require_listing_kept(&all, &lib, &unified, &fewer).unwrap();
    }

    #[test]
    fn selection_membership_refuses_an_empty_or_unknown_selection() {
        let (unified, cargo) = membership_fixture();
        for refused in [
            "-p a --test missing",
            "-p b --bins",
            "-p dep --lib",
            "-p nonexistent --lib",
        ] {
            assert!(
                members(&unified, &cargo, refused).is_err(),
                "{refused} was accepted"
            );
        }
    }

    struct Fixture {
        root: PathBuf,
        target: PathBuf,
        cargo: Value,
        metadata: Value,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hermit-prepared-nextest-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let target = root.join("custom-target");
            fs::create_dir(&target).unwrap();
            let executable = target.join("fixture-test");
            fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            let package = format!("path+file://{}#fixture@1.0.0", root.display());
            let cargo = serde_json::json!({"target_directory": target, "packages": [{
                "name": "fixture", "id": package, "source": null,
                "manifest_path": root.join("Cargo.toml"),
                "targets": [{"name": "fixture", "kind": ["lib"]}]
            }]});
            let metadata = serde_json::json!({"rust-build-meta": {
                "target-directory": target, "non-test-binaries": {}
            }, "rust-binaries": {"fixture": {
                "binary-id": "fixture", "package-id": package, "binary-name": "fixture",
                "kind": "lib", "build-platform": "target", "binary-path": executable
            }}});
            Self {
                root,
                target,
                cargo,
                metadata,
            }
        }
        fn selection(&self) -> (PreparedRecord, Vec<String>) {
            let path = self.root.join("nextest.json");
            fs::write(&path, serde_json::to_vec(&self.metadata).unwrap()).unwrap();
            let (binaries, runtime_files) =
                metadata_binaries(&self.metadata, &self.cargo, &self.target).unwrap();
            let args = vec!["-p".into(), "fixture".into(), "--lib".into()];
            let selected = SelectionRecord {
                build_args: args.clone(),
                metadata: file_identity(&path, false).unwrap(),
                binaries,
                runtime_files,
            };
            let cargo = self.root.join("cargo.json");
            fs::write(&cargo, serde_json::to_vec(&self.cargo).unwrap()).unwrap();
            let cpu_wrapper = self.cpu_wrapper();
            (
                PreparedRecord {
                    schema: RECORD_SCHEMA,
                    repository: self.root.clone(),
                    target: self.target.clone(),
                    sources: BTreeMap::new(),
                    rustc: String::new(),
                    cargo_config: BTreeMap::new(),
                    build_environment: BTreeMap::new(),
                    cargo_metadata: file_identity(&cargo, false).unwrap(),
                    selections: BTreeMap::from([(selection_key(&args), selected)]),
                    guests: vec![],
                    record_workloads: None,
                    cpu_wrapper,
                },
                args,
            )
        }

        fn cpu_wrapper(&self) -> CpuWrapperRecord {
            let executable = self.target.join(CPU_WRAPPER_TARGET);
            fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            let metadata = self.root.join("cpu-wrapper.jsonl");
            fs::write(
                &metadata,
                serde_json::to_vec(&serde_json::json!({
                    "reason": "compiler-artifact", "package_id": "wrapper-package",
                    "target": {"name": CPU_WRAPPER_TARGET, "kind": ["bin"]},
                    "profile": {"test": false}, "executable": executable,
                }))
                .unwrap(),
            )
            .unwrap();
            CpuWrapperRecord {
                metadata: file_identity(&metadata, false).unwrap(),
                executable: file_identity(&executable, true).unwrap(),
            }
        }

        fn wrapper_cargo(&self) -> Value {
            let mut cargo = self.cargo.clone();
            cargo["packages"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "name": CPU_WRAPPER_PACKAGE, "id": "wrapper-package",
                    "targets": [{"name": CPU_WRAPPER_TARGET, "kind": ["bin"]}],
                }));
            cargo
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).into()).collect()
    }

    fn record_fixture(fixture: &Fixture) -> (PreparedRecord, Value) {
        let (mut record, _) = fixture.selection();
        record.selections.values_mut().next().unwrap().build_args =
            argv(&["-p", "hermit", "--test", "record_replay"]);
        let directory = fixture.root.join("record-workloads");
        fs::create_dir(&directory).unwrap();
        let mut targets = Vec::new();
        let mut events = Vec::new();
        let mut workloads = Vec::new();
        for name in record_workloads::names() {
            let source = fixture.root.join(record_workloads::source(name).unwrap());
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(&source, "source witness\n").unwrap();
            let path = directory.join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            let cargo_origin = if let Some((_, target, _)) = record_workloads::RUST_SOURCES
                .into_iter()
                .find(|(alias, _, _)| *alias == name)
            {
                let origin = fixture.target.join(target);
                fs::copy(&path, &origin).unwrap();
                let target =
                    serde_json::json!({"name": target, "kind": ["bin"], "src_path": source});
                targets.push(target.clone());
                events.push(serde_json::json!({"reason":"compiler-artifact", "package_id":"record-package", "target":target, "profile":{"test":false}, "executable":origin}));
                Some(file_identity(&origin, true).unwrap())
            } else {
                None
            };
            workloads.push(RecordWorkload {
                name: name.into(),
                source: file_identity(&source, false).unwrap(),
                executable: file_identity(&path, true).unwrap(),
                cargo_origin,
            });
        }
        events.push(serde_json::json!({"reason":"build-finished", "success":true}));
        let events_path = fixture.root.join("guests.jsonl");
        fs::write(
            &events_path,
            events
                .iter()
                .map(|event| format!("{event}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let mut cargo = fixture.cargo.clone();
        cargo["packages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "name":record_workloads::PACKAGE, "id":"record-package", "source":null,
                "manifest_path":fixture.root.join("tests/Cargo.toml"), "targets":targets,
            }));
        record.record_workloads = Some(RecordWorkloads {
            cargo_events: file_identity(&events_path, false).unwrap(),
            compiler: file_identity(&record_workloads::c_compiler().unwrap(), true).unwrap(),
            c_flags: record_workloads::C_FLAGS.map(String::from).into(),
            workloads,
        });
        (record, cargo)
    }

    #[test]
    fn record_workload_verified_producer_envelope_reaches_the_complete_consumer() {
        let fixture = Fixture::new();
        let (record, cargo) = record_fixture(&fixture);
        let raw = verify_record_workloads(&record, &cargo).unwrap().unwrap();
        let population = record_workloads::consume_prepared(true, Some(&raw))
            .unwrap()
            .unwrap();
        assert_eq!(population.len(), 64);
        assert_eq!(
            population.iter().map(|w| w.name).collect::<BTreeSet<_>>(),
            record_workloads::names().collect()
        );
        assert_eq!(
            population
                .iter()
                .find(|w| w.name == "rs_clock_gettime")
                .unwrap()
                .path,
            fixture.root.join("record-workloads/rs_clock_gettime")
        );
        assert!(record.guests.is_empty());
        assert_eq!(cargo_guests::CARGO_GUEST_BINARIES.len(), 21);
    }

    #[test]
    fn record_workload_selection_and_population_are_required_without_defaults() {
        let fixture = Fixture::new();
        let (record, cargo) = record_fixture(&fixture);
        let mut missing = record.clone();
        missing.record_workloads = None;
        assert!(
            verify_record_workloads(&missing, &cargo)
                .unwrap_err()
                .contains("no prepared")
        );
        let mut unselected = record.clone();
        unselected.selections.clear();
        assert!(
            verify_record_workloads(&unselected, &cargo)
                .unwrap_err()
                .contains("without record_replay")
        );
        for duplicate in [false, true] {
            let mut changed = record.clone();
            let workloads = &mut changed.record_workloads.as_mut().unwrap().workloads;
            if duplicate {
                workloads.push(workloads[0].clone());
            } else {
                workloads.pop();
            }
            assert!(verify_record_workloads(&changed, &cargo).is_err());
        }
        let mut old_schema = record.clone();
        old_schema.schema = 2;
        assert!(
            verify_record(&fixture.root, &old_schema)
                .unwrap_err()
                .contains("schema")
        );
    }

    #[test]
    fn record_workload_refuses_recipe_source_origin_and_retained_artifact_changes() {
        let fixture = Fixture::new();
        let (record, cargo) = record_fixture(&fixture);
        let mut changed = record.clone();
        changed.record_workloads.as_mut().unwrap().c_flags[0] = "-O2".into();
        assert!(verify_record_workloads(&changed, &cargo).is_err());
        let mut changed = record.clone();
        changed.record_workloads.as_mut().unwrap().compiler.sha256 = "wrong".into();
        assert!(verify_record_workloads(&changed, &cargo).is_err());
        let mut changed = record.clone();
        changed.record_workloads.as_mut().unwrap().workloads[0]
            .source
            .path = fixture.root.join("wrong.c");
        assert!(verify_record_workloads(&changed, &cargo).is_err());
        let mut changed = record.clone();
        changed.record_workloads.as_mut().unwrap().workloads[0]
            .executable
            .path = fixture.target.join("outside-generation");
        assert!(verify_record_workloads(&changed, &cargo).is_err());
        let mut changed = record.clone();
        changed
            .record_workloads
            .as_mut()
            .unwrap()
            .workloads
            .iter_mut()
            .find(|w| w.cargo_origin.is_some())
            .unwrap()
            .cargo_origin = None;
        assert!(verify_record_workloads(&changed, &cargo).is_err());
        let original = &record.record_workloads.as_ref().unwrap().workloads[0].executable;
        fs::write(&original.path, "#!/bin/sh\nexit 1\n").unwrap();
        assert!(
            verify_record_workloads(&record, &cargo)
                .unwrap_err()
                .contains("changed")
        );
    }

    #[test]
    fn record_workload_generation_lock_remains_held_until_consumer_returns() {
        let fixture = Fixture::new();
        let consumer = LockedArtifacts::open(&fixture.root, false).unwrap();
        let competing = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(consumer.root.join("lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(competing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        );
        let root = fixture.root.clone();
        let (starting_tx, starting_rx) = std::sync::mpsc::channel();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            starting_tx.send(()).unwrap();
            let _lock = LockedArtifacts::open(&root, true).unwrap();
            locked_tx.send(()).unwrap();
        });
        starting_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            locked_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(consumer);
        locked_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        producer.join().unwrap();
    }

    #[test]
    fn nextest_runtime_profile_and_literal_filter_arguments_are_preserved() {
        let parsed = split_arguments(&argv(&[
            "--profile",
            "ci",
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "cli",
            "-j",
            "1",
            "-E",
            "test(/a b/)",
            "--",
            "--ignored",
            "--test",
            "literal;$(false)",
        ]))
        .unwrap();
        assert_eq!(
            parsed.build,
            argv(&[
                "-p",
                "hermit",
                "--features",
                "third-party-backends",
                "--test",
                "cli"
            ])
        );
        assert_eq!(
            parsed.runtime,
            argv(&[
                "--profile",
                "ci",
                "-j",
                "1",
                "-E",
                "test(/a b/)",
                "--",
                "--ignored",
                "--test",
                "literal;$(false)"
            ])
        );
        assert!(split_arguments(&argv(&["--cargo-profile", "ci"])).is_err());
        assert!(split_arguments(&argv(&["--release"])).is_err());
    }

    #[test]
    fn prepared_cpu_wrapper_requires_exact_normal_package_target_and_unique_artifact() {
        let f = Fixture::new();
        let wrapper = f.cpu_wrapper();
        let cargo = f.wrapper_cargo();
        let events = fs::read_to_string(&wrapper.metadata.path).unwrap();
        assert_eq!(
            cpu_wrapper_artifact(&events, &cargo, &f.target).unwrap(),
            wrapper.executable
        );
        for (field, value) in [
            ("package_id", serde_json::json!("another-package")),
            (
                "target",
                serde_json::json!({"name": CPU_WRAPPER_TARGET, "kind": ["test"]}),
            ),
            ("profile", serde_json::json!({"test": true})),
            ("executable", Value::Null),
        ] {
            let mut event: Value = serde_json::from_str(&events).unwrap();
            event[field] = value;
            assert!(
                cpu_wrapper_artifact(&event.to_string(), &cargo, &f.target).is_err(),
                "accepted {field}"
            );
        }
        for bad in [
            String::new(),
            "not JSON".into(),
            format!("{events}\n{events}\n"),
        ] {
            assert!(cpu_wrapper_artifact(&bad, &cargo, &f.target).is_err());
        }
        let mut ambiguous = cargo.clone();
        ambiguous["packages"]
            .as_array_mut()
            .unwrap()
            .push(cargo["packages"][1].clone());
        assert!(cpu_wrapper_artifact(&events, &ambiguous, &f.target).is_err());
        assert!(cpu_wrapper_artifact(&events, &cargo, &f.root.join("other-target")).is_err());
    }

    #[test]
    fn prepared_cpu_wrapper_refuses_missing_stale_and_nonexecutable_bytes_without_rebuilding() {
        for mode in ["metadata", "binary", "changed", "mode"] {
            let f = Fixture::new();
            let (record, _) = f.selection();
            let cargo = f.wrapper_cargo();
            assert!(verify_cpu_wrapper(&record, &cargo).is_ok());
            match mode {
                "metadata" => fs::remove_file(&record.cpu_wrapper.metadata.path).unwrap(),
                "binary" => fs::remove_file(&record.cpu_wrapper.executable.path).unwrap(),
                "changed" => {
                    fs::write(&record.cpu_wrapper.executable.path, "#!/bin/sh\nexit 23\n").unwrap()
                }
                "mode" => fs::set_permissions(
                    &record.cpu_wrapper.executable.path,
                    fs::Permissions::from_mode(0o644),
                )
                .unwrap(),
                _ => unreachable!(),
            }
            assert!(
                verify_cpu_wrapper(&record, &cargo).is_err(),
                "accepted {mode}"
            );
        }
    }

    #[test]
    fn prepared_cpu_wrapper_cannot_be_omitted_from_a_record() {
        let f = Fixture::new();
        let (record, _) = f.selection();
        let mut value = serde_json::to_value(record).unwrap();
        value.as_object_mut().unwrap().remove("cpu_wrapper");
        assert!(serde_json::from_value::<PreparedRecord>(value).is_err());
    }

    #[test]
    fn unknown_or_incomplete_build_options_cannot_fall_through_to_runtime() {
        for args in [
            vec!["--features"],
            vec!["--features", "--lib"],
            vec!["--test="],
            vec!["--future-build-setting"],
            vec!["--config", "build.target='other'"],
            vec!["--manifest-path=other/Cargo.toml"],
            vec!["--target-dir", "/other"],
        ] {
            assert!(split_arguments(&argv(&args)).is_err(), "accepted {args:?}");
        }
        assert_eq!(
            split_arguments(&argv(&["--target=x86_64-unknown-linux-gnu", "--lib"]))
                .unwrap()
                .build,
            argv(&["--target", "x86_64-unknown-linux-gnu", "--lib"])
        );
    }

    #[test]
    fn prepared_executable_bytes_are_checked_even_at_the_same_path() {
        let f = Fixture::new();
        let (record, args) = f.selection();
        assert!(verify_selection(&record, &f.cargo, &args).is_ok());
        fs::write(f.target.join("fixture-test"), "#!/bin/sh\nexit 1\n").unwrap();
        assert!(
            verify_selection(&record, &f.cargo, &args)
                .unwrap_err()
                .contains("changed")
        );
    }

    #[test]
    fn missing_metadata_missing_binary_and_wrong_target_all_refuse() {
        for mode in ["metadata", "binary", "target"] {
            let f = Fixture::new();
            let (mut record, args) = f.selection();
            match mode {
                "metadata" => {
                    fs::remove_file(&record.selections[&selection_key(&args)].metadata.path)
                        .unwrap()
                }
                "binary" => fs::remove_file(f.target.join("fixture-test")).unwrap(),
                "target" => record.target = f.root.join("unrelated-target"),
                _ => unreachable!(),
            }
            assert!(
                verify_selection(&record, &f.cargo, &args).is_err(),
                "accepted {mode}"
            );
        }
    }

    #[test]
    fn metadata_cannot_substitute_another_package_or_executable_identity() {
        let f = Fixture::new();
        for field in ["package-id", "binary-id", "binary-name", "kind"] {
            let mut metadata = f.metadata.clone();
            metadata["rust-binaries"]["fixture"][field] = "other".into();
            assert!(
                metadata_binaries(&metadata, &f.cargo, &f.target).is_err(),
                "accepted changed {field}"
            );
        }
        let mut metadata = f.metadata.clone();
        metadata["rust-binaries"]["other"] = metadata["rust-binaries"]["fixture"].clone();
        metadata["rust-binaries"]["other"]["binary-id"] = "other".into();
        assert!(
            metadata_binaries(&metadata, &f.cargo, &f.target)
                .unwrap_err()
                .contains("ambiguous executables")
        );
    }

    #[test]
    fn library_crate_types_preserve_the_exact_package_and_target_identity() {
        let mut fixture = Fixture::new();
        // Cargo reports this real detcore-dbt shape as cdylib+rlib; Nextest
        // correctly describes its test harness as a library test executable.
        fixture.cargo["packages"][0]["targets"][0]["kind"] = serde_json::json!(["cdylib", "rlib"]);
        assert!(metadata_binaries(&fixture.metadata, &fixture.cargo, &fixture.target).is_ok());
        fixture.cargo["packages"][0]["targets"][0]["name"] = "another_library".into();
        assert!(metadata_binaries(&fixture.metadata, &fixture.cargo, &fixture.target).is_err());
        fixture.cargo["packages"][0]["targets"][0]["name"] = "fixture".into();
        fixture.cargo["packages"][0]["targets"][0]["kind"] = serde_json::json!(["custom-build"]);
        assert!(metadata_binaries(&fixture.metadata, &fixture.cargo, &fixture.target).is_err());
    }

    #[test]
    fn a_second_build_selection_must_have_its_own_record() {
        let f = Fixture::new();
        let (mut record, args) = f.selection();
        let other = argv(&["-p", "fixture", "--lib", "--all-features"]);
        assert!(verify_selection(&record, &f.cargo, &other).is_err());
        let mut selected = record.selections[&selection_key(&args)].clone();
        selected.build_args = other.clone();
        record.selections.insert(selection_key(&other), selected);
        assert!(verify_selection(&record, &f.cargo, &args).is_ok());
        assert!(verify_selection(&record, &f.cargo, &other).is_ok());
        record
            .selections
            .get_mut(&selection_key(&other))
            .unwrap()
            .build_args = args;
        assert!(verify_selection(&record, &f.cargo, &other).is_err());
    }

    #[test]
    fn symlink_or_nonexecutable_artifacts_are_refused() {
        let f = Fixture::new();
        let binary = f.target.join("fixture-test");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(metadata_binaries(&f.metadata, &f.cargo, &f.target).is_err());
        fs::rename(&binary, f.target.join("real-test")).unwrap();
        std::os::unix::fs::symlink(f.target.join("real-test"), &binary).unwrap();
        assert!(metadata_binaries(&f.metadata, &f.cargo, &f.target).is_err());
    }

    #[test]
    fn changed_tracked_and_untracked_sources_change_the_identity() {
        let f = Fixture::new();
        fs::write(f.root.join(".gitignore"), "custom-target/\n").unwrap();
        fs::write(f.root.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "Cargo.toml", ".gitignore"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            git_bytes(&f.root, &args).unwrap();
        }
        let before = sources(&f.cargo, &f.root).unwrap();
        fs::write(f.root.join("Cargo.toml"), "[package]\nname='changed'\n").unwrap();
        assert_ne!(sources(&f.cargo, &f.root).unwrap(), before);
        git_bytes(&f.root, &["checkout", "--", "Cargo.toml"]).unwrap();
        assert_eq!(sources(&f.cargo, &f.root).unwrap(), before);
        fs::write(f.root.join("new-build-input"), "changed").unwrap();
        assert_ne!(sources(&f.cargo, &f.root).unwrap(), before);
        fs::remove_file(f.root.join("new-build-input")).unwrap();
        assert_eq!(sources(&f.cargo, &f.root).unwrap(), before);
        // Hermit's build script embeds HEAD even when an empty commit leaves
        // the complete source tree unchanged.
        git_bytes(
            &f.root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "same tree, different build identity",
            ],
        )
        .unwrap();
        assert_ne!(sources(&f.cargo, &f.root).unwrap(), before);
    }

    /// A disposable run summary must not move source identity, and a stray one
    /// still must.
    ///
    /// Validate run 1819 on hermit main ba3bfc9767 failed 26 nodes, 20 of them
    /// because `hermit run` wrote `.hermit-verify-summary-*` into the checkout
    /// root. This enumeration counts untracked, non-ignored files, so creating
    /// and dropping one during a run changed the identity under the prepared
    /// executables: 13 nodes refused "prepared executables are stale: source
    /// identity changed" and 6 more could not read a path that had vanished
    /// between enumeration and hashing. None of them ran a single test.
    ///
    /// The repair moves those summaries into `ignored/`, whose .gitignore entry
    /// already says everything under it "is disposable and must not taint
    /// source provenance". The SECOND HALF OF THIS TEST IS THE POINT: the fix
    /// must not be an exemption for the name. `scripts/validate.rs`
    /// checkout_attribution_bracket exists because run 1573 leaked these same
    /// files and its stated property is "No pathname is excused" -- so a stray
    /// summary anywhere the scanner still looks has to keep changing the
    /// identity, exactly as any other untracked file does.
    #[test]
    fn a_disposable_run_summary_is_not_source_but_a_stray_one_still_is() {
        let f = Fixture::new();
        fs::write(f.root.join(".gitignore"), "ignored/\n").unwrap();
        fs::write(f.root.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "Cargo.toml", ".gitignore"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            git_bytes(&f.root, &args).unwrap();
        }
        let before = sources(&f.cargo, &f.root).unwrap();

        // Positive: the summary where the repair puts it is invisible to source
        // accounting, both while it exists and after it is dropped. Either
        // transition is what refused the twenty nodes.
        let disposable = f.root.join("ignored");
        fs::create_dir_all(&disposable).unwrap();
        let summary = disposable.join(".hermit-verify-summary-fixture");
        fs::write(&summary, "summary").unwrap();
        assert_eq!(
            sources(&f.cargo, &f.root).unwrap(),
            before,
            "a disposable run summary tainted source identity"
        );
        fs::remove_file(&summary).unwrap();
        assert_eq!(
            sources(&f.cargo, &f.root).unwrap(),
            before,
            "dropping a disposable run summary tainted source identity"
        );

        // Negative, and it must stay this way: the same file name in the
        // scanned tree is still an untracked source. A repair that made this
        // pass too would have excused the pathname instead of moving the file.
        let stray = f.root.join(".hermit-verify-summary-fixture");
        fs::write(&stray, "summary").unwrap();
        assert_ne!(
            sources(&f.cargo, &f.root).unwrap(),
            before,
            "a stray summary in the scanned tree was excused rather than counted"
        );
        fs::remove_file(&stray).unwrap();
        assert_eq!(sources(&f.cargo, &f.root).unwrap(), before);
    }

    /// Resolve this repository's root the way git does, not by guessing.
    ///
    /// ⚠️ AN EARLIER VERSION TOOK THE FIRST ANCESTOR HOLDING A `.gitignore`.
    /// Adding `ci/.gitignore` — an ordinary thing to do — would have silently
    /// redirected every assertion below to that file instead, and they would
    /// all still have passed. Resolving by `.git` and then cross-checking
    /// against git's own answer removes the guess.
    fn repository_root() -> PathBuf {
        let candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|candidate| candidate.join(".git").exists())
            .expect("locating the repository root by .git")
            .to_path_buf();
        let output = crate::git_environment::git_command()
            .arg("-C")
            .arg(&candidate)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .expect("asking git for the work-tree root");
        assert!(
            output.status.success(),
            "git could not resolve the work tree"
        );
        let toplevel = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        assert_eq!(
            candidate.canonicalize().unwrap(),
            toplevel.canonicalize().unwrap(),
            "the resolved root disagrees with git's own"
        );
        candidate
    }

    fn is_ignored(root: &Path, relative: &str) -> bool {
        crate::git_environment::git_command()
            .arg("-C")
            .arg(root)
            .args(["check-ignore", "-q", relative])
            .status()
            .expect("running git check-ignore")
            .success()
    }

    /// This repository must ACTUALLY ignore `ignored/`, and must not exempt a
    /// run summary by name.
    ///
    /// ⚠️ TWO GAPS THIS CLOSES, BOTH FOUND BY A REVIEWER OF PR 3026. The
    /// fixture-based test above writes its own `.gitignore`, so it proves the
    /// mechanism and says nothing about this repository's configuration. And
    /// the earlier version of this test asserted only the ABSENCE of a by-name
    /// exemption. Between them, deleting `/ignored/` from the real file left
    /// both passing while the fix silently stopped working — the summaries
    /// would go back to being counted as prepared source.
    ///
    /// So this asks git rather than reading the file: `git check-ignore` is the
    /// same judgement `git ls-files --others --exclude-standard` makes, so it
    /// cannot drift from how the rule is spelled.
    #[test]
    fn the_repository_really_ignores_the_disposable_directory() {
        let root = repository_root();

        assert!(
            is_ignored(&root, "ignored/.hermit-verify-summary-probe"),
            "{}/.gitignore no longer ignores ignored/, so run summaries written \
             there are counted as prepared source again",
            root.display()
        );

        // Control: the check must discriminate. A tracked source path is not
        // ignored, so a version of `is_ignored` that answered yes to everything
        // would fail here.
        assert!(
            !is_ignored(&root, "ci/manifest-plan/src/nextest_binaries.rs"),
            "the ignore check answered yes for a tracked source file, so it is \
             not measuring anything"
        );

        // And the name itself must still not be exempted anywhere, so a stray
        // summary outside ignored/ keeps moving the identity.
        let text = fs::read_to_string(root.join(".gitignore")).expect("reading .gitignore");
        for line in text.lines() {
            let rule = line.split('#').next().unwrap_or_default().trim();
            assert!(
                !rule.contains("hermit-verify-summary")
                    && !rule.contains("hermit-backend-engagement-summary"),
                "{}/.gitignore exempts a run summary by name ({rule:?}); move the file into \
                 ignored/ instead of hiding it from source accounting",
                root.display()
            );
        }
        assert!(
            !is_ignored(&root, ".hermit-verify-summary-probe"),
            "a stray summary in the repository root is ignored, so a leak there \
             would no longer be visible to source accounting"
        );
    }

    /// The git repository-location variables this regression exports into a
    /// child test run. Listed here rather than borrowed from the code under
    /// test, so that shrinking the production list cannot also shrink the test.
    const INHERITED_LOCATION_VARIABLES: [&str; 8] = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ];

    /// Git for this regression's own bookkeeping: never steered by the
    /// environment the test itself was started in.
    fn bookkeeping_command(directory: &Path) -> Command {
        let mut command = Command::new("git");
        for name in INHERITED_LOCATION_VARIABLES {
            command.env_remove(name);
        }
        command.current_dir(directory);
        command
    }

    fn bookkeeping_git(directory: &Path, args: &[&str]) -> Vec<u8> {
        let output = bookkeeping_command(directory)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .expect("running bookkeeping git");
        assert!(
            output.status.success(),
            "bookkeeping git {args:?} in {} failed: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// A disposable repository standing in for the one `git rebase --exec` or a
    /// hook would name. It has a linked worktree because that is the incident's
    /// shape: `GIT_DIR` naming `.git/worktrees/<name>`, a directory not called
    /// `.git`, is what made `git init` record `core.bare = true` in the shared
    /// configuration and break every work-tree command in the main checkout.
    struct Throwaway {
        root: PathBuf,
        main: PathBuf,
        linked: PathBuf,
        linked_git_dir: PathBuf,
    }

    impl Throwaway {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hermit-git-location-throwaway-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let main = root.join("main");
            let linked = root.join("linked");
            fs::create_dir_all(&main).unwrap();
            bookkeeping_git(&main, &["init", "-q"]);
            fs::write(main.join("tracked"), "throwaway\n").unwrap();
            bookkeeping_git(&main, &["add", "tracked"]);
            bookkeeping_git(&main, &["commit", "-qm", "throwaway"]);
            bookkeeping_git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    linked.to_str().unwrap(),
                ],
            );
            let linked_git_dir = PathBuf::from(
                String::from_utf8(bookkeeping_git(
                    &linked,
                    &["rev-parse", "--absolute-git-dir"],
                ))
                .unwrap()
                .trim(),
            );
            assert_ne!(
                linked_git_dir.file_name().unwrap(),
                ".git",
                "the throwaway must reproduce a GIT_DIR that is not named .git"
            );
            Self {
                root,
                main,
                linked,
                linked_git_dir,
            }
        }

        /// HEAD, every ref, both indexes and the configuration, byte for byte,
        /// plus git's own reading of core.bare and of each HEAD.
        fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
            let mut state = BTreeMap::new();
            let common = self.main.join(".git");
            let mut files = vec![
                common.join("HEAD"),
                common.join("config"),
                common.join("index"),
                common.join("packed-refs"),
                self.linked_git_dir.join("HEAD"),
                self.linked_git_dir.join("index"),
                self.linked_git_dir.join("config.worktree"),
                self.linked.join(".git"),
            ];
            let mut pending = vec![common.join("refs")];
            while let Some(directory) = pending.pop() {
                for entry in fs::read_dir(&directory).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        pending.push(path);
                    } else {
                        files.push(path);
                    }
                }
            }
            for file in files {
                let key = file.strip_prefix(&self.root).unwrap().display().to_string();
                match fs::read(&file) {
                    Ok(bytes) => state.insert(key, bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        state.insert(key, b"<absent>".to_vec())
                    }
                    Err(error) => panic!("reading {}: {error}", file.display()),
                };
            }
            // Absent, false and true must all stay distinguishable, so this
            // records the exit status as well as the value.
            let bare = bookkeeping_command(&self.main)
                .args(["config", "--get", "core.bare"])
                .output()
                .unwrap();
            state.insert(
                "git config --get core.bare".into(),
                format!(
                    "{:?} {}",
                    bare.status.code(),
                    String::from_utf8_lossy(&bare.stdout)
                )
                .into_bytes(),
            );
            for (label, directory) in [("main", &self.main), ("linked", &self.linked)] {
                state.insert(
                    format!("{label}: git rev-parse HEAD"),
                    bookkeeping_git(directory, &["rev-parse", "HEAD"]),
                );
            }
            state.insert(
                "git for-each-ref".into(),
                bookkeeping_git(&self.main, &["for-each-ref"]),
            );
            state
        }
    }

    impl Drop for Throwaway {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// https://github.com/rrnewton/hermit/issues/3362: this module's
    /// scratch-repository tests, and the helpers that ask git about this
    /// checkout, must not act on a repository named by an inherited Git
    /// location variable.
    ///
    /// `git rebase --exec` exports `GIT_DIR` to the commands it runs, and hooks
    /// get `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`. On hermit main
    /// 6be37a833d a `cargo test -p hermit-manifest-plan --lib` run under
    /// `rebase --exec` committed six "fixture" commits onto a real worktree
    /// and set `core.bare = true` in the shared hermit configuration for five
    /// minutes.
    ///
    /// Each case re-runs the representative tests in a child process of this
    /// test binary with one variable, or the `GIT_WORK_TREE` and
    /// `GIT_INDEX_FILE` pair that hooks get together, pointing into a
    /// throwaway repository. It then requires both that the child passed and
    /// that the throwaway is byte-identical afterwards. The environment goes
    /// to the child only; this process never exports it, so concurrently
    /// running tests are unaffected.
    #[test]
    fn scratch_repository_tests_ignore_inherited_git_location_variables() {
        const CHILD_TESTS: [&str; 3] = [
            "nextest_binaries::tests::changed_tracked_and_untracked_sources_change_the_identity",
            "nextest_binaries::tests::a_disposable_run_summary_is_not_source_but_a_stray_one_still_is",
            "nextest_binaries::tests::the_repository_really_ignores_the_disposable_directory",
        ];
        // An unrelated repository, used only to prove that each variable
        // really reaches git in the child's environment. Without this control
        // a misspelled variable would make every case pass vacuously.
        let probe = Throwaway::new();
        type CaseValue = fn(&Throwaway) -> PathBuf;
        let variables: [(&str, CaseValue, &[&str]); 3] = [
            (
                "GIT_DIR",
                |throwaway| throwaway.linked_git_dir.clone(),
                &["rev-parse", "--absolute-git-dir"],
            ),
            (
                "GIT_WORK_TREE",
                |throwaway| throwaway.linked.clone(),
                &["rev-parse", "--show-toplevel"],
            ),
            (
                "GIT_INDEX_FILE",
                |throwaway| throwaway.linked_git_dir.join("index"),
                &["rev-parse", "--git-path", "index"],
            ),
        ];
        // Each variable alone, then the pair that hooks get together.
        let cases: [&[&str]; 4] = [
            &["GIT_DIR"],
            &["GIT_WORK_TREE"],
            &["GIT_INDEX_FILE"],
            &["GIT_WORK_TREE", "GIT_INDEX_FILE"],
        ];
        // Every case runs before anything is asserted, so a failure names
        // each variable that still leaks rather than only the first. Each case
        // gets its own throwaway, so one case's damage cannot hide another's.
        let mut failures = Vec::new();
        for names in cases {
            let throwaway = Throwaway::new();
            let mut inherited = Vec::new();
            for name in names {
                let (_, value, control) = variables
                    .iter()
                    .find(|(variable, _, _)| variable == name)
                    .expect("every case names a listed variable");
                let value = value(&throwaway);
                let live = bookkeeping_command(&probe.main)
                    .env(name, &value)
                    .args(*control)
                    .output()
                    .unwrap();
                let answer = PathBuf::from(String::from_utf8_lossy(&live.stdout).trim());
                assert_eq!(
                    answer.canonicalize().ok(),
                    value.canonicalize().ok(),
                    "control: {name} did not steer an unisolated git; stderr: {}",
                    String::from_utf8_lossy(&live.stderr)
                );
                inherited.push((*name, value));
            }
            let described = inherited
                .iter()
                .map(|(name, value)| format!("{name}={}", value.display()))
                .collect::<Vec<_>>()
                .join(" ");

            let before = throwaway.snapshot();
            let mut child = Command::new(std::env::current_exe().unwrap());
            for other in INHERITED_LOCATION_VARIABLES {
                child.env_remove(other);
            }
            let output = child
                .envs(inherited.iter().map(|(name, value)| (*name, value)))
                .args(["--exact", "--test-threads=1"])
                .args(CHILD_TESTS)
                .output()
                .expect("re-running this test binary");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let after = throwaway.snapshot();
            let show = |bytes: Option<&Vec<u8>>| match bytes {
                None => "<missing>".to_string(),
                Some(bytes) => match std::str::from_utf8(bytes) {
                    Ok(text) => format!("{text:?}"),
                    Err(_) => format!("{} bytes sha256 {:x}", bytes.len(), Sha256::digest(bytes)),
                },
            };
            let changed: Vec<_> = before
                .keys()
                .chain(after.keys())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|key| before.get(*key) != after.get(*key))
                .map(|key| {
                    format!(
                        "  {key}: {} -> {}",
                        show(before.get(key)),
                        show(after.get(key))
                    )
                })
                .collect();
            if !changed.is_empty() {
                failures.push(format!(
                    "with {described} the scratch-repository tests wrote into the named \
                     repository:\n{}",
                    changed.join("\n")
                ));
            }
            if !(output.status.success()
                && stdout.contains(&format!(
                    "test result: ok. {} passed; 0 failed",
                    CHILD_TESTS.len()
                )))
            {
                failures.push(format!(
                    "with {described} the representative tests did not all run and \
                     pass:\n{stdout}\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }
}
