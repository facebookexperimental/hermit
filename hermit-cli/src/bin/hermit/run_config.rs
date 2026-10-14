/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run configuration files: `hermit run --config FILE` and
//! `hermit run --save-config FILE`.
//!
//! A run config is a YAML document (JSON is accepted, being YAML) that names
//! `hermit run` options by their long flag names:
//!
//! ```yaml
//! schema: hermit-run-config/v1
//! hermit-version: 0.4.1 (2026-10-08, source revision not embedded)
//! global:                 # options before the subcommand: hermit [GLOBAL] run
//!   backend: ptrace
//!   log: info
//! run:                    # options of `hermit run`
//!   epoch: 2026-10-08T17:33:00.123456789+00:00
//!   seed: 42
//!   verify: true          # a flag: `true` gives it, `false` is the same as omitting it
//!   env: [FOO=1, HOME]    # a repeatable option: a list, one entry per occurrence
//! program: /bin/echo
//! args: [hello]
//! ```
//!
//! The contract is that a run config means exactly what typing its options
//! would. Loading turns every entry into the command-line option it names
//! (`--seed=42`, `--env=FOO=1`, ...) and hands the result to the same clap
//! parser as a typed command line, so value syntax, conflicts and
//! requirements are checked once, in one place, for both. Nothing here knows
//! what any particular option means; a new `hermit run` option is part of
//! the format the moment it exists. The `run_config` tests require a
//! round-trip sample for every option, so an option that does not survive
//! save and load fails a test rather than a reproduction.
//!
//! Precedence, highest first: an option given on the command line, the
//! file, the option's environment variable (`HERMIT_EPOCH`, `HERMIT_LOG`, ...),
//! its default. A command-line option replaces the file's value for that
//! option, a repeatable option's whole list included. A guest program on the
//! command line replaces the file's `program` and `args` together. The file
//! outranks the environment because it names its values explicitly, while
//! an inherited variable is exactly the kind of implicit input it records.
//!
//! `--save-config` writes the options the invocation was given, on the
//! command line or through their environment variables, plus the implicit
//! inputs Hermit resolved for the run: an epoch sampled from the host clock
//! or adopted from a replayed recording, and the seed `--seed-from` chose.
//! It does not write defaults: an explicit default can collide with clap's
//! conflict rules (`--network=local` beside `--no-namespace`), so the file
//! reproduces the run under the hermit that wrote it, whose version it
//! records. The host environment that `--base-env=host` passes through is
//! not recorded; `--base-env=minimal` or `--base-env=empty` with `--env`
//! makes the guest environment explicit.
//!
//! Unknown keys are refused, and so is a `schema` other than [`SCHEMA`]. The
//! top-level `seccomp:` key is reserved for a future syscall policy section
//! and is refused until that exists.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clap::Arg;
use clap::ArgAction;
use clap::ArgMatches;
use clap::Command;
use clap::builder::ValueParser;
use clap::parser::ValueSource;
use hermit::Error;
use serde::Deserialize;
use serde::Serialize;
use serde_yaml::Value;

/// The `schema` value of the format this hermit reads and writes.
pub(crate) const SCHEMA: &str = "hermit-run-config/v1";

const RUN: &str = "run";
const CONFIG_ID: &str = "config";
const SAVE_CONFIG_ID: &str = "save_config";
const FROM_ID: &str = "from";
const MANIFEST_ID: &str = "manifest";
const PROGRAM_ID: &str = "program";
const ARGS_ID: &str = "args";

/// The long names of the `hermit run` options whose resolved values
/// `--save-config` writes in place of what was given.
pub(crate) const EPOCH_KEY: &str = "epoch";
pub(crate) const SEED_KEY: &str = "seed";
pub(crate) const SEED_FROM_KEY: &str = "seed-from";
const REPLAY_NETWORKING_KEY: &str = "replay-networking";

/// One run config document. See the module documentation for the format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunConfig {
    /// Always [`SCHEMA`].
    pub(crate) schema: String,
    /// The version of the hermit that wrote the file. Informational: it is
    /// never checked.
    #[serde(
        rename = "hermit-version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) hermit_version: Option<String>,
    /// Options that come before the subcommand, by long name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) global: BTreeMap<String, Value>,
    /// Options of `hermit run`, by long name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) run: BTreeMap<String, Value>,
    /// The guest program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) program: Option<String>,
    /// The guest program's arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) args: Vec<String>,
    /// Reserved for a future syscall policy section; refused when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seccomp: Option<Value>,
}

impl RunConfig {
    fn new() -> Self {
        RunConfig {
            schema: SCHEMA.to_owned(),
            hermit_version: Some(crate::version::Version::get().to_owned()),
            global: BTreeMap::new(),
            run: BTreeMap::new(),
            program: None,
            args: Vec::new(),
            seccomp: None,
        }
    }

    /// The document `--save-config` writes.
    pub(crate) fn to_yaml(&self) -> Result<String, Error> {
        let body = serde_yaml::to_string(self).context("cannot encode the run config as YAML")?;
        Ok(format!(
            "# Written by `hermit run --save-config`. Reproduce the run with\n\
             # `hermit run --config FILE`; options given on that command line\n\
             # replace the ones here.\n{body}"
        ))
    }
}

/// Parses a run config document, refusing a missing or different `schema`,
/// unknown top-level keys and the reserved `seccomp` section. Option keys are
/// checked when the document is applied to a command ([`expand_argv`]).
pub(crate) fn parse(text: &str) -> Result<RunConfig, Error> {
    let document: Value =
        serde_yaml::from_str(text).context("the file is not a YAML (or JSON) document")?;
    let Value::Mapping(map) = &document else {
        anyhow::bail!(
            "a run config is a mapping that starts with `schema: {SCHEMA}`; this document is a \
             {}",
            kind(&document)
        );
    };
    match map.get("schema") {
        None => anyhow::bail!(
            "the document has no `schema` key; a run config starts with `schema: {SCHEMA}`. \
             Write one with `hermit run --save-config FILE`."
        ),
        Some(Value::String(schema)) if schema == SCHEMA => {}
        Some(other) => {
            let written_by = map
                .get("hermit-version")
                .and_then(Value::as_str)
                .map(|version| format!(" (it was written by hermit {version})"))
                .unwrap_or_default();
            anyhow::bail!(
                "the document has schema {}{written_by}, but this hermit reads only `{SCHEMA}`. \
                 Load it with the hermit that wrote it, or write it again with this hermit's \
                 `hermit run --save-config FILE`.",
                display_value(other)
            );
        }
    }
    const KEYS: &str = "`schema`, `hermit-version`, `global`, `run`, `program` and `args`";
    for key in map.keys() {
        if !matches!(
            key.as_str(),
            Some("schema" | "hermit-version" | "global" | "run" | "program" | "args" | "seccomp")
        ) {
            anyhow::bail!(
                "{} is not a run config key; the keys are {KEYS}",
                display_value(key)
            );
        }
    }
    let config: RunConfig = serde_yaml::from_value(document).with_context(|| {
        format!("a run config's keys are {KEYS}, described in docs/USER_GUIDE.md")
    })?;
    if config.seccomp.is_some() {
        anyhow::bail!(
            "the `seccomp` section is reserved for a syscall policy that this hermit does not \
             support yet; remove it from the file"
        );
    }
    Ok(config)
}

