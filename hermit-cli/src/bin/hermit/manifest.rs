/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run manifests: the "tiny recording" a `hermit run` leaves when it draws an
//! input from the host (https://github.com/rrnewton/hermit/issues/4271).
//!
//! A run whose every input is given by its options is reproducible from its
//! command line. A run that takes its virtual-time epoch from the host clock,
//! or its seed from `--seed-from=SystemRandom`, is not: those values are
//! inputs of the run that the command line does not hold. Such a run writes
//! `DATA/<id>/manifest.json`, where `DATA` is Hermit's data directory (as for
//! `hermit record`) and `<id>` a fresh recording ID. The manifest holds the
//! run's options with those inputs resolved, so
//!
//! ```text
//! hermit run --from DATA/<id>
//! ```
//!
//! reruns that execution: the same options, with the inputs Hermit drew
//! written out. What it does not hold, as a run config does not: the host
//! environment that `--base-env=host` (the default) passes through, the
//! working directory (recorded in `command.cwd` but not applied; relative
//! paths resolve against the rerun's), the files the guest reads, and a
//! description of the host machine beyond the preemption timer being off. A
//! run with no host-drawn input writes nothing, unless the manifest policy
//! says `always`, in which case it writes a `history` manifest: the same
//! document, kept as a local record of what ran. Output destinations (the
//! log file, `--verify-json`, guest output captures, ...) are left out, so a
//! rerun does not overwrite them. Option values are written as given,
//! `--env` values included; the file is created with mode 0600.
//!
//! ```json
//! {
//!   "schema": "hermit-recording-manifest/v1",
//!   "hermit": {"version": "0.4.1 (...)"},
//!   "id": "3f2a91...",
//!   "kind": "inputs-only",
//!   "created": "2026-10-13T23:55:00.123456Z",
//!   "command": {"program": "/usr/bin/python3", "args": ["sim.py"], "cwd": "/tmp"},
//!   "inputs": {
//!     "global": {"log": "info"},
//!     "run": {"epoch": "2026-10-13T23:55:00.123456Z", "seed": 42},
//!     "host_drawn": {"epoch": "host-now", "seed": "SystemRandom"}
//!   },
//!   "exit": {"code": 0}
//! }
//! ```
//!
//! `inputs` names options by their long flag names, exactly as a run config
//! (`--save-config`) does, and carries the same values, so the manifest needs
//! no second copy of the run config. `host_drawn` names which of them Hermit
//! drew from the host, and from where. `exit` is filled in when the guest
//! ends; a run that never finishes keeps a manifest without it.
//!
//! Three choices the owner left open are taken as the coordinator's defaults
//! (2026-10-13): the file is named `manifest.json`; `inputs` replaces the run
//! config rather than duplicating it; and a network trace stays at the path
//! its user names, with a sibling manifest.
//!
//! The policy is `nonreproducible-only` (the default), `always` or `never`.
//! Precedence, highest first: `hermit run --manifest=POLICY`, the `manifest`
//! key of the user config file `$XDG_CONFIG_HOME/hermit/config.yaml`
//! (`~/.config/hermit/config.yaml`), the `HERMIT_MANIFEST` environment
//! variable, the default.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use hermit::Error;
use hermit::Id;
use serde::Deserialize;
use serde::Serialize;

/// The `schema` value of the manifest format this hermit writes and reads.
pub(crate) const SCHEMA: &str = "hermit-recording-manifest/v1";

/// The manifest file's name inside its recording directory.
pub(crate) const MANIFEST_NAME: &str = hermit::MANIFEST_NAME;

/// The environment variable that sets the manifest policy below the user
/// config file.
pub(crate) const POLICY_ENV: &str = "HERMIT_MANIFEST";

/// The long name of `--max-timeslice`, which a run on a host without a
/// usable performance counter turns off by itself.
pub(crate) const MAX_TIMESLICE_KEY: &str = "max-timeslice";

/// Options that name where a run writes its output, by long name, before the
/// subcommand (`GLOBAL_OUTPUTS`) and after it (`RUN_OUTPUTS`). A manifest
/// leaves them out: they are not inputs of the run, and a rerun that reused
/// them would overwrite the original run's log, cores and reports.
const GLOBAL_OUTPUTS: &[&str] = &["log-file"];
const RUN_OUTPUTS: &[&str] = &[
    "backend-engagement-json",
    "fatal-core-dir",
    "guest-stderr",
    "guest-stdout",
    "preemption-stacktrace-log-file",
    "record-preemptions-to",
    "run-evidence-dir",
    "run-result-json",
    "summary-json",
    "verify-json",
    "verify-log-dir",
];

