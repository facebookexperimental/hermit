/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Which CPU profile each e2e cell's Hermit runs presented, and the CI image
//! they ran on.
//!
//! With no profile named, Hermit presents x86-64-v3 where the run and host
//! qualify and x86-64-v2 elsewhere, so a CI run must say which it got. Hermit
//! already records that in each run's manifest (`cpu_profile`: the profile,
//! why, and a refusal's words; `inputs.host_drawn["cpu-profile"]` when the
//! host decided). The runner points every Hermit invocation of a cell at one
//! data directory of the cell's ([`stage_dir`]), asks every `hermit run` for
//! a manifest ([`MANIFEST_ALWAYS_ARG`]), and moves that directory to the
//! attempt's own ([`data_dir`]) when the attempt's process ends
//! ([`retain_stage`]). It copies the choice out of those manifests into
//! `cpu-profile.json` beside them ([`record_attempt`]), and combines the
//! attempts of one result row ([`record_runs`]). Nothing is derived again: a
//! run that left no manifest (one refused before it started, exit 122) or
//! runs that disagree are recorded as unresolved, by name, never as a guessed
//! profile.
//!
//! The variable names a path that is the same for every attempt of a cell
//! because a guest can see it: Hermit hands its own environment to the guest
//! under its default `--base-env=host`, which a custom cell outside the
//! hermetic lane runs with, and the runs a custom cell's assertions compare
//! are its attempts. Every other cell's guest gets the runner's fixed
//! environment under `--base-env=minimal`.
//!
//! Per run of the runner, `cpu-profiles.jsonl` holds one row per result row
//! and `ci-environment.json` the CI image and the totals of final rows by
//! profile and reason ([`Totals`]), beside `results.jsonl`. No existing reader
//! opens either file. The last line the runner prints is [`Totals::summary`].

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value as JsonValue;

/// The variable that names where Hermit writes a run's manifest.
pub const DATA_DIR_ENV: &str = "HERMIT_DATA_DIR";
/// The `hermit run` flag that writes a manifest for every run, reproducible
/// or not. A flag rather than `HERMIT_MANIFEST`, so no variable of its own
/// reaches a guest that inherits Hermit's environment. `record start
/// --verify-run`, which replay cells run, writes no manifest
/// ([`REPLAY_WITHOUT_MANIFEST`]).
pub const MANIFEST_ALWAYS_ARG: &str = "--manifest=always";
/// Why a replay row's record names no profile: its `record start
/// --verify-run` runs write no manifest, so there is nothing to copy.
pub const REPLAY_WITHOUT_MANIFEST: &str = "record start --verify-run writes no manifest";
/// The file [`record_attempt`] writes into an attempt's data directory.
pub const RECORD_FILE: &str = "cpu-profile.json";
/// The per-attempt rows, beside `results.jsonl`.
pub const ROWS_FILE: &str = "cpu-profiles.jsonl";
/// The run's CI image and totals, beside `results.jsonl`.
pub const ENVIRONMENT_FILE: &str = "ci-environment.json";

/// The data directory of attempt `attempt` of the cell whose directory is
/// `cell_dir`: the manifests every Hermit run of that attempt wrote end up
/// under it ([`retain_stage`]).
pub fn data_dir(cell_dir: &Path, attempt: &str) -> PathBuf {
    cell_dir.join("hermit-data").join(attempt)
}

/// The directory every Hermit run of the cell whose directory is `cell_dir`
/// writes its manifest into while an attempt runs ([`DATA_DIR_ENV`]): the
/// same path for every attempt of the cell.
pub fn stage_dir(cell_dir: &Path) -> PathBuf {
    cell_dir.join("hermit-data-stage")
}

/// The stage and the attempt's own data directory of the attempt that ran as
/// `attempt` with environment `env`, or `None` where it invoked no Hermit (a
/// naked cell, whose environment names no [`DATA_DIR_ENV`]).
pub fn attempt_dirs(env: &BTreeMap<String, String>, attempt: &str) -> Option<(PathBuf, PathBuf)> {
    let stage = PathBuf::from(env.get(DATA_DIR_ENV)?);
    let retained = data_dir(stage.parent()?, attempt);
    Some((stage, retained))
}

