/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
use std::env;
use std::fs;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use criterion::Criterion;
use criterion::SamplingMode;
use criterion::Throughput;
use tempfile::TempDir;

const ALL_BACKENDS: [Backend; 7] = [
    Backend {
        name: "native",
        kind: BackendKind::Native,
    },
    Backend {
        name: "gvisor-systrap",
        kind: BackendKind::Gvisor("systrap"),
    },
    Backend {
        name: "gvisor-kvm",
        kind: BackendKind::Gvisor("kvm"),
    },
    Backend {
        name: "reverie-ptrace",
        kind: BackendKind::ReveriePtrace,
    },
    Backend {
        name: "reverie-dbi",
        kind: BackendKind::ReverieDbi,
    },
    Backend {
        name: "reverie-kvm",
        kind: BackendKind::ReverieKvm,
    },
    Backend {
        name: "reverie-sabre",
        kind: BackendKind::ReverieSabre,
    },
];

const ALL_OPERATIONS: [Operation; 4] = [
    Operation::Getpid,
    Operation::Read,
    Operation::Write,
    Operation::ClockGettime,
];

#[derive(Clone, Copy)]
struct Backend {
    name: &'static str,
    kind: BackendKind,
}

#[derive(Clone, Copy)]
enum BackendKind {
    Native,
    Gvisor(&'static str),
    ReveriePtrace,
    ReverieDbi,
    ReverieKvm,
    ReverieSabre,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Operation {
    Getpid,
    Read,
    Write,
    ClockGettime,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Getpid => "getpid",
            Self::Read => "read-devnull",
            Self::Write => "write-devnull",
            Self::ClockGettime => "clock-gettime",
        }
    }

    fn protocol_name(self) -> &'static str {
        match self {
            Self::Getpid => "getpid",
            Self::Read => "read",
            Self::Write => "write",
            Self::ClockGettime => "clock_gettime",
        }
    }
}

trait ReadWithFd: Read + AsRawFd {}
impl<T: Read + AsRawFd> ReadWithFd for T {}

struct GuestProcess {
    backend: &'static str,
    child: Child,
    stdin: BufWriter<Box<dyn Write>>,
    stdout: BufReader<Box<dyn ReadWithFd>>,
    timeout: Duration,
    stderr_path: PathBuf,
    _runsc_root: Option<TempDir>,
    _control_root: Option<TempDir>,
}

impl GuestProcess {
    fn start(backend: Backend, paths: &Paths, timeout: Duration) -> Result<Self> {
        let control_root = backend
            .uses_socket()
            .then(|| {
                tempfile::Builder::new()
                    .prefix("syscall-control-")
                    .tempdir()
            })
            .transpose()?;
        let socket_path = control_root
            .as_ref()
            .map(|root| root.path().join("rpc.sock"));
        let listener = socket_path
            .as_ref()
            .map(UnixListener::bind)
            .transpose()
            .context("binding helper control socket")?;
        let (mut command, runsc_root) = backend.command(paths, socket_path.as_deref())?;
        let stderr_path = paths
            .log_dir
            .join(format!("{}.log", backend.name.replace('/', "_")));
        let stderr = File::create(&stderr_path)
            .with_context(|| format!("creating {}", stderr_path.display()))?;
        if listener.is_some() {
            command.stdin(Stdio::null()).stdout(Stdio::null());
        } else {
            command.stdin(Stdio::piped()).stdout(Stdio::piped());
        }
        command
            .stderr(Stdio::from(stderr))
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("RUST_LOG", "off")
            .env("TZ", "UTC");

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning {}", backend.name))?;
        let connection: Result<(Box<dyn Write>, Box<dyn ReadWithFd>)> = (|| {
            if let Some(listener) = listener {
                wait_readable(listener.as_raw_fd(), timeout).with_context(|| {
                    format!("waiting for {} helper socket connection", backend.name)
                })?;
                let (stream, _) = listener.accept()?;
                let reader = stream.try_clone()?;
                Ok((
                    Box::new(stream) as Box<dyn Write>,
                    Box::new(reader) as Box<dyn ReadWithFd>,
                ))
            } else {
                let stdin: ChildStdin = child.stdin.take().context("child stdin was not piped")?;
                let stdout: ChildStdout =
                    child.stdout.take().context("child stdout was not piped")?;
                Ok((
                    Box::new(stdin) as Box<dyn Write>,
                    Box::new(stdout) as Box<dyn ReadWithFd>,
                ))
            }
        })();
        let (stdin, stdout) = match connection {
            Ok(connection) => connection,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let detail = fs::read_to_string(&stderr_path)
                    .unwrap_or_else(|read_error| format!("unable to read log: {read_error}"));
                bail!("{error:#}; stderr: {}", detail.trim());
            }
        };
        let mut process = Self {
            backend: backend.name,
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            timeout,
            stderr_path,
            _runsc_root: runsc_root,
            _control_root: control_root,
        };

