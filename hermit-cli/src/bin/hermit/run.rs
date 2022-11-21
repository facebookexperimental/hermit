/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs;
use std::fs::File;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use ::tracing::metadata::LevelFilter;
use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use colored::Colorize;
use detcore::BlockingMode;
use detcore::SchedHeuristic;
use detcore_model::config::DEFAULT_EPOCH_STR;
use hermit::Context;
use hermit::DetConfig;
use hermit::Error;
use lazy_static::lazy_static;
use rand::Rng;
use reverie::process::Bind;
use reverie::process::Command;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Mount;
use reverie::process::Namespace;
use reverie::process::Output;

use super::container::default_container;
use super::container::with_container;
use super::global_opts::GlobalOpts;
use super::tracing::init_file_tracing;
use super::verify::compare_two_runs;
use super::verify::temp_log_files;

const TMP_DIR: &str = "/tmp";

// Just a place to put the clap(flatten) directive..
#[derive(Debug, Parser, Clone)]
pub(crate) struct DetOptions {
    /// detcore configuration
    #[clap(flatten)]
    pub det_config: DetConfig,
}

/// Command-line options for the "run" subcommand.
#[derive(Debug, Parser, Clone)]
pub struct RunOpts {
    /// Program to run.
    #[clap(value_name = "PROGRAM")]
    program: PathBuf,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    #[clap(flatten)]
    pub(crate) det_opts: DetOptions,

    /// Enables strict mode. Currently this implies default mode plus
    /// --sequentialize-threads, and --deterministic-io.
    // #[clap(long, short)]
    // strict: bool,

    // Disables sequentialize threads. On by default
    #[clap(long)]
    pub(crate) no_sequentialize_threads: bool,

    // Disables deterministic io. On by default
    #[clap(long)]
    no_deterministic_io: bool,

    /// Pin all guest threads to one or more cores, so that they do not migrate
    /// during execution. This is off by default, but it is implied by setting
    /// `preemption_timeout` which requires stable RCB counters. RCB counters are
    /// not maintained consistently when Linux migrates a thread between cores.
    #[clap(long)]
    pin_threads: bool,

    /// Mount a file or directory. This uses the same syntax as the Docker
    /// `--mount` option. For simple bind-mount cases, use `bind` instead.
    #[clap(long, value_name = "path")]
    mount: Vec<Mount>,

    /// Bind-mounts the provided path to the same path inside of the container if
    /// it is not already available.
    #[clap(long, value_name = "path")]
    pub(crate) bind: Vec<Bind>,

    /// Disables external networking using a network namespace.
    #[clap(long, alias = "no-net", alias = "disable-networking")]
    no_networking: bool,

    /// Runs the given program in "lite" mode. In this mode, a PID namespace is
    /// created and `/tmp` is isolated. It is still possible to introduce
    /// non-determinism through time and thread scheduling. Can be combined with
    /// `--no-networking` to also disable networking.
    #[clap(long, conflicts_with = "chaos", conflicts_with = "verify")]
    lite: bool,

    /// Specifies the directory to use as `/tmp`. This path gets bind-mounted
    /// over `/tmp` and the guest program does not see the real `/tmp` directory.
    /// If this path does not exist, it is created.
    ///
    /// If this option is not specified, a temporary directory is created,
    /// mounted over `/tmp`, and deleted when the guest has exited.
    #[clap(long, value_name = "dirpath")]
    tmp: Option<PathBuf>,

    /// Exactly like "seed" but we generate a seed for you. This is useful if multiple
    /// hermit runs execute in parallel and rand based collisions exist.  "Args" generates
    /// the seed from the other arguments passed to hermit, "SystemRandom" uses system
    /// randomness to generate a seed, and creates a log message recording it.
    #[clap(long, value_name = "'Args'|'SystemRandom'")]
    seed_from: Option<SeedFrom>,

    /// After running, immediately run a SECOND time, and compare the two
    /// executions. This will exit with an error if the guest process does OR if
    /// the executions do not match. In order to match, they must have the same
    /// observed output (e.g. stdout/stderr), and the same log of internal
    /// scheduler steps.
    ///
    /// It's on the user to ensure that the command run is idempotent, and thus
    /// that the first run will not have any side effects that affect the
    /// execution of the second run.
    #[clap(long)]
    verify: bool,

