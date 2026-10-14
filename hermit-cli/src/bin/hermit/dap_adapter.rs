/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Starting `hermit-dap`, the Debug Adapter Protocol adapter that is built
//! beside `hermit`, for `run --dap-listen` and `replay --dap-listen`.

use std::net::TcpListener;
use std::os::fd::AsRawFd;
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
             build it with `cargo build -p hermit --bin hermit --bin hermit-dap`",
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

/// Supervises `run`, a `hermit run --gdbserver` on `gdbserver_port`. Once its
/// gdbserver listens, starts `hermit-dap` on `listener` and prints the attach
/// hint naming `program`. Returns the run's status when it ends. If the
/// adapter ends, for any reason, while the gdbserver still waits for its one
/// connection, no debugger can ever attach, so this ends the run and fails.
/// An adapter whose debugger connected and later left is
/// <https://github.com/rrnewton/hermit/issues/4313>.
pub fn supervise(
    mut run: Child,
    listener: TcpListener,
    gdbserver_port: u16,
    program: &Path,
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
                // Said in one write before hermit-dap shares this stderr, so
                // its own lines never land in the middle of this one.
                let line = format!(
                    "hermit run: starting hermit-dap to serve this run to one DAP client; {}\n",
                    attach_hint(program, gdbserver_port)
                );
                let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
                let started = listener
                    .take()
                    .context("the DAP listener was already used")
                    .and_then(|listener| ListeningAdapter::spawn(listener, gdb));
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
    /// Starts the adapter on `listener`, which it inherits as [`LISTEN_FD`],
    /// running `gdb` if given.
    pub fn spawn(listener: TcpListener, gdb: Option<&Path>) -> Result<Self, Error> {
        let adapter = hermit_dap_beside_hermit()?;
        let listener_fd = listener.as_raw_fd();
        let mut command = Command::new(&adapter);
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
