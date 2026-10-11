/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The integration test and standalone matrix use this driver and assertion path.
//! The native collector's exit is recorded separately from the actual CLI exit.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use detcore::logdiff::CANON_ADDRESS_ORDINAL_V1;
use detcore::logdiff::STRIP_WALL_CLOCK_PREFIX_V1;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::Verdict as VerificationVerdict;
use hermit::canonical_verdict::VerificationReport;

#[path = "../../hermit-cli/tests/common/backend_isolation.rs"]
pub mod conformance;

use conformance::Assertion;
use conformance::NamespacePolicy;
use conformance::Observation;
use conformance::Verdict;

pub const LAUNCH_MIGRATION_ASSERTIONS: [&str; 4] = [
    "namespace.pid.host_separation",
    "namespace.uts.host_separation",
    "scratch.host_separation",
    "pid.private_init",
];

// These are source inputs to the existing parser and the standalone's new
// local library dependencies, not proof of any physical guest generation.
const VERIFICATION_INPUTS: &[(&str, &str)] = &[
    ("verify-parser.rs", "hermit-cli/src/canonical_verdict.rs"),
    ("verify-policy.rs", "hermit-cli/src/bin/hermit/verify.rs"),
    ("verify-policy-tokens.rs", "detcore/src/logdiff.rs"),
    ("standalone-entry.rs", "tests/isolation/conformance.rs"),
    ("workspace-Cargo.toml", "Cargo.toml"),
    ("workspace-Cargo.lock", "Cargo.lock"),
    ("hermit-Cargo.toml", "hermit-cli/Cargo.toml"),
    ("detcore-Cargo.toml", "detcore/Cargo.toml"),
];
const VERIFY_REPORT_LIMIT: u64 = 16_777_216;

pub const REQUIRED_PATHS: [&str; 25] = [
    "run-raw",
    "verify-raw",
    "verify-normal",
    "record",
    "record-verify",
    "replay-autopilot",
    "replay-gdb-local-refusal",
    "replay-gdb-host",
    "mount-bind",
    "tmp-explicit",
    "cwd-under-tmp",
    "cwd-missing-refusal",
    "program-hidden-by-tmp",
    "interpreter-hidden-by-tmp",
    "explicit-workdir-hidden-caller",
    "stdin-finite",
    "stdin-tty",
    "stdin-closed",
    "stdout-closed",
    "stderr-closed",
    "run-gdb-local-refusal",
    "run-gdb-host",
    "record-gdb-local-refusal",
    "record-gdb-host",
    "image-root",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    pub backend: String,
    pub root: String,
    pub path: String,
    pub policy: NamespacePolicy,
}

impl Cell {
    pub fn name(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.backend,
            self.root,
            self.path,
            policy_name(self.policy),
        )
    }
}

pub fn policy_name(policy: NamespacePolicy) -> &'static str {
    match policy {
        NamespacePolicy::Private => "private",
        NamespacePolicy::HostNetwork => "host-network",
        NamespacePolicy::None => "none",
    }
}

pub fn required_cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for backend in conformance::BACKENDS {
        for root in ["host", "pinned"] {
            for policy in [
                NamespacePolicy::Private,
                NamespacePolicy::HostNetwork,
                NamespacePolicy::None,
            ] {
                for path in REQUIRED_PATHS {
                    cells.push(Cell {
                        backend: backend.to_owned(),
                        root: root.to_owned(),
                        path: path.to_owned(),
                        policy,
                    });
                }
            }
        }
    }
    // These are separate tools and explicit namespace-option refusal cases.
    // They retain their own reports and are never counted as Detcore parity.
    for root in ["host", "pinned"] {
        for (backend, path) in [
            ("sabre", "strace-prototype"),
            ("ptrace", "namespace-only"),
            ("ptrace", "no-namespace-image-refusal"),
            ("ptrace", "no-namespace-schedule-refusal"),
            ("ptrace", "no-namespace-preemption-refusal"),
        ] {
            cells.push(Cell {
                backend: backend.to_owned(),
                root: root.to_owned(),
                path: path.to_owned(),
                policy: NamespacePolicy::Private,
            });
        }
    }
    cells
}

pub struct Prepared {
    pub repository: PathBuf,
    pub hermit: PathBuf,
    pub artifacts: PathBuf,
    pub guest: PathBuf,
    pub native: PathBuf,
    pub installation: PathBuf,
    executor: Executor,
    input_inventory: String,
    cohort_inputs: BTreeMap<String, CohortInput>,
}

// Only ordinary host routes adopt this fixture. Named, covered-root and replay
// inputs still need their own actual preparation and binding.
fn ordinary_cohort(cell: &Cell) -> bool {
    cell.root == "host"
        && matches!(
            cell.path.as_str(),
            "run-raw"
                | "verify-raw"
                | "verify-normal"
                | "record"
                | "record-verify"
                | "stdin-finite"
        )
}

fn cohort_key(cell: &Cell) -> String {
    format!("{}:{}:{}", cell.root, cell.path, policy_name(cell.policy))
}

struct CohortInput {
    key: String,
    root: PathBuf,
    initial: String,
    initial_sha256: String,
}

