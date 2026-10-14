/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! One execution of a two-run verification, written to a directory by
//! `hermit run --verify-run DIR`, and the comparison of two such directories
//! by `hermit log-diff --verify-json REPORT DIR1 DIR2`.
//!
//! `run --verify` runs the guest twice in one Hermit process and compares the
//! runs. Splitting it lets a caller start the two runs itself, in parallel if
//! it likes, keep both logs, and still get the report `run --verify
//! --verify-json` writes. Nothing about the comparison is re-implemented
//! here: each directory holds what one of `run --verify`'s executions hands
//! its comparison (the guest's output and status, the verification log, the
//! skid-overshoot count, the run summary, the host-input record), and the
//! comparison is [`super::run::conclude_verification`], the function `run
//! --verify` concludes with.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use detcore_model::summary::RunSummary;
use hermit::Backend;
use hermit::Context;
use hermit::Error;
use hermit::SkidOvershootError;
use hermit::canonical_verdict::BranchCounterVerdict;
use hermit::canonical_verdict::ContainerDisposition;
use hermit::canonical_verdict::LogCompareStrictness;
use hermit::canonical_verdict::RunFailure;
use hermit::canonical_verdict::VerificationRun;
use hermit::host_input_change::read_host_inputs;
use reverie::process::ExitStatus;
use reverie::process::Output;
use serde::Deserialize;
use serde::Serialize;

use super::record_envelope::RecordEnvelope;
use super::run::CompletedVerificationRun;
use super::run::VerificationConclusion;
use super::run::VerifyAllow;
use super::run::conclude_verification;
use super::run::reject_first_run;
use super::verify::ComparisonOptions;
use super::verify::SignalTerminationReports;
use super::verify::container_failed_report;
use super::verify::emit_compared_guest_output;
use super::verify::record_exact_branch_counter;
use super::verify::run_failed_report;
use super::verify::verification_runtime_from_summaries;
use super::verify::write_pending_verification_json;
use super::verify::write_report_json;
use super::verify::write_skid_overshoot_without_comparison_json;

/// The record of how the run ended, written last: a directory without it is
/// an execution that did not finish writing its evidence.
pub(crate) const RECORD: &str = "verify-run.json";
/// The run's verification log, the log `run --verify` compares.
pub(crate) const LOG: &str = "run.log";
/// The guest's standard output.
pub(crate) const STDOUT: &str = "stdout";
/// The guest's standard error.
pub(crate) const STDERR: &str = "stderr";
/// The run's summary (`--summary-json`), when Detcore wrote one.
pub(crate) const SUMMARY: &str = "summary.json";
/// The host files the run's guest opened (`hermit::host_input_change`).
pub(crate) const HOST_INPUTS: &str = "host-inputs";
/// The recording a `record start --verify-run` execution made, which a
/// `--verify-replay-of` execution replays.
pub(crate) const RECORDING: &str = "recording";
/// The epoch a recording and its replay record: the replay runs at the
/// recording's own epoch, so the pair agrees on it by construction.
pub(crate) const RECORDING_EPOCH: &str = "recording";

/// The DBT backend's typed whole-process statistics stream, as DynamoRIO's
/// client wrote it (`backends::read_dbt_stats` decodes it).
#[cfg_attr(not(feature = "dbt"), allow(dead_code))]
pub(crate) const DBT_STATS: &str = "dbt-stats.bin";

/// The version of [`VerifyRunRecord`].
pub(crate) const SCHEMA: u32 = 1;

/// How one `--verify-run` execution ended, and the comparison it was run for.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifyRunRecord {
    pub(crate) schema: u32,
    /// The backend, as `--backend` spells it.
    pub(crate) backend: String,
    pub(crate) comparison: RecordedComparison,
    /// Whether the run used `--passthru-opt`, which decides whether a host
    /// file change can be named as a divergence's cause.
    pub(crate) passthru_opt: bool,
    /// The virtual-time epoch the run started its clock at (`--epoch`). `run
    /// --verify` gives its two runs one epoch; two separate runs agree only
    /// when the caller gives them the same one.
    pub(crate) epoch: String,
    /// `hermit host-capabilities`'s `exact_branch_counter` verdict, for a run
    /// on a backend that reads that counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exact_branch_counter: Option<BranchCounterVerdict>,
    /// What the DBT backend's comparison reads besides the log, for a DBT run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dbt: Option<DbtRunRecord>,
    /// Which half of which verification this is.
    #[serde(default, skip_serializing_if = "RunRole::is_run")]
    pub(crate) role: RunRole,
    pub(crate) outcome: VerifyRunOutcome,
}

