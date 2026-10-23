//! A fresh cgroup v2 child that holds one budgeted E2E invocation.
//!
//! The runner reads the invocation's live CPU from the kernel's own counter
//! (`cpu.stat` `usage_usec`) in that child, instead of scanning every host
//! process for members of the invocation's process group. The counter needs no
//! controller: cgroup v2 keeps it for every cgroup, and it covers every process
//! in the cgroup, including one that left the process group or session.
//!
//! The helpers below follow `OwnedAttemptCgroup` in the nextest CPU wrapper
//! (`src/bin/nextest-cpu-wrapper.rs`): the same descriptor identity checks and
//! the same bounded control-file parsing. They are copied rather than shared
//! because the wrapper is a separate binary; sharing them is a follow-up.

use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Set to exactly `1` by a launcher that runs the E2E manifest without
/// cgroups on purpose. Only then may the runner fall back to the agent-utils
/// process-group scan, and only when the invocation cgroup cannot be created
/// for lack of permission or of a cgroup v2 hierarchy. Any other value is
/// refused.
pub const ALLOW_PROCESS_GROUP_CPU_SCAN_ENV: &str = "HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN";

/// How long the processes left in the cgroup after its leader was reaped get
/// to exit after `cgroup.kill`. Matches the runner's SIGTERM-to-SIGKILL grace.
const LEFTOVER_KILL_GRACE: Duration = Duration::from_secs(10);
const LEFTOVER_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How many levels of cgroups the command may leave below its invocation
/// cgroup. A nested runner adds one level for each invocation it runs, so a
/// real tree is a few levels deep; a deeper one is refused, not walked.
const MAX_NESTED_CGROUP_DEPTH: usize = 32;
const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(0);

/// Read the fallback marker. Unset means the fallback is not allowed.
pub fn process_group_scan_allowed(value: Option<&OsStr>) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(value) if value.as_bytes() == b"1" => Ok(true),
        Some(value) => Err(format!(
            "{ALLOW_PROCESS_GROUP_CPU_SCAN_ENV}={value:?} is not a recognized value: set it to exactly 1 to allow the process-group CPU scan when an invocation cgroup cannot be created, or leave it unset"
        )),
    }
}

/// Why an invocation cgroup could not be created.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateError {
    /// True only when the failure shows that this process cannot place an
    /// invocation in a cgroup of its own here at all: there is no cgroup v2
    /// hierarchy at the runner's cgroup, or the runner lacks permission to
    /// create a child cgroup or to move a process into one. Every other
    /// failure (a malformed `/proc/self/cgroup`, a parent that is not a
    /// directory, a name collision, a failure after the child was created) is
    /// not eligible, so the marker never hides it.
    pub fallback_eligible: bool,
    pub message: String,
}

impl CreateError {
    fn eligible(message: String) -> Self {
        Self {
            fallback_eligible: true,
            message,
        }
    }

    fn ineligible(message: String) -> Self {
        Self {
            fallback_eligible: false,
            message,
        }
    }

    fn from_errno(message: String, error: &io::Error, eligible_errnos: &[i32]) -> Self {
        let eligible = error
            .raw_os_error()
            .is_some_and(|errno| eligible_errnos.contains(&errno));
        Self {
            fallback_eligible: eligible,
            message,
        }
    }
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

fn openat_file(directory: &File, name: &str, flags: i32) -> io::Result<File> {
    let name = CString::new(name).expect("invocation cgroup control names contain no NUL");
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory_at(directory: &File, name: &CStr) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The identity of the entry `name` in `directory`, without following a link,
/// and whether it is a directory.
fn entry_identity(directory: &File, name: &CStr) -> io::Result<(FileIdentity, bool)> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((
        FileIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        },
        stat.st_mode & libc::S_IFMT == libc::S_IFDIR,
    ))
}