// These named fields bind the observed object and owner-frozen input shape.
// Access time is not an immutable input: these reads can change it. All guest
// and native observation fields still go to the unchanged full checker.
fn cohort_object(metadata: &fs::Metadata) -> String {
    format!(
        "dev={} ino={} mode={:o} uid={} gid={} nlink={} size={} mtime={}:{} ctime={}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.uid(),
        metadata.gid(),
        metadata.nlink(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

impl CohortInput {
    fn workdir(&self) -> PathBuf {
        self.root.join("workdir")
    }

    fn sentinel(&self) -> PathBuf {
        self.root.join("sentinel")
    }

    fn initial_path(&self) -> PathBuf {
        self.root.with_extension("inputs")
    }

    fn prepare(key: String, root: PathBuf) -> Result<Self, String> {
        fs::create_dir(&root).map_err(|error| format!("fresh cohort input: {error}"))?;
        fs::create_dir(root.join("workdir"))
            .map_err(|error| format!("cohort cwd fixture: {error}"))?;
        fs::create_dir(root.join("sentinel"))
            .map_err(|error| format!("cohort descriptor fixture: {error}"))?;
        fs::write(
            root.join("sentinel/descriptor-marker"),
            b"controlled descriptor identity\n",
        )
        .map_err(|error| format!("cohort descriptor marker: {error}"))?;
        let mut input = Self {
            key,
            root,
            initial: String::new(),
            initial_sha256: String::new(),
        };
        input.initial = input.snapshot()?;
        input.initial_sha256 = digest_bytes(input.initial.as_bytes())?;
        fs::write(input.initial_path(), &input.initial)
            .map_err(|error| format!("initial cohort input receipt: {error}"))?;
        Ok(input)
    }

    fn snapshot(&self) -> Result<String, String> {
        let mut rows = String::new();
        for (relative, expected) in [
            ("", &["sentinel", "workdir"][..]),
            ("workdir", &[][..]),
            ("sentinel", &["descriptor-marker"][..]),
        ] {
            let path = self.root.join(relative);
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("cohort directory {}: {error}", path.display()))?;
            if !metadata.is_dir() {
                return Err(format!(
                    "cohort directory {} is not a real directory",
                    path.display()
                ));
            }
            let object = cohort_object(&metadata);
            let mut names = Vec::new();
            for entry in fs::read_dir(&path)
                .map_err(|error| format!("cohort entries {}: {error}", path.display()))?
            {
                if names.len() == 16 {
                    return Err(format!(
                        "cohort directory {} exceeds its declared input shape",
                        path.display()
                    ));
                }
                names.push(
                    entry
                        .map_err(|error| format!("cohort entry: {error}"))?
                        .file_name(),
                );
            }
            names.sort();
            let encoded = names
                .iter()
                .map(|name| hex_bytes(name.as_encoded_bytes()))
                .collect::<Vec<_>>();
            let declared = expected
                .iter()
                .map(|name| hex_bytes(name.as_bytes()))
                .collect::<Vec<_>>();
            if encoded != declared {
                return Err(format!(
                    "cohort directory {} {object} entries={encoded:?}, declared={declared:?}",
                    path.display()
                ));
            }
            rows.push_str(&format!(
                "DIRECTORY path={} {object} entries={}\n",
                hex_bytes(path.as_os_str().as_encoded_bytes()),
                encoded.join(",")
            ));
            let after = fs::symlink_metadata(&path)
                .map_err(|error| format!("cohort directory after inventory: {error}"))?;
            if cohort_object(&after) != object {
                return Err(format!(
                    "cohort directory {} changed during inventory",
                    path.display()
                ));
            }
        }
        let marker = self.sentinel().join("descriptor-marker");
        let metadata =
            fs::symlink_metadata(&marker).map_err(|error| format!("cohort marker: {error}"))?;
        if !metadata.is_file() {
            return Err("cohort descriptor marker is not a real regular file".to_owned());
        }
        let object = cohort_object(&metadata);
        let file = File::open(&marker).map_err(|error| format!("cohort marker open: {error}"))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("cohort marker object: {error}"))?;
        if cohort_object(&opened) != object {
            return Err("cohort marker changed before its content read".to_owned());
        }
        let mut bytes = Vec::new();
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cohort marker read: {error}"))?;
        if bytes.len() > 4096 {
            return Err("cohort marker exceeds its complete input receipt limit".to_owned());
        }
        let after = fs::symlink_metadata(&marker)
            .map_err(|error| format!("cohort marker after read: {error}"))?;
        if cohort_object(&after) != object {
            return Err("cohort marker changed during its content read".to_owned());
        }
        rows.push_str(&format!(
            "FILE path={} {object} bytes={} sha256={}\n",
            hex_bytes(marker.as_os_str().as_encoded_bytes()),
            hex_bytes(&bytes),
            digest_bytes(&bytes)?
        ));
        Ok(rows)
    }

    fn reference(&self) -> String {
        format!(
            "cohort={}\ninitial_path={}\ninitial_sha256={}\nworkdir={}\nsentinel={}\n",
            hex_bytes(self.key.as_bytes()),
            hex_bytes(self.initial_path().as_os_str().as_encoded_bytes()),
            self.initial_sha256,
            hex_bytes(self.workdir().as_os_str().as_encoded_bytes()),
            hex_bytes(self.sentinel().as_os_str().as_encoded_bytes())
        )
    }

    fn retain_snapshot(&self, path: &Path) -> Result<(), String> {
        match self.snapshot() {
            Ok(snapshot) => fs::write(path, snapshot)
                .map_err(|error| format!("cohort snapshot receipt: {error}")),
            Err(error) => {
                fs::write(path, format!("UNAVAILABLE {error}\n"))
                    .map_err(|write_error| format!("{error}; snapshot retention: {write_error}"))?;
                Err(error)
            }
        }
    }

    fn assertion(
        &self,
        directory: &Path,
        before: Result<(), String>,
        after: Result<(), String>,
    ) -> Assertion {
        let checked = (|| -> Result<String, String> {
            let failures = before
                .err()
                .into_iter()
                .chain(after.err())
                .collect::<Vec<_>>();
            if !failures.is_empty() {
                return Err(failures.join("; "));
            }
            let initial = read_bounded(&self.initial_path(), 65_536)?;
            let reference = read_bounded(&directory.join("cohort.inputs.reference"), 65_536)?;
            let before = read_bounded(&directory.join("cohort.inputs.before"), 65_536)?;
            let after = read_bounded(&directory.join("cohort.inputs.after"), 65_536)?;
            if initial != self.initial.as_bytes()
                || reference != self.reference().as_bytes()
                || before != initial
                || after != initial
            {
                return Err(format!(
                    "cohort input changed or receipt differs; initial_sha256={} before_sha256={} after_sha256={}; retain all raw fixture receipts",
                    digest_bytes(&initial)?,
                    digest_bytes(&before)?,
                    digest_bytes(&after)?
                ));
            }
            Ok(format!(
                "cohort={} initial_sha256={}; same literal cwd/sentinel paths, named object metadata, entries and complete marker bytes observed before and after this operand; original permissions/writability retained; no kernel immutability or whole-input claim",
                self.key, self.initial_sha256
            ))
        })();
        Assertion {
            name: "receipt.cohort_fixture".to_owned(),
            verdict: if checked.is_ok() {
                Verdict::Match
            } else {
                Verdict::NoResult
            },
            detail: checked.unwrap_or_else(|error| error),
        }
    }
}

fn prepare_cohort_inputs(artifacts: &Path) -> Result<BTreeMap<String, CohortInput>, String> {
    let root = artifacts.join("cohort-inputs");
    fs::create_dir(&root).map_err(|error| format!("cohort input owner root: {error}"))?;
    let mut inputs = BTreeMap::new();
    for cell in required_cells().into_iter().filter(ordinary_cohort) {
        let key = cohort_key(&cell);
        if let std::collections::btree_map::Entry::Vacant(entry) = inputs.entry(key.clone()) {
            entry.insert(CohortInput::prepare(
                key.clone(),
                root.join(key.replace(':', "-")),
            )?);
        }
    }
    Ok(inputs)
}

/// The integration call site explicitly supplies its shipped command executor.
/// No ancestor helper or environment marker selects this route.
pub type CaptureCommand = fn(&mut Command, &Path, Duration, u64) -> Capture;

#[derive(Debug)]
pub struct Capture {
    pub status: Option<ExitStatus>,
    pub stdout_read: u64,
    pub stdout_written: u64,
    pub stderr_read: u64,
    pub stderr_written: u64,
    pub stdout_eof: bool,
    pub stderr_eof: bool,
    pub reaped: bool,
    pub cleanup_complete: bool,
    pub elapsed: Duration,
    pub failure: Option<String>,
}

enum Executor {
    SafeHermit(PathBuf),
    Integration {
        capture: CaptureCommand,
        sources: Vec<PathBuf>,
        binary: PathBuf,
    },
}

struct Execution {
    status: Option<ExitStatus>,
    boundary: Result<String, String>,
}

type CollectedObservations = (
    Result<Observation, String>,
    Result<Observation, String>,
    String,
    Result<String, String>,
    Vec<Assertion>,
);

pub struct Collected {
    pub cell: Cell,
    pub directory: PathBuf,
    pub guest: Result<Observation, String>,
    pub native: Result<Observation, String>,
    pub assertions: Vec<Assertion>,
}

impl Collected {
    /// A deliberately limited migration regression. Every full-contract row
    /// remains in the report; this result makes no all-backend parity claim.
    pub fn launch_migration_matches(&self) -> bool {
        LAUNCH_MIGRATION_ASSERTIONS.iter().all(|name| {
            self.assertions
                .iter()
                .any(|row| row.name == *name && row.verdict == Verdict::Match)
        }) && self
            .assertions
            .iter()
            .any(|row| row.name == "execution.cli_exit" && row.verdict == Verdict::Match)
            && self
                .assertions
                .iter()
                .any(|row| row.name == "receipt.input_stability" && row.verdict == Verdict::Match)
            && executor_matches(&self.assertions)
            && self
                .assertions
                .iter()
                .all(|row| row.name != "receipt.typed_report" || row.verdict == Verdict::Match)
    }

    pub fn full_matches(&self) -> bool {
        executor_matches(&self.assertions) && conformance::all_match(&self.assertions)
    }
}

fn executor_matches(rows: &[Assertion]) -> bool {
    let mut rows = rows
        .iter()
        .filter(|row| row.name == "receipt.executor_boundary");
    rows.next().is_some_and(|row| row.verdict == Verdict::Match) && rows.next().is_none()
}

fn status_code(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| format!("signal:{status}"), |code| code.to_string())
}

fn digest(path: &Path) -> Result<String, String> {
    let output = Command::new("sha256sum")
        .arg("--")
        .arg(path)
        .output()
        .map_err(|error| format!("content hash invocation: {error}"))?;
    if !output.status.success() {
        return Err(format!("content hash failed: {}", output.status));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| "invalid hash output")?;
    let value = text.split_whitespace().next().ok_or("missing hash")?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid SHA-256 digest".to_owned());
    }
    Ok(value.to_owned())
}

