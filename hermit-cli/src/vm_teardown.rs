/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Handing KVM VM destruction to a longer-lived helper process.
//!
//! Closing a VM fd waits out a kernel RCU grace period in
//! `kvm_destroy_vm` (facebookexperimental/reverie#132), on whoever
//! closes last. A short-lived CLI gains nothing by moving that close
//! to another of its own threads: process exit closes every descriptor
//! the threads have not reached, in the exiting process, and inside
//! one CPU quota the deferred waits stop hiding under guest execution
//! (measured; see the commit that added this module). So the binary
//! instead spawns a small helper process (its own image re-executed in
//! a private role, `__hermit-vm-teardown-reap` in the bin's
//! vm_teardown_helper module) before the container clone, and this
//! sink passes each dropped backend's duplicated vCPU and VM
//! descriptors to it over a Unix socket with SCM_RIGHTS. The kernel
//! takes the file references at `sendmsg`, so the backend's own closes
//! right afterwards only drop references and the helper's closes are
//! the destroys.
//!
//! Ownership and lifetime, exactly:
//!
//! - The bin installs one sink per process, before the first container
//!   clone, through [`install_vm_teardown_sink`]. The container child
//!   inherits the installed sink and its socket at the clone; only the
//!   child adopts into it. The sink starts no threads in either
//!   process — a container clone copies none, and its callback return
//!   exits only the calling thread — so adopters count the helper's
//!   acknowledgements themselves, as `Inner::receive` describes.
//! - The helper serves until the socket reaches EOF — every sender
//!   copy closed, which happens when both the parent process and its
//!   container children have exited or been killed — then drains its
//!   queue and exits. Its bound past the run's own exit is the drain:
//!   one bundle's destroy per worker thread, every prior bundle having
//!   been destroyed or concurrently in flight since its adoption. If
//!   the helper dies at any point, the kernel closes the descriptors
//!   it holds in its own exit path, so every VM is still destroyed;
//!   later adoptions see the dead socket and fall back to the
//!   backend's synchronous destroy (the sink's duplicates close
//!   locally, then the backend's own field drops are the last ones).
//! - Adoption is bounded per adopting process: at most
//!   `MAX_IN_FLIGHT_BUNDLES` unacknowledged bundles are outstanding
//!   from it, and adoption blocks past that. A bundle keeps one guest
//!   memory's pinned pages alive, so an unbounded backlog would be
//!   unbounded host memory; blocking degenerates to synchronous
//!   teardown under overload instead. Sequential container children
//!   sharing one helper can transiently sum to a few such bounds while
//!   the helper drains the previous child's tail.
//!
//! Nothing here is guest-visible: adoption happens from a backend's
//! drop, after every guest-visible effect of that exit was published
//! on the dropping thread (see reverie-kvm's `teardown` module). With
//! no sink installed, `run_kvm` leaves every backend to destroy its
//! VM synchronously, exactly as it did before this module.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// The data byte accompanying one forwarded descriptor pair.
pub const BUNDLE_BYTE: u8 = b'V';
/// The helper's per-bundle acknowledgement, sent after both closes.
pub const ACK_BYTE: u8 = b'D';
/// Sent once by the helper when its receive loop is running. Senders do
/// not wait for it (the socket buffers bundles sent before the helper
/// starts); it exists so tests and diagnostics can tell a live helper
/// from a dead socket.
pub const READY_BYTE: u8 = b'R';

/// Bundles adopted but not yet acknowledged before adoption blocks.
/// The helper's worker pool destroys several concurrently on top of
/// these, so the outstanding pinned guest memory is this plus the
/// pool size, in guest memories. Sized with the pool: a bound the
/// pool cannot drain at fork rate serializes destroys back onto
/// adopting threads (measured fork-slope regression at 16 with a
/// 4-worker pool), while an unbounded one would pin unbounded guest
/// memory. Denominated in VMs, the byte cost is the touched pages of
/// at most this many guest windows (plus the helper's 16 workers and
/// its channel): the nominal ceiling is ~80 windows' worth of touched
/// pages — real guests touch tens of MiB of their 3 GiB window, so
/// expect hundreds of MiB, not the 240 GiB a fully touched set of
/// windows would pin. Saturation needs fork/exec churn far faster
/// than the destroy drain.
#[cfg(target_arch = "x86_64")]
const MAX_IN_FLIGHT_BUNDLES: usize = 64;