/// Reads and parses the run config at `path`.
pub(crate) fn read(path: &Path) -> Result<RunConfig, Error> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read run config {}", path.display()))?;
    parse(&text).with_context(|| format!("cannot load run config {}", path.display()))
}

/// Says so when a manifest's run started in another directory: a rerun
/// resolves relative paths against its own.
fn note_another_directory(manifest: &crate::manifest::Manifest) {
    let here = std::env::current_dir().ok();
    if let Some(there) = &manifest.command.cwd
        && here.as_deref() != Some(Path::new(there))
    {
        eprintln!(
            "hermit: note: the run this manifest records started in {there}; relative paths \
             in it resolve against the current directory"
        );
    }
}

/// [`from_manifest`] for the manifest read from `path`. A network
/// recording's rerun replays its trace: the trace the manifest names,
/// checked against the digest it recorded, becomes `--replay-networking`. A
/// syscall recording is replayed with `hermit replay`, not rerun.
pub(crate) fn from_manifest_at(
    manifest: &crate::manifest::Manifest,
    path: &Path,
) -> Result<RunConfig, Error> {
    use crate::manifest::Kind;
    let mut config = from_manifest(manifest)?;
    match manifest.kind {
        Kind::InputsOnly | Kind::History => {}
        Kind::Network => {
            let trace = crate::manifest::checked_network_trace(manifest, path)?;
            let trace = trace.to_str().ok_or_else(|| {
                anyhow::anyhow!("network trace path {} is not UTF-8", trace.display())
            })?;
            config
                .run
                .insert(REPLAY_NETWORKING_KEY.to_owned(), trace.into());
        }
        Kind::FullSyscall => anyhow::bail!(
            "{} describes a syscall recording, which `hermit run --from` does not rerun; replay \
             it with: hermit replay {}",
            path.display(),
            crate::manifest::directory_of(path).display()
        ),
    }
    Ok(config)
}

/// The run config a manifest's `inputs` and `command` stand for, which
/// `hermit run --from` loads as `--config` loads a file.
pub(crate) fn from_manifest(manifest: &crate::manifest::Manifest) -> Result<RunConfig, Error> {
    let section = |entries: &BTreeMap<String, serde_json::Value>| {
        entries
            .iter()
            .map(|(long, value)| {
                serde_json::from_value(value.clone())
                    .map(|value| (long.clone(), value))
                    .with_context(|| {
                        format!("the manifest's value for {long} is not an option value")
                    })
            })
            .collect::<Result<BTreeMap<String, Value>, Error>>()
    };
    let mut config = RunConfig::new();
    config.hermit_version = Some(manifest.hermit.version.clone());
    config.global = section(&manifest.inputs.global)?;
    config.run = section(&manifest.inputs.run)?;
    config.program = manifest.command.program.clone();
    config.args = manifest.command.args.clone();
    Ok(config)
}

/// One section of a manifest's `inputs`: option values by long name.
pub(crate) type JsonSection = BTreeMap<String, serde_json::Value>;

/// The `global` and `run` sections of a manifest's `inputs` for `config`,
/// the run config `--save-config` would write for the run.
pub(crate) fn to_manifest_parts(config: &RunConfig) -> Result<(JsonSection, JsonSection), Error> {
    let section = |entries: &BTreeMap<String, Value>| {
        entries
            .iter()
            .map(|(long, value)| {
                serde_json::to_value(value)
                    .map(|value| (long.clone(), value))
                    .with_context(|| format!("cannot encode the value of {long} as JSON"))
            })
            .collect::<Result<BTreeMap<_, _>, Error>>()
    };
    Ok((section(&config.global)?, section(&config.run)?))
}

/// Writes `config` to `path` as `--save-config` does, and reads the text back
/// so that a document that would not load is an error now rather than when
/// someone tries to reproduce the run.
pub(crate) fn write(path: &Path, config: &RunConfig) -> Result<(), Error> {
    let text = config.to_yaml()?;
    let reread = parse(&text).context("the run config this hermit encoded does not parse back")?;
    if reread != *config {
        anyhow::bail!(
            "the run config this hermit encoded parses back differently:\nwritten: \
             {config:?}\nread back: {reread:?}"
        );
    }
    std::fs::write(path, text)
        .with_context(|| format!("cannot write --save-config {}", path.display()))
}

/// How an option takes its value, and so how a run config spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// `key: true` gives the flag; `false` omits it.
    Flag,
    /// `key: N` gives the flag N times.
    Count,
    /// `key: VALUE`, one value.
    One,
    /// `key: [VALUE, ...]`, one entry per occurrence.
    Many,
}

/// One option a run config section may name.
#[derive(Clone, Debug)]
struct OptionSpec {
    id: String,
    long: String,
    shape: Shape,
}

fn shape(arg: &Arg) -> Option<Shape> {
    match arg.get_action() {
        ArgAction::SetTrue | ArgAction::SetFalse => Some(Shape::Flag),
        ArgAction::Count => Some(Shape::Count),
        ArgAction::Set => Some(Shape::One),
        ArgAction::Append => Some(Shape::Many),
        _ => None,
    }
}

/// The options of `command` that a run config can name: every option with
/// a long name, in declaration order. Positional arguments are the
/// document's `program` and `args`.
fn option_specs(command: &Command) -> Vec<OptionSpec> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_positional())
        .filter_map(|arg| {
            Some(OptionSpec {
                id: arg.get_id().as_str().to_owned(),
                long: arg.get_long()?.to_owned(),
                shape: shape(arg)?,
            })
        })
        .collect()
}

/// Options that belong to one invocation rather than to the run it
/// configures, so a run config neither holds nor accepts them.
fn invocation_only(id: &str) -> Option<&'static str> {
    match id {
        CONFIG_ID => Some("a run config cannot load another run config"),
        FROM_ID => Some("a run config cannot load a manifest"),
        MANIFEST_ID => Some(
            "whether to write a manifest is a choice of the invocation, not part of the run; \
             pass --manifest on the command line",
        ),
        SAVE_CONFIG_ID => Some(
            "where to save a config is a choice of the invocation, not part of the run; pass \
             --save-config on the command line",
        ),
        _ => None,
    }
}

fn run_command(command: &Command) -> &Command {
    command
        .find_subcommand(RUN)
        .expect("the hermit command has a `run` subcommand")
}

/// The run config holding the options of a parsed `hermit run` invocation
/// that were given on the command line or through an environment variable.
/// `global_matches` and `command` are the whole `hermit` invocation;
/// `run_matches` is its `run` subcommand.
pub(crate) fn capture(
    command: &Command,
    global_matches: &ArgMatches,
    run_matches: &ArgMatches,
) -> Result<RunConfig, Error> {
    let mut config = RunConfig::new();
    config.global = capture_section(command, global_matches)?;
    config.run = capture_section(run_command(command), run_matches)?;
    let positional = |id: &str| -> Result<Vec<String>, Error> {
        run_matches
            .get_raw(id)
            .into_iter()
            .flatten()
            .map(|value| utf8(value, id))
            .collect()
    };
    config.program = positional(PROGRAM_ID)?.pop();
    config.args = positional(ARGS_ID)?;
    Ok(config)
}

/// The global options `matches` took from the command line, respelled as
/// `--long[=VALUE]` tokens, for a command that reruns as `hermit [GLOBAL]
/// run --from` (`hermit replay`). Options from the environment are left to
/// the environment the rerun inherits.
pub(crate) fn global_tokens(
    command: &Command,
    matches: &ArgMatches,
) -> Result<Vec<OsString>, Error> {
    let specs = option_specs(command);
    let mut section = capture_section(command, matches)?;
    section.retain(|long, _| {
        specs.iter().any(|spec| {
            spec.long == *long && matches.value_source(&spec.id) == Some(ValueSource::CommandLine)
        })
    });
    file_tokens(Section::Global, &section, command, |_| false)
}

