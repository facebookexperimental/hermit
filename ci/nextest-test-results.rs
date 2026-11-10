#!/usr/bin/env -S rust-script --force
//! Convert nextest's versioned libtest JSON into dagrun's shared test-result file.
//!
//! ```cargo
//! [dependencies]
//! dagrun = { version = "0.15.0", git = "https://github.com/rrnewton/agent-utils.git", rev = "cc487cc3c65015ba798f295fd3d02ed3d71281a8" }
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! sha2 = "0.10"
//! ```

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use dagrun::TestAttemptOutcome;
use dagrun::TestAttemptResult;
use dagrun::TestResult;
use dagrun::TestResults;
use dagrun::TestResultsWriteError;
use dagrun::structured_test_results_recovery_path;
use nextest_cpu::AttemptCompletion;
use nextest_cpu::AttemptIdentity;
use nextest_cpu::AttemptRecord;
use nextest_cpu::BinaryMap;
use nextest_cpu::CpuReport;
use nextest_cpu::{listed_count, listed_identities};
use nextest_cpu::read_attempt_records;
use nextest_cpu::read_binary_map;
use nextest_cpu::write_binary_map_atomic;
use nextest_cpu::write_report_atomic;
use serde::Deserialize;
use serde_json::Value;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "manifest-plan/src/nextest_cpu.rs"]
// This shared module also contains the writer used by the compiled wrapper.
// The event adapter intentionally uses only its reader and report writer.
#[allow(dead_code)]
mod nextest_cpu;

fn usage() {
    println!(
        "usage: nextest-test-results.rs [--inventory NEXTEST_INVENTORY_JSON] EVENTS_JSONL NEXTEST_STATUS OUTPUT_OR_DASH [ATTEMPT_RECORD_DIR BINARY_MAP CPU_REPORT_OR_DASH]\n       nextest-test-results.rs --write-binary-map NEXTEST_INVENTORY_JSON OUTPUT\n       nextest-test-results.rs --listed-count NEXTEST_INVENTORY_JSON\n       nextest-test-results.rs --listed-ids NEXTEST_INVENTORY_JSON OUTPUT\n\n--inventory uses the same invocation's list JSON for typed event attribution, never for outcomes or CPU measurements. It is required with BINARY_MAP. Without a binary map it is optional, and overlapping same-name suites refuse."
    );
}

fn required_string<'a>(value: &'a Value, field: &str, line: usize) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("nextest-test-results line {line}: {field} must be a string"))
}

fn required_u64(value: &Value, field: &str, line: usize) -> Result<u64, String> {
    value.get(field).and_then(Value::as_u64).ok_or_else(|| {
        format!("nextest-test-results line {line}: {field} must be an unsigned integer")
    })
}

fn test_identity(name: &str, line: usize) -> Result<((String, String), String, u64), String> {
    let (name, attempts) = match name.rsplit_once('#') {
        Some((prefix, suffix)) if suffix.chars().all(|character| character.is_ascii_digit()) => {
            let attempts = suffix.parse::<u64>().map_err(|error| {
                format!("nextest-test-results line {line}: invalid retry count: {error}")
            })?;
            (prefix, attempts)
        }
        _ => (name, 1),
    };
    if attempts == 0 {
        return Err(format!(
            "nextest-test-results line {line}: retry count must be positive"
        ));
    }
    let (package, binary_and_test) = name.split_once("::").ok_or_else(|| {
        format!("nextest-test-results line {line}: name has no package separator")
    })?;
    let (binary, test) = binary_and_test
        .split_once('$')
        .ok_or_else(|| format!("nextest-test-results line {line}: name has no test separator"))?;
    if package.is_empty() || binary.is_empty() || test.is_empty() {
        return Err(format!(
            "nextest-test-results line {line}: name has an empty package, binary, or test"
        ));
    }
    Ok((
        (package.to_string(), binary.to_string()),
        test.to_string(),
        attempts,
    ))
}

