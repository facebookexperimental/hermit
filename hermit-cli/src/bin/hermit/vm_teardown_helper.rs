/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Exec-owned VM teardown helper. No code here runs in the container's
//! raw-clone child.
//!
//! The helper is this same binary re-executed in the private
//! `__hermit-vm-teardown-reap` role, spawned by [`ensure_installed`]
//! before the first KVM container clone (the shape the GDB watch
//! helper uses). It holds one end of a Unix socket; the other end is
//! installed in the library as the process's VM teardown sink
//! (`hermit::vm_teardown`), inherited by the container child at the
//! clone. Each dropped KVM backend forwards duplicates of its vCPU
//! and VM descriptors there, and this process closes them — vCPU
//! first — on a small worker pool, acknowledging each bundle. The
//! kernel VM destroy (an RCU grace-period wait in `close`) therefore
//! runs here, not on a guest exit path or in the exiting CLI process.
//!
//! Lifetime: the helper serves until the socket reaches EOF, which
//! needs every sender copy closed — the parent's installed sink and
//! each container child's inherited copy — then drains its queue and
//! exits. Past the run's own exit it therefore lives at most for one
//! drain of what was already adopted (concurrent destroys overlap).
//! Dying earlier is safe at every point: whatever descriptors it
//! holds close in its own exit path, so those VMs are still
//! destroyed, and later adoptions in the run see the dead socket and
//! fall back to synchronous destroys. It deliberately keeps the
//! run's process group and default signal dispositions (SIGPIPE
//! aside): a terminal interrupt, a supervisor's group or cgroup
//! kill, ends it like any other member of the run, with the same
//! kernel-close fallback.

use std::io;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use hermit::Context;
use hermit::Error;
use hermit::vm_teardown::ACK_BYTE;
use hermit::vm_teardown::READY_BYTE;
use hermit::vm_teardown::recv_vm_bundle;

pub(super) const ENTRY: &str = "__hermit-vm-teardown-reap";

/// Concurrent destroys overlap their RCU grace-period waits. The pool
/// must at least match the concurrency it replaces: without a sink,
/// every guest child destroys its VM on its own host thread, so a
/// fork-heavy run already has as many destroys in flight as it has
/// unjoined children (measured: a 4-worker pool at a 16-bundle bound
/// raised the fork_loop slope from 6.5 to 15.6 ms/fork inside one CPU
/// quota, because the backlog then serialized at adoption instead).
/// Eight concurrent closes measured ~3.6x the throughput of serial
/// ones in the Phase-0 probe.
const WORKERS: usize = 16;