/// Which half of which verification a `--verify-run` execution is.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunRole {
    /// One of `run --verify`'s two runs.
    #[default]
    Run,
    /// `record start --verify`'s recording.
    Record,
    /// `record start --verify`'s replay of a recording.
    Replay,
}

impl RunRole {
    fn is_run(&self) -> bool {
        *self == Self::Run
    }
}

/// What a DBT run's comparison reads besides its log and [`DBT_STATS`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DbtRunRecord {
    /// How many process-image initialization records the run's evidence
    /// held (`DbtEvidence::initialization_records`).
    pub(crate) initialization_records: u64,
}

/// The comparison the run's flags select, as `run --verify` derives it
/// (`RunOpts::verification_comparison_options`). Both runs of a comparison
/// must record the same one.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordedComparison {
    pub(crate) strictness: LogCompareStrictness,
    /// `--verify-verbose`: the all-level diagnostic comparison.
    pub(crate) diagnostic_full_trace: bool,
    /// Whether the records carry syscall output-buffer content.
    pub(crate) compare_io_buffers: bool,
    pub(crate) virtualize_time: bool,
}

impl RecordedComparison {
    pub(crate) fn from_options(options: &ComparisonOptions) -> Self {
        Self {
            strictness: options.strictness,
            diagnostic_full_trace: options.diagnostic_full_trace,
            compare_io_buffers: options.compare_io_buffers,
            virtualize_time: options.virtualize_time,
        }
    }

    /// The options `run --verify` compares with, for logs the caller owns.
    fn options(self) -> ComparisonOptions {
        ComparisonOptions {
            verbose: self.diagnostic_full_trace,
            strictness: self.strictness,
            compare_logs: true,
            diagnostic_full_trace: self.diagnostic_full_trace,
            compare_io_buffers: self.compare_io_buffers,
            keep_logs: false,
            match_overridden: false,
            failed_log_retention: None,
            record_envelope: RecordEnvelope::all_records_v1(),
            virtualize_time: self.virtualize_time,
            caller_owns_logs: true,
        }
    }
}

/// How one execution ended.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub(crate) enum VerifyRunOutcome {
    /// The guest ran to its end; its output is in [`STDOUT`] and [`STDERR`].
    Completed {
        status: RecordedStatus,
        skid_overshoots: u64,
        /// How many syscall DETLOG records an in-guest Tool forwarded into
        /// the log (LiteInst, SaBRe); `None` on a backend that forwards none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forwarded_syscalls: Option<u64>,
    },
    /// A typed failure of the run stopped it ([`RunFailure`]).
    RunFailed {
        failure: RunFailure,
        /// The failure's message, without the run's position in a pair, which
        /// the comparison adds where `run --verify` names it.
        detail: String,
    },
    /// The run's container child exited before returning the guest's output.
    ContainerFailed { disposition: ContainerDisposition },
    /// The run overshot its PMU skid this many times and returned no output.
    SkidOvershoot { count: u64 },
    /// An error that is none of the above stopped the run; `run --verify`
    /// leaves its pending no-result for such an error.
    Error { detail: String },
}

/// A guest's wait status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub(crate) enum RecordedStatus {
    Exited { code: i32 },
    Signaled { signal: i32, core_dumped: bool },
}

impl RecordedStatus {
    pub(crate) fn of(status: ExitStatus) -> Self {
        match status {
            ExitStatus::Exited(code) => Self::Exited { code },
            ExitStatus::Signaled(signal, core_dumped) => Self::Signaled {
                signal: signal as i32,
                core_dumped,
            },
            ExitStatus::SignaledRaw(signal, core_dumped) => Self::Signaled {
                signal,
                core_dumped,
            },
        }
    }

    fn status(self) -> ExitStatus {
        match self {
            Self::Exited { code } => ExitStatus::Exited(code),
            Self::Signaled {
                signal,
                core_dumped,
            } => ExitStatus::SignaledRaw(signal, core_dumped),
        }
    }
}

/// Write everything but the record to the new directory `directory`, then
/// the record, which commits it.
pub(crate) struct VerifyRunWriter {
    directory: PathBuf,
}