    /// If --verify is specified, indicates what guest exit status is required for
    /// hermit to consider the verification successful.  Both runs must satisfy this criteria,
    /// and hermit does not perform the second run if the first does not.
    #[clap(long, value_name = "success|failure|both", default_value = "success")]
    verify_allow: VerifyAllow,

    /// Print a summary of the process tree's execution to stderr before exiting.
    #[clap(long, short = 'u')]
    pub(crate) summary: bool,

    /// Containarize networking and warn for non-zero bindings. Implies
    /// `--no-networking`.
    #[clap(long)]
    analyze_networking: bool,

    /// The base environment that is presented to the guest. "Empty" is completely empty, and "Host"
    /// allows through all the environment variables in hermit's own environment.
    /// "Minimal" provides a minimal deterministic environment, setting only PATH, HOSTNAME, and HOME.
    #[clap(long, default_value = "host", value_name = "str", possible_values = &["empty", "minimal", "host"])]
    base_env: BaseEnv,

    /// Additionally append one or more environment variables to the container environment. If a
    /// name is provided without a value, pass that variable through from the host.
    #[clap(short = 'e', long, parse(try_from_str = parse_assignment), value_name="name[=val]")]
    env: Vec<(String, Option<String>)>,

    /// An option to set current directory for the guest process.
    /// Note that the directory is relative to the guest. i.e. all mounted directories will be respected (e.g /tmp)
    #[clap(long, value_name = "path")]
    workdir: Option<String>,

    /// For debugging, save the details of this final run config: printed to a file in a human
    /// readable format.
    #[clap(long, value_name = "path")]
    pub save_config: Option<PathBuf>,
}

fn parse_assignment(src: &str) -> Result<(String, Option<String>), Error> {
    lazy_static! {
        static ref ENV_RE: regex::Regex =
           // Here we are extremely permissive, allowing all charecters in the "Portable Character
           // Set", ISO/IEC 6429:1992 standard:
           regex::Regex::new("^([\x07-<>-~]+)=([\x07-~]*)$").unwrap();
        static ref VAR_RE: regex::Regex =
           regex::Regex::new("^([\x07-<>-~]+)$").unwrap();
    }
    if let Some(capture) = ENV_RE.captures(src) {
        if let (Some(name), Some(value)) = (capture.get(1), capture.get(2)) {
            Ok((name.as_str().to_owned(), Some(value.as_str().to_owned())))
        } else {
            anyhow::bail!("unable to parse name=value from '{}'", src)
        }
    } else if VAR_RE.is_match(src) {
        let var: String = src.to_owned();
        Ok((var, None))
    } else {
        anyhow::bail!("unable to parse env var name or name=value from '{}'", src)
    }
}

#[derive(Debug, Clone, Copy, Parser, Eq, PartialEq)]
pub enum VerifyAllow {
    Success,
    Failure,
    Both,
}

impl FromStr for VerifyAllow {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "success" => Ok(VerifyAllow::Success),
            "failure" => Ok(VerifyAllow::Failure),
            "both" => Ok(VerifyAllow::Both),
            _ => Err(format!("Could not parse: {:?}", s)),
        }
    }
}

