/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The one statically selected application's physical-backend boundary.
//!
//! An executable supplies one concrete adapter to [`super::main_with_backend`].
//! The common application still owns entry, argument parsing, input snapshots,
//! run disposition, comparison and publication. In particular neither a
//! physical capture nor backend facts are a verification verdict.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;

use clap::Arg;
use clap::ArgAction;
use clap::ArgGroup;
use clap::ArgMatches;
use clap::Command;
use clap::builder::ValueParser;
use clap::builder::ValueRange;
use hermit::DetConfig;
use hermit::Error;
use hermit::ExitStatus;
pub use hermit::canonical_verdict::VerificationRun;
use reverie::process::Output;
use tracing::metadata::LevelFilter;

/// A parsed mount specification retained exactly for command reconstruction.
pub use super::run::MountArg;
pub use super::run::RunLatches;
/// Stamp only the common invocation-bound pending NO-RESULT report. This is
/// the same fixed operation used by the shared application and saved comparison;
/// terminal verdicts and their publication remain owned by the common driver.
pub use super::verify::write_pending_verification_json;

/// The operation whose physical execution the application is preparing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Ordinary,
    SavedVerification,
    VerificationPair,
}

/// Immutable requirements declared before any physical run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceRequirement {
    /// The complete common log/output/status comparison, without native counters.
    Common,
    /// Additionally compare all six fields of [`NativeFacts`] exactly.
    NativeCounters,
}

/// The real source of this adapter's verification records. This is declared
/// before preparation; no empty/default transport can omit a required source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationLogTransport {
    /// Detcore emits through the coordinator's synchronous bounded subscriber.
    CoordinatorTracing,
    /// Detcore in the guest sends typed records through the core-owned socket.
    InGuestSocket,
    /// The maintained native collector supplies records and all native facts.
    NativeEvidence,
}

impl VerificationLogTransport {
    pub(super) fn validate(self, evidence: EvidenceRequirement) -> Result<(), Error> {
        match (self, evidence) {
            (Self::CoordinatorTracing | Self::InGuestSocket, EvidenceRequirement::Common)
            | (Self::NativeEvidence, EvidenceRequirement::NativeCounters) => Ok(()),
            _ => Err(Error::msg(
                "adapter log transport does not match its required verification evidence",
            )),
        }
    }
}

/// One statically linked physical adapter, never a registry or a verdict owner.
///
/// The selected executable supplies its own concrete options and state. None
/// of these public types depends on a third-party backend enum or feature.
pub trait BackendAdapter {
    const NAME: &'static str;
    /// Static record preprocessing support. Core retains the ptrace recorder,
    /// canonical program resolver, identity mounts, IO and publication.
    const PREPARES_RECORD: bool = false;
    /// Static syscall-tracing support, checked before host or artifact work.
    const SUPPORTS_STRACE: bool = false;
    const EVIDENCE: EvidenceRequirement;
    /// The actual physical runtime's facts, not an assurance or test verdict.
    const CAPABILITIES: reverie::BackendCapabilities;
    /// Required physical record transport; independent of descriptor sharing.
    const LOG_TRANSPORT: VerificationLogTransport;
    /// An immutable identity diagnostic, emitted only after both native decoders
    /// and their complete pair evidence succeeded. It cannot affect a verdict.
    /// Common comparison publishes JSON and every native comparison message
    /// before this diagnostic, including when the completed runs diverged.
    const NATIVE_IDENTITY_DIAGNOSTIC: Option<&'static str> = None;
    /// Reason text for the core's mandatory early refusal of a nondefault CPU
    /// profile. An adapter cannot admit a profile through this declaration.
    const NONDEFAULT_CPU_PROFILE_REFUSAL: &'static str = "only the ptrace backend is qualified";

    type Options;
    type Session;
    type Pending;
    type Facts;

    fn option_schemas(&self) -> Vec<OptionSchema> {
        Vec::new()
    }

    fn parse(&self, matches: &ArgMatches) -> Result<Self::Options, clap::Error>;

    /// Prepare only the supplied core-resolved original program. Return the
    /// rewritten file, or None when no overlay is required; this callback
    /// cannot select mounts or run a second recorder. Called at each actual
    /// recording-container preparation, after common admission.
    fn prepare_record_program(&self, _target: &Path) -> Result<Option<PathBuf>, Error> {
        Err(Error::new(super::container::PolicyRefusal)
            .context("this executable-selected backend has not admitted record preparation"))
    }

    /// Run only this executable's admitted physical tracer. Common logging
    /// admission has already completed; this hook cannot select a verdict.
    fn strace(&self, _program: &Path, _args: &[String]) -> Result<ExitStatus, Error> {
        Err(Error::new(super::container::PolicyRefusal)
            .context("this executable-selected backend does not support syscall tracing"))
    }

    /// Static operation support, checked before input/resource/writer work.
    /// This must not probe a runtime or create resources. Saved execution is
    /// opt-in only after the adapter uses the real core capture and, for native
    /// evidence, implements the raw save/restore owner callbacks below.
    fn validate_mode(&self, mode: Mode) -> Result<(), Error> {
        match mode {
            Mode::Ordinary | Mode::VerificationPair => Ok(()),
            Mode::SavedVerification => {
                Err(Error::new(super::container::PolicyRefusal).context(format!(
                    "--verify-run is not yet supported by executable-selected backend `{}`: \
                 this adapter has not admitted the saved-record operation; \
                 use live --verify instead",
                    Self::NAME
                )))
            }
        }
    }

    fn prepare(&self, options: Self::Options, request: Prepare<'_>)
    -> Result<Self::Session, Error>;

    fn execute(
        &self,
        session: &mut Self::Session,
        request: Execute<'_>,
    ) -> Result<Execution<Self::Pending>, Error>;

    /// Called only after common latches, log integrity and first-run exit policy.
    fn finish(
        &self,
        pending: Self::Pending,
        observation: Observation<'_>,
    ) -> Result<Self::Facts, Error>;

    /// Return complete observations, not matched flags or mutable verdicts.
    fn pair_evidence(
        &self,
        first: &Self::Facts,
        second: &Self::Facts,
    ) -> Result<PairEvidence, Error>;

    /// Preserve the backend's validated raw native records for the core-owned
    /// saved-run writer. This is not a serialization of comparison summaries.
    fn save_native_evidence(&self, _facts: &Self::Facts) -> Result<SavedNativeEvidence, Error> {
        Err(Error::new(hermit::BackendRunFailure)
            .context("backend does not implement validated raw native saved evidence"))
    }

    /// Restore through the same native decoder and refusal policy as a live
    /// run. The core still compares the complete `pair_evidence` dimensions.
    fn restore_native_evidence(
        &self,
        _evidence: SavedNativeEvidenceRef<'_>,
    ) -> Result<Self::Facts, Error> {
        Err(Error::new(hermit::BackendRunFailure)
            .context("backend does not implement validated raw native saved evidence restoration"))
    }
}

/// Backend-owned, already validated native record bytes. The core owns their
/// filename, create-new writes, synchronization, record metadata and publication
/// order. The backend owns their existing decoder, never the comparison verdict.
pub struct SavedNativeEvidence {
    raw: Vec<u8>,
    initialization_records: u64,
}

impl SavedNativeEvidence {
    pub fn new(raw: Vec<u8>, initialization_records: u64) -> Self {
        Self {
            raw,
            initialization_records,
        }
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn initialization_records(&self) -> u64 {
        self.initialization_records
    }

    /// Store only this exact payload in a new, uncommitted artifact directory.
    /// This uses the same core writer as the admitted saved-run operation. No
    /// record, outcome or comparison is published; only the saved-run driver
    /// can commit a complete run after its existing policy checks.
    pub fn create_uncommitted_artifact(
        &self,
        directory: &Path,
    ) -> Result<SavedNativeArtifact, Error> {
        super::verify_run::VerifyRunWriter::create(directory)?.write_native_evidence(self)
    }

    pub fn as_ref(&self) -> SavedNativeEvidenceRef<'_> {
        SavedNativeEvidenceRef {
            raw: Ok(&self.raw),
            initialization_records: self.initialization_records,
        }
    }
}

/// Immutable locator for the exact native payload written by the core owner.
/// Possessing this artifact does not publish or admit a saved verification run.
pub struct SavedNativeArtifact {
    pub(super) path: PathBuf,
    pub(super) initialization_records: u64,
}

impl SavedNativeArtifact {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<SavedNativeEvidence, Error> {
        Ok(SavedNativeEvidence::new(
            std::fs::read(&self.path)?,
            self.initialization_records,
        ))
    }
}

/// The actual raw payload and recorded initialization count read by the core.
/// A static adapter must decode them before returning its complete typed facts.
#[derive(Clone, Copy)]
pub struct SavedNativeEvidenceRef<'a> {
    pub(super) raw: Result<&'a [u8], &'a std::io::Error>,
    pub(super) initialization_records: u64,
}