/// Before an attempt: removes a stage an interrupted attempt left and the
/// attempt's own data directory from an earlier run into the same result
/// root, as the runner resets the attempt's tmp, workdir and verify-run
/// directories, so no earlier manifest is read as this attempt's.
pub fn reset_attempt(stage: &Path, retained: &Path) -> Result<(), String> {
    for dir in [stage, retained] {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot reset {}: {error}", dir.display())),
        }
    }
    Ok(())
}

/// After an attempt's process ends, however it ended: moves the stage, where
/// the attempt's Hermit runs wrote their manifests, to the attempt's own data
/// directory. Where no run created the stage (one refused before it started)
/// there is nothing to move, and the record says so ([`record_attempt`]).
pub fn retain_stage(stage: &Path, retained: &Path) -> Result<(), String> {
    if !stage
        .try_exists()
        .map_err(|error| format!("cannot inspect {}: {error}", stage.display()))?
    {
        return Ok(());
    }
    if let Some(parent) = retained.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    std::fs::rename(stage, retained).map_err(|error| {
        format!(
            "cannot move {} to {}: {error}",
            stage.display(),
            retained.display()
        )
    })
}

/// What one attempt's runs presented, copied from their manifests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Record {
    pub schema: u32,
    /// Whether every manifest that records a choice records the same one.
    /// False where no run left a manifest with a choice, or runs disagree;
    /// then `profile` and `reason` are absent and `unresolved` says why.
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `inputs.host_drawn["cpu-profile"]`: the reason, where the host decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_drawn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<String>,
    /// The manifests read, relative to the data directory, and the choice
    /// each recorded (`null` for one that recorded none).
    pub manifests: BTreeMap<String, JsonValue>,
}

/// The choice one manifest records: `cpu_profile` and the profile's
/// `host_drawn` entry, exactly as written.
fn choice_of(manifest: &JsonValue) -> Option<JsonValue> {
    let choice = manifest.get("cpu_profile")?.as_object()?;
    let mut copied = serde_json::Map::new();
    for key in ["profile", "reason", "detail"] {
        if let Some(value) = choice.get(key) {
            copied.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(drawn) = manifest
        .pointer("/inputs/host_drawn/cpu-profile")
        .filter(|value| !value.is_null())
    {
        copied.insert("host_drawn".to_owned(), drawn.clone());
    }
    Some(JsonValue::Object(copied))
}

/// The record for the manifests under `data_dir` (`*/manifest.json`).
pub fn read_record(data_dir: &Path) -> Record {
    let mut manifests = BTreeMap::new();
    collect_manifests(data_dir, "", &mut manifests);
    resolve(manifests)
}

/// Adds the choice of each manifest under `data_dir` to `manifests`, named
/// `<prefix><id>/manifest.json` (`null` for one that records none).
fn collect_manifests(data_dir: &Path, prefix: &str, manifests: &mut BTreeMap<String, JsonValue>) {
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let path = entry.path().join("manifest.json");
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let name = format!(
                "{prefix}{}/manifest.json",
                entry.file_name().to_string_lossy()
            );
            let choice = serde_json::from_slice::<JsonValue>(&bytes)
                .ok()
                .and_then(|manifest| choice_of(&manifest));
            manifests.insert(name, choice.unwrap_or(JsonValue::Null));
        }
    }
}

/// The record of `manifests`: resolved only where at least one records a
/// choice and every one that does records the same.
fn resolve(manifests: BTreeMap<String, JsonValue>) -> Record {
    let mut distinct: Vec<JsonValue> = Vec::new();
    for choice in manifests.values().filter(|choice| !choice.is_null()) {
        if !distinct.contains(choice) {
            distinct.push(choice.clone());
        }
    }
    let field = |choice: &JsonValue, key: &str| {
        choice
            .get(key)
            .and_then(JsonValue::as_str)
            .map(str::to_owned)
    };
    let unresolved = |why: &str, manifests| Record {
        schema: 1,
        resolved: false,
        profile: None,
        reason: None,
        detail: None,
        host_drawn: None,
        unresolved: Some(why.to_owned()),
        manifests,
    };
    match &distinct[..] {
        [] if manifests.is_empty() => unresolved("no run manifest", manifests),
        [] => unresolved("no manifest records a CPU profile", manifests),
        [choice] => Record {
            schema: 1,
            resolved: true,
            profile: field(choice, "profile"),
            reason: field(choice, "reason"),
            detail: field(choice, "detail"),
            host_drawn: field(choice, "host_drawn"),
            unresolved: None,
            manifests,
        },
        _ => unresolved(
            "the runs' manifests record different CPU profiles",
            manifests,
        ),
    }
}

