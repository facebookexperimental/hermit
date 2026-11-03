/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Dispatch input custody, used only by the pre-fbinit executable entry.
//!
//! The frame conveys only an existing stdin reservation/error. It grants no
//! private role, signal permission, resource admission, verdict or writer
//! authority. Shared-input manifests remain untouched for the target's normal
//! single adoption before descriptor retirement.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use hermit::compatibility::CoreCompatibility;
use hermit::compatibility::ExecutableIdentity;
use serde::Deserialize;
use serde::Serialize;

const SCHEMA: &str = "hermit-dispatch-stdin/v2";
pub(super) const LOCATOR_ENV: &str = "HERMIT_DISPATCH_INPUT_FD";
const MAX_FRAME_BYTES: usize = 32 * 1024;
const SEALS: i32 = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

unsafe extern "C" {
    static mut environ: *mut *mut libc::c_char;
}

/// Raw Unix environment at the single-threaded executable boundary. Keep
/// duplicate entries, their order, empty values and non-UTF8 bytes. A map or
/// reconstructed `vars_os` iterator is not the exec protocol's authority.
pub(super) struct Environment {
    entries: Vec<Vec<u8>>,
}

impl Environment {
    /// Called before fbinit, workers or any environment mutation by the app.
    pub(super) fn capture() -> Result<Self> {
        let limit = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
        ensure!(limit > 0, "cannot bound the current Unix environment");
        let limit = usize::try_from(limit)?;
        let table = unsafe { environ };
        if table.is_null() {
            return Ok(Self {
                entries: Vec::new(),
            });
        }
        let mut entries = Vec::new();
        let mut total = 0usize;
        for index in 0..=limit {
            // SAFETY: this is the runtime's NUL-terminated environment table;
            // the executable entry is single-threaded and does not mutate it.
            let pointer = unsafe { *table.add(index) };
            if pointer.is_null() {
                return Ok(Self { entries });
            }
            let remaining = limit
                .checked_sub(total)
                .context("Unix environment exceeds the kernel argument bound")?;
            let length = unsafe { libc::strnlen(pointer, remaining.saturating_add(1)) };
            ensure!(
                length <= remaining,
                "Unix environment exceeds the kernel argument bound"
            );
            total = total
                .checked_add(length + 1)
                .context("Unix environment length overflow")?;
            ensure!(
                total <= limit,
                "Unix environment exceeds the kernel argument bound"
            );
            entries
                .push(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) }.to_vec());
        }
        bail!("Unix environment has no bounded terminator")
    }

    pub(super) fn locator(&self) -> Result<Option<&OsStr>> {
        let prefix = format!("{LOCATOR_ENV}=");
        let mut values = self
            .entries
            .iter()
            .filter_map(|entry| entry.strip_prefix(prefix.as_bytes()));
        let value = values.next();
        ensure!(
            values.next().is_none(),
            "dispatch input locator occurs more than once"
        );
        Ok(value.map(OsStr::from_bytes))
    }

    pub(super) fn digest(&self) -> String {
        let mut framed = b"hermit-dispatch-raw-unix-environment/v1\0".to_vec();
        framed.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for entry in &self.entries {
            framed.extend_from_slice(&(entry.len() as u64).to_le_bytes());
            framed.extend_from_slice(entry);
        }
        detcore::Digest::new(&framed).to_string()
    }

    pub(super) fn require_unchanged(&self) -> Result<()> {
        ensure!(
            Self::capture()?.entries == self.entries,
            "raw Unix environment changed during executable dispatch; no backend was substituted"
        );
        Ok(())
    }

    /// The identity probe uses an empty environment. Loader injection cannot
    /// be silently cleared for final exec or admitted as if the probe had
    /// observed those constructors. Refuse it, preserving the caller's bytes.
    pub(super) fn require_probe_environment(&self) -> Result<()> {
        for entry in &self.entries {
            if let Some(index) = entry.iter().position(|byte| *byte == b'=') {
                let name = &entry[..index];
                let value = &entry[index + 1..];
                ensure!(
                    !name.starts_with(b"LD_") || value.is_empty(),
                    "external dispatch cannot qualify dynamic-loader LD_* overrides; unset them for this invocation or use the backend executable directly"
                );
            }
        }
        Ok(())
    }

    pub(super) fn inherited(&self, value: &OsStr) -> Result<Vec<std::ffi::CString>> {
        ensure!(
            self.locator()?.is_none(),
            "a dispatch handoff cannot be forwarded recursively"
        );
        let mut entries = self
            .entries
            .iter()
            .map(|entry| std::ffi::CString::new(entry.as_slice()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut locator = format!("{LOCATOR_ENV}=").into_bytes();
        locator.extend_from_slice(value.as_bytes());
        entries.push(std::ffi::CString::new(locator)?);
        Ok(entries)
    }

    fn original(&self, fd: RawFd, expected_digest: &str) -> Result<Self> {
        let locator = format!("{LOCATOR_ENV}={fd}").into_bytes();
        ensure!(
            self.locator()?.is_some() && self.entries.last() == Some(&locator),
            "dispatch input locator is not the single appended handoff descriptor"
        );
        let original = Self {
            entries: self.entries[..self.entries.len() - 1].to_vec(),
        };
        ensure!(
            original.digest() == expected_digest,
            "raw Unix environment changed across executable dispatch"
        );
        Ok(original)
    }

    pub(super) fn restore_after_adoption(&self) -> Result<()> {
        // SAFETY: the executable facade is still before fbinit/workers. The
        // validated protocol appended only this name; no original entry has it.
        unsafe { std::env::remove_var(LOCATOR_ENV) };
        self.require_unchanged()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DescriptorIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    special_device: u64,
    status_flags: i32,
}

fn descriptor_identity(fd: RawFd) -> Result<DescriptorIdentity> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat initializes the supplied stat on success; no data is read
    // from the descriptor and its open-file-description offset is unchanged.
    ensure!(
        unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == 0,
        "cannot identify original stdin reservation: {}",
        std::io::Error::last_os_error()
    );
    let stat = unsafe { stat.assume_init() };
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0,
        "cannot query original stdin status: {}",
        std::io::Error::last_os_error()
    );
    Ok(DescriptorIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        mode: stat.st_mode,
        special_device: stat.st_rdev,
        status_flags: flags,
    })
}