impl VerifyAllow {
    fn satisfies(&self, status: ExitStatus) -> bool {
        match self {
            VerifyAllow::Success => status == ExitStatus::SUCCESS,
            VerifyAllow::Failure => status != ExitStatus::SUCCESS,
            VerifyAllow::Both => true,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum BaseEnv {
    Empty,
    Minimal,
    Host,
}

impl FromStr for BaseEnv {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "empty" => Ok(BaseEnv::Empty),
            "minimal" => Ok(BaseEnv::Minimal),
            "host" => Ok(BaseEnv::Host),
            _ => Err(format!(
                "Expected Empty | Minimal | Host, could not parse: {:?}",
                s
            )),
        }
    }
}

/// Where to generate the random seed from.
#[derive(Debug, Clone)]
pub enum SeedFrom {
    Args,
    SystemRandom,
}

// Error boilerplate.
#[derive(Debug, Clone)]
pub struct ParseSeedFromError {
    details: String,
}

impl fmt::Display for ParseSeedFromError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl std::error::Error for ParseSeedFromError {
    fn description(&self) -> &str {
        &self.details
    }
}

impl FromStr for SeedFrom {
    type Err = ParseSeedFromError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "args" => Ok(SeedFrom::Args),
            "systemrandom" => Ok(SeedFrom::SystemRandom),
            _ => Err(ParseSeedFromError {
                details: format!("Expected Args | SystemRandom, could not parse: {:?}", s),
            }),
        }
    }
}

/// Displays as a string which needs only to be prepended with "hermit " to be a runnable command.
impl fmt::Display for RunOpts {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dop = &self.det_opts.det_config;

        if self.no_sequentialize_threads {
            write!(f, " --no-sequentialize-threads")?;
        }
        if self.no_deterministic_io {
            write!(f, " --no-deterministic-io")?;
            assert!(!dop.deterministic_io)
        } else {
            assert!(dop.deterministic_io)
        }
        if self.no_networking {
            write!(f, " --no-networking")?;
        }
        if self.lite {
            write!(f, " --lite")?;
        }
        if self.summary {
            write!(f, " --summary")?;
        }
        if self.analyze_networking {
            write!(f, " --analyze-networking")?;
        }
        if self.verify {
            write!(f, " --verify")?;
        }
        if let Some(p) = &self.tmp {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --tmp={}", shell_words::quote(s))?;
        }
        match &self.verify_allow {
            VerifyAllow::Success => {} // default
            VerifyAllow::Failure => {
                write!(f, " --verify-allow=failure")?;
            }
            VerifyAllow::Both => {
                write!(f, " --verify-allow=both")?;
            }
        }
        match &self.base_env {
            BaseEnv::Empty => {
                write!(f, " --base-env=empty")?;
            }
            BaseEnv::Minimal => {
                write!(f, " --base-env=minimal")?;
            }
            BaseEnv::Host => {} // default
        }
        for (key, m_val) in &self.env {
            if let Some(val) = m_val {
                write!(f, " --env={}={}", key, shell_words::quote(val))?;
            } else {
                write!(f, " --env={}", key)?;
            }
        }
        if let Some(p) = &self.workdir {
            write!(f, " --workdir={}", shell_words::quote(p))?;
        }
        if let Some(p) = &self.save_config {
            let s = p.to_str().expect("valid string provided to --save-config");
            write!(f, " --save-config={}", shell_words::quote(s))?;
        }

        for mount in &self.mount {
            let mut acc = Vec::new();
            if let Some(s) = &mount.get_source() {
                acc.push(format!("source={}", s.display()));
            }
            acc.push(format!("target={}", mount.get_target().display()));
            write!(f, "--mount={}", shell_words::quote(&acc.join(",")),)?;
        }
        for bind in &self.bind {
            let src = bind.source.to_str().expect("valid unicode bind source");
            let tar = bind.target.to_str().expect("valid unicode target");
            if bind.source == bind.target {
                write!(f, " --bind={}", shell_words::quote(src))?;
            } else {
                write!(
                    f,
                    " --bind={}:{}",
                    shell_words::quote(src),
                    shell_words::quote(tar)
                )?;
            }
        }

