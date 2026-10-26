/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What `hermit analyze` writes into its workspace as it goes, so that
//! `--resume` can continue an analysis a budget (or anything else) stopped.
//!
//! Three files:
//! - [`CHECKPOINT_FILE`]: what the analysis is of ([`Identity`]). A resume
//!   refuses when anything in it differs.
//! - [`JOURNAL_FILE`]: one line per trial that finished, appended as it does
//!   ([`JournalEntry`]): a key naming the trial's inputs, how it ended, and
//!   the files it wrote.
//! - [`PROGRESS_FILE`]: the phase running and the search's current interval
//!   or bracket ([`Progress`]), rewritten after every step.
//!
//! A resumed analysis runs again from its start. Every run whose key is in
//! the journal, and whose files are still there, takes the recorded outcome
//! instead of running (`RunData::launch`). Analyze decides only from trial
//! outcomes and its seed, so the resumed analysis makes exactly the
//! decisions the first one made, and goes on from where it stopped.

use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead as _;
use std::io::BufReader;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use serde::Deserialize;
use serde::Serialize;

/// The analysis's identity, in the workspace.
pub const CHECKPOINT_FILE: &str = "analyze-checkpoint.json";
/// The finished trials, in the workspace.
pub const JOURNAL_FILE: &str = "analyze-journal.jsonl";
/// The search's progress, in the workspace.
pub const PROGRESS_FILE: &str = "analyze-progress.json";
/// The format of all three.
pub const CHECKPOINT_VERSION: u32 = 1;

/// A file, by content.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileIdentity {
    pub path: PathBuf,
    pub sha256: String,
}

impl FileIdentity {
    pub fn of(path: &Path) -> anyhow::Result<Self> {
        let sha256 = detcore::Digest::digest_path(path)
            .with_context(|| format!("cannot read {}", path.display()))?
            .to_string();
        Ok(Self {
            path: path.to_path_buf(),
            sha256,
        })
    }
}

/// What an analysis is of: a resume refuses when any of it differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The hermit binary that ran the analysis.
    pub hermit: FileIdentity,
    /// The guest program, where it could be found on the host.
    pub program: Option<FileIdentity>,
    /// The `hermit run` arguments, `--run-arg` ones first.
    pub run_args: Vec<String>,
    /// The target criteria, by option.
    pub target: Vec<(String, String)>,
    /// The options that steer the search, by option.
    pub search: Vec<(String, String)>,
    /// The record files given as runs (`--run1-preemptions`,
    /// `--run2-preemptions`), by option.
    pub records: Vec<(String, FileIdentity)>,
    /// The seed of analyze's own random choices: `--analyze-seed`, or the
    /// one the first invocation drew.
    pub analyze_seed: u64,
}