fn descriptor_flags(fd: RawFd) -> Result<i32> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    ensure!(
        flags >= 0,
        "cannot query inherited descriptor: {}",
        std::io::Error::last_os_error()
    );
    Ok(flags)
}

fn set_flags(fd: RawFd, flags: i32) -> Result<()> {
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == 0,
        "cannot restore dispatch descriptor ownership: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn process_generation_of(pid: u32) -> Result<u64> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(format!("/proc/{pid}/stat"))?
        .take(8193)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8192,
        "process generation record exceeds its bound"
    );
    // comm is arbitrary Linux bytes; only its numeric suffix is UTF8.
    let boundary = bytes
        .windows(2)
        .rposition(|bytes| bytes == b") ")
        .context("process generation has no comm boundary")?;
    let fields = std::str::from_utf8(&bytes[boundary + 2..])?;
    // The suffix starts at field3 (state); starttime is field22.
    fields
        .split_whitespace()
        .nth(19)
        .context("process generation has no starttime")?
        .parse()
        .context("process generation has invalid starttime")
}

fn process_generation() -> Result<u64> {
    process_generation_of(std::process::id())
}

/// Only the held metadata child is transferred for reaping. A pidfd binds
/// that exact kernel process; neither an arbitrary numeric PID nor a private
/// runtime role is admitted by the frame.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProbeReaper {
    fd: RawFd,
    pid: u32,
    start_ticks: u64,
    descriptor: DescriptorIdentity,
}

/// `waitid` equivalents of a pending wait, WIFSTOPPED and terminal
/// WIFEXITED/WIFSIGNALED. None of the latter can satisfy mapping custody.
pub(super) enum StopObservation {
    Pending,
    Stopped,
    Terminated { code: i32, status: i32 },
}

