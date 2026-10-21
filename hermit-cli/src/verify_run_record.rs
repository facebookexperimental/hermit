/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Producer-owned wire record and raw streams for one retained execution.
//!
//! The CLI writes the final record after the output files. Both the CLI
//! comparator and the manifest runner read this schema; policy stays with
//! those consumers.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::canonical_verdict::BranchCounterVerdict;
use crate::canonical_verdict::ContainerDisposition;
use crate::canonical_verdict::LogCompareStrictness;
use crate::canonical_verdict::RunFailure;

/// The record of how the run ended, written last: a directory without it is
/// an execution that did not finish writing its evidence.
pub const RECORD: &str = "verify-run.json";
/// The run's verification log, the log `run --verify` compares.
pub const LOG: &str = "run.log";
/// The guest's standard output.
pub const STDOUT: &str = "stdout";
/// The guest's standard error.
pub const STDERR: &str = "stderr";
/// The run's summary (`--summary-json`), when Detcore wrote one.
pub const SUMMARY: &str = "summary.json";
/// The host files the run's guest opened (`hermit::host_input_change`).
pub const HOST_INPUTS: &str = "host-inputs";
/// The recording a `record start --verify-run` execution made, which a
/// `--verify-replay-of` execution replays.
pub const RECORDING: &str = "recording";
/// The epoch a recording and its replay record: the replay runs at the
/// recording's own epoch, so the pair agrees on it by construction.
pub const RECORDING_EPOCH: &str = "recording";

/// The DBT backend's typed whole-process statistics stream, as DynamoRIO's
/// client wrote it (Reverie decodes it; `backends::require_dbt_stats` applies verification policy).
pub const DBT_STATS: &str = "dbt-stats.bin";

/// The version of [`VerifyRunRecord`].
pub const SCHEMA: u32 = 1;

/// How one `--verify-run` execution ended, and the comparison it was run for.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyRunRecord {
    pub schema: u32,
    /// The backend, as `--backend` spells it.
    pub backend: String,
    pub comparison: RecordedComparison,
    /// Whether the run used `--passthru-opt`, which decides whether a host
    /// file change can be named as a divergence's cause.
    pub passthru_opt: bool,
    /// The virtual-time epoch the run started its clock at (`--epoch`). `run
    /// --verify` gives its two runs one epoch; two separate runs agree only
    /// when the caller gives them the same one.
    pub epoch: String,
    /// `hermit host-capabilities`'s `exact_branch_counter` verdict, for a run
    /// on a backend that reads that counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_branch_counter: Option<BranchCounterVerdict>,
    /// What the DBT backend's comparison reads besides the log, for a DBT run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dbt: Option<DbtRunRecord>,
    /// Which half of which verification this is.
    #[serde(default, skip_serializing_if = "RunRole::is_run")]
    pub role: RunRole,
    pub outcome: VerifyRunOutcome,
}

/// Which half of which verification a `--verify-run` execution is.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
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
pub struct DbtRunRecord {
    /// How many process-image initialization records the run's evidence
    /// held (`DbtEvidence::initialization_records`).
    pub initialization_records: u64,
}

/// The comparison the run's flags select, as `run --verify` derives it
/// (`RunOpts::verification_comparison_options`). Both runs of a comparison
/// must record the same one.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedComparison {
    pub strictness: LogCompareStrictness,
    /// `--verify-verbose`: the all-level diagnostic comparison.
    pub diagnostic_full_trace: bool,
    /// Whether the records carry syscall output-buffer content.
    pub compare_io_buffers: bool,
    pub virtualize_time: bool,
}

/// How one execution ended.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum VerifyRunOutcome {
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
pub enum RecordedStatus {
    Exited { code: i32 },
    Signaled { signal: i32, core_dumped: bool },
}

impl VerifyRunRecord {
    /// Read the final record, refusing incomplete or unsupported evidence.
    pub fn read(directory: &Path) -> Result<Self, String> {
        let path = directory.join(RECORD);
        let bytes = fs::read(&path).map_err(|error| {
            format!(
                "reading {}: a --verify-run directory without its record did not finish: {error}",
                path.display()
            )
        })?;
        let record: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parsing {}: {error}", path.display()))?;
        if record.schema != SCHEMA {
            return Err(format!(
                "{} has schema {}, and this Hermit reads schema {SCHEMA}",
                path.display(),
                record.schema
            ));
        }
        Ok(record)
    }
}

/// The guest's exact output bytes, separate from the producer's diagnostics.
pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Read both streams without treating invalid UTF-8 or missing files as empty.
pub fn read_output(directory: &Path) -> Result<RunOutput, String> {
    let read = |name: &str| {
        let path = directory.join(name);
        fs::read(&path).map_err(|error| format!("reading {}: {error}", path.display()))
    };
    Ok(RunOutput {
        stdout: read(STDOUT)?,
        stderr: read(STDERR)?,
    })
}
