/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native Linux controls and real Detcore DBT progress regressions for a root
//! inherited terminal. These one-run tests do not establish canonical parity.

#![cfg(feature = "dbt")]

use std::fs;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

#[path = "common/hermit_binary.rs"]
mod hermit_binary;

#[derive(Clone, Copy, Debug)]
enum Backend {
    Native,
    Dbt,
}

struct OwnedChild(std::process::Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Also reap the bounded launcher if controller setup or an assertion
        // unwinds. The maintained containing step owns descendant cleanup.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn raw_terminal() -> (fs::File, fs::File) {
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0,
        "create input PTY"
    );
    let master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    let mut settings = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut settings) },
        0
    );
    unsafe { libc::cfmakeraw(&mut settings) };
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) },
        0
    );
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) },
            0
        );
    }
    (master, slave)
}

fn nonblocking(fd: i32) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
}

struct Stream {
    retained: fs::File,
    bytes: Vec<u8>,
    eof: bool,
    error: Option<String>,
}

impl Stream {
    fn new(path: &Path) -> Self {
        Self {
            retained: fs::File::create(path).unwrap(),
            bytes: Vec::new(),
            eof: false,
            error: None,
        }
    }

    fn read_once(&mut self, source: &mut impl Read) {
        if self.eof || self.error.is_some() {
            return;
        }
        let mut buffer = [0; 8192];
        match source.read(&mut buffer) {
            Ok(0) => self.eof = true,
            Ok(length) => {
                if let Err(error) = self.retained.write_all(&buffer[..length]) {
                    self.error = Some(error.to_string());
                }
                self.bytes.extend_from_slice(&buffer[..length]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => self.error = Some(error.to_string()),
        }
    }
}

fn run(backend: Backend, exec: bool) {
    run_guest(
        backend,
        if exec { "exec" } else { "direct" },
        if exec {
            "pty-failed-exec-ok\npty-successful-exec-ok\n"
        } else {
            ""
        },
        "inherited_terminal_read.c",
    );
}

fn run_guest(backend: Backend, mode: &str, prefix: &str, guest_source: &str) {
    let exec = mode == "exec";
    let root = tempfile::Builder::new()
        .prefix("inherited-terminal-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("evidence directory")
        .keep();
    eprintln!("inherited terminal evidence: {}", root.display());
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let guest = root.join("inherited_terminal_read");
    let compiled = Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-O2", "-g", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c").join(guest_source))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("compile terminal guest");
    fs::write(root.join("compile.stdout"), &compiled.stdout).unwrap();
    fs::write(root.join("compile.stderr"), &compiled.stderr).unwrap();
    assert!(
        compiled.status.success(),
        "guest compilation failed: {compiled:?}"
    );
    assert!(!guest.with_extension("missing").exists());

    // GNU timeout supplies the same 30-second guest deadline to both paths.
    // The maintained containing step reaps every owned descendant immediately
    // after this one test returns, including descendants which changed groups.
    let mut command = Command::new("timeout");
    command.args(["--signal=TERM", "--kill-after=2s", "30s"]);
    if matches!(backend, Backend::Dbt) {
        command.arg(hermit_binary::hermit_binary()).args([
            "--backend=dbt",
            "--log=debug",
            "run",
            "--strict",
            "--base-env=minimal",
            "--epoch=2026-10-12T07:49:05+00:00",
            "--",
        ]);
    }
    command.arg(&guest).arg(mode);
    if matches!(backend, Backend::Dbt) {
        hermit_binary::configure_guest_execution(&mut command);
    }
    command
        .env_remove("RUST_LOG")
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE");
    fs::write(
        root.join("command.txt"),
        format!("backend={backend:?} exec={exec} relaxations=none\n{command:?}\n"),
    )
    .unwrap();
    let (mut input, terminal) = raw_terminal();
    nonblocking(input.as_raw_fd());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut child = OwnedChild(
        command
            .stdin(Stdio::from(terminal))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch bounded guest"),
    );
    let mut stdout = child.0.stdout.take().unwrap();
    let mut stderr = child.0.stderr.take().unwrap();
    nonblocking(stdout.as_raw_fd());
    nonblocking(stderr.as_raw_fd());
    let mut out = Stream::new(&root.join("guest.stdout"));
    let mut err = Stream::new(&root.join("guest.stderr"));
    let ready = b"pty-parent-ready\n";
    let before_input = format!("{prefix}pty-parent-ready\n").into_bytes();
    let mut input_sent = false;
    let mut controller_error = None;
    let mut status = None;
    let mut drain_until = None;
    loop {
        if status.is_none() {
            status = child.0.try_wait().expect("observe bounded guest status");
            if status.is_some() {
                drain_until = Some(Instant::now() + Duration::from_secs(1));
            }
        }
        // One bounded read per stream per iteration prevents a surviving noisy
        // writer from postponing either the status check or the drain deadline.
        out.read_once(&mut stdout);
        err.read_once(&mut stderr);
        if !input_sent
            && controller_error.is_none()
            && out.bytes.windows(ready.len()).any(|window| window == ready)
        {
            if status.is_some() || Instant::now() >= deadline {
                controller_error =
                    Some("ready marker arrived after guest deadline/exit".to_owned());
            } else if out.bytes != before_input {
                controller_error = Some("unexpected guest progress before host input".to_owned());
            } else {
                match input.write(b"X") {
                    Ok(1) => input_sent = true,
                    result => {
                        controller_error = Some(format!("write one terminal byte: {result:?}"))
                    }
                }
            }
        }
        if status.is_some()
            && ((out.eof && err.eof) || drain_until.is_some_and(|end| Instant::now() >= end))
        {
            break;
        }
        let mut descriptors = [
            libc::pollfd {
                fd: if out.eof || out.error.is_some() {
                    -1
                } else {
                    stdout.as_raw_fd()
                },
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: if err.eof || err.error.is_some() {
                    -1
                } else {
                    stderr.as_raw_fd()
                },
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
        ];
        unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 20) };
    }
    drop(input);
    let status = status.expect("bounded launcher was reaped");
    assert_eq!(child.0.wait().expect("reap bounded guest"), status);
    fs::write(root.join("status.txt"), format!("status={status}\ninput_sent={input_sent}\ncontroller_error={controller_error:?}\nstdout_eof={} stdout_error={:?}\nstderr_eof={} stderr_error={:?}\n", out.eof, out.error, err.eof, err.error)).unwrap();
    assert!(
        status.success(),
        "{backend:?} terminal guest failed: status={status}; input_sent={input_sent}; evidence={}",
        root.display()
    );
    assert!(
        input_sent && controller_error.is_none(),
        "controller: {controller_error:?}; evidence={}",
        root.display()
    );
    assert!(
        out.eof && err.eof && out.error.is_none() && err.error.is_none(),
        "incomplete guest streams; evidence={}",
        root.display()
    );
    assert_eq!(
        out.bytes,
        format!(
            "{prefix}pty-parent-ready\npty-read-a=1:X\npty-reaped-a\npty-read-b=1:B\npty-complete\n"
        )
        .as_bytes()
    );
    if matches!(backend, Backend::Dbt) {
        assert!(
            err.bytes
                .windows(b"DETLOG".len())
                .any(|window| window == b"DETLOG"),
            "real Detcore log absent; evidence={}",
            root.display()
        );
    } else {
        assert!(
            err.bytes.is_empty(),
            "native stderr: {}",
            String::from_utf8_lossy(&err.bytes)
        );
    }
}

#[test]
fn native_inherited_terminal_short_read_allows_parent_progress() {
    run(Backend::Native, false);
}

#[test]
fn native_terminal_alias_survives_failed_and_successful_exec() {
    run(Backend::Native, true);
}

#[test]
fn dbt_inherited_terminal_short_read_allows_parent_progress() {
    run(Backend::Dbt, false);
}

#[test]
fn dbt_terminal_alias_survives_failed_and_successful_exec() {
    run(Backend::Dbt, true);
}

#[test]
fn native_terminal_read_observes_peer_cleared_nonblocking_after_exec() {
    run_guest(
        Backend::Native,
        "peer-flags",
        "pty-peer-reader-restored\npty-peer-cleared\n",
        "inherited_terminal_peer_flags.c",
    );
}

#[test]
fn dbt_terminal_read_observes_peer_cleared_nonblocking_after_exec() {
    run_guest(
        Backend::Dbt,
        "peer-flags",
        "pty-peer-reader-restored\npty-peer-cleared\n",
        "inherited_terminal_peer_flags.c",
    );
}
