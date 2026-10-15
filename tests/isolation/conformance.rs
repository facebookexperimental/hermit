#!/usr/bin/env -S rust-script --force
//! ```cargo
//! [package]
//! edition = "2024"
//! [dependencies]
//! hermit-manifest-plan = { path = "../../ci/manifest-plan" }
//! detcore-model = { path = "../../detcore-model" }
//! ```

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub mod driver;
#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write as _;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn close_range(first: u32, last: u32, flags: i32) -> i32;
}

const SIGKILL: i32 = 9;
const CLOSE_RANGE_CLOEXEC: i32 = 4;

/// Marks every descriptor this process inherited beyond stdio close-on-exec,
/// so the cells' native baselines and guests see only the descriptors the
/// driver gives them, whatever the caller left open.
fn close_inherited_descriptors() -> Result<(), String> {
    // SAFETY: close_range only changes descriptor flags of this process.
    if unsafe { close_range(3, u32::MAX, CLOSE_RANGE_CLOEXEC) } != 0 {
        return Err(format!("close_range: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Copies `stream` into `path`, at most `limit` bytes, and returns the bytes
/// read, the bytes written, and whether it reached EOF.
fn copy_bounded(
    mut stream: impl Read,
    path: &Path,
    limit: u64,
) -> (u64, u64, bool, Option<String>) {
    let (mut read, mut written) = (0_u64, 0_u64);
    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(error) => return (0, 0, false, Some(format!("{}: {error}", path.display()))),
    };
    let mut buffer = [0_u8; 65_536];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return (read, written, true, None),
            Ok(count) => {
                read += count as u64;
                let room = limit.saturating_sub(written).min(count as u64) as usize;
                if let Err(error) = file.write_all(&buffer[..room]) {
                    return (
                        read,
                        written,
                        false,
                        Some(format!("{}: {error}", path.display())),
                    );
                }
                written += room as u64;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return (read, written, false, Some(format!("read: {error}"))),
        }
    }
}

/// The local executor: runs `command` as this process's own child, in a new
/// process group with stdin closed, copies its stdout and stderr into
/// `directory` up to `stream_bytes` each, kills its group at `deadline`, and
/// kills the group again once it exits so that no descendant outlives the
/// cell. Memory, task and CPU bounds are the caller's (the validation DAG
/// boxes the whole node); a wrapper is not nested inside it. Each cell
/// chooses its runs' working directory itself, so the E2E harness's
/// `HERMIT_E2E_EMPTY_WORKDIR=/test`, which validation exports and which makes
/// a DBT run refuse any other `--workdir`, is not passed on.
fn local_capture(
    command: &mut Command,
    directory: &Path,
    deadline: Duration,
    stream_bytes: u64,
) -> driver::Capture {
    let started = Instant::now();
    let mut capture = driver::Capture {
        status: None,
        stdout_read: 0,
        stdout_written: 0,
        stderr_read: 0,
        stderr_written: 0,
        stdout_eof: false,
        stderr_eof: false,
        reaped: false,
        cleanup_complete: false,
        elapsed: Duration::ZERO,
        failure: None,
    };
    let mut child = match command
        .env_remove("HERMIT_E2E_EMPTY_WORKDIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            capture.failure = Some(format!("spawn: {error}"));
            return capture;
        }
    };
    let group = child.id() as i32;
    let (sender, receiver) = mpsc::channel();
    for (name, stream) in [
        (
            "stdout",
            child
                .stdout
                .take()
                .map(|stream| Box::new(stream) as Box<dyn Read + Send>),
        ),
        (
            "stderr",
            child
                .stderr
                .take()
                .map(|stream| Box::new(stream) as Box<dyn Read + Send>),
        ),
    ] {
        let sender = sender.clone();
        let path = directory.join(name);
        std::thread::spawn(move || {
            let result = match stream {
                Some(stream) => copy_bounded(stream, &path, stream_bytes),
                None => (0, 0, false, Some(format!("{name} was not piped"))),
            };
            let _ = sender.send((name, result));
        });
    }
    drop(sender);
    let mut failures = Vec::new();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                capture.status = Some(status);
                break;
            }
            Ok(None) if started.elapsed() >= deadline => {
                // SAFETY: kill only sends a signal to the child's own group.
                unsafe { kill(-group, SIGKILL) };
                failures.push(format!(
                    "deadline of {deadline:?} reached; process group killed"
                ));
                capture.status = child.wait().ok();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                failures.push(format!("wait: {error}"));
                break;
            }
        }
    }
    capture.reaped = capture.status.is_some();
    // SAFETY: as above; the group outlives its leader only through stray
    // descendants, which this ends.
    unsafe { kill(-group, SIGKILL) };
    let grace = Instant::now() + Duration::from_secs(5);
    let mut streams = 0;
    while streams < 2 {
        let remaining = grace.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok((name, (read, written, eof, failure))) => {
                streams += 1;
                if name == "stdout" {
                    (
                        capture.stdout_read,
                        capture.stdout_written,
                        capture.stdout_eof,
                    ) = (read, written, eof);
                } else {
                    (
                        capture.stderr_read,
                        capture.stderr_written,
                        capture.stderr_eof,
                    ) = (read, written, eof);
                }
                failures.extend(failure);
            }
            Err(_) => {
                failures.push("a stream stayed open after the process group was killed".to_owned());
                break;
            }
        }
    }
    capture.cleanup_complete = streams == 2;
    capture.elapsed = started.elapsed();
    if !failures.is_empty() {
        capture.failure = Some(failures.join("; "));
    }
    capture
}