impl<'a> SavedNativeEvidenceRef<'a> {
    /// Preserve the actual saved-payload read result, including missing and
    /// unreadable files, for the backend's unchanged native refusal policy.
    pub fn raw(self) -> Result<&'a [u8], &'a std::io::Error> {
        self.raw
    }

    pub fn initialization_records(self) -> u64 {
        self.initialization_records
    }
}

/// A new option. Its value parser is arbitrary; its relations are confined to
/// the same extension schema. There is deliberately no core Command/Arg editor
/// or `overrides_with` operation at this boundary.
#[derive(Clone, Debug)]
pub struct BackendOption {
    argument: Arg,
    relations: Vec<String>,
}

impl BackendOption {
    pub fn new(id: impl Into<String>, long: impl Into<String>) -> Self {
        Self {
            argument: Arg::new(id.into()).long(long.into()),
            relations: Vec::new(),
        }
    }

    pub fn short(mut self, value: char) -> Self {
        self.argument = self.argument.short(value);
        self
    }

    pub fn alias(mut self, value: impl Into<String>) -> Self {
        self.argument = self.argument.alias(value.into());
        self
    }

    pub fn short_alias(mut self, value: char) -> Self {
        self.argument = self.argument.short_alias(value);
        self
    }

    pub fn help(mut self, value: impl Into<String>) -> Self {
        self.argument = self.argument.help(value.into());
        self
    }

    pub fn action(mut self, value: ArgAction) -> Self {
        self.argument = self.argument.action(value);
        self
    }

    pub fn value_parser(mut self, value: impl Into<ValueParser>) -> Self {
        self.argument = self.argument.value_parser(value);
        self
    }

    pub fn value_name(mut self, value: impl Into<String>) -> Self {
        self.argument = self.argument.value_name(value.into());
        self
    }

    pub fn num_args(mut self, value: impl Into<ValueRange>) -> Self {
        self.argument = self.argument.num_args(value);
        self
    }

    pub fn default_value(mut self, value: impl Into<String>) -> Self {
        self.argument = self.argument.default_value(value.into());
        self
    }

    pub fn env(mut self, value: impl Into<String>) -> Self {
        self.argument = self.argument.env(value.into());
        self
    }

    pub fn required(mut self, value: bool) -> Self {
        self.argument = self.argument.required(value);
        self
    }

    pub fn requires(mut self, extension_id: impl Into<String>) -> Self {
        let id = extension_id.into();
        self.argument = self.argument.requires(id.clone());
        self.relations.push(id);
        self
    }

    pub fn conflicts_with(mut self, extension_id: impl Into<String>) -> Self {
        let id = extension_id.into();
        self.argument = self.argument.conflicts_with(id.clone());
        self.relations.push(id);
        self
    }
}

/// An extension-only option group. Existing application groups remain intact.
#[derive(Clone, Debug)]
pub struct BackendOptionGroup {
    group: ArgGroup,
    arguments: Vec<String>,
}

impl BackendOptionGroup {
    pub fn new(id: impl Into<String>, arguments: impl IntoIterator<Item = String>) -> Self {
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        Self {
            group: ArgGroup::new(id.into()).args(arguments.clone()),
            arguments,
        }
    }

    pub fn required(mut self, value: bool) -> Self {
        self.group = self.group.required(value);
        self
    }

    pub fn multiple(mut self, value: bool) -> Self {
        self.group = self.group.multiple(value);
        self
    }
}

/// Add only extension options/groups to an existing command path (`[]` is root).
/// One final composed schema is used for CLI/config/replay and pending stamps.
#[derive(Clone, Debug)]
pub struct OptionSchema {
    path: Vec<String>,
    arguments: Vec<BackendOption>,
    groups: Vec<BackendOptionGroup>,
}

impl OptionSchema {
    pub fn new(path: impl IntoIterator<Item = String>) -> Self {
        Self {
            path: path.into_iter().collect(),
            arguments: Vec::new(),
            groups: Vec::new(),
        }
    }

    pub fn argument(mut self, argument: BackendOption) -> Self {
        self.arguments.push(argument);
        self
    }

    pub fn group(mut self, group: BackendOptionGroup) -> Self {
        self.groups.push(group);
        self
    }
}

#[derive(Default)]
struct Names {
    ids: BTreeSet<String>,
    longs: BTreeSet<String>,
    shorts: BTreeSet<char>,
}

impl Names {
    fn collect(command: &Command) -> Self {
        fn visit(command: &Command, names: &mut Names) {
            for argument in command.get_arguments() {
                names.insert_argument(argument);
            }
            for group in command.get_groups() {
                names.ids.insert(group.get_id().to_string());
            }
            for child in command.get_subcommands() {
                visit(child, names);
            }
        }
        let mut names = Self::default();
        visit(command, &mut names);
        // Clap adds these while building the command, after composition.
        names.ids.extend(["help".to_owned(), "version".to_owned()]);
        names
            .longs
            .extend(["help".to_owned(), "version".to_owned()]);
        names.shorts.extend(['h', 'V']);
        names
    }

    fn insert_argument(&mut self, argument: &Arg) {
        self.ids.insert(argument.get_id().to_string());
        self.longs.extend(argument.get_long().map(str::to_owned));
        self.longs.extend(
            argument
                .get_all_aliases()
                .into_iter()
                .flatten()
                .map(str::to_owned),
        );
        self.shorts.extend(argument.get_short());
        self.shorts
            .extend(argument.get_all_short_aliases().into_iter().flatten());
    }

    fn check_argument(&self, argument: &Arg) -> Result<(), Error> {
        anyhow::ensure!(
            !argument.get_id().as_str().is_empty(),
            "backend option IDs must be nonempty; choose a new extension-only ID"
        );
        anyhow::ensure!(
            !self.ids.contains(argument.get_id().as_str()),
            "backend option ID {:?} conflicts with an existing application option/group",
            argument.get_id()
        );
        let mut longs = BTreeSet::new();
        for long in argument
            .get_long()
            .into_iter()
            .chain(argument.get_all_aliases().into_iter().flatten())
        {
            anyhow::ensure!(
                !long.is_empty()
                    && !long.starts_with('-')
                    && !long.contains('=')
                    && !long.chars().any(char::is_whitespace)
                    && longs.insert(long),
                "invalid or duplicate backend long option {long:?}; choose a unique flag name"
            );
            anyhow::ensure!(
                !self.longs.contains(long),
                "backend option --{long} conflicts with an existing application option/alias"
            );
        }
        let mut shorts = BTreeSet::new();
        for short in argument
            .get_short()
            .into_iter()
            .chain(argument.get_all_short_aliases().into_iter().flatten())
        {
            anyhow::ensure!(
                short != '-'
                    && !short.is_whitespace()
                    && !short.is_control()
                    && shorts.insert(short),
                "invalid or duplicate backend short option {short:?}; choose a unique flag name"
            );
            anyhow::ensure!(
                !self.shorts.contains(&short),
                "backend option -{short} conflicts with an existing application option/alias"
            );
        }
        Ok(())
    }
}

pub(super) fn validate_name(name: &str) -> Result<(), Error> {
    use clap::ValueEnum;

    anyhow::ensure!(
        !name.is_empty()
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "backend identity must be a nonempty lowercase name with letters, digits and hyphens"
    );
    anyhow::ensure!(
        <hermit::Backend as ValueEnum>::from_str(name, false).is_err(),
        "backend identity `{name}` is already a built-in backend; choose a distinct executable identity"
    );
    Ok(())
}

