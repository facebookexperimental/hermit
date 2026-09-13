use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use hermit_manifest_plan::nextest_binaries::CPU_WRAPPER_ENV;
use hermit_manifest_plan::nextest_cpu::AttemptCompletion;
use hermit_manifest_plan::nextest_cpu::AttemptIdentity;
use hermit_manifest_plan::nextest_cpu::AttemptRecord;
use hermit_manifest_plan::nextest_cpu::BINARY_MAP_SCHEMA;
use hermit_manifest_plan::nextest_cpu::BinaryMap;
use hermit_manifest_plan::nextest_cpu::BinaryMapEntry;
use hermit_manifest_plan::nextest_cpu::CPU_BINARY_MAP_ENV;
use hermit_manifest_plan::nextest_cpu::CPU_RECORD_DIR_ENV;
use hermit_manifest_plan::nextest_cpu::CPU_REPORT_PATH_ENV;
use hermit_manifest_plan::nextest_cpu::read_attempt_records;
use hermit_manifest_plan::nextest_cpu::read_binary_map;
use hermit_manifest_plan::nextest_cpu::write_attempt_atomic;
use hermit_manifest_plan::nextest_cpu::write_binary_map_atomic;

const ATTEMPT_ENV: &str = "__NEXTEST_ATTEMPT";
const RUN_ID_ENV: &str = "NEXTEST_RUN_ID";
const PACKAGE_ENV: &str = "CARGO_PKG_NAME";
const CONTROL_ARM_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL";
const CONTROL_CWD_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CWD";
const CONTROL_PID_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_PID_FILE";
const CONTROL_SIGNAL_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_SIGNAL_FILE";
const CONTROL_SENTINEL_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_SENTINEL";
const INFRASTRUCTURE_EXIT: u8 = 70;

static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);
static CONTROL_SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn record_control_signal(_: libc::c_int) {
    // The control counts actual deliveries, without async-unsafe formatting or
    // allocation in the handler. The descriptor remains open until child exit.
    unsafe {
        libc::write(
            CONTROL_SIGNAL_FD.load(Ordering::SeqCst),
            b"T".as_ptr().cast(),
            1,
        );
    }
}

extern "C" fn remember_signal(signal: libc::c_int) {
    RECEIVED_SIGNAL.store(signal, Ordering::SeqCst);
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|error| format!("{name} must be present and valid UTF-8: {error}"))
}

fn identity_from_command(program: &OsStr, args: &[OsString]) -> Result<AttemptIdentity, String> {
    let mut command = Vec::with_capacity(args.len() + 1);
    command.push(program);
    command.extend(args.iter().map(OsString::as_os_str));
    let exact = command
        .windows(4)
        .find(|window| window[1] == OsStr::new("--exact") && window[3] == OsStr::new("--nocapture"))
        .ok_or_else(|| {
            "nextest wrapper command lacks the expected TEST_BINARY --exact TEST --nocapture sequence"
                .to_string()
        })?;
    let test = exact[2]
        .to_str()
        .ok_or_else(|| "nextest test name is not valid UTF-8".to_string())?;
    let map = read_binary_map(Path::new(&required_env(CPU_BINARY_MAP_ENV)?))?;
    let (package, binary) = map.identity_for_executable(Path::new(exact[0]))?;
    let command_package = required_env(PACKAGE_ENV)?;
    if command_package != package {
        return Err(format!(
            "nextest command package {command_package:?} disagrees with typed inventory package {package:?}"
        ));
    }
    let attempt = required_env(ATTEMPT_ENV)?
        .parse::<u64>()
        .map_err(|error| format!("{ATTEMPT_ENV} is not a positive integer: {error}"))?;
    let identity = AttemptIdentity {
        package: package.to_string(),
        binary: binary.to_string(),
        test: test.to_string(),
        attempt,
    };
    identity.validate()?;
    Ok(identity)
}