impl VerifyRunWriter {
    /// Claim `directory`, which must not exist, so no evidence of an earlier
    /// run can be read as this run's.
    pub(crate) fn create(directory: &Path) -> Result<Self, Error> {
        fs::create_dir(directory).with_context(|| {
            format!(
                "cannot create --verify-run directory {}: it must be a new directory",
                directory.display()
            )
        })?;
        Ok(Self {
            directory: directory.to_path_buf(),
        })
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    /// Write `bytes` as the new file `name`.
    pub(crate) fn write(&self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let path = self.path(name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Write the record last, and synchronize the directory, so a reader that
    /// finds the record finds every file it describes.
    pub(crate) fn commit(self, record: &VerifyRunRecord) -> Result<(), Error> {
        let mut bytes = serde_json::to_vec(record)?;
        bytes.push(b'\n');
        self.write(RECORD, &bytes)?;
        fs::File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("synchronizing {}", self.directory.display()))
    }
}

/// A `--verify-run` directory, read.
pub(crate) struct VerifyRunDirectory {
    directory: PathBuf,
    record: VerifyRunRecord,
}

impl VerifyRunDirectory {
    pub(crate) fn read(directory: &Path) -> Result<Self, Error> {
        let path = directory.join(RECORD);
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "reading {}: a --verify-run directory without its record did not finish",
                path.display()
            )
        })?;
        let record: VerifyRunRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if record.schema != SCHEMA {
            anyhow::bail!(
                "{} has schema {}, and this Hermit reads schema {SCHEMA}",
                path.display(),
                record.schema
            );
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            record,
        })
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// Refuse a directory that holds no completed recording to replay.
    pub(crate) fn recording(&self) -> Result<(), Error> {
        if self.record.role != RunRole::Record
            || !matches!(self.record.outcome, VerifyRunOutcome::Completed { .. })
        {
            anyhow::bail!(
                "{} holds no completed recording: it is a {:?} execution that ended {:?}",
                self.directory.display(),
                self.record.role,
                self.record.outcome
            );
        }
        Ok(())
    }

    fn summary(&self) -> Option<RunSummary> {
        let path = self.directory.join(SUMMARY);
        let bytes = fs::read(&path).ok()?;
        serde_json::from_slice(&bytes)
            .inspect_err(|error| {
                eprintln!(
                    "WARNING: verification runtime statistics unavailable: parsing {}: {error}",
                    path.display()
                )
            })
            .ok()
    }

    fn output(&self, status: RecordedStatus) -> Result<Output, Error> {
        let read = |name: &str| {
            let path = self.directory.join(name);
            fs::read(&path).with_context(|| format!("reading {}", path.display()))
        };
        Ok(Output {
            status: status.status(),
            stdout: read(STDOUT)?,
            stderr: read(STDERR)?,
        })
    }
}

/// The run's error for an outcome that is not a completed run, after writing
/// its report to `verify_json` as `run --verify` does for the same failure of
/// its run `run`. `earlier` is the completed earlier run of the pair, when
/// this is the second.
fn failed_run(
    verify_json: Option<&Path>,
    run: VerificationRun,
    label: &str,
    outcome: &VerifyRunOutcome,
    summaries: (Option<&RunSummary>, Option<&RunSummary>),
    earlier: Option<(&Output, u64)>,
) -> Result<Error, Error> {
    let error = match outcome {
        VerifyRunOutcome::Completed { .. } => unreachable!("a completed run did not fail"),
        VerifyRunOutcome::RunFailed { failure, detail } => {
            // `run --verify` names the run in a determinism loss's message
            // (`RunLatches::refuse_comparison`), and only there.
            let reported = if *failure == RunFailure::DeterminismLoss {
                format!("{label}: {detail}")
            } else {
                detail.clone()
            };
            if let Some(path) = verify_json {
                write_report_json(path, &run_failed_report(run, *failure, reported))?;
            }
            Error::msg(format!("{label}: {detail}"))
        }
        VerifyRunOutcome::ContainerFailed { disposition } => {
            if let Some(path) = verify_json {
                write_report_json(path, &container_failed_report(run, disposition.clone()))?;
            }
            Error::msg(format!(
                "{label}: the container exited before returning the guest's output: \
                 {disposition:?}"
            ))
        }
        VerifyRunOutcome::SkidOvershoot { count } => {
            let total = count.saturating_add(earlier.map_or(0, |(_, skid)| skid));
            if let Some(path) = verify_json {
                write_skid_overshoot_without_comparison_json(
                    path,
                    total,
                    verification_runtime_from_summaries(summaries.0, summaries.1),
                    earlier.map(|(output, _)| output.status),
                )?;
            }
            Error::new(SkidOvershootError::new(total))
        }
        VerifyRunOutcome::Error { detail } => Error::msg(format!("{label}: {detail}")),
    };
    Ok(error)
}