fn pidfd_pid(fd: RawFd) -> Result<u32> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/fdinfo/{fd}"))?
        .take(8193)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 8192, "pidfd identity exceeds its bound");
    let text = std::str::from_utf8(&bytes)?;
    let mut pids = text.lines().filter_map(|line| line.strip_prefix("Pid:\t"));
    let pid = pids
        .next()
        .context("dispatch cleanup descriptor is not a pidfd")?
        .parse()?;
    ensure!(pids.next().is_none(), "dispatch pidfd repeats its identity");
    Ok(pid)
}

impl ProbeReaper {
    pub(super) fn observe_stop(fd: RawFd, pid: u32) -> Result<StopObservation> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        ensure!(
            unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    fd as libc::id_t,
                    info.as_mut_ptr(),
                    libc::WSTOPPED | libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } == 0,
            "cannot observe held-probe stop: {}",
            std::io::Error::last_os_error()
        );
        let info = unsafe { info.assume_init() };
        let actual_pid = unsafe { info.si_pid() };
        if actual_pid == 0 {
            return Ok(StopObservation::Pending);
        }
        ensure!(
            actual_pid as u32 == pid,
            "held-probe stop wait returned another child"
        );
        let status = unsafe { info.si_status() };
        match info.si_code {
            libc::CLD_STOPPED => {
                ensure!(
                    status == libc::SIGSTOP,
                    "held probe stopped for an unexpected signal: {status}"
                );
                Ok(StopObservation::Stopped)
            }
            code @ (libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED) => {
                Ok(StopObservation::Terminated { code, status })
            }
            code => bail!("unexpected held-probe wait event: code={code}, status={status}"),
        }
    }

    pub(super) fn require_stopped(fd: RawFd, pid: u32) -> Result<()> {
        match Self::observe_stop(fd, pid)? {
            StopObservation::Stopped => Ok(()),
            StopObservation::Pending => bail!("held-probe stop has not been observed"),
            StopObservation::Terminated { code, status } => {
                bail!("held probe ended before executable custody: code={code}, status={status}")
            }
        }
    }

    pub(super) fn from_parent(fd: RawFd, pid: u32) -> Result<Self> {
        ensure!(
            fd > 2 && !(198..=204).contains(&fd),
            "invalid held-probe pidfd"
        );
        ensure!(
            pidfd_pid(fd)? == pid,
            "held probe pidfd belongs to another process"
        );
        Self::require_stopped(fd, pid)?;
        Ok(Self {
            fd,
            pid,
            start_ticks: process_generation_of(pid)?,
            descriptor: descriptor_identity(fd)?,
        })
    }

    fn bind_child(&self) -> Result<()> {
        ensure!(
            self.fd > 2 && !(198..=204).contains(&self.fd),
            "invalid inherited probe pidfd"
        );
        ensure!(
            pidfd_pid(self.fd)? == self.pid && process_generation_of(self.pid)? == self.start_ticks,
            "held probe process generation changed"
        );
        ensure!(
            descriptor_identity(self.fd)? == self.descriptor,
            "inherited held-probe pidfd changed"
        );
        // P_PIDFD wait is also an ownership check: an unrelated process is
        // not our child and cannot be signaled/reaped by this protocol.
        self.observe(false)?;
        Ok(())
    }

    fn observe(&self, reap: bool) -> Result<Option<(i32, i32)>> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let flags = libc::WEXITED | libc::WNOHANG | if reap { 0 } else { libc::WNOWAIT };
        ensure!(
            unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    self.fd as libc::id_t,
                    info.as_mut_ptr(),
                    flags,
                )
            } == 0,
            "cannot observe this exact held-probe child: {}",
            std::io::Error::last_os_error()
        );
        let info = unsafe { info.assume_init() };
        let pid = unsafe { info.si_pid() };
        if pid == 0 {
            return Ok(None);
        }
        ensure!(
            pid as u32 == self.pid,
            "held-probe wait returned another child"
        );
        Ok(Some((info.si_code, unsafe { info.si_status() })))
    }

    fn complete_after(&self, validate_input: impl FnOnce() -> Result<()>) -> Result<()> {
        self.bind_child()?;
        // Claim the descriptor only after its exact process/generation and
        // current child relationship are proven. Untrusted/mismatched frame
        // FDs are never signaled or adopted by a refusal path.
        let owner = unsafe { OwnedFd::from_raw_fd(self.fd) };
        let mut reaped = false;
        let result = (|| -> Result<()> {
            Self::require_stopped(self.fd, self.pid)?;
            ensure!(
                descriptor_flags(self.fd)? & libc::FD_CLOEXEC == 0,
                "held-probe pidfd was not transferred through exec"
            );
            validate_input()?;
            // The parent's control endpoint was CLOEXEC. Its closure releases the
            // old mapping only after this image's exec has admitted its executable.
            // No input owner is adopted until that exact child is cleanly reaped.
            set_flags(self.fd, libc::FD_CLOEXEC)?;
            ensure!(
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.fd,
                        libc::SIGCONT,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                } == 0,
                "cannot release the exact held probe after exec: {}",
                std::io::Error::last_os_error()
            );
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(2) {
                if let Some((code, status)) = self.observe(false)? {
                    ensure!(
                        self.observe(true)? == Some((code, status)),
                        "held-probe reap changed its status"
                    );
                    reaped = true;
                    ensure!(
                        code == libc::CLD_EXITED && status == 0,
                        "held-probe custody ended abnormally: code={code}, status={status}"
                    );
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            bail!("held-probe completion exceeded2seconds")
        })();
        // Preserve the original refusal even if cleanup succeeds. Handle every
        // error after the trusted binding, including flag/SIGCONT/wait errors.
        let cleanup = if result.is_err() && !reaped {
            self.kill_and_reap()
        } else {
            Ok(())
        };
        drop(owner);
        result.with_context(|| format!("held-probe completion refused; cleanup={cleanup:?}"))
    }

    fn kill_and_reap(&self) -> Result<()> {
        let signal = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        let signal_error = (signal < 0).then(std::io::Error::last_os_error);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if let Some(expected) = self.observe(false)? {
                ensure!(
                    self.observe(true)? == Some(expected),
                    "held-probe cleanup changed its exact status"
                );
                if let Some(error) = signal_error
                    && error.raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(error).context("held-probe cleanup signal refused after reaping");
                }
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!("held-probe cleanup could not reap within2seconds; signal_error={signal_error:?}")
    }
}