        if !dop.virtualize_time {
            write!(f, " --no-virtualize-time")?;
        }
        if !dop.virtualize_cpuid {
            write!(f, " --no-virtualize-cpuid")?;
        }
        if !dop.virtualize_metadata {
            write!(f, " --no-virtualize-metadata")?;
        }
        let default_epoch: DateTime<Utc> = DEFAULT_EPOCH_STR.parse::<DateTime<Utc>>().unwrap();
        if dop.epoch != default_epoch {
            write!(f, " --epoch={}", dop.epoch.to_rfc3339())?;
        }
        if dop.seed != 0 {
            write!(f, " --seed={}", dop.seed)?;
        }
        if let Some(m) = dop.clock_multiplier {
            write!(f, " --clock-multiplier={}", m)?;
        }
        if dop.imprecise_timers {
            write!(f, " --imprecise-timers")?;
        }
        if dop.chaos {
            write!(f, " --chaos")?;
        }
        if dop.record_preemptions {
            write!(f, " --record-preemptions")?;
        }
        if let Some(p) = &dop.record_preemptions_to {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --record-preemptions-to={}", shell_words::quote(s))?;
        }
        if let Some(p) = &dop.replay_preemptions_from {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --replay-preemptions-from={}", shell_words::quote(s))?;
        }
        if let Some(p) = &dop.replay_schedule_from {
            let s = p.to_str().expect("valid unicode path");
            write!(f, " --replay-schedule-from={}", shell_words::quote(s))?;
        }
        if dop.replay_exhausted_panic {
            write!(f, " --replay-exhausted-panic")?;
        }
        if dop.die_on_desync {
            write!(f, " --die-on-desync")?;
        }
        for (index, path) in &dop.stacktrace_event {
            write!(f, " --stacktrace-event={}", index)?;
            if let Some(p) = path {
                let s = p.to_str().expect("valid unicode path");
                write!(f, ",{}", shell_words::quote(s))?;
            }
        }
        if dop.preemption_stacktrace {
            write!(f, " --preemption-stacktrace")?;
        }
        if dop.panic_on_unsupported_syscalls {
            write!(f, " --panic-on-unsupported-syscalls")?;
        }
        if dop.kill_daemons {
            write!(f, " --kill-daemons")?;
        }
        if dop.gdbserver {
            write!(f, " --gdbserver")?;
        }
        if dop.gdbserver_port != /* default */ 1234u16 {
            write!(f, " --gdbserver-port={}", dop.gdbserver_port)?;
        }
        match &dop.preemption_timeout {
            Some(x) => {
                if *x != NonZeroU64::new(200_000_000).unwrap() {
                    write!(f, " --preemption-timeout={}", x)?;
                }
            }
            None => {
                write!(f, " --preemption-timeout=disabled")?;
            }
        }
        if dop.sigint_instakill {
            write!(f, " --sigint-instakill")?;
        }
        if dop.warn_non_zero_binds {
            write!(f, " --warn-non-zero-binds")?;
        }
        match &dop.sched_heuristic {
            SchedHeuristic::None => {}
            SchedHeuristic::ConnectBind => {
                write!(f, " --sched-heuristic=connectbind")?;
            }
            SchedHeuristic::Random => {
                write!(f, " --sched-heuristic=random")?;
            }
            SchedHeuristic::StickyRandom => {
                write!(f, " --sched-heuristic=stickyrandom")?;
            }
        }
        if let Some(s) = dop.sched_seed {
            write!(f, " --sched-seed={}", s)?;
        }
        if dop.sched_sticky_random_param != 0.0 {
            write!(
                f,
                " --sched-sticky-random-param={}",
                dop.sched_sticky_random_param
            )?;
        }
        if let Some(t) = dop.stop_after_turn {
            write!(f, " --stop-after-turn={}", t)?;
        }
        if let Some(i) = dop.stop_after_iter {
            write!(f, " --stop-after-iter={}", i)?;
        }
        if dop.debug_externalize_sockets {
            write!(f, " --debug-externalize-sockets")?;
        }
        match &dop.debug_futex_mode {
            BlockingMode::External => {
                write!(f, " --debug-futex-mode=external")?;
            }
            BlockingMode::Polling => {
                write!(f, " --debug-futex-mode=polling")?;
            }
            BlockingMode::Precise => { /* default */ }
        }
        if dop.no_rcb_time {
            write!(f, " --no-rcb-time")?;
        }
        if dop.detlog_heap {
            write!(f, " --detlog-heap")?;
        }
        if dop.detlog_stack {
            write!(f, " --detlog-stack")?;
        }
        if dop.sysinfo_uptime_offset != /* default */ 120 {
            write!(f, " --sysinfo-uptime-offset={}", dop.sysinfo_uptime_offset)?;
        }
        if dop.memory != 1_000_000_000 {
            write!(f, " --memory={}", dop.memory)?;
        }
        for (tid, rcb) in &dop.interrupt_at {
            write!(f, " --interrupt-at={}:{}", tid, rcb)?;
        }