/// Compare two `--verify-run` directories as `run --verify` compares its two
/// runs, publish the report to `verify_json`, and return the status `run
/// --verify` would exit with. `verify_allow` is `--verify-allow`, and
/// `verify_log_dir` is `--keep-logs --verify-log-dir`.
///
/// Run 2's directory is read only once run 1 is admitted to a comparison:
/// where `run --verify` would not have started run 2 (run 1 failed, or its
/// status is not allowed), a caller need not have run it either.
pub(crate) fn compare(
    directories: [&Path; 2],
    verify_json: Option<&Path>,
    verify_allow: VerifyAllow,
    verify_log_dir: Option<&Path>,
) -> Result<ExitStatus, Error> {
    // Stamp the invocation-bound no-result first, as `run --verify` does, so
    // a reused path never shows an earlier verdict.
    if let Some(path) = verify_json {
        write_pending_verification_json(path)?;
    }
    let run1 = VerifyRunDirectory::read(directories[0])?;
    let record1 = &run1.record;
    let backend = <Backend as clap::ValueEnum>::from_str(&record1.backend, false)
        .map_err(|error| Error::msg(format!("unknown backend {}: {error}", record1.backend)))?;
    if let Some(verdict) = record1.exact_branch_counter.clone() {
        record_exact_branch_counter(verdict);
        // The pending stamp predates the verdict; restamp it with the verdict,
        // as `run --verify` does once it knows it.
        if let Some(path) = verify_json {
            write_pending_verification_json(path)?;
        }
    }
    // `--keep-logs`: what `run --verify --keep-logs` retains on each path.
    let retain = |runs: &[&VerifyRunDirectory]| -> Result<(), Error> {
        let Some(logs) = verify_log_dir else {
            return Ok(());
        };
        retain_logs(logs, runs)
    };
    if record1.role == RunRole::Record {
        return compare_recording_and_replay(&run1, directories[1], verify_json, &retain);
    }
    let summary1 = run1.summary();

    let VerifyRunOutcome::Completed {
        status: status1,
        skid_overshoots: skid1,
        forwarded_syscalls: forwarded1,
    } = record1.outcome
    else {
        retain(&[&run1])?;
        return Err(failed_run(
            verify_json,
            VerificationRun::Run1,
            "run 1",
            &record1.outcome,
            (summary1.as_ref(), None),
            None,
        )?);
    };
    let out1 = run1.output(status1)?;
    // Prepared where `run --verify` prepares them, and queued the same way:
    // by every return below that does not verify the runs.
    let mut signal_reports = SignalTerminationReports::default();
    signal_reports.prepare("run 1", out1.status, &run1.directory.join(LOG));
    if let Some(error) =
        reject_first_run(verify_json, verify_allow, &out1, skid1, summary1.as_ref())
    {
        retain(&[&run1])?;
        return Err(error);
    }

    let run2 = match VerifyRunDirectory::read(directories[1]) {
        Ok(run2) => run2,
        Err(error) => {
            retain(&[&run1])?;
            return Err(error);
        }
    };
    let record2 = &run2.record;
    if record1.epoch != record2.epoch {
        anyhow::bail!(
            "the two --verify-run directories started their virtual clocks at different epochs \
             ({} and {}), so their logs differ in every timestamp; give both runs one --epoch \
             (or HERMIT_EPOCH), as `run --verify` gives its two runs one",
            record1.epoch,
            record2.epoch
        );
    }
    if record1.backend != record2.backend
        || record1.comparison != record2.comparison
        || record1.passthru_opt != record2.passthru_opt
        || record2.role != RunRole::Run
    {
        anyhow::bail!(
            "the two --verify-run directories were not run alike, so they are not compared: \
             {} ran {} with {:?} and passthru_opt={}; {} ran {} with {:?} and passthru_opt={}",
            directories[0].display(),
            record1.backend,
            record1.comparison,
            record1.passthru_opt,
            directories[1].display(),
            record2.backend,
            record2.comparison,
            record2.passthru_opt,
        );
    }
    let summary2 = run2.summary();

    let VerifyRunOutcome::Completed {
        status: status2,
        skid_overshoots: skid2,
        forwarded_syscalls: forwarded2,
    } = record2.outcome
    else {
        retain(&[&run1, &run2])?;
        return Err(failed_run(
            verify_json,
            VerificationRun::Run2,
            "run 2",
            &record2.outcome,
            (summary1.as_ref(), summary2.as_ref()),
            Some((&out1, skid1)),
        )?);
    };
    let out2 = run2.output(status2)?;
    if let Some(forwarded1) = forwarded1 {
        let forwarded2 = forwarded2
            .ok_or_else(|| Error::msg("run 2 forwarded no DETLOG records where run 1 did"))?;
        if forwarded1 == 0 || forwarded2 == 0 {
            anyhow::bail!(
                "{} verification captured no syscall DETLOG records: run1={forwarded1}, \
                 run2={forwarded2}",
                if backend == Backend::Sabre {
                    "SaBRe"
                } else {
                    "LiteInst"
                }
            );
        }
    }

    signal_reports.prepare("run 2", out2.status, &run2.directory.join(LOG));

    // The comparison consumes the logs it is given. Give it links, so the
    // logs in the directories stay as they are whatever it does with them.
    let links = tempfile::Builder::new()
        .prefix(".hermit-verify-run-compare-")
        .tempdir_in(run1.directory.parent().unwrap_or(Path::new(".")))
        .context("creating a directory for the compared logs' links")?;
    let link = |run: &VerifyRunDirectory, name: &str| -> Result<tempfile::TempPath, Error> {
        let source = run.directory.join(LOG);
        let target = links.path().join(name);
        fs::hard_link(&source, &target)
            .or_else(|_| fs::copy(&source, &target).map(|_| ()))
            .with_context(|| format!("linking {}", source.display()))?;
        Ok(tempfile::TempPath::try_from_path(target)?)
    };
    let logs = [link(&run1, "run1.log")?, link(&run2, "run2.log")?];
    let outcome = if backend == Backend::Dbt {
        conclude_dbt([&run1, &run2], [&out1, &out2], logs, verify_json)?
    } else {
        let [log1, log2] = logs;
        conclude_verification(
            VerificationConclusion {
                verify_json,
                comparison_options: record1.comparison.options(),
                #[cfg(feature = "dbt")]
                dbt: None,
                backend,
                passthru_opt: record1.passthru_opt,
            },
            CompletedVerificationRun {
                output: &out1,
                log: log1,
                skid_overshoots: skid1,
                summary: summary1,
                host_inputs: Some(read_host_inputs(&run1.directory.join(HOST_INPUTS))),
            },
            CompletedVerificationRun {
                output: &out2,
                log: log2,
                skid_overshoots: skid2,
                summary: summary2,
                host_inputs: Some(read_host_inputs(&run2.directory.join(HOST_INPUTS))),
            },
        )?
    };
    // After a match run 1's log is kept as the golden log, as `--keep-logs`
    // keeps it; otherwise both.
    if outcome.verified() {
        retain(&[&run1])?;
    } else {
        retain(&[&run1, &run2])?;
    }
    // What `run --verify` shows of the guest after its comparison: DBT's
    // adapter shows run 1's output after a match, and the others also after
    // a divergence where both runs' output agreed.
    if !outcome.verified() {
        if backend != Backend::Dbt {
            emit_compared_guest_output(false, &out1, &out2)?;
        }
        return outcome.into_exit_status();
    }
    signal_reports.verified();
    if backend == Backend::Kvm {
        eprintln!(":: Backend: KVM (reverie-kvm KvmGuest<Detcore>)");
    }
    emit_compared_guest_output(true, &out1, &out2)?;
    Ok(outcome.guest_status)
}