/// Sends one descriptor pair (`vcpu` first, mirroring the close order)
/// with a [`BUNDLE_BYTE`] over `stream`. The kernel references both
/// files for the receiver before this returns, so the caller may close
/// its own copies immediately afterwards. `MSG_NOSIGNAL`: a dead
/// helper must report `EPIPE` to the sender (which falls back to a
/// synchronous destroy), never kill a running guest's supervisor with
/// SIGPIPE.
pub fn send_vm_bundle(stream: &UnixStream, vcpu: RawFd, vm: RawFd) -> io::Result<()> {
    let mut byte = BUNDLE_BYTE;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let fds = [vcpu, vm];
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(2 * size_of::<RawFd>() as u32) } as usize;
    loop {
        // SAFETY: `msg` names `iov` (one live byte) and `control`
        // (sized for two descriptors); both outlive the call. The
        // CMSG macros only offset inside `control`.
        let sent = unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(2 * size_of::<RawFd>() as u32) as usize;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast::<RawFd>(), 2);
            libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL)
        };
        if sent == 1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Receives one bundle sent by [`send_vm_bundle`] from `fd`.
/// `Ok(None)` is a clean EOF before a bundle byte. A malformed message
/// (wrong byte, truncated control data, a descriptor count other than
/// two) is an error, and any descriptors it carried are closed before
/// returning it: the helper treats that as fatal and exits, after
/// which senders fall back to synchronous destroys.
pub fn recv_vm_bundle(fd: RawFd) -> io::Result<Option<(OwnedFd, OwnedFd)>> {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(2 * size_of::<RawFd>() as u32) } as usize;
    let received = loop {
        // SAFETY: as in send; `msg`, `iov` and `control` outlive the call.
        let received = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if received >= 0 {
            break received;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
        // A retry must not inherit a half-described control buffer.
        msg.msg_controllen = unsafe { libc::CMSG_SPACE(2 * size_of::<RawFd>() as u32) } as usize;
        msg.msg_flags = 0;
    };
    if received == 0 {
        return Ok(None);
    }
    // Take ownership of whatever descriptors arrived before judging the
    // message, so every error path below closes them.
    let mut taken: Vec<OwnedFd> = Vec::new();
    // SAFETY: CMSG_FIRSTHDR offsets inside the received control buffer;
    // a null header (no ancillary data) is the malformed case.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if !cmsg.is_null() {
        // SAFETY: `cmsg` describes the len-validated control buffer.
        let (level, ty, len) = unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len) };
        if level == libc::SOL_SOCKET && ty == libc::SCM_RIGHTS {
            let header = unsafe { libc::CMSG_LEN(0) } as usize;
            if len >= header && (len - header).is_multiple_of(size_of::<RawFd>()) {
                let count = (len - header) / size_of::<RawFd>();
                // The buffer holds at most two; a sender offering more
                // sets MSG_CTRUNC and the kernel closes the excess.
                for i in 0..count {
                    // SAFETY: entry i is inside the validated data area.
                    let raw =
                        unsafe { std::ptr::read(libc::CMSG_DATA(cmsg).cast::<RawFd>().add(i)) };
                    // SAFETY: the kernel installed this descriptor for
                    // this receive; nothing else owns it.
                    taken.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
            }
        }
    }
    let truncated = msg.msg_flags & libc::MSG_CTRUNC != 0;
    if received != 1 || byte != BUNDLE_BYTE || truncated || taken.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed VM teardown bundle",
        ));
    }
    let mut taken = taken.into_iter();
    Ok(Some((
        taken.next().expect("two descriptors checked"),
        taken.next().expect("two descriptors checked"),
    )))
}

