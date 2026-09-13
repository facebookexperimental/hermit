use std::collections::BTreeMap;
use std::collections::BTreeSet;
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
use hermit_manifest_plan::nextest_cpu::CPU_SOURCE;
use hermit_manifest_plan::nextest_cpu::CPU_SOURCE_ENFORCED;
use hermit_manifest_plan::nextest_cpu::read_attempt_records;
use hermit_manifest_plan::nextest_cpu::read_binary_map;
use hermit_manifest_plan::nextest_cpu::write_attempt_atomic;
use hermit_manifest_plan::nextest_cpu::write_binary_map_atomic;
use hermit_manifest_plan::timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV;

const ATTEMPT_ENV: &str = "__NEXTEST_ATTEMPT";
const RUN_ID_ENV: &str = "NEXTEST_RUN_ID";
const PACKAGE_ENV: &str = "CARGO_PKG_NAME";
const CONTROL_ARM_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL";
const CONTROL_CWD_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CWD";
const CONTROL_CAUSE_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CAUSE_FILE";
const CONTROL_PID_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_PID_FILE";
const CONTROL_PROC_ROOT_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_PROC_ROOT";
const CONTROL_SIGNAL_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_SIGNAL_FILE";
const CONTROL_SENTINEL_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_SENTINEL";
const INFRASTRUCTURE_EXIT: u8 = 70;
const CPU_TIMEOUT_EXIT: u8 = 124;
const NON_SIGNAL_CAUSE_RESERVED: i32 = -1;
// A wrapper owns one test process group. Sampling twice per second keeps the
// full-procfs scan bounded while retaining a sub-second enforcement boundary.
const CPU_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CHILD_REAP_DEADLINE: Duration = Duration::from_secs(5);

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
    let _ = RECEIVED_SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
}

fn received_external_signal() -> Option<i32> {
    let value = RECEIVED_SIGNAL.load(Ordering::SeqCst);
    (value > 0).then_some(value)
}

fn reserve_non_signal_cause() -> Result<(), i32> {
    match RECEIVED_SIGNAL.compare_exchange(
        0,
        NON_SIGNAL_CAUSE_RESERVED,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => Ok(()),
        Err(signal) if signal > 0 => Err(signal),
        Err(other) => panic!("invalid first-cause state {other}"),
    }
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

fn proc_root() -> PathBuf {
    if env::var_os(CONTROL_ARM_ENV).is_some() {
        env::var_os(CONTROL_PROC_ROOT_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/proc"))
    } else {
        PathBuf::from("/proc")
    }
}

fn proc_sample(path: &Path) -> Result<Option<(u32, u32, u64)>, String> {
    let stat_path = path.join("stat");
    let raw = match fs::read_to_string(&stat_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot read process accounting from {}: {error}",
                stat_path.display()
            ));
        }
    };
    let close = raw.rfind(')').ok_or_else(|| {
        format!(
            "process accounting from {} has no command terminator",
            stat_path.display()
        )
    })?;
    let fields = raw[close + 1..].split_whitespace().collect::<Vec<_>>();
    if fields.len() <= 14 {
        return Err(format!(
            "process accounting from {} has only {} fields",
            stat_path.display(),
            fields.len()
        ));
    }
    let pid = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("process directory {} is not valid UTF-8", path.display()))?
        .parse::<u32>()
        .map_err(|error| format!("invalid process directory {}: {error}", path.display()))?;
    let ppid = fields[1].parse::<u32>().map_err(|error| {
        format!(
            "invalid parent PID in process accounting from {}: {error}",
            stat_path.display()
        )
    })?;
    let ticks = [11usize, 12, 13, 14].into_iter().try_fold(
        0u64,
        |total, index| -> Result<u64, String> {
            let value = fields[index].parse::<u64>().map_err(|error| {
                format!(
                    "invalid CPU ticks in process accounting from {}: {error}",
                    stat_path.display()
                )
            })?;
            total.checked_add(value).ok_or_else(|| {
                format!(
                    "CPU ticks in process accounting from {} overflow u64",
                    stat_path.display()
                )
            })
        },
    )?;
    Ok(Some((pid, ppid, ticks)))
}

fn scan_processes(proc_root: &Path) -> Result<BTreeMap<u32, (u32, u64)>, String> {
    let entries = fs::read_dir(proc_root).map_err(|error| {
        format!(
            "cannot scan {} for test descendants: {error}",
            proc_root.display()
        )
    })?;
    let mut samples = BTreeMap::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot enumerate a process under {}: {error}",
                    proc_root.display()
                ));
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if let Some((pid, ppid, ticks)) = proc_sample(&entry.path())? {
            samples.insert(pid, (ppid, ticks));
        }
    }
    Ok(samples)
}

fn descendant_pids(
    root_pid: u32,
    samples: &BTreeMap<u32, (u32, u64)>,
) -> Result<BTreeSet<u32>, String> {
    if !samples.contains_key(&root_pid) {
        return Err(format!("wrapper PID {root_pid} is absent from procfs"));
    }
    let mut descendants = BTreeSet::from([root_pid]);
    loop {
        let before = descendants.len();
        for (&pid, &(ppid, _)) in samples {
            if descendants.contains(&ppid) {
                descendants.insert(pid);
            }
        }
        if descendants.len() == before {
            break;
        }
    }
    descendants.remove(&root_pid);
    Ok(descendants)
}