fn displayed_test_id(
    package: &str,
    binary: &str,
    test: &str,
    suite_kind: &str,
    stress_index: Option<u64>,
) -> String {
    let stress = stress_index.map_or_else(String::new, |index| format!("@stress-{index}"));
    let display_binary = if suite_kind == "lib" {
        format!("{package}{stress}")
    } else {
        format!("{package}::{binary}{stress}")
    };
    format!("{display_binary}${test}")
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SuiteIdentity {
    package: String,
    binary: String,
    kind: String,
    stress_index: Option<u64>,
}

impl SuiteIdentity {
    fn from_event(value: &Value, line: usize) -> Result<Self, String> {
        let nextest = value
            .get("nextest")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                format!("nextest-test-results line {line}: nextest must be an object")
            })?;
        let package = nextest
            .get("crate")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("nextest-test-results line {line}: nextest.crate must be a string")
            })?;
        let binary = nextest
            .get("test_binary")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("nextest-test-results line {line}: nextest.test_binary must be a string")
            })?;
        let kind = nextest.get("kind").and_then(Value::as_str).ok_or_else(|| {
            format!("nextest-test-results line {line}: nextest.kind must be a string")
        })?;
        if package.is_empty() || binary.is_empty() || kind.is_empty() {
            return Err(format!(
                "nextest-test-results line {line}: suite identity fields must be nonempty"
            ));
        }
        let stress_index = match nextest.get("stress_index") {
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                format!(
                    "nextest-test-results line {line}: nextest.stress_index must be an unsigned integer"
                )
            })?),
            None => None,
        };
        Ok(Self {
            package: package.into(),
            binary: binary.into(),
            kind: kind.into(),
            stress_index,
        })
    }

    fn test_name_key(&self) -> (String, String) {
        let binary = match self.stress_index {
            Some(index) => format!("{}@stress-{index}", self.binary),
            None => self.binary.clone(),
        };
        (self.package.clone(), binary)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ParsedEvents {
    executed_tests: u64,
    filtered_tests: u64,
    results: Vec<TestResult>,
    expected_attempts: Vec<ExpectedTest>,
}

#[derive(Debug, Eq, PartialEq)]
struct ExpectedTest {
    suite: SuiteIdentity,
    test: String,
    attempts: u64,
    passed: bool,
}

#[derive(Clone, Debug)]
struct OpenSuite {
    identity: SuiteIdentity,
    test_count: u64,
    passed: u64,
    failed: u64,
}

#[derive(Debug, Deserialize)]
struct InventoryCases {
    #[serde(rename = "rust-suites")]
    suites: BTreeMap<String, InventorySuite>,
}

#[derive(Debug, Deserialize)]
struct InventorySuite {
    testcases: BTreeMap<String, InventoryCase>,
    #[serde(skip)]
    start_count: u64,
    #[serde(skip)]
    ignored_count: u64,
}

#[derive(Debug, Deserialize)]
struct InventoryCase {
    ignored: bool,
    #[serde(rename = "filter-match")]
    filter_match: InventoryFilter,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
enum InventoryFilter {
    Matches {},
    Mismatch { reason: InventoryMismatch },
}

// Nextest metadata's known mismatch reasons. An unknown classification must
// refuse rather than silently remove a possible event name from an overlap.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum InventoryMismatch {
    Ignored,
    String,
    Expression,
    Partition,
    DefaultFilter,
    NotBenchmark,
    RerunAlreadyPassed,
}

impl InventoryCase {
    fn eligible(&self, event: &str) -> bool {
        let executes = matches!(self.filter_match, InventoryFilter::Matches {});
        let skips = matches!(
            self.filter_match,
            InventoryFilter::Mismatch {
                reason: InventoryMismatch::Ignored
            }
        );
        match event {
            "started" => executes || skips,
            "ignored" => skips,
            "ok" | "failed" => executes,
            _ => false,
        }
    }
}

#[derive(Debug)]
struct EventInventory {
    binary_map: BinaryMap,
    suites: BTreeMap<String, InventorySuite>,
}

impl EventInventory {
    fn from_bytes(bytes: &[u8], cpu_map: Option<&BinaryMap>) -> Result<Self, String> {
        let binary_map = BinaryMap::from_nextest_inventory(bytes)?;
        if let Some(cpu_map) = cpu_map {
            cpu_map.validate()?;
            if &binary_map != cpu_map {
                return Err("nextest inventory disagrees with the CPU binary map; supply both from the same invocation".into());
            }
        }
        let mut inventory: InventoryCases = serde_json::from_slice(bytes)
            .map_err(|error| format!("malformed nextest event inventory: {error}"))?;
        for suite in inventory.suites.values_mut() {
            for (name, case) in &suite.testcases {
                if name.is_empty() {
                    return Err("nextest inventory testcase name must be nonempty".into());
                }
                // The producer counts all ignored cases, even filtered ones,
                // plus matching nonignored cases. Zero does not exclude
                // started events for nonignored cases under --ignored.
                if case.ignored || matches!(case.filter_match, InventoryFilter::Matches {}) {
                    suite.start_count = suite
                        .start_count
                        .checked_add(1)
                        .ok_or("nextest inventory start count overflows u64")?;
                }
                if case.ignored {
                    suite.ignored_count = suite
                        .ignored_count
                        .checked_add(1)
                        .ok_or("nextest inventory ignored count overflows u64")?;
                }
            }
        }
        Ok(Self {
            binary_map,
            suites: inventory.suites,
        })
    }

    fn suite(&self, identity: &SuiteIdentity) -> Result<&InventorySuite, String> {
        let (_, binary) = self.binary_map.identity_for_suite(
            &identity.package,
            &identity.binary,
            &identity.kind,
        )?;
        self.suites.get(binary).ok_or_else(|| {
            format!("typed nextest suite {identity:?} is absent from the event inventory")
        })
    }
}

fn suite_population(
    test_count: u64,
    passed: u64,
    failed: u64,
    ignored: u64,
    filtered_out: u64,
    line: usize,
) -> Result<u64, String> {
    let executed = passed.checked_add(failed).ok_or_else(|| {
        format!("nextest-test-results line {line}: passed + failed overflows u64")
    })?;
    // nextest 0.9.100's libtest-json adapter subtracts selected ignored tests
    // from an unsigned running count. Its two observed sentinel shapes are
    // `0 - ignored` when none ran and `0 - executed` when a selected subset
    // ran. In both cases the suite-start `test_count` is the exact population.
    // Admit only those exact producer shapes; another overflowing value is
    // malformed rather than a count to wrap into a plausible small number.
    if filtered_out != 0
        && (filtered_out == 0u64.wrapping_sub(ignored)
            || filtered_out == 0u64.wrapping_sub(executed))
    {
        if executed > ignored || executed > test_count {
            return Err(format!(
                "nextest-test-results line {line}: wrapping filtered_out with executed count {executed}, ignored count {ignored}, and suite population {test_count} is inconsistent"
            ));
        }
        return Ok(test_count);
    }
    // A third shape from the same adapter, captured from nextest 0.9.100 in
    // test.pmu_integration_cases: a suite whose one ordinary test the filter
    // left out while its two `#[ignore]` tests were selected (hermit's
    // leveldb) reports `test_count` 2, `ignored` 2, nothing executed, and
    // `filtered_out` 0 - 1. Nothing ran and every counted test was ignored,
    // so `test_count` is again the typed population; admit only a wrapped
    // deficit smaller than that ignored count.
    let deficit = 0u64.wrapping_sub(filtered_out);
    if executed == 0 && ignored == test_count && deficit != 0 && deficit < ignored {
        return Ok(test_count);
    }
    let population = executed
        .checked_add(ignored)
        .and_then(|count| count.checked_add(filtered_out))
        .ok_or_else(|| {
            format!(
                "nextest-test-results line {line}: passed + failed + ignored + filtered_out overflows u64"
            )
        })?;
    // A skipped test can make nextest finalize one partial recap before the
    // selected tests run. The start count and terminal arithmetic are both
    // typed producer facts; the larger is the complete suite population.
    Ok(population.max(test_count))
}

fn parse_event_text(text: &str) -> Result<ParsedEvents, String> {
    parse_event_text_with_inventory(text, None)
}

fn parse_event_text_with_inventory(
    text: &str,
    inventory: Option<&EventInventory>,
) -> Result<ParsedEvents, String> {
    let mut results = BTreeMap::new();
    let mut expected_attempts = Vec::new();
    // Test rows carry package/binary but not target kind. Keep every currently
    // open suite by its full identity. Same rendered keys may overlap only
    // when same-invocation inventory proves disjoint potential event names.
    let mut open_suites = BTreeMap::<SuiteIdentity, OpenSuite>::new();
    let mut population_by_suite = BTreeMap::<SuiteIdentity, u64>::new();
    let mut executed_by_suite = BTreeMap::<SuiteIdentity, u64>::new();
    let mut suite_passed = 0u64;
    let mut suite_failed = 0u64;
    let mut saw_event = false;
    for (offset, line) in text.lines().enumerate() {
        let line_number = offset + 1;
        if line.trim().is_empty() {
            continue;
        }
        saw_event = true;
        let value: Value = serde_json::from_str(line).map_err(|error| {
            format!("nextest-test-results line {line_number}: invalid JSON: {error}")
        })?;
        let kind = required_string(&value, "type", line_number)?;
        let event = required_string(&value, "event", line_number)?;
        match (kind, event) {
            ("suite", "started") => {
                let suite = SuiteIdentity::from_event(&value, line_number)?;
                let key = suite.test_name_key();
                let open = OpenSuite {
                    identity: suite.clone(),
                    test_count: required_u64(&value, "test_count", line_number)?,
                    passed: 0,
                    failed: 0,
                };
                if open_suites.contains_key(&suite) {
                    return Err(format!(
                        "nextest-test-results line {line_number}: duplicate active suite identity {suite:?}"
                    ));
                }
                if let Some(inventory) = inventory {
                    let expected = inventory.suite(&suite)?.start_count;
                    if open.test_count != expected {
                        return Err(format!(
                            "nextest-test-results line {line_number}: suite {suite:?} start count {} disagrees with inventory count {expected}",
                            open.test_count
                        ));
                    }
                }
                for active in open_suites
                    .keys()
                    .filter(|active| active.test_name_key() == key)
                {
                    let disjoint = match inventory {
                        Some(inventory) => {
                            let left = inventory.suite(&suite)?;
                            let right = inventory.suite(active)?;
                            !left.testcases.iter().any(|(name, case)| {
                                case.eligible("started")
                                    && right
                                        .testcases
                                        .get(name)
                                        .is_some_and(|case| case.eligible("started"))
                            })
                        }
                        None => false,
                    };
                    if !disjoint {
                        return Err(format!(
                            "nextest-test-results line {line_number}: suite key {key:?} is ambiguous because {suite:?} started before {active:?} ended; supply same-invocation inventory with disjoint event-eligible names"
                        ));
                    }
                }
                open_suites.insert(suite, open);
            }
            ("suite", "ok" | "failed") => {
                let ended = SuiteIdentity::from_event(&value, line_number)?;
                let key = ended.test_name_key();
                let started = open_suites.remove(&ended).ok_or_else(|| {
                    if let Some(active) = open_suites.keys().find(|active| {
                        active.test_name_key() == key && active.kind != ended.kind
                    }) {
                        format!(
                            "nextest-test-results line {line_number}: suite ({:?}, {:?}) changed kind from {:?} to {:?}",
                            active.package, active.binary, active.kind, ended.kind
                        )
                    } else {
                        format!(
                            "nextest-test-results line {line_number}: suite {ended:?} ended before its typed start metadata"
                        )
                    }
                })?;
                let passed = required_u64(&value, "passed", line_number)?;
                let failed = required_u64(&value, "failed", line_number)?;
                if passed != started.passed || failed != started.failed {
                    let executed = passed
                        .checked_add(failed)
                        .ok_or("nextest-test-results: passed + failed overflows u64")?;
                    let observed = started
                        .passed
                        .checked_add(started.failed)
                        .ok_or("nextest-test-results: episode result count overflows u64")?;
                    return Err(format!(
                        "nextest-test-results line {line_number}: episode {ended:?}: typed suite records report {executed} executed test(s), but typed terminal test records report {observed}; passed/failed ({passed}, {failed}) disagree with ({}, {})",
                        started.passed, started.failed
                    ));
                }
                suite_passed = suite_passed.checked_add(passed).ok_or_else(|| {
                    "nextest-test-results: passed count overflows u64".to_string()
                })?;
                suite_failed = suite_failed.checked_add(failed).ok_or_else(|| {
                    "nextest-test-results: failed count overflows u64".to_string()
                })?;
                let ignored = required_u64(&value, "ignored", line_number)?;
                if let Some(inventory) = inventory {
                    let expected = inventory.suite(&ended)?.ignored_count;
                    if ignored != expected {
                        return Err(format!(
                            "nextest-test-results line {line_number}: suite {ended:?} ignored count {ignored} disagrees with inventory count {expected}"
                        ));
                    }
                }
                let population = suite_population(
                    started.test_count,
                    passed,
                    failed,
                    ignored,
                    required_u64(&value, "filtered_out", line_number)?,
                    line_number,
                )?;
                // nextest may launch one exact-filtered process per test,
                // repeating the same suite in multiple terminal records. Keep
                // the greatest complete population, then subtract the unique
                // terminal test records after the whole stream is parsed.
                // Never conflate a lib, bin, or stress suite that renders the
                // same test-name key.
                population_by_suite
                    .entry(ended)
                    .and_modify(|current| *current = (*current).max(population))
                    .or_insert(population);
            }
            ("test", "started" | "ignored" | "ok" | "failed") => {
                let name = required_string(&value, "name", line_number)?;
                let (key, test, attempts) = test_identity(name, line_number)?;
                let mut candidates = Vec::new();
                for identity in open_suites
                    .keys()
                    .filter(|suite| suite.test_name_key() == key)
                {
                    let eligible = match inventory {
                        Some(inventory) => inventory
                            .suite(identity)?
                            .testcases
                            .get(&test)
                            .is_some_and(|case| case.eligible(event)),
                        None => true,
                    };
                    if eligible {
                        candidates.push(identity.clone());
                    }
                }
                if candidates.len() != 1 {
                    return Err(format!(
                        "nextest-test-results line {line_number}: test {test:?} event {event:?} names suite {key:?} without one unambiguous open typed suite ({} eligible matches)",
                        candidates.len()
                    ));
                }
                let suite = open_suites.get_mut(&candidates[0]).ok_or_else(|| {
                    format!(
                        "nextest-test-results line {line_number}: test names suite {key:?} without one unambiguous open typed suite"
                    )
                })?;
                if matches!(event, "started" | "ignored") {
                    continue;
                }
                let episode_count = if event == "ok" {
                    &mut suite.passed
                } else {
                    &mut suite.failed
                };
                *episode_count = episode_count
                    .checked_add(1)
                    .ok_or("nextest-test-results: episode result count overflows u64")?;
                let id = displayed_test_id(
                    &suite.identity.package,
                    &suite.identity.binary,
                    &test,
                    &suite.identity.kind,
                    suite.identity.stress_index,
                );
                // Keep one terminal expectation. A retry suffix is a count,
                // not permission to allocate that many records while parsing.
                expected_attempts.push(ExpectedTest {
                    suite: suite.identity.clone(),
                    test: test.clone(),
                    attempts,
                    passed: event == "ok",
                });
                let result = TestResult::new(id.clone(), event == "ok", attempts)?;
                if results.insert(id.clone(), result).is_some() {
                    return Err(format!(
                        "nextest-test-results line {line_number}: duplicate terminal test id {id:?}"
                    ));
                }
                let count = executed_by_suite.entry(suite.identity.clone()).or_insert(0);
                *count = count.checked_add(1).ok_or_else(|| {
                    "nextest-test-results: suite terminal result count overflows u64".to_string()
                })?;
            }
            _ => {
                return Err(format!(
                    "nextest-test-results line {line_number}: unsupported {kind} event {event:?}"
                ));
            }
        }
    }
    if !open_suites.is_empty() {
        return Err(format!(
            "nextest-test-results ended before open suites published terminal events: {:?}",
            open_suites
                .into_values()
                .map(|suite| suite.identity)
                .collect::<Vec<_>>()
        ));
    }
    if !saw_event {
        return Err("nextest-test-results: event stream is empty".into());
    }
    let results = results.into_values().collect::<Vec<_>>();
    let executed_tests = u64::try_from(results.len())
        .map_err(|_| "nextest-test-results: terminal result count does not fit u64".to_string())?;
    let suite_executed = suite_passed
        .checked_add(suite_failed)
        .ok_or_else(|| "nextest-test-results: suite executed count overflows u64".to_string())?;
    if suite_executed != executed_tests {
        return Err(format!(
            "nextest-test-results: typed suite records report {suite_executed} executed test(s), but typed terminal test records report {executed_tests}"
        ));
    }
    let filtered_tests = population_by_suite.iter().try_fold(0u64, |total, (suite, population)| {
        let executed = executed_by_suite.get(suite).copied().unwrap_or(0);
        let skipped = population.checked_sub(executed).ok_or_else(|| {
            format!(
                "nextest-test-results: suite {suite:?} has {executed} terminal result(s), exceeding its typed population {population}"
            )
        })?;
        total
            .checked_add(skipped)
            .ok_or_else(|| "nextest-test-results: filtered count overflows u64".to_string())
    })?;
    Ok(ParsedEvents {
        executed_tests,
        filtered_tests,
        results,
        expected_attempts,
    })
}

/// Reconcile the typed attempts with the CPU wrapper's records.
/// Also returns the identities of attempts a fail-fast cancel killed in
/// flight (signal completions with no typed attempt); the caller records
/// those as cancelled, never as passed (dh-825jy).
fn reconcile_cpu_attempts_with_cancelled(
    parsed: &ParsedEvents,
    directory: &Path,
    binary_map: &BinaryMap,
) -> Result<(CpuReport, Vec<AttemptIdentity>), String> {
    for attempt in &parsed.expected_attempts {
        // The declared Nextest version has no stress-run wrapper identity.
        // Historical count-only parsing still preserves stress_index; never
        // alias such events onto an ordinary measured attempt.
        if attempt.suite.stress_index.is_some() {
            return Err("CPU reconciliation does not support stress_index identities".into());
        }
    }
    let records = read_attempt_records(directory)?;
    let required = parsed
        .expected_attempts
        .iter()
        .try_fold(0u64, |total, test| total.checked_add(test.attempts))
        .ok_or("missing=typed retry population overflows the available attempt count")?;
    if required > records.len() as u64 {
        return Err(format!(
            "missing=typed events require {required} attempt records, supplied {}",
            records.len()
        ));
    }
    // Any expansion is now bounded by records that were actually supplied.
    // Exact identity and success comparisons below remain mandatory.
    let mut expected = BTreeMap::<AttemptIdentity, bool>::new();
    for attempt in &parsed.expected_attempts {
        let (package, binary) = binary_map.identity_for_suite(
            &attempt.suite.package,
            &attempt.suite.binary,
            &attempt.suite.kind,
        )?;
        for number in 1..=attempt.attempts {
            let identity = AttemptIdentity {
                package: package.into(),
                binary: binary.into(),
                test: attempt.test.clone(),
                attempt: number,
            };
            if expected
                .insert(
                    identity.clone(),
                    attempt.passed && number == attempt.attempts,
                )
                .is_some()
            {
                return Err(format!(
                    "typed nextest events contain duplicate CPU attempt identity {identity:?}"
                ));
            }
        }
    }
    let mut actual = BTreeMap::<AttemptIdentity, AttemptRecord>::new();
    let mut run_id = None::<String>;
    for record in records {
        match &run_id {
            Some(expected) if expected != &record.run_id => {
                return Err(format!(
                    "nextest CPU attempt records mix run ids {expected:?} and {:?}",
                    record.run_id
                ));
            }
            None => run_id = Some(record.run_id.clone()),
            _ => {}
        }
        if actual.insert(record.identity.clone(), record).is_some() {
            return Err("duplicate CPU attempt record for one binary/test/attempt identity".into());
        }
    }

    let missing = expected
        .keys()
        .filter(|identity| !actual.contains_key(*identity))
        .cloned()
        .collect::<Vec<_>>();
    let unexpected = actual
        .keys()
        .filter(|identity| !expected.contains_key(*identity))
        .cloned()
        .collect::<Vec<_>>();
    // A fail-fast cancel kills attempts in flight: the CPU wrapper holds a
    // record for each, killed by a signal, but nextest never reported a
    // typed attempt for them. Those are cancelled attempts, kept in the
    // CPU report as such - not a disagreement, and never a pass. An
    // unexpected record that EXITED (any code) or hit its CPU budget is a
    // real disagreement: a test that ran to a verdict nextest did not
    // report. Missing records likewise still refuse (dh-825jy).
    let (cancelled, unexpected): (Vec<AttemptIdentity>, Vec<AttemptIdentity>) =
        unexpected.into_iter().partition(|identity| {
            matches!(
                actual[identity].completion,
                AttemptCompletion::Signal { .. }
                    | AttemptCompletion::SupervisorSignal { .. }
            )
        });
    if !missing.is_empty() || !unexpected.is_empty() {
        return Err(format!(
            "nextest CPU attempt identities disagree with typed events; missing={missing:?}; unexpected={unexpected:?}"
        ));
    }
    for (identity, passed) in &expected {
        let recorded_success = actual
            .get(identity)
            .expect("identity sets were checked above")
            .completion
            .is_success();
        if *passed != recorded_success {
            return Err(format!(
                "typed nextest event and attempt record disagree on whether {identity:?} exited successfully"
            ));
        }
    }
    Ok((
        CpuReport::new(run_id, actual.into_values().collect()),
        cancelled,
    ))
}

fn parse_events(path: &Path, inventory: Option<&EventInventory>) -> Result<ParsedEvents, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("nextest-test-results-read {}: {error}", path.display()))?;
    match inventory {
        Some(inventory) => parse_event_text_with_inventory(&text, Some(inventory)),
        None => parse_event_text(&text),
    }
}

fn parse_u64(value: String, name: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|error| format!("nextest-test-results {name}: {error}"))
}

/// Only the atomic structured-result publication boundary may use the existing
/// no-result status. Parse/schema/count/CPU errors remain ordinary refusals.
#[derive(Debug)]
enum ProducerError {
    Invalid(String),
    Publication(TestResultsWriteError),
    PublicationRecovery {
        primary: TestResultsWriteError,
        recovery: TestResultsWriteError,
    },
}

impl ProducerError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Invalid(_) | Self::Publication(TestResultsWriteError::Invalid(_)) => 2,
            Self::Publication(
                TestResultsWriteError::Write { .. } | TestResultsWriteError::Publish { .. },
            ) => 75,
            Self::PublicationRecovery { .. } => 75,
        }
    }
}

impl From<String> for ProducerError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl From<&str> for ProducerError {
    fn from(message: &str) -> Self {
        Self::Invalid(message.into())
    }
}

impl std::fmt::Display for ProducerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Publication(error) => std::fmt::Display::fmt(error, formatter),
            Self::PublicationRecovery { primary, recovery } => write!(
                formatter,
                "{primary}; structured-test-results-recovery also failed: {recovery}"
            ),
        }
    }
}

/// One runtime-skip marker a test helper appended to the run's sidecar file.
///
/// The line format is `HERMIT-TEST-SKIP: <test-name>\t<reason>`: the bare test
/// name the helper's caller passed (the parser matches it against the terminal
/// row's test path), a tab, and the nonempty reason on the same line.
#[derive(Debug, Eq, PartialEq)]
struct SkipMarker {
    name: String,
    reason: String,
}

const SKIP_MARKER_PREFIX: &str = "HERMIT-TEST-SKIP: ";
const SKID_INFRA_MARKER_PREFIX: &str = "HERMIT-SKID-INFRA: ";
/// One append must stay under PIPE_BUF so parallel test processes cannot
/// interleave entries in the shared sidecar.
const SKIP_MARKER_MAX_BYTES: usize = 4096;

fn parse_marker_sidecar(bytes: &[u8], prefix: &str, kind: &str) -> Result<Vec<SkipMarker>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.ends_with(b"\n") {
        return Err(format!(
            "nextest-test-results: {kind} sidecar ends in a truncated line; refusing to guess"
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|error| {
        format!("nextest-test-results: {kind} sidecar is not UTF-8: {error}")
    })?;
    let mut markers: Vec<SkipMarker> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.len() + 1 > SKIP_MARKER_MAX_BYTES {
            return Err(format!(
                "nextest-test-results: {kind} sidecar line {line_number} exceeds {SKIP_MARKER_MAX_BYTES} bytes"
            ));
        }
        let entry = line.strip_prefix(prefix).ok_or_else(|| {
            format!(
                "nextest-test-results: {kind} sidecar line {line_number} lacks the {prefix:?} prefix"
            )
        })?;
        let (name, reason) = entry.split_once('\t').ok_or_else(|| {
            format!(
                "nextest-test-results: {kind} sidecar line {line_number} has no tab before its reason"
            )
        })?;
        if name.is_empty() || name.trim() != name {
            return Err(format!(
                "nextest-test-results: {kind} sidecar line {line_number} has an empty or untrimmed test name"
            ));
        }
        if reason.is_empty() || reason.trim() != reason {
            return Err(format!(
                "nextest-test-results: {kind} sidecar line {line_number} has an empty or untrimmed reason"
            ));
        }
        if markers
            .iter()
            .any(|marker: &SkipMarker| marker.name == name)
        {
            return Err(format!(
                "nextest-test-results: {kind} sidecar names test {name:?} twice; refusing to guess"
            ));
        }
        markers.push(SkipMarker {
            name: name.to_string(),
            reason: reason.to_string(),
        });
    }
    Ok(markers)
}