/// [`compare`] for a recording (`run1`) and the directory of its replay, as
/// `record start --verify` compares them (`conclude_record_replay`). As
/// there, a failed recording or replay ends the comparison with its error
/// and the pending report, and every recording is replayed whatever its
/// status.
fn compare_recording_and_replay(
    run1: &VerifyRunDirectory,
    directory2: &Path,
    verify_json: Option<&Path>,
    retain: &dyn Fn(&[&VerifyRunDirectory]) -> Result<(), Error>,
) -> Result<ExitStatus, Error> {
    let completed = |run: &VerifyRunDirectory, label: &str| match &run.record.outcome {
        VerifyRunOutcome::Completed { status, .. } => run.output(*status),
        outcome => Err(Error::msg(format!("{label} did not complete: {outcome:?}"))),
    };
    let recording = match completed(run1, "the recording") {
        Ok(output) => output,
        Err(error) => {
            retain(&[run1])?;
            return Err(error);
        }
    };
    let run2 = match VerifyRunDirectory::read(directory2) {
        Ok(run2) => run2,
        Err(error) => {
            retain(&[run1])?;
            return Err(error);
        }
    };
    let (record1, record2) = (&run1.record, &run2.record);
    if record2.role != RunRole::Replay
        || record1.backend != record2.backend
        || record1.comparison != record2.comparison
    {
        anyhow::bail!(
            "{} is not the replay of the recording in {}: {:?} on {} with {:?}",
            directory2.display(),
            run1.directory.display(),
            record2.role,
            record2.backend,
            record2.comparison,
        );
    }
    let replay = match completed(&run2, "the replay") {
        Ok(output) => output,
        Err(error) => {
            retain(&[run1, &run2])?;
            return Err(error);
        }
    };
    let links = tempfile::Builder::new()
        .prefix(".hermit-verify-run-compare-")
        .tempdir_in(run1.directory.parent().unwrap_or(Path::new(".")))
        .context("creating a directory for the compared logs' links")?;
    let link = |run: &VerifyRunDirectory, name: &str| -> Result<tempfile::TempPath, Error> {
        let source = run.directory.join(LOG);
        let target = links.path().join(name);
        fs::hard_link(&source, &target)
            .or_else(|_| fs::copy(&source, &target).map(|_| ()))
            .with_context(|| format!("linking {}", source.display()))?;
        Ok(tempfile::TempPath::try_from_path(target)?)
    };
    let status = super::record_start::conclude_record_replay(
        [&recording, &replay],
        [link(run1, "record.log")?, link(&run2, "replay.log")?],
        record1.comparison.strictness,
        verify_json,
        true,
    );
    // After a match the recording's log is kept as the golden log, as
    // `--keep-logs` keeps one; otherwise both.
    match &status {
        Ok(_) if read_verified(verify_json) => retain(&[run1])?,
        _ => retain(&[run1, &run2])?,
    }
    status
}

