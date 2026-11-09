/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Executable custody for the pre-fbinit dispatch entry. An opened FD alone does
//! not prevent in-place writes: the successful metadata child must keep this
//! exact ELF mapped continuously until parent exec admits the same descriptor.

use std::convert::Infallible;
use std::ffi::CString;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use hermit::compatibility::CoreCompatibility;
use hermit::compatibility::ExecutableIdentity;
use hermit::compatibility::HELD_PROBE_ARG;
use hermit::compatibility::MAX_PROBE_BYTES;
use hermit::compatibility::Probe;

use super::dispatch_input::Environment;
use super::dispatch_input::ProbeReaper;
use super::dispatch_input::StopObservation;
use super::dispatch_input::Transfer;

const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const CLEANUP_GRACE: Duration = Duration::from_secs(2);
const STDERR_LIMIT: usize = 4096;

fn require_owned_child_waits() -> Result<()> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    ensure!(
        unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } == 0,
        "cannot establish held-probe child wait ownership: {}",
        std::io::Error::last_os_error()
    );
    let action = unsafe { action.assume_init() };
    // The pre-fbinit caller is single-threaded. With the default disposition
    // and no auto-reaping, a failed pidfd_open still leaves our Child's PID
    // reserved until our exact wait; a naked PID can never be reused first.
    // Do not replace an inherited handler or silently change signal policy.
    ensure!(
        action.sa_sigaction == libc::SIG_DFL && action.sa_flags & libc::SA_NOCLDWAIT == 0,
        "external dispatch requires default SIGCHLD ownership; automatic/custom child reaping is unsupported"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ImageIdentity {
    device: u64,
    inode: u64,
    length: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn image_identity(file: &File) -> Result<ImageIdentity> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.mode() & 0o111 != 0,
        "backend executable is not an executable regular file"
    );
    ensure!(
        metadata.mode() & 0o6000 == 0,
        "privileged backend executable is unsupported"
    );
    Ok(ImageIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

fn outside_reserved(fd: OwnedFd) -> Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 205) };
    ensure!(
        raw >= 0,
        "cannot allocate private dispatch descriptor: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn set_cloexec(fd: RawFd, enabled: bool) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    ensure!(
        flags >= 0,
        "dispatch descriptor is unavailable: {}",
        std::io::Error::last_os_error()
    );
    let flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == 0,
        "cannot preserve dispatch descriptor ownership: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// The first existing candidate owns any refusal: a broken adjacent binary
/// must not silently become a different PATH backend after a failed probe.
fn locate(name: &str) -> Result<PathBuf> {
    super::backend_hook::validate_name(name)?;
    let executable = format!("hermit-{name}");
    let current =
        std::env::current_exe().context("cannot locate the current executable directory")?;
    let adjacent = current
        .parent()
        .context("current executable has no directory")?
        .join(&executable);
    match std::fs::symlink_metadata(&adjacent) {
        Ok(_) => return Ok(adjacent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect the adjacent backend executable"),
    }
    let search =
        std::env::var_os("PATH").context("no adjacent backend executable and PATH is absent")?;
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(&executable);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect a PATH backend executable"),
        }
    }
    bail!(
        "backend `{name}` requires {executable}; install the matching executable adjacent to hermit or on PATH"
    )
}

struct HeldChild {
    child: Child,
    pidfd: Option<OwnedFd>,
    control: Option<OwnedFd>,
    reaped: bool,
    stopped: bool,
}

impl HeldChild {
    fn live(&self) -> Result<()> {
        let pidfd = self
            .pidfd
            .as_ref()
            .context("held probe has no exact process binding")?;
        let mut wait = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let status = unsafe { libc::poll(&mut wait, 1, 0) };
        ensure!(status == 0, "held probe mapping ended before final exec");
        Ok(())
    }

    fn stopped(&self) -> Result<()> {
        ensure!(
            self.stopped,
            "held probe was not frozen for executable custody"
        );
        ProbeReaper::require_stopped(
            self.pidfd
                .as_ref()
                .context("held probe has no pidfd")?
                .as_raw_fd(),
            self.child.id(),
        )
    }