fn cpu_usage_usec(pgid: u32) -> Result<u64, String> {
    let seconds = dagrun::proccpu::subtree_cpu_seconds_in(pgid, Path::new("/proc"))
        .ok_or_else(|| format!("cannot measure process group {pgid} from /proc"))?;
    let usec = seconds * 1_000_000.0;
    if !usec.is_finite() || usec.is_sign_negative() || usec > u64::MAX as f64 {
        return Err(format!(
            "process group {pgid} returned invalid CPU seconds {seconds}"
        ));
    }
    Ok(usec as u64)
}

fn elapsed_ms(started: Instant) -> Result<u64, String> {
    u64::try_from(started.elapsed().as_millis())
        .map_err(|error| format!("attempt wall duration does not fit u64 milliseconds: {error}"))
}

fn install_signal_handlers() -> Result<(), String> {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = remember_signal as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        action.sa_flags = 0;
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(format!(
                "cannot install signal handler for {signal}: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn wait_for_child(pid: u32) -> Result<Option<ExitStatus>, String> {
    loop {
        if RECEIVED_SIGNAL.load(Ordering::SeqCst) != 0 {
            return Ok(None);
        }
        let mut raw_status = 0;
        let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut raw_status, 0) };
        if waited == pid as libc::pid_t {
            return Ok(Some(ExitStatus::from_raw(raw_status)));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(format!("waitpid({pid}) failed: {error}"));
    }
}

fn propagate_signal(signal: i32) -> ! {
    unsafe {
        // Nextest owns signals to the test process group. Reflect this status
        // on the wrapper only: broadcasting here delivers a second signal to
        // a child that already received and caught Nextest's group signal.
        let mut action = std::mem::zeroed::<libc::sigaction>();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, signal);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(signal);
        libc::_exit(128 + signal);
    }
}

fn run_wrapper(args: Vec<OsString>) -> Result<ExitStatus, String> {
    let (program, child_args) = args
        .split_first()
        .ok_or_else(|| "nextest CPU wrapper requires a test command".to_string())?;
    let pid = std::process::id();
    let pgid = unsafe { libc::getpgrp() };
    if pgid != pid as i32 {
        return Err(format!(
            "nextest CPU wrapper PID {pid} is in process group {pgid}; refusing to attribute another process group's CPU"
        ));
    }
    let record_dir = PathBuf::from(required_env(CPU_RECORD_DIR_ENV)?);
    let run_id = required_env(RUN_ID_ENV)?;
    let identity = identity_from_command(program, child_args)?;
    let started = Instant::now();
    let cpu_started = cpu_usage_usec(pid)?;

    let mut command = Command::new(program);
    command.args(child_args);
    command.env_remove(CPU_BINARY_MAP_ENV);
    command.env_remove(CPU_RECORD_DIR_ENV);
    command.env_remove(CPU_REPORT_PATH_ENV);
    command.env_remove(CPU_WRAPPER_ENV);
    let child = command
        .spawn()
        .map_err(|error| format!("cannot execute nextest test command: {error}"))?;
    let child_pid = child.id();
    install_signal_handlers()?;
    let status = wait_for_child(child_pid)?;
    drop(child);

    let supervisor_signal = RECEIVED_SIGNAL.load(Ordering::SeqCst);
    let completion = if supervisor_signal != 0 {
        AttemptCompletion::SupervisorSignal {
            signal: supervisor_signal,
        }
    } else {
        let status = status
            .ok_or_else(|| "child status is missing without a supervisor signal".to_string())?;
        match (status.code(), status.signal()) {
            (Some(code), None) => AttemptCompletion::Exit { code },
            (None, Some(signal)) => AttemptCompletion::Signal { signal },
            _ => return Err(format!("child returned unsupported exit status {status:?}")),
        }
    };
    let cpu_finished = cpu_usage_usec(pid)?;
    let cpu_used = cpu_finished.checked_sub(cpu_started).ok_or_else(|| {
        format!("process-group CPU moved backwards from {cpu_started}us to {cpu_finished}us")
    })?;
    // SupervisorSignal records the snapshot at interruption. The test may
    // still consume CPU during Nextest's grace period; this is not a complete
    // post-cleanup CPU total. Nextest retains ownership of that lifecycle.
    let record = AttemptRecord::new(
        run_id,
        identity,
        cpu_used,
        elapsed_ms(started)?,
        completion.clone(),
    );
    write_attempt_atomic(&record_dir, &record)?;

    match completion {
        AttemptCompletion::Exit { .. } => {
            status.ok_or_else(|| "exit completion is missing its child status".to_string())
        }
        AttemptCompletion::Signal { signal } | AttemptCompletion::SupervisorSignal { signal } => {
            propagate_signal(signal)
        }
    }
}

