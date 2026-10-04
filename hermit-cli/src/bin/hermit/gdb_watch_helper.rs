/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exec-owned GDB supervisor. No code here runs in the container's raw-clone child.

use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::thread;
use std::time::Duration;

use hermit::Context;
use hermit::Error;
use serde::Deserialize;
use serde::Serialize;

pub(super) const ENTRY: &str = "__gdb-client-watch";
pub(super) const CLIENT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const RELEASE_RETRY_INTERVAL: Duration = Duration::from_millis(20);
const RELEASE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const RELEASE_GRACE_TICKS: u32 = 25;
const MAX_COMMAND_BYTES: usize = 8 * 1024 * 1024;
const WIRE_VERSION: u8 = 1;
pub(super) const DONE: u8 = 1;

// These are bounded state transitions, never one message per release attempt.
pub(super) const READY: u8 = 1;
pub(super) const SPAWN_FAILED: u8 = 2;
pub(super) const EXITED_BEFORE_DONE: u8 = 3;
pub(super) const RELEASE_CONNECTED: u8 = 4;
pub(super) const FINISHED: u8 = 5;
pub(super) const FAILED: u8 = 6;

#[derive(Serialize, Deserialize)]
pub(super) struct ClientCommand {
    program: Vec<u8>,
    args: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    directory: Option<Vec<u8>>,
    port: u16,
}

