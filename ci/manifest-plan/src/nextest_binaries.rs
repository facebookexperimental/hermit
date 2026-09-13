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
const RECORD_SCHEMA: u32 = 2;
pub const GUESTS_ENV: &str = "HERMIT_PREPARED_CARGO_GUESTS";
pub const CPU_WRAPPER_ENV: &str = "HERMIT_NEXTEST_CPU_WRAPPER_BIN";
const CPU_WRAPPER_PACKAGE: &str = "hermit-manifest-plan";
const CPU_WRAPPER_TARGET: &str = "nextest-cpu-wrapper";

#[path = "../../cargo-guest-binaries.rs"]
mod cargo_guests;

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
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mode() != after.mode()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(format!(
            "prepared input changed while hashing {}",
            path.display()
        ));
    }
    Ok(FileIdentity {
        path: path.to_owned(),
        size: before.len(),
        mode: before.mode(),
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let result = Command::new("git")
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
    cpu_wrapper: CpuWrapperRecord,
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

fn cargo_output(root: &Path, args: &[String], destination: &Path) -> Result<(), PreparationError> {
    let output = fs::File::create(destination).map_err(|e| e.to_string())?;
    let status = Command::new(root.join("ci/run-with-reverie-dbt-budget.sh"))
        .arg("cargo")
        .args(args)
        .env("CARGO_BUILD_JOBS", "8")
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

pub fn prepare(root: &Path, profile: &str) -> Result<(), PreparationError> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let selections = profile_selections(&root, profile)?;
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
    for (key, selection) in &selections {
        let mut args = vec![
            "nextest".into(),
            "list".into(),
            "--locked".into(),
            "--message-format".into(),
            "json".into(),
            "--list-type".into(),
            "binaries-only".into(),
        ];
        args.extend(selection.iter().cloned());
        cargo_output(&root, &args, &generation.join(format!("{key}.json")))?;
    }
    let needs_guests = selections.values().any(|args| {
        args.windows(2)
            .any(|args| args == ["--test", "hermit_modes"])
    });
    let guest_path = generation.join("guests.jsonl");
    if needs_guests {
        cargo_output(
            &root,
            &[
                "build",
                "--locked",
                "-p",
                "hermetic_infra_hermit_tests",
                "--bins",
                "--message-format=json",
            ]
            .map(String::from),
            &guest_path,
        )?;
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
        "prepared-nextest: published {} selections and {} Cargo guests for {profile}",
        record.selections.len(),
        record.guests.len()
    );
    Ok(())
}

pub fn run(
    root: &Path,
    operation: &str,
    config: Option<&Path>,
    args: &[String],
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
}