        write!(
            f,
            " -- {}",
            shell_words::quote(self.program.to_str().expect("valid unicode path"))
        )?;
        if !self.args.is_empty() {
            write!(f, " {}", shell_words::join(&self.args))?;
        }
        Ok(())
    }
}

#[test]
fn display_runopts1() {
    let vec: Vec<&str> = vec!["fakehermit", "fakeprog", "arg1", "arg2"];
    let mut ro = RunOpts::from_iter(vec.iter());
    ro.validate_args();
    assert_eq!(format!("{}", ro), " -- fakeprog arg1 arg2");
}

#[test]
fn display_runopts2() {
    let vec: Vec<&str> = vec![
        "fakehermit",
        "--sequentialize-threads",
        "fakeprog",
        "arg1",
        "arg2",
    ];
    let mut ro = RunOpts::from_iter(vec.iter());
    ro.validate_args();
    assert_eq!(format!("{}", ro), " -- fakeprog arg1 arg2");
}

#[test]
fn display_runopts3() {
    let vec: Vec<&str> = vec![
        "fakehermit",
        "--no-sequentialize-threads",
        "--no-virtualize-metadata",
        "--epoch=2000-12-31T23:59:59+00:00",
        "fakeprog",
        "arg1",
        "arg2",
    ];
    let mut ro = RunOpts::from_iter(vec.iter());
    ro.validate_args();
    assert_eq!(
        format!("{}", ro),
        " --no-sequentialize-threads --no-virtualize-metadata --epoch=2000-12-31T23:59:59+00:00 -- fakeprog arg1 arg2"
    );
}

#[test]
fn display_runopts4() {
    let vec: Vec<&str> = vec!["fakehermit", "--sequentialize-threads", "fakeprog", "arg1"];
    let mut ro = RunOpts::from_iter(vec.iter());
    ro.validate_args();
    assert_eq!(format!("{}", ro), " -- fakeprog arg1");
}

/// Create two logging destinations and two global configs. Returns non-zero exit
/// status if there was a difference in any component of the output.
impl RunOpts {
    pub fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Set up an early tracing option before we're ready to set the global default:

        // TODO(T124429978): temporarily disabling this because it inexplicably clobbers our
        // subsequent tracing_subscriber::fmt::init() call.
        // tracing::subscriber::with_default(super::tracing::stderr_subscriber(global.log), || {
        self.validate_args();
        // });