    fn require_mapped_image(&self, image: &ImageIdentity, phase: &str) -> Result<()> {
        // The child remains ours and unreaped. The pidfd liveness check and
        // exact opened /proc exe identity bracket each stop observation; an
        // exited, signaled or exec-away child is a refusal, never success.
        self.live()?;
        let mapped = File::open(format!("/proc/{}/exe", self.child.id()))
            .with_context(|| format!("cannot bind the held probe's executable {phase}"))?;
        ensure!(
            image_identity(&mapped)? == *image,
            "held probe mapped a different executable {phase}"
        );
        self.live()
    }

    fn freeze(&mut self, started: Instant, image: &ImageIdentity) -> Result<()> {
        self.require_mapped_image(image, "before stop")?;
        let pidfd = self
            .pidfd
            .as_ref()
            .context("held probe has no pidfd")?
            .as_raw_fd();
        ensure!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd,
                    libc::SIGSTOP,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            } == 0,
            "cannot freeze the exact held probe: {}",
            std::io::Error::last_os_error()
        );
        loop {
            ensure!(
                started.elapsed() < PROBE_DEADLINE,
                "held-probe stop exceeded5seconds"
            );
            match ProbeReaper::observe_stop(pidfd, self.child.id())? {
                StopObservation::Stopped => break,
                StopObservation::Pending => self.live()?,
                StopObservation::Terminated { code, status } => {
                    bail!("held probe ended while stopping: code={code}, status={status}")
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // SIGSTOP stops the whole owned metadata process. Once its actual
        // executable is bound here it cannot exec away and release the inode's
        // write denial while the parent is preparing final descriptor exec.
        // The target only resumes this exact pidfd after final exec admission.
        self.require_mapped_image(image, "after stop")?;
        self.stopped = true;
        self.stopped()
    }

    fn abandon(&mut self) -> Result<()> {
        if self.reaped {
            return Ok(());
        }
        // Never release a successful child voluntarily while final exec is
        // still possible. This function is only the abandonment/error path.
        self.control.take();
        let signal = if let Some(pidfd) = &self.pidfd {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    Ok(())
                } else {
                    Err(error)
                }
            } else {
                Ok(())
            }
        } else {
            // The owned Child is unreaped, so its PID has not been released.
            self.child.kill()
        };
        let cleanup = Instant::now();
        while cleanup.elapsed() < CLEANUP_GRACE {
            if self.child.try_wait()?.is_some() {
                self.reaped = true;
                return signal.map_err(Into::into);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!(
            "held-probe abandonment could not reap the exact child within2seconds; signal={signal:?}"
        )
    }
}

impl Drop for HeldChild {
    fn drop(&mut self) {
        let _ = self.abandon();
    }
}

fn nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "cannot bound a private probe pipe: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn drain(pipe: &mut impl Read, output: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buffer = [0u8; 1024];
    for _ in 0..32 {
        let take = buffer.len().min(limit.saturating_sub(output.len()) + 1);
        match pipe.read(&mut buffer[..take]) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                ensure!(
                    output.len() + count <= limit,
                    "held probe exceeded its output bound"
                );
                output.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

/// Owns the only opened candidate, successful live probe mapping and exact
/// child pidfd. It cannot be replaced by a later PATH lookup or pathname exec.
pub(super) struct HeldExecutable {
    image: File,
    identity: ImageIdentity,
    backend: ExecutableIdentity,
    core: CoreCompatibility,
    probe: HeldChild,
    started: Instant,
}

impl HeldExecutable {
    pub(super) fn prepare(name: &str, core: CoreCompatibility) -> Result<Self> {
        core.validate()?;
        require_owned_child_waits()?;
        let backend = ExecutableIdentity::backend(name)?;
        let path = locate(name)?;
        let image = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("cannot open backend executable {}", path.display()))?;
        let identity = image_identity(&image)?;
        let current = std::fs::metadata("/proc/self/exe")?;
        ensure!(
            identity.device != current.dev() || identity.inode != current.ino(),
            "backend executable resolves to this same executable; recursive dispatch is refused"
        );
        let mut header = [0u8; 16];
        ensure!(
            image.read_at(&mut header, 0)? == header.len() && &header[..4] == b"\x7fELF",
            "backend must be a native ELF executable, not a script/interpreter redirect"
        );
        let capabilities = unsafe {
            libc::fgetxattr(
                image.as_raw_fd(),
                c"security.capability".as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if capabilities < 0 {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.raw_os_error() == Some(libc::ENODATA)
                    || error.raw_os_error() == Some(libc::EOPNOTSUPP),
                "cannot inspect backend privilege metadata: {error}"
            );
        } else {
            ensure!(
                capabilities == 0,
                "file-capability backend executables are unsupported"
            );
        }
        let mut sockets = [-1; 2];
        ensure!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sockets.as_mut_ptr(),
                )
            } == 0,
            "cannot allocate held-probe control: {}",
            std::io::Error::last_os_error()
        );
        let parent_control = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
        let child_control = outside_reserved(unsafe { OwnedFd::from_raw_fd(sockets[1]) })?;
        let control_fd = child_control.as_raw_fd();
        let expected_parent = unsafe { libc::getpid() };
        let started = Instant::now();
        let mut command = Command::new(format!("/proc/self/fd/{}", image.as_raw_fd()));
        command
            .arg0("hermit-compatibility-probe")
            .arg(HELD_PROBE_ARG)
            .arg(control_fd.to_string())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Only async-signal-safe syscalls run between fork and descriptor exec.
        // Every inherited private/shared-input FD>=3 is closed by that exec;
        // the explicit control socket is the sole metadata protocol exception.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
                    || libc::syscall(
                        libc::SYS_close_range,
                        3u32,
                        u32::MAX,
                        libc::CLOSE_RANGE_CLOEXEC,
                    ) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                let flags = libc::fcntl(control_fd, libc::F_GETFD);
                if flags < 0
                    || libc::fcntl(control_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("cannot execute the opened backend metadata probe")?;
        drop(child_control);
        let mut probe = HeldChild {
            child,
            pidfd: None,
            control: Some(parent_control),
            reaped: false,
            stopped: false,
        };
        let result = (|| -> Result<()> {
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, probe.child.id(), 0) };
            ensure!(
                raw >= 0,
                "cannot bind the actual held-probe process: {}",
                std::io::Error::last_os_error()
            );
            probe.pidfd = Some(outside_reserved(unsafe {
                OwnedFd::from_raw_fd(raw as RawFd)
            })?);
            let mut stdout = probe
                .child
                .stdout
                .take()
                .context("held probe stdout is absent")?;
            let mut stderr = probe
                .child
                .stderr
                .take()
                .context("held probe stderr is absent")?;
            nonblocking(stdout.as_raw_fd())?;
            nonblocking(stderr.as_raw_fd())?;
            let mut output = Vec::new();
            let mut diagnostic = Vec::new();
            let (mut stdout_end, mut stderr_end) = (false, false);
            loop {
                probe.live()?;
                ensure!(
                    started.elapsed() < PROBE_DEADLINE,
                    "held metadata probe exceeded5seconds"
                );
                if !stdout_end {
                    stdout_end = drain(&mut stdout, &mut output, MAX_PROBE_BYTES)?;
                }
                if !stderr_end {
                    stderr_end = drain(&mut stderr, &mut diagnostic, STDERR_LIMIT)?;
                }
                if stdout_end && stderr_end {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            ensure!(
                diagnostic.is_empty(),
                "held probe emitted a diagnostic: {}",
                String::from_utf8_lossy(&diagnostic)
            );
            Probe::require(&output, &backend, &core)?;
            probe.live()?;
            probe.freeze(started, &identity)?;
            ensure!(
                image_identity(&image)? == identity,
                "opened backend metadata changed during the probe"
            );
            Ok(())
        })();
        if let Err(error) = result {
            let cleanup = probe.abandon();
            return Err(error).context(format!("held-probe refusal; cleanup={cleanup:?}"));
        }
        Ok(Self {
            image,
            identity,
            backend,
            core,
            probe,
            started,
        })
    }

    fn live(&self) -> Result<()> {
        ensure!(
            self.started.elapsed() < PROBE_DEADLINE,
            "parent dispatch preparation exceeded5seconds"
        );
        self.probe.live()?;
        self.probe.stopped()?;
        self.probe
            .require_mapped_image(&self.identity, "before final exec")?;
        ensure!(
            image_identity(&self.image)? == self.identity,
            "opened backend changed before final exec"
        );
        Ok(())
    }

    /// The common selector has already stamped the proven pending report.
    /// All raw argv and environment bytes are
    /// retained. A locator names sealed input custody, never a private role.
    pub(super) fn execute(
        mut self,
        argv: &[OsString],
        original_environment: &Environment,
    ) -> Result<Infallible> {
        let mut transfer = None;
        let mut inherited_pidfd = None;
        let result = (|| -> Result<Infallible> {
            self.live()?;
            ensure!(
                original_environment.locator()?.is_none(),
                "core executable already carries a dispatch input handoff; redispatch is refused"
            );
            original_environment.require_probe_environment()?;
            original_environment.require_unchanged()?;
            let pidfd = self
                .probe
                .pidfd
                .as_ref()
                .context("held probe lost its pidfd")?
                .as_raw_fd();
            let reaper = ProbeReaper::from_parent(pidfd, self.probe.child.id())?;
            transfer = Some(Transfer::prepare(
                argv,
                original_environment,
                self.backend.clone(),
                self.core.clone(),
                reaper,
            )?);
            let input = transfer
                .as_mut()
                .context("dispatch input transfer is absent")?;
            let (_, value) = input.environment();
            let arguments = CStringList::arguments(argv)?;
            let environment = CStringList::from_storage(original_environment.inherited(&value)?);
            self.live()?;
            original_environment.require_unchanged()?;
            inherited_pidfd = Some(pidfd);
            set_cloexec(pidfd, false)?;
            input.arm()?;
            // The control endpoint remains CLOEXEC until the kernel admits
            // this exact descriptor's image. No pathname is looked up again.
            unsafe {
                libc::syscall(
                    libc::SYS_execveat,
                    self.image.as_raw_fd(),
                    c"".as_ptr(),
                    arguments.pointers.as_ptr(),
                    environment.pointers.as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            Err(std::io::Error::last_os_error())
                .context("exec of the probed backend descriptor failed")
        })();
        // Every preparation/exec error follows this same explicit cleanup.
        // Drop remains a last-resort unwind guard, never a success receipt.
        let input_restore = transfer.map_or(Ok(()), Transfer::restore);
        let pidfd_restore = inherited_pidfd.map_or(Ok(()), |pidfd| set_cloexec(pidfd, true));
        let cleanup = self.probe.abandon();
        result.with_context(|| format!("dispatch refused without fallback; stdin_restore={input_restore:?}, pidfd_restore={pidfd_restore:?}, probe_cleanup={cleanup:?}"))
    }
}

struct CStringList {
    _storage: Vec<CString>,
    pointers: Vec<*const libc::c_char>,
}

impl CStringList {
    fn from_storage(storage: Vec<CString>) -> Self {
        let mut pointers: Vec<_> = storage.iter().map(|value| value.as_ptr()).collect();
        pointers.push(std::ptr::null());
        Self {
            _storage: storage,
            pointers,
        }
    }

    fn arguments(argv: &[OsString]) -> Result<Self> {
        ensure!(!argv.is_empty(), "dispatch requires original argv0");
        Ok(Self::from_storage(
            argv.iter()
                .map(|value| CString::new(value.as_os_str().as_bytes()))
                .collect::<std::result::Result<_, _>>()?,
        ))
    }
}
