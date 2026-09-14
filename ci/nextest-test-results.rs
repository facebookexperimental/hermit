#!/usr/bin/env -S rust-script --force
//! Convert nextest's versioned libtest JSON into dagrun's shared test-result file.
//!
//! ```cargo
//! [dependencies]
//! dagrun = { path = "../agent-utils/rs/dagrun" }
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! sha2 = "0.10"
//! ```

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use dagrun::TestResult;
use dagrun::TestResults;
use nextest_cpu::{
    read_attempt_records, read_binary_map, write_binary_map_atomic, write_report_atomic,
    AttemptIdentity, AttemptRecord, BinaryMap, CpuReport,
};
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
        "usage: nextest-test-results.rs EVENTS_JSONL NEXTEST_STATUS OUTPUT_OR_DASH [ATTEMPT_RECORD_DIR BINARY_MAP CPU_REPORT_OR_DASH]\n       nextest-test-results.rs --write-binary-map NEXTEST_INVENTORY_JSON OUTPUT"
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
    let mut results = BTreeMap::new();
    let mut expected_attempts = Vec::new();
    // Test rows carry package/binary but not target kind. Keep every currently
    // open typed suite by that rendered key so interleaved distinct suites stay
    // attributable. Two open suites with the same key are ambiguous and must
    // refuse: no field on their test rows could distinguish them.
    let mut open_suites = BTreeMap::<(String, String), OpenSuite>::new();
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
                };
                if let Some(active) = open_suites.insert(key.clone(), open) {
                    return Err(format!(
                        "nextest-test-results line {line_number}: suite key {key:?} is ambiguous because {suite:?} started before {active:?} ended"
                    ));
                }
            }
            ("suite", "ok" | "failed") => {
                let ended = SuiteIdentity::from_event(&value, line_number)?;
                let key = ended.test_name_key();
                let started = open_suites.remove(&key).ok_or_else(|| {
                    format!(
                        "nextest-test-results line {line_number}: suite {ended:?} ended before its typed start metadata"
                    )
                })?;
                if started.identity != ended {
                    if started.identity.package == ended.package
                        && started.identity.binary == ended.binary
                        && started.identity.kind != ended.kind
                    {
                        return Err(format!(
                            "nextest-test-results line {line_number}: suite ({:?}, {:?}) changed kind from {:?} to {:?}",
                            started.identity.package,
                            started.identity.binary,
                            started.identity.kind,
                            ended.kind
                        ));
                    }
                    return Err(format!(
                        "nextest-test-results line {line_number}: suite ended as {ended:?} after starting as {:?}",
                        started.identity
                    ));
                }
                let passed = required_u64(&value, "passed", line_number)?;
                let failed = required_u64(&value, "failed", line_number)?;
                suite_passed = suite_passed.checked_add(passed).ok_or_else(|| {
                    "nextest-test-results: passed count overflows u64".to_string()
                })?;
                suite_failed = suite_failed.checked_add(failed).ok_or_else(|| {
                    "nextest-test-results: failed count overflows u64".to_string()
                })?;
                let ignored = required_u64(&value, "ignored", line_number)?;
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
                let suite = open_suites.get(&key).ok_or_else(|| {
                    format!(
                        "nextest-test-results line {line_number}: test names suite {key:?} without one unambiguous open typed suite"
                    )
                })?;
                if matches!(event, "started" | "ignored") {
                    continue;
                }
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

fn reconcile_cpu_attempts(
    parsed: &ParsedEvents,
    directory: &Path,
    binary_map: &BinaryMap,
) -> Result<CpuReport, String> {
    for attempt in &parsed.expected_attempts {
        // The declared Nextest version has no stress-run wrapper identity.
        // Historical count-only parsing still preserves stress_index; never
        // alias such events onto an ordinary measured attempt.
        if attempt.suite.stress_index.is_some() {
            return Err("CPU reconciliation does not support stress_index identities".into());
        }
    }
    let records = read_attempt_records(directory)?;
    let required = parsed.expected_attempts.iter().try_fold(0u64, |total, test| total.checked_add(test.attempts))
        .ok_or("missing=typed retry population overflows the available attempt count")?;
    if required > records.len() as u64 {
        return Err(format!("missing=typed events require {required} attempt records, supplied {}", records.len()));
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
            if expected.insert(identity.clone(), attempt.passed && number == attempt.attempts).is_some() {
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
    Ok(CpuReport::new(run_id, actual.into_values().collect()))
}

fn parse_events(path: &Path) -> Result<ParsedEvents, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("nextest-test-results-read {}: {error}", path.display()))?;
    parse_event_text(&text)
}

fn parse_u64(value: String, name: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|error| format!("nextest-test-results {name}: {error}"))
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(first) = args.next() else {
        usage();
        return Err("nextest-test-results requires three arguments".into());
    };
    if first == "--help" || first == "-h" {
        usage();
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
        return write_binary_map_atomic(Path::new(&output), &map);
    }
    let events = first;
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
    let parsed = parse_events(Path::new(&events))?;
    let passed = u64::try_from(parsed.results.iter().filter(|result| result.passed).count())
        .map_err(|_| "nextest-test-results: passed result count does not fit u64".to_string())?;
    let failed = parsed
        .executed_tests
        .checked_sub(passed)
        .ok_or_else(|| "nextest-test-results: passed count exceeds executed count".to_string())?;
    if status == 0 && failed != 0 {
        return Err(format!(
            "nextest-test-results: successful nextest status disagrees with {failed} failed typed result(s)"
        ));
    }
    let cpu_report = match (attempt_records, binary_map, cpu_report) {
        (Some(records), Some(binary_map), Some(output)) => {
            let binary_map = read_binary_map(Path::new(&binary_map))?;
            let report = reconcile_cpu_attempts(&parsed, Path::new(&records), &binary_map)?;
            Some((report, output))
        }
        (None, None, None) => None,
        _ => unreachable!("argument pairing was checked above"),
    };
    let report =
        TestResults::current(parsed.executed_tests, parsed.filtered_tests, parsed.results)?;
    if let Some(expected) = match env::var("NEXTEST_EXPECTED_EXECUTED") {
        Ok(value) => Some(parse_u64(value, "NEXTEST_EXPECTED_EXECUTED")?),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err("nextest-test-results NEXTEST_EXPECTED_EXECUTED is not UTF-8".into());
        }
    } {
        if report.executed_tests != expected {
            return Err(format!(
                "nextest-test-results: expected {expected} tests to execute, saw {}; refusing because the selected set changed",
                report.executed_tests
            ));
        }
    }
    if output != "-" {
        report.write_current(Path::new(&output))?;
    }
    if let Some((cpu_report, output)) = cpu_report {
        if output != "-" {
            write_report_atomic(Path::new(&output), &cpu_report)?;
        }
    }
    println!("running {} tests", report.executed_tests);
    if status == 0 {
        println!(
            "test result: ok. {passed} passed; 0 failed; 0 ignored; {} filtered out",
            report.filtered_tests
        );
    } else {
        println!(
            "test result: FAILED. {passed} passed; {failed} failed; 0 ignored; {} filtered out",
            report.filtered_tests
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nextest-test-results: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nextest_cpu::{write_attempt_atomic, AttemptCompletion, BinaryMapEntry};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

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
        let report = reconcile_cpu_attempts(&parsed, &scratch.0, &map).unwrap();
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
        let events = sample_events().replace("\"kind\":\"lib\"", "\"kind\":\"lib\",\"stress_index\":1").replace("suite::suite$", "suite::suite@stress-1$");
        let parsed = parse_event_text(&events).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert!(parsed.results.iter().all(|row| row.id.contains("@stress-1")));
        let scratch = Scratch::new();
        let error = reconcile_cpu_attempts(&parsed, &scratch.0, &sample_binary_map()).unwrap_err();
        assert!(error.contains("does not support stress_index identities"), "{error}");
    }

    #[test]
    fn retry_suffix_does_not_allocate_attempts_without_matching_records() {
        let events = sample_events().replace("recovers#2", "recovers#18446744073709551615");
        let parsed = parse_event_text(&events).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert_eq!(parsed.results.iter().find(|r| r.id.ends_with("$recovers")).unwrap().attempts, u64::MAX);
        assert_eq!(parsed.expected_attempts.len(), 2);
        let scratch = Scratch::new();
        let error = reconcile_cpu_attempts(&parsed, &scratch.0, &sample_binary_map()).unwrap_err();
        assert!(error.contains("retry population overflows"), "{error}");
        let parsed = parse_event_text(&sample_events().replace("recovers#2", "recovers#999999999")).unwrap();
        let error = reconcile_cpu_attempts(&parsed, &scratch.0, &sample_binary_map()).unwrap_err();
        assert!(error.contains("require 1000000000 attempt records, supplied 0"), "{error}");
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
        let report = reconcile_cpu_attempts(
            &parse_event_text(sample_events()).unwrap(),
            &scratch.0,
            &sample_binary_map(),
        )
        .unwrap();
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
        let report = reconcile_cpu_attempts(
            &parse_event_text(sample_events()).unwrap(),
            &scratch.0,
            &sample_binary_map(),
        )
        .unwrap();
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
        let error = reconcile_cpu_attempts(
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
        let error = reconcile_cpu_attempts(
            &parse_event_text(sample_events()).unwrap(),
            &unexpected.0,
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
        let error = reconcile_cpu_attempts(
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
        let error = reconcile_cpu_attempts(
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
        let error = reconcile_cpu_attempts(
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
        assert!(error.contains("budget must be greater than zero"), "{error}");

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