fn digest_bytes(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("byte hash invocation: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("missing hash stdin")?
        .write_all(bytes)
        .map_err(|error| format!("byte hash input: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("byte hash wait: {error}"))?;
    if !output.status.success() {
        return Err(format!("byte hash failed: {}", output.status));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| "invalid byte hash output")?;
    let value = text.split_whitespace().next().ok_or("missing byte hash")?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid byte SHA-256 digest".to_owned());
    }
    Ok(value.to_owned())
}

fn compile(source: &Path, binary: &Path, log: &Path) -> Result<(), String> {
    let output = Command::new("cc")
        .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(binary)
        .output()
        .map_err(|error| format!("fixture compiler: {error}"))?;
    let status = output.status;
    fs::write(log, [&output.stdout[..], &output.stderr[..]].concat())
        .map_err(|error| format!("compiler receipt: {error}"))?;
    fs::write(log.with_extension("exit"), status_code(status))
        .map_err(|error| format!("compiler exit receipt: {error}"))?;
    if !status.success() {
        return Err(format!(
            "fixture compilation failed; inspect {}",
            log.display()
        ));
    }
    Ok(())
}

fn inventory_tree(root: &Path, path: &Path, rows: &mut Vec<String>) -> Result<(), String> {
    if rows.len() >= 8192 {
        return Err(
            "installed resource inventory exceeds its complete-observation limit".to_owned(),
        );
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "resource escaped declared installation")?;
    let name = hex_bytes(relative.as_os_str().as_encoded_bytes());
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            rows.push(format!(
                "UNAVAILABLE {name} errno={}\n",
                error.raw_os_error().unwrap_or(0)
            ));
            return Ok(());
        }
    };
    if metadata.is_dir() {
        rows.push(format!("DIRECTORY {name}\n"));
        let mut children = fs::read_dir(path)
            .map_err(|error| format!("complete installed directory inventory: {error}"))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("complete installed entry inventory: {error}"))?;
        children.sort();
        for child in children {
            inventory_tree(root, &child, rows)?;
        }
    } else if metadata.file_type().is_symlink() {
        let target =
            fs::read_link(path).map_err(|error| format!("installed resource link: {error}"))?;
        rows.push(format!(
            "LINK {name} target={}\n",
            hex_bytes(target.as_os_str().as_encoded_bytes())
        ));
        if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            rows.push(format!("FILE {name} sha256={}\n", digest(path)?));
        }
    } else if metadata.is_file() {
        rows.push(format!(
            "FILE {name} bytes={} sha256={}\n",
            metadata.len(),
            digest(path)?
        ));
    } else {
        return Err("declared resource tree contains an unsupported non-file object".to_owned());
    }
    Ok(())
}

fn installation_for(
    repository: &Path,
    hermit: &Path,
    explicit: Option<&Path>,
) -> Result<PathBuf, String> {
    let environment_selection = std::env::var_os("HERMIT_INSTALL_DIR").map(PathBuf::from);
    let selected = if let Some(explicit) = explicit {
        explicit.to_path_buf()
    } else if let Some(selected) = environment_selection {
        if selected.as_os_str().is_empty() {
            return Err("empty declared installation directory".to_owned());
        }
        selected
    } else if let Some(adjacent) = hermit
        .parent()
        .filter(|parent| parent.join("rsrcs").is_dir())
    {
        adjacent.to_path_buf()
    } else {
        hermit
            .parent()
            .and_then(Path::parent)
            .map(|target| target.join("install_pkg"))
            .filter(|candidate| candidate.join("rsrcs").is_dir())
            .unwrap_or_else(|| repository.join("target/install_pkg"))
    };
    // An absent declared installation is still passed explicitly to the CLI.
    // Its requested backend must refuse; it must not build resources implicitly.
    let absolute = if selected.is_absolute() {
        selected
    } else {
        repository.join(selected)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

fn input_inventory(prepared: &Prepared) -> Result<String, String> {
    let mut rows = Vec::new();
    for (role, path) in [
        ("hermit", &prepared.hermit),
        ("guest", &prepared.guest),
        ("native", &prepared.native),
    ] {
        rows.push(format!("EXECUTABLE {role} sha256={}\n", digest(path)?));
    }
    match &prepared.executor {
        Executor::SafeHermit(path) => {
            rows.push(format!("EXECUTABLE safehermit sha256={}\n", digest(path)?))
        }
        Executor::Integration {
            sources, binary, ..
        } => {
            rows.push(format!(
                "EXECUTABLE integration_executor sha256={}\n",
                digest(binary)?
            ));
            for (index, source) in sources.iter().enumerate() {
                rows.push(format!(
                    "SOURCE integration_executor_{index} sha256={}\n",
                    digest(source)?
                ));
            }
        }
    }
    for (snapshot, source) in VERIFICATION_INPUTS {
        rows.push(format!(
            "SOURCE {snapshot} sha256={}\n",
            digest(&prepared.repository.join(source))?
        ));
    }
    inventory_tree(&prepared.installation, &prepared.installation, &mut rows)?;
    Ok(rows.concat())
}

#[derive(Clone, Copy)]
struct ExecutionLimits {
    memory: u64,
    tasks: u64,
    cpu_seconds: u64,
    wall_seconds: u64,
    stream_bytes: u64,
}

const STAMP_LIMITS: ExecutionLimits = ExecutionLimits {
    memory: 268_435_456,
    tasks: 32,
    cpu_seconds: 10,
    wall_seconds: 30,
    stream_bytes: 1_048_576,
};
const NATIVE_LIMITS: ExecutionLimits = ExecutionLimits {
    memory: 4_294_967_296,
    tasks: 128,
    cpu_seconds: 30,
    wall_seconds: 90,
    stream_bytes: 16_777_216,
};

fn framed_fields(bytes: &[u8], prefix: &str) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid receipt encoding")?;
    if !text.ends_with('\n') {
        return Err("incomplete receipt framing".to_owned());
    }
    let mut fields = BTreeMap::new();
    for line in text.lines().filter_map(|line| line.strip_prefix(prefix)) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'))
        {
            continue;
        }
        if fields.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(format!("duplicate completion field {key}"));
        }
    }
    Ok(fields)
}

fn field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, String> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing completion field {key}"))
}

fn unsigned(fields: &BTreeMap<String, String>, key: &str) -> Result<u64, String> {
    let value = field(fields, key)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid unsigned completion field {key}"));
    }
    value
        .parse()
        .map_err(|_| format!("completion field {key} out of range"))
}

fn expected_native_outcome(path: &Path) -> Result<i32, String> {
    let bytes = fs::read(path).map_err(|error| format!("native completion receipt: {error}"))?;
    native_outcome(&bytes)
}

fn native_outcome(bytes: &[u8]) -> Result<i32, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid native completion encoding")?;
    if !text.ends_with('\n') {
        return Err("incomplete native completion framing".to_owned());
    }
    let mut footers = text
        .lines()
        .filter_map(|line| line.strip_prefix("NATIVE_COMPLETE backend-isolation-physical-v1 "));
    let footer = footers.next().ok_or("missing native completion footer")?;
    if footers.next().is_some() {
        return Err("duplicate native completion footer".to_owned());
    }
    let mut fields = BTreeMap::new();
    for entry in footer.split_whitespace() {
        let (key, value) = entry
            .split_once('=')
            .ok_or("invalid native completion field")?;
        if fields.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err("duplicate native completion field".to_owned());
        }
    }
    for key in ["snapshots", "identities", "checkpoints", "errors", "live"] {
        unsigned(&fields, key)?;
    }
    let observation = Observation::parse(bytes)?;
    let (code, signal) = observation.cli_exit()?;
    if unsigned(&fields, "errors")? != 0 {
        return Ok(2);
    }
    if signal != 0 {
        return i32::try_from(128 + signal).map_err(|_| "invalid CLI signal outcome".to_owned());
    }
    i32::try_from(code).map_err(|_| "invalid CLI exit outcome".to_owned())
}

