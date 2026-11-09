#!/usr/bin/env -S rust-script --force
//! Build a CLI with measured core compatibility from its actual Cargo workspace.
//! ```cargo
//! [dependencies]
//! anyhow = "1"
//! clap = { version = "4", features = ["derive"] }
//! libc = "0.2"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! sha2 = "0.10"
//! sha1_smol = "1"
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use clap::Parser;
use sha2::Digest;
use sha2::Sha256;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "../hermit-cli/compatibility_graph.rs"]
mod compatibility_graph;

#[path = "../hermit-cli/compatibility_rustc.rs"]
mod compatibility_rustc;

#[path = "../hermit-cli/compatibility_source.rs"]
mod compatibility_source;

#[path = "compatibility-rustc-wrapper.rs"]
mod compatibility_rustc_wrapper;

use compatibility_graph::BuildInput;
use compatibility_graph::Graph;
use compatibility_graph::INPUT_ENV;
use compatibility_graph::INPUT_SCHEMA;
use compatibility_graph::InputFile;
use compatibility_rustc::CAPTURE_ENV;
use compatibility_rustc::GATE_ENV;

const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const STAMP_ENV: &str = "HERMIT_STAMP_COMPATIBILITY";

/// This is a build producer, never a Hermit invocation or validation receipt.
/// The caller retains the normal compiler allocation and resource envelope.
#[derive(Debug, Parser)]
struct Args {
    /// Actual top-level workspace Cargo.toml, including for extension builds.
    #[arg(long)]
    manifest_path: PathBuf,
    /// Cargo package containing the executable to build.
    #[arg(long)]
    package: String,
    #[arg(long)]
    bin: String,
    /// Explicit Rust target; do not infer it from a different build workspace.
    #[arg(long)]
    target: String,
    #[arg(long, default_value = "release")]
    profile: String,
    #[arg(long, conflicts_with = "all_features")]
    features: Option<String>,
    #[arg(long, conflicts_with = "features")]
    all_features: bool,
    #[arg(long)]
    no_default_features: bool,
    #[arg(long)]
    offline: bool,
    /// New evidence directory; existing directories are never overwritten.
    #[arg(long)]
    output: PathBuf,
    /// The caller's one explicit target path. It must initially be absent.
    /// Cargo creates it; a bounded normal Cargo clean separates populations.
    #[arg(long)]
    target_dir: PathBuf,
}

fn exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn measure(path: &Path) -> Result<InputFile> {
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(InputFile {
        path,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    })
}

fn group_exists(pid: u32) -> Result<bool> {
    // This is only the fresh producer-owned process group, never a live
    // validation service or an identity inferred from an old numeric PID.
    let result = unsafe { libc::kill(-(pid as i32), 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error.into())
    }
}