fn burn_cpu(milliseconds: u64) {
    let mut started = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut started);
    }
    let target_ns = milliseconds.saturating_mul(1_000_000) as i128;
    let mut value = 1u64;
    loop {
        value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
        std::hint::black_box(value);
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now);
        }
        let elapsed = (now.tv_sec - started.tv_sec) as i128 * 1_000_000_000
            + (now.tv_nsec - started.tv_nsec) as i128;
        if elapsed >= target_ns {
            break;
        }
    }
}

fn control_child(mode: &str, args: &[OsString]) -> Result<ExitCode, String> {
    match mode {
        "success" | "failure" => {
            let expected = ["--exact", mode, "--nocapture"];
            if args
                .iter()
                .map(OsString::as_os_str)
                .ne(expected.iter().map(OsStr::new))
            {
                return Err(format!(
                    "control child received changed arguments: {args:?}"
                ));
            }
            let expected_cwd = PathBuf::from(required_env(CONTROL_CWD_ENV)?);
            if env::current_dir().map_err(|error| error.to_string())? != expected_cwd {
                return Err("control child received a changed working directory".into());
            }
            if required_env(CONTROL_SENTINEL_ENV)? != "preserved" {
                return Err("control child received a changed environment".into());
            }
            if env::var_os(CPU_RECORD_DIR_ENV).is_some()
                || env::var_os(CPU_BINARY_MAP_ENV).is_some()
                || env::var_os(CPU_WRAPPER_ENV).is_some()
            {
                return Err(
                    "measurement-only configuration leaked into the test environment".into(),
                );
            }
            println!("stdout-exact");
            eprintln!("stderr-exact");
            Ok(ExitCode::from(if mode == "success" { 0 } else { 23 }))
        }
        "signal" => unsafe {
            libc::raise(libc::SIGUSR1);
            libc::_exit(255);
        },
        "tree" => {
            let executable = env::current_exe().map_err(|error| error.to_string())?;
            let mut children = Vec::new();
            for _ in 0..2 {
                children.push(
                    Command::new(&executable)
                        .args(["--exact", "burn", "--nocapture"])
                        .env(CONTROL_ARM_ENV, "1")
                        .spawn()
                        .map_err(|error| format!("cannot spawn CPU child: {error}"))?,
                );
            }
            for mut child in children {
                let status = child
                    .wait()
                    .map_err(|error| format!("cannot wait for CPU child: {error}"))?;
                if !status.success() {
                    return Err(format!("CPU child failed with {status}"));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        "burn" => {
            burn_cpu(100);
            Ok(ExitCode::SUCCESS)
        }
        "hang" => {
            let path = PathBuf::from(required_env(CONTROL_PID_FILE_ENV)?);
            fs::write(&path, format!("{}\n", std::process::id()))
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        "catch-signal" => {
            let signal_file = fs::File::create(required_env(CONTROL_SIGNAL_FILE_ENV)?)
                .map_err(|e| e.to_string())?;
            CONTROL_SIGNAL_FD.store(signal_file.as_raw_fd(), Ordering::SeqCst);
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = record_control_signal as *const () as usize;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
            }
            if unsafe { libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) } != 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            fs::write(
                required_env(CONTROL_PID_FILE_ENV)?,
                std::process::id().to_string(),
            )
            .map_err(|e| e.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while signal_file.metadata().map_err(|e| e.to_string())?.len() == 0 {
                if Instant::now() >= deadline {
                    return Err("catching control was not signalled".into());
                }
                thread::sleep(Duration::from_millis(2));
            }
            thread::sleep(Duration::from_millis(350));
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(format!("unknown control-child mode {mode:?}")),
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let path = env::temp_dir().join(format!(
            "hermit-nextest-cpu-wrapper-self-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("attempts"))
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn control_command(
    executable: &Path,
    test_binary: &Path,
    scratch: &Path,
    mode: &str,
    attempt: u64,
) -> Command {
    let mut command = Command::new(executable);
    command
        .arg(test_binary)
        .args(["--exact", mode, "--nocapture"])
        .current_dir(scratch)
        .env(CPU_BINARY_MAP_ENV, scratch.join("binary-map.json"))
        .env(CPU_RECORD_DIR_ENV, scratch.join("attempts"))
        .env(RUN_ID_ENV, "self-test-run")
        .env(PACKAGE_ENV, "fixture")
        .env(ATTEMPT_ENV, attempt.to_string())
        .env(CONTROL_ARM_ENV, "1")
        .env(CONTROL_CWD_ENV, scratch)
        .env(CONTROL_SENTINEL_ENV, "preserved")
        .env(CPU_WRAPPER_ENV, executable)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

fn find_record<'a>(records: &'a [AttemptRecord], test: &str) -> Result<&'a AttemptRecord, String> {
    records
        .iter()
        .find(|record| record.identity.test == test)
        .ok_or_else(|| format!("self-test did not find the {test:?} attempt record"))
}

fn catching_signal_control(
    executable: &Path,
    test_binary: &Path,
    scratch: &Path,
    wrapped: bool,
) -> Result<Vec<u8>, String> {
    let suffix = if wrapped { "wrapped" } else { "native" };
    let pid_file = scratch.join(format!("{suffix}-catch.pid"));
    let signal_file = scratch.join(format!("{suffix}-catch.signals"));
    let mut command = if wrapped {
        control_command(executable, test_binary, scratch, "catch-signal", 1)
    } else {
        let mut command = Command::new(test_binary);
        command
            .args(["--exact", "catch-signal", "--nocapture"])
            .env(CONTROL_ARM_ENV, "1")
            .env_remove(CPU_RECORD_DIR_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        command
    };
    command
        .env(CONTROL_PID_FILE_ENV, &pid_file)
        .env(CONTROL_SIGNAL_FILE_ENV, &signal_file);
    let child = command.spawn().map_err(|e| e.to_string())?;
    let group = child.id() as i32;
    let deadline = Instant::now() + Duration::from_secs(5);
    let ready = loop {
        let pid = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.parse::<i32>().ok());
        let child_ready = pid.is_some_and(|pid| unsafe { libc::getpgid(pid) == group });
        let wrapper_ready = !wrapped
            || fs::read_to_string(format!("/proc/{group}/status"))
                .ok()
                .and_then(|s| {
                    s.lines().find_map(|line| {
                        line.strip_prefix("SigCgt:")
                            .and_then(|s| u64::from_str_radix(s.trim(), 16).ok())
                    })
                })
                .is_some_and(|mask| mask & (1 << (libc::SIGTERM - 1)) != 0);
        if child_ready && wrapper_ready {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(2));
    };
    if !ready {
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
        let _ = child.wait_with_output();
        return Err(format!("{suffix} catching control did not become ready"));
    }
    // Exactly one controller delivery in both cases. The child uses the same
    // handler and grace interval; its bytes expose wrapper rebroadcasts.
    if unsafe { libc::kill(-group, libc::SIGTERM) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if (wrapped && output.status.signal() != Some(libc::SIGTERM))
        || (!wrapped && !output.status.success())
        || !output.stdout.is_empty()
        || !output.stderr.is_empty()
    {
        return Err(format!(
            "{suffix} catching control changed observable behavior: {output:?}"
        ));
    }
    fs::read(signal_file).map_err(|e| e.to_string())
}

fn self_test() -> Result<(), String> {
    let scratch = Scratch::new()?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let test_binary = scratch.0.join("fixture_name-0123456789abcdef");
    std::os::unix::fs::symlink(&executable, &test_binary)
        .map_err(|error| format!("cannot link self-test binary: {error}"))?;
    let map = BinaryMap {
        schema: BINARY_MAP_SCHEMA,
        entries: vec![BinaryMapEntry {
            executable: test_binary
                .to_str()
                .ok_or_else(|| "self-test path is not UTF-8".to_string())?
                .into(),
            package: "fixture".into(),
            binary: "fixture::bin/fixture_name".into(),
            binary_name: "fixture_name".into(),
            kind: "bin".into(),
        }],
    };
    write_binary_map_atomic(&scratch.0.join("binary-map.json"), &map)?;

    let success = control_command(&executable, &test_binary, &scratch.0, "success", 1)
        .output()
        .map_err(|error| format!("cannot run success control: {error}"))?;
    if !success.status.success()
        || success.stdout != b"stdout-exact\n"
        || success.stderr != b"stderr-exact\n"
    {
        return Err(format!(
            "success control changed observable behavior: {success:?}"
        ));
    }

    let failure = control_command(&executable, &test_binary, &scratch.0, "failure", 1)
        .output()
        .map_err(|error| format!("cannot run failure control: {error}"))?;
    if failure.status.code() != Some(23)
        || failure.stdout != b"stdout-exact\n"
        || failure.stderr != b"stderr-exact\n"
    {
        return Err(format!(
            "failure control changed observable behavior: {failure:?}"
        ));
    }

    let signal = control_command(&executable, &test_binary, &scratch.0, "signal", 1)
        .output()
        .map_err(|error| format!("cannot run signal control: {error}"))?;
    if signal.status.signal() != Some(libc::SIGUSR1)
        || !signal.stdout.is_empty()
        || !signal.stderr.is_empty()
    {
        return Err(format!(
            "signal control changed observable behavior: {signal:?}"
        ));
    }

    let tree = control_command(&executable, &test_binary, &scratch.0, "tree", 1)
        .output()
        .map_err(|error| format!("cannot run process-tree control: {error}"))?;
    if !tree.status.success() {
        return Err(format!("process-tree control failed: {tree:?}"));
    }

    let substituted_binary = scratch.0.join("substituted-0123456789abcdef");
    std::os::unix::fs::symlink(&executable, &substituted_binary)
        .map_err(|error| format!("cannot link substituted self-test binary: {error}"))?;
    let substituted = control_command(&executable, &substituted_binary, &scratch.0, "success", 1)
        .output()
        .map_err(|error| format!("cannot run substituted-path control: {error}"))?;
    if substituted.status.code() != Some(INFRASTRUCTURE_EXIT.into())
        || !String::from_utf8_lossy(&substituted.stderr).contains("absent from the typed inventory")
    {
        return Err(format!(
            "substituted-path control was not refused: {substituted:?}"
        ));
    }

    let pid_file = scratch.0.join("wall-timeout-child.pid");
    let mut wall_command = control_command(&executable, &test_binary, &scratch.0, "hang", 1);
    wall_command.env(CONTROL_PID_FILE_ENV, &pid_file);
    let wall_child = wall_command
        .spawn()
        .map_err(|error| format!("cannot run wall-timeout control: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid_file.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !pid_file.is_file() {
        return Err("wall-timeout control child did not start".into());
    }
    if unsafe { libc::kill(-(wall_child.id() as i32), libc::SIGTERM) } != 0 {
        return Err(format!(
            "cannot signal wall-timeout process group: {}",
            io::Error::last_os_error()
        ));
    }
    let wall = wall_child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for wall-timeout control: {error}"))?;
    if wall.status.signal() != Some(libc::SIGTERM) {
        return Err(format!(
            "wall-timeout control did not preserve SIGTERM: {:?}",
            wall.status
        ));
    }

    let native_signals = catching_signal_control(&executable, &test_binary, &scratch.0, false)?;
    let wrapped_signals = catching_signal_control(&executable, &test_binary, &scratch.0, true)?;
    if native_signals != b"T" || wrapped_signals != native_signals {
        return Err(format!(
            "catching control changed signal delivery: native={native_signals:?}, wrapped={wrapped_signals:?}"
        ));
    }
    let records = read_attempt_records(&scratch.0.join("attempts"))?;
    let identities = records
        .iter()
        .map(|r| r.identity.test.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if records.len() != 6
        || identities
            != [
                "success",
                "failure",
                "signal",
                "tree",
                "hang",
                "catch-signal",
            ]
            .into_iter()
            .collect()
    {
        return Err(format!(
            "self-test expected six exact atomic attempt identities, found {identities:?} ({} records)",
            records.len()
        ));
    }
    if records
        .iter()
        .any(|record| record.identity.binary != "fixture::bin/fixture_name")
    {
        return Err("self-test did not preserve the typed binary identity".into());
    }
    if !matches!(
        find_record(&records, "success")?.completion,
        AttemptCompletion::Exit { code: 0 }
    ) || !matches!(
        find_record(&records, "failure")?.completion,
        AttemptCompletion::Exit { code: 23 }
    ) || !matches!(
        find_record(&records, "signal")?.completion,
        AttemptCompletion::Signal {
            signal: libc::SIGUSR1
        }
    ) || !matches!(
        find_record(&records, "catch-signal")?.completion,
        AttemptCompletion::SupervisorSignal {
            signal: libc::SIGTERM
        }
    ) || !matches!(
        find_record(&records, "hang")?.completion,
        AttemptCompletion::SupervisorSignal {
            signal: libc::SIGTERM
        }
    ) {
        return Err(
            "self-test attempt completion records do not preserve exit/signal status".into(),
        );
    }
    let tree_record = find_record(&records, "tree")?;
    if tree_record.cpu_usage_usec < 150_000 {
        return Err(format!(
            "process-tree control expected at least 150000us, measured {}us",
            tree_record.cpu_usage_usec
        ));
    }
    let duplicate = write_attempt_atomic(&scratch.0.join("attempts"), &records[0]);
    if duplicate.is_ok() {
        return Err("duplicate atomic attempt publication unexpectedly replaced a record".into());
    }
    println!(
        "nextest-cpu-wrapper: self-test PASS (process tree, success, failure, signal, wall timeout, typed identity, substituted path, atomic identity)"
    );
    Ok(())
}

fn main() -> ExitCode {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--self-test") {
        return match self_test() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("nextest-cpu-wrapper: {error}");
                ExitCode::from(INFRASTRUCTURE_EXIT)
            }
        };
    }
    if env::var_os(CONTROL_ARM_ENV).is_some() && env::var_os(CPU_RECORD_DIR_ENV).is_none() {
        let mode = args
            .windows(2)
            .find(|window| window[0] == "--exact")
            .and_then(|window| window[1].to_str());
        return match mode {
            Some(mode) => control_child(mode, &args).unwrap_or_else(|error| {
                eprintln!("nextest-cpu-wrapper control: {error}");
                ExitCode::from(INFRASTRUCTURE_EXIT)
            }),
            None => ExitCode::from(INFRASTRUCTURE_EXIT),
        };
    }
    match run_wrapper(args) {
        Ok(status) => ExitCode::from(status.code().unwrap_or(INFRASTRUCTURE_EXIT as i32) as u8),
        Err(error) => {
            eprintln!("nextest-cpu-wrapper: {error}");
            ExitCode::from(INFRASTRUCTURE_EXIT)
        }
    }
}