pub(super) fn compose(mut command: Command, schemas: Vec<OptionSchema>) -> Result<Command, Error> {
    let mut names = Names::collect(&command);
    for schema in schemas {
        let mut local = BTreeSet::new();
        for option in &schema.arguments {
            names.check_argument(&option.argument)?;
            anyhow::ensure!(
                local.insert(option.argument.get_id().to_string()),
                "duplicate backend option {:?}",
                option.argument.get_id()
            );
            names.insert_argument(&option.argument);
        }
        for group in &schema.groups {
            let id = group.group.get_id().to_string();
            anyhow::ensure!(
                !names.ids.contains(&id) && local.insert(id.clone()),
                "backend group {id:?} conflicts with an existing option/group"
            );
            names.ids.insert(id);
            let members = group.arguments.iter().collect::<BTreeSet<_>>();
            anyhow::ensure!(
                !group.arguments.is_empty()
                    && members.len() == group.arguments.len()
                    && group.arguments.iter().all(|id| schema
                        .arguments
                        .iter()
                        .any(|option| option.argument.get_id().as_str() == id)),
                "backend groups must contain unique options from their own schema"
            );
        }
        for option in &schema.arguments {
            anyhow::ensure!(
                option
                    .relations
                    .iter()
                    .all(|id| { local.contains(id) && option.argument.get_id().as_str() != id }),
                "backend option relations may reference only their own schema's options/groups"
            );
        }
        let mut target = &mut command;
        for name in &schema.path {
            target = target.find_subcommand_mut(name).ok_or_else(|| {
                Error::msg(format!("no application command at {:?}", schema.path))
            })?;
        }
        for option in schema.arguments {
            *target = std::mem::take(target).arg(option.argument);
        }
        for group in schema.groups {
            *target = std::mem::take(target).group(group.group);
        }
    }
    Ok(command)
}

/// One external configuration prepared before physical work. The common owner
/// captures terminal inheritance and signal-route normalization exactly once.
#[derive(Clone)]
pub(super) struct PreparedPhysical {
    capabilities: reverie::BackendCapabilities,
    config: DetConfig,
}

impl PreparedPhysical {
    pub(super) fn new(
        options: &super::run::RunOpts,
        capabilities: reverie::BackendCapabilities,
    ) -> Self {
        Self {
            capabilities,
            config: hermit::prepare_backend_config_with_capabilities(
                options.effective_det_config(),
                capabilities,
            ),
        }
    }

    #[cfg(test)]
    pub(super) fn config(&self) -> &DetConfig {
        &self.config
    }

    fn for_run(&self, options: &super::run::RunOpts) -> Self {
        let mut physical = self.clone();
        // Terminal/capability/signal preparation is invocation-owned. Only the
        // common verifier's private per-run provenance descriptor varies.
        options.apply_run_metadata(&mut physical.config);
        physical
    }
}

/// Common inputs already admitted by the application. The stdin owner moves
/// here once; a session must retain its replay state for a verification pair.
pub struct Prepare<'a> {
    pub(super) options: &'a super::run::RunOpts,
    pub(super) global: &'a super::global_opts::GlobalOpts,
    pub(super) mode: Mode,
    pub(super) stdin: Option<File>,
    pub(super) physical: Option<&'a PreparedPhysical>,
}

impl Prepare<'_> {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn backend_name(&self) -> &str {
        self.options.execution_backend_name()
    }

    pub fn program(&self) -> &Path {
        self.options.adapter_program()
    }

    pub fn arguments(&self) -> &[String] {
        self.options.adapter_arguments()
    }

    /// The actual command with common image/environment/argv0 policy applied.
    pub fn command(&self) -> Result<reverie::process::Command, Error> {
        self.options.guest_command()
    }

    pub fn summary_json(&self) -> Option<&Path> {
        self.options.summary_json.as_deref()
    }

    /// Resolve through the maintained guest mount/symlink visibility rules.
    pub fn executable_overlay_plan(&self) -> Result<ExecutableOverlay, Error> {
        self.options.adapter_executable_overlay_plan()
    }

    /// Bind both the actual command and its readonly overlay to one plan.
    pub fn command_with_executable_overlay(
        &self,
        plan: &ExecutableOverlay,
        source: Option<&Path>,
    ) -> Result<(reverie::process::Command, ContainerScope), Error> {
        let options = self.options.adapter_with_executable_overlay(plan, source)?;
        let command = options.guest_command()?;
        Ok((command, ContainerScope::prepared(&options, self.physical)))
    }

    pub fn environment(&self) -> Result<BTreeMap<OsString, OsString>, Error> {
        Ok(self.options.guest_command()?.get_captured_envs())
    }

    pub fn working_directory(&self) -> Option<&Path> {
        self.options.adapter_working_directory()
    }

    pub fn config(&self) -> DetConfig {
        match self.physical {
            Some(physical) => physical.for_run(self.options).config,
            None => self.options.effective_det_config(),
        }
    }

    pub fn binds(&self) -> Vec<(PathBuf, PathBuf)> {
        self.options.adapter_binds()
    }

    pub fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }

    pub fn logging(&self) -> Option<LevelFilter> {
        self.global.log
    }

    pub fn log_file(&self) -> Option<&Path> {
        self.global.log_file.as_deref()
    }

    pub fn saved_verification_directory(&self) -> Option<&Path> {
        self.options.adapter_saved_directory()
    }

    pub fn summary_requested(&self) -> bool {
        self.options.summary
    }

    pub fn engagement_path(&self) -> Option<&Path> {
        self.options.adapter_engagement_path()
    }

    pub fn verbose_verification(&self) -> bool {
        self.options.adapter_verbose_verification()
    }

    pub fn verification_json(&self) -> Option<&Path> {
        self.options.adapter_verification_json()
    }

    /// Keep ordinary coordinator diagnostics at the existing WARN cap while
    /// physical guest evidence uses its separate admitted logging channel.
    pub fn coordinator_tracing(&self) -> CoordinatorTracing {
        let mut options = self.global.clone();
        options.log = Some(
            options
                .log
                .unwrap_or(LevelFilter::WARN)
                .min(LevelFilter::WARN),
        );
        CoordinatorTracing { options }
    }

    /// Retain the existing owned container/identity/resource boundary. An
    /// extension does not acquire a second container implementation.
    pub fn container(&self) -> ContainerScope {
        ContainerScope::prepared(self.options, self.physical)
    }
}

/// A preprocessor's resolved executable paths, constructed only by the
/// maintained common resolver. Installing it rechecks the complete plan
/// against the receiving invocation, then applies the existing readonly
/// overlay ordering, mount-target and source-visibility rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutableOverlay {
    pub(super) guest: PathBuf,
    pub(super) host: PathBuf,
    pub(super) target: PathBuf,
}

impl ExecutableOverlay {
    pub fn guest_program(&self) -> &Path {
        &self.guest
    }
    pub fn host_program(&self) -> &Path {
        &self.host
    }
}

/// A controlled physical launch with the application's mounts and bounds.
#[derive(Clone)]
pub struct ContainerScope {
    options: super::run::RunOpts,
    physical: Option<PreparedPhysical>,
}

impl ContainerScope {
    fn prepared(options: &super::run::RunOpts, physical: Option<&PreparedPhysical>) -> Self {
        Self {
            options: match physical {
                Some(physical) => options.with_adapter_capabilities(physical.capabilities),
                None => options.clone(),
            },
            physical: physical.map(|physical| physical.for_run(options)),
        }
    }

    pub fn uses_namespace(&self) -> bool {
        self.options.adapter_uses_namespace()
    }

    /// Select a stable private runtime directory from this invocation's
    /// admitted bind targets. No host directory contents affect the choice.
    /// A launch outside the common namespace keeps its existing host path.
    pub fn runtime_directory_target(&self, prefix: &str) -> Result<Option<PathBuf>, Error> {
        if !self.uses_namespace() {
            return Ok(None);
        }
        anyhow::ensure!(
            !prefix.is_empty()
                && !prefix.as_bytes().contains(&0)
                && Path::new(prefix).file_name() == Some(std::ffi::OsStr::new(prefix)),
            "runtime directory prefix must be one normal path component"
        );
        let binds = self.options.adapter_binds();
        // Admission accepts only normal descendants of /tmp. Each declared
        // target can overlap at most one of these direct sibling directories.
        // Do not consult host contents: those differ between physical runs.
        let directory = (0..=binds.len())
            .map(|slot| PathBuf::from(format!("/tmp/{prefix}{slot}")))
            .find(|candidate| {
                binds.iter().all(|(_, target)| {
                    !candidate.starts_with(target) && !target.starts_with(candidate)
                })
            })
            .ok_or_else(|| Error::msg("runtime directory conflicts with declared binds"))?;
        Ok(Some(directory))
    }