        if self.lite {
            self.run_lite(global)
        } else if self.verify {
            self.verify(global)
        } else {
            self.run(global)
        }
    }

    /// Some arguments imply others. This is the place where that validation occurs.
    pub fn validate_args(&mut self) {
        let config = &mut self.det_opts.det_config;

        config.has_uts_namespace = true;

        if self.analyze_networking {
            config.warn_non_zero_binds = true;
        }

        config.sequentialize_threads = !self.no_sequentialize_threads;
        config.deterministic_io = !self.no_deterministic_io;

        // virtualize_metadata implies virtualize_time
        if config.virtualize_metadata && !config.virtualize_time {
            panic!(
                "virtualize-metadata can only be activated if virtualize-time is as well.  Conversely, --no-virtualize-time requires --no-virtualize-metadata."
            );
        }

        // Perform internal validation on the Config args, before taking into account the
        // hermit run args:
        config.validate();

        // This is a Detcore Config-internal matter, but relies on reverie_ptrace, which detcore is
        // allowed to depend on:
        if config.preemption_timeout.is_some() && !reverie_ptrace::is_perf_supported() {
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            eprintln!(
                "WARNING: --preemption-timout requires hardware perf counters \
                which is not supported on this host, resetting \
                preemption-timeout to 0"
            );
            config.preemption_timeout = None;
        }

        if let Some(sf) = &self.seed_from {
            let seed = match sf {
                SeedFrom::Args => {
                    let mut hasher = DefaultHasher::new();
                    self.args.hash(&mut hasher);
                    self.program.hash(&mut hasher);
                    hasher.finish()
                }
                SeedFrom::SystemRandom => {
                    let mut rng = rand::thread_rng();
                    let seed: u64 = rng.gen();
                    seed
                }
            };
            // TODO(T124429978): this could change back to tracing::warn! when the bug is fixed:
            eprintln!(
                "[hermit] auto setting --seed {0:?} --sched-seed {0:?}",
                seed
            );
            config.seed = seed;
        }

        // Deterministic RCB counts requires thread pinning.  But this only matters if
        // we're expecting full determinstic execution (sequentialize_threads).
        if config.preemption_timeout.is_some() && config.sequentialize_threads {
            self.pin_threads = true;
        }
    }

    fn tmpfs(&self) -> Result<Tmpfs, Error> {
        match self.tmp.as_ref() {
            Some(path) => {
                let path = path.as_path();
                fs::create_dir_all(path)?;
                Ok(Tmpfs::Path(path))
            }
            None => Ok(Tmpfs::Temp(tempfile::TempDir::new()?)),
        }
    }

    pub fn run(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let tmpfs = self.tmpfs()?;

        let mut container = self.container(tmpfs.path())?;

        with_container(&mut container, || self.run_in_container(global))
    }

    fn run_lite(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // TODO: Make this use detcore instead after detcore is capable of being
        // "lightweight".
        let _guard = global.init_tracing();

        let tmpfs = self.tmpfs()?;

        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .unshare(Namespace::PID)
            .map_root()
            .hostname("hermetic-container.local")
            .domainname("local")
            .mount(Mount::proc())
            .mounts(self.mounts(tmpfs.path())?);

        if self.no_networking {
            command.local_networking_only();
        }

        let mut child = command.spawn()?;

        let exit_status = child.wait_blocking()?;

        Ok(exit_status)
    }

    // Execution mode corresponding to `run --verify`:
    fn verify(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let (log1, log2) =
            temp_log_files("run1", "run2").context("Failed to create temporary log files")?;

        let (log1_file, log1_path) = log1.into_parts();
        let (log2_file, log2_path) = log2.into_parts();

        eprintln!(":: {}", "Run1...".yellow().bold());

        let out1: Output = self.run_verify(log1_file, global)?;
        if !self.verify_allow.satisfies(out1.status) {
            eprintln!(
                "First run errored during --verify, not continuing to a second. Stdout:\n{}\nStderr:\n{}",
                String::from_utf8_lossy(&out1.stdout),
                String::from_utf8_lossy(&out1.stderr),
            );
            return Err(Error::msg("First run during --verify exited in error"));
        }

        eprintln!(":: {}", "Run2...".yellow().bold());
        let out2 = self.run_verify(log2_file, global)?;

        compare_two_runs(
            &out1,
            log1_path,
            &out2,
            log2_path,
            "Success: deterministic.",
            "Failure: nondeterministic.",
        )
    }

    /// Returns the mounts to be used with the container.
    fn mounts(&self, tmpfs: &Path) -> Result<Vec<Mount>, Error> {
        let mut mounts = Vec::new();

        for mount in &self.mount {
            if let Ok(path) = mount.get_target().strip_prefix(TMP_DIR) {
                // If the target is in /tmp, change it so it goes to our
                // temporary /tmp instead.
                mounts.push(mount.clone().target(tmpfs.join(path)).touch_target());
            } else {
                mounts.push(mount.clone());
            }
        }

        for bind in &self.bind {
            let mount = Mount::from(bind.clone()).rshared();

            // Bind mounts currently only make sense for things in `/tmp` since
            // that is the only directory we overlay.
            if let Ok(relative_path) = mount.get_target().strip_prefix(TMP_DIR) {
                let target = tmpfs.join(relative_path);
                mounts.push(mount.target(target).touch_target());
            } else {
                tracing::warn!(
                    "The path {:?} is not in {}, --bind currently has no effect",
                    bind,
                    TMP_DIR
                );
            }
        }

        // Bind the /tmp/tmpXXXXXX tmpfs mount over /tmp to hide it. This way,
        // we still preserve the files or directories bind-mounted inside of it
        // while hiding the real /tmp.
        mounts.push(Mount::bind(tmpfs, TMP_DIR).rshared());

        Ok(mounts)
    }

    /// Returns a configured container to run a function in.
    fn container(&self, tmpfs: &Path) -> Result<Container, Error> {
        let mut container = default_container(self.pin_threads);

        if self.no_networking || self.analyze_networking {
            container.local_networking_only();
        }

        container.mounts(self.mounts(tmpfs)?);

        Ok(container)
    }

    pub fn run_verify(&self, log_file: fs::File, global: &GlobalOpts) -> Result<Output, Error> {
        // TODO: Get this working with `--tmp`? Each run could use a separate
        // subdirectory. Only preserve the temporary directory if verify failed?
        let tmpfs = tempfile::TempDir::new()?;

        let mut container = self.container(tmpfs.path())?;

        let mut log_file = Some(log_file);
        with_container(&mut container, || {
            self.run_verify_in_container(&mut log_file, global)
        })
    }

    fn merge_from_env_settings(&self, command: &mut Command) -> anyhow::Result<()> {
        for (var, m_val) in &self.env {
            if let Some(val) = m_val {
                command.env(var, val);
            } else if let Ok(value) = std::env::var(var) {
                command.env(var, &value);
            } else {
                anyhow::bail!(
                    "Attempt to pass through env var {}, but it is not set in the host environment",
                    var
                )
            }
        }
        Ok(())
    }

    fn save_config_to_disk(&self) -> Result<(), Error> {
        if let Some(path) = &self.save_config {
            let mut file = File::create(path)?;
            file.write_all(format!("{:#?}\n", self).as_bytes())?;
        }
        Ok(())
    }

    fn run_in_container(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();

        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(current_dir) = &self.workdir {
            command.current_dir(current_dir);
        }
        match self.base_env {
            BaseEnv::Empty => {
                command.env_clear();
                self.merge_from_env_settings(&mut command)?
            }
            BaseEnv::Minimal => {
                command.env_clear();
                command.env("HOSTNAME", "hermetic-container.local");
                command.env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
                command.env("HOME", "/root");
                self.merge_from_env_settings(&mut command)?
            }
            BaseEnv::Host => {
                // Let it all through.
                self.merge_from_env_settings(&mut command)?
            }
        }

        let config = self.det_opts.det_config.clone();
        self.save_config_to_disk()?;

        hermit::run(command, config, self.summary)
    }

    fn run_verify_in_container(
        &self,
        log_file: &mut Option<fs::File>,
        global: &GlobalOpts,
    ) -> Result<Output, Error> {
        // HACK: Use interior mutability to workaround not being able to pass
        // `log_file` by value. Guaranteed by caller to never panic.
        let log_file = log_file.take().unwrap();

        // Ensure at least a minimum DEBUG level.
        let level = if let Some(requested) = global.log {
            requested
        } else {
            LevelFilter::DEBUG
        };

        let _guard = init_file_tracing(Some(level), log_file);

        let mut command = Command::new(&self.program);
        command.args(&self.args);

        if let Some(current_dir) = &self.workdir {
            command.current_dir(current_dir);
        }

        let config = self.det_opts.det_config.clone();
        self.save_config_to_disk()?;

        hermit::run_with_output(command, config, self.summary)
    }
}

/// Represents a tmpfs location. There are different ways to construct `/tmp` for
/// the container and this encapsulates all of them.
enum Tmpfs<'a> {
    /// Use an existing path as `/tmp`.
    Path(&'a Path),

    /// Use a new temporary directory as `/tmp`.
    Temp(tempfile::TempDir),
}

impl<'a> Tmpfs<'a> {
    /// Returns the path to `/tmp`.
    pub fn path(&self) -> &Path {
        match self {
            Self::Path(path) => path,
            Self::Temp(temp) => temp.path(),
        }
    }
}