#[derive(Default)]
struct State {
    in_flight: usize,
    adopted: u64,
    destroyed: u64,
    dead: bool,
    /// Adoptions that fell back to closing locally because the helper
    /// was already dead or the send failed. The silent sync fallback
    /// this sink exists around is otherwise unobservable to an audit.
    fallbacks: u64,
    /// Holds of in-flight bundles, queued in send order as an
    /// ordering approximation: each acknowledgement drops the oldest.
    /// Pairing is NOT exact — the helper's workers close concurrently
    /// and their acks are anonymous count bytes, and sequential
    /// container children share this one socket, so an ack counted
    /// here can complete a predecessor's destroy and release this
    /// queue's head while its own VM is still being destroyed. That
    /// early release only lets the unmap race a live VM and pay the
    /// synchronous zap on the dropping thread (the pre-hold
    /// behavior); it can never make the unmap incorrect, because the
    /// descriptors the sink holds are what refcount the VM's
    /// lifetime, not the guest-memory clone. Exact pairing would
    /// need acknowledgement sequence numbers on the wire.
    holds: std::collections::VecDeque<Box<dyn std::any::Any + Send>>,
}

struct Inner {
    sender: Mutex<UnixStream>,
    /// Serializes acknowledgement receives: there is no reader thread
    /// (a container clone copies no threads, and a normal raw-clone
    /// callback return exits only its calling thread, so any thread
    /// started in the container child would keep it alive past its
    /// result). Adopters drain pending acknowledgements themselves,
    /// nonblocking, under this lock; an adopter that must wait for a
    /// free in-flight slot polls the socket, then drains again.
    receive: Mutex<()>,
    state: Mutex<State>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn fd(&self) -> RawFd {
        self.sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_raw_fd()
    }

    /// Counts every acknowledgement already waiting on the socket.
    /// Marks the helper dead at EOF or on a protocol error: any byte
    /// other than an ack or the one ready byte is taken as fatal,
    /// like a malformed bundle on the receiving side.
    fn drain_acks(&self) {
        let _receive = self
            .receive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fd = self.fd();
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: `buf` is a live 64-byte array for the call; the
            // descriptor is the sink's socket. MSG_DONTWAIT keeps the
            // socket itself blocking for sends.
            let received = unsafe {
                libc::recv(
                    fd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if received > 0 {
                let mut drop_holds = Vec::new();
                let dead = {
                    let mut state = self.lock();
                    for &byte in &buf[..received as usize] {
                        match byte {
                            ACK_BYTE => {
                                state.destroyed = state.destroyed.saturating_add(1);
                                state.in_flight = state.in_flight.saturating_sub(1);
                                if let Some(hold) = state.holds.pop_front() {
                                    drop_holds.push(hold);
                                }
                            }
                            // The helper announces readiness once; no
                            // state depends on it (the socket buffers).
                            READY_BYTE => {}
                            _ => state.dead = true,
                        }
                    }
                    if state.dead {
                        drop_holds.extend(state.holds.drain(..));
                    }
                    state.dead
                };
                // Holds drop outside the state lock (an unmap can be
                // slow), still under `receive`, so their order against
                // later acknowledgements is preserved.
                drop(drop_holds);
                if dead {
                    return;
                }
            } else if received == 0 {
                self.drop_all_holds();
                return;
            } else {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock => return,
                    io::ErrorKind::Interrupted => {}
                    _ => {
                        self.drop_all_holds();
                        return;
                    }
                }
            }
        }
    }

    /// Marks the helper dead and drops every queued hold: their VMs
    /// are being destroyed by the kernel as the helper's descriptors
    /// close (or were never adopted), so nothing needs them held past
    /// the destroy any longer. Called with `receive` held, from
    /// `drain_acks`'s terminal paths.
    fn drop_all_holds(&self) {
        let holds = {
            let mut state = self.lock();
            state.dead = true;
            std::mem::take(&mut state.holds)
        };
        drop(holds);
    }