    pub fn mounts_tmpfs_at(&self, target: &Path) -> bool {
        self.options.mounts_tmpfs_at(target)
    }

    pub fn has_shared_ld_so_cache(&self) -> bool {
        self.options.has_ld_so_cache_snapshot()
    }

    /// Add a guest-owned mount with the same ordering and fingerprint rules as
    /// an admitted guest mount, rather than as a private runtime mount.
    pub fn with_guest_mount(mut self, mount: MountArg) -> Self {
        self.options = self.options.with_guest_mount(mount);
        self
    }

    /// Additional physical-runtime inputs join the existing mount plan. This
    /// cannot replace the caller's options or the container's identity guard.
    pub fn with_runtime_mounts(
        mut self,
        binds: &[reverie::process::Bind],
        mounts: Vec<MountArg>,
    ) -> Self {
        self.options = self.options.with_adapter_mounts(binds, mounts);
        self
    }

    /// Use the one prepared physical declaration for both the container's
    /// procfs policy and the Detcore configuration, without assuming ptrace.
    pub fn run<G, F, T>(&self, guards: G, resources: &str, work: F) -> Result<(T, G), Error>
    where
        G: 'static,
        F: FnMut(&mut G, DetConfig) -> Result<T, Error> + 'static,
        T: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        self.run_inner(guards, resources, None, work)
    }

    /// Initialize coordinator tracing inside the same owned container. Tracing
    /// owns the PID-slot reservation on this path, and its guard remains alive
    /// through configuration and the complete physical operation.
    pub fn run_with_tracing<G, F, T>(
        &self,
        guards: G,
        resources: &str,
        tracing: CoordinatorTracing,
        work: F,
    ) -> Result<(T, G), Error>
    where
        G: 'static,
        F: FnMut(&mut G, DetConfig) -> Result<T, Error> + 'static,
        T: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        self.run_inner(guards, resources, Some(tracing), work)
    }

    fn run_inner<G, F, T>(
        &self,
        guards: G,
        resources: &str,
        tracing: Option<CoordinatorTracing>,
        mut work: F,
    ) -> Result<(T, G), Error>
    where
        G: 'static,
        F: FnMut(&mut G, DetConfig) -> Result<T, Error> + 'static,
        T: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        self.run_with_setup(
            guards,
            resources,
            move || {
                // Each real setup path owns exactly one PID-slot reservation.
                Ok(match &tracing {
                    Some(tracing) => Some(tracing.clone().enter()),
                    None => {
                        super::tracing::align_namespace_pid_baseline();
                        None
                    }
                })
            },
            move |guards, config, _tracing| work(guards, config),
        )
    }

    fn run_with_setup<G, S, Setup, F, T>(
        &self,
        guards: G,
        resources: &str,
        mut setup: Setup,
        mut work: F,
    ) -> Result<(T, G), Error>
    where
        G: 'static,
        S: 'static,
        Setup: FnMut() -> Result<S, Error> + 'static,
        F: FnMut(&mut G, DetConfig, S) -> Result<T, Error> + 'static,
        T: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        let physical = self.physical.as_ref().ok_or_else(|| {
            Error::msg("external physical configuration was not prepared by the application")
        })?;
        let options = &self.options;
        let config = physical.config.clone();
        let cwd = options.adapter_working_directory().map(Path::to_owned);
        options.launch_in_container(
            guards,
            resources,
            options.run_timeout(),
            move |guards, identity, native| {
                anyhow::ensure!(
                    native.is_none(),
                    "an external physical adapter cannot consume a native exec enrollment"
                );
                super::container::exit_init_after_publication();
                let setup = setup()?;
                let config = super::run::complete_container_config(
                    config.clone(),
                    identity,
                    cwd.as_deref(),
                )?;
                work(guards, config, setup)
            },
        )
    }
}

/// Private common tracing configuration retained until ordinary execution.
#[derive(Clone)]
pub struct CoordinatorTracing {
    options: super::global_opts::GlobalOpts,
}

impl CoordinatorTracing {
    #[must_use = "retain the coordinator tracing guard for the whole physical operation"]
    pub fn enter(self) -> CoordinatorTracingGuard {
        // The generic physical path owns a normal Linux task in the namespace.
        // Ptrace selects the same non-KVM tracing setup without a third-party
        // backend name at this public boundary. It does not select execution.
        CoordinatorTracingGuard {
            _guard: self
                .options
                .init_tracing_for_backend(hermit::Backend::Ptrace),
        }
    }
}

pub struct CoordinatorTracingGuard {
    _guard: Option<super::tracing::TracingGuard>,
}

/// One physical-operation request. Common comparison options are not writable
/// through it. A verification capture receives its actual log and run label.
pub struct Execute<'a> {
    pub(super) mode: Mode,
    pub(super) verification: Option<VerificationCapture<'a>>,
}

impl<'a> Execute<'a> {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn into_verification(self) -> Result<VerificationCapture<'a>, Error> {
        self.verification.ok_or_else(|| {
            Error::msg("a non-verification physical request has no verification log")
        })
    }
}

pub struct VerificationCapture<'a> {
    pub log: File,
    pub path: &'a Path,
    pub run: VerificationRun,
    pub(super) options: &'a super::run::RunOpts,
    pub(super) physical: Option<&'a PreparedPhysical>,
    pub(super) requested_log: Option<LevelFilter>,
    pub(super) log_budget: Option<super::tracing::LogBudget>,
    pub(super) log_transport: VerificationLogTransport,
    pub(super) forwarded_detlogs: super::run::ForwardedDetlogPlan,
}