fn descendant_cpu_usec_in(root_pid: u32, proc_root: &Path) -> Result<u64, String> {
    let samples = scan_processes(proc_root)?;
    let descendants = descendant_pids(root_pid, &samples)
        .map_err(|error| format!("{error}; accounting source {}", proc_root.display()))?;
    let ticks = descendants
        .iter()
        .try_fold(0u64, |total, pid| {
            total.checked_add(samples.get(pid).expect("descendant came from samples").1)
        })
        .ok_or_else(|| "nextest test subtree CPU ticks overflowed u64".to_string())?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return Err("cannot read the procfs clock-tick rate".into());
    }
    let usec = u128::from(ticks)
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_div(ticks_per_second as u128))
        .ok_or_else(|| "nextest test subtree CPU duration overflowed".to_string())?;
    u64::try_from(usec).map_err(|_| "nextest test subtree CPU duration exceeds u64".into())
}

fn descendant_cpu_usec(root_pid: u32) -> Result<u64, String> {
    descendant_cpu_usec_in(root_pid, &proc_root())
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

fn parse_positive_u64(value: &OsStr, label: &str) -> Result<u64, String> {
    let value = value
        .to_str()
        .ok_or_else(|| format!("{label} must be valid UTF-8"))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|error| format!("{label} must be a positive integer: {error}"))?;
    if parsed == 0 {
        return Err(format!("{label} must be greater than zero"));
    }
    Ok(parsed)
}

struct WrapperInvocation {
    cpu_budget_usec: Option<u64>,
    termination_grace: Duration,
    command: Vec<OsString>,
}

fn parse_wrapper_invocation(args: Vec<OsString>) -> Result<WrapperInvocation, String> {
    if args.first() != Some(&OsString::from("--cpu-timeout-usec")) {
        if args.is_empty() {
            return Err("nextest CPU wrapper requires a test command".into());
        }
        return Ok(WrapperInvocation {
            cpu_budget_usec: None,
            termination_grace: Duration::from_secs(2),
            command: args,
        });
    }
    if args.len() < 6
        || args[2] != OsStr::new("--termination-grace-ms")
        || args[4] != OsStr::new("--")
    {
        return Err(
            "nextest CPU wrapper budget form requires --cpu-timeout-usec USEC --termination-grace-ms MS -- TEST_COMMAND"
                .into(),
        );
    }
    let cpu_budget_usec = parse_positive_u64(&args[1], "--cpu-timeout-usec")?;
    let grace_ms = parse_positive_u64(&args[3], "--termination-grace-ms")?;
    let command = args[5..].to_vec();
    if command.is_empty() {
        return Err("nextest CPU wrapper requires a test command after --".into());
    }
    Ok(WrapperInvocation {
        cpu_budget_usec: Some(cpu_budget_usec),
        termination_grace: Duration::from_millis(grace_ms),
        command,
    })
}

fn install_subreaper() -> Result<(), String> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(format!(
            "cannot become the nextest test subtree reaper: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn timeval_usec(value: libc::timeval) -> Result<u64, String> {
    let seconds = u64::try_from(value.tv_sec)
        .map_err(|_| "wait4 returned a negative CPU seconds field".to_string())?;
    let microseconds = u64::try_from(value.tv_usec)
        .map_err(|_| "wait4 returned a negative CPU microseconds field".to_string())?;
    if microseconds >= 1_000_000 {
        return Err("wait4 returned an invalid CPU microseconds field".into());
    }
    seconds
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(microseconds))
        .ok_or_else(|| "wait4 CPU duration overflowed u64".to_string())
}