fn safe_completion(
    stderr: &[u8],
    resources: &[u8],
    actual: ExitStatus,
    payload_digest: &str,
    invocation_digest: &str,
    expected_payload: i32,
    limits: ExecutionLimits,
) -> Result<String, String> {
    let final_fields = framed_fields(stderr, "safehermit: ")?;
    let metrics = framed_fields(resources, "safehermit: ")?;
    for key in ["unit", "run_id", "binary_sha256"] {
        if field(&metrics, key)? != field(&final_fields, key)? {
            return Err(format!("resource/announcement identity differs for {key}"));
        }
    }
    if field(&metrics, "binary_sha256")? != payload_digest
        || field(&metrics, "invocation_sha256")? != invocation_digest
    {
        return Err("resource payload/invocation identity differs".to_owned());
    }
    for (key, expected) in [
        ("limit_memory_bytes", limits.memory),
        ("limit_swap_bytes", 0),
        ("limit_tasks", limits.tasks),
        ("limit_cpu_cores_micros", 1_000_000),
        ("limit_cpu_time_nsec", limits.cpu_seconds * 1_000_000_000),
        ("limit_wall_time_usec", limits.wall_seconds * 1_000_000),
    ] {
        if unsigned(&metrics, key)? != expected {
            return Err(format!("resource limit differs for {key}"));
        }
    }
    for key in ["memory_peak_bytes", "cpu_usage_nsec", "tasks_peak"] {
        unsigned(&metrics, key)?;
    }
    for key in [
        "cpu_time_limit_hit",
        "memory_oom_kills",
        "tasks_limit_hits",
        "wall_time_limit_hit",
        "log_cap_hit",
    ] {
        if unsigned(&metrics, key)? != 0 {
            return Err(format!("resource failure {key}"));
        }
    }
    unsigned(&final_fields, "bytes_written")?;
    if field(&final_fields, "resource_report_state")? != "COMPLETE"
        || field(&final_fields, "truncated")? != "false"
        || field(&final_fields, "unit_result")? != "success"
        || unsigned(&final_fields, "runner_exit_code")? != 0
    {
        return Err("wrapper resource/cleanup completion refused".to_owned());
    }
    let code = actual.code().ok_or("unexpectedly signaled wrapper")?;
    if unsigned(&final_fields, "exit_code")? != code as u64 || code != expected_payload {
        return Err(format!(
            "wrapper/payload status differs: wrapper={code} expected={expected_payload}"
        ));
    }
    Ok(format!(
        "standalone SafeHermit complete; source-bound payload exit={code}; outer resource finalization retained"
    ))
}

fn capture_completion(
    capture: &Capture,
    limits: ExecutionLimits,
    directory: &Path,
) -> Result<String, String> {
    if let Some(error) = &capture.failure {
        return Err(error.clone());
    }
    if capture.status.is_none()
        || !capture.stdout_eof
        || !capture.stderr_eof
        || !capture.reaped
        || !capture.cleanup_complete
        || capture.elapsed >= Duration::from_secs(limits.wall_seconds)
        || capture.stdout_read != capture.stdout_written
        || capture.stderr_read != capture.stderr_written
        || capture.stdout_written > limits.stream_bytes
        || capture.stderr_written > limits.stream_bytes
    {
        return Err("local capture/owned-child completion is incomplete".to_owned());
    }
    for (name, expected) in [
        ("stdout", capture.stdout_written),
        ("stderr", capture.stderr_written),
    ] {
        let actual = fs::metadata(directory.join(name))
            .map_err(|error| format!("retained {name}: {error}"))?
            .len();
        if actual != expected {
            return Err(format!(
                "retained {name} count differs: actual={actual} expected={expected}"
            ));
        }
    }
    Ok(format!(
        "integration local executor complete: stdout={} stderr={} bytes; direct child reaped; aggregate/descendant finalization belongs to the outer runner after this test exits",
        capture.stdout_written, capture.stderr_written
    ))
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("retained read: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "{} exceeds complete observation bound",
            path.display()
        ));
    }
    Ok(bytes)
}

fn require_verification_policy(report: &VerificationReport) -> Result<(), String> {
    report.require_canonical_comparison()?;
    let comparison = report
        .comparison
        .as_ref()
        .ok_or("verification omitted its comparison")?;
    let outputs = report
        .compared_outputs
        .as_ref()
        .ok_or("verification omitted its output/status operands")?;
    for (side, output) in [("left", &outputs.left), ("right", &outputs.right)] {
        for digest in [&output.stdout_sha256, &output.stderr_sha256] {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(format!("verification {side} output hash is not SHA-256"));
            }
        }
        if !matches!(
            (output.exit_code, output.signal),
            (Some(0..=255), None) | (None, Some(1..=64))
        ) {
            return Err(format!(
                "verification {side} output omitted a valid exclusive Linux disposition"
            ));
        }
    }
    if report.verdict == VerificationVerdict::Matched
        && report
            .compared_log_messages
            .is_some_and(|counts| counts.left != counts.right)
    {
        return Err("matched verification contradicts unequal compared INFO counts".into());
    }
    let display_name = match comparison.log_scope {
        Some(ComparedLogScope::Info) => "BitwiseInfoV1",
        Some(ComparedLogScope::FullTrace) => "BitwiseFullTraceV1",
        _ => {
            return Err(
                "verification did not compare the INFO stream or its full-trace superset".into(),
            );
        }
    };
    if comparison.display_name.as_deref() == Some(display_name)
        && comparison.compare_io_buffers == Some(true)
        && comparison.virtualize_time.is_some()
        && comparison.strip_lines == Some(false)
        && comparison.canonicalize_addresses == Some(true)
        && comparison.full_trace == Some(true)
        && comparison.exact_remainder == Some(true)
        && comparison
            .stripped_prefixes
            .as_ref()
            .is_some_and(|prefixes| {
                prefixes
                    .iter()
                    .map(String::as_str)
                    .eq([STRIP_WALL_CLOCK_PREFIX_V1])
            })
        && comparison.canonicalizations.as_ref().is_some_and(|rules| {
            rules
                .iter()
                .map(String::as_str)
                .eq([CANON_ADDRESS_ORDINAL_V1])
        })
        && comparison.ignore_lines == Some(false)
        && comparison.skip_commit == Some(false)
        && comparison.skip_detlog == Some(false)
    {
        Ok(())
    } else {
        Err("verification did not retain the complete canonical policy, output-buffer comparison, and exact output/status operands".into())
    }
}

/// Consume the existing current-producer schema without certifying a physical
/// phase. This pure boundary is shared by the collector and its reader control.
pub fn verification_assertions(bytes: Result<&[u8], String>) -> Vec<Assertion> {
    let report = bytes.and_then(VerificationReport::from_current_json_slice);
    let policy = report
        .as_ref()
        .map_err(Clone::clone)
        .and_then(require_verification_policy);
    let typed = match &report {
        Ok(report) => Assertion {
            name: "verification.report".into(),
            verdict: Verdict::Match,
            detail: format!(
                "current typed receipt: verdict={} verified={} bitwise_parity={} compared_INFO={:?} output_operands={} first_divergent_record={:?}; raw-byte retention is checked independently",
                report.verdict,
                report.verified,
                report.bitwise_parity,
                report
                    .compared_log_messages
                    .map(|counts| (counts.left, counts.right)),
                report.compared_outputs.is_some(),
                report.first_divergent_record,
            ),
        },
        Err(error) => Assertion {
            name: "verification.report".into(),
            verdict: Verdict::NoResult,
            detail: error.clone(),
        },
    };
    let comparison = match &policy {
        Ok(()) => Assertion {
            name: "verification.comparison_policy".into(),
            verdict: Verdict::Match,
            detail: "canonical named envelope, nonzero left/right INFO, buffer hashes, exact remainder, no line filters, and output/status operands; actual time policy retained".into(),
        },
        Err(error) => Assertion {
            name: "verification.comparison_policy".into(),
            verdict: Verdict::NoResult,
            detail: error.clone(),
        },
    };
    let parity = match &report {
        Ok(report) if report.verdict == VerificationVerdict::Diverged => Assertion {
            name: "verification.parity".into(),
            verdict: Verdict::Mismatch,
            detail: format!(
                "actual typed divergence retained; first divergent record={:?}; weaker or incomplete evidence cannot erase it",
                report.first_divergent_record
            ),
        },
        Ok(report) if report.verdict == VerificationVerdict::Matched => {
            match policy
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|_| report.require_canonical_match())
            {
                Ok(()) => Assertion {
                    name: "verification.parity".into(),
                    verdict: Verdict::Match,
                    detail: "typed matched verdict and complete canonical comparison agree, including exact guest status/stdout/stderr".into(),
                },
                Err(error) => Assertion {
                    name: "verification.parity".into(),
                    verdict: Verdict::NoResult,
                    detail: error,
                },
            }
        }
        Ok(report) => Assertion {
            name: "verification.parity".into(),
            verdict: Verdict::NoResult,
            detail: format!(
                "verification verdict={} supplies no admitted match",
                report.verdict
            ),
        },
        Err(error) => Assertion {
            name: "verification.parity".into(),
            verdict: Verdict::NoResult,
            detail: error.clone(),
        },
    };
    vec![
        typed,
        comparison,
        parity,
        Assertion {
            name: "verification.physical_phases".into(),
            verdict: Verdict::NoResult,
            detail: "typed comparison operands do not bind every physical generation, actual guest exec identity, or live native phase; all existing isolation and coverage checks remain required".into(),
        },
    ]
}