fn signal_group(pid: u32, signal: i32) -> Result<()> {
    if unsafe { libc::kill(-(pid as i32), signal) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

trait ReadPipe: Read + AsRawFd {}
impl<T: Read + AsRawFd> ReadPipe for T {}

struct Stream {
    pipe: Option<Box<dyn ReadPipe>>,
    output: File,
    bytes: usize,
}

impl Stream {
    fn new(pipe: Box<dyn ReadPipe>, output: File) -> Result<Self> {
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0
                && unsafe {
                    libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                } == 0,
            "cannot make the private producer pipe nonblocking"
        );
        Ok(Self {
            pipe: Some(pipe),
            output,
            bytes: 0,
        })
    }

    /// At most256KiB per observation, so a continuously writing child cannot
    /// keep this reader from observing the unchanged command deadline.
    fn drain(&mut self) -> Result<bool> {
        let Some(pipe) = self.pipe.as_mut() else {
            return Ok(false);
        };
        let mut buffer = [0u8; 8192];
        for _ in 0..32 {
            let remaining = OUTPUT_LIMIT - self.bytes;
            let take = buffer.len().min(remaining + 1);
            match pipe.read(&mut buffer[..take]) {
                Ok(0) => {
                    self.pipe.take();
                    return Ok(false);
                }
                Ok(count) => {
                    let kept = count.min(remaining);
                    self.output.write_all(&buffer[..kept])?;
                    self.bytes += kept;
                    if count > remaining {
                        return Ok(true);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }
}

/// Private nonblocking pipes bound both output and observation time. There
/// are no reader threads to join after a child or descendant holds a pipe.
fn capture(command: &mut Command, directory: &Path, name: &str, seconds: u64) -> Result<PathBuf> {
    let stdout_path = directory.join(format!("{name}.stdout.jsonl"));
    let stderr_path = directory.join(format!("{name}.stderr.log"));
    let stdout_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&stdout_path)?;
    let stderr_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&stderr_path)?;
    let argv = format!("{command:?}");
    let started = Instant::now();
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            // No Child handle was returned. Retain that fact instead of
            // inventing an exit code or successful cleanup for a missing PID.
            exclusive(
                &directory.join(format!("{name}.receipt.json")),
                &serde_json::to_vec_pretty(&serde_json::json!({
                    "command":argv,"wall_limit_seconds":seconds,"spawn_returned_child":false,
                    "spawn_error":error.to_string(),"exit_code":null,"pid":null,"pgid":null,
                    "remaining_group":null,"cleanup_signals":[],
                }))?,
            )?;
            return Err(error).with_context(|| format!("cannot start {name}"));
        }
    };
    let pid = child.id();
    let mut status = None;
    let mut reason: Option<&str> = None;
    let mut capture_errors = Vec::new();
    let mut signals = Vec::new();
    let mut streams = match (|| -> Result<[Stream; 2]> {
        Ok([
            Stream::new(
                Box::new(child.stdout.take().context("missing private stdout")?),
                stdout_file,
            )?,
            Stream::new(
                Box::new(child.stderr.take().context("missing private stderr")?),
                stderr_file,
            )?,
        ])
    })() {
        Ok(streams) => Some(streams),
        Err(error) => {
            capture_errors.push(format!("pipe setup: {error:#}"));
            reason = Some("capture-error");
            None
        }
    };
    loop {
        if let Some(streams) = &mut streams {
            for stream in streams {
                match stream.drain() {
                    Ok(true) => {
                        reason.get_or_insert("output-limit");
                    }
                    Ok(false) => {}
                    Err(error) => {
                        capture_errors.push(format!("pipe capture: {error:#}"));
                        reason.get_or_insert("capture-error");
                    }
                }
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => {
                    capture_errors.push(format!("child status: {error}"));
                    reason.get_or_insert("status-error");
                }
            }
        }
        let ended = streams
            .as_ref()
            .is_some_and(|streams| streams.iter().all(|stream| stream.pipe.is_none()));
        if status.is_some() && ended {
            break;
        }
        if started.elapsed() >= Duration::from_secs(seconds) {
            reason.get_or_insert("timeout");
        }
        if reason.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let present = match group_exists(pid) {
        Ok(present) => present,
        Err(error) => {
            capture_errors.push(format!("group status: {error:#}"));
            reason.get_or_insert("group-status-error");
            true
        }
    };
    if reason.is_some() || present {
        reason.get_or_insert("remaining-owned-group");
        if let Err(error) = signal_group(pid, libc::SIGTERM) {
            capture_errors.push(format!("TERM: {error:#}"));
        }
        signals.push("SIGTERM");
        let grace = Instant::now();
        loop {
            if status.is_none() {
                match child.try_wait() {
                    Ok(observed) => status = observed,
                    Err(error) => capture_errors.push(format!("cleanup status: {error}")),
                }
            }
            match group_exists(pid) {
                Ok(false) => break,
                Err(error) => {
                    capture_errors.push(format!("cleanup group status: {error:#}"));
                    break;
                }
                Ok(true) => {}
            }
            if grace.elapsed() >= Duration::from_secs(30) {
                break;
            }
            if let Some(streams) = &mut streams {
                for stream in streams {
                    if let Err(error) = stream.drain() {
                        capture_errors.push(format!("cleanup capture: {error:#}"));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if group_exists(pid).unwrap_or(true) {
            if let Err(error) = signal_group(pid, libc::SIGKILL) {
                capture_errors.push(format!("KILL: {error:#}"));
            }
            signals.push("SIGKILL");
        }
        // This final reaping/absence observation is bounded too. Missing
        // closure stays a failed envelope; there is no blocking wait/join.
        let reap = Instant::now();
        while reap.elapsed() < Duration::from_secs(5) {
            if status.is_none() {
                match child.try_wait() {
                    Ok(observed) => status = observed,
                    Err(error) => {
                        capture_errors.push(format!("reap: {error}"));
                        break;
                    }
                }
            }
            if status.is_some() && !group_exists(pid).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if let Some(streams) = &mut streams {
        for stream in streams {
            if let Err(error) = stream
                .drain()
                .and_then(|_| stream.output.sync_all().map_err(Into::into))
            {
                capture_errors.push(format!("final capture: {error:#}"));
            }
        }
    }
    let remaining = group_exists(pid).unwrap_or(true);
    let captured = streams.as_ref().map_or(0, |streams| {
        streams
            .iter()
            .filter(|stream| stream.pipe.is_none())
            .count()
    });
    let receipt = serde_json::json!({
        "command":argv,"pid":pid,"pgid":pid,"wall_limit_seconds":seconds,
        "termination_grace_seconds":30,"final_reap_observation_seconds":5,"pipe_limit_bytes":OUTPUT_LIMIT,
        "elapsed_seconds":started.elapsed().as_secs_f64(),"status":status.as_ref().map(ToString::to_string),
        "exit_code":status.as_ref().and_then(std::process::ExitStatus::code),"envelope_failure":reason,
        "capture_errors":capture_errors,"captured_streams":captured,"remaining_group":remaining,"cleanup_signals":signals,
    });
    exclusive(
        &directory.join(format!("{name}.receipt.json")),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    ensure!(
        reason.is_none()
            && capture_errors.is_empty()
            && !remaining
            && captured == 2
            && status.is_some_and(|status| status.success()),
        "{name} failed; retained child/envelope status is separate in its receipt"
    );
    Ok(stdout_path)
}

fn cargo(args: &Args, manifest: &Path, operation: &str) -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(manifest.parent().expect("absolute manifest has parent"))
        .arg(operation)
        .arg("--locked")
        .arg("--manifest-path")
        .arg(manifest);
    if args.offline {
        command.arg("--offline");
    }
    if operation != "clean" {
        if let Some(features) = &args.features {
            command.args(["--features", features]);
        }
        if args.all_features {
            command.arg("--all-features");
        }
        if args.no_default_features {
            command.arg("--no-default-features");
        }
    }
    command
        .env_remove(STAMP_ENV)
        .env_remove(INPUT_ENV)
        .env_remove(CAPTURE_ENV)
        .env_remove(GATE_ENV);
    command
}

fn build(args: &Args, manifest: &Path, target: &Path, config: &Path) -> Result<Command> {
    let mut command = cargo(args, manifest, "build");
    command
        .args([
            "--package",
            &args.package,
            "--bin",
            &args.bin,
            "--target",
            &args.target,
            "--profile",
            &args.profile,
            "--message-format",
            "json-render-diagnostics",
        ])
        .arg("--target-dir")
        .arg(target)
        .env("RUSTC_WRAPPER", std::env::current_exe()?)
        .env(CAPTURE_ENV, config);
    Ok(command)
}

fn captured(config: &Path, output: &Path) -> Result<InputFile> {
    let bundle = compatibility_rustc_wrapper::bundle(config)?;
    let bytes = serde_json::to_vec(&bundle)?;
    ensure!(
        bytes.len() <= compatibility_rustc::MAX_CAPTURE_BYTES,
        "actual rustc bundle exceeds its bound"
    );
    exclusive(output, &bytes)?;
    let file = measure(output)?;
    compatibility_rustc::Bundle::read(&file).map_err(anyhow::Error::msg)?;
    Ok(file)
}

fn stamp_from_messages(path: &Path, package: &str) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)?;
    let mut stamps = Vec::new();
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line)?;
        if value["reason"] != "build-script-executed" || value["package_id"] != package {
            continue;
        }
        for pair in value["env"]
            .as_array()
            .context("build script has no environment output")?
        {
            if pair[0] == "HERMIT_CORE_COMPATIBILITY" {
                let embedded: serde_json::Value =
                    serde_json::from_str(pair[1].as_str().context("stamp is not text")?)?;
                ensure!(
                    embedded["availability"] == "measured",
                    "final build has no measured core stamp"
                );
                stamps.push(embedded["stamp"].clone());
            }
        }
    }
    ensure!(
        stamps.len() == 1,
        "final artifact build must report exactly one measured core build script"
    );
    Ok(stamps.pop().expect("one measured stamp"))
}

#[derive(Default)]
struct Progress {
    directory: Option<PathBuf>,
    stage: &'static str,
}

fn run(args: Args, progress: &mut Progress) -> Result<()> {
    // Silently replacing an existing wrapper would alter the build policy.
    // Composition is unsupported until its exact ordering can be measured.
    for name in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        CAPTURE_ENV,
        GATE_ENV,
        INPUT_ENV,
        STAMP_ENV,
    ] {
        ensure!(
            std::env::var_os(name).is_none_or(|value| value.is_empty()),
            "compatibility producer cannot compose inherited {name}; use a supported clean producer context"
        );
    }
    let manifest = args.manifest_path.canonicalize()?;
    ensure!(
        manifest
            .file_name()
            .is_some_and(|name| name == "Cargo.toml"),
        "--manifest-path must name Cargo.toml"
    );
    ensure!(
        args.output.is_absolute(),
        "--output must be an absolute new directory"
    );
    ensure!(
        !args.output.exists(),
        "--output already exists; use a new evidence directory"
    );
    ensure!(
        args.target_dir.is_absolute(),
        "--target-dir must be an explicit absolute path"
    );
    let target_parent = args
        .target_dir
        .parent()
        .context("target path has no parent")?
        .canonicalize()?;
    let target_name = args
        .target_dir
        .file_name()
        .context("target path has no basename")?;
    ensure!(
        target_parent.join(target_name) == args.target_dir,
        "target parent must resolve exactly, without symlink or dot-path indirection"
    );
    ensure!(
        !std::env::current_exe()?.starts_with(&args.target_dir),
        "the producer executable must be materialized outside the target that normal Cargo clean will remove"
    );
    match std::fs::symlink_metadata(&args.target_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect the explicit target path"),
        Ok(_) => bail!(
            "--target-dir already exists; clean the caller-owned target normally before starting this producer"
        ),
    }
    std::fs::create_dir(&args.output)?;
    progress.directory = Some(args.output.clone());
    progress.stage = "measuring-top-level-inputs";
    let lock_path = manifest
        .parent()
        .context("manifest has no parent")?
        .join("Cargo.lock");
    let manifest_before = measure(&manifest)?;
    let lock_before = measure(&lock_path)?;

    let mut metadata = cargo(&args, &manifest, "metadata");
    metadata.args(["--format-version", "1", "--filter-platform", &args.target]);
    progress.stage = "locked-metadata";
    let metadata_path = capture(&mut metadata, &args.output, "locked-metadata", 120)?;
    let metadata_file = measure(&metadata_path)?;
    let source_snapshot =
        compatibility_source::snapshot(&metadata_file, false).map_err(anyhow::Error::msg)?;
    let source_path = args.output.join("source-before-builds.json");
    exclusive(&source_path, &serde_json::to_vec(&source_snapshot)?)?;
    // One caller-owned target path is reused. Its initial absence and a
    // successful normal Cargo clean between populations require both builds
    // to compile from an empty target. No target/artifact copies are made.
    let target = &args.target_dir;
    let first_config =
        compatibility_rustc_wrapper::prepare(&args.output.join("first-rustc"), target)?;
    progress.stage = "first-unstamped-artifact";
    let mut first_build = build(&args, &manifest, target, &first_config)?;
    let first_path = capture(
        &mut first_build,
        &args.output,
        "first-unstamped-artifact",
        900,
    )?;
    let first_rustc = captured(&first_config, &args.output.join("first-actual-rustc.json"))?;
    ensure!(
        compatibility_source::snapshot(&metadata_file, false).map_err(anyhow::Error::msg)?
            == source_snapshot,
        "complete core/Reverie source changed during the first build"
    );
    // Cargo created this root. Refuse symlinks or an invalid/missing standard
    // cache tag; this producer never manufactures a tag or forces deletion.
    use std::os::unix::fs::MetadataExt;
    let target_metadata = std::fs::symlink_metadata(target)?;
    ensure!(
        target_metadata.is_dir() && target_metadata.uid() == unsafe { libc::geteuid() },
        "producer target is not its real owned cache directory"
    );
    let tag = target.join("CACHEDIR.TAG");
    ensure!(
        std::fs::symlink_metadata(&tag)?.is_file(),
        "Cargo target tag is not a real file"
    );
    let mut tag_bytes = Vec::new();
    File::open(&tag)?.take(4097).read_to_end(&mut tag_bytes)?;
    ensure!(
        tag_bytes.len() <= 4096
            && tag_bytes.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55\n"),
        "Cargo target has no valid standard cache tag; normal cleanup is required"
    );
    progress.stage = "normal-between-population-clean";
    let mut clean = cargo(&args, &manifest, "clean");
    clean.arg("--target-dir").arg(target);
    capture(
        &mut clean,
        &args.output,
        "normal-between-population-clean",
        120,
    )?;
    ensure!(
        matches!(std::fs::symlink_metadata(target), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
        "normal Cargo clean did not remove the sole producer target"
    );
    let second_config =
        compatibility_rustc_wrapper::prepare(&args.output.join("second-rustc"), target)?;
    progress.stage = "second-unstamped-artifact";
    let mut second_build = build(&args, &manifest, target, &second_config)?;
    let second_path = capture(
        &mut second_build,
        &args.output,
        "second-unstamped-artifact",
        900,
    )?;
    let second_rustc = captured(
        &second_config,
        &args.output.join("second-actual-rustc.json"),
    )?;
    ensure!(
        compatibility_source::snapshot(&metadata_file, false).map_err(anyhow::Error::msg)?
            == source_snapshot,
        "complete core/Reverie source changed during the second build"
    );
    manifest_before
        .read(OUTPUT_LIMIT)
        .map_err(anyhow::Error::msg)?;
    lock_before.read(OUTPUT_LIMIT).map_err(anyhow::Error::msg)?;
    let input = BuildInput {
        schema: INPUT_SCHEMA.to_owned(),
        metadata: metadata_file,
        first_artifacts: measure(&first_path)?,
        artifacts: measure(&second_path)?,
        rustc_first: first_rustc,
        rustc_second: second_rustc,
        source_snapshot: measure(&source_path)?,
        manifest: manifest_before,
        lock: lock_before,
        package: args.package.clone(),
        binary: args.bin.clone(),
        target: args.target.clone(),
        profile: args.profile.clone(),
    };
    let mut first_input = input.clone();
    first_input.artifacts = input.first_artifacts.clone();
    let first = Graph::read(&first_input).map_err(anyhow::Error::msg)?;
    let second = Graph::read(&input).map_err(anyhow::Error::msg)?;
    ensure!(
        first.units == second.units,
        "the two fresh builds selected different Cargo units/features"
    );
    for graph in [&first, &second] {
        graph.core_library_unit().map_err(anyhow::Error::msg)?;
    }
    let first_bundle =
        compatibility_rustc::Bundle::read(&input.rustc_first).map_err(anyhow::Error::msg)?;
    let second_bundle =
        compatibility_rustc::Bundle::read(&input.rustc_second).map_err(anyhow::Error::msg)?;
    let rustc_cohort = compatibility_rustc::cohort(&first, &first_bundle, &second, &second_bundle)
        .map_err(anyhow::Error::msg)?;
    let core = second.named("hermit-run").map_err(anyhow::Error::msg)?;
    let input_path = args.output.join("actual-build-input.json");
    exclusive(&input_path, &serde_json::to_vec_pretty(&input)?)?;
    // A stamp is requested only after both complete actual populations match.
    // The final compiler wrapper checks every recompiled shared unit against
    // that population before invoking rustc. Unchanged dependencies stay in
    // the second, already measured population at the same sole target path;
    // no foreign target is warm seeded.
    progress.stage = "measured-final-artifact";
    let final_config =
        compatibility_rustc_wrapper::prepare(&args.output.join("final-rustc"), target)?;
    let mut final_build = build(&args, &manifest, target, &final_config)?;
    final_build
        .env(STAMP_ENV, "1")
        .env(INPUT_ENV, &input_path)
        .env(GATE_ENV, &input_path);
    let final_path = capture(
        &mut final_build,
        &args.output,
        "measured-final-artifact",
        900,
    )?;
    let final_rustc = captured(&final_config, &args.output.join("final-actual-rustc.json"))?;
    ensure!(
        compatibility_source::snapshot(&input.metadata, false).map_err(anyhow::Error::msg)?
            == source_snapshot,
        "complete core/Reverie source changed during the final gated build"
    );
    let mut final_graph = Graph::read(&input).map_err(anyhow::Error::msg)?;
    final_graph
        .read_artifacts(&std::fs::read(&final_path)?)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        second.units == final_graph.units,
        "final Cargo units/features/profiles differ from the measured build input"
    );
    let final_bundle =
        compatibility_rustc::Bundle::read(&final_rustc).map_err(anyhow::Error::msg)?;
    let expected =
        compatibility_rustc::shared(&second, &second_bundle).map_err(anyhow::Error::msg)?;
    for captured in &final_bundle.commands {
        if let Some(actual) = compatibility_rustc::for_invocation_with_reference(
            &final_graph,
            &final_bundle,
            &second_bundle,
            &captured.invocation,
        )
        .map_err(anyhow::Error::msg)?
        {
            ensure!(
                expected.contains_key(&serde_json::to_string(&actual)?),
                "final actual shared-unit command differs from the two measured populations"
            );
        }
    }
    let expected_auxiliary =
        compatibility_rustc::auxiliary(&second, &second_bundle).map_err(anyhow::Error::msg)?;
    let final_auxiliary =
        compatibility_rustc::auxiliary_with_reference(&final_graph, &final_bundle, &second_bundle)
            .map_err(anyhow::Error::msg)?;
    for (actual, count) in final_auxiliary {
        ensure!(
            expected_auxiliary
                .get(&actual)
                .is_some_and(|expected| count <= *expected),
            "final auxiliary compiler population is not a subset of the actual cold population"
        );
    }
    let primary = final_graph
        .named(&args.package)
        .map_err(anyhow::Error::msg)?;
    let executable = final_graph
        .executables
        .get(&(primary.id.clone(), args.bin.clone()))
        .context("final build has no selected executable artifact")?;
    let stamp = stamp_from_messages(&final_path, &core.id)?;
    let artifact = measure(executable)?;
    let mut inputs = BTreeMap::new();
    for (name, file) in [
        ("manifest", &input.manifest),
        ("lock", &input.lock),
        ("metadata", &input.metadata),
        ("first_units", &input.first_artifacts),
        ("source_snapshot", &input.source_snapshot),
        ("second_units", &input.artifacts),
        ("first_rustc", &input.rustc_first),
        ("second_rustc", &input.rustc_second),
        ("final_rustc", &final_rustc),
    ] {
        file.read(OUTPUT_LIMIT).map_err(anyhow::Error::msg)?;
        inputs.insert(name, file);
    }
    progress.stage = "publishing-measured-artifact";
    exclusive(
        &args.output.join("completed-build.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "schema":"hermit-compatible-cli-build/v1", "input_schema":INPUT_SCHEMA,
            "package":args.package,"binary":args.bin,"target":args.target,"profile":args.profile,
            "sole_target_directory":target,
            "inputs":inputs,"artifact":artifact,"core_stamp":stamp,
            "actual_shared_rustc":rustc_cohort,
            "runtime_qualification":false,"canonical_publisher_attestation":false,
        }))?,
    )?;
    progress.stage = "completed-build";
    println!("measured CLI artifact: {}", executable.display());
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    if let Some(code) = compatibility_rustc_wrapper::maybe_run() {
        return code;
    }
    let mut progress = Progress::default();
    match run(Args::parse(), &mut progress) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Write only inside the new directory this invocation created.
            // A refusal never invents a completed artifact, child status,
            // runtime qualification or successful target cleanup.
            if let Some(directory) = &progress.directory {
                let receipt = serde_json::json!({
                    "schema":"hermit-compatible-cli-refusal/v1","stage":progress.stage,
                    "producer_exit_code":2,"error":format!("{error:#}"),
                    "completed_build_record_present":directory.join("completed-build.json").exists(),
                    "runtime_qualification":false,"canonical_publisher_attestation":false,
                    "target_cleanup":"Only the recorded normal between-population Cargo clean; no error-path deletion",
                });
                if let Err(secondary) = serde_json::to_vec_pretty(&receipt)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| exclusive(&directory.join("failed-build.json"), &bytes))
                {
                    eprintln!("cannot retain compatibility refusal receipt: {secondary:#}");
                }
            }
            eprintln!("compatible CLI build refused: {error:#}");
            ExitCode::from(2)
        }
    }
}