/// An assertion's family: its name with every run of digits written `N`, so
/// that a descriptor's or a task's number does not make a new family.
fn family(name: &str) -> String {
    let mut family = String::new();
    let mut in_number = false;
    for character in name.chars() {
        if !character.is_ascii_digit() {
            family.push(character);
        } else if !in_number {
            family.push('N');
        }
        in_number = character.is_ascii_digit();
    }
    family
}

/// What a ledger row expects.
enum Expectation {
    /// Cells, as `root:path:policy` with `*` for any, that collect no guest
    /// observations (`guest.complete` is not a match).
    Uncollected([String; 3]),
    /// An assertion family that does not match in a collected cell, with the
    /// verdicts it may have instead, in the cells a `root:path:policy`
    /// pattern selects.
    Family(String, BTreeSet<String>, [String; 3]),
}

fn pattern_selects(pattern: &[String; 3], cell: &driver::Cell) -> bool {
    [
        cell.root.as_str(),
        cell.path.as_str(),
        driver::policy_name(cell.policy),
    ]
    .iter()
    .zip(pattern)
    .all(|(value, wanted)| wanted == "*" || wanted == value)
}

fn read_pattern(text: &str) -> Option<[String; 3]> {
    <[String; 3]>::try_from(text.split(':').map(str::to_owned).collect::<Vec<_>>()).ok()
}

/// One row of the expected-mismatch ledger: the backends it names, what it
/// expects, and the issue that tracks it.
struct Expected {
    line: usize,
    backends: Vec<String>,
    expectation: Expectation,
    issue: String,
}

impl Expected {
    fn names(&self, backend: &str) -> bool {
        self.backends.iter().any(|named| named == backend)
    }

    fn selects(&self, cell: &driver::Cell) -> bool {
        let Expectation::Uncollected(pattern) = &self.expectation else {
            return false;
        };
        self.names(&cell.backend) && pattern_selects(pattern, cell)
    }
}

