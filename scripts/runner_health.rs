#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Report the health of a repository's registered self-hosted GitHub runners.
//!
//! Run locally through the forward proxy:
//!
//! ```text
//! with-proxy ./scripts/runner_health.rs
//! ```
//!
//! GitHub reports whether a runner is online, but not when it went offline.
//! This script persists the first observation of an offline runner so repeated
//! checks can alert only after the configured grace period has really elapsed.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const DEFAULT_REPOSITORY: &str = "rrnewton/hermit";
const DEFAULT_OFFLINE_THRESHOLD_SECONDS: u64 = 60 * 60;
const DEFAULT_RUN_SCAN_LIMIT: usize = 20;
const DEFAULT_STATE_FILE: &str = "target/runner-health/state.tsv";
const DEFAULT_WORKFLOWS: &[&str] = &["ci-selfhosted.yml", "validation-levels.yml", "ci-dag.yml"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Runner {
    id: u64,
    name: String,
    os: String,
    status: String,
    busy: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RunnerState {
    first_seen_offline: Option<u64>,
    last_success_epoch: Option<u64>,
    last_success_at: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkflowRun {
    id: u64,
    name: String,
    conclusion: String,
    updated_at: String,
    html_url: String,
}

#[derive(Debug, Clone)]
struct SuccessfulJob {
    runner_name: String,
    completed_epoch: u64,
    completed_at: String,
}

#[derive(Debug)]
struct Options {
    repository: String,
    offline_threshold_seconds: u64,
    run_scan_limit: usize,
    state_file: PathBuf,
    workflows: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Evaluation {
    health: &'static str,
    offline_for: Option<u64>,
    alert: bool,
}

fn usage() -> &'static str {
    "Usage: runner_health.rs [OPTIONS]\n\
\n\
Options:\n\
  --repo OWNER/REPO                 Repository to inspect (default: rrnewton/hermit)\n\
  --offline-threshold-seconds N     Alert threshold (default: 3600)\n\
  --run-scan-limit N                Completed runs scanned per workflow (default: 20)\n\
  --state-file PATH                 Persistent state file\n\
                                      (default: target/runner-health/state.tsv)\n\
  --workflow FILE_OR_ID             Self-hosted workflow to scan; repeatable\n\
                                      (defaults: ci-selfhosted.yml, validation-levels.yml, ci-dag.yml)\n\
  -h, --help                        Show this help\n\
\n\
Exit status: 0 healthy/grace period, 1 health alert, 2 operational error."
}

fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut repository = DEFAULT_REPOSITORY.to_owned();
    let mut offline_threshold_seconds = DEFAULT_OFFLINE_THRESHOLD_SECONDS;
    let mut run_scan_limit = DEFAULT_RUN_SCAN_LIMIT;
    let mut state_file = PathBuf::from(DEFAULT_STATE_FILE);
    let mut workflows = Vec::new();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| {
            iter.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--repo" => repository = value("--repo")?,
            "--offline-threshold-seconds" => {
                offline_threshold_seconds = value("--offline-threshold-seconds")?
                    .parse()
                    .map_err(|_| "--offline-threshold-seconds must be an integer".to_owned())?;
            }
            "--run-scan-limit" => {
                run_scan_limit = value("--run-scan-limit")?
                    .parse()
                    .map_err(|_| "--run-scan-limit must be an integer".to_owned())?;
            }
            "--state-file" => state_file = PathBuf::from(value("--state-file")?),
            "--workflow" => workflows.push(value("--workflow")?),
            _ => return Err(format!("unknown option: {arg}")),
        }
    }

    if repository.split_once('/').is_none() {
        return Err("--repo must use OWNER/REPO syntax".to_owned());
    }
    if offline_threshold_seconds == 0 {
        return Err("--offline-threshold-seconds must be greater than zero".to_owned());
    }
    if !(1..=100).contains(&run_scan_limit) {
        return Err("--run-scan-limit must be between 1 and 100".to_owned());
    }
    if workflows.is_empty() {
        workflows = DEFAULT_WORKFLOWS
            .iter()
            .map(|value| (*value).to_owned())
            .collect();
    }

    Ok(Some(Options {
        repository,
        offline_threshold_seconds,
        run_scan_limit,
        state_file,
        workflows,
    }))
}

fn gh_api(endpoint: &str, jq: &str, paginate: bool) -> Result<String, String> {
    let gh = env::var_os("GH").unwrap_or_else(|| "gh".into());
    let mut command = Command::new(gh);
    command.arg("api");
    if paginate {
        command.arg("--paginate");
    }
    command.arg(endpoint).args(["--jq", jq]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| format!("failed to execute {rendered}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{rendered} failed with {}:\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("gh output was not UTF-8: {error}"))
}

fn parse_runners(input: &str) -> Result<Vec<Runner>, String> {
    let mut runners = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, line) in input.lines().filter(|line| !line.is_empty()).enumerate() {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(format!(
                "runner row {} has {} fields, expected 5",
                index + 1,
                fields.len()
            ));
        }
        let id = fields[0]
            .parse::<u64>()
            .map_err(|_| format!("runner row {} has invalid id", index + 1))?;
        let busy = match fields[4] {
            "true" => true,
            "false" => false,
            other => {
                return Err(format!(
                    "runner row {} has invalid busy value {other:?}",
                    index + 1
                ));
            }
        };
        if !ids.insert(id) {
            return Err(format!("duplicate runner id {id}"));
        }
        runners.push(Runner {
            id,
            name: fields[1].to_owned(),
            os: fields[2].to_owned(),
            status: fields[3].to_owned(),
            busy,
        });
    }
    runners.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(runners)
}

fn parse_workflow_runs(input: &str) -> Result<Vec<WorkflowRun>, String> {
    let mut runs = Vec::new();
    for (index, line) in input.lines().filter(|line| !line.is_empty()).enumerate() {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(format!(
                "workflow run row {} has {} fields, expected 5",
                index + 1,
                fields.len()
            ));
        }
        runs.push(WorkflowRun {
            id: fields[0]
                .parse()
                .map_err(|_| format!("workflow run row {} has invalid id", index + 1))?,
            name: fields[1].to_owned(),
            conclusion: fields[2].to_owned(),
            updated_at: fields[3].to_owned(),
            html_url: fields[4].to_owned(),
        });
    }
    Ok(runs)
}

