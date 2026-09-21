use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
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
use hermit_manifest_plan::nextest_cpu::CPU_SOURCE_CGROUP_V2;
use hermit_manifest_plan::nextest_cpu::CPU_SOURCE_PROCFS_AND_REAPED;
use hermit_manifest_plan::nextest_cpu::CPU_SOURCE_REAPED;
use hermit_manifest_plan::nextest_cpu::Wait4Record;
use hermit_manifest_plan::nextest_cpu::read_attempt_records;
use hermit_manifest_plan::nextest_cpu::read_binary_map;
use hermit_manifest_plan::nextest_cpu::write_attempt_atomic;
use hermit_manifest_plan::nextest_cpu::write_binary_map_atomic;
use hermit_manifest_plan::timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV;

const ATTEMPT_ENV: &str = "__NEXTEST_ATTEMPT";
const RUN_ID_ENV: &str = "NEXTEST_RUN_ID";
const PACKAGE_ENV: &str = "CARGO_PKG_NAME";
const CONTROL_ARM_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL";
const CONTROL_ACCOUNTING_FAILURE_FILE_ENV: &str =
    "HERMIT_NEXTEST_CPU_CONTROL_ACCOUNTING_FAILURE_FILE";
const CONTROL_CWD_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CWD";
const CONTROL_CAUSE_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CAUSE_FILE";
const CONTROL_CGROUP_PATH_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CGROUP_PATH_FILE";
const CONTROL_CLEANUP_ERROR_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CLEANUP_ERROR";
const CONTROL_ENROLLMENT_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_ENROLLMENT_FILE";
const CONTROL_FINAL_CPU_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_FINAL_CPU_FILE";
const CONTROL_FINAL_READ_FAILURE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_FINAL_READ_FAILURE";
const CONTROL_FINAL_REGRESSION_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_FINAL_REGRESSION";
const CONTROL_PID_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_PID_FILE";
const CONTROL_PROC_ROOT_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_PROC_ROOT";
const CONTROL_RESERVED_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_RESERVED_FILE";
const CONTROL_RESUME_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_RESUME_FILE";
const CONTROL_SIGNAL_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_SIGNAL_FILE";
const CONTROL_WAIT4_RESUME_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_WAIT4_RESUME_FILE";
const CONTROL_WAIT4_STORED_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_WAIT4_STORED_FILE";
const CONTROL_SENTINEL_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_SENTINEL";
const INFRASTRUCTURE_EXIT: u8 = 70;
const CPU_TIMEOUT_EXIT: u8 = 124;
const NON_SIGNAL_CAUSE_RESERVED: i32 = -1;
// A wrapper owns one attempt cgroup. Sampling twice per second retains the
// original sub-second enforcement boundary without scanning unrelated tasks.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn file_identity(file: &File, label: &str) -> Result<FileIdentity, String> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(format!(
            "cannot inspect {label} identity: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

