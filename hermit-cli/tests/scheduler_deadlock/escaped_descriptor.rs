/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the LICENSE file.
 */

//! A descriptor sent outside the guest still refers to the same Linux object.
//! The host can wake its guest peer, so an idle retry is not a guest deadlock.

use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::hermit_test as hermit_binary;

struct BoundedChild(Child);

impl Drop for BoundedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            // The timeout wrapper leads this private process group. Kill only
            // while its unreaped child identity still belongs to this harness.
            unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
            let _ = self.0.wait();
        }
    }
}

fn drain(mut reader: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut retained = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("reading guest diagnostics: {error}"),
            };
            let keep = count.min((128 * 1024usize).saturating_sub(retained.len()));
            retained.extend_from_slice(&buffer[..keep]);
        }
        retained
    })
}

fn receive_descriptor(stream: &UnixStream) -> io::Result<File> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    loop {
        let mut byte = 0u8;
        let mut vector = libc::iovec {
            iov_base: std::ptr::from_mut(&mut byte).cast(),
            iov_len: 1,
        };
        // usize alignment is sufficient for cmsghdr on the supported x86_64 ABI.
        let mut control = [0usize; 8];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control);
        let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        if received == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no passed fd"));
        }
        assert_eq!(message.msg_flags & libc::MSG_CTRUNC, 0);
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if !header.is_null() {
            assert_eq!(unsafe { (*header).cmsg_level }, libc::SOL_SOCKET);
            assert_eq!(unsafe { (*header).cmsg_type }, libc::SCM_RIGHTS);
            assert_eq!(unsafe { (*header).cmsg_len }, unsafe {
                libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as usize
            });
            let fd = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>()) };
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
    }
}

fn run_case(program: &Path, mode: &str, hermit: bool) {
    let directory = tempfile::tempdir().expect("create socket directory");
    let socket_path = directory.path().join("socket");
    let listener = UnixListener::bind(&socket_path).expect("bind host receiver");
    listener.set_nonblocking(true).unwrap();
    let mut command = Command::new("timeout");
    command.args(["--kill-after=1s", "15s"]);
    if hermit {
        command
            .arg(hermit_binary::hermit_binary())
            .args(["--backend=ptrace", "run", "--strict", "--base-env=minimal"])
            .arg(format!(
                "--mount=type=bind,source={},target=/tmp/escaped-descriptor",
                directory.path().display()
            ))
            .arg("--");
    }
    command.arg(program).arg(mode).arg(if hermit {
        Path::new("/tmp/escaped-descriptor/socket")
    } else {
        &socket_path
    });
    if hermit {
        hermit_binary::configure_guest_execution(&mut command);
    }
    let child = command
        .env_remove("LD_LIBRARY_PATH")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start descriptor guest");
    let mut child = BoundedChild(child);
    let stdout = drain(child.0.stdout.take().unwrap());
    let stderr = drain(child.0.stderr.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    let transfer = loop {
        match listener.accept() {
            Ok((stream, _)) => break receive_descriptor(&stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if child.0.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "guest did not connect",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => break Err(error),
        }
    };
    let action = transfer.and_then(|mut fd| {
        // The guest has no runnable peer or virtual timer. Let it reach the
        // blocked retry before the host supplies the only source of progress.
        thread::sleep(Duration::from_millis(500));
        if mode == "write" || mode == "writev" {
            let mut bytes = [0u8; 4096];
            fd.read_exact(&mut bytes)
        } else {
            fd.write_all(b"x")
        }
    });
    let status = child.0.wait().expect("reap descriptor guest");
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    assert!(
        status.success(),
        "{mode}, hermit={hermit}: {}; host action={action:?}\nstdout: {}\nstderr: {}",
        status,
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    action.expect("host must act on the descriptor received from the guest");
    assert_eq!(String::from_utf8_lossy(&stdout), format!("RESULT {mode}\n"));
}

fn check(mode: &str) {
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("create test build root");
    let directory =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("create guest build directory");
    let program = directory.path().join("escaped_descriptor");
    let compiled = Command::new("timeout")
        .args([
            "--kill-after=1s",
            "10s",
            "cc",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/escaped_descriptor.c"))
        .arg("-o")
        .arg(&program)
        .output()
        .expect("compile descriptor guest");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    run_case(&program, mode, false);
    run_case(&program, mode, true);
}

#[test]
fn a_pipe_read_can_be_woken_through_an_escaped_write_end() {
    check("read");
}

#[test]
fn a_socketpair_read_can_be_woken_through_an_escaped_peer() {
    check("socket");
}

#[test]
fn a_pipe_write_can_be_woken_through_an_escaped_read_end() {
    check("write");
}

#[test]
fn a_pipe_writev_can_be_woken_through_an_escaped_read_end() {
    check("writev");
}