/// Removes the output destinations from a manifest's `global` and `run`
/// sections.
pub(crate) fn drop_outputs(
    global: &mut BTreeMap<String, serde_json::Value>,
    run: &mut BTreeMap<String, serde_json::Value>,
) {
    for long in GLOBAL_OUTPUTS {
        global.remove(*long);
    }
    for long in RUN_OUTPUTS {
        run.remove(*long);
    }
}

/// When a `hermit run` writes a manifest.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    clap::ValueEnum
)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ManifestPolicy {
    /// Only when the run drew an input from the host, so its command line
    /// alone does not reproduce it.
    #[default]
    NonreproducibleOnly,
    /// For every run: a reproducible one writes a `history` manifest.
    Always,
    /// Never.
    Never,
}

impl std::str::FromStr for ManifestPolicy {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        <ManifestPolicy as clap::ValueEnum>::from_str(text, false).map_err(|_| {
            anyhow::anyhow!(
                "unknown manifest policy {text:?}; expected nonreproducible-only, always or never"
            )
        })
    }
}

/// The user config file, `$XDG_CONFIG_HOME/hermit/config.yaml`. Unknown keys
/// are refused, as in a run config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfig {
    /// The manifest policy.
    #[serde(default)]
    pub(crate) manifest: Option<ManifestPolicy>,
}

/// The data directory manifests are written under: `HERMIT_DATA_DIR`, else
/// the default `hermit record` uses (`$XDG_CACHE_HOME/hermit`).
pub(crate) fn data_dir() -> PathBuf {
    match std::env::var_os("HERMIT_DATA_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => hermit::HermitData::new().data_dir().to_owned(),
    }
}

/// Where the user config file is, if this host has a config directory.
pub(crate) fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("hermit").join("config.yaml"))
}

/// Reads the user config file at `path`; a missing file is an empty config.
pub(crate) fn read_user_config(path: &Path) -> Result<UserConfig, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(UserConfig::default()),
        Ok(text) => serde_yaml::from_str(&text)
            .with_context(|| format!("cannot load the hermit user config {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UserConfig::default()),
        Err(error) => Err(error)
            .with_context(|| format!("cannot read the hermit user config {}", path.display())),
    }
}

/// The policy in effect: `flag`, else the user config's, else `env` (the
/// value of [`POLICY_ENV`]), else the default.
pub(crate) fn resolve_policy(
    flag: Option<ManifestPolicy>,
    user_config: &UserConfig,
    env: Option<&str>,
) -> Result<ManifestPolicy, Error> {
    if let Some(policy) = flag.or(user_config.manifest) {
        return Ok(policy);
    }
    match env {
        Some(text) => text
            .parse()
            .with_context(|| format!("cannot use {POLICY_ENV}={text}")),
        None => Ok(ManifestPolicy::default()),
    }
}

/// What a manifest records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Kind {
    /// A run that drew an input from the host: the manifest is the whole
    /// recording.
    InputsOnly,
    /// A reproducible run, kept under the `always` policy.
    History,
}

/// The `hermit` that wrote a manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Writer {
    /// Its version string, as `hermit --version` prints it.
    pub(crate) version: String,
}

/// The guest command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Command {
    pub(crate) program: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    /// The host working directory the run started in. Informational: a
    /// rerun uses its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
}

/// The run's options, with every input Hermit resolved written out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inputs {
    /// Options before the subcommand, by long name.
    #[serde(default)]
    pub(crate) global: BTreeMap<String, serde_json::Value>,
    /// Options of `hermit run`, by long name.
    #[serde(default)]
    pub(crate) run: BTreeMap<String, serde_json::Value>,
    /// The options whose values Hermit drew from the host, each with where
    /// it drew it from (`host-now` for an epoch from the host clock,
    /// `SystemRandom` for a seed from `--seed-from=SystemRandom`).
    #[serde(default)]
    pub(crate) host_drawn: BTreeMap<String, String>,
}

/// How the guest ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) signal: Option<i32>,
}

/// One manifest document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    /// Always [`SCHEMA`].
    pub(crate) schema: String,
    pub(crate) hermit: Writer,
    pub(crate) id: String,
    pub(crate) kind: Kind,
    /// When the run started, RFC 3339 in UTC, from the host clock.
    pub(crate) created: String,
    pub(crate) command: Command,
    pub(crate) inputs: Inputs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit: Option<Exit>,
}