impl ClientCommand {
    /// The CLI's two builders use program/args and inherited standard streams.
    /// Command has no getters for stdio, env_clear, credentials or pre_exec;
    /// this internal interface does not accept those additional settings.
    pub(super) fn new(command: Command, port: u16) -> Self {
        Self {
            program: command.get_program().as_bytes().to_vec(),
            args: command
                .get_args()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            env: command
                .get_envs()
                .map(|(key, value)| {
                    (
                        key.as_bytes().to_vec(),
                        value.map(|value| value.as_bytes().to_vec()),
                    )
                })
                .collect(),
            directory: command
                .get_current_dir()
                .map(|path| path.as_os_str().as_bytes().to_vec()),
            port,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(OsString::from_vec(self.program.clone()));
        command.args(self.args.iter().cloned().map(OsString::from_vec));
        for (key, value) in &self.env {
            let key = OsString::from_vec(key.clone());
            match value {
                Some(value) => {
                    command.env(key, OsString::from_vec(value.clone()));
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        if let Some(directory) = &self.directory {
            command.current_dir(OsString::from_vec(directory.clone()));
        }
        #[cfg(test)]
        {
            command.env_remove(TEST_CONTROL_FD);
            command.env_remove(TEST_PARENT_FD);
        }
        command
    }

    pub(super) fn send(&self, stream: &mut UnixStream) -> Result<(), Error> {
        let bytes = bincode::serde::encode_to_vec(self, bincode::config::standard())?;
        anyhow::ensure!(
            bytes.len() <= MAX_COMMAND_BYTES,
            "GDB helper command is too large"
        );
        stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
        stream.write_all(&bytes)?;
        Ok(())
    }

    fn receive(stream: &mut UnixStream) -> Result<Self, Error> {
        let mut size = [0; 4];
        stream
            .read_exact(&mut size)
            .context("read GDB helper command length")?;
        let size = u32::from_le_bytes(size) as usize;
        anyhow::ensure!(size <= MAX_COMMAND_BYTES, "GDB helper command is too large");
        let mut bytes = vec![0; size];
        stream
            .read_exact(&mut bytes)
            .context("read GDB helper command")?;
        let (command, consumed) = bincode::serde::decode_from_slice(
            &bytes,
            bincode::config::standard().with_limit::<MAX_COMMAND_BYTES>(),
        )?;
        anyhow::ensure!(consumed == size, "trailing bytes in GDB helper command");
        Ok(command)
    }
}

/// All status frames have the same eight-byte, versioned shape.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Status {
    pub(super) kind: u8,
    pub(super) value: u32,
}

impl Status {
    fn bytes(&self) -> [u8; 8] {
        let value = self.value.to_le_bytes();
        [
            WIRE_VERSION,
            self.kind,
            0,
            0,
            value[0],
            value[1],
            value[2],
            value[3],
        ]
    }

    fn parse(bytes: [u8; 8]) -> Result<Self, Error> {
        anyhow::ensure!(
            bytes[0] == WIRE_VERSION && bytes[2..4] == [0, 0],
            "invalid GDB helper status frame"
        );
        anyhow::ensure!(
            matches!(
                bytes[1],
                READY | SPAWN_FAILED | EXITED_BEFORE_DONE | RELEASE_CONNECTED | FINISHED | FAILED
            ),
            "unknown GDB helper status {}",
            bytes[1]
        );
        Ok(Self {
            kind: bytes[1],
            value: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        })
    }
}

#[derive(Default)]
pub(super) struct StatusReader {
    bytes: [u8; 8],
    filled: usize,
}

impl StatusReader {
    /// Retain a partial frame across nonblocking test observations. EOF is an
    /// error even at a frame boundary until a FINISHED frame has been received.
    pub(super) fn next(&mut self, stream: &mut UnixStream) -> Result<Option<Status>, Error> {
        while self.filled < self.bytes.len() {
            match stream.read(&mut self.bytes[self.filled..]) {
                Ok(0) => anyhow::bail!(
                    "{} GDB helper status",
                    if self.filled == 0 {
                        "EOF before final"
                    } else {
                        "truncated"
                    }
                ),
                Ok(count) => self.filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error).context("read GDB helper status"),
            }
        }
        self.filled = 0;
        Status::parse(self.bytes).map(Some)
    }
}

fn send_status(stream: &mut UnixStream, kind: u8, value: u32) -> io::Result<()> {
    match stream.write_all(&Status { kind, value }.bytes()) {
        // Dropping the parent watch deliberately closes this endpoint. The
        // helper must still reap GDB, but has nobody left to report back to.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

pub(super) fn parent_exited(parent: RawFd) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: parent,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one initialized pollfd with no borrowed pointer escaping.
        let result = unsafe { libc::poll(&mut poll, 1, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        return Ok(poll.revents != 0);
    }
}

fn container_done(stream: &mut UnixStream, parent: RawFd) -> Result<bool, Error> {
    if parent_exited(parent)? {
        return Ok(true);
    }
    let mut byte = [0];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Ok(true),
            Ok(_) if byte[0] == DONE => return Ok(true),
            Ok(_) => anyhow::bail!("invalid GDB helper control message"),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error).context("read GDB helper control"),
        }
    }
}

fn supervise(
    stream: &mut UnixStream,
    parent: RawFd,
    client: &mut std::process::Child,
    port: u16,
) -> Result<(), Error> {
    // Completion or parent death stops probes, but never abandons the GDB reap.
    loop {
        match client.try_wait().context("poll GDB client")? {
            Some(_) => break,
            None if container_done(stream, parent)? => {
                client.wait().context("reap GDB client")?;
                send_status(stream, FINISHED, 0)?;
                return Ok(());
            }
            None => thread::sleep(CLIENT_POLL_INTERVAL),
        }
    }
    if container_done(stream, parent)? {
        send_status(stream, FINISHED, 0)?;
        return Ok(());
    }
    send_status(stream, EXITED_BEFORE_DONE, 0)?;
    let mut connected = false;
    while !container_done(stream, parent)? {
        let peer = SocketAddr::from(([127, 0, 0, 1], port));
        if let Ok(connection) = TcpStream::connect_timeout(&peer, RELEASE_CONNECT_TIMEOUT) {
            drop(connection);
            // This remains an unauthenticated probe. A stranger can latch the
            // existing early-exit report; success does not identify our listener.
            if !connected {
                connected = true;
                send_status(stream, RELEASE_CONNECTED, 0)?;
            }
            for _ in 0..RELEASE_GRACE_TICKS {
                if container_done(stream, parent)? {
                    send_status(stream, FINISHED, u32::from(connected))?;
                    return Ok(());
                }
                thread::sleep(RELEASE_RETRY_INTERVAL);
            }
        } else {
            thread::sleep(RELEASE_RETRY_INTERVAL);
        }
    }
    send_status(stream, FINISHED, u32::from(connected))?;
    Ok(())
}

fn run(control_fd: RawFd, parent_fd: RawFd) -> Result<(), Error> {
    anyhow::ensure!(
        control_fd >= 3 && parent_fd >= 3 && control_fd != parent_fd,
        "invalid GDB helper descriptors"
    );
    // SAFETY: the private exec entry transfers these two distinct inherited
    // descriptors exactly once. They must not survive into the GDB exec.
    let mut stream = unsafe { UnixStream::from_raw_fd(control_fd) };
    let parent = unsafe { OwnedFd::from_raw_fd(parent_fd) };
    stream
        .peer_addr()
        .context("GDB helper control descriptor is not a Unix socket")?;
    for fd in [stream.as_raw_fd(), parent.as_raw_fd()] {
        // SAFETY: both descriptors are owned above; fcntl touches no memory.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error()).context("protect GDB helper descriptors");
        }
    }
    let spec = ClientCommand::receive(&mut stream)?;
    if parent_exited(parent.as_raw_fd())? {
        return Ok(());
    }
    let mut client = match spec.command().spawn() {
        Ok(client) => client,
        Err(error) => {
            send_status(
                &mut stream,
                SPAWN_FAILED,
                error.raw_os_error().unwrap_or(libc::EIO) as u32,
            )?;
            return Err(error)
                .context("Failed to run gdb command. Please make sure it is in your $PATH.");
        }
    };
    // Ready means the helper owns an actual GDB Child, not just that it execed.
    let result = (|| {
        send_status(&mut stream, READY, client.id())?;
        stream.set_nonblocking(true)?;
        supervise(&mut stream, parent.as_raw_fd(), &mut client, spec.port)
    })();
    if let Err(error) = result {
        // Even a broken control/status channel cannot discard the owned client.
        let reaped = client
            .wait()
            .context("reap GDB after helper channel failure");
        let _ = send_status(&mut stream, FAILED, libc::EIO as u32);
        reaped?;
        return Err(error);
    }
    Ok(())
}