impl Identity {
    /// What differs between this recorded identity and `now`, one line each:
    /// empty when nothing does.
    pub fn differences(&self, now: &Identity) -> Vec<String> {
        let mut differ = Vec::new();
        if self.hermit.sha256 != now.hermit.sha256 {
            differ.push(format!(
                "the hermit binary: recorded {} (sha256 {}), now {} (sha256 {})",
                self.hermit.path.display(),
                self.hermit.sha256,
                now.hermit.path.display(),
                now.hermit.sha256
            ));
        }
        match (&self.program, &now.program) {
            (Some(recorded), Some(current)) if recorded.sha256 == current.sha256 => {}
            (None, None) => {}
            (recorded, current) => {
                let describe = |program: &Option<FileIdentity>| match program {
                    Some(program) => {
                        format!("{} (sha256 {})", program.path.display(), program.sha256)
                    }
                    None => "not found on the host".to_owned(),
                };
                differ.push(format!(
                    "the guest program: recorded {}, now {}",
                    describe(recorded),
                    describe(current)
                ));
            }
        }
        if self.run_args != now.run_args {
            differ.push(format!(
                "the run arguments: recorded {}, now {}",
                shell_words::join(&self.run_args),
                shell_words::join(&now.run_args)
            ));
        }
        for (what, recorded, current) in [
            ("target", &self.target, &now.target),
            ("search option", &self.search, &now.search),
        ] {
            for (option, value) in recorded {
                let now_value = current
                    .iter()
                    .find(|(name, _)| name == option)
                    .map(|(_, value)| value);
                if now_value != Some(value) {
                    differ.push(format!(
                        "the {what} {option}: recorded {value}, now {}",
                        now_value.map_or("unset", String::as_str)
                    ));
                }
            }
        }
        if self.records != now.records {
            let describe = |records: &Vec<(String, FileIdentity)>| {
                if records.is_empty() {
                    return "none".to_owned();
                }
                records
                    .iter()
                    .map(|(option, file)| {
                        format!("{option}={} (sha256 {})", file.path.display(), file.sha256)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            differ.push(format!(
                "the records and schedules given as runs: recorded {}, now {}",
                describe(&self.records),
                describe(&now.records)
            ));
        }
        if self.analyze_seed != now.analyze_seed {
            differ.push(format!(
                "--analyze-seed: recorded {}, now {}",
                self.analyze_seed, now.analyze_seed
            ));
        }
        differ
    }
}

/// [`CHECKPOINT_FILE`]'s contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    /// `hermit --version` of the first invocation, for the reader.
    pub hermit_version: String,
    pub identity: Identity,
}

impl Checkpoint {
    pub fn write(&self, workspace: &Path) -> anyhow::Result<()> {
        write_atomically(
            &workspace.join(CHECKPOINT_FILE),
            &serde_json::to_vec_pretty(self)?,
        )
    }

    /// The checkpoint of the workspace `workspace`, which `--resume` names.
    pub fn read(workspace: &Path) -> anyhow::Result<Self> {
        let path = workspace.join(CHECKPOINT_FILE);
        let text = std::fs::read(&path).with_context(|| {
            format!(
                "--resume={} names no hermit analyze workspace with a checkpoint: cannot read {}. \
                 Pass the workspace a partial report named",
                workspace.display(),
                path.display()
            )
        })?;
        let checkpoint: Self = serde_json::from_slice(&text).with_context(|| {
            format!(
                "cannot parse the checkpoint {}: it is damaged, or not hermit analyze's",
                path.display()
            )
        })?;
        if checkpoint.version != CHECKPOINT_VERSION {
            anyhow::bail!(
                "the checkpoint {} has format version {}, and this hermit reads version {}. \
                 Resume it with the hermit that wrote it ({})",
                path.display(),
                checkpoint.version,
                CHECKPOINT_VERSION,
                checkpoint.hermit_version
            );
        }
        Ok(checkpoint)
    }
}

/// Write `bytes` to `path` so that a reader sees the old contents or the
/// new, never part of them.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let partial = path.with_extension("partial");
    let mut file =
        File::create(&partial).with_context(|| format!("cannot create {}", partial.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&partial, path)
        .with_context(|| format!("cannot rename {} to {}", partial.display(), path.display()))
}

/// How a journaled trial ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum RecordedEnd {
    /// It finished; `matched` says whether it met the target criteria.
    Finished { matched: bool },
    /// Its own `--timeout` (the trial bound, never the budget's) ended it.
    Timeout,
    /// Hermit refused it at an operation its guest reached.
    Refusal,
}

/// A file a trial wrote, relative to the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedFile {
    pub path: PathBuf,
    pub sha256: String,
}

/// One line of [`JOURNAL_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub run: String,
    /// Names the trial's inputs: its name, its `hermit run` command, and the
    /// contents of the schedule or record it replays.
    pub key: String,
    pub end: RecordedEnd,
    /// How long it ran, in milliseconds, if it finished.
    pub wall_ms: Option<u64>,
    /// The size of the trial's own log file, if it wrote one.
    pub log_bytes: Option<u64>,
    /// The files it wrote that analyze reads afterwards.
    pub outputs: Vec<RecordedFile>,
    /// The schedule it recorded, which holds exactly the events of the
    /// schedule it replayed: when analyze has since removed the recording,
    /// a copy of the replayed schedule (written again by the resumed
    /// analysis before it asks) stands in for it. `(recorded, replayed)`.
    #[serde(default)]
    pub recording_replays_input: Option<(PathBuf, PathBuf)>,
}