fn parse_skip_sidecar(bytes: &[u8]) -> Result<Vec<SkipMarker>, String> {
    parse_marker_sidecar(bytes, SKIP_MARKER_PREFIX, "skip")
}

fn parse_skid_infra_sidecar(bytes: &[u8]) -> Result<Vec<SkipMarker>, String> {
    parse_marker_sidecar(bytes, SKID_INFRA_MARKER_PREFIX, "skid-infra")
}

/// The skip markers for this run, when the runner wired the sidecar channel.
///
/// `HERMIT_TEST_SKIP_FILE` is set by `run-nextest-counted.sh` only for a node
/// that opted into schema 5 (`HERMIT_NEXTEST_RESULTS_SCHEMA=5`). An unset
/// variable means the channel is off and there is nothing to read.
fn read_skip_markers() -> Result<Vec<SkipMarker>, ProducerError> {
    let path = match env::var("HERMIT_TEST_SKIP_FILE") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(Vec::new()),
        Err(env::VarError::NotUnicode(_)) => {
            return Err("nextest-test-results HERMIT_TEST_SKIP_FILE is not UTF-8".into());
        }
    };
    if path.is_empty() {
        return Err("nextest-test-results HERMIT_TEST_SKIP_FILE must not be empty".into());
    }
    let bytes = fs::read(&path).map_err(|error| {
        format!("nextest-test-results: cannot read skip sidecar {path}: {error}")
    })?;
    Ok(parse_skip_sidecar(&bytes)?)
}

/// The skid-infra markers for this run, when the runner wired the sidecar
/// channel (`HERMIT_SKID_INFRA_FILE`, set by `run-nextest-counted.sh`
/// beside the skip sidecar for a schema-5 node). An unset variable means
/// the channel is off and there is nothing to read.
fn read_skid_infra_markers() -> Result<Vec<SkipMarker>, ProducerError> {
    let path = match env::var("HERMIT_SKID_INFRA_FILE") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(Vec::new()),
        Err(env::VarError::NotUnicode(_)) => {
            return Err("nextest-test-results HERMIT_SKID_INFRA_FILE is not UTF-8".into());
        }
    };
    if path.is_empty() {
        return Err("nextest-test-results HERMIT_SKID_INFRA_FILE must not be empty".into());
    }
    let bytes = fs::read(&path).map_err(|error| {
        format!("nextest-test-results: cannot read skid-infra sidecar {path}: {error}")
    })?;
    Ok(parse_skid_infra_sidecar(&bytes)?)
}

/// Whether this node opted into schema-5 structured results.
fn schema_5_opted_in() -> Result<bool, ProducerError> {
    match env::var("HERMIT_NEXTEST_RESULTS_SCHEMA") {
        Ok(value) if value == "5" => Ok(true),
        Ok(value) => Err(format!(
            "nextest-test-results: HERMIT_NEXTEST_RESULTS_SCHEMA must be 5 when set, got {value:?}"
        )
        .into()),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(env::VarError::NotUnicode(_)) => {
            Err("nextest-test-results HERMIT_NEXTEST_RESULTS_SCHEMA is not UTF-8".into())
        }
    }
}

/// Synthesize classified attempts for one terminal row.
///
/// The event stream reports only the terminal outcome and the attempt count;
/// nextest retries only failures, so attempts before the terminal one failed.
fn classified_attempts(result: &TestResult) -> Result<Vec<TestAttemptResult>, String> {
    let mut attempts = Vec::new();
    for attempt in 1..result.attempts {
        attempts.push(TestAttemptResult::new(
            attempt,
            TestAttemptOutcome::Failed,
            Some(format!(
                "nextest attempt {attempt} of {} failed before the terminal attempt",
                result.attempts
            )),
        )?);
    }
    let terminal = if result.passed {
        TestAttemptResult::new(result.attempts, TestAttemptOutcome::Passed, None)?
    } else {
        TestAttemptResult::new(
            result.attempts,
            TestAttemptOutcome::Failed,
            Some("nextest reported this test failed".into()),
        )?
    };
    attempts.push(terminal);
    Ok(attempts)
}

/// Classify sidecar-marked rows as skips and give every row classified
/// attempts, as schema 5 requires. A marker naming a test with no terminal
/// row, more than one row, or a failed row refuses the report: a skip can
/// never launder a failure, and a marker without a started test is a
/// harness bug.
fn apply_skip_markers(
    results: Vec<TestResult>,
    markers: &[SkipMarker],
) -> Result<Vec<TestResult>, String> {
    let mut results = results;
    for marker in markers {
        let matches: Vec<usize> = results
            .iter()
            .enumerate()
            .filter(|(_, result)| {
                let test = result
                    .id
                    .rsplit_once('$')
                    .map_or(result.id.as_str(), |(_, test)| test);
                test == marker.name || test.ends_with(&format!("::{}", marker.name))
            })
            .map(|(index, _)| index)
            .collect();
        let [index] = matches.as_slice() else {
            return Err(format!(
                "nextest-test-results: skip marker for {:?} matches {} terminal row(s); refusing to guess",
                marker.name,
                matches.len()
            ));
        };
        let row = &results[*index];
        if !row.passed {
            return Err(format!(
                "nextest-test-results: skip marker for {:?} names a test that did not pass; a skip cannot launder a failure",
                marker.name
            ));
        }
        let mut attempts = classified_attempts(row)?;
        let terminal = attempts.pop().expect("classified_attempts is nonempty");
        debug_assert_eq!(terminal.outcome, TestAttemptOutcome::Passed);
        attempts.push(TestAttemptResult::new(
            row.attempts,
            TestAttemptOutcome::Skipped,
            Some(marker.reason.clone()),
        )?);
        results[*index] = TestResult::skipped(row.id.clone(), attempts, marker.reason.clone())?;
    }
    for result in &mut results {
        if result.attempt_results.is_none() {
            result.attempt_results = Some(classified_attempts(result)?);
        }
    }
    Ok(results)
}

/// The one shared skid budget a skid-infra marker must claim to have
/// exhausted (`detcore_model::SKID_RETRY_MAX_ATTEMPTS`); this producer
/// links no detcore-model, so the value is pinned here and in its test.
const SKID_INFRA_ATTEMPTS: u64 = 5;

/// The attempt count a skid-infra marker's reason claims
/// (`all <n> attempt(s) ...`), or `None` when it names none.
fn skid_infra_claimed_attempts(reason: &str) -> Option<u64> {
    reason
        .strip_prefix("all ")?
        .split_once(" attempt(s)")?
        .0
        .parse()
        .ok()
}

/// Classify skid-infra-marked rows' terminal attempts as
/// infrastructure errors. Runs after [`apply_skip_markers`], so every
/// row carries classified attempts. A marker naming a test with no
/// terminal row, more than one row, a passed row or a skipped row
/// refuses the report: the helper records the marker only after its
/// whole skid budget ended in typed refusals, and the caller's
/// assertion on the final refusal is what fails the raw row — a pass
/// or a skip beside the marker contradicts that, and guessing would
/// launder it. The row stays a non-pass (the step still fails at this
/// layer); only its terminal cause changes, from an ordinary failure
/// to the infrastructure verdict the marker names.
fn apply_skid_infra_markers(
    results: Vec<TestResult>,
    markers: &[SkipMarker],
) -> Result<Vec<TestResult>, String> {
    let mut results = results;
    for marker in markers {
        let matches: Vec<usize> = results
            .iter()
            .enumerate()
            .filter(|(_, result)| {
                let test = result.id.rsplit_once('$').map_or(result.id.as_str(), |(_, test)| test);
                test == marker.name || test.ends_with(&format!("::{}", marker.name))
            })
            .map(|(index, _)| index)
            .collect();
        let [index] = matches.as_slice() else {
            return Err(format!(
                "nextest-test-results: skid-infra marker for {:?} matches {} terminal row(s); refusing to guess",
                marker.name,
                matches.len()
            ));
        };
        if skid_infra_claimed_attempts(&marker.reason) != Some(SKID_INFRA_ATTEMPTS) {
            return Err(format!(
                "nextest-test-results: skid-infra marker for {:?} does not claim the shared skid budget of {SKID_INFRA_ATTEMPTS} attempts (reason {:?}); refusing to reclassify it",
                marker.name, marker.reason
            ));
        }
        let row = &results[*index];
        if row.passed || row.is_skipped() {
            return Err(format!(
                "nextest-test-results: skid-infra marker for {:?} names a test that did not fail; refusing to reclassify it",
                marker.name
            ));
        }
        let mut attempts = row.attempt_results.clone().ok_or_else(|| {
            format!(
                "nextest-test-results: skid-infra marker for {:?} names a row without classified attempts",
                marker.name
            )
        })?;
        let terminal = attempts.pop().expect("classified attempts are nonempty");
        if terminal.outcome != TestAttemptOutcome::Failed {
            return Err(format!(
                "nextest-test-results: skid-infra marker for {:?} names a row whose terminal attempt is {}, not a failure",
                marker.name,
                terminal.outcome.value()
            ));
        }
        attempts.push(TestAttemptResult::new(
            row.attempts,
            TestAttemptOutcome::InfrastructureError,
            Some(format!("SKID-INFRA: {}", marker.reason)),
        )?);
        results[*index] = TestResult::with_attempt_results(row.id.clone(), false, attempts)?;
    }
    Ok(results)
}

/// Record fail-fast-cancelled in-flight attempts as cancelled on the
/// rows they belong to (dh-825jy). A cancelled attempt follows its test's
/// terminal typed attempt; it is never a pass, and a test with no typed
/// row at all stays only in the CPU report, so executed counts never
/// move. A cancelled identity that is not the row's next attempt is a
/// real disagreement and refuses.
fn apply_cancelled_attempts(
    results: Vec<TestResult>,
    cancelled: &[AttemptIdentity],
    binary_map: Option<&BinaryMap>,
) -> Result<Vec<TestResult>, String> {
    if cancelled.is_empty() {
        return Ok(results);
    }
    let Some(binary_map) = binary_map else {
        return Err("nextest-test-results: cancelled attempts without a binary map".into());
    };
    let mut results = results;
    for identity in cancelled {
        let kind = binary_map
            .entries
            .iter()
            .find(|entry| {
                entry.package == identity.package && entry.binary == identity.binary
            })
            .map(|entry| entry.kind.as_str())
            .ok_or_else(|| {
                format!(
                    "nextest-test-results: cancelled attempt {identity:?} names no binary-map entry"
                )
            })?;
        let id = displayed_test_id(
            &identity.package,
            &identity.binary,
            &identity.test,
            kind,
            None,
        );
        let Some(row) = results.iter_mut().find(|row| row.id == id) else {
            continue; // no typed row: the CPU report alone records it
        };
        if row.passed || identity.attempt != row.attempts + 1 {
            return Err(format!(
                "nextest-test-results: cancelled attempt {identity:?} does not follow the terminal attempt of {id:?}"
            ));
        }
        let mut attempts = row.attempt_results.clone().ok_or_else(|| {
            format!("nextest-test-results: row {id:?} lacks classified attempts")
        })?;
        attempts.push(TestAttemptResult::new(
            identity.attempt,
            TestAttemptOutcome::Cancelled,
            Some(
                "attempt was cancelled in flight by a fail-fast cancel (dh-825jy)"
                    .into(),
            ),
        )?);
        *row = TestResult::with_attempt_results(row.id.clone(), false, attempts)?;
    }
    Ok(results)
}

fn publish_with_recovery(
    report: &TestResults,
    path: &Path,
    schema_5: bool,
) -> Result<(), ProducerError> {
    let write = |target: &Path| {
        if schema_5 {
            report.write_skipped_typed(target)
        } else {
            report.write_current_typed(target)
        }
    };
    match write(path) {
        Ok(()) => Ok(()),
        Err(primary @ TestResultsWriteError::Invalid(_)) => {
            Err(ProducerError::Publication(primary))
        }
        Err(
            primary @ (TestResultsWriteError::Write { .. } | TestResultsWriteError::Publish { .. }),
        ) => {
            let recovery = structured_test_results_recovery_path(path);
            match write(&recovery) {
                Ok(()) => Err(ProducerError::Publication(primary)),
                Err(recovery) => Err(ProducerError::PublicationRecovery { primary, recovery }),
            }
        }
    }
}