impl VerificationCapture<'_> {
    pub fn config(&self) -> DetConfig {
        match self.physical {
            Some(physical) => physical.for_run(self.options).config,
            None => self.options.effective_det_config(),
        }
    }

    pub fn container(&self) -> ContainerScope {
        ContainerScope::prepared(self.options, self.physical)
    }

    /// The actual admitted logging request, with the canonical native evidence
    /// floor and the existing explicit full-trace diagnostic policy.
    pub fn evidence_log_level(&self) -> LevelFilter {
        super::verify::verification_log_level(
            self.requested_log,
            super::verify::LogCompareStrictness::Canonical,
            self.options.adapter_verbose_verification(),
        )
        .max(LevelFilter::INFO)
    }

    /// Consume this capture through the existing container/resource owner.
    /// The closure performs only physical work. Writer policy, forwarding,
    /// drain/flush, loss latches and comparison remain in the common library.
    pub fn run_in_container<G, F, P>(
        self,
        guards: G,
        resources: &str,
        work: F,
    ) -> Result<(PhysicalRun<P>, G), Error>
    where
        G: 'static,
        F: FnMut(&mut G, VerificationCommand) -> Result<CapturedOutput<P>, Error> + 'static,
        P: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        let options = self.options.clone();
        self.run_with_options(options, guards, resources, work)
    }

    /// Same common capture, with the preprocessor's validated readonly
    /// executable plan also applied to this physical run's own container.
    pub fn run_in_container_with_executable_overlay<G, F, P>(
        self,
        plan: &ExecutableOverlay,
        source: Option<&Path>,
        guards: G,
        resources: &str,
        work: F,
    ) -> Result<(PhysicalRun<P>, G), Error>
    where
        G: 'static,
        F: FnMut(&mut G, VerificationCommand) -> Result<CapturedOutput<P>, Error> + 'static,
        P: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        let options = self.options.adapter_with_executable_overlay(plan, source)?;
        self.run_with_options(options, guards, resources, work)
    }

    fn run_with_options<G, F, P>(
        self,
        options: super::run::RunOpts,
        guards: G,
        resources: &str,
        mut work: F,
    ) -> Result<(PhysicalRun<P>, G), Error>
    where
        G: 'static,
        F: FnMut(&mut G, VerificationCommand) -> Result<CapturedOutput<P>, Error> + 'static,
        P: serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        anyhow::ensure!(
            self.log_transport != VerificationLogTransport::NativeEvidence,
            "native evidence must use its maintained typed capture, not the common tracing capture"
        );
        let container = ContainerScope::prepared(&options, self.physical);
        let level = super::verify::verification_log_level(
            self.requested_log,
            options.verification_strictness(),
            options.adapter_verbose_verification(),
        );
        let mut file = Some(self.log);
        let mut forwarded = self.forwarded_detlogs;
        let budget = self.log_budget;
        let run = self.run;
        let verification_json = options.adapter_verification_json().map(Path::to_owned);
        let ((output, skid_overshoots, latches, pending), guards) = container.run_with_setup(
            guards,
            resources,
            move || {
                let file = file
                    .take()
                    .ok_or_else(|| Error::msg("verification capture log was already consumed"))?;
                super::run::VerificationLogSession::open(
                    file,
                    &mut forwarded,
                    level,
                    budget.clone(),
                )
            },
            move |guards, config, mut log| {
                let _deadlock =
                    super::verify::record_terminal_deadlock(verification_json.as_deref(), run);
                let result = (|| {
                    let command = options.guest_command()?;
                    work(
                        guards,
                        VerificationCommand {
                            command,
                            config,
                            print_summary: options.summary,
                            summary_json: options.summary_json.clone(),
                        },
                    )
                })();
                // Drain/clear and flush before propagating a physical error too.
                let latches = log.finish();
                let captured = result?;
                Ok((
                    captured.output,
                    captured.skid_overshoots,
                    latches,
                    captured.pending,
                ))
            },
        )?;
        Ok((
            PhysicalRun {
                output,
                skid_overshoots,
                latches,
                terminal_outcome: None,
                pending,
            },
            guards,
        ))
    }

    /// Install the common terminal-deadlock report only for this capture's
    /// actual run and report path. Keep the returned affine guard in scope
    /// until the physical run and its failure handling have completed.
    #[must_use = "retain the terminal-deadlock guard through the physical run"]
    pub fn record_terminal_deadlock(&self) -> TerminalDeadlockGuard {
        TerminalDeadlockGuard {
            _guard: super::verify::record_terminal_deadlock(
                self.options.adapter_verification_json(),
                self.run,
            ),
        }
    }
}

/// Affine ownership of the common scheduler's temporary deadlock report hook.
pub struct TerminalDeadlockGuard {
    _guard: Option<super::verify::TerminalDeadlockRecording>,
}

/// Admitted physical inputs. These carry no comparator or verdict authority.
pub struct VerificationCommand {
    pub command: reverie::process::Command,
    pub config: DetConfig,
    pub print_summary: bool,
    pub summary_json: Option<PathBuf>,
}

/// What the physical callback actually observed, before common latch checks.
pub struct CapturedOutput<P> {
    pub output: Output,
    pub skid_overshoots: u64,
    pub pending: P,
}

/// Exact physical output plus deferred state, without a verdict.
pub struct PhysicalRun<P> {
    pub output: Output,
    pub skid_overshoots: u64,
    pub latches: RunLatches,
    /// The scheduler terminal outcome the run ended with, if the backend
    /// unwound the guest at one (a deadlock under Hermit's model or a virtual
    /// timeout); `output.status` is then the outcome's.
    pub terminal_outcome: Option<detcore::TerminalOutcome>,
    pub pending: P,
}

pub enum Execution<P> {
    Finished(ExitStatus),
    Verification(PhysicalRun<P>),
}

impl<P> Execution<P> {
    pub(super) fn finished(self, mode: Mode) -> Result<ExitStatus, Error> {
        match self {
            Self::Finished(status) if mode != Mode::VerificationPair => Ok(status),
            _ => Err(Error::msg(
                "backend execution did not match its prepared operation mode",
            )),
        }
    }

    pub(super) fn verification(self) -> Result<PhysicalRun<P>, Error> {
        match self {
            Self::Verification(run) => Ok(run),
            Self::Finished(_) => Err(Error::msg(
                "backend returned a finished status instead of a physical verification capture",
            )),
        }
    }
}

pub struct Observation<'a> {
    pub output: &'a Output,
    pub run: VerificationRun,
}

/// All native equality dimensions are mandatory and retain their exact types.
/// `statistics_diagnostic` is the validated full snapshot's original Debug
/// spelling; it is only diagnostic text, never an equality predicate.
#[derive(Clone)]
pub struct NativeFacts {
    pub counted_branches: u64,
    pub intercepted_syscalls: u64,
    pub rewritten_syscalls: u64,
    pub stdin_reads: u64,
    pub memory_hash_fold: u64,
    pub initialization_records: usize,
    pub statistics_diagnostic: String,
}

impl std::fmt::Debug for NativeFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.statistics_diagnostic)
    }
}

impl super::verify::NativeStatistics for NativeFacts {
    fn intercepted_syscalls(&self) -> u64 {
        self.intercepted_syscalls
    }
    fn rewritten_syscalls(&self) -> u64 {
        self.rewritten_syscalls
    }
    fn stdin_reads(&self) -> u64 {
        self.stdin_reads
    }
    fn memory_hash_fold(&self) -> u64 {
        self.memory_hash_fold
    }
}

#[derive(Clone, Debug)]
pub enum PairEvidence {
    Common,
    NativeCounters {
        first: NativeFacts,
        second: NativeFacts,
    },
}

impl EvidenceRequirement {
    pub(super) fn validate(self, evidence: PairEvidence) -> Result<PairEvidence, Error> {
        if !matches!(
            (self, &evidence),
            (Self::Common, PairEvidence::Common)
                | (Self::NativeCounters, PairEvidence::NativeCounters { .. })
        ) {
            return Err(Error::new(hermit::BackendRunFailure)
                .context("backend omitted or changed its predeclared verification evidence"));
        }
        Ok(evidence)
    }
}