/// Called before normal CLI parsing or logging. Returning None is the normal
/// CLI path; a recognized but malformed private entry is an explicit error.
pub(super) fn maybe_run() -> Option<Result<(), Error>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(ENTRY)) {
        return None;
    }
    Some((|| {
        let mut descriptor = || -> Result<RawFd, Error> {
            args.next()
                .context("missing GDB helper descriptor")?
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-text GDB helper descriptor"))?
                .parse()
                .context("invalid GDB helper descriptor")
        };
        let control = descriptor()?;
        let parent = descriptor()?;
        anyhow::ensure!(args.next().is_none(), "unexpected GDB helper arguments");
        run(control, parent)
    })())
}

#[cfg(test)]
const TEST_CONTROL_FD: &str = "HERMIT_TEST_GDB_WATCH_CONTROL_FD";
#[cfg(test)]
const TEST_PARENT_FD: &str = "HERMIT_TEST_GDB_WATCH_PARENT_FD";

pub(super) fn helper_command(control: RawFd, parent: RawFd) -> Command {
    // Re-exec this exact image even if its on-disk path was replaced/unlinked.
    // Resolving current_exe() to a pathname would reopen whichever image now
    // owns that name. This project and its pidfd contract are Linux-specific.
    let mut command = Command::new("/proc/self/exe");
    #[cfg(not(test))]
    command
        .arg(ENTRY)
        .arg(control.to_string())
        .arg(parent.to_string());
    // A libtest executable does not enter the production main. Re-exec an
    // explicit test role which runs the identical exec-owned supervisor.
    #[cfg(test)]
    command
        .arg("--exact")
        .arg("gdb_watch_helper::tests::exec_helper_entry")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(TEST_CONTROL_FD, control.to_string())
        .env(TEST_PARENT_FD, parent.to_string());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_helper_entry() {
        let Some(control) = std::env::var_os(TEST_CONTROL_FD) else {
            return;
        };
        let control = control.into_string().unwrap().parse().unwrap();
        let parent = std::env::var(TEST_PARENT_FD).unwrap().parse().unwrap();
        // Do not unwind into the harness while a helper role is still live.
        match run(control, parent) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("GDB helper failed: {error:#}");
                std::process::exit(125);
            }
        }
    }

    #[test]
    fn status_rejects_unknown_and_truncated_frames() {
        assert!(Status::parse([WIRE_VERSION, 99, 0, 0, 0, 0, 0, 0]).is_err());
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer
            .write_all(
                &Status {
                    kind: READY,
                    value: 123,
                }
                .bytes()[..7],
            )
            .unwrap();
        drop(writer);
        assert!(
            StatusReader::default()
                .next(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
        let (mut reader, writer) = UnixStream::pair().unwrap();
        drop(writer);
        assert!(
            StatusReader::default()
                .next(&mut reader)
                .unwrap_err()
                .to_string()
                .contains("EOF")
        );
    }
}