fn argv_digest(argv: &[OsString]) -> Result<String> {
    // Length framing preserves empty/non-UTF8 operands and every byte after
    // `--`. No original argv member is rewritten by this input protocol.
    let mut framed = b"hermit-dispatch-raw-unix-argv/v1\0".to_vec();
    framed.extend_from_slice(&(argv.len() as u64).to_le_bytes());
    for argument in argv {
        let bytes = argument.as_os_str().as_bytes();
        ensure!(!bytes.contains(&0), "Unix argv contains NUL");
        framed.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        framed.extend_from_slice(bytes);
    }
    Ok(detcore::Digest::new(&framed).to_string())
}

fn public_argv(argv: &[OsString]) -> Result<()> {
    ensure!(!argv.is_empty(), "dispatch input has no argv0");
    // Existing private entries require their exact argv1 marker. The input
    // frame cannot be used to enter one of those separately owned protocols.
    ensure!(
        !argv
            .get(1)
            .is_some_and(|arg| arg.as_os_str().as_bytes().starts_with(b"__hermit")),
        "dispatch input cannot authorize a private executable role"
    );
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    schema: String,
    pid: u32,
    start_ticks: u64,
    argv_sha256: String,
    environment_sha256: String,
    target: ExecutableIdentity,
    core: CoreCompatibility,
    /// The unchanged startup encoding: a saved FD>=3, or -errno-1.
    original: i32,
    descriptor: Option<DescriptorIdentity>,
    probe: ProbeReaper,
}