fn run(control_fd: RawFd) -> Result<(), Error> {
    anyhow::ensure!(control_fd >= 3, "invalid VM teardown helper descriptor");
    // SAFETY: the private exec entry transfers this inherited
    // descriptor exactly once.
    let stream = unsafe { UnixStream::from_raw_fd(control_fd) };
    stream
        .peer_addr()
        .context("VM teardown helper descriptor is not a Unix socket")?;
    // SAFETY: the descriptor is owned above; fcntl touches no memory.
    if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error()).context("protect VM teardown helper descriptor");
    }
    super::retire_inherited_descriptors(&[stream.as_fd()])
        .context("retire unsolicited VM teardown helper descriptors")?;
    // Name the process for leftover audits: a bounded helper with this
    // comm may outlive its run by one drain, and no other process.
    // SAFETY: the name is a NUL-terminated literal within the kernel's
    // 15-byte comm limit; prctl only reads it.
    unsafe {
        libc::prctl(
            libc::PR_SET_NAME,
            c"hermit-vm-reap".as_ptr() as libc::c_ulong,
            0,
            0,
            0,
        );
        // Acknowledgements to a sender that died mid-run must report
        // EPIPE, not kill the drain with SIGPIPE.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let ack = Arc::new(Mutex::new(
        stream.try_clone().context("clone VM teardown socket")?,
    ));
    let (sender, receiver) = mpsc::channel::<(OwnedFd, OwnedFd)>();
    let receiver = Arc::new(Mutex::new(receiver));
    let mut workers = Vec::new();
    for _ in 0..WORKERS {
        let receiver = Arc::clone(&receiver);
        let ack = Arc::clone(&ack);
        workers.push(
            std::thread::Builder::new()
                .name("vm-teardown-reap".into())
                .spawn(move || {
                    loop {
                        let bundle = {
                            let receiver = receiver.lock().unwrap_or_else(|p| p.into_inner());
                            receiver.recv()
                        };
                        let Ok((vcpu, vm)) = bundle else { return };
                        drop(vcpu);
                        drop(vm);
                        let mut ack = ack.lock().unwrap_or_else(|p| p.into_inner());
                        if ack.write_all(&[ACK_BYTE]).is_err() {
                            return;
                        }
                    }
                })
                .context("spawn VM teardown worker")?,
        );
    }
    {
        let mut ack = ack.lock().unwrap_or_else(|p| p.into_inner());
        let _ = ack.write_all(&[READY_BYTE]);
    }
    let mut result = Ok(());
    loop {
        match recv_vm_bundle(stream.as_raw_fd()) {
            Ok(Some(bundle)) => {
                if sender.send(bundle).is_err() {
                    result = Err(Error::msg("VM teardown workers stopped early"));
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                result = Err(error).context("receive VM teardown bundle");
                break;
            }
        }
    }
    // EOF (or a fatal receive error): closing the channel lets the
    // workers drain what was already adopted, then stop.
    drop(sender);
    for worker in workers {
        let _ = worker.join();
    }
    result
}

/// Called before normal CLI parsing or logging. Returning None is the
/// normal CLI path; a recognized but malformed private entry is an
/// explicit error.
pub(super) fn maybe_run() -> Option<Result<(), Error>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(ENTRY)) {
        return None;
    }
    Some((|| {
        let control: RawFd = args
            .next()
            .context("missing VM teardown helper descriptor")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-text VM teardown helper descriptor"))?
            .parse()
            .context("invalid VM teardown helper descriptor")?;
        anyhow::ensure!(
            args.next().is_none(),
            "unexpected VM teardown helper arguments"
        );
        run(control)
    })())
}

#[cfg(test)]
const TEST_FD: &str = "HERMIT_TEST_VM_TEARDOWN_FD";