fn parse_successful_jobs(input: &str) -> Result<Vec<SuccessfulJob>, String> {
    let mut jobs = Vec::new();
    for (index, line) in input.lines().filter(|line| !line.is_empty()).enumerate() {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 2 {
            return Err(format!(
                "job row {} has {} fields, expected 2",
                index + 1,
                fields.len()
            ));
        }
        jobs.push(SuccessfulJob {
            runner_name: fields[0].to_owned(),
            completed_epoch: parse_github_timestamp(fields[1])?,
            completed_at: fields[1].to_owned(),
        });
    }
    Ok(jobs)
}

fn parse_github_timestamp(timestamp: &str) -> Result<u64, String> {
    let body = timestamp
        .strip_suffix('Z')
        .ok_or_else(|| format!("unsupported GitHub timestamp: {timestamp}"))?;
    let (date, time) = body
        .split_once('T')
        .ok_or_else(|| format!("unsupported GitHub timestamp: {timestamp}"))?;
    let date = date
        .split('-')
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("unsupported GitHub timestamp: {timestamp}"))?;
    let time = time
        .split(':')
        .map(|part| part.split('.').next().unwrap_or(part).parse::<i64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("unsupported GitHub timestamp: {timestamp}"))?;
    if date.len() != 3
        || time.len() != 3
        || !(1..=12).contains(&date[1])
        || !(1..=31).contains(&date[2])
        || !(0..=23).contains(&time[0])
        || !(0..=59).contains(&time[1])
        || !(0..=60).contains(&time[2])
    {
        return Err(format!("unsupported GitHub timestamp: {timestamp}"));
    }
    let days = days_from_civil(date[0], date[1], date[2]);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(time[0] * 3_600 + time[1] * 60 + time[2]))
        .ok_or_else(|| format!("timestamp out of range: {timestamp}"))?;
    u64::try_from(seconds).map_err(|_| format!("timestamp predates Unix epoch: {timestamp}"))
}