fn capture_section(
    command: &Command,
    matches: &ArgMatches,
) -> Result<BTreeMap<String, Value>, Error> {
    let mut section = BTreeMap::new();
    for spec in option_specs(command) {
        if invocation_only(&spec.id).is_some()
            || !matches!(
                matches.value_source(&spec.id),
                Some(ValueSource::CommandLine | ValueSource::EnvVariable)
            )
        {
            continue;
        }
        let raw = || -> Result<Vec<Value>, Error> {
            matches
                .get_raw(&spec.id)
                .into_iter()
                .flatten()
                .map(|value| utf8(value, &spec.long).map(|value| scalar(&value)))
                .collect()
        };
        let value = match spec.shape {
            Shape::Flag => Value::Bool(true),
            Shape::Count => Value::Number(u64::from(matches.get_count(&spec.id)).into()),
            Shape::One => raw()?
                .pop()
                .with_context(|| format!("--{} was given without a value", spec.long))?,
            Shape::Many => Value::Sequence(raw()?),
        };
        section.insert(spec.long, value);
    }
    Ok(section)
}

fn utf8(value: &OsStr, option: &str) -> Result<String, Error> {
    value.to_str().map(str::to_owned).with_context(|| {
        format!(
            "cannot save {option}: its value {value:?} is not UTF-8, which a YAML run config \
             cannot hold"
        )
    })
}

/// A command-line value as YAML: a canonical unsigned integer becomes a
/// number, anything else a string, so the value loads back as the same text.
fn scalar(text: &str) -> Value {
    match text.parse::<u64>() {
        Ok(number) if number.to_string() == text => Value::Number(number.into()),
        _ => Value::String(text.to_owned()),
    }
}

/// Rewrites a `hermit [GLOBAL] run --config FILE [OPTIONS] [-- PROGRAM ARGS]`
/// invocation into the command line it means: the file's options and guest,
/// less every option the command line gives itself, followed by the
/// command line's own options and guest. Returns `None`, leaving the
/// invocation to the ordinary parser, when it is not a `hermit run` that
/// names `--config`, or when it does not parse even leniently (an unknown
/// option, `--help`), which that parser then reports.
pub(crate) fn expand_argv(
    command: &Command,
    argv: &[OsString],
) -> Result<Option<Vec<OsString>>, Error> {
    let names = |long: &str| {
        let with_value = format!("{long}=");
        argv.iter()
            .any(|arg| arg == long || arg.to_str().is_some_and(|arg| arg.starts_with(&with_value)))
    };
    if !names("--config") && !names("--from") {
        return Ok(None);
    }
    let Ok(matches) = lenient_command(command).try_get_matches_from(argv) else {
        return Ok(None);
    };
    let Some(run_matches) = matches.subcommand_matches(RUN) else {
        return Ok(None);
    };
    let given_paths = |id: &str| -> Vec<&OsString> {
        run_matches
            .get_many::<OsString>(id)
            .into_iter()
            .flatten()
            .collect()
    };
    let (configs, manifests) = (given_paths(CONFIG_ID), given_paths(FROM_ID));
    let (path, config) = match (configs.as_slice(), manifests.as_slice()) {
        ([], []) => return Ok(None),
        ([path], []) => (PathBuf::from(path), read(Path::new(path))?),
        ([], [spec]) => {
            use crate::manifest::Target;
            let path = match crate::manifest::resolve(&crate::manifest::data_dir(), spec)? {
                Target::Manifest(path) => path,
                Target::Recording { data_dir, id } => anyhow::bail!(
                    "{} is a syscall recording, which `hermit run --from` does not rerun; \
                     replay it with: hermit replay {}",
                    spec.to_string_lossy(),
                    data_dir.join(id.to_string()).display()
                ),
            };
            let manifest = crate::manifest::read(&path)?;
            note_another_directory(&manifest);
            let config = from_manifest_at(&manifest, &path)?;
            (path, config)
        }
        ([_, ..], [_, ..]) => {
            anyhow::bail!("--config and --from both name a run's options; give one of them")
        }
        (paths, []) | ([], paths) => anyhow::bail!(
            "--{} was given {} times; give one",
            if configs.is_empty() { "from" } else { "config" },
            paths.len()
        ),
    };
    let run = run_command(command);
    let given =
        |matches: &ArgMatches, id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
    let in_section = |section: Section, entries, matches| {
        file_tokens(section, entries, command, |id| given(matches, id))
            .with_context(|| format!("cannot load run config {}", path.display()))
    };

    let mut expanded = vec![argv.first().cloned().unwrap_or_else(|| "hermit".into())];
    expanded.extend(in_section(Section::Global, &config.global, &matches)?);
    expanded.extend(given_tokens(command, &matches));
    expanded.push(RUN.into());
    expanded.extend(in_section(Section::Run, &config.run, run_matches)?);
    expanded.extend(given_tokens(run, run_matches));
    let guest: Vec<OsString> = if run_matches.contains_id(PROGRAM_ID) {
        [PROGRAM_ID, ARGS_ID]
            .into_iter()
            .flat_map(|id| run_matches.get_many::<OsString>(id).into_iter().flatten())
            .cloned()
            .collect()
    } else {
        config
            .program
            .iter()
            .chain(&config.args)
            .map(OsString::from)
            .collect()
    };
    if !guest.is_empty() {
        expanded.push("--".into());
        expanded.extend(guest);
    }
    Ok(Some(expanded))
}

/// A copy of the `hermit` command that accepts every option of the global
/// level and of `run` with any value and in any combination, keeping only
/// the names and value arity. It finds what the command line gave without
/// applying the rules that the file's options may be what satisfies, such as
/// `--verify-strict` requiring `--verify`.
fn lenient_command(command: &Command) -> Command {
    let lenient = |source: &Command| {
        let positionals = source
            .get_arguments()
            .filter(|arg| arg.is_positional())
            .count();
        let mut index = 0;
        let mut copy = Command::new(source.get_name().to_owned())
            .disable_help_flag(true)
            .disable_version_flag(true)
            .trailing_var_arg(source.is_trailing_var_arg_set());
        for arg in source.get_arguments() {
            let mut lenient = Arg::new(arg.get_id().as_str().to_owned());
            if arg.is_positional() {
                index += 1;
                lenient = lenient.index(index).value_parser(ValueParser::os_string());
                lenient = if index == positionals {
                    lenient
                        .action(ArgAction::Append)
                        .num_args(0..)
                        .allow_hyphen_values(true)
                        .trailing_var_arg(source.is_trailing_var_arg_set())
                } else {
                    lenient.action(ArgAction::Set)
                };
                copy = copy.arg(lenient);
                continue;
            }
            if let Some(long) = arg.get_long() {
                lenient = lenient.long(long.to_owned());
            }
            if let Some(short) = arg.get_short() {
                lenient = lenient.short(short);
            }
            if let Some(aliases) = arg.get_all_aliases() {
                lenient = lenient.aliases(aliases.into_iter().map(str::to_owned));
            }
            if let Some(shorts) = arg.get_all_short_aliases() {
                lenient = lenient.short_aliases(shorts);
            }
            // A global option may follow the subcommand, as on a typed command line.
            lenient = lenient.global(arg.is_global_set());
            lenient = match shape(arg) {
                Some(Shape::Flag | Shape::Count) => lenient.action(ArgAction::Count),
                _ => lenient
                    .action(ArgAction::Append)
                    .value_parser(ValueParser::os_string())
                    .num_args(1)
                    .allow_hyphen_values(arg.is_allow_hyphen_values_set())
                    .require_equals(arg.is_require_equals_set()),
            };
            copy = copy.arg(lenient);
        }
        copy
    };
    lenient(command)
        .allow_external_subcommands(true)
        .subcommand(lenient(run_command(command)))
}

