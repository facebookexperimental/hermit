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

use detcore_model::TerminalOutcome;
use detcore_model::summary::RunSummary;
use hermit::Backend;
use hermit::Context;
use hermit::Error;
use hermit::SkidOvershootError;
#[cfg(test)]
use hermit::canonical_verdict::LogCompareStrictness;
use hermit::canonical_verdict::RunFailure;
use hermit::canonical_verdict::VerificationRun;
use hermit::host_input_change::read_host_inputs;
pub(crate) use hermit::verify_run_record::DBT_STATS;
pub(crate) use hermit::verify_run_record::DbtRunRecord;
pub(crate) use hermit::verify_run_record::HOST_INPUTS;
pub(crate) use hermit::verify_run_record::LOG;
pub(crate) use hermit::verify_run_record::RECORD;
pub(crate) use hermit::verify_run_record::RECORDING;
pub(crate) use hermit::verify_run_record::RECORDING_EPOCH;
pub(crate) use hermit::verify_run_record::RecordedComparison;
pub(crate) use hermit::verify_run_record::RecordedStatus;
pub(crate) use hermit::verify_run_record::RunRole;
pub(crate) use hermit::verify_run_record::SCHEMA;
pub(crate) use hermit::verify_run_record::STDERR;
pub(crate) use hermit::verify_run_record::STDOUT;
pub(crate) use hermit::verify_run_record::SUMMARY;
pub(crate) use hermit::verify_run_record::VerifyRunOutcome;
pub(crate) use hermit::verify_run_record::VerifyRunRecord;
use reverie::process::ExitStatus;
use reverie::process::Output;

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

pub(crate) fn recorded_comparison(options: &ComparisonOptions) -> RecordedComparison {
    RecordedComparison {
        strictness: options.strictness,
        diagnostic_full_trace: options.diagnostic_full_trace,
        compare_io_buffers: options.compare_io_buffers,
        virtualize_time: options.virtualize_time,
    }
}

/// The options `run --verify` compares with, for logs the caller owns.
fn comparison_options(recorded: RecordedComparison) -> ComparisonOptions {
    ComparisonOptions {
        verbose: recorded.diagnostic_full_trace,
        strictness: recorded.strictness,
        compare_logs: true,
        diagnostic_full_trace: recorded.diagnostic_full_trace,
        compare_io_buffers: recorded.compare_io_buffers,
        keep_logs: false,
        match_overridden: false,
        failed_log_retention: None,
        record_envelope: RecordEnvelope::all_records_v1(),
        virtualize_time: recorded.virtualize_time,
        caller_owns_logs: true,
    }
}

pub(crate) fn recorded_status(status: ExitStatus) -> RecordedStatus {
    match status {
        ExitStatus::Exited(code) => RecordedStatus::Exited { code },
        ExitStatus::Signaled(signal, core_dumped) => RecordedStatus::Signaled {
            signal: signal as i32,
            core_dumped,
        },
        ExitStatus::SignaledRaw(signal, core_dumped) => RecordedStatus::Signaled {
            signal,
            core_dumped,
        },
    }
}

