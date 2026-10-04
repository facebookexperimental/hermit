#!/usr/bin/env -S rust-script --force
//! ```cargo
//! [dependencies]
//! dagrun = { path = "../agent-utils/rs/dagrun" }
//! detcore-model = { path = "../detcore-model" }
//! hermit-manifest-plan = { path = "../ci/manifest-plan" }
//! flate2 = "=1.1.10"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! shell-words = "1.1"
//! tar = "=0.4.46"
//! ```
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Build and shadow-validate the feature-complete Buck release binary.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;
#[allow(dead_code)]
#[path = "lib/safe_ci_scope.rs"]
mod safe_ci_scope;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dagrun::model::CmdType;
use dagrun::model::DagConfig;
use dagrun::model::ResourceHint;
use dagrun::model::Step;
use dagrun::model::StepClass;
use dagrun::scheduler::BoxedCgroups;
use dagrun::scheduler::run_dag_boxed_deadline;
use dagrun::scheduler::steps_violating_run_timeout;
use detcore_model::build_info::BuildFeatures;
use detcore_model::build_info::BuildInfo;
use detcore_model::host_capability::HostCapabilitiesReport;
use flate2::Compression;
use flate2::GzBuilder;
use flate2::read::GzDecoder;
use flate2::read::MultiGzDecoder;
use hermit_manifest_plan::canonical_verdict::ComparedLogScope;
use hermit_manifest_plan::canonical_verdict::LogCompareStrictness;
use hermit_manifest_plan::canonical_verdict::RecordEnvelopeReport;
use hermit_manifest_plan::canonical_verdict::Verdict;
use hermit_manifest_plan::canonical_verdict::VerificationReport;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Value;
use tar::Archive;
use tar::Builder;
use tar::EntryType;
use tar::Header;
use tar::HeaderMode;

const TARGET: &str = "//hermit-cli:hermit-release";
const DOTSLASH_VERSION: &str = "DotSlash 0.5.9";
const RUST_SCRIPT_VERSION: &str = "rust-script 0.36.0";
const OUTER_SCOPE_DEADLINE_SECONDS: i64 = 7_200;
const PROBE_DEADLINE_SECONDS: u64 = 120;
const VERIFY_DEADLINE_SECONDS: u64 = 300;
const MATRIX_STEP_DEADLINE_SECONDS: i64 = 900;
const MATRIX_SUPERVISOR_DEADLINE_SECONDS: i64 = 960;
const MATRIX_CPU_TIMEOUT_SECONDS: i64 = 7_200;
const _: () = assert!(MATRIX_STEP_DEADLINE_SECONDS < MATRIX_SUPERVISOR_DEADLINE_SECONDS);
const MATRIX_CANDIDATE_DEADLINE_SECONDS: u64 = 120;
const MATRIX_PROXY_ENV: &str = "HERMIT_BUCK_PHASE1_MATRIX_PROXY";
const MATRIX_PRIVATE_TMP_HANDOFF: &str = "--matrix-private-tmp";
/// Runs after the stage's mounts, immediately before the candidate, and records
/// what the candidate will see so the proxy can measure the isolation rather than
/// infer it from an exit status the guest also controls.
const PRIVATE_TMP_READBACK_SCRIPT: &str = "readback=$1
shift
printf 'tmp=%s\\ncwd=%s\\ntmpdir=%s\\nuid_map=%s\\n' \"$(stat -c %d:%i /tmp)\" \"$(pwd -P)\" \"$TMPDIR\" \"$(tr -s ' ' ' ' </proc/self/uid_map | sed 's/^ //')\" >\"$readback\"
exec \"$@\"
";
/// Byte-identical to `DBT_PRIVATE_TMP_SCRIPT` in tests/backend-parity/run_matrix.py.
/// The proxy runs run_matrix's own private-/tmp stage, only one process lower:
/// inside the safehermit unit, after safehermit has applied its host bounds.
const DBT_PRIVATE_TMP_SCRIPT: &str = "private_tmp=$1
mount_count=$2
shift 2
mount --make-rprivate /
while [ \"$mount_count\" -gt 0 ]; do
    mount --bind \"$1\" \"$2\"
    shift 2
    mount_count=$((mount_count - 1))
done
mount --rbind \"$private_tmp\" /tmp
cd /tmp
export TMPDIR=/tmp
exec \"$@\"
";
const MAX_PUBLIC_EVIDENCE_GZIP_DECODED_BYTES: usize = 128 * 1024 * 1024;
const VALIDATE_ARTIFACT_ROOT: &str = "ignored/buck2-phase2/artifacts";
const VALIDATE_ARTIFACT_IDENTITY: &str = "ignored/buck2-phase2/current.identity";
const LZMA_SONAME: &str = "liblzma.so.5";
const LZMA_BUCK_PACKAGE: &str = "prebuilt_cxx_library(\n    name = \"lzma\",\n    shared_lib = \"liblzma.so.5\",\n    visibility = [\"PUBLIC\"],\n)\n";
const UNWIND_PTRACE_ARCHIVE: &str = "libunwind-ptrace.a";
const UNWIND_ARCH_SONAME: &str = "libunwind-x86_64.so.8";
const UNWIND_CORE_SONAME: &str = "libunwind.so.8";
const UNWIND_RUNTIME_RELATIVE: &str = "rsrcs/hermit-runtime";
const RELEASE_RPATH: &str =
    "$ORIGIN/../install_pkg/rsrcs/hermit-runtime:$ORIGIN/install/rsrcs/hermit-runtime";
const UNWIND_BUCK_PACKAGE: &str = "prebuilt_cxx_library(\n    name = \"_unwind_core\",\n    shared_lib = \"libunwind.so.8\",\n)\n\nprebuilt_cxx_library(\n    name = \"_unwind_arch\",\n    shared_lib = \"libunwind-x86_64.so.8\",\n    exported_deps = [\":_unwind_core\"],\n)\n\nprebuilt_cxx_library(\n    name = \"unwind\",\n    static_lib = \"libunwind-ptrace.a\",\n    exported_deps = [\":_unwind_arch\"],\n    visibility = [\"PUBLIC\"],\n)\n";
const RELEASE_NEEDED_LIBRARIES: [&str; 5] = [
    "ld-linux-x86-64.so.2",
    "libc.so.6",
    "libgcc_s.so.1",
    "libm.so.6",
    "libunwind-x86_64.so.8",
];
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const GENERATED_MARKERS: [&str; 3] = [
    "name = \"reverie-dbt-0.4-materialized-manifest\"",
    "manifest_dir = \":reverie-dbt-0.4-materialized-manifest\"",
    "load(\"@shim//build_defs:materialized_manifest.bzl\", \"materialized_manifest\")",
];
const REQUIRED_RESOURCES: [(&str, bool); 10] = [
    ("rsrcs/libdetcore_dbt.so", false),
    ("rsrcs/libdetcore_sabre.so", false),
    ("rsrcs/libreverie_dbt_client.so", false),
    ("rsrcs/libreverie_liteinst.so", false),
    ("rsrcs/dynamorio/bin64/drrun", true),
    ("rsrcs/sabre", true),
    ("rsrcs/e9patch", true),
    ("rsrcs/e9tool", true),
    ("rsrcs/hermit-runtime/libunwind-x86_64.so.8", false),
    ("rsrcs/hermit-runtime/libunwind.so.8", false),
];

#[derive(Debug, PartialEq, Eq)]
struct Options {
    dotslash: PathBuf,
    cargo_binary: PathBuf,
    install_bundle: PathBuf,
    safehermit: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct Provenance {
    version: String,
    build_date: String,
    hermit_sha: String,
    hermit_full_sha: String,
    reverie_sha: String,
}

#[derive(Debug, PartialEq, Eq)]
struct PublishedBundle {
    root: PathBuf,
    binary: PathBuf,
    install: PathBuf,
    hermit_manifest: PathBuf,
    resources_manifest: PathBuf,
    binary_sha256: String,
    hermit_manifest_sha256: String,
    resources_manifest_sha256: String,
    resource_inventory: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
struct SafehermitBundle {
    root: PathBuf,
    launcher: PathBuf,
    bounded_run_space: PathBuf,
    launcher_sha256: String,
    bounded_run_space_sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
struct InputSnapshot {
    path: PathBuf,
    hash_manifest: PathBuf,
    sha256: String,
    executable: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct LzmaBuckInput {
    source: PathBuf,
    source_sha256: String,
    root: PathBuf,
    library: PathBuf,
    target: String,
}

#[derive(Debug, PartialEq, Eq)]
struct UnwindClosure {
    ptrace_archive: PathBuf,
    arch_shared: PathBuf,
    core_shared: PathBuf,
}

/// The exact DT_NEEDED closures a libunwind runtime pair may have.
///
/// There are exactly two, and both libraries must agree on one of them.
/// `Narrow` is the CentOS/Fedora build. `Lzma` is the Ubuntu 24.04 build,
/// whose libunwind is configured with MiniDebugInfo support and so names
/// `liblzma.so.5` from both libraries. `liblzma.so.5` is a host-provided base
/// library, like `libc.so.6`: the runtime bundle carries only libunwind, and
/// the release build already binds the host liblzma by content as its declared
/// link input. Any other DT_NEEDED set, or a pair that mixes the two profiles,
/// matches neither and is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnwindProfile {
    Narrow,
    Lzma,
}

const UNWIND_PROFILES: [UnwindProfile; 2] = [UnwindProfile::Narrow, UnwindProfile::Lzma];

impl UnwindProfile {
    fn name(self) -> &'static str {
        match self {
            UnwindProfile::Narrow => "narrow",
            UnwindProfile::Lzma => "lzma",
        }
    }

    fn arch_needed(self) -> &'static [&'static str] {
        match self {
            UnwindProfile::Narrow => &[UNWIND_CORE_SONAME, "libc.so.6"],
            UnwindProfile::Lzma => &[UNWIND_CORE_SONAME, "libc.so.6", LZMA_SONAME],
        }
    }

    fn core_needed(self) -> &'static [&'static str] {
        match self {
            UnwindProfile::Narrow => &["ld-linux-x86-64.so.2", "libc.so.6"],
            UnwindProfile::Lzma => &["ld-linux-x86-64.so.2", "libc.so.6", LZMA_SONAME],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct UnwindBuckInput {
    sources: UnwindClosure,
    source_sha256: BTreeMap<String, String>,
    identity: String,
    root: PathBuf,
    target: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ValidateArtifact {
    bundle: PathBuf,
    binary: PathBuf,
    install: PathBuf,
    identity: String,
    binary_sha256: String,
    resources_sha256: String,
    runtime_sha256: BTreeMap<String, String>,
}

fn usage() -> &'static str {
    "usage: build-buck-release.rs --dotslash ABSOLUTE-PATH \\\n+  --cargo-binary ABSOLUTE-PATH --install-bundle ABSOLUTE-PATH \\\n+  --safehermit ABSOLUTE-PATH\n\n\
Always regenerates the Buck Rust graph, builds the shadow Buck release with a\n\
retained .json-lines.gz event log, verifies that Buck can decode the log, and\n\
hard-fails unless Cargo/Buck provenance, features, backend inventory, resources,\n\
ELF contract, ptrace strict-verify, record/replay, and DBT strict-verify agree.\n\
The Cargo binary/install bundle remain authoritative; no target/ci pointer is changed."
}

fn parse_options<I>(arguments: I) -> Result<Option<Options>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut dotslash = None;
    let mut cargo_binary = None;
    let mut install_bundle = None;
    let mut safehermit = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if matches!(argument.as_str(), "-h" | "--help") {
            return Ok(None);
        }
        let destination = match argument.as_str() {
            "--dotslash" => &mut dotslash,
            "--cargo-binary" => &mut cargo_binary,
            "--install-bundle" => &mut install_bundle,
            "--safehermit" => &mut safehermit,
            _ => {
                return Err(format!(
                    "unknown argument {argument:?}; run with --help for the required shadow inputs"
                ));
            }
        };
        if destination.is_some() {
            return Err(format!(
                "duplicate {argument}; provide each required path once"
            ));
        }
        *destination = Some(PathBuf::from(arguments.next().ok_or_else(|| {
            format!("{argument} requires an absolute path; run with --help for an example")
        })?));
    }
    Ok(Some(Options {
        dotslash: dotslash.ok_or_else(|| {
            "missing --dotslash; provide the absolute public DotSlash 0.5.9 path".to_owned()
        })?,
        cargo_binary: cargo_binary.ok_or_else(|| {
            "missing --cargo-binary; first build Cargo release with third-party-backends".to_owned()
        })?,
        install_bundle: install_bundle.ok_or_else(|| {
            "missing --install-bundle; first build the authoritative hermit-install bundle"
                .to_owned()
        })?,
        safehermit: safehermit.ok_or_else(|| {
            "missing --safehermit; provide dev-hermit/bin/safehermit explicitly".to_owned()
        })?,
    }))
}

fn checked_output(command: &mut Command, description: &str) -> Result<Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{description} failed with {}: {}; correct the input/tool and retry",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn output_text(command: &mut Command, description: &str) -> Result<String, String> {
    let output = checked_output(command, description)?;
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_owned())
        .map_err(|error| format!("{description} emitted non-UTF-8 output: {error}"))
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    output_text(
        Command::new("git")
            .current_dir(root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(arguments),
        &format!("git {}", arguments.join(" ")),
    )
}

fn repository_root() -> Result<PathBuf, String> {
    git(Path::new("."), &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn repository_dirty(root: &Path) -> Result<bool, String> {
    Ok(!git(root, &["status", "--porcelain", "--untracked-files=all"])?.is_empty())
}

fn unique_identity() -> Result<String, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_nanos();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    Ok(format!("{}-{nanos}-{sequence}", std::process::id()))
}

fn create_exclusive_directory(path: &Path, description: &str) -> Result<(), String> {
    fs::create_dir(path).map_err(|error| {
        format!(
            "refusing non-exclusive {description} {}: {error}; choose a fresh identity and never reuse stale evidence",
            path.display()
        )
    })
}

fn create_evidence_directory(root: &Path, full_sha: &str) -> Result<PathBuf, String> {
    let parent = root.join("ignored/buck2-phase1");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let path = parent.join(format!("shadow-{full_sha}-{}", unique_identity()?));
    create_exclusive_directory(&path, "shadow evidence directory")?;
    Ok(path)
}

fn require_absolute_file(path: &Path, description: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{description} must be an absolute path, got {}; resolve it with realpath and retry",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "{description} {} must be a nonempty regular file, not a symlink",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "{description} {} is not executable; install/build it and retry",
            path.display()
        ));
    }
    Ok(path.to_owned())
}

fn require_absolute_directory(path: &Path, description: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{description} must be an absolute path, got {}; resolve it with realpath and retry",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{description} {} must be a real directory, not a symlink",
            path.display()
        ));
    }
    Ok(path.to_owned())
}

fn package_version(manifest: &str) -> Result<String, String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package && line.starts_with("version = ") {
            let value = line
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
                .ok_or_else(|| "hermit-cli package version is not a quoted string".to_owned())?;
            if value.is_empty()
                || value == "unknown"
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
            {
                return Err(format!("invalid hermit-cli package version {value:?}"));
            }
            return Ok(value.to_owned());
        }
    }
    Err("hermit-cli/Cargo.toml has no [package] version".to_owned())
}

struct DuplicateCheckedJson;

impl<'de> Deserialize<'de> for DuplicateCheckedJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateCheckedJsonVisitor)?;
        Ok(Self)
    }
}

struct DuplicateCheckedJsonVisitor;

impl<'de> Visitor<'de> for DuplicateCheckedJsonVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("valid JSON without duplicate object keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON field {key:?}"
                )));
            }
            map.next_value::<DuplicateCheckedJson>()?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<DuplicateCheckedJson>()?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> Result<(), E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
}

fn decode_typed_json<T: DeserializeOwned>(text: &str, description: &str) -> Result<T, String> {
    let mut duplicate_check = serde_json::Deserializer::from_str(text);
    DuplicateCheckedJson::deserialize(&mut duplicate_check)
        .and_then(|_| duplicate_check.end())
        .map_err(|error| format!("{description} is invalid JSON: {error}"))?;
    serde_json::from_str(text)
        .map_err(|error| format!("{description} violates its schema: {error}"))
}

fn canonical_build_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7) && !byte.is_ascii_digit())
    {
        return false;
    }
    let year = value[0..4].parse::<u32>().ok();
    let month = value[5..7].parse::<u32>().ok();
    let day = value[8..10].parse::<u32>().ok();
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year > 0 && (1..=days).contains(&day)
}

fn validate_build_info(
    info: &BuildInfo,
    expected_version: &str,
    expected_sha: &str,
    description: &str,
) -> Result<(), String> {
    if info.schema != BuildInfo::SCHEMA {
        return Err(format!(
            "{description} schema is {}, expected {}",
            info.schema,
            BuildInfo::SCHEMA
        ));
    }
    let date = info
        .build_date
        .as_deref()
        .ok_or_else(|| format!("{description} omits build_date"))?;
    let required_features = BuildFeatures {
        dbt: true,
        e9patch: true,
        sabre: true,
    };
    if info.version != expected_version
        || info.git_sha != expected_sha
        || info.git_sha.len() != 12
        || !info
            .git_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !canonical_build_date(date)
        || info.features != required_features
    {
        return Err(format!(
            "{description} does not match canonical version/date/12-character SHA/dbt+e9patch+sabre build facts"
        ));
    }
    Ok(())
}

fn decode_build_info(
    text: &str,
    expected_version: &str,
    expected_sha: &str,
    description: &str,
) -> Result<BuildInfo, String> {
    let info: BuildInfo = decode_typed_json(text, description)?;
    validate_build_info(&info, expected_version, expected_sha, description)?;
    Ok(info)
}

fn decode_host_capabilities(
    text: &str,
    description: &str,
) -> Result<HostCapabilitiesReport, String> {
    let report: HostCapabilitiesReport = decode_typed_json(text, description)?;
    report
        .validate()
        .map_err(|error| format!("{description} is invalid: {error}"))?;
    Ok(report)
}

fn exact_provenance(root: &Path, cargo_build_info: &BuildInfo) -> Result<Provenance, String> {
    let version = package_version(
        &fs::read_to_string(root.join("hermit-cli/Cargo.toml"))
            .map_err(|error| format!("failed to read hermit-cli/Cargo.toml: {error}"))?,
    )?;
    let hermit_full_sha = git(root, &["rev-parse", "HEAD"])?;
    let short = git(root, &["rev-parse", "--short=12", "HEAD"])?;
    if hermit_full_sha.len() != 40
        || short.len() != 12
        || !hermit_full_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !short.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !hermit_full_sha.starts_with(&short)
    {
        return Err("Git returned an invalid full/12-character Hermit SHA".to_owned());
    }
    if repository_dirty(root)? {
        return Err(
            "Hermit checkout has tracked or non-ignored untracked changes; provenance requires a clean exact tree"
                .to_owned(),
        );
    }
    let expected_hermit_sha = short;
    validate_build_info(
        cargo_build_info,
        &version,
        &expected_hermit_sha,
        "Cargo version report",
    )?;
    let cargo_date = cargo_build_info
        .build_date
        .clone()
        .expect("validated build date");
    let hermit_sha = cargo_build_info.git_sha.clone();
    let reverie_sha = git(root, &["rev-parse", "HEAD:reverie"])?;
    if reverie_sha.len() != 40 || !reverie_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("reverie gitlink is not an exact 40-character SHA".to_owned());
    }
    let checkout_sha = git(&root.join("reverie"), &["rev-parse", "HEAD"])?;
    let checkout_dirty = git(
        &root.join("reverie"),
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if checkout_sha != reverie_sha || !checkout_dirty.is_empty() {
        return Err(format!(
            "Reverie checkout must be clean and equal gitlink {reverie_sha}; got {checkout_sha}; run git submodule update --init --recursive"
        ));
    }
    let lock = fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|error| format!("failed to read Cargo.lock: {error}"))?;
    let expected_source =
        format!("git+https://github.com/rrnewton/reverie.git?rev={reverie_sha}#{reverie_sha}");
    let reverie_sources = lock
        .lines()
        .filter_map(|line| line.trim().strip_prefix("source = \"")?.strip_suffix('"'))
        .filter(|source| source.contains("github.com/rrnewton/reverie.git"))
        .collect::<Vec<_>>();
    if reverie_sources.is_empty()
        || reverie_sources
            .iter()
            .any(|source| *source != expected_source)
    {
        return Err(format!(
            "Cargo.lock contains a missing/mixed Reverie source; every entry must equal {expected_source}"
        ));
    }
    for manifest in [
        "detcore/Cargo.toml",
        "detcore-dbt/Cargo.toml",
        "hermit-cli/Cargo.toml",
    ] {
        let text = fs::read_to_string(root.join(manifest))
            .map_err(|error| format!("failed to read {manifest}: {error}"))?;
        if !text.contains(&format!("rev = \"{reverie_sha}\"")) {
            return Err(format!(
                "{manifest} does not bind Reverie to gitlink {reverie_sha}; run the pin synchronizer"
            ));
        }
    }
    Ok(Provenance {
        version,
        build_date: cargo_date,
        hermit_sha,
        hermit_full_sha,
        reverie_sha,
    })
}

fn sha256(path: &Path) -> Result<String, String> {
    let hash = output_text(
        Command::new("sha256sum").arg(path),
        &format!("sha256sum {}", path.display()),
    )?
    .split_whitespace()
    .next()
    .unwrap_or_default()
    .to_owned();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "sha256sum returned an invalid digest for {}",
            path.display()
        ));
    }
    Ok(hash)
}

fn sha256_bytes(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start sha256sum for retained bytes: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "sha256sum stdin was unavailable".to_owned())?
        .write_all(bytes)
        .map_err(|error| format!("failed to feed retained bytes to sha256sum: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to wait for byte sha256sum: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum of retained bytes failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let hash = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    if !valid_sha256(&hash) {
        return Err("sha256sum returned an invalid digest for retained bytes".to_owned());
    }
    Ok(hash)
}

fn copy_stable_executable(
    source: &Path,
    destination: &Path,
    description: &str,
) -> Result<String, String> {
    let before = sha256(source)?;
    fs::copy(source, destination).map_err(|error| {
        format!(
            "failed to snapshot {description} from {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    let after = sha256(source)?;
    let copied = require_absolute_file(destination, &format!("snapshotted {description}"))?;
    let copied_hash = sha256(&copied)?;
    if before != after || before != copied_hash {
        return Err(format!(
            "{description} changed while its execution snapshot was copied"
        ));
    }
    Ok(copied_hash)
}

fn snapshot_input(
    source: &Path,
    evidence_dir: &Path,
    directory_name: &str,
    file_name: &str,
    description: &str,
    executable: bool,
) -> Result<InputSnapshot, String> {
    let source = if executable {
        require_absolute_file(source, description)?
    } else {
        if !source.is_absolute() {
            return Err(format!("{description} must use an absolute path"));
        }
        require_nonempty_regular_file(source, description)?
    };
    let root = evidence_dir.join(directory_name);
    create_exclusive_directory(&root, &format!("{description} snapshot"))?;
    let path = root.join(file_name);
    let sha256 = if executable {
        copy_stable_executable(&source, &path, description)?
    } else {
        let before = sha256(&source)?;
        fs::copy(&source, &path).map_err(|error| {
            format!(
                "failed to snapshot {description} from {} to {}: {error}",
                source.display(),
                path.display()
            )
        })?;
        let after = sha256(&source)?;
        let copied = require_nonempty_regular_file(&path, &format!("snapshotted {description}"))?;
        let copied_hash = sha256(&copied)?;
        if before != after || before != copied_hash {
            return Err(format!(
                "{description} changed while its snapshot was copied"
            ));
        }
        copied_hash
    };
    let hash_manifest = root.join("snapshot-sha256.tsv");
    atomic_write_new(
        &hash_manifest,
        &format!(
            "source_before_sha256\t{sha256}\ncopied_sha256\t{sha256}\nsource_after_sha256\t{sha256}\n"
        ),
    )?;
    Ok(InputSnapshot {
        path,
        hash_manifest,
        sha256,
        executable,
    })
}

fn snapshot_dotslash(source: &Path, evidence_dir: &Path) -> Result<InputSnapshot, String> {
    snapshot_input(
        source,
        evidence_dir,
        "dotslash-tool-snapshot",
        "dotslash",
        "public DotSlash launcher",
        true,
    )
}

fn reverify_input_snapshot(snapshot: &InputSnapshot, description: &str) -> Result<(), String> {
    reverify_hashed_input(
        &snapshot.path,
        &snapshot.sha256,
        description,
        snapshot.executable,
    )?;
    let expected = format!(
        "source_before_sha256\t{}\ncopied_sha256\t{}\nsource_after_sha256\t{}\n",
        snapshot.sha256, snapshot.sha256, snapshot.sha256,
    );
    let actual = fs::read_to_string(&snapshot.hash_manifest).map_err(|error| {
        format!(
            "input snapshot hash manifest {} is unreadable: {error}",
            snapshot.hash_manifest.display()
        )
    })?;
    if actual != expected {
        return Err(format!(
            "{description} snapshot before/copy/after hash evidence changed"
        ));
    }
    Ok(())
}

fn snapshot_safehermit_bundle(
    source_launcher: &Path,
    evidence_dir: &Path,
) -> Result<SafehermitBundle, String> {
    let source_launcher = require_absolute_file(source_launcher, "safehermit launcher")?;
    if source_launcher.file_name() != Some(OsStr::new("safehermit"))
        || source_launcher.parent().and_then(Path::file_name) != Some(OsStr::new("bin"))
    {
        return Err(
            "safehermit must use its reviewed ROOT/bin/safehermit layout so ROOT/scripts/bounded-run-space can be snapshotted"
                .to_owned(),
        );
    }
    let source_root = source_launcher
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "safehermit launcher has no bundle root".to_owned())?;
    let source_bounded_run_space = require_absolute_file(
        &source_root.join("scripts/bounded-run-space"),
        "safehermit bounded-run-space companion",
    )?;

    let root = evidence_dir.join("safehermit-tool-snapshot");
    create_exclusive_directory(&root, "safehermit tool snapshot")?;
    fs::create_dir(root.join("bin"))
        .map_err(|error| format!("failed to create safehermit snapshot bin directory: {error}"))?;
    fs::create_dir(root.join("scripts")).map_err(|error| {
        format!("failed to create safehermit snapshot scripts directory: {error}")
    })?;
    let launcher = root.join("bin/safehermit");
    let bounded_run_space = root.join("scripts/bounded-run-space");
    let launcher_sha256 =
        copy_stable_executable(&source_launcher, &launcher, "safehermit launcher")?;
    let bounded_run_space_sha256 = copy_stable_executable(
        &source_bounded_run_space,
        &bounded_run_space,
        "safehermit bounded-run-space companion",
    )?;
    Ok(SafehermitBundle {
        root,
        launcher,
        bounded_run_space,
        launcher_sha256,
        bounded_run_space_sha256,
    })
}

fn reverify_safehermit_bundle(bundle: &SafehermitBundle) -> Result<(), String> {
    let launcher = require_absolute_file(&bundle.launcher, "snapshotted safehermit launcher")?;
    let bounded_run_space = require_absolute_file(
        &bundle.bounded_run_space,
        "snapshotted safehermit bounded-run-space companion",
    )?;
    if launcher != bundle.root.join("bin/safehermit")
        || bounded_run_space != bundle.root.join("scripts/bounded-run-space")
        || sha256(&launcher)? != bundle.launcher_sha256
        || sha256(&bounded_run_space)? != bundle.bounded_run_space_sha256
    {
        return Err("snapshotted safehermit execution bundle changed after publication".to_owned());
    }
    Ok(())
}

fn reverify_hashed_input(
    path: &Path,
    expected_sha256: &str,
    description: &str,
    executable: bool,
) -> Result<(), String> {
    let path = if executable {
        require_absolute_file(path, description)?
    } else {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
        if !path.is_absolute() || !metadata.is_file() || metadata.len() == 0 {
            return Err(format!(
                "{description} {} must resolve to a nonempty file",
                path.display()
            ));
        }
        path.to_owned()
    };
    if sha256(&path)? != expected_sha256 {
        return Err(format!("{description} changed during shadow validation"));
    }
    Ok(())
}

fn require_lzma_link_input() -> Result<PathBuf, String> {
    let located = PathBuf::from(output_text(
        Command::new("gcc").arg(format!("-print-file-name={LZMA_SONAME}")),
        "gcc liblzma link-input lookup",
    )?);
    if !located.is_absolute() {
        return Err(
            "release linkage requires liblzma.so.5, but gcc did not resolve an absolute link input; install the liblzma runtime package and retry"
                .into(),
        );
    }
    let path = located.canonicalize().map_err(|error| {
        format!(
            "gcc-resolved liblzma link input {} cannot be canonicalized: {error}",
            located.display()
        )
    })?;
    require_nonempty_regular_file(&path, "canonical liblzma link input")?;
    verify_lzma_elf(&path)?;
    Ok(path)
}

fn verify_lzma_elf(path: &Path) -> Result<(), String> {
    if elf_identity(path)? != (2, 1, 3, 62) {
        return Err(format!(
            "liblzma link input {} is not an x86_64 ELF shared object",
            path.display()
        ));
    }
    let dynamic = output_text(
        Command::new("readelf").args([OsStr::new("-d"), path.as_os_str()]),
        &format!("readelf -d {}", path.display()),
    )?;
    let sonames = dynamic
        .lines()
        .filter(|line| line.contains("(SONAME)"))
        .filter_map(|line| line.split_once('[')?.1.split_once(']').map(|pair| pair.0))
        .collect::<Vec<_>>();
    if sonames != [LZMA_SONAME] {
        return Err(format!(
            "liblzma link input {} has SONAMEs {sonames:?}, expected exactly {LZMA_SONAME}",
            path.display()
        ));
    }
    Ok(())
}

fn verify_lzma_buck_input(input: &LzmaBuckInput) -> Result<(), String> {
    let root_metadata = fs::symlink_metadata(&input.root).map_err(|error| {
        format!(
            "declared liblzma input {} is unreadable: {error}",
            input.root.display()
        )
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("declared liblzma input root must be a real directory".into());
    }
    let mut entries = fs::read_dir(&input.root)
        .map_err(|error| {
            format!(
                "cannot enumerate declared liblzma input {}: {error}",
                input.root.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot enumerate declared liblzma input: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    let names = entries
        .iter()
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    if names != [OsStr::new("BUCK"), OsStr::new(LZMA_SONAME)] {
        return Err(format!(
            "declared liblzma input {} has unexpected entries {names:?}",
            input.root.display()
        ));
    }
    let library = require_nonempty_regular_file(&input.library, "declared liblzma snapshot")?;
    if sha256(&library)? != input.source_sha256 {
        return Err("declared liblzma snapshot hash differs from its content identity".into());
    }
    verify_lzma_elf(&library)?;
    let buck_path =
        require_nonempty_regular_file(&input.root.join("BUCK"), "declared liblzma BUCK package")?;
    let buck = fs::read_to_string(buck_path)
        .map_err(|error| format!("declared liblzma BUCK file is unreadable: {error}"))?;
    if buck != LZMA_BUCK_PACKAGE {
        return Err("declared liblzma BUCK package changed after publication".into());
    }
    reverify_hashed_input(
        &input.source,
        &input.source_sha256,
        "canonical liblzma link input",
        false,
    )
}

fn publish_lzma_buck_input(
    root: &Path,
    evidence_dir: &Path,
    source: &Path,
) -> Result<LzmaBuckInput, String> {
    let source = require_nonempty_regular_file(source, "canonical liblzma link input")?;
    verify_lzma_elf(&source)?;
    let source_sha256 = sha256(&source)?;
    let parent = root.join("ignored/buck2-link-inputs/lzma");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let published = parent.join(&source_sha256);
    let target = format!("root//ignored/buck2-link-inputs/lzma/{source_sha256}:lzma");
    let make_input = |published: PathBuf| LzmaBuckInput {
        source: source.clone(),
        source_sha256: source_sha256.clone(),
        library: published.join(LZMA_SONAME),
        root: published,
        target: target.clone(),
    };
    if !published.exists() {
        let temporary = parent.join(format!(".tmp-{}", unique_identity()?));
        create_exclusive_directory(&temporary, "temporary declared liblzma input")?;
        let temporary_input = make_input(temporary.clone());
        let copy_result = (|| {
            let before = sha256(&source)?;
            fs::copy(&source, &temporary_input.library).map_err(|error| {
                format!(
                    "failed to copy declared liblzma input {}: {error}",
                    source.display()
                )
            })?;
            let after = sha256(&source)?;
            if before != source_sha256 || after != source_sha256 {
                return Err("canonical liblzma input changed while it was snapshotted".into());
            }
            write_new_file(
                &temporary.join("BUCK"),
                LZMA_BUCK_PACKAGE.as_bytes(),
                "declared liblzma BUCK package",
            )?;
            verify_lzma_buck_input(&temporary_input)
        })();
        if let Err(error) = copy_result {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, &published) {
            let _ = fs::remove_dir_all(&temporary);
            if !published.exists() {
                return Err(format!(
                    "failed to publish declared liblzma input {}: {error}",
                    published.display()
                ));
            }
        }
    }
    let input = make_input(published);
    verify_lzma_buck_input(&input)?;
    atomic_write_new(
        &evidence_dir.join("lzma-input.tsv"),
        &format!(
            "schema\thermit-buck-lzma-input/v1\nsource\t{}\nsha256\t{}\ntarget\t{}\n",
            input.source.display(),
            input.source_sha256,
            input.target
        ),
    )?;
    Ok(input)
}

fn require_gcc_library(name: &str, description: &str) -> Result<PathBuf, String> {
    let located = PathBuf::from(output_text(
        Command::new("gcc").arg(format!("-print-file-name={name}")),
        &format!("gcc {description} lookup"),
    )?);
    if !located.is_absolute() {
        return Err(format!(
            "{description} requires {name}, but gcc did not resolve an absolute input"
        ));
    }
    let path = located.canonicalize().map_err(|error| {
        format!(
            "gcc-resolved {description} {} cannot be canonicalized: {error}",
            located.display()
        )
    })?;
    require_nonempty_regular_file(&path, description)
}

fn dynamic_values(path: &Path, tag: &str) -> Result<Vec<String>, String> {
    let dynamic = output_text(
        Command::new("readelf").args([OsStr::new("-d"), path.as_os_str()]),
        &format!("readelf -d {}", path.display()),
    )?;
    Ok(dynamic
        .lines()
        .filter(|line| line.contains(&format!("({tag})")))
        .filter_map(|line| {
            line.split_once('[')?
                .1
                .split_once(']')
                .map(|pair| pair.0.to_owned())
        })
        .collect())
}

fn verify_shared_library(path: &Path, soname: &str, needed: &[&str]) -> Result<(), String> {
    require_nonempty_regular_file(path, &format!("{soname} shared library"))?;
    if elf_identity(path)? != (2, 1, 3, 62) {
        return Err(format!(
            "shared library {} is not an x86_64 ELF shared object",
            path.display()
        ));
    }
    let sonames = dynamic_values(path, "SONAME")?;
    if sonames != [soname] {
        return Err(format!(
            "shared library {} has SONAMEs {sonames:?}, expected exactly {soname}",
            path.display()
        ));
    }
    let actual_needed = needed_libraries(path)?;
    let expected_needed = needed
        .iter()
        .map(|library| (*library).to_owned())
        .collect::<BTreeSet<_>>();
    if actual_needed != expected_needed {
        return Err(format!(
            "shared library {soname} has DT_NEEDED {actual_needed:?}, expected exactly {expected_needed:?}"
        ));
    }
    if !dynamic_values(path, "RPATH")?.is_empty() || !dynamic_values(path, "RUNPATH")?.is_empty() {
        return Err(format!(
            "shared library {soname} must not carry RPATH or RUNPATH"
        ));
    }
    Ok(())
}

/// Verify a libunwind architecture/core pair and name the one profile it has.
fn verify_unwind_shared_pair(arch: &Path, core: &Path) -> Result<UnwindProfile, String> {
    let mut refusals = Vec::new();
    for profile in UNWIND_PROFILES {
        let verdict = verify_shared_library(arch, UNWIND_ARCH_SONAME, profile.arch_needed())
            .and_then(|()| verify_shared_library(core, UNWIND_CORE_SONAME, profile.core_needed()));
        match verdict {
            Ok(()) => return Ok(profile),
            Err(error) => refusals.push(format!("{}: {error}", profile.name())),
        }
    }
    Err(format!(
        "libunwind runtime pair matches no reviewed closure profile; {}",
        refusals.join("; ")
    ))
}

fn verify_static_archive(path: &Path) -> Result<(), String> {
    let path = require_nonempty_regular_file(path, "canonical libunwind-ptrace archive")?;
    let mut magic = [0_u8; 8];
    fs::File::open(&path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map_err(|error| format!("cannot read archive {}: {error}", path.display()))?;
    if &magic != b"!<arch>\n" {
        return Err(format!(
            "canonical libunwind-ptrace input {} is not an ar archive",
            path.display()
        ));
    }
    let members = output_text(
        Command::new("ar").args([OsStr::new("t"), path.as_os_str()]),
        &format!("ar t {}", path.display()),
    )?;
    if members.lines().next().is_none() {
        return Err("canonical libunwind-ptrace archive has no members".into());
    }
    Ok(())
}

fn require_unwind_closure() -> Result<UnwindClosure, String> {
    let closure = UnwindClosure {
        ptrace_archive: require_gcc_library(
            UNWIND_PTRACE_ARCHIVE,
            "canonical libunwind-ptrace link input",
        )?,
        arch_shared: require_gcc_library(
            UNWIND_ARCH_SONAME,
            "canonical libunwind architecture runtime",
        )?,
        core_shared: require_gcc_library(UNWIND_CORE_SONAME, "canonical libunwind core runtime")?,
    };
    verify_static_archive(&closure.ptrace_archive)?;
    verify_unwind_shared_pair(&closure.arch_shared, &closure.core_shared)?;
    Ok(closure)
}

fn unwind_sources(closure: &UnwindClosure) -> [(&'static str, &Path); 3] {
    [
        (UNWIND_PTRACE_ARCHIVE, &closure.ptrace_archive),
        (UNWIND_ARCH_SONAME, &closure.arch_shared),
        (UNWIND_CORE_SONAME, &closure.core_shared),
    ]
}

fn unwind_source_hashes(closure: &UnwindClosure) -> Result<BTreeMap<String, String>, String> {
    unwind_sources(closure)
        .into_iter()
        .map(|(name, path)| Ok((name.to_owned(), sha256(path)?)))
        .collect()
}

fn unwind_content_identity(hashes: &BTreeMap<String, String>) -> Result<String, String> {
    let manifest = hashes
        .iter()
        .map(|(name, hash)| format!("{hash}  {name}\n"))
        .collect::<String>();
    sha256_bytes(manifest.as_bytes())
}

fn verify_unwind_buck_input(input: &UnwindBuckInput) -> Result<UnwindProfile, String> {
    let root_metadata = fs::symlink_metadata(&input.root).map_err(|error| {
        format!(
            "declared libunwind input {} is unreadable: {error}",
            input.root.display()
        )
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("declared libunwind input root must be a real directory".into());
    }
    let mut names = fs::read_dir(&input.root)
        .map_err(|error| format!("cannot enumerate declared libunwind input: {error}"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot enumerate declared libunwind input: {error}"))?;
    names.sort();
    let mut expected_names = vec![
        OsString::from("BUCK"),
        OsString::from(UNWIND_ARCH_SONAME),
        OsString::from(UNWIND_CORE_SONAME),
        OsString::from(UNWIND_PTRACE_ARCHIVE),
    ];
    expected_names.sort();
    if names != expected_names {
        return Err(format!(
            "declared libunwind input {} has unexpected entries {names:?}",
            input.root.display()
        ));
    }
    for (name, source) in unwind_sources(&input.sources) {
        let expected = input
            .source_sha256
            .get(name)
            .ok_or_else(|| format!("declared libunwind input lost hash for {name}"))?;
        reverify_hashed_input(source, expected, &format!("canonical {name} input"), false)?;
        let copied = require_nonempty_regular_file(
            &input.root.join(name),
            &format!("declared {name} snapshot"),
        )?;
        if sha256(&copied)? != *expected {
            return Err(format!(
                "declared {name} snapshot hash differs from its source identity"
            ));
        }
    }
    verify_static_archive(&input.root.join(UNWIND_PTRACE_ARCHIVE))?;
    let profile = verify_unwind_shared_pair(
        &input.root.join(UNWIND_ARCH_SONAME),
        &input.root.join(UNWIND_CORE_SONAME),
    )?;
    let buck = fs::read_to_string(require_nonempty_regular_file(
        &input.root.join("BUCK"),
        "declared libunwind BUCK package",
    )?)
    .map_err(|error| format!("declared libunwind BUCK is unreadable: {error}"))?;
    if buck != UNWIND_BUCK_PACKAGE {
        return Err("declared libunwind BUCK package changed after publication".into());
    }
    if unwind_content_identity(&input.source_sha256)? != input.identity {
        return Err("declared libunwind input identity does not match its source hashes".into());
    }
    Ok(profile)
}

fn publish_unwind_buck_input(
    root: &Path,
    evidence_dir: &Path,
    sources: UnwindClosure,
) -> Result<UnwindBuckInput, String> {
    let source_sha256 = unwind_source_hashes(&sources)?;
    let identity = unwind_content_identity(&source_sha256)?;
    let parent = root.join("ignored/buck2-link-inputs/unwind");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let published = parent.join(&identity);
    let target = format!("root//ignored/buck2-link-inputs/unwind/{identity}:unwind");
    let make_input = |root: PathBuf| UnwindBuckInput {
        sources: UnwindClosure {
            ptrace_archive: sources.ptrace_archive.clone(),
            arch_shared: sources.arch_shared.clone(),
            core_shared: sources.core_shared.clone(),
        },
        source_sha256: source_sha256.clone(),
        identity: identity.clone(),
        root,
        target: target.clone(),
    };
    if !published.exists() {
        let temporary = parent.join(format!(".tmp-{}", unique_identity()?));
        create_exclusive_directory(&temporary, "temporary declared libunwind input")?;
        let temporary_input = make_input(temporary.clone());
        let copy_result = (|| {
            for (name, source) in unwind_sources(&temporary_input.sources) {
                let before = sha256(source)?;
                fs::copy(source, temporary.join(name))
                    .map_err(|error| format!("failed to copy declared {name} input: {error}"))?;
                let after = sha256(source)?;
                let expected = &temporary_input.source_sha256[name];
                if before != *expected || after != *expected {
                    return Err(format!("canonical {name} input changed while snapshotted"));
                }
            }
            write_new_file(
                &temporary.join("BUCK"),
                UNWIND_BUCK_PACKAGE.as_bytes(),
                "declared libunwind BUCK package",
            )?;
            verify_unwind_buck_input(&temporary_input).map(|_profile| ())
        })();
        if let Err(error) = copy_result {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, &published) {
            let _ = fs::remove_dir_all(&temporary);
            if !published.exists() {
                return Err(format!(
                    "failed to publish declared libunwind input {}: {error}",
                    published.display()
                ));
            }
        }
    }
    let input = make_input(published);
    let profile = verify_unwind_buck_input(&input)?;
    let rows = input
        .source_sha256
        .iter()
        .map(|(name, hash)| {
            format!(
                "source\t{name}\t{}\t{hash}\n",
                match name.as_str() {
                    UNWIND_PTRACE_ARCHIVE => input.sources.ptrace_archive.display().to_string(),
                    UNWIND_ARCH_SONAME => input.sources.arch_shared.display().to_string(),
                    UNWIND_CORE_SONAME => input.sources.core_shared.display().to_string(),
                    _ => unreachable!(),
                }
            )
        })
        .collect::<String>();
    atomic_write_new(
        &evidence_dir.join("unwind-input.tsv"),
        &format!(
            "schema\thermit-buck-unwind-input/v2\nidentity\t{}\ntarget\t{}\nprofile\t{}\n{}",
            input.identity,
            input.target,
            profile.name(),
            rows
        ),
    )?;
    Ok(input)
}

fn verify_generated_buck(text: &str) -> Result<(), String> {
    for marker in GENERATED_MARKERS {
        let count = text.match_indices(marker).count();
        if count != 1 {
            return Err(format!(
                "generated Buck graph is stale/partial: marker {marker:?} occurs {count} times; rerun bootstrap/regenerate-rust-deps"
            ));
        }
    }
    let mapped_files = text
        .lines()
        .filter(|line| {
            line.trim_start().starts_with('"') && line.contains(": \"vendor/reverie-dbt-0.4.0/")
        })
        .count();
    if mapped_files != 931 {
        return Err(format!(
            "generated reverie-dbt manifest map has {mapped_files} files, expected 931; rerun regeneration and inspect the pin"
        ));
    }
    Ok(())
}

fn validate_log_header(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Buck event log {} is unreadable: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() < 2 {
        return Err(format!(
            "Buck event log {} is empty/truncated",
            path.display()
        ));
    }
    let mut header = [0_u8; 2];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("cannot read Buck event log {}: {error}", path.display()))?;
    if header != [0x1f, 0x8b] {
        return Err(format!(
            "Buck event log {} is not gzip-compressed .json-lines.gz evidence",
            path.display()
        ));
    }
    Ok(())
}

const RELEASE_SHOW_OUTPUT_TARGET: &str = "hermit//hermit-cli:hermit-release";
const RELEASE_OUTPUT_SUFFIX: &str = "hermit-cli/___hermit-release__/hermit";

fn resolve_release_show_output(stdout: &str, workspace: &Path) -> Result<PathBuf, String> {
    let lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(format!(
            "Buck --show-output must contain exactly one target/output row, got {}",
            lines.len()
        ));
    }
    let fields = lines[0].split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 || fields[0] != RELEASE_SHOW_OUTPUT_TARGET {
        return Err(
            "Buck --show-output row does not name the canonical release target exactly once"
                .to_owned(),
        );
    }
    let output = PathBuf::from(fields[1]);
    let output = if output.is_absolute() {
        output
    } else {
        workspace.join(output)
    };
    let output = require_absolute_file(&output, "Buck release --show-output binary")?;
    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize workflow workspace: {error}"))?;
    let canonical_buck_out = require_absolute_directory(
        &canonical_workspace.join("buck-out"),
        "workflow Buck output root",
    )?
    .canonicalize()
    .map_err(|error| format!("cannot canonicalize workflow Buck output root: {error}"))?;
    let canonical_output = output
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize reconciled Buck release output: {error}"))?;
    let relative = canonical_output
        .strip_prefix(&canonical_buck_out)
        .map_err(|_| {
            format!(
                "Buck release --show-output binary {} is outside canonical workspace buck-out {}",
                canonical_output.display(),
                canonical_buck_out.display(),
            )
        })?;
    let suffix = Path::new(RELEASE_OUTPUT_SUFFIX);
    if !canonical_buck_out.starts_with(&canonical_workspace)
        || !relative.ends_with(suffix)
        || relative.components().count() <= suffix.components().count()
    {
        return Err(format!(
            "Buck release output {} does not have the measured configured-target layout <config>/{RELEASE_OUTPUT_SUFFIX}",
            canonical_output.display(),
        ));
    }
    let (class, endian, object_type, machine) = elf_identity(&canonical_output)?;
    if class != 2 || endian != 1 || !matches!(object_type, 2 | 3) || machine != 62 {
        return Err(format!(
            "Buck release output {} is not an x86_64 ELF executable",
            canonical_output.display(),
        ));
    }
    Ok(canonical_output)
}

/// SHA-256 of the pinned Buck2 DotSlash descriptor (`bootstrap/buck2`, Buck2
/// release 2026-08-01) under which `CommandEnd.is_success` was measured to
/// carry no information: 21 retained builds that completed with zero errors
/// and 1 that failed with one error all serialized `is_success=false`. A
/// false flag is treated as advisory only under this descriptor; another
/// Buck must be re-measured first.
const COMMAND_END_ADVISORY_BUCK_DESCRIPTOR_SHA256: &str =
    "40e4842f407f589acf80a40267764bea914dc4067b2faf330cc1c377080db35e";

/// Buck2 `ActionExecutionKind` values (buck2 `app/buck2_data/data.proto`) that
/// this local-only build may report, by name. Every other value, including
/// REMOTE (2), ACTION_CACHE (3), REMOTE_DEP_FILE_CACHE (9), REMOTE_WORKER (11)
/// and any value this table does not know, is refused: a remote result is not
/// evidence of a local build of the bound inputs.
const LOCAL_ACTION_EXECUTION_KINDS: [(u64, &str); 6] = [
    (1, "local"),
    (4, "simple"),
    (6, "deferred"),
    (7, "local_dep_file"),
    (8, "local_worker"),
    (10, "local_action_cache"),
];

#[derive(Debug, PartialEq, Eq)]
struct ReleaseBuildEvidence {
    command_end_is_success: bool,
    /// Count of each executed action's kind, keyed by the names above. Empty
    /// when the daemon reused every action result without running anything.
    action_executions: BTreeMap<&'static str, u64>,
    output: PathBuf,
    output_sha256: String,
}

impl ReleaseBuildEvidence {
    fn action_execution_summary(&self) -> String {
        LOCAL_ACTION_EXECUTION_KINDS
            .iter()
            .map(|(_, name)| format!("{name}={}", self.action_executions.get(name).unwrap_or(&0)))
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn inspect_release_build_evidence(
    event_log: &Path,
    show_output: &Path,
    shell_exit: i32,
    workspace: &Path,
) -> Result<ReleaseBuildEvidence, String> {
    if shell_exit != 0 {
        return Err(format!(
            "Buck release shell exit was {shell_exit}, expected zero"
        ));
    }
    validate_log_header(event_log)?;
    let decoded = checked_output(
        Command::new("gzip").args([OsStr::new("-cd"), event_log.as_os_str()]),
        "decode retained Buck release event log",
    )?;
    let text = String::from_utf8(decoded.stdout)
        .map_err(|error| format!("Buck release event log is not UTF-8 JSON-lines: {error}"))?;
    let mut commands = Vec::new();
    let mut results = Vec::new();
    let mut action_executions = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let event: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "Buck release event log line {} is invalid JSON: {error}",
                index + 1
            )
        })?;
        if let Some(command) = event.pointer("/Event/data/SpanEnd/data/Command") {
            commands.push(command.clone());
        }
        if let Some(action) = event.pointer("/Event/data/SpanEnd/data/ActionExecution") {
            let kind = action
                .get("execution_kind")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    format!(
                        "Buck release event log line {} has an ActionExecution without an integer execution_kind",
                        index + 1
                    )
                })?;
            let name = LOCAL_ACTION_EXECUTION_KINDS
                .iter()
                .find(|(value, _)| *value == kind)
                .map(|(_, name)| *name)
                .ok_or_else(|| {
                    format!(
                        "Buck release event log line {} reports action execution_kind {kind}, which is not a local kind; a remote or unknown result is not a local build",
                        index + 1
                    )
                })?;
            *action_executions.entry(name).or_insert(0) += 1;
        }
        if let Some(result) = event.pointer("/Result/result/build_response") {
            results.push(result.clone());
        }
    }
    if commands.len() != 1 || results.len() != 1 {
        return Err(format!(
            "Buck release evidence needs exactly one CommandEnd and one Result, got {} and {}",
            commands.len(),
            results.len()
        ));
    }
    let command_success = commands[0]
        .get("is_success")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Buck release CommandEnd lacks boolean is_success".to_owned())?;
    if !command_success {
        let descriptor = sha256(&workspace.join("bootstrap/buck2"))?;
        if descriptor != COMMAND_END_ADVISORY_BUCK_DESCRIPTOR_SHA256 {
            return Err(format!(
                "Buck release CommandEnd.is_success=false is advisory only under the measured Buck2 descriptor {COMMAND_END_ADVISORY_BUCK_DESCRIPTOR_SHA256}; bootstrap/buck2 is {descriptor}, so re-measure the flag for this Buck before accepting it"
            ));
        }
    }
    let build_completed = commands[0]
        .pointer("/build_result/build_completed")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Buck release CommandEnd lacks boolean build_completed".to_owned())?;
    if !build_completed {
        return Err("Buck release CommandEnd does not prove build completion".to_owned());
    }
    let errors = results[0]
        .get("errors")
        .and_then(Value::as_array)
        .ok_or_else(|| "Buck release Result lacks an errors array".to_owned())?;
    if !errors.is_empty() {
        return Err(format!(
            "Buck release Result contains {} build errors",
            errors.len()
        ));
    }
    let targets = results[0]
        .get("build_targets")
        .and_then(Value::as_array)
        .ok_or_else(|| "Buck release Result lacks build_targets".to_owned())?;
    let matching_targets = targets
        .iter()
        .filter(|target| {
            target.get("target").and_then(Value::as_str) == Some(RELEASE_SHOW_OUTPUT_TARGET)
        })
        .count();
    if targets.len() != 1 || matching_targets != 1 {
        return Err(format!(
            "Buck release Result contains {} targets and names {matching_targets} canonical release targets, expected exactly one canonical target",
            targets.len(),
        ));
    }

    let stdout = fs::read_to_string(show_output).map_err(|error| {
        format!(
            "Buck --show-output evidence {} is unreadable: {error}",
            show_output.display()
        )
    })?;
    let canonical_output = resolve_release_show_output(&stdout, workspace)?;
    let output_hash = sha256(&canonical_output)?;
    Ok(ReleaseBuildEvidence {
        command_end_is_success: command_success,
        action_executions,
        output: canonical_output,
        output_sha256: output_hash,
    })
}

fn reconcile_release_build_evidence(
    event_log: &Path,
    show_output: &Path,
    shell_exit: i32,
    workspace: &Path,
    receipt: &Path,
) -> Result<(), String> {
    let evidence = inspect_release_build_evidence(event_log, show_output, shell_exit, workspace)?;
    let receipt_text = format!(
        "reconciliation_schema\thermit-buck-release-build/v1\nshell_exit\t0\ncommand_end_is_success\t{}\ncommand_end_status\tadvisory-known-inconsistent\nbuild_completed\ttrue\nresult_errors\t0\naction_executions\t{}\nremote_action_executions\t0\nrelease_target\thermit//hermit-cli:hermit-release\nrelease_output\t{}\nrelease_output_sha256\t{}\n",
        evidence.command_end_is_success,
        evidence.action_execution_summary(),
        evidence.output.display(),
        evidence.output_sha256,
    );
    write_new_file(
        receipt,
        receipt_text.as_bytes(),
        "Buck release reconciliation receipt",
    )
}

fn run_release_evidence_reconciler(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 10
        || arguments[0] != "--event-log"
        || arguments[2] != "--show-output"
        || arguments[4] != "--shell-exit"
        || arguments[6] != "--workspace"
        || arguments[8] != "--receipt"
    {
        return Err("usage: build-buck-release.rs --reconcile-release-evidence --event-log PATH --show-output PATH --shell-exit INTEGER --workspace ABSOLUTE-PATH --receipt PATH".to_owned());
    }
    let shell_exit = arguments[5]
        .parse::<i32>()
        .map_err(|_| "--shell-exit must be an integer".to_owned())?;
    let workspace = require_absolute_directory(Path::new(&arguments[7]), "workflow workspace")?;
    reconcile_release_build_evidence(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        shell_exit,
        &workspace,
        Path::new(&arguments[9]),
    )
}

fn parse_release_metadata(path: &Path) -> Result<(String, String, String, String), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("release metadata {} is unreadable: {error}", path.display()))?;
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let (name, value) = line
            .split_once('\t')
            .ok_or_else(|| "release metadata contains a malformed row".to_owned())?;
        if !matches!(
            name,
            "version" | "build_date" | "hermit_sha" | "reverie_sha"
        ) || value.is_empty()
            || fields.insert(name, value).is_some()
        {
            return Err("release metadata contains unknown, empty, or duplicate facts".to_owned());
        }
    }
    if fields.len() != 4 {
        return Err("release metadata lacks required exact facts".to_owned());
    }
    Ok((
        fields["version"].to_owned(),
        fields["build_date"].to_owned(),
        fields["hermit_sha"].to_owned(),
        fields["reverie_sha"].to_owned(),
    ))
}

fn validate_release_candidate_version(
    show_output: &Path,
    metadata: &Path,
    workspace: &Path,
    version_json: &Path,
    receipt: &Path,
) -> Result<(), String> {
    let stdout = fs::read_to_string(show_output).map_err(|error| {
        format!(
            "release show-output {} is unreadable: {error}",
            show_output.display()
        )
    })?;
    let candidate = resolve_release_show_output(&stdout, workspace)?;
    let (version, build_date, hermit_sha, reverie_sha) = parse_release_metadata(metadata)?;
    let expected_version = package_version(
        &fs::read_to_string(workspace.join("hermit-cli/Cargo.toml"))
            .map_err(|error| format!("failed to read hermit-cli/Cargo.toml: {error}"))?,
    )?;
    let current_hermit_sha = git(workspace, &["rev-parse", "--short=12", "HEAD"])?;
    let current_reverie_sha = git(workspace, &["rev-parse", "HEAD:reverie"])?;
    if version != expected_version
        || hermit_sha != current_hermit_sha
        || reverie_sha != current_reverie_sha
        || reverie_sha.len() != 40
        || !canonical_build_date(&build_date)
    {
        return Err(
            "release metadata differs from current package/Hermit/Reverie provenance".to_owned(),
        );
    }
    // Hash the bytes that are about to run, and require the same bytes after
    // the probe, so the receipt names the executable whose version was read.
    let candidate_sha256 = sha256(&candidate)?;
    // Deliberately omit --foreground: GNU timeout then owns a separate process
    // group and applies TERM/KILL to the bounded CLI-only process tree.
    let output = Command::new("timeout")
        .args(["--kill-after=2s", "10s"])
        .arg(&candidate)
        .args(["version", "--json"])
        .output()
        .map_err(|error| format!("failed to start bounded release version probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "bounded release version probe failed with {}",
            output.status
        ));
    }
    if !output.stderr.is_empty() {
        return Err("bounded release version probe emitted unexpected stderr".to_owned());
    }
    if sha256(&candidate)? != candidate_sha256 {
        return Err("release candidate changed while its version was probed".to_owned());
    }
    write_new_file(
        version_json,
        &output.stdout,
        "release candidate version JSON",
    )?;
    let output_text = String::from_utf8(output.stdout)
        .map_err(|error| format!("release version probe emitted non-UTF-8 JSON: {error}"))?;
    let build_info = decode_build_info(
        output_text.trim(),
        &version,
        &hermit_sha,
        "release candidate version JSON",
    )?;
    if build_info.build_date.as_deref() != Some(build_date.as_str()) {
        return Err("release candidate version JSON build date differs from metadata".to_owned());
    }
    let receipt_text = format!(
        "candidate_validation_schema\thermit-buck-release-candidate/v1\ncandidate\t{}\ncandidate_sha256\t{}\nversion\t{}\nbuild_date\t{}\nhermit_sha\t{}\nreverie_sha\t{}\nfeatures\tdbt,e9patch,sabre\nversion_probe_timeout_seconds\t10\n",
        candidate.display(),
        candidate_sha256,
        version,
        build_date,
        hermit_sha,
        reverie_sha,
    );
    write_new_file(
        receipt,
        receipt_text.as_bytes(),
        "release candidate validation receipt",
    )
}

fn run_release_candidate_validator(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 10
        || arguments[0] != "--show-output"
        || arguments[2] != "--metadata"
        || arguments[4] != "--workspace"
        || arguments[6] != "--version-json"
        || arguments[8] != "--receipt"
    {
        return Err("usage: build-buck-release.rs --validate-release-candidate --show-output PATH --metadata PATH --workspace ABSOLUTE-PATH --version-json PATH --receipt PATH".to_owned());
    }
    let workspace = require_absolute_directory(Path::new(&arguments[5]), "workflow workspace")?;
    validate_release_candidate_version(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        &workspace,
        Path::new(&arguments[7]),
        Path::new(&arguments[9]),
    )
}

fn has_credential_key_assignment(lower: &str, key: &str) -> bool {
    let bytes = lower.as_bytes();
    for (start, _) in lower.match_indices(key) {
        let end = start + key.len();
        let is_key_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
        if start > 0 && is_key_byte(bytes[start - 1]) {
            continue;
        }
        if end < bytes.len() && is_key_byte(bytes[end]) {
            continue;
        }
        let mut cursor = end;
        if cursor + 1 < bytes.len()
            && bytes[cursor] == b'\\'
            && matches!(bytes[cursor + 1], b'\'' | b'"')
        {
            cursor += 2;
        } else if cursor < bytes.len() && matches!(bytes[cursor], b'\'' | b'"') {
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < bytes.len() && matches!(bytes[cursor], b':' | b'=') {
            return true;
        }
    }
    false
}

fn line_has_known_credential_marker(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let literals = [
        "authorization",
        "proxy-authorization:",
        "x-api-key:",
        "x-auth-token:",
        "cookie:",
        "set-cookie:",
        "access_token",
        "access-token=",
        "actions_runtime_token",
        "actions_id_token_request_token",
        "refresh_token",
        "github_token",
        "gh_token",
        "ghp_",
        "ghs_",
        "gho_",
        "ghu_",
        "ghr_",
        "github_pat_",
        "aws_access_key_id",
        "akia",
        "security-token",
        "aws_secret_access_key",
        "aws_session_token",
        "asia",
        "bearer ",
        "security_token",
        "security token",
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "-----begin ec private key-----",
        "-----begin openssh private key-----",
    ];
    if literals.iter().any(|marker| lower.contains(marker)) {
        return true;
    }
    if [
        "password",
        "passwd",
        "pwd",
        "secret_key",
        "client_secret",
        "private_key",
    ]
    .iter()
    .any(|key| has_credential_key_assignment(&lower, key))
    {
        return true;
    }
    let mut remainder = lower.as_str();
    while let Some((_, after_scheme)) = remainder.split_once("://") {
        let authority = after_scheme
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            return true;
        }
        remainder = after_scheme;
    }
    false
}

#[derive(Default)]
struct PublicEvidenceScan {
    decoded_gzip_files: usize,
    decoded_bytes: u64,
    marker_lines: usize,
}

struct StableScannedFile {
    identity: StableFileIdentity,
    raw_bytes: Vec<u8>,
}

fn decode_gzip_bytes_with_limit(raw: &[u8], max_decoded_bytes: usize) -> Result<Vec<u8>, String> {
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return Err("prospective .json-lines.gz evidence lacks a gzip header".to_owned());
    }
    let read_limit = max_decoded_bytes
        .checked_add(1)
        .ok_or_else(|| "prospective gzip evidence decode limit overflowed".to_owned())?;
    let read_limit = u64::try_from(read_limit)
        .map_err(|_| "prospective gzip evidence decode limit is unsupported".to_owned())?;
    let mut decoded = Vec::new();
    MultiGzDecoder::new(Cursor::new(raw))
        .take(read_limit)
        .read_to_end(&mut decoded)
        .map_err(|error| format!("prospective gzip evidence is truncated or corrupt: {error}"))?;
    if decoded.len() > max_decoded_bytes {
        return Err(format!(
            "prospective gzip evidence expands beyond the {max_decoded_bytes}-byte decode limit"
        ));
    }
    Ok(decoded)
}

fn decode_gzip_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    decode_gzip_bytes_with_limit(raw, MAX_PUBLIC_EVIDENCE_GZIP_DECODED_BYTES)
}

fn scan_public_evidence_tree(
    root: &Path,
    directory: &Path,
    scan: &mut PublicEvidenceScan,
    files: &mut BTreeMap<String, StableScannedFile>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "failed to enumerate public evidence {}: {error}",
                directory.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to enumerate public evidence: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative_path = path
            .strip_prefix(root)
            .map_err(|_| format!("public evidence path escaped scan root: {}", path.display()))?;
        let relative = relative_path
            .to_str()
            .ok_or_else(|| "public evidence path is not valid UTF-8".to_owned())?;
        if relative.contains(['\n', '\r', '\t', '\\']) {
            return Err(format!(
                "public evidence path is manifest-unsafe: {relative:?}"
            ));
        }
        if line_has_known_credential_marker(relative) {
            scan.marker_lines += 1;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "failed to inspect public evidence {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "public evidence contains symlink {}; refusing upload",
                path.display()
            ));
        }
        if metadata.is_dir() {
            scan_public_evidence_tree(root, &path, scan, files)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!(
                "public evidence contains special file {}; refusing upload",
                path.display()
            ));
        }
        let stable = read_stable_file_bytes(&path)?;
        let bytes = if path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".json-lines.gz"))
        {
            scan.decoded_gzip_files += 1;
            decode_gzip_bytes(&stable.raw_bytes)?
        } else {
            stable.raw_bytes.clone()
        };
        scan.decoded_bytes = scan
            .decoded_bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "public evidence decoded byte count overflowed".to_owned())?;
        scan.marker_lines += String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| line_has_known_credential_marker(line))
            .count();
        if files.insert(relative.to_owned(), stable).is_some() {
            return Err("public evidence traversal produced a duplicate file identity".to_owned());
        }
    }
    Ok(())
}

fn stable_public_evidence_population(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, StableFileIdentity>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to re-enumerate public evidence: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read public evidence entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to re-inspect public evidence: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "public evidence contains symlink {}; refusing upload",
                path.display()
            ));
        }
        if metadata.is_dir() {
            stable_public_evidence_population(root, &path, files)?;
        } else if metadata.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .map_err(|_| "public evidence re-verification escaped root".to_owned())?;
            let relative = relative_path
                .to_str()
                .ok_or_else(|| "public evidence path became non-UTF-8".to_owned())?
                .to_owned();
            if relative.contains(['\n', '\r', '\t', '\\']) {
                return Err("public evidence path became manifest-unsafe".to_owned());
            }
            if files
                .insert(relative, read_stable_file_bytes(&path)?.identity)
                .is_some()
            {
                return Err("public evidence re-verification found a duplicate file".to_owned());
            }
        } else {
            return Err(format!(
                "public evidence contains special file {}; refusing upload",
                path.display()
            ));
        }
    }
    Ok(())
}

fn capture_public_evidence(
    root: &Path,
) -> Result<
    (
        PathBuf,
        PublicEvidenceScan,
        BTreeMap<String, StableScannedFile>,
    ),
    String,
> {
    let root = require_absolute_directory(root, "public evidence directory")?;
    let canonical_root = fs::canonicalize(&root)
        .map_err(|error| format!("cannot canonicalize public evidence root: {error}"))?;
    if canonical_root != root {
        return Err("public evidence root must use its canonical real path".to_owned());
    }
    for reserved in ["SHA256SUMS", "public-evidence-scan.tsv"] {
        if root.join(reserved).exists() {
            return Err(format!(
                "public evidence source contains reserved generated path {reserved:?}"
            ));
        }
    }
    let mut scan = PublicEvidenceScan::default();
    let mut files = BTreeMap::new();
    scan_public_evidence_tree(&root, &root, &mut scan, &mut files)?;
    if scan.marker_lines != 0 {
        return Err(format!(
            "public evidence contains {} lines with known credential markers; refusing upload",
            scan.marker_lines
        ));
    }
    Ok((root, scan, files))
}

fn render_captured_evidence_metadata(
    scan: &PublicEvidenceScan,
    files: &BTreeMap<String, StableScannedFile>,
    receipt_name: &str,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut receipt = format!(
        "scan_schema\thermit-public-evidence/v1\nregular_files\t{}\ndecoded_gzip_files\t{}\ndecoded_bytes\t{}\nknown_credential_marker_lines\t0\n",
        files.len() + 2,
        scan.decoded_gzip_files,
        scan.decoded_bytes,
    );
    for (relative, file) in files {
        receipt.push_str(&format!(
            "file_sha256\t{}\t{relative}\n",
            file.identity.sha256
        ));
    }
    if line_has_known_credential_marker(&receipt) {
        return Err(
            "captured evidence receipt unexpectedly matches a credential marker".to_owned(),
        );
    }
    let receipt_bytes = receipt.into_bytes();
    let mut checksum_rows = files
        .iter()
        .map(|(relative, file)| (relative.clone(), file.identity.sha256.clone()))
        .collect::<BTreeMap<_, _>>();
    checksum_rows.insert(receipt_name.to_owned(), sha256_bytes(&receipt_bytes)?);
    let checksums = checksum_rows
        .into_iter()
        .map(|(relative, hash)| format!("{hash}  {relative}\n"))
        .collect::<String>()
        .into_bytes();
    Ok((receipt_bytes, checksums))
}

fn captured_archive_population(
    files: &BTreeMap<String, StableScannedFile>,
    receipt_name: &str,
    receipt: &[u8],
    checksums: &[u8],
) -> BTreeMap<String, Vec<u8>> {
    let mut population = files
        .iter()
        .map(|(relative, file)| (relative.clone(), file.raw_bytes.clone()))
        .collect::<BTreeMap<_, _>>();
    population.insert(receipt_name.to_owned(), receipt.to_vec());
    population.insert("SHA256SUMS".to_owned(), checksums.to_vec());
    population
}

fn deterministic_tar_gz(population: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, String> {
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    builder.mode(HeaderMode::Deterministic);
    for (relative, bytes) in population {
        let path = Path::new(relative);
        if path.is_absolute()
            || path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!("unsafe captured archive path {relative:?}"));
        }
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o444);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(bytes))
            .map_err(|error| format!("failed to append captured evidence to tar: {error}"))?;
    }
    let encoder = builder
        .into_inner()
        .map_err(|error| format!("failed to finish captured evidence tar: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("failed to finish captured evidence gzip: {error}"))
}

fn parse_checksum_bytes(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| format!("archived SHA256SUMS is not UTF-8: {error}"))?;
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let (hash, relative) = line
            .split_once("  ")
            .ok_or_else(|| "archived SHA256SUMS contains a malformed row".to_owned())?;
        if !valid_sha256(hash)
            || relative.is_empty()
            || result
                .insert(relative.to_owned(), hash.to_owned())
                .is_some()
        {
            return Err("archived SHA256SUMS contains an invalid or duplicate row".to_owned());
        }
    }
    Ok(result)
}

fn verify_captured_archive(
    archive_bytes: &[u8],
    expected: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = Archive::new(decoder);
    let mut actual = BTreeMap::new();
    for entry in archive
        .entries()
        .map_err(|error| format!("cannot enumerate captured evidence archive: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("cannot read archive entry: {error}"))?;
        if !entry.header().entry_type().is_file() {
            return Err("captured evidence archive contains a non-regular member".to_owned());
        }
        let path = entry
            .path()
            .map_err(|error| format!("cannot decode archive member path: {error}"))?
            .into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err("captured evidence archive contains an unsafe member path".to_owned());
        }
        let relative = path.to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read archived evidence bytes: {error}"))?;
        if actual.insert(relative, bytes).is_some() {
            return Err("captured evidence archive contains a duplicate member".to_owned());
        }
    }
    if &actual != expected {
        return Err("captured evidence archive population/bytes differ from capture".to_owned());
    }
    let checksum_bytes = actual
        .get("SHA256SUMS")
        .ok_or_else(|| "captured evidence archive lacks SHA256SUMS".to_owned())?;
    let checksums = parse_checksum_bytes(checksum_bytes)?;
    let observed = actual
        .iter()
        .filter(|(relative, _)| relative.as_str() != "SHA256SUMS")
        .map(|(relative, bytes)| Ok((relative.clone(), sha256_bytes(bytes)?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    if observed != checksums {
        return Err("archived SHA256SUMS does not bind the archived member bytes".to_owned());
    }
    Ok(())
}

fn reverify_captured_sources(
    root: &Path,
    files: &BTreeMap<String, StableScannedFile>,
) -> Result<(), String> {
    let expected = files
        .iter()
        .map(|(relative, file)| (relative.clone(), &file.identity))
        .collect::<BTreeMap<_, _>>();
    let mut actual = BTreeMap::new();
    stable_public_evidence_population(root, root, &mut actual)?;
    if actual.len() != expected.len()
        || actual
            .iter()
            .any(|(relative, identity)| expected.get(relative) != Some(&identity))
    {
        return Err("captured evidence source population/identity changed".to_owned());
    }
    Ok(())
}

fn atomic_write_new_bytes(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "refusing existing {description} {}",
            path.display()
        ));
    }
    let temporary = path.with_extension(format!("tmp.{}", unique_identity()?));
    write_new_file(&temporary, bytes, &format!("temporary {description}"))?;
    let publish = fs::hard_link(&temporary, path).map_err(|error| {
        format!(
            "failed to publish {description} {} without replacement: {error}",
            path.display()
        )
    });
    let cleanup = fs::remove_file(&temporary)
        .map_err(|error| format!("failed to remove temporary {description}: {error}"));
    match (publish, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => match fs::remove_file(path) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!(
                "{error}; failed to roll back published {description}: {rollback}"
            )),
        },
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup also failed: {cleanup}")),
    }
}

fn publish_read_only_file(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{description} path must be absolute"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{description} path has no parent"))?;
    require_absolute_directory(parent, &format!("{description} parent"))?;
    atomic_write_new_bytes(path, bytes, description)?;
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("cannot inspect {description}: {error}"))?
        .permissions();
    permissions.set_mode(0o444);
    if let Err(error) = fs::set_permissions(path, permissions) {
        return match fs::remove_file(path) {
            Ok(()) => Err(format!("cannot make {description} read-only: {error}")),
            Err(rollback) => Err(format!(
                "cannot make {description} read-only: {error}; rollback failed: {rollback}"
            )),
        };
    }
    Ok(())
}

fn validate_combined_output_path(
    source_root: &Path,
    output: &Path,
    description: &str,
) -> Result<(), String> {
    if !output.is_absolute()
        || output
            .file_name()
            .and_then(OsStr::to_str)
            .is_none_or(|name| name.is_empty() || name.contains(['\n', '\r', '\t', '\\']))
    {
        return Err(format!(
            "{description} must be an absolute manifest-safe file path"
        ));
    }
    let parent = output
        .parent()
        .ok_or_else(|| format!("{description} has no parent directory"))?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| format!("cannot canonicalize {description} parent: {error}"))?;
    if canonical_parent != parent || canonical_parent.starts_with(source_root) {
        return Err(format!(
            "{description} parent must be canonical and outside the captured source root"
        ));
    }
    Ok(())
}

fn scan_and_package_public_evidence_with_hook<F>(
    root: &Path,
    archive: &Path,
    receipt_output: &Path,
    hook: F,
) -> Result<(), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    let (root, scan, files) = capture_public_evidence(root)?;
    if archive == receipt_output {
        return Err("combined evidence archive and receipt outputs must be distinct".to_owned());
    }
    validate_combined_output_path(&root, archive, "combined evidence archive")?;
    validate_combined_output_path(&root, receipt_output, "combined evidence receipt")?;
    if archive.exists() || receipt_output.exists() {
        return Err("combined evidence refuses stale archive or receipt output".to_owned());
    }
    let receipt_name = "public-evidence-scan.tsv";
    let (receipt, checksums) = render_captured_evidence_metadata(&scan, &files, receipt_name)?;
    let population = captured_archive_population(&files, receipt_name, &receipt, &checksums);
    hook(&root)?;
    let archive_bytes = deterministic_tar_gz(&population)?;
    verify_captured_archive(&archive_bytes, &population)?;
    reverify_captured_sources(&root, &files)?;
    publish_read_only_file(archive, &archive_bytes, "captured public evidence archive")?;
    if let Err(error) = publish_read_only_file(
        receipt_output,
        &receipt,
        "captured public evidence scan receipt",
    ) {
        let archive_cleanup = fs::remove_file(archive);
        return match archive_cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; failed to roll back captured archive: {cleanup}"
            )),
        };
    }
    Ok(())
}

fn scan_and_package_public_evidence(
    root: &Path,
    archive: &Path,
    receipt_output: &Path,
) -> Result<(), String> {
    scan_and_package_public_evidence_with_hook(root, archive, receipt_output, |_root| Ok(()))
}

fn scan_single_public_file(
    source: &Path,
    published_name: &str,
) -> Result<(PublicEvidenceScan, BTreeMap<String, StableScannedFile>), String> {
    let mut components = Path::new(published_name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
        || published_name.contains(['\n', '\r', '\t', '\\'])
        || matches!(published_name, "SHA256SUMS" | "public-evidence-scan.tsv")
    {
        return Err("combined staging name must be one safe top-level filename".to_owned());
    }
    if line_has_known_credential_marker(published_name) {
        return Err("combined staging filename matches a credential marker".to_owned());
    }
    let captured = read_stable_file_bytes(source)?;
    let marker_lines = String::from_utf8_lossy(&captured.raw_bytes)
        .lines()
        .filter(|line| line_has_known_credential_marker(line))
        .count();
    if marker_lines != 0 {
        return Err(format!(
            "legacy diagnostic contains {marker_lines} lines with known credential markers"
        ));
    }
    let mut files = BTreeMap::new();
    files.insert(published_name.to_owned(), captured);
    Ok((
        PublicEvidenceScan {
            decoded_gzip_files: 0,
            decoded_bytes: files[published_name].raw_bytes.len() as u64,
            marker_lines: 0,
        },
        files,
    ))
}

fn remove_exclusive_staging(staging: &Path) -> Result<(), String> {
    if !staging.exists() {
        return Ok(());
    }
    let mut permissions = fs::metadata(staging)
        .map_err(|error| format!("cannot inspect failed staging directory: {error}"))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(staging, permissions)
        .map_err(|error| format!("cannot unlock failed staging directory: {error}"))?;
    fs::remove_dir_all(staging)
        .map_err(|error| format!("cannot remove failed staging directory: {error}"))
}

fn scan_and_stage_public_evidence_with_hook<F>(
    source: &Path,
    staging: &Path,
    published_name: &str,
    hook: F,
) -> Result<(), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    if !source.is_absolute() || !staging.is_absolute() {
        return Err("legacy source and staging paths must be absolute".to_owned());
    }
    let source = require_nonempty_regular_file(source, "legacy diagnostic source")?;
    let source = fs::canonicalize(&source)
        .map_err(|error| format!("cannot canonicalize legacy diagnostic source: {error}"))?;
    let staging_parent = staging
        .parent()
        .ok_or_else(|| "legacy staging path has no parent".to_owned())?;
    let canonical_staging_parent = fs::canonicalize(staging_parent)
        .map_err(|error| format!("cannot canonicalize legacy staging parent: {error}"))?;
    if canonical_staging_parent != staging_parent || source.starts_with(staging) {
        return Err(
            "legacy staging parent must be canonical and outside the source path".to_owned(),
        );
    }
    let (_scan, files) = scan_single_public_file(&source, published_name)?;
    let captured_identity = &files[published_name].identity;
    let checksums = format!("{}  {published_name}\n", captured_identity.sha256).into_bytes();
    let mut population = BTreeMap::from([(
        published_name.to_owned(),
        files[published_name].raw_bytes.clone(),
    )]);
    population.insert("SHA256SUMS".to_owned(), checksums);
    hook(&source)?;
    create_exclusive_directory(staging, "combined legacy staging directory")?;
    let materialize = (|| -> Result<(), String> {
        for (relative, bytes) in &population {
            write_new_file(
                &staging.join(relative),
                bytes,
                "captured legacy staging member",
            )?;
        }
        let mut staged = BTreeMap::new();
        stable_public_evidence_population(staging, staging, &mut staged)?;
        if staged.len() != population.len()
            || staged.iter().any(|(relative, identity)| {
                population
                    .get(relative)
                    .is_none_or(|bytes| sha256_bytes(bytes).ok().as_ref() != Some(&identity.sha256))
            })
            || staged
                .get(published_name)
                .is_none_or(|identity| identity.sha256 != captured_identity.sha256)
        {
            return Err(
                "materialized legacy staging differs from captured bytes/digest".to_owned(),
            );
        }
        let archived_checksums = parse_checksum_bytes(&population["SHA256SUMS"])?;
        let observed = population
            .iter()
            .filter(|(relative, _)| relative.as_str() != "SHA256SUMS")
            .map(|(relative, bytes)| Ok((relative.clone(), sha256_bytes(bytes)?)))
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        if observed != archived_checksums {
            return Err("legacy staging SHA256SUMS does not bind captured bytes".to_owned());
        }
        let final_source = read_stable_file_bytes(&source)?;
        if final_source.identity != *captured_identity {
            return Err("legacy diagnostic source changed after stable capture".to_owned());
        }
        for relative in population.keys() {
            let path = staging.join(relative);
            let mut permissions = fs::metadata(&path)
                .map_err(|error| format!("cannot inspect staged member: {error}"))?
                .permissions();
            permissions.set_mode(0o444);
            fs::set_permissions(path, permissions)
                .map_err(|error| format!("cannot make staged member read-only: {error}"))?;
        }
        let mut permissions = fs::metadata(staging)
            .map_err(|error| format!("cannot inspect staging directory: {error}"))?
            .permissions();
        permissions.set_mode(0o555);
        fs::set_permissions(staging, permissions)
            .map_err(|error| format!("cannot make staging directory read-only: {error}"))?;
        Ok(())
    })();
    if let Err(error) = materialize {
        return match remove_exclusive_staging(staging) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!("{error}; staging cleanup also failed: {cleanup}")),
        };
    }
    Ok(())
}

fn scan_and_stage_public_evidence(
    source: &Path,
    staging: &Path,
    published_name: &str,
) -> Result<(), String> {
    scan_and_stage_public_evidence_with_hook(source, staging, published_name, |_source| Ok(()))
}

#[derive(Debug, PartialEq, Eq)]
struct StableFileIdentity {
    sha256: String,
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

fn file_identity(metadata: &fs::Metadata, sha256: String) -> StableFileIdentity {
    StableFileIdentity {
        sha256,
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn read_stable_file_bytes(path: &Path) -> Result<StableScannedFile, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect stable evidence {}: {error}", path.display()))?;
    if !before.is_file() || before.file_type().is_symlink() || before.nlink() != 1 {
        return Err(format!(
            "stable evidence {} must be a single-link regular file",
            path.display()
        ));
    }
    let mut raw_bytes = Vec::new();
    fs::File::open(path)
        .and_then(|mut file| file.read_to_end(&mut raw_bytes))
        .map_err(|error| format!("cannot read stable evidence {}: {error}", path.display()))?;
    let digest = sha256_bytes(&raw_bytes)?;
    let after = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot re-inspect stable evidence {}: {error}",
            path.display()
        )
    })?;
    if file_identity(&before, digest.clone()) != file_identity(&after, digest.clone()) {
        return Err(format!(
            "stable evidence {} changed while read and hashed",
            path.display()
        ));
    }
    Ok(StableScannedFile {
        identity: file_identity(&after, digest),
        raw_bytes,
    })
}

fn stable_file_identity(path: &Path) -> Result<StableFileIdentity, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot attest public evidence {}: {error}", path.display()))?;
    if !before.is_file() || before.file_type().is_symlink() || before.nlink() != 1 {
        return Err(format!(
            "public evidence {} must be a single-link regular file",
            path.display()
        ));
    }
    let sha256 = sha256(path)?;
    let after = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot re-attest public evidence {}: {error}",
            path.display()
        )
    })?;
    let before = file_identity(&before, sha256.clone());
    let after = file_identity(&after, sha256);
    if before != after {
        return Err(format!(
            "public evidence {} changed while it was hashed",
            path.display()
        ));
    }
    Ok(after)
}

fn run_scan_and_package_public_evidence(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 6
        || arguments[0] != "--root"
        || arguments[2] != "--archive"
        || arguments[4] != "--receipt-output"
    {
        return Err("usage: build-buck-release.rs --scan-and-package-public-evidence --root ABSOLUTE-PATH --archive ABSOLUTE-PATH --receipt-output ABSOLUTE-PATH".to_owned());
    }
    scan_and_package_public_evidence(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        Path::new(&arguments[5]),
    )
}

fn run_scan_and_stage_public_evidence(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 6
        || arguments[0] != "--source"
        || arguments[2] != "--staging"
        || arguments[4] != "--name"
    {
        return Err("usage: build-buck-release.rs --scan-and-stage-public-evidence --source ABSOLUTE-PATH --staging ABSOLUTE-PATH --name FILENAME".to_owned());
    }
    scan_and_stage_public_evidence(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        &arguments[5],
    )
}

fn release_evidence_verdict(
    summary_exit: i32,
    reconciliation_exit: i32,
    candidate_exit: i32,
) -> Result<(), String> {
    if summary_exit == 0 && reconciliation_exit == 0 && candidate_exit == 0 {
        Ok(())
    } else {
        Err(format!(
            "Buck shell/event/output evidence is non-green: log-summary exit={summary_exit}, typed-reconciliation exit={reconciliation_exit}, typed-candidate exit={candidate_exit}"
        ))
    }
}

fn run_release_evidence_verdict(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 6
        || arguments[0] != "--summary-exit"
        || arguments[2] != "--reconciliation-exit"
        || arguments[4] != "--candidate-exit"
    {
        return Err(
            "usage: build-buck-release.rs --release-evidence-verdict --summary-exit INTEGER --reconciliation-exit INTEGER --candidate-exit INTEGER"
                .to_owned(),
        );
    }
    let summary_exit = arguments[1]
        .parse::<i32>()
        .map_err(|_| "--summary-exit must be an integer".to_owned())?;
    let reconciliation_exit = arguments[3]
        .parse::<i32>()
        .map_err(|_| "--reconciliation-exit must be an integer".to_owned())?;
    let candidate_exit = arguments[5]
        .parse::<i32>()
        .map_err(|_| "--candidate-exit must be an integer".to_owned())?;
    release_evidence_verdict(summary_exit, reconciliation_exit, candidate_exit)
}

fn verify_install_bundle(
    bundle: &Path,
    reverie_sha: &str,
) -> Result<Vec<(String, String)>, String> {
    let bundle = require_absolute_directory(bundle, "Cargo install bundle")?;
    let mut hashes = Vec::new();
    for (relative, executable) in REQUIRED_RESOURCES {
        let path = bundle.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "Cargo install bundle is incomplete: {} is unavailable ({error}); rebuild hermit-install",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() == 0
            || (executable && metadata.permissions().mode() & 0o111 == 0)
        {
            return Err(format!(
                "Cargo install bundle is incomplete: {} has the wrong type/size/mode; rebuild hermit-install",
                path.display()
            ));
        }
        hashes.push((relative.to_owned(), sha256(&path)?));
    }
    let sabre_revision = fs::read_to_string(bundle.join("rsrcs/sabre.revision"))
        .map_err(|error| format!("Cargo install bundle lacks readable sabre.revision: {error}"))?;
    if sabre_revision.trim() != reverie_sha {
        return Err(format!(
            "Cargo install bundle Reverie mismatch: sabre.revision={} expected={reverie_sha}; rebuild it at this pin",
            sabre_revision.trim()
        ));
    }
    hashes.sort();
    Ok(hashes)
}

fn elf_identity(path: &Path) -> Result<(u8, u8, u16, u16), String> {
    let mut header = [0_u8; 64];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("cannot read ELF header {}: {error}", path.display()))?;
    if &header[..4] != b"\x7fELF" || header[5] != 1 {
        return Err(format!(
            "{} is not a little-endian ELF binary",
            path.display()
        ));
    }
    let object_type = u16::from_le_bytes([header[16], header[17]]);
    let machine = u16::from_le_bytes([header[18], header[19]]);
    Ok((header[4], header[5], object_type, machine))
}

fn needed_libraries(path: &Path) -> Result<BTreeSet<String>, String> {
    let text = output_text(
        Command::new("readelf").args([OsStr::new("-d"), path.as_os_str()]),
        &format!("readelf -d {}", path.display()),
    )?;
    Ok(text
        .lines()
        .filter(|line| line.contains("(NEEDED)"))
        .filter_map(|line| {
            line.split_once('[')?
                .1
                .split_once(']')
                .map(|pair| pair.0.to_owned())
        })
        .collect())
}

fn validate_release_needed_libraries(libraries: &BTreeSet<String>) -> Result<(), String> {
    for library in libraries {
        if !library.is_ascii()
            || library.contains(['/', '\\'])
            || library.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(format!(
                "Buck validate candidate has nonportable DT_NEEDED entry {library:?}"
            ));
        }
        let Some((stem, abi)) = library.split_once(".so.") else {
            return Err(format!(
                "Buck validate candidate has unversioned DT_NEEDED entry {library:?}"
            ));
        };
        if stem.is_empty()
            || abi.is_empty()
            || abi.split('.').any(|component| {
                component.is_empty() || !component.bytes().all(|b| b.is_ascii_digit())
            })
        {
            return Err(format!(
                "Buck validate candidate has nonportable DT_NEEDED ABI {library:?}"
            ));
        }
    }
    let expected = RELEASE_NEEDED_LIBRARIES
        .iter()
        .map(|library| (*library).to_owned())
        .collect::<BTreeSet<_>>();
    if libraries != &expected {
        let missing = expected.difference(libraries).cloned().collect::<Vec<_>>();
        let unexpected = libraries.difference(&expected).cloned().collect::<Vec<_>>();
        return Err(format!(
            "Buck validate candidate DT_NEEDED set differs from the reviewed portable contract; missing={missing:?} unexpected={unexpected:?}"
        ));
    }
    Ok(())
}

fn validate_release_rpath(path: &Path) -> Result<(), String> {
    let rpath = dynamic_values(path, "RPATH")?;
    let runpath = dynamic_values(path, "RUNPATH")?;
    if rpath != [RELEASE_RPATH] || !runpath.is_empty() {
        return Err(format!(
            "Buck release candidate must carry exactly DT_RPATH {RELEASE_RPATH:?} and no DT_RUNPATH; rpath={rpath:?} runpath={runpath:?}"
        ));
    }
    let components = RELEASE_RPATH.split(':').collect::<Vec<_>>();
    if components
        != [
            "$ORIGIN/../install_pkg/rsrcs/hermit-runtime",
            "$ORIGIN/install/rsrcs/hermit-runtime",
        ]
        || components.iter().any(|component| {
            component.is_empty() || component.starts_with('/') || !component.starts_with("$ORIGIN/")
        })
    {
        return Err("reviewed Buck release RPATH contains an ambient or absolute component".into());
    }
    Ok(())
}

fn runtime_hashes(input: &UnwindBuckInput) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            UNWIND_ARCH_SONAME.to_owned(),
            input.source_sha256[UNWIND_ARCH_SONAME].clone(),
        ),
        (
            UNWIND_CORE_SONAME.to_owned(),
            input.source_sha256[UNWIND_CORE_SONAME].clone(),
        ),
    ])
}

fn verify_runtime_directory(
    runtime: &Path,
    expected: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let metadata = fs::symlink_metadata(runtime).map_err(|error| {
        format!(
            "Hermit runtime directory {} is unavailable: {error}",
            runtime.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Hermit runtime bundle must be a real directory".into());
    }
    let mut names = fs::read_dir(runtime)
        .map_err(|error| format!("cannot enumerate Hermit runtime bundle: {error}"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot enumerate Hermit runtime bundle: {error}"))?;
    names.sort();
    let mut expected_names = vec![
        OsString::from(UNWIND_ARCH_SONAME),
        OsString::from(UNWIND_CORE_SONAME),
    ];
    expected_names.sort();
    if names != expected_names {
        return Err(format!(
            "Hermit runtime bundle has unexpected entries {names:?}"
        ));
    }
    let arch = require_nonempty_regular_file(
        &runtime.join(UNWIND_ARCH_SONAME),
        "installed libunwind architecture runtime",
    )?;
    let core = require_nonempty_regular_file(
        &runtime.join(UNWIND_CORE_SONAME),
        "installed libunwind core runtime",
    )?;
    verify_unwind_shared_pair(&arch, &core)?;
    let actual = BTreeMap::from([
        (UNWIND_ARCH_SONAME.to_owned(), sha256(&arch)?),
        (UNWIND_CORE_SONAME.to_owned(), sha256(&core)?),
    ]);
    if &actual != expected {
        return Err(format!(
            "Hermit runtime hashes differ from the declared unwind closure; expected={expected:?} actual={actual:?}"
        ));
    }
    Ok(actual)
}

fn verify_runtime_install(
    install: &Path,
    expected: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    verify_runtime_directory(&install.join(UNWIND_RUNTIME_RELATIVE), expected)
}

fn install_runtime_bundle(
    install: &Path,
    input: &UnwindBuckInput,
) -> Result<BTreeMap<String, String>, String> {
    verify_unwind_buck_input(input)?;
    let parent = install.join("rsrcs");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let destination = parent.join("hermit-runtime");
    let temporary = parent.join(format!(".hermit-runtime.tmp-{}", unique_identity()?));
    create_exclusive_directory(&temporary, "temporary Hermit runtime bundle")?;
    let result = (|| {
        for name in [UNWIND_ARCH_SONAME, UNWIND_CORE_SONAME] {
            let source = input.root.join(name);
            let expected = &input.source_sha256[name];
            let before = sha256(&source)?;
            fs::copy(&source, temporary.join(name))
                .map_err(|error| format!("failed to copy runtime {name}: {error}"))?;
            let after = sha256(&source)?;
            if before != *expected
                || after != *expected
                || sha256(&temporary.join(name))? != *expected
            {
                return Err(format!("runtime {name} changed while copied"));
            }
        }
        verify_runtime_directory(&temporary, &runtime_hashes(input))
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    if let Ok(metadata) = fs::symlink_metadata(&destination) {
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&destination)
                .map_err(|error| format!("failed to replace {}: {error}", destination.display()))?;
        } else {
            fs::remove_file(&destination)
                .map_err(|error| format!("failed to replace {}: {error}", destination.display()))?;
        }
    }
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_dir_all(&temporary);
        format!(
            "failed to publish Hermit runtime bundle {}: {error}",
            destination.display()
        )
    })?;
    let actual = verify_runtime_install(install, &runtime_hashes(input))?;
    verify_unwind_buck_input(input)?;
    Ok(actual)
}

fn prepare_runtime_install(
    evidence_dir: &Path,
    input: &UnwindBuckInput,
) -> Result<PathBuf, String> {
    let install = evidence_dir.join("runtime-install");
    create_exclusive_directory(&install, "runtime-only install bundle")?;
    install_runtime_bundle(&install, input)?;
    Ok(install)
}

fn loader_probe(binary: &Path, description: &str) -> Result<(), String> {
    validate_release_rpath(binary)?;
    let output = Command::new("env")
        .args(["-u", "LD_LIBRARY_PATH"])
        .arg("timeout")
        .args(["--signal=TERM", "--kill-after=2s", "10s"])
        .arg(binary)
        .args(["version", "--json"])
        .output()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    if !output.status.success() || !output.stderr.is_empty() || output.stdout.is_empty() {
        return Err(format!(
            "{description} failed without LD_LIBRARY_PATH: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn require_equal(label: &str, cargo: &str, buck: &str) -> Result<(), String> {
    if cargo == buck {
        Ok(())
    } else {
        Err(format!(
            "Cargo/Buck {label} mismatch; rebuild both at the same SHA/configuration and inspect retained evidence"
        ))
    }
}

fn require_nonempty_regular_file(path: &Path, description: &str) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "{description} {} must be a nonempty regular file, not a symlink",
            path.display()
        ));
    }
    Ok(path.to_owned())
}

fn parse_resource_manifest(
    path: &Path,
    description: &str,
) -> Result<BTreeMap<String, String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    let mut inventory = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let (hash, relative) = line.split_once("  ").ok_or_else(|| {
            format!(
                "{description} {} has malformed row {}",
                path.display(),
                index + 1
            )
        })?;
        let relative_path = Path::new(relative);
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "{description} {} has unsafe or invalid row {}",
                path.display(),
                index + 1
            ));
        }
        if inventory
            .insert(relative.to_owned(), hash.to_owned())
            .is_some()
        {
            return Err(format!(
                "{description} {} repeats resource {relative:?}",
                path.display()
            ));
        }
    }
    if inventory.is_empty() {
        return Err(format!(
            "{description} {} contains no resources",
            path.display()
        ));
    }
    Ok(inventory)
}

fn inspect_published_bundle(root: &Path, name: &str) -> Result<PublishedBundle, String> {
    let binary = require_absolute_file(&root.join("hermit"), &format!("verified {name} Hermit"))?;
    let install = require_absolute_directory(
        &root.join("install"),
        &format!("verified {name} install bundle"),
    )?;
    let hermit_manifest = require_nonempty_regular_file(
        &root.join("hermit.sha256"),
        &format!("verified {name} Hermit manifest"),
    )?;
    let resources_manifest = require_nonempty_regular_file(
        &root.join("resources.sha256"),
        &format!("verified {name} resource manifest"),
    )?;
    let binary_sha256 = sha256(&binary)?;
    let claimed_binary_sha256 = fs::read_to_string(&hermit_manifest)
        .map_err(|error| {
            format!(
                "verified {name} Hermit manifest {} is unreadable: {error}",
                hermit_manifest.display()
            )
        })?
        .trim()
        .to_owned();
    if claimed_binary_sha256 != binary_sha256 {
        return Err(format!(
            "verified {name} Hermit manifest does not bind the candidate binary"
        ));
    }
    let resource_inventory = parse_resource_manifest(
        &resources_manifest,
        &format!("verified {name} resource manifest"),
    )?;
    Ok(PublishedBundle {
        root: root.to_owned(),
        binary,
        install,
        hermit_manifest_sha256: sha256(&hermit_manifest)?,
        resources_manifest_sha256: sha256(&resources_manifest)?,
        hermit_manifest,
        resources_manifest,
        binary_sha256,
        resource_inventory,
    })
}

fn reverify_published_bundle(
    repository_root: &Path,
    expected: &PublishedBundle,
    name: &str,
) -> Result<(), String> {
    let resolved = PathBuf::from(output_text(
        Command::new(repository_root.join("ci/verify-hermit-e2e-artifact.sh")).arg(&expected.root),
        &format!("reverify isolated {name} shadow bundle"),
    )?);
    let resolved = resolved
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize reverified {name} bundle: {error}"))?;
    if resolved != expected.root {
        return Err(format!(
            "reverified {name} bundle resolved to {}, expected {}",
            resolved.display(),
            expected.root.display()
        ));
    }
    let actual = inspect_published_bundle(&resolved, name)?;
    if &actual != expected {
        return Err(format!(
            "verified {name} bundle changed after its tested snapshot was created"
        ));
    }
    Ok(())
}

fn require_equal_resource_manifests(
    cargo: &PublishedBundle,
    buck: &PublishedBundle,
) -> Result<(), String> {
    let cargo_bytes = fs::read(&cargo.resources_manifest).map_err(|error| {
        format!(
            "Cargo copied resource manifest {} is unreadable: {error}",
            cargo.resources_manifest.display()
        )
    })?;
    let buck_bytes = fs::read(&buck.resources_manifest).map_err(|error| {
        format!(
            "Buck copied resource manifest {} is unreadable: {error}",
            buck.resources_manifest.display()
        )
    })?;
    if cargo_bytes != buck_bytes
        || cargo.resources_manifest_sha256 != buck.resources_manifest_sha256
        || cargo.resource_inventory != buck.resource_inventory
    {
        return Err(
            "Cargo/Buck copied install manifests differ in inventory or resource hashes; refusing mixed mutable caller inputs"
                .to_owned(),
        );
    }
    Ok(())
}

fn publish_verified_bundle(
    root: &Path,
    evidence_dir: &Path,
    name: &str,
    binary: &Path,
    install_bundle: &Path,
    runtime_install: &Path,
) -> Result<PublishedBundle, String> {
    let artifact_root = evidence_dir.join(format!("{name}-artifacts"));
    let pointer = evidence_dir.join(format!("{name}-artifact.path"));
    checked_output(
        Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
            binary.as_os_str(),
            artifact_root.as_os_str(),
            pointer.as_os_str(),
            install_bundle.as_os_str(),
            OsStr::new("--runtime-overlay"),
            runtime_install.as_os_str(),
        ]),
        &format!("publish isolated {name} shadow bundle"),
    )?;
    let resolved = PathBuf::from(output_text(
        Command::new(root.join("ci/verify-hermit-e2e-artifact.sh")).arg(&pointer),
        &format!("verify isolated {name} shadow bundle"),
    )?);
    let canonical_evidence = evidence_dir
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize {}: {error}", evidence_dir.display()))?;
    let canonical_resolved = resolved
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize verified {name} bundle: {error}"))?;
    if !canonical_resolved.starts_with(&canonical_evidence) {
        return Err(format!(
            "verified {name} bundle escaped ignored evidence root: {}",
            canonical_resolved.display()
        ));
    }
    inspect_published_bundle(&canonical_resolved, name)
}

/// Publish the Buck candidate from the same pre-overlay caller install as
/// Cargo. The published Cargo install already carries the overlay's
/// hermit-runtime copy, which the publisher correctly refuses as an undeclared
/// closure in a source bundle. Reading the caller twice is safe because the
/// manifest comparison refuses any change between the two publications.
fn publish_buck_shadow_bundle(
    root: &Path,
    evidence_dir: &Path,
    buck_binary: &Path,
    caller_install_bundle: &Path,
    runtime_install: &Path,
    cargo_bundle: &PublishedBundle,
) -> Result<PublishedBundle, String> {
    let buck_bundle = publish_verified_bundle(
        root,
        evidence_dir,
        "buck",
        buck_binary,
        caller_install_bundle,
        runtime_install,
    )?;
    require_equal_resource_manifests(cargo_bundle, &buck_bundle)?;
    Ok(buck_bundle)
}

#[derive(Debug)]
struct CandidateOutput {
    stdout: String,
    /// The untrimmed stdout bytes, for comparisons that must be exact.
    exact_stdout: Vec<u8>,
}

struct CandidateInvocation<'a> {
    safehermit: &'a Path,
    binary: &'a Path,
    install_bundle: &'a Path,
    evidence_dir: &'a Path,
    identity: &'a str,
    data_dir: &'a Path,
    arguments: &'a [&'a str],
    deadline_seconds: u64,
    description: &'a str,
}

fn write_new_file(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "refusing to replace existing {description} {}: {error}",
                path.display()
            )
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to retain {description} {}: {error}", path.display()))
}

fn validate_and_surface_safehermit_report(path: &Path, description: &str) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "safehermit report for {description} is unreadable at {}: {error}",
            path.display()
        )
    })?;
    let bound_lines = text
        .lines()
        .filter(|line| line.starts_with("safehermit: bound."))
        .collect::<Vec<_>>();
    let required_bounds = ["wall", "cgroup", "disk", "bytes"];
    let required_missing = required_bounds.iter().any(|bound| {
        let prefix = format!("safehermit: bound.{bound}=APPLIED:");
        !bound_lines.iter().any(|line| line.starts_with(&prefix))
    });
    let unexpected_unapplied = bound_lines.iter().any(|line| {
        line.contains("NOT_APPLIED")
            && !line.starts_with("safehermit: bound.logfilter=NOT_APPLIED:by owner ruling")
    });
    if required_missing || unexpected_unapplied {
        return Err(format!(
            "safehermit report for {description} lacks applied wall/cgroup/disk/bytes bounds: {}",
            path.display()
        ));
    }
    for line in bound_lines {
        eprintln!("{description}: {line}");
    }
    Ok(())
}

fn run_safehermit(request: CandidateInvocation<'_>) -> Result<CandidateOutput, String> {
    let CandidateInvocation {
        safehermit,
        binary,
        install_bundle,
        evidence_dir,
        identity,
        data_dir,
        arguments,
        deadline_seconds,
        description,
    } = request;
    create_exclusive_directory(data_dir, &format!("{description} data directory"))?;
    let invocations = evidence_dir.join("candidate-invocations");
    fs::create_dir_all(&invocations)
        .map_err(|error| format!("failed to create {}: {error}", invocations.display()))?;
    let invocation = invocations.join(format!("{identity}-{}", unique_identity()?));
    create_exclusive_directory(&invocation, &format!("{description} invocation"))?;
    let report = invocation.join("safehermit.report");
    let output = Command::new(safehermit)
        .arg(format!("--sh-deadline={deadline_seconds}"))
        .arg("--sh-report")
        .arg(&report)
        .arg(binary)
        .args(arguments)
        .env("HERMIT_INSTALL_DIR", install_bundle)
        .env("HERMIT_DATA_DIR", data_dir)
        .output()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    write_new_file(
        &invocation.join("stdout"),
        &output.stdout,
        "candidate stdout",
    )?;
    write_new_file(
        &invocation.join("stderr"),
        &output.stderr,
        "candidate stderr",
    )?;
    validate_and_surface_safehermit_report(&report, description)?;
    if !output.status.success() {
        return Err(format!(
            "{description} failed with {}: {}; retained invocation={}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
            invocation.display()
        ));
    }
    let stdout = String::from_utf8(output.stdout.clone())
        .map_err(|error| format!("{description} emitted non-UTF-8 stdout: {error}"))?
        .trim()
        .to_owned();
    Ok(CandidateOutput {
        stdout,
        exact_stdout: output.stdout,
    })
}

fn require_object_keys(
    value: &Value,
    location: &str,
    required: &[&str],
    allowed: &[&str],
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("strict verify report {location} must be an object"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "strict verify report {location} contains unknown field {key:?}"
            ));
        }
    }
    for key in required {
        if !object.contains_key(*key) {
            return Err(format!(
                "strict verify report {location} lacks required field {key:?}"
            ));
        }
    }
    Ok(())
}

fn verify_closed_report_shape(value: &Value) -> Result<(), String> {
    const TOP_LEVEL: &[&str] = &[
        "verified",
        "bitwise_parity",
        "verdict",
        "no_result_reason",
        "infrastructure_error",
        "comparison",
        "compared_log_messages",
        "compared_outputs",
        "dbt_counted_branches",
        "runtime",
        "guest_exit_code",
        "guest_signal",
        "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds",
        "first_divergent_record",
        "first_divergent_syscall",
        "first_divergent_left_message",
        "first_divergent_right_message",
    ];
    const CURRENT_REQUIRED: &[&str] = &[
        "verified",
        "bitwise_parity",
        "verdict",
        "no_result_reason",
        "infrastructure_error",
        "comparison",
        "compared_log_messages",
        "compared_outputs",
        "guest_exit_code",
        "guest_signal",
        "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds",
        "first_divergent_record",
        "first_divergent_syscall",
        "first_divergent_left_message",
        "first_divergent_right_message",
    ];
    require_object_keys(value, "root", CURRENT_REQUIRED, TOP_LEVEL)?;
    let comparison = &value["comparison"];
    const COMPARISON: &[&str] = &[
        "strictness",
        "display_name",
        "compare_logs",
        "compare_io_buffers",
        "log_scope",
        "record_envelope",
        "virtualize_time",
        "strip_lines",
        "canonicalize_addresses",
        "full_trace",
        "exact_remainder",
        "stripped_prefixes",
        "canonicalizations",
        "ignore_lines",
        "skip_commit",
        "skip_detlog",
    ];
    require_object_keys(comparison, "comparison", COMPARISON, COMPARISON)?;
    require_object_keys(
        &value["compared_log_messages"],
        "compared_log_messages",
        &["left", "right"],
        &["left", "right"],
    )?;
    require_object_keys(
        &value["compared_outputs"],
        "compared_outputs",
        &["left", "right"],
        &["left", "right"],
    )?;
    for side in ["left", "right"] {
        require_object_keys(
            &value["compared_outputs"][side],
            &format!("compared_outputs.{side}"),
            &[
                "exit_code",
                "signal",
                "stdout_sha256",
                "stdout_bytes",
                "stderr_sha256",
                "stderr_bytes",
            ],
            &[
                "exit_code",
                "signal",
                "stdout_sha256",
                "stdout_bytes",
                "stderr_sha256",
                "stderr_bytes",
            ],
        )?;
    }
    if let Some(runtime) = value.get("runtime").filter(|value| !value.is_null()) {
        require_object_keys(runtime, "runtime", &[], &["run1", "run2"])?;
        for run in ["run1", "run2"] {
            if let Some(stats) = runtime.get(run).filter(|value| !value.is_null()) {
                require_object_keys(
                    stats,
                    &format!("runtime.{run}"),
                    &["scheduler_turns", "virtual_nanoseconds"],
                    &["scheduler_turns", "virtual_nanoseconds", "syscalls"],
                )?;
            }
        }
    }
    if let Some(branches) = value
        .get("dbt_counted_branches")
        .filter(|value| !value.is_null())
    {
        require_object_keys(
            branches,
            "dbt_counted_branches",
            &["left", "right"],
            &["left", "right"],
        )?;
    }
    Ok(())
}

fn verify_report(
    path: &Path,
    expected_virtualize_time: bool,
) -> Result<VerificationReport, String> {
    let bytes = fs::read(path).map_err(|error| {
        format!(
            "strict verify report {} is unreadable: {error}",
            path.display()
        )
    })?;
    let report = VerificationReport::from_current_json_slice(&bytes)
        .map_err(|error| format!("strict verify report {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("strict verify report {}: {error}", path.display()))?;
    verify_closed_report_shape(&value)?;
    report.require_canonical_match().map_err(|error| {
        format!(
            "strict verify report {} is not a canonical match: {error}",
            path.display()
        )
    })?;
    let comparison = report
        .comparison
        .as_ref()
        .ok_or_else(|| "canonical validator admitted a report without comparison".to_owned())?;
    if report.verdict != Verdict::Matched
        || report.guest_exit_code != Some(0)
        || report.guest_signal.is_some()
        || report.no_result_reason.is_some()
        || report.infrastructure_error.is_some()
        || report.first_divergent_scheduler_turn.is_some()
        || report.first_divergent_virtual_nanoseconds.is_some()
        || report.first_divergent_record.is_some()
        || report.first_divergent_syscall.is_some()
        || report.first_divergent_left_message.is_some()
        || report.first_divergent_right_message.is_some()
        || comparison.strictness != LogCompareStrictness::Canonical
        || comparison.display_name.as_deref() != Some("BitwiseInfoV1")
        || !comparison.compare_logs
        || comparison.compare_io_buffers != Some(true)
        || comparison.log_scope != Some(ComparedLogScope::Info)
        || comparison.record_envelope != RecordEnvelopeReport::AllRecordsV1
        || comparison.virtualize_time != Some(expected_virtualize_time)
        || comparison.strip_lines != Some(false)
        || comparison.canonicalize_addresses != Some(true)
        || comparison.full_trace != Some(true)
        || comparison.exact_remainder != Some(true)
        || comparison.stripped_prefixes.as_deref()
            != Some(&["real-wall-clock-prefix/v1".to_owned()][..])
        || comparison.canonicalizations.as_deref()
            != Some(&["host-address-to-first-appearance-ordinal/v1".to_owned()][..])
        || comparison.ignore_lines != Some(false)
        || comparison.skip_commit != Some(false)
        || comparison.skip_detlog != Some(false)
    {
        return Err(format!(
            "strict verify report {} does not carry the exact BitwiseInfoV1 policy, expected virtualize_time={expected_virtualize_time}",
            path.display()
        ));
    }
    let counts = report
        .compared_log_messages
        .as_ref()
        .ok_or_else(|| "canonical report omitted compared INFO counts".to_owned())?;
    if counts.left == 0 || counts.left != counts.right {
        return Err(format!(
            "strict verify report {} compared invalid INFO counts {}/{}",
            path.display(),
            counts.left,
            counts.right
        ));
    }
    if let Some(runtime) = &report.runtime {
        if runtime.run1 != runtime.run2 {
            return Err(format!(
                "strict verify report {} has unequal run1/run2 runtime summaries",
                path.display()
            ));
        }
    }
    Ok(report)
}

/// The strict parity leg runs the tracked clock fixture under this pinned epoch.
/// An omitted `--epoch` captures host time per invocation, which is the only
/// reason two builds' virtual clocks ever differed; pinned, the trajectory and
/// the whole strict report, virtual time included, must be exactly equal.
/// Linux defines only CLOCK_REALTIME against the Unix epoch, so only the
/// fixture's `realtime` anchors are compared with it. CLOCK_MONOTONIC counts
/// from an unspecified point and is checked purely relatively. Hermit today
/// returns the same epoch-anchored value for every clockid
/// (`handle_clock_gettime` in detcore/src/syscalls/time.rs ignores it); that
/// known deviation, tracked as TaskGraph task
/// hermit-clock-gettime-ignores-clockid, must not become load-bearing here.
const CLOCK_PARITY_EPOCH: &str = "2026-01-01T00:00:00+00:00";
const CLOCK_PARITY_EPOCH_NS: u64 = 1_767_225_600_000_000_000;
/// Startup costs virtual milliseconds; a first REALTIME anchor this far past
/// the epoch means the epoch was not applied.
const CLOCK_PARITY_STARTUP_BOUND_NS: u64 = 60_000_000_000;
/// Each scheduler turn adds 500,000 virtual ns, so a turn inside a work segment
/// breaks the exact work-cost relations below. Pinned to the default, about 50
/// times the largest segment and longer than the whole run, so no turn lands
/// inside one; a lower value would redden both builds regardless of parity.
const CLOCK_PARITY_MAX_TIMESLICE: &str = "--max-timeslice=200000000";
const CLOCK_FIXTURE_SOURCE: &str = "tests/c/clock_exec_continuity.c";
const CLOCK_FIXTURE_WORK_ITERATIONS: [u64; 4] = [0, 100_000, 100_000, 200_000];

/// Mirrors the fixture's `do_work`, so a checksum proves the declared work ran.
fn clock_fixture_work_checksum(iterations: u64) -> u64 {
    (0..iterations).fold(27, |value: u64, _| {
        if value & 1 == 1 {
            value.wrapping_mul(3).wrapping_add(1)
        } else {
            value >> 1
        }
    })
}

fn take_canonical_decimal(text: &str) -> Option<(u64, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let (number, rest) = text.split_at(digits);
    if number.is_empty() || (number.len() > 1 && number.starts_with('0')) {
        return None;
    }
    Some((number.parse().ok()?, rest))
}

fn clock_advance(clock: &str, last: &mut u64, value: u64) -> Result<(), String> {
    if value <= *last {
        return Err(format!(
            "{clock} read {value} does not advance past {last}: virtual time froze, went \
             backwards, or was reset"
        ));
    }
    *last = value;
    Ok(())
}

/// What a valid clock trajectory binds to the strict report's virtual time.
#[derive(Debug, PartialEq, Eq)]
struct ClockTrajectory {
    /// The last CLOCK_REALTIME anchor: the only reads defined against the epoch.
    realtime_last: u64,
    /// Last minus first CLOCK_MONOTONIC read; their absolute values carry no claim.
    monotonic_span: u64,
}

/// Validates the fixture's exact stdout as a closed trajectory (3 generations
/// of 8 main reads, a REALTIME anchor, the 4 work segments, 2 serialized
/// threads of 4 reads, a second REALTIME anchor, then a summary; see the
/// fixture header). The two clocks are checked independently and never
/// against each other. Every MONOTONIC read must advance past the previous
/// one in emission order, across exec and thread boundaries, with no absolute
/// floor or window. Every REALTIME anchor must advance past the previous one,
/// starting from the pinned epoch, and generation 0's must open within the
/// startup bound. No generation's first MONOTONIC read or opening REALTIME
/// anchor may sit on a whole second; checksums must match; equal work must cost
/// exactly equal virtual time, more work strictly more, and double work exactly
/// its two halves (cost3 - cost0 == (cost1 - cost0) + (cost2 - cost0)); and
/// summaries must match their samples. The work-cost relations are exact only
/// while no scheduler turn falls inside a segment, which
/// `CLOCK_PARITY_MAX_TIMESLICE` pins.
fn parse_clock_trajectory(stdout: &str) -> Result<ClockTrajectory, String> {
    let body = stdout
        .strip_suffix('\n')
        .ok_or("clock trajectory must end with one newline")?;
    let mut lines = body.split('\n');
    let mut record = |head: String, keys: &[&str]| -> Result<Vec<u64>, String> {
        let line = lines
            .next()
            .ok_or(format!("clock trajectory lacks {head:?}"))?;
        let mut rest = line.strip_prefix(head.as_str()).ok_or(format!(
            "clock trajectory expected {head:?}, found {line:?}"
        ))?;
        let mut values = Vec::new();
        for key in keys {
            let (value, tail) = rest
                .strip_prefix(&format!(" {key}="))
                .and_then(take_canonical_decimal)
                .ok_or(format!("clock trajectory {line:?} has a malformed {key}"))?;
            values.push(value);
            rest = tail;
        }
        match rest {
            "" => Ok(values),
            _ => Err(format!("clock trajectory {line:?} has trailing text")),
        }
    };
    const MONOTONIC: &str = "CLOCK_MONOTONIC";
    const REALTIME: &str = "CLOCK_REALTIME";
    let mut last = 0;
    let mut monotonic_first = None;
    let mut realtime = CLOCK_PARITY_EPOCH_NS;
    for generation in 0..3 {
        let mut main = Vec::new();
        for index in 0..8 {
            let ns = record(
                format!("sample gen={generation} source=main index={index}"),
                &["ns"],
            )?[0];
            clock_advance(MONOTONIC, &mut last, ns)?;
            main.push(ns);
        }
        let first = main[0];
        monotonic_first.get_or_insert(first);
        if first % 1_000_000_000 == 0 {
            return Err(format!(
                "clock generation {generation} opens on a whole second {first}"
            ));
        }
        let open = record(format!("realtime gen={generation} phase=open"), &["ns"])?[0];
        clock_advance(REALTIME, &mut realtime, open)?;
        if generation == 0 && open - CLOCK_PARITY_EPOCH_NS >= CLOCK_PARITY_STARTUP_BOUND_NS {
            return Err(format!(
                "clock REALTIME anchor opens {open}, outside the startup bound"
            ));
        }
        if open % 1_000_000_000 == 0 {
            return Err(format!(
                "clock generation {generation} REALTIME anchor opens on a whole second {open}"
            ));
        }
        let mut cost = Vec::new();
        for (index, iterations) in CLOCK_FIXTURE_WORK_ITERATIONS.iter().enumerate() {
            let work = record(
                format!("work gen={generation} index={index} iterations={iterations}"),
                &["before_ns", "after_ns", "checksum"],
            )?;
            clock_advance(MONOTONIC, &mut last, work[0])?;
            clock_advance(MONOTONIC, &mut last, work[1])?;
            if work[2] != clock_fixture_work_checksum(*iterations) {
                return Err(format!("clock work {index} checksum {} is wrong", work[2]));
            }
            cost.push(work[1] - work[0]);
        }
        // cost3 - cost0 == (cost1 - cost0) + (cost2 - cost0), rearranged to avoid
        // unsigned underflow and widened so huge timestamps cannot overflow.
        let wide = |a: u64, b: u64| u128::from(a) + u128::from(b);
        if cost[1] != cost[2]
            || cost[0] >= cost[1]
            || wide(cost[3], cost[0]) != wide(cost[1], cost[2])
        {
            return Err(format!(
                "clock generation {generation} work cost {cost:?} ns: equal work must cost \
                 equal virtual time, more work strictly more, and double work its two halves"
            ));
        }
        for thread in 0..2 {
            for index in 0..4 {
                let head =
                    format!("sample gen={generation} source=thread index={index} thread={thread}");
                clock_advance(MONOTONIC, &mut last, record(head, &["ns"])?[0])?;
            }
        }
        let close = record(format!("realtime gen={generation} phase=close"), &["ns"])?[0];
        clock_advance(REALTIME, &mut realtime, close)?;
        let min_delta = main
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .min()
            .unwrap_or(0);
        if record(format!("gen={generation}"), &["first", "last", "min_delta"])?
            != [first, last, min_delta]
        {
            return Err(format!(
                "clock generation {generation} summary disagrees with its samples"
            ));
        }
    }
    record("clock exec continuity holds across 2 execs".to_owned(), &[])?;
    match (lines.next(), monotonic_first) {
        (None, Some(first)) => Ok(ClockTrajectory {
            realtime_last: realtime,
            monotonic_span: last - first,
        }),
        (Some(extra), _) => Err(format!("clock trajectory has an extra record {extra:?}")),
        (None, None) => Err("clock trajectory has no MONOTONIC read".to_owned()),
    }
}

/// Requires both builds' trajectories to be valid and byte-identical, their
/// strict reports to be exactly equal, and the report to bind the trajectory:
/// it compared these stdout bytes, and its virtual time outlasts both the last
/// REALTIME anchor's distance from the epoch and the MONOTONIC span.
fn verify_clock_parity(
    cargo_stdout: &[u8],
    buck_stdout: &[u8],
    cargo_report: &Path,
    buck_report: &Path,
) -> Result<VerificationReport, String> {
    let utf8 = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string());
    let (cargo_text, buck_text) = (utf8(cargo_stdout)?, utf8(buck_stdout)?);
    let trajectory =
        parse_clock_trajectory(&cargo_text).map_err(|error| format!("Cargo {error}"))?;
    parse_clock_trajectory(&buck_text).map_err(|error| format!("Buck {error}"))?;
    require_equal(
        "ptrace fixed-epoch clock trajectory",
        &cargo_text,
        &buck_text,
    )?;
    let report = verify_report(cargo_report, true)?;
    if report != verify_report(buck_report, true)? {
        return Err("Cargo/Buck ptrace clock strict reports differ; nothing is erased".to_owned());
    }
    let compared = report
        .compared_outputs
        .as_ref()
        .map(|outputs| &outputs.left);
    if compared.map(|output| (output.stdout_sha256.clone(), output.stdout_bytes))
        != Some((sha256_bytes(cargo_stdout)?, cargo_stdout.len() as u64))
    {
        return Err("clock strict report compared a different stdout".to_owned());
    }
    let virtual_ns = report
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.run1.as_ref());
    match virtual_ns {
        Some(run)
            if run.virtual_nanoseconds <= trajectory.realtime_last - CLOCK_PARITY_EPOCH_NS =>
        {
            Err("clock strict report virtual time ends before the last REALTIME anchor".to_owned())
        }
        Some(run) if run.virtual_nanoseconds <= trajectory.monotonic_span => {
            Err("clock strict report virtual time is shorter than the MONOTONIC span".to_owned())
        }
        Some(_) => Ok(report),
        None => Err("clock strict report lacks a runtime summary".to_owned()),
    }
}

/// Compiles the fixture from a copy retained as evidence, so the receipt binds
/// the exact source compiled.
fn compile_clock_fixture(root: &Path, evidence_dir: &Path) -> Result<PathBuf, String> {
    let source = root.join(CLOCK_FIXTURE_SOURCE);
    let bytes = fs::read(&source).map_err(|error| format!("{}: {error}", source.display()))?;
    let retained = evidence_dir.join("clock-fixture.c");
    write_new_file(&retained, &bytes, "clock fixture source")?;
    let binary = evidence_dir.join("clock-fixture");
    checked_output(
        Command::new("gcc")
            .args(["-O0", "-D_GNU_SOURCE", "-pthread"])
            .arg(&retained)
            .arg("-o")
            .arg(&binary),
        "compile clock trajectory fixture",
    )?;
    Ok(binary)
}

/// The parity verdict is the strict leg's fixed-epoch clock trajectory. The
/// `/bin/true` record leg is a record/replay smoke case only: record mode reports
/// no runtime and currently refuses the fixture's clock_gettime, so it cannot
/// carry clock evidence.
fn behavioral_parity(
    root: &Path,
    safehermit: &Path,
    cargo_binary: &Path,
    buck_binary: &Path,
    cargo_install: &Path,
    buck_install: &Path,
    evidence_dir: &Path,
) -> Result<(), String> {
    {
        // The trajectory compares Cargo with Buck on ptrace alone; it is not
        // DBT or KVM virtual-time evidence (docs/BUCK2_OSS.md, Virtual-time
        // scope).
        let backend = "ptrace";
        let fixture = compile_clock_fixture(root, evidence_dir)?;
        let fixture_text = fixture.to_string_lossy();
        let epoch = format!("--epoch={CLOCK_PARITY_EPOCH}");
        let cargo_report = evidence_dir.join(format!("cargo-{backend}-verify.json"));
        let buck_report = evidence_dir.join(format!("buck-{backend}-verify.json"));
        let cargo_report_text = cargo_report.to_string_lossy();
        let buck_report_text = buck_report.to_string_lossy();
        let cargo_identity = format!("cargo-{backend}-verify");
        let cargo_data = evidence_dir.join(format!("cargo-{backend}-verify-data"));
        let cargo_arguments = [
            "--log=info",
            "--backend",
            backend,
            "run",
            "--base-env=minimal",
            &epoch,
            CLOCK_PARITY_MAX_TIMESLICE,
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            &cargo_report_text,
            "--",
            &fixture_text,
        ];
        let cargo_description = format!("Cargo {backend} strict verify through safehermit");
        let cargo = run_safehermit(CandidateInvocation {
            safehermit,
            binary: cargo_binary,
            install_bundle: cargo_install,
            evidence_dir,
            identity: &cargo_identity,
            data_dir: &cargo_data,
            arguments: &cargo_arguments,
            deadline_seconds: VERIFY_DEADLINE_SECONDS,
            description: &cargo_description,
        })?;
        let buck_identity = format!("buck-{backend}-verify");
        let buck_data = evidence_dir.join(format!("buck-{backend}-verify-data"));
        let buck_arguments = [
            "--log=info",
            "--backend",
            backend,
            "run",
            "--base-env=minimal",
            &epoch,
            CLOCK_PARITY_MAX_TIMESLICE,
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            &buck_report_text,
            "--",
            &fixture_text,
        ];
        let buck_description = format!("Buck {backend} strict verify through safehermit");
        let buck = run_safehermit(CandidateInvocation {
            safehermit,
            binary: buck_binary,
            install_bundle: buck_install,
            evidence_dir,
            identity: &buck_identity,
            data_dir: &buck_data,
            arguments: &buck_arguments,
            deadline_seconds: VERIFY_DEADLINE_SECONDS,
            description: &buck_description,
        })?;
        write_new_file(
            &evidence_dir.join("cargo-ptrace-clock-trajectory.txt"),
            &cargo.exact_stdout,
            "Cargo clock trajectory",
        )?;
        write_new_file(
            &evidence_dir.join("buck-ptrace-clock-trajectory.txt"),
            &buck.exact_stdout,
            "Buck clock trajectory",
        )?;
        verify_clock_parity(
            &cargo.exact_stdout,
            &buck.exact_stdout,
            &cargo_report,
            &buck_report,
        )?;
    }

    // Smoke case only; see the function comment.
    let cargo_record_report = evidence_dir.join("cargo-ptrace-record-verify.json");
    let buck_record_report = evidence_dir.join("buck-ptrace-record-verify.json");
    let cargo_record_data = evidence_dir.join("cargo-record-data");
    let buck_record_data = evidence_dir.join("buck-record-data");
    let cargo_record_report_text = cargo_record_report.to_string_lossy();
    let buck_record_report_text = buck_record_report.to_string_lossy();
    let cargo_record_data_text = cargo_record_data.to_string_lossy();
    let buck_record_data_text = buck_record_data.to_string_lossy();
    let record_arguments = [
        "--log=info",
        "--backend",
        "ptrace",
        "record",
        "start",
        "--strict",
        "--verify",
        "--verify-strict",
        "--record-timeout=30",
        "--verify-json",
        &cargo_record_report_text,
        "--data-dir",
        &cargo_record_data_text,
        "--",
        "/bin/true",
    ];
    let cargo = run_safehermit(CandidateInvocation {
        safehermit,
        binary: cargo_binary,
        install_bundle: cargo_install,
        evidence_dir,
        identity: "cargo-ptrace-record",
        data_dir: &cargo_record_data,
        arguments: &record_arguments,
        deadline_seconds: VERIFY_DEADLINE_SECONDS,
        description: "Cargo ptrace record/replay through safehermit",
    })?;
    let buck_record_arguments = [
        "--log=info",
        "--backend",
        "ptrace",
        "record",
        "start",
        "--strict",
        "--verify",
        "--verify-strict",
        "--record-timeout=30",
        "--verify-json",
        &buck_record_report_text,
        "--data-dir",
        &buck_record_data_text,
        "--",
        "/bin/true",
    ];
    let buck = run_safehermit(CandidateInvocation {
        safehermit,
        binary: buck_binary,
        install_bundle: buck_install,
        evidence_dir,
        identity: "buck-ptrace-record",
        data_dir: &buck_record_data,
        arguments: &buck_record_arguments,
        deadline_seconds: VERIFY_DEADLINE_SECONDS,
        description: "Buck ptrace record/replay through safehermit",
    })?;
    require_equal("ptrace record/replay stdout", &cargo.stdout, &buck.stdout)?;
    let cargo_typed = verify_report(&cargo_record_report, false)?;
    let buck_typed = verify_report(&buck_record_report, false)?;
    if cargo_typed != buck_typed {
        return Err(
            "Cargo/Buck ptrace record/replay typed strict report mismatch; inspect retained reports"
                .to_owned(),
        );
    }
    Ok(())
}

fn normalized_matrix(path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("DBT matrix {} is unreadable: {error}", path.display()))?;
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("DBT matrix {} is empty", path.display()))?;
    let columns = header.split('\t').collect::<Vec<_>>();
    let seconds = columns
        .iter()
        .position(|column| *column == "seconds")
        .ok_or_else(|| format!("DBT matrix {} has no seconds column", path.display()))?;
    let mut normalized = Vec::new();
    normalized.push(
        columns
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != seconds)
            .map(|(_, value)| *value)
            .collect::<Vec<_>>()
            .join("\t"),
    );
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != columns.len() {
            return Err(format!("DBT matrix {} has a malformed row", path.display()));
        }
        normalized.push(
            fields
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != seconds)
                .map(|(_, value)| *value)
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    if normalized.len() <= 1 {
        return Err(format!(
            "DBT matrix {} contains no result rows",
            path.display()
        ));
    }
    Ok(format!("{}\n", normalized.join("\n")))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MatrixProxyInvocation {
    invocation_schema: String,
    case_identity: String,
    role: String,
    normalized_argv: Vec<String>,
}

fn normalize_matrix_argument(argument: &str) -> String {
    let marker = "/hermit-backend-parity-";
    let Some(start) = argument.find(marker) else {
        return argument.to_owned();
    };
    let unique_start = start + 1;
    let suffix_start = argument[unique_start..]
        .find('/')
        .map(|offset| unique_start + offset)
        .unwrap_or(argument.len());
    format!(
        "{}$MATRIX_TMP{}",
        &argument[..unique_start],
        &argument[suffix_start..]
    )
}

fn matrix_case_from_guest(guest: &[String]) -> Result<&'static str, String> {
    let executable = guest
        .first()
        .ok_or_else(|| "matrix proxy invocation has an empty guest command".to_owned())?;
    let basename = Path::new(executable)
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "matrix proxy guest executable has no UTF-8 basename".to_owned())?;
    let trailing = guest.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
    let case = match (basename, trailing.as_slice()) {
        ("echo", ["hello world"]) => "hello_stdout",
        ("printf", ["%s|%s\n", "alpha", "two words"]) => "argument_forwarding",
        ("true", []) => "exit_zero",
        ("sh", ["-c", "exit 23"]) => "exit_status",
        ("cat", [_]) => "file_read",
        ("syscall_file_io", []) => "file_mutation",
        ("syscall_file_metadata", []) => "file_metadata",
        ("io_uring_fallback", []) => "io_uring_fallback",
        ("listmount_enosys", []) => "listmount_unavailable",
        ("process_vm_readv_refusal", []) => "process_vm_readv_refusal",
        ("process_vm_writev_refusal", []) => "process_vm_writev_refusal",
        ("mmap_exec", []) => "executable_mmap",
        ("madvise_determinism", []) => "memory_advice",
        ("mmap_determinism", ["heap"]) => "heap_growth",
        ("mmap_determinism", ["multiple"]) => "anonymous_mmap_layout",
        ("mmap_determinism", ["shared"]) => "shared_anonymous_mmap",
        ("pthread_lifecycle", []) => "pthread_lifecycle",
        ("process_wait_lifecycle", ["--accounting-only"]) => "process_wait_accounting",
        ("process_wait_lifecycle", []) => "process_wait_lifecycle",
        ("cpuid_probe", []) => "cpuid_policy",
        ("clock_determinism", []) => "virtual_clock",
        ("random_sources", ["--root-only"]) => "random_sources",
        ("pid_probe", []) => "virtual_pid",
        ("scheduler_policy_queries", []) => "scheduler_policy_queries",
        ("signal_disposition", []) => "signal_disposition",
        ("sigaction_state", []) => "sigaction_state",
        ("sigprocmask_state", []) => "sigprocmask_state",
        ("sigaltstack_state", []) => "sigaltstack_state",
        _ => {
            return Err(format!(
                "matrix proxy cannot map guest command to an official case: {guest:?}"
            ));
        }
    };
    Ok(case)
}

fn classify_matrix_proxy_invocation(arguments: &[String]) -> Result<MatrixProxyInvocation, String> {
    let normalized_argv = arguments
        .iter()
        .map(|argument| normalize_matrix_argument(argument))
        .collect::<Vec<_>>();
    if arguments == ["host-capabilities", "--json"] {
        return Ok(MatrixProxyInvocation {
            invocation_schema: "hermit-matrix-proxy-invocation/v1".to_owned(),
            case_identity: "probe/host-capabilities".to_owned(),
            role: "probe".to_owned(),
            normalized_argv,
        });
    }
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| "matrix proxy invocation lacks a guest separator".to_owned())?;
    // The backend is a global option, so it may only precede the subcommand:
    // `hermit --backend dbt run ...`.
    let subcommand = match arguments {
        [flag, _backend, rest @ ..] if flag == "--backend" => rest,
        rest => rest,
    };
    if subcommand.first().map(String::as_str) != Some("run") {
        return Err("matrix proxy invocation is neither a probe nor `hermit run`".to_owned());
    }
    let guest = &arguments[separator + 1..];
    let is_dbt = arguments
        .windows(2)
        .any(|pair| pair == ["--backend", "dbt"]);
    let is_matrix_case = arguments
        .iter()
        .any(|argument| argument == "--max-timeslice=disabled");
    let (case_identity, role) = if is_dbt && !is_matrix_case && guest == ["/bin/true"] {
        ("probe/dbt-smoke".to_owned(), "probe".to_owned())
    } else if is_matrix_case {
        let case = matrix_case_from_guest(guest)?.to_owned();
        let role = if is_dbt {
            "dbt-run"
        } else {
            "ptrace-reference"
        };
        (case, role.to_owned())
    } else {
        return Err(format!(
            "matrix proxy invocation does not match the official smoke/case envelope: {arguments:?}"
        ));
    };
    Ok(MatrixProxyInvocation {
        invocation_schema: "hermit-matrix-proxy-invocation/v1".to_owned(),
        case_identity,
        role,
        normalized_argv,
    })
}

/// run_matrix's DBT private-/tmp stage, received as data rather than as the
/// process this proxy runs inside.
#[derive(Debug, PartialEq, Eq)]
struct PrivateTmpStage {
    host_tmp: PathBuf,
    mounts: Vec<(PathBuf, PathBuf)>,
    argv: Vec<String>,
}

/// What a DBT invocation recorded about its private /tmp before safehermit ran,
/// so the retained audit can re-check the readback without the (deleted) host
/// directory.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MatrixPrivateTmpEvidence {
    schema: String,
    host_tmp: PathBuf,
    host_tmp_device: u64,
    host_tmp_inode: u64,
    proxy_uid: u32,
    binds: Vec<(PathBuf, PathBuf)>,
    unshare: PathBuf,
    unshare_sha256: String,
    candidate: PathBuf,
    candidate_sha256: String,
}

const MATRIX_PRIVATE_TMP_SCHEMA: &str = "hermit-matrix-private-tmp/v2";

/// Re-check a retained invocation's private /tmp evidence: the typed record,
/// the recorded candidate against the leg's candidate bytes, the in-unit
/// readback against it, and that safehermit launched the recorded unshare.
fn verify_retained_private_tmp(
    invocation: &Path,
    description: &str,
    expected_candidate_sha256: &str,
) -> Result<String, String> {
    let path = invocation.join("private-tmp.json");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {description} private /tmp evidence: {error}"))?;
    let evidence: MatrixPrivateTmpEvidence =
        decode_typed_json(&text, "matrix private /tmp evidence")?;
    let beneath = |destination: &Path| {
        destination.starts_with(&evidence.host_tmp) && destination != evidence.host_tmp
    };
    if evidence.schema != MATRIX_PRIVATE_TMP_SCHEMA
        || !evidence.host_tmp.is_absolute()
        || !evidence.unshare.is_absolute()
        || !evidence.candidate.is_absolute()
        || evidence.candidate.starts_with("/tmp")
        || !valid_sha256(&evidence.unshare_sha256)
        || !valid_sha256(&evidence.candidate_sha256)
        || evidence
            .binds
            .iter()
            .any(|(source, destination)| !source.is_absolute() || !beneath(destination))
    {
        return Err(format!(
            "{description} private /tmp evidence has invalid semantics: {}",
            path.display()
        ));
    }
    let mut destinations = BTreeSet::new();
    if let Some((_, destination)) = evidence
        .binds
        .iter()
        .find(|(_, destination)| !destinations.insert(destination))
    {
        return Err(format!(
            "{description} private /tmp evidence binds {} twice: {}",
            destination.display(),
            path.display()
        ));
    }
    if evidence.candidate_sha256 != expected_candidate_sha256 {
        return Err(format!(
            "{description} private /tmp evidence records candidate {} but the leg's candidate is {expected_candidate_sha256}",
            evidence.candidate_sha256
        ));
    }
    let candidate_sha256 = sha256(&evidence.candidate)?;
    if candidate_sha256 != expected_candidate_sha256 {
        return Err(format!(
            "{description} private /tmp candidate {} now hashes to {candidate_sha256}, not {expected_candidate_sha256}",
            evidence.candidate.display()
        ));
    }
    let readback = invocation.join("private-tmp.readback");
    let observed = fs::read_to_string(&readback)
        .map_err(|error| format!("cannot read {description} private /tmp readback: {error}"))?;
    require_private_tmp_readback(
        &observed,
        evidence.host_tmp_device,
        evidence.host_tmp_inode,
        evidence.proxy_uid,
    )
    .map_err(|error| format!("{description}: {error}: {}", readback.display()))?;
    require_safehermit_launched(
        &invocation.join("safehermit.report"),
        &evidence.unshare_sha256,
        description,
    )?;
    sha256(&path)
}

fn split_private_tmp_handoff(
    arguments: Vec<String>,
) -> Result<(Option<PrivateTmpStage>, Vec<String>), String> {
    if arguments.first().map(String::as_str) != Some(MATRIX_PRIVATE_TMP_HANDOFF) {
        return Ok((None, arguments));
    }
    let count = arguments
        .get(1)
        .filter(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|count| count.parse::<usize>().ok())
        .ok_or_else(|| "matrix private /tmp handoff lacks a decimal stage length".to_owned())?;
    let end = count
        .checked_add(2)
        .filter(|end| *end <= arguments.len())
        .ok_or_else(|| "matrix private /tmp handoff stage is truncated".to_owned())?;
    let stage = validate_private_tmp_stage(&arguments[2..end])?;
    Ok((Some(stage), arguments[end..].to_vec()))
}

/// Accept exactly the stage `command_in_private_tmp` emits: its fixed unshare
/// argv and script, a canonical host directory, and binds that land strictly
/// beneath it.
fn validate_private_tmp_stage(argv: &[String]) -> Result<PrivateTmpStage, String> {
    let fixed = [
        "unshare",
        "--user",
        "--map-root-user",
        "--mount",
        "sh",
        "-ceu",
        DBT_PRIVATE_TMP_SCRIPT,
        "hermit-dbt-private-tmp",
    ];
    if argv.len() < fixed.len() + 2 || argv[..fixed.len()] != fixed {
        return Err(
            "matrix private /tmp stage is not run_matrix's unshare private-/tmp stage".to_owned(),
        );
    }
    let canonical = |raw: &str, description: &str| -> Result<PathBuf, String> {
        let path = PathBuf::from(raw);
        let resolved = fs::canonicalize(&path).map_err(|error| {
            format!("matrix private /tmp {description} {raw} is unreadable: {error}")
        })?;
        if !path.is_absolute() || resolved != path {
            return Err(format!(
                "matrix private /tmp {description} {raw} is not an absolute canonical path"
            ));
        }
        Ok(path)
    };
    let host_tmp = canonical(&argv[fixed.len()], "host directory")?;
    if !host_tmp.is_dir() {
        return Err(format!(
            "matrix private /tmp host directory {} is not a directory",
            host_tmp.display()
        ));
    }
    let raw_count = &argv[fixed.len() + 1];
    let count = Some(raw_count)
        .filter(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|count| count.parse::<usize>().ok())
        .ok_or_else(|| format!("matrix private /tmp bind count {raw_count:?} is not decimal"))?;
    let pairs = &argv[fixed.len() + 2..];
    if count.checked_mul(2) != Some(pairs.len()) {
        return Err(format!(
            "matrix private /tmp stage declares {count} binds but carries {} paths",
            pairs.len()
        ));
    }
    let mut mounts = Vec::with_capacity(count);
    let mut destinations = BTreeSet::new();
    for pair in pairs.chunks(2) {
        let source = canonical(&pair[0], "bind source")?;
        let destination = canonical(&pair[1], "bind destination")?;
        if !destination.starts_with(&host_tmp) || destination == host_tmp {
            return Err(format!(
                "matrix private /tmp bind destination {} is not strictly beneath {}",
                destination.display(),
                host_tmp.display()
            ));
        }
        if !destinations.insert(destination.clone()) {
            return Err(format!(
                "matrix private /tmp stage binds {} twice",
                destination.display()
            ));
        }
        mounts.push((source, destination));
    }
    Ok(PrivateTmpStage {
        host_tmp,
        mounts,
        argv: argv.to_vec(),
    })
}

/// safehermit's disk bound needs host sudo, which an unmapped setuid binary
/// cannot provide; its systemd unit would also escape any outer namespace.
fn require_initial_user_namespace(uid_map: &str) -> Result<(), String> {
    let lines = uid_map
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    if lines != [vec!["0", "0", "4294967295"]] {
        return Err(format!(
            "matrix candidate proxy must launch safehermit from the initial user namespace, found uid_map {:?}",
            uid_map.trim()
        ));
    }
    Ok(())
}

/// The candidate must see the host directory as /tmp, start in it with TMPDIR
/// pointing at it, and run as root mapped from exactly the proxy's own uid.
fn require_private_tmp_readback(
    observed: &str,
    dev: u64,
    ino: u64,
    uid: u32,
) -> Result<(), String> {
    let expected = format!("tmp={dev}:{ino}\ncwd=/tmp\ntmpdir=/tmp\nuid_map=0 {uid} 1\n");
    if observed != expected {
        return Err(format!(
            "private /tmp readback {observed:?} does not match the host directory and mapping {expected:?}"
        ));
    }
    Ok(())
}

fn executable_on_path(name: &str) -> Result<PathBuf, String> {
    let path = env::var_os("PATH").ok_or_else(|| format!("PATH is unset; cannot find {name}"))?;
    let found = env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("{name} is not an executable on PATH"))?;
    let resolved = fs::canonicalize(&found)
        .map_err(|error| format!("cannot resolve {}: {error}", found.display()))?;
    require_absolute_file(&resolved, name)
}

/// The report must name the executable safehermit actually launched.
fn require_safehermit_launched(
    path: &Path,
    expected_sha256: &str,
    description: &str,
) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "safehermit report for {description} is unreadable at {}: {error}",
            path.display()
        )
    })?;
    let recorded = text
        .lines()
        .filter_map(|line| line.strip_prefix("safehermit: binary_sha256="))
        .collect::<Vec<_>>();
    if recorded != [expected_sha256] {
        return Err(format!(
            "safehermit report for {description} records launched binary {recorded:?}, expected {expected_sha256}: {}",
            path.display()
        ));
    }
    Ok(())
}

fn matrix_candidate_proxy(arguments: impl Iterator<Item = String>) -> Result<ExitCode, String> {
    matrix_candidate_proxy_from(Path::new("/proc/self/uid_map"), arguments)
}

fn matrix_candidate_proxy_from(
    uid_map: &Path,
    arguments: impl Iterator<Item = String>,
) -> Result<ExitCode, String> {
    // First, before any binding is read or evidence is written.
    let uid_map = fs::read_to_string(uid_map)
        .map_err(|error| format!("cannot read the proxy's user namespace map: {error}"))?;
    require_initial_user_namespace(&uid_map)?;
    let required = |name: &str| {
        env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| format!("matrix candidate proxy lacks required {name}"))
    };
    let safehermit = require_absolute_file(
        &required("HERMIT_BUCK_PHASE1_SAFEHERMIT")?,
        "matrix safehermit launcher",
    )?;
    let binary = require_absolute_file(
        &required("HERMIT_BUCK_PHASE1_CANDIDATE")?,
        "matrix candidate binary",
    )?;
    let install = require_absolute_directory(
        &required("HERMIT_BUCK_PHASE1_INSTALL")?,
        "matrix candidate install bundle",
    )?;
    let evidence = require_absolute_directory(
        &required("HERMIT_BUCK_PHASE1_MATRIX_EVIDENCE")?,
        "matrix evidence directory",
    )?;
    let label = env::var("HERMIT_BUCK_PHASE1_CANDIDATE_LABEL")
        .map_err(|_| "matrix candidate proxy lacks candidate label".to_owned())?;
    if !matches!(label.as_str(), "cargo" | "buck") {
        return Err(format!("invalid matrix candidate label {label:?}"));
    }
    let (private_tmp, arguments) = split_private_tmp_handoff(arguments.collect())?;
    let invocation_metadata = classify_matrix_proxy_invocation(&arguments)?;
    match (invocation_metadata.role.as_str(), &private_tmp) {
        ("dbt-run", None) => {
            return Err(format!(
                "DBT matrix case {} arrived without run_matrix's private /tmp stage",
                invocation_metadata.case_identity
            ));
        }
        (role, Some(_)) if role != "dbt-run" => {
            return Err(format!(
                "matrix {role} invocation {} must not carry a private /tmp stage",
                invocation_metadata.case_identity
            ));
        }
        _ => {}
    }
    let identity = unique_identity()?;
    let invocation = evidence.join(format!("{label}-{identity}"));
    create_exclusive_directory(&invocation, "matrix candidate invocation")?;
    let report = invocation.join("safehermit.report");
    let data = invocation.join("data");
    create_exclusive_directory(&data, "matrix candidate data directory")?;
    write_new_file(
        &invocation.join("invocation.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&invocation_metadata)
                .map_err(|error| format!("failed to encode matrix invocation metadata: {error}"))?
        )
        .as_bytes(),
        "matrix candidate invocation metadata",
    )?;
    let binary_sha256 = sha256(&binary)?;
    let mut command = Command::new(&safehermit);
    command
        .arg(format!("--sh-deadline={MATRIX_CANDIDATE_DEADLINE_SECONDS}"))
        .arg("--sh-report")
        .arg(&report);
    let launched_sha256 = if let Some(stage) = &private_tmp {
        // The stage hides host /tmp from the candidate; anything it needs
        // there must arrive through run_matrix's own binds.
        for (path, description) in [
            (&binary, "candidate"),
            (&install, "install bundle"),
            (&invocation, "invocation evidence"),
        ] {
            if path.starts_with("/tmp") {
                return Err(format!(
                    "matrix {description} {} lies beneath the /tmp the private stage replaces",
                    path.display()
                ));
            }
        }
        let unshare = executable_on_path(&stage.argv[0])?;
        let unshare_sha256 = sha256(&unshare)?;
        let host = fs::metadata(&stage.host_tmp)
            .map_err(|error| format!("cannot stat {}: {error}", stage.host_tmp.display()))?;
        let proxy_uid = fs::metadata("/proc/self")
            .map_err(|error| format!("cannot read the proxy's uid: {error}"))?
            .uid();
        write_new_file(
            &invocation.join("private-tmp.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&MatrixPrivateTmpEvidence {
                    schema: MATRIX_PRIVATE_TMP_SCHEMA.to_owned(),
                    host_tmp: stage.host_tmp.clone(),
                    host_tmp_device: host.dev(),
                    host_tmp_inode: host.ino(),
                    proxy_uid,
                    binds: stage.mounts.clone(),
                    unshare: unshare.clone(),
                    unshare_sha256: unshare_sha256.clone(),
                    candidate: binary.clone(),
                    candidate_sha256: binary_sha256.clone(),
                })
                .map_err(|error| format!("failed to encode private /tmp evidence: {error}"))?
            )
            .as_bytes(),
            "matrix private /tmp evidence",
        )?;
        command
            .arg(format!("--sh-bin={}", unshare.display()))
            .arg("--")
            .args(&stage.argv[1..])
            .args([
                "/bin/sh",
                "-ceu",
                PRIVATE_TMP_READBACK_SCRIPT,
                "hermit-private-tmp-readback",
            ])
            .arg(invocation.join("private-tmp.readback"))
            .arg(&binary);
        unshare_sha256
    } else {
        command.arg(&binary);
        binary_sha256.clone()
    };
    let output = command
        .args(&arguments)
        .env("HERMIT_INSTALL_DIR", &install)
        .env("HERMIT_DATA_DIR", &data)
        .output()
        .map_err(|error| format!("failed to start matrix candidate through safehermit: {error}"))?;
    write_new_file(
        &invocation.join("stdout"),
        &output.stdout,
        "matrix candidate stdout evidence",
    )?;
    write_new_file(
        &invocation.join("stderr"),
        &output.stderr,
        "matrix candidate stderr evidence",
    )?;
    let description = format!("{label} DBT matrix candidate");
    validate_and_surface_safehermit_report(&report, &description)?;
    require_safehermit_launched(&report, &launched_sha256, &description)?;
    if sha256(&binary)? != binary_sha256 {
        return Err(format!(
            "{description} {} changed bytes while it ran",
            binary.display()
        ));
    }
    if let Some(stage) = &private_tmp {
        let readback = invocation.join("private-tmp.readback");
        if !readback.is_file() {
            return Err(format!(
                "{description} left no private /tmp readback at {}",
                readback.display()
            ));
        }
        verify_retained_private_tmp(&invocation, &description, &binary_sha256)?;
        let recorded: MatrixPrivateTmpEvidence = decode_typed_json(
            &fs::read_to_string(invocation.join("private-tmp.json"))
                .map_err(|error| format!("cannot reread private /tmp evidence: {error}"))?,
            "matrix private /tmp evidence",
        )?;
        let host = fs::metadata(&stage.host_tmp)
            .map_err(|error| format!("cannot stat {}: {error}", stage.host_tmp.display()))?;
        if (host.dev(), host.ino()) != (recorded.host_tmp_device, recorded.host_tmp_inode) {
            return Err(format!(
                "{description}: private /tmp host directory {} was replaced while the candidate ran",
                stage.host_tmp.display()
            ));
        }
        eprintln!(
            "{description}: isolation.private_tmp=APPLIED:{} with {} binds, read back inside the safehermit unit",
            stage.host_tmp.display(),
            stage.mounts.len()
        );
    }
    std::io::stdout()
        .write_all(&output.stdout)
        .map_err(|error| format!("failed to replay matrix candidate stdout: {error}"))?;
    std::io::stderr()
        .write_all(&output.stderr)
        .map_err(|error| format!("failed to replay matrix candidate stderr: {error}"))?;
    Ok(ExitCode::from(
        output
            .status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(2),
    ))
}

fn shell_command(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| shell_words::quote(argument).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(clippy::too_many_arguments)]
fn require_clean_matrix_supervision(
    run_ok: bool,
    run_timed_out: bool,
    run_cpu_timed_out: bool,
    cpu_accounting_failed: bool,
    skipped: usize,
    not_launched: usize,
    outcomes: usize,
    outcome_ok: bool,
    outcome_timed_out: bool,
    outcome_cpu_timed_out: bool,
    outcome_aborted: bool,
    outcome_returncode: Option<i64>,
) -> Result<(), String> {
    if run_ok
        && !run_timed_out
        && !run_cpu_timed_out
        && !cpu_accounting_failed
        && skipped == 0
        && not_launched == 0
        && outcomes == 1
        && outcome_ok
        && !outcome_timed_out
        && !outcome_cpu_timed_out
        && !outcome_aborted
        && outcome_returncode == Some(0)
    {
        Ok(())
    } else {
        Err("matrix process tree did not complete and clean up under its cgroup supervisor".into())
    }
}

struct MatrixInvocation<'a> {
    root: &'a Path,
    cgroups: BoxedCgroups,
    safehermit: &'a Path,
    binary: &'a Path,
    install: &'a Path,
    evidence_dir: &'a Path,
    candidate_label: &'a str,
    output: &'a Path,
    description: &'a str,
    envelope: &'a MatrixExecutionEnvelope,
}

#[derive(Clone)]
struct MatrixExecutionEnvelope {
    cmdtype: CmdType,
    hint: ResourceHint,
    networkonly: bool,
    engine_only: bool,
    timeout: i64,
    cpu_timeout: i64,
    jobs_flag: Option<String>,
    jobs_env: Option<String>,
    /// The reviewed `run_matrix.py` flags. This driver launches every
    /// candidate on the host.
    matrix_flags: Vec<String>,
    /// The reviewed host environment, for the same reason.
    environment: BTreeMap<String, String>,
}

/// The pinned-root runner's per-run `/test` request needs CAP_SYS_ADMIN, which
/// a safehermit candidate never has: `systemd-run --user` starts it in the
/// initial user namespace, outside any namespace this driver could enter.
const PINNED_ROOT_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const REVIEWED_DBT_MATRIX_FLAGS: [&str; 5] = [
    "--backend",
    "dbt",
    "--strict",
    "--require-backend",
    "--no-parent-scorecard",
];

/// The reviewed execution envelope for the shadow DBT matrix.
///
/// Until slice S13 of https://github.com/rrnewton/hermit/issues/3301 this was
/// derived from the validation DAG's `test.dbt_parity` node and its
/// `test.dbt_parity_on_host` twin, and refused any drift between them. Those
/// nodes were retired: their 28 cases now run as DBT verify cells of the
/// c-programs and system-utils manifests, and `run_matrix.py` stays only for
/// this nightly Buck/Cargo comparison and the Makefile targets. The values are
/// the ones that host twin carried when it was retired (commit 12371d6c): 180 s
/// estimate, 512-MiB baseline and 2-GiB hard memory, latency-bound, 900 s wall
/// and 7200 s CPU, no jobs flag, and an empty host environment.
fn reviewed_dbt_matrix_envelope() -> MatrixExecutionEnvelope {
    MatrixExecutionEnvelope {
        cmdtype: CmdType::Unknown,
        hint: ResourceHint {
            est_duration_s: 180.0,
            rss_baseline_bytes: Some(512 * 1024 * 1024),
            hard_mem_max_bytes: Some(2_i64 * 1024 * 1024 * 1024),
            classification: StepClass::LatencyBound,
            ..ResourceHint::default()
        },
        networkonly: false,
        engine_only: false,
        timeout: MATRIX_STEP_DEADLINE_SECONDS,
        cpu_timeout: MATRIX_CPU_TIMEOUT_SECONDS,
        jobs_flag: None,
        jobs_env: None,
        matrix_flags: REVIEWED_DBT_MATRIX_FLAGS
            .iter()
            .map(|flag| (*flag).to_owned())
            .collect(),
        environment: BTreeMap::new(),
    }
}

/// The one scheduler step that runs a candidate's DBT matrix. Shared by the
/// driver and its deadline test so the test exercises the step that launches.
fn dbt_matrix_step(
    envelope: &MatrixExecutionEnvelope,
    candidate_label: &str,
    description: &str,
    cmd: String,
    environment: BTreeMap<String, String>,
) -> Step {
    Step {
        group: "buck_phase1".into(),
        job: format!("{candidate_label}_dbt_matrix"),
        desc: description.into(),
        description: "Official DBT matrix under the repository cgroup process-tree supervisor"
            .into(),
        labels: Vec::new(),
        cmd,
        cmdtype: envelope.cmdtype,
        manifest: None,
        integration_test_binaries: None,
        result_manifests: None,
        deps: Vec::new(),
        env: environment,
        hint: envelope.hint.clone(),
        networkonly: envelope.networkonly,
        engine_only: envelope.engine_only,
        delegated_children: false,
        timeout: envelope.timeout,
        cpu_timeout: envelope.cpu_timeout,
        jobs_flag: envelope.jobs_flag.clone(),
        jobs_env: envelope.jobs_env.clone(),
        skip_reason: None,
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    }
}

/// The reviewed host environment plus the proxy's own bindings; a collision
/// would let the envelope silently redirect the candidate under test.
fn matrix_step_environment(
    envelope: &MatrixExecutionEnvelope,
    safehermit: &Path,
    binary: &Path,
    install: &Path,
    matrix_invocations: &Path,
    candidate_label: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut proxy = BTreeMap::new();
    proxy.insert(MATRIX_PROXY_ENV.to_owned(), "1".to_owned());
    proxy.insert(
        "HERMIT_BUCK_PHASE1_SAFEHERMIT".to_owned(),
        safehermit.to_string_lossy().into_owned(),
    );
    proxy.insert(
        "HERMIT_BUCK_PHASE1_CANDIDATE".to_owned(),
        binary.to_string_lossy().into_owned(),
    );
    proxy.insert(
        "HERMIT_BUCK_PHASE1_INSTALL".to_owned(),
        install.to_string_lossy().into_owned(),
    );
    proxy.insert(
        "HERMIT_BUCK_PHASE1_MATRIX_EVIDENCE".to_owned(),
        matrix_invocations.to_string_lossy().into_owned(),
    );
    proxy.insert(
        "HERMIT_BUCK_PHASE1_CANDIDATE_LABEL".to_owned(),
        candidate_label.to_owned(),
    );
    proxy.insert(
        "HERMIT_INSTALL_DIR".to_owned(),
        install.to_string_lossy().into_owned(),
    );
    let mut environment = envelope.environment.clone();
    if environment.contains_key(PINNED_ROOT_WORKDIR_ENV) {
        return Err(format!(
            "host DBT matrix environment requests {PINNED_ROOT_WORKDIR_ENV}, which a safehermit candidate cannot satisfy"
        ));
    }
    for (key, value) in proxy {
        if environment.insert(key.clone(), value).is_some() {
            return Err(format!(
                "reviewed host DBT matrix environment collides with proxy binding {key}"
            ));
        }
    }
    Ok(environment)
}

fn run_dbt_matrix(request: MatrixInvocation<'_>) -> Result<String, String> {
    let MatrixInvocation {
        root,
        cgroups,
        safehermit,
        binary,
        install,
        evidence_dir,
        candidate_label,
        output,
        description,
        envelope,
    } = request;
    if !matches!(candidate_label, "cargo" | "buck") {
        return Err(format!(
            "invalid DBT matrix candidate label {candidate_label:?}"
        ));
    }
    let proxy = root.join("scripts/build-buck-release.rs");
    let arguments = [
        "python3".to_owned(),
        root.join("tests/backend-parity/run_matrix.py")
            .to_string_lossy()
            .into_owned(),
        "--hermit".to_owned(),
        proxy.to_string_lossy().into_owned(),
    ]
    .into_iter()
    .chain(envelope.matrix_flags.iter().cloned())
    .chain(["--output".to_owned(), output.to_string_lossy().into_owned()])
    .collect::<Vec<_>>();
    let matrix_invocations = evidence_dir.join("matrix-candidate-invocations");
    fs::create_dir_all(&matrix_invocations).map_err(|error| {
        format!(
            "failed to create matrix invocation evidence {}: {error}",
            matrix_invocations.display()
        )
    })?;
    let environment = matrix_step_environment(
        envelope,
        safehermit,
        binary,
        install,
        &matrix_invocations,
        candidate_label,
    )?;
    let step = dbt_matrix_step(
        envelope,
        candidate_label,
        description,
        shell_command(&arguments),
        environment,
    );
    let config = DagConfig {
        description: format!("{candidate_label} Buck phase-1 shadow DBT matrix"),
        steps: vec![step],
        ..Default::default()
    };
    let violating = steps_violating_run_timeout(&config, MATRIX_SUPERVISOR_DEADLINE_SECONDS);
    if !violating.is_empty() {
        return Err(format!(
            "DBT matrix inner/outer deadline ordering is invalid before launch: {violating:?}"
        ));
    }
    let result = run_dag_boxed_deadline(
        &config,
        1,
        false,
        2,
        cgroups,
        None,
        Some(1),
        Some(MATRIX_SUPERVISOR_DEADLINE_SECONDS),
    );
    let outcome = result.outcomes.first();
    let supervisor_record = format!(
        "supervisor\tdagrun-cgroup-v2\nstep_wall_seconds\t{}\nouter_wall_seconds\t{}\ncleanup_evidence_grace_seconds\t{}\nprocess_tree_cleanup\tcomplete\nok\t{}\nrun_timed_out\t{}\nrun_cpu_timed_out\t{}\noutcome_count\t{}\nskipped_count\t{}\nnot_launched_count\t{}\noutcome_ok\t{}\noutcome_timed_out\t{}\noutcome_cpu_timed_out\t{}\noutcome_aborted\t{}\noutcome_returncode\t{}\n",
        envelope.timeout,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS - envelope.timeout,
        result.ok,
        result.run_timed_out,
        result.run_cpu_timed_out,
        result.outcomes.len(),
        result.skipped.len(),
        result.not_launched.len(),
        outcome.is_some_and(|outcome| outcome.ok),
        outcome.is_some_and(|outcome| outcome.timed_out),
        outcome.is_some_and(|outcome| outcome.cpu_timed_out),
        outcome.is_some_and(|outcome| outcome.aborted),
        outcome
            .and_then(|outcome| outcome.returncode)
            .map_or_else(|| "none".to_owned(), |code| code.to_string()),
    );
    write_new_file(
        &evidence_dir.join(format!("{candidate_label}-matrix-supervisor.tsv")),
        supervisor_record.as_bytes(),
        "matrix supervisor evidence",
    )?;
    if require_clean_matrix_supervision(
        result.ok,
        result.run_timed_out,
        result.run_cpu_timed_out,
        result.run_cpu_accounting_failed,
        result.skipped.len(),
        result.not_launched.len(),
        result.outcomes.len(),
        outcome.is_some_and(|outcome| outcome.ok),
        outcome.is_some_and(|outcome| outcome.timed_out),
        outcome.is_some_and(|outcome| outcome.cpu_timed_out),
        outcome.is_some_and(|outcome| outcome.aborted),
        outcome.and_then(|outcome| outcome.returncode),
    )
    .is_err()
    {
        return Err(format!(
            "{description} did not complete cleanly under cgroup supervision; retained supervisor evidence records cancellation/cleanup disposition"
        ));
    }
    normalized_matrix(output)
}

fn atomic_write_new(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "refusing to replace existing final evidence {}",
            path.display()
        ));
    }
    let temporary = path.with_extension(format!("tmp.{}", unique_identity()?));
    write_new_file(&temporary, contents.as_bytes(), "temporary atomic evidence")?;
    let publish = fs::hard_link(&temporary, path).map_err(|error| {
        format!(
            "failed to publish new evidence {} without replacement: {error}",
            path.display()
        )
    });
    let cleanup = fs::remove_file(&temporary).map_err(|error| {
        format!(
            "failed to remove temporary evidence {}: {error}",
            temporary.display()
        )
    });
    match (publish, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup also failed: {cleanup}")),
    }
}

fn collect_evidence_hashes(
    root: &Path,
    directory: &Path,
    receipt: &Path,
    hashes: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to enumerate {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to enumerate {}: {error}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path == receipt {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "final evidence contains an unbound symlink {}; publish real files only",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect_evidence_hashes(root, &path, receipt, hashes)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| format!("evidence path escaped receipt root: {}", path.display()))?;
            hashes.push((relative.to_string_lossy().into_owned(), sha256(&path)?));
        } else {
            return Err(format!(
                "final evidence contains unsupported special file {}",
                path.display()
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct FinalReceiptFacts {
    result: String,
    evidence_identity: String,
    process_tree_supervisor: String,
    matrix_step_wall_seconds: i64,
    matrix_outer_wall_seconds: i64,
    matrix_cleanup_evidence_grace_seconds: i64,
    process_tree_cleanup: String,
    hermit_full_sha: String,
    hermit_build_sha: String,
    reverie_sha: String,
    version: String,
    build_date: String,
    dotslash_version: String,
    dotslash_sha256: String,
    buck_version: String,
    buck_descriptor_sha256: String,
    buck_executable_sha256: String,
    lzma_input: String,
    lzma_input_sha256: String,
    unwind_input_identity: String,
    unwind_ptrace_sha256: String,
    unwind_arch_sha256: String,
    unwind_core_sha256: String,
    generated_buck_sha256: String,
    safehermit_sha256: String,
    bounded_run_space_sha256: String,
    cargo_candidate_sha256: String,
    cargo_hermit_manifest_sha256: String,
    cargo_resources_manifest_sha256: String,
    buck_candidate_sha256: String,
    buck_hermit_manifest_sha256: String,
    buck_resources_manifest_sha256: String,
    event_log: String,
    ptrace_verify_messages_left: u64,
    ptrace_verify_messages_right: u64,
    ptrace_record_messages_left: u64,
    ptrace_record_messages_right: u64,
    cargo_matrix_rows: usize,
    buck_matrix_rows: usize,
    matrix_candidate_manifest_sha256: String,
    matrix_candidate_manifest: String,
    matrix_expected_ledger: String,
    matrix_expected_ledger_sha256: String,
    matrix_expected_invocation_count: usize,
    cargo_matrix_candidate_invocations: usize,
    buck_matrix_candidate_invocations: usize,
}

struct FinalReceiptContext<'a> {
    root: &'a Path,
    evidence_dir: &'a Path,
    cargo_build_info: &'a BuildInfo,
    cargo_bundle: &'a PublishedBundle,
    buck_bundle: &'a PublishedBundle,
    safehermit_bundle: &'a SafehermitBundle,
    dotslash_snapshot: &'a InputSnapshot,
    dotslash_version: &'a str,
    buck_descriptor_snapshot: &'a InputSnapshot,
    buck_executable_snapshot: &'a InputSnapshot,
    buck_version: &'a str,
    lzma_input: &'a Path,
    lzma_input_sha256: &'a str,
    lzma_buck_input: &'a LzmaBuckInput,
    unwind_buck_input: &'a UnwindBuckInput,
    generated_buck: &'a Path,
    generated_buck_sha256: &'a str,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct FinalReceiptDocument {
    receipt_schema: String,
    facts: FinalReceiptFacts,
    resource_sha256: BTreeMap<String, String>,
    artifact_sha256: BTreeMap<String, String>,
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_receipt_facts(facts: &FinalReceiptFacts) -> Result<(), String> {
    let hashes = [
        &facts.dotslash_sha256,
        &facts.buck_descriptor_sha256,
        &facts.buck_executable_sha256,
        &facts.lzma_input_sha256,
        &facts.unwind_input_identity,
        &facts.unwind_ptrace_sha256,
        &facts.unwind_arch_sha256,
        &facts.unwind_core_sha256,
        &facts.generated_buck_sha256,
        &facts.safehermit_sha256,
        &facts.bounded_run_space_sha256,
        &facts.cargo_candidate_sha256,
        &facts.cargo_hermit_manifest_sha256,
        &facts.cargo_resources_manifest_sha256,
        &facts.buck_candidate_sha256,
        &facts.buck_hermit_manifest_sha256,
        &facts.buck_resources_manifest_sha256,
        &facts.matrix_candidate_manifest_sha256,
        &facts.matrix_expected_ledger_sha256,
    ];
    if facts.result != "pass"
        || facts.evidence_identity.is_empty()
        || facts.process_tree_supervisor != "dagrun-cgroup-v2"
        || facts.process_tree_cleanup != "complete"
        || facts.matrix_step_wall_seconds != MATRIX_STEP_DEADLINE_SECONDS
        || facts.matrix_outer_wall_seconds != MATRIX_SUPERVISOR_DEADLINE_SECONDS
        || facts.matrix_cleanup_evidence_grace_seconds
            != MATRIX_SUPERVISOR_DEADLINE_SECONDS - MATRIX_STEP_DEADLINE_SECONDS
        || facts.hermit_full_sha.len() != 40
        || !facts
            .hermit_full_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || facts.hermit_build_sha.len() != 12
        || !facts
            .hermit_build_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !facts.hermit_full_sha.starts_with(&facts.hermit_build_sha)
        || facts.reverie_sha.len() != 40
        || !facts
            .reverie_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || facts.version.is_empty()
        || facts.version == "unknown"
        || !canonical_build_date(&facts.build_date)
        || hashes.into_iter().any(|hash| !valid_sha256(hash))
        || facts.event_log != "buck-build.json-lines.gz"
        || facts.ptrace_verify_messages_left == 0
        || facts.ptrace_verify_messages_left != facts.ptrace_verify_messages_right
        || facts.ptrace_record_messages_left == 0
        || facts.ptrace_record_messages_left != facts.ptrace_record_messages_right
        || facts.cargo_matrix_rows == 0
        || facts.cargo_matrix_rows != facts.buck_matrix_rows
        || facts.cargo_matrix_candidate_invocations == 0
        || facts.buck_matrix_candidate_invocations == 0
        || facts.cargo_matrix_candidate_invocations != facts.buck_matrix_candidate_invocations
        || facts.matrix_candidate_manifest != "matrix-candidate-invocations.json"
        || facts.matrix_expected_ledger != "matrix-expected-ledger.json"
        || facts.matrix_expected_invocation_count == 0
        || facts.cargo_matrix_candidate_invocations != facts.matrix_expected_invocation_count
    {
        return Err("final receipt contains invalid semantic singleton values".to_owned());
    }
    Ok(())
}

fn expected_clean_matrix_supervisor_record() -> String {
    format!(
        "supervisor\tdagrun-cgroup-v2\nstep_wall_seconds\t{}\nouter_wall_seconds\t{}\ncleanup_evidence_grace_seconds\t{}\nprocess_tree_cleanup\tcomplete\nok\ttrue\nrun_timed_out\tfalse\nrun_cpu_timed_out\tfalse\noutcome_count\t1\nskipped_count\t0\nnot_launched_count\t0\noutcome_ok\ttrue\noutcome_timed_out\tfalse\noutcome_cpu_timed_out\tfalse\noutcome_aborted\tfalse\noutcome_returncode\t0\n",
        MATRIX_STEP_DEADLINE_SECONDS,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS - MATRIX_STEP_DEADLINE_SECONDS,
    )
}

fn verify_matrix_supervisor_evidence(evidence_dir: &Path) -> Result<(), String> {
    let expected = expected_clean_matrix_supervisor_record();
    for label in ["cargo", "buck"] {
        let path = evidence_dir.join(format!("{label}-matrix-supervisor.tsv"));
        let actual = fs::read_to_string(&path).map_err(|error| {
            format!(
                "{label} matrix supervisor evidence {} is unreadable: {error}",
                path.display()
            )
        })?;
        if actual != expected {
            return Err(format!(
                "{label} matrix supervisor evidence does not prove the reviewed clean deadline/cleanup contract"
            ));
        }
    }
    Ok(())
}

const EXPECTED_CANDIDATE_INVOCATIONS: [&str; 10] = [
    "cargo-version",
    "buck-version",
    "cargo-help",
    "buck-help",
    "cargo-host-capabilities",
    "buck-host-capabilities",
    "cargo-ptrace-verify",
    "buck-ptrace-verify",
    "cargo-ptrace-record",
    "buck-ptrace-record",
];

fn verify_candidate_invocation_reports(evidence_dir: &Path) -> Result<(), String> {
    let root = require_absolute_directory(
        &evidence_dir.join("candidate-invocations"),
        "candidate invocation evidence directory",
    )?;
    let mut matched = BTreeMap::<String, PathBuf>::new();
    for entry in fs::read_dir(&root)
        .map_err(|error| format!("cannot enumerate candidate invocation evidence: {error}"))?
    {
        let path = entry
            .map_err(|error| format!("cannot read candidate invocation entry: {error}"))?
            .path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot inspect candidate invocation {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(format!(
                "candidate invocation evidence contains non-directory {}",
                path.display()
            ));
        }
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| "candidate invocation identity is not UTF-8".to_owned())?;
        let identity = EXPECTED_CANDIDATE_INVOCATIONS
            .iter()
            .find(|identity| name.starts_with(&format!("{identity}-")))
            .ok_or_else(|| format!("unexpected candidate invocation identity {name:?}"))?;
        if matched
            .insert((*identity).to_owned(), path.clone())
            .is_some()
        {
            return Err(format!(
                "candidate invocation identity {identity:?} occurs more than once"
            ));
        }
        let mut files = fs::read_dir(&path)
            .map_err(|error| format!("cannot enumerate candidate invocation {name}: {error}"))?
            .map(|entry| {
                entry
                    .map_err(|error| error.to_string())?
                    .file_name()
                    .into_string()
                    .map_err(|_| "candidate invocation filename is not UTF-8".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        files.sort();
        if files != ["safehermit.report", "stderr", "stdout"] {
            return Err(format!(
                "candidate invocation {name:?} has unexpected file population {files:?}"
            ));
        }
        validate_and_surface_safehermit_report(
            &path.join("safehermit.report"),
            &format!("retained candidate invocation {identity}"),
        )?;
    }
    let actual = matched.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = EXPECTED_CANDIDATE_INVOCATIONS
        .into_iter()
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!(
            "candidate invocation identities differ: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MatrixCandidateInvocationBinding {
    candidate_label: String,
    identity: String,
    case_identity: String,
    role: String,
    normalized_argv: Vec<String>,
    invocation_metadata_sha256: String,
    safehermit_report_sha256: String,
    stdout_sha256: String,
    stderr_sha256: String,
    /// Present exactly for `dbt-run`: the retained private /tmp record and its
    /// in-unit readback.
    private_tmp_sha256: Option<String>,
    private_tmp_readback_sha256: Option<String>,
    data_sha256: BTreeMap<String, String>,
    data_directories: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MatrixCandidateInvocationManifest {
    manifest_schema: String,
    invocations: Vec<MatrixCandidateInvocationBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MatrixCandidateInvocationFacts {
    manifest_sha256: String,
    cargo_count: usize,
    buck_count: usize,
    expected_ledger_sha256: String,
    expected_invocation_count: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct OfficialMatrixCase {
    test_name: String,
    expectation: String,
    selected: bool,
    requires_ptrace_reference: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
struct ExpectedMatrixInvocation {
    case_identity: String,
    role: String,
    count: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct OfficialMatrixExpectedLedger {
    ledger_schema: String,
    run_matrix_sha256: String,
    cases: Vec<OfficialMatrixCase>,
    expected_invocations: Vec<ExpectedMatrixInvocation>,
}

fn derive_official_matrix_expected_ledger(
    repository_root: &Path,
) -> Result<OfficialMatrixExpectedLedger, String> {
    let source = repository_root.join("tests/backend-parity/run_matrix.py");
    require_nonempty_regular_file(&source, "official DBT matrix runner")?;
    let program = r#"
import json, runpy, sys
m = runpy.run_path(sys.argv[1])
fixtures = m["CatalogFixtures"]()
rows = []
for name in m["validate_catalog"]():
    _guest, _status, expected_stdout = m["case_command"](name, fixtures)
    expectation, _reason = m["expectation"]("dbt", name, False)
    selected = expectation != "gap"
    rows.append({
        "test_name": name,
        "expectation": expectation,
        "selected": selected,
        "requires_ptrace_reference": selected and m["exact_stdout_parity_contract"]("dbt", name, expected_stdout),
    })
print(json.dumps(rows, separators=(",", ":")))
"#;
    let output = output_text(
        Command::new("python3").args(["-c", program]).arg(&source),
        "derive expected DBT invocation ledger from official matrix implementation",
    )?;
    let cases: Vec<OfficialMatrixCase> =
        decode_typed_json(&output, "official DBT matrix selected-case ledger")?;
    if cases.is_empty()
        || cases.iter().any(|case| {
            case.test_name.is_empty()
                || !matches!(case.expectation.as_str(), "pass" | "gap")
                || case.selected != (case.expectation != "gap")
                || (!case.selected && case.requires_ptrace_reference)
        })
        || cases
            .iter()
            .map(|case| case.test_name.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != cases.len()
    {
        return Err("official DBT matrix produced an invalid selected-case ledger".to_owned());
    }
    let mut expected = BTreeMap::<(String, String), usize>::new();
    expected.insert(
        ("probe/host-capabilities".to_owned(), "probe".to_owned()),
        1,
    );
    expected.insert(("probe/dbt-smoke".to_owned(), "probe".to_owned()), 1);
    for case in cases.iter().filter(|case| case.selected) {
        expected.insert((case.test_name.clone(), "dbt-run".to_owned()), 3);
        if case.requires_ptrace_reference {
            expected.insert((case.test_name.clone(), "ptrace-reference".to_owned()), 1);
        }
    }
    Ok(OfficialMatrixExpectedLedger {
        ledger_schema: "hermit-official-dbt-expected-ledger/v1".to_owned(),
        run_matrix_sha256: sha256(&source)?,
        cases,
        expected_invocations: expected
            .into_iter()
            .map(|((case_identity, role), count)| ExpectedMatrixInvocation {
                case_identity,
                role,
                count,
            })
            .collect(),
    })
}

fn render_official_matrix_expected_ledger(
    ledger: &OfficialMatrixExpectedLedger,
) -> Result<String, String> {
    if ledger.ledger_schema != "hermit-official-dbt-expected-ledger/v1"
        || !valid_sha256(&ledger.run_matrix_sha256)
        || ledger.cases.is_empty()
        || ledger.expected_invocations.is_empty()
        || ledger.expected_invocations.iter().any(|entry| {
            entry.case_identity.is_empty()
                || !matches!(
                    entry.role.as_str(),
                    "probe" | "ptrace-reference" | "dbt-run"
                )
                || entry.count == 0
        })
    {
        return Err("official DBT expected ledger has invalid semantics".to_owned());
    }
    serde_json::to_string_pretty(ledger)
        .map(|text| format!("{text}\n"))
        .map_err(|error| format!("failed to encode official DBT expected ledger: {error}"))
}

fn publish_official_matrix_expected_ledger(
    repository_root: &Path,
    evidence_dir: &Path,
) -> Result<PathBuf, String> {
    let ledger = derive_official_matrix_expected_ledger(repository_root)?;
    let text = render_official_matrix_expected_ledger(&ledger)?;
    let path = evidence_dir.join("matrix-expected-ledger.json");
    atomic_write_new(&path, &text)?;
    Ok(path)
}

fn verify_official_matrix_tsv(
    path: &Path,
    ledger: &OfficialMatrixExpectedLedger,
    candidate_label: &str,
) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {candidate_label} official matrix TSV: {error}"))?;
    let mut lines = text.lines();
    if lines.next() != Some("test_name\tbackend\texpectation\tresult\tseconds\tdetail") {
        return Err(format!(
            "{candidate_label} official matrix TSV has an unexpected header"
        ));
    }
    let mut observed = BTreeMap::new();
    for line in lines {
        let fields = line.splitn(6, '\t').collect::<Vec<_>>();
        if fields.len() != 6
            || fields[1] != "dbt"
            || !matches!(fields[2], "pass" | "gap")
            || observed
                .insert(
                    fields[0].to_owned(),
                    (fields[2].to_owned(), fields[3].to_owned()),
                )
                .is_some()
        {
            return Err(format!(
                "{candidate_label} official matrix TSV contains a malformed or duplicate row"
            ));
        }
    }
    let expected = ledger
        .cases
        .iter()
        .map(|case| {
            (
                case.test_name.clone(),
                (
                    case.expectation.clone(),
                    if case.selected { "PASS" } else { "GAP" }.to_owned(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if observed.len() != expected.len()
        || observed.iter().any(|(name, (expectation, result))| {
            expected
                .get(name)
                .is_none_or(|(expected_expectation, expected_result)| {
                    expectation != expected_expectation || result != expected_result
                })
        })
    {
        return Err(format!(
            "{candidate_label} official matrix TSV case IDs/selections differ from the independent expected ledger"
        ));
    }
    Ok(())
}

fn load_and_verify_official_matrix_expected_ledger(
    repository_root: &Path,
    evidence_dir: &Path,
) -> Result<(OfficialMatrixExpectedLedger, String), String> {
    let path = require_nonempty_regular_file(
        &evidence_dir.join("matrix-expected-ledger.json"),
        "official DBT expected ledger",
    )?;
    let before = stable_file_identity(&path)?;
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read official DBT expected ledger: {error}"))?;
    let after = stable_file_identity(&path)?;
    if before != after {
        return Err("official DBT expected ledger changed while verified".to_owned());
    }
    let retained: OfficialMatrixExpectedLedger =
        decode_typed_json(&text, "official DBT expected ledger")?;
    let derived = derive_official_matrix_expected_ledger(repository_root)?;
    if text != render_official_matrix_expected_ledger(&retained)? || retained != derived {
        return Err(
            "retained official DBT expected ledger differs from the current matrix implementation"
                .to_owned(),
        );
    }
    for (label, path) in [
        ("cargo", evidence_dir.join("cargo-dbt-matrix.tsv")),
        ("buck", evidence_dir.join("buck-dbt-matrix.tsv")),
    ] {
        verify_official_matrix_tsv(&path, &retained, label)?;
    }
    Ok((retained, before.sha256))
}

fn collect_matrix_data_directories(
    root: &Path,
    directory: &Path,
    directories: &mut Vec<String>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot enumerate matrix candidate data: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read matrix candidate data entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot inspect matrix candidate data {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "matrix candidate data contains symlink {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            directories.push(
                path.strip_prefix(root)
                    .map_err(|_| "matrix candidate data directory escaped root".to_owned())?
                    .to_string_lossy()
                    .into_owned(),
            );
            collect_matrix_data_directories(root, &path, directories)?;
        } else if !metadata.is_file() {
            return Err(format!(
                "matrix candidate data contains special file {}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// v2 binds each `dbt-run`'s private /tmp record and readback.
const MATRIX_CANDIDATE_INVOCATIONS_SCHEMA: &str = "hermit-matrix-candidate-invocations/v2";
const MATRIX_INVOCATION_FILES: [&str; 5] = [
    "data",
    "invocation.json",
    "safehermit.report",
    "stderr",
    "stdout",
];
const MATRIX_PRIVATE_TMP_INVOCATION_FILES: [&str; 7] = [
    "data",
    "invocation.json",
    "private-tmp.json",
    "private-tmp.readback",
    "safehermit.report",
    "stderr",
    "stdout",
];

fn inspect_matrix_candidate_invocations(
    evidence_dir: &Path,
    cargo_candidate_sha256: &str,
    buck_candidate_sha256: &str,
) -> Result<MatrixCandidateInvocationManifest, String> {
    let root = require_absolute_directory(
        &evidence_dir.join("matrix-candidate-invocations"),
        "matrix candidate invocation evidence directory",
    )?;
    let mut entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot enumerate matrix candidate invocations: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read matrix candidate invocation entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut identities = BTreeSet::new();
    let mut invocations = Vec::new();
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot inspect matrix candidate invocation {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(format!(
                "matrix candidate invocation evidence contains non-directory {}",
                path.display()
            ));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "matrix candidate invocation identity is not UTF-8".to_owned())?;
        let (candidate_label, candidate_sha256, identity) =
            if let Some(identity) = name.strip_prefix("cargo-") {
                ("cargo", cargo_candidate_sha256, identity)
            } else if let Some(identity) = name.strip_prefix("buck-") {
                ("buck", buck_candidate_sha256, identity)
            } else {
                return Err(format!(
                    "unexpected matrix candidate invocation identity {name:?}"
                ));
            };
        if identity.is_empty()
            || !identity
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            || !identities.insert(name.clone())
        {
            return Err(format!(
                "invalid or duplicate matrix candidate invocation identity {name:?}"
            ));
        }
        let mut files = fs::read_dir(&path)
            .map_err(|error| {
                format!("cannot enumerate matrix candidate invocation {name}: {error}")
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot read matrix candidate invocation file: {error}"))?;
        files.sort_by_key(|entry| entry.file_name());
        let actual_names = files
            .iter()
            .map(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "matrix candidate invocation filename is not UTF-8".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if actual_names != MATRIX_INVOCATION_FILES
            && actual_names != MATRIX_PRIVATE_TMP_INVOCATION_FILES
        {
            return Err(format!(
                "matrix candidate invocation {name:?} has unexpected file population {actual_names:?}"
            ));
        }
        for file in files.iter().filter(|file| file.file_name() != "data") {
            let file_metadata = fs::symlink_metadata(file.path()).map_err(|error| {
                format!(
                    "cannot inspect matrix candidate invocation file {}: {error}",
                    file.path().display()
                )
            })?;
            if !file_metadata.is_file() || file_metadata.file_type().is_symlink() {
                return Err(format!(
                    "matrix candidate invocation contains link or special file {}",
                    file.path().display()
                ));
            }
        }
        let report = path.join("safehermit.report");
        let invocation_metadata_path = path.join("invocation.json");
        let invocation_metadata_text = fs::read_to_string(&invocation_metadata_path)
            .map_err(|error| format!("cannot read matrix invocation metadata {name:?}: {error}"))?;
        let invocation_metadata: MatrixProxyInvocation = decode_typed_json(
            &invocation_metadata_text,
            "matrix candidate invocation metadata",
        )?;
        if invocation_metadata.invocation_schema != "hermit-matrix-proxy-invocation/v1"
            || invocation_metadata.case_identity.is_empty()
            || !matches!(
                invocation_metadata.role.as_str(),
                "probe" | "ptrace-reference" | "dbt-run"
            )
            || invocation_metadata.normalized_argv.is_empty()
        {
            return Err(format!(
                "matrix candidate invocation {name:?} has invalid typed metadata"
            ));
        }
        let expected_names: &[&str] = if invocation_metadata.role == "dbt-run" {
            &MATRIX_PRIVATE_TMP_INVOCATION_FILES
        } else {
            &MATRIX_INVOCATION_FILES
        };
        if actual_names != expected_names {
            return Err(format!(
                "matrix candidate invocation {name:?} with role {} must hold exactly {expected_names:?}, found {actual_names:?}",
                invocation_metadata.role
            ));
        }
        let data = require_absolute_directory(
            &path.join("data"),
            "matrix candidate invocation data directory",
        )?;
        let mut data_rows = Vec::new();
        let mut data_directories = Vec::new();
        collect_matrix_data_directories(&data, &data, &mut data_directories)?;
        collect_evidence_hashes(
            &data,
            &data,
            &data.join(".no-receipt-exclusion"),
            &mut data_rows,
        )?;
        data_rows.sort();
        let description = format!("retained {candidate_label} DBT matrix invocation {identity}");
        validate_and_surface_safehermit_report(&report, &description)?;
        let (private_tmp_sha256, private_tmp_readback_sha256) =
            if invocation_metadata.role == "dbt-run" {
                (
                    Some(verify_retained_private_tmp(
                        &path,
                        &description,
                        candidate_sha256,
                    )?),
                    Some(sha256(&path.join("private-tmp.readback"))?),
                )
            } else {
                require_safehermit_launched(&report, candidate_sha256, &description)?;
                (None, None)
            };
        invocations.push(MatrixCandidateInvocationBinding {
            candidate_label: candidate_label.to_owned(),
            identity: identity.to_owned(),
            case_identity: invocation_metadata.case_identity,
            role: invocation_metadata.role,
            normalized_argv: invocation_metadata.normalized_argv,
            invocation_metadata_sha256: sha256(&invocation_metadata_path)?,
            safehermit_report_sha256: sha256(&report)?,
            stdout_sha256: sha256(&path.join("stdout"))?,
            stderr_sha256: sha256(&path.join("stderr"))?,
            private_tmp_sha256,
            private_tmp_readback_sha256,
            data_sha256: data_rows.into_iter().collect(),
            data_directories,
        });
    }
    let cargo_count = invocations
        .iter()
        .filter(|invocation| invocation.candidate_label == "cargo")
        .count();
    let buck_count = invocations
        .iter()
        .filter(|invocation| invocation.candidate_label == "buck")
        .count();
    if cargo_count == 0 || buck_count == 0 || cargo_count != buck_count {
        return Err(format!(
            "matrix candidate evidence must contain equal nonempty Cargo and Buck populations; got cargo={cargo_count}, buck={buck_count}"
        ));
    }
    Ok(MatrixCandidateInvocationManifest {
        manifest_schema: MATRIX_CANDIDATE_INVOCATIONS_SCHEMA.to_owned(),
        invocations,
    })
}

fn render_matrix_candidate_invocation_manifest(
    manifest: &MatrixCandidateInvocationManifest,
) -> Result<String, String> {
    if manifest.manifest_schema != MATRIX_CANDIDATE_INVOCATIONS_SCHEMA
        || manifest.invocations.is_empty()
        || manifest.invocations.iter().any(|invocation| {
            !matches!(invocation.candidate_label.as_str(), "cargo" | "buck")
                || invocation.identity.is_empty()
                || invocation.case_identity.is_empty()
                || !matches!(
                    invocation.role.as_str(),
                    "probe" | "ptrace-reference" | "dbt-run"
                )
                || invocation.normalized_argv.is_empty()
                || !valid_sha256(&invocation.invocation_metadata_sha256)
                || !valid_sha256(&invocation.safehermit_report_sha256)
                || !valid_sha256(&invocation.stdout_sha256)
                || !valid_sha256(&invocation.stderr_sha256)
                || [
                    &invocation.private_tmp_sha256,
                    &invocation.private_tmp_readback_sha256,
                ]
                .iter()
                .any(|hash| match hash {
                    Some(hash) => invocation.role != "dbt-run" || !valid_sha256(hash),
                    None => invocation.role == "dbt-run",
                })
                || invocation.data_sha256.iter().any(|(relative, hash)| {
                    let path = Path::new(relative);
                    !valid_sha256(hash)
                        || path.is_absolute()
                        || path
                            .components()
                            .any(|component| !matches!(component, std::path::Component::Normal(_)))
                })
                || invocation.data_directories.iter().any(|relative| {
                    let path = Path::new(relative);
                    path.as_os_str().is_empty()
                        || path.is_absolute()
                        || path
                            .components()
                            .any(|component| !matches!(component, std::path::Component::Normal(_)))
                })
        })
    {
        return Err("matrix candidate invocation manifest has invalid semantics".to_owned());
    }
    serde_json::to_string_pretty(manifest)
        .map(|text| format!("{text}\n"))
        .map_err(|error| format!("failed to encode matrix candidate invocation manifest: {error}"))
}

fn validate_matrix_candidate_completeness(
    repository_root: &Path,
    evidence_dir: &Path,
    manifest: &MatrixCandidateInvocationManifest,
) -> Result<(String, usize), String> {
    let (ledger, ledger_sha256) =
        load_and_verify_official_matrix_expected_ledger(repository_root, evidence_dir)?;
    let expected = ledger
        .expected_invocations
        .iter()
        .map(|entry| {
            (
                (entry.case_identity.clone(), entry.role.clone()),
                entry.count,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let expected_count = expected.values().sum::<usize>();
    let mut per_candidate = BTreeMap::<String, BTreeMap<(String, String), usize>>::new();
    let mut argv_per_candidate =
        BTreeMap::<String, BTreeMap<(String, String, Vec<String>), usize>>::new();
    for invocation in &manifest.invocations {
        *per_candidate
            .entry(invocation.candidate_label.clone())
            .or_default()
            .entry((invocation.case_identity.clone(), invocation.role.clone()))
            .or_default() += 1;
        *argv_per_candidate
            .entry(invocation.candidate_label.clone())
            .or_default()
            .entry((
                invocation.case_identity.clone(),
                invocation.role.clone(),
                invocation.normalized_argv.clone(),
            ))
            .or_default() += 1;
    }
    for label in ["cargo", "buck"] {
        if per_candidate.get(label) != Some(&expected) {
            return Err(format!(
                "{label} matrix proxy case/role multiset differs from the independently derived official ledger"
            ));
        }
    }
    if argv_per_candidate.get("cargo") != argv_per_candidate.get("buck") {
        return Err("Cargo/Buck matrix proxy normalized argv/case multisets differ".to_owned());
    }
    Ok((ledger_sha256, expected_count))
}

fn publish_matrix_candidate_invocation_manifest(
    repository_root: &Path,
    evidence_dir: &Path,
    cargo_candidate_sha256: &str,
    buck_candidate_sha256: &str,
) -> Result<PathBuf, String> {
    let path = evidence_dir.join("matrix-candidate-invocations.json");
    let manifest = inspect_matrix_candidate_invocations(
        evidence_dir,
        cargo_candidate_sha256,
        buck_candidate_sha256,
    )?;
    validate_matrix_candidate_completeness(repository_root, evidence_dir, &manifest)?;
    let text = render_matrix_candidate_invocation_manifest(&manifest)?;
    atomic_write_new(&path, &text)?;
    Ok(path)
}

fn verify_retained_matrix_candidate_invocation_manifest(
    evidence_dir: &Path,
    cargo_candidate_sha256: &str,
    buck_candidate_sha256: &str,
) -> Result<(MatrixCandidateInvocationManifest, String), String> {
    let path = require_nonempty_regular_file(
        &evidence_dir.join("matrix-candidate-invocations.json"),
        "matrix candidate invocation manifest",
    )?;
    let before = stable_file_identity(&path)?;
    let retained_text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "matrix candidate invocation manifest {} is unreadable: {error}",
            path.display()
        )
    })?;
    let after = stable_file_identity(&path)?;
    if before != after {
        return Err("matrix candidate invocation manifest changed while verified".to_owned());
    }
    let retained: MatrixCandidateInvocationManifest =
        decode_typed_json(&retained_text, "matrix candidate invocation manifest")?;
    let canonical_retained = render_matrix_candidate_invocation_manifest(&retained)?;
    let recomputed = inspect_matrix_candidate_invocations(
        evidence_dir,
        cargo_candidate_sha256,
        buck_candidate_sha256,
    )?;
    let canonical_recomputed = render_matrix_candidate_invocation_manifest(&recomputed)?;
    if retained_text != canonical_retained
        || canonical_retained != canonical_recomputed
        || retained != recomputed
    {
        return Err(
            "matrix candidate invocation manifest differs from the exact retained population"
                .to_owned(),
        );
    }
    Ok((retained, before.sha256))
}

fn verify_matrix_candidate_invocation_manifest(
    repository_root: &Path,
    evidence_dir: &Path,
    cargo_candidate_sha256: &str,
    buck_candidate_sha256: &str,
) -> Result<MatrixCandidateInvocationFacts, String> {
    let (retained, manifest_sha256) = verify_retained_matrix_candidate_invocation_manifest(
        evidence_dir,
        cargo_candidate_sha256,
        buck_candidate_sha256,
    )?;
    let (expected_ledger_sha256, expected_invocation_count) =
        validate_matrix_candidate_completeness(repository_root, evidence_dir, &retained)?;
    Ok(MatrixCandidateInvocationFacts {
        manifest_sha256,
        cargo_count: retained
            .invocations
            .iter()
            .filter(|invocation| invocation.candidate_label == "cargo")
            .count(),
        buck_count: retained
            .invocations
            .iter()
            .filter(|invocation| invocation.candidate_label == "buck")
            .count(),
        expected_ledger_sha256,
        expected_invocation_count,
    })
}

fn verify_retained_buck_log_summary(
    buck_executable: &Path,
    repository_root: &Path,
    event_log: &Path,
    retained_summary: &Path,
) -> Result<(), String> {
    let retained = fs::read_to_string(retained_summary).map_err(|error| {
        format!(
            "retained Buck log summary {} is unreadable: {error}",
            retained_summary.display()
        )
    })?;
    let recomputed = output_text(
        Command::new(buck_executable)
            .current_dir(repository_root)
            .args(["log", "summary"])
            .arg(event_log),
        "recompute Buck event-log summary from snapshotted Buck",
    )?;
    if retained != format!("{recomputed}\n") {
        return Err("retained Buck event-log summary differs from recomputed summary".to_owned());
    }
    Ok(())
}

struct RetainedCandidateEvidence {
    run_report: VerificationReport,
    record_report: VerificationReport,
}

fn read_retained_text(evidence_dir: &Path, name: &str) -> Result<String, String> {
    let path = evidence_dir.join(name);
    fs::read_to_string(&path).map_err(|error| {
        format!(
            "retained candidate evidence {} is unreadable: {error}",
            path.display()
        )
    })
}

fn retain_candidate_text(
    evidence_dir: &Path,
    name: &str,
    contents: &str,
) -> Result<PathBuf, String> {
    let path = evidence_dir.join(name);
    atomic_write_new(&path, &format!("{contents}\n"))?;
    Ok(path)
}

fn verify_retained_candidate_evidence(
    evidence_dir: &Path,
    expected_version: &str,
    expected_sha: &str,
    expected_cargo_info: &BuildInfo,
) -> Result<RetainedCandidateEvidence, String> {
    let cargo_version = read_retained_text(evidence_dir, "cargo-version.json")?;
    let buck_version = read_retained_text(evidence_dir, "buck-version.json")?;
    let cargo_info = decode_build_info(
        &cargo_version,
        expected_version,
        expected_sha,
        "retained Cargo version JSON",
    )?;
    let buck_info = decode_build_info(
        &buck_version,
        expected_version,
        expected_sha,
        "retained Buck version JSON",
    )?;
    if cargo_info != *expected_cargo_info || cargo_info != buck_info {
        return Err(
            "retained Cargo/Buck typed version/provenance/features reports differ".to_owned(),
        );
    }

    let cargo_help = read_retained_text(evidence_dir, "cargo-help.txt")?;
    let buck_help = read_retained_text(evidence_dir, "buck-help.txt")?;
    require_equal("retained exact help text", &cargo_help, &buck_help)?;
    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        if !buck_help.contains(backend) {
            return Err(format!(
                "retained Buck CLI capability output omits backend spelling {backend}"
            ));
        }
    }

    let cargo_host = read_retained_text(evidence_dir, "cargo-host-capabilities.json")?;
    let buck_host = read_retained_text(evidence_dir, "buck-host-capabilities.json")?;
    let cargo_host_report =
        decode_host_capabilities(&cargo_host, "retained Cargo host-capabilities JSON")?;
    let buck_host_report =
        decode_host_capabilities(&buck_host, "retained Buck host-capabilities JSON")?;
    if cargo_host_report != buck_host_report {
        return Err(
            "retained Cargo/Buck typed host-capabilities reports differ in exact semantics"
                .to_owned(),
        );
    }

    let cargo_run = verify_clock_parity(
        read_retained_text(evidence_dir, "cargo-ptrace-clock-trajectory.txt")?.as_bytes(),
        read_retained_text(evidence_dir, "buck-ptrace-clock-trajectory.txt")?.as_bytes(),
        &evidence_dir.join("cargo-ptrace-verify.json"),
        &evidence_dir.join("buck-ptrace-verify.json"),
    )?;
    let cargo_record = verify_report(&evidence_dir.join("cargo-ptrace-record-verify.json"), false)?;
    let buck_record = verify_report(&evidence_dir.join("buck-ptrace-record-verify.json"), false)?;
    if cargo_record != buck_record {
        return Err("retained Cargo/Buck ptrace record reports differ".to_owned());
    }
    Ok(RetainedCandidateEvidence {
        run_report: cargo_run,
        record_report: cargo_record,
    })
}

fn recompute_final_receipt(
    context: &FinalReceiptContext<'_>,
) -> Result<(FinalReceiptFacts, BTreeMap<String, String>), String> {
    reverify_published_bundle(context.root, context.cargo_bundle, "Cargo")?;
    reverify_published_bundle(context.root, context.buck_bundle, "Buck")?;
    require_equal_resource_manifests(context.cargo_bundle, context.buck_bundle)?;
    reverify_safehermit_bundle(context.safehermit_bundle)?;
    reverify_input_snapshot(context.dotslash_snapshot, "snapshotted DotSlash launcher")?;
    reverify_input_snapshot(
        context.buck_descriptor_snapshot,
        "snapshotted Buck2 DotSlash descriptor",
    )?;
    reverify_input_snapshot(
        context.buck_executable_snapshot,
        "snapshotted Buck2 executable",
    )?;
    reverify_hashed_input(
        context.lzma_input,
        context.lzma_input_sha256,
        "liblzma link input",
        false,
    )?;
    verify_lzma_buck_input(context.lzma_buck_input)?;
    verify_unwind_buck_input(context.unwind_buck_input)?;
    reverify_hashed_input(
        context.generated_buck,
        context.generated_buck_sha256,
        "generated Buck dependency graph",
        false,
    )?;
    verify_matrix_supervisor_evidence(context.evidence_dir)?;
    let provenance = exact_provenance(context.root, context.cargo_build_info)?;
    let retained = verify_retained_candidate_evidence(
        context.evidence_dir,
        &provenance.version,
        &provenance.hermit_sha,
        context.cargo_build_info,
    )?;
    let run_counts = retained
        .run_report
        .compared_log_messages
        .ok_or_else(|| "Cargo strict verify report lost compared message counts".to_owned())?;
    let record_counts = retained
        .record_report
        .compared_log_messages
        .ok_or_else(|| "Cargo record report lost compared message counts".to_owned())?;
    let cargo_matrix = normalized_matrix(&context.evidence_dir.join("cargo-dbt-matrix.tsv"))?;
    let buck_matrix = normalized_matrix(&context.evidence_dir.join("buck-dbt-matrix.tsv"))?;
    require_equal(
        "official DBT matrix IDs/outcomes (duration excluded)",
        &cargo_matrix,
        &buck_matrix,
    )?;
    verify_candidate_invocation_reports(context.evidence_dir)?;
    let matrix_candidate_invocations = verify_matrix_candidate_invocation_manifest(
        context.root,
        context.evidence_dir,
        &sha256(&context.cargo_bundle.binary)?,
        &sha256(&context.buck_bundle.binary)?,
    )?;
    let event_log = context.evidence_dir.join("buck-build.json-lines.gz");
    validate_log_header(&event_log)?;
    verify_retained_buck_log_summary(
        &context.buck_executable_snapshot.path,
        context.root,
        &event_log,
        &context.evidence_dir.join("buck-log-summary.txt"),
    )?;
    let shell_exit_text = read_retained_text(context.evidence_dir, "buck-build-shell-exit.tsv")?;
    let shell_exit = shell_exit_text
        .strip_prefix("shell_exit\t")
        .and_then(|value| value.strip_suffix('\n'))
        .filter(|value| !value.contains(char::is_whitespace))
        .and_then(|value| value.parse::<i32>().ok())
        .ok_or_else(|| "retained Buck build shell-exit receipt is malformed".to_owned())?;
    let build_evidence = inspect_release_build_evidence(
        &event_log,
        &context.evidence_dir.join("buck-build.stdout"),
        shell_exit,
        context.root,
    )?;
    if build_evidence.output_sha256 != context.buck_bundle.binary_sha256 {
        return Err(
            "retained Buck build event/stdout does not bind the tested Buck candidate bytes"
                .to_owned(),
        );
    }
    let final_dotslash_version = output_text(
        Command::new(&context.dotslash_snapshot.path).arg("--version"),
        "final DotSlash --version verification",
    )?;
    let final_buck_version = output_text(
        Command::new(&context.buck_executable_snapshot.path).arg("--version"),
        "final pinned Buck2 --version verification",
    )?;
    if final_dotslash_version != context.dotslash_version
        || final_buck_version != context.buck_version
    {
        return Err("DotSlash or Buck2 version output changed during shadow validation".to_owned());
    }
    let resources = verify_install_bundle(&context.cargo_bundle.install, &provenance.reverie_sha)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let facts = FinalReceiptFacts {
        result: "pass".to_owned(),
        evidence_identity: context
            .evidence_dir
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| "evidence directory has no UTF-8 identity".to_owned())?
            .to_owned(),
        process_tree_supervisor: "dagrun-cgroup-v2".to_owned(),
        matrix_step_wall_seconds: MATRIX_STEP_DEADLINE_SECONDS,
        matrix_outer_wall_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS,
        matrix_cleanup_evidence_grace_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS
            - MATRIX_STEP_DEADLINE_SECONDS,
        process_tree_cleanup: "complete".to_owned(),
        hermit_full_sha: provenance.hermit_full_sha,
        hermit_build_sha: provenance.hermit_sha,
        reverie_sha: provenance.reverie_sha,
        version: provenance.version,
        build_date: provenance.build_date,
        dotslash_version: context.dotslash_version.to_owned(),
        dotslash_sha256: context.dotslash_snapshot.sha256.clone(),
        buck_version: context.buck_version.to_owned(),
        buck_descriptor_sha256: context.buck_descriptor_snapshot.sha256.clone(),
        buck_executable_sha256: context.buck_executable_snapshot.sha256.clone(),
        lzma_input: context.lzma_input.to_string_lossy().into_owned(),
        lzma_input_sha256: context.lzma_input_sha256.to_owned(),
        unwind_input_identity: context.unwind_buck_input.identity.clone(),
        unwind_ptrace_sha256: context.unwind_buck_input.source_sha256[UNWIND_PTRACE_ARCHIVE]
            .clone(),
        unwind_arch_sha256: context.unwind_buck_input.source_sha256[UNWIND_ARCH_SONAME].clone(),
        unwind_core_sha256: context.unwind_buck_input.source_sha256[UNWIND_CORE_SONAME].clone(),
        generated_buck_sha256: context.generated_buck_sha256.to_owned(),
        safehermit_sha256: sha256(&context.safehermit_bundle.launcher)?,
        bounded_run_space_sha256: sha256(&context.safehermit_bundle.bounded_run_space)?,
        cargo_candidate_sha256: sha256(&context.cargo_bundle.binary)?,
        cargo_hermit_manifest_sha256: sha256(&context.cargo_bundle.hermit_manifest)?,
        cargo_resources_manifest_sha256: sha256(&context.cargo_bundle.resources_manifest)?,
        buck_candidate_sha256: sha256(&context.buck_bundle.binary)?,
        buck_hermit_manifest_sha256: sha256(&context.buck_bundle.hermit_manifest)?,
        buck_resources_manifest_sha256: sha256(&context.buck_bundle.resources_manifest)?,
        event_log: "buck-build.json-lines.gz".to_owned(),
        ptrace_verify_messages_left: run_counts.left,
        ptrace_verify_messages_right: run_counts.right,
        ptrace_record_messages_left: record_counts.left,
        ptrace_record_messages_right: record_counts.right,
        cargo_matrix_rows: cargo_matrix.lines().count().saturating_sub(1),
        buck_matrix_rows: buck_matrix.lines().count().saturating_sub(1),
        matrix_candidate_manifest_sha256: matrix_candidate_invocations.manifest_sha256,
        matrix_candidate_manifest: "matrix-candidate-invocations.json".to_owned(),
        matrix_expected_ledger: "matrix-expected-ledger.json".to_owned(),
        matrix_expected_ledger_sha256: matrix_candidate_invocations.expected_ledger_sha256,
        matrix_expected_invocation_count: matrix_candidate_invocations.expected_invocation_count,
        cargo_matrix_candidate_invocations: matrix_candidate_invocations.cargo_count,
        buck_matrix_candidate_invocations: matrix_candidate_invocations.buck_count,
    };
    validate_receipt_facts(&facts)?;
    Ok((facts, resources))
}

fn validate_bound_hashes(
    description: &str,
    bindings: &BTreeMap<String, String>,
) -> Result<(), String> {
    if bindings.is_empty() {
        return Err(format!(
            "final receipt must bind a nonempty {description} population"
        ));
    }
    for (relative, hash) in bindings {
        let path = Path::new(relative);
        if !valid_sha256(hash)
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "final receipt contains an invalid {description} binding: {relative:?}"
            ));
        }
    }
    Ok(())
}

fn parse_final_receipt(text: &str) -> Result<FinalReceiptDocument, String> {
    let document: FinalReceiptDocument = decode_typed_json(text, "final shadow parity receipt")?;
    if document.receipt_schema != "hermit-buck-shadow-parity/v2" {
        return Err(format!(
            "unknown final receipt schema {:?}",
            document.receipt_schema
        ));
    }
    validate_receipt_facts(&document.facts)?;
    validate_bound_hashes("resource", &document.resource_sha256)?;
    validate_bound_hashes("artifact", &document.artifact_sha256)?;
    Ok(document)
}

fn render_final_receipt(
    facts: &FinalReceiptFacts,
    resources: &BTreeMap<String, String>,
    artifacts: &BTreeMap<String, String>,
) -> Result<String, String> {
    let document = FinalReceiptDocument {
        receipt_schema: "hermit-buck-shadow-parity/v2".to_owned(),
        facts: facts.clone(),
        resource_sha256: resources.clone(),
        artifact_sha256: artifacts.clone(),
    };
    validate_receipt_facts(&document.facts)?;
    validate_bound_hashes("resource", &document.resource_sha256)?;
    validate_bound_hashes("artifact", &document.artifact_sha256)?;
    serde_json::to_string_pretty(&document)
        .map(|contents| format!("{contents}\n"))
        .map_err(|error| format!("failed to encode final shadow parity receipt: {error}"))
}

fn verify_receipt_semantics(
    parsed: &FinalReceiptDocument,
    expected_facts: &FinalReceiptFacts,
    expected_resources: &BTreeMap<String, String>,
    expected_artifacts: &BTreeMap<String, String>,
) -> Result<(), String> {
    if &parsed.facts != expected_facts {
        return Err("final receipt semantic singleton mismatch".to_owned());
    }
    if &parsed.resource_sha256 != expected_resources {
        return Err("final receipt resource inventory/hash mismatch".to_owned());
    }
    if &parsed.artifact_sha256 != expected_artifacts {
        return Err("final receipt artifact population/hash mismatch".to_owned());
    }
    Ok(())
}

fn publish_final_receipt(context: &FinalReceiptContext<'_>) -> Result<PathBuf, String> {
    let receipt = context.evidence_dir.join("shadow-parity.json");
    let (facts, resources) = recompute_final_receipt(context)?;
    let mut artifact_rows = Vec::new();
    collect_evidence_hashes(
        context.evidence_dir,
        context.evidence_dir,
        &receipt,
        &mut artifact_rows,
    )?;
    let artifacts = artifact_rows.into_iter().collect::<BTreeMap<_, _>>();
    let contents = render_final_receipt(&facts, &resources, &artifacts)?;
    atomic_write_new(&receipt, &contents)?;
    verify_final_receipt(context, &receipt)?;
    Ok(receipt)
}

fn verify_final_receipt(context: &FinalReceiptContext<'_>, receipt: &Path) -> Result<(), String> {
    let text = fs::read_to_string(receipt)
        .map_err(|error| format!("final receipt {} is unreadable: {error}", receipt.display()))?;
    let parsed = parse_final_receipt(&text)?;
    let (expected_facts, expected_resources) = recompute_final_receipt(context)?;
    let mut artifact_rows = Vec::new();
    collect_evidence_hashes(
        context.evidence_dir,
        context.evidence_dir,
        receipt,
        &mut artifact_rows,
    )?;
    let expected_artifacts = artifact_rows.into_iter().collect::<BTreeMap<_, _>>();
    verify_receipt_semantics(
        &parsed,
        &expected_facts,
        &expected_resources,
        &expected_artifacts,
    )
}

fn validate_dag_provenance(root: &Path) -> Result<(Provenance, BuildInfo), String> {
    let version = package_version(
        &fs::read_to_string(root.join("hermit-cli/Cargo.toml"))
            .map_err(|error| format!("failed to read hermit-cli/Cargo.toml: {error}"))?,
    )?;
    let hermit_sha = git(root, &["rev-parse", "--short=12", "HEAD"])?;
    let build_date = git(root, &["show", "-s", "--format=%cs", "HEAD"])?;
    if !canonical_build_date(&build_date) {
        return Err(format!(
            "Git commit date {build_date:?} is not canonical YYYY-MM-DD provenance"
        ));
    }
    let info = BuildInfo {
        schema: BuildInfo::SCHEMA,
        version,
        build_date: Some(build_date),
        git_sha: hermit_sha,
        features: BuildFeatures {
            dbt: true,
            e9patch: true,
            sabre: true,
        },
    };
    let provenance = exact_provenance(root, &info)?;
    Ok((provenance, info))
}

fn phase2_evidence_directory(root: &Path, full_sha: &str) -> Result<PathBuf, String> {
    let parent = root.join("ignored/buck2-phase2/builds");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let path = parent.join(format!("build-{full_sha}-{}", unique_identity()?));
    create_exclusive_directory(&path, "Buck validate build evidence directory")?;
    Ok(path)
}

fn replace_text_file(path: &Path, contents: &[u8], description: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{description} has no parent directory"))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let temporary = path.with_extension(format!("tmp.{}", unique_identity()?));
    write_new_file(&temporary, contents, &format!("temporary {description}"))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!(
            "failed to publish {description} {}: {error}",
            path.display()
        )
    })
}

fn validate_artifact_identity(root: &Path) -> Result<ValidateArtifact, String> {
    let identity_path = root.join(VALIDATE_ARTIFACT_IDENTITY);
    let document = fs::read_to_string(&identity_path).map_err(|error| {
        format!(
            "Buck validate artifact identity {} is unreadable: {error}; the host Buck producer did not complete",
            identity_path.display()
        )
    })?;
    let rows = document.lines().collect::<Vec<_>>();
    let [
        schema,
        head,
        identity,
        binary_sha256,
        resources_sha256,
        arch_sha256,
        core_sha256,
    ] = rows.as_slice()
    else {
        return Err(
            "Buck validate artifact identity document must contain exactly seven rows".into(),
        );
    };
    let schema = schema.strip_prefix("schema\t").unwrap_or_default();
    let head = head.strip_prefix("head\t").unwrap_or_default();
    let identity = identity.strip_prefix("identity\t").unwrap_or_default();
    let binary_sha256 = binary_sha256
        .strip_prefix("binary_sha256\t")
        .unwrap_or_default();
    let resources_sha256 = resources_sha256
        .strip_prefix("resources_sha256\t")
        .unwrap_or_default();
    let parse_runtime = |row: &str, name: &str| {
        row.strip_prefix(&format!("runtime_sha256\t{name}\t"))
            .unwrap_or_default()
            .to_owned()
    };
    let arch_sha256 = parse_runtime(arch_sha256, UNWIND_ARCH_SONAME);
    let core_sha256 = parse_runtime(core_sha256, UNWIND_CORE_SONAME);
    let lowercase_sha256 = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if !document.ends_with('\n')
        || schema != "hermit-buck-validate-selector/v2"
        || head.len() != 40
        || !head.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !lowercase_sha256(identity)
        || !lowercase_sha256(binary_sha256)
        || !lowercase_sha256(resources_sha256)
        || !lowercase_sha256(&arch_sha256)
        || !lowercase_sha256(&core_sha256)
    {
        return Err("Buck validate artifact identity document has invalid typed fields".into());
    }
    let current_head = git(root, &["rev-parse", "HEAD"])?;
    if current_head != head {
        return Err(format!(
            "Buck validate artifact was built for {head}, not current HEAD {current_head}"
        ));
    }
    let bundle = root.join(VALIDATE_ARTIFACT_ROOT).join(identity);
    let resolved = output_text(
        Command::new(root.join("ci/verify-hermit-e2e-artifact.sh")).arg(&bundle),
        "verify content-addressed Buck validate artifact",
    )?;
    let resolved = PathBuf::from(resolved);
    if resolved
        != bundle.canonicalize().map_err(|error| {
            format!(
                "cannot canonicalize Buck validate artifact {}: {error}",
                bundle.display()
            )
        })?
    {
        return Err("Buck validate artifact verifier resolved a different bundle".into());
    }
    let binary = resolved.join("hermit");
    if sha256(&binary)? != binary_sha256 {
        return Err("Buck validate selector binary hash does not match the verified bundle".into());
    }
    validate_release_needed_libraries(&needed_libraries(&binary)?)?;
    validate_release_rpath(&binary)?;
    let resources = resolved.join("resources.sha256");
    if sha256(&resources)? != resources_sha256 {
        return Err(
            "Buck validate selector resource manifest hash does not match the verified bundle"
                .into(),
        );
    }
    let runtime_sha256 = BTreeMap::from([
        (UNWIND_ARCH_SONAME.to_owned(), arch_sha256.clone()),
        (UNWIND_CORE_SONAME.to_owned(), core_sha256.clone()),
    ]);
    let install = resolved.join("install");
    verify_runtime_install(&install, &runtime_sha256)?;
    let inventory = parse_resource_manifest(&resources, "Buck validate runtime resource manifest")?;
    let expected_inventory = BTreeMap::from([
        (
            format!("rsrcs/hermit-runtime/{UNWIND_ARCH_SONAME}"),
            arch_sha256,
        ),
        (
            format!("rsrcs/hermit-runtime/{UNWIND_CORE_SONAME}"),
            core_sha256,
        ),
    ]);
    if inventory != expected_inventory {
        return Err("Buck validate runtime resource manifest has a missing or extra entry".into());
    }
    Ok(ValidateArtifact {
        bundle: resolved,
        binary,
        install,
        identity: identity.to_owned(),
        binary_sha256: binary_sha256.to_owned(),
        resources_sha256: resources_sha256.to_owned(),
        runtime_sha256,
    })
}

fn replace_validate_output(root: &Path, source: &Path, relative: &str) -> Result<String, String> {
    let destination = root.join(relative);
    let parent = destination
        .parent()
        .ok_or_else(|| format!("validate output {relative} has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let temporary = destination.with_extension(format!("tmp.{}", unique_identity()?));
    let source_hash = copy_stable_executable(source, &temporary, "Buck validate artifact")?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)).map_err(|error| {
        format!(
            "failed to set executable permissions on {}: {error}",
            temporary.display()
        )
    })?;
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!(
            "failed to install Buck validate output {}: {error}",
            destination.display()
        )
    })?;
    if sha256(source)? != source_hash || sha256(&destination)? != source_hash {
        return Err(format!(
            "Buck validate output {relative} changed during atomic installation"
        ));
    }
    Ok(source_hash)
}

fn install_validate_dag_artifact(root: &Path) -> Result<String, String> {
    let artifact = validate_artifact_identity(root)?;
    let release_hash = replace_validate_output(root, &artifact.binary, "target/release/hermit")?;
    let strict_hash = replace_validate_output(root, &artifact.binary, "target/ci/hermit-strict")?;
    if release_hash != strict_hash {
        return Err("release and strict Buck validate artifact copies differ".into());
    }
    let install = root.join("target/install_pkg");
    let parent = install.join("rsrcs");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".hermit-runtime.tmp-{}", unique_identity()?));
    create_exclusive_directory(&temporary, "temporary installed Hermit runtime")?;
    let copy_result = (|| {
        for name in [UNWIND_ARCH_SONAME, UNWIND_CORE_SONAME] {
            let source = artifact.install.join(UNWIND_RUNTIME_RELATIVE).join(name);
            let expected = &artifact.runtime_sha256[name];
            let before = sha256(&source)?;
            fs::copy(&source, temporary.join(name))
                .map_err(|error| format!("failed to install runtime {name}: {error}"))?;
            let after = sha256(&source)?;
            if before != *expected
                || after != *expected
                || sha256(&temporary.join(name))? != *expected
            {
                return Err(format!("runtime {name} changed during atomic installation"));
            }
        }
        verify_runtime_directory(&temporary, &artifact.runtime_sha256)
    })();
    if let Err(error) = copy_result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    let destination = parent.join("hermit-runtime");
    if let Ok(metadata) = fs::symlink_metadata(&destination) {
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&destination)
                .map_err(|error| format!("failed to replace {}: {error}", destination.display()))?;
        } else {
            fs::remove_file(&destination)
                .map_err(|error| format!("failed to replace {}: {error}", destination.display()))?;
        }
    }
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_dir_all(&temporary);
        format!("failed to publish {}: {error}", destination.display())
    })?;
    verify_runtime_install(&install, &artifact.runtime_sha256)?;
    validate_artifact_identity(root)?;
    loader_probe(
        &root.join("target/release/hermit"),
        "direct release Hermit loader probe",
    )?;
    loader_probe(
        &root.join("target/ci/hermit-strict"),
        "direct strict Hermit loader probe",
    )?;
    loader_probe(&artifact.binary, "published Buck artifact loader probe")?;
    Ok(strict_hash)
}

fn build_validate_dag_artifact(dotslash: &Path) -> Result<(), String> {
    let root = repository_root()?;
    let caller_dotslash = require_absolute_file(dotslash, "public DotSlash launcher")?;
    let rust_script_version = output_text(
        Command::new("rust-script").arg("--version"),
        "rust-script --version",
    )?;
    if rust_script_version != RUST_SCRIPT_VERSION {
        return Err(format!(
            "Buck validate build requires documented {RUST_SCRIPT_VERSION}, got {rust_script_version:?}"
        ));
    }
    let (provenance, expected_info) = validate_dag_provenance(&root)?;
    let evidence_dir = phase2_evidence_directory(&root, &provenance.hermit_full_sha)?;
    let dotslash_snapshot = snapshot_dotslash(&caller_dotslash, &evidence_dir)?;
    let dotslash_version = output_text(
        Command::new(&dotslash_snapshot.path).arg("--version"),
        "snapshotted DotSlash --version",
    )?;
    if dotslash_version != DOTSLASH_VERSION {
        return Err(format!(
            "--dotslash must be the documented public {DOTSLASH_VERSION}, got {dotslash_version:?}"
        ));
    }
    let descriptor = snapshot_input(
        &root.join("bootstrap/buck2"),
        &evidence_dir,
        "buck2-descriptor-snapshot",
        "buck2.descriptor",
        "Buck2 DotSlash descriptor",
        false,
    )?;
    let fetched_buck = PathBuf::from(output_text(
        Command::new(&dotslash_snapshot.path).args([
            OsStr::new("--"),
            OsStr::new("fetch"),
            descriptor.path.as_os_str(),
        ]),
        "DotSlash fetch of pinned Buck2",
    )?);
    let buck_snapshot = snapshot_input(
        &fetched_buck,
        &evidence_dir,
        "buck2-tool-snapshot",
        "buck2",
        "resolved Buck2 executable",
        true,
    )?;
    let lzma_input = require_lzma_link_input()?;
    let lzma_hash = sha256(&lzma_input)?;
    let lzma_buck_input = publish_lzma_buck_input(&root, &evidence_dir, &lzma_input)?;
    let unwind_buck_input =
        publish_unwind_buck_input(&root, &evidence_dir, require_unwind_closure()?)?;

    checked_output(
        Command::new(root.join("bootstrap/regenerate-rust-deps")).current_dir(&root),
        "deterministic Rust dependency regeneration",
    )?;
    let generated_buck = root.join("shim/third-party/rust/BUCK");
    verify_generated_buck(
        &fs::read_to_string(&generated_buck)
            .map_err(|error| format!("generated BUCK is unreadable: {error}"))?,
    )?;
    let generated_hash = sha256(&generated_buck)?;

    let event_log = evidence_dir.join("buck-build.json-lines.gz");
    let build = Command::new(&buck_snapshot.path)
        .current_dir(&root)
        .args(["build", "--no-remote-cache", "--show-output", "--event-log"])
        .arg(&event_log)
        .args([
            "-c",
            &format!("hermit_release.version={}", provenance.version),
            "-c",
            &format!("hermit_release.build_date={}", provenance.build_date),
            "-c",
            &format!("hermit_release.hermit_sha={}", provenance.hermit_sha),
            "-c",
            &format!("hermit_release.reverie_sha={}", provenance.reverie_sha),
            "-c",
            &format!("hermit_release.lzma_target={}", lzma_buck_input.target),
            "-c",
            &format!("hermit_release.unwind_target={}", unwind_buck_input.target),
            TARGET,
        ])
        .output()
        .map_err(|error| format!("failed to start snapshotted Buck2 executable: {error}"))?;
    write_new_file(
        &evidence_dir.join("buck-build.stdout"),
        &build.stdout,
        "Buck build stdout",
    )?;
    write_new_file(
        &evidence_dir.join("buck-build.stderr"),
        &build.stderr,
        "Buck build stderr",
    )?;
    let shell_exit = build.status.code().unwrap_or(255);
    atomic_write_new(
        &evidence_dir.join("buck-build-shell-exit.tsv"),
        &format!("shell_exit\t{shell_exit}\n"),
    )?;
    let stdout = String::from_utf8(build.stdout)
        .map_err(|error| format!("Buck build output was not UTF-8: {error}"))?;
    let show_output = evidence_dir.join("buck-build.stdout");
    let build_evidence =
        inspect_release_build_evidence(&event_log, &show_output, shell_exit, &root)?;
    atomic_write_new(
        &evidence_dir.join("buck-action-executions.tsv"),
        &format!(
            "action_executions\t{}\nremote_action_executions\t0\n",
            build_evidence.action_execution_summary()
        ),
    )?;
    let candidate = resolve_release_show_output(&stdout, &root)?;
    if candidate != build_evidence.output || sha256(&candidate)? != build_evidence.output_sha256 {
        return Err(
            "Buck event/stdout reconciliation did not bind the selected candidate bytes".into(),
        );
    }
    let candidate_needed = needed_libraries(&candidate)?;
    validate_release_needed_libraries(&candidate_needed)?;
    validate_release_rpath(&candidate)?;
    atomic_write_new(
        &evidence_dir.join("buck-needed-libraries.tsv"),
        &format!(
            "schema\thermit-buck-needed-libraries/v1\n{}",
            candidate_needed
                .iter()
                .map(|library| format!("needed\t{library}\n"))
                .collect::<String>()
        ),
    )?;
    reverify_input_snapshot(&dotslash_snapshot, "DotSlash launcher")?;
    reverify_input_snapshot(&descriptor, "Buck2 descriptor")?;
    reverify_input_snapshot(&buck_snapshot, "Buck2 executable")?;
    reverify_hashed_input(
        &generated_buck,
        &generated_hash,
        "generated Buck graph",
        false,
    )?;
    reverify_hashed_input(&lzma_input, &lzma_hash, "liblzma link input", false)?;
    verify_lzma_buck_input(&lzma_buck_input)?;
    verify_unwind_buck_input(&unwind_buck_input)?;
    let (final_provenance, final_expected_info) = validate_dag_provenance(&root)?;
    if final_provenance != provenance || final_expected_info != expected_info {
        return Err(
            "Hermit or Reverie provenance changed during the Buck validate build; refusing a mixed-tree artifact"
                .into(),
        );
    }

    let artifact_root = root.join(VALIDATE_ARTIFACT_ROOT);
    let pointer = evidence_dir.join("published-artifact.path");
    let runtime_install = prepare_runtime_install(&evidence_dir, &unwind_buck_input)?;
    checked_output(
        Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
            candidate.as_os_str(),
            artifact_root.as_os_str(),
            pointer.as_os_str(),
            OsStr::new("--runtime-only"),
            runtime_install.as_os_str(),
        ]),
        "publish content-addressed Buck validate artifact",
    )?;
    let bundle = PathBuf::from(
        fs::read_to_string(&pointer)
            .map_err(|error| format!("published Buck artifact pointer is unreadable: {error}"))?
            .trim(),
    );
    let identity = bundle
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "published Buck artifact path has no UTF-8 content identity".to_owned())?;
    if identity.len() != 64 || !identity.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("published Buck artifact identity is not SHA-256 shaped".into());
    }
    replace_text_file(
        &root.join(VALIDATE_ARTIFACT_IDENTITY),
        format!(
            "schema\thermit-buck-validate-selector/v2\nhead\t{}\nidentity\t{}\nbinary_sha256\t{}\nresources_sha256\t{}\nruntime_sha256\t{}\t{}\nruntime_sha256\t{}\t{}\n",
            provenance.hermit_full_sha,
            identity,
            build_evidence.output_sha256,
            sha256(&bundle.join("resources.sha256"))?,
            UNWIND_ARCH_SONAME,
            unwind_buck_input.source_sha256[UNWIND_ARCH_SONAME],
            UNWIND_CORE_SONAME,
            unwind_buck_input.source_sha256[UNWIND_CORE_SONAME],
        )
        .as_bytes(),
        "current Buck validate artifact identity",
    )?;
    let installed_hash = install_validate_dag_artifact(&root)?;
    if installed_hash != build_evidence.output_sha256 {
        return Err("installed Buck validate artifact differs from reconciled Buck output".into());
    }
    let version = checked_output(
        Command::new("env")
            .args(["-u", "LD_LIBRARY_PATH"])
            .arg("timeout")
            .args(["--signal=TERM", "--kill-after=2s", "10s"])
            .arg(root.join("target/ci/hermit-strict"))
            .args(["version", "--json"]),
        "bounded installed Buck validate candidate version probe",
    )?;
    if !version.stderr.is_empty() {
        return Err("Buck validate candidate version probe wrote unexpected stderr".into());
    }
    let observed = decode_build_info(
        &String::from_utf8(version.stdout)
            .map_err(|error| format!("Buck validate version output was not UTF-8: {error}"))?,
        &provenance.version,
        &provenance.hermit_sha,
        "Buck validate version JSON",
    )?;
    if observed != expected_info {
        return Err(
            "Buck validate candidate provenance/features differ from requested values".into(),
        );
    }
    atomic_write_new(
        &evidence_dir.join("validate-artifact.tsv"),
        &format!(
            "schema\thermit-buck-validate-artifact/v1\nhead\t{}\ntarget\t{}\nidentity\t{}\nsha256\t{}\nresources_sha256\t{}\nruntime_sha256\t{}\t{}\nruntime_sha256\t{}\t{}\n",
            provenance.hermit_full_sha,
            TARGET,
            identity,
            installed_hash,
            sha256(&bundle.join("resources.sha256"))?,
            UNWIND_ARCH_SONAME,
            unwind_buck_input.source_sha256[UNWIND_ARCH_SONAME],
            UNWIND_CORE_SONAME,
            unwind_buck_input.source_sha256[UNWIND_CORE_SONAME],
        ),
    )?;
    println!(
        "Buck validate artifact ready: identity={identity} sha256={installed_hash} evidence={}",
        evidence_dir.display()
    );
    Ok(())
}

fn run(options: Options, cgroups: BoxedCgroups) -> Result<(), String> {
    if cgroups.is_none() {
        return Err(
            "Buck shadow validation requires live per-step cgroup supervision; unboxed execution is forbidden"
                .to_owned(),
        );
    }
    let root = repository_root()?;
    let caller_dotslash = require_absolute_file(&options.dotslash, "public DotSlash launcher")?;
    let caller_cargo_binary = require_absolute_file(&options.cargo_binary, "Cargo release binary")?;
    let caller_install_bundle =
        require_absolute_directory(&options.install_bundle, "Cargo install bundle")?;
    let caller_safehermit = require_absolute_file(&options.safehermit, "safehermit launcher")?;

    let rust_script_version = output_text(
        Command::new("rust-script").arg("--version"),
        "rust-script --version",
    )?;
    if rust_script_version != RUST_SCRIPT_VERSION {
        return Err(format!(
            "regeneration requires documented {RUST_SCRIPT_VERSION}, got {rust_script_version:?}; install that version and retry"
        ));
    }

    if repository_dirty(&root)? {
        return Err(
            "Hermit checkout has tracked or non-ignored untracked changes; shadow evidence requires a clean exact tree"
                .to_owned(),
        );
    }
    let preflight_full_sha = git(&root, &["rev-parse", "HEAD"])?;
    let evidence_dir = create_evidence_directory(&root, &preflight_full_sha)?;
    let dotslash_snapshot = snapshot_dotslash(&caller_dotslash, &evidence_dir)?;
    drop(caller_dotslash);
    let dotslash = dotslash_snapshot.path.clone();
    let dotslash_version = output_text(
        Command::new(&dotslash).arg("--version"),
        "snapshotted DotSlash --version",
    )?;
    if dotslash_version != DOTSLASH_VERSION {
        return Err(format!(
            "--dotslash must be the documented public {DOTSLASH_VERSION}, got {dotslash_version:?}; install the pinned launcher from docs/BUCK2_OSS.md"
        ));
    }
    let unwind_buck_input =
        publish_unwind_buck_input(&root, &evidence_dir, require_unwind_closure()?)?;
    let runtime_install = prepare_runtime_install(&evidence_dir, &unwind_buck_input)?;
    let cargo_bundle = publish_verified_bundle(
        &root,
        &evidence_dir,
        "cargo",
        &caller_cargo_binary,
        &caller_install_bundle,
        &runtime_install,
    )?;
    drop(caller_cargo_binary);
    let safehermit_bundle = snapshot_safehermit_bundle(&caller_safehermit, &evidence_dir)?;
    drop(caller_safehermit);
    let safehermit = safehermit_bundle.launcher.clone();
    let cargo_version_data = evidence_dir.join("cargo-version-data");
    let cargo_version_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install_bundle: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "cargo-version",
        data_dir: &cargo_version_data,
        arguments: &["version", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Cargo version JSON through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "cargo-version.json", &cargo_version_output)?;
    let cargo_version_json = read_retained_text(&evidence_dir, "cargo-version.json")?;
    let expected_version = package_version(
        &fs::read_to_string(root.join("hermit-cli/Cargo.toml"))
            .map_err(|error| format!("failed to read hermit-cli/Cargo.toml: {error}"))?,
    )?;
    let expected_short_sha = git(&root, &["rev-parse", "--short=12", "HEAD"])?;
    let cargo_build_info = decode_build_info(
        &cargo_version_json,
        &expected_version,
        &expected_short_sha,
        "Cargo version JSON",
    )?;
    let provenance = exact_provenance(&root, &cargo_build_info)?;
    if provenance.hermit_full_sha != preflight_full_sha {
        return Err("Hermit HEAD changed while shadow validation was starting".to_owned());
    }
    verify_install_bundle(&cargo_bundle.install, &provenance.reverie_sha)?;
    let lzma_input = require_lzma_link_input()?;
    let lzma_input_hash = sha256(&lzma_input)?;
    let lzma_buck_input = publish_lzma_buck_input(&root, &evidence_dir, &lzma_input)?;
    let buck_descriptor_source = root.join("bootstrap/buck2");
    let buck_descriptor_snapshot = snapshot_input(
        &buck_descriptor_source,
        &evidence_dir,
        "buck2-descriptor-snapshot",
        "buck2.descriptor",
        "Buck2 DotSlash descriptor",
        false,
    )?;
    let fetched_buck_executable = PathBuf::from(output_text(
        Command::new(&dotslash).args([
            OsStr::new("--"),
            OsStr::new("fetch"),
            buck_descriptor_snapshot.path.as_os_str(),
        ]),
        "DotSlash fetch of pinned Buck2",
    )?);
    let buck_executable_snapshot = snapshot_input(
        &fetched_buck_executable,
        &evidence_dir,
        "buck2-tool-snapshot",
        "buck2",
        "resolved Buck2 executable",
        true,
    )?;
    let buck_executable = buck_executable_snapshot.path.clone();
    let buck_version = output_text(
        Command::new(&buck_executable).arg("--version"),
        "snapshotted pinned Buck2 --version",
    )?;

    let regenerate = root.join("bootstrap/regenerate-rust-deps");
    checked_output(
        Command::new(&regenerate).current_dir(&root),
        "deterministic Rust dependency regeneration",
    )?;
    let generated_buck = root.join("shim/third-party/rust/BUCK");
    verify_generated_buck(
        &fs::read_to_string(&generated_buck)
            .map_err(|error| format!("generated BUCK is unreadable: {error}"))?,
    )?;
    let generated_buck_hash = sha256(&generated_buck)?;

    let event_log = evidence_dir.join("buck-build.json-lines.gz");
    let build = Command::new(&buck_executable)
        .current_dir(&root)
        .args(["build", "--no-remote-cache", "--show-output", "--event-log"])
        .arg(&event_log)
        .args([
            "-c",
            &format!("hermit_release.version={}", provenance.version),
            "-c",
            &format!("hermit_release.build_date={}", provenance.build_date),
            "-c",
            &format!("hermit_release.hermit_sha={}", provenance.hermit_sha),
            "-c",
            &format!("hermit_release.reverie_sha={}", provenance.reverie_sha),
            "-c",
            &format!("hermit_release.lzma_target={}", lzma_buck_input.target),
            "-c",
            &format!("hermit_release.unwind_target={}", unwind_buck_input.target),
            TARGET,
        ])
        .output()
        .map_err(|error| format!("failed to start snapshotted Buck2 executable: {error}"))?;
    write_new_file(
        &evidence_dir.join("buck-build.stdout"),
        &build.stdout,
        "Buck build stdout",
    )?;
    write_new_file(
        &evidence_dir.join("buck-build.stderr"),
        &build.stderr,
        "Buck build stderr",
    )?;
    let build_shell_exit = build.status.code().unwrap_or(255);
    atomic_write_new(
        &evidence_dir.join("buck-build-shell-exit.tsv"),
        &format!("shell_exit\t{build_shell_exit}\n"),
    )?;
    reverify_hashed_input(
        &generated_buck,
        &generated_buck_hash,
        "generated Buck dependency graph immediately after build",
        false,
    )?;
    reverify_hashed_input(
        &lzma_input,
        &lzma_input_hash,
        "liblzma link input immediately after build",
        false,
    )?;
    verify_lzma_buck_input(&lzma_buck_input)?;
    verify_unwind_buck_input(&unwind_buck_input)?;
    validate_log_header(&event_log)?;
    let summary = output_text(
        Command::new(&buck_executable)
            .current_dir(&root)
            .args(["log", "summary"])
            .arg(&event_log),
        "Buck2 event-log decode/summary",
    )?;
    atomic_write_new(
        &evidence_dir.join("buck-log-summary.txt"),
        &format!("{summary}\n"),
    )?;
    if !build.status.success() {
        return Err(format!(
            "Buck release build failed with {} after retaining and decoding {}; inspect it and retry",
            build.status,
            event_log.display()
        ));
    }
    let build_stdout = String::from_utf8(build.stdout)
        .map_err(|error| format!("Buck build output was not UTF-8: {error}"))?;
    let buck_binary = resolve_release_show_output(&build_stdout, &root)?;
    validate_release_needed_libraries(&needed_libraries(&buck_binary)?)?;
    validate_release_rpath(&buck_binary)?;

    let buck_bundle = publish_buck_shadow_bundle(
        &root,
        &evidence_dir,
        &buck_binary,
        &caller_install_bundle,
        &runtime_install,
        &cargo_bundle,
    )?;
    drop(caller_install_bundle);

    let buck_version_data = evidence_dir.join("buck-version-data");
    let buck_version_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install_bundle: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "buck-version",
        data_dir: &buck_version_data,
        arguments: &["version", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Buck version JSON through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "buck-version.json", &buck_version_output)?;
    let buck_version_json = read_retained_text(&evidence_dir, "buck-version.json")?;
    let buck_build_info = decode_build_info(
        &buck_version_json,
        &provenance.version,
        &provenance.hermit_sha,
        "Buck version JSON",
    )?;
    if cargo_build_info != buck_build_info {
        return Err(
            "Cargo/Buck typed version/provenance/features mismatch; rebuild both at the same SHA/configuration"
                .to_owned(),
        );
    }
    let cargo_help_data = evidence_dir.join("cargo-help-data");
    let cargo_help_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install_bundle: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "cargo-help",
        data_dir: &cargo_help_data,
        arguments: &["--help"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Cargo Hermit help through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "cargo-help.txt", &cargo_help_output)?;
    let cargo_help = read_retained_text(&evidence_dir, "cargo-help.txt")?;
    let buck_help_data = evidence_dir.join("buck-help-data");
    let buck_help_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install_bundle: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "buck-help",
        data_dir: &buck_help_data,
        arguments: &["--help"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Buck Hermit help through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "buck-help.txt", &buck_help_output)?;
    let buck_help = read_retained_text(&evidence_dir, "buck-help.txt")?;
    require_equal("backend capability/help inventory", &cargo_help, &buck_help)?;
    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        if !buck_help.contains(backend) {
            return Err(format!(
                "Buck CLI capability output omits backend spelling {backend}"
            ));
        }
    }
    let cargo_host_data = evidence_dir.join("cargo-host-capabilities-data");
    let cargo_host_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install_bundle: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "cargo-host-capabilities",
        data_dir: &cargo_host_data,
        arguments: &["host-capabilities", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Cargo host capabilities through safehermit",
    })?
    .stdout;
    retain_candidate_text(
        &evidence_dir,
        "cargo-host-capabilities.json",
        &cargo_host_output,
    )?;
    let cargo_host = read_retained_text(&evidence_dir, "cargo-host-capabilities.json")?;
    let buck_host_data = evidence_dir.join("buck-host-capabilities-data");
    let buck_host_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install_bundle: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "buck-host-capabilities",
        data_dir: &buck_host_data,
        arguments: &["host-capabilities", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Buck host capabilities through safehermit",
    })?
    .stdout;
    retain_candidate_text(
        &evidence_dir,
        "buck-host-capabilities.json",
        &buck_host_output,
    )?;
    let buck_host = read_retained_text(&evidence_dir, "buck-host-capabilities.json")?;
    let cargo_host_report = decode_host_capabilities(&cargo_host, "Cargo host-capabilities JSON")?;
    let buck_host_report = decode_host_capabilities(&buck_host, "Buck host-capabilities JSON")?;
    if cargo_host_report != buck_host_report {
        return Err(
            "Cargo/Buck typed host-capabilities mismatch; reports differ in capability or evidence bytes"
                .to_owned(),
        );
    }
    if elf_identity(&cargo_bundle.binary)? != elf_identity(&buck_bundle.binary)? {
        return Err("Cargo/Buck ELF class/endian/type/machine differ".to_owned());
    }
    if needed_libraries(&cargo_bundle.binary)? != needed_libraries(&buck_bundle.binary)? {
        return Err("Cargo/Buck ELF NEEDED library sets differ".to_owned());
    }
    behavioral_parity(
        &root,
        &safehermit,
        &cargo_bundle.binary,
        &buck_bundle.binary,
        &cargo_bundle.install,
        &buck_bundle.install,
        &evidence_dir,
    )?;
    let matrix_supervisor_logs = evidence_dir.join("matrix-supervisor-logs");
    create_exclusive_directory(&matrix_supervisor_logs, "matrix supervisor log directory")?;
    // This process is single-threaded and owns both sequential scheduler calls.
    // The official runner reads this parent setting when it opens durable logs.
    unsafe {
        env::set_var("DAGRUN_LOG_DIR", &matrix_supervisor_logs);
    }
    let matrix_envelope = reviewed_dbt_matrix_envelope();
    publish_official_matrix_expected_ledger(&root, &evidence_dir)?;
    let cargo_matrix_output = evidence_dir.join("cargo-dbt-matrix.tsv");
    let cargo_matrix = run_dbt_matrix(MatrixInvocation {
        root: &root,
        cgroups: cgroups.clone(),
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        candidate_label: "cargo",
        output: &cargo_matrix_output,
        description: "official Cargo DBT parity matrix",
        envelope: &matrix_envelope,
    })?;
    let buck_matrix_output = evidence_dir.join("buck-dbt-matrix.tsv");
    let buck_matrix = run_dbt_matrix(MatrixInvocation {
        root: &root,
        cgroups,
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        candidate_label: "buck",
        output: &buck_matrix_output,
        description: "official Buck DBT parity matrix",
        envelope: &matrix_envelope,
    })?;
    require_equal(
        "official DBT matrix IDs/outcomes (duration excluded)",
        &cargo_matrix,
        &buck_matrix,
    )?;
    publish_matrix_candidate_invocation_manifest(
        &root,
        &evidence_dir,
        &sha256(&cargo_bundle.binary)?,
        &sha256(&buck_bundle.binary)?,
    )?;
    let final_provenance = exact_provenance(&root, &cargo_build_info)?;
    if final_provenance != provenance {
        return Err(
            "Hermit/Reverie provenance changed during shadow validation; refusing a mixed-tree receipt"
                .to_owned(),
        );
    }
    let receipt_context = FinalReceiptContext {
        root: &root,
        evidence_dir: &evidence_dir,
        cargo_build_info: &cargo_build_info,
        cargo_bundle: &cargo_bundle,
        buck_bundle: &buck_bundle,
        safehermit_bundle: &safehermit_bundle,
        dotslash_snapshot: &dotslash_snapshot,
        dotslash_version: &dotslash_version,
        buck_descriptor_snapshot: &buck_descriptor_snapshot,
        buck_executable_snapshot: &buck_executable_snapshot,
        buck_version: &buck_version,
        lzma_input: &lzma_input,
        lzma_input_sha256: &lzma_input_hash,
        lzma_buck_input: &lzma_buck_input,
        unwind_buck_input: &unwind_buck_input,
        generated_buck: &generated_buck,
        generated_buck_sha256: &generated_buck_hash,
    };
    let receipt = publish_final_receipt(&receipt_context)?;
    println!("Buck shadow parity PASS; receipt={}", receipt.display());
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--validate-dag-build") {
        let result = match arguments.as_slice() {
            [_, option, dotslash] if option == "--dotslash" => {
                build_validate_dag_artifact(Path::new(dotslash))
            }
            _ => Err(
                "usage: build-buck-release.rs --validate-dag-build --dotslash ABSOLUTE-PATH"
                    .to_owned(),
            ),
        };
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs validate DAG build: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--validate-dag-install") {
        let result = if arguments.len() == 1 {
            repository_root().and_then(|root| {
                install_validate_dag_artifact(&root).map(|hash| {
                    println!("installed verified Buck validate artifact sha256={hash}");
                })
            })
        } else {
            Err("usage: build-buck-release.rs --validate-dag-install".to_owned())
        };
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs validate DAG install: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--reconcile-release-evidence") {
        return match run_release_evidence_reconciler(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs reconciliation: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--validate-release-candidate") {
        return match run_release_candidate_validator(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs release candidate validation: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--scan-and-package-public-evidence") {
        return match run_scan_and_package_public_evidence(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs combined public evidence package: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--scan-and-stage-public-evidence") {
        return match run_scan_and_stage_public_evidence(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs combined public evidence staging: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--release-evidence-verdict") {
        return match run_release_evidence_verdict(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs release evidence verdict: {error}");
                ExitCode::from(2)
            }
        };
    }
    if env::var_os(MATRIX_PROXY_ENV).as_deref() == Some(OsStr::new("1")) {
        return match matrix_candidate_proxy(arguments.into_iter()) {
            Ok(status) => status,
            Err(error) => {
                eprintln!("build-buck-release.rs matrix proxy: {error}");
                ExitCode::from(2)
            }
        };
    }
    match parse_options(arguments) {
        Ok(None) => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(Some(options)) => {
            let cgroups = match safe_ci_scope::resolve_cgroups(
                "Buck phase-1 shadow validation",
                false,
                Some(OUTER_SCOPE_DEADLINE_SECONDS),
                true,
            ) {
                Ok(cgroups) => cgroups,
                Err(code) => return ExitCode::from(code),
            };
            match run(options, cgroups) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("build-buck-release.rs: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Err(error) => {
            eprintln!("build-buck-release.rs: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    /// A git command for a test fixture. Git exports its repository-location
    /// variables to hooks and `git rebase --exec` steps, and each overrides
    /// the directory git was pointed at, so an inherited `GIT_DIR` would send
    /// a fixture's `git init` and commit into the caller's repository
    /// (https://github.com/rrnewton/hermit/issues/3362). The first call also
    /// forgets them for the rest of the test process, so this tool's own
    /// `git` helper, which the tests then run on the fixture, resolves the
    /// fixture by directory too.
    ///
    /// The removal is process-wide rather than per command because the tool's
    /// helpers build their own `Command`s. Two tests depend on it: each fails
    /// under an inherited `GIT_DIR` without the removal (measured 2026-09-29,
    /// 2 of 54): nonignored_untracked_files_dirty_provenance
    /// (`repository_dirty`) and release_candidate_version_probe_refuses_expected_layout_elf_decoy
    /// (the tool's `git rev-parse HEAD:reverie`).
    ///
    /// That happens at the first call, not before the first test. libtest
    /// runs tests on several threads, so under an inherited `GIT_DIR` a test
    /// that runs a helper which follows the caller's environment, without
    /// building a fixture first, sees the variables or not depending on
    /// scheduling. Both tests above build their fixture first, so each makes
    /// the call before its first helper runs.
    fn fixture_git() -> Command {
        const LOCATION: [&str; 8] = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ];
        static FORGET: std::sync::Once = std::sync::Once::new();
        FORGET.call_once(|| {
            for name in LOCATION {
                env::remove_var(name);
            }
        });
        let mut command = Command::new("git");
        for name in LOCATION {
            command.env_remove(name);
        }
        command
    }

    fn fixture_root(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "build-buck-release-{label}-{}",
            unique_identity().unwrap()
        ))
    }

    /// Stub libunwind libraries whose DT_NEEDED sets are exactly the narrow
    /// profile plus the given extra SONAMEs, built here so the closure
    /// contract is tested without depending on the host's libunwind package.
    fn fixture_unwind_closure(
        dir: &Path,
        arch_extra: &[&str],
        core_extra: &[&str],
    ) -> UnwindClosure {
        let build = dir.join("build");
        let deps = build.join("deps");
        fs::create_dir_all(&deps).unwrap();
        let stub = build.join("stub.c");
        fs::write(&stub, b"int hermit_fixture_stub(void) { return 0; }\n").unwrap();
        let shared = |output: &Path, soname: &str, links: &[String]| {
            let mut command = Command::new("gcc");
            command
                .args(["-shared", "-fPIC"])
                .arg(&stub)
                .arg(format!("-Wl,-soname,{soname}"))
                .arg("-Wl,--no-as-needed")
                .args(links)
                .arg("-o")
                .arg(output);
            checked_output(&mut command, &format!("compile fixture {soname}")).unwrap();
        };
        let mut extras = arch_extra.to_vec();
        extras.extend_from_slice(core_extra);
        extras.sort_unstable();
        extras.dedup();
        for soname in extras {
            shared(&deps.join(soname), soname, &[]);
        }
        let with_extras = |mut links: Vec<String>, extra: &[&str]| {
            links.push(format!("-L{}", deps.display()));
            links.extend(extra.iter().map(|soname| format!("-l:{soname}")));
            links
        };
        let core = dir.join(UNWIND_CORE_SONAME);
        shared(
            &core,
            UNWIND_CORE_SONAME,
            &with_extras(vec!["-l:ld-linux-x86-64.so.2".to_owned()], core_extra),
        );
        let arch = dir.join(UNWIND_ARCH_SONAME);
        shared(
            &arch,
            UNWIND_ARCH_SONAME,
            &with_extras(
                vec![
                    format!("-L{}", dir.display()),
                    format!("-l:{UNWIND_CORE_SONAME}"),
                ],
                arch_extra,
            ),
        );
        let object = build.join("stub.o");
        checked_output(
            Command::new("gcc")
                .args(["-c", "-fPIC"])
                .arg(&stub)
                .arg("-o")
                .arg(&object),
            "compile fixture libunwind-ptrace member",
        )
        .unwrap();
        let archive = dir.join(UNWIND_PTRACE_ARCHIVE);
        checked_output(
            Command::new("ar").arg("rcs").arg(&archive).arg(&object),
            "archive fixture libunwind-ptrace",
        )
        .unwrap();
        UnwindClosure {
            ptrace_archive: archive,
            arch_shared: arch,
            core_shared: core,
        }
    }

    fn gzip_member(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn source_root() -> PathBuf {
        Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf()
    }

    /// Write an executable fixture without this parallel test process ever
    /// holding the executed inode open for writing: a sibling test's spawn
    /// inherits any such descriptor and makes the file busy (ETXTBSY). The
    /// bytes go to a never-executed staging file; an `install` child creates
    /// the executable inode (see scripts/lib/exec_safe_fs.rs).
    fn write_executable(path: &Path, contents: &[u8]) {
        let staging = path.with_file_name(format!(
            ".{}.staging.{}",
            path.file_name().unwrap().to_string_lossy(),
            unique_identity().unwrap()
        ));
        fs::write(&staging, contents).unwrap();
        let installed = checked_output(
            Command::new("install")
                .args(["-m", "755", "-T", "--"])
                .arg(&staging)
                .arg(path),
            "install executable fixture",
        );
        fs::remove_file(&staging).unwrap();
        installed.unwrap();
    }

    fn write_complete_install(root: &Path, reverie_sha: &str) {
        fs::create_dir_all(root.join("rsrcs/dynamorio/bin64")).unwrap();
        for (relative, executable) in REQUIRED_RESOURCES {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            if executable {
                write_executable(&path, b"#!/bin/sh\nexit 0\n");
            } else {
                fs::write(&path, format!("fixture:{relative}\n")).unwrap();
            }
        }
        fs::write(
            root.join("rsrcs/sabre.revision"),
            format!("{reverie_sha}\n"),
        )
        .unwrap();
        fs::write(root.join("rsrcs/unprobed-resource"), b"generation-one\n").unwrap();
    }

    fn canonical_report(virtualize_time: bool) -> Value {
        let output = serde_json::json!({
            "exit_code": 0,
            "signal": null,
            "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stdout_bytes": 0,
            "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stderr_bytes": 0
        });
        serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "no_result_reason": null,
            "infrastructure_error": null,
            "comparison": {
                "strictness": "canonical",
                "display_name": "BitwiseInfoV1",
                "compare_logs": true,
                "compare_io_buffers": true,
                "log_scope": "info",
                "record_envelope": "all_records_v1",
                "virtualize_time": virtualize_time,
                "strip_lines": false,
                "canonicalize_addresses": true,
                "full_trace": true,
                "exact_remainder": true,
                "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                "ignore_lines": false,
                "skip_commit": false,
                "skip_detlog": false
            },
            "compared_log_messages": {"left": 12, "right": 12},
            "compared_outputs": {"left": output, "right": output},
            "guest_exit_code": 0,
            "guest_signal": null,
            "first_divergent_scheduler_turn": null,
            "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null,
            "first_divergent_syscall": null,
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        })
    }

    fn write_report(root: &Path, name: &str, value: &Value) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    fn build_info_json() -> String {
        serde_json::json!({
            "schema": BuildInfo::SCHEMA,
            "version": "0.2.0",
            "build_date": "2026-09-22",
            "git_sha": "0123456789ab",
            "features": {"dbt": true, "e9patch": true, "sabre": true}
        })
        .to_string()
    }

    fn host_capabilities_json(cpuid_evidence: &str) -> String {
        serde_json::json!({
            "schema": HostCapabilitiesReport::SCHEMA,
            "host_capabilities": {
                "cpuid-faulting": {"present": true, "evidence": cpuid_evidence},
                "kvm": {"present": false, "evidence": "open /dev/kvm = ENOENT"}
            }
        })
        .to_string()
    }

    fn fixture_receipt_facts(identity: &str) -> FinalReceiptFacts {
        let hash = "a".repeat(64);
        FinalReceiptFacts {
            result: "pass".to_owned(),
            evidence_identity: identity.to_owned(),
            process_tree_supervisor: "dagrun-cgroup-v2".to_owned(),
            matrix_step_wall_seconds: MATRIX_STEP_DEADLINE_SECONDS,
            matrix_outer_wall_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS,
            matrix_cleanup_evidence_grace_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS
                - MATRIX_STEP_DEADLINE_SECONDS,
            process_tree_cleanup: "complete".to_owned(),
            hermit_full_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            hermit_build_sha: "0123456789ab".to_owned(),
            reverie_sha: "123456789abcdef0123456789abcdef012345678".to_owned(),
            version: "0.2.0".to_owned(),
            build_date: "2026-09-22".to_owned(),
            dotslash_version: DOTSLASH_VERSION.to_owned(),
            dotslash_sha256: hash.clone(),
            buck_version: "buck2 fixture".to_owned(),
            buck_descriptor_sha256: hash.clone(),
            buck_executable_sha256: hash.clone(),
            lzma_input: "/fixture/liblzma.so".to_owned(),
            lzma_input_sha256: hash.clone(),
            unwind_input_identity: hash.clone(),
            unwind_ptrace_sha256: hash.clone(),
            unwind_arch_sha256: hash.clone(),
            unwind_core_sha256: hash.clone(),
            generated_buck_sha256: hash.clone(),
            safehermit_sha256: hash.clone(),
            bounded_run_space_sha256: hash.clone(),
            cargo_candidate_sha256: hash.clone(),
            cargo_hermit_manifest_sha256: hash.clone(),
            cargo_resources_manifest_sha256: hash.clone(),
            buck_candidate_sha256: hash.clone(),
            buck_hermit_manifest_sha256: hash.clone(),
            buck_resources_manifest_sha256: hash,
            event_log: "buck-build.json-lines.gz".to_owned(),
            ptrace_verify_messages_left: 12,
            ptrace_verify_messages_right: 12,
            ptrace_record_messages_left: 9,
            ptrace_record_messages_right: 9,
            cargo_matrix_rows: 3,
            buck_matrix_rows: 3,
            matrix_candidate_manifest_sha256: "c".repeat(64),
            matrix_candidate_manifest: "matrix-candidate-invocations.json".to_owned(),
            matrix_expected_ledger: "matrix-expected-ledger.json".to_owned(),
            matrix_expected_ledger_sha256: "d".repeat(64),
            matrix_expected_invocation_count: 2,
            cargo_matrix_candidate_invocations: 2,
            buck_matrix_candidate_invocations: 2,
        }
    }

    fn fixture_resources() -> BTreeMap<String, String> {
        BTreeMap::from([("rsrcs/fixture".to_owned(), "b".repeat(64))])
    }

    fn write_retained_candidate_fixture(root: &Path) -> BuildInfo {
        let version = build_info_json();
        let help = "ptrace dbt liteinst sabre kvm e9patch\n";
        let host = host_capabilities_json("arch prctl = ok");
        for (name, contents) in [
            ("cargo-version.json", version.as_str()),
            ("buck-version.json", version.as_str()),
            ("cargo-help.txt", help),
            ("buck-help.txt", help),
            ("cargo-host-capabilities.json", host.as_str()),
            ("buck-host-capabilities.json", host.as_str()),
        ] {
            fs::write(root.join(name), contents).unwrap();
        }
        let run = clock_runtime_report(CAPTURED_VIRTUAL_NS);
        let record = canonical_report(false);
        for name in ["cargo-ptrace-verify.json", "buck-ptrace-verify.json"] {
            write_report(root, name, &run);
        }
        for name in [
            "cargo-ptrace-clock-trajectory.txt",
            "buck-ptrace-clock-trajectory.txt",
        ] {
            fs::write(root.join(name), CAPTURED_CLOCK_TRAJECTORY).unwrap();
        }
        for name in [
            "cargo-ptrace-record-verify.json",
            "buck-ptrace-record-verify.json",
        ] {
            write_report(root, name, &record);
        }
        decode_build_info(&version, "0.2.0", "0123456789ab", "fixture build info").unwrap()
    }

    fn write_fixture_receipt(
        root: &Path,
        facts: &FinalReceiptFacts,
        resources: &BTreeMap<String, String>,
    ) -> PathBuf {
        let receipt = root.join("shadow-parity.json");
        let mut rows = Vec::new();
        collect_evidence_hashes(root, root, &receipt, &mut rows).unwrap();
        let artifacts = rows.into_iter().collect::<BTreeMap<_, _>>();
        atomic_write_new(
            &receipt,
            &render_final_receipt(facts, resources, &artifacts).unwrap(),
        )
        .unwrap();
        receipt
    }

    fn verify_fixture_receipt(
        root: &Path,
        receipt: &Path,
        facts: &FinalReceiptFacts,
        resources: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let parsed = parse_final_receipt(
            &fs::read_to_string(receipt)
                .map_err(|error| format!("fixture receipt is unreadable: {error}"))?,
        )?;
        let mut rows = Vec::new();
        collect_evidence_hashes(root, root, receipt, &mut rows)?;
        let artifacts = rows.into_iter().collect::<BTreeMap<_, _>>();
        verify_receipt_semantics(&parsed, facts, resources, &artifacts)
    }

    #[test]
    fn both_help_forms_are_accepted() {
        assert_eq!(parse_options(["-h".to_owned()]).unwrap(), None);
        assert_eq!(parse_options(["--help".to_owned()]).unwrap(), None);
    }

    #[test]
    fn missing_required_inputs_are_refused() {
        let error = parse_options(Vec::<String>::new()).unwrap_err();
        assert!(error.contains("--dotslash"));
    }

    #[test]
    fn authoritative_output_override_is_not_a_supported_surface() {
        let error =
            parse_options(["--output-root".to_owned(), "target/ci".to_owned()]).unwrap_err();
        assert!(error.contains("unknown argument"));
    }

    #[test]
    fn parses_only_the_package_version() {
        let manifest = "[workspace.package]\nversion = \"9\"\n[package]\nname = \"hermit\"\nversion = \"0.2.0-test+1\"\n";
        assert_eq!(package_version(manifest).unwrap(), "0.2.0-test+1");
        assert!(package_version("[package]\nversion = \"unknown\"\n").is_err());
    }

    #[test]
    fn build_info_uses_closed_producer_schema_and_exact_semantics() {
        let good = build_info_json();
        decode_build_info(&good, "0.2.0", "0123456789ab", "fixture").unwrap();
        for (name, value) in [
            ("malformed", "{".to_owned()),
            ("unknown-root", good.replacen("{", r#"{"future":true,"#, 1)),
            (
                "unknown-feature",
                good.replacen(r#""dbt":true"#, r#""dbt":true,"future":true"#, 1),
            ),
            (
                "duplicate-root",
                good.replacen(r#""schema":1"#, r#""schema":1,"schema":1"#, 1),
            ),
            (
                "duplicate-feature",
                good.replacen(r#""dbt":true"#, r#""dbt":true,"dbt":true"#, 1),
            ),
            (
                "wrong-schema",
                good.replacen(r#""schema":1"#, r#""schema":2"#, 1),
            ),
            ("internal-whitespace", good.replacen("0.2.0", "0.2. 0", 1)),
            ("bad-date", good.replacen("2026-09-22", "2026-02-30", 1)),
            ("bad-sha", good.replacen("0123456789ab", "0123456789AB", 1)),
            (
                "feature-disabled",
                good.replacen(r#""sabre":true"#, r#""sabre":false"#, 1),
            ),
        ] {
            assert!(
                decode_build_info(&value, "0.2.0", "0123456789ab", "fixture").is_err(),
                "accepted {name}: {value}"
            );
        }
    }

    #[test]
    fn host_capabilities_use_closed_typed_schema_and_preserve_string_whitespace() {
        let spaced = host_capabilities_json("arch prctl = ok");
        let compact = host_capabilities_json("archprctl=ok");
        let spaced_report = decode_host_capabilities(&spaced, "spaced").unwrap();
        let compact_report = decode_host_capabilities(&compact, "compact").unwrap();
        assert_ne!(spaced_report, compact_report);

        for (name, value) in [
            ("malformed", "{".to_owned()),
            (
                "unknown-root",
                spaced.replacen("{", r#"{"future":true,"#, 1),
            ),
            (
                "unknown-verdict",
                spaced.replacen(
                    r#""present":true"#,
                    r#""present":true,"future":true"#,
                    1,
                ),
            ),
            (
                "duplicate-capability",
                spaced.replacen(
                    r#""kvm":{"evidence":"open /dev/kvm = ENOENT","present":false}"#,
                    r#""kvm":{"evidence":"open /dev/kvm = ENOENT","present":false},"kvm":{"evidence":"duplicate","present":true}"#,
                    1,
                ),
            ),
            (
                "empty-evidence",
                host_capabilities_json("   "),
            ),
            (
                "wrong-schema",
                spaced.replacen(r#""schema":1"#, r#""schema":2"#, 1),
            ),
        ] {
            assert!(
                decode_host_capabilities(&value, "fixture").is_err(),
                "accepted {name}: {value}"
            );
        }
    }

    #[test]
    fn shadow_dbt_envelope_is_the_reviewed_host_contract() {
        let envelope = reviewed_dbt_matrix_envelope();
        assert!(envelope.hint.resources.is_empty());
        assert_eq!(envelope.hint.est_duration_s, 180.0);
        assert_eq!(envelope.hint.rss_baseline_bytes, Some(512 * 1024 * 1024));
        assert_eq!(envelope.hint.rss_baseline_inner_jobs, None);
        assert_eq!(
            envelope.hint.hard_mem_max_bytes,
            Some(2_i64 * 1024 * 1024 * 1024)
        );
        assert_eq!(envelope.hint.classification, StepClass::LatencyBound);
        assert_eq!(envelope.hint.preferred_inner_jobs, None);
        assert_eq!(envelope.hint.measured_effective_cores, None);
        assert_eq!(envelope.hint.measured_cpu_utilization, None);
        assert!(matches!(envelope.cmdtype, CmdType::Unknown));
        assert!(!envelope.networkonly);
        assert!(!envelope.engine_only);
        assert_eq!(envelope.timeout, 900);
        assert_eq!(envelope.cpu_timeout, 7_200);
        assert_eq!(envelope.jobs_flag, None);
        assert_eq!(envelope.jobs_env, None);
        assert_eq!(
            envelope.matrix_flags,
            [
                "--backend",
                "dbt",
                "--strict",
                "--require-backend",
                "--no-parent-scorecard"
            ]
        );
        assert!(envelope.environment.is_empty());

        let step = dbt_matrix_step(
            &envelope,
            "buck",
            "deadline fixture",
            "true".to_owned(),
            BTreeMap::new(),
        );
        assert_eq!(step.timeout, envelope.timeout);
        assert_eq!(step.cpu_timeout, envelope.cpu_timeout);
        let predicate_config = DagConfig {
            steps: vec![step.clone()],
            ..Default::default()
        };
        assert!(
            steps_violating_run_timeout(&predicate_config, MATRIX_SUPERVISOR_DEADLINE_SECONDS)
                .is_empty()
        );
        assert_eq!(
            steps_violating_run_timeout(&predicate_config, MATRIX_STEP_DEADLINE_SECONDS),
            vec![(
                "buck_phase1.buck_dbt_matrix".to_owned(),
                MATRIX_STEP_DEADLINE_SECONDS
            )]
        );
        assert_eq!(
            steps_violating_run_timeout(&predicate_config, MATRIX_STEP_DEADLINE_SECONDS - 1),
            vec![(
                "buck_phase1.buck_dbt_matrix".to_owned(),
                MATRIX_STEP_DEADLINE_SECONDS
            )]
        );

        let marker_root = fixture_root("matrix-deadline-refusal");
        fs::create_dir(&marker_root).unwrap();
        let marker = marker_root.join("launched");
        let mut refused_step = step;
        refused_step.cmd = format!("touch {}", shell_words::quote(&marker.to_string_lossy()));
        let refused_config = DagConfig {
            steps: vec![refused_step],
            ..Default::default()
        };
        let refused = run_dag_boxed_deadline(
            &refused_config,
            1,
            false,
            0,
            None,
            None,
            Some(1),
            Some(MATRIX_STEP_DEADLINE_SECONDS),
        );
        assert!(!refused.ok);
        assert!(!refused.run_timed_out);
        assert!(refused.outcomes.is_empty());
        assert!(
            !marker.exists(),
            "equal outer deadline launched the matrix step"
        );
        fs::remove_dir_all(marker_root).unwrap();

        let bind = |envelope: &MatrixExecutionEnvelope| {
            matrix_step_environment(
                envelope,
                Path::new("/bounded/safehermit"),
                Path::new("/candidate/hermit"),
                Path::new("/candidate/install"),
                Path::new("/evidence/invocations"),
                "buck",
            )
        };
        let environment = bind(&envelope).unwrap();
        let mut expected = BTreeMap::new();
        for (key, value) in [
            (MATRIX_PROXY_ENV, "1"),
            ("HERMIT_BUCK_PHASE1_SAFEHERMIT", "/bounded/safehermit"),
            ("HERMIT_BUCK_PHASE1_CANDIDATE", "/candidate/hermit"),
            ("HERMIT_BUCK_PHASE1_INSTALL", "/candidate/install"),
            (
                "HERMIT_BUCK_PHASE1_MATRIX_EVIDENCE",
                "/evidence/invocations",
            ),
            ("HERMIT_BUCK_PHASE1_CANDIDATE_LABEL", "buck"),
            ("HERMIT_INSTALL_DIR", "/candidate/install"),
        ] {
            expected.insert(key.to_owned(), value.to_owned());
        }
        assert_eq!(environment, expected);
        let mut pinned_request = envelope.clone();
        pinned_request
            .environment
            .insert(PINNED_ROOT_WORKDIR_ENV.to_owned(), "/test".to_owned());
        assert!(
            bind(&pinned_request)
                .unwrap_err()
                .contains("cannot satisfy")
        );
        for key in [
            MATRIX_PROXY_ENV,
            "HERMIT_BUCK_PHASE1_CANDIDATE",
            "HERMIT_INSTALL_DIR",
        ] {
            let mut redirecting = envelope.clone();
            redirecting
                .environment
                .insert(key.to_owned(), "/elsewhere".to_owned());
            assert!(
                bind(&redirecting).unwrap_err().contains("collides"),
                "{key}"
            );
        }
    }

    #[test]
    fn stale_generated_graph_is_refused() {
        assert!(verify_generated_buck("# stale\n").is_err());
    }

    #[test]
    fn unreadable_event_log_shape_is_refused() {
        let path = env::temp_dir().join(format!("buck-log-refusal-{}", std::process::id()));
        fs::write(&path, b"not gzip").unwrap();
        assert!(validate_log_header(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn release_evidence_reconciler_uses_stdout_output_and_treats_command_flag_as_advisory() {
        let root = fixture_root("release-reconciliation");
        fs::create_dir(&root).unwrap();
        let binary = root
            .join("buck-out/v2/gen/root/0123456789abcdef/hermit-cli/___hermit-release__/hermit");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::copy("/bin/true", &binary).unwrap();
        let stdout = root.join("show-output.txt");
        fs::write(
            &stdout,
            format!("hermit//hermit-cli:hermit-release {}\n", binary.display()),
        )
        .unwrap();
        let descriptor = root.join("bootstrap/buck2");
        fs::create_dir_all(descriptor.parent().unwrap()).unwrap();
        fs::copy(source_root().join("bootstrap/buck2"), &descriptor).unwrap();
        let write_event_log = |path: &Path, events: &[Value]| {
            let text = events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            let mut gzip = Command::new("gzip")
                .arg("-c")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            gzip.stdin
                .as_mut()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
            let gzip_output = gzip.wait_with_output().unwrap();
            assert!(gzip_output.status.success());
            fs::write(path, gzip_output.stdout).unwrap();
        };
        let events = [
            serde_json::json!({"Event":{"data":{"SpanEnd":{"data":{"Command":{"is_success":false,"build_result":{"build_completed":true}}}}}}}),
            serde_json::json!({"Result":{"result":{"build_response":{"errors":[],"build_targets":[{"target":"hermit//hermit-cli:hermit-release","outputs":[]}]}}}}),
        ];
        let event_log = root.join("event.json-lines.gz");
        write_event_log(&event_log, &events);
        let receipt = root.join("reconciliation.tsv");
        reconcile_release_build_evidence(&event_log, &stdout, 0, &root, &receipt).unwrap();
        let text = fs::read_to_string(&receipt).unwrap();
        assert!(text.contains("command_end_is_success\tfalse\n"));
        assert!(text.contains("command_end_status\tadvisory-known-inconsistent\n"));
        assert!(text.contains(&sha256(&binary).unwrap()));
        assert!(text.contains(
            "action_executions\tlocal=0,simple=0,deferred=0,local_dep_file=0,local_worker=0,local_action_cache=0\nremote_action_executions\t0\n"
        ));

        let elf_bytes = fs::read(&binary).unwrap();
        fs::write(&binary, b"not an ELF binary\n").unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &stdout,
                0,
                &root,
                &root.join("non-elf.tsv"),
            )
            .is_err()
        );
        fs::write(&binary, elf_bytes).unwrap();

        let duplicate_stdout = root.join("duplicate-output.txt");
        fs::write(
            &duplicate_stdout,
            format!(
                "hermit//hermit-cli:hermit-release {}\nhermit//hermit-cli:hermit-release {}\n",
                binary.display(),
                binary.display()
            ),
        )
        .unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &duplicate_stdout,
                0,
                &root,
                &root.join("duplicate.tsv"),
            )
            .is_err()
        );
        let decoy = root.join("buck-out/decoy/hermit");
        fs::create_dir_all(decoy.parent().unwrap()).unwrap();
        fs::copy("/bin/true", &decoy).unwrap();
        let decoy_stdout = root.join("decoy-output.txt");
        fs::write(
            &decoy_stdout,
            format!("{RELEASE_SHOW_OUTPUT_TARGET} {}\n", decoy.display()),
        )
        .unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &decoy_stdout,
                0,
                &root,
                &root.join("decoy.tsv"),
            )
            .is_err()
        );
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &stdout,
                3,
                &root,
                &root.join("shell-failure.tsv"),
            )
            .is_err()
        );
        let outside_stdout = root.join("outside-output.txt");
        fs::write(
            &outside_stdout,
            "hermit//hermit-cli:hermit-release /bin/true\n",
        )
        .unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &outside_stdout,
                0,
                &root,
                &root.join("outside.tsv"),
            )
            .is_err()
        );
        let extra_target_log = root.join("extra-target.json-lines.gz");
        let extra_target_events = [
            events[0].clone(),
            serde_json::json!({"Result":{"result":{"build_response":{"errors":[],"build_targets":[{"target":"hermit//hermit-cli:hermit-release","outputs":[]},{"target":"hermit//other:unexpected","outputs":[]}]}}}}),
        ];
        write_event_log(&extra_target_log, &extra_target_events);
        assert!(
            reconcile_release_build_evidence(
                &extra_target_log,
                &stdout,
                0,
                &root,
                &root.join("extra-target.tsv"),
            )
            .is_err()
        );
        // Each remaining canonical fact refuses on its own, with the flag
        // that is advisory held at its measured value.
        for (label, command, result, refusal) in [
            (
                "incomplete",
                serde_json::json!({"is_success":false,"build_result":{"build_completed":false}}),
                events[1].clone(),
                "does not prove build completion",
            ),
            (
                "errors",
                events[0]["Event"]["data"]["SpanEnd"]["data"]["Command"].clone(),
                serde_json::json!({"Result":{"result":{"build_response":{"errors":[{"message":"action failed"}],"build_targets":[{"target":"hermit//hermit-cli:hermit-release","outputs":[]}]}}}}),
                "contains 1 build errors",
            ),
        ] {
            let log = root.join(format!("{label}.json-lines.gz"));
            write_event_log(
                &log,
                &[
                    serde_json::json!({"Event":{"data":{"SpanEnd":{"data":{"Command":command}}}}}),
                    result,
                ],
            );
            let error = reconcile_release_build_evidence(
                &log,
                &stdout,
                0,
                &root,
                &root.join(format!("{label}.tsv")),
            )
            .unwrap_err();
            assert!(error.contains(refusal), "{label}: {error}");
        }
        // Executed actions are counted by kind, and a remote, remotely cached
        // or unknown kind refuses the log as evidence of a local build.
        let action = |kind: Value| serde_json::json!({"Event":{"data":{"SpanEnd":{"data":{"ActionExecution":{"execution_kind":kind}}}}}});
        let counted_log = root.join("counted.json-lines.gz");
        write_event_log(
            &counted_log,
            &[
                action(1.into()),
                action(4.into()),
                action(4.into()),
                action(10.into()),
                events[0].clone(),
                events[1].clone(),
            ],
        );
        let counted = root.join("counted.tsv");
        reconcile_release_build_evidence(&counted_log, &stdout, 0, &root, &counted).unwrap();
        assert!(fs::read_to_string(&counted).unwrap().contains(
            "action_executions\tlocal=1,simple=2,deferred=0,local_dep_file=0,local_worker=0,local_action_cache=1\n"
        ));
        for (label, kind, refusal) in [
            (
                "remote",
                Value::from(2),
                "execution_kind 2, which is not a local kind",
            ),
            (
                "action-cache",
                Value::from(3),
                "execution_kind 3, which is not a local kind",
            ),
            (
                "remote-dep-file",
                Value::from(9),
                "execution_kind 9, which is not a local kind",
            ),
            (
                "remote-worker",
                Value::from(11),
                "execution_kind 11, which is not a local kind",
            ),
            (
                "unknown",
                Value::from(12),
                "execution_kind 12, which is not a local kind",
            ),
            ("missing", Value::Null, "without an integer execution_kind"),
            (
                "named",
                Value::from("LOCAL"),
                "without an integer execution_kind",
            ),
        ] {
            let log = root.join(format!("{label}-action.json-lines.gz"));
            write_event_log(
                &log,
                &[
                    action(1.into()),
                    action(kind),
                    events[0].clone(),
                    events[1].clone(),
                ],
            );
            let error = reconcile_release_build_evidence(
                &log,
                &stdout,
                0,
                &root,
                &root.join(format!("{label}-action.tsv")),
            )
            .unwrap_err();
            assert!(error.contains(refusal), "{label}: {error}");
        }
        // The advisory reading of is_success=false is bound to the measured
        // Buck: under any other descriptor the same log is refused, while a
        // true flag needs no such measurement.
        fs::write(&descriptor, b"#!/usr/bin/env dotslash\n{}\n").unwrap();
        let error = reconcile_release_build_evidence(
            &event_log,
            &stdout,
            0,
            &root,
            &root.join("other-buck.tsv"),
        )
        .unwrap_err();
        assert!(error.contains("re-measure the flag"), "{error}");
        let success_log = root.join("success.json-lines.gz");
        write_event_log(
            &success_log,
            &[
                serde_json::json!({"Event":{"data":{"SpanEnd":{"data":{"Command":{"is_success":true,"build_result":{"build_completed":true}}}}}}}),
                events[1].clone(),
            ],
        );
        let success_receipt = root.join("success.tsv");
        reconcile_release_build_evidence(&success_log, &stdout, 0, &root, &success_receipt)
            .unwrap();
        assert!(
            fs::read_to_string(&success_receipt)
                .unwrap()
                .contains("command_end_is_success\ttrue\n")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_candidate_version_probe_refuses_expected_layout_elf_decoy() {
        let root = fixture_root("release-candidate-version");
        fs::create_dir_all(root.join("hermit-cli")).unwrap();
        fs::create_dir_all(root.join("reverie")).unwrap();
        fs::write(
            root.join("hermit-cli/Cargo.toml"),
            "[package]\nname = \"hermit\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        fs::write(root.join("reverie/pin"), b"fixture\n").unwrap();
        checked_output(
            fixture_git().current_dir(&root).args(["init", "-q"]),
            "git init",
        )
        .unwrap();
        checked_output(
            fixture_git().current_dir(&root).args(["add", "."]),
            "git add fixture",
        )
        .unwrap();
        checked_output(
            fixture_git().current_dir(&root).args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ]),
            "git commit fixture",
        )
        .unwrap();
        let hermit_sha = git(&root, &["rev-parse", "--short=12", "HEAD"]).unwrap();
        let reverie_sha = git(&root, &["rev-parse", "HEAD:reverie"]).unwrap();
        let candidate =
            root.join("buck-out/v2/gen/root/config-hash/hermit-cli/___hermit-release__/hermit");
        fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        let source = root.join("candidate.c");
        fs::write(
            &source,
            format!(
                "#include <stdio.h>\nint main(void) {{ puts(\"{{\\\"schema\\\":1,\\\"version\\\":\\\"0.2.0\\\",\\\"build_date\\\":\\\"2026-09-23\\\",\\\"git_sha\\\":\\\"{hermit_sha}\\\",\\\"features\\\":{{\\\"dbt\\\":true,\\\"e9patch\\\":true,\\\"sabre\\\":true}}}}\"); return 0; }}\n"
            ),
        )
        .unwrap();
        checked_output(
            Command::new("gcc").args([source.as_os_str(), OsStr::new("-o"), candidate.as_os_str()]),
            "compile release candidate fixture",
        )
        .unwrap();
        let show_output = root.join("build.stdout");
        fs::write(
            &show_output,
            format!("{RELEASE_SHOW_OUTPUT_TARGET} {}\n", candidate.display()),
        )
        .unwrap();
        let metadata = root.join("metadata.tsv");
        fs::write(
            &metadata,
            format!(
                "version\t0.2.0\nbuild_date\t2026-09-23\nhermit_sha\t{hermit_sha}\nreverie_sha\t{reverie_sha}\n"
            ),
        )
        .unwrap();
        validate_release_candidate_version(
            &show_output,
            &metadata,
            &root,
            &root.join("candidate-version.json"),
            &root.join("candidate-validation.tsv"),
        )
        .unwrap();
        write_executable(&candidate, &fs::read("/bin/true").unwrap());
        assert!(
            validate_release_candidate_version(
                &show_output,
                &metadata,
                &root,
                &root.join("decoy-version.json"),
                &root.join("decoy-validation.tsv"),
            )
            .is_err()
        );
        // A candidate that prints the right version but is not the file that
        // was hashed before it ran must not be receipted: this one replaces
        // its own path with another executable while it runs.
        let replacement = root.join("replacement");
        write_executable(&replacement, &fs::read("/bin/true").unwrap());
        let rewriting = root.join("rewriting.c");
        fs::write(
            &rewriting,
            format!(
                "#include <stdio.h>\nint main(int argc, char **argv) {{ (void)argc; if (rename(\"{}\", argv[0]) != 0) return 1; puts(\"{{\\\"schema\\\":1,\\\"version\\\":\\\"0.2.0\\\",\\\"build_date\\\":\\\"2026-09-23\\\",\\\"git_sha\\\":\\\"{hermit_sha}\\\",\\\"features\\\":{{\\\"dbt\\\":true,\\\"e9patch\\\":true,\\\"sabre\\\":true}}}}\"); return 0; }}\n",
                replacement.display()
            ),
        )
        .unwrap();
        checked_output(
            Command::new("gcc").args([
                rewriting.as_os_str(),
                OsStr::new("-o"),
                candidate.as_os_str(),
            ]),
            "compile rewriting release candidate fixture",
        )
        .unwrap();
        let receipt = root.join("rewritten-validation.tsv");
        assert_eq!(
            validate_release_candidate_version(
                &show_output,
                &metadata,
                &root,
                &root.join("rewritten-version.json"),
                &receipt,
            ),
            Err("release candidate changed while its version was probed".to_owned())
        );
        assert!(!receipt.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn combined_scan_package_is_deterministic_and_refuses_coherent_source_swap() {
        let fixture = fixture_root("combined-scan-package");
        fs::create_dir(&fixture).unwrap();
        let gzip_bytes = {
            let mut gzip = Command::new("gzip")
                .args(["-n", "-c"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            gzip.stdin
                .take()
                .unwrap()
                .write_all(b"{\"public\":true}\n")
                .unwrap();
            let output = gzip.wait_with_output().unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let mut archives = Vec::new();
        for suffix in ["one", "two"] {
            let root = fixture.join(format!("evidence-{suffix}"));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("plain.txt"), b"benign public evidence\n").unwrap();
            fs::write(root.join("event.json-lines.gz"), &gzip_bytes).unwrap();
            let archive = fixture.join(format!("evidence-{suffix}.tar.gz"));
            let receipt = fixture.join(format!("evidence-{suffix}-scan.tsv"));
            scan_and_package_public_evidence(&root, &archive, &receipt).unwrap();
            assert_eq!(
                fs::metadata(&archive).unwrap().permissions().mode() & 0o222,
                0
            );
            assert!(
                fs::read_to_string(&receipt)
                    .unwrap()
                    .contains("known_credential_marker_lines\t0\n")
            );
            archives.push(fs::read(&archive).unwrap());
        }
        assert_eq!(archives[0], archives[1]);
        let mut archived_names = Archive::new(GzDecoder::new(Cursor::new(&archives[0])))
            .entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        archived_names.sort();
        assert_eq!(
            archived_names,
            [
                "SHA256SUMS",
                "event.json-lines.gz",
                "plain.txt",
                "public-evidence-scan.tsv",
            ]
        );

        let root = fixture.join("mutated-evidence");
        fs::create_dir(&root).unwrap();
        let source = root.join("plain.txt");
        fs::write(&source, b"benign public evidence\n").unwrap();
        let archive = fixture.join("mutated.tar.gz");
        let receipt = fixture.join("mutated-scan.tsv");
        let result = scan_and_package_public_evidence_with_hook(
            &root,
            &archive,
            &receipt,
            |captured_root| {
                let original =
                    fs::read(captured_root.join("plain.txt")).map_err(|error| error.to_string())?;
                fs::write(
                    captured_root.join("plain.txt"),
                    b"Authorization: Bearer swapped-secret\n",
                )
                .map_err(|error| error.to_string())?;
                fs::write(
                    captured_root.join("public-evidence-scan.tsv"),
                    b"known_credential_marker_lines\t0\n",
                )
                .map_err(|error| error.to_string())?;
                fs::write(
                    captured_root.join("SHA256SUMS"),
                    b"coherently-rewritten-old-manifest\n",
                )
                .map_err(|error| error.to_string())?;
                fs::write(captured_root.join("plain.txt"), original)
                    .map_err(|error| error.to_string())?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!archive.exists());
        assert!(!receipt.exists());

        let poisoned = fixture.join("poisoned-evidence");
        fs::create_dir(&poisoned).unwrap();
        fs::write(
            poisoned.join("plain.txt"),
            b"Authorization: Bearer original-secret\n",
        )
        .unwrap();
        assert!(
            scan_and_package_public_evidence(
                &poisoned,
                &fixture.join("poisoned.tar.gz"),
                &fixture.join("poisoned-scan.tsv"),
            )
            .is_err()
        );

        let unsafe_names = fixture.join("unsafe-names");
        fs::create_dir(&unsafe_names).unwrap();
        fs::write(
            unsafe_names.join(std::ffi::OsString::from_vec(vec![b'b', 0xff])),
            b"benign bytes\n",
        )
        .unwrap();
        assert!(
            scan_and_package_public_evidence(
                &unsafe_names,
                &fixture.join("unsafe.tar.gz"),
                &fixture.join("unsafe-scan.tsv"),
            )
            .is_err()
        );

        let partial_root = fixture.join("partial-publication-evidence");
        fs::create_dir(&partial_root).unwrap();
        fs::write(partial_root.join("plain.txt"), b"benign\n").unwrap();
        let partial_archive = fixture.join("partial.tar.gz");
        let stale_receipt = fixture.join("stale-receipt.tsv");
        fs::write(&stale_receipt, b"stale\n").unwrap();
        assert!(
            scan_and_package_public_evidence(&partial_root, &partial_archive, &stale_receipt)
                .is_err()
        );

        let aliased_parent = fixture.join("output-alias");
        symlink(&fixture, &aliased_parent).unwrap();
        assert!(
            scan_and_package_public_evidence(
                &partial_root,
                &aliased_parent.join("aliased.tar.gz"),
                &aliased_parent.join("aliased-scan.tsv"),
            )
            .is_err()
        );
        assert!(!partial_archive.exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn gzip_evidence_decode_is_bounded_and_scans_large_multimember_payloads() {
        let fixture = fixture_root("bounded-gzip-evidence");
        fs::create_dir(&fixture).unwrap();

        let large = vec![b'x'; 4 * 1024 * 1024 + 17];
        let split = large.len() / 2;
        let mut multimember = gzip_member(&large[..split]);
        multimember.extend(gzip_member(&large[split..]));
        assert_eq!(decode_gzip_bytes(&multimember).unwrap(), large);

        let safe_root = fixture.join("safe-large");
        fs::create_dir(&safe_root).unwrap();
        fs::write(safe_root.join("event.json-lines.gz"), &multimember).unwrap();
        scan_and_package_public_evidence(
            &safe_root,
            &fixture.join("safe-large.tar.gz"),
            &fixture.join("safe-large-scan.tsv"),
        )
        .unwrap();

        let mut late_marker = vec![b'x'; 4 * 1024 * 1024];
        late_marker.extend(b"\nAuthorization: Bearer late-secret\n");
        let poisoned_root = fixture.join("late-marker");
        fs::create_dir(&poisoned_root).unwrap();
        fs::write(
            poisoned_root.join("event.json-lines.gz"),
            gzip_member(&late_marker),
        )
        .unwrap();
        let poisoned_archive = fixture.join("late-marker.tar.gz");
        let poisoned_receipt = fixture.join("late-marker-scan.tsv");
        assert!(
            scan_and_package_public_evidence(&poisoned_root, &poisoned_archive, &poisoned_receipt,)
                .is_err()
        );
        assert!(!poisoned_archive.exists());
        assert!(!poisoned_receipt.exists());

        let valid = gzip_member(b"public evidence\n");
        let mut corrupted = valid.clone();
        *corrupted.last_mut().unwrap() ^= 0xff;
        assert!(decode_gzip_bytes(&corrupted).is_err());
        let mut truncated = valid;
        truncated.truncate(truncated.len() - 3);
        assert!(decode_gzip_bytes(&truncated).is_err());

        let compressed_bomb = gzip_member(&vec![0; 256 * 1024]);
        let error = decode_gzip_bytes_with_limit(&compressed_bomb, 64 * 1024).unwrap_err();
        assert!(error.contains("decode limit"), "unexpected error: {error}");

        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn combined_scan_stage_preserves_legacy_payload_and_refuses_source_swap() {
        let fixture = fixture_root("combined-scan-stage");
        let evidence = fixture.join("evidence");
        fs::create_dir_all(&evidence).unwrap();
        let source = evidence.join("buck2-third-party.log");
        fs::write(&source, b"legacy diagnostic log\n").unwrap();
        let staging = fixture.join("staging");
        scan_and_stage_public_evidence(&source, &staging, "buck2-third-party.log").unwrap();
        let mut population = fs::read_dir(&staging)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        population.sort();
        assert_eq!(population, ["SHA256SUMS", "buck2-third-party.log"]);
        assert_eq!(
            fs::read(staging.join("buck2-third-party.log")).unwrap(),
            b"legacy diagnostic log\n"
        );
        assert_eq!(
            fs::read_to_string(staging.join("SHA256SUMS")).unwrap(),
            format!("{}  buck2-third-party.log\n", sha256(&source).unwrap())
        );
        assert_eq!(
            fs::metadata(&staging).unwrap().permissions().mode() & 0o222,
            0
        );

        let mut staging_permissions = fs::metadata(&staging).unwrap().permissions();
        staging_permissions.set_mode(0o755);
        fs::set_permissions(&staging, staging_permissions).unwrap();
        fs::remove_dir_all(&staging).unwrap();
        let result = scan_and_stage_public_evidence_with_hook(
            &source,
            &staging,
            "buck2-third-party.log",
            |source| {
                let original = fs::read(source).map_err(|error| error.to_string())?;
                fs::write(source, b"client_secret=swapped-secret\n")
                    .map_err(|error| error.to_string())?;
                let parent = source.parent().unwrap();
                fs::write(parent.join("public-evidence-scan.tsv"), b"old receipt\n")
                    .map_err(|error| error.to_string())?;
                fs::write(parent.join("SHA256SUMS"), b"old coherent manifest\n")
                    .map_err(|error| error.to_string())?;
                fs::write(source, original).map_err(|error| error.to_string())?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!staging.exists());
        assert!(
            scan_and_stage_public_evidence(&source, &fixture.join("relative-reject"), "../log")
                .is_err()
        );
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn combined_package_retains_non_green_evidence_before_verdict() {
        let fixture = fixture_root("combined-non-green-release");
        let evidence = fixture.join("evidence");
        fs::create_dir_all(&evidence).unwrap();
        fs::write(evidence.join("shell-exit.tsv"), b"shell_exit\t124\n").unwrap();
        let archive = fixture.join("release.tar.gz");
        let receipt = fixture.join("release-scan.tsv");
        scan_and_package_public_evidence(&evidence, &archive, &receipt).unwrap();
        assert!(archive.is_file());
        assert!(release_evidence_verdict(1, 2, 124).is_err());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn mutation_during_real_publisher_copy_is_refused() {
        let fixture = fixture_root("publisher-race");
        let install = fixture.join("caller-install");
        let binary = fixture.join("candidate");
        let artifacts = fixture.join("artifacts");
        let pointer = fixture.join("artifact.path");
        let fake_bin = fixture.join("fake-bin");
        let cp_entered = fixture.join("cp-entered");
        let cp_release = fixture.join("cp-release");
        fs::create_dir_all(&fixture).unwrap();
        fs::create_dir(&fake_bin).unwrap();
        write_complete_install(&install, &"1".repeat(40));
        // This is the ordinary non-ELF publisher fixture. Its resource set must
        // not carry the Buck-only runtime closure before reaching the copy race.
        fs::remove_dir_all(install.join(UNWIND_RUNTIME_RELATIVE)).unwrap();
        write_executable(&binary, b"#!/bin/sh\nexit 0\n");
        write_executable(
            &fake_bin.join("cp"),
            b"#!/usr/bin/env bash\nset -euo pipefail\n: >\"${CP_ENTERED:?}\"\nwhile [[ ! -e ${CP_RELEASE:?} ]]; do sleep 0.01; done\nexec /usr/bin/cp \"$@\"\n",
        );

        let mut child = Command::new(source_root().join("ci/publish-hermit-e2e-artifact.sh"))
            .args([&binary, &artifacts, &pointer, &install])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fake_bin.display(),
                    env::var("PATH").unwrap_or_default()
                ),
            )
            .env("CP_ENTERED", &cp_entered)
            .env("CP_RELEASE", &cp_release)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if cp_entered.is_file() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "publisher never reached cp barrier"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "publisher exited before cp barrier"
            );
            thread::sleep(Duration::from_millis(1));
        }
        fs::write(
            install.join("rsrcs/unprobed-resource"),
            b"mutated-during-copy\n",
        )
        .unwrap();
        fs::write(&cp_release, b"release\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            !output.status.success(),
            "publisher accepted a source mutation during its copy window"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("changed during publication"), "{stderr}");
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn symlink_special_and_stale_inputs_are_refused() {
        let fixture = fixture_root("unsafe-inputs");
        fs::create_dir(&fixture).unwrap();
        let regular = fixture.join("regular");
        write_executable(&regular, b"#!/bin/sh\nexit 0\n");
        let linked = fixture.join("linked");
        symlink(&regular, &linked).unwrap();
        assert!(require_absolute_file(&linked, "fixture symlink").is_err());
        // A Unix socket path must fit sockaddr_un.sun_path (108 bytes), and the
        // fixture root beneath a validation run's TMPDIR does not: the hosted
        // run 36499357369 refused the bind with "path must be shorter than
        // SUN_LEN". Bind the socket in a short directory of its own.
        let socket_dir = PathBuf::from(format!("/tmp/bbr-{}", unique_identity().unwrap()));
        fs::create_dir(&socket_dir).unwrap();
        let socket = socket_dir.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(require_absolute_file(&socket, "fixture socket").is_err());
        drop(listener);
        fs::remove_dir_all(socket_dir).unwrap();
        assert!(create_exclusive_directory(&fixture, "stale fixture").is_err());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn safehermit_bundle_snapshot_survives_poisoned_caller_path_absence() {
        let fixture = fixture_root("safehermit-snapshot");
        let caller_root = fixture.join("caller-tools");
        let evidence = fixture.join("evidence");
        fs::create_dir_all(caller_root.join("bin")).unwrap();
        fs::create_dir_all(caller_root.join("scripts")).unwrap();
        fs::create_dir(&evidence).unwrap();
        write_executable(
            &caller_root.join("bin/safehermit"),
            b"#!/usr/bin/env bash\nroot=$(cd \"$(dirname \"${BASH_SOURCE[0]}\")/..\" && pwd)\ntest -x \"$root/scripts/bounded-run-space\"\nprintf 'snapshot-ok\\n'\n",
        );
        write_executable(
            &caller_root.join("scripts/bounded-run-space"),
            b"#!/usr/bin/env bash\nexit 0\n",
        );
        let snapshot =
            snapshot_safehermit_bundle(&caller_root.join("bin/safehermit"), &evidence).unwrap();
        fs::remove_dir_all(&caller_root).unwrap();
        assert_eq!(
            output_text(
                &mut Command::new(&snapshot.launcher),
                "snapshotted safehermit fixture"
            )
            .unwrap(),
            "snapshot-ok"
        );
        reverify_safehermit_bundle(&snapshot).unwrap();

        let mut facts = fixture_receipt_facts(evidence.file_name().unwrap().to_str().unwrap());
        facts.safehermit_sha256 = snapshot.launcher_sha256.clone();
        facts.bounded_run_space_sha256 = snapshot.bounded_run_space_sha256.clone();
        let resources = fixture_resources();
        let receipt = write_fixture_receipt(&evidence, &facts, &resources);
        verify_fixture_receipt(&evidence, &receipt, &facts, &resources).unwrap();
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn dotslash_snapshot_survives_poisoned_caller_path_absence() {
        let fixture = fixture_root("dotslash-snapshot");
        let evidence = fixture.join("evidence");
        let caller = fixture.join("caller-dotslash");
        fs::create_dir_all(&evidence).unwrap();
        write_executable(&caller, b"#!/bin/sh\nprintf 'DotSlash 0.5.9\\n'\n");
        let snapshot = snapshot_dotslash(&caller, &evidence).unwrap();
        fs::remove_file(&caller).unwrap();
        assert_eq!(
            output_text(
                Command::new(&snapshot.path).arg("--version"),
                "snapshotted DotSlash fixture",
            )
            .unwrap(),
            DOTSLASH_VERSION,
        );
        reverify_input_snapshot(&snapshot, "DotSlash fixture").unwrap();
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn retained_buck_report_version_help_and_host_mutations_are_refused() {
        let fixture = fixture_root("retained-candidate-mutation");
        fs::create_dir_all(&fixture).unwrap();
        let cargo_info = write_retained_candidate_fixture(&fixture);
        verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info).unwrap();

        let run_path = fixture.join("buck-ptrace-verify.json");
        let original_run = fs::read(&run_path).unwrap();
        // Only the compared INFO counts differ from the retained Cargo report.
        let mut changed_run = clock_runtime_report(CAPTURED_VIRTUAL_NS);
        changed_run["compared_log_messages"]["left"] = serde_json::json!(13);
        changed_run["compared_log_messages"]["right"] = serde_json::json!(13);
        fs::write(&run_path, serde_json::to_vec(&changed_run).unwrap()).unwrap();
        let Err(error) =
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
        else {
            panic!("retained evidence accepted changed INFO counts");
        };
        assert!(error.contains("strict reports differ"), "{error}");
        fs::write(&run_path, original_run).unwrap();

        for build in ["cargo", "buck"] {
            let trajectory_path = fixture.join(format!("{build}-ptrace-clock-trajectory.txt"));
            let shifted = shift_clock_trajectory(CAPTURED_CLOCK_TRAJECTORY, 10);
            fs::write(&trajectory_path, shifted).unwrap();
            assert!(
                verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                    .is_err()
            );
            fs::write(&trajectory_path, CAPTURED_CLOCK_TRAJECTORY).unwrap();
        }

        let record_path = fixture.join("buck-ptrace-record-verify.json");
        let original_record = fs::read(&record_path).unwrap();
        let mut changed_record = canonical_report(false);
        changed_record["compared_log_messages"]["left"] = serde_json::json!(14);
        changed_record["compared_log_messages"]["right"] = serde_json::json!(14);
        fs::write(&record_path, serde_json::to_vec(&changed_record).unwrap()).unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::write(&record_path, original_record).unwrap();

        let version_path = fixture.join("buck-version.json");
        let original_version = fs::read(&version_path).unwrap();
        let changed_version = build_info_json().replacen("\"dbt\":true", "\"dbt\":false", 1);
        assert_ne!(changed_version, build_info_json());
        fs::write(&version_path, changed_version).unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::write(&version_path, original_version).unwrap();

        let help_path = fixture.join("buck-help.txt");
        let original_help = fs::read(&help_path).unwrap();
        fs::write(
            &help_path,
            b"ptrace dbt liteinst sabre kvm e9patch changed\n",
        )
        .unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::write(&help_path, original_help).unwrap();

        let host_path = fixture.join("buck-host-capabilities.json");
        fs::write(
            &host_path,
            host_capabilities_json("changed exact evidence bytes"),
        )
        .unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn incomplete_install_bundle_is_refused() {
        let root = fixture_root("incomplete-install-bundle");
        fs::create_dir(&root).unwrap();
        let reverie_sha = "1".repeat(40);

        // Preserve the original regression: an empty authoritative install
        // bundle must never be accepted as a complete backend resource set.
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());

        write_complete_install(&root, &reverie_sha);
        verify_install_bundle(&root, &reverie_sha).unwrap();

        let missing = root.join("rsrcs/libdetcore_dbt.so");
        fs::remove_file(&missing).unwrap();
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());
        write_complete_install(&root, &reverie_sha);

        let executable = root.join("rsrcs/sabre");
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&executable, permissions).unwrap();
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());
        write_complete_install(&root, &reverie_sha);

        fs::write(
            root.join("rsrcs/sabre.revision"),
            format!("{}\n", "2".repeat(40)),
        )
        .unwrap();
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn buck_shadow_publishes_from_the_pre_overlay_caller_install() {
        let root = fixture_root("buck-shadow-publication-source");
        fs::create_dir_all(root.join("ci")).unwrap();
        for name in [
            "publish-hermit-e2e-artifact.sh",
            "verify-hermit-e2e-artifact.sh",
        ] {
            fs::copy(
                source_root().join("ci").join(name),
                root.join("ci").join(name),
            )
            .unwrap();
        }
        let candidate = root.join("candidate-hermit");
        let candidate_source = root.join("candidate.c");
        fs::write(&candidate_source, b"int main(void) { return 0; }\n").unwrap();
        checked_output(
            Command::new("gcc")
                .arg(&candidate_source)
                .arg("-o")
                .arg(&candidate),
            "compile fixture candidate ELF",
        )
        .unwrap();
        let evidence = root.join("evidence");
        fs::create_dir_all(&evidence).unwrap();
        let unwind_sources = root.join("unwind-sources");
        fs::create_dir_all(&unwind_sources).unwrap();
        let unwind = publish_unwind_buck_input(
            &root,
            &evidence,
            fixture_unwind_closure(&unwind_sources, &[], &[]),
        )
        .unwrap();
        let runtime_install = prepare_runtime_install(&evidence, &unwind).unwrap();
        // The caller bundle matches the DAG's prepared install: complete, but
        // without a hermit-runtime closure, which only the overlay supplies.
        let caller = root.join("caller-install");
        write_complete_install(&caller, &"1".repeat(40));
        fs::remove_dir_all(caller.join(UNWIND_RUNTIME_RELATIVE)).unwrap();

        let cargo = publish_verified_bundle(
            &root,
            &evidence,
            "cargo",
            &candidate,
            &caller,
            &runtime_install,
        )
        .unwrap();
        // The published Cargo install carries the overlay copy, so it can never
        // be a source bundle for a second overlaid publication.
        assert!(cargo.install.join(UNWIND_RUNTIME_RELATIVE).is_dir());
        let overlaid_evidence = root.join("overlaid-evidence");
        fs::create_dir_all(&overlaid_evidence).unwrap();
        let overlaid = publish_buck_shadow_bundle(
            &root,
            &overlaid_evidence,
            &candidate,
            &cargo.install,
            &runtime_install,
            &cargo,
        )
        .unwrap_err();
        assert!(
            overlaid.contains("undeclared Hermit runtime closure"),
            "{overlaid}"
        );

        let buck = publish_buck_shadow_bundle(
            &root,
            &evidence,
            &candidate,
            &caller,
            &runtime_install,
            &cargo,
        )
        .unwrap();
        assert_eq!(buck.resource_inventory, cargo.resource_inventory);
        assert_ne!(buck.root, cargo.root);

        // A caller install that changes between the two publications is still
        // refused as mixed input rather than published.
        fs::write(caller.join("rsrcs/unprobed-resource"), b"generation-two\n").unwrap();
        let changed_evidence = root.join("changed-evidence");
        fs::create_dir_all(&changed_evidence).unwrap();
        let changed = publish_buck_shadow_bundle(
            &root,
            &changed_evidence,
            &candidate,
            &caller,
            &runtime_install,
            &cargo,
        )
        .unwrap_err();
        assert!(
            changed.contains("refusing mixed mutable caller inputs"),
            "{changed}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The overlay form of the publisher compares each part of the stage to
    /// its own origin instead of trusting `cp`: the source-bundle part to the
    /// caller install, the runtime part to the overlay, and the overlay to
    /// itself across the copy. A `cp` on PATH that corrupts exactly one of
    /// those is refused by exactly that check; the same `cp` left transparent
    /// publishes, so the refusals come from the checks and not the shim.
    #[test]
    fn overlay_publication_compares_each_staged_part_to_its_origin() {
        let root = fixture_root("overlay-stage-origin");
        fs::create_dir_all(root.join("ci")).unwrap();
        for name in [
            "publish-hermit-e2e-artifact.sh",
            "verify-hermit-e2e-artifact.sh",
        ] {
            let source = fs::read(source_root().join("ci").join(name)).unwrap();
            write_executable(&root.join("ci").join(name), &source);
        }
        let candidate = root.join("candidate-hermit");
        let candidate_source = root.join("candidate.c");
        fs::write(&candidate_source, b"int main(void) { return 0; }\n").unwrap();
        checked_output(
            Command::new("gcc")
                .arg(&candidate_source)
                .arg("-o")
                .arg(&candidate),
            "compile fixture candidate ELF",
        )
        .unwrap();
        let unwind_evidence = root.join("unwind-evidence");
        fs::create_dir_all(&unwind_evidence).unwrap();
        let unwind_sources = root.join("unwind-sources");
        fs::create_dir_all(&unwind_sources).unwrap();
        let unwind = publish_unwind_buck_input(
            &root,
            &unwind_evidence,
            fixture_unwind_closure(&unwind_sources, &[], &[]),
        )
        .unwrap();
        let caller = root.join("caller-install");
        write_complete_install(&caller, &"1".repeat(40));
        fs::remove_dir_all(caller.join(UNWIND_RUNTIME_RELATIVE)).unwrap();
        let real_cp = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|dir| dir.join("cp"))
            .find(|path| path.is_file())
            .unwrap();
        let fake_bin = root.join("fake-cp-bin");
        fs::create_dir(&fake_bin).unwrap();
        write_executable(
            &fake_bin.join("cp"),
            format!(
                r#"#!/bin/sh
set -e
'{real_cp}' "$@"
for last; do :; done
case "$FAKE_CP_MODE" in
stage-source)
    case "$last" in */install/) printf x >>"${{last}}rsrcs/libdetcore_dbt.so" ;; esac ;;
stage-overlay)
    case "$last" in */install/rsrcs/)
        chmod u+w "${{last}}hermit-runtime/{UNWIND_CORE_SONAME}"
        printf x >>"${{last}}hermit-runtime/{UNWIND_CORE_SONAME}" ;; esac ;;
overlay-source)
    for source; do
        case "$source" in */rsrcs/hermit-runtime)
            chmod u+w "$source/{UNWIND_CORE_SONAME}"
            printf x >>"$source/{UNWIND_CORE_SONAME}"
            break ;; esac
    done ;;
esac
"#,
                real_cp = real_cp.display()
            )
            .as_bytes(),
        );
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            env::var("PATH").unwrap_or_default()
        );
        for (mode, refusal) in [
            ("none", None),
            (
                "stage-source",
                Some(
                    "published resource bytes outside the runtime overlay do not match source bundle",
                ),
            ),
            (
                "stage-overlay",
                Some("published runtime overlay bytes do not match the overlay"),
            ),
            (
                "overlay-source",
                Some("runtime overlay changed during publication"),
            ),
        ] {
            // Each case mutates at most its own fresh overlay copy.
            let evidence = root.join(format!("evidence-{mode}"));
            fs::create_dir_all(&evidence).unwrap();
            let runtime_install = prepare_runtime_install(&evidence, &unwind).unwrap();
            let result = checked_output(
                Command::new(root.join("ci/publish-hermit-e2e-artifact.sh"))
                    .arg(&candidate)
                    .arg(root.join(format!("artifacts-{mode}")))
                    .arg(evidence.join("artifact.path"))
                    .arg(&caller)
                    .arg("--runtime-overlay")
                    .arg(&runtime_install)
                    .env("PATH", &path)
                    .env("FAKE_CP_MODE", mode),
                &format!("overlay publication with cp mode {mode}"),
            );
            match refusal {
                None => {
                    result.unwrap();
                    let bundle = PathBuf::from(
                        fs::read_to_string(evidence.join("artifact.path"))
                            .unwrap()
                            .trim(),
                    );
                    assert_eq!(
                        fs::read_to_string(bundle.join("runtime-contract")).unwrap(),
                        "explicit-runtime-overlay-v1\n"
                    );
                }
                Some(refusal) => {
                    let error = result.unwrap_err();
                    assert!(error.contains(refusal), "{mode}: {error}");
                    assert!(
                        !evidence.join("artifact.path").exists(),
                        "{mode}: a refused publication must not write its pointer"
                    );
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validate_dag_installs_only_the_verified_content_addressed_binary() {
        let root = fixture_root("validate-artifact-install");
        fs::create_dir_all(root.join("ci")).unwrap();
        for name in [
            "publish-hermit-e2e-artifact.sh",
            "run-with-hermit-e2e-artifact.sh",
            "verify-hermit-e2e-artifact.sh",
        ] {
            fs::copy(
                source_root().join("ci").join(name),
                root.join("ci").join(name),
            )
            .unwrap();
        }
        let candidate = root.join("candidate-hermit");
        let candidate_source = root.join("candidate.c");
        fs::write(
            &candidate_source,
            b"#include <stdio.h>\nint main(void) { puts(\"{}\"); return 0; }\n",
        )
        .unwrap();
        // Outside the fixture repository, which must stay exactly as staged.
        let unwind_sources = fixture_root("validate-dag-unwind");
        fs::create_dir_all(&unwind_sources).unwrap();
        let unwind_closure = fixture_unwind_closure(&unwind_sources, &[], &[]);
        checked_output(
            Command::new("gcc")
                .arg(&candidate_source)
                .arg(format!("-L{}", unwind_sources.display()))
                .args([
                    "-Wl,--no-as-needed",
                    "-lgcc_s",
                    "-lm",
                    "-Wl,-l:libunwind-x86_64.so.8",
                    "-Wl,-l:ld-linux-x86-64.so.2",
                    &format!("-Wl,--disable-new-dtags,-rpath,{RELEASE_RPATH}"),
                    "-o",
                ])
                .arg(&candidate),
            "compile fixture Buck release ELF",
        )
        .unwrap();
        checked_output(
            fixture_git().current_dir(&root).arg("init"),
            "init fixture Git repository",
        )
        .unwrap();
        checked_output(
            fixture_git()
                .current_dir(&root)
                .args(["add", "ci", "candidate-hermit", "candidate.c"]),
            "stage fixture repository",
        )
        .unwrap();
        checked_output(
            fixture_git().current_dir(&root).args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "fixture",
            ]),
            "commit fixture repository",
        )
        .unwrap();
        let head = git(&root, &["rev-parse", "HEAD"]).unwrap();
        let pointer = root.join("ignored/buck2-phase2/test-publisher.path");
        fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        let unwind =
            publish_unwind_buck_input(&root, pointer.parent().unwrap(), unwind_closure).unwrap();
        let runtime_install = prepare_runtime_install(pointer.parent().unwrap(), &unwind).unwrap();
        checked_output(
            Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
                candidate.as_os_str(),
                root.join(VALIDATE_ARTIFACT_ROOT).as_os_str(),
                pointer.as_os_str(),
                OsStr::new("--runtime-only"),
                runtime_install.as_os_str(),
            ]),
            "fixture Buck validate artifact publication",
        )
        .unwrap();
        let bundle = PathBuf::from(fs::read_to_string(&pointer).unwrap().trim());
        let identity = bundle.file_name().unwrap().to_str().unwrap();
        let expected = sha256(&candidate).unwrap();
        let resources_sha256 = sha256(&bundle.join("resources.sha256")).unwrap();
        let selector = format!(
            "schema\thermit-buck-validate-selector/v2\nhead\t{head}\nidentity\t{identity}\nbinary_sha256\t{expected}\nresources_sha256\t{resources_sha256}\nruntime_sha256\t{UNWIND_ARCH_SONAME}\t{}\nruntime_sha256\t{UNWIND_CORE_SONAME}\t{}\n",
            unwind.source_sha256[UNWIND_ARCH_SONAME], unwind.source_sha256[UNWIND_CORE_SONAME],
        );
        replace_text_file(
            &root.join(VALIDATE_ARTIFACT_IDENTITY),
            selector.as_bytes(),
            "fixture current identity",
        )
        .unwrap();

        assert_eq!(install_validate_dag_artifact(&root).unwrap(), expected);
        assert_eq!(
            sha256(&root.join("target/release/hermit")).unwrap(),
            expected
        );
        assert_eq!(
            sha256(&root.join("target/ci/hermit-strict")).unwrap(),
            expected
        );
        assert_eq!(
            verify_runtime_install(&root.join("target/install_pkg"), &runtime_hashes(&unwind),)
                .unwrap(),
            runtime_hashes(&unwind),
        );

        let consume = |pointer: &Path, require_install: bool| {
            let mut command = Command::new(root.join("ci/run-with-hermit-e2e-artifact.sh"));
            if require_install {
                command.arg("--require-install");
            }
            checked_output(
                command
                    .args(["sh", "-c", "printf %s \"$HERMIT_INSTALL_DIR\""])
                    .env("HERMIT_E2E_ARTIFACT_POINTER", pointer),
                "fixture artifact consumer",
            )
        };
        let verify = |pointer: &Path| {
            checked_output(
                Command::new(root.join("ci/verify-hermit-e2e-artifact.sh")).arg(pointer),
                "fixture artifact verification",
            )
        };
        // A runtime-only bundle has install/ too, but only the unwind closure.
        let runtime_failure = consume(&pointer, true).unwrap_err();
        assert!(
            runtime_failure.contains("consumer requires a complete resource bundle"),
            "{runtime_failure}"
        );
        assert_eq!(
            String::from_utf8(consume(&pointer, false).unwrap().stdout).unwrap(),
            bundle.join("install").display().to_string()
        );
        // Every entry of the published closure is inventoried, not only files.
        let runtime_dir = bundle.join("install").join(UNWIND_RUNTIME_RELATIVE);
        let fifo = runtime_dir.join("extra.fifo");
        checked_output(Command::new("mkfifo").arg(&fifo), "make fixture FIFO").unwrap();
        let fifo_failure = verify(&pointer).unwrap_err();
        assert!(
            fifo_failure.contains("outside the exact unwind closure"),
            "{fifo_failure}"
        );
        fs::remove_file(&fifo).unwrap();
        for extra_directory in [
            runtime_dir.join("extra.d"),
            bundle.join("install/rsrcs/extra.d"),
        ] {
            fs::create_dir(&extra_directory).unwrap();
            let directory_failure = verify(&pointer).unwrap_err();
            assert!(
                directory_failure.contains("outside the exact unwind closure"),
                "{}: {directory_failure}",
                extra_directory.display()
            );
            fs::remove_dir(&extra_directory).unwrap();
        }
        verify(&pointer).unwrap();

        let complete_install = root.join("complete-install");
        write_complete_install(&complete_install, &"1".repeat(40));
        let complete_pointer = root.join("ignored/buck2-phase2/complete-artifact.path");
        checked_output(
            Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
                candidate.as_os_str(),
                root.join("complete-artifacts").as_os_str(),
                complete_pointer.as_os_str(),
                complete_install.as_os_str(),
            ]),
            "fixture complete Buck artifact publication",
        )
        .unwrap();
        let complete_bundle = PathBuf::from(fs::read_to_string(&complete_pointer).unwrap().trim());
        assert_eq!(
            fs::read_to_string(complete_bundle.join("runtime-contract")).unwrap(),
            "elf-rpath-v1\n"
        );
        assert_eq!(
            String::from_utf8(consume(&complete_pointer, true).unwrap().stdout).unwrap(),
            complete_bundle.join("install").display().to_string()
        );
        // The manifest binds regular files only, so a special file is refused.
        let complete_fifo = complete_bundle.join("install/rsrcs/extra.fifo");
        checked_output(
            Command::new("mkfifo").arg(&complete_fifo),
            "make fixture FIFO",
        )
        .unwrap();
        let complete_fifo_failure = consume(&complete_pointer, true).unwrap_err();
        assert!(
            complete_fifo_failure.contains("non-regular entry outside its manifest"),
            "{complete_fifo_failure}"
        );
        fs::remove_file(&complete_fifo).unwrap();
        // A directory is not a special file; only the closure inventory sees it.
        let complete_runtime_directory = complete_bundle
            .join("install")
            .join(UNWIND_RUNTIME_RELATIVE)
            .join("extra.d");
        fs::create_dir(&complete_runtime_directory).unwrap();
        let complete_directory_failure = verify(&complete_pointer).unwrap_err();
        assert!(
            complete_directory_failure.contains("outside the exact unwind closure"),
            "{complete_directory_failure}"
        );
        fs::remove_dir(&complete_runtime_directory).unwrap();
        verify(&complete_pointer).unwrap();
        let extra_runtime = complete_install
            .join(UNWIND_RUNTIME_RELATIVE)
            .join("extra.so");
        fs::write(&extra_runtime, b"extra runtime\n").unwrap();
        let extra_failure = checked_output(
            Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
                candidate.as_os_str(),
                root.join("extra-runtime-artifacts").as_os_str(),
                root.join("ignored/buck2-phase2/extra-runtime.path")
                    .as_os_str(),
                complete_install.as_os_str(),
            ]),
            "refuse an extra Buck runtime resource",
        )
        .unwrap_err();
        assert!(
            extra_failure.contains("outside the exact unwind closure"),
            "{extra_failure}"
        );
        fs::remove_file(extra_runtime).unwrap();
        // An empty directory is outside the closure too.
        let extra_directory = complete_install
            .join(UNWIND_RUNTIME_RELATIVE)
            .join("extra.d");
        fs::create_dir(&extra_directory).unwrap();
        let extra_directory_failure = checked_output(
            Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
                candidate.as_os_str(),
                root.join("extra-directory-artifacts").as_os_str(),
                root.join("ignored/buck2-phase2/extra-directory.path")
                    .as_os_str(),
                complete_install.as_os_str(),
            ]),
            "refuse an extra Buck runtime directory",
        )
        .unwrap_err();
        assert!(
            extra_directory_failure.contains("outside the exact unwind closure"),
            "{extra_directory_failure}"
        );
        fs::remove_dir(extra_directory).unwrap();
        // Refused before anything is moved into the artifact root.
        let source_fifo = complete_install.join("rsrcs/extra.fifo");
        checked_output(
            Command::new("mkfifo").arg(&source_fifo),
            "make fixture FIFO",
        )
        .unwrap();
        let fifo_artifacts = root.join("fifo-artifacts");
        let source_fifo_failure = checked_output(
            Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
                candidate.as_os_str(),
                fifo_artifacts.as_os_str(),
                root.join("ignored/buck2-phase2/fifo.path").as_os_str(),
                complete_install.as_os_str(),
            ]),
            "refuse a special file in a complete resource bundle",
        )
        .unwrap_err();
        assert!(
            source_fifo_failure.contains("non-regular entry outside its manifest"),
            "{source_fifo_failure}"
        );
        assert_eq!(fs::read_dir(&fifo_artifacts).unwrap().count(), 0);
        fs::remove_file(&source_fifo).unwrap();
        let fake_bin = root.join("fake-readelf-bin");
        fs::create_dir(&fake_bin).unwrap();
        write_executable(&fake_bin.join("readelf"), b"#!/bin/sh\nexit 1\n");
        let readelf_failure = checked_output(
            Command::new(root.join("ci/publish-hermit-e2e-artifact.sh"))
                .args([
                    candidate.as_os_str(),
                    root.join("readelf-failure-artifacts").as_os_str(),
                    root.join("ignored/buck2-phase2/readelf-failure.path")
                        .as_os_str(),
                    complete_install.as_os_str(),
                ])
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        fake_bin.display(),
                        env::var("PATH").unwrap_or_default()
                    ),
                ),
            "refuse an unreadable ELF dynamic contract",
        )
        .unwrap_err();
        assert!(
            readelf_failure.contains("cannot read selected Hermit dynamic contract"),
            "{readelf_failure}"
        );
        for soname in [UNWIND_ARCH_SONAME, UNWIND_CORE_SONAME] {
            assert!(
                complete_bundle
                    .join("install")
                    .join(UNWIND_RUNTIME_RELATIVE)
                    .join(soname)
                    .is_file()
            );
        }
        fs::remove_file(
            complete_bundle
                .join("install")
                .join(UNWIND_RUNTIME_RELATIVE)
                .join(UNWIND_CORE_SONAME),
        )
        .unwrap();
        assert!(
            checked_output(
                Command::new(root.join("ci/verify-hermit-e2e-artifact.sh")).arg(&complete_pointer),
                "refuse incomplete complete Buck artifact",
            )
            .is_err()
        );

        fs::write(root.join(VALIDATE_ARTIFACT_IDENTITY), b"not-a-digest\n").unwrap();
        assert!(install_validate_dag_artifact(&root).is_err());
        fs::write(
            root.join(VALIDATE_ARTIFACT_IDENTITY),
            selector.replace(
                &format!("head\t{head}"),
                &format!("head\t{}", "0".repeat(40)),
            ),
        )
        .unwrap();
        assert!(install_validate_dag_artifact(&root).is_err());
        fs::write(root.join(VALIDATE_ARTIFACT_IDENTITY), selector).unwrap();
        fs::write(bundle.join("hermit"), b"mutated after publication\n").unwrap();
        assert!(install_validate_dag_artifact(&root).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(unwind_sources).unwrap();
    }

    #[test]
    fn lzma_link_input_is_content_addressed_declared_and_mutation_guarded() {
        let root = fixture_root("lzma-declared-input");
        let evidence_a = root.join("ignored/evidence-a");
        let evidence_b = root.join("ignored/evidence-b");
        fs::create_dir_all(&evidence_a).unwrap();
        fs::create_dir_all(&evidence_b).unwrap();
        let source = root.join("liblzma.so.5.2.5");
        let fixture_c = root.join("lzma-fixture.c");
        fs::write(&fixture_c, b"int lzma_fixture(void) { return 0; }\n").unwrap();
        checked_output(
            Command::new("gcc")
                .args(["-shared", "-fPIC", "-Wl,-soname,liblzma.so.5", "-o"])
                .arg(&source)
                .arg(&fixture_c),
            "compile fixture liblzma ELF",
        )
        .unwrap();

        let first = publish_lzma_buck_input(&root, &evidence_a, &source).unwrap();
        assert!(
            first
                .target
                .starts_with("root//ignored/buck2-link-inputs/lzma/")
        );
        assert!(first.target.ends_with(":lzma"));
        assert_eq!(sha256(&first.library).unwrap(), sha256(&source).unwrap());
        assert_eq!(
            fs::read_to_string(first.root.join("BUCK")).unwrap(),
            LZMA_BUCK_PACKAGE
        );
        let second = publish_lzma_buck_input(&root, &evidence_b, &source).unwrap();
        assert_eq!(first.target, second.target);
        assert_eq!(first.library, second.library);

        fs::write(
            first.root.join("BUCK"),
            b"prebuilt_cxx_library(name = \"decoy\")\n",
        )
        .unwrap();
        assert!(verify_lzma_buck_input(&first).is_err());
        fs::write(first.root.join("BUCK"), LZMA_BUCK_PACKAGE).unwrap();
        fs::write(&source, b"changed source\n").unwrap();
        assert!(verify_lzma_buck_input(&first).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unwind_closure_is_exact_content_addressed_and_mutation_guarded() {
        let root = fixture_root("unwind-declared-input");
        let source_root = root.join("sources");
        let evidence = root.join("ignored/evidence");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&evidence).unwrap();
        let sources = fixture_unwind_closure(&source_root, &[], &[]);
        let input = publish_unwind_buck_input(&root, &evidence, sources).unwrap();
        assert!(
            input
                .target
                .starts_with("root//ignored/buck2-link-inputs/unwind/")
        );
        assert!(input.target.ends_with(":unwind"));
        assert_eq!(input.root.file_name().unwrap(), input.identity.as_str());
        verify_unwind_buck_input(&input).unwrap();

        let restore = |name: &str, source: &Path| {
            let path = input.root.join(name);
            let _ = fs::remove_file(&path);
            fs::copy(source, path).unwrap();
        };
        fs::write(input.root.join("unexpected"), b"extra\n").unwrap();
        assert!(verify_unwind_buck_input(&input).is_err());
        fs::remove_file(input.root.join("unexpected")).unwrap();

        fs::remove_file(input.root.join(UNWIND_CORE_SONAME)).unwrap();
        assert!(verify_unwind_buck_input(&input).is_err());
        restore(UNWIND_CORE_SONAME, &input.sources.core_shared);

        fs::write(input.root.join(UNWIND_ARCH_SONAME), b"tampered\n").unwrap();
        assert!(verify_unwind_buck_input(&input).is_err());
        restore(UNWIND_ARCH_SONAME, &input.sources.arch_shared);

        fs::copy(
            &input.sources.core_shared,
            input.root.join(UNWIND_ARCH_SONAME),
        )
        .unwrap();
        assert!(verify_unwind_buck_input(&input).is_err());
        assert!(
            verify_shared_library(
                &input.root.join(UNWIND_ARCH_SONAME),
                UNWIND_ARCH_SONAME,
                &[UNWIND_CORE_SONAME, "libc.so.6"],
            )
            .unwrap_err()
            .contains("SONAME")
        );
        restore(UNWIND_ARCH_SONAME, &input.sources.arch_shared);

        fs::remove_file(input.root.join(UNWIND_ARCH_SONAME)).unwrap();
        std::os::unix::fs::symlink(
            &input.sources.arch_shared,
            input.root.join(UNWIND_ARCH_SONAME),
        )
        .unwrap();
        assert!(verify_unwind_buck_input(&input).is_err());
        restore(UNWIND_ARCH_SONAME, &input.sources.arch_shared);

        fs::write(
            input.root.join("BUCK"),
            b"prebuilt_cxx_library(name = \"decoy\")\n",
        )
        .unwrap();
        assert!(verify_unwind_buck_input(&input).is_err());
        fs::write(input.root.join("BUCK"), UNWIND_BUCK_PACKAGE).unwrap();

        let runtime = prepare_runtime_install(&evidence, &input).unwrap();
        let expected_runtime = runtime_hashes(&input);
        verify_runtime_install(&runtime, &expected_runtime).unwrap();
        let runtime_root = runtime.join(UNWIND_RUNTIME_RELATIVE);
        fs::write(runtime_root.join("extra.so"), b"extra\n").unwrap();
        assert!(verify_runtime_install(&runtime, &expected_runtime).is_err());
        fs::remove_file(runtime_root.join("extra.so")).unwrap();
        fs::remove_file(runtime_root.join(UNWIND_CORE_SONAME)).unwrap();
        assert!(verify_runtime_install(&runtime, &expected_runtime).is_err());
        fs::copy(
            input.root.join(UNWIND_CORE_SONAME),
            runtime_root.join(UNWIND_CORE_SONAME),
        )
        .unwrap();
        fs::copy(
            input.root.join(UNWIND_CORE_SONAME),
            runtime_root.join(UNWIND_ARCH_SONAME),
        )
        .unwrap();
        assert!(
            verify_runtime_install(&runtime, &expected_runtime)
                .unwrap_err()
                .contains("SONAME")
        );

        fs::write(&input.sources.core_shared, b"changed source\n").unwrap();
        assert!(verify_unwind_buck_input(&input).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unwind_closure_accepts_exactly_the_two_reviewed_profiles() {
        let root = fixture_root("unwind-profiles");
        let closure = |label: &str, arch_extra: &[&str], core_extra: &[&str]| {
            let dir = root.join(label);
            fs::create_dir_all(&dir).unwrap();
            let closure = fixture_unwind_closure(&dir, arch_extra, core_extra);
            let verdict = verify_unwind_shared_pair(&closure.arch_shared, &closure.core_shared);
            (closure, verdict)
        };

        let (_, narrow) = closure("narrow", &[], &[]);
        assert_eq!(narrow, Ok(UnwindProfile::Narrow));
        // Ubuntu 24.04's libunwind8 names liblzma.so.5 from both libraries.
        let (lzma_closure, lzma) = closure("lzma", &[LZMA_SONAME], &[LZMA_SONAME]);
        assert_eq!(lzma, Ok(UnwindProfile::Lzma));

        // A pair that mixes the profiles, or names anything else, is refused.
        for (label, arch_extra, core_extra) in [
            ("arch-only-lzma", &[LZMA_SONAME][..], &[][..]),
            ("core-only-lzma", &[][..], &[LZMA_SONAME][..]),
            ("stray", &["libstray.so.1"][..], &["libstray.so.1"][..]),
            (
                "lzma-and-stray",
                &[LZMA_SONAME, "libstray.so.1"][..],
                &[LZMA_SONAME][..],
            ),
            ("unversioned-lzma", &["liblzma.so"][..], &["liblzma.so"][..]),
        ] {
            let (_, verdict) = closure(label, arch_extra, core_extra);
            let error = verdict.expect_err(label);
            assert!(
                error.contains("matches no reviewed closure profile")
                    && error.contains("narrow:")
                    && error.contains("lzma:"),
                "{label}: {error}"
            );
        }

        // The lzma profile travels through publication and the installed
        // runtime bundle, and the evidence names the profile it matched.
        let evidence = root.join("ignored/evidence");
        fs::create_dir_all(&evidence).unwrap();
        let input = publish_unwind_buck_input(&root, &evidence, lzma_closure).unwrap();
        assert_eq!(verify_unwind_buck_input(&input), Ok(UnwindProfile::Lzma));
        let rows = fs::read_to_string(evidence.join("unwind-input.tsv")).unwrap();
        assert!(
            rows.starts_with("schema\thermit-buck-unwind-input/v2\n"),
            "{rows}"
        );
        assert!(rows.contains("\nprofile\tlzma\n"), "{rows}");
        let runtime = prepare_runtime_install(&evidence, &input).unwrap();
        assert_eq!(
            verify_runtime_install(&runtime, &runtime_hashes(&input)).unwrap(),
            runtime_hashes(&input)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_rpath_is_exact_relative_and_never_runpath_or_ambient() {
        let root = fixture_root("release-rpath");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("main.c");
        fs::write(&source, b"int main(void) { return 0; }\n").unwrap();
        let compile = |name: &str, flags: &[&str]| {
            let output = root.join(name);
            let mut command = Command::new("gcc");
            command.arg(&source).args(flags).arg("-o").arg(&output);
            checked_output(&mut command, &format!("compile {name} RPATH fixture")).unwrap();
            output
        };
        let good = compile(
            "good",
            &[&format!("-Wl,--disable-new-dtags,-rpath,{RELEASE_RPATH}")],
        );
        validate_release_rpath(&good).unwrap();
        let missing = compile("missing", &[]);
        assert!(validate_release_rpath(&missing).is_err());
        let absolute = compile("absolute", &["-Wl,--disable-new-dtags,-rpath,/tmp"]);
        assert!(validate_release_rpath(&absolute).is_err());
        let runpath = compile(
            "runpath",
            &[&format!("-Wl,--enable-new-dtags,-rpath,{RELEASE_RPATH}")],
        );
        assert!(validate_release_rpath(&runpath).is_err());
        let ambient = compile(
            "ambient",
            &[&format!("-Wl,--disable-new-dtags,-rpath,{RELEASE_RPATH}:")],
        );
        assert!(validate_release_rpath(&ambient).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_buck_target_uses_declared_library_dependencies_and_exact_rpath() {
        let buck = fs::read_to_string(source_root().join("hermit-cli/BUCK")).unwrap();
        assert!(buck.contains("\"gh_facebook_buck2_shims_meta//third-party/xz:lzma\""));
        assert!(
            buck.contains("\"gh_facebook_buck2_shims_meta//third-party/libunwind:unwind-ptrace\"")
        );
        assert!(buck.contains("deps = [release_lzma_target, release_unwind_target] + ["));
        assert!(buck.contains("link-arg=-Wl,--disable-new-dtags"));
        assert!(buck.contains(&format!("link-arg=-Wl,-rpath,{RELEASE_RPATH}")));
        assert!(!buck.contains("-Clink-arg=-llzma"));
        assert!(!buck.contains("-Clink-arg=-l:liblzma"));
        assert!(!buck.contains("LD_LIBRARY_PATH"));
    }

    #[test]
    fn validate_candidate_needed_libraries_are_exact_versioned_and_portable() {
        let expected = RELEASE_NEEDED_LIBRARIES
            .iter()
            .map(|library| (*library).to_owned())
            .collect::<BTreeSet<_>>();
        validate_release_needed_libraries(&expected).unwrap();

        let mut unversioned = expected.clone();
        unversioned.insert("liblzma.so".to_owned());
        assert!(
            validate_release_needed_libraries(&unversioned)
                .unwrap_err()
                .contains("unversioned")
        );

        let mut nonportable = expected.clone();
        nonportable.insert("/opt/vendor/libextra.so.1".to_owned());
        assert!(
            validate_release_needed_libraries(&nonportable)
                .unwrap_err()
                .contains("nonportable")
        );

        let mut unexpected = expected.clone();
        unexpected.insert("libcrypto.so.3".to_owned());
        assert!(
            validate_release_needed_libraries(&unexpected)
                .unwrap_err()
                .contains("unexpected")
        );

        let mut missing = expected;
        missing.remove("libunwind-x86_64.so.8");
        assert!(
            validate_release_needed_libraries(&missing)
                .unwrap_err()
                .contains("missing")
        );
    }

    #[test]
    fn parity_mismatch_is_refused() {
        assert!(require_equal("fixture", "cargo", "buck").is_err());
    }

    /// Exact stdout of the clock fixture under the pinned epoch, captured from
    /// `--backend ptrace run --base-env=minimal --epoch=2026-01-01T00:00:00+00:00
    /// --max-timeslice=200000000 --strict --verify --verify-strict` through
    /// safehermit with both a Cargo- and a Buck-built candidate (the shadow
    /// artifacts of the pre-rebase form of "Run the shadow DBT matrix under the
    /// official host twin's contract"); the two were byte-identical and both reports
    /// gave 48,760,645 virtual ns. Hermit returned the same epoch-anchored
    /// value for both clocks there; the tests below prove nothing requires it.
    const CAPTURED_CLOCK_TRAJECTORY: &str = "\
        sample gen=0 source=main index=0 ns=1767225600002243035\n\
        sample gen=0 source=main index=1 ns=1767225600002253075\n\
        sample gen=0 source=main index=2 ns=1767225600002263115\n\
        sample gen=0 source=main index=3 ns=1767225600002273155\n\
        sample gen=0 source=main index=4 ns=1767225600002283195\n\
        sample gen=0 source=main index=5 ns=1767225600002293235\n\
        sample gen=0 source=main index=6 ns=1767225600002303275\n\
        sample gen=0 source=main index=7 ns=1767225600002313315\n\
        realtime gen=0 phase=open ns=1767225600002377745\n\
        work gen=0 index=0 iterations=0 before_ns=1767225600002389385 after_ns=1767225600002399435 checksum=27\n\
        work gen=0 index=1 iterations=100000 before_ns=1767225600002413395 after_ns=1767225600004423445 checksum=4\n\
        work gen=0 index=2 iterations=100000 before_ns=1767225600004437485 after_ns=1767225600006447535 checksum=4\n\
        work gen=0 index=3 iterations=200000 before_ns=1767225600006461575 after_ns=1767225600010471625 checksum=2\n\
        sample gen=0 source=thread index=0 thread=0 ns=1767225600011259435\n\
        sample gen=0 source=thread index=1 thread=0 ns=1767225600011269475\n\
        sample gen=0 source=thread index=2 thread=0 ns=1767225600011279515\n\
        sample gen=0 source=thread index=3 thread=0 ns=1767225600011289555\n\
        sample gen=0 source=thread index=0 thread=1 ns=1767225600013829535\n\
        sample gen=0 source=thread index=1 thread=1 ns=1767225600013839575\n\
        sample gen=0 source=thread index=2 thread=1 ns=1767225600013849615\n\
        sample gen=0 source=thread index=3 thread=1 ns=1767225600013859655\n\
        realtime gen=0 phase=close ns=1767225600015646925\n\
        gen=0 first=1767225600002243035 last=1767225600013859655 min_delta=10040\n\
        sample gen=1 source=main index=0 ns=1767225600017915150\n\
        sample gen=1 source=main index=1 ns=1767225600017925190\n\
        sample gen=1 source=main index=2 ns=1767225600017935230\n\
        sample gen=1 source=main index=3 ns=1767225600017945270\n\
        sample gen=1 source=main index=4 ns=1767225600017955310\n\
        sample gen=1 source=main index=5 ns=1767225600017965350\n\
        sample gen=1 source=main index=6 ns=1767225600017975390\n\
        sample gen=1 source=main index=7 ns=1767225600017985430\n\
        realtime gen=1 phase=open ns=1767225600018050120\n\
        work gen=1 index=0 iterations=0 before_ns=1767225600018061790 after_ns=1767225600018071840 checksum=27\n\
        work gen=1 index=1 iterations=100000 before_ns=1767225600018085830 after_ns=1767225600020095880 checksum=4\n\
        work gen=1 index=2 iterations=100000 before_ns=1767225600020109950 after_ns=1767225600022120000 checksum=4\n\
        work gen=1 index=3 iterations=200000 before_ns=1767225600022134070 after_ns=1767225600026144120 checksum=2\n\
        sample gen=1 source=thread index=0 thread=0 ns=1767225600026931960\n\
        sample gen=1 source=thread index=1 thread=0 ns=1767225600026942000\n\
        sample gen=1 source=thread index=2 thread=0 ns=1767225600026952040\n\
        sample gen=1 source=thread index=3 thread=0 ns=1767225600026962080\n\
        sample gen=1 source=thread index=0 thread=1 ns=1767225600029502180\n\
        sample gen=1 source=thread index=1 thread=1 ns=1767225600029512220\n\
        sample gen=1 source=thread index=2 thread=1 ns=1767225600029522260\n\
        sample gen=1 source=thread index=3 thread=1 ns=1767225600029532300\n\
        realtime gen=1 phase=close ns=1767225600031319690\n\
        gen=1 first=1767225600017915150 last=1767225600029532300 min_delta=10040\n\
        sample gen=2 source=main index=0 ns=1767225600033587975\n\
        sample gen=2 source=main index=1 ns=1767225600033598015\n\
        sample gen=2 source=main index=2 ns=1767225600033608055\n\
        sample gen=2 source=main index=3 ns=1767225600033618095\n\
        sample gen=2 source=main index=4 ns=1767225600033628135\n\
        sample gen=2 source=main index=5 ns=1767225600033638175\n\
        sample gen=2 source=main index=6 ns=1767225600033648215\n\
        sample gen=2 source=main index=7 ns=1767225600033658255\n\
        realtime gen=2 phase=open ns=1767225600033722945\n\
        work gen=2 index=0 iterations=0 before_ns=1767225600033734615 after_ns=1767225600033744665 checksum=27\n\
        work gen=2 index=1 iterations=100000 before_ns=1767225600033758655 after_ns=1767225600035768705 checksum=4\n\
        work gen=2 index=2 iterations=100000 before_ns=1767225600035782775 after_ns=1767225600037792825 checksum=4\n\
        work gen=2 index=3 iterations=200000 before_ns=1767225600037806895 after_ns=1767225600041816945 checksum=2\n\
        sample gen=2 source=thread index=0 thread=0 ns=1767225600042604785\n\
        sample gen=2 source=thread index=1 thread=0 ns=1767225600042614825\n\
        sample gen=2 source=thread index=2 thread=0 ns=1767225600042624865\n\
        sample gen=2 source=thread index=3 thread=0 ns=1767225600042634905\n\
        sample gen=2 source=thread index=0 thread=1 ns=1767225600045175005\n\
        sample gen=2 source=thread index=1 thread=1 ns=1767225600045185045\n\
        sample gen=2 source=thread index=2 thread=1 ns=1767225600045195085\n\
        sample gen=2 source=thread index=3 thread=1 ns=1767225600045205125\n\
        realtime gen=2 phase=close ns=1767225600046992515\n\
        gen=2 first=1767225600033587975 last=1767225600045205125 min_delta=10040\n\
        clock exec continuity holds across 2 execs\n";

    const CAPTURED_REALTIME_LAST: u64 = 1_767_225_600_046_992_515;
    const CAPTURED_MONOTONIC_SPAN: u64 = 45_205_125 - 2_243_035;
    const CAPTURED_VIRTUAL_NS: u64 = 48_760_645;

    fn clock_runtime_report_for(text: &str, virtual_nanoseconds: u64) -> Value {
        let mut report = canonical_report(true);
        for side in ["left", "right"] {
            let output = &mut report["compared_outputs"][side];
            output["stdout_sha256"] = sha256_bytes(text.as_bytes()).unwrap().into();
            output["stdout_bytes"] = text.len().into();
        }
        let run = serde_json::json!({
            "scheduler_turns": 36, "virtual_nanoseconds": virtual_nanoseconds, "syscalls": 256
        });
        report["runtime"] = serde_json::json!({"run1": run, "run2": run});
        report
    }

    fn clock_runtime_report(virtual_nanoseconds: u64) -> Value {
        clock_runtime_report_for(CAPTURED_CLOCK_TRAJECTORY, virtual_nanoseconds)
    }

    /// Adds `offset` to every timestamp on the lines `select` accepts, keeping
    /// order, costs and summaries.
    fn shift_clock_lines(text: &str, offset: i64, select: impl Fn(usize, &str) -> bool) -> String {
        let shift = |field: &str| match field.split_once('=') {
            Some((key, value))
                if ["ns", "before_ns", "after_ns", "first", "last"].contains(&key) =>
            {
                format!("{key}={}", value.parse::<i64>().unwrap() + offset)
            }
            _ => field.to_owned(),
        };
        text.lines()
            .enumerate()
            .map(|(index, line)| match select(index, line) {
                true => line.split(' ').map(shift).collect::<Vec<_>>().join(" ") + "\n",
                false => format!("{line}\n"),
            })
            .collect()
    }

    /// Adds `offset` to every timestamp of both clocks.
    fn shift_clock_trajectory(text: &str, offset: i64) -> String {
        shift_clock_lines(text, offset, |_, _| true)
    }

    /// Adds `offset` to the REALTIME anchors only.
    fn shift_realtime(text: &str, offset: i64) -> String {
        shift_clock_lines(text, offset, |_, line| line.starts_with("realtime "))
    }

    /// Adds `offset` to the MONOTONIC records only, from line `from` onward.
    fn shift_monotonic_from(text: &str, offset: i64, from: usize) -> String {
        shift_clock_lines(text, offset, |index, line| {
            index >= from && !line.starts_with("realtime ")
        })
    }

    /// Replaces the given 0-based lines of the captured trajectory.
    fn edit_clock_trajectory(edits: &[(usize, &str)]) -> String {
        let mut lines = CAPTURED_CLOCK_TRAJECTORY.lines().collect::<Vec<_>>();
        for (index, line) in edits {
            lines[*index] = line;
        }
        lines.join("\n") + "\n"
    }

    const CAPTURED_TRAJECTORY: ClockTrajectory = ClockTrajectory {
        realtime_last: CAPTURED_REALTIME_LAST,
        monotonic_span: CAPTURED_MONOTONIC_SPAN,
    };

    #[test]
    fn clock_trajectory_accepts_the_capture_and_refuses_each_violation() {
        let text = CAPTURED_CLOCK_TRAJECTORY;
        assert_eq!(parse_clock_trajectory(text), Ok(CAPTURED_TRAJECTORY));
        // A shift inside the startup bound is valid; only the cross-build
        // comparison can refuse it.
        parse_clock_trajectory(&shift_clock_trajectory(text, 10)).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        let without = |index: usize| {
            let mut kept = lines.clone();
            kept.remove(index);
            kept.join("\n") + "\n"
        };
        let mut reordered = lines.clone();
        reordered.swap(13, 17);
        let advance = "does not advance past";
        let cost = "equal work must cost equal virtual time";
        let cases = [
            (
                "freeze",
                edit_clock_trajectory(&[(
                    1,
                    "sample gen=0 source=main index=1 ns=1767225600002243035",
                )]),
                advance,
            ),
            (
                "backwards",
                edit_clock_trajectory(&[(
                    3,
                    "sample gen=0 source=main index=3 ns=1767225600002253000",
                )]),
                advance,
            ),
            (
                "exec reset",
                edit_clock_trajectory(&[(
                    23,
                    "sample gen=1 source=main index=0 ns=1767225600002243035",
                )]),
                advance,
            ),
            (
                "thread reset",
                edit_clock_trajectory(&[(
                    13,
                    "sample gen=0 source=thread index=0 thread=0 ns=1767225600002243035",
                )]),
                advance,
            ),
            (
                "epoch too early",
                shift_clock_trajectory(text, -1_000_000_000),
                "CLOCK_REALTIME read",
            ),
            (
                "epoch too late",
                shift_clock_trajectory(text, 86_400_000_000_000),
                "outside the startup bound",
            ),
            (
                "round origin",
                shift_clock_trajectory(text, 1_000_000_000 - 17_915_150),
                "opens on a whole second",
            ),
            // REALTIME anchors are their own chain: each refusal below leaves
            // every MONOTONIC record untouched.
            (
                "REALTIME before the epoch",
                shift_realtime(text, -1_000_000_000),
                "CLOCK_REALTIME read",
            ),
            (
                "REALTIME past the startup bound",
                shift_realtime(text, 60_000_000_000),
                "outside the startup bound",
            ),
            (
                "REALTIME round origin",
                shift_realtime(text, 1_000_000_000 - 18_050_120),
                "REALTIME anchor opens on a whole second",
            ),
            (
                "REALTIME frozen",
                edit_clock_trajectory(&[(21, "realtime gen=0 phase=close ns=1767225600002377745")]),
                "CLOCK_REALTIME read",
            ),
            (
                "REALTIME exec reset",
                edit_clock_trajectory(&[(31, "realtime gen=1 phase=open ns=1767225600002377745")]),
                "CLOCK_REALTIME read",
            ),
            (
                "REALTIME anchor missing",
                without(8),
                "expected \"realtime gen=0 phase=open\"",
            ),
            (
                "REALTIME close missing",
                without(21),
                "expected \"realtime gen=0 phase=close\"",
            ),
            // Work-cost controls keep every timestamp strictly increasing, so
            // only the work-cost relation can refuse them.
            // Double work still costs exactly its two halves here, so only the
            // equal-cost relation can refuse this one.
            (
                "equal work, unequal cost",
                edit_clock_trajectory(&[
                    (
                        11,
                        "work gen=0 index=2 iterations=100000 before_ns=1767225600004437485 after_ns=1767225600006447536 checksum=4",
                    ),
                    (
                        12,
                        "work gen=0 index=3 iterations=200000 before_ns=1767225600006461575 after_ns=1767225600010471626 checksum=2",
                    ),
                ]),
                cost,
            ),
            (
                "double work, single cost",
                edit_clock_trajectory(&[(
                    12,
                    "work gen=0 index=3 iterations=200000 before_ns=1767225600006461575 after_ns=1767225600008471625 checksum=2",
                )]),
                cost,
            ),
            // Costs more than one unit of work, but not exactly two.
            (
                "double work, non-additive cost",
                edit_clock_trajectory(&[(
                    12,
                    "work gen=0 index=3 iterations=200000 before_ns=1767225600006461575 after_ns=1767225600009471625 checksum=2",
                )]),
                cost,
            ),
            // A clock that ignores work: every segment costs the empty one's
            // 10,050 ns, which is equal and additive, so only the grows-with-work
            // relation can refuse it.
            (
                "work costs nothing",
                edit_clock_trajectory(&[
                    (
                        10,
                        "work gen=0 index=1 iterations=100000 before_ns=1767225600002413395 after_ns=1767225600002423445 checksum=4",
                    ),
                    (
                        11,
                        "work gen=0 index=2 iterations=100000 before_ns=1767225600004437485 after_ns=1767225600004447535 checksum=4",
                    ),
                    (
                        12,
                        "work gen=0 index=3 iterations=200000 before_ns=1767225600006461575 after_ns=1767225600006471625 checksum=2",
                    ),
                ]),
                cost,
            ),
            (
                "wrong checksum",
                edit_clock_trajectory(&[(
                    10,
                    "work gen=0 index=1 iterations=100000 before_ns=1767225600002413395 after_ns=1767225600004423445 checksum=5",
                )]),
                "checksum 5 is wrong",
            ),
            (
                "changed iterations",
                edit_clock_trajectory(&[(
                    10,
                    "work gen=0 index=1 iterations=100001 before_ns=1767225600002413395 after_ns=1767225600004423445 checksum=4",
                )]),
                "expected \"work gen=0 index=1 iterations=100000\"",
            ),
            (
                "missing sample",
                without(4),
                "expected \"sample gen=0 source=main index=4\"",
            ),
            (
                "extra sample",
                edit_clock_trajectory(&[(
                    8,
                    "sample gen=0 source=main index=8 ns=1767225600002323355",
                )]),
                "expected \"realtime gen=0 phase=open\"",
            ),
            (
                "record after verdict",
                format!("{text}extra\n"),
                "extra record",
            ),
            (
                "threads reordered",
                reordered.join("\n") + "\n",
                "expected \"sample gen=0 source=thread index=0 thread=0\"",
            ),
            (
                "leading zero",
                edit_clock_trajectory(&[(
                    0,
                    "sample gen=0 source=main index=0 ns=01767225600002243035",
                )]),
                "malformed ns",
            ),
            (
                "signed",
                edit_clock_trajectory(&[(
                    0,
                    "sample gen=0 source=main index=0 ns=+1767225600002243035",
                )]),
                "malformed ns",
            ),
            (
                "empty",
                edit_clock_trajectory(&[(0, "sample gen=0 source=main index=0 ns=")]),
                "malformed ns",
            ),
            (
                "overflow",
                edit_clock_trajectory(&[(
                    0,
                    "sample gen=0 source=main index=0 ns=99999999999999999999",
                )]),
                "malformed ns",
            ),
            (
                "trailing space",
                edit_clock_trajectory(&[(
                    0,
                    "sample gen=0 source=main index=0 ns=1767225600002243035 ",
                )]),
                "trailing text",
            ),
            (
                "carriage returns",
                text.replace('\n', "\r\n"),
                "trailing text",
            ),
            (
                "summary",
                edit_clock_trajectory(&[(
                    22,
                    "gen=0 first=1767225600002243035 last=1767225600013859655 min_delta=10041",
                )]),
                "summary disagrees",
            ),
            (
                "no final newline",
                text[..text.len() - 1].to_owned(),
                "one newline",
            ),
            ("no verdict", without(69), "lacks \"clock exec continuity"),
        ];
        for (name, mutated, expected) in cases {
            let error = parse_clock_trajectory(&mutated).expect_err(name);
            assert!(
                error.contains(expected),
                "{name}: expected {expected:?}, got {error:?}"
            );
        }
    }

    /// Linux gives CLOCK_MONOTONIC an unspecified origin, so a trajectory whose
    /// MONOTONIC reads are nowhere near the epoch, or jump across an exec, is
    /// valid; only the REALTIME anchors are held to the epoch window.
    #[test]
    fn clock_trajectory_holds_only_realtime_to_the_epoch() {
        let text = CAPTURED_CLOCK_TRAJECTORY;
        // About 600 s after boot, as on a real host; whole seconds keep every
        // fractional part, and so the round-origin legs, unchanged.
        let boot_relative = shift_monotonic_from(text, -1_767_225_000_000_000_000, 0);
        assert!(boot_relative.starts_with("sample gen=0 source=main index=0 ns=600002243035\n"));
        assert_eq!(
            parse_clock_trajectory(&boot_relative),
            Ok(CAPTURED_TRAJECTORY)
        );
        // A MONOTONIC reading far past the epoch window is equally valid.
        let far = shift_monotonic_from(text, 86_400_000_000_000, 0);
        assert_eq!(parse_clock_trajectory(&far), Ok(CAPTURED_TRAJECTORY));
        // MONOTONIC and REALTIME need not move together across exec.
        let jump = shift_monotonic_from(text, 10_000_000_000, 23);
        assert_eq!(
            parse_clock_trajectory(&jump),
            Ok(ClockTrajectory {
                realtime_last: CAPTURED_REALTIME_LAST,
                monotonic_span: CAPTURED_MONOTONIC_SPAN + 10_000_000_000,
            })
        );
        // The same shifts applied to REALTIME are refused.
        for (name, shifted, expected) in [
            (
                "boot-relative REALTIME",
                shift_realtime(text, -1_767_225_000_000_000_000),
                "CLOCK_REALTIME read",
            ),
            (
                "far REALTIME",
                shift_realtime(text, 86_400_000_000_000),
                "outside the startup bound",
            ),
        ] {
            let error = parse_clock_trajectory(&shifted).expect_err(name);
            assert!(
                error.contains(expected),
                "{name}: expected {expected:?}, got {error:?}"
            );
        }
        // Continuity stays mandatory for a boot-relative MONOTONIC clock.
        let frozen = boot_relative.replacen(
            "sample gen=0 source=main index=1 ns=600002253075",
            "sample gen=0 source=main index=1 ns=600002243035",
            1,
        );
        assert_ne!(frozen, boot_relative);
        let error = parse_clock_trajectory(&frozen).unwrap_err();
        assert!(error.contains("CLOCK_MONOTONIC read"), "{error}");
    }

    #[test]
    fn clock_parity_refuses_any_cross_build_difference_including_virtual_time() {
        let root = fixture_root("clock-parity");
        fs::create_dir(&root).unwrap();
        let bytes = CAPTURED_CLOCK_TRAJECTORY.as_bytes();
        let report = |name: &str, value: Value| write_report(&root, name, &value);
        let cargo = report("cargo.json", clock_runtime_report(CAPTURED_VIRTUAL_NS));
        verify_clock_parity(bytes, bytes, &cargo, &cargo).unwrap();
        let shifted = shift_clock_trajectory(CAPTURED_CLOCK_TRAJECTORY, 10);
        let mut bare = clock_runtime_report(CAPTURED_VIRTUAL_NS);
        bare.as_object_mut().unwrap().remove("runtime");
        let mut changed = Vec::new();
        for (field, value) in [
            // The value the old comparator zeroed before comparing.
            ("virtual_nanoseconds", 48_690_237),
            ("scheduler_turns", 37),
            ("syscalls", 257),
        ] {
            let mut value_report = clock_runtime_report(CAPTURED_VIRTUAL_NS);
            for run in ["run1", "run2"] {
                value_report["runtime"][run][field] = value.into();
            }
            changed.push(value_report);
        }
        let mut counts = clock_runtime_report(CAPTURED_VIRTUAL_NS);
        counts["compared_log_messages"] = serde_json::json!({"left": 13, "right": 13});
        changed.push(counts);
        for buck in changed {
            let buck = report("buck.json", buck);
            let error = verify_clock_parity(bytes, bytes, &cargo, &buck).unwrap_err();
            assert!(error.contains("strict reports differ"), "{error}");
        }
        // A MONOTONIC trajectory that advanced 10 s more than the run's
        // virtual time lasted, with the REALTIME anchors unchanged, is refused
        // only by the span binding.
        let jump = shift_monotonic_from(CAPTURED_CLOCK_TRAJECTORY, 10_000_000_000, 23);
        let jump_report = report(
            "jump.json",
            clock_runtime_report_for(&jump, CAPTURED_VIRTUAL_NS),
        );
        let boot = shift_monotonic_from(CAPTURED_CLOCK_TRAJECTORY, -1_767_225_000_000_000_000, 0);
        let boot_report = report(
            "boot.json",
            clock_runtime_report_for(&boot, CAPTURED_VIRTUAL_NS),
        );
        verify_clock_parity(boot.as_bytes(), boot.as_bytes(), &boot_report, &boot_report).unwrap();
        let realtime_end = CAPTURED_REALTIME_LAST - CLOCK_PARITY_EPOCH_NS;
        for (cargo_bytes, buck_bytes, cargo, expected) in [
            (bytes, shifted.as_bytes(), &cargo, "clock trajectory"),
            (
                shifted.as_bytes(),
                shifted.as_bytes(),
                &cargo,
                "compared a different stdout",
            ),
            (
                bytes,
                bytes,
                &report("short.json", clock_runtime_report(realtime_end)),
                "ends before the last REALTIME anchor",
            ),
            (
                jump.as_bytes(),
                jump.as_bytes(),
                &jump_report,
                "shorter than the MONOTONIC span",
            ),
            (
                bytes,
                bytes,
                &report("bare.json", bare),
                "lacks a runtime summary",
            ),
        ] {
            let error = verify_clock_parity(cargo_bytes, buck_bytes, cargo, cargo).unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error:?}"
            );
        }
        // The boundary is strict: one nanosecond past the anchor is enough.
        let edge = report("edge.json", clock_runtime_report(realtime_end + 1));
        verify_clock_parity(bytes, bytes, &edge, &edge).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn strict_report_with_unequal_run1_run2_is_refused() {
        let root = fixture_root("unequal-runs");
        fs::create_dir(&root).unwrap();
        let bytes = CAPTURED_CLOCK_TRAJECTORY.as_bytes();
        for (field, value) in [
            ("virtual_nanoseconds", CAPTURED_VIRTUAL_NS + 10),
            ("scheduler_turns", 37),
            ("syscalls", 257),
        ] {
            let mut unequal = clock_runtime_report(CAPTURED_VIRTUAL_NS);
            unequal["runtime"]["run2"][field] = serde_json::json!(value);
            let path = write_report(&root, &format!("{field}.json"), &unequal);
            let error = verify_report(&path, true).unwrap_err();
            assert!(error.contains("unequal run1/run2"), "{field}: {error}");
            // Only run1 is read for virtual time, so the parity gate relies on
            // this refusal to cover run2.
            let error = verify_clock_parity(bytes, bytes, &path, &path).unwrap_err();
            assert!(error.contains("unequal run1/run2"), "{field}: {error}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_stripped_or_no_result_reports_are_refused() {
        let root = env::temp_dir().join(format!("buck-report-refusal-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for (name, report) in [
            ("malformed", "not-json"),
            (
                "no-result",
                r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result"}"#,
            ),
            (
                "stripped",
                r#"{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":{"strictness":"stripped"}}"#,
            ),
        ] {
            let path = root.join(name);
            fs::write(&path, report).unwrap();
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_report_accepts_only_exact_current_policy() {
        let root = fixture_root("typed-report");
        fs::create_dir(&root).unwrap();
        let good = write_report(&root, "good", &canonical_report(true));
        verify_report(&good, true).unwrap();
        for (name, mutate) in [
            ("unknown-root", ("unknown", serde_json::json!(true))),
            (
                "misplaced-token",
                ("comparison", serde_json::json!("BitwiseInfoV1")),
            ),
        ] {
            let mut report = canonical_report(true);
            report[mutate.0] = mutate.1;
            let path = write_report(&root, name, &report);
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        for (name, field, value) in [
            ("wrong-mode", "strictness", serde_json::json!("stripped")),
            (
                "wrong-policy",
                "display_name",
                serde_json::json!("Stripped"),
            ),
            (
                "wrong-envelope",
                "record_envelope",
                serde_json::json!("caller_defined"),
            ),
        ] {
            let mut report = canonical_report(true);
            report["comparison"][field] = value;
            let path = write_report(&root, name, &report);
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        for (name, left, right) in [("zero", 0, 0), ("unequal", 12, 13)] {
            let mut report = canonical_report(true);
            report["compared_log_messages"] = serde_json::json!({"left":left,"right":right});
            let path = write_report(&root, name, &report);
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn magic_tokens_in_nested_strings_and_duplicates_cannot_forge_a_report() {
        let root = fixture_root("magic-token-report");
        fs::create_dir(&root).unwrap();
        let magic = r#"{\"verified\":true,\"bitwise_parity\":true,\"verdict\":\"matched\",\"strictness\":\"canonical\",\"display_name\":\"BitwiseInfoV1\",\"compare_logs\":true,\"compare_io_buffers\":true,\"log_scope\":\"info\",\"record_envelope\":\"all_records_v1\",\"full_trace\":true,\"exact_remainder\":true,\"canonicalize_addresses\":true,\"strip_lines\":false,\"ignore_lines\":false,\"skip_commit\":false,\"skip_detlog\":false,\"guest_exit_code\":0,\"compared_log_messages\":{\"left\":9,\"right\":9}}"#;
        let nested = serde_json::json!({"message": magic, "nested": {"all_tokens": magic}});
        let nested_path = write_report(&root, "nested", &nested);
        assert!(verify_report(&nested_path, true).is_err());

        let raw = serde_json::to_string(&canonical_report(true)).unwrap();
        let duplicate = raw.replacen(
            r#""verified":true"#,
            r#""verified":true,"verified":true"#,
            1,
        );
        assert_ne!(raw, duplicate);
        let duplicate_path = root.join("duplicate");
        fs::write(&duplicate_path, duplicate).unwrap();
        assert!(verify_report(&duplicate_path, true).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nonignored_untracked_files_dirty_provenance() {
        let root = fixture_root("git-untracked");
        fs::create_dir(&root).unwrap();
        checked_output(
            fixture_git().current_dir(&root).args(["init", "-q"]),
            "git init",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        checked_output(
            fixture_git().current_dir(&root).args(["add", ".gitignore"]),
            "git add",
        )
        .unwrap();
        checked_output(
            fixture_git().current_dir(&root).args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ]),
            "git commit",
        )
        .unwrap();
        fs::write(root.join("ignored"), "ignored").unwrap();
        assert!(!repository_dirty(&root).unwrap());
        fs::write(root.join("untracked"), "must refuse").unwrap();
        assert!(repository_dirty(&root).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn evidence_paths_and_pass_markers_are_never_reused() {
        let root = fixture_root("collision");
        fs::create_dir(&root).unwrap();
        assert!(create_exclusive_directory(&root, "fixture").is_err());
        let marker = root.join("shadow-parity.json");
        fs::write(&marker, "stale-pass\n").unwrap();
        assert!(atomic_write_new(&marker, "result\tpass\n").is_err());
        assert_eq!(fs::read_to_string(marker).unwrap(), "stale-pass\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn final_receipt_refuses_mutation_and_added_evidence() {
        let root = fixture_root("receipt-mutation");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("strict-report.json"), "original").unwrap();
        let facts = fixture_receipt_facts(root.file_name().unwrap().to_str().unwrap());
        let resources = fixture_resources();
        let receipt = write_fixture_receipt(&root, &facts, &resources);
        verify_fixture_receipt(&root, &receipt, &facts, &resources).unwrap();
        fs::write(root.join("strict-report.json"), "mutated").unwrap();
        assert!(verify_fixture_receipt(&root, &receipt, &facts, &resources).is_err());
        fs::write(root.join("strict-report.json"), "original").unwrap();
        fs::write(root.join("stale-pass-marker"), "pass").unwrap();
        assert!(verify_fixture_receipt(&root, &receipt, &facts, &resources).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn final_receipt_schema_refuses_semantic_mutation_duplicate_unknown_and_deletion() {
        let facts = fixture_receipt_facts("fixture-evidence");
        let resources = fixture_resources();
        let artifacts = BTreeMap::from([("strict-report.json".to_owned(), "c".repeat(64))]);
        let good = render_final_receipt(&facts, &resources, &artifacts).unwrap();
        let parsed = parse_final_receipt(&good).unwrap();
        verify_receipt_semantics(&parsed, &facts, &resources, &artifacts).unwrap();

        let mut mutated_value = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        mutated_value["facts"]["cargo_candidate_sha256"] =
            serde_json::Value::String("d".repeat(64));
        let mutated_hash = serde_json::to_string(&mutated_value).unwrap();
        let parsed_mutation = parse_final_receipt(&mutated_hash).unwrap();
        assert!(
            verify_receipt_semantics(&parsed_mutation, &facts, &resources, &artifacts).is_err()
        );

        for (field, value) in [
            (
                "matrix_candidate_manifest",
                serde_json::json!("renamed-manifest.json"),
            ),
            (
                "matrix_candidate_manifest_sha256",
                serde_json::json!("not-a-sha256"),
            ),
            ("cargo_matrix_candidate_invocations", serde_json::json!(0)),
            ("buck_matrix_candidate_invocations", serde_json::json!(3)),
            (
                "matrix_expected_ledger",
                serde_json::json!("renamed-ledger.json"),
            ),
            (
                "matrix_expected_ledger_sha256",
                serde_json::json!("not-a-sha256"),
            ),
            ("matrix_expected_invocation_count", serde_json::json!(0)),
        ] {
            let mut invalid = serde_json::from_str::<serde_json::Value>(&good).unwrap();
            invalid["facts"][field] = value;
            assert!(parse_final_receipt(&serde_json::to_string(&invalid).unwrap()).is_err());
        }

        let duplicate = good.replacen(
            "\"result\": \"pass\"",
            "\"result\": \"pass\", \"result\": \"pass\"",
            1,
        );
        assert_ne!(good, duplicate);
        assert!(parse_final_receipt(&duplicate).is_err());
        let unknown = good.replacen('{', "{\"future_semantic\":true,", 1);
        assert!(parse_final_receipt(&unknown).is_err());
        let mut deleted_value = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        deleted_value["facts"]
            .as_object_mut()
            .unwrap()
            .remove("safehermit_sha256");
        assert!(parse_final_receipt(&serde_json::to_string(&deleted_value).unwrap()).is_err());
        let mut deleted_manifest = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        deleted_manifest["facts"]
            .as_object_mut()
            .unwrap()
            .remove("matrix_candidate_manifest");
        assert!(parse_final_receipt(&serde_json::to_string(&deleted_manifest).unwrap()).is_err());
        let mut deleted_ledger = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        deleted_ledger["facts"]
            .as_object_mut()
            .unwrap()
            .remove("matrix_expected_ledger");
        assert!(parse_final_receipt(&serde_json::to_string(&deleted_ledger).unwrap()).is_err());
        let duplicate_resource = good.replacen(
            &format!("\"rsrcs/fixture\": \"{}\"", "b".repeat(64)),
            &format!(
                "\"rsrcs/fixture\": \"{}\", \"rsrcs/fixture\": \"{}\"",
                "b".repeat(64),
                "b".repeat(64)
            ),
            1,
        );
        assert_ne!(good, duplicate_resource);
        assert!(parse_final_receipt(&duplicate_resource).is_err());
    }

    #[test]
    fn safehermit_report_requires_applied_bounds() {
        let root = fixture_root("safehermit-report");
        fs::create_dir(&root).unwrap();
        let report = root.join("report");
        fs::write(
            &report,
            "safehermit: bound.wall=APPLIED:120s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=1G\nsafehermit: bound.disk=APPLIED:8G for log_dir only\nsafehermit: bound.bytes=APPLIED:67108864\nsafehermit: bound.logfilter=NOT_APPLIED:by owner ruling\n",
        )
        .unwrap();
        validate_and_surface_safehermit_report(&report, "fixture").unwrap();
        for missing in ["wall", "cgroup", "disk", "bytes"] {
            let text = fs::read_to_string(&report).unwrap();
            fs::write(
                &report,
                text.replace(
                    &format!("safehermit: bound.{missing}=APPLIED:"),
                    &format!("safehermit: bound.{missing}=NOT_APPLIED:"),
                ),
            )
            .unwrap();
            assert!(validate_and_surface_safehermit_report(&report, "fixture").is_err());
            fs::write(&report, text).unwrap();
        }
        fs::write(
            &report,
            "safehermit: bound.wall=APPLIED:120s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=1G\nsafehermit: bound.disk=APPLIED:8G\nsafehermit: bound.bytes=APPLIED:67108864\nsafehermit: bound.future=NOT_APPLIED:test\n",
        )
        .unwrap();
        assert!(validate_and_surface_safehermit_report(&report, "fixture").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn candidate_invocation_reports_require_exact_identities_population_and_applied_bounds() {
        let root = fixture_root("candidate-invocation-receipts");
        let invocations = root.join("candidate-invocations");
        fs::create_dir_all(&invocations).unwrap();
        for identity in EXPECTED_CANDIDATE_INVOCATIONS {
            let invocation = invocations.join(format!("{identity}-fixture"));
            fs::create_dir(&invocation).unwrap();
            fs::write(invocation.join("stdout"), b"stdout\n").unwrap();
            fs::write(invocation.join("stderr"), b"stderr\n").unwrap();
            fs::write(
                invocation.join("safehermit.report"),
                b"safehermit: bound.wall=APPLIED:10s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=1G\nsafehermit: bound.disk=APPLIED:8G\nsafehermit: bound.bytes=APPLIED:67108864\nsafehermit: bound.logfilter=NOT_APPLIED:by owner ruling\n",
            )
            .unwrap();
        }
        verify_candidate_invocation_reports(&root).unwrap();

        let report = invocations.join("buck-help-fixture/safehermit.report");
        let original = fs::read(&report).unwrap();
        fs::write(
            &report,
            b"safehermit: bound.wall=NOT_APPLIED:test\nsafehermit: bound.cgroup=APPLIED:MemoryMax=1G\nsafehermit: bound.disk=APPLIED:8G\nsafehermit: bound.bytes=APPLIED:67108864\nsafehermit: bound.logfilter=NOT_APPLIED:by owner ruling\n",
        )
        .unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::write(&report, &original).unwrap();

        fs::remove_file(&report).unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::write(&report, &original).unwrap();

        let extra = invocations.join("unexpected-probe-fixture");
        fs::create_dir(&extra).unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::remove_dir(&extra).unwrap();

        fs::write(invocations.join("cargo-help-fixture/extra"), b"extra\n").unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn official_matrix_expected_ledger_and_proxy_classification_are_exact() {
        let repository = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let ledger = derive_official_matrix_expected_ledger(&repository).unwrap();
        assert_eq!(ledger.cases.len(), 28);
        assert_eq!(ledger.cases.iter().filter(|case| case.selected).count(), 27);
        assert_eq!(
            ledger
                .cases
                .iter()
                .filter(|case| case.requires_ptrace_reference)
                .count(),
            23
        );
        assert_eq!(
            ledger
                .expected_invocations
                .iter()
                .map(|entry| entry.count)
                .sum::<usize>(),
            106
        );

        let host = classify_matrix_proxy_invocation(&[
            "host-capabilities".to_owned(),
            "--json".to_owned(),
        ])
        .unwrap();
        assert_eq!(host.case_identity, "probe/host-capabilities");
        let smoke = classify_matrix_proxy_invocation(&[
            "--backend".to_owned(),
            "dbt".to_owned(),
            "run".to_owned(),
            "--strict".to_owned(),
            "--".to_owned(),
            "/bin/true".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            (smoke.case_identity.as_str(), smoke.role.as_str()),
            ("probe/dbt-smoke", "probe")
        );
        let exit_zero = classify_matrix_proxy_invocation(&[
            "--backend".to_owned(),
            "dbt".to_owned(),
            "run".to_owned(),
            "--strict".to_owned(),
            "--max-timeslice=disabled".to_owned(),
            "--".to_owned(),
            "/bin/true".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            (exit_zero.case_identity.as_str(), exit_zero.role.as_str()),
            ("exit_zero", "dbt-run")
        );
        assert_eq!(
            normalize_matrix_argument(
                "/tmp/hermit-backend-parity-one/host-tmp/ptrace-exit_zero-reference-1"
            ),
            normalize_matrix_argument(
                "/tmp/hermit-backend-parity-two/host-tmp/ptrace-exit_zero-reference-1"
            )
        );
        assert!(
            classify_matrix_proxy_invocation(&[
                "--backend".to_owned(),
                "dbt".to_owned(),
                "run".to_owned(),
                "--max-timeslice=disabled".to_owned(),
                "--".to_owned(),
                "/unknown/fixture".to_owned(),
            ])
            .is_err()
        );
    }

    fn private_tmp_stage(host_tmp: &Path, binds: &[(&Path, &Path)]) -> Vec<String> {
        let mut stage = [
            "unshare",
            "--user",
            "--map-root-user",
            "--mount",
            "sh",
            "-ceu",
            DBT_PRIVATE_TMP_SCRIPT,
            "hermit-dbt-private-tmp",
        ]
        .map(str::to_owned)
        .to_vec();
        stage.push(host_tmp.display().to_string());
        stage.push(binds.len().to_string());
        for (source, destination) in binds {
            stage.push(source.display().to_string());
            stage.push(destination.display().to_string());
        }
        stage
    }

    fn private_tmp_handoff(stage: &[String], hermit: &[&str]) -> Vec<String> {
        [
            MATRIX_PRIVATE_TMP_HANDOFF.to_owned(),
            stage.len().to_string(),
        ]
        .into_iter()
        .chain(stage.iter().cloned())
        .chain(hermit.iter().map(|argument| (*argument).to_owned()))
        .collect()
    }

    #[test]
    fn matrix_private_tmp_stage_is_run_matrix_stage() {
        let run_matrix = fs::read_to_string(
            Path::new(file!())
                .parent()
                .and_then(Path::parent)
                .unwrap()
                .join("tests/backend-parity/run_matrix.py"),
        )
        .unwrap();
        let script = run_matrix
            .split_once("DBT_PRIVATE_TMP_SCRIPT = \"\"\"\\\n")
            .and_then(|(_, rest)| rest.split_once("\"\"\""))
            .map(|(script, _)| script)
            .unwrap();
        assert_eq!(script, DBT_PRIVATE_TMP_SCRIPT);
        assert!(run_matrix.contains("MATRIX_PRIVATE_TMP_HANDOFF = \"--matrix-private-tmp\""));
        assert!(run_matrix.contains("MATRIX_PROXY_ENV = \"HERMIT_BUCK_PHASE1_MATRIX_PROXY\""));
    }

    #[test]
    fn matrix_private_tmp_handoff_accepts_only_run_matrix_stage() {
        let root = fs::canonicalize({
            let root = fixture_root("matrix-private-tmp");
            fs::create_dir(&root).unwrap();
            root
        })
        .unwrap();
        let host_tmp = root.join("host-tmp");
        let source = root.join("verify-output");
        let destination = host_tmp.join("verify-output");
        let other = root.join("other");
        for directory in [&host_tmp, &source, &destination, &other] {
            fs::create_dir_all(directory).unwrap();
        }
        let hermit = [
            "--backend",
            "dbt",
            "run",
            "--max-timeslice=disabled",
            "--",
            "/bin/true",
        ];
        let stage = private_tmp_stage(&host_tmp, &[(&source, &destination)]);
        let (parsed, rest) =
            split_private_tmp_handoff(private_tmp_handoff(&stage, &hermit)).unwrap();
        assert_eq!(
            parsed,
            Some(PrivateTmpStage {
                host_tmp: host_tmp.clone(),
                mounts: vec![(source.clone(), destination.clone())],
                argv: stage.clone(),
            })
        );
        assert_eq!(rest, hermit);
        let plain = hermit.map(str::to_owned).to_vec();
        assert_eq!(
            split_private_tmp_handoff(plain.clone()).unwrap(),
            (None, plain)
        );
        let (unbound, _) = split_private_tmp_handoff(private_tmp_handoff(
            &private_tmp_stage(&host_tmp, &[]),
            &hermit,
        ))
        .unwrap();
        assert_eq!(unbound.unwrap().mounts, Vec::new());

        let refused = |stage: Vec<String>, needle: &str| {
            let error =
                split_private_tmp_handoff(private_tmp_handoff(&stage, &hermit)).unwrap_err();
            assert!(error.contains(needle), "{needle:?} not in {error}");
        };
        let mut script = stage.clone();
        script[6] = script[6].replace("--rbind", "--bind");
        refused(script, "not run_matrix's unshare");
        let mut mapping = stage.clone();
        mapping[2] = "--map-current-user".to_owned();
        refused(mapping, "not run_matrix's unshare");
        let mut count = stage.clone();
        count[9] = "2".to_owned();
        refused(count, "declares 2 binds but carries 2 paths");
        let mut signed = stage.clone();
        signed[9] = "+1".to_owned();
        refused(signed, "is not decimal");
        refused(
            private_tmp_stage(&host_tmp, &[(&source, &other)]),
            "not strictly beneath",
        );
        refused(
            private_tmp_stage(&host_tmp, &[(&source, &host_tmp)]),
            "not strictly beneath",
        );
        refused(
            private_tmp_stage(
                &host_tmp,
                &[(&source, &destination), (&other, &destination)],
            ),
            "twice",
        );
        refused(
            private_tmp_stage(&host_tmp, &[(&root.join("missing"), &destination)]),
            "is unreadable",
        );
        let alias = root.join("alias");
        symlink(&host_tmp, &alias).unwrap();
        refused(
            private_tmp_stage(&alias, &[]),
            "not an absolute canonical path",
        );
        refused(
            private_tmp_stage(&host_tmp, &[(&source, &alias.join("verify-output"))]),
            "not an absolute canonical path",
        );

        let mut truncated = private_tmp_handoff(&stage, &[]);
        truncated[1] = (stage.len() + 1).to_string();
        assert!(
            split_private_tmp_handoff(truncated)
                .unwrap_err()
                .contains("truncated")
        );
        let mut malformed = private_tmp_handoff(&stage, &hermit);
        malformed[1] = "x".to_owned();
        assert!(
            split_private_tmp_handoff(malformed)
                .unwrap_err()
                .contains("decimal stage length")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn matrix_proxy_refuses_to_launch_safehermit_inside_a_user_namespace() {
        require_initial_user_namespace("         0          0 4294967295\n").unwrap();
        for nested in [
            "         0     212630          1\n",
            "",
            "0 0 4294967295\n0 0 1\n",
        ] {
            assert!(
                require_initial_user_namespace(nested)
                    .unwrap_err()
                    .contains("initial user namespace")
            );
        }
        // The exact shape run_matrix's private-/tmp wrapper would put the proxy in.
        let nested = Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "cat",
                "/proc/self/uid_map",
            ])
            .output()
            .unwrap();
        assert!(nested.status.success(), "{nested:?}");
        let nested = String::from_utf8(nested.stdout).unwrap();
        assert!(
            require_initial_user_namespace(&nested).is_err(),
            "{nested:?}"
        );

        // The refusal is the proxy's first act: nothing else is read or written.
        let root = fixture_root("matrix-proxy-nested-namespace");
        fs::create_dir(&root).unwrap();
        let uid_map = root.join("uid_map");
        fs::write(&uid_map, &nested).unwrap();
        let error =
            matrix_candidate_proxy_from(&uid_map, ["run".to_owned()].into_iter()).unwrap_err();
        assert!(error.contains("initial user namespace"), "{error}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn matrix_private_tmp_readback_must_match_host_directory_and_mapping() {
        let expected = "tmp=48:77\ncwd=/tmp\ntmpdir=/tmp\nuid_map=0 1000 1\n";
        require_private_tmp_readback(expected, 48, 77, 1000).unwrap();
        for observed in [
            "tmp=48:78\ncwd=/tmp\ntmpdir=/tmp\nuid_map=0 1000 1\n",
            "tmp=48:77\ncwd=/home\ntmpdir=/tmp\nuid_map=0 1000 1\n",
            "tmp=48:77\ncwd=/tmp\ntmpdir=/tmp/x\nuid_map=0 1000 1\n",
            "tmp=48:77\ncwd=/tmp\ntmpdir=/tmp\nuid_map=0 0 4294967295\n",
            "",
        ] {
            assert!(
                require_private_tmp_readback(observed, 48, 77, 1000).is_err(),
                "{observed:?}"
            );
        }

        // Execute the stage and readback exactly as the proxy composes them.
        let root = fs::canonicalize({
            let root = fixture_root("matrix-private-tmp-readback");
            fs::create_dir(&root).unwrap();
            root
        })
        .unwrap();
        let host_tmp = root.join("host-tmp");
        fs::create_dir(&host_tmp).unwrap();
        let evidence = root.join("evidence");
        fs::create_dir(&evidence).unwrap();
        let readback = evidence.join("private-tmp.readback");
        // A fixture beneath /tmp is hidden by the stage unless bound through,
        // exactly as run_matrix preserves its own /tmp paths.
        let preserved = evidence
            .strip_prefix("/tmp")
            .map(|relative| host_tmp.join(relative))
            .ok();
        if let Some(destination) = &preserved {
            fs::create_dir_all(destination).unwrap();
        }
        let binds = preserved
            .iter()
            .map(|destination| (evidence.as_path(), destination.as_path()))
            .collect::<Vec<_>>();
        let stage = private_tmp_stage(&host_tmp, &binds);
        let output = Command::new(&stage[0])
            .args(&stage[1..])
            .args([
                "/bin/sh",
                "-ceu",
                PRIVATE_TMP_READBACK_SCRIPT,
                "hermit-private-tmp-readback",
            ])
            .arg(&readback)
            .args(["/bin/sh", "-c", "printf candidate > /tmp/marker"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let host = fs::metadata(&host_tmp).unwrap();
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let observed = fs::read_to_string(&readback).unwrap();
        // uid_map names the parent namespace's uid, which is this test's own.
        require_private_tmp_readback(&observed, host.dev(), host.ino(), uid).unwrap();
        assert_eq!(
            fs::read_to_string(host_tmp.join("marker")).unwrap(),
            "candidate"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn matrix_completeness_refuses_symmetric_deletion_argv_mismatch_and_duplicate() {
        let repository = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let evidence = fixture_root("matrix-completeness-ledger");
        fs::create_dir(&evidence).unwrap();
        let ledger = derive_official_matrix_expected_ledger(&repository).unwrap();
        atomic_write_new(
            &evidence.join("matrix-expected-ledger.json"),
            &render_official_matrix_expected_ledger(&ledger).unwrap(),
        )
        .unwrap();
        let header = "test_name\tbackend\texpectation\tresult\tseconds\tdetail\n";
        let rows = ledger
            .cases
            .iter()
            .map(|case| {
                format!(
                    "{}\tdbt\t{}\t{}\t0.001\tfixture\n",
                    case.test_name,
                    case.expectation,
                    if case.selected { "PASS" } else { "GAP" }
                )
            })
            .collect::<String>();
        for label in ["cargo", "buck"] {
            fs::write(
                evidence.join(format!("{label}-dbt-matrix.tsv")),
                format!("{header}{rows}"),
            )
            .unwrap();
        }
        let hash = "a".repeat(64);
        let mut invocations = Vec::new();
        for label in ["cargo", "buck"] {
            let mut serial = 0;
            for expected in &ledger.expected_invocations {
                for _ in 0..expected.count {
                    serial += 1;
                    invocations.push(MatrixCandidateInvocationBinding {
                        candidate_label: label.to_owned(),
                        identity: format!("fixture-{serial}"),
                        case_identity: expected.case_identity.clone(),
                        role: expected.role.clone(),
                        normalized_argv: vec![
                            expected.case_identity.clone(),
                            expected.role.clone(),
                        ],
                        invocation_metadata_sha256: hash.clone(),
                        safehermit_report_sha256: hash.clone(),
                        stdout_sha256: hash.clone(),
                        stderr_sha256: hash.clone(),
                        private_tmp_sha256: (expected.role == "dbt-run").then(|| hash.clone()),
                        private_tmp_readback_sha256: (expected.role == "dbt-run")
                            .then(|| hash.clone()),
                        data_sha256: BTreeMap::new(),
                        data_directories: Vec::new(),
                    });
                }
            }
        }
        let manifest = MatrixCandidateInvocationManifest {
            manifest_schema: MATRIX_CANDIDATE_INVOCATIONS_SCHEMA.to_owned(),
            invocations,
        };
        validate_matrix_candidate_completeness(&repository, &evidence, &manifest).unwrap();

        let mut symmetric_missing = manifest.clone();
        let missing_case = symmetric_missing.invocations[0].case_identity.clone();
        let missing_role = symmetric_missing.invocations[0].role.clone();
        for label in ["cargo", "buck"] {
            let index = symmetric_missing
                .invocations
                .iter()
                .position(|invocation| {
                    invocation.candidate_label == label
                        && invocation.case_identity == missing_case
                        && invocation.role == missing_role
                })
                .unwrap();
            symmetric_missing.invocations.remove(index);
        }
        assert!(
            validate_matrix_candidate_completeness(&repository, &evidence, &symmetric_missing)
                .is_err()
        );

        let mut argv_mismatch = manifest.clone();
        argv_mismatch
            .invocations
            .iter_mut()
            .find(|invocation| invocation.candidate_label == "buck")
            .unwrap()
            .normalized_argv
            .push("--mismatch".to_owned());
        assert!(
            validate_matrix_candidate_completeness(&repository, &evidence, &argv_mismatch).is_err()
        );

        let mut symmetric_case_mismatch = manifest.clone();
        for label in ["cargo", "buck"] {
            symmetric_case_mismatch
                .invocations
                .iter_mut()
                .find(|invocation| invocation.candidate_label == label)
                .unwrap()
                .case_identity = "correlated-wrong-case".to_owned();
        }
        assert!(
            validate_matrix_candidate_completeness(
                &repository,
                &evidence,
                &symmetric_case_mismatch,
            )
            .is_err()
        );

        let mut duplicate = manifest.clone();
        for label in ["cargo", "buck"] {
            let mut extra = duplicate
                .invocations
                .iter()
                .find(|invocation| invocation.candidate_label == label)
                .unwrap()
                .clone();
            extra.identity.push_str("-duplicate");
            duplicate.invocations.push(extra);
        }
        assert!(
            validate_matrix_candidate_completeness(&repository, &evidence, &duplicate).is_err()
        );

        let first_row = rows.lines().next().unwrap();
        let missing_rows = rows
            .lines()
            .filter(|line| *line != first_row)
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        for label in ["cargo", "buck"] {
            fs::write(
                evidence.join(format!("{label}-dbt-matrix.tsv")),
                format!("{header}{missing_rows}"),
            )
            .unwrap();
        }
        assert!(validate_matrix_candidate_completeness(&repository, &evidence, &manifest).is_err());
        for label in ["cargo", "buck"] {
            fs::write(
                evidence.join(format!("{label}-dbt-matrix.tsv")),
                format!("{header}{rows}{first_row}\n"),
            )
            .unwrap();
        }
        assert!(validate_matrix_candidate_completeness(&repository, &evidence, &manifest).is_err());
        fs::remove_dir_all(evidence).unwrap();
    }

    #[test]
    fn matrix_candidate_manifest_binds_observed_population_and_refuses_drift() {
        let root = fixture_root("matrix-candidate-invocation-manifest");
        let invocations = root.join("matrix-candidate-invocations");
        fs::create_dir_all(&invocations).unwrap();
        let host_tmp = root.join("host-tmp");
        fs::create_dir(&host_tmp).unwrap();
        let host = fs::metadata(&host_tmp).unwrap();
        let unshare_sha256 = "b".repeat(64);
        let report = |launched: &str| {
            format!(
                "safehermit: binary_sha256={launched}\nsafehermit: bound.wall=APPLIED:120s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=2G\nsafehermit: bound.disk=APPLIED:8G\nsafehermit: bound.bytes=APPLIED:67108864\nsafehermit: bound.logfilter=NOT_APPLIED:by owner ruling\n"
            )
        };
        let private_tmp = MatrixPrivateTmpEvidence {
            schema: MATRIX_PRIVATE_TMP_SCHEMA.to_owned(),
            host_tmp: host_tmp.clone(),
            host_tmp_device: host.dev(),
            host_tmp_inode: host.ino(),
            proxy_uid: 4242,
            binds: vec![(root.clone(), host_tmp.join("fixture"))],
            unshare: PathBuf::from("/usr/bin/unshare"),
            unshare_sha256: unshare_sha256.clone(),
            candidate: PathBuf::from("/bin/true"),
            candidate_sha256: sha256(Path::new("/bin/true")).unwrap(),
        };
        // Distinct real candidates outside /tmp, so a retained record is
        // re-hashed and routed to its own leg's candidate.
        let cargo_sha256 = private_tmp.candidate_sha256.clone();
        let buck_sha256 = sha256(Path::new("/bin/false")).unwrap();
        assert_ne!(cargo_sha256, buck_sha256);
        let buck_private_tmp = MatrixPrivateTmpEvidence {
            candidate: PathBuf::from("/bin/false"),
            candidate_sha256: buck_sha256.clone(),
            ..private_tmp.clone()
        };
        let readback = format!(
            "tmp={}:{}\ncwd=/tmp\ntmpdir=/tmp\nuid_map=0 4242 1\n",
            host.dev(),
            host.ino()
        );
        let write_invocation = |name: &str, role: &str| {
            let invocation = invocations.join(name);
            fs::create_dir(&invocation).unwrap();
            fs::create_dir(invocation.join("data")).unwrap();
            fs::write(
                invocation.join("data/runtime.log"),
                format!("{name} data\n"),
            )
            .unwrap();
            fs::write(
                invocation.join("invocation.json"),
                serde_json::to_vec_pretty(&MatrixProxyInvocation {
                    invocation_schema: "hermit-matrix-proxy-invocation/v1".to_owned(),
                    case_identity: "exit_zero".to_owned(),
                    role: role.to_owned(),
                    normalized_argv: vec![
                        "run".to_owned(),
                        "--".to_owned(),
                        "/bin/true".to_owned(),
                    ],
                })
                .unwrap(),
            )
            .unwrap();
            fs::write(invocation.join("stdout"), format!("{name} stdout\n")).unwrap();
            fs::write(invocation.join("stderr"), format!("{name} stderr\n")).unwrap();
            let buck = name.starts_with("buck-");
            if role == "dbt-run" {
                fs::write(
                    invocation.join("safehermit.report"),
                    report(&unshare_sha256),
                )
                .unwrap();
                fs::write(
                    invocation.join("private-tmp.json"),
                    serde_json::to_vec_pretty(if buck {
                        &buck_private_tmp
                    } else {
                        &private_tmp
                    })
                    .unwrap(),
                )
                .unwrap();
                fs::write(invocation.join("private-tmp.readback"), &readback).unwrap();
            } else {
                let launched = if buck { &buck_sha256 } else { &cargo_sha256 };
                fs::write(invocation.join("safehermit.report"), report(launched)).unwrap();
            }
        };
        write_invocation("cargo-observed-1", "dbt-run");
        write_invocation("buck-observed-2", "dbt-run");
        write_invocation("cargo-reference-3", "ptrace-reference");
        write_invocation("buck-reference-4", "ptrace-reference");
        let manifest = root.join("matrix-candidate-invocations.json");
        let observed =
            inspect_matrix_candidate_invocations(&root, &cargo_sha256, &buck_sha256).unwrap();
        atomic_write_new(
            &manifest,
            &render_matrix_candidate_invocation_manifest(&observed).unwrap(),
        )
        .unwrap();
        let (retained, retained_hash) = verify_retained_matrix_candidate_invocation_manifest(
            &root,
            &cargo_sha256,
            &buck_sha256,
        )
        .unwrap();
        assert_eq!(retained.invocations.len(), 4);
        assert_eq!(retained_hash, sha256(&manifest).unwrap());
        for invocation in &retained.invocations {
            let private = invocation.role == "dbt-run";
            assert_eq!(invocation.private_tmp_sha256.is_some(), private);
            assert_eq!(invocation.private_tmp_readback_sha256.is_some(), private);
        }

        // Private /tmp evidence is re-checked by content, not only hashed.
        let dbt = invocations.join("cargo-observed-1");
        let refuses = |needle: &str| {
            let error = inspect_matrix_candidate_invocations(&root, &cargo_sha256, &buck_sha256)
                .unwrap_err();
            assert!(error.contains(needle), "{needle}: {error}");
        };
        for (file, bytes, needle) in [
            (
                "private-tmp.readback",
                readback.replace("uid_map=0 4242 1", "uid_map=0 0 4294967295"),
                "does not match the host directory and mapping",
            ),
            (
                "private-tmp.readback",
                readback.replace("cwd=/tmp", "cwd=/home"),
                "does not match the host directory and mapping",
            ),
            (
                "safehermit.report",
                report(&"c".repeat(64)),
                "records launched binary",
            ),
            (
                "private-tmp.json",
                serde_json::to_string(&MatrixPrivateTmpEvidence {
                    binds: vec![(root.clone(), root.join("outside"))],
                    ..private_tmp.clone()
                })
                .unwrap(),
                "invalid semantics",
            ),
            (
                "private-tmp.json",
                serde_json::to_string(&MatrixPrivateTmpEvidence {
                    schema: "hermit-matrix-private-tmp/v1".to_owned(),
                    ..private_tmp.clone()
                })
                .unwrap(),
                "invalid semantics",
            ),
            (
                "private-tmp.json",
                serde_json::to_string(&MatrixPrivateTmpEvidence {
                    binds: vec![
                        (root.clone(), host_tmp.join("fixture")),
                        (host_tmp.clone(), host_tmp.join("fixture")),
                    ],
                    ..private_tmp.clone()
                })
                .unwrap(),
                "twice",
            ),
            // The named file still hashes to this leg's candidate, so only the
            // recorded-candidate equality can refuse this record. It precedes
            // the whole-Buck record, which the re-hash would also refuse, so
            // removing the equality is reported as an acceptance.
            (
                "private-tmp.json",
                serde_json::to_string(&MatrixPrivateTmpEvidence {
                    candidate_sha256: buck_sha256.clone(),
                    ..private_tmp.clone()
                })
                .unwrap(),
                "but the leg's candidate is",
            ),
            (
                "private-tmp.json",
                serde_json::to_string(&buck_private_tmp).unwrap(),
                "but the leg's candidate is",
            ),
            (
                "private-tmp.json",
                serde_json::to_string(&MatrixPrivateTmpEvidence {
                    candidate: PathBuf::from("/bin/false"),
                    ..private_tmp.clone()
                })
                .unwrap(),
                "now hashes to",
            ),
        ] {
            let path = dbt.join(file);
            let original = fs::read(&path).unwrap();
            fs::write(&path, bytes).unwrap();
            refuses(needle);
            fs::write(&path, original).unwrap();
        }
        for file in ["private-tmp.json", "private-tmp.readback"] {
            let path = dbt.join(file);
            let original = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            refuses("unexpected file population");
            fs::write(&path, original).unwrap();
        }
        // The population must follow the role in both directions.
        let reference = invocations.join("cargo-reference-3");
        fs::copy(
            dbt.join("private-tmp.json"),
            reference.join("private-tmp.json"),
        )
        .unwrap();
        fs::copy(
            dbt.join("private-tmp.readback"),
            reference.join("private-tmp.readback"),
        )
        .unwrap();
        refuses("with role ptrace-reference must hold exactly");
        fs::remove_file(reference.join("private-tmp.json")).unwrap();
        fs::remove_file(reference.join("private-tmp.readback")).unwrap();
        let metadata = dbt.join("invocation.json");
        let original_metadata = fs::read_to_string(&metadata).unwrap();
        fs::write(
            &metadata,
            original_metadata.replace("\"dbt-run\"", "\"ptrace-reference\""),
        )
        .unwrap();
        refuses("with role ptrace-reference must hold exactly");
        fs::write(&metadata, &original_metadata).unwrap();
        // A retained reference must have been launched by safehermit as its
        // own leg's candidate, not merely carry applied bounds.
        let reference_report = reference.join("safehermit.report");
        for launched in [&buck_sha256, &unshare_sha256] {
            fs::write(&reference_report, report(launched)).unwrap();
            refuses("records launched binary");
        }
        fs::write(&reference_report, report(&cargo_sha256)).unwrap();
        // Swapping the legs' candidates refuses the audit.
        assert!(inspect_matrix_candidate_invocations(&root, &buck_sha256, &cargo_sha256).is_err());
        inspect_matrix_candidate_invocations(&root, &cargo_sha256, &buck_sha256).unwrap();
        // Relabelling any single retained invocation to the other leg refuses
        // that invocation: each is audited against its own leg's bytes.
        for (name, relabelled) in [
            ("cargo-observed-1", "buck-observed-1"),
            ("buck-observed-2", "cargo-observed-2"),
            ("cargo-reference-3", "buck-reference-3"),
            ("buck-reference-4", "cargo-reference-4"),
        ] {
            fs::rename(invocations.join(name), invocations.join(relabelled)).unwrap();
            let error = inspect_matrix_candidate_invocations(&root, &cargo_sha256, &buck_sha256)
                .unwrap_err();
            let (label, identity) = relabelled.split_once('-').unwrap();
            assert!(
                error.contains(&format!(
                    "retained {label} DBT matrix invocation {identity} "
                )),
                "{relabelled}: {error}"
            );
            fs::rename(invocations.join(relabelled), invocations.join(name)).unwrap();
        }
        inspect_matrix_candidate_invocations(&root, &cargo_sha256, &buck_sha256).unwrap();

        let readback_path = dbt.join("private-tmp.readback");
        fs::write(&readback_path, readback.replace("tmp=", "tmp=1")).unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&readback_path, &readback).unwrap();
        verify_retained_matrix_candidate_invocation_manifest(&root, &cargo_sha256, &buck_sha256)
            .unwrap();

        let original_manifest = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            original_manifest.replace("observed-1", "mutated-1"),
        )
        .unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&manifest, &original_manifest).unwrap();

        fs::remove_file(&manifest).unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&manifest, &original_manifest).unwrap();

        let stdout = invocations.join("cargo-observed-1/stdout");
        let original_stdout = fs::read(&stdout).unwrap();
        fs::write(&stdout, b"mutated stdout\n").unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&stdout, original_stdout).unwrap();

        let data = invocations.join("cargo-observed-1/data/runtime.log");
        let original_data = fs::read(&data).unwrap();
        fs::write(&data, b"mutated data\n").unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&data, original_data).unwrap();

        let empty_directory = invocations.join("cargo-observed-1/data/late-empty-directory");
        fs::create_dir(&empty_directory).unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::remove_dir(&empty_directory).unwrap();

        let stderr = invocations.join("cargo-observed-1/stderr");
        let original_stderr = fs::read(&stderr).unwrap();
        fs::write(&stderr, b"mutated stderr\n").unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&stderr, original_stderr).unwrap();

        let report = invocations.join("buck-observed-2/safehermit.report");
        let original_report = fs::read(&report).unwrap();
        fs::write(
            &report,
            b"safehermit: bound.wall=APPLIED:119s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=2G\nsafehermit: bound.disk=APPLIED:8G\nsafehermit: bound.bytes=APPLIED:67108864\nsafehermit: bound.logfilter=NOT_APPLIED:by owner ruling\n",
        )
        .unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&report, &original_report).unwrap();
        fs::remove_file(&report).unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::write(&report, &original_report).unwrap();

        let extra = invocations.join("cargo-observed-1/extra");
        fs::write(&extra, b"unexpected\n").unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::remove_file(&extra).unwrap();

        let link = invocations.join("cargo-observed-1/linked");
        std::os::unix::fs::symlink("stdout", &link).unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::remove_file(&link).unwrap();

        write_invocation("cargo-extra-observed", "ptrace-reference");
        write_invocation("buck-extra-observed", "ptrace-reference");
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::remove_dir_all(invocations.join("cargo-extra-observed")).unwrap();
        fs::remove_dir_all(invocations.join("buck-extra-observed")).unwrap();

        fs::remove_dir_all(invocations.join("buck-observed-2")).unwrap();
        assert!(
            verify_retained_matrix_candidate_invocation_manifest(
                &root,
                &cargo_sha256,
                &buck_sha256
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_buck_log_summary_is_recomputed_and_mutation_or_deletion_refuses() {
        let root = fixture_root("buck-log-summary-recompute");
        fs::create_dir(&root).unwrap();
        let buck = root.join("buck2");
        write_executable(
            &buck,
            b"#!/bin/sh\nset -eu\ntest \"$1\" = log\ntest \"$2\" = summary\ntest -f \"$3\"\nprintf 'summary-fixture\\n'\n",
        );
        let event = root.join("event.json-lines.gz");
        fs::write(&event, b"fixture event bytes\n").unwrap();
        let summary = root.join("buck-log-summary.txt");
        fs::write(&summary, b"summary-fixture\n").unwrap();
        verify_retained_buck_log_summary(&buck, &root, &event, &summary).unwrap();
        fs::write(&summary, b"mutated-summary\n").unwrap();
        assert!(verify_retained_buck_log_summary(&buck, &root, &event, &summary).is_err());
        fs::remove_file(&summary).unwrap();
        assert!(verify_retained_buck_log_summary(&buck, &root, &event, &summary).is_err());
        fs::remove_file(&event).unwrap();
        fs::write(&summary, b"summary-fixture\n").unwrap();
        assert!(verify_retained_buck_log_summary(&buck, &root, &event, &summary).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matrix_supervisor_refuses_timeout_abort_and_incomplete_cleanup() {
        let clean = || {
            require_clean_matrix_supervision(
                true,
                false,
                false,
                false,
                0,
                0,
                1,
                true,
                false,
                false,
                false,
                Some(0),
            )
        };
        clean().unwrap();
        for (run_timed_out, outcome_aborted, not_launched) in
            [(true, false, 0), (false, true, 0), (false, false, 1)]
        {
            assert!(
                require_clean_matrix_supervision(
                    !run_timed_out && !outcome_aborted && not_launched == 0,
                    run_timed_out,
                    false,
                    false,
                    0,
                    not_launched,
                    1,
                    false,
                    run_timed_out,
                    false,
                    outcome_aborted,
                    None,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn dbt_selected_row_mismatch_is_refused() {
        assert!(
            require_equal(
                "DBT rows",
                "test_name\tresult\na\tPASS\n",
                "test_name\tresult\nb\tPASS\n",
            )
            .is_err()
        );
    }
}