// Howard Hinnant's civil-date conversion, offset to the Unix epoch.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn load_state(path: &Path) -> Result<BTreeMap<u64, RunnerState>, String> {
    let input = match fs::read_to_string(path) {
        Ok(input) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let mut states = BTreeMap::new();
    for (index, line) in input.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err(format!(
                "{}:{}: expected four tab-separated fields",
                path.display(),
                index + 1
            ));
        }
        let parse_optional = |value: &str| -> Result<Option<u64>, String> {
            if value == "-" {
                Ok(None)
            } else {
                value.parse().map(Some).map_err(|_| {
                    format!("{}:{}: invalid state timestamp", path.display(), index + 1)
                })
            }
        };
        let id = fields[0]
            .parse()
            .map_err(|_| format!("{}:{}: invalid runner id", path.display(), index + 1))?;
        states.insert(
            id,
            RunnerState {
                first_seen_offline: parse_optional(fields[1])?,
                last_success_epoch: parse_optional(fields[2])?,
                last_success_at: (fields[3] != "-").then(|| fields[3].to_owned()),
            },
        );
    }
    Ok(states)
}

fn save_state(path: &Path, states: &BTreeMap<u64, RunnerState>) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let mut output = String::from(
        "# runner_health_state_v1\n# id\tfirst_seen_offline\tlast_success_epoch\tlast_success_at\n",
    );
    for (id, state) in states {
        let first = state
            .first_seen_offline
            .map_or_else(|| "-".to_owned(), |value| value.to_string());
        let success = state
            .last_success_epoch
            .map_or_else(|| "-".to_owned(), |value| value.to_string());
        let success_at = state.last_success_at.as_deref().unwrap_or("-");
        output.push_str(&format!("{id}\t{first}\t{success}\t{success_at}\n"));
    }
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temporary, output)
        .map_err(|error| format!("failed to write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))
}

fn evaluate_runner(
    runner: &Runner,
    state: &mut RunnerState,
    now: u64,
    threshold: u64,
) -> Evaluation {
    match runner.status.as_str() {
        "online" => {
            state.first_seen_offline = None;
            Evaluation {
                health: if runner.busy {
                    "online/busy"
                } else {
                    "online/idle"
                },
                offline_for: None,
                alert: false,
            }
        }
        "offline" => {
            let first_seen = *state.first_seen_offline.get_or_insert(now);
            let offline_for = now.saturating_sub(first_seen);
            Evaluation {
                health: if offline_for > threshold {
                    "OFFLINE/ALERT"
                } else {
                    "offline/grace"
                },
                offline_for: Some(offline_for),
                alert: offline_for > threshold,
            }
        }
        _ => Evaluation {
            health: "UNKNOWN/ALERT",
            offline_for: None,
            alert: true,
        },
    }
}

fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    }
}

fn annotation(kind: &str, message: &str) {
    if env::var_os("GITHUB_ACTIONS").is_some() {
        let escaped = message
            .replace('%', "%25")
            .replace('\r', "%0D")
            .replace('\n', "%0A");
        println!("::{kind} title=Self-hosted runner health::{escaped}");
    }
}

fn current_epoch() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| format!("system clock predates Unix epoch: {error}"))
}