        match process.read_line() {
            Ok(line) if line.trim() == "READY syscall-server-v1" => Ok(process),
            Ok(line) => {
                let detail = process.stderr_detail();
                bail!(
                    "{} returned unexpected handshake {:?}; stderr: {}",
                    backend.name,
                    line.trim(),
                    detail
                )
            }
            Err(error) => {
                let detail = process.stderr_detail();
                bail!(
                    "{} failed before handshake: {error:#}; stderr: {}",
                    backend.name,
                    detail
                )
            }
        }
    }

    fn run_batch(&mut self, operation: Operation, iterations: u64) -> Result<Duration> {
        let started = Instant::now();
        writeln!(self.stdin, "{} {iterations}", operation.protocol_name())?;
        self.stdin.flush()?;
        let response = self.read_line()?;
        let elapsed = started.elapsed();
        self.validate_response(operation, iterations, &response)?;
        Ok(elapsed)
    }

    fn validate_response(
        &self,
        operation: Operation,
        iterations: u64,
        response: &str,
    ) -> Result<()> {
        let fields: Vec<&str> = response.split_whitespace().collect();
        if fields.first() == Some(&"ERR") {
            bail!("{} helper error: {}", self.backend, response.trim());
        }
        if fields.len() != 4
            || fields[0] != "OK"
            || fields[1] != operation.protocol_name()
            || fields[2] != iterations.to_string()
            || fields[3].parse::<u64>().is_err()
        {
            bail!(
                "{} returned malformed response for {} x {iterations}: {:?}",
                self.backend,
                operation.name(),
                response.trim()
            );
        }
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        wait_readable(self.stdout.get_ref().as_raw_fd(), self.timeout)
            .with_context(|| format!("waiting for {} helper output", self.backend))?;
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line)?;
        if bytes == 0 {
            let status = self.child.try_wait()?;
            bail!(
                "{} closed stdout (status {status:?}); stderr: {}",
                self.backend,
                self.stderr_detail()
            );
        }
        Ok(line)
    }

    fn stderr_detail(&self) -> String {
        fs::read_to_string(&self.stderr_path)
            .unwrap_or_else(|error| format!("unable to read log: {error}"))
            .trim()
            .chars()
            .take(2_000)
            .collect()
    }
}

impl Drop for GuestProcess {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit 0");
        let _ = self.stdin.flush();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Paths {
    helper: PathBuf,
    runsc: PathBuf,
    counter2: PathBuf,
    drrun: PathBuf,
    dbi_client: PathBuf,
    kvm_counter: PathBuf,
    riptrace: PathBuf,
    riptrace_plugin: PathBuf,
    sabre: PathBuf,
    repository: PathBuf,
    log_dir: PathBuf,
}

impl Paths {
    fn discover() -> Result<Self> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repository = manifest
            .join("../..")
            .canonicalize()
            .context("locating Hermit repository root")?;
        let development_root = repository
            .parent()
            .and_then(Path::parent)
            .context("locating dev-hermit root")?;
        let reverie_slot = development_root.join("worktrees_reverie/slot111");
        let criterion_home = env::var_os("CRITERION_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| manifest.join("target/criterion"));
        let log_dir = criterion_home.join("backend-logs");
        fs::create_dir_all(&log_dir)?;

        Ok(Self {
            helper: PathBuf::from(env!("SYSCALL_BENCH_HELPER")),
            runsc: env_path("RUNSC_BIN").unwrap_or_else(|| {
                development_root.join("experiments/gvisor/bazel-bin/runsc/runsc_/runsc")
            }),
            counter2: env_path("COUNTER2")
                .unwrap_or_else(|| reverie_slot.join("target/debug/counter2")),
            drrun: env_path("DRRUN").unwrap_or_else(|| {
                reverie_slot.join(
                    "target/debug/build/reverie-dbi-7dc355fded6b3407/out/\
                     dynamorio-install/bin64/drrun",
                )
            }),
            dbi_client: env_path("DBI_CLIENT").unwrap_or_else(|| {
                reverie_slot.join("target/debug/reverie-dbi-native/libreverie_dbi_client.so")
            }),
            kvm_counter: env_path("KVM_COUNTER")
                .unwrap_or_else(|| reverie_slot.join("target/debug/reverie-kvm-counter")),
            riptrace: env_path("RIPTRACE")
                .unwrap_or_else(|| reverie_slot.join("target/debug/riptrace")),
            riptrace_plugin: env_path("RIPTRACE_PLUGIN")
                .unwrap_or_else(|| reverie_slot.join("target/debug/libriptrace_plugin.so")),
            sabre: env_path("SABRE").unwrap_or_else(|| reverie_slot.join("target/sabre/sabre")),
            repository,
            log_dir,
        })
    }
}

impl Backend {
    fn uses_socket(self) -> bool {
        matches!(
            self.kind,
            BackendKind::ReverieDbi | BackendKind::ReverieSabre
        )
    }

