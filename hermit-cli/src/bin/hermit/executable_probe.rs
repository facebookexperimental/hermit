/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Identity-only entry. This runs before fbinit, private roles, inherited input
//! adoption, descriptor retirement, log budgets and application parsing.
//!
//! The caller of a subprocess probe owns its finite deadline and output pipes.
//! This entry never reads stdin, opens a runtime resource, or accepts authority
//! from a user-supplied backend option/environment value.

use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::ensure;
use hermit::compatibility::ExecutableIdentity;
use hermit::compatibility::HELD_PROBE_ARG;
use hermit::compatibility::PROBE_ARG;
use hermit::compatibility::Probe;

enum Request {
    Single,
    Held(RawFd),
}

fn requested() -> Option<Result<Request>> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.as_slice() {
        [_, option] if option == PROBE_ARG => Some(Ok(Request::Single)),
        [_, option, ..] if option == PROBE_ARG => Some(Err(anyhow::anyhow!(
            "identity probe accepts no further arguments"
        ))),
        [_, option, control] if option == HELD_PROBE_ARG => Some((|| {
            let bytes = control.as_os_str().as_bytes();
            ensure!(
                !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit),
                "held probe requires a control descriptor"
            );
            let fd: RawFd = std::str::from_utf8(bytes)?.parse()?;
            validate_control(fd)?;
            Ok(Request::Held(fd))
        })()),
        [_, option, ..] if option == HELD_PROBE_ARG => Some(Err(anyhow::anyhow!(
            "held probe requires exactly one control descriptor"
        ))),
        _ => None,
    }
}

/// Metadata writes have a finite local bound. NONBLOCK is restored even on an
/// error; inherited stdout/stderr open-file descriptions must not be changed
/// for another process after this entry returns.
fn write_bounded(fd: RawFd, bytes: &[u8]) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(flags >= 0, "probe output descriptor is unavailable");
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "cannot bound probe output"
    );
    let result = (|| -> Result<()> {
        let started = Instant::now();
        let mut written = 0;
        while written < bytes.len() {
            ensure!(
                started.elapsed() < Duration::from_secs(2),
                "probe output deadline exceeded"
            );
            let count =
                unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
            if count > 0 {
                written += count as usize;
                continue;
            }
            ensure!(count != 0, "probe output made no progress");
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    let remaining = Duration::from_secs(2).saturating_sub(started.elapsed());
                    let mut wait = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
                    let status = unsafe { libc::poll(&mut wait, 1, timeout) };
                    if status < 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
                    {
                        continue;
                    }
                    ensure!(
                        status > 0 && wait.revents & libc::POLLOUT != 0,
                        "probe output deadline/refusal"
                    );
                }
                _ => return Err(error.into()),
            }
        }
        Ok(())
    })();
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } == 0,
        "cannot restore probe output flags"
    );
    result
}

fn validate_control(fd: RawFd) -> Result<()> {
    ensure!(
        fd > 2 && !(198..=204).contains(&fd),
        "held probe control uses a reserved descriptor"
    );
    ensure!(
        super::STARTUP_STDIN.load(std::sync::atomic::Ordering::Acquire) != fd,
        "held probe cannot borrow its startup stdin reservation"
    );
    let mut kind: i32 = 0;
    let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut i32).cast(),
                &mut size,
            )
        } == 0
            && kind == libc::SOCK_SEQPACKET,
        "held probe control is not a dedicated seqpacket socket"
    );
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let mut address_size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    ensure!(
        unsafe { libc::getsockname(fd, address.as_mut_ptr().cast(), &mut address_size) } == 0
            && unsafe { address.assume_init() }.sun_family as i32 == libc::AF_UNIX,
        "held probe control is not a Unix socket"
    );
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut credentials_size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut credentials_size,
            )
        } == 0,
        "cannot bind held probe parent"
    );
    ensure!(
        unsafe { credentials.assume_init() }.pid == unsafe { libc::getppid() },
        "held probe control belongs to another parent"
    );
    let mut death_signal: i32 = 0;
    ensure!(
        unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut death_signal, 0, 0, 0) } == 0
            && death_signal == libc::SIGKILL,
        "held probe lacks parent-death custody"
    );
    Ok(())
}

fn hold(control: RawFd) -> i32 {
    // No post-response timeout: voluntarily releasing this mapping before
    // parent exec would reopen the inode-write gap. The parent owns the finite
    // deadline and kills/reaps its child on abandonment. We never read bytes
    // from this socket (even MSG_PEEK), much less any guest stdin descriptor.
    loop {
        let mut wait = libc::pollfd {
            fd: control,
            events: libc::POLLIN | libc::POLLRDHUP,
            revents: 0,
        };
        let status = unsafe { libc::poll(&mut wait, 1, -1) };
        if status < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if status <= 0 {
            return 125;
        }
        if wait.revents & (libc::POLLHUP | libc::POLLRDHUP) != 0 {
            return 0;
        }
        // The parent only closes the socket. Data is an invalid protocol, not
        // input to consume or authority to enter an application/private role.
        if wait.revents & (libc::POLLIN | libc::POLLERR | libc::POLLNVAL) != 0 {
            return 125;
        }
    }
}

fn report(result: Result<(Vec<u8>, Request)>) -> i32 {
    match result {
        Ok((bytes, request)) => {
            if write_bounded(libc::STDOUT_FILENO, &bytes).is_err() {
                return 125;
            }
            match request {
                Request::Single => 0,
                Request::Held(control) => {
                    // EOF frames the complete response while the executable
                    // itself stays mapped on the held control socket.
                    unsafe {
                        libc::close(libc::STDOUT_FILENO);
                        libc::close(libc::STDERR_FILENO);
                    }
                    hold(control)
                }
            }
        }
        Err(error) => {
            // No ordinary display/logging path: it creates app state. The
            // protocol parent also caps this separate diagnostic pipe.
            let message = format!("Hermit executable compatibility refused: {error}\n");
            let bytes = message.as_bytes();
            let _ = write_bounded(libc::STDERR_FILENO, &bytes[..bytes.len().min(4096)]);
            125
        }
    }
}

pub(super) fn core() -> Option<i32> {
    requested().map(|request| {
        report((|| {
            let request = request?;
            let bytes = Probe::current(ExecutableIdentity::Core)?.encode()?;
            Ok((bytes, request))
        })())
    })
}

pub(super) fn backend(name: &str, input_handoff: bool) -> Option<i32> {
    requested().map(|request| report((|| {
        let request = request?;
        ensure!(input_handoff || matches!(&request, Request::Single),
            "held backend probe requires executable_entry_with_backend and its complete input handoff");
        super::backend_hook::validate_name(name)?;
        let reserved = super::STARTUP_STDIN.load(std::sync::atomic::Ordering::Acquire);
        ensure!(reserved >= 3 || (-4096..=-2).contains(&reserved),
            "backend executable lacks its exactly-once preinit stdin reservation");
        let bytes = Probe::current(ExecutableIdentity::backend(name)?)?.encode()?;
        Ok((bytes, request))
    })()))
}