struct JournalState {
    /// The journal file, when the journal is on.
    file: Option<File>,
    entries: HashMap<String, JournalEntry>,
    /// Trials answered from the journal in this invocation.
    answered: u64,
}

/// The journal of finished trials. Cloning shares it; the default is off
/// (`hermit bisect`, tests).
#[derive(Clone)]
pub struct Journal {
    state: Arc<Mutex<JournalState>>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(JournalState {
                file: None,
                entries: HashMap::new(),
                answered: 0,
            })),
        }
    }
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().unwrap();
        f.debug_struct("Journal")
            .field("on", &state.file.is_some())
            .field("entries", &state.entries.len())
            .finish()
    }
}

impl Journal {
    /// Open the journal in `workspace`, reading the entries already there
    /// (a resume), and append to it from now on. A line the previous
    /// invocation was writing when it died is ignored.
    pub fn open(workspace: &Path) -> anyhow::Result<Self> {
        let path = workspace.join(JOURNAL_FILE);
        let mut entries = HashMap::new();
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file).lines() {
                let line = line.with_context(|| format!("cannot read {}", path.display()))?;
                if let Ok(entry) = serde_json::from_str::<JournalEntry>(&line) {
                    entries.insert(entry.key.clone(), entry);
                }
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // A line cut short by a crash would otherwise run into the next.
        if std::fs::metadata(&path)?.len() > 0 && !std::fs::read(&path)?.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        Ok(Self {
            state: Arc::new(Mutex::new(JournalState {
                file: Some(file),
                entries,
                answered: 0,
            })),
        })
    }

    pub fn is_on(&self) -> bool {
        self.state.lock().unwrap().file.is_some()
    }

    /// How many entries the journal holds.
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    pub fn lookup(&self, key: &str) -> Option<JournalEntry> {
        self.state.lock().unwrap().entries.get(key).cloned()
    }

    /// Count a trial answered from the journal.
    pub fn note_answered(&self) {
        self.state.lock().unwrap().answered += 1;
    }

    /// Trials answered from the journal in this invocation.
    pub fn answered(&self) -> u64 {
        self.state.lock().unwrap().answered
    }

    /// Append `entry` and make it durable before the analysis reads the
    /// trial's outcome.
    pub fn record(&self, entry: JournalEntry) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        let Some(file) = state.file.as_mut() else {
            return Ok(());
        };
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_data()?;
        state.entries.insert(entry.key.clone(), entry);
        Ok(())
    }
}

/// Which part of the analysis is running.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    #[default]
    Starting,
    TargetRun,
    SearchForTarget,
    Minimize,
    Selfcheck,
    TargetSchedule,
    Baseline,
    Bisect,
    PrefixSearch,
    StackTraces,
}

impl Phase {
    pub fn describe(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::TargetRun => "running the target run",
            Phase::SearchForTarget => "searching for a run that meets the target (--search)",
            Phase::Minimize => "minimizing the target run's interventions (--minimize)",
            Phase::Selfcheck => "checking that the target run replays (--selfcheck)",
            Phase::TargetSchedule => "recording the target run's schedule",
            Phase::Baseline => "choosing the baseline run",
            Phase::Bisect => "bisecting between the passing and the failing schedule",
            Phase::PrefixSearch => "searching the target schedule's prefixes (--prefix-search)",
            Phase::StackTraces => "taking the stack traces of the events found",
        }
    }
}