/// Writes `data_dir/cpu-profile.json` from the manifests there and returns
/// the record. The file is written for every attempt that invoked Hermit,
/// creating `data_dir` where no run did (a run refused before it started
/// creates none), so its absence never stands for a profile. An unwritable
/// file is reported in the record, never hidden.
pub fn record_attempt(data_dir: &Path) -> Record {
    let mut record = read_record(data_dir);
    let bytes = serde_json::to_vec_pretty(&record).expect("a record serializes");
    let written = std::fs::create_dir_all(data_dir)
        .and_then(|()| std::fs::write(data_dir.join(RECORD_FILE), bytes));
    if let Err(error) = written {
        record.resolved = false;
        record.profile = None;
        record.reason = None;
        record.detail = None;
        record.host_drawn = None;
        record.unresolved = Some(format!("{RECORD_FILE} could not be written: {error}"));
    }
    record
}

/// The record of one result row, whose attempts ran as `runs` (each its
/// label and data directory): [`record_attempt`] for each, so each attempt's
/// directory holds its own `cpu-profile.json`, then their manifests together,
/// named `<label>/<id>/manifest.json`. A chaos row runs one attempt per seed
/// and a custom row one per run.
pub fn record_runs(runs: &[(String, PathBuf)]) -> Record {
    let mut manifests = BTreeMap::new();
    let mut unwritable = None;
    for (label, dir) in runs {
        let record = record_attempt(dir);
        if unwritable.is_none() {
            unwritable = record
                .unresolved
                .clone()
                .filter(|why| why.starts_with(RECORD_FILE));
        }
        for (name, choice) in record.manifests {
            manifests.insert(format!("{label}/{name}"), choice);
        }
    }
    let mut record = resolve(manifests);
    if let Some(why) = unwritable {
        record.resolved = false;
        record.profile = None;
        record.reason = None;
        record.detail = None;
        record.host_drawn = None;
        record.unresolved = Some(why);
    }
    record
}

/// The record of one result row of mode `mode` ([`record_runs`]). A replay
/// row whose runs left no manifest says why: `record start --verify-run`
/// writes none ([`REPLAY_WITHOUT_MANIFEST`]).
pub fn record_row(mode: &str, runs: &[(String, PathBuf)]) -> Record {
    let mut record = record_runs(runs);
    if mode == "replay" && record.unresolved.as_deref() == Some("no run manifest") {
        record.unresolved = Some(REPLAY_WITHOUT_MANIFEST.to_owned());
    }
    record
}

/// The record of result row `row` ([`record_row`]) over every attempt the
/// row ran, or `None` where none invoked Hermit (no attempt's environment
/// names [`DATA_DIR_ENV`]). The row's own copy of the environment is its
/// first attempt's, so reading only that would record the first of a chaos
/// row's seeds or a custom row's runs and none of the others.
pub fn record_result_row(row: &crate::runner::CellResult) -> Option<Record> {
    let runs: Vec<(String, PathBuf)> = row
        .attempts
        .iter()
        .filter_map(|attempt| {
            attempt_dirs(&attempt.env, &attempt.index)
                .map(|(_, retained)| (attempt.index.clone(), retained))
        })
        .collect();
    (!runs.is_empty()).then(|| record_row(&row.mode, &runs))
}

/// The CI image of the environment the cells ran in, from `/etc/os-release`'s
/// text: `centos9`, `el10`, or `other:<ID>-<VERSION_ID>`, with the fields it
/// was read from.
pub fn ci_image(os_release: &str) -> (String, BTreeMap<String, String>) {
    let mut fields = BTreeMap::new();
    for line in os_release.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if matches!(key, "ID" | "VERSION_ID" | "PLATFORM_ID") {
                fields.insert(key.to_owned(), value.trim().trim_matches('"').to_owned());
            }
        }
    }
    let get = |key: &str| fields.get(key).map(String::as_str).unwrap_or("");
    let major = get("VERSION_ID").split('.').next().unwrap_or("");
    let image = match (get("ID"), major, get("PLATFORM_ID")) {
        ("centos", "9", _) => "centos9".to_owned(),
        (_, _, "platform:el10") => "el10".to_owned(),
        (id, _, _) => format!("other:{id}-{}", get("VERSION_ID")),
    };
    (image, fields)
}