fn valid_encoding(value: i32) -> bool {
    value >= 3 || (-4096..=-2).contains(&value)
}

fn sealed_frame(bytes: &[u8]) -> Result<OwnedFd> {
    ensure!(
        bytes.len() <= MAX_FRAME_BYTES,
        "dispatch stdin frame exceeds its bound"
    );
    let original = super::verify_inputs::new_unsealed_memfd(c"hermit-dispatch-stdin")?;
    // Keep the protocol frame outside descriptors reserved by the existing
    // native-input adapter. This does not relocate the original stdin owner.
    let raw = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 205) };
    ensure!(
        raw >= 0,
        "cannot allocate dispatch input descriptor: {}",
        std::io::Error::last_os_error()
    );
    let owner = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut writer = File::from(owner.try_clone()?);
    writer.write_all(bytes)?;
    drop(writer);
    ensure!(
        unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_ADD_SEALS, SEALS) } == 0,
        "cannot seal dispatch input frame: {}",
        std::io::Error::last_os_error()
    );
    Ok(owner)
}

/// Affine parent-side ownership. `Drop` restores the original reservation on
/// abandonment. The caller must use `restore` after an exec error so any flag
/// restoration error is included in its ordinary refusal, never a fallback.
pub(super) struct Transfer {
    original: i32,
    descriptor: Option<OwnedFd>,
    original_flags: Option<i32>,
    frame: OwnedFd,
    armed: bool,
    restored: bool,
}

impl Transfer {
    pub(super) fn prepare(
        argv: &[OsString],
        environment: &Environment,
        target: ExecutableIdentity,
        core: CoreCompatibility,
        probe: ProbeReaper,
    ) -> Result<Self> {
        public_argv(argv)?;
        ensure!(
            matches!(target, ExecutableIdentity::Backend { .. }),
            "dispatch input requires a compiled backend target"
        );
        core.validate()?;
        let original = super::STARTUP_STDIN.load(Ordering::Acquire);
        ensure!(
            valid_encoding(original),
            "original startup stdin is not available exactly once"
        );
        let original_flags = if original >= 3 {
            Some(descriptor_flags(original)?)
        } else {
            None
        };
        if let Some(flags) = original_flags {
            ensure!(
                flags & libc::FD_CLOEXEC != 0,
                "startup stdin reservation is already inherited by another transfer"
            );
        }
        let frame = Frame {
            schema: SCHEMA.to_owned(),
            pid: std::process::id(),
            start_ticks: process_generation()?,
            argv_sha256: argv_digest(argv)?,
            environment_sha256: environment.digest(),
            target,
            core,
            original,
            descriptor: if original >= 3 {
                Some(descriptor_identity(original)?)
            } else {
                None
            },
            probe,
        };
        let frame = sealed_frame(&serde_json::to_vec(&frame)?)?;
        super::STARTUP_STDIN
            .compare_exchange(
                original,
                super::STDIN_TAKEN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                anyhow::anyhow!("startup stdin owner changed during dispatch preparation")
            })?;
        // Only after the atomic transfer succeeds does this guard own the FD.
        let descriptor = (original >= 3).then(|| unsafe { OwnedFd::from_raw_fd(original) });
        Ok(Self {
            original,
            descriptor,
            original_flags,
            frame,
            armed: false,
            restored: false,
        })
    }

    /// The last pre-exec step, after probe/custody validation. No process is
    /// spawned while the original stdin descriptor is inheritable.
    pub(super) fn arm(&mut self) -> Result<(&'static str, OsString)> {
        ensure!(!self.armed, "dispatch stdin transfer was armed twice");
        if let (Some(fd), Some(flags)) = (&self.descriptor, self.original_flags) {
            set_flags(fd.as_raw_fd(), flags & !libc::FD_CLOEXEC)?;
        }
        if let Err(error) = set_flags(self.frame.as_raw_fd(), 0) {
            if let (Some(fd), Some(flags)) = (&self.descriptor, self.original_flags) {
                set_flags(fd.as_raw_fd(), flags)
                    .context("restore stdin after frame inheritance refusal")?;
            }
            return Err(error);
        }
        self.armed = true;
        Ok((LOCATOR_ENV, self.frame.as_raw_fd().to_string().into()))
    }

    pub(super) fn environment(&self) -> (&'static str, OsString) {
        (LOCATOR_ENV, self.frame.as_raw_fd().to_string().into())
    }

    fn restore_inner(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        if let (Some(fd), Some(flags)) = (&self.descriptor, self.original_flags) {
            set_flags(fd.as_raw_fd(), flags)?;
        }
        set_flags(self.frame.as_raw_fd(), libc::FD_CLOEXEC)?;
        super::STARTUP_STDIN
            .compare_exchange(
                super::STDIN_TAKEN,
                self.original,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| anyhow::anyhow!("startup stdin owner changed before exec restoration"))?;
        if let Some(fd) = self.descriptor.take() {
            let _ = fd.into_raw_fd();
        }
        self.restored = true;
        Ok(())
    }

    pub(super) fn restore(mut self) -> Result<()> {
        self.restore_inner()
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        // Best effort during unwinding/abandonment. An explicit exec error uses
        // restore() above; it cannot report successful restoration by Drop.
        let _ = self.restore_inner();
    }
}

