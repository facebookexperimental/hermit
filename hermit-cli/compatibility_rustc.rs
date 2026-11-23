/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Actual rustc inputs for the compatibility producer. Cargo's artifact
//! profile is deliberately not treated as the complete compiler recipe.
//!
//! Raw Unix argv, response-file bytes and compiler identity remain in the
//! evidence bundle. Only the finite path/fingerprint rules below normalize
//! comparison inputs; every other argument is retained, in order. Unhandled
//! response-file expansion refuses a stamp rather than guessing its grammar.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use super::compatibility_graph::Graph;
use super::compatibility_graph::InputFile;
use super::compatibility_graph::Package;
use super::compatibility_graph::Unit;

pub const CAPTURE_ENV: &str = "HERMIT_COMPATIBILITY_RUSTC_CAPTURE";
pub const GATE_ENV: &str = "HERMIT_COMPATIBILITY_RUSTC_GATE";
pub const CAPTURE_SCHEMA: &str = "hermit-actual-rustc-capture/v3";
pub const NORMALIZATION: &str = "hermit-shared-rustc-semantics/v3";
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_COMMAND_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_COMMANDS: usize = 4096;

/// Byte arrays avoid silently repairing non-UTF8 Unix paths/arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub mode: u32,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compiler {
    pub path: Vec<u8>,
    pub identity: FileIdentity,
    pub sha256: String,
    pub verbose_version: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerQuery {
    pub argv: Vec<Vec<u8>>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// Directory size/timestamps change as unrelated Cargo units finish. The
/// directory inode/type and the complete relevant candidate inventory, not
/// unrelated entries, own this narrowly restricted search observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SysrootArtifact {
    pub path: Vec<u8>,
    pub identity: FileIdentity,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SysrootSearch {
    pub argument: usize,
    pub directory: Vec<u8>,
    pub canonical_directory: Vec<u8>,
    pub identity: DirectoryIdentity,
    pub candidates: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SysrootExtern {
    pub argument: usize,
    pub name: Vec<u8>,
    pub compiler: Compiler,
    pub host: String,
    pub query: CompilerQuery,
    pub directory: Vec<u8>,
    pub canonical_directory: Vec<u8>,
    pub identity: DirectoryIdentity,
    pub artifacts: Vec<SysrootArtifact>,
    pub searches: Vec<SysrootSearch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub package: Option<String>,
    pub version: Option<String>,
    pub manifest_directory: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseFile {
    pub argument: usize,
    pub path: Vec<u8>,
    pub identity: FileIdentity,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// Actual compiler input, distinct from guest stdin. Old captures lacking
/// these bytes are not promoted to this schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CompilerInput {
    File {
        argument: usize,
        path: Vec<u8>,
        canonical_path: Vec<u8>,
        identity: FileIdentity,
        bytes: Vec<u8>,
        sha256: String,
    },
    Stdin {
        argument: usize,
        bytes: Vec<u8>,
        sha256: String,
    },
}

impl CompilerInput {
    pub fn argument(&self) -> usize {
        match self {
            Self::File { argument, .. } | Self::Stdin { argument, .. } => *argument,
        }
    }
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::File { bytes, .. } | Self::Stdin { bytes, .. } => bytes,
        }
    }
    pub fn sha256(&self) -> &str {
        match self {
            Self::File { sha256, .. } | Self::Stdin { sha256, .. } => sha256,
        }
    }
    pub fn validate(&self, command: &Invocation) -> Result<(), String> {
        let argument = command
            .argv
            .get(self.argument())
            .ok_or_else(|| "captured compiler source has no actual argument".to_owned())?;
        if self.bytes().len() > MAX_COMMAND_BYTES || digest(self.bytes()) != self.sha256() {
            return Err("captured compiler source bytes/digest differ".to_owned());
        }
        match self {
            Self::Stdin { .. } if argument == b"-" => Ok(()),
            Self::File {
                path: raw,
                canonical_path,
                identity,
                bytes,
                ..
            } => {
                if !argument.ends_with(b".rs")
                    || argument.starts_with(b"-")
                    || absolute(&path(&command.cwd)?, argument)? != path(raw)?
                    || !path(canonical_path)?.is_absolute()
                    || identity.bytes != bytes.len() as u64
                {
                    return Err(
                        "captured compiler file is not its exact source argument".to_owned()
                    );
                }
                Ok(())
            }
            _ => Err("captured compiler stdin is not its exact source argument".to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub schema: String,
    pub pid: u32,
    pub compiler: Compiler,
    pub cwd: Vec<u8>,
    pub argv: Vec<Vec<u8>>,
    pub context: Context,
    pub response_files: Vec<ResponseFile>,
    pub source: Option<CompilerInput>,
    /// Mandatory even when empty. Old captures cannot acquire a sysroot
    /// resolution assertion after their compiler population has finished.
    pub sysroot_externs: Vec<SysrootExtern>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub schema: String,
    pub invocation_sha256: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub inputs_unchanged: bool,
    pub capture_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Captured {
    pub invocation: Invocation,
    pub completion: Completion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema: String,
    pub wrapper_sha256: String,
    pub target_directory: PathBuf,
    pub commands: Vec<Captured>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn full_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn path(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    if bytes.contains(&0) {
        return Err("captured Unix path contains NUL".to_owned());
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

fn bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

fn absolute(cwd: &Path, raw: &[u8]) -> Result<PathBuf, String> {
    let value = path(raw)?;
    Ok(if value.is_absolute() {
        value
    } else {
        cwd.join(value)
    })
}

fn text(value: &[u8], owner: &str) -> Result<String, String> {
    String::from_utf8(value.to_vec())
        .map_err(|_| format!("non-UTF8 {owner} cannot be classified without guessing"))
}

/// One integrity predicate for a captured completion: a recorded exit code
/// with no signal, no capture error, and unchanged inputs. Success itself is
/// owned by the typed outcome classifier below, not by capture integrity.
fn completion_well_formed(completion: &Completion) -> bool {
    completion.exit_code.is_some()
        && completion.signal.is_none()
        && completion.inputs_unchanged
        && completion.capture_error.is_none()
}

impl Bundle {
    pub fn read(input: &InputFile) -> Result<Self, String> {
        let bundle: Self = serde_json::from_slice(&input.read(MAX_CAPTURE_BYTES)?)
            .map_err(|error| format!("invalid actual rustc capture: {error}"))?;
        if bundle.schema != CAPTURE_SCHEMA
            || bundle.commands.is_empty()
            || bundle.commands.len() > MAX_COMMANDS
            || !bundle.target_directory.is_absolute()
            || !full_digest(&bundle.wrapper_sha256)
        {
            return Err("unsupported or incomplete actual rustc capture".to_owned());
        }
        for captured in &bundle.commands {
            let command = &captured.invocation;
            let completion = &captured.completion;
            let encoded = serde_json::to_vec(command).map_err(|error| error.to_string())?;
            if command.schema != CAPTURE_SCHEMA
                || completion.schema != CAPTURE_SCHEMA
                || command.pid != completion.pid
                || encoded.len() > MAX_COMMAND_BYTES
                || digest(&encoded) != completion.invocation_sha256
                || !completion_well_formed(completion)
                || !full_digest(&command.compiler.sha256)
                || command.argv.iter().any(|argument| argument.contains(&0))
                || !path(&command.compiler.path)?.is_absolute()
                || !path(&command.cwd)?.is_absolute()
            {
                return Err("rustc capture contains an incomplete/changed command".to_owned());
            }
            let version = std::str::from_utf8(&command.compiler.verbose_version)
                .map_err(|_| "captured compiler identity is non-UTF8".to_owned())?;
            if !version.lines().any(|line| {
                line.strip_prefix("commit-hash: ").is_some_and(|value| {
                    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
                })
            }) {
                return Err("captured compiler has no exact compiler commit".to_owned());
            }
            if let Some(source) = &command.source {
                source.validate(command)?;
            } else if !compiler_probe(command) {
                return Err("emitting compiler command has no actual captured source".to_owned());
            }
            for response in &command.response_files {
                if digest(&response.bytes) != response.sha256
                    || command
                        .argv
                        .get(response.argument)
                        .is_none_or(|arg| !arg.starts_with(b"@"))
                {
                    return Err(
                        "captured response file is not bound to its actual argument".to_owned()
                    );
                }
            }
            validate_sysroot_externs(command, &bundle.target_directory)?;
        }
        Ok(bundle)
    }
}

/// An artifact reference carries package/source, actual features and the
/// partial Cargo profile as additional dimensions. Complete codegen arguments
/// are bound separately for every compiled unit in the shared core closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    package: String,
    version: String,
    source: String,
    unit: Unit,
}

fn package_source(package: &Package, core_root: &Path) -> Result<String, String> {
    // A source workspace package has source=null, while the identical core
    // package in an extension workspace has a Git source ID. Its separately
    // measured full core identity owns that distinction; compare the same
    // core-relative package in either actual Cargo graph.
    if let Ok(relative) = package.manifest_path.strip_prefix(core_root) {
        return Ok(format!(
            "core-source:{}",
            relative
                .to_str()
                .ok_or_else(|| "non-UTF8 core package path".to_owned())?
        ));
    }
    if let Some(source) = &package.source {
        return Ok(source.clone());
    }
    Err(format!(
        "shared package {} is a foreign path dependency without a measured source producer",
        package.name
    ))
}

fn artifact_identity(
    graph: &Graph,
    package: &str,
    unit: &Unit,
    core_root: &Path,
) -> Result<ArtifactIdentity, String> {
    let package = graph
        .packages
        .get(package)
        .ok_or_else(|| "artifact has no actual package".to_owned())?;
    Ok(ArtifactIdentity {
        package: package.name.clone(),
        version: package.version.clone(),
        source: package_source(package, core_root)?,
        unit: unit.clone(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Argument {
    /// Includes unknown options/values unchanged; no omitted default is guessed.
    Raw {
        bytes: Vec<u8>,
    },
    Source {
        relative: Vec<u8>,
    },
    OutputDirectory,
    IncrementalDirectory,
    CargoFingerprint {
        option: String,
    },
    DependencyDirectory,
    Extern {
        name: Vec<u8>,
        artifact: Box<ArtifactIdentity>,
    },
    SysrootExtern {
        name: Vec<u8>,
        host: String,
        artifacts: Vec<SysrootArtifactSemantics>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SysrootArtifactSemantics {
    filename: Vec<u8>,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Semantics {
    package: String,
    version: String,
    source: String,
    source_file: Vec<u8>,
    source_sha256: String,
    cargo_unit: Unit,
    compiler: Vec<u8>,
    compiler_sha256: String,
    arguments: Vec<Argument>,
    /// Every explicit -C argument is retained, in order. Defaults are bound
    /// by the exact compiler/target; absent options are not filled by guesses.
    explicit_codegen: Vec<Vec<u8>>,
}

fn option<'a>(
    argument: &'a [u8],
    name: &[u8],
    next: Option<&'a Vec<u8>>,
) -> Option<(&'a [u8], usize)> {
    if argument == name {
        return next.map(|value| (value.as_slice(), 2));
    }
    argument
        .strip_prefix(name)
        .and_then(|suffix| suffix.strip_prefix(b"="))
        .map(|value| (value, 1))
}

fn split_equal(value: &[u8]) -> Option<(&[u8], &[u8])> {
    value
        .iter()
        .position(|byte| *byte == b'=')
        .map(|index| (&value[..index], &value[index + 1..]))
}

pub struct SysrootShape {
    pub arguments: Vec<usize>,
    pub host: String,
    pub searches: Vec<(usize, Vec<u8>)>,
}

/// Only the observed host proc-macro form is supported. In particular, a
/// bare extern is not a general permission to search for an arbitrary crate.
pub fn sysroot_extern_shape(command: &Invocation) -> Result<Option<SysrootShape>, String> {
    let mut arguments = Vec::new();
    let mut index = 0;
    while index < command.argv.len() {
        let arg = &command.argv[index];
        if let Some((value, count)) = option(arg, b"--extern", command.argv.get(index + 1)) {
            if split_equal(value).is_none() {
                if value != b"proc_macro" {
                    return Err("unsupported bare rustc extern name".to_owned());
                }
                arguments.push(index);
            }
            index += count;
        } else {
            if arg == b"--extern" {
                return Err("rustc extern has no value".to_owned());
            }
            index += 1;
        }
    }
    if arguments.is_empty() {
        return Ok(None);
    }
    if arguments.len() != 1
        || !command.response_files.is_empty()
        || command.argv.iter().any(|arg| {
            arg.starts_with(b"@")
                || arg == b"--target"
                || arg.starts_with(b"--target=")
                || arg == b"--sysroot"
                || arg.starts_with(b"--sysroot=")
                || arg == b"--library-path"
                || arg.starts_with(b"--library-path=")
        })
    {
        return Err(
            "implicit proc_macro has a duplicate, target/sysroot/search override or response file"
                .to_owned(),
        );
    }
    let crate_types: Vec<_> = command
        .argv
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| option(arg, b"--crate-type", command.argv.get(index + 1)))
        .map(|(value, _)| value)
        .collect();
    if crate_types != [b"proc-macro".as_slice()] {
        return Err("implicit proc_macro is not one host proc-macro compilation".to_owned());
    }
    let version = text(&command.compiler.verbose_version, "compiler identity")?;
    let hosts: Vec<_> = version
        .lines()
        .filter_map(|line| line.strip_prefix("host: "))
        .collect();
    if hosts.len() != 1
        || hosts[0].is_empty()
        || !hosts[0]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("implicit proc_macro has no unique exact compiler host".to_owned());
    }
    let mut searches = Vec::new();
    index = 0;
    while index < command.argv.len() {
        let arg = &command.argv[index];
        let library = if arg == b"-L" {
            Some((
                command
                    .argv
                    .get(index + 1)
                    .ok_or_else(|| "rustc search has no value".to_owned())?
                    .as_slice(),
                2,
            ))
        } else {
            arg.strip_prefix(b"-L")
                .filter(|value| !value.is_empty())
                .map(|value| (value, 1))
        };
        if let Some((value, count)) = library {
            let directory = value
                .strip_prefix(b"dependency=")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "implicit proc_macro has an unclassified library search".to_owned()
                })?;
            searches.push((index, directory.to_vec()));
            index += count;
        } else {
            index += 1;
        }
    }
    if searches.len() != 1 {
        return Err(
            "implicit proc_macro requires the observed single dependency search".to_owned(),
        );
    }
    Ok(Some(SysrootShape {
        arguments,
        host: hosts[0].to_owned(),
        searches,
    }))
}

/// Return the exact crate-disambiguator stem and artifact kind. Other crate
/// names (including proc_macro2) are unrelated; unknown matching forms refuse.
pub fn proc_macro_artifact_name(name: &[u8]) -> Result<Option<(Vec<u8>, bool)>, String> {
    if !(name == b"libproc_macro"
        || name.starts_with(b"libproc_macro-")
        || name.starts_with(b"libproc_macro."))
    {
        return Ok(None);
    }
    let (stem, rlib) = if let Some(stem) = name.strip_suffix(b".rlib") {
        (stem, true)
    } else if let Some(stem) = name.strip_suffix(b".rmeta") {
        (stem, false)
    } else {
        return Err("unrecognized matching proc_macro artifact form".to_owned());
    };
    let hash = stem
        .strip_prefix(b"libproc_macro-")
        .ok_or_else(|| "proc_macro artifact has no exact stem".to_owned())?;
    if hash.len() != 16
        || !hash
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err("proc_macro artifact has an unsupported disambiguator".to_owned());
    }
    Ok(Some((stem.to_vec(), rlib)))
}

fn canonical_absolute(value: &[u8]) -> Result<PathBuf, String> {
    let value = path(value)?;
    if !value.is_absolute()
        || value.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err("sysroot custody has a noncanonical absolute path".to_owned());
    }
    Ok(value)
}

fn sysroot_semantics(
    command: &Invocation,
    target: &Path,
) -> Result<BTreeMap<usize, Argument>, String> {
    if command.schema != CAPTURE_SCHEMA {
        return Err("unsupported actual rustc capture schema".to_owned());
    }
    let Some(shape) = sysroot_extern_shape(command)? else {
        if !command.sysroot_externs.is_empty() {
            return Err("sysroot capture has no corresponding actual bare extern".to_owned());
        }
        return Ok(BTreeMap::new());
    };
    if command.sysroot_externs.len() != shape.arguments.len() {
        return Err("implicit extern capture is missing or repeated".to_owned());
    }
    let mut result = BTreeMap::new();
    for captured in &command.sysroot_externs {
        let directory = canonical_absolute(&captured.directory)?;
        let canonical = canonical_absolute(&captured.canonical_directory)?;
        let mut query_output = captured.directory.clone();
        query_output.push(b'\n');
        if !shape.arguments.contains(&captured.argument)
            || captured.name != b"proc_macro"
            || captured.compiler != command.compiler
            || captured.host != shape.host
            || captured.query.argv != [b"--print".to_vec(), b"target-libdir".to_vec()]
            || captured.query.stdout != query_output
            || !captured.query.stderr.is_empty()
            || captured.query.exit_code != 0
            || !directory.ends_with(Path::new("lib/rustlib").join(&shape.host).join("lib"))
            || !canonical.ends_with(Path::new("lib/rustlib").join(&shape.host).join("lib"))
            || captured.identity.mode & 0o170000 != 0o040000
            || captured.artifacts.len() != 2
            || captured.searches.len() != shape.searches.len()
        {
            return Err("implicit extern compiler/query/argument binding differs".to_owned());
        }
        let cwd = path(&command.cwd)?;
        let mut search_indices = BTreeSet::new();
        for search in &captured.searches {
            let raw = shape
                .searches
                .iter()
                .find(|(argument, _)| *argument == search.argument)
                .ok_or_else(|| "sysroot search has no exact argument".to_owned())?;
            let resolved = canonical_absolute(&search.canonical_directory)?;
            if absolute(&cwd, &raw.1)? != path(&search.directory)?
                || !resolved.starts_with(target)
                || resolved == target
                || search.identity.mode & 0o170000 != 0o040000
                || !search.candidates.is_empty()
                || !search_indices.insert(search.argument)
            {
                return Err(
                    "implicit extern search is outside its fresh target or contains a shadow"
                        .to_owned(),
                );
            }
        }
        let mut artifacts = Vec::new();
        let mut stems = BTreeSet::new();
        let mut kinds = BTreeSet::new();
        let mut total = 0u64;
        for artifact in &captured.artifacts {
            let filename = canonical_absolute(&artifact.path)?;
            let basename = filename
                .file_name()
                .ok_or_else(|| "sysroot artifact has no filename".to_owned())?;
            let basename = bytes(Path::new(basename));
            let (stem, rlib) = proc_macro_artifact_name(&basename)?
                .ok_or_else(|| "sysroot capture contains a different crate".to_owned())?;
            total = total
                .checked_add(artifact.identity.bytes)
                .ok_or_else(|| "sysroot artifact size overflow".to_owned())?;
            if filename.parent() != Some(canonical.as_path())
                || artifact.identity.mode & 0o170000 != 0o100000
                || artifact.identity.bytes == 0
                || total > MAX_CAPTURE_BYTES as u64
                || !full_digest(&artifact.sha256)
                || !kinds.insert(rlib)
            {
                return Err(
                    "implicit extern artifact custody is incomplete or ambiguous".to_owned(),
                );
            }
            stems.insert(stem);
            artifacts.push(SysrootArtifactSemantics {
                filename: basename,
                bytes: artifact.identity.bytes,
                sha256: artifact.sha256.clone(),
            });
        }
        if stems.len() != 1 || kinds.len() != 2 {
            return Err("implicit extern rlib/rmeta stems do not match exactly".to_owned());
        }
        artifacts.sort_by(|a, b| a.filename.cmp(&b.filename));
        if result
            .insert(
                captured.argument,
                Argument::SysrootExtern {
                    name: captured.name.clone(),
                    host: captured.host.clone(),
                    artifacts,
                },
            )
            .is_some()
        {
            return Err("implicit extern capture repeats an argument".to_owned());
        }
    }
    Ok(result)
}

pub fn validate_sysroot_externs(command: &Invocation, target: &Path) -> Result<(), String> {
    sysroot_semantics(command, target).map(|_| ())
}

fn actual_features(command: &Invocation) -> Result<Vec<String>, String> {
    let mut features = BTreeSet::new();
    for (index, argument) in command.argv.iter().enumerate() {
        let Some((cfg, _)) = option(argument, b"--cfg", command.argv.get(index + 1)) else {
            continue;
        };
        if let Some(feature) = cfg.strip_prefix(b"feature=\"") {
            let feature = feature
                .strip_suffix(b"\"")
                .ok_or_else(|| "actual feature cfg has unsupported quoting".to_owned())?;
            if !features.insert(text(feature, "actual feature cfg")?) {
                return Err("rustc repeats a feature cfg".to_owned());
            }
        }
    }
    Ok(features.into_iter().collect())
}

pub fn compiler_probe(command: &Invocation) -> bool {
    // Cargo can issue queries while package context is present. Do not treat
    // an actual emitted unit as a query merely because --print also appears.
    command.argv == [b"-vV".to_vec()]
        || command.argv == [b"--version".to_vec()]
        || command.argv == [b"--version".to_vec(), b"--verbose".to_vec()]
        || (command
            .argv
            .iter()
            .any(|argument| argument == b"--print" || argument.starts_with(b"--print="))
            && !command.argv.iter().any(|argument| {
                matches!(argument.as_slice(), b"--emit" | b"--out-dir" | b"-o")
                    || argument.starts_with(b"--emit=")
                    || argument.starts_with(b"--out-dir=")
                    || (argument.starts_with(b"-o") && !argument.starts_with(b"--"))
            }))
}

fn owner<'a>(graph: &'a Graph, command: &Invocation) -> Result<&'a Package, String> {
    let directory = command
        .context
        .manifest_directory
        .as_deref()
        .ok_or_else(|| "actual rustc unit has no Cargo manifest context".to_owned())?;
    let manifest =
        super::compatibility_graph::canonical_source_file(&path(directory)?.join("Cargo.toml"))?;
    let mut matches = graph
        .packages
        .values()
        .filter(|package| package.manifest_path == manifest);
    let package = matches
        .next()
        .ok_or_else(|| "rustc manifest context is not one actual Cargo package".to_owned())?;
    if matches.next().is_some() {
        return Err("rustc manifest context is not one actual Cargo package".to_owned());
    }
    if command.context.package.as_deref() != Some(package.name.as_str())
        || command.context.version.as_deref() != Some(package.version.as_str())
    {
        return Err("rustc Cargo name/version differs from its actual manifest package".to_owned());
    }
    Ok(package)
}

struct EmittedUnitOptions {
    name: String,
    crate_types: Vec<String>,
    output: PathBuf,
    optimization: String,
}

fn emitted_unit_options(command: &Invocation) -> Result<EmittedUnitOptions, String> {
    let mut name = None;
    let mut crate_types = None;
    let mut output = None;
    let mut optimization = None;
    for (index, argument) in command.argv.iter().enumerate() {
        for (option_name, slot) in [
            (b"--crate-name".as_slice(), &mut name),
            (b"--crate-type".as_slice(), &mut crate_types),
            (b"--out-dir".as_slice(), &mut output),
        ] {
            if option(argument, option_name, command.argv.get(index + 1))
                .is_some_and(|(value, _)| slot.replace(value.to_vec()).is_some())
            {
                return Err("actual rustc repeats a Cargo unit identity option".to_owned());
            }
        }
        let codegen = if argument == b"-C" {
            command.argv.get(index + 1).map(Vec::as_slice)
        } else {
            argument
                .strip_prefix(b"-C")
                .filter(|value| !value.is_empty())
        };
        let repeated_optimization =
            match codegen.and_then(|value| value.strip_prefix(b"opt-level=")) {
                Some(value) => optimization
                    .replace(text(value, "actual optimization level")?)
                    .is_some(),
                None => false,
            };
        if repeated_optimization {
            return Err("actual rustc repeats its optimization level".to_owned());
        }
    }
    let mut crate_types = text(
        &crate_types.ok_or_else(|| "actual Cargo unit has no crate type".to_owned())?,
        "actual crate type",
    )?
    .split(',')
    .map(str::to_owned)
    .collect::<Vec<_>>();
    crate_types.sort();
    if crate_types.iter().any(String::is_empty)
        || crate_types.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err("actual Cargo unit has invalid or repeated crate types".to_owned());
    }
    Ok(EmittedUnitOptions {
        name: text(
            &name.ok_or_else(|| "actual Cargo unit has no crate name".to_owned())?,
            "actual crate name",
        )?,
        crate_types,
        output: absolute(
            &path(&command.cwd)?,
            &output.ok_or_else(|| "actual Cargo unit has no output directory".to_owned())?,
        )?,
        // rustc's default optimization level is zero, as in the measured host build.
        optimization: optimization.unwrap_or_else(|| "0".to_owned()),
    })
}

/// Resolve only a captured source against actual artifact-unit locators. An
/// unrelated unbuilt metadata target is never required to exist.
fn actual_source_unit<'a>(
    graph: &'a Graph,
    package: &Package,
    command: &Invocation,
) -> Result<Option<(usize, PathBuf, &'a Unit)>, String> {
    let source = command
        .source
        .as_ref()
        .ok_or_else(|| "actual compiler source was not captured".to_owned())?;
    source.validate(command)?;
    let CompilerInput::File { canonical_path, .. } = source else {
        return Ok(None);
    };
    let candidate = path(canonical_path)?;
    let features = actual_features(command)?;
    let mut sources = Vec::new();
    for unit in graph.units.get(&package.id).into_iter().flatten() {
        if unit.features != features {
            continue;
        }
        let locators = graph
            .source_locators
            .get(&(package.id.clone(), unit.key()?))
            .ok_or_else(|| "actual Cargo unit has no source locator proof".to_owned())?;
        let declared = super::compatibility_graph::canonical_source_file(&locators.declared)?;
        let emitted = super::compatibility_graph::canonical_source_file(&locators.emitted)?;
        if declared != emitted {
            return Err(
                "actual Cargo target source differs from its metadata declaration".to_owned(),
            );
        }
        if candidate == declared {
            sources.push((declared, unit));
        }
    }
    if sources.is_empty() {
        return Ok(None);
    }
    let options = emitted_unit_options(command)?;
    let mut found = None;
    for (declared, unit) in sources {
        let mut crate_types = unit.crate_types.clone();
        crate_types.sort();
        if unit.target.replace('-', "_") != options.name
            || crate_types != options.crate_types
            || unit
                .profile
                .get("opt_level")
                .and_then(serde_json::Value::as_str)
                != Some(options.optimization.as_str())
        {
            continue;
        }
        let output_matches = graph.artifacts.iter().any(|(filename, (owner, artifact))| {
            owner == &package.id
                && artifact == unit
                && (filename.parent() == Some(options.output.as_path())
                    // Cargo publishes an executable one level above rustc's deps output.
                    || (unit.kind == ["bin"]
                        && unit.crate_types == ["bin"]
                        && options.output.file_name() == Some(std::ffi::OsStr::new("deps"))
                        && filename.parent() == options.output.parent()
                        && graph.executables.get(&(package.id.clone(), unit.target.clone()))
                            == Some(filename)))
        });
        if !output_matches {
            continue;
        }
        if found.replace((source.argument(), declared, unit)).is_some() {
            return Err("actual rustc source has ambiguous emitted Cargo unit owners".to_owned());
        }
    }
    found
        .map(Some)
        .ok_or_else(|| "actual rustc source has no matching emitted Cargo unit identity".to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum AuxiliaryArgument {
    Raw {
        bytes: Vec<u8>,
    },
    FileSource {
        relative: Vec<u8>,
    },
    StdinSource,
    BuildOutput {
        option: String,
        relative: Vec<u8>,
    },
    AutocfgCrateName {
        ordinal: String,
        generator: Box<ArtifactIdentity>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auxiliary {
    owner: ArtifactIdentity,
    output_scope: Vec<u8>,
    compiler: Vec<u8>,
    compiler_sha256: String,
    source_sha256: String,
    arguments: Vec<AuxiliaryArgument>,
}

/// The named source of a proved auxiliary invocation, for producer output.
/// Exact bytes remain bound in the `Auxiliary` arguments for core auxiliaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AuxiliarySource {
    File { relative: Vec<u8> },
    Stdin,
}

/// The ownership/output proof every auxiliary invocation must pass, whether
/// or not its owner is in the measured core closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuxiliaryProof {
    pub owner_package: String,
    pub source: AuxiliarySource,
    pub output_option: String,
    pub output_relative: Vec<u8>,
}

/// One admitted nonzero auxiliary outcome, named for retained producer
/// output. This is a receipt record, never part of the stamp schema. Only
/// the producer script constructs it; the hermit-cli build script includes
/// this module for stamp measurement and does not name receipts.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedAuxiliaryFailure {
    pub owner_package: String,
    pub source: String,
    pub output_option: String,
    pub output_relative: String,
    pub exit_code: i32,
}

/// Every captured rustc command falls in exactly one class. The outcome rule
/// is the single exhaustive match in `require_outcome`; no call site may
/// interpret an untyped `None` as admission again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Query,
    CargoUnit,
    CoreAuxiliary {
        auxiliary: Box<Auxiliary>,
        proof: AuxiliaryProof,
    },
    ExtensionAuxiliary {
        proof: AuxiliaryProof,
    },
}

fn require_outcome(class: &Classification, completion: &Completion) -> Result<(), String> {
    if !completion_well_formed(completion) {
        return Err("compiler completion is incomplete/changed".to_owned());
    }
    match class {
        Classification::Query if completion.exit_code != Some(0) => {
            Err("compiler query did not complete successfully".to_owned())
        }
        Classification::CargoUnit if completion.exit_code != Some(0) => {
            Err("actual Cargo compiler command did not succeed".to_owned())
        }
        Classification::Query
        | Classification::CargoUnit
        | Classification::CoreAuxiliary { .. }
        | Classification::ExtensionAuxiliary { .. } => Ok(()),
    }
}

// autocfg 1.5.1 src/lib.rs:297–303 and620–641: its random namespace
// prevents temporary probe collisions; the ordinal and every other input
// remain exact. The actual compiled generator source must have these bytes.
const AUTOCFG_SOURCE: &str = "772630d0e09d06f343fd72284c8330eacde87d2a4ee22f70af62a0e2119fbf05";

fn autocfg_ordinal(name: &[u8]) -> Option<&[u8]> {
    let rest = name.strip_prefix(b"autocfg_")?;
    let (nonce, ordinal) = rest.split_at_checked(16)?;
    let ordinal = ordinal.strip_prefix(b"_")?;
    (nonce
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        && !ordinal.is_empty()
        && ordinal.iter().all(u8::is_ascii_digit)
        && (ordinal == b"0" || !ordinal.starts_with(b"0")))
    .then_some(ordinal)
}

fn autocfg_generator(
    graph: &Graph,
    bundle: &Bundle,
    package: &Package,
    core_root: &Path,
) -> Result<ArtifactIdentity, String> {
    let closure = graph.closure(&package.id)?;
    let generators: Vec<_> = graph
        .packages
        .values()
        .filter(|candidate| {
            closure.contains(&candidate.id)
                && candidate.name == "autocfg"
                && candidate.version == "1.5.1"
                && candidate.source.as_deref()
                    == Some("registry+https://github.com/rust-lang/crates.io-index")
        })
        .collect();
    if generators.len() != 1 {
        return Err("auxiliary random crate name has no exact measured generator".to_owned());
    }
    let generator = generators[0];
    let units: Vec<_> = graph
        .units
        .get(&generator.id)
        .into_iter()
        .flatten()
        .filter(|unit| unit.kind == ["lib"] && unit.target == "autocfg")
        .collect();
    if units.len() != 1 {
        return Err("autocfg generator has no unique actual library unit".to_owned());
    }
    let mut matching = 0;
    for captured in &bundle.commands {
        let command = &captured.invocation;
        if compiler_probe(command) {
            continue;
        }
        if owner(graph, command)?.id != generator.id {
            continue;
        }
        if actual_source_unit(graph, generator, command)?
            .is_some_and(|(_, _, unit)| unit == units[0])
        {
            if command.source.as_ref().map(CompilerInput::sha256) != Some(AUTOCFG_SOURCE) {
                return Err(
                    "autocfg nonce policy does not recognize the actual generator source"
                        .to_owned(),
                );
            }
            matching += 1;
        }
    }
    if matching != 1 {
        return Err("autocfg nonce policy requires one real generator compiler capture".to_owned());
    }
    artifact_identity(graph, &generator.id, units[0], core_root)
}

/// Assign one captured command its one class, by this one function.
/// Callers may classify the same command again for a different check; the
/// class is a function of its inputs, so the results agree. `reference` is a complete
/// actual cold population proving the autocfg generator, not a replacement
/// for this invocation's newly captured input or outcome.
pub fn classify(
    graph: &Graph,
    bundle: &Bundle,
    reference: &Bundle,
    command: &Invocation,
) -> Result<Classification, String> {
    validate_sysroot_externs(command, &bundle.target_directory)?;
    if compiler_probe(command) {
        return Ok(Classification::Query);
    }
    let package = owner(graph, command)?;
    if let Some((_, _, unit)) = actual_source_unit(graph, package, command)? {
        if !command.sysroot_externs.is_empty()
            && (unit.kind != ["proc-macro"] || unit.crate_types != ["proc-macro"])
        {
            return Err("implicit proc_macro is not an actual Cargo proc-macro unit".to_owned());
        }
        return Ok(Classification::CargoUnit);
    }
    let (proof, auxiliary) = prove_auxiliary(graph, bundle, reference, command, package)?;
    Ok(match auxiliary {
        Some(auxiliary) => Classification::CoreAuxiliary {
            auxiliary: Box::new(auxiliary),
            proof,
        },
        None => Classification::ExtensionAuxiliary { proof },
    })
}

/// Legacy mapping over `classify` for callers that only need the counted
/// core-closure auxiliary. Outcome admission must use `classify`, never
/// interpret this `None` (query, Cargo unit, and extension auxiliary all
/// map to it here).
pub fn auxiliary_for_invocation(
    graph: &Graph,
    bundle: &Bundle,
    reference: &Bundle,
    command: &Invocation,
) -> Result<Option<Auxiliary>, String> {
    Ok(match classify(graph, bundle, reference, command)? {
        Classification::CoreAuxiliary { auxiliary, .. } => Some(*auxiliary),
        Classification::Query
        | Classification::CargoUnit
        | Classification::ExtensionAuxiliary { .. } => None,
    })
}

fn prove_auxiliary(
    graph: &Graph,
    bundle: &Bundle,
    reference: &Bundle,
    command: &Invocation,
    package: &Package,
) -> Result<(AuxiliaryProof, Option<Auxiliary>), String> {
    if !command.sysroot_externs.is_empty() {
        return Err(
            "implicit sysroot externs in auxiliary compiler calls are unsupported".to_owned(),
        );
    }
    if !command.response_files.is_empty() || command.argv.iter().any(|arg| arg.starts_with(b"@")) {
        return Err(
            "auxiliary response expansion is unsupported; compatibility is unavailable".to_owned(),
        );
    }
    let core = graph.named("hermit-run")?;
    let core_root = core
        .manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "core manifest has no source root".to_owned())?;
    let owners: Vec<_> = graph
        .units
        .get(&package.id)
        .into_iter()
        .flatten()
        .filter(|unit| unit.kind == ["custom-build"] && unit.crate_types == ["bin"])
        .collect();
    if owners.len() != 1 {
        return Err("unmatched compiler has no unique actual build-script owner".to_owned());
    }
    let source = command
        .source
        .as_ref()
        .expect("actual_source_unit checked captured source");
    let cwd = path(&command.cwd)?;
    if cwd.canonicalize().map_err(|error| error.to_string())?
        != package.manifest_path.parent().expect("manifest parent")
    {
        return Err("auxiliary compiler cwd is not its actual package".to_owned());
    }
    let mut outputs = Vec::new();
    for (index, argument) in command.argv.iter().enumerate() {
        for name in [b"--out-dir".as_slice(), b"-o".as_slice()] {
            if let Some((value, count)) = option(argument, name, command.argv.get(index + 1)) {
                outputs.push((index, count, name, absolute(&cwd, value)?));
            }
        }
    }
    if outputs.len() != 1 {
        return Err("auxiliary compiler has no unique bounded output".to_owned());
    }
    let (output_index, output_count, output_option, output) = &outputs[0];
    let roots: Vec<_> = graph
        .build_outputs
        .get(&package.id)
        .into_iter()
        .flatten()
        .filter(|root| root.starts_with(&bundle.target_directory) && output.starts_with(root))
        .collect();
    if roots.len() != 1 {
        return Err(
            "auxiliary compiler output is not its actual Cargo build-script out_dir".to_owned(),
        );
    }
    let root = roots[0];
    if [root.as_path(), output.as_path()].iter().any(|path| {
        path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    }) {
        return Err("auxiliary output has a noncanonical relative component".to_owned());
    }
    let scope = root
        .strip_prefix(&bundle.target_directory)
        .map_err(|error| error.to_string())?;
    let components: Vec<_> = scope.components().collect();
    let build = components
        .iter()
        .position(|component| component.as_os_str() == "build")
        .ok_or_else(|| "auxiliary build output has no Cargo build scope".to_owned())?;
    if !(1..=2).contains(&build)
        || components.len() != build + 3
        || components
            .last()
            .is_none_or(|component| component.as_os_str() != "out")
    {
        return Err("auxiliary build output has an unsupported Cargo layout".to_owned());
    }
    let mut output_scope = PathBuf::new();
    for component in &components[..build] {
        output_scope.push(component.as_os_str());
    }
    let proof = AuxiliaryProof {
        owner_package: package.name.clone(),
        source: match source {
            CompilerInput::File { canonical_path, .. } => AuxiliarySource::File {
                relative: bytes(
                    path(canonical_path)?
                        .strip_prefix(package.manifest_path.parent().expect("manifest parent"))
                        .map_err(|_| "auxiliary source is outside its actual package".to_owned())?,
                ),
            },
            CompilerInput::Stdin { .. } => AuxiliarySource::Stdin,
        },
        output_option: text(output_option, "auxiliary output option")?,
        output_relative: bytes(
            output
                .strip_prefix(root)
                .map_err(|error| error.to_string())?,
        ),
    };
    let mut arguments = Vec::new();
    let mut index = 0;
    while index < command.argv.len() {
        let argument = &command.argv[index];
        if index == source.argument() {
            arguments.push(match source {
                CompilerInput::File { canonical_path, .. } => {
                    let source = path(canonical_path)?;
                    let relative = source
                        .strip_prefix(package.manifest_path.parent().expect("manifest parent"))
                        .map_err(|_| "auxiliary source is outside its actual package".to_owned())?;
                    AuxiliaryArgument::FileSource {
                        relative: bytes(relative),
                    }
                }
                CompilerInput::Stdin { .. } => AuxiliaryArgument::StdinSource,
            });
            index += 1;
            continue;
        }
        if index == *output_index {
            arguments.push(AuxiliaryArgument::BuildOutput {
                option: text(output_option, "auxiliary output option")?,
                relative: bytes(
                    output
                        .strip_prefix(root)
                        .map_err(|error| error.to_string())?,
                ),
            });
            index += *output_count;
            continue;
        }
        if let Some((name, count)) = option(argument, b"--crate-name", command.argv.get(index + 1))
        {
            if let Some(ordinal) = autocfg_ordinal(name) {
                let lib = command
                    .argv
                    .iter()
                    .filter(|arg| arg.as_slice() == b"--crate-type=lib")
                    .count()
                    == 1;
                let ir = command
                    .argv
                    .iter()
                    .filter(|arg| arg.as_slice() == b"--emit=llvm-ir")
                    .count()
                    == 1;
                if !matches!(source, CompilerInput::Stdin { .. })
                    || !lib
                    || !ir
                    || *output_option != b"--out-dir"
                {
                    return Err(
                        "autocfg nonce is outside the exact measured stdin/lib/llvm-ir form"
                            .to_owned(),
                    );
                }
                arguments.push(AuxiliaryArgument::AutocfgCrateName {
                    ordinal: text(ordinal, "autocfg ordinal")?,
                    generator: Box::new(autocfg_generator(graph, reference, package, core_root)?),
                });
                index += count;
                continue;
            }
            if name.starts_with(b"autocfg_") {
                return Err("unrecognized autocfg generated name".to_owned());
            }
        }
        arguments.push(AuxiliaryArgument::Raw {
            bytes: argument.clone(),
        });
        index += 1;
    }
    if !graph.closure(&core.id)?.contains(&package.id) {
        // The extension's additional build-script units still require exact
        // ownership/input/output proof, but are not the shared core cohort.
        return Ok((proof, None));
    }
    Ok((
        proof,
        Some(Auxiliary {
            owner: artifact_identity(graph, &package.id, owners[0], core_root)?,
            output_scope: bytes(&output_scope),
            compiler: command.compiler.verbose_version.clone(),
            compiler_sha256: command.compiler.sha256.clone(),
            source_sha256: source.sha256().to_owned(),
            arguments,
        }),
    ))
}

pub fn auxiliary(graph: &Graph, bundle: &Bundle) -> Result<BTreeMap<String, usize>, String> {
    auxiliary_with_reference(graph, bundle, bundle)
}

pub fn auxiliary_with_reference(
    graph: &Graph,
    bundle: &Bundle,
    reference: &Bundle,
) -> Result<BTreeMap<String, usize>, String> {
    let mut result = BTreeMap::new();
    for captured in &bundle.commands {
        // Every command gets its one class here; a negative auxiliary
        // probe is counted with its exact outcome, never reclassified as
        // success, and failed queries/Cargo units refuse here so the
        // producer's final-population path cannot omit the outcome rule.
        let class = classify(graph, bundle, reference, &captured.invocation)?;
        require_outcome(&class, &captured.completion)?;
        if let Classification::CoreAuxiliary { auxiliary, .. } = class {
            let key = serde_json::to_string(&(
                auxiliary,
                captured.completion.exit_code,
                captured.completion.signal,
            ))
            .map_err(|error| error.to_string())?;
            *result.entry(key).or_insert(0) += 1;
        }
    }
    Ok(result)
}

/// Every admitted nonzero auxiliary outcome in this bundle, named for the
/// producer's retained output. Classification refusals propagate; nothing
/// is listed for successful commands. Only the producer script calls it;
/// the hermit-cli build script includes this module for stamp measurement.
#[allow(dead_code)]
pub fn admitted_auxiliary_failures(
    graph: &Graph,
    bundle: &Bundle,
    reference: &Bundle,
) -> Result<Vec<AdmittedAuxiliaryFailure>, String> {
    let mut failures = Vec::new();
    for captured in &bundle.commands {
        let class = classify(graph, bundle, reference, &captured.invocation)?;
        require_outcome(&class, &captured.completion)?;
        let proof = match &class {
            Classification::CoreAuxiliary { proof, .. }
            | Classification::ExtensionAuxiliary { proof } => proof,
            Classification::Query | Classification::CargoUnit => continue,
        };
        if captured.completion.exit_code == Some(0) {
            continue;
        }
        failures.push(AdmittedAuxiliaryFailure {
            owner_package: proof.owner_package.clone(),
            source: match &proof.source {
                AuxiliarySource::File { relative } => {
                    String::from_utf8_lossy(relative).into_owned()
                }
                AuxiliarySource::Stdin => "<stdin>".to_owned(),
            },
            output_option: proof.output_option.clone(),
            output_relative: String::from_utf8_lossy(&proof.output_relative).into_owned(),
            exit_code: captured.completion.exit_code.expect("well-formed outcome"),
        });
    }
    // Receipt order is by the named failure itself, never bundle order.
    failures.sort_by(|left, right| {
        (
            &left.owner_package,
            &left.source,
            &left.output_option,
            &left.output_relative,
            left.exit_code,
        )
            .cmp(&(
                &right.owner_package,
                &right.source,
                &right.output_option,
                &right.output_relative,
                right.exit_code,
            ))
    });
    Ok(failures)
}

fn queries(bundle: &Bundle) -> Result<BTreeMap<String, usize>, String> {
    let mut result = BTreeMap::new();
    for captured in &bundle.commands {
        let command = &captured.invocation;
        if compiler_probe(command) {
            require_outcome(&Classification::Query, &captured.completion)?;
            let key = serde_json::to_string(&(
                &command.argv,
                &command.context,
                &command.source,
                &command.compiler.verbose_version,
                &command.compiler.sha256,
                captured.completion.exit_code,
                captured.completion.signal,
            ))
            .map_err(|error| error.to_string())?;
            *result.entry(key).or_insert(0) += 1;
        }
    }
    Ok(result)
}

/// Only the core CLI entry executable is absent from an extension's shared
/// library compilation. In particular, custom-build units also have a bin
/// crate type, and remain required along with every other core/dependency unit.
fn core_entry_binary(core: &Package, package: &Package, unit: &Unit) -> bool {
    if package.id != core.id
        || unit.target != "hermit"
        || unit.kind.len() != 1
        || unit.kind[0] != "bin"
        || unit.crate_types.len() != 1
        || unit.crate_types[0] != "bin"
    {
        return false;
    }
    let Some(crate_directory) = core.manifest_path.parent() else {
        return false;
    };
    let mut targets = package.targets.iter().filter(|target| {
        target.name == unit.target
            && target.kind == unit.kind
            && target.crate_types == unit.crate_types
    });
    targets.next().is_some_and(|target| {
        super::compatibility_graph::canonical_source_file(&target.src_path)
            .is_ok_and(|source| source == crate_directory.join("src/bin/hermit/main.rs"))
    }) && targets.next().is_none()
}

pub fn for_invocation(
    graph: &Graph,
    bundle: &Bundle,
    command: &Invocation,
) -> Result<Option<Semantics>, String> {
    for_invocation_with_reference(graph, bundle, bundle, command)
}

pub fn for_invocation_with_reference(
    graph: &Graph,
    bundle: &Bundle,
    reference: &Bundle,
    command: &Invocation,
) -> Result<Option<Semantics>, String> {
    let sysroot = sysroot_semantics(command, &bundle.target_directory)?;
    if compiler_probe(command) {
        return Ok(None);
    }
    let package = owner(graph, command)?;
    let core = graph.named("hermit-run")?;
    let closure = graph.closure(&core.id)?;
    // The source/API producer supports only a full measured core checkout.
    let core_root = core
        .manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "core manifest has no source root".to_owned())?;
    if !command.response_files.is_empty() || command.argv.iter().any(|arg| arg.starts_with(b"@")) {
        return Err("response-file bytes were captured, but verified rustc response expansion is not yet supported; compatibility is unavailable".to_owned());
    }
    let cwd = path(&command.cwd)?;
    let Some((source_index, source_path, actual_unit)) =
        actual_source_unit(graph, package, command)?
    else {
        auxiliary_for_invocation(graph, bundle, reference, command)?;
        return Ok(None);
    };
    if !sysroot.is_empty()
        && (actual_unit.kind != ["proc-macro"] || actual_unit.crate_types != ["proc-macro"])
    {
        return Err("implicit proc_macro is not an actual Cargo proc-macro unit".to_owned());
    }
    if !closure.contains(&package.id) || core_entry_binary(core, package, actual_unit) {
        return Ok(None);
    }
    let relative = source_path
        .strip_prefix(package.manifest_path.parent().expect("manifest has parent"))
        .map_err(|_| "rustc source is outside its actual package".to_owned())?;
    let mut output = None;
    for (index, argument) in command.argv.iter().enumerate() {
        let Some((value, _)) = option(argument, b"--out-dir", command.argv.get(index + 1)) else {
            continue;
        };
        if output.replace(absolute(&cwd, value)?).is_some() {
            return Err("rustc repeats its output directory".to_owned());
        }
    }
    let output = output.ok_or_else(|| "actual rustc unit has no output directory".to_owned())?;
    if !output.starts_with(&bundle.target_directory) {
        return Err("actual rustc output is outside the producer's fresh target".to_owned());
    }
    let artifact_directories: BTreeSet<_> = graph
        .artifacts
        .keys()
        .filter_map(|path| path.parent())
        .collect();
    let mut arguments = Vec::new();
    let mut explicit_codegen = Vec::new();
    let mut index = 0;
    while index < command.argv.len() {
        let argument = &command.argv[index];
        if index == source_index {
            arguments.push(Argument::Source {
                relative: bytes(relative),
            });
            index += 1;
            continue;
        }
        if let Some((_, count)) = option(argument, b"--out-dir", command.argv.get(index + 1)) {
            arguments.push(Argument::OutputDirectory);
            index += count;
            continue;
        }
        let codegen = if argument == b"-C" {
            command.argv.get(index + 1).map(|arg| (arg.as_slice(), 2))
        } else {
            argument
                .strip_prefix(b"-C")
                .filter(|value| !value.is_empty())
                .map(|value| (value, 1))
        };
        if let Some((value, count)) = codegen {
            if let Some(path) = value.strip_prefix(b"incremental=") {
                if !absolute(&cwd, path)?.starts_with(&bundle.target_directory) {
                    return Err("rustc incremental output is outside the fresh target".to_owned());
                }
                arguments.push(Argument::IncrementalDirectory);
            } else if let Some((name, setting)) = split_equal(value) {
                if matches!(name, b"metadata" | b"extra-filename") {
                    let hash = setting.strip_prefix(b"-").unwrap_or(setting);
                    if hash.is_empty() || !hash.iter().all(u8::is_ascii_hexdigit) {
                        return Err("non-Cargo rustc fingerprint cannot be normalized".to_owned());
                    }
                    arguments.push(Argument::CargoFingerprint {
                        option: text(name, "Cargo fingerprint")?,
                    });
                } else {
                    explicit_codegen.push(value.to_vec());
                    arguments.extend(
                        command.argv[index..index + count]
                            .iter()
                            .cloned()
                            .map(|bytes| Argument::Raw { bytes }),
                    );
                }
            } else {
                explicit_codegen.push(value.to_vec());
                arguments.extend(
                    command.argv[index..index + count]
                        .iter()
                        .cloned()
                        .map(|bytes| Argument::Raw { bytes }),
                );
            }
            index += count;
            continue;
        }
        if let Some((value, count)) = option(argument, b"--extern", command.argv.get(index + 1)) {
            if split_equal(value).is_none() {
                arguments.push(
                    sysroot
                        .get(&index)
                        .ok_or_else(|| {
                            "implicit extern has no exact captured resolution".to_owned()
                        })?
                        .clone(),
                );
                index += count;
                continue;
            }
            let (name, filename) = split_equal(value).ok_or_else(|| {
                "unsupported rustc extern without a bound artifact path".to_owned()
            })?;
            let filename = absolute(&cwd, filename)?;
            let (owner, unit) = graph.artifacts.get(&filename).ok_or_else(|| {
                "rustc extern is not in the actual Cargo artifact inventory".to_owned()
            })?;
            arguments.push(Argument::Extern {
                name: name.to_vec(),
                artifact: Box::new(artifact_identity(graph, owner, unit, core_root)?),
            });
            index += count;
            continue;
        }
        let library = if argument == b"-L" {
            command.argv.get(index + 1).map(|arg| (arg.as_slice(), 2))
        } else {
            argument
                .strip_prefix(b"-L")
                .filter(|value| !value.is_empty())
                .map(|value| (value, 1))
        };
        if let Some((path, count)) = library
            .and_then(|(value, count)| value.strip_prefix(b"dependency=").map(|path| (path, count)))
        {
            let path = absolute(&cwd, path)?;
            if path.starts_with(&bundle.target_directory)
                && artifact_directories.contains(path.as_path())
            {
                arguments.push(Argument::DependencyDirectory);
                index += count;
                continue;
            }
        }
        arguments.push(Argument::Raw {
            bytes: argument.clone(),
        });
        index += 1;
    }
    Ok(Some(Semantics {
        package: package.name.clone(),
        version: package.version.clone(),
        source: package_source(package, core_root)?,
        source_file: bytes(relative),
        source_sha256: command
            .source
            .as_ref()
            .expect("source checked")
            .sha256()
            .to_owned(),
        cargo_unit: actual_unit.clone(),
        compiler: command.compiler.verbose_version.clone(),
        compiler_sha256: command.compiler.sha256.clone(),
        arguments,
        explicit_codegen,
    }))
}

/// Complete actual shared-unit population, not metadata's feature union. The
/// extension's own packages are allowed outside this measured core closure.
pub fn shared(graph: &Graph, bundle: &Bundle) -> Result<BTreeMap<String, Semantics>, String> {
    let mut result = BTreeMap::new();
    let mut units = BTreeSet::new();
    for captured in &bundle.commands {
        let class = classify(graph, bundle, bundle, &captured.invocation)?;
        require_outcome(&class, &captured.completion)?;
        // Keep the per-command for_invocation call for every Cargo unit,
        // including the core entry binary and extension units: it retains
        // the response-file and implicit-proc_macro refusals for units
        // that contribute no shared semantics.
        if let Some(semantics) = for_invocation(graph, bundle, &captured.invocation)? {
            let key = serde_json::to_string(&semantics).map_err(|error| error.to_string())?;
            units.insert((
                semantics.package.clone(),
                semantics.version.clone(),
                semantics.source.clone(),
                semantics.cargo_unit.key()?,
            ));
            if result.insert(key, semantics).is_some() {
                return Err(
                    "shared rustc unit was compiled more than once without a unique identity"
                        .to_owned(),
                );
            }
        }
    }
    let core = graph.named("hermit-run")?;
    let core_root = core
        .manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "core manifest has no source root".to_owned())?;
    for id in graph.closure(&core.id)? {
        let package = &graph.packages[&id];
        for unit in graph.units.get(&id).into_iter().flatten() {
            if core_entry_binary(core, package, unit) {
                continue;
            }
            if !units.contains(&(
                package.name.clone(),
                package.version.clone(),
                package_source(package, core_root)?,
                unit.key()?,
            )) {
                return Err(format!(
                    "shared Cargo unit {}:{} has an artifact but no actual rustc command; cached population is not accepted",
                    package.name, unit.target
                ));
            }
        }
    }
    if result.is_empty() {
        return Err("no actual shared rustc units were captured".to_owned());
    }
    Ok(result)
}