/// The final attempts' records of one run of the runner, counted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub cells_by_profile: BTreeMap<String, u64>,
    /// Keyed `<profile>:<reason>`.
    pub cells_by_reason: BTreeMap<String, u64>,
    /// Cells whose Hermit runs left no agreeing record ([`Record::unresolved`]).
    pub unresolved: u64,
    /// Replay cells, whose runs write no manifest ([`REPLAY_WITHOUT_MANIFEST`]).
    pub replay_without_manifest: u64,
    /// Cells that ran no Hermit (naked, host-inapplicable, or never started).
    pub without_hermit: u64,
}

impl Totals {
    /// Counts a cell's final attempt: its record, or `None` where it ran no
    /// Hermit.
    pub fn count(&mut self, record: Option<&Record>) {
        match record {
            None => self.without_hermit += 1,
            Some(Record {
                resolved: true,
                profile: Some(profile),
                reason,
                ..
            }) => {
                *self.cells_by_profile.entry(profile.clone()).or_default() += 1;
                let reason = reason.as_deref().unwrap_or("unknown");
                *self
                    .cells_by_reason
                    .entry(format!("{profile}:{reason}"))
                    .or_default() += 1;
            }
            Some(record) if record.unresolved.as_deref() == Some(REPLAY_WITHOUT_MANIFEST) => {
                self.replay_without_manifest += 1
            }
            Some(_) => self.unresolved += 1,
        }
    }