/// Whether the report at `verify_json` says the runs were verified; false
/// when there is no report or it cannot be read.
fn read_verified(verify_json: Option<&Path>) -> bool {
    verify_json
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|report| report["verified"] == true)
}

/// Keep the logs of `runs` in `logs` under the names `run --verify
/// --keep-logs` gives them (`run1_log_*`, `run2_log_*`), as links, so the
/// directories keep theirs.
fn retain_logs(logs: &Path, runs: &[&VerifyRunDirectory]) -> Result<(), Error> {
    fs::create_dir_all(logs).with_context(|| format!("creating {}", logs.display()))?;
    eprintln!(":: Verification logs retained:");
    for (index, run) in runs.iter().enumerate() {
        let source = run.directory.join(LOG);
        let retained = tempfile::Builder::new()
            .prefix(&format!("run{}_log_", index + 1))
            .make_in(logs, |path| fs::hard_link(&source, path))
            .with_context(|| format!("retaining {} in {}", source.display(), logs.display()))?
            .into_temp_path()
            .keep()
            .context("keeping a retained log")?;
        eprintln!("::   run {}: {}", index + 1, retained.display());
    }
    Ok(())
}

/// Compare two completed DBT runs as `run_dbt` compares its two
/// (`backends::conclude_dbt_verification`): the logs, then the counted-branch
/// clocks and the typed statistics in each directory's [`DBT_STATS`].
#[cfg(feature = "dbt")]
fn conclude_dbt(
    runs: [&VerifyRunDirectory; 2],
    outputs: [&Output; 2],
    logs: [tempfile::TempPath; 2],
    verify_json: Option<&Path>,
) -> Result<super::verify::VerificationOutcome, Error> {
    use super::backends::DbtCompletedRun;
    use super::backends::DbtConclusion;
    use super::backends::conclude_dbt_verification;
    use super::backends::read_dbt_stats;

    let [log1, log2] = logs;
    let completed = |run: &VerifyRunDirectory, output, log| -> Result<DbtCompletedRun<'_>, Error> {
        let record = run.record.dbt.ok_or_else(|| {
            Error::msg(format!(
                "{} is a DBT run whose record has no DBT section",
                run.directory.display()
            ))
        })?;
        Ok(DbtCompletedRun {
            output,
            log,
            stats: read_dbt_stats(&run.directory.join(DBT_STATS))?,
            initialization_records: usize::try_from(record.initialization_records)?,
        })
    };
    let comparison = runs[0].record.comparison;
    conclude_dbt_verification(
        completed(runs[0], outputs[0], log1)?,
        completed(runs[1], outputs[1], log2)?,
        DbtConclusion {
            verify_json,
            verify_verbose: comparison.diagnostic_full_trace,
            compare_io_buffers: comparison.compare_io_buffers,
            virtualize_time: comparison.virtualize_time,
            keep_logs: false,
            caller_owns_logs: true,
        },
    )
}