fn rusage_usec(usage: &libc::rusage) -> Result<u64, String> {
    timeval_usec(usage.ru_utime)?
        .checked_add(timeval_usec(usage.ru_stime)?)
        .ok_or_else(|| "wait4 CPU duration overflowed u64".to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildPopulation {
    Present,
    Empty,
}

fn reap_available_children(
    direct_pid: u32,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
) -> Result<ChildPopulation, String> {
    loop {
        let mut raw_status = 0;
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
        let waited = unsafe { libc::wait4(-1, &mut raw_status, libc::WNOHANG, &mut usage) };
        if waited > 0 {
            *reaped_cpu_usec = reaped_cpu_usec
                .checked_add(rusage_usec(&usage)?)
                .ok_or_else(|| "nextest test subtree CPU total overflowed u64".to_string())?;
            if waited as u32 == direct_pid {
                *direct_status = Some(ExitStatus::from_raw(raw_status));
            }
            continue;
        }
        if waited == 0 {
            return Ok(ChildPopulation::Present);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(ChildPopulation::Empty);
        }
        return Err(format!("wait4(-1) failed: {error}"));
    }
}

fn child_group_exists(pgid: u32) -> Result<bool, String> {
    if unsafe { libc::kill(-(pgid as i32), 0) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(format!(
            "cannot inspect child process group {pgid}: {error}"
        )),
    }
}

fn signal_child_group(pgid: u32, signal: i32) -> Result<(), String> {
    if unsafe { libc::kill(-(pgid as i32), signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!(
            "cannot signal child process group {pgid} with {signal}: {error}"
        ))
    }
}

fn signal_process(pid: u32, signal: i32) -> Result<(), String> {
    if unsafe { libc::kill(pid as i32, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!(
            "cannot signal child process {pid} with {signal}: {error}"
        ))
    }
}

fn live_descendants() -> Result<Vec<i32>, String> {
    let root_pid = std::process::id();
    let samples = scan_processes(Path::new("/proc"))?;
    descendant_pids(root_pid, &samples)?
        .into_iter()
        .map(|pid| i32::try_from(pid).map_err(|_| format!("test descendant PID {pid} exceeds i32")))
        .collect()
}

fn signal_live_descendants(signal: i32) -> Result<(), String> {
    let mut errors = Vec::new();
    for pid in live_descendants()? {
        if unsafe { libc::kill(pid, signal) } == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            errors.push(format!("PID {pid}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "cannot signal live test descendants with {signal}: {}",
            errors.join("; ")
        ))
    }
}

fn hard_kill_descendants_and_reap(
    direct_pid: u32,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
) -> Result<(), String> {
    let deadline = Instant::now() + CHILD_REAP_DEADLINE;
    let mut first_error = None;
    loop {
        for result in [
            signal_process(direct_pid, libc::SIGKILL),
            signal_live_descendants(libc::SIGKILL),
        ] {
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        let population = match reap_available_children(direct_pid, direct_status, reaped_cpu_usec) {
            Ok(population) => population,
            Err(error) => {
                first_error.get_or_insert(error);
                ChildPopulation::Present
            }
        };
        let descendants_empty = match live_descendants() {
            Ok(descendants) => descendants.is_empty(),
            Err(error) => {
                first_error.get_or_insert(error);
                false
            }
        };
        if population == ChildPopulation::Empty && descendants_empty {
            return match first_error {
                Some(error) => Err(format!(
                    "test descendants required cleanup after a lifecycle error: {error}"
                )),
                None => Ok(()),
            };
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "cannot prove test descendants were reaped after SIGKILL: {first_error:?}"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn observed_cpu_usec(reaped_cpu_usec: u64) -> Result<u64, String> {
    reaped_cpu_usec
        .checked_add(descendant_cpu_usec(std::process::id())?)
        .ok_or_else(|| "nextest test subtree CPU total overflowed u64".to_string())
}

fn reap_until_empty(
    direct_pid: u32,
    pgid: u32,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
    deadline: Instant,
) -> Result<bool, String> {
    loop {
        let population = reap_available_children(direct_pid, direct_status, reaped_cpu_usec)?;
        if population == ChildPopulation::Empty && !child_group_exists(pgid)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_and_reap(
    direct_pid: u32,
    pgid: u32,
    signal: i32,
    grace: Duration,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
) -> Result<(), String> {
    let mut first_error = signal_child_group(pgid, signal).err();
    if first_error.is_none() {
        match reap_until_empty(
            direct_pid,
            pgid,
            direct_status,
            reaped_cpu_usec,
            Instant::now() + grace,
        ) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => first_error = Some(error),
        }
    }
    let deadline = Instant::now() + CHILD_REAP_DEADLINE;
    let mut hard_error = None;
    let hard_reaped = loop {
        for result in [
            signal_child_group(pgid, libc::SIGKILL),
            signal_live_descendants(libc::SIGKILL),
        ] {
            if let Err(error) = result {
                hard_error.get_or_insert(error);
            }
        }
        let population = match reap_available_children(direct_pid, direct_status, reaped_cpu_usec) {
            Ok(population) => population,
            Err(error) => {
                hard_error.get_or_insert(error);
                ChildPopulation::Present
            }
        };
        let group_exists = match child_group_exists(pgid) {
            Ok(exists) => exists,
            Err(error) => {
                hard_error.get_or_insert(error);
                true
            }
        };
        if population == ChildPopulation::Empty && !group_exists {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(50));
    };
    match (first_error, hard_error, hard_reaped) {
        (None, None, true) => Ok(()),
        (Some(error), None, true) => Err(format!(
            "test subtree required hard cleanup after a lifecycle error: {error}"
        )),
        (first_error, hard_error, hard_reaped) => Err(format!(
            "cannot prove child process group {pgid} and adopted descendants were reaped after SIGKILL: gentle={first_error:?}; hard={hard_error:?}; reaped={hard_reaped}"
        )),
    }
}

fn propagate_signal(signal: i32) -> ! {
    unsafe {
        // The child-owned group has already received this signal exactly once.
        // Reflect the same terminal status on the wrapper without broadcasting
        // it again.
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

#[derive(Debug)]
enum WrapperOutcome {
    Status(ExitStatus),
    CpuTimeout,
}

#[derive(Debug)]
enum FirstCause {
    Exit(ExitStatus),
    CpuTimeout { observed_cpu_usec: u64 },
    ExternalSignal { signal: i32 },
    AccountingUnavailable { error: String },
}

fn reserve_or_external(candidate: FirstCause) -> FirstCause {
    match reserve_non_signal_cause() {
        Ok(()) => candidate,
        Err(signal) => FirstCause::ExternalSignal { signal },
    }
}

fn completion_from_status(status: ExitStatus) -> Result<AttemptCompletion, String> {
    match (status.code(), status.signal()) {
        (Some(code), None) => Ok(AttemptCompletion::Exit { code }),
        (None, Some(signal)) => Ok(AttemptCompletion::Signal { signal }),
        _ => Err(format!("child returned unsupported exit status {status:?}")),
    }
}

fn wait_for_direct_child(
    direct_pid: u32,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
) -> FirstCause {
    loop {
        if let Some(supervisor_signal) = received_external_signal() {
            return FirstCause::ExternalSignal {
                signal: supervisor_signal,
            };
        }
        let mut raw_status = 0;
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
        let waited =
            unsafe { libc::wait4(direct_pid as libc::pid_t, &mut raw_status, 0, &mut usage) };
        if waited == direct_pid as libc::pid_t {
            if let Err(signal) = reserve_non_signal_cause() {
                return FirstCause::ExternalSignal { signal };
            }
            let used = match rusage_usec(&usage) {
                Ok(used) => used,
                Err(error) => return FirstCause::AccountingUnavailable { error },
            };
            *reaped_cpu_usec = match reaped_cpu_usec.checked_add(used) {
                Some(total) => total,
                None => {
                    return FirstCause::AccountingUnavailable {
                        error: "nextest test subtree CPU total overflowed u64".into(),
                    };
                }
            };
            let status = ExitStatus::from_raw(raw_status);
            *direct_status = Some(status);
            return FirstCause::Exit(status);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return match reserve_non_signal_cause() {
            Ok(()) => FirstCause::AccountingUnavailable {
                error: format!("wait4({direct_pid}) failed: {error}"),
            },
            Err(signal) => FirstCause::ExternalSignal { signal },
        };
    }
}

fn run_wrapper(args: Vec<OsString>) -> Result<WrapperOutcome, String> {
    let invocation = parse_wrapper_invocation(args)?;
    let (program, child_args) = invocation
        .command
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
    RECEIVED_SIGNAL.store(0, Ordering::SeqCst);
    install_signal_handlers()?;
    if invocation.cpu_budget_usec.is_some() {
        install_subreaper()?;
    }

    let mut command = Command::new(program);
    command.args(child_args);
    command.env_remove(CPU_BINARY_MAP_ENV);
    command.env_remove(CPU_RECORD_DIR_ENV);
    command.env_remove(CPU_REPORT_PATH_ENV);
    command.env_remove(CPU_WRAPPER_ENV);
    command.env_remove(TEST_CPU_TIMEOUT_MULTIPLIER_ENV);
    command.env_remove(CONTROL_CAUSE_FILE_ENV);
    command.env_remove(CONTROL_PROC_ROOT_ENV);
    if invocation.cpu_budget_usec.is_some() {
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("cannot execute nextest test command: {error}"))?;
    let child_pid = child.id();
    drop(child);
    let mut direct_status = None;
    let mut reaped_cpu_usec = 0u64;
    let mut max_cpu_usec = 0u64;
    let cause = if let Some(cpu_budget_usec) = invocation.cpu_budget_usec {
        let mut next_cpu_poll = Instant::now();
        loop {
            if let Some(supervisor_signal) = received_external_signal() {
                break FirstCause::ExternalSignal {
                    signal: supervisor_signal,
                };
            }
            let population = match reap_available_children(
                child_pid,
                &mut direct_status,
                &mut reaped_cpu_usec,
            ) {
                Ok(population) => population,
                Err(error) => {
                    break reserve_or_external(FirstCause::AccountingUnavailable { error });
                }
            };
            if let Some(status) = direct_status {
                if let Err(signal) = reserve_non_signal_cause() {
                    break FirstCause::ExternalSignal { signal };
                }
                let observed = match observed_cpu_usec(reaped_cpu_usec) {
                    Ok(observed) => observed,
                    Err(error) => break FirstCause::AccountingUnavailable { error },
                };
                max_cpu_usec = max_cpu_usec.max(observed);
                if observed >= cpu_budget_usec {
                    break FirstCause::CpuTimeout {
                        observed_cpu_usec: observed,
                    };
                }
                break FirstCause::Exit(status);
            }
            if population == ChildPopulation::Empty {
                break reserve_or_external(FirstCause::AccountingUnavailable {
                    error: "nextest test subtree disappeared before its direct child was reaped"
                        .into(),
                });
            }
            let now = Instant::now();
            if now >= next_cpu_poll {
                let observed = match observed_cpu_usec(reaped_cpu_usec) {
                    Ok(observed) => observed,
                    Err(error) => {
                        break reserve_or_external(FirstCause::AccountingUnavailable { error });
                    }
                };
                max_cpu_usec = max_cpu_usec.max(observed);
                if observed >= cpu_budget_usec {
                    let cause = reserve_or_external(FirstCause::CpuTimeout {
                        observed_cpu_usec: observed,
                    });
                    if !matches!(cause, FirstCause::CpuTimeout { .. }) {
                        break cause;
                    }
                    if let Ok(path) = env::var(CONTROL_CAUSE_FILE_ENV) {
                        let _ = fs::write(path, b"cpu_timeout\n");
                    }
                    break cause;
                }
                next_cpu_poll = now + CPU_POLL_INTERVAL;
            }
            thread::sleep(Duration::from_millis(10));
        }
    } else {
        wait_for_direct_child(child_pid, &mut direct_status, &mut reaped_cpu_usec)
    };

    if invocation.cpu_budget_usec.is_none() {
        max_cpu_usec = reaped_cpu_usec;
        if matches!(cause, FirstCause::ExternalSignal { .. }) {
            match observed_cpu_usec(reaped_cpu_usec) {
                Ok(observed) => max_cpu_usec = max_cpu_usec.max(observed),
                Err(error) => {
                    eprintln!(
                        "nextest-cpu-wrapper: CPU accounting became unavailable while recording an external signal: {error}"
                    );
                    if let FirstCause::ExternalSignal { signal } = cause {
                        propagate_signal(signal);
                    }
                }
            }
        }
        if let FirstCause::AccountingUnavailable { ref error } = cause {
            let cleanup =
                hard_kill_descendants_and_reap(child_pid, &mut direct_status, &mut reaped_cpu_usec);
            return Err(match cleanup {
                Ok(()) => format!(
                    "CPU accounting became unavailable; stopped the test subtree rather than returning an unmeasured result: {error}"
                ),
                Err(cleanup_error) => format!(
                    "CPU accounting became unavailable ({error}); test-subtree cleanup also failed: {cleanup_error}"
                ),
            });
        }
        let completion = match cause {
            FirstCause::Exit(status) => completion_from_status(status)?,
            FirstCause::ExternalSignal { signal } => AttemptCompletion::SupervisorSignal { signal },
            FirstCause::CpuTimeout { .. } | FirstCause::AccountingUnavailable { .. } => {
                unreachable!("measurement-only wrapper cannot select this cause")
            }
        };
        let record = AttemptRecord::new_with_source(
            run_id,
            identity,
            max_cpu_usec,
            elapsed_ms(started)?,
            completion.clone(),
            CPU_SOURCE,
        );
        write_attempt_atomic(&record_dir, &record)?;
        return match completion {
            AttemptCompletion::Exit { .. } => direct_status
                .map(WrapperOutcome::Status)
                .ok_or_else(|| "exit completion is missing its child status".to_string()),
            AttemptCompletion::Signal { signal }
            | AttemptCompletion::SupervisorSignal { signal } => propagate_signal(signal),
            AttemptCompletion::CpuTimeout { .. } => {
                unreachable!("measurement-only wrapper cannot publish a CPU timeout")
            }
        };
    }

    let cleanup_signal = match cause {
        FirstCause::ExternalSignal { signal } => signal,
        _ => libc::SIGTERM,
    };
    if let Err(cleanup_error) = terminate_and_reap(
        child_pid,
        child_pid,
        cleanup_signal,
        invocation.termination_grace,
        &mut direct_status,
        &mut reaped_cpu_usec,
    ) {
        return Err(match &cause {
            FirstCause::AccountingUnavailable { error } => format!(
                "CPU accounting became unavailable ({error}); test-subtree cleanup also failed: {cleanup_error}"
            ),
            _ => format!("test-subtree cleanup failed: {cleanup_error}"),
        });
    }
    max_cpu_usec = max_cpu_usec.max(reaped_cpu_usec);
    if let FirstCause::AccountingUnavailable { ref error } = cause {
        return Err(format!(
            "CPU accounting became unavailable; stopped the test subtree rather than disabling its budget: {error}"
        ));
    }

    let completion = match cause {
        FirstCause::Exit(status) => completion_from_status(status)?,
        FirstCause::CpuTimeout { observed_cpu_usec } => AttemptCompletion::CpuTimeout {
            cpu_budget_usec: invocation
                .cpu_budget_usec
                .expect("CPU-timeout cause requires an enabled budget"),
            observed_cpu_usec,
        },
        FirstCause::ExternalSignal { signal } => AttemptCompletion::SupervisorSignal { signal },
        FirstCause::AccountingUnavailable { .. } => unreachable!("returned above"),
    };
    let record = AttemptRecord::new_with_source(
        run_id,
        identity,
        max_cpu_usec,
        elapsed_ms(started)?,
        completion.clone(),
        CPU_SOURCE_ENFORCED,
    );
    write_attempt_atomic(&record_dir, &record)?;

    match completion {
        AttemptCompletion::Exit { .. } => direct_status
            .map(WrapperOutcome::Status)
            .ok_or_else(|| "exit completion is missing its child status".to_string()),
        AttemptCompletion::Signal { signal } | AttemptCompletion::SupervisorSignal { signal } => {
            propagate_signal(signal)
        }
        AttemptCompletion::CpuTimeout { .. } => Ok(WrapperOutcome::CpuTimeout),
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
                || env::var_os(CPU_REPORT_PATH_ENV).is_some()
                || env::var_os(CPU_WRAPPER_ENV).is_some()
                || env::var_os(TEST_CPU_TIMEOUT_MULTIPLIER_ENV).is_some()
                || env::var_os(CONTROL_CAUSE_FILE_ENV).is_some()
                || env::var_os(CONTROL_PROC_ROOT_ENV).is_some()
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
        "burn-long" => {
            burn_cpu(500);
            Ok(ExitCode::SUCCESS)
        }
        "hang" | "stop-hang" => {
            let path = PathBuf::from(required_env(CONTROL_PID_FILE_ENV)?);
            fs::write(&path, format!("{}\n", std::process::id()))
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        "escaped-burner" => {
            if unsafe { libc::setsid() } < 0 {
                return Err(format!(
                    "cannot move stubborn control out of the original process group: {}",
                    io::Error::last_os_error()
                ));
            }
            let path = PathBuf::from(required_env(CONTROL_PID_FILE_ENV)?);
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            fs::write(&path, format!("{}\n", std::process::id()))
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            loop {
                burn_cpu(500);
            }
        }
        "wait-for-escaped-burner" => {
            let executable = env::current_exe().map_err(|error| error.to_string())?;
            let pid_path = PathBuf::from(required_env(CONTROL_PID_FILE_ENV)?);
            let child = Command::new(executable)
                .args(["--exact", "escaped-burner", "--nocapture"])
                .env(CONTROL_ARM_ENV, "1")
                .spawn()
                .map_err(|error| format!("cannot spawn stubborn CPU descendant: {error}"))?;
            drop(child);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pid_path.is_file() {
                if Instant::now() >= deadline {
                    return Err("stubborn CPU descendant did not start".into());
                }
                thread::sleep(Duration::from_millis(2));
            }
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
    control_command_with_limits(
        executable,
        test_binary,
        scratch,
        mode,
        attempt,
        10_000_000,
        2_000,
    )
}

fn control_command_with_limits(
    executable: &Path,
    test_binary: &Path,
    scratch: &Path,
    mode: &str,
    attempt: u64,
    cpu_budget_usec: u64,
    termination_grace_ms: u64,
) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--cpu-timeout-usec")
        .arg(cpu_budget_usec.to_string())
        .arg("--termination-grace-ms")
        .arg(termination_grace_ms.to_string())
        .arg("--")
        .arg(test_binary)
        .args(["--exact", mode, "--nocapture"])
        .current_dir(scratch)
        .env(CPU_BINARY_MAP_ENV, scratch.join("binary-map.json"))
        .env(CPU_RECORD_DIR_ENV, scratch.join("attempts"))
        .env(CPU_REPORT_PATH_ENV, scratch.join("unused-report.json"))
        .env(RUN_ID_ENV, "self-test-run")
        .env(PACKAGE_ENV, "fixture")
        .env(ATTEMPT_ENV, attempt.to_string())
        .env(CONTROL_ARM_ENV, "1")
        .env(CONTROL_CWD_ENV, scratch)
        .env(CONTROL_SENTINEL_ENV, "preserved")
        .env(CONTROL_PROC_ROOT_ENV, "/proc")
        .env(CPU_WRAPPER_ENV, executable)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

fn measurement_control_command(
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
        .env(CPU_REPORT_PATH_ENV, scratch.join("unused-report.json"))
        .env(RUN_ID_ENV, "self-test-run")
        .env(PACKAGE_ENV, "fixture")
        .env(ATTEMPT_ENV, attempt.to_string())
        .env(CONTROL_ARM_ENV, "1")
        .env(CONTROL_CWD_ENV, scratch)
        .env(CONTROL_SENTINEL_ENV, "preserved")
        .env(TEST_CPU_TIMEOUT_MULTIPLIER_ENV, "99")
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

fn find_record_attempt<'a>(
    records: &'a [AttemptRecord],
    test: &str,
    attempt: u64,
) -> Result<&'a AttemptRecord, String> {
    records
        .iter()
        .find(|record| record.identity.test == test && record.identity.attempt == attempt)
        .ok_or_else(|| format!("self-test did not find the {test:?} attempt {attempt} record"))
}

fn wait_for_file(path: &Path, label: &str) -> Result<(), String> {
    // This waits for CPU-driven controls as well as process startup. Keep the
    // wall allowance loose so host contention cannot turn a CPU test red.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.is_file() {
        if Instant::now() >= deadline {
            return Err(format!("{label} did not appear: {}", path.display()));
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn read_pid(path: &Path) -> Result<i32, String> {
    fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .trim()
        .parse::<i32>()
        .map_err(|error| format!("invalid PID in {}: {error}", path.display()))
}

fn process_exists(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn write_proc_control(
    root: &Path,
    pid: u32,
    parent: u32,
    process_group: u32,
    cpu: [u64; 4],
) -> Result<(), String> {
    let directory = root.join(pid.to_string());
    fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let stat = format!(
        "{pid} (control ) name) R {parent} {process_group} 0 0 0 0 0 0 0 0 {} {} {} {}\n",
        cpu[0], cpu[1], cpu[2], cpu[3]
    );
    fs::write(directory.join("stat"), stat)
        .map_err(|error| format!("cannot write procfs control: {error}"))
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
        let child_ready = pid.is_some_and(|pid| {
            let expected_group = if wrapped { pid } else { group };
            unsafe { libc::getpgid(pid) == expected_group }
        });
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

    let proc_control = scratch.0.join("proc-control");
    write_proc_control(&proc_control, 100, 1, 100, [99, 99, 99, 99])?;
    write_proc_control(&proc_control, 101, 100, 900, [3, 4, 5, 6])?;
    if descendant_cpu_usec_in(100, &proc_control)? == 0 {
        return Err("procfs subtree control omitted a descendant in another process group".into());
    }
    fs::create_dir(proc_control.join("102"))
        .map_err(|error| format!("cannot create malformed procfs control: {error}"))?;
    fs::write(proc_control.join("102/stat"), b"malformed\n")
        .map_err(|error| format!("cannot write malformed procfs control: {error}"))?;
    if descendant_cpu_usec_in(100, &proc_control).is_ok() {
        return Err("malformed process accounting was silently omitted".into());
    }

    let measurement =
        measurement_control_command(&executable, &test_binary, &scratch.0, "success", 2)
            .output()
            .map_err(|error| format!("cannot run measurement-only control: {error}"))?;
    if !measurement.status.success()
        || measurement.stdout != b"stdout-exact\n"
        || measurement.stderr != b"stderr-exact\n"
    {
        return Err(format!(
            "measurement-only control changed observable behavior or leaked its multiplier: {measurement:?}"
        ));
    }

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

    let forced = control_command_with_limits(
        &executable,
        &test_binary,
        &scratch.0,
        "burn-long",
        1,
        50_000,
        100,
    )
    .output()
    .map_err(|error| format!("cannot run forced CPU-timeout control: {error}"))?;
    if forced.status.code() != Some(CPU_TIMEOUT_EXIT.into()) {
        return Err(format!("forced CPU-timeout control returned {forced:?}"));
    }

    let stopped_pid_file = scratch.0.join("stopped-child.pid");
    let mut stopped_command = control_command_with_limits(
        &executable,
        &test_binary,
        &scratch.0,
        "stop-hang",
        1,
        50_000,
        100,
    );
    stopped_command.env(CONTROL_PID_FILE_ENV, &stopped_pid_file);
    let stopped = stopped_command
        .spawn()
        .map_err(|error| format!("cannot run stopped low-CPU control: {error}"))?;
    wait_for_file(&stopped_pid_file, "stopped low-CPU child PID")?;
    let stopped_pid = read_pid(&stopped_pid_file)?;
    if unsafe { libc::kill(-stopped_pid, libc::SIGSTOP) } != 0 {
        return Err(format!(
            "cannot stop low-CPU child group: {}",
            io::Error::last_os_error()
        ));
    }
    thread::sleep(Duration::from_millis(300));
    if !process_exists(stopped.id() as i32) {
        return Err("stopped low-CPU control hit the CPU bound while consuming no CPU".into());
    }
    unsafe {
        libc::kill(-stopped_pid, libc::SIGCONT);
    }
    if unsafe { libc::kill(-(stopped.id() as i32), libc::SIGTERM) } != 0 {
        return Err(format!(
            "cannot end stopped low-CPU control: {}",
            io::Error::last_os_error()
        ));
    }
    let stopped = stopped
        .wait_with_output()
        .map_err(|error| format!("cannot wait for stopped low-CPU control: {error}"))?;
    if stopped.status.signal() != Some(libc::SIGTERM) {
        return Err(format!(
            "stopped low-CPU control did not preserve external SIGTERM: {:?}",
            stopped.status
        ));
    }

    let accounting_pid_file = scratch.0.join("accounting-child.pid");
    let accounting_proc = scratch.0.join("accounting-proc");
    std::os::unix::fs::symlink("/proc", &accounting_proc)
        .map_err(|error| format!("cannot link accounting-control procfs: {error}"))?;
    let mut accounting_command = control_command_with_limits(
        &executable,
        &test_binary,
        &scratch.0,
        "hang",
        2,
        10_000_000,
        100,
    );
    accounting_command
        .env(CONTROL_PID_FILE_ENV, &accounting_pid_file)
        .env(CONTROL_PROC_ROOT_ENV, &accounting_proc);
    let accounting = accounting_command
        .spawn()
        .map_err(|error| format!("cannot run missing-accounting control: {error}"))?;
    wait_for_file(&accounting_pid_file, "missing-accounting child PID")?;
    let accounting_pid = read_pid(&accounting_pid_file)?;
    fs::remove_file(&accounting_proc)
        .map_err(|error| format!("cannot remove accounting-control procfs link: {error}"))?;
    let accounting = accounting
        .wait_with_output()
        .map_err(|error| format!("cannot wait for missing-accounting control: {error}"))?;
    if accounting.status.code() != Some(INFRASTRUCTURE_EXIT.into())
        || !String::from_utf8_lossy(&accounting.stderr)
            .contains("CPU accounting became unavailable")
    {
        return Err(format!(
            "missing-accounting control did not fail closed: {accounting:?}"
        ));
    }
    if process_exists(accounting_pid) {
        return Err(format!(
            "missing-accounting child {accounting_pid} survived fail-closed cleanup"
        ));
    }

    let stubborn_pid_file = scratch.0.join("escaped-burner-child.pid");
    let cause_file = scratch.0.join("cpu-first-cause");
    let mut race_command = control_command_with_limits(
        &executable,
        &test_binary,
        &scratch.0,
        "wait-for-escaped-burner",
        1,
        50_000,
        100,
    );
    race_command
        .env(CONTROL_PID_FILE_ENV, &stubborn_pid_file)
        .env(CONTROL_CAUSE_FILE_ENV, &cause_file);
    let race = race_command
        .spawn()
        .map_err(|error| format!("cannot run timeout race control: {error}"))?;
    wait_for_file(&stubborn_pid_file, "stubborn descendant PID")?;
    wait_for_file(&cause_file, "CPU first-cause marker")?;
    if unsafe { libc::kill(race.id() as i32, libc::SIGINT) } != 0 {
        return Err(format!(
            "cannot deliver late external signal to timeout race: {}",
            io::Error::last_os_error()
        ));
    }
    let race = race
        .wait_with_output()
        .map_err(|error| format!("cannot wait for timeout race control: {error}"))?;
    if race.status.code() != Some(CPU_TIMEOUT_EXIT.into()) {
        return Err(format!(
            "late external signal replaced the CPU first cause: {:?}",
            race.status
        ));
    }
    let stubborn_pid = read_pid(&stubborn_pid_file)?;
    if process_exists(stubborn_pid) {
        return Err(format!(
            "stubborn descendant {stubborn_pid} survived CPU-timeout cleanup"
        ));
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
    if records.len() != 10
        || identities
            != [
                "success",
                "failure",
                "signal",
                "tree",
                "hang",
                "burn-long",
                "stop-hang",
                "wait-for-escaped-burner",
                "catch-signal",
            ]
            .into_iter()
            .collect()
    {
        return Err(format!(
            "self-test expected ten exact atomic attempt identities, found {identities:?} ({} records)",
            records.len()
        ));
    }
    if records
        .iter()
        .any(|record| record.identity.binary != "fixture::bin/fixture_name")
    {
        return Err("self-test did not preserve the typed binary identity".into());
    }
    if find_record_attempt(&records, "success", 1)?.cpu_source != CPU_SOURCE_ENFORCED
        || find_record_attempt(&records, "success", 2)?.cpu_source != CPU_SOURCE
    {
        return Err(
            "self-test did not distinguish enforced and measurement-only CPU sources".into(),
        );
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
    let forced_record = find_record(&records, "burn-long")?;
    if !matches!(
        forced_record.completion,
        AttemptCompletion::CpuTimeout {
            cpu_budget_usec: 50_000,
            observed_cpu_usec,
        } if observed_cpu_usec >= 50_000
    ) {
        return Err(format!(
            "forced control did not retain its CPU-timeout boundary: {forced_record:?}"
        ));
    }
    if !matches!(
        find_record(&records, "stop-hang")?.completion,
        AttemptCompletion::SupervisorSignal {
            signal: libc::SIGTERM
        }
    ) {
        return Err("stopped low-CPU control was misclassified as a CPU timeout".into());
    }
    if !matches!(
        find_record(&records, "wait-for-escaped-burner")?.completion,
        AttemptCompletion::CpuTimeout {
            cpu_budget_usec: 50_000,
            observed_cpu_usec,
        } if observed_cpu_usec >= 50_000
    ) {
        return Err("late external signal replaced the CPU timeout first cause".into());
    }
    let duplicate = write_attempt_atomic(&scratch.0.join("attempts"), &records[0]);
    if duplicate.is_ok() {
        return Err("duplicate atomic attempt publication unexpectedly replaced a record".into());
    }
    println!(
        "nextest-cpu-wrapper: self-test PASS (process tree, success, failure, signal, wall timeout, CPU timeout, stopped low-CPU wall delay, missing accounting fail-closed, first-cause race, no survivors, typed identity, substituted path, atomic identity)"
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
        Ok(WrapperOutcome::Status(status)) => {
            ExitCode::from(status.code().unwrap_or(INFRASTRUCTURE_EXIT as i32) as u8)
        }
        Ok(WrapperOutcome::CpuTimeout) => ExitCode::from(CPU_TIMEOUT_EXIT),
        Err(error) => {
            eprintln!("nextest-cpu-wrapper: {error}");
            ExitCode::from(INFRASTRUCTURE_EXIT)
        }
    }
}