/// Reads the ledger: tab-separated `backends selector verdicts issue why`,
/// `#` comments. A selector that is a `root:path:policy` cell pattern (`*`
/// for any) expects `uncollected`; a selector `FAMILY` or
/// `FAMILY@root:path:policy` names an assertion family and expects
/// `mismatch` and/or `no_result`; adding `match` says the family matches
/// where the host hides the defect, and exempts the row from staleness.
/// Backends are named one by one, and every row links its GitHub issue.
fn read_ledger(path: &Path) -> Result<Vec<Expected>, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        let [backends, selector, verdicts, issue, why] = fields[..] else {
            return Err(format!(
                "{}:{line_number}: expected 5 tab-separated fields",
                path.display()
            ));
        };
        let backends: Vec<_> = backends.split(',').map(str::to_owned).collect();
        if let Some(unknown) = backends
            .iter()
            .find(|backend| !driver::conformance::BACKENDS.contains(&backend.as_str()))
        {
            return Err(format!(
                "{}:{line_number}: unknown backend {unknown:?}",
                path.display()
            ));
        }
        if !issue.starts_with("https://github.com/") || why.trim().is_empty() {
            return Err(format!(
                "{}:{line_number}: every row needs a full issue URL and a reason",
                path.display()
            ));
        }
        let verdicts: BTreeSet<_> = verdicts.split(',').map(str::to_owned).collect();
        let (family, cells) = selector.split_once('@').unwrap_or((selector, "*:*:*"));
        let Some(cells) = read_pattern(cells) else {
            return Err(format!(
                "{}:{line_number}: a cell pattern is root:path:policy",
                path.display()
            ));
        };
        let expectation = if !selector.contains('@') && selector.contains(':') {
            let Some(pattern) = read_pattern(selector) else {
                return Err(format!(
                    "{}:{line_number}: a cell pattern is root:path:policy",
                    path.display()
                ));
            };
            if verdicts != BTreeSet::from(["uncollected".to_owned()]) {
                return Err(format!(
                    "{}:{line_number}: a cell pattern expects `uncollected`",
                    path.display()
                ));
            }
            Expectation::Uncollected(pattern)
        } else {
            if !verdicts
                .iter()
                .any(|verdict| verdict == "mismatch" || verdict == "no_result")
                || !verdicts
                    .iter()
                    .all(|verdict| matches!(verdict.as_str(), "mismatch" | "no_result" | "match"))
            {
                return Err(format!(
                    "{}:{line_number}: an assertion family expects mismatch and/or no_result, and may add match",
                    path.display()
                ));
            }
            Expectation::Family(family.to_owned(), verdicts, cells)
        };
        rows.push(Expected {
            line: line_number,
            backends,
            expectation,
            issue: issue.to_owned(),
        });
    }
    Ok(rows)
}

/// Holds the collected cells to the ledger and returns every disagreement:
/// a cell that collects nothing or an assertion that does not match where no
/// row expects it, and, for each backend whose every required cell ran, a
/// row that no longer describes anything (it must be deleted, so that what
/// now matches stays matched).
fn check_ledger(
    ledger: &[Expected],
    collected: &[driver::Collected],
    complete: &BTreeSet<String>,
) -> Vec<String> {
    use driver::conformance::Verdict;

    let mut failures = Vec::new();
    let mut used = BTreeSet::new();
    for result in collected {
        let cell = &result.cell;
        let backend = cell.backend.as_str();
        let guest = result
            .assertions
            .iter()
            .find(|row| row.name == "guest.complete");
        let collected_guest = guest.is_some_and(|row| row.verdict == Verdict::Match);
        let cell_row = ledger.iter().find(|row| row.selects(cell));
        match (collected_guest, cell_row) {
            (false, Some(row)) => {
                used.insert((row.line, backend.to_owned()));
                continue;
            }
            (false, None) => {
                failures.push(format!(
                    "UNEXPECTED: {} collected no guest observations ({}), and no ledger row expects that",
                    cell.name(),
                    guest.map_or("no guest.complete row", |row| row.detail.as_str()),
                ));
                continue;
            }
            (true, Some(row)) => failures.push(format!(
                "STALE: ledger line {} ({}) expects {} to collect nothing, but it now does; delete or narrow the row",
                row.line,
                row.issue,
                cell.name()
            )),
            (true, None) => {}
        }
        for assertion in &result.assertions {
            let verdict = assertion.verdict.name();
            if assertion.verdict == Verdict::Match {
                continue;
            }
            let family = family(&assertion.name);
            let row = ledger.iter().find(|row| {
                row.names(backend)
                    && matches!(&row.expectation, Expectation::Family(name, verdicts, cells)
                        if *name == family && verdicts.contains(verdict) && pattern_selects(cells, cell))
            });
            match row {
                Some(row) => {
                    used.insert((row.line, backend.to_owned()));
                }
                None => failures.push(format!(
                    "UNEXPECTED: {} {} is {verdict} ({}), and no ledger row expects that",
                    cell.name(),
                    assertion.name,
                    assertion.detail,
                )),
            }
        }
    }
    for row in ledger {
        if matches!(&row.expectation, Expectation::Family(_, verdicts, _) if verdicts.contains("match"))
        {
            continue;
        }
        for backend in &row.backends {
            if complete.contains(backend) && !used.contains(&(row.line, backend.clone())) {
                failures.push(format!(
                    "STALE: ledger line {} ({}) names {backend}, but nothing on {backend} needs it any more; delete {backend} from the row",
                    row.line, row.issue
                ));
            }
        }
    }
    failures
}