fn run(options: Options) -> Result<bool, String> {
    let now = current_epoch()?;
    let runner_endpoint = format!("repos/{}/actions/runners?per_page=100", options.repository);
    let runners = parse_runners(&gh_api(
        &runner_endpoint,
        ".runners[] | [.id, .name, .os, .status, .busy] | @tsv",
        true,
    )?)?;
    if runners.is_empty() {
        println!(
            "Self-hosted runner health: {}\nregistered: 0\nALERT: no self-hosted runners are registered",
            options.repository
        );
        annotation("error", "no self-hosted runners are registered");
        return Ok(true);
    }

    let registered_names = runners
        .iter()
        .map(|runner| runner.name.clone())
        .collect::<BTreeSet<_>>();
    let mut runs = BTreeMap::new();
    for workflow in &options.workflows {
        let endpoint = format!(
            "repos/{}/actions/workflows/{workflow}/runs?status=completed&per_page={}",
            options.repository, options.run_scan_limit
        );
        for workflow_run in parse_workflow_runs(&gh_api(
            &endpoint,
            ".workflow_runs[] | [.id, .name, (.conclusion // \"\"), .updated_at, .html_url] | @tsv",
            false,
        )?)? {
            runs.insert(workflow_run.id, workflow_run);
        }
    }
    let mut runs = runs.into_values().collect::<Vec<_>>();
    runs.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));

    let mut states = load_state(&options.state_file)?;
    states.retain(|id, _| runners.iter().any(|runner| runner.id == *id));
    let mut found_success = BTreeSet::new();
    for workflow_run in &runs {
        if found_success.len() == registered_names.len() {
            break;
        }
        if matches!(
            workflow_run.conclusion.as_str(),
            "skipped" | "cancelled" | ""
        ) {
            continue;
        }
        let endpoint = format!(
            "repos/{}/actions/runs/{}/jobs?filter=latest&per_page=100",
            options.repository, workflow_run.id
        );
        for job in parse_successful_jobs(&gh_api(
            &endpoint,
            ".jobs[] | select(.conclusion == \"success\" and (.runner_name // \"\") != \"\" and (.completed_at // \"\") != \"\") | [.runner_name, .completed_at] | @tsv",
            true,
        )?)? {
            if !registered_names.contains(&job.runner_name) {
                continue;
            }
            found_success.insert(job.runner_name.clone());
            let runner = runners
                .iter()
                .find(|runner| runner.name == job.runner_name)
                .unwrap();
            let state = states.entry(runner.id).or_default();
            if state
                .last_success_epoch
                .is_none_or(|previous| job.completed_epoch > previous)
            {
                state.last_success_epoch = Some(job.completed_epoch);
                state.last_success_at = Some(job.completed_at);
            }
        }
    }

    println!("Self-hosted runner health: {}", options.repository);
    println!("registered: {}", runners.len());
    println!(
        "offline alert threshold: {}",
        format_age(options.offline_threshold_seconds)
    );
    if let Some(run) = runs.iter().find(|run| run.conclusion == "success") {
        println!(
            "last successful workflow run (may not use a self-hosted runner): {} | {} | {}",
            run.updated_at, run.name, run.html_url
        );
    } else {
        println!("last successful workflow run: none in scanned history");
    }
    println!();
    println!(
        "{:<28} {:<8} {:<14} {:<22} {:<12}",
        "RUNNER", "OS", "STATUS", "LAST SUCCESS", "OFFLINE FOR"
    );

    let mut alerts = 0usize;
    let mut offline = 0usize;
    for runner in &runners {
        let state = states.entry(runner.id).or_default();
        let evaluation = evaluate_runner(runner, state, now, options.offline_threshold_seconds);
        if evaluation.offline_for.is_some() {
            offline += 1;
        }
        if evaluation.alert {
            alerts += 1;
        }
        let last_success = match (&state.last_success_at, state.last_success_epoch) {
            (Some(timestamp), Some(epoch)) => {
                format!("{} ({})", timestamp, format_age(now.saturating_sub(epoch)))
            }
            _ => "unknown".to_owned(),
        };
        let offline_for = evaluation
            .offline_for
            .map_or_else(|| "-".to_owned(), format_age);
        println!(
            "{:<28} {:<8} {:<14} {:<22} {:<12}",
            runner.name, runner.os, evaluation.health, last_success, offline_for
        );
        if evaluation.alert {
            annotation(
                "error",
                &format!(
                    "runner {} is {}{}",
                    runner.name,
                    runner.status,
                    evaluation
                        .offline_for
                        .map(|seconds| format!(" for {}", format_age(seconds)))
                        .unwrap_or_default()
                ),
            );
        } else if let Some(seconds) = evaluation.offline_for {
            annotation(
                "warning",
                &format!(
                    "runner {} is offline for {} (within {} grace period)",
                    runner.name,
                    format_age(seconds),
                    format_age(options.offline_threshold_seconds)
                ),
            );
        }
    }
    save_state(&options.state_file, &states)?;
    println!();
    println!(
        "summary: registered={} online={} offline={} alerts={} state={}",
        runners.len(),
        runners.len() - offline,
        offline,
        alerts,
        options.state_file.display()
    );
    Ok(alerts > 0)
}