    fn uses_one_shot_regression(self) -> bool {
        matches!(self.kind, BackendKind::ReverieKvm)
    }

    fn command(self, paths: &Paths, socket: Option<&Path>) -> Result<(Command, Option<TempDir>)> {
        match self.kind {
            BackendKind::Native => {
                require_executable(&paths.helper, "syscall helper")?;
                let mut command = Command::new(&paths.helper);
                command.arg("--server");
                Ok((command, None))
            }
            BackendKind::Gvisor(platform) => {
                require_executable(&paths.runsc, "gVisor runsc binary")?;
                require_executable(&paths.helper, "syscall helper")?;
                let root = tempfile::Builder::new()
                    .prefix(&format!("runsc-{platform}-"))
                    .tempdir()?;
                // SAFETY: these calls only read the credentials of this process.
                let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
                let mut command = Command::new(&paths.runsc);
                command
                    .arg(format!("--root={}", root.path().display()))
                    .arg(format!("--platform={platform}"))
                    .arg("--network=none")
                    .arg("--ignore-cgroups=true")
                    .arg("do")
                    .arg("--quiet")
                    .arg(format!("--uid-map=0 {uid} 1"))
                    .arg(format!("--gid-map=0 {gid} 1"))
                    .arg(format!("--cwd={}", paths.repository.display()))
                    .arg(&paths.helper)
                    .arg("--server");
                Ok((command, Some(root)))
            }
            BackendKind::ReveriePtrace => {
                require_executable(&paths.counter2, "Reverie counter2")?;
                let mut command = Command::new(&paths.counter2);
                command.arg("--");
                append_helper(&mut command, paths, socket);
                Ok((command, None))
            }
            BackendKind::ReverieDbi => {
                require_executable(&paths.drrun, "DynamoRIO drrun")?;
                require_file(&paths.dbi_client, "Reverie DBI counter client")?;
                let mut command = Command::new(&paths.drrun);
                command
                    .args(["-quiet", "-disable_rseq", "-stack_size", "2M", "-c"])
                    .arg(&paths.dbi_client)
                    .arg("-summary")
                    .arg("--");
                append_helper(&mut command, paths, socket);
                Ok((command, None))
            }
            BackendKind::ReverieKvm => {
                require_executable(&paths.kvm_counter, "Reverie KVM counter")?;
                let mut command = Command::new(&paths.kvm_counter);
                command.arg("--");
                append_helper(&mut command, paths, socket);
                Ok((command, None))
            }
            BackendKind::ReverieSabre => {
                require_executable(&paths.riptrace, "Reverie SaBRe runner")?;
                require_executable(&paths.sabre, "SaBRe loader")?;
                require_file(&paths.riptrace_plugin, "Reverie SaBRe plugin")?;
                let mut command = Command::new(&paths.riptrace);
                command
                    .arg("--sabre")
                    .arg(&paths.sabre)
                    .arg("--plugin")
                    .arg(&paths.riptrace_plugin)
                    .args(["--quiet", "--summary", "--"]);
                append_helper(&mut command, paths, socket);
                Ok((command, None))
            }
        }
    }
}

fn append_helper(command: &mut Command, paths: &Paths, socket: Option<&Path>) {
    command.arg(&paths.helper);
    if let Some(socket) = socket {
        command.arg("--socket").arg(socket);
    } else {
        command.arg("--server");
    }
}

fn run_one_shot(
    backend: Backend,
    paths: &Paths,
    operation: Operation,
    iterations: u64,
) -> Result<Duration> {
    if !backend.uses_one_shot_regression() {
        bail!("{} is not configured for one-shot regression", backend.name);
    }
    require_executable(&paths.kvm_counter, "Reverie KVM counter")?;
    require_executable(&paths.helper, "syscall helper")?;
    let mut command = Command::new(&paths.kvm_counter);
    command
        .arg("--")
        .arg(&paths.helper)
        .arg("--run")
        .arg(operation.protocol_name())
        .arg(iterations.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("RUST_LOG", "off")
        .env("TZ", "UTC");
    let started = Instant::now();
    let output = command.output()?;
    let elapsed = started.elapsed();
    if !output.status.success() {
        bail!(
            "{} one-shot {} x {iterations} exited {:?}: {}",
            backend.name,
            operation.name(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(elapsed)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("criterion syscall benchmark failed: {error:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    pin_current_process()?;
    let paths = Paths::discover()?;
    let backends = selected_backends()?;
    let operations = selected_operations()?;
    let timeout = env_duration("SYSCALL_BENCH_TIMEOUT_SECS", 120.0)?;
    let one_shot_floor = env_usize("SYSCALL_BENCH_ONESHOT_MIN_CALLS", 1_000, 1)? as u64;
    let one_shot_scale = env_usize("SYSCALL_BENCH_ONESHOT_SCALE", 1_000, 1)? as u64;
    let fixed_counts = fixed_counts()?;
    let require_all = env_bool("SYSCALL_BENCH_REQUIRE_ALL", false)?;
    let capability_path = capability_path();
    if let Some(parent) = capability_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut capabilities = BufWriter::new(File::create(&capability_path)?);
    writeln!(capabilities, "backend\tsyscall\tstatus\tdetail")?;
    let fixed_path = capability_path.with_file_name("fixed-counts.tsv");
    let mut fixed_output = BufWriter::new(File::create(&fixed_path)?);
    writeln!(
        fixed_output,
        "backend\tsyscall\tcount\telapsed_ns\tns_per_syscall"
    )?;

    eprintln!("syscall helper: {}", paths.helper.display());
    eprintln!("runsc: {}", paths.runsc.display());
    eprintln!("counter2: {}", paths.counter2.display());
    eprintln!("drrun: {}", paths.drrun.display());
    eprintln!("DBI client: {}", paths.dbi_client.display());
    eprintln!("KVM counter: {}", paths.kvm_counter.display());
    eprintln!("riptrace: {}", paths.riptrace.display());
    eprintln!("SaBRe: {}", paths.sabre.display());
    eprintln!("capabilities: {}", capability_path.display());
    eprintln!("fixed getpid anchors: {}", fixed_path.display());

    let mut criterion = Criterion::default()
        .sample_size(env_usize("SYSCALL_BENCH_SAMPLE_SIZE", 20, 10)?)
        .warm_up_time(env_duration("SYSCALL_BENCH_WARMUP_SECS", 2.0)?)
        .measurement_time(env_duration("SYSCALL_BENCH_MEASUREMENT_SECS", 5.0)?)
        .nresamples(env_usize("SYSCALL_BENCH_RESAMPLES", 50_000, 1_000)?)
        .confidence_level(0.95)
        .significance_level(0.05)
        .noise_threshold(0.02)
        .configure_from_args();

    let mut missing = Vec::new();
    for operation in operations {
        let mut group = criterion.benchmark_group(format!("marginal/{}", operation.name()));
        group.sampling_mode(SamplingMode::Linear);
        group.throughput(Throughput::Elements(1));

        for backend in &backends {
            if backend.uses_one_shot_regression() {
                if let Err(error) = run_one_shot(*backend, &paths, operation, 16) {
                    record_capability(
                        &mut capabilities,
                        backend.name,
                        operation.name(),
                        "unsupported",
                        &format!("{error:#}"),
                    )?;
                    missing.push(format!("{}/{}: {error:#}", backend.name, operation.name()));
                    continue;
                }
                record_capability(
                    &mut capabilities,
                    backend.name,
                    operation.name(),
                    "pass",
                    &format!(
                        "16-call semantic preflight passed; one-shot floor={one_shot_floor} scale={one_shot_scale}"
                    ),
                )?;
                capabilities.flush()?;
                if operation == Operation::Getpid {
                    run_fixed_one_shot_series(
                        *backend,
                        &paths,
                        operation,
                        &fixed_counts,
                        &mut fixed_output,
                    )?;
                    fixed_output.flush()?;
                }
                group.throughput(Throughput::Elements(one_shot_scale));
                group.bench_function(
                    format!("{}__scale_{one_shot_scale}", backend.name),
                    |bencher| {
                        bencher.iter_custom(|iterations| {
                            let actual_iterations = iterations
                                .checked_mul(one_shot_scale)
                                .and_then(|value| value.checked_add(one_shot_floor))
                                .unwrap_or_else(|| panic!("KVM iteration count overflow"));
                            run_one_shot(*backend, &paths, operation, actual_iterations)
                                .unwrap_or_else(|error| {
                                    panic!(
                                        "{}/{} failed during one-shot measurement: {error:#}",
                                        backend.name,
                                        operation.name()
                                    )
                                })
                        });
                    },
                );
                group.throughput(Throughput::Elements(1));
                continue;
            }

            let mut guest = match GuestProcess::start(*backend, &paths, timeout) {
                Ok(guest) => guest,
                Err(error) => {
                    record_capability(
                        &mut capabilities,
                        backend.name,
                        operation.name(),
                        "blocked",
                        &format!("{error:#}"),
                    )?;
                    missing.push(format!("{}/{}: {error:#}", backend.name, operation.name()));
                    continue;
                }
            };

            if let Err(error) = guest.run_batch(operation, 16) {
                record_capability(
                    &mut capabilities,
                    backend.name,
                    operation.name(),
                    "unsupported",
                    &format!("{error:#}"),
                )?;
                missing.push(format!("{}/{}: {error:#}", backend.name, operation.name()));
                continue;
            }
            record_capability(
                &mut capabilities,
                backend.name,
                operation.name(),
                "pass",
                "16-call semantic preflight passed",
            )?;
            capabilities.flush()?;
            if operation == Operation::Getpid {
                run_fixed_persistent_series(
                    backend.name,
                    &mut guest,
                    operation,
                    &fixed_counts,
                    &mut fixed_output,
                )?;
                fixed_output.flush()?;
            }

            group.bench_function(backend.name, |bencher| {
                bencher.iter_custom(|iterations| {
                    guest
                        .run_batch(operation, iterations)
                        .unwrap_or_else(|error| {
                            panic!(
                                "{}/{} failed during measurement: {error:#}",
                                backend.name,
                                operation.name()
                            )
                        })
                });
            });
        }
        group.finish();
    }
    criterion.final_summary();
    capabilities.flush()?;
    fixed_output.flush()?;

    if require_all && !missing.is_empty() {
        bail!(
            "required backend/syscall rows were unavailable:\n{}",
            missing.join("\n")
        );
    }
    if !missing.is_empty() {
        eprintln!("{} backend/syscall rows were not measured", missing.len());
    }
    Ok(())
}

fn run_fixed_persistent_series(
    backend: &str,
    guest: &mut GuestProcess,
    operation: Operation,
    counts: &[u64],
    output: &mut impl Write,
) -> Result<()> {
    for &count in counts {
        let elapsed = guest
            .run_batch(operation, count)
            .with_context(|| format!("running fixed {backend}/{} x {count}", operation.name()))?;
        record_fixed_count(output, backend, operation, count, elapsed)?;
    }
    Ok(())
}

fn run_fixed_one_shot_series(
    backend: Backend,
    paths: &Paths,
    operation: Operation,
    counts: &[u64],
    output: &mut impl Write,
) -> Result<()> {
    for &count in counts {
        let elapsed = run_one_shot(backend, paths, operation, count).with_context(|| {
            format!(
                "running fixed {}/{} x {count}",
                backend.name,
                operation.name()
            )
        })?;
        record_fixed_count(output, backend.name, operation, count, elapsed)?;
    }
    Ok(())
}

fn record_fixed_count(
    output: &mut impl Write,
    backend: &str,
    operation: Operation,
    count: u64,
    elapsed: Duration,
) -> Result<()> {
    let elapsed_ns = elapsed.as_nanos();
    writeln!(
        output,
        "{backend}\t{}\t{count}\t{elapsed_ns}\t{:.6}",
        operation.name(),
        elapsed_ns as f64 / count as f64
    )?;
    Ok(())
}

fn wait_readable(fd: i32, timeout: Duration) -> io::Result<()> {
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    loop {
        // SAFETY: descriptor points to one initialized pollfd for the duration of this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no response within {:.3}s", timeout.as_secs_f64()),
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn pin_current_process() -> Result<()> {
    let Some(cpu) = env::var_os("SYSCALL_BENCH_CPU") else {
        eprintln!("warning: SYSCALL_BENCH_CPU is unset; process affinity is not pinned");
        return Ok(());
    };
    let cpu: usize = cpu
        .to_string_lossy()
        .parse()
        .context("SYSCALL_BENCH_CPU must be a logical CPU number")?;
    // SAFETY: cpu_set_t is initialized before use and passed with its exact size.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error()).context("pinning benchmark CPU affinity");
        }
    }
    eprintln!("pinned benchmark and inherited children to CPU {cpu}");
    Ok(())
}

fn selected_backends() -> Result<Vec<Backend>> {
    let selected = env::var("SYSCALL_BENCH_BACKENDS").unwrap_or_else(|_| {
        ALL_BACKENDS
            .iter()
            .map(|item| item.name)
            .collect::<Vec<_>>()
            .join(",")
    });
    selected
        .split(',')
        .filter(|name| !name.is_empty())
        .map(|name| {
            ALL_BACKENDS
                .iter()
                .copied()
                .find(|backend| backend.name == name)
                .with_context(|| format!("unknown backend {name:?}"))
        })
        .collect()
}

fn selected_operations() -> Result<Vec<Operation>> {
    let selected = env::var("SYSCALL_BENCH_SYSCALLS").unwrap_or_else(|_| {
        ALL_OPERATIONS
            .iter()
            .map(|item| item.name())
            .collect::<Vec<_>>()
            .join(",")
    });
    selected
        .split(',')
        .filter(|name| !name.is_empty())
        .map(|name| {
            ALL_OPERATIONS
                .iter()
                .copied()
                .find(|operation| operation.name() == name)
                .with_context(|| format!("unknown syscall {name:?}"))
        })
        .collect()
}

fn fixed_counts() -> Result<Vec<u64>> {
    let value = env::var("SYSCALL_BENCH_FIXED_COUNTS")
        .unwrap_or_else(|_| "1000,10000,100000,1000000".to_owned());
    let counts: Vec<u64> = value
        .split(',')
        .filter(|item| !item.is_empty())
        .map(|item| {
            let count = item
                .parse::<u64>()
                .with_context(|| format!("invalid fixed syscall count {item:?}"))?;
            if count == 0 {
                bail!("fixed syscall counts must be greater than zero");
            }
            Ok(count)
        })
        .collect::<Result<_>>()?;
    if counts.is_empty() {
        bail!("SYSCALL_BENCH_FIXED_COUNTS must contain at least one count");
    }
    Ok(counts)
}

fn record_capability(
    output: &mut impl Write,
    backend: &str,
    syscall: &str,
    status: &str,
    detail: &str,
) -> Result<()> {
    let detail = detail.replace(['\t', '\n', '\r'], " ");
    writeln!(output, "{backend}\t{syscall}\t{status}\t{detail}")?;
    Ok(())
}

fn capability_path() -> PathBuf {
    env_path("SYSCALL_BENCH_CAPABILITIES").unwrap_or_else(|| {
        env::var_os("CRITERION_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/criterion"))
            .join("capabilities.tsv")
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_duration(name: &str, default: f64) -> Result<Duration> {
    let seconds = env::var(name)
        .ok()
        .map(|value| value.parse::<f64>())
        .transpose()
        .with_context(|| format!("{name} must be a number of seconds"))?
        .unwrap_or(default);
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("{name} must be finite and greater than zero");
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn env_usize(name: &str, default: usize, minimum: usize) -> Result<usize> {
    let value = env::var(name)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .with_context(|| format!("{name} must be an integer"))?
        .unwrap_or(default);
    if value < minimum {
        bail!("{name} must be at least {minimum}");
    }
    Ok(value)
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    match value.to_string_lossy().as_ref() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => bail!("{name} must be 0/1, true/false, or yes/no"),
    }
}

fn require_executable(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("{description} not found at {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{description} is not a file: {}", path.display());
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!("{description} is not executable: {}", path.display());
    }
    Ok(())
}

fn require_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("{description} not found at {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{description} is not a file: {}", path.display());
    }
    Ok(())
}