/// The default search's current interval: a schedule whose replay misses
/// the target and one whose replay meets it, both in the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BisectInterval {
    /// The search pass at whose start this was the interval; none for the
    /// endpoint runs, before the search replayed them.
    pub pass: Option<usize>,
    /// How far apart the two are, once a pass measured it.
    pub edit_distance: Option<usize>,
    pub swap_distance: Option<usize>,
    /// The schedule whose replay misses the target.
    pub passing: PathBuf,
    pub passing_events: usize,
    /// The schedule whose replay meets the target.
    pub failing: PathBuf,
    pub failing_events: usize,
    /// How many events the two share from the start: the first event at
    /// which they differ, the candidate deciding event, has this index.
    pub common_prefix: usize,
    /// That event in the passing schedule and in the failing one, if each
    /// has one.
    pub passing_event: Option<String>,
    pub failing_event: Option<String>,
    /// What a resumed analysis needs to go on from here, once the search
    /// started its passes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<IntervalResume>,
}

/// Where the default search stood at an interval: what a resumed analysis
/// restarts the search from (`SearchStart::Interval`), rather than
/// replaying its passes, whose trials need not keep their schedules. The
/// digests are sha256s of the schedules' events (their lengths and events
/// as `std::hash::Hash` writes them).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IntervalResume {
    /// The search's two endpoints, which the interval narrows: a resumed
    /// search between other endpoints starts over.
    pub endpoints: String,
    /// The schedules in `passing` and `failing`: a file that does not hold
    /// what the interval names (a write the analysis did not finish) is
    /// not restarted from.
    pub passing_hash: String,
    pub failing_hash: String,
    /// Whether the event-level search had the interval, rather than the
    /// alignment search (--run-needleman).
    pub event_level: bool,
    /// How many trials the search had started, so the next is named as the
    /// uninterrupted search names it, and how many of them a bound stopped
    /// or Hermit refused, for the warnings the result carries.
    pub trials: usize,
    pub unfinished: usize,
    pub refused: usize,
}

/// `--prefix-search`'s current bracket. Short prefixes miss the target and
/// long ones meet it: replaying the first `misses` events (then scheduling
/// without chaos) finished and missed, replaying the first `meets` events
/// met it, so the deciding event is one of events `misses` to `meets - 1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrefixBracket {
    /// The target schedule's length.
    pub events: usize,
    /// The continuation the trials ran at, as the report says it.
    pub continuation: String,
    /// The longest prefix whose last trial finished and missed the target,
    /// and that trial.
    pub misses: Option<usize>,
    pub misses_run: Option<String>,
    /// The shortest prefix whose trial met the target, and that trial.
    pub meets: Option<usize>,
    pub meets_run: Option<String>,
    /// Prefixes in between whose trials a bound stopped or Hermit refused:
    /// not outcomes.
    pub unfinished: Vec<usize>,
}

/// [`PROGRESS_FILE`]'s contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<BisectInterval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bracket: Option<PrefixBracket>,
    /// The search's result, once it has one (the stack traces may follow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<String>,
}

/// The analysis's progress, written to [`PROGRESS_FILE`] on every change
/// once a workspace is set. Cloning shares it.
#[derive(Clone, Default)]
pub struct ProgressHandle {
    state: Arc<Mutex<(Progress, Option<PathBuf>)>>,
}

impl std::fmt::Debug for ProgressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ProgressHandle")
            .field(&self.state.lock().unwrap().0)
            .finish()
    }
}