impl Manifest {
    /// A manifest of `kind` for a run of `command` with `inputs`.
    pub(crate) fn new(id: Id, kind: Kind, command: Command, inputs: Inputs) -> Self {
        Manifest {
            schema: SCHEMA.to_owned(),
            hermit: Writer {
                version: crate::version::Version::get().to_owned(),
            },
            id: id.to_string(),
            kind,
            created: rfc3339_utc(std::time::SystemTime::now()),
            command,
            inputs,
            exit: None,
        }
    }
}

/// `time` as RFC 3339 in UTC with microseconds, `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
fn rfc3339_utc(time: std::time::SystemTime) -> String {
    let since = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since.as_secs();
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    // Howard Hinnant's civil_from_days, for days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        of_day / 3_600,
        of_day / 60 % 60,
        of_day % 60,
        since.subsec_micros()
    )
}

/// Writes `manifest` to `path` atomically (a temporary file in the same
/// directory, then a rename), creating the directory, and reads it back so a
/// document that would not load is an error now.
pub(crate) fn write(path: &Path, manifest: &Manifest) -> Result<(), Error> {
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("manifest path {} has no directory", path.display()))?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let mut text = serde_json::to_string_pretty(manifest).context("cannot encode the manifest")?;
    text.push('\n');
    let reread = parse(&text).context("the manifest this hermit encoded does not parse back")?;
    anyhow::ensure!(
        reread == *manifest,
        "the manifest this hermit encoded parses back differently"
    );
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("cannot create a temporary file in {}", directory.display()))?;
    std::io::Write::write_all(&mut file, text.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.persist(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Parses a manifest document, refusing another `schema` and unknown keys.
pub(crate) fn parse(text: &str) -> Result<Manifest, Error> {
    let manifest: Manifest =
        serde_json::from_str(text).context("the file is not a hermit manifest")?;
    anyhow::ensure!(
        manifest.schema == SCHEMA,
        "the manifest's schema is {:?}; this hermit reads {SCHEMA:?}",
        manifest.schema
    );
    Ok(manifest)
}

/// Reads the manifest at `path`: a `manifest.json` file, or the recording
/// directory that holds one.
pub(crate) fn read(path: &Path) -> Result<Manifest, Error> {
    let file = if path.is_dir() {
        path.join(MANIFEST_NAME)
    } else {
        path.to_owned()
    };
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("cannot read manifest {}", file.display()))?;
    parse(&text).with_context(|| format!("cannot load manifest {}", file.display()))
}

/// How many manifest-only recordings (`inputs-only` and `history`) a data
/// directory keeps; writing one more deletes the oldest. Each is a few KB,
/// so the data directory holds at most a few MB of them. Syscall and network
/// recordings are never pruned.
pub(crate) const KEEP_MANIFEST_ONLY: usize = 1000;

/// Deletes the oldest manifest-only recordings in `data_dir` beyond `keep`,
/// by their `created` time. A directory that also holds a syscall recording
/// (`metadata.json`) is never touched, nor is one whose manifest does not
/// parse. Best effort: concurrent runs may prune the same directory, so a
/// failed removal is ignored.
pub(crate) fn prune(data_dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|directory| {
            directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.parse::<Id>().is_ok())
                && !directory.join(hermit::RECORDING_METADATA_NAME).exists()
        })
        .filter_map(|directory| {
            let manifest = read(&directory).ok()?;
            Some((manifest.created, directory))
        })
        .collect();
    if found.len() <= keep {
        return;
    }
    found.sort();
    for (_, directory) in &found[..found.len() - keep] {
        let _ = std::fs::remove_dir_all(directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest::new(
            Id::unique(),
            Kind::InputsOnly,
            Command {
                program: Some("/bin/date".to_owned()),
                args: vec!["-u".to_owned()],
                cwd: Some("/tmp".to_owned()),
            },
            Inputs {
                global: BTreeMap::from([("log".to_owned(), "info".into())]),
                run: BTreeMap::from([
                    ("epoch".to_owned(), "2026-10-13T23:55:00.123456Z".into()),
                    ("seed".to_owned(), 42.into()),
                ]),
                host_drawn: BTreeMap::from([
                    ("epoch".to_owned(), "host-now".to_owned()),
                    ("seed".to_owned(), "SystemRandom".to_owned()),
                ]),
            },
        )
    }

    #[test]
    fn creation_times_are_rfc3339_utc() {
        let at = |seconds: u64, micros: u32| {
            rfc3339_utc(
                std::time::UNIX_EPOCH
                    + std::time::Duration::from_secs(seconds)
                    + std::time::Duration::from_micros(micros.into()),
            )
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(at(951_782_400, 7), "2000-02-29T00:00:00.000007Z");
        assert_eq!(at(1_791_935_976, 123_456), "2026-10-13T23:59:36.123456Z");
    }

    #[test]
    fn a_manifest_survives_write_and_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("id").join(MANIFEST_NAME);
        let mut manifest = sample();
        manifest.exit = Some(Exit {
            code: Some(3),
            signal: None,
        });
        write(&path, &manifest).unwrap();
        assert_eq!(read(&path).unwrap(), manifest);
        assert_eq!(read(path.parent().unwrap()).unwrap(), manifest);
    }

    /// Only the oldest manifest-only recordings beyond the limit go; a
    /// syscall recording beside them stays.
    #[test]
    fn pruning_keeps_the_newest_manifest_only_recordings() {
        let directory = tempfile::tempdir().unwrap();
        let mut ids = Vec::new();
        for second in 0..4 {
            let mut manifest = sample();
            manifest.created = format!("2026-10-13T23:55:0{second}.000000Z");
            let id = manifest.id.clone();
            write(&directory.path().join(&id).join(MANIFEST_NAME), &manifest).unwrap();
            ids.push(id);
        }
        let recording = directory.path().join(Id::unique().to_string());
        std::fs::create_dir_all(&recording).unwrap();
        std::fs::write(recording.join(hermit::RECORDING_METADATA_NAME), "{}").unwrap();
        prune(directory.path(), 2);
        let exists = |id: &String| directory.path().join(id).exists();
        assert_eq!(
            ids.iter().map(exists).collect::<Vec<_>>(),
            [false, false, true, true]
        );
        assert!(recording.exists());
    }

    /// Every output destination named is a real option, so a renamed one
    /// cannot slip back into manifests unnoticed, and none is left behind.
    #[test]
    fn output_destinations_are_left_out() {
        use clap::CommandFactory;
        let hermit = crate::Args::command();
        let run = hermit.find_subcommand("run").unwrap();
        let longs = |command: &clap::Command| -> Vec<String> {
            command
                .get_arguments()
                .filter_map(|arg| arg.get_long().map(str::to_owned))
                .collect()
        };
        for long in GLOBAL_OUTPUTS {
            assert!(longs(&hermit).contains(&long.to_string()), "--{long}");
        }
        for long in RUN_OUTPUTS {
            assert!(longs(run).contains(&long.to_string()), "--{long}");
        }
        let mut global = BTreeMap::from([
            ("log-file".to_owned(), "/x.log".into()),
            ("log".to_owned(), "info".into()),
        ]);
        let mut run_inputs = BTreeMap::from([
            ("verify-json".to_owned(), "/v.json".into()),
            ("seed".to_owned(), 3.into()),
        ]);
        drop_outputs(&mut global, &mut run_inputs);
        assert_eq!(global.keys().collect::<Vec<_>>(), ["log"]);
        assert_eq!(run_inputs.keys().collect::<Vec<_>>(), ["seed"]);
    }

    #[test]
    fn another_schema_and_unknown_keys_are_refused() {
        let text = serde_json::to_string(&sample()).unwrap();
        let other = text.replace(SCHEMA, "hermit-recording-manifest/v0");
        assert!(parse(&other).is_err());
        let unknown = text.replacen('{', "{\"surprise\":1,", 1);
        assert!(parse(&unknown).is_err());
    }

    /// The flag outranks the user config, which outranks the environment,
    /// which outranks the default.
    #[test]
    fn the_policy_follows_flag_then_file_then_environment() {
        let file = UserConfig {
            manifest: Some(ManifestPolicy::Never),
        };
        let none = UserConfig::default();
        assert_eq!(
            resolve_policy(Some(ManifestPolicy::Always), &file, Some("never")).unwrap(),
            ManifestPolicy::Always
        );
        assert_eq!(
            resolve_policy(None, &file, Some("always")).unwrap(),
            ManifestPolicy::Never
        );
        assert_eq!(
            resolve_policy(None, &none, Some("always")).unwrap(),
            ManifestPolicy::Always
        );
        assert_eq!(
            resolve_policy(None, &none, None).unwrap(),
            ManifestPolicy::NonreproducibleOnly
        );
        assert!(resolve_policy(None, &none, Some("sometimes")).is_err());
    }

    #[test]
    fn the_user_config_refuses_unknown_keys_and_accepts_a_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        assert!(read_user_config(&path).unwrap().manifest.is_none());
        std::fs::write(&path, "manifest: always\n").unwrap();
        assert_eq!(
            read_user_config(&path).unwrap().manifest,
            Some(ManifestPolicy::Always)
        );
        std::fs::write(&path, "manifest: always\ncolour: blue\n").unwrap();
        assert!(read_user_config(&path).is_err());
    }
}