/// The command-line options of a lenient parse, respelled as `--long[=VALUE]`
/// tokens, `--config` left out.
fn given_tokens(command: &Command, matches: &ArgMatches) -> Vec<OsString> {
    let mut tokens = Vec::new();
    for spec in option_specs(command) {
        if spec.id == CONFIG_ID
            || spec.id == FROM_ID
            || matches.value_source(&spec.id) != Some(ValueSource::CommandLine)
        {
            continue;
        }
        match spec.shape {
            Shape::Flag | Shape::Count => {
                for _ in 0..matches.get_count(&spec.id) {
                    tokens.push(format!("--{}", spec.long).into());
                }
            }
            Shape::One | Shape::Many => {
                for value in matches.get_many::<OsString>(&spec.id).into_iter().flatten() {
                    tokens.push(valued_token(&spec.long, value));
                }
            }
        }
    }
    tokens
}

fn valued_token(long: &str, value: &OsStr) -> OsString {
    let mut token = OsString::from(format!("--{long}="));
    token.push(value);
    token
}

/// The two option sections of a run config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    /// `global:`, the options before the subcommand.
    Global,
    /// `run:`, the options of `hermit run`.
    Run,
}

impl Section {
    fn name(self) -> &'static str {
        match self {
            Section::Global => "global",
            Section::Run => RUN,
        }
    }

    /// The command whose options this section names, given the whole
    /// `hermit` command.
    fn command(self, hermit: &Command) -> &Command {
        match self {
            Section::Global => hermit,
            Section::Run => run_command(hermit),
        }
    }

    fn other(self) -> Section {
        match self {
            Section::Global => Section::Run,
            Section::Run => Section::Global,
        }
    }

    fn help(self) -> &'static str {
        match self {
            Section::Global => "`hermit --help`",
            Section::Run => "`hermit run --help`",
        }
    }
}

/// The command-line tokens for one section of a run config, skipping the
/// options `given` reports the command line gives itself. `hermit` is the
/// whole `hermit` command.
fn file_tokens(
    section: Section,
    entries: &BTreeMap<String, Value>,
    hermit: &Command,
    given: impl Fn(&str) -> bool,
) -> Result<Vec<OsString>, Error> {
    let specs = option_specs(section.command(hermit));
    let mut tokens = Vec::new();
    for (key, value) in entries {
        let Some(spec) = specs.iter().find(|spec| spec.long == *key) else {
            anyhow::bail!("{}", unknown_key(section, key, hermit));
        };
        let section = section.name();
        if let Some(reason) = invocation_only(&spec.id) {
            anyhow::bail!("`{section}.{key}` is not allowed in a run config: {reason}");
        }
        if given(&spec.id) {
            continue;
        }
        let wrong = |expected: &str| {
            anyhow::anyhow!(
                "`{section}.{key}` must be {expected}, not {}",
                display_value(value)
            )
        };
        match (spec.shape, value) {
            (Shape::Flag, Value::Bool(true)) => tokens.push(format!("--{key}").into()),
            (Shape::Flag, Value::Bool(false)) => {}
            (Shape::Flag, _) => {
                return Err(wrong(
                    "`true` (a flag; write `true` to give it, or leave the key out)",
                ));
            }
            (Shape::Count, Value::Number(number)) => {
                let count = number
                    .as_u64()
                    .ok_or_else(|| wrong("a count (a non-negative integer)"))?;
                for _ in 0..count {
                    tokens.push(format!("--{key}").into());
                }
            }
            (Shape::Count, _) => return Err(wrong("a count (a non-negative integer)")),
            (Shape::One, Value::Sequence(_)) => {
                return Err(wrong(&format!(
                    "one value (--{key} is not repeatable, so it takes no list)"
                )));
            }
            (Shape::One, value) => {
                let text = scalar_text(value).ok_or_else(|| wrong("a single value"))?;
                tokens.push(valued_token(key, OsStr::new(&text)));
            }
            (Shape::Many, Value::Sequence(values)) => {
                for value in values {
                    let text =
                        scalar_text(value).ok_or_else(|| wrong("a list of single values"))?;
                    tokens.push(valued_token(key, OsStr::new(&text)));
                }
            }
            (Shape::Many, value) => {
                let text = scalar_text(value).ok_or_else(|| wrong("a list of single values"))?;
                tokens.push(valued_token(key, OsStr::new(&text)));
            }
        }
    }
    Ok(tokens)
}

/// The command-line text of a YAML scalar, or `None` for a null, list or
/// mapping.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => None,
    }
}