fn read_frame(fd: RawFd) -> Result<Frame> {
    ensure!(
        fd > 2 && !(198..=204).contains(&fd),
        "invalid dispatch input descriptor"
    );
    ensure!(
        descriptor_flags(fd)? & libc::FD_CLOEXEC == 0,
        "dispatch input descriptor was not inherited across exec"
    );
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    ensure!(
        seals >= 0 && seals & SEALS == SEALS,
        "dispatch input frame is not immutable"
    );
    let identity = descriptor_identity(fd)?;
    ensure!(
        identity.mode & libc::S_IFMT == libc::S_IFREG,
        "dispatch input frame is not a sealed regular file"
    );
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    ensure!(
        unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == 0,
        "cannot size dispatch input frame: {}",
        std::io::Error::last_os_error()
    );
    let size = unsafe { stat.assume_init() }.st_size;
    ensure!(
        size > 0 && size as u64 <= MAX_FRAME_BYTES as u64,
        "dispatch input frame exceeds its bound or is empty"
    );
    let mut bytes = vec![0u8; size as usize];
    // pread never changes the shared offset, including for a sealed frame.
    let mut offset = 0;
    while offset < bytes.len() {
        let count = unsafe {
            libc::pread(
                fd,
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
                offset as libc::off_t,
            )
        };
        if count < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        ensure!(
            count > 0,
            "cannot read complete dispatch input frame: {}",
            std::io::Error::last_os_error()
        );
        offset += count as usize;
    }
    let frame: Frame = serde_json::from_slice(&bytes).context("invalid dispatch input frame")?;
    ensure!(
        frame.schema == SCHEMA && valid_encoding(frame.original),
        "unsupported dispatch input encoding"
    );
    ensure!(
        (frame.original >= 3) == frame.descriptor.is_some(),
        "dispatch input descriptor/error encoding is incomplete"
    );
    Ok(frame)
}

fn restore_original_stdin_closedness(original: i32) -> Result<()> {
    if original != -libc::EBADF - 1 {
        return Ok(());
    }
    // Only the fully authenticated handoff's exact original EBADF encoding
    // authorizes this close. Rust startup may have replaced fd0 with /dev/null,
    // even when the preceding exec inherited a physically closed descriptor.
    // Never retry close: on Linux an error can follow descriptor retirement.
    if unsafe { libc::close(libc::STDIN_FILENO) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EBADF) {
            return Err(error).context("cannot restore original closed stdin after dispatch");
        }
    }
    Ok(())
}

