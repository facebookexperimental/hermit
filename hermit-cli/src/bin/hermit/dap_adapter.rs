/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Starting `hermit-dap`, the Debug Adapter Protocol adapter that is built
//! beside `hermit`, for `run --dap-listen` and `replay --dap-listen`.

use std::ffi::CString;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use hermit::Context;
use hermit::Error;

/// How long a finished run waits for its adapter to end on its own, as it does
/// once the DAP client has seen the program exit and disconnected, before
/// stopping it.
const ADAPTER_EXIT_GRACE: Duration = Duration::from_secs(5);

/// The descriptor the adapter receives its listening socket on.
const LISTEN_FD: i32 = 3;

/// The `hermit-dap` beside this `hermit` executable.
pub fn hermit_dap_beside_hermit() -> Result<PathBuf, Error> {
    let adapter = std::env::current_exe()
        .context("Failed to locate hermit")?
        .with_file_name("hermit-dap");
    if !adapter.is_file() {
        anyhow::bail!(
            "--dap-listen needs hermit-dap beside hermit, and {} does not exist; \
             build it with `cargo build -p hermit-run --bin hermit --bin hermit-dap`",
            adapter.display()
        );
    }
    Ok(adapter)
}

/// The line that tells the user what a DAP client's attach request must name.
pub fn attach_hint(program: &Path, gdbserver_port: u16) -> String {
    format!(
        "its attach request needs \"program\": {:?} and \"target\": \"127.0.0.1:{gdbserver_port}\"",
        program.display().to_string()
    )
}

/// Binds `--dap-listen`'s `[HOST:]PORT` (`PORT` alone means loopback), so an
/// address that does not parse or cannot be listened on is refused here,
/// before the run starts, and the adapter is handed a socket already
/// listening.
pub fn bind(argument: &str) -> Result<TcpListener, Error> {
    let (host, port) = argument.rsplit_once(':').unwrap_or(("127.0.0.1", argument));
    let port: u16 = port.parse().map_err(|_| {
        anyhow::anyhow!("--dap-listen {argument:?} needs a port from 0 to 65535, as [HOST:]PORT")
    })?;
    anyhow::ensure!(
        !host.is_empty(),
        "--dap-listen {argument:?} needs a host before ':', or just a port"
    );
    let address = format!("{host}:{port}");
    TcpListener::bind(&address).map_err(|error| {
        anyhow::anyhow!(
            "--dap-listen cannot listen on {address}: {error}; pass another port, or port 0 \
             to let the system pick a free one"
        )
    })
}

/// `command` started so that it dies with this process: SIGKILL when this
/// process exits, armed before exec, and refused if this process had already
/// died before it was armed.
pub fn spawn_owned(command: &mut Command) -> std::io::Result<Child> {
    // SAFETY: getpid has no arguments; read before fork, compared after.
    let parent = unsafe { libc::getpid() };
    // SAFETY: only async-signal-safe calls (prctl, getppid) between fork and
    // exec; they touch no memory of the parent.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // A parent that died before the line above never sends the
            // signal: the child was already reparented. Notice that here.
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Whether anything in this network namespace listens on TCP `port`, read
/// from /proc so as not to use up a gdbserver's one connection.
fn tcp_port_is_listening(port: u16) -> bool {
    let wanted = format!(":{port:04X}");
    ["/proc/net/tcp", "/proc/net/tcp6"].iter().any(|table| {
        std::fs::read_to_string(table).is_ok_and(|text| {
            text.lines().skip(1).any(|row| {
                let fields: Vec<&str> = row.split_whitespace().collect();
                fields.len() > 3 && fields[1].ends_with(&wanted) && fields[3] == "0A"
            })
        })
    })
}

/// How often [`supervise`] looks at its two children.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a run that is being ended may take to exit after SIGTERM before
/// it is killed.
const RUN_STOP_GRACE: Duration = Duration::from_secs(10);

/// The process of `run` that its gdbserver serves: its one traced
/// descendant, which waits, stopped, for the debugger before it executes the
/// guest program, already inside the guest's namespaces.
fn traced_descendant(run: u32) -> Option<i32> {
    // Each process's parent, from /proc/PID/stat ("PID (COMM) STATE PPID ...").
    let parents: Vec<(i32, i32)> = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|entry| {
            let pid: i32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (_, rest) = stat.rsplit_once(") ")?;
            Some((pid, rest.split_whitespace().nth(1)?.parse().ok()?))
        })
        .collect();
    let mut family = vec![run as i32];
    let mut index = 0;
    while index < family.len() {
        let parent = family[index];
        family.extend(
            parents
                .iter()
                .filter(|(_, ppid)| *ppid == parent)
                .map(|(pid, _)| *pid),
        );
        index += 1;
    }
    let mut traced = family.into_iter().skip(1).filter(|pid| {
        std::fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("TracerPid:"))
                .is_some_and(|tracer| tracer.trim() != "0")
        })
    });
    let first = traced.next()?;
    traced.next().is_none().then_some(first)
}