pub fn cohort(
    first_graph: &Graph,
    first: &Bundle,
    second_graph: &Graph,
    second: &Bundle,
) -> Result<serde_json::Value, String> {
    if first.wrapper_sha256 != second.wrapper_sha256 {
        return Err("the two actual builds used different compiler capture executables".to_owned());
    }
    let compiler = first
        .commands
        .first()
        .ok_or_else(|| "the first compiler population is empty".to_owned())?;
    require_compiler(
        first,
        &compiler.invocation.compiler.verbose_version,
        &compiler.invocation.compiler.sha256,
    )?;
    require_compiler(
        second,
        &compiler.invocation.compiler.verbose_version,
        &compiler.invocation.compiler.sha256,
    )?;
    let expected = shared(first_graph, first)?;
    let observed = shared(second_graph, second)?;
    if expected != observed {
        return Err(
            "the two actual builds have different shared rustc semantics/populations".to_owned(),
        );
    }
    let auxiliary_first = auxiliary(first_graph, first)?;
    let auxiliary_second = auxiliary(second_graph, second)?;
    if auxiliary_first != auxiliary_second || queries(first)? != queries(second)? {
        return Err(
            "the two actual builds have different auxiliary compiler/query inputs or outcomes"
                .to_owned(),
        );
    }
    let bytes = serde_json::to_vec(&expected).map_err(|error| error.to_string())?;
    Ok(
        serde_json::json!({"schema":CAPTURE_SCHEMA,"normalization":NORMALIZATION,
        "shared_units":expected.len(),"shared_sha256":digest(&bytes),
        "auxiliary_commands":auxiliary_first.values().sum::<usize>(),
        "auxiliary_sha256":digest(&serde_json::to_vec(&auxiliary_first).map_err(|error| error.to_string())?)}),
    )
}