fn retain_verification_report(directory: &Path) -> Vec<Assertion> {
    let source = directory.join("verify.json");
    let retained = directory.join("verify.retained.json");
    let bytes = read_bounded(&source, VERIFY_REPORT_LIMIT);
    let mut rows = verification_assertions(bytes.as_deref().map_err(Clone::clone));
    let retention = bytes.as_ref().map_err(Clone::clone).and_then(|bytes| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&retained)
            .map_err(|error| format!("fresh verification receipt: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("complete verification receipt copy: {error}"))?;
        let expected = digest_bytes(bytes)?;
        if digest(&retained)? != expected
            || read_bounded(&source, VERIFY_REPORT_LIMIT)? != *bytes
        {
            return Err("verification read/copy/source bytes differ".into());
        }
        Ok(format!(
            "complete bounded read and exact fresh byte copy: bytes={} sha256={expected}; both files enter observations.sha256",
            bytes.len()
        ))
    });
    rows.push(match retention {
        Ok(detail) => Assertion {
            name: "receipt.verify_json".into(),
            verdict: Verdict::Match,
            detail,
        },
        Err(detail) => Assertion {
            name: "receipt.verify_json".into(),
            verdict: Verdict::NoResult,
            detail,
        },
    });
    rows
}

fn retain_command(command: &Command, directory: &Path) -> Result<(), String> {
    let arguments = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| format!("{}\n", hex_bytes(argument.as_encoded_bytes())))
        .collect::<String>();
    fs::write(directory.join("executor.argv.hex"), arguments)
        .map_err(|error| format!("actual executor argv receipt: {error}"))
}

fn capture_version(prepared: &Prepared) -> Result<(), String> {
    let directory = prepared.artifacts.join("version");
    fs::create_dir(&directory).map_err(|error| format!("fresh stamp directory: {error}"))?;
    let execution = prepared.execute(
        &prepared.hermit,
        &["--version".into()],
        &directory,
        STAMP_LIMITS,
        None,
    );
    for name in ["stdout", "stderr"] {
        fs::copy(
            directory.join(name),
            prepared.artifacts.join(format!("hermit.version.{name}")),
        )
        .map_err(|error| format!("retained CLI stamp {name}: {error}"))?;
    }
    let status = execution
        .status
        .ok_or("required CLI stamp left no status")?;
    fs::write(
        prepared.artifacts.join("hermit.version.exit"),
        status_code(status),
    )
    .map_err(|error| format!("CLI stamp exit receipt: {error}"))?;
    execution.boundary?;
    if status.success() {
        Ok(())
    } else {
        Err("required CLI stamp collection failed".to_owned())
    }
}

pub fn find_safehermit(repository: &Path) -> Result<PathBuf, String> {
    repository
        .ancestors()
        .map(|parent| parent.join("bin/safehermit"))
        .find(|path| path.is_file())
        .ok_or_else(|| "safehermit is unavailable; select its path explicitly".to_owned())
}

pub fn integration_artifacts(
    repository: &Path,
    scope: &str,
    backend: &str,
) -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("HERMIT_ISOLATION_ARTIFACTS") {
        if explicit.is_empty() {
            return Err("empty requested isolation artifact directory".to_owned());
        }
        return Ok(PathBuf::from(explicit));
    }
    let invocation = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("artifact timestamp: {error}"))?
        .as_nanos();
    Ok(repository.join("ignored").join(format!(
        "backend-isolation-{scope}-{backend}-{}-{invocation}",
        std::process::id()
    )))
}

impl Prepared {
    pub fn build(
        repository: &Path,
        hermit: &Path,
        safehermit: &Path,
        artifacts: &Path,
    ) -> Result<Self, String> {
        Self::build_with_installation(repository, hermit, safehermit, artifacts, None)
    }

    pub fn build_with_installation(
        repository: &Path,
        hermit: &Path,
        safehermit: &Path,
        artifacts: &Path,
        installation: Option<&Path>,
    ) -> Result<Self, String> {
        let safehermit = safehermit
            .canonicalize()
            .map_err(|error| format!("safehermit: {error}"))?;
        Self::prepare(
            repository,
            hermit,
            Executor::SafeHermit(safehermit),
            artifacts,
            installation,
        )
    }