/// `openat2(dir, path, flags, RESOLVE_IN_ROOT)`: `path` resolved as if `dir`
/// were the root, so absolute symbolic links and `..` stay inside it.
fn open_in_root(dir: &OwnedFd, path: &Path, flags: i32) -> std::io::Result<OwnedFd> {
    let relative = path.strip_prefix("/").unwrap_or(path);
    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    let path = CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `open_how` holds only integers, and zero is the value the
    // kernel requires of every field this call does not set.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;
    // SAFETY: `path` is NUL-terminated, and `how` is an `open_how` of the
    // size passed. glibc has no wrapper for openat2.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            path.as_ptr(),
            &how as *const libc::open_how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat2 returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// `path` with every symbolic link resolved as the guest whose root is `root`
/// sees it, the way exec resolves it: absolute links lead from that root.
fn canonical_in_root(root: &OwnedFd, path: &Path) -> std::io::Result<PathBuf> {
    let mut pending: std::collections::VecDeque<std::ffi::OsString> = path
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        match component.as_bytes() {
            b"/" | b"." => continue,
            b".." => {
                resolved.pop();
                continue;
            }
            _ => {}
        }
        let candidate = resolved.join(&component);
        let fd = open_in_root(root, &candidate, libc::O_PATH | libc::O_NOFOLLOW)?;
        let metadata = std::fs::File::from(fd.try_clone()?).metadata()?;
        if !metadata.file_type().is_symlink() {
            resolved = candidate;
            continue;
        }
        links += 1;
        if links > 40 {
            return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
        }
        let mut target = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: an empty path reads the link `fd` itself names, which an
        // O_PATH | O_NOFOLLOW descriptor allows, into `target`'s bytes.
        let length = unsafe {
            libc::readlinkat(
                fd.as_raw_fd(),
                c"".as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if length < 0 {
            return Err(std::io::Error::last_os_error());
        }
        target.truncate(length as usize);
        let target = PathBuf::from(std::ffi::OsString::from_vec(target));
        if target.is_absolute() {
            resolved = PathBuf::from("/");
        }
        for component in target.components().rev() {
            pending.push_front(component.as_os_str().to_owned());
        }
    }
    Ok(resolved)
}

/// Where the guest finds its program: a path it names (`Named`), or the
/// candidates of a `PATH` search, in order (`Search`), of which the guest runs
/// the first that is a regular file with an execute bit.
#[derive(Debug)]
pub enum GuestProgram {
    Named(PathBuf),
    Search(Vec<PathBuf>),
}

/// Whether `path`, followed as the guest sees it, is a regular file with an
/// execute bit: the test the run's own `PATH` search applies.
fn executable_in_root(root: &OwnedFd, path: &Path) -> bool {
    canonical_in_root(root, path)
        .and_then(|canonical| open_in_root(root, &canonical, libc::O_PATH))
        .and_then(|fd| std::fs::File::from(fd).metadata())
        .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
}

/// The host file a debugger must load for the program the guest of process
/// `guest_pid` is about to execute, `guest_program`, with `to_host` mapping a
/// guest path without symbolic links to the host file the run's mounts make
/// it. Both steps happen in the guest's own view, through its root: the
/// `PATH` search, so a mount that makes an earlier candidate executable
/// selects it there as it does for the guest, and the symbolic links of the
/// selected path. The answer is then checked: the file the guest opens there
/// must be that same host file (device and inode), readable, and an ELF
/// executable, since a debugger can load nothing else. Anything else is
/// refused with the reason, before any hint is given.
pub fn identify_debuggee(
    guest_pid: i32,
    guest_program: &GuestProgram,
    to_host: impl Fn(&Path) -> Option<PathBuf>,
) -> Result<PathBuf, Error> {
    let root = std::fs::File::open(format!("/proc/{guest_pid}/root"))
        .map(OwnedFd::from)
        .with_context(|| format!("Failed to open the root of the guest's process {guest_pid}"))?;
    let guest_program = match guest_program {
        GuestProgram::Named(path) => path.as_path(),
        GuestProgram::Search(candidates) => candidates
            .iter()
            .find(|candidate| executable_in_root(&root, candidate))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--dap-listen finds no executable among the guest's PATH candidates {:?}",
                    candidates
                )
            })?,
    };
    let canonical = canonical_in_root(&root, guest_program).with_context(|| {
        format!(
            "--dap-listen cannot follow {} through the guest's file system",
            guest_program.display()
        )
    })?;
    let host = to_host(&canonical).ok_or_else(|| {
        anyhow::anyhow!(
            "--dap-listen cannot tell the debugger which host file is the guest's {} ({}): \
             no mount of this run maps it to one",
            guest_program.display(),
            canonical.display()
        )
    })?;
    let guest_file = match open_in_root(&root, &canonical, libc::O_RDONLY) {
        Ok(fd) => std::fs::File::from(fd),
        Err(error) if error.raw_os_error() == Some(libc::EACCES) => anyhow::bail!(
            "--dap-listen cannot read {}, so a debugger could not load it either; \
             make it readable",
            guest_program.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "--dap-listen cannot open the guest's {}",
                    guest_program.display()
                )
            });
        }
    };
    let mut magic = [0u8; 4];
    let read = std::io::Read::read(&mut (&guest_file), &mut magic).unwrap_or(0);
    if magic.starts_with(b"#!") {
        anyhow::bail!(
            "--dap-listen debugs an executable, and the guest's {} is a #! script; to debug \
             its interpreter, run the interpreter with the script as its argument, for \
             example hermit run --dap-listen=ADDRESS -- /bin/sh {}",
            guest_program.display(),
            guest_program.display()
        );
    }
    anyhow::ensure!(
        read == magic.len() && &magic == b"\x7fELF",
        "--dap-listen debugs an ELF executable, and the guest's {} is not one",
        guest_program.display()
    );
    let guest_metadata = guest_file
        .metadata()
        .context("Failed to stat the guest's program")?;
    let host_metadata =
        std::fs::metadata(&host).with_context(|| format!("Failed to stat {}", host.display()))?;
    anyhow::ensure!(
        (guest_metadata.dev(), guest_metadata.ino()) == (host_metadata.dev(), host_metadata.ino()),
        "--dap-listen cannot tell the debugger which host file the guest runs: the guest's {} \
         ({}) is not the host's {}",
        guest_program.display(),
        canonical.display(),
        host.display()
    );
    Ok(host)
}