/// The common nonwaiting diagnostic channel for a physical adapter's existing
/// preparation banner. This does not select a log filter, writer or verdict.
/// The original macro keeps the current invocation's log-cap policy.
pub fn controller_diagnostic(message: std::fmt::Arguments<'_>) {
    super::tracing::diagnostic_eprintln!("{message}");
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;

    use clap::CommandFactory;
    use clap::Parser;

    use super::*;

    /// Synthetic physical captures exercise the real application boundary;
    /// they are not guest execution or a backend qualification.
    #[derive(Default)]
    struct StaticAdapter {
        events: RefCell<Vec<String>>,
        first_loss: bool,
        first_exit: i32,
        fail_finish: Option<VerificationRun>,
        omit_evidence: bool,
        changed_dimension: Option<usize>,
    }

    struct Session {
        mode: Mode,
        count: u64,
    }

    impl BackendAdapter for StaticAdapter {
        const NAME: &'static str = "dynamorio";
        const EVIDENCE: EvidenceRequirement = EvidenceRequirement::NativeCounters;
        const CAPABILITIES: reverie::BackendCapabilities = reverie::BackendCapabilities::DBT;
        const LOG_TRANSPORT: VerificationLogTransport = VerificationLogTransport::NativeEvidence;

        type Options = u64;
        type Session = Session;
        type Pending = (VerificationRun, u64);
        type Facts = NativeFacts;

        fn option_schemas(&self) -> Vec<OptionSchema> {
            vec![
                OptionSchema::new(["run".to_owned()]).argument(
                    BackendOption::new("adapter_count", "adapter-count")
                        .value_parser(clap::value_parser!(u64))
                        .default_value("7")
                        .help("Synthetic adapter counter for the static-boundary test"),
                ),
            ]
        }

        fn parse(&self, matches: &ArgMatches) -> Result<u64, clap::Error> {
            Ok(matches
                .subcommand_matches("run")
                .and_then(|run| run.get_one::<u64>("adapter_count"))
                .copied()
                .unwrap_or(7))
        }

        fn prepare(&self, count: u64, mut request: Prepare<'_>) -> Result<Session, Error> {
            assert_eq!(request.backend_name(), Self::NAME);
            assert!(request.take_stdin().is_none());
            self.events
                .borrow_mut()
                .push(format!("prepare:{:?}", request.mode()));
            Ok(Session {
                mode: request.mode(),
                count,
            })
        }

        fn execute(
            &self,
            session: &mut Session,
            request: Execute<'_>,
        ) -> Result<Execution<Self::Pending>, Error> {
            anyhow::ensure!(
                session.mode == request.mode(),
                "synthetic adapter mode changed"
            );
            let mut capture = request.into_verification()?;
            self.events
                .borrow_mut()
                .push(format!("execute:{:?}", capture.run));
            capture.log.write_all(
                b"1970-01-01T00:00:00.000000Z INFO detcore: synthetic static-adapter record\n",
            )?;
            let first = capture.run == VerificationRun::Run1;
            Ok(Execution::Verification(PhysicalRun {
                output: Output {
                    status: ExitStatus::Exited(if first { self.first_exit } else { 0 }),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
                skid_overshoots: 0,
                latches: RunLatches {
                    determinism_loss: (first && self.first_loss)
                        .then(|| "synthetic latch".to_owned()),
                    log_sink_failure: None,
                },
                terminal_outcome: None,
                pending: (capture.run, session.count),
            }))
        }

        fn finish(
            &self,
            (run, count): Self::Pending,
            observation: Observation<'_>,
        ) -> Result<NativeFacts, Error> {
            assert_eq!(run, observation.run);
            self.events.borrow_mut().push(format!("finish:{run:?}"));
            anyhow::ensure!(
                self.fail_finish != Some(run),
                "synthetic deferred facts failure"
            );
            let mut facts = NativeFacts {
                counted_branches: count,
                intercepted_syscalls: 4,
                rewritten_syscalls: 3,
                stdin_reads: 2,
                memory_hash_fold: 19,
                initialization_records: 1,
                statistics_diagnostic: "synthetic exact native facts".to_owned(),
            };
            if run == VerificationRun::Run2 {
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
            first: &NativeFacts,
            second: &NativeFacts,
        ) -> Result<PairEvidence, Error> {
            self.events.borrow_mut().push("pair".to_owned());
            Ok(if self.omit_evidence {
                PairEvidence::Common
            } else {
                PairEvidence::NativeCounters {
                    first: first.clone(),
                    second: second.clone(),
                }
            })
        }
    }

    fn options(directory: &Path) -> super::super::run::RunOpts {
        let mut options = super::super::run::RunOpts::parse_from([
            "run",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
            directory.join("logs").to_str().unwrap(),
            "--verify-json",
            directory.join("report.json").to_str().unwrap(),
            "--",
            "/bin/true",
        ]);
        options
            .select_application_backend(StaticAdapter::NAME)
            .unwrap();
        options
    }

    fn run(adapter: &StaticAdapter, directory: &Path) -> Result<ExitStatus, Error> {
        let options = options(directory);
        super::super::verify::write_pending_verification_json(&directory.join("report.json"))?;
        options.verify_sharing_ld_so_cache(
            &super::super::global_opts::GlobalOpts::parse_from(["hermit"]),
            None,
            adapter,
            7,
        )
    }

    fn report(directory: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(directory.join("report.json")).unwrap()).unwrap()
    }

    struct WrongTransport(StaticAdapter);

    impl BackendAdapter for WrongTransport {
        const NAME: &'static str = StaticAdapter::NAME;
        const EVIDENCE: EvidenceRequirement = StaticAdapter::EVIDENCE;
        const CAPABILITIES: reverie::BackendCapabilities = StaticAdapter::CAPABILITIES;
        const LOG_TRANSPORT: VerificationLogTransport = VerificationLogTransport::InGuestSocket;

        type Options = <StaticAdapter as BackendAdapter>::Options;
        type Session = <StaticAdapter as BackendAdapter>::Session;
        type Pending = <StaticAdapter as BackendAdapter>::Pending;
        type Facts = <StaticAdapter as BackendAdapter>::Facts;

        fn parse(&self, matches: &ArgMatches) -> Result<Self::Options, clap::Error> {
            self.0.parse(matches)
        }

        fn prepare(
            &self,
            options: Self::Options,
            request: Prepare<'_>,
        ) -> Result<Self::Session, Error> {
            self.0.prepare(options, request)
        }

        fn execute(
            &self,
            session: &mut Self::Session,
            request: Execute<'_>,
        ) -> Result<Execution<Self::Pending>, Error> {
            self.0.execute(session, request)
        }

        fn finish(
            &self,
            pending: Self::Pending,
            observation: Observation<'_>,
        ) -> Result<Self::Facts, Error> {
            self.0.finish(pending, observation)
        }

        fn pair_evidence(
            &self,
            first: &Self::Facts,
            second: &Self::Facts,
        ) -> Result<PairEvidence, Error> {
            self.0.pair_evidence(first, second)
        }
    }

    #[test]
    fn verification_transport_mismatch_refuses_before_prepare_or_capture() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = WrongTransport(StaticAdapter::default());
        let options = options(directory.path());
        super::super::verify::write_pending_verification_json(
            &directory.path().join("report.json"),
        )
        .unwrap();
        let error = options
            .verify_sharing_ld_so_cache(
                &super::super::global_opts::GlobalOpts::parse_from(["hermit"]),
                None,
                &adapter,
                7,
            )
            .unwrap_err();
        assert!(error.to_string().contains("log transport does not match"));
        assert!(adapter.0.events.borrow().is_empty());
        assert!(!directory.path().join("logs").exists());
        let report = report(directory.path());
        assert_eq!(report["verdict"], "no_result");
        assert_eq!(report["verified"], false);
        for transport in [
            VerificationLogTransport::CoordinatorTracing,
            VerificationLogTransport::InGuestSocket,
        ] {
            assert!(transport.validate(EvidenceRequirement::Common).is_ok());
            assert!(
                transport
                    .validate(EvidenceRequirement::NativeCounters)
                    .is_err()
            );
        }
        assert!(
            VerificationLogTransport::NativeEvidence
                .validate(EvidenceRequirement::Common)
                .is_err()
        );
    }

    #[test]
    fn static_adapter_options_keep_core_parser_and_config_rules() {
        let adapter = StaticAdapter::default();
        let schema = compose(super::super::Args::command(), adapter.option_schemas()).unwrap();
        let matches = schema
            .clone()
            .try_get_matches_from([
                "hermit",
                "run",
                "--adapter-count=13",
                "--verify",
                "--",
                "/bin/true",
            ])
            .unwrap();
        assert_eq!(adapter.parse(&matches).unwrap(), 13);
        let args = super::super::args_from_matches_with_schema(
            &matches,
            || std::time::UNIX_EPOCH,
            &schema,
        )
        .unwrap();
        let super::super::Subcommand::Run(options) = args.command else {
            panic!("run options");
        };
        let saved = options.saved_run_config().unwrap();
        assert_eq!(saved.run["adapter-count"], serde_yaml::Value::from(13));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        super::super::run_config::write(&path, &saved).unwrap();
        let argv = super::super::run_config::expand_argv(
            &schema,
            &[
                "hermit".into(),
                "run".into(),
                "--config".into(),
                path.into_os_string(),
            ],
        )
        .unwrap()
        .unwrap();
        let replayed = schema.clone().try_get_matches_from(argv).unwrap();
        assert_eq!(adapter.parse(&replayed).unwrap(), 13);
        assert_eq!(
            schema
                .clone()
                .try_get_matches_from([
                    "hermit",
                    "run",
                    "--verify",
                    "--namespace-only",
                    "--",
                    "/bin/true",
                ])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::ArgumentConflict,
        );
        for command in [
            vec!["hermit", "--help"],
            vec!["hermit", "-h"],
            vec!["hermit", "run", "--help"],
            vec!["hermit", "run", "-h"],
        ] {
            assert_eq!(
                schema
                    .clone()
                    .try_get_matches_from(command)
                    .unwrap_err()
                    .kind(),
                clap::error::ErrorKind::DisplayHelp
            );
        }
        // The current core qualifies virtual CPU topology only for ptrace.
        // An executable-selected adapter cannot inherit that default identity.
        let mut unqualified =
            super::super::run::RunOpts::parse_from(["run", "--virtual-cpus=1", "--", "/bin/true"]);
        unqualified
            .select_application_backend(StaticAdapter::NAME)
            .unwrap();
        let global = super::super::global_opts::GlobalOpts::parse_from(["hermit-dynamorio"]);
        let error = unqualified
            .main_with_backend(&global, &adapter, 7)
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("--virtual-cpus"), "{message}");
        assert!(message.contains("dynamorio"), "{message}");
        assert!(adapter.events.borrow().is_empty());
    }

    #[test]
    fn extension_schema_refuses_core_ids_aliases_groups_and_relations() {
        for schema in [
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("verify", "new-flag")),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "verify")),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "new-flag").alias("verify")),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "new-flag").short_alias('h')),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "new-flag").requires("verify")),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "new-flag").conflicts_with("verify")),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "new-flag").requires("new_flag")),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "new-flag"))
                .group(BackendOptionGroup::new("program", ["new_flag".to_owned()])),
            OptionSchema::new(["run".to_owned()])
                .argument(BackendOption::new("new_flag", "new-flag"))
                .group(BackendOptionGroup::new("new_group", ["verify".to_owned()])),
        ] {
            assert!(compose(super::super::Args::command(), vec![schema]).is_err());
        }
    }

    // Original 207/208 declaration/admission half; private fixture shape is
    // the same real PreparedPhysical -> Prepare::container path used below.
    #[test]
    fn runtime_directory_target_avoids_declared_targets_without_mutating_options() {
        let directory = tempfile::tempdir().unwrap();
        let mut options = options(directory.path());
        for target in [
            "/tmp/.hermit-dbt-stats-0/child",
            "/tmp/.hermit-dbt-stats-1",
            "/tmp/.hermit-dbt-stats-2-other",
            "/ignored/.hermit-dbt-stats-2",
        ] {
            options
                .bind
                .push(reverie::process::Bind::new(directory.path(), target));
        }
        // Bind the real declared capabilities before freezing the admitted
        // options; ContainerScope must preserve those, including this field.
        options = options.with_adapter_capabilities(StaticAdapter::CAPABILITIES);
        let original = format!("{options:?}");
        let physical = PreparedPhysical::new(&options, StaticAdapter::CAPABILITIES);
        let global = super::super::global_opts::GlobalOpts::parse_from(["hermit"]);
        let request = Prepare {
            options: &options,
            global: &global,
            mode: Mode::VerificationPair,
            stdin: None,
            physical: Some(&physical),
        };
        let scope = request.container();
        let target = Some(PathBuf::from("/tmp/.hermit-dbt-stats-2"));
        assert_eq!(
            scope
                .runtime_directory_target(".hermit-dbt-stats-")
                .unwrap(),
            target
        );
        assert_eq!(
            scope
                .runtime_directory_target(".hermit-dbt-stats-")
                .unwrap(),
            target
        );
        assert_eq!(format!("{options:?}"), original);
        assert_eq!(format!("{:?}", scope.options), original);
    }

    #[test]
    fn runtime_directory_target_without_namespace_keeps_the_host_address() {
        let mut options =
            super::super::run::RunOpts::parse_from(["run", "--no-namespace", "--", "/bin/true"]);
        options
            .select_application_backend(StaticAdapter::NAME)
            .unwrap();
        let physical = PreparedPhysical::new(&options, StaticAdapter::CAPABILITIES);
        let global = super::super::global_opts::GlobalOpts::parse_from(["hermit"]);
        let request = Prepare {
            options: &options,
            global: &global,
            mode: Mode::Ordinary,
            stdin: None,
            physical: Some(&physical),
        };
        assert!(!request.container().uses_namespace());
        assert_eq!(
            request
                .container()
                .runtime_directory_target(".hermit-dbt-stats-")
                .unwrap(),
            None,
        );
    }

    #[test]
    fn external_container_uses_its_declared_process_signal_capability() {
        let mut options = super::super::run::RunOpts::parse_from(["run", "--", "/bin/true"]);
        options.det_opts.det_config.in_guest_reserved_signals = 17;
        options
            .det_opts
            .det_config
            .in_guest_refuses_host_timed_producers = true;
        options.det_opts.det_config.in_guest_signal_delivery = true;
        options.det_opts.det_config.guest_may_inherit_a_terminal = true;
        let builtin = options.mounts_directory_placeholder();
        assert_eq!(
            builtin,
            detcore::needs_directory_placeholder(&hermit::backend_capabilities(
                hermit::Backend::Ptrace,
            )),
        );
        for provides in [false, true] {
            let mut capabilities = hermit::backend_capabilities(hermit::Backend::Ptrace);
            capabilities.provides_process_signal_control = provides;
            let declared = options.with_adapter_capabilities(capabilities);
            assert_eq!(
                declared.mounts_directory_placeholder(),
                detcore::needs_directory_placeholder(&capabilities),
            );
            assert_eq!(options.mounts_directory_placeholder(), builtin);

            let physical = PreparedPhysical::new(&options, capabilities);
            assert_eq!(physical.capabilities, capabilities);
            assert_eq!(physical.config.backend, capabilities);
            assert_eq!(physical.config.in_guest_reserved_signals, 0);
            assert!(!physical.config.in_guest_refuses_host_timed_producers);
            assert!(!physical.config.in_guest_signal_delivery);
            let expected = hermit::prepare_backend_config_with_capabilities(
                options.effective_det_config(),
                capabilities,
            );
            assert_eq!(
                serde_json::to_value(&physical.config).unwrap(),
                serde_json::to_value(expected).unwrap(),
            );
            let request = Prepare {
                options: &options,
                global: &super::super::global_opts::GlobalOpts::parse_from(["hermit"]),
                mode: Mode::VerificationPair,
                stdin: None,
                physical: Some(&physical),
            };
            let first = request.container();
            let second = first.clone();
            for scope in [first, second] {
                assert_eq!(
                    scope.options.mounts_directory_placeholder(),
                    detcore::needs_directory_placeholder(&capabilities),
                );
                assert_eq!(
                    serde_json::to_value(scope.physical.unwrap().config).unwrap(),
                    serde_json::to_value(request.config()).unwrap(),
                );
            }
            assert_eq!(options.det_opts.det_config.in_guest_reserved_signals, 17);
            assert!(
                options
                    .det_opts
                    .det_config
                    .in_guest_refuses_host_timed_producers
            );
            assert!(options.det_opts.det_config.in_guest_signal_delivery);
        }
        let physical = PreparedPhysical::new(&options, reverie::BackendCapabilities::DBT);
        assert_eq!(physical.config.backend, reverie::BackendCapabilities::DBT);
        assert!(!physical.config.guest_may_inherit_a_terminal);
        assert!(options.det_opts.det_config.guest_may_inherit_a_terminal);
        for (requested_log, expected) in [
            (None, LevelFilter::INFO),
            (Some(LevelFilter::WARN), LevelFilter::INFO),
            (Some(LevelFilter::DEBUG), LevelFilter::DEBUG),
            (Some(LevelFilter::TRACE), LevelFilter::TRACE),
        ] {
            let capture = VerificationCapture {
                log: tempfile::tempfile().unwrap(),
                path: Path::new("unused-synthetic-log"),
                run: VerificationRun::Run1,
                options: &options,
                physical: Some(&physical),
                requested_log,
                log_budget: None,
                log_transport: VerificationLogTransport::NativeEvidence,
                forwarded_detlogs: super::super::run::ForwardedDetlogPlan::None,
            };
            assert_eq!(capture.evidence_log_level(), expected);
            assert_eq!(
                serde_json::to_value(capture.config()).unwrap(),
                serde_json::to_value(&physical.config).unwrap(),
            );
        }
    }

    #[test]
    fn external_saved_run_refuses_before_adapter_and_writer_creation() {
        let directory = tempfile::tempdir().unwrap();
        let saved = directory.path().join("saved");
        let mut options = super::super::run::RunOpts::parse_from([
            "run",
            "--verify-run",
            saved.to_str().unwrap(),
            "--",
            "/bin/true",
        ]);
        options
            .select_application_backend(StaticAdapter::NAME)
            .unwrap();
        let adapter = StaticAdapter::default();
        let result = options.main_with_backend(
            &super::super::global_opts::GlobalOpts::parse_from(["hermit-dynamorio"]),
            &adapter,
            7,
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("use live --verify instead"));
        assert!(
            !saved.exists(),
            "a refused saved run must not claim its directory"
        );
        assert!(adapter.events.borrow().is_empty());
    }

    #[test]
    fn saved_external_identity_is_exact_for_configs_and_manifests() {
        use super::super::manifest;

        let adapter = StaticAdapter::default();
        let schema = compose(super::super::Args::command(), adapter.option_schemas()).unwrap();
        let matches = schema
            .clone()
            .try_get_matches_from([
                "hermit-dynamorio",
                "run",
                "--adapter-count=13",
                "--",
                "/bin/true",
            ])
            .unwrap();
        let args = super::super::args_from_matches_with_schema(
            &matches,
            || std::time::UNIX_EPOCH,
            &schema,
        )
        .unwrap();
        let super::super::Subcommand::Run(mut options) = args.command else {
            panic!("run options");
        };
        options
            .select_application_backend(StaticAdapter::NAME)
            .unwrap();
        let saved = options.saved_run_config().unwrap();
        assert!(
            saved
                .to_yaml()
                .unwrap()
                .contains("executable-selected backend `dynamorio`")
        );
        assert_eq!(
            saved.global["backend"],
            serde_yaml::Value::from("dynamorio")
        );
        let (global, run) = super::super::run_config::to_manifest_parts(&saved).unwrap();
        assert_eq!(global["backend"], "dynamorio");
        let document = manifest::Manifest::new(
            hermit::Id::unique(),
            manifest::Kind::History,
            manifest::Command {
                program: saved.program.clone(),
                args: saved.args.clone(),
                cwd: None,
            },
            manifest::Inputs {
                global,
                run,
                host_drawn: BTreeMap::new(),
            },
        );
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.yaml");
        let manifest = directory.path().join("manifest.json");
        super::super::run_config::write(&config, &saved).unwrap();
        manifest::write(&manifest, &document).unwrap();
        for (flag, path) in [("--config", config), ("--from", manifest)] {
            let argv = [
                "hermit-dynamorio".into(),
                "run".into(),
                flag.into(),
                path.into_os_string(),
            ];
            let expanded = super::super::run_config::expand_argv_for_application(
                &schema,
                &argv,
                Some(StaticAdapter::NAME),
            )
            .unwrap()
            .unwrap();
            let replayed = schema.clone().try_get_matches_from(expanded).unwrap();
            assert_eq!(adapter.parse(&replayed).unwrap(), 13);
            let replayed = super::super::args_from_matches_with_schema(
                &replayed,
                || std::time::UNIX_EPOCH,
                &schema,
            )
            .unwrap();
            assert!(replayed.global.backend.is_none());
            assert!(
                super::super::run_config::expand_argv_for_application(
                    &schema,
                    &argv,
                    Some("different-adapter"),
                )
                .is_err()
            );
            let core = super::super::run_config::expand_argv(&schema, &argv);
            assert!(
                match core {
                    Err(_) => true,
                    Ok(Some(argv)) => schema.clone().try_get_matches_from(argv).is_err(),
                    Ok(None) => false,
                },
                "ordinary Hermit must refuse an external identity, never default to ptrace"
            );
        }

        // The implicit builtin default is intentionally absent from ordinary
        // saved files. An extension must not reinterpret that omission.
        let implicit =
            super::super::run_config::parse("schema: hermit-run-config/v1\nprogram: /bin/true\n")
                .unwrap();
        let implicit_config = directory.path().join("implicit-config.yaml");
        let implicit_manifest = directory.path().join("implicit-manifest.json");
        super::super::run_config::write(&implicit_config, &implicit).unwrap();
        let (global, run) = super::super::run_config::to_manifest_parts(&implicit).unwrap();
        let document = manifest::Manifest::new(
            hermit::Id::unique(),
            manifest::Kind::History,
            manifest::Command {
                program: implicit.program.clone(),
                args: implicit.args.clone(),
                cwd: None,
            },
            manifest::Inputs {
                global,
                run,
                host_drawn: BTreeMap::new(),
            },
        );
        manifest::write(&implicit_manifest, &document).unwrap();
        let builtin_schema = super::super::Args::command();
        for (flag, path) in [("--config", implicit_config), ("--from", implicit_manifest)] {
            let argv = [
                "hermit-dynamorio".into(),
                "run".into(),
                flag.into(),
                path.into_os_string(),
            ];
            let error = super::super::run_config::expand_argv_for_application(
                &schema,
                &argv,
                Some(StaticAdapter::NAME),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("no saved backend identity"));

            let expanded = super::super::run_config::expand_argv(&builtin_schema, &argv)
                .unwrap()
                .unwrap();
            let matches = builtin_schema
                .clone()
                .try_get_matches_from(expanded)
                .unwrap();
            let builtin = super::super::args_from_matches_with_schema(
                &matches,
                || std::time::UNIX_EPOCH,
                &builtin_schema,
            )
            .unwrap();
            assert!(builtin.global.backend.is_none());
        }
    }

    #[test]
    fn executable_identity_is_neutral_and_modes_must_match() {
        let directory = tempfile::tempdir().unwrap();
        let options = options(directory.path());
        assert_eq!(options.execution_backend_name(), "dynamorio");
        for name in ["ptrace", "kvm", "in-guest-trap"] {
            assert!(
                validate_name(name).is_err(),
                "reserved builtin identity {name}"
            );
        }
        assert!(
            Execution::<()>::Finished(ExitStatus::Exited(0))
                .verification()
                .is_err()
        );
        assert!(
            Execution::<()>::Finished(ExitStatus::Exited(0))
                .finished(Mode::VerificationPair)
                .is_err()
        );
        let adapter = StaticAdapter::default();
        let mut session = Session {
            mode: Mode::Ordinary,
            count: 7,
        };
        let error = adapter
            .execute(
                &mut session,
                Execute {
                    mode: Mode::VerificationPair,
                    verification: None,
                },
            )
            .err()
            .expect("operation mismatch must fail before physical execution");
        assert!(error.to_string().contains("mode changed"));
        assert!(adapter.events.borrow().is_empty());
    }

    #[test]
    fn common_owner_preserves_latch_exit_and_deferred_finish_precedence() {
        for (loss, exit, expected, events) in [
            (
                true,
                0,
                "synthetic latch",
                vec!["prepare:VerificationPair", "execute:Run1"],
            ),
            (
                false,
                23,
                "First run during --verify",
                vec!["prepare:VerificationPair", "execute:Run1"],
            ),
            (
                false,
                0,
                "synthetic deferred facts failure",
                vec!["prepare:VerificationPair", "execute:Run1", "finish:Run1"],
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let adapter = StaticAdapter {
                first_loss: loss,
                first_exit: exit,
                fail_finish: Some(VerificationRun::Run1),
                ..Default::default()
            };
            let error = run(&adapter, directory.path()).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert_eq!(*adapter.events.borrow(), events);
            let report = report(directory.path());
            assert_eq!(report["verdict"], "no_result");
            assert_eq!(report["verified"], false);
        }
    }

    #[test]
    fn missing_declared_evidence_never_reaches_a_verdict() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = StaticAdapter {
            omit_evidence: true,
            ..Default::default()
        };
        let error = run(&adapter, directory.path()).unwrap_err();
        assert!(format!("{error:#}").contains("omitted or changed its predeclared"));
        assert_eq!(
            *adapter.events.borrow(),
            [
                "prepare:VerificationPair",
                "execute:Run1",
                "finish:Run1",
                "execute:Run2",
                "finish:Run2",
                "pair"
            ]
        );
        let report = report(directory.path());
        assert_eq!(report["verdict"], "no_result");
        assert_eq!(report["verified"], false);
        assert_eq!(report["no_result_reason"]["kind"], "run_failed");
        assert_eq!(report["no_result_reason"]["failure"], "backend_failure");
        assert_eq!(report["no_result_reason"]["run"], "run2");
    }

    #[test]
    fn every_native_dimension_supplements_the_same_common_comparison() {
        for dimension in [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)] {
            let directory = tempfile::tempdir().unwrap();
            let adapter = StaticAdapter {
                changed_dimension: dimension,
                ..Default::default()
            };
            let status = run(&adapter, directory.path()).unwrap();
            assert_eq!(
                status.success(),
                dimension.is_none(),
                "dimension {dimension:?}"
            );
            let report = report(directory.path());
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
    }
}