    pub fn build_integration(
        repository: &Path,
        hermit: &Path,
        artifacts: &Path,
        capture: CaptureCommand,
        entry_source: &Path,
    ) -> Result<Self, String> {
        let binary = std::env::current_exe()
            .map_err(|error| format!("integration executor binary: {error}"))?;
        let sources = [
            repository.join("hermit-cli/tests/common/bounded_command.rs"),
            repository.join(entry_source),
        ]
        .into_iter()
        .map(|path| {
            path.canonicalize()
                .map_err(|error| format!("integration executor source: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
        Self::prepare(
            repository,
            hermit,
            Executor::Integration {
                capture,
                sources,
                binary,
            },
            artifacts,
            None,
        )
    }

    fn prepare(
        repository: &Path,
        hermit: &Path,
        executor: Executor,
        artifacts: &Path,
        installation: Option<&Path>,
    ) -> Result<Self, String> {
        let repository = repository
            .canonicalize()
            .map_err(|error| format!("repository: {error}"))?;
        let hermit = hermit
            .canonicalize()
            .map_err(|error| format!("Hermit executable: {error}"))?;
        let artifacts = if artifacts.is_absolute() {
            artifacts.to_path_buf()
        } else {
            repository.join(artifacts)
        };
        if !(artifacts.starts_with(repository.join("ignored"))
            || artifacts.starts_with(repository.join("target")))
        {
            return Err(
                "isolation artifacts must be under this repository's ignored/ or target/"
                    .to_owned(),
            );
        }
        let ignored = repository.join("ignored");
        if !ignored.exists() {
            fs::create_dir(&ignored).map_err(|error| format!("ignored artifact root: {error}"))?;
        }
        let parent = artifacts
            .parent()
            .ok_or("artifact directory has no parent")?
            .canonicalize()
            .map_err(|error| format!("existing artifact parent: {error}"))?;
        if !(parent.starts_with(&ignored) || parent.starts_with(repository.join("target"))) {
            return Err(
                "canonical artifact parent escaped this repository's ignored/ or target/"
                    .to_owned(),
            );
        }
        let artifacts = parent.join(
            artifacts
                .file_name()
                .ok_or("artifact directory must have a normal name")?,
        );
        fs::create_dir(&artifacts).map_err(|error| format!("fresh artifact directory: {error}"))?;
        let artifacts = artifacts
            .canonicalize()
            .map_err(|error| format!("artifacts: {error}"))?;
        if !(artifacts.starts_with(repository.join("ignored"))
            || artifacts.starts_with(repository.join("target")))
        {
            return Err(
                "isolation artifacts must be under this repository's ignored/ or target/"
                    .to_owned(),
            );
        }
        let guest_source = artifacts.join("backend_isolation.c");
        let native_source = artifacts.join("native_launcher.c");
        fs::copy(
            repository.join("tests/isolation/backend_isolation.c"),
            &guest_source,
        )
        .map_err(|error| format!("guest producer snapshot: {error}"))?;
        fs::copy(
            repository.join("tests/isolation/native_launcher.c"),
            &native_source,
        )
        .map_err(|error| format!("native producer snapshot: {error}"))?;
        fs::copy(
            repository.join("hermit-cli/tests/common/backend_isolation.rs"),
            artifacts.join("assertions.rs"),
        )
        .map_err(|error| format!("assertion producer snapshot: {error}"))?;
        fs::copy(
            repository.join("tests/isolation/driver.rs"),
            artifacts.join("driver.rs"),
        )
        .map_err(|error| format!("driver producer snapshot: {error}"))?;
        let guest = artifacts.join("guest");
        let native = artifacts.join("native-launcher");
        compile(&guest_source, &guest, &artifacts.join("guest.compile.log"))?;
        compile(
            &native_source,
            &native,
            &artifacts.join("native.compile.log"),
        )?;
        let cohort_inputs = prepare_cohort_inputs(&artifacts)?;
        let mut hashes = String::new();
        let mut producers = vec![
            guest_source,
            native_source,
            guest.clone(),
            native.clone(),
            hermit.clone(),
            artifacts.join("assertions.rs"),
            artifacts.join("driver.rs"),
            std::env::current_exe()
                .map_err(|error| format!("Rust collector executable: {error}"))?,
        ];
        for (snapshot, source) in VERIFICATION_INPUTS {
            let path = artifacts.join(snapshot);
            fs::copy(repository.join(source), &path)
                .map_err(|error| format!("verification input snapshot {snapshot}: {error}"))?;
            producers.push(path);
        }
        match &executor {
            Executor::SafeHermit(path) => producers.push(path.clone()),
            Executor::Integration { sources, .. } => {
                for (index, source) in sources.iter().enumerate() {
                    let snapshot = artifacts.join(format!("executor-source-{index}.rs"));
                    fs::copy(source, &snapshot)
                        .map_err(|error| format!("executor source snapshot: {error}"))?;
                    producers.push(snapshot);
                }
            }
        }
        for input in cohort_inputs.values() {
            producers.push(input.initial_path());
        }
        for path in producers {
            hashes.push_str(&format!("{}  {}\n", digest(&path)?, path.display()));
        }
        fs::write(artifacts.join("inputs.sha256"), hashes)
            .map_err(|error| format!("producer hashes: {error}"))?;
        let installation = installation_for(&repository, &hermit, installation)?;
        let mut prepared = Self {
            repository,
            hermit,
            artifacts,
            guest,
            native,
            installation,
            executor,
            input_inventory: String::new(),
            cohort_inputs,
        };
        fs::write(
            prepared.artifacts.join("installation-root.hex"),
            hex_bytes(prepared.installation.as_os_str().as_encoded_bytes()),
        )
        .map_err(|error| format!("declared resource root receipt: {error}"))?;
        prepared.input_inventory = input_inventory(&prepared)?;
        fs::write(
            prepared.artifacts.join("installation.inputs"),
            &prepared.input_inventory,
        )
        .map_err(|error| format!("installed resource inventory: {error}"))?;
        capture_version(&prepared)?;
        Ok(prepared)
    }

    fn execute(
        &self,
        payload: &Path,
        arguments: &[OsString],
        directory: &Path,
        limits: ExecutionLimits,
        physical: Option<&Path>,
    ) -> Execution {
        let mut known_status = None;
        let attempted = (|| -> Result<Execution, String> {
            let mut command;
            let execution = match &self.executor {
                Executor::Integration { capture, .. } => {
                    command = Command::new(payload);
                    command
                        .args(arguments)
                        .current_dir(&self.repository)
                        .env("HERMIT_INSTALL_DIR", &self.installation);
                    retain_command(&command, directory)?;
                    let captured = capture(
                        &mut command,
                        directory,
                        Duration::from_secs(limits.wall_seconds),
                        limits.stream_bytes,
                    );
                    known_status = captured.status;
                    fs::write(
                        directory.join("capture.receipt"),
                        format!("{captured:#?}\n"),
                    )
                    .map_err(|error| format!("complete local capture receipt: {error}"))?;
                    let boundary = capture_completion(&captured, limits, directory).and_then(|detail| {
                        let expected = physical.map_or(Ok(0), expected_native_outcome)?;
                        if captured.status.and_then(|status| status.code()) != Some(expected) {
                            return Err(format!("local executor/payload status differs: {:?}, expected={expected}", captured.status));
                        }
                        Ok(detail)
                    });
                    Execution {
                        status: captured.status,
                        boundary,
                    }
                }
                Executor::SafeHermit(wrapper) => {
                    let resources = directory.join("resources");
                    let mut invocation = payload.as_os_str().as_encoded_bytes().to_vec();
                    invocation.push(0);
                    for argument in arguments {
                        invocation.extend_from_slice(argument.as_encoded_bytes());
                        invocation.push(0);
                    }
                    let invocation_digest = digest_bytes(&invocation)?;
                    let payload_digest = digest(payload)?;
                    command = Command::new(wrapper);
                    command
                        .arg(format!("--sh-resource-report={}", resources.display()))
                        .arg(format!("--sh-memory-max-bytes={}", limits.memory))
                        .arg(format!("--sh-tasks-max={}", limits.tasks))
                        .arg("--sh-cpu-cores=1")
                        .arg(format!("--sh-cpu-timeout={}", limits.cpu_seconds))
                        .arg(format!("--sh-deadline={}", limits.wall_seconds))
                        .arg(format!("--sh-max-log-bytes={}", limits.stream_bytes))
                        .arg(format!("--sh-bin={}", payload.display()))
                        .arg("--")
                        .args(arguments)
                        .current_dir(&self.repository)
                        .env("SAFEHERMIT_LOG_ROOT", self.artifacts.join("safehermit"))
                        .env("HERMIT_INSTALL_DIR", &self.installation)
                        .stdin(Stdio::null())
                        .stdout(
                            File::create(directory.join("stdout"))
                                .map_err(|error| format!("stdout capture: {error}"))?,
                        )
                        .stderr(
                            File::create(directory.join("stderr"))
                                .map_err(|error| format!("stderr capture: {error}"))?,
                        );
                    retain_command(&command, directory)?;
                    let status = command
                        .status()
                        .map_err(|error| format!("SafeHermit executor: {error}"))?;
                    known_status = Some(status);
                    let boundary = (|| {
                        let expected = physical.map_or(Ok(0), expected_native_outcome)?;
                        let stderr = read_bounded(&directory.join("stderr"), limits.stream_bytes)?;
                        let stdout = fs::metadata(directory.join("stdout"))
                            .map_err(|error| format!("stdout count: {error}"))?
                            .len();
                        if stdout > limits.stream_bytes {
                            return Err("stdout exceeds complete capture bound".to_owned());
                        }
                        let resources = read_bounded(&resources, 1024 * 1024)?;
                        safe_completion(
                            &stderr,
                            &resources,
                            status,
                            &payload_digest,
                            &invocation_digest,
                            expected,
                            limits,
                        )
                    })();
                    Execution {
                        status: Some(status),
                        boundary,
                    }
                }
            };
            let identity = match &self.executor {
                Executor::SafeHermit(path) => {
                    format!("executor=safehermit binary_sha256={}\n", digest(path)?)
                }
                Executor::Integration {
                    sources, binary, ..
                } => {
                    let mut identity =
                        format!("executor=integration binary_sha256={}\n", digest(binary)?);
                    for (index, source) in sources.iter().enumerate() {
                        identity.push_str(&format!("source_{index}_sha256={}\n", digest(source)?));
                    }
                    identity
                }
            };
            fs::write(
                directory.join("executor.boundary"),
                format!(
                    "{identity}status={:?}\nverdict={:?}\n",
                    execution.status, execution.boundary
                ),
            )
            .map_err(|error| format!("retained executor completion verdict: {error}"))?;
            Ok(execution)
        })();
        attempted.unwrap_or_else(|error| Execution {
            status: known_status,
            boundary: Err(error),
        })
    }

    pub fn collect(&self, cell: Cell) -> Collected {
        let directory = self.artifacts.join(cell.name().replace(':', "-"));
        let collected = self.collect_inner(&cell, &directory);
        let (guest, native, execution, boundary, verification) = match collected {
            Ok(results) => results,
            Err(error) => (
                Err(error.clone()),
                Err(error.clone()),
                error.clone(),
                Err(error),
                Vec::new(),
            ),
        };
        let expected_cwd = self
            .cohort_inputs
            .get(&cohort_key(&cell))
            .map_or_else(|| directory.join("workdir"), CohortInput::workdir);
        let mut assertions = conformance::check_isolation(
            guest.as_ref().map_err(Clone::clone),
            native.as_ref().map_err(Clone::clone),
            cell.policy,
            expected_cwd.as_os_str().as_encoded_bytes(),
        );
        let cli_exit = native
            .as_ref()
            .map_err(Clone::clone)
            .and_then(Observation::cli_exit);
        assertions.push(match cli_exit {
            Ok((code, signal)) => Assertion {
                name: "execution.cli_exit".to_owned(),
                verdict: if code == 0 && signal == 0 {
                    Verdict::Match
                } else {
                    Verdict::Mismatch
                },
                detail: format!("CLI code={code}, signal={signal}; collector {execution}"),
            },
            Err(error) => Assertion {
                name: "execution.cli_exit".to_owned(),
                verdict: Verdict::NoResult,
                detail: error,
            },
        });
        assertions.push(match input_inventory(self) {
            Ok(inventory) => Assertion {
                name: "receipt.input_stability".to_owned(),
                verdict: if inventory == self.input_inventory { Verdict::Match } else { Verdict::NoResult },
                detail: "content hashes of the CLI, selected executor and sources, guest, native helper, verification parser/policy/standalone/manifest/lock inputs, and complete declared installation before and after collection".to_owned(),
            },
            Err(error) => Assertion { name: "receipt.input_stability".to_owned(), verdict: Verdict::NoResult, detail: error },
        });
        assertions.push(match boundary {
            Ok(detail) => Assertion {
                name: "receipt.executor_boundary".to_owned(),
                verdict: Verdict::Match,
                detail,
            },
            Err(detail) => Assertion {
                name: "receipt.executor_boundary".to_owned(),
                verdict: Verdict::NoResult,
                detail,
            },
        });
        if !matches!(cell.path.as_str(), "run-raw" | "stdin-finite") {
            assertions.push(Assertion {
                name: "coverage.required_path".to_owned(),
                verdict: Verdict::NoResult,
                detail: format!("{} requires its complete physical-generation, fixture, and strict-comparison evidence; ordinary report bytes do not establish it", cell.path),
            });
        }
        assertions.extend(verification);
        if !assertions
            .iter()
            .any(|row| row.name == "receipt.cohort_fixture")
        {
            assertions.push(Assertion {
                name: "receipt.cohort_fixture".to_owned(),
                verdict: Verdict::NoResult,
                detail: if ordinary_cohort(&cell) {
                    "ordinary shared cwd/marker preparation or per-operand capture did not complete; retained collector and checker failures remain".to_owned()
                } else {
                    "special or pinned cohort keeps its original per-cell paths; its declared input binding remains unfinished, without redirecting it to an ordinary fixture".to_owned()
                },
            });
        }
        assertions.push(Assertion {
            name: "receipt.cohort_complete_input".to_owned(),
            verdict: Verdict::NoResult,
            detail: "shared ordinary cwd/marker observations do not authenticate whole input equality: native baseline/PID/IPC/socket argv, per-cell output/store paths, special-route inputs, and all physical phase/generation bindings remain explicit gaps; no raw path or observation normalization".to_owned(),
        });
        if let Err(error) = conformance::write_report(
            &directory.join("assertions.json"),
            &cell.name(),
            &assertions,
        ) {
            assertions.push(Assertion {
                name: "receipt.typed_report".to_owned(),
                verdict: Verdict::NoResult,
                detail: error,
            });
        }
        Collected {
            cell,
            directory,
            guest,
            native,
            assertions,
        }
    }

    fn collect_inner(
        &self,
        cell: &Cell,
        directory: &Path,
    ) -> Result<CollectedObservations, String> {
        fs::create_dir(directory).map_err(|error| format!("fresh cell artifacts: {error}"))?;
        let cohort = self.cohort_inputs.get(&cohort_key(cell));
        let (workdir, sentinel) = match cohort {
            Some(input) => (input.workdir(), input.sentinel()),
            None => {
                let workdir = directory.join("workdir");
                let sentinel = directory.join("sentinel");
                fs::create_dir(&workdir).map_err(|error| format!("cwd fixture: {error}"))?;
                fs::create_dir(&sentinel)
                    .map_err(|error| format!("descriptor fixture: {error}"))?;
                fs::write(
                    sentinel.join("descriptor-marker"),
                    b"controlled descriptor identity\n",
                )
                .map_err(|error| format!("descriptor marker: {error}"))?;
                (workdir, sentinel)
            }
        };
        let physical = directory.join("physical");
        let stdout = directory.join("stdout");
        let stderr = directory.join("stderr");
        let mut arguments: Vec<OsString> = vec![
            "--observe".into(),
            physical.as_os_str().to_owned(),
            "--guest-name".into(),
            "guest".into(),
            "--eager-input".into(),
            "--sentinel-directory".into(),
            sentinel.as_os_str().to_owned(),
            "--".into(),
            self.hermit.as_os_str().to_owned(),
            "--log=info".into(),
            format!("--backend={}", cell.backend).into(),
        ];
        let record = cell.path.starts_with("record") || cell.path.starts_with("replay");
        if record {
            arguments.extend([
                "record".into(),
                "start".into(),
                format!("--data-dir={}", directory.join("recording").display()).into(),
            ]);
        } else {
            arguments.extend([
                "run".into(),
                "--strict".into(),
                "--max-timeslice=disabled".into(),
            ]);
        }
        if cell.path != "verify-normal" && !record {
            arguments.push("--no-virtualize-metadata".into());
        }
        if cell.path.starts_with("verify") || cell.path == "record-verify" {
            arguments.extend([
                "--verify".into(),
                "--verify-strict".into(),
                format!("--verify-json={}", directory.join("verify.json").display()).into(),
            ]);
        }
        match cell.policy {
            NamespacePolicy::Private => {}
            NamespacePolicy::HostNetwork => arguments.push("--network=host".into()),
            NamespacePolicy::None => arguments.push("--no-namespace".into()),
        }
        arguments.extend([
            "--base-env=minimal".into(),
            format!("--workdir={}", workdir.display()).into(),
            "--".into(),
            self.guest.as_os_str().to_owned(),
            "--observer-barrier".into(),
        ]);
        if cell.path == "verify-normal" {
            arguments.push("--normal-metadata".into());
        }
        let invocation = arguments
            .iter()
            .map(|arg| format!("{}\n", hex_bytes(arg.as_encoded_bytes())))
            .collect::<String>();
        fs::write(directory.join("argv.hex"), invocation)
            .map_err(|error| format!("command receipt: {error}"))?;
        fs::write(directory.join("measurement.conf"), format!("backend={}\nroot={}\npath={}\nnamespace_policy={}\nlog=info\nmetadata={}\nrelaxations=retired-branch max-timeslice disabled for isolation measurement; raw metadata exposes physical identities\nassurance=isolation observation; no L1 or L2 claim\n", cell.backend, cell.root, cell.path, policy_name(cell.policy), if cell.path == "verify-normal" { "normal" } else { "raw" }))
            .map_err(|error| format!("measurement configuration receipt: {error}"))?;
        if cell.root != "host" {
            return Err("pinned required cell remains no_result until its immediate-host baseline, loader inputs, and scoped native observer are collected; no host-run substitution".to_owned());
        }
        let cohort_before = cohort.map(|input| {
            fs::write(directory.join("cohort.inputs.reference"), input.reference())
                .map_err(|error| format!("cohort reference receipt: {error}"))?;
            input.retain_snapshot(&directory.join("cohort.inputs.before"))
        });
        let execution = self.execute(
            &self.native,
            &arguments,
            directory,
            NATIVE_LIMITS,
            Some(&physical),
        );
        let cohort_after =
            cohort.map(|input| input.retain_snapshot(&directory.join("cohort.inputs.after")));
        let code = execution
            .status
            .map_or_else(|| "unavailable".to_owned(), status_code);
        fs::write(directory.join("collector.exit"), &code)
            .map_err(|error| format!("collector exit receipt: {error}"))?;
        let guest = Observation::read(&stdout);
        let native = Observation::read(&physical);
        let verification_requested =
            cell.path.starts_with("verify") || cell.path == "record-verify";
        let mut verification = if verification_requested {
            retain_verification_report(directory)
        } else {
            Vec::new()
        };
        if let (Some(input), Some(before), Some(after)) = (cohort, cohort_before, cohort_after) {
            verification.push(input.assertion(directory, before, after));
        }
        let mut boundary = execution.boundary;
        let mut hashes = String::new();
        let mut observations = vec![
            stdout,
            stderr,
            physical,
            directory.join("executor.boundary"),
        ];
        if matches!(&self.executor, Executor::SafeHermit(_)) {
            observations.push(directory.join("resources"));
        } else {
            observations.push(directory.join("capture.receipt"));
        }
        if verification_requested {
            observations.push(directory.join("verify.json"));
            observations.push(directory.join("verify.retained.json"));
        }
        if let Some(input) = cohort {
            observations.push(input.initial_path());
            observations.push(directory.join("cohort.inputs.reference"));
            observations.push(directory.join("cohort.inputs.before"));
            observations.push(directory.join("cohort.inputs.after"));
        }
        for path in observations {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            match digest(&path) {
                Ok(digest) => hashes.push_str(&format!("{digest}  {name}\n")),
                Err(error) => {
                    hashes.push_str(&format!("UNAVAILABLE {name} {error}\n"));
                    if boundary.is_ok() {
                        boundary = Err(error);
                    }
                }
            }
        }
        if let Err(error) = fs::write(directory.join("observations.sha256"), hashes) {
            let error = format!("observation hashes: {error}");
            boundary = Err(match boundary {
                Ok(_) => error,
                Err(previous) => format!("{previous}; {error}"),
            });
        }
        Ok((guest, native, code, boundary, verification))
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("String writes succeed");
    }
    encoded
}

#[cfg(test)]
mod executor_controls {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn scoped(boundary: Option<Verdict>) -> Collected {
        let mut assertions = LAUNCH_MIGRATION_ASSERTIONS
            .into_iter()
            .chain(["execution.cli_exit", "receipt.input_stability"])
            .map(|name| Assertion {
                name: name.to_owned(),
                verdict: Verdict::Match,
                detail: "synthetic admission control".to_owned(),
            })
            .collect::<Vec<_>>();
        if let Some(verdict) = boundary {
            assertions.push(Assertion {
                name: "receipt.executor_boundary".to_owned(),
                verdict,
                detail: "synthetic boundary control".to_owned(),
            });
        }
        Collected {
            cell: Cell {
                backend: "dbt".to_owned(),
                root: "host".to_owned(),
                path: "run-raw".to_owned(),
                policy: NamespacePolicy::Private,
            },
            directory: PathBuf::new(),
            guest: Err("not a guest execution".to_owned()),
            native: Err("not a native observation".to_owned()),
            assertions,
        }
    }

    fn safe_evidence(code: u64) -> (String, String) {
        let binary = "a".repeat(64);
        let invocation = "b".repeat(64);
        let stderr = format!(
            "safehermit: unit=owned-control\nsafehermit: run_id=control\nsafehermit: binary_sha256={binary}\nsafehermit: bytes_written=12\nsafehermit: truncated=false\nsafehermit: runner_exit_code=0\nsafehermit: unit_result=success\nsafehermit: resource_report_state=COMPLETE\nsafehermit: exit_code={code}\n"
        );
        let mut resources = format!(
            "safehermit: unit=owned-control\nsafehermit: run_id=control\nsafehermit: binary_sha256={binary}\nsafehermit: invocation_sha256={invocation}\n"
        );
        for (key, value) in [
            ("memory_peak_bytes", 1),
            ("cpu_usage_nsec", 1),
            ("tasks_peak", 1),
            ("limit_memory_bytes", NATIVE_LIMITS.memory),
            ("limit_swap_bytes", 0),
            ("limit_tasks", NATIVE_LIMITS.tasks),
            ("limit_cpu_cores_micros", 1_000_000),
            (
                "limit_cpu_time_nsec",
                NATIVE_LIMITS.cpu_seconds * 1_000_000_000,
            ),
            (
                "limit_wall_time_usec",
                NATIVE_LIMITS.wall_seconds * 1_000_000,
            ),
            ("cpu_time_limit_hit", 0),
            ("memory_oom_kills", 0),
            ("tasks_limit_hits", 0),
            ("wall_time_limit_hit", 0),
            ("log_cap_hit", 0),
        ] {
            resources.push_str(&format!("safehermit: {key}={value}\n"));
        }
        (stderr, resources)
    }

    fn check_safe(
        stderr: &str,
        resources: &str,
        actual: i32,
        expected: i32,
    ) -> Result<String, String> {
        safe_completion(
            stderr.as_bytes(),
            resources.as_bytes(),
            ExitStatus::from_raw(actual << 8),
            &"a".repeat(64),
            &"b".repeat(64),
            expected,
            NATIVE_LIMITS,
        )
    }

    #[test]
    fn executor_failure_blocks_otherwise_successful_launch_rows() {
        assert!(scoped(Some(Verdict::Match)).launch_migration_matches());
        assert!(!scoped(Some(Verdict::NoResult)).launch_migration_matches());
        assert!(!scoped(Some(Verdict::Mismatch)).launch_migration_matches());
    }

    #[test]
    fn typed_report_failure_blocks_otherwise_successful_launch_rows() {
        let mut result = scoped(Some(Verdict::Match));
        assert!(result.launch_migration_matches());
        result.assertions.push(Assertion {
            name: "receipt.typed_report".to_owned(),
            verdict: Verdict::NoResult,
            detail: "synthetic required report-write failure".to_owned(),
        });
        assert!(!result.launch_migration_matches());
    }

    #[test]
    fn missing_or_duplicate_executor_boundary_cannot_match() {
        assert!(!scoped(None).launch_migration_matches());
        let mut duplicate = scoped(Some(Verdict::Match));
        duplicate.assertions.push(Assertion {
            name: "receipt.executor_boundary".to_owned(),
            verdict: Verdict::Match,
            detail: "duplicate".to_owned(),
        });
        assert!(!duplicate.launch_migration_matches());
        assert!(!duplicate.full_matches());
    }

    #[test]
    fn incomplete_duplicate_or_conflicting_safe_receipt_refuses() {
        let (stderr, resources) = safe_evidence(0);
        assert!(check_safe(&stderr, &resources, 0, 0).is_ok());
        assert!(
            check_safe(
                &stderr,
                &resources.replace("safehermit: tasks_peak=1\n", ""),
                0,
                0
            )
            .is_err()
        );
        assert!(
            check_safe(
                &stderr,
                &format!("{resources}safehermit: unit=owned-control\n"),
                0,
                0
            )
            .is_err()
        );
        assert!(
            check_safe(
                &stderr.replace("exit_code=0", "exit_code=1"),
                &resources,
                0,
                0
            )
            .is_err()
        );
    }

    #[test]
    fn late_resource_failure_blocks_scoped_payload_success() {
        let (stderr, resources) = safe_evidence(2);
        let late = stderr.replace(
            "resource_report_state=COMPLETE",
            "resource_report_state=INCOMPLETE:reference release failed",
        );
        let outcome = check_safe(&late, &resources, 2, 0);
        assert!(outcome.is_err());
        let result = scoped(Some(if outcome.is_ok() {
            Verdict::Match
        } else {
            Verdict::NoResult
        }));
        assert!(!result.launch_migration_matches());
        assert!(
            check_safe(
                &stderr,
                &resources.replace("memory_oom_kills=0", "memory_oom_kills=1"),
                2,
                2
            )
            .is_err()
        );
    }

    #[test]
    fn collector_two_requires_source_bound_footer_outcome() {
        let (stderr, resources) = safe_evidence(2);
        assert!(check_safe(&stderr, &resources, 2, 2).is_ok());
        assert!(check_safe(&stderr, &resources, 2, 0).is_err());
        let complete = "CLI_EXIT child exited=1 code=0 signal=0\nNATIVE_COMPLETE backend-isolation-physical-v1 snapshots=1 identities=1 checkpoints=0 errors=0 live=0\n";
        assert_eq!(native_outcome(complete.as_bytes()).unwrap(), 0);
        assert_eq!(
            native_outcome(complete.replace("errors=0", "errors=1").as_bytes()).unwrap(),
            2
        );
    }

    #[test]
    fn missing_duplicate_or_partial_native_footer_refuses() {
        let footer = "NATIVE_COMPLETE backend-isolation-physical-v1 snapshots=1 identities=1 checkpoints=0 errors=0 live=0\n";
        let cli = "CLI_EXIT child exited=1 code=0 signal=0\n";
        assert!(native_outcome(cli.as_bytes()).is_err());
        assert!(native_outcome(format!("{cli}{footer}{footer}").as_bytes()).is_err());
        assert!(native_outcome(format!("{cli}{}", footer.trim_end()).as_bytes()).is_err());
    }
}