    /// The runner's last line: each profile with its count and reasons, then
    /// the unresolved, replay and Hermit-less cells, then the CI image.
    pub fn summary(&self, image: &str) -> String {
        let profiles = self
            .cells_by_profile
            .iter()
            .map(|(profile, count)| {
                let reasons = self
                    .cells_by_reason
                    .iter()
                    .filter_map(|(key, n)| {
                        key.strip_prefix(&format!("{profile}:"))
                            .map(|reason| format!("{reason} {n}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{profile} {count} ({reasons})")
            })
            .collect::<Vec<_>>();
        let profiles = if profiles.is_empty() {
            "none recorded".to_owned()
        } else {
            profiles.join("; ")
        };
        format!(
            "test-harness: cpu profile: {profiles}; unresolved {}; replay without a manifest {}; \
             without Hermit {}; ci image {image}",
            self.unresolved, self.replay_without_manifest, self.without_hermit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty directory of this test's own (the crate's tests use no
    /// temporary-directory crate).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cpu-profile-record-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(dir: &Path, id: &str, manifest: JsonValue) {
        std::fs::create_dir_all(dir.join(id)).unwrap();
        std::fs::write(
            dir.join(id).join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    /// A manifest as Hermit writes one for a profile auto chose by the host.
    fn admitted() -> JsonValue {
        serde_json::json!({
            "schema": "hermit-manifest/1",
            "kind": "inputs-only",
            "inputs": {"run": {"cpu-profile": "x86-64-v3"}, "host_drawn": {"cpu-profile": "admitted"}},
            "cpu_profile": {"profile": "x86-64-v3", "reason": "admitted"},
            "cpu_profile_residual": "With --cpu-profile x86-64-v3, ...",
        })
    }

    /// cpu-profile.json copies the manifests' choice field for field:
    /// `cpu_profile.profile`, `.reason`, `.detail` and
    /// `inputs.host_drawn["cpu-profile"]`, and names each manifest read.
    #[test]
    fn the_record_copies_the_manifests_choice() {
        let dir = scratch("copies");
        write_manifest(&dir, "run1", admitted());
        write_manifest(&dir, "run2", admitted());
        let record = record_attempt(&dir);
        let written: JsonValue =
            serde_json::from_slice(&std::fs::read(dir.join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "schema": 1,
                "resolved": true,
                "profile": "x86-64-v3",
                "reason": "admitted",
                "host_drawn": "admitted",
                "manifests": {
                    "run1/manifest.json": {"profile": "x86-64-v3", "reason": "admitted", "host_drawn": "admitted"},
                    "run2/manifest.json": {"profile": "x86-64-v3", "reason": "admitted", "host_drawn": "admitted"},
                },
            })
        );
        assert_eq!(serde_json::to_value(&record).unwrap(), written);

        // A refusal's words come through as `detail`; a choice the run's own
        // request made is not host-drawn.
        let dir = scratch("refusal-words");
        write_manifest(
            &dir,
            "only",
            serde_json::json!({
                "kind": "history",
                "inputs": {"host_drawn": {}},
                "cpu_profile": {
                    "profile": "x86-64-v2",
                    "reason": "request-not-admissible",
                    "detail": "only the ptrace backend is qualified",
                },
            }),
        );
        let record = read_record(&dir);
        assert!(record.resolved);
        assert_eq!(record.profile.as_deref(), Some("x86-64-v2"));
        assert_eq!(record.reason.as_deref(), Some("request-not-admissible"));
        assert_eq!(
            record.detail.as_deref(),
            Some("only the ptrace backend is qualified")
        );
        assert_eq!(record.host_drawn, None);
    }

    /// Never a guessed profile: no manifest (a run refused before it
    /// started), a manifest without a choice (an older Hermit), and runs that
    /// disagree are each unresolved, by name, with no profile.
    #[test]
    fn a_record_without_an_agreeing_manifest_is_unresolved_by_name() {
        let empty = scratch("empty");
        let record = record_attempt(&empty);
        assert_eq!(
            serde_json::to_value(&record).unwrap(),
            serde_json::json!({
                "schema": 1,
                "resolved": false,
                "unresolved": "no run manifest",
                "manifests": {},
            })
        );
        assert!(empty.join(RECORD_FILE).is_file());

        let older = scratch("older");
        write_manifest(&older, "a", serde_json::json!({"kind": "history"}));
        let record = read_record(&older);
        assert_eq!(
            (
                record.resolved,
                record.profile,
                record.unresolved.as_deref()
            ),
            (false, None, Some("no manifest records a CPU profile"))
        );

        let split = scratch("split");
        write_manifest(&split, "a", admitted());
        write_manifest(
            &split,
            "b",
            serde_json::json!({
                "inputs": {"host_drawn": {"cpu-profile": "qualification-refused"}},
                "cpu_profile": {"profile": "x86-64-v2", "reason": "qualification-refused", "detail": "x"},
            }),
        );
        let record = read_record(&split);
        assert_eq!(
            (
                record.resolved,
                record.profile,
                record.unresolved.as_deref()
            ),
            (
                false,
                None,
                Some("the runs' manifests record different CPU profiles")
            )
        );
        assert_eq!(record.manifests.len(), 2);

        // A run refused before it started leaves no data directory at all:
        // the record is written anyway, unresolved by name.
        let missing = empty.join("no-such-attempt");
        let record = record_attempt(&missing);
        assert_eq!(record.unresolved.as_deref(), Some("no run manifest"));
        let written: JsonValue =
            serde_json::from_slice(&std::fs::read(missing.join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!(written["resolved"], false);
        assert_eq!(written.get("profile"), None);
    }

    #[test]
    fn the_ci_image_comes_from_os_release() {
        let (image, fields) = ci_image(
            "NAME=\"CentOS Stream\"\nID=\"centos\"\nVERSION_ID=\"9\"\nPLATFORM_ID=\"platform:el9\"\n",
        );
        assert_eq!(image, "centos9");
        assert_eq!(fields["PLATFORM_ID"], "platform:el9");
        assert_eq!(
            ci_image("ID=\"centos\"\nVERSION_ID=\"10\"\nPLATFORM_ID=\"platform:el10\"\n").0,
            "el10"
        );
        assert_eq!(
            ci_image("ID=ubuntu\nVERSION_ID=\"24.04\"\n").0,
            "other:ubuntu-24.04"
        );
    }

    /// The summary line names each profile with its reasons, and the cells
    /// that resolved none or ran no Hermit, so none is hidden.
    #[test]
    fn the_summary_counts_final_attempts_by_profile_and_reason() {
        let resolved = |profile: &str, reason: &str| Record {
            schema: 1,
            resolved: true,
            profile: Some(profile.to_owned()),
            reason: Some(reason.to_owned()),
            detail: None,
            host_drawn: None,
            unresolved: None,
            manifests: BTreeMap::new(),
        };
        let mut totals = Totals::default();
        for record in [
            resolved("x86-64-v3", "admitted"),
            resolved("x86-64-v3", "admitted"),
            resolved("x86-64-v2", "request-not-admissible"),
            resolved("x86-64-v2", "declared"),
        ] {
            totals.count(Some(&record));
        }
        totals.count(Some(&read_record(Path::new("/nonexistent/attempt"))));
        totals.count(Some(&record_row("replay", &[])));
        totals.count(None);
        assert_eq!(
            totals.summary("centos9"),
            "test-harness: cpu profile: x86-64-v2 2 (declared 1, request-not-admissible 1); \
             x86-64-v3 2 (admitted 2); unresolved 1; replay without a manifest 1; \
             without Hermit 1; ci image centos9"
        );
        assert_eq!(
            Totals::default().summary("el10"),
            "test-harness: cpu profile: none recorded; unresolved 0; replay without a manifest 0; \
             without Hermit 0; ci image el10"
        );
    }

    /// A row's attempts share the cell's stage and keep records of their own:
    /// [`attempt_dirs`] names the stage from the environment and the
    /// attempt's directory beside it; [`retain_stage`] moves the stage there;
    /// [`record_runs`] writes each attempt's `cpu-profile.json` and resolves
    /// the row from all their manifests, named by attempt; a replay row with
    /// none says why ([`record_row`]).
    #[test]
    fn a_rows_attempts_share_one_stage_and_keep_records_of_their_own() {
        let cell = scratch("row");
        let stage = stage_dir(&cell);
        let env = BTreeMap::from([(
            DATA_DIR_ENV.to_owned(),
            stage.to_string_lossy().into_owned(),
        )]);
        let mut runs = Vec::new();
        for (attempt, id) in [("seed-1", "a"), ("seed-2", "b")] {
            let (named_stage, retained) = attempt_dirs(&env, attempt).unwrap();
            assert_eq!(named_stage, stage);
            assert_eq!(retained, data_dir(&cell, attempt));
            reset_attempt(&named_stage, &retained).unwrap();
            write_manifest(&stage, id, admitted());
            retain_stage(&named_stage, &retained).unwrap();
            assert!(!stage.exists());
            runs.push((attempt.to_owned(), retained));
        }
        assert_eq!(attempt_dirs(&BTreeMap::new(), "1"), None);
        // A third attempt that wrote nothing leaves no directory to move.
        let (_, nothing) = attempt_dirs(&env, "seed-3").unwrap();
        retain_stage(&stage, &nothing).unwrap();
        assert!(!nothing.exists());
        runs.push(("seed-3".to_owned(), nothing.clone()));

        let record = record_row("chaos", &runs);
        assert!(record.resolved);
        assert_eq!(
            (record.profile.as_deref(), record.reason.as_deref()),
            (Some("x86-64-v3"), Some("admitted"))
        );
        assert_eq!(
            record.manifests.keys().collect::<Vec<_>>(),
            ["seed-1/a/manifest.json", "seed-2/b/manifest.json"]
        );
        // Each attempt's directory holds its own record, the empty one too.
        for (attempt, resolved) in [("seed-1", true), ("seed-2", true), ("seed-3", false)] {
            let written: JsonValue = serde_json::from_slice(
                &std::fs::read(data_dir(&cell, attempt).join(RECORD_FILE)).unwrap(),
            )
            .unwrap();
            assert_eq!(written["resolved"], resolved, "{attempt}");
        }
        // Attempts that disagree leave the row unresolved.
        let other = data_dir(&cell, "seed-4");
        write_manifest(
            &other,
            "c",
            serde_json::json!({"cpu_profile": {"profile": "x86-64-v2", "reason": "qualification-refused"}}),
        );
        runs.push(("seed-4".to_owned(), other));
        assert_eq!(
            record_row("chaos", &runs).unresolved.as_deref(),
            Some("the runs' manifests record different CPU profiles")
        );

        // A replay row's record start writes no manifest: unresolved, named.
        let replay = scratch("replay-row");
        let runs = [("1".to_owned(), data_dir(&replay, "1"))];
        let record = record_row("replay", &runs);
        assert_eq!(
            (
                record.resolved,
                record.profile,
                record.unresolved.as_deref()
            ),
            (false, None, Some(REPLAY_WITHOUT_MANIFEST))
        );
        assert_eq!(
            record_row("verify", &runs).unresolved.as_deref(),
            Some("no run manifest")
        );
        // A reset removes a leftover stage and the attempt's old directory.
        std::fs::create_dir_all(stage.join("stale")).unwrap();
        reset_attempt(&stage, &data_dir(&cell, "seed-1")).unwrap();
        assert!(!stage.exists() && !data_dir(&cell, "seed-1").exists());
    }
}
