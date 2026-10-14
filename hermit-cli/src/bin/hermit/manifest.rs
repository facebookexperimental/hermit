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
    "record-networking",
    "fatal-core-dir",
    "preemption-stacktrace-log-file",
    "record-preemptions-to",
    "summary-json",
    "verify-json",
    "verify-log-dir",
    "verify-run",
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
    /// A `hermit run --record-networking` recording: the manifest and the
    /// network trace it names (`artifacts.network_trace`).
    Network,
    /// A `hermit record` syscall recording: the manifest beside the
    /// recording it describes (`artifacts.syscall_recording`).
    FullSyscall,
}

impl Kind {
    /// Whether a data directory prunes recordings of this kind
    /// ([`KEEP_MANIFEST_ONLY`]). Recordings a user asked for are never
    /// pruned: a syscall recording is not, and a network recording's trace
    /// and the manifest beside it stay at the path its user named. Only the
    /// data directory's copy of that manifest, which `hermit replay` finds
    /// by ID, is pruned.
    pub(crate) fn pruned(self) -> bool {
        matches!(self, Kind::InputsOnly | Kind::History | Kind::Network)
    }

    /// Whether a recording of this kind becomes `DATA/last`, which a bare
    /// `hermit replay` replays: a recording the user asked for. A run's own
    /// manifest moves only `DATA/last-run`. Without `--epoch` every `hermit
    /// run` draws its epoch from the host and leaves an `inputs-only`
    /// manifest, so if those moved `last`, any run, in any terminal, would
    /// take a bare `hermit replay` away from the recording just made, which
    /// is not what `rr record` then `rr replay` does.
    pub(crate) fn moves_last(self) -> bool {
        matches!(self, Kind::Network | Kind::FullSyscall)
    }

    /// The kind as the manifest spells it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::InputsOnly => "inputs-only",
            Kind::History => "history",
            Kind::Network => "network",
            Kind::FullSyscall => "full-syscall",
        }
    }
}

/// The format name of a network trace (`artifacts.network_trace.format`).
pub(crate) const NETWORK_TRACE_FORMAT: &str = "hermit-net-trace";

/// A network trace a manifest names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkTrace {
    /// The trace's path, relative to the manifest's directory.
    pub(crate) path: String,
    /// Always [`NETWORK_TRACE_FORMAT`].
    pub(crate) format: String,
    /// The trace's format version, its SHA-256 and its size, filled in once
    /// the run has written the trace. A rerun refuses a trace that does not
    /// match them, or a manifest without them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bytes: Option<u64>,
}

/// The syscall recording a `full-syscall` manifest describes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyscallRecording {
    /// The recording's `RECORD_VERSION`, as its `metadata.json` holds it.
    pub(crate) record_version: serde_json::Value,
    /// The recording's metadata file, relative to the manifest's directory.
    /// `hermit replay` still reads it; it stays beside the manifest as a
    /// compatibility copy for one `RECORD_VERSION`.
    pub(crate) metadata: String,
}

/// The heavier artifacts a manifest names.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) network_trace: Option<NetworkTrace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) syscall_recording: Option<SyscallRecording>,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

impl Exit {
    /// How a guest that ended with `status` ended.
    pub(crate) fn from_status(status: hermit::ExitStatus) -> Self {
        match status {
            hermit::ExitStatus::Exited(code) => Exit {
                code: Some(code),
                signal: None,
            },
            hermit::ExitStatus::Signaled(signal, _) => Exit {
                code: None,
                signal: Some(signal as i32),
            },
            hermit::ExitStatus::SignaledRaw(signal, _) => Exit {
                code: None,
                signal: Some(signal),
            },
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) artifacts: Option<Artifacts>,
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
            artifacts: None,
        }
    }
}

/// Where a network recording's manifest goes: beside its trace, named after
/// it, `TRACE.manifest.json`. The trace stays at the path its user named.
pub(crate) fn network_manifest_path(trace: &Path) -> PathBuf {
    let mut name = trace.file_name().unwrap_or_default().to_owned();
    name.push(".manifest.json");
    trace.with_file_name(name)
}