fn main() -> ExitCode {
    let options = match parse_options(env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("runner_health.rs: {error}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };
    match run(options) {
        Ok(true) => ExitCode::from(1),
        Ok(false) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runner_health.rs: {error}");
            annotation("error", &error);
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(status: &str, busy: bool) -> Runner {
        Runner {
            id: 7,
            name: "runner-1".to_owned(),
            os: "Linux".to_owned(),
            status: status.to_owned(),
            busy,
        }
    }

    #[test]
    fn parses_github_utc_timestamps() {
        assert_eq!(parse_github_timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_github_timestamp("2026-01-01T00:00:00Z").unwrap(),
            1_767_225_600
        );
        assert_eq!(
            parse_github_timestamp("2026-01-01T00:00:00.123Z").unwrap(),
            1_767_225_600
        );
    }

    #[test]
    fn parses_runner_rows() {
        let runners =
            parse_runners("2\talpha\tLinux\tonline\ttrue\n3\tbeta\tLinux\toffline\tfalse\n")
                .unwrap();
        assert_eq!(runners.len(), 2);
        assert_eq!(runners[0].name, "alpha");
        assert!(runners[0].busy);
        assert_eq!(runners[1].status, "offline");
    }

    #[test]
    fn offline_alert_waits_for_the_threshold() {
        let mut state = RunnerState::default();
        let first = evaluate_runner(&runner("offline", false), &mut state, 10_000, 3_600);
        assert_eq!(first.offline_for, Some(0));
        assert!(!first.alert);
        let boundary = evaluate_runner(&runner("offline", false), &mut state, 13_600, 3_600);
        assert_eq!(boundary.offline_for, Some(3_600));
        assert!(!boundary.alert);
        let later = evaluate_runner(&runner("offline", false), &mut state, 13_601, 3_600);
        assert_eq!(later.offline_for, Some(3_601));
        assert!(later.alert);
    }

    #[test]
    fn online_runner_clears_offline_state() {
        let mut state = RunnerState {
            first_seen_offline: Some(1),
            ..RunnerState::default()
        };
        let evaluation = evaluate_runner(&runner("online", true), &mut state, 10_000, 3_600);
        assert_eq!(evaluation.health, "online/busy");
        assert!(!evaluation.alert);
        assert_eq!(state.first_seen_offline, None);
    }

    #[test]
    fn state_round_trips() {
        let path = env::temp_dir().join(format!("runner-health-test-{}.tsv", std::process::id()));
        let mut states = BTreeMap::new();
        states.insert(
            7,
            RunnerState {
                first_seen_offline: Some(10),
                last_success_epoch: Some(20),
                last_success_at: Some("1970-01-01T00:00:20Z".to_owned()),
            },
        );
        save_state(&path, &states).unwrap();
        assert_eq!(load_state(&path).unwrap(), states);
        fs::remove_file(path).unwrap();
    }
}