#[cfg(not(feature = "dbt"))]
fn conclude_dbt(
    _runs: [&VerifyRunDirectory; 2],
    _outputs: [&Output; 2],
    _logs: [tempfile::TempPath; 2],
    _verify_json: Option<&Path>,
) -> Result<super::verify::VerificationOutcome, Error> {
    anyhow::bail!("these --verify-run directories are DBT runs, and this Hermit has no DBT backend")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_status_round_trips() {
        for status in [
            ExitStatus::Exited(0),
            ExitStatus::Exited(3),
            ExitStatus::SignaledRaw(9, false),
            ExitStatus::SignaledRaw(11, true),
        ] {
            let recorded = RecordedStatus::of(status);
            let back = recorded.status();
            assert_eq!(back.code(), status.code(), "{status:?}");
            assert_eq!(back.signal(), status.signal(), "{status:?}");
            assert_eq!(RecordedStatus::of(back), recorded, "{status:?}");
        }
    }

    #[test]
    fn a_record_round_trips_and_refuses_unknown_fields() {
        let record = VerifyRunRecord {
            schema: SCHEMA,
            backend: "ptrace".into(),
            comparison: RecordedComparison {
                strictness: LogCompareStrictness::Canonical,
                diagnostic_full_trace: false,
                compare_io_buffers: true,
                virtualize_time: true,
            },
            passthru_opt: false,
            epoch: "2026-01-01T00:00:00Z".into(),
            exact_branch_counter: None,
            dbt: None,
            role: RunRole::Run,
            outcome: VerifyRunOutcome::Completed {
                status: RecordedStatus::Exited { code: 0 },
                skid_overshoots: 0,
                forwarded_syscalls: None,
            },
        };
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(
            serde_json::from_value::<VerifyRunRecord>(json.clone()).unwrap(),
            record
        );
        let mut extra = json;
        extra["surprise"] = serde_json::json!(1);
        assert!(serde_json::from_value::<VerifyRunRecord>(extra).is_err());
    }

    #[test]
    fn a_directory_is_new_and_its_record_is_written_last() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("run1");
        let writer = VerifyRunWriter::create(&directory).unwrap();
        assert!(VerifyRunWriter::create(&directory).is_err());
        writer.write(STDOUT, b"out").unwrap();
        assert!(writer.write(STDOUT, b"again").is_err());
        assert!(VerifyRunDirectory::read(&directory).is_err());
        writer
            .commit(&VerifyRunRecord {
                schema: SCHEMA,
                backend: "ptrace".into(),
                comparison: RecordedComparison {
                    strictness: LogCompareStrictness::Stripped,
                    diagnostic_full_trace: false,
                    compare_io_buffers: true,
                    virtualize_time: true,
                },
                passthru_opt: false,
                epoch: "2026-01-01T00:00:00Z".into(),
                exact_branch_counter: None,
                dbt: None,
                role: RunRole::Run,
                outcome: VerifyRunOutcome::Error {
                    detail: "fixture".into(),
                },
            })
            .unwrap();
        assert!(VerifyRunDirectory::read(&directory).is_ok());
    }
}