/// Supervises `run`, a `hermit run --gdbserver` on `gdbserver_port`. Once its
/// gdbserver listens and its guest process is waiting, names the host file the
/// debugger must load with `identify` (given that process), then starts
/// `hermit-dap` on `listener` and prints the attach hint. Returns the run's
/// status when it ends. If the adapter ends, for any reason, while the
/// gdbserver still waits for its one connection, no debugger can ever attach,
/// so this ends the run and fails; so does a refusal of `identify`, or a
/// missing `hermit-dap`. An adapter whose debugger connected and later left is
/// <https://github.com/rrnewton/hermit/issues/4313>.
pub fn supervise(
    mut run: Child,
    listener: TcpListener,
    gdbserver_port: u16,
    identify: impl Fn(i32) -> Result<PathBuf, Error>,
    gdb: Option<&Path>,
) -> Result<reverie::process::ExitStatus, Error> {
    let mut listener = Some(listener);
    let mut adapter: Option<ListeningAdapter> = None;
    let mut connected = false;
    loop {
        if let Some(status) = run.try_wait().context("Failed to poll the run")? {
            // Gives a connected client its grace period to finish.
            drop(adapter);
            return Ok(status.into());
        }
        let listening = tcp_port_is_listening(gdbserver_port);
        match adapter.as_mut() {
            None if listening => {
                let Some(guest) = traced_descendant(run.id()) else {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                };
                let started = hermit_dap_beside_hermit().and_then(|path| {
                    let program = identify(guest)?;
                    // Said in one write before hermit-dap shares this stderr,
                    // so its own lines never land in the middle of this one.
                    let line = format!(
                        "hermit run: starting hermit-dap to serve this run to one DAP client; {}\n",
                        attach_hint(&program, gdbserver_port)
                    );
                    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
                    let listener = listener
                        .take()
                        .context("the DAP listener was already used")?;
                    ListeningAdapter::spawn(&path, listener, gdb)
                });
                match started {
                    Ok(started) => adapter = Some(started),
                    Err(error) => {
                        end_run(&mut run);
                        return Err(error);
                    }
                }
            }
            Some(started) if !connected => {
                if !listening {
                    // The gdbserver took its one connection.
                    connected = true;
                } else if let Some(status) = started
                    .child
                    .try_wait()
                    .context("Failed to poll hermit-dap")?
                {
                    end_run(&mut run);
                    anyhow::bail!(
                        "hermit-dap ended ({status}) before a debugger connected to this run's \
                         gdbserver on port {gdbserver_port}, so none can attach; ended the run \
                         rather than leave it waiting"
                    );
                }
            }
            _ => {}
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Ends `run` with SIGTERM, then SIGKILL after [`RUN_STOP_GRACE`], and reaps it.
fn end_run(run: &mut Child) {
    // SAFETY: kill has no pointer arguments.
    unsafe {
        libc::kill(run.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + RUN_STOP_GRACE;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = run.try_wait() {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let _ = run.kill();
    let _ = run.wait();
}

/// A `hermit-dap --listen-fd` serving a `hermit run --gdbserver`. It runs
/// outside the guest's namespaces, as the GDB client of a plain
/// `--gdbserver` run does, and is stopped when this value is dropped: at once
/// if no client ever connected, or after [`ADAPTER_EXIT_GRACE`] if its
/// session has not ended by then. It also dies with this process.
#[derive(Debug)]
pub struct ListeningAdapter {
    child: Child,
}

impl ListeningAdapter {
    /// Starts `adapter` on `listener`, which it inherits as [`LISTEN_FD`],
    /// running `gdb` if given.
    pub fn spawn(adapter: &Path, listener: TcpListener, gdb: Option<&Path>) -> Result<Self, Error> {
        let listener_fd = listener.as_raw_fd();
        let mut command = Command::new(adapter);
        command
            .arg("--listen-fd")
            .arg(LISTEN_FD.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if let Some(gdb) = gdb {
            command.arg("--gdb").arg(gdb);
        }
        // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
        // exec; they touch no memory of the parent.
        unsafe {
            command.pre_exec(move || {
                // dup2 clears close-on-exec on the copy; a socket already at
                // LISTEN_FD needs it cleared by hand.
                if listener_fd == LISTEN_FD {
                    if libc::fcntl(LISTEN_FD, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(listener_fd, LISTEN_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = spawn_owned(&mut command)
            .with_context(|| format!("Failed to start {}", adapter.display()))?;
        // The adapter owns the socket now; this process's copy closes here.
        drop(listener);
        Ok(Self { child })
    }

    /// Whether the adapter is still waiting for its client. `hermit-dap`
    /// becomes GDB, by `exec`, once a client connects.
    fn still_waiting_for_a_client(&self) -> bool {
        std::fs::read_to_string(format!("/proc/{}/comm", self.child.id()))
            .is_ok_and(|comm| comm.trim_end() == "hermit-dap")
    }
}

impl Drop for ListeningAdapter {
    fn drop(&mut self) {
        let grace = if self.still_waiting_for_a_client() {
            Duration::ZERO
        } else {
            ADAPTER_EXIT_GRACE
        };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
