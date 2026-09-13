//! Read a current `--verify-json` file through Hermit's producer-owned type.
//!
//! Shell and Python tests call this instead of maintaining their own accepted
//! verdict strings. A new [`Verdict`] variant therefore makes this exhaustive
//! match fail to compile; an unavailable, incomplete, or malformed report is a
//! named refusal and never falls back to a human-readable banner.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use hermit::canonical_verdict::Verdict;
use hermit::canonical_verdict::VerificationReport;

const HELP: &str = "\
Usage: verification-report [--json] <REQUIREMENT> <PATH>

Read a current Hermit --verify-json report through the producer-owned schema.

Requirements:
  matched          Require a typed matched verdict
  canonical-match  Require a typed match under the canonical comparison policy

Options:
  --json          Print the parsed report, even when the requirement is unmet
  -h, --help       Print this help

Exit status: 0 when the requirement is met, 1 when unmet, 2 on a refusal.
JSON output alone does not mean verification passed.";

fn refuse(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("verification-report: REFUSED: {message}");
    ExitCode::from(2)
}

fn read_current_report(path: &Path) -> Result<VerificationReport, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
    VerificationReport::from_current_json_value(value)
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn require_match(report: &VerificationReport) -> Result<(), String> {
    match report.verdict {
        Verdict::Matched if report.verified => Ok(()),
        Verdict::Matched => {
            Err("verdict is matched but verified is false; the typed fields disagree".into())
        }
        Verdict::Diverged if !report.verified => {
            Err("verification verdict is diverged, not matched".into())
        }
        Verdict::Diverged => {
            Err("verdict is diverged but verified is true; the typed fields disagree".into())
        }
        Verdict::NoResult if !report.verified => {
            Err("verification verdict is no_result, not matched".into())
        }
        Verdict::NoResult => {
            Err("verdict is no_result but verified is true; the typed fields disagree".into())
        }
        Verdict::InfrastructureError if !report.verified => {
            Err("verification verdict is infrastructure_error, not matched".into())
        }
        Verdict::InfrastructureError => Err(
            "verdict is infrastructure_error but verified is true; the typed fields disagree"
                .into(),
        ),
    }
}

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let emit_json = if let Some(index) = arguments.iter().position(|arg| arg == "--json") {
        arguments.remove(index);
        true
    } else {
        false
    };
    if matches!(arguments.as_slice(), [flag] if matches!(flag.as_str(), "-h" | "--help")) {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let mut args = arguments.into_iter();
    let Some(requirement) = args.next() else {
        return refuse("usage: verification-report [--json] matched|canonical-match PATH");
    };
    let Some(path) = args.next() else {
        return refuse("usage: verification-report [--json] matched|canonical-match PATH");
    };
    if args.next().is_some() {
        return refuse("usage: verification-report [--json] matched|canonical-match PATH");
    }

    let report = match read_current_report(Path::new(&path)) {
        Ok(report) => report,
        Err(error) => return refuse(error),
    };
    if emit_json {
        match serde_json::to_string(&report) {
            Ok(json) => println!("{json}"),
            Err(error) => return refuse(format!("cannot encode parsed report: {error}")),
        }
    }
    if let Err(error) = require_match(&report) {
        eprintln!("verification-report: {error}");
        return ExitCode::FAILURE;
    }

    match requirement.as_str() {
        "matched" => ExitCode::SUCCESS,
        "canonical-match" => match report.require_canonical_match() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("verification-report: {error}");
                ExitCode::FAILURE
            }
        },
        other => refuse(format!(
            "unknown requirement {other:?}; expected matched or canonical-match"
        )),
    }
}
