// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use ::tracing::metadata::LevelFilter;
use clap::Parser;
use colored::Colorize;
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

#[derive(Debug, Parser)]
pub(crate) struct DetOptions {
    /// detcore configuration
    #[clap(flatten)]
    pub det_config: DetConfig,
}

/// Command-line options for the "run" subcommand.
#[derive(Debug, Parser)]
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
    no_sequentialize_threads: bool,

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
    #[clap(
        long,
        conflicts_with = "chaos",
        conflicts_with = "verify"
        // conflicts_with = "strict"
    )]
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
    #[clap(long, value_name = "Args|SystemRandom")]
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

    /// Additionally append one or more environment variables to the container environment.
    #[clap(short, long, parse(try_from_str = parse_assignment))]
    env: Vec<(String, String)>,

    /// An option to set current directory for the guest process.
    /// Note that the directory is relative to the guest. i.e. all mounted directories will be respected (e.g /tmp)
    #[clap(long)]
    workdir: Option<String>,
}

fn parse_assignment(src: &str) -> Result<(String, String), Error> {
    lazy_static! {
        static ref ENV_RE: regex::Regex =
            regex::Regex::new("^([\x07-<>-~]+)=([\x07-~]*)$").unwrap();
    }
    if let Some(capture) = ENV_RE.captures(src) {
        if let (Some(name), Some(value)) = (capture.get(1), capture.get(2)) {
            return Ok((name.as_str().to_owned(), value.as_str().to_owned()));
        }
    }
    anyhow::bail!("unable to parse name=value from '{}'", src)
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

        // if self.strict {
        config.sequentialize_threads = !self.no_sequentialize_threads;
        config.deterministic_io = !self.no_deterministic_io;
        config.virtualize_time = true;
        config.virtualize_metadata = true;
        config.virtualize_cpuid = true;
        // }

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

    fn merge_from_env_settings(&self, command: &mut Command) {
        for assignment in &self.env {
            command.env(&assignment.0, &assignment.1);
        }
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
                self.merge_from_env_settings(&mut command)
            }
            BaseEnv::Minimal => {
                command.env_clear();
                command.env("HOSTNAME", "hermetic-container.local");
                command.env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
                command.env("HOME", "/root");
                self.merge_from_env_settings(&mut command)
            }
            BaseEnv::Host => {
                // Let it all through.
                self.merge_from_env_settings(&mut command)
            }
        }

        let config = self.det_opts.det_config.clone();

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