fn run(mut args: impl Iterator<Item = String>) -> Result<(), ProducerError> {
    let Some(first) = args.next() else {
        usage();
        return Err("nextest-test-results requires three arguments".into());
    };
    if first == "--help" || first == "-h" {
        usage();
        return Ok(());
    }
    if first == "--listed-count" {
        let inventory = args
            .next()
            .ok_or_else(|| "nextest-test-results missing NEXTEST_INVENTORY_JSON".to_string())?;
        if args.next().is_some() {
            return Err(
                "nextest-test-results listed-count mode received too many arguments".into(),
            );
        }
        let bytes = fs::read(&inventory)
            .map_err(|error| format!("cannot read nextest inventory {inventory}: {error}"))?;
        let listed = listed_count(&bytes)?;
        println!("{listed}");
        return Ok(());
    }
    if first == "--listed-ids" {
        let inventory = args
            .next()
            .ok_or_else(|| "nextest-test-results missing NEXTEST_INVENTORY_JSON".to_string())?;
        let output = args
            .next()
            .ok_or_else(|| "nextest-test-results missing listed-ids OUTPUT".to_string())?;
        if args.next().is_some() {
            return Err(
                "nextest-test-results listed-ids mode received too many arguments".into(),
            );
        }
        let bytes = fs::read(&inventory)
            .map_err(|error| format!("cannot read nextest inventory {inventory}: {error}"))?;
        let identities = listed_identities(&bytes)?;
        let mut text = identities.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        fs::write(&output, text)
            .map_err(|error| format!("cannot write listed ids {output}: {error}"))?;
        return Ok(());
    }
    if first == "--write-binary-map" {
        let inventory = args
            .next()
            .ok_or_else(|| "nextest-test-results missing NEXTEST_INVENTORY_JSON".to_string())?;
        let output = args
            .next()
            .ok_or_else(|| "nextest-test-results missing binary-map OUTPUT".to_string())?;
        if args.next().is_some() {
            return Err("nextest-test-results binary-map mode received too many arguments".into());
        }
        let bytes = fs::read(&inventory)
            .map_err(|error| format!("cannot read nextest inventory {inventory}: {error}"))?;
        let map = BinaryMap::from_nextest_inventory(&bytes)?;
        return write_binary_map_atomic(Path::new(&output), &map).map_err(ProducerError::from);
    }
    let (events, inventory_path) = if first == "--inventory" {
        let inventory = args.next().ok_or(
            "nextest-test-results --inventory requires NEXTEST_INVENTORY_JSON before EVENTS_JSONL",
        )?;
        let events = args
            .next()
            .ok_or("nextest-test-results missing EVENTS_JSONL")?;
        (events, Some(inventory))
    } else {
        (first, None)
    };
    let status = parse_u64(
        args.next()
            .ok_or_else(|| "nextest-test-results missing NEXTEST_STATUS".to_string())?,
        "nextest_status",
    )?;
    let output = args
        .next()
        .ok_or_else(|| "nextest-test-results missing OUTPUT".to_string())?;
    let attempt_records = args.next();
    let binary_map = args.next();
    let cpu_report = args.next();
    if args.next().is_some() {
        return Err("nextest-test-results received too many arguments".into());
    }
    if attempt_records.is_some() != binary_map.is_some()
        || attempt_records.is_some() != cpu_report.is_some()
    {
        return Err(
            "nextest-test-results requires ATTEMPT_RECORD_DIR, BINARY_MAP, and CPU_REPORT_OR_DASH together".into(),
        );
    }
    if binary_map.is_some() && inventory_path.is_none() {
        return Err("nextest-test-results requires --inventory when BINARY_MAP is supplied".into());
    }
    let binary_map = binary_map
        .as_deref()
        .map(|path| read_binary_map(Path::new(path)))
        .transpose()?;
    let inventory = inventory_path
        .map(|path| {
            let bytes = fs::read(&path)
                .map_err(|error| format!("cannot read nextest inventory {path}: {error}"))?;
            EventInventory::from_bytes(&bytes, binary_map.as_ref())
        })
        .transpose()?;
    let parsed = parse_events(Path::new(&events), inventory.as_ref())?;
    let schema_5 = schema_5_opted_in()?;
    let markers = read_skip_markers()?;
    if !markers.is_empty() && !schema_5 {
        return Err(
            "nextest-test-results: skip markers were recorded but this node has not opted into schema 5 (HERMIT_NEXTEST_RESULTS_SCHEMA=5); refusing to fold them into passes"
                .into(),
        );
    }
    let skid_infra_markers = read_skid_infra_markers()?;
    if !skid_infra_markers.is_empty() && !schema_5 {
        return Err(
            "nextest-test-results: skid-infra markers were recorded but this node has not opted into schema 5 (HERMIT_NEXTEST_RESULTS_SCHEMA=5); refusing to fold them into ordinary failures"
                .into(),
        );
    }
    let (cpu_report, cancelled_attempts) = match (attempt_records, binary_map.as_ref(), cpu_report) {
        (Some(records), Some(binary_map), Some(output)) => {
            let (report, cancelled) =
                reconcile_cpu_attempts_with_cancelled(&parsed, Path::new(&records), binary_map)?;
            (Some((report, output)), cancelled)
        }
        (None, None, None) => (None, Vec::new()),
        _ => unreachable!("argument pairing was checked above"),
    };
    let results = if schema_5 {
        apply_cancelled_attempts(
            apply_skid_infra_markers(
                apply_skip_markers(parsed.results, &markers)?,
                &skid_infra_markers,
            )?,
            &cancelled_attempts,
            binary_map.as_ref(),
        )?
    } else {
        parsed.results
    };
    let passed = u64::try_from(results.iter().filter(|result| result.passed).count())
        .map_err(|_| "nextest-test-results: passed result count does not fit u64".to_string())?;
    let skipped = u64::try_from(results.iter().filter(|result| result.is_skipped()).count())
        .map_err(|_| "nextest-test-results: skipped result count does not fit u64".to_string())?;
    let failed = parsed
        .executed_tests
        .checked_sub(passed + skipped)
        .ok_or_else(|| "nextest-test-results: passed count exceeds executed count".to_string())?;
    if status == 0 && failed != 0 {
        return Err(format!(
            "nextest-test-results: successful nextest status disagrees with {failed} failed typed result(s)"
        ).into());
    }
    let report = if schema_5 {
        TestResults::skipped(parsed.executed_tests, parsed.filtered_tests, results)?
    } else {
        TestResults::current(parsed.executed_tests, parsed.filtered_tests, results)?
    };
    if let Some(expected) = match env::var("NEXTEST_EXPECTED_EXECUTED") {
        Ok(value) => Some(parse_u64(value, "NEXTEST_EXPECTED_EXECUTED")?),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err("nextest-test-results NEXTEST_EXPECTED_EXECUTED is not UTF-8".into());
        }
    } {
        if report.executed_tests != expected {
            // Carry the outcome the refusal already knows. The typed report is
            // deliberately NOT written here -- publishing it would record a test
            // population nobody sanctioned, which is the whole point of this
            // ratchet -- so this message is the only place the run can say what
            // actually happened. Without the breakdown a node where every test
            // passed and a node that was failing produce the same line, and the
            // consumer records both as an unexplained failure with no test ids.
            return Err(format!(
                "nextest-test-results: expected {expected} tests to execute, saw {}, \
                 of which {passed} passed and {failed} failed; refusing because the \
                 selected set changed",
                report.executed_tests
            )
            .into());
        }
    }
    if output != "-" {
        publish_with_recovery(&report, Path::new(&output), schema_5)?;
    }
    if let Some((cpu_report, output)) = cpu_report {
        if output != "-" {
            write_report_atomic(Path::new(&output), &cpu_report)?;
        }
    }
    println!("running {} tests", report.executed_tests);
    let skipped_summary = if skipped > 0 {
        format!("; {skipped} skipped")
    } else {
        String::new()
    };
    if status == 0 {
        println!(
            "test result: ok. {passed} passed; 0 failed{skipped_summary}; 0 ignored; {} filtered out",
            report.filtered_tests
        );
    } else {
        println!(
            "test result: FAILED. {passed} passed; {failed} failed{skipped_summary}; 0 ignored; {} filtered out",
            report.filtered_tests
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run(env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nextest-test-results: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use nextest_cpu::AttemptCompletion;
    use nextest_cpu::BinaryMapEntry;
    use nextest_cpu::write_attempt_atomic;

    use super::*;

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

    // Entirely synthetic controls, not a reconstruction of a lost event stream.
    mod inventory_controls {
        use serde_json::json;

        use super::*;

        fn case(ignored: bool, reason: Option<&str>) -> Value {
            let filter = match reason {
                Some(reason) => json!({"status":"mismatch", "reason":reason}),
                None => json!({"status":"matches"}),
            };
            json!({"ignored":ignored, "filter-match":filter})
        }

        fn inventory(lib: Value, bin: Value) -> Value {
            json!({"rust-suites": {
                "pkg": {"package-name":"pkg", "binary-id":"pkg", "binary-name":"same",
                    "kind":"lib", "binary-path":"/synthetic/lib", "testcases":lib},
                "pkg::bin/same": {"package-name":"pkg", "binary-id":"pkg::bin/same", "binary-name":"same",
                    "kind":"bin", "binary-path":"/synthetic/bin", "testcases":bin}
            }})
        }

        fn loaded(value: &Value) -> EventInventory {
            let bytes = serde_json::to_vec(value).unwrap();
            let cpu_map = BinaryMap::from_nextest_inventory(&bytes).unwrap();
            EventInventory::from_bytes(&bytes, Some(&cpu_map)).unwrap()
        }

        fn start(kind: &str, count: u64) -> Value {
            json!({"type":"suite", "event":"started", "test_count":count,
                "nextest":{"crate":"pkg", "test_binary":"same", "kind":kind}})
        }

        fn end(kind: &str, passed: u64, failed: u64, ignored: u64, filtered: u64) -> Value {
            json!({"type":"suite", "event":if failed == 0 {"ok"} else {"failed"},
                "passed":passed, "failed":failed, "ignored":ignored, "filtered_out":filtered,
                "nextest":{"crate":"pkg", "test_binary":"same", "kind":kind}})
        }

        fn event(event: &str, test: &str) -> Value {
            json!({"type":"test", "event":event, "name":format!("pkg::same${test}")})
        }

        fn text(rows: &[Value]) -> String {
            rows.iter().map(|row| format!("{row}\n")).collect()
        }

        fn parse(rows: &[Value], inventory: &EventInventory) -> Result<ParsedEvents, String> {
            parse_event_text_with_inventory(&text(rows), Some(inventory))
        }

        fn refuses(rows: &[Value], inventory: &EventInventory, message: &str) {
            let error = parse(rows, inventory).unwrap_err();
            assert!(error.contains(message), "expected {message:?}, got {error}");
        }

        #[test]
        fn eight_executions_and_one_distinct_ignored_case_in_either_kind_and_order() {
            // The examined lib8/binignored1 shape, with synthetic names and
            // terminal populations, not the lost run's raw events.
            let mut executing = json!({"filtered":case(false, Some("expression"))});
            for number in 0..8 {
                executing[format!("runs_{number}")] = case(false, None);
            }
            let skipped = json!({"skip":case(true, Some("ignored"))});
            for reverse_kinds in [false, true] {
                let (run_kind, skip_kind) = if reverse_kinds {
                    ("bin", "lib")
                } else {
                    ("lib", "bin")
                };
                let inv = loaded(&if reverse_kinds {
                    inventory(skipped.clone(), executing.clone())
                } else {
                    inventory(executing.clone(), skipped.clone())
                });
                for reverse_starts in [false, true] {
                    for reverse_ends in [false, true] {
                        let mut rows = vec![start(run_kind, 8), start(skip_kind, 1)];
                        if reverse_starts {
                            rows.swap(0, 1);
                        }
                        rows.push(event("started", "skip"));
                        // Conservative synthetic publication: a producer may
                        // leave this ignored row queued until a test finishes.
                        rows.push(event("ignored", "skip"));
                        for number in 0..8 {
                            rows.push(event("started", &format!("runs_{number}")));
                            rows.push(event("ok", &format!("runs_{number}")));
                        }
                        let mut ends =
                            [end(run_kind, 8, 0, 0, 1), end(skip_kind, 0, 0, 1, u64::MAX)];
                        if reverse_ends {
                            ends.swap(0, 1);
                        }
                        rows.extend(ends);
                        let parsed = parse(&rows, &inv).unwrap();
                        assert_eq!((parsed.executed_tests, parsed.filtered_tests), (8, 2));
                        assert_eq!(parsed.results.len(), 8);
                        let scratch = Scratch::new();
                        for (number, expected) in parsed.expected_attempts.iter().enumerate() {
                            assert_eq!(expected.suite.kind, run_kind);
                            assert_eq!(expected.test, format!("runs_{number}"));
                            let cpu_id = if reverse_kinds {
                                "pkg::bin/same"
                            } else {
                                "pkg"
                            };
                            write_record(
                                &scratch.0,
                                &identified_attempt(
                                    "pkg",
                                    cpu_id,
                                    &expected.test,
                                    1,
                                    AttemptCompletion::Exit { code: 0 },
                                ),
                            );
                        }
                        assert_eq!(
                            reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &inv.binary_map)
                                .unwrap()
                                .0.attempts
                                .len(),
                            8
                        );
                    }
                }
            }
        }

        #[test]
        fn zero_count_started_under_ignored_and_complete_empty_overlaps() {
            let inv = loaded(&inventory(
                json!({"ordinary":case(false, Some("ignored"))}),
                json!({}),
            ));
            let mut rows = vec![
                start("lib", 0),
                start("bin", 0),
                event("started", "ordinary"),
                end("bin", 0, 0, 0, 0),
                // One filtered ordinary case plus the skipped-event running
                // underflow wraps back to zero in the examined producer.
                end("lib", 0, 0, 0, 0),
            ];
            let parsed = parse(&rows, &inv).unwrap();
            assert_eq!((parsed.executed_tests, parsed.filtered_tests), (0, 0));
            assert!(parsed.results.is_empty());
            // Separately labelled conservative synthetic ignored-row case;
            // not a claim that finalize flushes a producer's ignored buffer.
            rows.insert(3, event("ignored", "ordinary"));
            assert_eq!(parse(&rows, &inv).unwrap(), parsed);
            refuses(
                &[start("lib", 0), event("ok", "ordinary")],
                &inv,
                "0 eligible matches",
            );
            let empty = loaded(&inventory(
                json!({"a":case(false, Some("expression")), "b":case(false, Some("expression")), "c":case(false, Some("expression"))}),
                json!({"a":case(false, Some("expression")), "b":case(false, Some("expression"))}),
            ));
            let rows = [
                start("lib", 0),
                start("bin", 0),
                end("bin", 0, 0, 0, 2),
                end("lib", 0, 0, 0, 3),
            ];
            assert_eq!(parse(&rows, &empty).unwrap().filtered_tests, 5);
            assert!(
                parse_event_text(&text(&rows))
                    .unwrap_err()
                    .contains("ambiguous")
            );
            refuses(
                &rows[..2],
                &empty,
                "before open suites published terminal events",
            );
        }

        #[test]
        fn positive_disjoint_execution_names_and_shared_ineligible_names() {
            let inv = loaded(&inventory(
                json!({"left":case(false, None), "right":case(false, Some("expression"))}),
                json!({"right":case(false, None), "left":case(false, Some("partition"))}),
            ));
            let rows = [
                start("lib", 1),
                start("bin", 1),
                event("ok", "right"),
                event("ok", "left"),
                end("lib", 1, 0, 0, 1),
                end("bin", 1, 0, 0, 1),
            ];
            let parsed = parse(&rows, &inv).unwrap();
            assert_eq!((parsed.executed_tests, parsed.filtered_tests), (2, 2));
            assert_eq!(
                parsed.results,
                vec![
                    TestResult::new("pkg$left".into(), true, 1).unwrap(),
                    TestResult::new("pkg::same$right".into(), true, 1).unwrap(),
                ]
            );
        }

        #[test]
        fn ambiguity_refuses_at_start_even_without_a_test_row() {
            for right_case in [case(false, None), case(true, Some("ignored"))] {
                let inv = loaded(&inventory(
                    json!({"shared":case(false, None)}),
                    json!({"shared":right_case}),
                ));
                refuses(&[start("lib", 1), start("bin", 1)], &inv, "ambiguous");
            }
            let zero = loaded(&inventory(
                json!({"shared":case(false, Some("ignored"))}),
                json!({"shared":case(false, Some("ignored"))}),
            ));
            refuses(&[start("lib", 0), start("bin", 0)], &zero, "ambiguous");
            for count in [0, 1] {
                let cases = if count == 0 {
                    json!({})
                } else {
                    json!({"case":case(false, None)})
                };
                let inv = loaded(&inventory(cases, json!({})));
                refuses(
                    &[start("lib", count), start("lib", count)],
                    &inv,
                    "duplicate active suite identity",
                );
            }
        }

        #[test]
        fn event_specific_membership_and_closed_suite_never_fall_back_to_sibling() {
            for kind in ["started", "ignored", "ok", "failed"] {
                let selected = if kind == "ignored" {
                    case(true, Some("ignored"))
                } else {
                    case(false, None)
                };
                let inv = loaded(&inventory(
                    json!({"case":selected}),
                    json!({"case":case(false, Some("expression"))}),
                ));
                let ignored = u64::from(kind == "ignored");
                refuses(
                    &[
                        start("lib", 1),
                        start("bin", 0),
                        end("lib", 0, 0, ignored, 0),
                        event(kind, "case"),
                    ],
                    &inv,
                    "0 eligible matches",
                );
            }
            let inv = loaded(&inventory(
                json!({
                    "run":case(false, None), "skip":case(true, Some("ignored")),
                    "filter":case(false, Some("expression"))
                }),
                json!({}),
            ));
            for (kind, name) in [
                ("ok", "skip"),
                ("failed", "skip"),
                ("ok", "filter"),
                ("failed", "filter"),
                ("started", "filter"),
                ("ignored", "run"),
                ("started", "renamed"),
                ("ok", "run#bogus"),
            ] {
                refuses(
                    &[start("lib", 2), event(kind, name)],
                    &inv,
                    "0 eligible matches",
                );
            }
            for suffix in ["0", "18446744073709551616"] {
                refuses(
                    &[start("lib", 2), event("ok", &format!("run#{suffix}"))],
                    &inv,
                    "retry count",
                );
            }
        }

        #[test]
        fn malformed_classification_and_inventory_never_enable_fallback() {
            let original = inventory(json!({"case":case(false, None)}), json!({}));
            for bad_case in [
                json!({}),
                json!({"ignored":false}),
                json!({"ignored":"false", "filter-match":{"status":"matches"}}),
                json!({"ignored":false, "filter-match":{"status":"unknown"}}),
                json!({"ignored":false, "filter-match":{"status":"mismatch"}}),
                json!({"ignored":false, "filter-match":{"status":"mismatch", "reason":false}}),
                json!({"ignored":false, "filter-match":{"status":"mismatch", "reason":"unknown"}}),
                json!({"ignored":false, "filter-match":{"status":"matches", "reason":"ignored"}}),
            ] {
                let mut bad = original.clone();
                bad["rust-suites"]["pkg"]["testcases"]["case"] = bad_case;
                assert!(
                    EventInventory::from_bytes(&serde_json::to_vec(&bad).unwrap(), None).is_err(),
                    "{bad}"
                );
            }
            for bad in [
                json!({}),
                json!({"rust-suites":[]}),
                json!({"rust-suites":{"pkg":{}}}),
            ] {
                assert!(
                    EventInventory::from_bytes(&serde_json::to_vec(&bad).unwrap(), None).is_err()
                );
            }
            assert!(EventInventory::from_bytes(b"not JSON", None).is_err());
            let mut missing = original.clone();
            missing["rust-suites"]["pkg"]
                .as_object_mut()
                .unwrap()
                .remove("testcases");
            assert!(
                EventInventory::from_bytes(&serde_json::to_vec(&missing).unwrap(), None).is_err()
            );
            let empty_name = inventory(json!({"":case(false, None)}), json!({}));
            assert!(
                EventInventory::from_bytes(&serde_json::to_vec(&empty_name).unwrap(), None)
                    .unwrap_err()
                    .contains("nonempty")
            );
            // All known other mismatch reasons are ineligible. Unknown future
            // reasons must refuse instead of silently removing a candidate.
            for reason in [
                "string",
                "expression",
                "partition",
                "default-filter",
                "not-benchmark",
                "rerun-already-passed",
            ] {
                let inv = loaded(&inventory(
                    json!({"case":case(false, Some(reason))}),
                    json!({}),
                ));
                refuses(
                    &[start("lib", 0), event("started", "case")],
                    &inv,
                    "0 eligible matches",
                );
            }
        }

        #[test]
        fn inventory_projection_must_equal_existing_cpu_map() {
            let original = inventory(json!({"case":case(false, None)}), json!({}));
            let map = loaded(&original).binary_map;
            for (field, value) in [
                ("package-name", "other"),
                ("binary-name", "other"),
                ("kind", "test"),
                ("binary-path", "/synthetic/substituted"),
                ("binary-id", "other"),
            ] {
                let mut bad = original.clone();
                bad["rust-suites"]["pkg"][field] = json!(value);
                assert!(
                    EventInventory::from_bytes(&serde_json::to_vec(&bad).unwrap(), Some(&map))
                        .is_err(),
                    "{field}"
                );
            }
            let mut duplicate = original.clone();
            duplicate["rust-suites"]["pkg::bin/same"]["kind"] = json!("lib");
            assert!(
                EventInventory::from_bytes(&serde_json::to_vec(&duplicate).unwrap(), None)
                    .unwrap_err()
                    .contains("ambiguous")
            );
            let mut invalid_map = map.clone();
            invalid_map.schema += 1;
            assert!(
                EventInventory::from_bytes(
                    &serde_json::to_vec(&original).unwrap(),
                    Some(&invalid_map)
                )
                .is_err()
            );
        }

        #[test]
        fn start_and_ignored_counts_must_match_but_inventory_never_supplies_results() {
            let inv = loaded(&inventory(
                json!({"run":case(false, None), "skip":case(true, Some("expression"))}),
                json!({}),
            ));
            refuses(
                &[start("lib", 1)],
                &inv,
                "start count 1 disagrees with inventory count 2",
            );
            refuses(
                &[start("lib", 2), end("lib", 0, 0, 0, 0)],
                &inv,
                "ignored count 0 disagrees",
            );
            let parsed = parse(&[start("lib", 2), end("lib", 0, 0, 1, 1)], &inv).unwrap();
            assert_eq!((parsed.executed_tests, parsed.filtered_tests), (0, 2));
            assert!(parsed.expected_attempts.is_empty());
        }

        #[test]
        fn compensating_cross_suite_and_cross_episode_outcomes_refuse() {
            let inv = loaded(&inventory(
                json!({"left":case(false, None)}),
                json!({"right":case(false, None)}),
            ));
            let valid = vec![
                start("lib", 1),
                start("bin", 1),
                event("ok", "left"),
                event("failed", "right"),
                end("lib", 1, 0, 0, 0),
                end("bin", 0, 1, 0, 0),
            ];
            assert_eq!(parse(&valid, &inv).unwrap().executed_tests, 2);
            // Same aggregate totals, swapped per-suite outcomes.
            let mut swapped = valid.clone();
            swapped[4] = end("lib", 0, 1, 0, 0);
            swapped[5] = end("bin", 1, 0, 0, 0);
            refuses(&swapped, &inv, "passed/failed");
            let mut stolen = valid.clone();
            stolen.remove(3);
            stolen[3] = end("lib", 0, 0, 0, 0);
            stolen[4] = end("bin", 1, 0, 0, 0);
            refuses(&stolen, &inv, "episode");
            // Also protect successive episodes of the same typed identity,
            // including the existing no-inventory path.
            let recaps = [
                start("lib", 1),
                event("ok", "left"),
                end("lib", 0, 0, 0, 0),
                start("lib", 1),
                end("lib", 1, 0, 0, 0),
            ];
            assert!(
                parse_event_text(&text(&recaps))
                    .unwrap_err()
                    .contains("episode")
            );
            refuses(&recaps, &inv, "episode");
        }

        #[test]
        fn wrong_typed_terminals_duplicates_and_missing_terminals_refuse() {
            let inv = loaded(&inventory(json!({}), json!({})));
            for (field, value) in [
                ("crate", "other"),
                ("test_binary", "other"),
                ("kind", "test"),
            ] {
                let mut terminal = end("lib", 0, 0, 0, 0);
                terminal["nextest"][field] = json!(value);
                assert!(parse(&[start("lib", 0), start("bin", 0), terminal], &inv).is_err());
            }
            refuses(&[end("lib", 0, 0, 0, 0)], &inv, "ended before");
            refuses(
                &[
                    start("lib", 0),
                    end("lib", 0, 0, 0, 0),
                    end("lib", 0, 0, 0, 0),
                ],
                &inv,
                "ended before",
            );
            refuses(
                &[start("lib", 0), start("bin", 0), end("lib", 0, 0, 0, 0)],
                &inv,
                "before open suites",
            );
            let inv = loaded(&inventory(json!({"run":case(false, None)}), json!({})));
            refuses(
                &[start("lib", 1), event("ok", "run"), event("ok", "run#2")],
                &inv,
                "duplicate terminal test id",
            );
        }

        #[test]
        fn partial_recaps_selected_ignored_and_wrapping_keep_exact_population_math() {
            let inv = loaded(&inventory(
                json!({"one":case(true, None), "two":case(true, None),
                "three":case(true, Some("expression"))}),
                json!({}),
            ));
            let rows = [
                start("lib", 3),
                start("bin", 0),
                event("started", "one"),
                end("lib", 0, 0, 3, 0),
                start("lib", 3),
                event("ok", "one"),
                event("ok", "two#2"),
                end("lib", 2, 0, 3, u64::MAX - 1),
                end("bin", 0, 0, 0, 4),
            ];
            let parsed = parse(&rows, &inv).unwrap();
            assert_eq!((parsed.executed_tests, parsed.filtered_tests), (2, 5));
            assert_eq!(parsed.results[1].attempts, 2);
            let mut malformed = rows.clone();
            malformed[7]["filtered_out"] = json!(u64::MAX - 3);
            refuses(&malformed, &inv, "overflows u64");
            // Exact all-ignored and smaller wrapped-deficit sentinels.
            for filtered in [u64::MAX - 2, u64::MAX] {
                let parsed = parse(
                    &[
                        start("lib", 3),
                        start("bin", 0),
                        end("lib", 0, 0, 3, filtered),
                        end("bin", 0, 0, 0, 0),
                    ],
                    &inv,
                )
                .unwrap();
                assert_eq!((parsed.executed_tests, parsed.filtered_tests), (0, 3));
            }
            // Inventory totals cannot replace the typed recap population.
            let ordinary = loaded(&inventory(json!({"one":case(false, None)}), json!({})));
            assert_eq!(
                parse(
                    &[start("lib", 1), event("ok", "one"), end("lib", 1, 0, 0, 7)],
                    &ordinary
                )
                .unwrap()
                .filtered_tests,
                7
            );
        }

        #[test]
        fn checked_arithmetic_and_population_underflow_remain_refusals() {
            assert!(
                suite_population(1, u64::MAX, 1, 0, 0, 1)
                    .unwrap_err()
                    .contains("overflows")
            );
            assert!(
                suite_population(1, 2, 0, 1, u64::MAX, 1)
                    .unwrap_err()
                    .contains("inconsistent")
            );
            assert!(
                suite_population(1, 0, 0, u64::MAX, 2, 1)
                    .unwrap_err()
                    .contains("overflows")
            );
            let rows = [
                start("lib", 1),
                event("ok", "one"),
                end("lib", 1, 0, 0, 0),
                start("lib", 1),
                event("ok", "two"),
                end("lib", 1, 0, 0, 0),
            ];
            assert!(
                parse_event_text(&text(&rows))
                    .unwrap_err()
                    .contains("exceeding its typed population")
            );
        }

        #[test]
        fn retries_keep_exact_cpu_identity_and_refuse_missing_or_different_outcomes() {
            let inv = loaded(&inventory(json!({"run":case(false, None)}), json!({})));
            let parsed = parse(
                &[
                    start("lib", 1),
                    start("bin", 0),
                    event("ok", "run#2"),
                    end("bin", 0, 0, 0, 0),
                    end("lib", 1, 0, 0, 0),
                ],
                &inv,
            )
            .unwrap();
            let scratch = Scratch::new();
            write_record(
                &scratch.0,
                &identified_attempt("pkg", "pkg", "run", 1, AttemptCompletion::Exit { code: 23 }),
            );
            assert!(
                reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &inv.binary_map)
                    .unwrap_err()
                    .contains("missing=")
            );
            write_record(
                &scratch.0,
                &identified_attempt("pkg", "pkg", "run", 2, AttemptCompletion::Exit { code: 0 }),
            );
            assert_eq!(
                reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &inv.binary_map)
                    .unwrap()
                    .0.attempts
                    .len(),
                2
            );
            let wrong = Scratch::new();
            for number in [1, 2] {
                write_record(
                    &wrong.0,
                    &identified_attempt(
                        "pkg",
                        "pkg",
                        "run",
                        number,
                        AttemptCompletion::Exit { code: 0 },
                    ),
                );
            }
            assert!(
                reconcile_cpu_attempts_with_cancelled(&parsed, &wrong.0, &inv.binary_map)
                    .unwrap_err()
                    .contains("disagree on whether")
            );
        }

        #[test]
        fn stress_identity_is_exact_and_remains_unsupported_for_cpu() {
            let inv = loaded(&inventory(
                json!({"run":case(false, None), "skip":case(true, Some("ignored"))}),
                json!({}),
            ));
            let mut rows = Vec::new();
            for index in [1, 2] {
                let mut row = start("lib", 2);
                row["nextest"]["stress_index"] = json!(index);
                rows.push(row);
            }
            for index in [1, 2] {
                rows.push(json!({"type":"test", "event":"ok", "name":format!("pkg::same@stress-{index}$run")}));
                let mut row = end("lib", 1, 0, 1, 0);
                row["nextest"]["stress_index"] = json!(index);
                rows.push(row);
            }
            let parsed = parse(&rows, &inv).unwrap();
            assert_eq!((parsed.executed_tests, parsed.filtered_tests), (2, 2));
            assert_eq!(parsed.results[0].id, "pkg@stress-1$run");
            assert_eq!(parsed.results[1].id, "pkg@stress-2$run");
            let scratch = Scratch::new();
            assert!(
                reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &inv.binary_map)
                    .unwrap_err()
                    .contains("does not support stress_index")
            );
            for bad in [json!(-1), json!("1"), Value::Null] {
                let mut mutated = rows.clone();
                mutated[0]["nextest"]["stress_index"] = bad;
                refuses(&mutated, &inv, "stress_index must be an unsigned integer");
            }
            let mut wrong_suffix = rows.clone();
            wrong_suffix[2]["name"] = json!("pkg::same@stress-3$run");
            refuses(&wrong_suffix, &inv, "0 eligible matches");
            let mut wrong_terminal = rows.clone();
            wrong_terminal[3]["nextest"]["stress_index"] = json!(3);
            refuses(&wrong_terminal, &inv, "ended before");
            // Queued ignored rows omit stress in the examined generator.
            // Refuse that omission; never map it onto a stress suite.
            refuses(
                &[rows[0].clone(), event("ignored", "skip")],
                &inv,
                "0 eligible matches",
            );
        }

        fn check_binary_map_requires_inventory(status: u64) {
            let scratch = Scratch::new();
            let records = Scratch::new();
            let passed = status == 0;
            let inventory = inventory(json!({"run":case(false, None)}), json!({}));
            let inventory_bytes = serde_json::to_vec(&inventory).unwrap();
            let inventory_path = scratch.0.join("inventory.json");
            fs::write(&inventory_path, &inventory_bytes).unwrap();
            let map_path = scratch.0.join("binary-map.json");
            write_binary_map_atomic(
                &map_path,
                &BinaryMap::from_nextest_inventory(&inventory_bytes).unwrap(),
            )
            .unwrap();
            let events = scratch.0.join("events.jsonl");
            fs::write(
                &events,
                text(&[
                    start("lib", 1),
                    event("started", "run"),
                    event(if passed { "ok" } else { "failed" }, "run"),
                    end("lib", u64::from(passed), u64::from(!passed), 0, 0),
                ]),
            )
            .unwrap();
            let record = identified_attempt(
                "pkg",
                "pkg",
                "run",
                1,
                AttemptCompletion::Exit {
                    code: if passed { 0 } else { 23 },
                },
            );
            write_record(&records.0, &record);
            let counts = scratch.0.join("counts.json");
            let cpu = scratch.0.join("cpu.json");
            let args = vec![
                events.display().to_string(),
                status.to_string(),
                counts.display().to_string(),
                records.0.display().to_string(),
                map_path.display().to_string(),
                cpu.display().to_string(),
            ];
            let result = run(args.clone().into_iter());
            assert_eq!(
                (
                    counts.exists(),
                    structured_test_results_recovery_path(&counts).exists(),
                    cpu.exists(),
                ),
                (false, false, false),
                "missing inventory published results for nextest status {status}"
            );
            let error = result.unwrap_err();
            assert_eq!(error.exit_code(), 2);
            assert_eq!(
                error.to_string(),
                "nextest-test-results requires --inventory when BINARY_MAP is supplied"
            );

            // The same otherwise-valid inputs publish when the carrier is present.
            let mut admitted = vec!["--inventory".into(), inventory_path.display().to_string()];
            admitted.extend(args.clone());
            run(admitted.into_iter()).unwrap();
            let check_counts = |path: &Path| {
                let report = TestResults::from_json_slice(&fs::read(path).unwrap()).unwrap();
                assert_eq!((report.executed_tests, report.filtered_tests), (1, 0));
                let results = report.results.unwrap();
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].id, "pkg$run");
                assert_eq!(results[0].passed, passed);
                assert_eq!(results[0].attempts, 1);
            };
            check_counts(&counts);
            let report: CpuReport = serde_json::from_slice(&fs::read(&cpu).unwrap()).unwrap();
            assert_eq!(report.attempts, vec![record]);

            // The explicit no-map legacy invocation remains supported.
            let legacy = scratch.0.join("legacy-counts.json");
            let mut legacy_args = args[..3].to_vec();
            legacy_args[2] = legacy.display().to_string();
            run(legacy_args.into_iter()).unwrap();
            check_counts(&legacy);
        }

        #[test]
        fn binary_map_requires_inventory_for_successful_nextest() {
            check_binary_map_requires_inventory(0);
        }

        #[test]
        fn binary_map_requires_inventory_for_failed_nextest() {
            check_binary_map_requires_inventory(100);
        }
    }

    #[test]
    fn only_typed_publication_io_uses_no_result_status() {
        use std::io;
        let invalid = ProducerError::Publication(TestResultsWriteError::Invalid(
            "permission denied is only text in an invalid report".into(),
        ));
        assert_eq!(invalid.exit_code(), 2);
        assert_eq!(
            ProducerError::from("structured-test-results-write fake: denied").exit_code(),
            2
        );
        for error in [
            TestResultsWriteError::Write {
                path: PathBuf::from("temporary"),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            TestResultsWriteError::Publish {
                path: PathBuf::from("destination"),
                source: io::Error::from(io::ErrorKind::IsADirectory),
            },
        ] {
            let message = error.to_string();
            let mapped = ProducerError::Publication(error);
            assert_eq!(mapped.exit_code(), 75);
            assert_eq!(mapped.to_string(), message);
        }
    }

    #[test]
    fn publication_failure_preserves_validated_schema_two_rows_in_recovery() {
        let scratch = Scratch::new();
        let report = TestResults::current(
            2,
            3,
            vec![
                TestResult::new("suite$passes".into(), true, 1).unwrap(),
                TestResult::new("suite$fails".into(), false, 1).unwrap(),
            ],
        )
        .unwrap();
        let expected = scratch.0.join("expected.json");
        publish_with_recovery(&report, &expected, false).unwrap();
        assert!(!structured_test_results_recovery_path(&expected).exists());

        let primary = scratch.0.join("primary.json");
        fs::create_dir(&primary).unwrap();
        let error = publish_with_recovery(&report, &primary, false).unwrap_err();
        assert_eq!(error.exit_code(), 75);
        match error {
            ProducerError::Publication(TestResultsWriteError::Publish { path, .. }) => {
                assert_eq!(path, primary)
            }
            other => panic!("expected the original publish error, got {other:?}"),
        }
        let recovery = structured_test_results_recovery_path(&primary);
        assert_eq!(fs::read(&recovery).unwrap(), fs::read(&expected).unwrap());
        assert_eq!(
            TestResults::from_json_slice(&fs::read(recovery).unwrap()).unwrap(),
            report
        );
    }

    #[test]
    fn skip_sidecar_parses_markers_and_refuses_malformed_lines() {
        assert_eq!(parse_skip_sidecar(b"").unwrap(), Vec::new());
        assert_eq!(
            parse_skip_sidecar(b"HERMIT-TEST-SKIP: run_kvm_case\tno /dev/kvm on this host\n")
                .unwrap(),
            vec![SkipMarker {
                name: "run_kvm_case".into(),
                reason: "no /dev/kvm on this host".into(),
            }]
        );
        // A truncated final line, a duplicate, a missing tab, an empty
        // reason, and an overlong line each refuse rather than guess.
        assert!(parse_skip_sidecar(b"HERMIT-TEST-SKIP: a\tb").is_err());
        assert!(parse_skip_sidecar(b"HERMIT-TEST-SKIP: a\tb\nHERMIT-TEST-SKIP: a\tc\n").is_err());
        assert!(parse_skip_sidecar(b"HERMIT-TEST-SKIP: a b\n").is_err());
        assert!(parse_skip_sidecar(b"HERMIT-TEST-SKIP: a\t\n").is_err());
        assert!(parse_skip_sidecar(b"a\tb\n").is_err());
        let overlong = format!("HERMIT-TEST-SKIP: a\t{}\n", "x".repeat(4096));
        assert!(parse_skip_sidecar(overlong.as_bytes()).is_err());
    }

    #[test]
    fn skip_markers_classify_ok_rows_and_refuse_the_rest() {
        let rows = vec![
            TestResult::new("suite$skips".into(), true, 1).unwrap(),
            TestResult::new("suite$runs".into(), true, 2).unwrap(),
            TestResult::new("suite$fails".into(), false, 1).unwrap(),
        ];
        let marker = SkipMarker {
            name: "skips".into(),
            reason: "no /dev/kvm on this host".into(),
        };
        let classified = apply_skip_markers(rows.clone(), &[marker]).unwrap();
        assert!(classified[0].is_skipped());
        assert_eq!(
            classified[0].skip_reason.as_deref(),
            Some("no /dev/kvm on this host")
        );
        assert!(classified[1].passed);
        // Every row carries classified attempts at schema 5.
        assert!(classified.iter().all(|row| row.attempt_results.is_some()));
        let report = TestResults::skipped(3, 0, classified).unwrap();
        let scratch = Scratch::new();
        let path = scratch.0.join("skipped.json");
        publish_with_recovery(&report, &path, true).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            TestResults::from_declared_schema_json_slice(&bytes, 5).unwrap(),
            report
        );

        // A marker for a failed test, an absent test, or two rows sharing
        // one test name each refuse.
        let launder = SkipMarker {
            name: "fails".into(),
            reason: "no /dev/kvm on this host".into(),
        };
        assert!(
            apply_skip_markers(rows.clone(), &[launder])
                .unwrap_err()
                .contains("launder")
        );
        let absent = SkipMarker {
            name: "never_ran".into(),
            reason: "no /dev/kvm on this host".into(),
        };
        assert!(
            apply_skip_markers(rows.clone(), &[absent])
                .unwrap_err()
                .contains("matches 0")
        );
        let ambiguous_rows = vec![
            TestResult::new("alpha::tool$case".into(), true, 1).unwrap(),
            TestResult::new("beta::tool$case".into(), true, 1).unwrap(),
        ];
        let ambiguous = SkipMarker {
            name: "case".into(),
            reason: "no /dev/kvm on this host".into(),
        };
        assert!(
            apply_skip_markers(ambiguous_rows, &[ambiguous])
                .unwrap_err()
                .contains("matches 2")
        );
    }

    #[test]
    fn skid_infra_markers_reclassify_failed_rows_and_refuse_the_rest() {
        // Parse: the skid-infra sidecar has the skip sidecar's shape and
        // strictness, under its own prefix.
        assert_eq!(
            parse_skid_infra_sidecar(
                b"HERMIT-SKID-INFRA: spinner\tall 5 attempt(s) exited 122; verdict SKID-INFRA\n"
            )
            .unwrap(),
            vec![SkipMarker {
                name: "spinner".into(),
                reason: "all 5 attempt(s) exited 122; verdict SKID-INFRA".into(),
            }]
        );
        assert!(parse_skid_infra_sidecar(b"HERMIT-TEST-SKIP: a\tb\n").is_err());
        assert!(parse_skid_infra_sidecar(b"HERMIT-SKID-INFRA: a\tb").is_err());

        let rows = vec![
            TestResult::new("suite$spinner".into(), false, 1).unwrap(),
            TestResult::new("suite$runs".into(), true, 1).unwrap(),
            TestResult::new("suite$fails".into(), false, 1).unwrap(),
        ];
        let classified = apply_skip_markers(rows.clone(), &[]).unwrap();
        let marker = SkipMarker {
            name: "spinner".into(),
            reason: "all 5 attempt(s) exited 122; verdict SKID-INFRA (not a test failure)"
                .into(),
        };
        let reclassified = apply_skid_infra_markers(classified, &[marker]).unwrap();
        // Still a non-pass, but its terminal cause is the infrastructure
        // verdict, not an ordinary failure; the unmarked failure beside
        // it stays an ordinary failure.
        assert!(!reclassified[0].passed && !reclassified[0].is_skipped());
        let terminal = reclassified[0].attempt_results.as_ref().unwrap().last().unwrap();
        assert_eq!(terminal.outcome, TestAttemptOutcome::InfrastructureError);
        assert!(
            terminal.detail.as_deref().unwrap().starts_with("SKID-INFRA: "),
            "{terminal:?}"
        );
        let other = reclassified[2].attempt_results.as_ref().unwrap().last().unwrap();
        assert_eq!(other.outcome, TestAttemptOutcome::Failed);
        // The schema-5 report round-trips with the reclassified attempt.
        let report = TestResults::skipped(3, 0, reclassified).unwrap();
        let scratch = Scratch::new();
        let path = scratch.0.join("skid-infra.json");
        publish_with_recovery(&report, &path, true).unwrap();
        assert_eq!(
            TestResults::from_declared_schema_json_slice(&fs::read(&path).unwrap(), 5).unwrap(),
            report
        );

        // A marker for a passed test, an absent test, or a skipped test
        // each refuse: only a failed row can carry the verdict.
        let classified = apply_skip_markers(rows.clone(), &[]).unwrap();
        for (name, needle) in [("runs", "did not fail"), ("never_ran", "matches 0")] {
            let marker = SkipMarker {
                name: name.into(),
                reason: "all 5 attempt(s) exited 122; verdict SKID-INFRA".into(),
            };
            assert!(
                apply_skid_infra_markers(classified.clone(), &[marker])
                    .unwrap_err()
                    .contains(needle),
                "{name}"
            );
        }
        // A marker claiming a bound below the shared skid budget (a
        // single-attempt probe, a 3-attempt helper) is not an exhaustion.
        for claimed in ["all 1 attempt(s) exited 122; verdict SKID-INFRA",
                        "all 3 attempt(s) exited 122; verdict SKID-INFRA",
                        "verdict SKID-INFRA"] {
            let marker = SkipMarker {
                name: "spinner".into(),
                reason: claimed.into(),
            };
            assert!(
                apply_skid_infra_markers(classified.clone(), &[marker])
                    .unwrap_err()
                    .contains("shared skid budget"),
                "{claimed}"
            );
        }
        let skipped = apply_skip_markers(
            rows.clone(),
            &[SkipMarker {
                name: "runs".into(),
                reason: "no /dev/kvm on this host".into(),
            }],
        )
        .unwrap();
        let marker = SkipMarker {
            name: "runs".into(),
            reason: "all 5 attempt(s) exited 122; verdict SKID-INFRA".into(),
        };
        assert!(apply_skid_infra_markers(skipped, &[marker])
            .unwrap_err()
            .contains("did not fail"));
    }

    #[test]
    fn recovery_failure_reports_both_errors_without_weakening_status() {
        let scratch = Scratch::new();
        let report = TestResults::current(
            1,
            0,
            vec![TestResult::new("suite$case".into(), true, 1).unwrap()],
        )
        .unwrap();
        let primary = scratch.0.join("primary.json");
        fs::create_dir(&primary).unwrap();
        fs::create_dir(structured_test_results_recovery_path(&primary)).unwrap();
        let error = publish_with_recovery(&report, &primary, false).unwrap_err();
        assert_eq!(error.exit_code(), 75);
        assert!(matches!(error, ProducerError::PublicationRecovery { .. }));
        let message = error.to_string();
        assert!(
            message.contains("structured-test-results-publish"),
            "{message}"
        );
        assert!(
            message.contains("structured-test-results-recovery also failed"),
            "{message}"
        );
    }

    #[test]
    fn invalid_report_never_writes_recovery() {
        let scratch = Scratch::new();
        let primary = scratch.0.join("primary.json");
        let invalid = TestResults {
            executed_tests: 2,
            filtered_tests: 0,
            results: Some(vec![TestResult::new("suite$case".into(), true, 1).unwrap()]),
            selection: None,
        };
        let error = publish_with_recovery(&invalid, &primary, false).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(matches!(
            error,
            ProducerError::Publication(TestResultsWriteError::Invalid(_))
        ));
        assert!(!primary.exists());
        assert!(!structured_test_results_recovery_path(&primary).exists());
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "hermit-nextest-test-results-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_events() -> &'static str {
        concat!(
            r#"{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"suite::suite$passes"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"suite::suite$recovers#2"}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"filtered_out":0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
        )
    }

    fn attempt(test: &str, attempt: u64, completion: AttemptCompletion) -> AttemptRecord {
        identified_attempt("suite", "suite", test, attempt, completion)
    }

    fn identified_attempt(
        package: &str,
        binary: &str,
        test: &str,
        attempt: u64,
        completion: AttemptCompletion,
    ) -> AttemptRecord {
        AttemptRecord::new(
            "sample-run".into(),
            AttemptIdentity {
                package: package.into(),
                binary: binary.into(),
                test: test.into(),
                attempt,
            },
            12_345,
            67,
            completion,
        )
    }

    fn write_record(directory: &Path, record: &AttemptRecord) {
        write_attempt_atomic(directory, record).unwrap();
    }

    fn sample_binary_map() -> BinaryMap {
        BinaryMap {
            schema: nextest_cpu::BINARY_MAP_SCHEMA,
            entries: vec![BinaryMapEntry {
                executable: "/tmp/suite-binary".into(),
                package: "suite".into(),
                binary: "suite".into(),
                binary_name: "suite".into(),
                kind: "lib".into(),
            }],
        }
    }

    #[test]
    fn raw_names_keep_the_suite_identity_and_retry_count() {
        assert_eq!(
            test_identity("pkg::internal_lib$module::case#2", 1).unwrap(),
            (
                ("pkg".into(), "internal_lib".into()),
                "module::case".into(),
                2
            )
        );
        assert_eq!(
            test_identity("pkg::integration_case$module::case", 1).unwrap(),
            (
                ("pkg".into(), "integration_case".into()),
                "module::case".into(),
                1
            )
        );
        assert_eq!(
            displayed_test_id("hermit-detcore", "detcore", "module::case", "lib", None),
            "hermit-detcore$module::case"
        );
        assert_eq!(
            displayed_test_id("hermit", "cli", "module::case", "test", None),
            "hermit::cli$module::case"
        );
    }

    #[test]
    fn same_named_lib_and_bin_suites_keep_distinct_typed_identities() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"hermit::hermit$shared_case#2\",\"exec_time\":0.1}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"measured\":0,\"filtered_out\":2,\"exec_time\":0.1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"hermit::hermit$shared_case\",\"exec_time\":0.1}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"measured\":0,\"filtered_out\":3,\"exec_time\":0.1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
        );
        let parsed = parse_event_text(events).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert_eq!(parsed.filtered_tests, 5);
        assert_eq!(
            parsed.results,
            vec![
                TestResult::new("hermit$shared_case".into(), true, 2).unwrap(),
                TestResult::new("hermit::hermit$shared_case".into(), true, 1).unwrap(),
            ]
        );
        let map = BinaryMap {
            schema: nextest_cpu::BINARY_MAP_SCHEMA,
            entries: vec![
                BinaryMapEntry {
                    executable: "/tmp/hermit-lib".into(),
                    package: "hermit".into(),
                    binary: "hermit".into(),
                    binary_name: "hermit".into(),
                    kind: "lib".into(),
                },
                BinaryMapEntry {
                    executable: "/tmp/hermit-bin".into(),
                    package: "hermit".into(),
                    binary: "hermit::bin/hermit".into(),
                    binary_name: "hermit".into(),
                    kind: "bin".into(),
                },
            ],
        };
        let scratch = Scratch::new();
        for record in [
            identified_attempt(
                "hermit",
                "hermit",
                "shared_case",
                1,
                AttemptCompletion::Exit { code: 23 },
            ),
            identified_attempt(
                "hermit",
                "hermit",
                "shared_case",
                2,
                AttemptCompletion::Exit { code: 0 },
            ),
            identified_attempt(
                "hermit",
                "hermit::bin/hermit",
                "shared_case",
                1,
                AttemptCompletion::Exit { code: 0 },
            ),
        ] {
            write_record(&scratch.0, &record);
        }
        let report = reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &map).unwrap().0;
        assert_eq!(report.attempts.len(), 3);
    }

    #[test]
    fn one_suite_changing_kind_is_still_refused() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":0,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":0,\"failed\":0,\"ignored\":0,\"measured\":0,\"filtered_out\":0,\"exec_time\":0.0,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
        );
        let error = parse_event_text(events).unwrap_err();
        assert!(
            error.contains("changed kind from \"lib\" to \"bin\""),
            "{error}"
        );
    }

    #[test]
    fn distinct_suites_may_interleave_and_ignore_additive_fields() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"future\":true,\"nextest\":{\"crate\":\"alpha\",\"test_binary\":\"alpha\",\"kind\":\"lib\",\"future\":\"ok\"}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"nextest\":{\"crate\":\"beta\",\"test_binary\":\"tool\",\"kind\":\"bin\"}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"alpha::alpha$case\"}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"beta::tool$case\"}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"filtered_out\":0,\"nextest\":{\"crate\":\"beta\",\"test_binary\":\"tool\",\"kind\":\"bin\"}}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"filtered_out\":0,\"nextest\":{\"crate\":\"alpha\",\"test_binary\":\"alpha\",\"kind\":\"lib\"}}\n",
        );
        assert_eq!(
            parse_event_text(events).unwrap().results,
            vec![
                TestResult::new("alpha$case".into(), true, 1).unwrap(),
                TestResult::new("beta::tool$case".into(), true, 1).unwrap(),
            ]
        );
    }

    #[test]
    fn lib_stress_suites_keep_distinct_ids() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":1}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"pkg::pkg@stress-1$case\"}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"filtered_out\":0,\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":1}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":2}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"pkg::pkg@stress-2$case\"}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"filtered_out\":0,\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":2}}\n",
        );
        assert_eq!(
            parse_event_text(events).unwrap().results,
            vec![
                TestResult::new("pkg@stress-1$case".into(), true, 1).unwrap(),
                TestResult::new("pkg@stress-2$case".into(), true, 1).unwrap(),
            ]
        );
    }

    #[test]
    fn ambiguous_wrong_and_incomplete_suite_streams_refuse() {
        let ambiguous = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":0,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":0,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
        );
        assert!(
            parse_event_text(ambiguous)
                .unwrap_err()
                .contains("ambiguous")
        );

        let wrong_row = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":0,\"nextest\":{\"crate\":\"alpha\",\"test_binary\":\"alpha\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"test\",\"event\":\"failed\",\"name\":\"beta::beta$case\"}\n",
        );
        assert!(
            parse_event_text(wrong_row)
                .unwrap_err()
                .contains("without one unambiguous open typed suite")
        );

        let end_without_start = "{\"type\":\"suite\",\"event\":\"ok\",\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\"}}\n";
        assert!(
            parse_event_text(end_without_start)
                .unwrap_err()
                .contains("ended before its typed start metadata")
        );

        let unterminated = "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":0,\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\"}}\n";
        assert!(
            parse_event_text(unterminated)
                .unwrap_err()
                .contains("before open suites published terminal events")
        );
    }

    #[test]
    fn aggregate_counts_come_from_typed_suite_and_test_events() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"suite::suite$passes"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"suite::suite$passes","exec_time":0.1}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"suite::suite$recovers"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"suite::suite$recovers#2","exec_time":0.2}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":7,"exec_time":0.3,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
        );
        let parsed = parse_event_text(events).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert_eq!(parsed.filtered_tests, 7);
        assert_eq!(parsed.results.len(), 2);

        let mutated = events.replace("\"filtered_out\":7", "\"filtered_out\":11");
        let parsed = parse_event_text(&mutated).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert_eq!(parsed.filtered_tests, 11);

        let extra_test = events
            .replace("\"test_count\":2", "\"test_count\":3")
            .replace("\"passed\":2", "\"passed\":3")
            .replace(
                r#"{"type":"suite","event":"ok""#,
                concat!(
                    r#"{"type":"test","event":"started","name":"suite::suite$third"}"#,
                    "\n",
                    r#"{"type":"test","event":"ok","name":"suite::suite$third","exec_time":0.1}"#,
                    "\n",
                    r#"{"type":"suite","event":"ok""#,
                ),
            );
        let parsed = parse_event_text(&extra_test).unwrap();
        assert_eq!(parsed.executed_tests, 3);
        assert_eq!(parsed.filtered_tests, 7);
    }

    #[test]
    fn repeated_suite_recaps_do_not_multiply_filtered_count() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":214,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"hermit::rr_suite$ignored_one"}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":213,"measured":0,"filtered_out":0,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
            "\n",
            r#"{"type":"suite","event":"started","test_count":214,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"hermit::rr_suite$runs"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::rr_suite$runs","exec_time":0.1}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":1,"failed":0,"ignored":213,"measured":0,"filtered_out":0,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
        );
        let parsed = parse_event_text(events).unwrap();
        assert_eq!(parsed.executed_tests, 1);
        assert_eq!(parsed.filtered_tests, 213);
    }

    #[test]
    fn all_ignored_wrapping_filtered_count_keeps_the_typed_ignored_count() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":3,"nextest":{"crate":"hermit","test_binary":"analyze","kind":"test"}}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":3,"measured":0,"filtered_out":18446744073709551613,"nextest":{"crate":"hermit","test_binary":"analyze","kind":"test"}}"#,
        );
        let parsed = parse_event_text(events).unwrap();
        assert_eq!(parsed.executed_tests, 0);
        assert_eq!(parsed.filtered_tests, 3);
    }

    #[test]
    fn a_filtered_ordinary_test_beside_selected_ignored_ones_keeps_the_typed_count() {
        // Captured from nextest 0.9.100 in test.pmu_integration_cases.
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"hermit","test_binary":"leveldb","kind":"test"}}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":2,"measured":0,"filtered_out":18446744073709551615,"exec_time":0,"nextest":{"crate":"hermit","test_binary":"leveldb","kind":"test"}}"#,
        );
        let parsed = parse_event_text(events).unwrap();
        assert_eq!(parsed.executed_tests, 0);
        assert_eq!(parsed.filtered_tests, 2);
    }

    #[test]
    fn selected_ignored_tests_use_the_more_complete_typed_suite_record() {
        // Captured from nextest 0.9.100 for `--ignored` with six selected
        // tests in a fifteen-test binary. The first recap reports no executed
        // tests; the second reports all six and represents `0 - 6` in the
        // unsigned filtered_out field. The exact skipped population is nine.
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":15,"nextest":{"crate":"hermit","test_binary":"app_strict_verify","kind":"test"}}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"hermit::app_strict_verify$one"}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":15,"measured":0,"filtered_out":0,"nextest":{"crate":"hermit","test_binary":"app_strict_verify","kind":"test"}}"#,
            "\n",
            r#"{"type":"suite","event":"started","test_count":15,"nextest":{"crate":"hermit","test_binary":"app_strict_verify","kind":"test"}}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::app_strict_verify$one"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::app_strict_verify$two"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::app_strict_verify$three"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::app_strict_verify$four"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::app_strict_verify$five"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::app_strict_verify$six"}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":6,"failed":0,"ignored":15,"measured":0,"filtered_out":18446744073709551610,"nextest":{"crate":"hermit","test_binary":"app_strict_verify","kind":"test"}}"#,
        );
        let parsed = parse_event_text(events).unwrap();
        assert_eq!(parsed.executed_tests, 6);
        assert_eq!(parsed.filtered_tests, 9);

        let malformed = events.replace(
            "\"filtered_out\":18446744073709551610",
            "\"filtered_out\":18446744073709551611",
        );
        assert!(
            parse_event_text(&malformed)
                .unwrap_err()
                .contains("ignored + filtered_out overflows u64")
        );
    }

    #[test]
    fn missing_typed_count_fails_by_field_name() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":1,"measured":0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
        );
        assert!(
            parse_event_text(events)
                .unwrap_err()
                .contains("filtered_out must be an unsigned integer")
        );

        let disagreeing = events
            .replace("\"passed\":0", "\"passed\":1")
            .replace("\"measured\":0,", "\"measured\":0,\"filtered_out\":0,");
        assert!(parse_event_text(&disagreeing)
            .unwrap_err()
            .contains(
                "typed suite records report 1 executed test(s), but typed terminal test records report 0"
            ));
    }

    #[test]
    fn historical_stress_events_remain_distinct_and_refuse_unscoped_cpu_records() {
        let events = sample_events()
            .replace("\"kind\":\"lib\"", "\"kind\":\"lib\",\"stress_index\":1")
            .replace("suite::suite$", "suite::suite@stress-1$");
        let parsed = parse_event_text(&events).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert!(
            parsed
                .results
                .iter()
                .all(|row| row.id.contains("@stress-1"))
        );
        let scratch = Scratch::new();
        let error = reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &sample_binary_map()).unwrap_err();
        assert!(
            error.contains("does not support stress_index identities"),
            "{error}"
        );
    }

    #[test]
    fn retry_suffix_does_not_allocate_attempts_without_matching_records() {
        let events = sample_events().replace("recovers#2", "recovers#18446744073709551615");
        let parsed = parse_event_text(&events).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert_eq!(
            parsed
                .results
                .iter()
                .find(|r| r.id.ends_with("$recovers"))
                .unwrap()
                .attempts,
            u64::MAX
        );
        assert_eq!(parsed.expected_attempts.len(), 2);
        let scratch = Scratch::new();
        let error = reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &sample_binary_map()).unwrap_err();
        assert!(error.contains("retry population overflows"), "{error}");
        let parsed =
            parse_event_text(&sample_events().replace("recovers#2", "recovers#999999999")).unwrap();
        let error = reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &sample_binary_map()).unwrap_err();
        assert!(
            error.contains("require 1000000000 attempt records, supplied 0"),
            "{error}"
        );
    }

    #[test]
    fn cpu_attempts_reconcile_with_typed_retries() {
        let scratch = Scratch::new();
        write_record(
            &scratch.0,
            &attempt("passes", 1, AttemptCompletion::Exit { code: 0 }),
        );
        write_record(
            &scratch.0,
            &attempt("recovers", 1, AttemptCompletion::Exit { code: 23 }),
        );
        write_record(
            &scratch.0,
            &attempt("recovers", 2, AttemptCompletion::Exit { code: 0 }),
        );
        let report = reconcile_cpu_attempts_with_cancelled(
            &parse_event_text(sample_events()).unwrap(),
            &scratch.0,
            &sample_binary_map(),
        )
    .unwrap()
        .0;
        assert_eq!(report.run_id.as_deref(), Some("sample-run"));
        assert_eq!(report.attempts.len(), 3);
    }

    #[test]
    fn cpu_timeout_attempts_reconcile_with_typed_retries() {
        let scratch = Scratch::new();
        write_record(
            &scratch.0,
            &attempt("passes", 1, AttemptCompletion::Exit { code: 0 }),
        );
        write_record(
            &scratch.0,
            &attempt(
                "recovers",
                1,
                AttemptCompletion::CpuTimeout {
                    cpu_budget_usec: 10_000,
                    observed_cpu_usec: 12_345,
                },
            ),
        );
        write_record(
            &scratch.0,
            &attempt("recovers", 2, AttemptCompletion::Exit { code: 0 }),
        );
        let report = reconcile_cpu_attempts_with_cancelled(
            &parse_event_text(sample_events()).unwrap(),
            &scratch.0,
            &sample_binary_map(),
        )
    .unwrap()
        .0;
        assert_eq!(report.run_id.as_deref(), Some("sample-run"));
        assert_eq!(report.attempts.len(), 3);
    }

    #[test]
    fn cpu_reconciliation_refuses_missing_and_unexpected_attempts() {
        let missing = Scratch::new();
        write_record(
            &missing.0,
            &attempt("passes", 1, AttemptCompletion::Exit { code: 0 }),
        );
        let error = reconcile_cpu_attempts_with_cancelled(
            &parse_event_text(sample_events()).unwrap(),
            &missing.0,
            &sample_binary_map(),
        )
        .unwrap_err();
        assert!(error.contains("missing="), "{error}");

        let unexpected = Scratch::new();
        for record in [
            attempt("passes", 1, AttemptCompletion::Exit { code: 0 }),
            attempt("recovers", 1, AttemptCompletion::Exit { code: 23 }),
            attempt("recovers", 2, AttemptCompletion::Exit { code: 0 }),
            attempt("other", 1, AttemptCompletion::Exit { code: 0 }),
        ] {
            write_record(&unexpected.0, &record);
        }
        let error = reconcile_cpu_attempts_with_cancelled(
            &parse_event_text(sample_events()).unwrap(),
            &unexpected.0,
            &sample_binary_map(),
        )
        .unwrap_err();
        assert!(error.contains("unexpected="), "{error}");
    }

    /// dh-825jy, the bb8e4570 shape: a fail-fast cancel leaves two
    /// in-flight attempts with signal CPU records and no typed attempts.
    /// They reconcile as cancelled - the real failure stays named, the
    /// cancelled attempts are kept and never passed - while an unexpected
    /// record that EXITED still refuses.
    #[test]
    fn fail_fast_cancelled_attempts_reconcile_as_cancelled() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":3,\"nextest\":{\"crate\":\"suite\",\"test_binary\":\"suite\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"suite::suite$passes\"}\n",
            "{\"type\":\"test\",\"event\":\"failed\",\"name\":\"suite::suite$fails\"}\n",
            "{\"type\":\"suite\",\"event\":\"failed\",\"passed\":1,\"failed\":1,\"ignored\":0,\"measured\":0,\"filtered_out\":0,\"nextest\":{\"crate\":\"suite\",\"test_binary\":\"suite\",\"kind\":\"lib\"}}\n",
        );
        let parsed = parse_event_text(events).unwrap();
        assert_eq!(parsed.results.len(), 2);
        assert!(!parsed.results.iter().find(|row| row.id.ends_with("$fails")).unwrap().passed);

        let scratch = Scratch::new();
        for record in [
            attempt("passes", 1, AttemptCompletion::Exit { code: 0 }),
            attempt("fails", 1, AttemptCompletion::Exit { code: 23 }),
            // In flight at the cancel: the failed test's retry and a
            // test nextest never reported at all.
            attempt("fails", 2, AttemptCompletion::SupervisorSignal { signal: 9 }),
            attempt("never_reported", 1, AttemptCompletion::Signal { signal: 15 }),
        ] {
            write_record(&scratch.0, &record);
        }
        let (report, cancelled) =
            reconcile_cpu_attempts_with_cancelled(&parsed, &scratch.0, &sample_binary_map())
                .unwrap();
        assert_eq!(report.attempts.len(), 4);
        assert_eq!(cancelled.len(), 2);
        assert!(cancelled.iter().all(|identity| identity.attempt >= 1));
        assert!(
            report.attempts.iter().all(|record| !matches!(
                record.completion,
                AttemptCompletion::Signal { .. } | AttemptCompletion::SupervisorSignal { .. }
            ) || !record.completion.is_success()),
            "a cancelled attempt was recorded as a success"
        );

        // The schema-5 row for the real failure gains the cancelled
        // attempt, in sequence, as `cancelled` - never `passed`.
        let rows = vec![TestResult::new("suite$fails".into(), false, 1).unwrap()];
        let rows = apply_skip_markers(rows, &[]).unwrap();
        let rows = apply_cancelled_attempts(rows, &cancelled, Some(&sample_binary_map())).unwrap();
        let row = &rows[0];
        assert!(!row.passed);
        assert_eq!(row.attempts, 2);
        let attempts = row.attempt_results.as_ref().unwrap();
        assert_eq!(attempts[0].outcome, TestAttemptOutcome::Failed);
        assert_eq!(attempts[1].outcome, TestAttemptOutcome::Cancelled);

        // An unexpected record that EXITED is still a disagreement.
        let exited = Scratch::new();
        for record in [
            attempt("passes", 1, AttemptCompletion::Exit { code: 0 }),
            attempt("fails", 1, AttemptCompletion::Exit { code: 23 }),
            attempt("fails", 2, AttemptCompletion::Exit { code: 1 }),
        ] {
            write_record(&exited.0, &record);
        }
        let error = reconcile_cpu_attempts_with_cancelled(
            &parsed,
            &exited.0,
            &sample_binary_map(),
        )
        .unwrap_err();
        assert!(error.contains("unexpected="), "{error}");
    }

    #[test]
    fn cpu_record_reader_refuses_duplicate_substituted_and_malformed_rows() {
        let duplicate = Scratch::new();
        let record = attempt("passes", 1, AttemptCompletion::Exit { code: 0 });
        let original = write_attempt_atomic(&duplicate.0, &record).unwrap();
        fs::copy(&original, duplicate.0.join("zzzz-duplicate.json")).unwrap();
        let error = read_attempt_records(&duplicate.0).unwrap_err();
        assert!(error.contains("duplicate attempt record"), "{error}");

        let substituted = Scratch::new();
        fs::write(
            substituted.0.join("wrong-name.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let error = read_attempt_records(&substituted.0).unwrap_err();
        assert!(error.contains("is substituted"), "{error}");

        let malformed = Scratch::new();
        fs::write(malformed.0.join("malformed.json"), b"not JSON\n").unwrap();
        let error = read_attempt_records(&malformed.0).unwrap_err();
        assert!(error.contains("malformed attempt record"), "{error}");
    }

    #[test]
    fn cpu_reconciliation_refuses_false_pass() {
        let false_pass = Scratch::new();
        write_record(
            &false_pass.0,
            &attempt("passes", 1, AttemptCompletion::Exit { code: 23 }),
        );
        write_record(
            &false_pass.0,
            &attempt("recovers", 1, AttemptCompletion::Exit { code: 23 }),
        );
        write_record(
            &false_pass.0,
            &attempt("recovers", 2, AttemptCompletion::Exit { code: 0 }),
        );
        let error = reconcile_cpu_attempts_with_cancelled(
            &parse_event_text(sample_events()).unwrap(),
            &false_pass.0,
            &sample_binary_map(),
        )
        .unwrap_err();
        assert!(error.contains("disagree on whether"), "{error}");

        let false_cpu_timeout = Scratch::new();
        write_record(
            &false_cpu_timeout.0,
            &attempt(
                "passes",
                1,
                AttemptCompletion::CpuTimeout {
                    cpu_budget_usec: 10_000,
                    observed_cpu_usec: 12_345,
                },
            ),
        );
        write_record(
            &false_cpu_timeout.0,
            &attempt("recovers", 1, AttemptCompletion::Exit { code: 23 }),
        );
        write_record(
            &false_cpu_timeout.0,
            &attempt("recovers", 2, AttemptCompletion::Exit { code: 0 }),
        );
        let error = reconcile_cpu_attempts_with_cancelled(
            &parse_event_text(sample_events()).unwrap(),
            &false_cpu_timeout.0,
            &sample_binary_map(),
        )
        .unwrap_err();
        assert!(error.contains("disagree on whether"), "{error}");

        let false_failure = Scratch::new();
        write_record(
            &false_failure.0,
            &attempt("passes", 1, AttemptCompletion::Exit { code: 0 }),
        );
        write_record(
            &false_failure.0,
            &attempt("recovers", 1, AttemptCompletion::Exit { code: 0 }),
        );
        write_record(
            &false_failure.0,
            &attempt("recovers", 2, AttemptCompletion::Exit { code: 0 }),
        );
        let error = reconcile_cpu_attempts_with_cancelled(
            &parse_event_text(sample_events()).unwrap(),
            &false_failure.0,
            &sample_binary_map(),
        )
        .unwrap_err();
        assert!(error.contains("disagree on whether"), "{error}");
    }

    #[test]
    fn cpu_timeout_records_require_an_exact_valid_boundary() {
        let mut zero_budget = attempt(
            "timeout",
            1,
            AttemptCompletion::CpuTimeout {
                cpu_budget_usec: 0,
                observed_cpu_usec: 12_345,
            },
        );
        let error = zero_budget.validate().unwrap_err();
        assert!(
            error.contains("budget must be greater than zero"),
            "{error}"
        );

        zero_budget.completion = AttemptCompletion::CpuTimeout {
            cpu_budget_usec: 12_000,
            observed_cpu_usec: 11_999,
        };
        let error = zero_budget.validate().unwrap_err();
        assert!(error.contains("below its budget 12000us"), "{error}");

        zero_budget.completion = AttemptCompletion::CpuTimeout {
            cpu_budget_usec: 10_000,
            observed_cpu_usec: 12_346,
        };
        let error = zero_budget.validate().unwrap_err();
        assert!(error.contains("CPU total 12345us is below"), "{error}");

        let valid = attempt(
            "timeout",
            1,
            AttemptCompletion::CpuTimeout {
                cpu_budget_usec: 10_000,
                observed_cpu_usec: 12_345,
            },
        );
        valid.validate().unwrap();
        let mut wrong_source = valid.clone();
        wrong_source.cpu_source = nextest_cpu::CPU_SOURCE_REAPED;
        let error = wrong_source.validate().unwrap_err();
        assert!(error.contains("requires cpu_source"), "{error}");
        assert_eq!(
            serde_json::to_value(valid.completion).unwrap(),
            serde_json::json!({
                "kind": "cpu_timeout",
                "cpu_budget_usec": 10_000,
                "observed_cpu_usec": 12_345,
            })
        );
    }

    #[test]
    fn listed_count_is_the_will_run_inventory() {
        // Selected tests count by filter-match alone. This is the population
        // a successful run must execute exactly (executed == listed).
        let inventory = br#"{
            "rust-suites": {
                "suite": {
                    "package-name": "suite",
                    "binary-id": "suite",
                    "binary-name": "suite",
                    "kind": "lib",
                    "binary-path": "/tmp/suite-binary",
                    "testcases": {
                        "runs": {"ignored": false, "filter-match": {"status": "matches"}},
                        "also_runs": {"ignored": false, "filter-match": {"status": "matches"}},
                        "ignored_one": {"ignored": true, "filter-match": {"status": "matches"}},
                        "filtered_out": {"ignored": false, "filter-match": {"status": "mismatch"}}
                    }
                }
            }
        }"#;
        // `matches` alone is the will-run population: the ignored testcase
        // marked `matches` counts (the run's arguments run it), the
        // mismatched one does not.
        assert_eq!(listed_count(inventory).unwrap(), 3);
        let unknown = String::from_utf8(inventory.to_vec())
            .unwrap()
            .replace("\"matches\"", "\"surprise\"");
        assert!(
            listed_count(unknown.as_bytes())
                .unwrap_err()
                .contains("unknown Nextest filter status")
        );
    }

    #[test]
    fn listed_identities_are_the_sorted_census_of_the_same_population() {
        // The census gate diffs these lists base vs candidate: identities
        // are package/binary/test, sorted, exactly the `matches` population
        // listed_count counts (a mismatched testcase is not listed here).
        let inventory = br#"{
            "rust-suites": {
                "suite": {
                    "package-name": "suite",
                    "binary-id": "suite",
                    "binary-name": "suite",
                    "kind": "lib",
                    "binary-path": "/tmp/suite-binary",
                    "testcases": {
                        "runs": {"ignored": false, "filter-match": {"status": "matches"}},
                        "also_runs": {"ignored": false, "filter-match": {"status": "matches"}},
                        "ignored_one": {"ignored": true, "filter-match": {"status": "matches"}},
                        "filtered_out": {"ignored": false, "filter-match": {"status": "mismatch"}}
                    }
                }
            }
        }"#;
        assert_eq!(
            listed_identities(inventory).unwrap(),
            [
                "suite/suite/also_runs".to_string(),
                "suite/suite/ignored_one".into(),
                "suite/suite/runs".into(),
            ]
        );
        assert_eq!(
            listed_identities(inventory).unwrap().len() as u64,
            listed_count(inventory).unwrap()
        );
    }

    #[test]
    fn binary_map_refuses_ambiguous_and_substituted_paths() {
        let inventory = br#"{
            "rust-suites": {
                "suite": {
                    "package-name": "suite",
                    "binary-id": "suite",
                    "binary-name": "suite",
                    "kind": "lib",
                    "binary-path": "/tmp/shared-binary"
                },
                "suite::bin/suite": {
                    "package-name": "suite",
                    "binary-id": "suite::bin/suite",
                    "binary-name": "suite",
                    "kind": "bin",
                    "binary-path": "/tmp/shared-binary"
                }
            }
        }"#;
        let error = BinaryMap::from_nextest_inventory(inventory).unwrap_err();
        assert!(error.contains("executable path"), "{error}");
        assert!(error.contains("is ambiguous"), "{error}");

        let map = sample_binary_map();
        let error = map
            .identity_for_executable(Path::new("/tmp/substituted-binary"))
            .unwrap_err();
        assert!(error.contains("absent from the typed inventory"), "{error}");
    }
}