    /// Polls until the socket is readable (an acknowledgement or EOF)
    /// or `timeout` passes (unbounded if `None`).
    fn wait_readable(&self, timeout: Option<Duration>) {
        let mut poll = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.map_or(-1, |t| {
            i32::try_from(t.as_millis().max(1)).unwrap_or(i32::MAX)
        });
        // SAFETY: `poll` is one valid pollfd for the call. Any result
        // (timeout, error, NVAL) just returns to the caller's recheck
        // of state, which decides what happens next.
        let _ = unsafe { libc::poll(&mut poll, 1, millis) };
    }
}

/// The process's [`reverie_kvm::VmTeardownSink`], forwarding dropped
/// backends' VM descriptors to the teardown helper process.
pub struct HelperTeardownSink {
    inner: Arc<Inner>,
}

impl HelperTeardownSink {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            inner: Arc::new(Inner {
                sender: Mutex::new(stream),
                receive: Mutex::new(()),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Bundles handed to the helper and acknowledged destroyed.
    pub fn counts(&self) -> (u64, u64) {
        let state = self.inner.lock();
        (state.adopted, state.destroyed)
    }

    /// Whether the helper is known gone; adoptions then close their
    /// duplicates locally and the backend's own drop destroys the VM.
    pub fn is_dead(&self) -> bool {
        self.inner.lock().dead
    }

    /// Adoptions that fell back to closing locally — the helper was
    /// already dead, or the send failed — instead of reaching it.
    pub fn fallbacks(&self) -> u64 {
        self.inner.lock().fallbacks
    }

    /// Waits until every adopted bundle is acknowledged destroyed, the
    /// helper dies, or `timeout` passes; reports whether it drained.
    /// Nothing on the run path waits — the helper draining after the
    /// run is the point — but tests and audits use this to prove no VM
    /// outlives its bound.
    pub fn wait_drained(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.inner.drain_acks();
            let remaining = {
                let state = self.inner.lock();
                if state.in_flight == 0 {
                    return true;
                }
                if state.dead {
                    return false;
                }
                match deadline.checked_duration_since(Instant::now()) {
                    Some(remaining) => remaining,
                    None => return false,
                }
            };
            self.inner.wait_readable(Some(remaining));
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl HelperTeardownSink {
    fn adopt(&self, vcpu: OwnedFd, vm: OwnedFd, hold: Option<Box<dyn std::any::Any + Send>>) {
        // Claim an in-flight slot, draining acknowledgements (and
        // polling for the next one, if the bound is reached) until a
        // slot frees or the helper turns out dead. No lock is held
        // across the poll, and no thread is started: see `receive`.
        loop {
            self.inner.drain_acks();
            {
                let mut state = self.inner.lock();
                if state.dead {
                    state.fallbacks = state.fallbacks.saturating_add(1);
                    return;
                }
                if state.in_flight < MAX_IN_FLIGHT_BUNDLES {
                    state.in_flight += 1;
                    state.adopted = state.adopted.saturating_add(1);
                    break;
                }
            }
            self.inner.wait_readable(None);
        }
        // The hold is queued under the sender lock, right after its
        // send succeeds, so the hold queue stays in the helper's
        // receive order even with concurrent adopters. Matching acks
        // to holds from there is approximate, not exact (see
        // `State::holds`): the queue order is the best ordering the
        // count-only ack protocol offers without sequence numbers.
        let mut hold = hold;
        let sent = {
            let sender = self
                .inner
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let sent = send_vm_bundle(&sender, vcpu.as_raw_fd(), vm.as_raw_fd());
            if sent.is_ok()
                && let Some(hold) = hold.take()
            {
                self.inner.lock().holds.push_back(hold);
            }
            sent
        };
        // On success the helper holds its own references and these
        // close the sink's duplicates; on failure they were never
        // transferred, and closing them here leaves the backend's own
        // field drops (which run right after adoption) as the destroy.
        drop(vcpu);
        drop(vm);
        if sent.is_err() {
            let mut state = self.inner.lock();
            state.dead = true;
            state.adopted = state.adopted.saturating_sub(1);
            state.in_flight = state.in_flight.saturating_sub(1);
            state.fallbacks = state.fallbacks.saturating_add(1);
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl reverie_kvm::VmTeardownSink for HelperTeardownSink {
    fn adopt_vm(&self, vcpu: OwnedFd, vm: OwnedFd) {
        self.adopt(vcpu, vm, None);
    }

    fn adopt_vm_with_hold(&self, vcpu: OwnedFd, vm: OwnedFd, hold: Box<dyn std::any::Any + Send>) {
        self.adopt(vcpu, vm, Some(hold));
    }
}

static SINK: OnceLock<Arc<HelperTeardownSink>> = OnceLock::new();

/// Installs the process's teardown sink on `stream`, the sender end of
/// the socket to the helper process. The binary calls this once,
/// before its first container clone, so the container child inherits
/// the installed sink and its socket. Returns whether this call
/// installed it; a later call drops its stream and changes nothing.
pub fn install_vm_teardown_sink(stream: UnixStream) -> bool {
    SINK.set(Arc::new(HelperTeardownSink::new(stream))).is_ok()
}

/// Whether a teardown sink is installed in this process.
pub fn vm_teardown_sink_installed() -> bool {
    SINK.get().is_some()
}

/// The installed teardown sink, for `run_kvm` to hand to each backend.
#[cfg(target_arch = "x86_64")]
pub fn vm_teardown_sink() -> Option<Arc<dyn reverie_kvm::VmTeardownSink>> {
    match SINK.get() {
        Some(sink) => Some(sink.clone()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "x86_64")]
    use std::io::Write;
    #[cfg(target_arch = "x86_64")]
    use std::sync::mpsc;

    #[cfg(target_arch = "x86_64")]
    use reverie_kvm::VmTeardownSink as _;

    use super::*;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a valid two-element array for pipe2 to fill.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: pipe2 just created both descriptors for this call.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn is_eof(fd: &OwnedFd) -> bool {
        // Deadline poll, not an instant check: a parallel test between
        // fork and exec transiently holds a duplicate of every pipe
        // write end in this process, so EOF can lag the closes this
        // test owns by that exec window. A genuinely leaked duplicate
        // still fails, at the deadline.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            // SAFETY: `poll` is one valid pollfd for the duration of the call.
            let ready =
                unsafe { libc::poll(&mut poll, 1, i32::try_from(timeout).unwrap_or(i32::MAX)) };
            if ready == 1 && poll.revents & libc::POLLHUP != 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// A stand-in helper: announces ready, receives bundles, closes
    /// them (vcpu first), and acknowledges each. Stops at EOF.
    #[cfg(target_arch = "x86_64")]
    fn fake_helper(mut stream: UnixStream, hold_acks: Option<mpsc::Receiver<()>>) {
        let _ = stream.write_all(&[READY_BYTE]);
        let mut held = hold_acks;
        while let Ok(Some((vcpu, vm))) = recv_vm_bundle(stream.as_raw_fd()) {
            if let Some(gate) = &held
                && gate.recv().is_err()
            {
                held = None;
            }
            drop(vcpu);
            drop(vm);
            if stream.write_all(&[ACK_BYTE]).is_err() {
                return;
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn adopted_bundles_close_in_the_helper_and_drain() {
        let (sender, helper) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || fake_helper(helper, None));
        let sink = HelperTeardownSink::new(sender);
        let (read_a, write_a) = pipe();
        let (read_b, write_b) = pipe();
        sink.adopt_vm(write_a, write_b);
        assert!(sink.wait_drained(Duration::from_secs(10)));
        assert_eq!(sink.counts(), (1, 1));
        assert!(is_eof(&read_a) && is_eof(&read_b));
        drop(sink);
        thread.join().unwrap();
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn adoption_blocks_at_the_in_flight_bound_until_an_ack() {
        let (sender, helper) = UnixStream::pair().unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || fake_helper(helper, Some(release_rx)));
        let sink = Arc::new(HelperTeardownSink::new(sender));
        let mut readers = Vec::new();
        for _ in 0..MAX_IN_FLIGHT_BUNDLES {
            let (read_a, write_a) = pipe();
            let (read_b, write_b) = pipe();
            readers.push((read_a, read_b));
            sink.adopt_vm(write_a, write_b);
        }
        // The next adoption waits for an acknowledgement that the fake
        // helper withholds until released.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let blocked = {
            let sink = Arc::clone(&sink);
            std::thread::spawn(move || {
                let (read_a, write_a) = pipe();
                let (read_b, write_b) = pipe();
                sink.adopt_vm(write_a, write_b);
                let _ = done_tx.send(());
                (read_a, read_b)
            })
        };
        assert!(
            done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "adoption past the in-flight bound did not block"
        );
        release_tx.send(()).unwrap();
        let (last_a, last_b) = blocked.join().unwrap();
        readers.push((last_a, last_b));
        // Release every remaining withheld acknowledgement.
        drop(release_tx);
        assert!(sink.wait_drained(Duration::from_secs(10)));
        assert_eq!(
            sink.counts(),
            (
                (MAX_IN_FLIGHT_BUNDLES + 1) as u64,
                (MAX_IN_FLIGHT_BUNDLES + 1) as u64
            )
        );
        for (read_a, read_b) in &readers {
            assert!(is_eof(read_a) && is_eof(read_b));
        }
        drop(sink);
        // The Arc in the blocked thread is gone; closing the last sink
        // copy above ended the socket, so the helper exits.
        thread.join().unwrap();
    }

    /// A hold survives until its bundle's acknowledgement is drained:
    /// while the helper withholds the ack, the hold is not dropped,
    /// so a guest-memory clone in it cannot unmap against a live VM.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn hold_drops_only_after_acknowledgement() {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let (sender, helper) = UnixStream::pair().unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || fake_helper(helper, Some(release_rx)));
        let sink = HelperTeardownSink::new(sender);
        let (_read_a, write_a) = pipe();
        let (_read_b, write_b) = pipe();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        sink.adopt_vm_with_hold(write_a, write_b, Box::new(Flag(Arc::clone(&dropped))));
        assert!(
            !dropped.load(std::sync::atomic::Ordering::SeqCst),
            "hold dropped before its bundle was acknowledged"
        );
        release_tx.send(()).unwrap();
        assert!(sink.wait_drained(Duration::from_secs(10)));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        drop(release_tx);
        drop(sink);
        thread.join().unwrap();
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn dead_helper_falls_back_to_local_close() {
        let (sender, helper) = UnixStream::pair().unwrap();
        drop(helper);
        let sink = HelperTeardownSink::new(sender);
        let (read_a, write_a) = pipe();
        let (read_b, write_b) = pipe();
        // The send fails (no receiver); the duplicates close locally
        // and nothing is counted as adopted.
        sink.adopt_vm(write_a, write_b);
        assert!(sink.is_dead());
        assert_eq!(sink.counts(), (0, 0));
        assert!(is_eof(&read_a) && is_eof(&read_b));
        // Later adoptions are local closes too, without blocking.
        let (read_c, write_c) = pipe();
        let (read_d, write_d) = pipe();
        sink.adopt_vm(write_c, write_d);
        assert!(is_eof(&read_c) && is_eof(&read_d));
        assert_eq!(sink.fallbacks(), 2);
    }

    #[test]
    fn malformed_bundle_is_refused_and_its_fds_closed() {
        let (sender, helper) = UnixStream::pair().unwrap();
        let (read_a, write_a) = pipe();
        // One descriptor where a bundle promises two.
        let sent = {
            let mut byte = BUNDLE_BYTE;
            let mut iov = libc::iovec {
                iov_base: (&mut byte as *mut u8).cast(),
                iov_len: 1,
            };
            let fds = [write_a.as_raw_fd()];
            let mut control = [0usize; 8];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as usize;
            // SAFETY: as in send_vm_bundle, with a one-fd payload.
            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as usize;
                std::ptr::write(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fds[0]);
                libc::sendmsg(sender.as_raw_fd(), &msg, libc::MSG_NOSIGNAL)
            }
        };
        assert_eq!(sent, 1);
        drop(write_a);
        assert!(recv_vm_bundle(helper.as_raw_fd()).is_err());
        // The received duplicate was closed with the refusal, so the
        // pipe reaches EOF once the sender stream is gone too.
        drop(sender);
        assert!(is_eof(&read_a));
    }
}