fn exit_status(status: RecordedStatus) -> ExitStatus {
    match status {
        RecordedStatus::Exited { code } => ExitStatus::Exited(code),
        RecordedStatus::Signaled {
            signal,
            core_dumped,
        } => ExitStatus::SignaledRaw(signal, core_dumped),
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

    /// Fixed native-payload storage, shared by the admitted saved driver and
    /// immutable backend evidence artifacts. The caller cannot choose another
    /// filename or publish a record through this operation.
    pub(crate) fn write_native_evidence(
        &self,
        evidence: &super::backend_hook::SavedNativeEvidence,
    ) -> Result<super::backend_hook::SavedNativeArtifact, Error> {
        self.write(DBT_STATS, evidence.raw())?;
        Ok(super::backend_hook::SavedNativeArtifact {
            path: self.path(DBT_STATS),
            initialization_records: evidence.initialization_records(),
        })
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
        let record = VerifyRunRecord::read(directory).map_err(Error::msg)?;
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
        let output = hermit::verify_run_record::read_output(&self.directory).map_err(Error::msg)?;
        Ok(Output {
            status: exit_status(status),
            stdout: output.stdout,
            stderr: output.stderr,
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
    compare_with_application(
        directories,
        verify_json,
        verify_allow,
        verify_log_dir,
        None,
        |_, _| {
            Err(Error::msg(
                "ordinary core comparison has no executable-selected native decoder",
            ))
        },
    )
}

#[derive(Clone, Copy)]
struct SavedApplication {
    name: &'static str,
    evidence: super::backend_hook::EvidenceRequirement,
    transport: super::backend_hook::VerificationLogTransport,
    native_identity_diagnostic: Option<&'static str>,
}

/// Only the executable's statically linked adapter can admit its named saved
/// records. Ordinary core comparison continues to reject unknown backends.
pub(super) fn compare_for_adapter<A: super::backend_hook::BackendAdapter>(
    directories: [&Path; 2],
    verify_json: Option<&Path>,
    verify_allow: VerifyAllow,
    verify_log_dir: Option<&Path>,
    adapter: &A,
) -> Result<ExitStatus, Error> {
    compare_with_application(
        directories,
        verify_json,
        verify_allow,
        verify_log_dir,
        Some(SavedApplication {
            name: A::NAME,
            evidence: A::EVIDENCE,
            transport: A::LOG_TRANSPORT,
            native_identity_diagnostic: A::NATIVE_IDENTITY_DIAGNOSTIC,
        }),
        |first, second| {
            let restore = |run: &VerifyRunDirectory| -> Result<A::Facts, Error> {
                let metadata = run.record.dbt.ok_or_else(|| {
                    Error::msg(format!(
                        "{} is a native run whose record has no native evidence section",
                        run.directory.display()
                    ))
                })?;
                let raw = fs::read(run.directory.join(DBT_STATS));
                adapter.restore_native_evidence(super::backend_hook::SavedNativeEvidenceRef {
                    raw: raw.as_ref().map(Vec::as_slice),
                    initialization_records: metadata.initialization_records,
                })
            };
            let first = restore(first)?;
            let second = restore(second)?;
            A::EVIDENCE.validate(adapter.pair_evidence(&first, &second)?)
        },
    )
}

fn compare_with_application(
    directories: [&Path; 2],
    verify_json: Option<&Path>,
    verify_allow: VerifyAllow,
    verify_log_dir: Option<&Path>,
    application: Option<SavedApplication>,
    native_evidence: impl FnOnce(
        &VerifyRunDirectory,
        &VerifyRunDirectory,
    ) -> Result<super::backend_hook::PairEvidence, Error>,
) -> Result<ExitStatus, Error> {
    // Stamp the invocation-bound no-result first, as `run --verify` does, so
    // a reused path never shows an earlier verdict.
    if let Some(path) = verify_json {
        write_pending_verification_json(path)?;
    }
    if let Some(application) = application {
        super::backend_hook::validate_name(application.name)?;
        application.transport.validate(application.evidence)?;
    }
    let run1 = VerifyRunDirectory::read(directories[0])?;
    let record1 = &run1.record;
    let (backend, application_backend) = match application {
        Some(application) => {
            anyhow::ensure!(
                record1.backend == application.name && record1.role == RunRole::Run,
                "saved backend `{}` does not match executable-selected backend `{}`",
                record1.backend,
                application.name
            );
            if application.evidence == super::backend_hook::EvidenceRequirement::Common {
                anyhow::ensure!(
                    record1.dbt.is_none(),
                    "a Common saved run cannot contain native evidence metadata"
                );
            }
            // This value is not a physical/backend fallback: the common
            // conclusion receives the exact application identity below, as on
            // the live statically selected path. No builtin is executed.
            (Backend::Ptrace, Some(application.name))
        }
        None => (
            <Backend as clap::ValueEnum>::from_str(&record1.backend, false).map_err(|error| {
                Error::msg(format!("unknown backend {}: {error}", record1.backend))
            })?,
            None,
        ),
    };
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
        guest_deadlock: guest_deadlock1,
        virtual_timeout_ns: virtual_timeout1,
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
    let outcome1 = TerminalOutcome::from_fields(guest_deadlock1, virtual_timeout1);
    // Prepared where `run --verify` prepares them, and queued the same way:
    // by every return below that does not verify the runs.
    let mut signal_reports = SignalTerminationReports::default();
    signal_reports.prepare("run 1", out1.status, &run1.directory.join(LOG));
    // A run 1 that ended with a terminal outcome (a deadlock under Hermit's
    // model, a virtual timeout) passes `--verify-allow` as `run --verify`'s
    // does (`rejected_first_run`): neither is a guest result, so the default
    // `success` refuses it.
    if let Some(error) =
        reject_first_run(verify_json, verify_allow, &out1, skid1, summary1.as_ref())
    {
        retain(&[&run1])?;
        return Err(super::run::rejected_first_run(error, outcome1));
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
        guest_deadlock: guest_deadlock2,
        virtual_timeout_ns: virtual_timeout2,
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
    if let Some(application) = application {
        if application.evidence == super::backend_hook::EvidenceRequirement::Common {
            anyhow::ensure!(
                record2.dbt.is_none(),
                "a Common saved run cannot contain native evidence metadata"
            );
        }
        let requires_socket =
            application.transport == super::backend_hook::VerificationLogTransport::InGuestSocket;
        anyhow::ensure!(
            forwarded1.is_some() == requires_socket && forwarded2.is_some() == requires_socket,
            "saved backend `{}` omitted or changed its declared verification log transport",
            application.name
        );
    }
    if let Some(forwarded1) = forwarded1 {
        let forwarded2 = forwarded2
            .ok_or_else(|| Error::msg("run 2 forwarded no DETLOG records where run 1 did"))?;
        if forwarded1 == 0 || forwarded2 == 0 {
            anyhow::bail!(
                "{} verification captured no syscall DETLOG records: run1={forwarded1}, \
                 run2={forwarded2}",
                application_backend.unwrap_or("LiteInst")
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
    let native_selected = application.is_some_and(|application| {
        application.evidence == super::backend_hook::EvidenceRequirement::NativeCounters
    });
    let evidence = if native_selected {
        match native_evidence(&run1, &run2)? {
            super::backend_hook::PairEvidence::NativeCounters { first, second } => {
                Some(super::verify::NativeEvidenceComparison::new(
                    application_backend.expect("static native identity"),
                    &first,
                    &second,
                ))
            }
            super::backend_hook::PairEvidence::Common => {
                return Err(Error::new(hermit::BackendRunFailure)
                    .context("backend omitted or changed its predeclared verification evidence"));
            }
        }
    } else {
        None
    };
    let outcome = {
        let [log1, log2] = logs;
        conclude_verification(
            VerificationConclusion {
                verify_json,
                comparison_options: comparison_options(record1.comparison),
                evidence,
                native_identity_diagnostic: application
                    .and_then(|application| application.native_identity_diagnostic),
                backend,
                application_backend,
                passthru_opt: record1.passthru_opt,
                // A `--verify-run` record does not say.
                rewrite_exclusives: None,
                discarded_attempts: Vec::new(),
            },
            CompletedVerificationRun {
                output: &out1,
                log: log1,
                skid_overshoots: skid1,
                summary: summary1,
                host_inputs: (!native_selected)
                    .then(|| read_host_inputs(&run1.directory.join(HOST_INPUTS))),
            },
            CompletedVerificationRun {
                output: &out2,
                log: log2,
                skid_overshoots: skid2,
                summary: summary2,
                host_inputs: (!native_selected)
                    .then(|| read_host_inputs(&run2.directory.join(HOST_INPUTS))),
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
        if !native_selected {
            emit_compared_guest_output(false, &out1, &out2)?;
        }
        return outcome.into_exit_status();
    }
    signal_reports.verified();
    if backend == Backend::Kvm {
        eprintln!(":: Backend: KVM (reverie-kvm KvmGuest<Detcore>)");
    }
    emit_compared_guest_output(true, &out1, &out2)?;
    // Compared runs (an explicit `--verify-allow` of failure or both) that
    // both ended with the same terminal outcome end with it, as `run --verify`
    // ends.
    if let Some(outcome) = outcome1
        && outcome1 == TerminalOutcome::from_fields(guest_deadlock2, virtual_timeout2)
    {
        return Err(Error::new(super::container::SchedulerOutcome(outcome)));
    }
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
        VerifyRunOutcome::Completed {
            status,
            guest_deadlock,
            virtual_timeout_ns,
            ..
        } => run.output(*status).map(|output| {
            (
                output,
                TerminalOutcome::from_fields(*guest_deadlock, *virtual_timeout_ns),
            )
        }),
        outcome => Err(Error::msg(format!("{label} did not complete: {outcome:?}"))),
    };
    let (recording, recording_outcome) = match completed(run1, "the recording") {
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
    let (replay, replay_outcome) = match completed(&run2, "the replay") {
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
    // A matched pair returns the recording's status; when both runs ended
    // with the same terminal outcome that status is the outcome's, reported
    // as the outcome, as `record start --verify` reports it.
    super::record_start::ended_pair_outcome(status?, recording_outcome, replay_outcome)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic adapter facts qualify the core's saved comparison boundary,
    /// not any runtime decoder or guest. The actual native decoder has its own
    /// backend-owned controls; this adapter must never be physically prepared.
    #[derive(Default)]
    struct SavedNativeControl {
        restored: std::cell::Cell<usize>,
        changed_dimension: Option<usize>,
        omit_evidence: bool,
    }

    impl super::super::backend_hook::BackendAdapter for SavedNativeControl {
        const NAME: &'static str = "saved-native-control";
        const EVIDENCE: super::super::backend_hook::EvidenceRequirement =
            super::super::backend_hook::EvidenceRequirement::NativeCounters;
        const CAPABILITIES: reverie::BackendCapabilities = reverie::BackendCapabilities::PTRACE;
        const LOG_TRANSPORT: super::super::backend_hook::VerificationLogTransport =
            super::super::backend_hook::VerificationLogTransport::NativeEvidence;
        type Options = ();
        type Session = ();
        type Pending = ();
        type Facts = super::super::backend_hook::NativeFacts;

        fn parse(&self, _: &clap::ArgMatches) -> Result<(), clap::Error> {
            Ok(())
        }
        fn validate_mode(&self, mode: super::super::backend_hook::Mode) -> Result<(), Error> {
            match mode {
                super::super::backend_hook::Mode::Ordinary
                | super::super::backend_hook::Mode::VerificationPair
                | super::super::backend_hook::Mode::SavedVerification => Ok(()),
            }
        }
        fn prepare(&self, _: (), _: super::super::backend_hook::Prepare<'_>) -> Result<(), Error> {
            panic!("offline comparison must not prepare a physical run")
        }
        fn execute(
            &self,
            _: &mut (),
            _: super::super::backend_hook::Execute<'_>,
        ) -> Result<super::super::backend_hook::Execution<()>, Error> {
            panic!("offline comparison must not execute a physical run")
        }
        fn finish(
            &self,
            _: (),
            _: super::super::backend_hook::Observation<'_>,
        ) -> Result<Self::Facts, Error> {
            panic!("offline comparison must not finish a physical run")
        }
        fn restore_native_evidence(
            &self,
            evidence: super::super::backend_hook::SavedNativeEvidenceRef<'_>,
        ) -> Result<Self::Facts, Error> {
            let raw = evidence
                .raw()
                .map_err(|error| Error::msg(format!("synthetic saved read failed: {error}")))?;
            anyhow::ensure!(
                raw == b"synthetic raw native control",
                "synthetic native payload changed"
            );
            let mut facts = super::super::backend_hook::NativeFacts {
                counted_branches: 7,
                intercepted_syscalls: 4,
                rewritten_syscalls: 3,
                stdin_reads: 2,
                memory_hash_fold: 19,
                initialization_records: usize::try_from(evidence.initialization_records())?,
                statistics_diagnostic: "synthetic complete native observations".to_owned(),
            };
            let restored = self.restored.get();
            self.restored.set(restored + 1);
            if restored == 1 {
                match self.changed_dimension {
                    None => {}
                    Some(0) => facts.counted_branches += 1,
                    Some(1) => facts.intercepted_syscalls += 1,
                    Some(2) => facts.rewritten_syscalls += 1,
                    Some(3) => facts.stdin_reads += 1,
                    Some(4) => facts.memory_hash_fold += 1,
                    Some(5) => facts.initialization_records += 1,
                    Some(_) => unreachable!(),
                }
            }
            Ok(facts)
        }
        fn pair_evidence(
            &self,
            first: &Self::Facts,
            second: &Self::Facts,
        ) -> Result<super::super::backend_hook::PairEvidence, Error> {
            Ok(if self.omit_evidence {
                super::super::backend_hook::PairEvidence::Common
            } else {
                super::super::backend_hook::PairEvidence::NativeCounters {
                    first: first.clone(),
                    second: second.clone(),
                }
            })
        }
    }

    fn saved_comparison_record(name: &str, native: bool) -> VerifyRunRecord {
        VerifyRunRecord {
            schema: SCHEMA,
            backend: name.to_owned(),
            comparison: RecordedComparison {
                strictness: LogCompareStrictness::Canonical,
                diagnostic_full_trace: false,
                compare_io_buffers: true,
                virtualize_time: true,
            },
            passthru_opt: false,
            epoch: "2026-01-01T00:00:00Z".to_owned(),
            exact_branch_counter: None,
            dbt: native.then_some(DbtRunRecord {
                initialization_records: 1,
            }),
            role: RunRole::Run,
            outcome: VerifyRunOutcome::Completed {
                status: RecordedStatus::Exited { code: 0 },
                skid_overshoots: 0,
                forwarded_syscalls: None,
                guest_deadlock: false,
                virtual_timeout_ns: None,
            },
        }
    }

    fn write_saved_comparison_control(directory: &Path, record: &VerifyRunRecord) {
        let writer = VerifyRunWriter::create(directory).unwrap();
        writer
            .write(
                LOG,
                b"1970-01-01T00:00:00.000000Z INFO detcore: synthetic static-adapter record\n",
            )
            .unwrap();
        writer.write(STDOUT, b"").unwrap();
        writer.write(STDERR, b"").unwrap();
        if record.dbt.is_some() {
            writer
                .write(DBT_STATS, b"synthetic raw native control")
                .unwrap();
        } else {
            writer.write(HOST_INPUTS, b"{\"records\":0}\n").unwrap();
        }
        writer.commit(record).unwrap();
    }

    fn saved_comparison_report(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn named_saved_comparison_keeps_all_native_dimensions_and_refuses_missing_evidence() {
        use super::super::backend_hook::BackendAdapter;

        for dimension in [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)] {
            let parent = tempfile::tempdir().unwrap();
            let directories = [parent.path().join("first"), parent.path().join("second")];
            let report = parent.path().join("report.json");
            let record = saved_comparison_record(SavedNativeControl::NAME, true);
            for directory in &directories {
                write_saved_comparison_control(directory, &record);
            }
            let adapter = SavedNativeControl {
                changed_dimension: dimension,
                ..Default::default()
            };
            let status = compare_for_adapter(
                [&directories[0], &directories[1]],
                Some(&report),
                VerifyAllow::Success,
                None,
                &adapter,
            )
            .unwrap();
            assert_eq!(adapter.restored.get(), 2);
            assert_eq!(status.success(), dimension.is_none());
            let report = saved_comparison_report(&report);
            assert_eq!(report["verified"], dimension.is_none());
            assert_eq!(
                report["verdict"],
                if dimension.is_none() {
                    "matched"
                } else {
                    "diverged"
                }
            );
            assert_eq!(report["comparison"]["record_envelope"], "all_records_v1");
            assert_eq!(report["comparison"]["strictness"], "canonical");
            assert_eq!(report["dbt_counted_branches"]["left"], 7);
            assert_eq!(
                report["dbt_counted_branches"]["right"],
                if dimension == Some(0) { 8 } else { 7 }
            );
        }
        for missing in ["metadata", "payload", "evidence_kind", "empty_payload"] {
            let parent = tempfile::tempdir().unwrap();
            let directories = [parent.path().join("first"), parent.path().join("second")];
            let report = parent.path().join("report.json");
            let mut first = saved_comparison_record(SavedNativeControl::NAME, true);
            if missing == "metadata" {
                first.dbt = None;
            }
            write_saved_comparison_control(&directories[0], &first);
            write_saved_comparison_control(
                &directories[1],
                &saved_comparison_record(SavedNativeControl::NAME, true),
            );
            if missing == "payload" {
                fs::remove_file(directories[0].join(DBT_STATS)).unwrap();
            }
            if missing == "empty_payload" {
                fs::write(directories[0].join(DBT_STATS), b"").unwrap();
            }
            fs::write(&report, b"{\"verified\":true}").unwrap();
            let adapter = SavedNativeControl {
                omit_evidence: missing == "evidence_kind",
                ..Default::default()
            };
            let result = compare_for_adapter(
                [&directories[0], &directories[1]],
                Some(&report),
                VerifyAllow::Success,
                None,
                &adapter,
            );
            assert!(result.is_err(), "missing {missing} must refuse");
            let report = saved_comparison_report(&report);
            assert_eq!(report["verdict"], "no_result");
            assert_eq!(report["verified"], false);
            assert_eq!(report["bitwise_parity"], false);
            assert!(report.get("dbt_counted_branches").is_none());
        }
    }

    #[test]
    fn named_saved_comparison_has_exact_identity_roles_options_and_transport() {
        use super::super::backend_hook::EvidenceRequirement;
        use super::super::backend_hook::VerificationLogTransport;

        for defect in [
            None,
            Some("first_name"),
            Some("missing_name"),
            Some("second_name"),
            Some("first_role"),
            Some("second_role"),
            Some("epoch"),
            Some("comparison"),
            Some("native_first"),
            Some("native_second"),
            Some("unexpected_socket"),
            Some("missing_socket"),
            Some("empty_socket"),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let directories = [parent.path().join("first"), parent.path().join("second")];
            let report = parent.path().join("report.json");
            let mut first = saved_comparison_record("saved-common-control", false);
            let mut second = first.clone();
            let mut transport = VerificationLogTransport::CoordinatorTracing;
            match defect {
                None => {}
                Some("first_name") => first.backend = "ptrace".to_owned(),
                Some("missing_name") => first.backend.clear(),
                Some("second_name") => second.backend = "ptrace".to_owned(),
                Some("first_role") => first.role = RunRole::Record,
                Some("second_role") => second.role = RunRole::Replay,
                Some("epoch") => second.epoch = "2026-01-02T00:00:00Z".to_owned(),
                Some("comparison") => second.comparison.compare_io_buffers = false,
                Some("native_first") => {
                    first.dbt = Some(DbtRunRecord {
                        initialization_records: 1,
                    })
                }
                Some("native_second") => {
                    second.dbt = Some(DbtRunRecord {
                        initialization_records: 1,
                    })
                }
                Some("unexpected_socket" | "empty_socket") => {
                    let count = if defect == Some("empty_socket") { 0 } else { 1 };
                    for record in [&mut first, &mut second] {
                        let VerifyRunOutcome::Completed {
                            forwarded_syscalls, ..
                        } = &mut record.outcome
                        else {
                            unreachable!()
                        };
                        *forwarded_syscalls = Some(count);
                    }
                    if defect == Some("empty_socket") {
                        transport = VerificationLogTransport::InGuestSocket;
                    }
                }
                Some("missing_socket") => transport = VerificationLogTransport::InGuestSocket,
                _ => unreachable!(),
            }
            write_saved_comparison_control(&directories[0], &first);
            write_saved_comparison_control(&directories[1], &second);
            fs::write(&report, b"{\"verified\":true}").unwrap();
            let result = compare_with_application(
                [&directories[0], &directories[1]],
                Some(&report),
                VerifyAllow::Success,
                None,
                Some(SavedApplication {
                    name: "saved-common-control",
                    evidence: EvidenceRequirement::Common,
                    transport,
                    native_identity_diagnostic: None,
                }),
                |_, _| panic!("Common saved comparison must not invoke a native decoder"),
            );
            assert_eq!(result.is_ok(), defect.is_none(), "{defect:?}: {result:?}");
            let value = saved_comparison_report(&report);
            assert_eq!(value["verified"], defect.is_none(), "{defect:?}: {value}");
            if defect.is_some() {
                assert_eq!(value["verdict"], "no_result", "{defect:?}: {value}");
            } else {
                assert_eq!(value["verdict"], "matched");
                let ordinary = compare(
                    [&directories[0], &directories[1]],
                    Some(&report),
                    VerifyAllow::Success,
                    None,
                );
                assert!(
                    format!("{:#}", ordinary.unwrap_err())
                        .contains("unknown backend saved-common-control")
                );
                assert_eq!(saved_comparison_report(&report)["verdict"], "no_result");
            }
        }
    }

    #[test]
    fn a_recorded_status_round_trips() {
        for status in [
            ExitStatus::Exited(0),
            ExitStatus::Exited(3),
            ExitStatus::SignaledRaw(9, false),
            ExitStatus::SignaledRaw(11, true),
        ] {
            let recorded = recorded_status(status);
            let back = exit_status(recorded);
            assert_eq!(back.code(), status.code(), "{status:?}");
            assert_eq!(back.signal(), status.signal(), "{status:?}");
            assert_eq!(recorded_status(back), recorded, "{status:?}");
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
                guest_deadlock: false,
                virtual_timeout_ns: None,
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