/// Why `key` names no option of `section`, with the spelling that would.
/// `hermit` is the whole `hermit` command.
fn unknown_key(section: Section, key: &str, hermit: &Command) -> String {
    let command = section.command(hermit);
    let names = |section: Section, key: &str| {
        option_specs(section.command(hermit))
            .iter()
            .any(|spec| spec.long == key)
    };
    let by_alias = command.get_arguments().find(|arg| {
        arg.get_all_aliases()
            .is_some_and(|aliases| aliases.contains(&key))
    });
    let hyphenated = key.replace('_', "-");
    let other = section.other();
    let suggestion = if let Some(long) = by_alias.and_then(Arg::get_long) {
        format!("`{key}` is an alias; write the option's name, `{long}`")
    } else if names(section, &hyphenated) {
        format!("keys are long option names, with hyphens: write `{hyphenated}`")
    } else if names(other, key) || names(other, &hyphenated) {
        format!(
            "it is an option of the `{}` section; move it there",
            other.name()
        )
    } else {
        format!(
            "the keys of `{}` are the long option names {} shows, without their leading `--`",
            section.name(),
            section.help()
        )
    };
    format!("`{}.{key}` names no option: {suggestion}", section.name())
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Sequence(_) => "list",
        Value::Mapping(_) => "mapping",
        Value::Tagged(_) => "tagged value",
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => format!("`{text}`"),
        Value::Number(number) => format!("`{number}`"),
        Value::Bool(flag) => format!("`{flag}`"),
        other => format!("a {}", kind(other)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;
    use std::time::UNIX_EPOCH;

    use clap::CommandFactory;

    use super::*;

    /// The clock the saving parse samples an omitted epoch from.
    const SAVING_CLOCK: u64 = 1_700_000_000;
    /// The clock the loading parse would sample, if the file did not give
    /// the epoch.
    const LOADING_CLOCK: u64 = 1_800_000_000;

    /// One sample invocation per option a run config can hold: its long name
    /// and command-line tokens that give it a non-default value (with any
    /// option it requires), here for the global options. An option
    /// without a row fails `every_option_has_a_round_trip_sample`, so a new
    /// option cannot join `hermit run` without proving it survives a save and
    /// a load.
    const GLOBAL_SAMPLES: &[(&str, &[&str])] = &[
        ("log", &["--log=info"]),
        (
            "log-file",
            &["-l", "debug", "--log-file=/nonexistent/hermit.log"],
        ),
        ("max-log-bytes", &["--max-log-bytes=8G"]),
        ("backend", &["--backend", "ptrace"]),
        (
            "unsafe-ignore-host-seccomp",
            &["--unsafe-ignore-host-seccomp"],
        ),
    ];

    /// The `run` rows of the sample table; see [`GLOBAL_SAMPLES`].
    const RUN_SAMPLES: &[(&str, &[&str])] = &[
        ("happens-before", &["--happens-before=/tmp/hb.json"]),
        (
            "hb-list-events",
            &["--happens-before=/tmp/hb.json", "--hb-list-events"],
        ),
        ("fatal-core-dir", &["--fatal-core-dir=/tmp/cores"]),
        (
            "fatal-core-max-bytes",
            &["--fatal-core-dir=/tmp/cores", "--fatal-core-max-bytes=1000"],
        ),
        (
            "fatal-core-total-max-bytes",
            &[
                "--fatal-core-dir=/tmp/cores",
                "--fatal-core-total-max-bytes=2000",
            ],
        ),
        ("no-virtualize-time", &["--no-virtualize-time"]),
        ("no-virtualize-cpuid", &["--no-virtualize-cpuid"]),
        ("epoch", &["--epoch", "2001-02-03T04:05:06.123Z"]),
        ("seed", &["--seed=7"]),
        ("rng-seed", &["--rng-seed=18446744073709551615"]),
        ("fuzz-seed", &["--fuzz-seed=9"]),
        ("clock-multiplier", &["--clock-multiplier=1.50"]),
        ("no-virtualize-metadata", &["--no-virtualize-metadata"]),
        ("sequentialize-threads", &["--sequentialize-threads"]),
        ("runs-post-fork", &["--runs-post-fork=parent"]),
        ("passthru-opt", &["--passthru-opt"]),
        ("imprecise-timers", &["--imprecise-timers"]),
        ("chaos", &["--chaos"]),
        ("fuzz-futexes", &["--fuzz-futexes"]),
        ("chaos-target-races", &["--chaos-target-races"]),
        (
            "chaos-per-thread-slowdown",
            &["--chaos-per-thread-slowdown"],
        ),
        (
            "chaos-slowdown-max-factor",
            &["--chaos-slowdown-max-factor=4.0"],
        ),
        ("chaos-epoch-length-ns", &["--chaos-epoch-length-ns=100"]),
        ("record-preemptions", &["--record-preemptions"]),
        (
            "record-preemptions-to",
            &["--record-preemptions-to=/tmp/p.json"],
        ),
        (
            "replay-preemptions-from",
            &["--replay-preemptions-from=/tmp/r.json"],
        ),
        (
            "replay-schedule-from",
            &["--replay-schedule-from=/tmp/s.json"],
        ),
        ("replay-exhausted-panic", &["--replay-exhausted-panic"]),
        ("die-on-desync", &["--die-on-desync"]),
        (
            "stacktrace-event",
            &["--stacktrace-event=3", "-s", "5,/tmp/st.txt"],
        ),
        ("stacktrace-signal", &["--stacktrace-signal=SIGINT"]),
        ("preemption-stacktrace", &["--preemption-stacktrace"]),
        (
            "preemption-stacktrace-log-file",
            &["--preemption-stacktrace-log-file=/tmp/pst.log"],
        ),
        ("deterministic-io", &["--deterministic-io"]),
        (
            "panic-on-unsupported-syscalls",
            &["--panic-on-unsupported-syscalls"],
        ),
        ("panic-on-rbc-overshoot", &["--panic-on-rcb-overshoot"]),
        ("kill-daemons", &["--kill-daemons"]),
        ("gdbserver", &["--gdbserver"]),
        ("gdbserver-port", &["--gdbserver-port=4321"]),
        ("max-timeslice", &["--preemption-timeout=disabled"]),
        ("target-timeslice", &["--target-timeslice=5000"]),
        (
            "target-timeslice-syscalls-only",
            &[
                "--target-timeslice=5000",
                "--target-timeslice-syscalls-only",
            ],
        ),
        ("futex-wake-yields", &["--futex-wake-yields"]),
        ("scheduler-turn-cost", &["--scheduler-turn-cost=10000"]),
        ("sigint-instakill", &["--sigint-instakill"]),
        ("warn-non-zero-binds", &["--warn-non-zero-binds"]),
        ("sched-heuristic", &["--sched-heuristic=random"]),
        ("sched-seed", &["--sched-seed=11"]),
        (
            "sched-sticky-random-param",
            &["--sched-sticky-random-param=0.5"],
        ),
        ("stop-after-turn", &["--stop-after-turn=3"]),
        ("stop-after-iter", &["--stop-after-iter=4"]),
        (
            "debug-externalize-sockets",
            &["--debug-externalize-sockets"],
        ),
        ("debug-futex-mode", &["--debug-futex-mode=polling"]),
        ("no-rcb-time", &["--no-rcb-time"]),
        ("detlog-heap", &["--detlog-heap"]),
        ("detlog-stack", &["--detlog-stack"]),
        ("detlog-regs", &["--detlog-regs"]),
        ("no-detlog-io-buffers", &["--no-detlog-io-buffers"]),
        ("detlog-regs-cadence", &["--detlog-regs-cadence=2"]),
        ("sysinfo-uptime-offset", &["--sysinfo-uptime-offset=60"]),
        ("memory", &["--memory=2GB"]),
        (
            "interrupt-at",
            &["--interrupt-at=1:100", "--interrupt-at", "2:200"],
        ),
        ("strict", &["--strict"]),
        (
            "allow-unsupported-syscalls",
            &["--allow-unsupported-syscalls"],
        ),
        ("timeout", &["--timeout=30"]),
        ("no-sequentialize-threads", &["--no-sequentialize-threads"]),
        ("no-deterministic-io", &["--no-deterministic-io"]),
        ("pin-threads", &["--pin-threads"]),
        ("skid-margin", &["--skid-margin=5"]),
        (
            "mount",
            &[
                "--mount=type=tmpfs,target=/x",
                "--mount",
                "type=bind,source=/tmp,target=/y",
            ],
        ),
        ("bind", &["--bind=/tmp", "--bind=/etc:/tmp/etc"]),
        ("network", &["--net=host"]),
        ("record-networking", &["--record-networking=/tmp/new.trace"]),
        ("replay-networking", &["--replay-networking=/tmp/old.trace"]),
        ("namespace-only", &["--lite"]),
        ("no-namespace", &["--core-only"]),
        ("strace-only", &["--strace-only"]),
        ("tmp", &["--tmp=/tmp/guest tmp"]),
        ("image", &["--image=docker.io/library/busybox@sha256:00"]),
        ("seed-from", &["--seed-from=Args"]),
        ("verify", &["--verify"]),
        ("verify-verbose", &["--verify", "--verify-verbose"]),
        ("verify-strict", &["--verify", "--verify-strict"]),
        ("verify-allow", &["--verify-allow=both"]),
        ("print-verify-logs", &["--verify", "--verify-logs"]),
        ("keep-logs", &["--verify", "--keep-logs"]),
        (
            "verify-log-dir",
            &["--verify", "--keep-logs", "--verify-log-dir=/tmp/vl"],
        ),
        ("verify-json", &["--verify", "--verify-json=/tmp/v.json"]),
        ("verify-run", &["--verify-run=/tmp/vr"]),
        ("summary", &["-u"]),
        ("summary-json", &["--summary-json=/tmp/s.json"]),
        (
            "backend-engagement-json",
            &["--backend-engagement-json=/tmp/be.json"],
        ),
        ("analyze-networking", &["--analyze-networking"]),
        ("base-env", &["--base-env=empty"]),
        (
            "env",
            &["-e", "A=1", "--env=B= two words ", "--env=PASS_THROUGH"],
        ),
        ("workdir", &["--workdir=/tmp"]),
        ("save-config", &["--save-config=/tmp/x.yaml"]),
        ("manifest", &["--manifest=never"]),
    ];

    /// Every sample row with its section.
    fn samples() -> impl Iterator<Item = (Section, &'static str, &'static [&'static str])> {
        let rows = |section, rows: &'static [(&'static str, &'static [&'static str])]| {
            rows.iter()
                .map(move |(long, tokens)| (section, *long, *tokens))
        };
        rows(Section::Global, GLOBAL_SAMPLES).chain(rows(Section::Run, RUN_SAMPLES))
    }

    /// The guest every sample runs, with arguments that look like options.
    const GUEST: &[&str] = &["--", "/bin/echo", "x", "-y", "--seed=3"];

    fn argv(global: &[&str], run: &[&str], guest: &[&str]) -> Vec<OsString> {
        std::iter::once("hermit")
            .chain(global.iter().copied())
            .chain(std::iter::once(RUN))
            .chain(run.iter().copied())
            .chain(guest.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn sample_argv(section: Section, tokens: &[&str]) -> Vec<OsString> {
        match section {
            Section::Global => argv(tokens, &[], GUEST),
            Section::Run => argv(&[], tokens, GUEST),
        }
    }

    /// What a run config must reproduce of a parsed invocation: the global
    /// options and the `run` options, compared field by field.
    #[derive(Debug, PartialEq, Eq)]
    struct Parsed {
        global: String,
        run: String,
    }

    fn parse_at(argv: &[OsString], seconds: u64) -> (crate::Args, ArgMatches) {
        let matches = crate::Args::command()
            .try_get_matches_from(argv)
            .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
        let args = crate::args_from_matches_with_clock(&matches, || {
            UNIX_EPOCH + Duration::from_secs(seconds)
        })
        .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
        (args, matches)
    }

    fn parsed(args: &crate::Args) -> Parsed {
        let crate::Subcommand::Run(run) = &args.command else {
            panic!("not a `run` command: {args:?}");
        };
        Parsed {
            global: format!("{:#?}", args.global),
            run: run.config_equivalence_key(),
        }
    }

    fn load(path: &Path, run: &[&str]) -> Vec<OsString> {
        let mut command_line = argv(&[], &["--config"], &[]);
        command_line.push(path.into());
        command_line.extend(run.iter().map(OsString::from));
        expand_argv(&crate::Args::command(), &command_line)
            .unwrap_or_else(|error| panic!("{command_line:?} should load: {error:#}"))
            .unwrap_or_else(|| panic!("{command_line:?} names --config"))
    }

    /// Parses `argv`, saves it as `--save-config` would, loads the file
    /// through `--config` at a later clock, and returns both parses and the
    /// saved document.
    fn save_and_load(argv: &[OsString]) -> (Parsed, Parsed, RunConfig) {
        let (mut args, matches) = parse_at(argv, SAVING_CLOCK);
        let config = capture(
            &crate::Args::command(),
            &matches,
            matches.subcommand_matches(RUN).unwrap(),
        )
        .unwrap();
        let crate::Subcommand::Run(run) = &mut args.command else {
            panic!("{argv:?} is not a `run` command");
        };
        run.set_config_capture(config);
        let saved = run.saved_run_config().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run.yaml");
        write(&path, &saved).unwrap();
        assert_eq!(read(&path).unwrap(), saved);
        let (reloaded, _) = parse_at(&load(&path, &[]), LOADING_CLOCK);
        (parsed(&args), parsed(&reloaded), saved)
    }

    /// A manifest stands for the run config `--save-config` would write: for
    /// every option, a manifest made from an invocation that gives it, loaded
    /// with `--from`, reproduces every parsed field, as `--config` does
    /// (https://github.com/rrnewton/hermit/issues/4271).
    #[test]
    fn every_option_survives_a_manifest_and_from() {
        for (section, long, tokens) in samples() {
            let argv = sample_argv(section, tokens);
            let (mut args, matches) = parse_at(&argv, SAVING_CLOCK);
            let config = capture(
                &crate::Args::command(),
                &matches,
                matches.subcommand_matches(RUN).unwrap(),
            )
            .unwrap();
            let crate::Subcommand::Run(run) = &mut args.command else {
                panic!("{argv:?} is not a `run` command");
            };
            run.set_config_capture(config);
            let saved = run.saved_run_config().unwrap();
            let (global, run_inputs) = to_manifest_parts(&saved).unwrap();
            let manifest = crate::manifest::Manifest::new(
                hermit::Id::unique(),
                crate::manifest::Kind::InputsOnly,
                crate::manifest::Command {
                    program: saved.program.clone(),
                    args: saved.args.clone(),
                    cwd: None,
                },
                crate::manifest::Inputs {
                    global,
                    run: run_inputs,
                    host_drawn: BTreeMap::new(),
                },
            );
            assert_eq!(from_manifest(&manifest).unwrap().run, saved.run, "--{long}");
            let directory = tempfile::tempdir().unwrap();
            let path = directory
                .path()
                .join("id")
                .join(crate::manifest::MANIFEST_NAME);
            crate::manifest::write(&path, &manifest).unwrap();
            let command_line = argv_for_from(path.parent().unwrap());
            let expanded = expand_argv(&crate::Args::command(), &command_line)
                .unwrap()
                .expect("--from expands");
            let (reloaded, _) = parse_at(&expanded, LOADING_CLOCK);
            assert_eq!(parsed(&args), parsed(&reloaded), "--{long}: {manifest:#?}");
        }
    }

    fn argv_for_from(path: &Path) -> Vec<OsString> {
        let mut command_line = argv(&[], &["--from"], &[]);
        command_line.push(path.into());
        command_line
    }

    /// A run config cannot hold `--from` or `--manifest`, which belong to
    /// one invocation.
    #[test]
    fn a_run_config_refuses_from_and_manifest() {
        let directory = tempfile::tempdir().unwrap();
        for entry in ["from: /x", "manifest: always"] {
            let path = config_file(
                &directory,
                &format!("schema: {SCHEMA}\nrun:\n  {entry}\nprogram: /bin/true\n"),
            );
            let mut command_line = argv(&[], &["--config"], &[]);
            command_line.push(path.into());
            assert!(
                expand_argv(&crate::Args::command(), &command_line).is_err(),
                "{entry}"
            );
        }
    }

    /// A network manifest's rerun replays the trace it names, resolved
    /// against the manifest's directory; a syscall recording's manifest is
    /// refused with the `hermit replay` command to use instead.
    #[test]
    fn from_a_network_manifest_replays_its_trace() {
        use crate::manifest;
        let directory = tempfile::tempdir().unwrap();
        let trace = directory.path().join("net.trace");
        std::fs::write(&trace, b"HERMIT-NET-TRACE\x04\0\0\0").unwrap();
        let (sha256, bytes) = manifest::digest_and_size(&trace).unwrap();
        let mut document = manifest::Manifest::new(
            hermit::Id::unique(),
            manifest::Kind::Network,
            manifest::Command {
                program: Some("/bin/true".to_owned()),
                args: Vec::new(),
                cwd: None,
            },
            manifest::Inputs {
                global: BTreeMap::new(),
                run: BTreeMap::from([("seed".to_owned(), 7.into())]),
                host_drawn: BTreeMap::new(),
            },
        );
        document.artifacts = Some(manifest::Artifacts {
            network_trace: Some(manifest::NetworkTrace {
                path: "net.trace".to_owned(),
                format: manifest::NETWORK_TRACE_FORMAT.to_owned(),
                version: Some(4),
                sha256: Some(sha256),
                bytes: Some(bytes),
            }),
            syscall_recording: None,
        });
        let path = manifest::network_manifest_path(&trace);
        let config = from_manifest_at(&document, &path).unwrap();
        assert_eq!(
            config.run[REPLAY_NETWORKING_KEY],
            Value::from(trace.to_str().unwrap())
        );
        assert_eq!(config.run["seed"], Value::from(7));

        document.kind = manifest::Kind::FullSyscall;
        let error = from_manifest_at(&document, &path).unwrap_err();
        assert!(format!("{error:#}").contains("hermit replay"), "{error:#}");
    }

    /// `--from` and `--config` together are refused, as two sources for one
    /// run's options.
    #[test]
    fn from_and_config_together_are_refused() {
        let mut command_line = argv(&[], &["--config=/nonexistent.yaml", "--from"], &[]);
        command_line.push("/nonexistent".into());
        let error = expand_argv(&crate::Args::command(), &command_line).unwrap_err();
        assert!(
            format!("{error:#}").contains("give one of them"),
            "{error:#}"
        );
    }

    #[test]
    fn every_option_has_a_round_trip_sample() {
        let hermit = crate::Args::command();
        let options: BTreeSet<(&str, String)> = [Section::Global, Section::Run]
            .into_iter()
            .flat_map(|section| {
                option_specs(section.command(&hermit))
                    .into_iter()
                    .filter(|spec| spec.id != CONFIG_ID && spec.id != FROM_ID)
                    .map(move |spec| (section.name(), spec.long))
            })
            .collect();
        let sampled: BTreeSet<(&str, String)> = samples()
            .map(|(section, long, _)| (section.name(), long.to_owned()))
            .collect();
        assert_eq!(
            sampled.len(),
            samples().count(),
            "an option has two sample rows"
        );
        assert_eq!(
            options.difference(&sampled).collect::<Vec<_>>(),
            Vec::<&(&str, String)>::new(),
            "these options have no sample row proving they survive --save-config and --config"
        );
        assert_eq!(
            sampled.difference(&options).collect::<Vec<_>>(),
            Vec::<&(&str, String)>::new(),
            "these sample rows name no option"
        );
    }

    /// The guarantee itself: for every option, saving an invocation that
    /// gives it and loading the file reproduces every parsed field.
    #[test]
    fn every_option_survives_save_and_load() {
        let (default, reloaded_default, _) = save_and_load(&sample_argv(Section::Run, &[]));
        assert_eq!(default, reloaded_default);
        for (section, long, tokens) in samples() {
            let (before, after, saved) = save_and_load(&sample_argv(section, tokens));
            assert_eq!(before, after, "--{long}: {saved:#?}");
            let entries = match section {
                Section::Global => &saved.global,
                Section::Run => &saved.run,
            };
            if invocation_only(&long.replace('-', "_")).is_some() {
                // Never written, and left out of the comparison key.
                assert!(!entries.contains_key(long), "{saved:#?}");
            } else {
                assert!(entries.contains_key(long), "--{long}: {saved:#?}");
                assert_ne!(
                    before, default,
                    "the --{long} sample {tokens:?} parses to the defaults, so it proves nothing"
                );
            }
            assert_eq!(saved.program.as_deref(), Some("/bin/echo"));
            assert_eq!(saved.args, ["x", "-y", "--seed=3"]);
        }
    }

    /// An epoch sampled from the host clock is written out, so a load at
    /// another time starts from the saved epoch.
    #[test]
    fn a_host_clock_epoch_is_saved_and_reused() {
        let (before, after, saved) = save_and_load(&sample_argv(Section::Run, &[]));
        assert_eq!(saved.run[EPOCH_KEY], "2023-11-14T22:13:20+00:00");
        assert_eq!(before, after);
    }

    /// A value from an option's environment variable is an implicit input,
    /// and is saved as an explicit one.
    #[test]
    fn an_environment_variable_value_is_saved() {
        const VARIABLE: &str = "HERMIT_RUN_CONFIG_TEST_ONLY_VARIABLE";
        // SAFETY: no other code reads or writes this test-only variable.
        unsafe { std::env::set_var(VARIABLE, "from-env") };
        let command = Command::new("probe")
            .arg(Arg::new("probe").long("probe").env(VARIABLE))
            .arg(Arg::new("unset").long("unset").default_value("default"));
        let matches = command.clone().try_get_matches_from(["probe"]).unwrap();
        let section = capture_section(&command, &matches).unwrap();
        assert_eq!(
            section,
            BTreeMap::from([("probe".to_owned(), "from-env".into())])
        );
    }

    fn config_file(directory: &tempfile::TempDir, text: &str) -> std::path::PathBuf {
        let path = directory.path().join("run.yaml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn run_options(argv: &[OsString]) -> crate::Args {
        parse_at(argv, LOADING_CLOCK).0
    }

    /// The command line outranks the file option by option: a repeatable
    /// option's list is replaced whole, and the options it does not give
    /// come from the file. Short spellings count as given.
    #[test]
    fn command_line_options_replace_the_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = config_file(
            &directory,
            "schema: hermit-run-config/v1\n\
             global: {log: info, log-file: /from/file.log}\n\
             run: {seed: 1, env: [A=1, B=2], verify: true, summary: true}\n\
             program: /bin/from-file\n\
             args: [x]\n",
        );
        let mut command_line = argv(&["--log-file=/from/cli.log"], &["--config"], &[]);
        command_line.push(path.clone().into());
        command_line.extend(["--seed=2", "-e", "C=3"].map(OsString::from));
        let expanded = expand_argv(&crate::Args::command(), &command_line)
            .unwrap()
            .unwrap();
        let args = run_options(&expanded);
        assert_eq!(
            args.global.log,
            Some(tracing::level_filters::LevelFilter::INFO)
        );
        assert_eq!(args.global.log_file, Some("/from/cli.log".into()));
        let crate::Subcommand::Run(run) = &args.command else {
            panic!("{expanded:?}");
        };
        let expected = run_options(&argv(
            &["--log=info", "--log-file=/from/cli.log"],
            &["--seed=2", "--env=C=3", "--verify", "--summary"],
            &["--", "/bin/from-file", "x"],
        ));
        let crate::Subcommand::Run(expected) = &expected.command else {
            unreachable!()
        };
        assert_eq!(
            run.config_equivalence_key(),
            expected.config_equivalence_key()
        );

        // A guest on the command line replaces the file's program and args.
        let args = run_options(&load(&path, &["--", "/bin/from-cli", "-y"]));
        let crate::Subcommand::Run(run) = &args.command else {
            unreachable!()
        };
        let expected = run_options(&argv(
            &["--log=info", "--log-file=/from/file.log"],
            &[
                "--seed=1",
                "--env=A=1",
                "--env=B=2",
                "--verify",
                "--summary",
            ],
            &["--", "/bin/from-cli", "-y"],
        ));
        let crate::Subcommand::Run(expected) = &expected.command else {
            unreachable!()
        };
        assert_eq!(
            run.config_equivalence_key(),
            expected.config_equivalence_key()
        );
    }

    /// The file's options can satisfy what the command line's require, and
    /// the combined command line is checked like a typed one.
    #[test]
    fn the_combined_command_line_is_checked_as_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = config_file(
            &directory,
            "{\"schema\": \"hermit-run-config/v1\", \"run\": {\"verify\": true}, \"program\": \"/bin/true\"}",
        );
        run_options(&load(&path, &["--verify-strict"]));

        let path = config_file(
            &directory,
            "schema: hermit-run-config/v1\nrun: {verify-strict: true}\n",
        );
        let expanded = load(&path, &["--", "/bin/true"]);
        let error = crate::Args::command()
            .try_get_matches_from(&expanded)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--verify"), "{error}");
    }

    /// `false` for a flag is the same as leaving it out.
    #[test]
    fn a_false_flag_is_omitted() {
        let directory = tempfile::tempdir().unwrap();
        let path = config_file(
            &directory,
            "schema: hermit-run-config/v1\nrun: {verify: false, seed: 4}\nprogram: /bin/true\n",
        );
        let mut expected = argv(&[], &["--seed=4"], &["--", "/bin/true"]);
        assert_eq!(load(&path, &[]), expected);
        expected.truncate(3);
        assert_ne!(expected, Vec::<OsString>::new());
    }

    fn load_error(text: &str) -> String {
        let directory = tempfile::tempdir().unwrap();
        let path = config_file(&directory, text);
        let mut command_line = argv(&[], &["--config"], &[]);
        command_line.push(path.into());
        format!(
            "{:#}",
            expand_argv(&crate::Args::command(), &command_line).unwrap_err()
        )
    }

    #[test]
    fn unknown_keys_are_refused_with_the_working_spelling() {
        for (text, expected) in [
            (
                "schema: hermit-run-config/v1\nrun: {no_namespace: true}\n",
                "`run.no_namespace` names no option: keys are long option names, with hyphens: \
                 write `no-namespace`",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {lite: true}\n",
                "`run.lite` names no option: `lite` is an alias; write the option's name, \
                 `namespace-only`",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {backend: ptrace}\n",
                "`run.backend` names no option: it is an option of the `global` section",
            ),
            (
                "schema: hermit-run-config/v1\nglobal: {seed: 3}\n",
                "`global.seed` names no option: it is an option of the `run` section",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {frobnicate: 1}\n",
                "the keys of `run` are the long option names `hermit run --help` shows",
            ),
            (
                "schema: hermit-run-config/v1\nruns: {}\n",
                "`runs` is not a run config key; the keys are `schema`",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {config: other.yaml}\n",
                "a run config cannot load another run config",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {save-config: x.yaml}\n",
                "pass --save-config on the command line",
            ),
        ] {
            let error = load_error(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn a_missing_or_different_schema_is_refused() {
        for (text, expected) in [
            (
                "schema: hermit-run-config/v2\nhermit-version: '9.9'\nrun: {new-option: 1}\n",
                "the document has schema `hermit-run-config/v2` (it was written by hermit 9.9), \
                 but this hermit reads only `hermit-run-config/v1`",
            ),
            (
                "run: {seed: 1}\n",
                "the document has no `schema` key; a run config starts with `schema: \
                 hermit-run-config/v1`",
            ),
            ("schema: 1\n", "the document has schema `1`"),
            ("- schema\n", "this document is a list"),
            ("schema: [\n", "not a YAML (or JSON) document"),
        ] {
            let error = load_error(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn the_reserved_seccomp_section_is_refused() {
        let error = load_error(
            "{\"schema\": \"hermit-run-config/v1\", \"seccomp\": {\"defaultAction\": \"SCMP_ACT_ERRNO\"}}",
        );
        assert!(
            error.contains("the `seccomp` section is reserved"),
            "{error}"
        );
    }

    #[test]
    fn values_of_the_wrong_shape_are_refused() {
        for (text, expected) in [
            (
                "schema: hermit-run-config/v1\nrun: {verify: yes}\n",
                "`run.verify` must be `true` (a flag",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {seed: [1, 2]}\n",
                "`run.seed` must be one value (--seed is not repeatable, so it takes no list)",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {seed: null}\n",
                "`run.seed` must be a single value, not a null",
            ),
            (
                "schema: hermit-run-config/v1\nrun: {env: [{A: 1}]}\n",
                "`run.env` must be a list of single values",
            ),
        ] {
            let error = load_error(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
        // A single value is a one-entry list.
        let directory = tempfile::tempdir().unwrap();
        let path = config_file(
            &directory,
            "schema: hermit-run-config/v1\nrun: {env: A=1}\n",
        );
        assert_eq!(load(&path, &[]), argv(&[], &["--env=A=1"], &[]));
    }

    /// Only a top-level `hermit run` that names `--config` is rewritten.
    #[test]
    fn other_invocations_are_left_to_the_parser() {
        let hermit = crate::Args::command();
        for command_line in [
            argv(&[], &[], &["--", "/bin/echo", "--config", "x"]),
            argv(&[], &["--config=x", "--help"], &[]),
            ["hermit", "oci", "run", "--config=x", "image", "/bin/true"]
                .map(OsString::from)
                .to_vec(),
            argv(&[], &["--seed=1"], &["--", "/bin/true"]),
        ] {
            assert_eq!(
                expand_argv(&hermit, &command_line).unwrap(),
                None,
                "{command_line:?}"
            );
        }
        let error = expand_argv(&hermit, &argv(&[], &["--config=a", "--config=b"], &[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--config was given 2 times"), "{error}");
    }

    /// A global option given after the subcommand, as clap allows for one
    /// declared global, is saved in the `global` section and recognised on a
    /// command line that loads a config.
    #[test]
    fn a_global_option_after_the_subcommand_is_saved_and_loaded() {
        let command_line = argv(&[], &["--unsafe-ignore-host-seccomp"], GUEST);
        let (before, after, saved) = save_and_load(&command_line);
        assert_eq!(before, after);
        assert_eq!(
            saved.global["unsafe-ignore-host-seccomp"], true,
            "{saved:?}"
        );
        assert!(!saved.run.contains_key("unsafe-ignore-host-seccomp"));

        let directory = tempfile::tempdir().unwrap();
        let path = config_file(
            &directory,
            "schema: hermit-run-config/v1\nprogram: /bin/true\n",
        );
        let expanded = load(&path, &["--unsafe-ignore-host-seccomp"]);
        assert!(
            run_options(&expanded).global.unsafe_ignore_host_seccomp,
            "{expanded:?}"
        );
    }

    /// The names `--save-config` substitutes resolved values under are real
    /// options of `hermit run`.
    #[test]
    fn resolved_input_keys_name_run_options() {
        let hermit = crate::Args::command();
        let longs: Vec<String> = option_specs(run_command(&hermit))
            .into_iter()
            .map(|spec| spec.long)
            .collect();
        for key in [EPOCH_KEY, SEED_KEY, SEED_FROM_KEY] {
            assert!(longs.iter().any(|long| long == key), "{key}");
        }
    }
}