/// The stamp's presentation compiler identity must be the same actual
/// frontend measured before/after every command, not a separate tool on PATH.
pub fn require_compiler(bundle: &Bundle, version: &[u8], sha256: &str) -> Result<(), String> {
    if !full_digest(sha256)
        || bundle.commands.iter().any(|captured| {
            captured.invocation.compiler.verbose_version != version
                || captured.invocation.compiler.sha256 != sha256
        })
    {
        return Err(
            "stamp compiler identity differs from the actual captured compiler frontend".to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysroot_proc_macro_semantics_require_complete_capture_and_exact_artifacts() {
        use std::os::unix::fs::MetadataExt;
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!(
            "hermit-sysroot-semantics-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let _fixture = Fixture(root.clone());
        let target = root.join("target");
        let deps = target.join("debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        let definitions = [
            ("core", "hermit-run", "hermit", "lib", "hermit-cli"),
            ("helper", "helper", "helper", "lib", "helper"),
            (
                "macro",
                "fixture-macro",
                "fixture_macro",
                "proc-macro",
                "macro",
            ),
        ];
        for (_, name, _, _, directory) in &definitions {
            std::fs::create_dir_all(root.join(directory).join("src")).unwrap();
            std::fs::write(
                root.join(directory).join("Cargo.toml"),
                format!("[package]\nname={name:?}\nversion=\"0.5.4\"\n"),
            )
            .unwrap();
            std::fs::write(
                root.join(directory).join("src/lib.rs"),
                b"// real fixture source\n",
            )
            .unwrap();
        }
        let manifest = root.join("Cargo.toml");
        let lock = root.join("Cargo.lock");
        std::fs::write(&manifest, b"[workspace]\n").unwrap();
        std::fs::write(&lock, b"version = 4\n").unwrap();
        let metadata = serde_json::json!({"version":1,"workspace_root":root,
            "workspace_members":definitions.iter().map(|(id,_,_,_,_)| id).collect::<Vec<_>>(),
            "packages":definitions.iter().map(|(id,name,crate_name,kind,directory)| serde_json::json!({
                "id":id,"name":name,"version":"0.5.4","source":null,
                "manifest_path":root.join(directory).join("Cargo.toml"),
                "targets":[{"name":crate_name,"kind":[kind],"crate_types":[kind],"src_path":root.join(directory).join("src/lib.rs")}]
            })).collect::<Vec<_>>(),
            "resolve":{"nodes":[{"id":"core","deps":[{"pkg":"macro","dep_kinds":[{"kind":null}]}]},
                {"id":"macro","deps":[{"pkg":"helper","dep_kinds":[{"kind":null}]}]},
                {"id":"helper","deps":[]}]}});
        let metadata_path = root.join("metadata.json");
        std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let mut artifact_bytes = Vec::new();
        for (id, _, name, kind, directory) in &definitions {
            let extension = if *kind == "proc-macro" { "so" } else { "rlib" };
            let artifact = serde_json::json!({"reason":"compiler-artifact","package_id":id,
                "target":{"name":name,"kind":[kind],"crate_types":[kind],"src_path":root.join(directory).join("src/lib.rs")},
                "features":[],"profile":{"opt_level":"0","test":false},"filenames":[deps.join(format!("lib{name}.{extension}"))],"executable":null});
            artifact_bytes.extend(serde_json::to_vec(&artifact).unwrap());
            artifact_bytes.push(b'\n');
        }
        artifact_bytes.extend(b"{\"reason\":\"build-finished\",\"success\":true}\n");
        let artifacts_path = root.join("artifacts.jsonl");
        std::fs::write(&artifacts_path, artifact_bytes).unwrap();
        let input_file = |path: &Path| InputFile {
            path: path.to_owned(),
            sha256: digest(&std::fs::read(path).unwrap()),
        };
        let artifacts = input_file(&artifacts_path);
        let input = super::super::compatibility_graph::BuildInput {
            schema: super::super::compatibility_graph::INPUT_SCHEMA.to_owned(),
            metadata: input_file(&metadata_path),
            first_artifacts: artifacts.clone(),
            artifacts: artifacts.clone(),
            rustc_first: artifacts.clone(),
            rustc_second: artifacts.clone(),
            source_snapshot: artifacts,
            manifest: input_file(&manifest),
            lock: input_file(&lock),
            package: "hermit-run".to_owned(),
            binary: "hermit".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            profile: "dev".to_owned(),
        };
        let graph = Graph::read(&input).unwrap();
        let file_identity = |path: &Path| {
            let meta = std::fs::metadata(path).unwrap();
            FileIdentity {
                device: meta.dev(),
                inode: meta.ino(),
                bytes: meta.len(),
                mode: meta.mode(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                changed: (meta.ctime(), meta.ctime_nsec()),
            }
        };
        let compiler_path = root.join("fixture-rustc");
        std::fs::write(&compiler_path, b"fixture compiler, not executed").unwrap();
        let compiler = Compiler {
            path: bytes(&compiler_path),
            identity: file_identity(&compiler_path),
            sha256: digest(&std::fs::read(&compiler_path).unwrap()),
            verbose_version: format!(
                "rustc fixture\ncommit-hash: {}\nhost: x86_64-unknown-linux-gnu\n",
                "b".repeat(40)
            )
            .into_bytes(),
        };
        let source = root.join("macro/src/lib.rs");
        let mut command = Invocation {
            schema: CAPTURE_SCHEMA.to_owned(),
            pid: 1,
            compiler,
            cwd: bytes(&root),
            argv: vec![
                b"--crate-name".to_vec(),
                b"fixture_macro".to_vec(),
                bytes(&source),
                b"--crate-type".to_vec(),
                b"proc-macro".to_vec(),
                b"--emit=link".to_vec(),
                b"--out-dir".to_vec(),
                bytes(&deps),
                b"-L".to_vec(),
                [b"dependency=".as_slice(), &bytes(&deps)].concat(),
                b"--extern".to_vec(),
                b"proc_macro".to_vec(),
                b"--extern".to_vec(),
                [b"helper=".as_slice(), &bytes(&deps.join("libhelper.rlib"))].concat(),
            ],
            context: Context {
                package: Some("fixture-macro".to_owned()),
                version: Some("0.5.4".to_owned()),
                manifest_directory: Some(bytes(&root.join("macro"))),
            },
            response_files: vec![],
            source: Some(CompilerInput::File {
                argument: 2,
                path: bytes(&source),
                canonical_path: bytes(&source),
                identity: file_identity(&source),
                bytes: std::fs::read(&source).unwrap(),
                sha256: digest(&std::fs::read(&source).unwrap()),
            }),
            sysroot_externs: vec![],
        };
        let libdir = root.join("toolchain/lib/rustlib/x86_64-unknown-linux-gnu/lib");
        std::fs::create_dir_all(&libdir).unwrap();
        let mut sysroot_artifacts = Vec::new();
        for kind in ["rlib", "rmeta"] {
            let path = libdir.join(format!("libproc_macro-0123456789abcdef.{kind}"));
            std::fs::write(&path, format!("real fixture {kind} bytes")).unwrap();
            sysroot_artifacts.push(SysrootArtifact {
                path: bytes(&path),
                identity: file_identity(&path),
                sha256: digest(&std::fs::read(&path).unwrap()),
            });
        }
        let directory_identity = |path: &Path| {
            let meta = std::fs::metadata(path).unwrap();
            DirectoryIdentity {
                device: meta.dev(),
                inode: meta.ino(),
                mode: meta.mode(),
            }
        };
        command.sysroot_externs.push(SysrootExtern {
            argument: 10,
            name: b"proc_macro".to_vec(),
            compiler: command.compiler.clone(),
            host: "x86_64-unknown-linux-gnu".to_owned(),
            query: CompilerQuery {
                argv: vec![b"--print".to_vec(), b"target-libdir".to_vec()],
                stdout: [bytes(&libdir), vec![b'\n']].concat(),
                stderr: vec![],
                exit_code: 0,
            },
            directory: bytes(&libdir),
            canonical_directory: bytes(&libdir),
            identity: directory_identity(&libdir),
            artifacts: sysroot_artifacts,
            searches: vec![SysrootSearch {
                argument: 8,
                directory: bytes(&deps),
                canonical_directory: bytes(&deps),
                identity: directory_identity(&deps),
                candidates: vec![],
            }],
        });
        let bundle = Bundle {
            schema: CAPTURE_SCHEMA.to_owned(),
            wrapper_sha256: "c".repeat(64),
            target_directory: target,
            commands: vec![],
        };
        let actual = for_invocation(&graph, &bundle, &command).unwrap().unwrap();
        assert_eq!(
            actual
                .arguments
                .iter()
                .filter(|arg| matches!(arg, Argument::SysrootExtern { .. }))
                .count(),
            1
        );
        assert_eq!(
            actual
                .arguments
                .iter()
                .filter(|arg| matches!(arg,Argument::Extern {name,..} if name==b"helper"))
                .count(),
            1,
            "ordinary name=path Cargo artifact semantics must stay present"
        );
        let refusal =
            |changed: &Invocation| assert!(for_invocation(&graph, &bundle, changed).is_err());
        for mutation in 0..14 {
            let mut changed = command.clone();
            match mutation {
                0 => changed.schema = "hermit-actual-rustc-capture/v2".to_owned(),
                1 => changed.sysroot_externs.clear(),
                2 => changed
                    .sysroot_externs
                    .push(changed.sysroot_externs[0].clone()),
                3 => changed.sysroot_externs[0].argument = 12,
                4 => changed.sysroot_externs[0].name = b"other".to_vec(),
                5 => changed.sysroot_externs[0].compiler.sha256 = "d".repeat(64),
                6 => changed.sysroot_externs[0].host = "other-host".to_owned(),
                7 => changed.sysroot_externs[0].query.exit_code = 1,
                8 => {
                    changed.sysroot_externs[0].artifacts.pop();
                }
                9 => changed.sysroot_externs[0].artifacts[0].sha256 = "incomplete".to_owned(),
                10 => {
                    changed.sysroot_externs[0].artifacts[1].path =
                        bytes(&libdir.join("libproc_macro-fedcba9876543210.rmeta"))
                }
                11 => changed.sysroot_externs[0].searches[0]
                    .candidates
                    .push(b"libproc_macro-shadow.rlib".to_vec()),
                12 => changed.sysroot_externs[0].searches[0].argument = 7,
                _ => changed
                    .argv
                    .extend([b"--extern".to_vec(), b"proc_macro".to_vec()]),
            }
            refusal(&changed);
        }
        let mut changed = command.clone();
        changed.sysroot_externs[0].artifacts[0].sha256 = "e".repeat(64);
        assert_ne!(
            for_invocation(&graph, &bundle, &changed).unwrap().unwrap(),
            actual,
            "a changed required artifact must never compare equal"
        );
        let mut unrelated = command.clone();
        unrelated.sysroot_externs.clear();
        unrelated.argv[11] = b"other".to_vec();
        refusal(&unrelated);
        let mut missing = serde_json::to_value(&command).unwrap();
        missing.as_object_mut().unwrap().remove("sysroot_externs");
        assert!(
            serde_json::from_value::<Invocation>(missing).is_err(),
            "old capture fields cannot be retrofitted"
        );
        // The fixture's Cargo unit, with its target kind and crate types replaced.
        let retyped_unit = |kind: &str, crate_type: &str| {
            let mut graph = Graph::read(&input).unwrap();
            let unit = &mut graph.units.get_mut("macro").unwrap()[0];
            let old_key = unit.key().unwrap();
            unit.kind = vec![kind.to_owned()];
            unit.crate_types = vec![crate_type.to_owned()];
            let key = unit.key().unwrap();
            let locators = graph
                .source_locators
                .remove(&("macro".to_owned(), old_key))
                .unwrap();
            graph
                .source_locators
                .insert(("macro".to_owned(), key), locators);
            for (owner, artifact) in graph.artifacts.values_mut() {
                if owner == "macro" {
                    artifact.kind = vec![kind.to_owned()];
                    artifact.crate_types = vec![crate_type.to_owned()];
                }
            }
            graph
        };
        // A library compilation with an implicit proc_macro extern is refused
        // by the sysroot shape check, before any Cargo unit is matched.
        let wrong_unit = retyped_unit("lib", "lib");
        let mut wrong_command = command.clone();
        wrong_command.argv[4] = b"lib".to_vec();
        assert!(
            for_invocation(&wrong_unit, &bundle, &wrong_command)
                .unwrap_err()
                .contains("not one host proc-macro compilation")
        );
        // A proc-macro compilation whose matching Cargo unit is not a
        // proc-macro target reaches the unit check itself.
        let wrong_kind = retyped_unit("lib", "proc-macro");
        assert!(
            for_invocation(&wrong_kind, &bundle, &command)
                .unwrap_err()
                .contains("actual Cargo proc-macro")
        );
    }

    /// Synthetic Cargo/capture fixtures exercise the actual normalizer; they
    /// do not execute a compiler, producer, probe, Hermit or guest.
    #[test]
    fn shared_library_cohort_excludes_only_core_entry_and_requires_every_other_unit() {
        struct FixtureDirectory(PathBuf);
        impl Drop for FixtureDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!(
            "hermit-rustc-cohort-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let _fixture = FixtureDirectory(root.clone());
        let target_directory = root.join("target");
        let definitions = [
            (
                "core",
                "hermit-run",
                "hermit",
                "lib",
                "lib",
                "hermit-cli/src/lib.rs",
            ),
            (
                "core",
                "hermit-run",
                "build-script-build",
                "custom-build",
                "bin",
                "hermit-cli/build.rs",
            ),
            (
                "dependency",
                "shared-dependency",
                "shared_dependency",
                "lib",
                "lib",
                "shared-dependency/src/lib.rs",
            ),
            (
                "build-dependency",
                "build-dependency",
                "build_dependency",
                "lib",
                "lib",
                "build-dependency/src/lib.rs",
            ),
            (
                "core",
                "hermit-run",
                "hermit-dap",
                "bin",
                "bin",
                "hermit-cli/src/bin/hermit-dap/main.rs",
            ),
            (
                "core",
                "hermit-run",
                "hermit",
                "bin",
                "bin",
                "hermit-cli/src/bin/hermit/main.rs",
            ),
        ];
        let packages = [
            ("core", "hermit-run", "hermit-cli"),
            ("dependency", "shared-dependency", "shared-dependency"),
            ("build-dependency", "build-dependency", "build-dependency"),
        ];
        for (_, _, _, _, _, source) in &definitions {
            let path = root.join(source);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "// synthetic normalizer fixture\n").unwrap();
        }
        for (_, name, directory) in &packages {
            std::fs::write(
                root.join(directory).join("Cargo.toml"),
                format!("[package]\nname = {name:?}\nversion = \"0.5.4\"\n"),
            )
            .unwrap();
        }
        let manifest = root.join("Cargo.toml");
        let lock = root.join("Cargo.lock");
        std::fs::write(&manifest, "[workspace]\n").unwrap();
        std::fs::write(&lock, "version = 4\n").unwrap();
        let metadata = serde_json::json!({
            "version": 1,
            "workspace_root": root,
            "workspace_members": packages.iter().map(|(id, _, _)| id).collect::<Vec<_>>(),
            "packages": packages.iter().map(|(id, name, directory)| {
                serde_json::json!({
                    "id": id, "name": name, "version": "0.5.4", "source": null,
                    "manifest_path": root.join(directory).join("Cargo.toml"),
                    "targets": definitions.iter().filter(|definition| definition.0 == *id)
                        .map(|(_, _, name, kind, crate_type, source)| serde_json::json!({
                            "name": name, "kind": [kind], "crate_types": [crate_type],
                            "src_path": root.join(source),
                        })).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
            "resolve": {"nodes": [
                {"id":"core", "deps":[
                    {"pkg":"dependency", "dep_kinds":[{"kind":null}]},
                    {"pkg":"build-dependency", "dep_kinds":[{"kind":"build"}]},
                ]},
                {"id":"dependency", "deps":[]},
                {"id":"build-dependency", "deps":[]},
            ]},
        });
        let metadata_path = root.join("metadata.json");
        std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let input_file = |path: &Path| InputFile {
            path: path.to_owned(),
            sha256: digest(&std::fs::read(path).unwrap()),
        };
        let profile = serde_json::json!({
            "opt_level":"0", "debuginfo":2, "debug_assertions":true,
            "overflow_checks":true, "test":false,
        });
        let graph_for = |include_core_entry: bool| {
            let length = definitions.len() - usize::from(!include_core_entry);
            let mut stream = Vec::new();
            for (index, (id, _, name, kind, crate_type, source)) in
                definitions[..length].iter().enumerate()
            {
                let filename = target_directory
                    .join("debug/deps")
                    .join(format!("unit-{index}"));
                let artifact = serde_json::json!({
                    "reason":"compiler-artifact", "package_id":id,
                    "target":{"name":name,"kind":[kind],"crate_types":[crate_type],"src_path":root.join(source)},
                    "features":[], "profile":profile,
                    "filenames":[filename], "executable":null,
                });
                stream.extend(serde_json::to_vec(&artifact).unwrap());
                stream.push(b'\n');
            }
            stream.extend(b"{\"reason\":\"build-finished\",\"success\":true}\n");
            let path = root.join(if include_core_entry {
                "artifacts-with-cli.jsonl"
            } else {
                "artifacts-library.jsonl"
            });
            std::fs::write(&path, stream).unwrap();
            let artifacts = input_file(&path);
            let input = super::super::compatibility_graph::BuildInput {
                schema: super::super::compatibility_graph::INPUT_SCHEMA.to_owned(),
                metadata: input_file(&metadata_path),
                first_artifacts: artifacts.clone(),
                artifacts: artifacts.clone(),
                rustc_first: artifacts.clone(),
                rustc_second: artifacts.clone(),
                source_snapshot: artifacts,
                manifest: input_file(&manifest),
                lock: input_file(&lock),
                package: "hermit-run".to_owned(),
                binary: "hermit".to_owned(),
                target: "x86_64-unknown-linux-gnu".to_owned(),
                profile: "dev".to_owned(),
            };
            Graph::read(&input).unwrap()
        };
        let compiler = Compiler {
            path: bytes(&root.join("fixture-rustc")),
            identity: FileIdentity {
                device: 0,
                inode: 0,
                bytes: 1,
                mode: 0,
                modified: (0, 0),
                changed: (0, 0),
            },
            sha256: "a".repeat(64),
            verbose_version: format!("rustc fixture\ncommit-hash: {}\n", "b".repeat(40))
                .into_bytes(),
        };
        let captures = definitions
            .iter()
            .enumerate()
            .map(|(index, (id, name, target, _, crate_type, source))| {
                let directory = packages.iter().find(|package| package.0 == *id).unwrap().2;
                let invocation = Invocation {
                    schema: CAPTURE_SCHEMA.to_owned(),
                    pid: index as u32 + 1,
                    compiler: compiler.clone(),
                    cwd: bytes(&root),
                    argv: vec![
                        b"--crate-name".to_vec(),
                        target.replace('-', "_").into_bytes(),
                        bytes(&root.join(source)),
                        b"--crate-type".to_vec(),
                        crate_type.as_bytes().to_vec(),
                        b"--emit=link".to_vec(),
                        b"--out-dir".to_vec(),
                        bytes(&target_directory.join("debug/deps")),
                        b"-C".to_vec(),
                        b"opt-level=0".to_vec(),
                    ],
                    context: Context {
                        package: Some((*name).to_owned()),
                        version: Some("0.5.4".to_owned()),
                        manifest_directory: Some(bytes(&root.join(directory))),
                    },
                    response_files: vec![],
                    sysroot_externs: vec![],
                    source: Some(CompilerInput::File {
                        argument: 2,
                        path: bytes(&root.join(source)),
                        canonical_path: bytes(&root.join(source)),
                        identity: FileIdentity {
                            device: 0,
                            inode: 0,
                            bytes: std::fs::read(root.join(source)).unwrap().len() as u64,
                            mode: 0o100644,
                            modified: (0, 0),
                            changed: (0, 0),
                        },
                        bytes: std::fs::read(root.join(source)).unwrap(),
                        sha256: digest(&std::fs::read(root.join(source)).unwrap()),
                    }),
                };
                let completion = Completion {
                    schema: CAPTURE_SCHEMA.to_owned(),
                    invocation_sha256: digest(&serde_json::to_vec(&invocation).unwrap()),
                    pid: invocation.pid,
                    exit_code: Some(0),
                    signal: None,
                    inputs_unchanged: true,
                    capture_error: None,
                };
                Captured {
                    invocation,
                    completion,
                }
            })
            .collect::<Vec<_>>();
        let with_entry = Bundle {
            schema: CAPTURE_SCHEMA.to_owned(),
            wrapper_sha256: "c".repeat(64),
            target_directory: target_directory.clone(),
            commands: captures,
        };
        let mut library_only = with_entry.clone();
        library_only.commands.pop();
        let core_graph = graph_for(true);
        let extension_graph = graph_for(false);
        let expected = shared(&core_graph, &with_entry).unwrap();
        assert_eq!(expected.len(), definitions.len() - 1);
        assert_eq!(expected, shared(&extension_graph, &library_only).unwrap());
        assert!(cohort(&core_graph, &with_entry, &extension_graph, &library_only).is_ok());
        assert!(
            for_invocation(
                &core_graph,
                &with_entry,
                &with_entry.commands.last().unwrap().invocation
            )
            .unwrap()
            .is_none()
        );
        // A core CLI artifact does not require an executable-only capture in
        // the shared library cohort, but every other unit is still mandatory.
        assert_eq!(expected, shared(&core_graph, &library_only).unwrap());
        for index in 0..library_only.commands.len() {
            let mut missing = library_only.clone();
            missing.commands.remove(index);
            assert!(
                shared(&extension_graph, &missing)
                    .unwrap_err()
                    .contains("cached population is not accepted")
            );
            let mut changed = library_only.clone();
            changed.commands[index]
                .invocation
                .argv
                .extend([b"-C".to_vec(), b"opt-level=3".to_vec()]);
            changed.commands[index].completion.invocation_sha256 =
                digest(&serde_json::to_vec(&changed.commands[index].invocation).unwrap());
            assert!(cohort(&core_graph, &with_entry, &extension_graph, &changed).is_err());
        }

        // A real leased-home symlink is a locator, not a different package or
        // source. Both the Cargo declaration and captured argv remain bound.
        let alias = root.join("cargo-homes/slot-0");
        std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut aliased = library_only.clone();
        for captured in &mut aliased.commands {
            let command = &mut captured.invocation;
            let manifest = path(command.context.manifest_directory.as_ref().unwrap()).unwrap();
            command.context.manifest_directory =
                Some(bytes(&alias.join(manifest.strip_prefix(&root).unwrap())));
            let Some(CompilerInput::File {
                argument,
                path: raw,
                canonical_path,
                ..
            }) = &mut command.source
            else {
                panic!("file fixture");
            };
            let aliased_source =
                alias.join(path(canonical_path).unwrap().strip_prefix(&root).unwrap());
            command.argv[*argument] = bytes(&aliased_source);
            *raw = bytes(&aliased_source);
        }
        assert_eq!(expected, shared(&extension_graph, &aliased).unwrap());
        let mut mismatch_graph = graph_for(false);
        let dep = &library_only.commands[2].invocation;
        let key = mismatch_graph.units["dependency"][0].key().unwrap();
        let unrelated = root.join("shared-dependency/unrelated.rs");
        std::fs::write(&unrelated, b"// a real different file\n").unwrap();
        mismatch_graph
            .source_locators
            .get_mut(&("dependency".to_owned(), key))
            .unwrap()
            .emitted = unrelated;
        assert!(
            for_invocation(&mismatch_graph, &library_only, dep).is_err(),
            "real unrelated source must refuse"
        );
        let actual = root.join("shared-dependency/src/lib.rs");
        let saved = std::fs::read(&actual).unwrap();
        std::fs::remove_file(&actual).unwrap();
        assert!(
            for_invocation(&extension_graph, &library_only, dep).is_err(),
            "missing actual unit source must refuse"
        );
        std::fs::write(actual, saved).unwrap();
        let mut incomplete = library_only.clone();
        incomplete.commands[0].invocation.source = None;
        assert!(
            shared(&extension_graph, &incomplete).is_err(),
            "missing input cannot become a unit"
        );

        // Auxiliary emitter inputs/options/outcomes have a separate complete
        // comparison. They never fill a missing actual Cargo artifact unit.
        let mut aux_graph = graph_for(false);
        let package = aux_graph.packages["dependency"].clone();
        let custom = Unit {
            package: package.name.clone(),
            version: package.version.clone(),
            target: "build-script-build".to_owned(),
            kind: vec!["custom-build".to_owned()],
            crate_types: vec!["bin".to_owned()],
            features: vec![],
            profile: profile.clone(),
        };
        let build_source = root.join("shared-dependency/build.rs");
        std::fs::write(&build_source, b"fn main() {}\n").unwrap();
        aux_graph.source_locators.insert(
            (package.id.clone(), custom.key().unwrap()),
            super::super::compatibility_graph::SourceLocators {
                declared: build_source.clone(),
                emitted: build_source,
            },
        );
        aux_graph.units.get_mut(&package.id).unwrap().push(custom);
        let out = target_directory.join("debug/build/shared-dependency-actual/out");
        aux_graph
            .build_outputs
            .insert(package.id.clone(), [out.clone()].into_iter().collect());
        let probe = root.join("shared-dependency/probe.rs");
        let body = b"pub fn actual_probe() {}\n";
        std::fs::write(&probe, body).unwrap();
        let mut auxiliary_capture = library_only.commands[2].clone();
        let command = &mut auxiliary_capture.invocation;
        command.cwd = bytes(package.manifest_path.parent().unwrap());
        command.argv[1] = b"probe".to_vec();
        command.argv[2] = bytes(&probe);
        command.argv[7] = bytes(&out);
        command.source = Some(CompilerInput::File {
            argument: 2,
            path: bytes(&probe),
            canonical_path: bytes(&probe),
            identity: FileIdentity {
                bytes: body.len() as u64,
                ..compiler.identity.clone()
            },
            bytes: body.to_vec(),
            sha256: digest(body),
        });
        let mut aux_bundle = library_only.clone();
        aux_bundle.commands = vec![auxiliary_capture.clone()];
        let measured = auxiliary(&aux_graph, &aux_bundle).unwrap();
        assert_eq!(measured.values().sum::<usize>(), 1);
        assert!(
            for_invocation(&aux_graph, &aux_bundle, &auxiliary_capture.invocation)
                .unwrap()
                .is_none()
        );
        for alteration in 0..5 {
            let mut changed = aux_bundle.clone();
            let command = &mut changed.commands[0].invocation;
            match alteration {
                0 => command.argv.push(b"--cfg=changed_probe_semantics".to_vec()),
                1 => {
                    if let Some(CompilerInput::File {
                        bytes,
                        sha256,
                        identity,
                        ..
                    }) = &mut command.source
                    {
                        bytes.push(b' ');
                        *sha256 = digest(bytes);
                        identity.bytes = bytes.len() as u64;
                    }
                }
                2 => command.source = None,
                3 => {
                    command.argv[7] = bytes(&target_directory.join("debug/build/foreign-owner/out"))
                }
                _ => changed.commands[0].completion.exit_code = Some(101),
            }
            assert!(
                !auxiliary(&aux_graph, &changed).is_ok_and(|actual| actual == measured),
                "changed or incomplete auxiliary input/options/outcome must refuse equality"
            );
        }
        let mut stdin = aux_bundle.clone();
        stdin.commands[0].invocation.argv[2] = b"-".to_vec();
        stdin.commands[0].invocation.source = Some(CompilerInput::Stdin {
            argument: 2,
            bytes: body.to_vec(),
            sha256: digest(body),
        });
        assert_eq!(
            auxiliary(&aux_graph, &stdin)
                .unwrap()
                .values()
                .sum::<usize>(),
            1
        );
        let mut missing = stdin.clone();
        missing.commands[0].invocation.source = None;
        assert!(
            auxiliary(&aux_graph, &missing).is_err(),
            "stdin bytes cannot be inferred from argv"
        );
        missing = stdin.clone();
        if let Some(CompilerInput::Stdin { bytes, .. }) = &mut missing.commands[0].invocation.source
        {
            bytes.push(b' ');
        }
        assert!(
            auxiliary(&aux_graph, &missing).is_err(),
            "stdin digest must bind exact bytes"
        );
        // Failed stdin probes are admitted by outcome like file probes, in
        // both output forms, and named from their proof summaries.
        let mut failed_stdin = stdin.clone();
        failed_stdin.commands[0].completion.exit_code = Some(1);
        assert_eq!(
            auxiliary(&aux_graph, &failed_stdin)
                .unwrap()
                .values()
                .sum::<usize>(),
            1
        );
        assert_eq!(
            admitted_auxiliary_failures(&aux_graph, &failed_stdin, &stdin).unwrap(),
            vec![AdmittedAuxiliaryFailure {
                owner_package: "shared-dependency".to_owned(),
                source: "<stdin>".to_owned(),
                output_option: "--out-dir".to_owned(),
                output_relative: String::new(),
                exit_code: 1,
            }]
        );
        let mut failed_output_stdin = stdin.clone();
        failed_output_stdin.commands[0].invocation.argv[6] = b"-o".to_vec();
        failed_output_stdin.commands[0].invocation.argv[7] = bytes(&out.join("probe.rmeta"));
        failed_output_stdin.commands[0].completion.exit_code = Some(1);
        assert_eq!(
            auxiliary(&aux_graph, &failed_output_stdin)
                .unwrap()
                .values()
                .sum::<usize>(),
            1
        );
        assert_eq!(
            admitted_auxiliary_failures(&aux_graph, &failed_output_stdin, &stdin).unwrap(),
            vec![AdmittedAuxiliaryFailure {
                owner_package: "shared-dependency".to_owned(),
                source: "<stdin>".to_owned(),
                output_option: "-o".to_owned(),
                output_relative: "probe.rmeta".to_owned(),
                exit_code: 1,
            }]
        );
        for name in [
            b"autocfg_0123456789abcdef_0".as_slice(),
            b"autocfg_fedcba9876543210_0",
        ] {
            let mut unknown_generator = stdin.clone();
            unknown_generator.commands[0].invocation.argv = vec![
                b"--crate-name".to_vec(),
                name.to_vec(),
                b"-".to_vec(),
                b"--crate-type=lib".to_vec(),
                b"--emit=llvm-ir".to_vec(),
                b"--out-dir".to_vec(),
                bytes(&out),
            ];
            assert!(
                auxiliary(&aux_graph, &unknown_generator)
                    .unwrap_err()
                    .contains("exact measured generator"),
                "nonce form without actual exact generator must refuse"
            );
        }
        assert_eq!(
            autocfg_ordinal(b"autocfg_0123456789abcdef_17"),
            Some(b"17".as_slice())
        );
        for invalid in [
            b"arbitrary_0123456789abcdef_17".as_slice(),
            b"autocfg_0123456789abcdeg_17",
            b"autocfg_0123456789abcdef_017",
        ] {
            assert!(autocfg_ordinal(invalid).is_none());
        }
        // Admitted failed auxiliary probes are counted with their exact
        // outcome, and every other failed class refuses by name.
        let mut failed_aux = aux_bundle.clone();
        failed_aux.commands[0].completion.exit_code = Some(1);
        let failed_measured = auxiliary(&aux_graph, &failed_aux).unwrap();
        assert_eq!(failed_measured.values().sum::<usize>(), 1);
        assert_ne!(failed_measured, measured);
        assert_eq!(
            auxiliary_with_reference(&aux_graph, &failed_aux, &aux_bundle).unwrap(),
            failed_measured
        );
        failed_aux.commands[0].completion.invocation_sha256 =
            digest(&serde_json::to_vec(&failed_aux.commands[0].invocation).unwrap());
        let failed_aux_path = root.join("failed-aux-capture.json");
        std::fs::write(&failed_aux_path, serde_json::to_vec(&failed_aux).unwrap()).unwrap();
        assert!(
            Bundle::read(&input_file(&failed_aux_path)).is_ok(),
            "a well-formed failed auxiliary capture is integrity-valid"
        );
        let mut signaled_aux = failed_aux.clone();
        signaled_aux.commands[0].completion.signal = Some(9);
        let signaled_path = root.join("signaled-aux-capture.json");
        std::fs::write(&signaled_path, serde_json::to_vec(&signaled_aux).unwrap()).unwrap();
        assert!(
            Bundle::read(&input_file(&signaled_path))
                .unwrap_err()
                .contains("incomplete/changed"),
            "a signaled completion is never an admitted probe"
        );
        // Each remaining integrity clause is planted separately so removing
        // any one of them from completion_well_formed fails by name.
        for name in ["capture-error", "inputs-changed", "missing-exit-code"] {
            let mut planted = failed_aux.clone();
            let completion = &mut planted.commands[0].completion;
            match name {
                "capture-error" => {
                    completion.capture_error = Some("planted capture error".to_owned())
                }
                "inputs-changed" => completion.inputs_unchanged = false,
                _ => completion.exit_code = None,
            }
            let planted_path = root.join(format!("planted-{name}.json"));
            std::fs::write(&planted_path, serde_json::to_vec(&planted).unwrap()).unwrap();
            assert!(
                Bundle::read(&input_file(&planted_path))
                    .unwrap_err()
                    .contains("incomplete/changed"),
                "{name} must refuse at Bundle::read"
            );
            assert!(
                auxiliary_with_reference(&aux_graph, &planted, &aux_bundle)
                    .unwrap_err()
                    .contains("incomplete/changed"),
                "{name} must refuse in the classifier"
            );
        }
        assert_eq!(
            admitted_auxiliary_failures(&aux_graph, &failed_aux, &aux_bundle).unwrap(),
            vec![AdmittedAuxiliaryFailure {
                owner_package: "shared-dependency".to_owned(),
                source: "probe.rs".to_owned(),
                output_option: "--out-dir".to_owned(),
                output_relative: String::new(),
                exit_code: 1,
            }]
        );

        // Failed shared, entry, and query commands refuse on every path,
        // including the producer's final-population auxiliary check.
        let mut failed_shared = library_only.clone();
        failed_shared.commands[2].completion.exit_code = Some(1);
        assert!(
            shared(&extension_graph, &failed_shared)
                .unwrap_err()
                .contains("actual Cargo compiler command did not succeed")
        );
        assert!(
            auxiliary_with_reference(&extension_graph, &failed_shared, &library_only)
                .unwrap_err()
                .contains("actual Cargo compiler command did not succeed")
        );
        let planted_path = root.join("planted-failed-shared.json");
        std::fs::write(&planted_path, serde_json::to_vec(&failed_shared).unwrap()).unwrap();
        let planted = Bundle::read(&input_file(&planted_path)).unwrap();
        assert!(shared(&extension_graph, &planted).is_err());
        assert!(auxiliary_with_reference(&extension_graph, &planted, &library_only).is_err());
        let mut failed_entry = with_entry.clone();
        failed_entry
            .commands
            .last_mut()
            .unwrap()
            .completion
            .exit_code = Some(1);
        assert!(shared(&core_graph, &failed_entry).is_err());
        assert!(auxiliary_with_reference(&core_graph, &failed_entry, &with_entry).is_err());
        let mut query_capture = aux_bundle.commands[0].clone();
        query_capture.invocation.argv = vec![b"-vV".to_vec()];
        query_capture.invocation.source = None;
        query_capture.completion.exit_code = Some(1);
        query_capture.completion.invocation_sha256 =
            digest(&serde_json::to_vec(&query_capture.invocation).unwrap());
        let query_bundle = Bundle {
            schema: CAPTURE_SCHEMA.to_owned(),
            wrapper_sha256: "c".repeat(64),
            target_directory: target_directory.clone(),
            commands: vec![query_capture],
        };
        assert!(
            queries(&query_bundle)
                .unwrap_err()
                .contains("compiler query did not complete successfully")
        );
        assert!(
            auxiliary_with_reference(&aux_graph, &query_bundle, &aux_bundle)
                .unwrap_err()
                .contains("compiler query did not complete successfully")
        );

        // A core-owned failed probe participates in cohort equality: two
        // identical failed populations pass, a flipped outcome refuses.
        let mut cohort_graph = graph_for(false);
        let core_out = target_directory.join("debug/build/hermit-run-actual/out");
        cohort_graph
            .build_outputs
            .insert("core".to_owned(), [core_out.clone()].into_iter().collect());
        let core_probe = root.join("hermit-cli/probe.rs");
        std::fs::write(&core_probe, body).unwrap();
        let mut core_aux_capture = library_only.commands[2].clone();
        {
            let command = &mut core_aux_capture.invocation;
            command.pid = 999;
            command.cwd = bytes(&root.join("hermit-cli"));
            command.context = Context {
                package: Some("hermit-run".to_owned()),
                version: Some("0.5.4".to_owned()),
                manifest_directory: Some(bytes(&root.join("hermit-cli"))),
            };
            command.argv[1] = b"probe".to_vec();
            command.argv[2] = bytes(&core_probe);
            command.argv[7] = bytes(&core_out);
            command.source = Some(CompilerInput::File {
                argument: 2,
                path: bytes(&core_probe),
                canonical_path: bytes(&core_probe),
                identity: FileIdentity {
                    bytes: body.len() as u64,
                    ..compiler.identity.clone()
                },
                bytes: body.to_vec(),
                sha256: digest(body),
            });
            core_aux_capture.completion.pid = 999;
            core_aux_capture.completion.invocation_sha256 =
                digest(&serde_json::to_vec(command).unwrap());
            core_aux_capture.completion.exit_code = Some(1);
        }
        let mut cohort_failed = library_only.clone();
        cohort_failed.commands.push(core_aux_capture.clone());
        let mut cohort_success = cohort_failed.clone();
        cohort_success
            .commands
            .last_mut()
            .unwrap()
            .completion
            .exit_code = Some(0);
        assert!(cohort(&cohort_graph, &cohort_failed, &cohort_graph, &cohort_failed).is_ok());
        assert!(
            cohort(
                &cohort_graph,
                &cohort_failed,
                &cohort_graph,
                &cohort_success
            )
            .is_err(),
            "a flipped auxiliary outcome must refuse cohort equality"
        );
        assert_eq!(
            admitted_auxiliary_failures(&cohort_graph, &cohort_failed, &cohort_failed).unwrap(),
            vec![AdmittedAuxiliaryFailure {
                owner_package: "hermit-run".to_owned(),
                source: "probe.rs".to_owned(),
                output_option: "--out-dir".to_owned(),
                output_relative: String::new(),
                exit_code: 1,
            }]
        );
        // Core-closure proof failures refuse, including response files.
        let mut foreign_core_aux = cohort_failed.clone();
        foreign_core_aux
            .commands
            .last_mut()
            .unwrap()
            .invocation
            .argv[7] = bytes(&target_directory.join("debug/build/foreign-owner/out"));
        assert!(
            auxiliary_with_reference(&cohort_graph, &foreign_core_aux, &cohort_failed)
                .unwrap_err()
                .contains("build-script out_dir")
        );
        let mut response_core_aux = cohort_failed.clone();
        response_core_aux
            .commands
            .last_mut()
            .unwrap()
            .invocation
            .argv
            .push(b"@response".to_vec());
        assert!(
            auxiliary_with_reference(&cohort_graph, &response_core_aux, &cohort_failed)
                .unwrap_err()
                .contains("auxiliary response expansion")
        );

        // An extension-own failed auxiliary is admitted after proof and
        // named, but not counted in the shared core auxiliary map.
        let ext_directory = root.join("extension-probe");
        std::fs::create_dir_all(&ext_directory).unwrap();
        std::fs::write(
            ext_directory.join("Cargo.toml"),
            "[package]\nname = \"extension-probe\"\nversion = \"0.5.4\"\n",
        )
        .unwrap();
        let ext_build = ext_directory.join("build.rs");
        std::fs::write(&ext_build, b"fn main() {}\n").unwrap();
        let ext_probe = ext_directory.join("probe.rs");
        std::fs::write(&ext_probe, body).unwrap();
        let mut ext_package = package.clone();
        ext_package.id = "extension".to_owned();
        ext_package.name = "extension-probe".to_owned();
        ext_package.manifest_path = ext_directory.join("Cargo.toml");
        let ext_unit = Unit {
            package: ext_package.name.clone(),
            version: ext_package.version.clone(),
            target: "build-script-build".to_owned(),
            kind: vec!["custom-build".to_owned()],
            crate_types: vec!["bin".to_owned()],
            features: vec![],
            profile: profile.clone(),
        };
        cohort_graph.source_locators.insert(
            (ext_package.id.clone(), ext_unit.key().unwrap()),
            super::super::compatibility_graph::SourceLocators {
                declared: ext_build.clone(),
                emitted: ext_build,
            },
        );
        cohort_graph
            .units
            .insert(ext_package.id.clone(), vec![ext_unit]);
        let ext_out = target_directory.join("debug/build/extension-probe-actual/out");
        cohort_graph.build_outputs.insert(
            ext_package.id.clone(),
            [ext_out.clone()].into_iter().collect(),
        );
        cohort_graph
            .packages
            .insert(ext_package.id.clone(), ext_package.clone());
        let mut ext_capture = core_aux_capture.clone();
        {
            let command = &mut ext_capture.invocation;
            command.pid = 1000;
            command.cwd = bytes(&ext_directory);
            command.context = Context {
                package: Some("extension-probe".to_owned()),
                version: Some("0.5.4".to_owned()),
                manifest_directory: Some(bytes(&ext_directory)),
            };
            command.argv[2] = bytes(&ext_probe);
            command.argv[7] = bytes(&ext_out);
            command.source = Some(CompilerInput::File {
                argument: 2,
                path: bytes(&ext_probe),
                canonical_path: bytes(&ext_probe),
                identity: FileIdentity {
                    bytes: body.len() as u64,
                    ..compiler.identity.clone()
                },
                bytes: body.to_vec(),
                sha256: digest(body),
            });
            ext_capture.completion.pid = 1000;
            ext_capture.completion.invocation_sha256 =
                digest(&serde_json::to_vec(command).unwrap());
        }
        let ext_bundle = Bundle {
            schema: CAPTURE_SCHEMA.to_owned(),
            wrapper_sha256: "c".repeat(64),
            target_directory: target_directory.clone(),
            commands: vec![ext_capture.clone()],
        };
        assert!(auxiliary(&cohort_graph, &ext_bundle).unwrap().is_empty());
        assert_eq!(
            admitted_auxiliary_failures(&cohort_graph, &ext_bundle, &ext_bundle).unwrap(),
            vec![AdmittedAuxiliaryFailure {
                owner_package: "extension-probe".to_owned(),
                source: "probe.rs".to_owned(),
                output_option: "--out-dir".to_owned(),
                output_relative: String::new(),
                exit_code: 1,
            }]
        );
        let mut ext_foreign = ext_bundle.clone();
        ext_foreign.commands[0].invocation.argv[7] =
            bytes(&target_directory.join("debug/build/foreign-owner/out"));
        assert!(
            auxiliary_with_reference(&cohort_graph, &ext_foreign, &ext_bundle)
                .unwrap_err()
                .contains("build-script out_dir")
        );
        // A failed extension-owned Cargo unit is still a Cargo unit: it
        // contributes no shared semantics, but its failure must refuse.
        let ext_lib_path = ext_directory.join("lib.rs");
        std::fs::write(&ext_lib_path, body).unwrap();
        let ext_lib_unit = Unit {
            package: ext_package.name.clone(),
            version: ext_package.version.clone(),
            target: "extension_probe".to_owned(),
            kind: vec!["lib".to_owned()],
            crate_types: vec!["lib".to_owned()],
            features: vec![],
            profile: profile.clone(),
        };
        cohort_graph.source_locators.insert(
            (ext_package.id.clone(), ext_lib_unit.key().unwrap()),
            super::super::compatibility_graph::SourceLocators {
                declared: ext_lib_path.clone(),
                emitted: ext_lib_path.clone(),
            },
        );
        cohort_graph.artifacts.insert(
            ext_out.join("libextension_probe.rlib"),
            (ext_package.id.clone(), ext_lib_unit.clone()),
        );
        cohort_graph
            .units
            .get_mut(&ext_package.id)
            .unwrap()
            .push(ext_lib_unit);
        let mut ext_unit_capture = ext_capture.clone();
        {
            let command = &mut ext_unit_capture.invocation;
            command.pid = 1001;
            command.argv[1] = b"extension_probe".to_vec();
            command.argv[2] = bytes(&ext_lib_path);
            command.source = Some(CompilerInput::File {
                argument: 2,
                path: bytes(&ext_lib_path),
                canonical_path: bytes(&ext_lib_path),
                identity: FileIdentity {
                    bytes: body.len() as u64,
                    ..compiler.identity.clone()
                },
                bytes: body.to_vec(),
                sha256: digest(body),
            });
            ext_unit_capture.completion.pid = 1001;
            ext_unit_capture.completion.invocation_sha256 =
                digest(&serde_json::to_vec(command).unwrap());
        }
        let ext_unit_bundle = Bundle {
            schema: CAPTURE_SCHEMA.to_owned(),
            wrapper_sha256: "c".repeat(64),
            target_directory: target_directory.clone(),
            commands: vec![ext_unit_capture.clone()],
        };
        assert_eq!(
            classify(
                &cohort_graph,
                &ext_unit_bundle,
                &ext_unit_bundle,
                &ext_unit_capture.invocation
            )
            .unwrap(),
            Classification::CargoUnit
        );
        assert!(
            shared(&cohort_graph, &ext_unit_bundle)
                .unwrap_err()
                .contains("actual Cargo compiler command did not succeed")
        );
        assert!(
            auxiliary_with_reference(&cohort_graph, &ext_unit_bundle, &ext_bundle)
                .unwrap_err()
                .contains("actual Cargo compiler command did not succeed")
        );
        // A core-closure probe whose source lies outside its owner package
        // fails the auxiliary proof by name.
        let outside_path = root.join("outside-owner.rs");
        std::fs::write(&outside_path, body).unwrap();
        let mut outside_capture = core_aux_capture.clone();
        {
            let command = &mut outside_capture.invocation;
            command.argv[2] = bytes(&outside_path);
            command.source = Some(CompilerInput::File {
                argument: 2,
                path: bytes(&outside_path),
                canonical_path: bytes(&outside_path),
                identity: FileIdentity {
                    bytes: body.len() as u64,
                    ..compiler.identity.clone()
                },
                bytes: body.to_vec(),
                sha256: digest(body),
            });
            outside_capture.completion.invocation_sha256 =
                digest(&serde_json::to_vec(command).unwrap());
        }
        let outside_bundle = Bundle {
            schema: CAPTURE_SCHEMA.to_owned(),
            wrapper_sha256: "c".repeat(64),
            target_directory: target_directory.clone(),
            commands: vec![outside_capture],
        };
        assert!(
            auxiliary_with_reference(&cohort_graph, &outside_bundle, &cohort_failed)
                .unwrap_err()
                .contains("outside its actual package")
        );

        // The schema cannot retrofit missing old stdin bytes.
        let mut old = serde_json::to_value(&stdin).unwrap();
        old["schema"] = "hermit-actual-rustc-capture/v1".into();
        let old_path = root.join("old-incomplete-capture.json");
        std::fs::write(&old_path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(Bundle::read(&input_file(&old_path)).is_err());
    }

    struct SourceUnitFixture {
        root: PathBuf,
        graph: Graph,
        command: Invocation,
    }

    impl Drop for SourceUnitFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn source_unit_fixture() -> SourceUnitFixture {
        use std::os::unix::fs::MetadataExt;

        use super::super::compatibility_graph::SourceLocators;
        use super::super::compatibility_graph::Target;
        let root = std::env::temp_dir().join(format!(
            "hermit-source-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("lib.rs");
        let body = b"pub fn same_real_source() {}\n";
        std::fs::write(&source, body).unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let identity = FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            mode: metadata.mode(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        };
        let package = Package {
            id: "cfg-if".to_owned(),
            name: "cfg-if".to_owned(),
            version: "1.0.4".to_owned(),
            source: None,
            manifest_path: root.join("Cargo.toml"),
            targets: vec![Target {
                name: "cfg_if".to_owned(),
                kind: vec!["lib".to_owned()],
                crate_types: vec!["lib".to_owned()],
                src_path: source.clone(),
            }],
        };
        let mut units = Vec::new();
        let mut artifacts = BTreeMap::new();
        let mut source_locators = BTreeMap::new();
        for (optimization, directory) in [
            ("0", "target/validate/deps"),
            ("3", "target/x86_64-unknown-linux-gnu/validate/deps"),
        ] {
            let unit = Unit {
                package: package.name.clone(),
                version: package.version.clone(),
                target: "cfg_if".to_owned(),
                kind: vec!["lib".to_owned()],
                crate_types: vec!["lib".to_owned()],
                features: vec![],
                profile: serde_json::json!({"opt_level":optimization,"debuginfo":0,"debug_assertions":true,"overflow_checks":true,"test":false}),
            };
            source_locators.insert(
                (package.id.clone(), unit.key().unwrap()),
                SourceLocators {
                    declared: source.clone(),
                    emitted: source.clone(),
                },
            );
            artifacts.insert(
                root.join(directory).join("libcfg_if.rlib"),
                (package.id.clone(), unit.clone()),
            );
            units.push(unit);
        }
        let graph = Graph::for_source_unit_test(
            [(package.id.clone(), package)].into_iter().collect(),
            [("cfg-if".to_owned(), units)].into_iter().collect(),
            artifacts,
            source_locators,
        );
        let command = Invocation {
            schema: CAPTURE_SCHEMA.to_owned(),
            pid: 1,
            compiler: Compiler {
                path: bytes(&root.join("not-executed-rustc")),
                identity: identity.clone(),
                sha256: "a".repeat(64),
                verbose_version: b"synthetic selector control; compiler not executed".to_vec(),
            },
            cwd: bytes(&root),
            argv: vec![
                b"--crate-name".to_vec(),
                b"cfg_if".to_vec(),
                bytes(&source),
                b"--crate-type".to_vec(),
                b"lib".to_vec(),
                b"--out-dir".to_vec(),
                bytes(&root.join("target/x86_64-unknown-linux-gnu/validate/deps")),
                b"-C".to_vec(),
                b"opt-level=3".to_vec(),
                b"--target".to_vec(),
                b"x86_64-unknown-linux-gnu".to_vec(),
            ],
            context: Context {
                package: Some("cfg-if".to_owned()),
                version: Some("1.0.4".to_owned()),
                manifest_directory: Some(bytes(&root)),
            },
            response_files: vec![],
            sysroot_externs: vec![],
            source: Some(CompilerInput::File {
                argument: 2,
                path: bytes(&source),
                canonical_path: bytes(&source),
                identity,
                bytes: body.to_vec(),
                sha256: digest(body),
            }),
        };
        SourceUnitFixture {
            root,
            graph,
            command,
        }
    }

    #[test]
    fn emitted_source_unit_distinguishes_host_and_target_profiles_and_outputs() {
        let fixture = source_unit_fixture();
        let package = &fixture.graph.packages["cfg-if"];
        let (_, _, target) = actual_source_unit(&fixture.graph, package, &fixture.command)
            .unwrap()
            .unwrap();
        assert_eq!(target.profile["opt_level"], "3");
        let mut host = fixture.command.clone();
        host.argv.truncate(7); // The real host form has no target or explicit opt-level.
        host.argv[6] = bytes(&fixture.root.join("target/validate/deps"));
        let (_, _, unit) = actual_source_unit(&fixture.graph, package, &host)
            .unwrap()
            .unwrap();
        assert_eq!(unit.profile["opt_level"], "0");
        assert_ne!(target, unit);
        let mut crossed = fixture.command.clone();
        crossed.argv[6] = host.argv[6].clone();
        assert!(actual_source_unit(&fixture.graph, package, &crossed).is_err());
        let mut wrong_name = fixture.command.clone();
        wrong_name.argv[1] = b"different_crate".to_vec();
        assert!(actual_source_unit(&fixture.graph, package, &wrong_name).is_err());
    }

    #[test]
    fn classified_failed_query_and_cargo_unit_refuse_in_final_auxiliary_path() {
        let fixture = source_unit_fixture();
        std::fs::write(
            fixture.root.join("Cargo.toml"),
            "[package]\nname = \"cfg-if\"\nversion = \"1.0.4\"\n",
        )
        .unwrap();
        let captured = |command: Invocation| {
            let completion = Completion {
                schema: CAPTURE_SCHEMA.to_owned(),
                invocation_sha256: digest(&serde_json::to_vec(&command).unwrap()),
                pid: command.pid,
                exit_code: Some(1),
                signal: None,
                inputs_unchanged: true,
                capture_error: None,
            };
            Bundle {
                schema: CAPTURE_SCHEMA.to_owned(),
                wrapper_sha256: "c".repeat(64),
                target_directory: fixture.root.join("target"),
                commands: vec![Captured {
                    invocation: command,
                    completion,
                }],
            }
        };
        let failed_unit = captured(fixture.command.clone());
        assert_eq!(
            classify(&fixture.graph, &failed_unit, &failed_unit, &fixture.command).unwrap(),
            Classification::CargoUnit
        );
        assert!(
            auxiliary_with_reference(&fixture.graph, &failed_unit, &failed_unit)
                .unwrap_err()
                .contains("actual Cargo compiler command did not succeed")
        );

        let mut query = fixture.command.clone();
        query.argv = vec![b"-vV".to_vec()];
        query.source = None;
        let failed_query = captured(query.clone());
        assert_eq!(
            classify(&fixture.graph, &failed_query, &failed_query, &query).unwrap(),
            Classification::Query
        );
        assert!(
            auxiliary_with_reference(&fixture.graph, &failed_query, &failed_query)
                .unwrap_err()
                .contains("compiler query did not complete successfully")
        );
    }

    #[test]
    fn emitted_source_unit_refuses_indistinguishable_duplicate_owners() {
        let mut fixture = source_unit_fixture();
        let duplicate = fixture.graph.units["cfg-if"][1].clone();
        fixture
            .graph
            .units
            .get_mut("cfg-if")
            .unwrap()
            .push(duplicate);
        assert!(
            actual_source_unit(
                &fixture.graph,
                &fixture.graph.packages["cfg-if"],
                &fixture.command
            )
            .unwrap_err()
            .contains("ambiguous emitted Cargo unit owners")
        );
    }
}