pub(super) fn helper_command(control: RawFd) -> Command {
    // Re-exec this exact image even if its on-disk path was replaced or
    // unlinked; resolving current_exe() would reopen whichever image
    // now owns that name.
    let mut command = Command::new("/proc/self/exe");
    #[cfg(not(test))]
    command.arg(ENTRY).arg(control.to_string());
    // A libtest executable does not enter the production main. Re-exec
    // an explicit test role which runs the identical helper.
    #[cfg(test)]
    command
        .arg("--exact")
        .arg("app::vm_teardown_helper::tests::exec_helper_entry")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(TEST_FD, control.to_string());
    // The helper must not hold the run's stdio open: an inherited
    // stdout/stderr pipe would delay its EOF until the helper drains,
    // for exactly the readers this teardown exists to unblock.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Spawns the helper and returns the sender end of its socket plus the
/// child handle. The caller decides what owns each; dropping the
/// child handle detaches it (it is reaped by init or a subreaper
/// after this process exits).
fn spawn_helper() -> Result<(std::process::Child, UnixStream), Error> {
    let (sender, helper_end) = UnixStream::pair().context("create VM teardown helper socket")?;
    let control = helper_end.as_raw_fd();
    let mut launcher = helper_command(control);
    // The only pre_exec work is async-signal-safe fcntl on the one
    // owned descriptor: it is not inheritable in this process, only
    // in the exec'd helper.
    unsafe {
        launcher.pre_exec(move || {
            if libc::fcntl(control, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = launcher.spawn().context("start VM teardown helper")?;
    drop(helper_end);
    Ok((child, sender))
}

/// Installs the process's VM teardown sink, spawning the helper on
/// first use. Call before the first container clone of a KVM launch;
/// later calls do nothing. A helper that cannot start is a warning,
/// never a failed run: with no sink installed, backends destroy
/// their VMs synchronously at drop, as they did before.
pub(super) fn ensure_installed() {
    if hermit::vm_teardown::vm_teardown_sink_installed() {
        return;
    }
    match spawn_helper() {
        Ok((child, sender)) => {
            if hermit::vm_teardown::install_vm_teardown_sink(sender) {
                // Deliberately detached: the helper outlives this
                // process by at most one drain (see the module docs)
                // and is then reaped by init or a subreaper. Waiting
                // for it here would put the teardown back on the exit
                // path this exists to remove.
                drop(child);
            } else {
                // Another thread installed first; this helper's
                // socket was dropped by the refused install, so it
                // sees EOF and exits. Reap it rather than detach a
                // process nobody adopted.
                let mut child = child;
                let _ = child.wait();
            }
        }
        Err(error) => {
            crate::app::tracing::diagnostic_eprintln!(
                "hermit: WARNING: the VM teardown helper did not start ({error:#}); \
                 KVM VM destruction stays on the dropping threads"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::time::Duration;
    use std::time::Instant;

    use hermit::vm_teardown::ACK_BYTE;
    use hermit::vm_teardown::READY_BYTE;
    use hermit::vm_teardown::send_vm_bundle;

    use super::*;

    #[test]
    fn exec_helper_entry() {
        let Some(control) = std::env::var_os(TEST_FD) else {
            return;
        };
        let control: RawFd = control.into_string().unwrap().parse().unwrap();
        // The shipped executable reserves its startup stdin in a
        // preinit hook (main.rs) before the helper role retires
        // unsolicited descriptors. This role re-executes the library's
        // unit-test image, which has no preinit hook, so establish the
        // same reservation once here, as the GDB helper's entry does.
        assert_eq!(
            super::super::STARTUP_STDIN.load(std::sync::atomic::Ordering::Acquire),
            super::super::STDIN_UNCAPTURED
        );
        // SAFETY: this fresh exec image has run nothing that changes
        // its descriptor table; libtest's main thread only waits for
        // this test.
        unsafe { super::super::capture_startup_stdin() };
        match run(control) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                super::super::display_entry_error("VM teardown helper failed", &error);
                std::process::exit(125);
            }
        }
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a valid two-element array for pipe2 to fill.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: pipe2 just created both descriptors for this call.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn is_eof(fd: &OwnedFd) -> bool {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: `poll` is one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        ready == 1 && poll.revents & libc::POLLHUP != 0
    }

    fn read_byte(stream: &mut UnixStream, deadline: Instant) -> u8 {
        stream
            .set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))
            .unwrap();
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).expect("read helper byte");
        byte[0]
    }

    /// The exec'd helper closes forwarded descriptors, acknowledges,
    /// and exits 0 once the sender closes (EOF after drain).
    #[test]
    fn helper_process_closes_bundles_and_exits_at_eof() {
        let (mut child, mut sender) = spawn_helper().expect("spawn helper");
        let deadline = Instant::now() + Duration::from_secs(30);
        assert_eq!(read_byte(&mut sender, deadline), READY_BYTE);
        let (read_a, write_a) = pipe();
        let (read_b, write_b) = pipe();
        send_vm_bundle(&sender, write_a.as_raw_fd(), write_b.as_raw_fd()).unwrap();
        drop(write_a);
        drop(write_b);
        assert_eq!(read_byte(&mut sender, deadline), ACK_BYTE);
        assert!(is_eof(&read_a) && is_eof(&read_b));
        drop(sender);
        let status = child.wait().expect("reap helper");
        assert!(status.success(), "helper exit: {status}");
    }
}