fn cgroup_text(file: &File, label: &str) -> Result<String, String> {
    const LIMIT: usize = 8192;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let count = file
            .read_at(&mut chunk, bytes.len() as u64)
            .map_err(|error| format!("cannot read {label}: {error}"))?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > LIMIT {
            return Err(format!("{label} exceeds the {LIMIT}-byte accounting bound"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|error| format!("{label} is not valid UTF-8: {error}"))
}

fn cgroup_field(file: &File, file_label: &str, field: &str) -> Result<u64, String> {
    let text = cgroup_text(file, file_label)?;
    let mut found = None;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        let value = words
            .next()
            .ok_or_else(|| format!("{file_label} has no value for {name:?}"))?;
        if words.next().is_some() {
            return Err(format!("{file_label} has extra fields on line {line:?}"));
        }
        if name == field {
            if found.is_some() {
                return Err(format!("{file_label} repeats field {field:?}"));
            }
            found = Some(
                value
                    .parse::<u64>()
                    .map_err(|error| format!("{file_label} has invalid {field}: {error}"))?,
            );
        }
    }
    found.ok_or_else(|| format!("{file_label} is missing field {field:?}"))
}

fn openat_file(directory: &File, name: &str, flags: i32, label: &str) -> Result<File, String> {
    let name = CString::new(name).expect("owned cgroup control names contain no NUL");
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(format!(
            "cannot open {label}: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn current_cgroup_directory() -> Result<PathBuf, String> {
    let raw = fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("cannot read /proc/self/cgroup: {error}"))?;
    let mut unified = raw.lines().filter_map(|line| line.strip_prefix("0::"));
    let path = unified
        .next()
        .ok_or_else(|| "/proc/self/cgroup has no unified cgroup v2 entry".to_string())?;
    if unified.next().is_some() {
        return Err("/proc/self/cgroup has multiple unified cgroup v2 entries".into());
    }
    let relative = Path::new(path.trim_start_matches('/'));
    if relative.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(format!(
            "unified cgroup path {path:?} is not a relative kernel path"
        ));
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

struct OwnedAttemptCgroup {
    parent: File,
    child: File,
    cpu_stat: File,
    events: File,
    procs_read: File,
    procs_write: File,
    kill: File,
    name: CString,
    identity: FileIdentity,
    path: PathBuf,
}

impl OwnedAttemptCgroup {
    fn create(identity: &AttemptIdentity) -> Result<Self, String> {
        let parent_path = current_cgroup_directory()?;
        Self::create_in(&parent_path, identity)
    }

    fn create_in(parent_path: &Path, identity: &AttemptIdentity) -> Result<Self, String> {
        let parent = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent_path)
            .map_err(|error| {
                format!(
                    "cannot open delegated cgroup {}: {error}",
                    parent_path.display()
                )
            })?;
        let mut filesystem = unsafe { std::mem::zeroed::<libc::statfs>() };
        if unsafe { libc::fstatfs(parent.as_raw_fd(), &mut filesystem) } != 0 {
            return Err(format!(
                "cannot inspect delegated cgroup filesystem: {}",
                io::Error::last_os_error()
            ));
        }
        const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
        if filesystem.f_type != CGROUP2_SUPER_MAGIC {
            return Err(format!(
                "delegated cgroup {} is not on cgroup v2",
                parent_path.display()
            ));
        }
        let name_text = format!(
            "hermit-nextest-attempt-{}-{}",
            std::process::id(),
            &identity.key()[..16]
        );
        let name = CString::new(name_text.as_bytes())
            .map_err(|_| "owned cgroup name contains a NUL byte".to_string())?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
            return Err(format!(
                "cannot create fresh attempt cgroup {}: {}",
                parent_path.join(&name_text).display(),
                io::Error::last_os_error()
            ));
        }
        let result = (|| {
            let child = openat_file(
                &parent,
                &name_text,
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                "owned attempt cgroup",
            )?;
            let identity = file_identity(&child, "owned attempt cgroup")?;
            let open_control = |control: &str, flags: i32| -> Result<File, String> {
                let file = openat_file(
                    &child,
                    control,
                    flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    &format!("owned attempt cgroup {control}"),
                )?;
                let control_identity = file_identity(&file, control)?;
                if control_identity.device != identity.device {
                    return Err(format!(
                        "owned attempt cgroup {control} is on device {}, expected {}",
                        control_identity.device, identity.device
                    ));
                }
                Ok(file)
            };
            let cpu_stat = open_control("cpu.stat", libc::O_RDONLY)?;
            let events = open_control("cgroup.events", libc::O_RDONLY)?;
            let procs_read = open_control("cgroup.procs", libc::O_RDONLY)?;
            let procs_write = open_control("cgroup.procs", libc::O_WRONLY)?;
            let kill = open_control("cgroup.kill", libc::O_WRONLY)?;
            let owned = Self {
                parent: parent.try_clone().map_err(|error| {
                    format!("cannot retain delegated cgroup descriptor: {error}")
                })?,
                child,
                cpu_stat,
                events,
                procs_read,
                procs_write,
                kill,
                name: name.clone(),
                identity,
                path: parent_path.join(&name_text),
            };
            owned.verify_identity()?;
            if owned.cpu_usage_usec()? != 0 {
                return Err("fresh attempt cgroup has nonzero cpu.stat usage_usec".into());
            }
            if owned.populated()? || !owned.procs_empty()? {
                return Err("fresh attempt cgroup is not empty before enrollment".into());
            }
            Ok(owned)
        })();
        match result {
            Ok(owned) => Ok(owned),
            Err(error) => {
                if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                    == 0
                {
                    Err(error)
                } else {
                    Err(format!(
                        "{error}; partial owned-cgroup initialization cleanup also failed: {}",
                        io::Error::last_os_error()
                    ))
                }
            }
        }
    }

    fn verify_identity(&self) -> Result<(), String> {
        let held = file_identity(&self.child, "held attempt cgroup")?;
        if held != self.identity {
            return Err("held attempt cgroup identity changed".into());
        }
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(format!(
                "cannot authenticate owned attempt cgroup path: {}",
                io::Error::last_os_error()
            ));
        }
        let named = FileIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        };
        if named != self.identity || stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err("owned attempt cgroup path was replaced".into());
        }
        Ok(())
    }

    fn enrollment_fd(&self) -> i32 {
        self.procs_write.as_raw_fd()
    }

    fn cpu_usage_usec(&self) -> Result<u64, String> {
        self.verify_identity()?;
        cgroup_field(&self.cpu_stat, "owned attempt cpu.stat", "usage_usec")
    }

    fn populated(&self) -> Result<bool, String> {
        self.verify_identity()?;
        match cgroup_field(&self.events, "owned attempt cgroup.events", "populated")? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!(
                "owned attempt cgroup.events has invalid populated value {value}"
            )),
        }
    }

    fn procs_empty(&self) -> Result<bool, String> {
        self.verify_identity()?;
        Ok(cgroup_text(&self.procs_read, "owned attempt cgroup.procs")?
            .trim()
            .is_empty())
    }

    fn kill(&self) -> Result<(), String> {
        self.verify_identity()?;
        loop {
            let written = unsafe { libc::write(self.kill.as_raw_fd(), b"1\n".as_ptr().cast(), 2) };
            if written == 2 {
                return Ok(());
            }
            if written < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(if written < 0 {
                format!(
                    "cannot kill owned attempt cgroup: {}",
                    io::Error::last_os_error()
                )
            } else {
                format!("short write to owned attempt cgroup.kill: {written} bytes")
            });
        }
    }

    fn remove_empty(&mut self) -> Result<(), String> {
        self.verify_identity()?;
        if self.populated()? || !self.procs_empty()? {
            return Err("cannot remove populated owned attempt cgroup".into());
        }
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(format!(
                "cannot remove owned empty attempt cgroup: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

struct PidFd {
    file: File,
    pid: u32,
}

impl PidFd {
    fn open(pid: u32) -> Result<Self, String> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(format!(
                "cannot open pidfd for direct child {pid}: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Self {
            file: unsafe { File::from_raw_fd(fd as i32) },
            pid,
        })
    }

    fn signal(&self, signal: i32) -> Result<(), String> {
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.file.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } == 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(format!(
                "cannot signal direct child pidfd with {signal}: {error}"
            ))
        }
    }
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

fn pause_after_wait4_storage_for_control() -> Result<(), String> {
    if env::var_os(CONTROL_ARM_ENV).is_none() {
        return Ok(());
    }
    let Some(marker) = env::var_os(CONTROL_WAIT4_STORED_FILE_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    let resume = PathBuf::from(required_env(CONTROL_WAIT4_RESUME_FILE_ENV)?);
    if !marker.exists() {
        fs::write(&marker, b"stored\n")
            .map_err(|error| format!("cannot publish wait4-storage control marker: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !resume.is_file() {
            if Instant::now() >= deadline {
                return Err("wait4-storage control was not released".into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    Ok(())
}

fn controlled_accounting_failure() -> Option<String> {
    env::var_os(CONTROL_ARM_ENV)?;
    env::var_os(CONTROL_ACCOUNTING_FAILURE_FILE_ENV)
        .filter(|path| Path::new(path).is_file())
        .map(|_| "owned attempt CPU accounting read failed under the self-test control".into())
}

fn wait4_record(
    waited: libc::pid_t,
    status: i32,
    usage: &libc::rusage,
) -> Result<Wait4Record, String> {
    let pid = u32::try_from(waited)
        .map_err(|_| format!("wait4 returned invalid positive PID {waited}"))?;
    let record = Wait4Record {
        pid,
        status,
        user_cpu_usec: timeval_usec(usage.ru_utime)?,
        system_cpu_usec: timeval_usec(usage.ru_stime)?,
    };
    record.validate()?;
    Ok(record)
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
    wait4: &mut Vec<Wait4Record>,
) -> Result<ChildPopulation, String> {
    loop {
        let mut raw_status = 0;
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
        let waited = unsafe { libc::wait4(-1, &mut raw_status, libc::WNOHANG, &mut usage) };
        if waited > 0 {
            let receipt = wait4_record(waited, raw_status, &usage)?;
            let used = receipt.cpu_usage_usec()?;
            wait4.push(receipt);
            pause_after_wait4_storage_for_control()?;
            *reaped_cpu_usec = reaped_cpu_usec
                .checked_add(used)
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

fn observed_cpu_usec(reaped_cpu_usec: u64) -> Result<u64, String> {
    reaped_cpu_usec
        .checked_add(descendant_cpu_usec(std::process::id())?)
        .ok_or_else(|| "nextest test subtree CPU total overflowed u64".to_string())
}

fn reap_owned_cgroup_until_empty(
    direct_pid: u32,
    cgroup: &OwnedAttemptCgroup,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
    wait4: &mut Vec<Wait4Record>,
    deadline: Instant,
) -> Result<bool, String> {
    loop {
        let population =
            reap_available_children(direct_pid, direct_status, reaped_cpu_usec, wait4)?;
        if population == ChildPopulation::Empty && !cgroup.populated()? && cgroup.procs_empty()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct CleanupOutcome {
    error: Option<String>,
}

fn terminate_owned_cgroup(
    direct_pidfd: &PidFd,
    cgroup: &OwnedAttemptCgroup,
    signal: i32,
    grace: Duration,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
    wait4: &mut Vec<Wait4Record>,
) -> Result<CleanupOutcome, String> {
    let mut first_error = if env::var_os(CONTROL_ARM_ENV).is_some()
        && env::var_os(CONTROL_CLEANUP_ERROR_ENV).is_some()
    {
        Some("gentle cleanup failed under the self-test control".into())
    } else if direct_status.is_none() {
        direct_pidfd.signal(signal).err()
    } else {
        None
    };
    if first_error.is_none() {
        match reap_owned_cgroup_until_empty(
            direct_pidfd.pid,
            cgroup,
            direct_status,
            reaped_cpu_usec,
            wait4,
            Instant::now() + grace,
        ) {
            Ok(true) => return Ok(CleanupOutcome { error: None }),
            Ok(false) => {}
            Err(error) => first_error = Some(error),
        }
    }
    let deadline = Instant::now() + CHILD_REAP_DEADLINE;
    let mut hard_error = cgroup.kill().err();
    let hard_reaped = loop {
        let population = match reap_available_children(
            direct_pidfd.pid,
            direct_status,
            reaped_cpu_usec,
            wait4,
        ) {
            Ok(population) => population,
            Err(error) => {
                hard_error.get_or_insert(error);
                ChildPopulation::Present
            }
        };
        let populated = match cgroup.populated() {
            Ok(populated) => populated,
            Err(error) => {
                hard_error.get_or_insert(error);
                true
            }
        };
        let procs_empty = match cgroup.procs_empty() {
            Ok(empty) => empty,
            Err(error) => {
                hard_error.get_or_insert(error);
                false
            }
        };
        if population == ChildPopulation::Empty && !populated && procs_empty {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(50));
    };
    if !hard_reaped {
        return Err(format!(
            "cannot prove the owned attempt cgroup is empty and adopted descendants were reaped after cgroup.kill: gentle={first_error:?}; hard={hard_error:?}"
        ));
    }
    let error = match (first_error, hard_error) {
        (None, None) => None,
        (gentle, hard) => Some(format!(
            "test subtree required hard cleanup after a lifecycle error: gentle={gentle:?}; hard={hard:?}"
        )),
    };
    Ok(CleanupOutcome { error })
}

fn kill_direct_child_and_reap(
    direct_pid: u32,
    direct_pidfd: &PidFd,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
    wait4: &mut Vec<Wait4Record>,
) -> Result<(), String> {
    let mut first_error = direct_pidfd.signal(libc::SIGKILL).err();
    let deadline = Instant::now() + CHILD_REAP_DEADLINE;
    loop {
        match reap_available_children(direct_pid, direct_status, reaped_cpu_usec, wait4) {
            Ok(ChildPopulation::Empty) => {
                return match first_error {
                    Some(error) => Err(format!(
                        "direct child required cleanup after a lifecycle error: {error}"
                    )),
                    None => Ok(()),
                };
            }
            Ok(ChildPopulation::Present) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "cannot prove the direct child was reaped after pidfd SIGKILL: {first_error:?}"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn kill_owned_cgroup_and_reap(
    direct_pid: u32,
    cgroup: &OwnedAttemptCgroup,
    direct_status: &mut Option<ExitStatus>,
    reaped_cpu_usec: &mut u64,
    wait4: &mut Vec<Wait4Record>,
) -> Result<(), String> {
    cgroup.kill()?;
    if reap_owned_cgroup_until_empty(
        direct_pid,
        cgroup,
        direct_status,
        reaped_cpu_usec,
        wait4,
        Instant::now() + CHILD_REAP_DEADLINE,
    )? {
        Ok(())
    } else {
        Err("cannot prove the owned attempt cgroup is empty and adopted descendants were reaped after cgroup.kill".into())
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

fn reserved_or_external(candidate: FirstCause, already_reserved: bool) -> FirstCause {
    if already_reserved {
        candidate
    } else {
        reserve_or_external(candidate)
    }
}

fn pause_after_terminal_reservation_for_control() -> Result<(), String> {
    if env::var_os(CONTROL_ARM_ENV).is_none() {
        return Ok(());
    }
    let Some(marker) = env::var_os(CONTROL_RESERVED_FILE_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    let resume = PathBuf::from(required_env(CONTROL_RESUME_FILE_ENV)?);
    fs::write(&marker, b"reserved\n")
        .map_err(|error| format!("cannot publish terminal-reservation control marker: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !resume.is_file() {
        if Instant::now() >= deadline {
            return Err("terminal-reservation control was not released".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
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
    wait4: &mut Vec<Wait4Record>,
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
            let receipt = match wait4_record(waited, raw_status, &usage) {
                Ok(receipt) => receipt,
                Err(error) => return FirstCause::AccountingUnavailable { error },
            };
            let used = match receipt.cpu_usage_usec() {
                Ok(used) => used,
                Err(error) => return FirstCause::AccountingUnavailable { error },
            };
            wait4.push(receipt);
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
            if let Err(error) = pause_after_wait4_storage_for_control() {
                return FirstCause::AccountingUnavailable { error };
            }
            if let Err(signal) = reserve_non_signal_cause() {
                return FirstCause::ExternalSignal { signal };
            }
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

    let mut attempt_cgroup = invocation
        .cpu_budget_usec
        .map(|_| OwnedAttemptCgroup::create(&identity))
        .transpose()?;
    if let (Some(cgroup), Some(path)) = (
        attempt_cgroup.as_ref(),
        env::var_os(CONTROL_CGROUP_PATH_FILE_ENV),
    ) {
        if let Err(error) = fs::write(path, format!("{}\n", cgroup.path.display())) {
            let cleanup = attempt_cgroup
                .as_mut()
                .expect("control path requires an owned cgroup")
                .remove_empty();
            return Err(match cleanup {
                Ok(()) => format!("cannot publish owned-cgroup control path: {error}"),
                Err(cleanup_error) => format!(
                    "cannot publish owned-cgroup control path ({error}); owned empty cgroup cleanup also failed: {cleanup_error}"
                ),
            });
        }
    }

    let mut command = Command::new(program);
    command.args(child_args);
    command.env_remove(CPU_BINARY_MAP_ENV);
    command.env_remove(CPU_RECORD_DIR_ENV);
    command.env_remove(CPU_REPORT_PATH_ENV);
    command.env_remove(CPU_WRAPPER_ENV);
    command.env_remove(TEST_CPU_TIMEOUT_MULTIPLIER_ENV);
    command.env_remove(CONTROL_CAUSE_FILE_ENV);
    command.env_remove(CONTROL_ACCOUNTING_FAILURE_FILE_ENV);
    command.env_remove(CONTROL_CGROUP_PATH_FILE_ENV);
    command.env_remove(CONTROL_CLEANUP_ERROR_ENV);
    command.env_remove(CONTROL_FINAL_CPU_FILE_ENV);
    command.env_remove(CONTROL_FINAL_READ_FAILURE_ENV);
    command.env_remove(CONTROL_FINAL_REGRESSION_ENV);
    command.env_remove(CONTROL_PROC_ROOT_ENV);
    command.env_remove(CONTROL_RESERVED_FILE_ENV);
    command.env_remove(CONTROL_RESUME_FILE_ENV);
    command.env_remove(CONTROL_WAIT4_RESUME_FILE_ENV);
    command.env_remove(CONTROL_WAIT4_STORED_FILE_ENV);
    if invocation.cpu_budget_usec.is_some() {
        command.process_group(0);
        let enrollment_fd = attempt_cgroup
            .as_ref()
            .expect("budgeted path created an attempt cgroup")
            .enrollment_fd();
        unsafe {
            command.pre_exec(move || {
                loop {
                    let written = libc::write(enrollment_fd, b"0\n".as_ptr().cast(), 2);
                    if written == 2 {
                        return Ok(());
                    }
                    if written < 0 {
                        let error = *libc::__errno_location();
                        if error == libc::EINTR {
                            continue;
                        }
                        return Err(io::Error::from_raw_os_error(error));
                    }
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
            });
        }
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let cleanup = attempt_cgroup
                .as_mut()
                .map(OwnedAttemptCgroup::remove_empty)
                .transpose();
            return Err(match cleanup {
                Ok(_) => format!("cannot execute nextest test command: {error}"),
                Err(cleanup_error) => format!(
                    "cannot execute nextest test command ({error}); owned empty cgroup cleanup also failed: {cleanup_error}"
                ),
            });
        }
    };
    let child_pid = child.id();
    let direct_pidfd = match PidFd::open(child_pid) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            if let Some(cgroup) = attempt_cgroup.as_mut() {
                let mut direct_status = None;
                let mut reaped_cpu_usec = 0;
                let mut wait4 = Vec::new();
                drop(child);
                let cleanup = kill_owned_cgroup_and_reap(
                    child_pid,
                    cgroup,
                    &mut direct_status,
                    &mut reaped_cpu_usec,
                    &mut wait4,
                )
                .and_then(|()| cgroup.remove_empty());
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => {
                        format!("{error}; owned cgroup cleanup also failed: {cleanup_error}")
                    }
                });
            }
            let mut child = child;
            let cleanup = child.kill().and_then(|()| child.wait()).map(|_| ());
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => {
                    format!("{error}; direct-child cleanup also failed: {cleanup_error}")
                }
            });
        }
    };
    drop(child);
    let mut direct_status = None;
    let mut reaped_cpu_usec = 0u64;
    let mut wait4 = Vec::new();
    let mut max_cpu_usec = 0u64;
    let cause = if let Some(cpu_budget_usec) = invocation.cpu_budget_usec {
        let cgroup = attempt_cgroup
            .as_ref()
            .expect("budgeted path created an attempt cgroup");
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
                &mut wait4,
            ) {
                Ok(population) => population,
                Err(error) => {
                    break reserve_or_external(FirstCause::AccountingUnavailable { error });
                }
            };
            if let Some(supervisor_signal) = received_external_signal() {
                break FirstCause::ExternalSignal {
                    signal: supervisor_signal,
                };
            }
            let now = Instant::now();
            if let Some(status) = direct_status.filter(|status| !status.success()) {
                match reserve_non_signal_cause() {
                    Ok(()) => {
                        if let Err(error) = pause_after_terminal_reservation_for_control() {
                            break FirstCause::AccountingUnavailable { error };
                        }
                        break FirstCause::Exit(status);
                    }
                    Err(signal) => break FirstCause::ExternalSignal { signal },
                }
            }
            let cgroup_empty = match (cgroup.populated(), cgroup.procs_empty()) {
                (Ok(false), Ok(true)) => true,
                (Ok(_), Ok(_)) => false,
                (Err(error), _) | (_, Err(error)) => {
                    break reserve_or_external(FirstCause::AccountingUnavailable { error });
                }
            };
            let completed_success = direct_status.is_some_and(|status| status.success())
                && population == ChildPopulation::Empty
                && cgroup_empty;
            let already_reserved = if completed_success {
                match reserve_non_signal_cause() {
                    Ok(()) => {
                        if let Err(error) = pause_after_terminal_reservation_for_control() {
                            break FirstCause::AccountingUnavailable { error };
                        }
                        true
                    }
                    Err(signal) => break FirstCause::ExternalSignal { signal },
                }
            } else {
                false
            };
            if now >= next_cpu_poll || completed_success {
                if let Some(error) = controlled_accounting_failure() {
                    break reserved_or_external(
                        FirstCause::AccountingUnavailable { error },
                        already_reserved,
                    );
                }
                let observed = match cgroup.cpu_usage_usec() {
                    Ok(observed) => observed,
                    Err(error) => {
                        break reserved_or_external(
                            FirstCause::AccountingUnavailable { error },
                            already_reserved,
                        );
                    }
                };
                if observed < max_cpu_usec {
                    break reserved_or_external(
                        FirstCause::AccountingUnavailable {
                            error: format!(
                                "owned attempt cpu.stat regressed from {max_cpu_usec}us to {observed}us"
                            ),
                        },
                        already_reserved,
                    );
                }
                max_cpu_usec = observed;
                if observed >= cpu_budget_usec {
                    let cause = reserved_or_external(
                        FirstCause::CpuTimeout {
                            observed_cpu_usec: observed,
                        },
                        already_reserved,
                    );
                    if !matches!(cause, FirstCause::CpuTimeout { .. }) {
                        break cause;
                    }
                    if let Ok(path) = env::var(CONTROL_CAUSE_FILE_ENV) {
                        let _ = fs::write(path, b"cpu_timeout\n");
                    }
                    break cause;
                }
                if now >= next_cpu_poll {
                    next_cpu_poll = now + CPU_POLL_INTERVAL;
                }
            }
            if completed_success {
                break FirstCause::Exit(
                    direct_status.expect("completed success has a direct child status"),
                );
            }
            if direct_status.is_none() && population == ChildPopulation::Empty {
                break reserved_or_external(
                    FirstCause::AccountingUnavailable {
                        error:
                            "nextest test subtree disappeared before its direct child was reaped"
                                .into(),
                    },
                    already_reserved,
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    } else {
        wait_for_direct_child(
            child_pid,
            &mut direct_status,
            &mut reaped_cpu_usec,
            &mut wait4,
        )
    };

    if invocation.cpu_budget_usec.is_none() {
        max_cpu_usec = reaped_cpu_usec;
        let mut cpu_source = CPU_SOURCE_REAPED;
        if matches!(cause, FirstCause::ExternalSignal { .. }) {
            match observed_cpu_usec(reaped_cpu_usec) {
                Ok(observed) => {
                    max_cpu_usec = max_cpu_usec.max(observed);
                    cpu_source = CPU_SOURCE_PROCFS_AND_REAPED;
                }
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
            let cleanup = kill_direct_child_and_reap(
                child_pid,
                &direct_pidfd,
                &mut direct_status,
                &mut reaped_cpu_usec,
                &mut wait4,
            );
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
            cpu_source,
        )
        .with_wait4(wait4);
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

    let cpu_budget_usec = invocation
        .cpu_budget_usec
        .expect("budgeted path requires an enabled CPU budget");
    let cleanup_signal = match cause {
        FirstCause::ExternalSignal { signal } => signal,
        _ => libc::SIGTERM,
    };
    let cgroup = attempt_cgroup
        .as_mut()
        .expect("budgeted path created an attempt cgroup");
    let cleanup = match terminate_owned_cgroup(
        &direct_pidfd,
        cgroup,
        cleanup_signal,
        invocation.termination_grace,
        &mut direct_status,
        &mut reaped_cpu_usec,
        &mut wait4,
    ) {
        Ok(cleanup) => cleanup,
        Err(cleanup_error) => {
            return Err(match &cause {
                FirstCause::AccountingUnavailable { error } => format!(
                    "CPU accounting became unavailable ({error}); test-subtree cleanup also failed: {cleanup_error}"
                ),
                _ => format!(
                    "primary outcome {cause:?}; test-subtree cleanup failed: {cleanup_error}"
                ),
            });
        }
    };

    let final_cpu_result = if env::var_os(CONTROL_ARM_ENV).is_some()
        && env::var_os(CONTROL_FINAL_READ_FAILURE_ENV).is_some()
    {
        Err("final owned cgroup CPU read failed under the self-test control".into())
    } else {
        cgroup
            .cpu_usage_usec()
            .map_err(|error| format!("final owned cgroup CPU accounting became unavailable: {error}"))
            .and_then(|actual| {
                if env::var_os(CONTROL_ARM_ENV).is_some()
                    && env::var_os(CONTROL_FINAL_REGRESSION_ENV).is_some()
                {
                    max_cpu_usec.checked_sub(1).ok_or_else(|| {
                        "final-regression control had no prior positive CPU observation".into()
                    })
                } else {
                    Ok(actual)
                }
            })
            .and_then(|final_cpu_usec| {
                if final_cpu_usec < max_cpu_usec {
                    Err(format!(
                        "final owned attempt cpu.stat regressed from {max_cpu_usec}us to {final_cpu_usec}us"
                    ))
                } else {
                    Ok(final_cpu_usec)
                }
            })
    };
    let mut finalization_errors = Vec::new();
    if let FirstCause::AccountingUnavailable { error } = &cause {
        finalization_errors.push(format!("CPU accounting became unavailable: {error}"));
    }
    if let Some(error) = cleanup.error {
        finalization_errors.push(format!("cleanup completed with an error: {error}"));
    }
    let final_cpu_usec = match final_cpu_result {
        Ok(value) => Some(value),
        Err(error) => {
            finalization_errors.push(error);
            None
        }
    };
    if let (Some(final_cpu_usec), Some(path)) =
        (final_cpu_usec, env::var_os(CONTROL_FINAL_CPU_FILE_ENV))
    {
        if let Err(error) = fs::write(path, format!("{final_cpu_usec}\n")) {
            finalization_errors.push(format!("cannot publish final cgroup CPU control: {error}"));
        }
    }
    if let Err(error) = cgroup.remove_empty() {
        finalization_errors.push(format!("owned empty cgroup removal failed: {error}"));
    }
    if !finalization_errors.is_empty() {
        return Err(format!(
            "primary outcome {cause:?}; {}",
            finalization_errors.join("; ")
        ));
    }
    max_cpu_usec = final_cpu_usec.expect("successful finalization retained final CPU");

    let cause = match cause {
        FirstCause::Exit(status) if status.success() && max_cpu_usec >= cpu_budget_usec => {
            FirstCause::CpuTimeout {
                observed_cpu_usec: max_cpu_usec,
            }
        }
        cause => cause,
    };
    let completion = match cause {
        FirstCause::Exit(status) => completion_from_status(status)?,
        FirstCause::CpuTimeout { observed_cpu_usec } => AttemptCompletion::CpuTimeout {
            cpu_budget_usec,
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
        CPU_SOURCE_CGROUP_V2,
    )
    .with_wait4(wait4);
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
        "success" | "failure" | "wait4-race" => {
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
        "failure-after-burn" => {
            burn_cpu(150);
            Ok(ExitCode::from(23))
        }
        "final-regression" => {
            burn_cpu(100);
            Ok(ExitCode::SUCCESS)
        }
        "peer-sleep" => {
            let enrollment = PathBuf::from(required_env(CONTROL_ENROLLMENT_FILE_ENV)?);
            fs::write(
                &enrollment,
                fs::read_to_string("/proc/self/cgroup").map_err(|error| {
                    format!("cannot read enrolled control cgroup membership: {error}")
                })?,
            )
            .map_err(|error| {
                format!(
                    "cannot write enrolled control membership {}: {error}",
                    enrollment.display()
                )
            })?;
            fs::write(
                required_env(CONTROL_PID_FILE_ENV)?,
                format!("{}\n", std::process::id()),
            )
            .map_err(|error| format!("cannot write enrolled control PID: {error}"))?;
            thread::sleep(Duration::from_millis(700));
            Ok(ExitCode::SUCCESS)
        }
        "auto-reap-ignored" | "auto-reap-nocldwait" => {
            unsafe {
                if mode == "auto-reap-ignored" {
                    if libc::signal(libc::SIGCHLD, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(format!(
                            "cannot install SIGCHLD=SIG_IGN control: {}",
                            io::Error::last_os_error()
                        ));
                    }
                } else {
                    let mut action = std::mem::zeroed::<libc::sigaction>();
                    action.sa_sigaction = libc::SIG_DFL;
                    libc::sigemptyset(&mut action.sa_mask);
                    action.sa_flags = libc::SA_NOCLDWAIT;
                    if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
                        return Err(format!(
                            "cannot install SA_NOCLDWAIT control: {}",
                            io::Error::last_os_error()
                        ));
                    }
                }
            }
            let executable = env::current_exe().map_err(|error| error.to_string())?;
            let child = Command::new(executable)
                .args(["--exact", "burn-auto-reaped", "--nocapture"])
                .env(CONTROL_ARM_ENV, "1")
                .spawn()
                .map_err(|error| format!("cannot spawn auto-reaped CPU child: {error}"))?;
            let child_pid = child.id() as libc::pid_t;
            drop(child);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let waited =
                    unsafe { libc::waitpid(child_pid, std::ptr::null_mut(), libc::WNOHANG) };
                if waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                    break;
                }
                if waited > 0 {
                    return Err("auto-reap control unexpectedly returned a wait receipt".into());
                }
                if waited < 0 {
                    return Err(format!(
                        "auto-reap control waitpid failed: {}",
                        io::Error::last_os_error()
                    ));
                }
                if Instant::now() >= deadline {
                    return Err("auto-reap control child did not disappear".into());
                }
                thread::sleep(Duration::from_millis(2));
            }
            Ok(ExitCode::SUCCESS)
        }
        "burn-auto-reaped" => {
            burn_cpu(250);
            Ok(ExitCode::SUCCESS)
        }
        "hang" | "stop-hang" | "measurement-signal" => {
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
        "wait-for-escaped-burner" | "exit-after-escaped-burner" => {
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
            if mode == "exit-after-escaped-burner" {
                Ok(ExitCode::SUCCESS)
            } else {
                loop {
                    thread::sleep(Duration::from_secs(60));
                }
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
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let raw = fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if raw.ends_with('\n') {
            return raw
                .trim()
                .parse::<i32>()
                .map_err(|error| format!("invalid PID in {}: {error}", path.display()));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "PID in {} was not completely written",
                path.display()
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn require_refusal_and_removed_cgroup(
    output: &std::process::Output,
    cgroup_path_file: &Path,
    expected_error: &str,
    label: &str,
) -> Result<(), String> {
    let path = PathBuf::from(
        fs::read_to_string(cgroup_path_file)
            .map_err(|error| format!("cannot read {label} cgroup path: {error}"))?
            .trim(),
    );
    if output.status.code() != Some(INFRASTRUCTURE_EXIT.into())
        || !String::from_utf8_lossy(&output.stderr).contains(expected_error)
        || path.exists()
    {
        return Err(format!(
            "{label} did not refuse and remove its proved-empty owned cgroup {}: {output:?}",
            path.display()
        ));
    }
    Ok(())
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

    let ownership_identity = AttemptIdentity {
        package: "fixture".into(),
        binary: "fixture::bin/fixture_name".into(),
        test: "cgroup-ownership".into(),
        attempt: 1,
    };
    let mut owned = OwnedAttemptCgroup::create(&ownership_identity)?;
    if OwnedAttemptCgroup::create(&ownership_identity).is_ok() {
        return Err("owned attempt cgroup creation clobbered an existing name".into());
    }
    owned.remove_empty()?;

    if unsafe { libc::geteuid() } != 0 {
        let read_only_identity = AttemptIdentity {
            test: "read-only-cgroup-parent".into(),
            ..ownership_identity.clone()
        };
        let nested_identity = AttemptIdentity {
            test: "read-only-cgroup-child".into(),
            ..ownership_identity.clone()
        };
        let mut read_only_parent = OwnedAttemptCgroup::create(&read_only_identity)?;
        fs::set_permissions(&read_only_parent.path, fs::Permissions::from_mode(0o555))
            .map_err(|error| format!("cannot make cgroup delegation control read-only: {error}"))?;
        let nested = OwnedAttemptCgroup::create_in(&read_only_parent.path, &nested_identity);
        fs::set_permissions(&read_only_parent.path, fs::Permissions::from_mode(0o755)).map_err(
            |error| format!("cannot restore cgroup delegation control permissions: {error}"),
        )?;
        if let Ok(mut nested) = nested {
            nested.remove_empty()?;
            read_only_parent.remove_empty()?;
            return Err("read-only cgroup delegation was accepted".into());
        }
        read_only_parent.remove_empty()?;
    }
    let missing_parent = scratch.0.join("missing-cgroup-parent");
    if OwnedAttemptCgroup::create_in(&missing_parent, &ownership_identity).is_ok() {
        return Err("missing cgroup delegation was accepted".into());
    }
    let replaced_parent = scratch.0.join("replaced-cgroup-parent");
    std::os::unix::fs::symlink(current_cgroup_directory()?, &replaced_parent)
        .map_err(|error| format!("cannot create replaced-cgroup control: {error}"))?;
    if OwnedAttemptCgroup::create_in(&replaced_parent, &ownership_identity).is_ok() {
        return Err("a substituted cgroup delegation path was accepted".into());
    }

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

    let measurement_signal_pid_file = scratch.0.join("measurement-signal-child.pid");
    let mut measurement_signal_command = measurement_control_command(
        &executable,
        &test_binary,
        &scratch.0,
        "measurement-signal",
        1,
    );
    measurement_signal_command.env(CONTROL_PID_FILE_ENV, &measurement_signal_pid_file);
    let measurement_signal = measurement_signal_command
        .spawn()
        .map_err(|error| format!("cannot run measurement-signal control: {error}"))?;
    wait_for_file(&measurement_signal_pid_file, "measurement-signal child PID")?;
    if unsafe { libc::kill(-(measurement_signal.id() as i32), libc::SIGTERM) } != 0 {
        return Err(format!(
            "cannot signal measurement-only process group: {}",
            io::Error::last_os_error()
        ));
    }
    let measurement_signal = measurement_signal
        .wait_with_output()
        .map_err(|error| format!("cannot wait for measurement-signal control: {error}"))?;
    if measurement_signal.status.signal() != Some(libc::SIGTERM) {
        return Err(format!(
            "measurement-signal control did not preserve SIGTERM: {:?}",
            measurement_signal.status
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

    let reserved_file = scratch.0.join("terminal-cause-reserved");
    let resume_file = scratch.0.join("resume-after-terminal-cause");
    let mut reserved_exit_command =
        control_command(&executable, &test_binary, &scratch.0, "success", 3);
    reserved_exit_command
        .env(CONTROL_RESERVED_FILE_ENV, &reserved_file)
        .env(CONTROL_RESUME_FILE_ENV, &resume_file);
    let reserved_exit = reserved_exit_command
        .spawn()
        .map_err(|error| format!("cannot run reserved-exit control: {error}"))?;
    wait_for_file(&reserved_file, "terminal-cause reservation marker")?;
    if unsafe { libc::kill(reserved_exit.id() as i32, libc::SIGINT) } != 0 {
        return Err(format!(
            "cannot deliver signal after terminal-cause reservation: {}",
            io::Error::last_os_error()
        ));
    }
    // Give the installed handler time to observe the signal while the wrapper
    // remains paused inside the already-reserved non-signal cause.
    thread::sleep(Duration::from_millis(50));
    fs::write(&resume_file, b"resume\n")
        .map_err(|error| format!("cannot release reserved-exit control: {error}"))?;
    let reserved_exit = reserved_exit
        .wait_with_output()
        .map_err(|error| format!("cannot wait for reserved-exit control: {error}"))?;
    if !reserved_exit.status.success()
        || reserved_exit.stdout != b"stdout-exact\n"
        || reserved_exit.stderr != b"stderr-exact\n"
    {
        return Err(format!(
            "a signal delivered after terminal observation replaced the exit cause: {reserved_exit:?}"
        ));
    }

    let wait4_stored_file = scratch.0.join("wait4-receipt-stored");
    let wait4_resume_file = scratch.0.join("resume-after-wait4-receipt");
    let mut wait4_race_command =
        control_command(&executable, &test_binary, &scratch.0, "wait4-race", 1);
    wait4_race_command
        .env(CONTROL_WAIT4_STORED_FILE_ENV, &wait4_stored_file)
        .env(CONTROL_WAIT4_RESUME_FILE_ENV, &wait4_resume_file);
    let wait4_race = wait4_race_command
        .spawn()
        .map_err(|error| format!("cannot run wait4-receipt race control: {error}"))?;
    wait_for_file(&wait4_stored_file, "stored wait4 receipt marker")?;
    if unsafe { libc::kill(wait4_race.id() as i32, libc::SIGINT) } != 0 {
        return Err(format!(
            "cannot deliver signal after wait4 receipt storage: {}",
            io::Error::last_os_error()
        ));
    }
    fs::write(&wait4_resume_file, b"resume\n")
        .map_err(|error| format!("cannot release wait4-receipt control: {error}"))?;
    let wait4_race = wait4_race
        .wait_with_output()
        .map_err(|error| format!("cannot wait for wait4-receipt race control: {error}"))?;
    if wait4_race.status.signal() != Some(libc::SIGINT) {
        return Err(format!(
            "wait4-receipt race did not preserve the winning supervisor signal: {wait4_race:?}"
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

    for mode in ["auto-reap-ignored", "auto-reap-nocldwait"] {
        let auto_reaped = control_command_with_limits(
            &executable,
            &test_binary,
            &scratch.0,
            mode,
            1,
            150_000,
            100,
        )
        .output()
        .map_err(|error| format!("cannot run {mode} control: {error}"))?;
        if auto_reaped.status.code() != Some(CPU_TIMEOUT_EXIT.into()) {
            return Err(format!(
                "{mode} CPU was absent from owned-cgroup accounting: {auto_reaped:?}"
            ));
        }
    }

    let exit_over_budget = control_command_with_limits(
        &executable,
        &test_binary,
        &scratch.0,
        "failure-after-burn",
        1,
        50_000,
        100,
    )
    .output()
    .map_err(|error| format!("cannot run nonzero-exit CPU race control: {error}"))?;
    if exit_over_budget.status.code() != Some(23) {
        return Err(format!(
            "a later budget check replaced an observed nonzero exit: {exit_over_budget:?}"
        ));
    }

    let peer_pid_file = scratch.0.join("peer-exclusion-child.pid");
    let enrollment_file = scratch.0.join("peer-exclusion-membership");
    let cgroup_path_file = scratch.0.join("peer-exclusion-cgroup");
    let peer_final_cpu_file = scratch.0.join("peer-exclusion-final-cpu");
    let peer = Command::new(&executable)
        .args(["--exact", "burn-long", "--nocapture"])
        .env(CONTROL_ARM_ENV, "1")
        .spawn()
        .map_err(|error| format!("cannot start unrelated CPU peer: {error}"))?;
    let peer_pid = peer.id();
    let mut peer_wrapper = control_command_with_limits(
        &executable,
        &test_binary,
        &scratch.0,
        "peer-sleep",
        1,
        100_000,
        100,
    );
    peer_wrapper
        .env(CONTROL_PID_FILE_ENV, &peer_pid_file)
        .env(CONTROL_ENROLLMENT_FILE_ENV, &enrollment_file)
        .env(CONTROL_CGROUP_PATH_FILE_ENV, &cgroup_path_file)
        .env(CONTROL_FINAL_CPU_FILE_ENV, &peer_final_cpu_file);
    let peer_wrapper = peer_wrapper
        .spawn()
        .map_err(|error| format!("cannot start peer-exclusion wrapper: {error}"))?;
    wait_for_file(&peer_pid_file, "peer-exclusion child PID")?;
    wait_for_file(&enrollment_file, "peer-exclusion child membership")?;
    wait_for_file(&cgroup_path_file, "peer-exclusion cgroup path")?;
    let enrolled_pid = read_pid(&peer_pid_file)?;
    let owned_path = PathBuf::from(
        fs::read_to_string(&cgroup_path_file)
            .map_err(|error| format!("cannot read peer-exclusion cgroup path: {error}"))?
            .trim(),
    );
    let owned_procs = fs::read_to_string(owned_path.join("cgroup.procs"))
        .map_err(|error| format!("cannot read peer-exclusion cgroup.procs: {error}"))?;
    let owned_pids = owned_procs
        .split_whitespace()
        .map(|pid| pid.parse::<u32>().map_err(|error| error.to_string()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if !owned_pids.contains(&(enrolled_pid as u32)) || owned_pids.contains(&peer_pid) {
        return Err(format!(
            "owned attempt enrollment or peer exclusion was wrong: child={enrolled_pid}, peer={peer_pid}, members={owned_pids:?}"
        ));
    }
    let peer_wrapper = peer_wrapper
        .wait_with_output()
        .map_err(|error| format!("cannot wait for peer-exclusion wrapper: {error}"))?;
    let peer = peer
        .wait_with_output()
        .map_err(|error| format!("cannot wait for unrelated CPU peer: {error}"))?;
    if !peer_wrapper.status.success() || !peer.status.success() || owned_path.exists() {
        return Err(format!(
            "peer-exclusion control failed or leaked its owned cgroup: wrapper={peer_wrapper:?}, peer={peer:?}, path={} exists={}",
            owned_path.display(),
            owned_path.exists()
        ));
    }
    let peer_final_cpu = fs::read_to_string(&peer_final_cpu_file)
        .map_err(|error| format!("cannot read peer-exclusion final CPU: {error}"))?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid peer-exclusion final CPU: {error}"))?;

    let exited_parent_pid_file = scratch.0.join("exited-parent-descendant.pid");
    // This value distinguishes the repaired path from the old implementation
    // on this host: the old path returned exit 0 after 1.757s of subtree CPU,
    // while 50ms was already small enough to time out before the repair.
    let exited_parent_budget_usec = 500_000;
    let mut exited_parent_command = control_command_with_limits(
        &executable,
        &test_binary,
        &scratch.0,
        "exit-after-escaped-burner",
        1,
        exited_parent_budget_usec,
        100,
    );
    exited_parent_command.env(CONTROL_PID_FILE_ENV, &exited_parent_pid_file);
    let exited_parent = exited_parent_command
        .output()
        .map_err(|error| format!("cannot run exited-parent CPU control: {error}"))?;
    if exited_parent.status.code() != Some(CPU_TIMEOUT_EXIT.into()) {
        return Err(format!(
            "a successful parent hid its over-budget descendant: {exited_parent:?}"
        ));
    }
    let exited_parent_descendant = read_pid(&exited_parent_pid_file)?;
    if process_exists(exited_parent_descendant) {
        return Err(format!(
            "escaped descendant {exited_parent_descendant} survived post-parent CPU-timeout cleanup"
        ));
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
    let accounting_path_file = scratch.0.join("accounting-cgroup.path");
    let accounting_failure_file = scratch.0.join("accounting-read-failure");
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
        .env(CONTROL_CGROUP_PATH_FILE_ENV, &accounting_path_file)
        .env(
            CONTROL_ACCOUNTING_FAILURE_FILE_ENV,
            &accounting_failure_file,
        );
    let accounting = accounting_command
        .spawn()
        .map_err(|error| format!("cannot run missing-accounting control: {error}"))?;
    wait_for_file(&accounting_pid_file, "missing-accounting child PID")?;
    wait_for_file(&accounting_path_file, "missing-accounting cgroup path")?;
    let accounting_pid = read_pid(&accounting_pid_file)?;
    let accounting_path = PathBuf::from(
        fs::read_to_string(&accounting_path_file)
            .map_err(|error| format!("cannot read missing-accounting cgroup path: {error}"))?
            .trim(),
    );
    fs::write(&accounting_failure_file, b"fail\n")
        .map_err(|error| format!("cannot trigger accounting read failure: {error}"))?;
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
    if accounting_path.exists() {
        return Err(format!(
            "missing-accounting control leaked owned cgroup {}",
            accounting_path.display()
        ));
    }

    let final_read_path_file = scratch.0.join("final-read-failure-cgroup.path");
    let mut final_read_command =
        control_command(&executable, &test_binary, &scratch.0, "success", 4);
    final_read_command
        .env(CONTROL_CGROUP_PATH_FILE_ENV, &final_read_path_file)
        .env(CONTROL_FINAL_READ_FAILURE_ENV, "1");
    let final_read = final_read_command
        .output()
        .map_err(|error| format!("cannot run final-read refusal control: {error}"))?;
    require_refusal_and_removed_cgroup(
        &final_read,
        &final_read_path_file,
        "final owned cgroup CPU read failed",
        "final-read refusal control",
    )?;

    let regression_path_file = scratch.0.join("final-regression-cgroup.path");
    let mut regression_command =
        control_command(&executable, &test_binary, &scratch.0, "final-regression", 1);
    regression_command
        .env(CONTROL_CGROUP_PATH_FILE_ENV, &regression_path_file)
        .env(CONTROL_FINAL_REGRESSION_ENV, "1");
    let regression = regression_command
        .output()
        .map_err(|error| format!("cannot run final-regression refusal control: {error}"))?;
    require_refusal_and_removed_cgroup(
        &regression,
        &regression_path_file,
        "cpu.stat regressed",
        "final-regression refusal control",
    )?;

    let cleanup_pid_file = scratch.0.join("cleanup-error-child.pid");
    let cleanup_path_file = scratch.0.join("cleanup-error-cgroup.path");
    let mut cleanup_command = control_command(&executable, &test_binary, &scratch.0, "hang", 3);
    cleanup_command
        .env(CONTROL_PID_FILE_ENV, &cleanup_pid_file)
        .env(CONTROL_CGROUP_PATH_FILE_ENV, &cleanup_path_file)
        .env(CONTROL_CLEANUP_ERROR_ENV, "1");
    let cleanup = cleanup_command
        .spawn()
        .map_err(|error| format!("cannot run proved-hard-cleanup refusal control: {error}"))?;
    wait_for_file(&cleanup_pid_file, "proved-hard-cleanup child PID")?;
    wait_for_file(&cleanup_path_file, "proved-hard-cleanup cgroup path")?;
    let cleanup_pid = read_pid(&cleanup_pid_file)?;
    if unsafe { libc::kill(cleanup.id() as i32, libc::SIGTERM) } != 0 {
        return Err(format!(
            "cannot trigger proved-hard-cleanup control: {}",
            io::Error::last_os_error()
        ));
    }
    let cleanup = cleanup
        .wait_with_output()
        .map_err(|error| format!("cannot wait for proved-hard-cleanup control: {error}"))?;
    require_refusal_and_removed_cgroup(
        &cleanup,
        &cleanup_path_file,
        "cleanup completed with an error",
        "proved-hard-cleanup refusal control",
    )?;
    if process_exists(cleanup_pid) {
        return Err(format!(
            "proved-hard-cleanup child {cleanup_pid} survived cgroup.kill"
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
    if records.len() != 18
        || identities
            != [
                "success",
                "measurement-signal",
                "failure",
                "signal",
                "tree",
                "hang",
                "burn-long",
                "exit-after-escaped-burner",
                "stop-hang",
                "wait-for-escaped-burner",
                "catch-signal",
                "auto-reap-ignored",
                "auto-reap-nocldwait",
                "failure-after-burn",
                "peer-sleep",
                "wait4-race",
            ]
            .into_iter()
            .collect()
    {
        return Err(format!(
            "self-test expected the original thirteen and five added exact atomic attempt identities, found {identities:?} ({} records)",
            records.len()
        ));
    }
    if records
        .iter()
        .any(|record| record.identity.binary != "fixture::bin/fixture_name")
    {
        return Err("self-test did not preserve the typed binary identity".into());
    }
    if find_record_attempt(&records, "success", 1)?.cpu_source != CPU_SOURCE_CGROUP_V2
        || find_record_attempt(&records, "success", 2)?.cpu_source != CPU_SOURCE_REAPED
        || find_record_attempt(&records, "success", 3)?.cpu_source != CPU_SOURCE_CGROUP_V2
        || find_record(&records, "measurement-signal")?.cpu_source != CPU_SOURCE_PROCFS_AND_REAPED
    {
        return Err(
            "self-test did not distinguish cgroup-v2, wait4-only, and procfs-plus-wait4 CPU sources".into(),
        );
    }
    if !matches!(
        find_record(&records, "success")?.completion,
        AttemptCompletion::Exit { code: 0 }
    ) || !matches!(
        find_record_attempt(&records, "success", 3)?.completion,
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
    ) || !matches!(
        find_record(&records, "measurement-signal")?.completion,
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
    let exited_parent_record = find_record(&records, "exit-after-escaped-burner")?;
    if !matches!(
        exited_parent_record.completion,
        AttemptCompletion::CpuTimeout {
            cpu_budget_usec,
            observed_cpu_usec,
        } if cpu_budget_usec == exited_parent_budget_usec
            && observed_cpu_usec >= exited_parent_budget_usec
    ) || exited_parent_record.cpu_usage_usec < exited_parent_budget_usec
    {
        return Err(format!(
            "successful parent hid an over-budget descendant in its record: {exited_parent_record:?}"
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
    let exit_over_budget_record = find_record(&records, "failure-after-burn")?;
    if !matches!(
        exit_over_budget_record.completion,
        AttemptCompletion::Exit { code: 23 }
    ) || exit_over_budget_record.cpu_usage_usec < 50_000
    {
        return Err(format!(
            "nonzero exit was not retained ahead of a later budget result: {exit_over_budget_record:?}"
        ));
    }
    for mode in ["auto-reap-ignored", "auto-reap-nocldwait"] {
        let record = find_record(&records, mode)?;
        if record.cpu_source != CPU_SOURCE_CGROUP_V2
            || !matches!(
                record.completion,
                AttemptCompletion::CpuTimeout {
                    cpu_budget_usec: 150_000,
                    observed_cpu_usec,
                } if observed_cpu_usec >= 150_000
            )
        {
            return Err(format!(
                "{mode} did not retain auto-reaped descendant CPU: {record:?}"
            ));
        }
    }
    let peer_record = find_record(&records, "peer-sleep")?;
    if peer_record.cpu_source != CPU_SOURCE_CGROUP_V2
        || peer_record.cpu_usage_usec >= 100_000
        || peer_record.cpu_usage_usec != peer_final_cpu
        || !matches!(peer_record.completion, AttemptCompletion::Exit { code: 0 })
    {
        return Err(format!(
            "unrelated peer CPU entered the owned attempt total: {peer_record:?}"
        ));
    }
    let wait4_race_record = find_record(&records, "wait4-race")?;
    if !matches!(
        wait4_race_record.completion,
        AttemptCompletion::SupervisorSignal {
            signal: libc::SIGINT
        }
    ) || !wait4_race_record
        .wait4
        .iter()
        .any(|receipt| ExitStatus::from_raw(receipt.status).code() == Some(23))
    {
        return Err(format!(
            "wait4 receipt was lost when a signal won terminal classification: {wait4_race_record:?}"
        ));
    }
    let duplicate = write_attempt_atomic(&scratch.0.join("attempts"), &records[0]);
    if duplicate.is_ok() {
        return Err("duplicate atomic attempt publication unexpectedly replaced a record".into());
    }
    println!(
        "nextest-cpu-wrapper: self-test PASS (owned cgroup v2 accounting, SIG_IGN and SA_NOCLDWAIT auto-reap, peer exclusion, pre-exec enrollment, process tree, success, failure, signal, wall timeout, CPU timeout, 500ms post-parent CPU boundary, stopped low-CPU wall delay, missing and final accounting fail-closed, final-counter regression, recovered hard-cleanup refusal, CPU, exit and wait4 first-cause races, cgroup.kill cleanup, no survivors, typed identity, truthful CPU source, substituted path, atomic identity)"
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