impl Progress {
    /// The progress an earlier invocation wrote into `workspace`, if it
    /// wrote one that reads.
    pub fn read(workspace: &Path) -> Option<Self> {
        let bytes = std::fs::read(workspace.join(PROGRESS_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

impl ProgressHandle {
    /// Start from where the earlier invocation of a resumed analysis got
    /// to: its interval stays the progress's until this one's search
    /// replaces it, so an analysis stopped again before its search keeps it.
    pub fn carry(&self, earlier: &Progress) {
        self.update(|progress| progress.interval = earlier.interval.clone());
    }

    /// Write the progress into `workspace` from now on.
    pub fn write_into(&self, workspace: &Path) {
        self.state.lock().unwrap().1 = Some(workspace.join(PROGRESS_FILE));
        self.update(|_| {});
    }

    pub fn snapshot(&self) -> Progress {
        self.state.lock().unwrap().0.clone()
    }

    /// Enter `phase`. A search left behind (the default search that falls
    /// back to a prefix search) leaves its interval or bracket behind too:
    /// the progress names only the running search's.
    pub fn phase(&self, phase: Phase) {
        self.update(|progress| {
            progress.phase = phase;
            match phase {
                Phase::Bisect => progress.bracket = None,
                Phase::PrefixSearch => progress.interval = None,
                _ => {}
            }
        });
    }

    pub fn interval(&self, interval: BisectInterval) {
        self.update(|progress| progress.interval = Some(interval));
    }

    pub fn bracket(&self, bracket: PrefixBracket) {
        self.update(|progress| progress.bracket = Some(bracket));
    }

    pub fn decided(&self, decided: String) {
        self.update(|progress| progress.decided = Some(decided));
    }

    /// Change the progress and write it. A write that fails is reported and
    /// does not stop the analysis: a resume falls back on its journal.
    fn update(&self, change: impl FnOnce(&mut Progress)) {
        let mut state = self.state.lock().unwrap();
        change(&mut state.0);
        if let Some(path) = &state.1 {
            let written = serde_json::to_vec_pretty(&state.0)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| write_atomically(path, &bytes));
            if let Err(error) = written {
                crate::app::analyze::workspace::note(&format!(
                    ":: cannot write the analysis's progress to {}: {error:#}",
                    path.display()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(dir: &Path) -> Identity {
        let hermit = dir.join("hermit");
        let program = dir.join("program");
        std::fs::write(&hermit, "hermit build 1").unwrap();
        std::fs::write(&program, "guest build 1").unwrap();
        Identity {
            hermit: FileIdentity::of(&hermit).unwrap(),
            program: Some(FileIdentity::of(&program).unwrap()),
            run_args: vec!["--chaos".into(), program.display().to_string()],
            target: vec![("--target-stdout".into(), "FAILED".into())],
            search: vec![("--prefix-search".into(), "true".into())],
            records: Vec::new(),
            analyze_seed: 7,
        }
    }

    #[test]
    fn a_checkpoint_round_trips_and_names_its_workspace_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            hermit_version: "hermit 0.5.1".into(),
            identity: identity(dir.path()),
        };
        checkpoint.write(dir.path()).unwrap();
        assert_eq!(Checkpoint::read(dir.path()).unwrap(), checkpoint);
        let empty = tempfile::tempdir().unwrap();
        let error = Checkpoint::read(empty.path()).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("names no hermit analyze workspace with a checkpoint: cannot read")
                && format!("{error:#}").contains("Pass the workspace a partial report named"),
            "{error:#}"
        );
        // Another format version names the hermit that reads it.
        Checkpoint {
            version: CHECKPOINT_VERSION + 1,
            ..checkpoint
        }
        .write(dir.path())
        .unwrap();
        let error = Checkpoint::read(dir.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("Resume it with the hermit that wrote it (hermit 0.5.1)"),
            "{error:#}"
        );
    }

    /// Every part of the identity that a resume compares is named when it
    /// differs, with both values.
    #[test]
    fn every_difference_in_the_identity_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = identity(dir.path());
        assert!(recorded.differences(&recorded.clone()).is_empty());

        let mut now = recorded.clone();
        now.hermit.sha256 = "0".repeat(64);
        now.program.as_mut().unwrap().sha256 = "1".repeat(64);
        now.run_args.push("--extra".into());
        now.target[0].1 = "PASSED".into();
        now.search[0].1 = "false".into();
        now.analyze_seed = 8;
        let schedule = dir.path().join("run1.events");
        std::fs::write(&schedule, "schedule 1").unwrap();
        now.records.push((
            "--run1-schedule".into(),
            FileIdentity::of(&schedule).unwrap(),
        ));
        let differ = recorded.differences(&now);
        assert_eq!(differ.len(), 7, "{differ:#?}");
        for expected in [
            "the hermit binary: recorded",
            "the guest program: recorded",
            "the run arguments: recorded --chaos",
            "the target --target-stdout: recorded FAILED, now PASSED",
            "the search option --prefix-search: recorded true, now false",
            "the records and schedules given as runs: recorded none, now --run1-schedule=",
            "--analyze-seed: recorded 7, now 8",
        ] {
            assert!(
                differ.iter().any(|line| line.starts_with(expected)),
                "{expected:?} not in {differ:#?}"
            );
        }
        // A program no longer on the host differs too.
        let mut gone = recorded.clone();
        gone.program = None;
        assert_eq!(
            recorded.differences(&gone),
            [format!(
                "the guest program: recorded {} (sha256 {}), now not found on the host",
                recorded.program.as_ref().unwrap().path.display(),
                recorded.program.as_ref().unwrap().sha256
            )]
        );
    }

    #[test]
    fn the_journal_survives_reopening_and_a_line_cut_short() {
        let dir = tempfile::tempdir().unwrap();
        let entry = |key: &str| JournalEntry {
            run: format!("run_{key}"),
            key: key.into(),
            end: RecordedEnd::Finished { matched: true },
            wall_ms: Some(1500),
            log_bytes: None,
            outputs: vec![RecordedFile {
                path: "run_a.stdout".into(),
                sha256: "ab".repeat(32),
            }],
            recording_replays_input: None,
        };
        let journal = Journal::open(dir.path()).unwrap();
        assert!(journal.is_on());
        journal.record(entry("a")).unwrap();
        journal.record(entry("b")).unwrap();
        drop(journal);
        // A crash in the middle of a line leaves it cut short.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.path().join(JOURNAL_FILE))
            .unwrap();
        file.write_all(b"{\"run\":\"run_c\",\"ke").unwrap();
        drop(file);

        let journal = Journal::open(dir.path()).unwrap();
        assert_eq!(journal.len(), 2);
        assert_eq!(journal.lookup("a"), Some(entry("a")));
        assert_eq!(journal.lookup("c"), None);
        journal.record(entry("d")).unwrap();
        drop(journal);
        let journal = Journal::open(dir.path()).unwrap();
        assert_eq!(journal.len(), 3);
        assert_eq!(journal.lookup("d"), Some(entry("d")));
        // The default journal is off and records nothing.
        let off = Journal::default();
        assert!(!off.is_on());
        off.record(entry("e")).unwrap();
        assert_eq!(off.lookup("e"), None);
    }

    #[test]
    fn progress_is_rewritten_on_every_change() {
        let dir = tempfile::tempdir().unwrap();
        let progress = ProgressHandle::default();
        progress.phase(Phase::TargetRun);
        assert!(!dir.path().join(PROGRESS_FILE).exists());
        progress.write_into(dir.path());
        progress.phase(Phase::Bisect);
        progress.interval(BisectInterval {
            pass: Some(26),
            edit_distance: Some(720_878),
            swap_distance: Some(1),
            passing: "interval_passing.events".into(),
            passing_events: 39_279,
            failing: "interval_failing.events".into(),
            failing_events: 760_156,
            common_prefix: 853,
            passing_event: None,
            failing_event: None,
            resume: None,
        });
        // The default search falls back to a prefix search: its interval goes.
        progress.phase(Phase::PrefixSearch);
        assert_eq!(progress.snapshot().interval, None);
        let bracket = PrefixBracket {
            events: 100,
            continuation: "at a 200000000 ns timeslice".into(),
            misses: Some(40),
            misses_run: Some("prefix_round_004".into()),
            meets: Some(50),
            meets_run: Some("prefix_round_003".into()),
            unfinished: vec![45],
        };
        progress.bracket(bracket.clone());
        let read: Progress =
            serde_json::from_slice(&std::fs::read(dir.path().join(PROGRESS_FILE)).unwrap())
                .unwrap();
        assert_eq!(read.phase, Phase::PrefixSearch);
        assert_eq!(read.bracket, Some(bracket));
        assert_eq!(read, progress.snapshot());
    }
}