fn selection(value: &str) -> Vec<&str> {
    value.split(',').collect()
}

fn validate_selection(
    name: &str,
    selected: &Option<String>,
    allowed: BTreeSet<String>,
) -> Result<(), String> {
    let Some(selected) = selected else {
        return Ok(());
    };
    let mut seen = BTreeSet::new();
    for value in selection(selected) {
        if !allowed.contains(value) {
            return Err(format!(
                "{name} contains an unknown or empty selection {value:?}"
            ));
        }
        if !seen.insert(value) {
            return Err(format!("{name} repeats selection {value:?}"));
        }
    }
    Ok(())
}

fn bytes_hex(value: &[u8]) -> String {
    let mut encoded = String::new();
    for byte in value {
        write!(encoded, "{byte:02x}").expect("String writes succeed");
    }
    encoded
}

fn write_summary(
    prepared: &driver::Prepared,
    required: &[driver::Cell],
    collected: &[driver::Collected],
    accepted_scope: &str,
) -> Result<(), String> {
    use driver::conformance::Verdict;

    let mut summary = format!(
        "# schema=backend-isolation-row-summary-v1\n# accepted_scope={accepted_scope} counts_scope=full_assertion_rows\n# source_inputs=inputs.sha256 installation_inputs=installation.inputs\n# separate tool/refusal entries remain in assertions.tsv and required-cells.tsv, excluded from backend totals\nbackend\texecution_path\tcollection_state\trequired_ids\tattempted_ids\tuncollected_ids\temitted_rows\tmatch\tmismatch\tno_result\n",
    );
    let mut retained = String::from(
        "# schema=backend-isolation-assertion-rows-v1\n# name_hex and detail_hex retain the complete original UTF-8 bytes; multiplicity is preserved\ncell\trow_index\tname_hex\tverdict\tdetail_hex\n",
    );
    for result in collected {
        for (index, row) in result.assertions.iter().enumerate() {
            writeln!(
                retained,
                "{}\t{index}\t{}\t{}\t{}",
                result.cell.name(),
                bytes_hex(row.name.as_bytes()),
                row.verdict.name(),
                bytes_hex(row.detail.as_bytes()),
            )
            .expect("String writes succeed");
        }
    }
    for backend in driver::conformance::BACKENDS {
        let required_ids = required
            .iter()
            .filter(|cell| {
                cell.backend == backend && driver::REQUIRED_PATHS.contains(&cell.path.as_str())
            })
            .count();
        let operands: Vec<_> = collected
            .iter()
            .filter(|result| {
                result.cell.backend == backend
                    && driver::REQUIRED_PATHS.contains(&result.cell.path.as_str())
            })
            .collect();
        let attempted_ids = operands.len();
        let uncollected_ids = required_ids - attempted_ids;
        let execution_path = if backend == "e9patch" {
            "ptrace_with_e9patch_preprocessing"
        } else {
            backend
        };
        if operands.is_empty() {
            writeln!(summary, "{backend}\t{execution_path}\tuncollected\t{required_ids}\t0\t{uncollected_ids}\tNA\tNA\tNA\tNA")
                .expect("String writes succeed");
            println!(
                "backend={backend} required_ids={required_ids} attempted_ids=0 uncollected_ids={uncollected_ids} emitted_rows=unmeasured M=unmeasured X=unmeasured NR=unmeasured"
            );
            continue;
        }
        let mut counts = [0_usize; 3];
        for result in operands {
            for row in &result.assertions {
                counts[match row.verdict {
                    Verdict::Match => 0,
                    Verdict::Mismatch => 1,
                    Verdict::NoResult => 2,
                }] += 1;
            }
        }
        let [matched, mismatched, no_result] = counts;
        let emitted_rows = matched + mismatched + no_result;
        writeln!(summary, "{backend}\t{execution_path}\tattempted\t{required_ids}\t{attempted_ids}\t{uncollected_ids}\t{emitted_rows}\t{matched}\t{mismatched}\t{no_result}")
            .expect("String writes succeed");
        println!(
            "backend={backend} required_ids={required_ids} attempted_ids={attempted_ids} uncollected_ids={uncollected_ids} emitted_rows={emitted_rows} M={matched} X={mismatched} NR={no_result}"
        );
    }
    let ordinary = required
        .iter()
        .filter(|cell| driver::REQUIRED_PATHS.contains(&cell.path.as_str()))
        .count();
    let cohorts: BTreeSet<_> = required
        .iter()
        .filter(|cell| driver::REQUIRED_PATHS.contains(&cell.path.as_str()))
        .map(|cell| {
            format!(
                "{}:{}:{}",
                cell.root,
                cell.path,
                driver::policy_name(cell.policy),
            )
        })
        .collect();
    let backends = driver::conformance::BACKENDS.len();
    let pair_slots = cohorts.len() * backends * (backends - 1) / 2;
    let mut ledger = format!(
        "# schema=backend-isolation-required-ledger-v1\n# required_ids={} ordinary_ids={ordinary} separate_entries={} required_cohorts={} required_pair_slots={pair_slots}\n# cohort_comparison=uncollected pair_comparison=uncollected; no parity or whole-input authority is inferred from row counts\n# uncollected coverage no_result is not an emitted assertion row\ncell\tclass\tcollection_state\tfull_verdict\temitted_rows\treport_path_hex\n",
        required.len(),
        required.len() - ordinary,
        cohorts.len(),
    );
    for cell in required {
        let class = if driver::REQUIRED_PATHS.contains(&cell.path.as_str()) {
            "backend_cell"
        } else {
            "separate_entry"
        };
        if let Some(result) = collected.iter().find(|result| result.cell == *cell) {
            writeln!(
                ledger,
                "{}\t{class}\tattempted\t{}\t{}\t{}",
                cell.name(),
                if result.full_matches() {
                    "match"
                } else {
                    "mismatch_or_no_result"
                },
                result.assertions.len(),
                bytes_hex(
                    result
                        .directory
                        .join("assertions.json")
                        .as_os_str()
                        .as_encoded_bytes(),
                ),
            )
            .expect("String writes succeed");
        } else {
            writeln!(
                ledger,
                "{}\t{class}\tuncollected\tunmeasured\tNA\tNA",
                cell.name()
            )
            .expect("String writes succeed");
        }
    }
    let mut failures = Vec::new();
    for (name, bytes) in [
        ("assertions.tsv", retained),
        ("summary.tsv", summary),
        ("required-cells.tsv", ledger),
    ] {
        if let Err(error) = fs::write(prepared.artifacts.join(name), bytes.as_bytes()) {
            failures.push(format!("{name}: {error}"));
            eprintln!("isolation conformance retained {name} after publication failure:\n{bytes}");
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn run() -> Result<bool, String> {
    let mut repository = std::env::current_dir().map_err(|error| format!("repository: {error}"))?;
    let mut hermit = None;
    let mut safehermit = None;
    let mut artifacts = None;
    let mut installation = None;
    let mut descriptor_obligations = None;
    let mut descriptor_obligations_sha256 = None;
    let mut backends = None;
    let mut roots = None;
    let mut paths = None;
    let mut policies = None;
    let mut scope = "full".to_owned();
    let mut executor = "safehermit".to_owned();
    let mut expected = None;
    let mut list = false;
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if matches!(argument.as_str(), "--help" | "-h") {
            println!("Usage: conformance.rs --hermit PATH --artifacts DIR [OPTIONS]");
            println!("Defaults to every required backend, root, path, and namespace policy.");
            println!(
                "Options: --repository DIR --safehermit PATH --installation DIR --backends LIST --roots LIST --paths LIST --policies LIST --descriptor-obligations FILE --descriptor-obligations-sha256 HEX --assert-scope full|launch-migration --executor safehermit|local --expected LEDGER --list"
            );
            println!(
                "--executor local runs each command as this process's own bounded child, for a runner such as the validation DAG that bounds the whole run itself."
            );
            println!(
                "--expected LEDGER passes when every cell and assertion that does not match is a ledger row, and every row still describes a failure."
            );
            println!(
                "Missing resources and incomplete observations fail requested cells. A selected scope makes no broader parity claim."
            );
            println!(
                "Artifacts include lossless assertion rows, per-backend M/X/NR totals, and the complete required-cell ledger; uncollected IDs are excluded from emitted-row totals."
            );
            return Ok(true);
        }
        if argument == "--list" {
            list = true;
            index += 1;
            continue;
        }
        let (name, value, consumed) = if let Some((name, value)) = argument.split_once('=') {
            (name, value, 1)
        } else {
            (
                argument.as_str(),
                arguments
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument} requires a value"))?
                    .as_str(),
                2,
            )
        };
        match name {
            "--repository" => repository = PathBuf::from(value),
            "--hermit" => hermit = Some(PathBuf::from(value)),
            "--safehermit" => safehermit = Some(PathBuf::from(value)),
            "--artifacts" => artifacts = Some(PathBuf::from(value)),
            "--installation" => installation = Some(PathBuf::from(value)),
            "--descriptor-obligations" => {
                if descriptor_obligations
                    .replace(PathBuf::from(value))
                    .is_some()
                {
                    return Err("--descriptor-obligations is repeated".to_owned());
                }
            }
            "--descriptor-obligations-sha256" => {
                if descriptor_obligations_sha256
                    .replace(value.to_owned())
                    .is_some()
                {
                    return Err("--descriptor-obligations-sha256 is repeated".to_owned());
                }
            }
            "--backends" => backends = Some(value.to_owned()),
            "--roots" => roots = Some(value.to_owned()),
            "--paths" => paths = Some(value.to_owned()),
            "--policies" => policies = Some(value.to_owned()),
            "--assert-scope" => scope = value.to_owned(),
            "--executor" => executor = value.to_owned(),
            "--expected" => expected = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option {name}")),
        }
        index += consumed;
    }
    if !matches!(scope.as_str(), "full" | "launch-migration") {
        return Err("--assert-scope must be full or launch-migration".to_owned());
    }
    if !matches!(executor.as_str(), "safehermit" | "local") {
        return Err("--executor must be safehermit or local".to_owned());
    }
    if expected.is_some() && scope != "full" {
        return Err("--expected holds the full scope".to_owned());
    }
    let ledger = expected.as_deref().map(read_ledger).transpose()?;
    let required = driver::required_cells();
    validate_selection(
        "--backends",
        &backends,
        required.iter().map(|cell| cell.backend.clone()).collect(),
    )?;
    validate_selection(
        "--roots",
        &roots,
        required.iter().map(|cell| cell.root.clone()).collect(),
    )?;
    validate_selection(
        "--paths",
        &paths,
        required.iter().map(|cell| cell.path.clone()).collect(),
    )?;
    validate_selection(
        "--policies",
        &policies,
        required
            .iter()
            .map(|cell| driver::policy_name(cell.policy).to_owned())
            .collect(),
    )?;
    let cells: Vec<_> = required
        .iter()
        .filter(|cell| {
            backends
                .as_ref()
                .is_none_or(|value| selection(value).contains(&cell.backend.as_str()))
                && roots
                    .as_ref()
                    .is_none_or(|value| selection(value).contains(&cell.root.as_str()))
                && paths
                    .as_ref()
                    .is_none_or(|value| selection(value).contains(&cell.path.as_str()))
                && policies.as_ref().is_none_or(|value| {
                    selection(value).contains(&driver::policy_name(cell.policy))
                })
        })
        .cloned()
        .collect();
    if cells.is_empty() {
        return Err("selection names no required cells".to_owned());
    }
    if scope == "launch-migration"
        && (backends.is_none()
            || roots.is_none()
            || paths.is_none()
            || policies.is_none()
            || cells.iter().any(|cell| {
                cell.root != "host"
                    || cell.path != "run-raw"
                    || cell.policy != driver::conformance::NamespacePolicy::Private
            }))
    {
        return Err(
            "launch-migration scope requires an explicit host/run-raw/private selection".to_owned(),
        );
    }
    let descriptor_registry = match (descriptor_obligations, descriptor_obligations_sha256) {
        (None, None) => None,
        (Some(path), Some(expected)) => Some(driver::load_descriptor_registry(&path, &expected, &cells)?),
        _ => return Err("--descriptor-obligations and --descriptor-obligations-sha256 must be supplied together".to_owned()),
    };
    if list {
        for cell in cells {
            println!("{}", cell.name());
        }
        return Ok(true);
    }
    let hermit = hermit.ok_or("--hermit is required")?;
    let artifacts = artifacts
        .ok_or("--artifacts is required and must name a fresh ignored/ or target/ directory")?;
    close_inherited_descriptors()?;
    let prepared = if executor == "local" {
        driver::Prepared::build_integration(
            &repository,
            &hermit,
            &artifacts,
            local_capture,
            Path::new("tests/isolation/conformance.rs"),
            installation.as_deref(),
            descriptor_registry,
        )?
    } else {
        let safehermit = safehermit.map_or_else(|| driver::find_safehermit(&repository), Ok)?;
        driver::Prepared::build_with_installation_and_registry(
            &repository,
            &hermit,
            &safehermit,
            &artifacts,
            installation.as_deref(),
            descriptor_registry,
        )?
    };
    let mut matches = true;
    let mut collected = Vec::new();
    // A backend whose every required cell runs here: only for it can a
    // ledger row be found to describe nothing any more.
    let complete: BTreeSet<String> = driver::conformance::BACKENDS
        .iter()
        .filter(|backend| {
            let required = required.iter().filter(|cell| cell.backend == **backend);
            let mut selected = cells.iter().filter(|cell| cell.backend == **backend);
            required.count() == selected.clone().count() && selected.next().is_some()
        })
        .map(|backend| backend.to_string())
        .collect();
    for cell in cells {
        let result = prepared.collect(cell);
        let accepted = if scope == "launch-migration" {
            result.launch_migration_matches()
        } else {
            result.full_matches()
        };
        let failures = result
            .assertions
            .iter()
            .filter(|row| row.verdict != driver::conformance::Verdict::Match)
            .count();
        println!(
            "{} scope={} verdict={} full_required_failures={} report={}",
            result.cell.name(),
            scope,
            if accepted {
                "match"
            } else {
                "mismatch_or_no_result"
            },
            failures,
            result.directory.join("assertions.json").display()
        );
        matches &= accepted;
        collected.push(result);
    }
    if let Some(ledger) = &ledger {
        let failures = check_ledger(ledger, &collected, &complete);
        let mut families: BTreeMap<&str, usize> = BTreeMap::new();
        for row in ledger {
            for backend in &row.backends {
                *families.entry(backend.as_str()).or_default() += 1;
            }
        }
        for failure in &failures {
            println!("{failure}");
        }
        println!(
            "expected-mismatch ledger: {} cells checked against {} rows ({families:?} per backend); stale rows judged for {complete:?}; disagreements={}",
            collected.len(),
            ledger.len(),
            failures.len(),
        );
        matches = failures.is_empty();
    }
    write_summary(&prepared, &required, &collected, &scope).map_err(|error| {
        format!(
            "summary publication: {error}; checker verdict={}; emitted assertion verdicts are unchanged",
            if matches {
                "match"
            } else {
                "mismatch_or_no_result"
            },
        )
    })?;
    Ok(matches)
}

fn main() {
    rust_script_prelude::init();
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("isolation conformance: {error}");
            std::process::exit(2);
        }
    }
}