/// Target-side entry, before fbinit/private handlers/retirement. This never
/// reads fd0 and never adopts a shared-input manifest. The returned environment
/// is the exact original environment, excluding only our appended locator.
pub(super) fn adopt(
    fd: RawFd,
    argv: &[OsString],
    environment: &Environment,
    compiled: &ExecutableIdentity,
    core: &CoreCompatibility,
) -> Result<Environment> {
    public_argv(argv)?;
    ensure!(
        matches!(compiled, ExecutableIdentity::Backend { .. }),
        "a core executable cannot redispatch an input handoff"
    );
    let frame = read_frame(fd)?;
    ensure!(
        frame.pid == std::process::id() && frame.start_ticks == process_generation()?,
        "dispatch input belongs to another process generation"
    );
    ensure!(
        &frame.target == compiled && frame.argv_sha256 == argv_digest(argv)?,
        "dispatch input is bound to another executable or raw argv"
    );
    let original_environment = environment.original(fd, &frame.environment_sha256)?;
    frame.core.require_exact(core)?;
    let fresh = super::STARTUP_STDIN.load(Ordering::Acquire);
    ensure!(
        valid_encoding(fresh),
        "target binary did not reserve startup stdin exactly once"
    );
    ensure!(
        fresh != fd && frame.original != fd,
        "dispatch frame aliases a stdin owner"
    );
    ensure!(
        frame.probe.fd != fd && frame.probe.fd != fresh && frame.probe.fd != frame.original,
        "held-probe cleanup aliases an input owner"
    );
    frame.probe.complete_after(|| {
        if let Some(expected) = &frame.descriptor {
            ensure!(
                frame.original != fresh,
                "dispatch cannot adopt its fresh sanitized-stdin capture"
            );
            ensure!(
                descriptor_flags(frame.original)? & libc::FD_CLOEXEC == 0,
                "original stdin reservation was not inherited across exec"
            );
            ensure!(
                &descriptor_identity(frame.original)? == expected,
                "original stdin descriptor identity/status changed across dispatch"
            );
        }
        Ok(())
    })?;
    // All validation is complete before either input descriptor is adopted.
    set_flags(fd, libc::FD_CLOEXEC)?;
    let frame_owner = unsafe { OwnedFd::from_raw_fd(fd) };
    let original_owner = if frame.original >= 3 {
        set_flags(frame.original, libc::FD_CLOEXEC)?;
        Some(unsafe { OwnedFd::from_raw_fd(frame.original) })
    } else {
        None
    };
    super::STARTUP_STDIN
        .compare_exchange(
            fresh,
            super::STDIN_TAKEN,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map_err(|_| anyhow::anyhow!("target startup stdin owner changed during handoff"))?;
    if fresh >= 3 {
        drop(unsafe { OwnedFd::from_raw_fd(fresh) });
    }
    if let Some(original) = original_owner {
        let _ = original.into_raw_fd();
    }
    super::STARTUP_STDIN.store(frame.original, Ordering::Release);
    restore_original_stdin_closedness(frame.original)?;
    drop(frame_owner);
    Ok(original_environment)
}

pub(super) fn locator(value: &OsStr) -> Result<RawFd> {
    let bytes = value.as_bytes();
    ensure!(
        !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit),
        "dispatch input locator is not a descriptor"
    );
    let fd: RawFd = std::str::from_utf8(bytes)?.parse()?;
    if fd <= 2 || (198..=204).contains(&fd) {
        bail!("dispatch input locator names a reserved descriptor");
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    #[test]
    fn raw_environment_handoff_preserves_bytes_order_duplicates_and_refuses_tampering() {
        let original = Environment {
            entries: vec![
                b"X=first".to_vec(),
                b"X=second".to_vec(),
                b"EMPTY=".to_vec(),
                vec![b'R', b'=', 0xff],
                b"entry-without-equals".to_vec(),
            ],
        };
        let received = Environment {
            entries: original
                .inherited(OsStr::new("205"))
                .unwrap()
                .into_iter()
                .map(|entry| entry.into_bytes())
                .collect(),
        };
        assert_eq!(received.locator().unwrap(), Some(OsStr::new("205")));
        let recovered = received.original(205, &original.digest()).unwrap();
        assert_eq!(recovered.entries, original.entries);

        let mut reordered = Environment {
            entries: received.entries.clone(),
        };
        reordered.entries.swap(0, 1);
        assert!(reordered.original(205, &original.digest()).is_err());
        assert!(received.original(206, &original.digest()).is_err());
        let mut duplicate = Environment {
            entries: received.entries.clone(),
        };
        duplicate
            .entries
            .push(b"HERMIT_DISPATCH_INPUT_FD=206".to_vec());
        assert!(duplicate.locator().is_err());
        assert!(duplicate.original(206, &original.digest()).is_err());
        assert!(
            received.inherited(OsStr::new("206")).is_err(),
            "a target must not forward an input handoff recursively"
        );
        let loader = Environment {
            entries: vec![b"LD_PRELOAD=/not-qualified.so".to_vec()],
        };
        assert!(loader.require_probe_environment().is_err());
        Environment {
            entries: vec![b"LD_PRELOAD=".to_vec()],
        }
        .require_probe_environment()
        .unwrap();
    }

    #[test]
    fn raw_argv_binding_keeps_non_utf8_guest_controls_out_of_private_authority() {
        let argv = vec![
            OsString::from("hermit"),
            OsString::from("run"),
            OsString::from("--"),
            OsString::from("__hermit-private"),
            OsString::from("--backend=ptrace"),
            OsString::from_vec(vec![0xff]),
            OsString::new(),
        ];
        public_argv(&argv).unwrap();
        let mut changed = argv.clone();
        changed.swap(5, 6);
        assert_ne!(argv_digest(&argv).unwrap(), argv_digest(&changed).unwrap());
        changed[1] = "__hermit-private".into();
        assert!(public_argv(&changed).is_err());
        assert!(locator(OsStr::new("198")).is_err());
        assert!(locator(OsStr::new("205x")).is_err());
        assert!(locator(OsStr::new("-2")).is_err());
        assert_eq!(locator(OsStr::new("205")).unwrap(), 205);
    }

    #[test]
    fn closed_original_stdin_restoration_preserves_other_errors_and_unread_owner() {
        const CHILD: &str = "HERMIT_TEST_DISPATCH_CLOSED_STDIN_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut pipe = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let read = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
            let mut write = unsafe { File::from_raw_fd(pipe[1]) };
            let input = b"original input\0is not consumed\n";
            write.write_all(input).unwrap();
            drop(write);
            assert_eq!(unsafe { libc::dup2(read.as_raw_fd(), 0) }, 0);
            let reservation = unsafe { libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3) };
            assert!(reservation >= 3);
            let mut reservation = unsafe { File::from_raw_fd(reservation) };
            drop(read);
            let before = descriptor_identity(0).unwrap();
            let flags = descriptor_flags(0).unwrap();
            for error in [libc::EIO, libc::EMFILE, libc::EINVAL] {
                restore_original_stdin_closedness(-error - 1).unwrap();
                assert_eq!(descriptor_identity(0).unwrap(), before);
                assert_eq!(descriptor_flags(0).unwrap(), flags);
            }
            restore_original_stdin_closedness(-libc::EBADF - 1).unwrap();
            assert_eq!(unsafe { libc::fcntl(0, libc::F_GETFD) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            restore_original_stdin_closedness(-libc::EBADF - 1).unwrap();
            assert_eq!(
                descriptor_identity(reservation.as_raw_fd()).unwrap(),
                before
            );
            let mut unread = Vec::new();
            std::io::Read::read_to_end(&mut reservation, &mut unread).unwrap();
            assert_eq!(unread, input);
            return;
        }

        // A fresh unit-test process owns fd0; no multithreaded runner is forked
        // and no Hermit application or guest is entered by this control.
        let module = module_path!().split_once("::").unwrap().1;
        let name = format!(
            "{module}::closed_original_stdin_restoration_preserves_other_errors_and_unread_owner"
        );
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!("closed-stdin control exceeded 5s: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "closed-stdin control failed: {output:?}"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}