/// The names of the cgroups directly below `directory`, which are its
/// subdirectories; its control files are skipped.
fn nested_cgroup_names(directory: &File) -> io::Result<Vec<CString>> {
    // fdopendir owns the descriptor it is given and moves its offset, so it
    // gets a fresh descriptor for the same directory.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    let mut names = Vec::new();
    let outcome = loop {
        // readdir reports an error only through errno.
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            break match error.raw_os_error() {
                Some(0) => Ok(()),
                _ => Err(error),
            };
        }
        // SAFETY: a non-null readdir result stays valid until the next
        // readdir or closedir on this stream, and its name is NUL-terminated.
        let (name, kind) = unsafe { (CStr::from_ptr((*entry).d_name.as_ptr()), (*entry).d_type) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let is_directory = match kind {
            libc::DT_DIR => true,
            libc::DT_UNKNOWN => match entry_identity(directory, name) {
                Ok((_, is_directory)) => is_directory,
                Err(error) => break Err(error),
            },
            _ => false,
        };
        if is_directory {
            names.push(name.to_owned());
        }
    };
    unsafe {
        libc::closedir(stream);
    }
    outcome.map(|()| names)
}

/// Remove every cgroup below `directory`, deepest first. `level` is how far
/// below the invocation cgroup the entries of `directory` are. Each nested
/// cgroup is opened through its parent's descriptor without following a link
/// and must be on the invocation cgroup's filesystem, and its name is removed
/// only while it still names the directory that was just emptied.
fn remove_nested_cgroups(
    directory: &File,
    path: &Path,
    device: u64,
    level: usize,
) -> Result<(), String> {
    let names = nested_cgroup_names(directory)
        .map_err(|error| format!("cannot list cgroup {}: {error}", path.display()))?;
    for name in names {
        let nested_path = path.join(OsStr::from_bytes(name.to_bytes()));
        if level > MAX_NESTED_CGROUP_DEPTH {
            return Err(format!(
                "nested cgroup {} is more than {MAX_NESTED_CGROUP_DEPTH} levels below the invocation cgroup",
                nested_path.display()
            ));
        }
        let nested = open_directory_at(directory, &name).map_err(|error| {
            format!(
                "cannot open nested cgroup {}: {error}",
                nested_path.display()
            )
        })?;
        let identity = file_identity(&nested, "nested cgroup")?;
        if identity.device != device {
            return Err(format!(
                "nested cgroup {} is on device {}, expected {device}",
                nested_path.display(),
                identity.device
            ));
        }
        remove_nested_cgroups(&nested, &nested_path, device, level + 1)?;
        let (named, is_directory) = entry_identity(directory, &name).map_err(|error| {
            format!(
                "cannot authenticate nested cgroup {}: {error}",
                nested_path.display()
            )
        })?;
        if named != identity || !is_directory {
            return Err(format!(
                "nested cgroup {} was replaced",
                nested_path.display()
            ));
        }
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0
        {
            return Err(format!(
                "cannot remove nested cgroup {}: {}",
                nested_path.display(),
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Map the unified (`0::`) line of `/proc/self/cgroup` to its directory under
/// `/sys/fs/cgroup`. The path is relative to this process's cgroup namespace,
/// as `/sys/fs/cgroup` is for a hierarchy mounted inside that namespace.
fn unified_cgroup_directory(raw: &str) -> Result<PathBuf, CreateError> {
    let mut unified = raw.lines().filter_map(|line| line.strip_prefix("0::"));
    let path = unified.next().ok_or_else(|| {
        CreateError::eligible("/proc/self/cgroup has no unified cgroup v2 entry".into())
    })?;
    if unified.next().is_some() {
        return Err(CreateError::ineligible(
            "/proc/self/cgroup has multiple unified cgroup v2 entries".into(),
        ));
    }
    let relative = Path::new(path.trim_start_matches('/'));
    if relative.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(CreateError::ineligible(format!(
            "unified cgroup path {path:?} is not a relative kernel path"
        )));
    }
    Ok(Path::new(CGROUP_ROOT).join(relative))
}

fn current_cgroup_directory() -> Result<PathBuf, CreateError> {
    let raw = fs::read_to_string("/proc/self/cgroup").map_err(|error| {
        CreateError::from_errno(
            format!("cannot read /proc/self/cgroup: {error}"),
            &error,
            &[libc::ENOENT],
        )
    })?;
    unified_cgroup_directory(&raw)
}

fn invocation_name() -> String {
    // The process nonce keeps a directory left behind by an earlier runner
    // that had the same PID from colliding with this runner's names.
    static NONCE: OnceLock<u64> = OnceLock::new();
    let nonce = NONCE.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64)
    });
    format!(
        "hermit-e2e-invocation-{}-{nonce:x}-{}",
        std::process::id(),
        NEXT_INVOCATION.fetch_add(1, Ordering::Relaxed)
    )
}

/// One invocation's cgroup: a fresh, empty child of the runner's own cgroup,
/// held through descriptors whose identity is checked before every use.
pub struct InvocationCgroup {
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
    finished: Option<Result<(), String>>,
}

impl InvocationCgroup {
    /// Create the cgroup under the runner's own cgroup.
    pub fn create() -> Result<Self, CreateError> {
        Self::create_in(&current_cgroup_directory()?)
    }

    /// Create the cgroup under `parent_path`, which must be the cgroup that
    /// holds this process: moving a process into the child needs write access
    /// to the `cgroup.procs` of the common ancestor of both cgroups.
    pub fn create_in(parent_path: &Path) -> Result<Self, CreateError> {
        let parent = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent_path)
            .map_err(|error| {
                CreateError::from_errno(
                    format!("cannot open cgroup {}: {error}", parent_path.display()),
                    &error,
                    &[libc::EACCES, libc::EPERM, libc::ENOENT],
                )
            })?;
        let mut filesystem = unsafe { std::mem::zeroed::<libc::statfs>() };
        if unsafe { libc::fstatfs(parent.as_raw_fd(), &mut filesystem) } != 0 {
            return Err(CreateError::ineligible(format!(
                "cannot inspect the filesystem of cgroup {}: {}",
                parent_path.display(),
                io::Error::last_os_error()
            )));
        }
        if filesystem.f_type != CGROUP2_SUPER_MAGIC {
            return Err(CreateError::eligible(format!(
                "{} is not a cgroup v2 directory",
                parent_path.display()
            )));
        }
        // The child process moves itself out of this cgroup before exec. That
        // is permitted only with write access to this cgroup's cgroup.procs, so
        // check it now, while a refusal can still choose the fallback, rather
        // than at spawn time.
        openat_file(
            &parent,
            "cgroup.procs",
            libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
        .map_err(|error| {
            CreateError::from_errno(
                format!(
                    "cannot move processes out of cgroup {}: {}/cgroup.procs: {error}",
                    parent_path.display(),
                    parent_path.display()
                ),
                &error,
                &[libc::EACCES, libc::EPERM, libc::EROFS],
            )
        })?;
        let name_text = invocation_name();
        let path = parent_path.join(&name_text);
        let name = CString::new(name_text.as_bytes())
            .map_err(|_| CreateError::ineligible("cgroup name contains a NUL byte".into()))?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
            let error = io::Error::last_os_error();
            return Err(CreateError::from_errno(
                format!(
                    "cannot create invocation cgroup {}: {error}",
                    path.display()
                ),
                &error,
                &[libc::EACCES, libc::EPERM, libc::EROFS],
            ));
        }
        let result = (|| {
            let child = openat_file(
                &parent,
                &name_text,
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
            .map_err(|error| format!("cannot open invocation cgroup: {error}"))?;
            let identity = file_identity(&child, "invocation cgroup")?;
            let open_control = |control: &str, flags: i32| -> Result<File, String> {
                let file = openat_file(&child, control, flags | libc::O_CLOEXEC | libc::O_NOFOLLOW)
                    .map_err(|error| format!("cannot open invocation cgroup {control}: {error}"))?;
                let control_identity = file_identity(&file, control)?;
                if control_identity.device != identity.device {
                    return Err(format!(
                        "invocation cgroup {control} is on device {}, expected {}",
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
            let mut created = Self {
                parent: parent.try_clone().map_err(|error| {
                    format!("cannot retain the parent cgroup descriptor: {error}")
                })?,
                child,
                cpu_stat,
                events,
                procs_read,
                procs_write,
                kill,
                name: name.clone(),
                identity,
                path: path.clone(),
                finished: None,
            };
            let fresh = created.verify_identity().and_then(|()| {
                if created.cpu_usage_usec()? != 0 {
                    return Err("fresh invocation cgroup has nonzero cpu.stat usage_usec".into());
                }
                if created.populated()? || !created.procs_empty()? {
                    return Err("fresh invocation cgroup is not empty before enrollment".into());
                }
                Ok(())
            });
            match fresh {
                Ok(()) => Ok(created),
                Err(error) => {
                    // The directory is removed below; Drop must not repeat it.
                    created.finished = Some(Err(error.clone()));
                    Err(error)
                }
            }
        })();
        result.map_err(|error| {
            let message = if unsafe {
                libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
            } == 0
            {
                format!("invocation cgroup {}: {error}", path.display())
            } else {
                format!(
                    "invocation cgroup {}: {error}; removing the partly initialized cgroup also failed: {}",
                    path.display(),
                    io::Error::last_os_error()
                )
            };
            CreateError::ineligible(message)
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This cgroup as the unified (`0::`) line of `/proc/<pid>/cgroup` names
    /// it for a process inside it, or `None` when its directory is not under
    /// `/sys/fs/cgroup`.
    pub fn kernel_path(&self) -> Option<String> {
        let relative = self.path.strip_prefix(CGROUP_ROOT).ok()?.to_str()?;
        Some(format!("/{relative}"))
    }

    /// The `cgroup.procs` descriptor a child writes `0` to before exec. It is
    /// close-on-exec, so the program never inherits it.
    pub fn enrollment_fd(&self) -> RawFd {
        self.procs_write.as_raw_fd()
    }

    fn verify_identity(&self) -> Result<(), String> {
        let held = file_identity(&self.child, "held invocation cgroup")?;
        if held != self.identity {
            return Err("held invocation cgroup identity changed".into());
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
                "cannot authenticate invocation cgroup path {}: {}",
                self.path.display(),
                io::Error::last_os_error()
            ));
        }
        let named = FileIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        };
        if named != self.identity || stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(format!(
                "invocation cgroup path {} was replaced",
                self.path.display()
            ));
        }
        Ok(())
    }

    /// Total CPU of every process that has run in this cgroup, in
    /// microseconds. The kernel keeps it monotonic for one cgroup.
    pub fn cpu_usage_usec(&self) -> Result<u64, String> {
        self.verify_identity()?;
        cgroup_field(&self.cpu_stat, "invocation cgroup cpu.stat", "usage_usec")
    }

    /// Whether a live process remains. A zombie no longer counts.
    pub fn populated(&self) -> Result<bool, String> {
        self.verify_identity()?;
        match cgroup_field(&self.events, "invocation cgroup.events", "populated")? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!(
                "invocation cgroup.events has invalid populated value {value}"
            )),
        }
    }

    /// Whether `cgroup.procs` lists no process. One short read decides it, so a
    /// long list cannot exceed a parsing bound.
    pub fn procs_empty(&self) -> Result<bool, String> {
        self.verify_identity()?;
        let mut chunk = [0u8; 64];
        let count = self
            .procs_read
            .read_at(&mut chunk, 0)
            .map_err(|error| format!("cannot read invocation cgroup.procs: {error}"))?;
        Ok(count == 0)
    }

    /// Send SIGKILL to every process in this cgroup and its descendants.
    pub fn kill(&self) -> Result<(), String> {
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
                    "cannot write invocation cgroup.kill: {}",
                    io::Error::last_os_error()
                )
            } else {
                format!("short write to invocation cgroup.kill: {written} bytes")
            });
        }
    }

    fn empty(&self) -> Result<bool, String> {
        Ok(!self.populated()? && self.procs_empty()?)
    }

    fn kill_leftovers(&self) -> Result<(), String> {
        if self.empty()? {
            return Ok(());
        }
        self.kill()?;
        let deadline = Instant::now() + LEFTOVER_KILL_GRACE;
        loop {
            if self.empty()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "processes remain {} s after cgroup.kill",
                    LEFTOVER_KILL_GRACE.as_secs()
                ));
            }
            thread::sleep(LEFTOVER_POLL_INTERVAL);
        }
    }

    fn remove_empty(&self) -> Result<(), String> {
        self.verify_identity()?;
        if !self.empty()? {
            return Err("it is still populated".into());
        }
        // The command may have made cgroups of its own below this one, as a
        // nested runner does, and rmdir refuses a cgroup that still has a
        // child. `populated` covers the whole subtree, so nothing is alive
        // anywhere below here and each nested cgroup can go, deepest first.
        remove_nested_cgroups(&self.child, &self.path, self.identity.device, 1)?;
        self.verify_identity()?;
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(format!("rmdir: {}", io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Kill whatever is left in the cgroup, wait up to 10 s for it to exit, and
    /// remove the cgroup together with any cgroups the command made below it.
    /// Called once its leader has been reaped (or could not be), and by Drop;
    /// later calls return the first outcome.
    pub fn finish(&mut self) -> Result<(), String> {
        if let Some(outcome) = &self.finished {
            return outcome.clone();
        }
        let outcome = self
            .kill_leftovers()
            .and_then(|()| self.remove_empty())
            .map_err(|error| {
                format!(
                    "cannot remove invocation cgroup {}: {error}",
                    self.path.display()
                )
            });
        self.finished = Some(outcome.clone());
        outcome
    }
}

impl Drop for InvocationCgroup {
    fn drop(&mut self) {
        if self.finished.is_none() {
            if let Err(error) = self.finish() {
                eprintln!("{error}");
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "hermit-invocation-cgroup-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    /// A real invocation cgroup, or `None` when this environment declares with
    /// the fallback marker that it cannot create one. Without the marker, a
    /// creation failure fails the test.
    pub(crate) fn real_cgroup_or_declared_absent(test: &str) -> Option<InvocationCgroup> {
        match InvocationCgroup::create() {
            Ok(cgroup) => Some(cgroup),
            Err(error)
                if error.fallback_eligible
                    && std::env::var_os(ALLOW_PROCESS_GROUP_CPU_SCAN_ENV).as_deref()
                        == Some(OsStr::new("1")) =>
            {
                println!(
                    "SKIPPED {test}: {ALLOW_PROCESS_GROUP_CPU_SCAN_ENV}=1 declares no invocation cgroup here: {}",
                    error.message
                );
                None
            }
            Err(error) => panic!("cannot create a real invocation cgroup: {error:?}"),
        }
    }

    #[test]
    fn the_process_group_scan_marker_accepts_only_exactly_one() {
        assert_eq!(process_group_scan_allowed(None), Ok(false));
        assert_eq!(process_group_scan_allowed(Some(OsStr::new("1"))), Ok(true));
        for value in ["0", "yes", "", "true", " 1", "1\n", "01"] {
            let error = process_group_scan_allowed(Some(OsStr::new(value)))
                .expect_err("only exactly 1 allows the fallback");
            assert!(
                error.contains(ALLOW_PROCESS_GROUP_CPU_SCAN_ENV),
                "{value:?}: {error}"
            );
            assert!(error.contains(&format!("{value:?}")), "{value:?}: {error}");
        }
    }

    #[test]
    fn the_unified_cgroup_entry_maps_to_sys_fs_cgroup_or_is_classified() {
        assert_eq!(
            unified_cgroup_directory("12:pids:/legacy\n0::/user.slice/a.scope\n"),
            Ok(PathBuf::from("/sys/fs/cgroup/user.slice/a.scope"))
        );
        assert_eq!(
            unified_cgroup_directory("0::/\n"),
            Ok(PathBuf::from("/sys/fs/cgroup"))
        );
        // A cgroup v1-only host has no cgroup v2 hierarchy to place a child in.
        let absent = unified_cgroup_directory("4:memory:/x\n").unwrap_err();
        assert!(absent.fallback_eligible, "{absent:?}");
        for malformed in ["0::/a\n0::/b\n", "0::/a/../../etc\n"] {
            let error = unified_cgroup_directory(malformed).unwrap_err();
            assert!(!error.fallback_eligible, "{malformed:?}: {error:?}");
        }
    }

    #[test]
    fn a_missing_or_non_cgroup_parent_is_eligible_and_a_file_parent_is_not() {
        let root = scratch("parents");
        let missing = root.join("missing");
        let error = InvocationCgroup::create_in(&missing)
            .err()
            .expect("a missing parent cannot hold a cgroup");
        assert!(error.fallback_eligible, "{error:?}");
        assert!(error.message.contains(&missing.display().to_string()));

        let plain = root.join("plain");
        fs::create_dir(&plain).unwrap();
        let error = InvocationCgroup::create_in(&plain)
            .err()
            .expect("a plain directory is not a cgroup");
        assert!(error.fallback_eligible, "{error:?}");
        assert!(error.message.contains("is not a cgroup v2 directory"));
        assert_eq!(fs::read_dir(&plain).unwrap().count(), 0);

        let file = root.join("file");
        fs::write(&file, b"").unwrap();
        let error = InvocationCgroup::create_in(&file)
            .err()
            .expect("a regular file cannot hold a cgroup");
        assert!(!error.fallback_eligible, "{error:?}");
        assert!(error.message.contains("Not a directory"), "{error:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_symlinked_parent_cgroup_is_refused_and_not_eligible() {
        let root = scratch("symlink");
        // Point the link at the runner's real cgroup when there is one, so the
        // refusal comes from the link itself and not from what it names.
        let target = match current_cgroup_directory() {
            Ok(path) if path.is_dir() => path,
            _ => {
                let plain = root.join("plain");
                fs::create_dir(&plain).unwrap();
                plain
            }
        };
        let link = root.join("replaced-cgroup-parent");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = InvocationCgroup::create_in(&link)
            .err()
            .expect("a substituted cgroup parent path is refused");
        assert!(!error.fallback_eligible, "{error:?}");
        assert!(
            error.message.contains(&link.display().to_string()),
            "{error:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fresh_invocation_cgroup_counts_its_process_monotonically_and_is_removed() {
        let Some(mut cgroup) = real_cgroup_or_declared_absent(
            "a_fresh_invocation_cgroup_counts_its_process_monotonically_and_is_removed",
        ) else {
            return;
        };
        let path = cgroup.path().to_owned();
        assert!(path.is_dir());
        assert_eq!(cgroup.cpu_usage_usec(), Ok(0));
        assert_eq!(cgroup.populated(), Ok(false));
        assert_eq!(cgroup.procs_empty(), Ok(true));
        let enrollment_fd = cgroup.enrollment_fd();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "i=0; while [ $i -lt 400000 ]; do i=$((i+1)); done"]);
        unsafe {
            command.pre_exec(move || {
                if libc::write(enrollment_fd, b"0\n".as_ptr().cast(), 2) == 2 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let mut samples = vec![cgroup.cpu_usage_usec().unwrap()];
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            samples.push(cgroup.cpu_usage_usec().unwrap());
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        samples.push(cgroup.cpu_usage_usec().unwrap());
        assert!(
            samples.windows(2).all(|pair| pair[0] <= pair[1]),
            "{samples:?}"
        );
        assert!(*samples.last().unwrap() > 0, "{samples:?}");
        assert_eq!(cgroup.populated(), Ok(false));
        cgroup.finish().unwrap();
        assert!(!path.exists(), "{}", path.display());
        // A later call reports the same outcome rather than acting again.
        cgroup.finish().unwrap();
    }

    #[test]
    fn a_read_only_parent_cgroup_is_eligible_for_the_fallback() {
        if unsafe { libc::geteuid() } == 0 {
            // Root ignores the permission bits this test relies on.
            println!(
                "SKIPPED a_read_only_parent_cgroup_is_eligible_for_the_fallback: running as root"
            );
            return;
        }
        let Some(mut parent) = real_cgroup_or_declared_absent(
            "a_read_only_parent_cgroup_is_eligible_for_the_fallback",
        ) else {
            return;
        };
        let parent_path = parent.path().to_owned();
        fs::set_permissions(&parent_path, fs::Permissions::from_mode(0o555)).unwrap();
        let directory = InvocationCgroup::create_in(&parent_path).err();
        fs::set_permissions(&parent_path, fs::Permissions::from_mode(0o755)).unwrap();
        let procs = parent_path.join("cgroup.procs");
        fs::set_permissions(&procs, fs::Permissions::from_mode(0o444)).unwrap();
        let migration = InvocationCgroup::create_in(&parent_path).err();
        fs::set_permissions(&procs, fs::Permissions::from_mode(0o644)).unwrap();
        parent.finish().unwrap();

        let directory = directory.expect("a read-only cgroup directory refuses a child");
        assert!(directory.fallback_eligible, "{directory:?}");
        assert!(
            directory
                .message
                .contains("cannot create invocation cgroup"),
            "{directory:?}"
        );
        let migration = migration.expect("a read-only cgroup.procs refuses migration");
        assert!(migration.fallback_eligible, "{migration:?}");
        assert!(migration.message.contains("cgroup.procs"), "{migration:?}");
        assert!(!parent_path.exists());
    }

    /// Shell text that sets `cg` to the directory of the cgroup the shell runs
    /// in, from the unified line of its own `/proc/self/cgroup`.
    pub(crate) const OWN_CGROUP_SH: &str = "while IFS= read -r line; do case $line in 0::*) cg=/sys/fs/cgroup${line#0::};; esac; done < /proc/self/cgroup; ";

    /// When dropped, removes what a failed test left of an invocation cgroup
    /// this test process created: the cgroups nested in it, deepest first, and
    /// then the cgroup itself. The location is the cgroup directory, or a file
    /// holding a copy of a member's `/proc/<pid>/cgroup`. Any other directory
    /// is left alone.
    pub(crate) struct RemoveLeftoverCgroup(pub(crate) PathBuf);

    impl Drop for RemoveLeftoverCgroup {
        fn drop(&mut self) {
            fn remove_tree(directory: &Path) {
                if let Ok(entries) = fs::read_dir(directory) {
                    for entry in entries.flatten() {
                        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                            remove_tree(&entry.path());
                        }
                    }
                }
                let _ = fs::remove_dir(directory);
            }
            let directory = if self.0.starts_with(CGROUP_ROOT) {
                self.0.clone()
            } else {
                let Some(relative) = fs::read_to_string(&self.0).ok().and_then(|text| {
                    text.lines()
                        .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
                }) else {
                    return;
                };
                Path::new(CGROUP_ROOT).join(relative.trim_start_matches('/'))
            };
            let ours = format!("hermit-e2e-invocation-{}-", std::process::id());
            if directory
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&ours))
            {
                remove_tree(&directory);
            }
        }
    }

    /// When dropped, SIGKILLs the process whose PID a test script wrote to the
    /// file, so a failed assertion cannot leave it running.
    pub(crate) struct KillPidFile(pub(crate) PathBuf);

    impl Drop for KillPidFile {
        fn drop(&mut self) {
            if let Some(pid) = fs::read_to_string(&self.0)
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    /// Run `script` with /bin/sh as a member of `cgroup` and wait for it.
    fn run_enrolled(cgroup: &InvocationCgroup, script: &str) -> std::process::ExitStatus {
        let enrollment_fd = cgroup.enrollment_fd();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        unsafe {
            command.pre_exec(move || {
                if libc::write(enrollment_fd, b"0\n".as_ptr().cast(), 2) == 2 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        command.status().unwrap()
    }

    #[test]
    fn empty_cgroups_nested_by_the_command_are_removed_by_finish_and_by_drop() {
        let test = "empty_cgroups_nested_by_the_command_are_removed_by_finish_and_by_drop";
        for removed_by_drop in [false, true] {
            let Some(mut cgroup) = real_cgroup_or_declared_absent(test) else {
                return;
            };
            let path = cgroup.path().to_owned();
            let _leftovers = RemoveLeftoverCgroup(path.clone());
            // The command makes cgroups of its own below the one it runs in,
            // as a nested runner does, and leaves them empty.
            let status = run_enrolled(
                &cgroup,
                &format!(r#"{OWN_CGROUP_SH}mkdir "$cg/nested" "$cg/nested/deeper" "$cg/sibling""#),
            );
            assert!(status.success(), "{status:?}");
            assert!(path.join("nested/deeper").is_dir());
            assert!(path.join("sibling").is_dir());
            if removed_by_drop {
                drop(cgroup);
            } else {
                assert_eq!(cgroup.finish(), Ok(()));
            }
            assert!(
                !path.exists(),
                "removed by drop: {removed_by_drop}: {} is left behind",
                path.display()
            );
        }
    }

    #[test]
    fn a_populated_nested_cgroup_is_killed_and_removed_by_finish() {
        let test = "a_populated_nested_cgroup_is_killed_and_removed_by_finish";
        let Some(mut cgroup) = real_cgroup_or_declared_absent(test) else {
            return;
        };
        let path = cgroup.path().to_owned();
        let _leftovers = RemoveLeftoverCgroup(path.clone());
        let root = scratch("populated-nested");
        let pid_file = root.join("nested.pid");
        let _sleeper = KillPidFile(pid_file.clone());
        // The command starts a child in a session of its own, which moves into
        // a nested cgroup and sleeps, and then exits without waiting for it.
        let status = run_enrolled(
            &cgroup,
            &format!(
                r#"{OWN_CGROUP_SH}mkdir "$cg/nested" && setsid /bin/sh -c 'echo $$ > "$1/cgroup.procs" && echo $$ > "$2" && exec sleep 60' sh "$cg/nested" '{pid}' & while [ ! -s '{pid}' ]; do sleep 0.05; done"#,
                pid = pid_file.display()
            ),
        );
        assert!(status.success(), "{status:?}");
        let sleeper: libc::pid_t = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let membership = fs::read_to_string(format!("/proc/{sleeper}/cgroup")).unwrap();
        assert!(membership.trim_end().ends_with("/nested"), "{membership:?}");
        assert_eq!(cgroup.populated(), Ok(true));
        assert_eq!(cgroup.finish(), Ok(()));
        assert!(!path.exists(), "{} is left behind", path.display());
        if let Ok(stat) = fs::read_to_string(format!("/proc/{sleeper}/stat")) {
            let state = stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next());
            // A zombie waiting for its new parent is dead; a live process with
            // this PID must be a later one outside the removed cgroup.
            if state != Some(Some('Z')) {
                let membership =
                    fs::read_to_string(format!("/proc/{sleeper}/cgroup")).unwrap_or_default();
                assert!(
                    !membership.contains("/nested"),
                    "sleeper {sleeper} survived: {stat}"
                );
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}