/// The format version in a network trace's header: the 16-byte magic, then
/// a little-endian `u32`.
pub(crate) fn network_trace_version(trace: &Path) -> Result<u32, Error> {
    use std::io::Read;
    let magic = detcore_model::network_trace::NETWORK_TRACE_MAGIC;
    let mut header = vec![0u8; magic.len() + 4];
    std::fs::File::open(trace)
        .and_then(|mut file| file.read_exact(&mut header))
        .with_context(|| {
            format!(
                "cannot read the header of network trace {}",
                trace.display()
            )
        })?;
    anyhow::ensure!(
        header.starts_with(&magic),
        "{} is not a hermit network trace",
        trace.display()
    );
    Ok(u32::from_le_bytes(
        header[magic.len()..].try_into().unwrap(),
    ))
}

/// The directory a manifest path names: the path itself when it is a
/// directory, else its parent, `.` for a bare file name.
pub(crate) fn directory_of(path: &Path) -> &Path {
    if path.is_dir() {
        return path;
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// The SHA-256 (lowercase hex) and size of the file at `path`.
pub(crate) fn digest_and_size(path: &Path) -> Result<(String, u64), Error> {
    let digest = detcore::Digest::digest_path(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let bytes = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    Ok((digest.to_string(), bytes))
}

/// The network trace a `network` manifest at `manifest_path` names, resolved
/// against the manifest's directory and checked against the size and digest
/// the manifest recorded.
pub(crate) fn checked_network_trace(
    manifest: &Manifest,
    manifest_path: &Path,
) -> Result<PathBuf, Error> {
    let trace = manifest
        .artifacts
        .as_ref()
        .and_then(|artifacts| artifacts.network_trace.as_ref())
        .ok_or_else(|| anyhow::anyhow!("this network manifest names no network trace"))?;
    let path = directory_of(manifest_path).join(&trace.path);
    let (Some(sha256), Some(bytes)) = (&trace.sha256, trace.bytes) else {
        anyhow::bail!(
            "the network recording {} never finished: its manifest has no digest for it",
            path.display()
        );
    };
    let (actual, actual_bytes) = digest_and_size(&path)?;
    anyhow::ensure!(
        actual_bytes == bytes && actual == *sha256,
        "network trace {} is not the one its manifest recorded ({actual_bytes} bytes, sha256 \
         {actual}; the manifest says {bytes} bytes, sha256 {sha256})",
        path.display()
    );
    Ok(path)
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

/// How many manifest-only recordings (`inputs-only`, `history`, and the
/// copies of `network` manifests) a data directory keeps; writing one more
/// deletes the oldest. Each is a few KB, so the data directory holds at most
/// a few MB of them. Syscall recordings, network traces, and the manifests
/// beside those traces are never pruned.
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
            manifest
                .kind
                .pruned()
                .then_some((manifest.created, directory))
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

/// The fewest hex digits that name a recording by ID prefix.
pub(crate) const MIN_ID_PREFIX: usize = 6;

/// What a recording name resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A manifest, of any kind.
    Manifest(PathBuf),
    /// A syscall recording without a manifest: `hermit record` wrote none
    /// before <https://github.com/rrnewton/hermit/issues/4271>.
    Recording { data_dir: PathBuf, id: Id },
}

/// How a command that takes a recording names one, for its help.
pub(crate) const TARGET_HELP: &str = "`last` (the newest `hermit record` or `--record-networking` \
     recording), `last-run` (the newest of any kind, a plain run's manifest included), a \
     recording ID or a unique prefix of at least 6 hex digits, or the path of a manifest.json \
     or of its directory. To name a file called `last`, write `./last`";

/// Resolves a recording name against `data_dir`: `last` or `last-run`, an
/// existing path (a manifest, or a recording directory), or an ID or a
/// unique prefix of at least [`MIN_ID_PREFIX`] hex digits, as git accepts
/// commit prefixes. Each refusal names what to run instead.
pub(crate) fn resolve(data_dir: &Path, spec: &std::ffi::OsStr) -> Result<Target, Error> {
    let text = spec.to_str();
    if let Some(name @ (hermit::LAST | hermit::LAST_RUN)) = text {
        let file = data_dir.join(name);
        let id = match std::fs::read_to_string(&file) {
            Ok(id) => id,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(no_latest(data_dir, name));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", file.display()));
            }
        };
        let id: Id = id
            .trim()
            .parse()
            .with_context(|| format!("{} does not hold a recording ID", file.display()))?;
        return target_of_id(data_dir, id).with_context(|| format!("cannot replay `{name}`"));
    }
    let path = Path::new(spec);
    if path.is_file() {
        // An existing path is read as a path, before an ID prefix; say so
        // when it is not a manifest.
        read(path).with_context(|| {
            format!(
                "{} names the file {} here, which is read as a path before any recording ID \
                 and is not a manifest; to name a recording, give more digits of its ID or the \
                 whole ID, which `hermit list --json` prints",
                spec.to_string_lossy(),
                path.display()
            )
        })?;
        return Ok(Target::Manifest(path.to_owned()));
    }
    if path.is_dir() {
        if path.join(MANIFEST_NAME).is_file() {
            return Ok(Target::Manifest(path.join(MANIFEST_NAME)));
        }
        if !path.join(hermit::RECORDING_METADATA_NAME).is_file() {
            anyhow::bail!(
                "{} is not a recording: it holds neither {MANIFEST_NAME} nor {}; list \
                 recordings with: hermit list",
                path.display(),
                hermit::RECORDING_METADATA_NAME
            );
        }
        let (data_dir, id) = recording_location(&std::path::absolute(path)?, None)?;
        return Ok(Target::Recording { data_dir, id });
    }
    if let Some(id) = text.and_then(|text| text.parse::<Id>().ok()) {
        return target_of_id(data_dir, id);
    }
    let Some(prefix) = text
        .filter(|text| {
            (MIN_ID_PREFIX..=32).contains(&text.len())
                && text.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
    else {
        anyhow::bail!(
            "{} names no recording: give {TARGET_HELP}; list recordings with: hermit list",
            path.display()
        );
    };
    let ids = hermit::HermitData::with_dir(data_dir).try_recordings()?;
    let mut matching: Vec<Id> = ids
        .into_iter()
        .filter(|id| id.to_string().starts_with(&prefix))
        .collect();
    matching.sort_by_key(Id::to_string);
    match matching.as_slice() {
        [id] => target_of_id(data_dir, *id),
        [] => anyhow::bail!(
            "no recording in {} has an ID starting with {prefix}; list them with: hermit list",
            data_dir.display()
        ),
        several => anyhow::bail!(
            "{} recordings in {} have an ID starting with {prefix} ({}); give more digits",
            several.len(),
            data_dir.display(),
            several
                .iter()
                .map(Id::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The refusal for a data directory without a `last` or `last-run` file,
/// saying what that file names and what the directory does hold.
fn no_latest(data_dir: &Path, name: &str) -> Error {
    let held = hermit::HermitData::with_dir(data_dir)
        .try_recordings()
        .map_or(0, |ids| ids.len());
    if held == 0 {
        return anyhow::anyhow!(
            "{} has no recordings yet: `hermit record`, `hermit run --record-networking`, and a \
             `hermit run` that draws an input from the host each make one; list them with: \
             hermit list",
            data_dir.display()
        );
    }
    let (names, instead) = if name == hermit::LAST {
        (
            "the newest `hermit record` or `--record-networking` recording, and none has been \
             made here",
            "; `last-run` names the newest of any kind",
        )
    } else {
        (
            "the newest recording of any kind; hermit writes it with each recording, and none \
             of the recordings here was made since it began to",
            "",
        )
    };
    anyhow::anyhow!(
        "{} has no `{name}`, which names {names}. It holds {held} {}, which `hermit list` \
         lists; name one by its ID{instead}",
        data_dir.display(),
        if held == 1 { "recording" } else { "recordings" }
    )
}

/// Where the syscall recording in `directory` is, as the replayer finds a
/// recording: a data directory and an ID, the directory's name. A recording
/// copied to a directory with another name is refused with the name to give
/// it (`id`, from its manifest, when it has one).
pub(crate) fn recording_location(
    directory: &Path,
    id: Option<&str>,
) -> Result<(PathBuf, Id), Error> {
    let name = directory.file_name().and_then(|name| name.to_str());
    match (name.and_then(|name| name.parse().ok()), directory.parent()) {
        (Some(id), Some(parent)) => Ok((parent.to_owned(), id)),
        _ => anyhow::bail!(
            "{} holds a syscall recording, but `hermit replay` finds one by its directory's \
             name, which must be a recording ID, and this directory is named {:?}; rename it to \
             {}",
            directory.display(),
            name.unwrap_or_default(),
            id.map_or_else(
                || "a recording ID (32 hex digits; any unused one will do)".to_owned(),
                str::to_owned
            )
        ),
    }
}

/// The recording `id` in `data_dir`: its manifest, else its syscall
/// recording.
fn target_of_id(data_dir: &Path, id: Id) -> Result<Target, Error> {
    let directory = data_dir.join(id.to_string());
    if directory.join(MANIFEST_NAME).is_file() {
        Ok(Target::Manifest(directory.join(MANIFEST_NAME)))
    } else if directory.join(hermit::RECORDING_METADATA_NAME).is_file() {
        Ok(Target::Recording {
            data_dir: data_dir.to_owned(),
            id,
        })
    } else {
        anyhow::bail!(
            "{} has no recording {id}: there is no {id}/{MANIFEST_NAME} or {id}/{} in it. A \
             data directory keeps only the newest {KEEP_MANIFEST_ONLY} manifests of runs, and \
             `hermit record rm` deletes a recording; list what it holds with: hermit list",
            data_dir.display(),
            hermit::RECORDING_METADATA_NAME
        )
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

    /// A network recording's manifest goes beside its trace, and a rerun
    /// accepts the trace only with the size and digest the manifest
    /// recorded; a manifest written before the run ended has none.
    #[test]
    fn a_network_manifest_names_and_checks_its_trace() {
        let directory = tempfile::tempdir().unwrap();
        let trace = directory.path().join("net.trace");
        assert_eq!(
            network_manifest_path(&trace),
            directory.path().join("net.trace.manifest.json")
        );
        let mut bytes = detcore_model::network_trace::NETWORK_TRACE_MAGIC.to_vec();
        bytes.extend(4u32.to_le_bytes());
        bytes.extend([0u8; 8]);
        std::fs::write(&trace, &bytes).unwrap();
        assert_eq!(network_trace_version(&trace).unwrap(), 4);

        let mut manifest = sample();
        manifest.kind = Kind::Network;
        let mut named = NetworkTrace {
            path: "net.trace".to_owned(),
            format: NETWORK_TRACE_FORMAT.to_owned(),
            version: None,
            sha256: None,
            bytes: None,
        };
        manifest.artifacts = Some(Artifacts {
            network_trace: Some(named.clone()),
            syscall_recording: None,
        });
        let path = network_manifest_path(&trace);
        assert!(
            checked_network_trace(&manifest, &path).is_err(),
            "no digest yet"
        );
        let (sha256, size) = digest_and_size(&trace).unwrap();
        named.version = Some(4);
        named.sha256 = Some(sha256);
        named.bytes = Some(size);
        manifest.artifacts.as_mut().unwrap().network_trace = Some(named);
        assert_eq!(checked_network_trace(&manifest, &path).unwrap(), trace);
        bytes.push(1);
        std::fs::write(&trace, &bytes).unwrap();
        assert!(
            checked_network_trace(&manifest, &path).is_err(),
            "changed trace"
        );
    }

    /// Only manifest-only kinds are pruned: network and syscall recordings
    /// were asked for, and stay.
    #[test]
    fn pruning_spares_recordings_the_user_asked_for() {
        assert!(Kind::InputsOnly.pruned() && Kind::History.pruned() && Kind::Network.pruned());
        assert!(!Kind::FullSyscall.pruned());
        let directory = tempfile::tempdir().unwrap();
        let mut syscall = sample();
        syscall.kind = Kind::FullSyscall;
        syscall.created = "2000-01-01T00:00:00.000000Z".to_owned();
        let kept = syscall.id.clone();
        write(&directory.path().join(&kept).join(MANIFEST_NAME), &syscall).unwrap();
        // The data directory's copy of a network manifest; its trace and the
        // manifest beside it live elsewhere, where pruning never looks.
        let mut network = sample();
        network.kind = Kind::Network;
        write(
            &directory.path().join(&network.id).join(MANIFEST_NAME),
            &network,
        )
        .unwrap();
        let newer = sample();
        write(
            &directory.path().join(&newer.id).join(MANIFEST_NAME),
            &newer,
        )
        .unwrap();
        prune(directory.path(), 0);
        assert!(directory.path().join(&kept).exists());
        assert!(!directory.path().join(&network.id).exists());
        assert!(!directory.path().join(&newer.id).exists());
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

    /// Writes a manifest of `kind` as recording `id` in `data_dir` and
    /// notes it as `hermit run` does.
    fn store(data_dir: &Path, id: Id, kind: Kind) {
        let mut manifest = sample();
        manifest.id = id.to_string();
        manifest.kind = kind;
        write(
            &data_dir.join(id.to_string()).join(MANIFEST_NAME),
            &manifest,
        )
        .unwrap();
        hermit::HermitData::with_dir(data_dir)
            .note_latest(&id, kind.moves_last())
            .unwrap();
    }

    #[test]
    fn a_recording_is_named_by_last_by_prefix_or_by_path() {
        let data = tempfile::tempdir().unwrap();
        let resolve = |spec: &str| resolve(data.path(), std::ffi::OsStr::new(spec));
        let error = resolve("last").unwrap_err().to_string();
        assert!(
            error.contains("has no recordings yet") && error.contains("hermit list"),
            "{error}"
        );

        let inputs: Id = "3f2a91c40000400080000000000000a1".parse().unwrap();
        let history: Id = "3f2a91c50000400080000000000000a2".parse().unwrap();
        let network: Id = "7aedacba0000400080000000000000a3".parse().unwrap();
        store(data.path(), inputs, Kind::InputsOnly);
        store(data.path(), history, Kind::History);
        let manifest_of =
            |id: Id| Target::Manifest(data.path().join(id.to_string()).join(MANIFEST_NAME));
        // A run's own manifest moves `last-run`, never `last`; the refusal
        // says what `last` names and that there are other recordings.
        assert_eq!(resolve("last-run").unwrap(), manifest_of(history));
        let error = resolve("last").unwrap_err().to_string();
        assert!(
            error.contains("has no `last`")
                && error.contains("It holds 2 recordings")
                && error.contains("`last-run`"),
            "{error}"
        );
        // A recording the user asked for moves both.
        store(data.path(), network, Kind::Network);
        assert_eq!(resolve("last").unwrap(), manifest_of(network));
        assert_eq!(resolve("last-run").unwrap(), manifest_of(network));
        store(data.path(), history, Kind::History);
        assert_eq!(resolve("last").unwrap(), manifest_of(network));
        assert_eq!(resolve("last-run").unwrap(), manifest_of(history));

        assert_eq!(resolve("3F2A91C4").unwrap(), manifest_of(inputs));
        assert_eq!(resolve(&history.to_string()).unwrap(), manifest_of(history));
        let error = resolve("3f2a91").unwrap_err().to_string();
        assert!(
            error.contains("2 recordings") && error.contains("give more digits"),
            "{error}"
        );
        let error = resolve("abcdef").unwrap_err().to_string();
        assert!(
            error.contains("no recording in") && error.contains("hermit list"),
            "{error}"
        );
        // Five digits are too few to be a prefix, and not a path either.
        let error = resolve("3f2a9").unwrap_err().to_string();
        assert!(
            error.contains("names no recording") && error.contains("hermit list"),
            "{error}"
        );
        let directory = data.path().join(inputs.to_string());
        assert_eq!(
            resolve(directory.to_str().unwrap()).unwrap(),
            manifest_of(inputs)
        );
        // An existing file is read as a path; one that is not a manifest is
        // refused saying so.
        let stray = data.path().join("7aedacba");
        std::fs::write(&stray, "not a manifest").unwrap();
        let error = format!("{:#}", resolve(stray.to_str().unwrap()).unwrap_err());
        assert!(
            error.contains("is read as a path before any recording ID"),
            "{error}"
        );

        // A syscall recording without a manifest, by path and by ID.
        let recording = Id::unique();
        let directory = data.path().join(recording.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(hermit::RECORDING_METADATA_NAME), "{}").unwrap();
        let expected = Target::Recording {
            data_dir: data.path().to_owned(),
            id: recording,
        };
        assert_eq!(resolve(directory.to_str().unwrap()).unwrap(), expected);
        assert_eq!(resolve(&recording.to_string()).unwrap(), expected);
        // Copied under another name, it is refused with the true reason.
        let renamed = data.path().join("myrec");
        std::fs::create_dir_all(&renamed).unwrap();
        std::fs::write(renamed.join(hermit::RECORDING_METADATA_NAME), "{}").unwrap();
        let error = resolve(renamed.to_str().unwrap()).unwrap_err().to_string();
        assert!(
            error.contains("holds a syscall recording")
                && error.contains("named \"myrec\"")
                && error.contains("rename it to"),
            "{error}"
        );

        // A bare file name's directory is the current one.
        assert_eq!(directory_of(Path::new(MANIFEST_NAME)), Path::new("."));
        assert_eq!(directory_of(data.path()), data.path());

        // A recording `last` names that is gone.
        std::fs::remove_dir_all(data.path().join(network.to_string())).unwrap();
        let error = resolve("last").unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("has no recording {network}")),
            "{error:#}"
        );
    }
}
