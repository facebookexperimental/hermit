/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Maintained CLI process-lifecycle regression. An explicit private role
//! enters before runtime threads; each case has its own process because retained
//! owners deliberately keep process-wide admission closed. Actual production
//! adapter and backing guards are used, not a Detcore failure simulator.
use std::cell::RefCell;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use reverie::process::Container;
use reverie::process::Namespace;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;

#[repr(C)]
struct Shared {
    pid: AtomicI32,
    ready: AtomicI32,
    check: AtomicI32,
    observed: AtomicI32,
    stop: AtomicI32,
    drops: AtomicI32,
    retired_before_drop: AtomicI32,
    monitor_self_exit_reason: AtomicI32,
}
struct WorkerObservation {
    receiver: std::os::unix::net::UnixDatagram,
    fd: RefCell<Option<OwnedFd>>,
}
impl WorkerObservation {
    fn receive_once(&self) {
        if self.fd.borrow().is_none() {
            *self.fd.borrow_mut() = Some(receive_fd(self.receiver.as_raw_fd()));
        }
    }
}
struct Guard {
    directory: ManuallyDrop<tempfile::TempDir>,
    original: i32,
    shared: *const Shared,
    worker: Rc<WorkerObservation>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if unsafe { libc::getpid() } == self.original {
            let shared = unsafe { &*self.shared };
            if shared.pid.load(Ordering::SeqCst) > 0 {
                // Observe the descriptor transferred by the original worker's
                // parent before releasing any backing path. A later sample
                // after run returns cannot establish this ordering.
                self.worker.receive_once();
                let fd = self.worker.fd.borrow();
                shared
                    .retired_before_drop
                    .store(i32::from(ready(fd.as_ref().unwrap())), Ordering::SeqCst);
            }
            unsafe { &*self.shared }
                .drops
                .fetch_add(1, Ordering::SeqCst);
            unsafe { ManuallyDrop::drop(&mut self.directory) };
        }
    }
}
#[derive(Debug, Deserialize)]
struct Value(u64);
static BAD_WIRE: AtomicI32 = AtomicI32::new(0);
impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if BAD_WIRE.load(Ordering::SeqCst) == 1 {
            serializer.serialize_u8(7)
        } else {
            serializer.serialize_u64(self.0)
        }
    }
}
pub(crate) fn wait(deadline: Instant, mut yes: impl FnMut() -> bool) -> bool {
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        if yes() {
            return true;
        }
        std::thread::yield_now();
    }
}
pub(crate) fn pidfd(pid: i32) -> OwnedFd {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0, "pidfd: {}", std::io::Error::last_os_error());
    unsafe { OwnedFd::from_raw_fd(fd) }
}
pub(crate) fn transfer_fd(socket: i32, fd: i32) {
    let mut byte = 1u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write(libc::CMSG_DATA(c).cast::<i32>(), fd);
        assert_eq!(libc::sendmsg(socket, &msg, 0), 1);
    }
}
pub(crate) fn receive_fd(socket: i32) -> OwnedFd {
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
    msg.msg_controllen = std::mem::size_of_val(&control);
    unsafe {
        assert_eq!(
            libc::recvmsg(
                socket,
                &mut msg,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC
            ),
            1
        );
        let c = libc::CMSG_FIRSTHDR(&msg);
        assert!(!c.is_null());
        assert_eq!((*c).cmsg_type, libc::SCM_RIGHTS);
        OwnedFd::from_raw_fd(std::ptr::read(libc::CMSG_DATA(c).cast::<i32>()))
    }
}
pub(crate) fn ready(fd: &OwnedFd) -> bool {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    assert!(n >= 0, "poll refusal");
    n > 0
}
// These are fixture worker exits, never guest/product completion receipts.
const WORKER_SUPERVISOR_GONE: i32 = 38;
const WORKER_ABSOLUTE_CEILING: i32 = 39;
const WORKER_MONITOR_REFUSED: i32 = 40;

fn container_case(mode: String, deadline: Instant, end: u64) -> i32 {
    let start = Instant::now();
    assert_eq!(
        std::fs::read_dir("/proc/self/task").unwrap().count(),
        1,
        "call-before-threads precondition"
    );
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let memory = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            std::mem::size_of::<Shared>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(memory, libc::MAP_FAILED);
    let shared = memory.cast::<Shared>();
    unsafe {
        shared.write(Shared {
            pid: AtomicI32::new(0),
            ready: AtomicI32::new(0),
            check: AtomicI32::new(0),
            observed: AtomicI32::new(-1),
            stop: AtomicI32::new(0),
            drops: AtomicI32::new(0),
            retired_before_drop: AtomicI32::new(-1),
            monitor_self_exit_reason: AtomicI32::new(0),
        })
    };
    let s = unsafe { &*shared };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().to_owned();
    let (receiver, sender) = std::os::unix::net::UnixDatagram::pair().unwrap();
    let worker_observation = Rc::new(WorkerObservation {
        receiver,
        fd: RefCell::new(None),
    });
    let guard = Guard {
        directory: ManuallyDrop::new(directory),
        original: unsafe { libc::getpid() },
        shared,
        worker: worker_observation.clone(),
    };
    let child_path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let private = mode.starts_with("private-");
    let success = mode == "success";
    let mut container = Container::new();
    if private {
        container.unshare(Namespace::PID).map_root();
    }
    let selected = mode.clone();
    // Capture T while it is live, before P/G exist. P deliberately exits in
    // these cases; a death signal tied to P would invalidate their live-G proof.
    let supervisor = pidfd(unsafe { libc::getpid() });
    let worker_ceiling = end.checked_add(2_000_000_000).unwrap();
    let result = super::owned_container::run(
        &mut container,
        guard,
        path.display().to_string(),
        private,
        "owned-container-lifecycle",
        None,
        move |_| {
            if success {
                return Ok(Value(41));
            }
            let child = unsafe { libc::fork() };
            assert!(child >= 0);
            if child == 0 {
                // Host procfs remains mounted, even in the private PID namespace.
                let host_pid = std::fs::read_link("/proc/self")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .parse::<i32>()
                    .unwrap();
                s.pid.store(host_pid, Ordering::SeqCst);
                // Close inherited workload descriptors except the original T
                // pidfd. This descriptor cannot retarget after PID reuse.
                let monitor = supervisor.as_raw_fd() as u32;
                assert!(monitor >= 3);
                if monitor > 3 {
                    assert_eq!(
                        unsafe { libc::syscall(libc::SYS_close_range, 3u32, monitor - 1, 0) },
                        0
                    );
                }
                assert_eq!(
                    unsafe { libc::syscall(libc::SYS_close_range, monitor + 1, u32::MAX, 0) },
                    0
                );
                s.ready.store(1, Ordering::SeqCst);
                loop {
                    if s.check.load(Ordering::SeqCst) == 1 {
                        let exists = unsafe { libc::access(child_path.as_ptr(), libc::F_OK) } == 0;
                        s.observed.store(i32::from(exists), Ordering::SeqCst);
                    }
                    if s.stop.load(Ordering::SeqCst) == 1 {
                        unsafe { libc::_exit(37) }
                    }
                    let mut owner = libc::pollfd {
                        fd: supervisor.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let polled = unsafe { libc::poll(&mut owner, 1, 0) };
                    if polled < 0 || owner.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                        s.monitor_self_exit_reason
                            .store(WORKER_MONITOR_REFUSED, Ordering::SeqCst);
                        unsafe { libc::_exit(WORKER_MONITOR_REFUSED) }
                    }
                    if owner.revents & libc::POLLIN != 0 {
                        s.monitor_self_exit_reason
                            .store(WORKER_SUPERVISOR_GONE, Ordering::SeqCst);
                        unsafe { libc::_exit(WORKER_SUPERVISOR_GONE) }
                    }
                    // Independent of T reaching its stop store. This is the
                    // original 3s absolute budget plus the existing 2s rescue,
                    // never extra time for the original product predicate.
                    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
                    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
                        s.monitor_self_exit_reason
                            .store(WORKER_MONITOR_REFUSED, Ordering::SeqCst);
                        unsafe { libc::_exit(WORKER_MONITOR_REFUSED) }
                    }
                    let now = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
                    if now >= worker_ceiling {
                        s.monitor_self_exit_reason
                            .store(WORKER_ABSOLUTE_CEILING, Ordering::SeqCst);
                        unsafe { libc::_exit(WORKER_ABSOLUTE_CEILING) }
                    }
                    unsafe {
                        libc::sched_yield();
                    }
                }
            }
            let original_worker = pidfd(child);
            transfer_fd(sender.as_raw_fd(), original_worker.as_raw_fd());
            assert!(wait(deadline, || s.ready.load(Ordering::SeqCst) == 1));
            match selected.as_str() {
                "abnormal"
                | "private-abnormal"
                | "containment-child-assertion"
                | "containment-child-owner-death" => unsafe { libc::_exit(23) },
                "reported" => Err(anyhow::anyhow!("ORIGINAL_REPORTED_FAILURE")),
                "malformed" => {
                    BAD_WIRE.store(1, Ordering::SeqCst);
                    Ok(Value(41))
                }
                _ => panic!("unknown mode"),
            }
        },
    );
    let elapsed = start.elapsed();
    let (classification, message, retained, typed_status) = match &result {
        Ok((value, _)) => (format!("success:{}", value.0), String::new(), false, false),
        Err(error) => (
            super::classify_failure(error),
            format!("{error:#}"),
            error
                .downcast_ref::<super::owned_container::ParentCleanupUnconfirmed>()
                .is_some(),
            error
                .downcast_ref::<super::container::ContainerChildExit>()
                .is_some_and(|x| x.0 == reverie::process::ExitStatus::Exited(23)),
        ),
    };
    let worker = s.pid.load(Ordering::SeqCst);
    let fd = if worker > 0 {
        worker_observation.receive_once();
        worker_observation.fd.borrow_mut().take()
    } else {
        None
    };
    let live = fd.as_ref().is_some_and(|fd| !ready(fd));
    let exists = path.exists();
    if mode.starts_with("containment-child-") {
        // The control may trigger failure only after this actual no-namespace
        // outcome: P exited23, while original G and its backing remain live.
        assert!(!private && live && exists && retained && typed_status);
        let channel = unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(libc::STDIN_FILENO) };
        channel
            .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
            .unwrap();
        transfer_fd(channel.as_raw_fd(), fd.as_ref().unwrap().as_raw_fd());
        let proof = serde_json::to_vec(&serde_json::json!({
            "worker":worker, "live":live, "guard_exists":exists,
            "retained":retained, "typed_original_exit23":typed_status,
            "phase":"before-stop-store"
        }))
        .unwrap();
        assert_eq!(channel.send(&proof).unwrap(), proof.len());
        let mut trigger = [0u8; 1];
        channel
            .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
            .unwrap();
        assert_eq!(channel.recv(&mut trigger).unwrap(), 1);
        assert_eq!(trigger, [1]);
        // Restrict observation of this private control process before induced
        // failure. Only T's mm changes; G already exists. The assertion still
        // unwinds to exit101; this precaution is not a core-dump mechanism.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }, 0);
        assert_eq!(channel.send(&[2]).unwrap(), 1);
        if mode == "containment-child-assertion" {
            assert_ne!(
                trigger,
                [1],
                "intentional original CLI assertion before stop store"
            );
        }
        assert_eq!(mode, "containment-child-owner-death");
        // R owns the original T pidfd and kills it only after the live proof.
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    if live {
        s.check.store(1, Ordering::SeqCst);
        assert!(wait(deadline, || s.observed.load(Ordering::SeqCst) != -1));
    }
    let observed = s.observed.load(Ordering::SeqCst);
    let before_deadline = Instant::now() < deadline;
    let monitor_self_exit_reason = s.monitor_self_exit_reason.load(Ordering::SeqCst);
    let predicate = if success {
        result.as_ref().is_ok_and(|(v, _)| v.0 == 41)
            && exists
            && s.drops.load(Ordering::SeqCst) == 0
    } else if private {
        result.is_err()
            && worker > 0
            && fd.is_some()
            && !live
            && !exists
            && !retained
            && typed_status
            && s.drops.load(Ordering::SeqCst) == 1
            && s.retired_before_drop.load(Ordering::SeqCst) == 1
    } else {
        result.is_err()
            && worker > 0
            && fd.is_some()
            && live
            && exists
            && observed == 1
            && retained
            && s.drops.load(Ordering::SeqCst) == 0
            && (mode != "abnormal" || typed_status)
            && (mode != "reported" || message.contains("ORIGINAL_REPORTED_FAILURE"))
            && (mode != "malformed"
                || (message.contains("UnexpectedEnd") && message.contains("decode refused")))
    } && before_deadline
        && monitor_self_exit_reason == 0;
    println!(
        "{}",
        serde_json::json!({"mode":mode,"predicate":predicate,"before_deadline":before_deadline,"seconds":elapsed.as_secs_f64(),"worker_host_pid":worker,"worker_live_before_cleanup":live,"guard_exists":exists,"worker_observed_guard":observed,"parent_guard_drops":s.drops.load(Ordering::SeqCst),"worker_retired_before_guard_drop":s.retired_before_drop.load(Ordering::SeqCst),"retained_diagnostic":retained,"typed_original_exit23":typed_status,"class":classification,"message":message,"monitor_self_exit_reason":monitor_self_exit_reason,"phase":"original predicate before separate cleanup"})
    );
    // Separate two-second diagnostic teardown, never used to satisfy predicate.
    let rescue = Instant::now() + Duration::from_secs(2);
    s.stop.store(1, Ordering::SeqCst);
    if let Some(fd) = &fd {
        if !wait(rescue, || ready(fd)) {
            assert_eq!(
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                },
                0
            );
        }
        let mut status = 0;
        let mut got = 0;
        let mut wait_errno = None;
        assert!(wait(rescue, || {
            got = unsafe { libc::waitpid(worker, &mut status, libc::WNOHANG) };
            wait_errno = (got < 0)
                .then(|| std::io::Error::last_os_error().raw_os_error())
                .flatten();
            got != 0
        }));
        println!(
            "{}",
            serde_json::json!({"phase":"separate teardown","pid":worker,"waitpid":got,"raw_status":status,"wait_errno":wait_errno,"ready":ready(fd),"within_rescue":Instant::now()<rescue})
        );
        assert!(got == worker || (private && got == -1 && wait_errno == Some(libc::ECHILD)));
    }
    drop(result);
    if predicate { 0 } else { 1 }
}

fn containment_control(mode: &str, deadline: Instant, end: u64) -> i32 {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;

    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let selected = match mode {
        "containment-early-assertion" => "containment-child-assertion",
        "containment-cli-owner-death" => "containment-child-owner-death",
        _ => panic!("unknown containment control"),
    };
    let (channel, child_channel) = std::os::unix::net::UnixDatagram::pair().unwrap();
    channel
        .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
        .unwrap();
    let parent = unsafe { libc::getpid() };
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .env("HERMIT_INTERNAL_CLI_LIFECYCLE", "1")
        .args(["__hermit-cli-lifecycle", selected, &end.to_string()])
        .stdin(std::process::Stdio::from(OwnedFd::from(child_channel)));
    // T depends on its true supervisor R, unlike G's deliberately exiting P.
    // If a control panics, R's death stops T and G observes its original pidfd.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let cli = pidfd(i32::try_from(child.id()).unwrap());
    assert!(wait(deadline, || {
        let mut socket = libc::pollfd {
            fd: channel.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut socket, 1, 0) };
        assert!(n >= 0);
        n == 1 && socket.revents & libc::POLLIN != 0
    }));
    // The fd comes from P's original handle via T; no numeric-PID reacquisition.
    let worker = receive_fd(channel.as_raw_fd());
    let mut bytes = [0u8; 1024];
    channel
        .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
        .unwrap();
    let n = channel.recv(&mut bytes).unwrap();
    let proof: serde_json::Value = serde_json::from_slice(&bytes[..n]).unwrap();
    assert_eq!(proof["phase"], "before-stop-store");
    for field in ["live", "guard_exists", "retained", "typed_original_exit23"] {
        assert_eq!(proof[field], true, "missing actual proof: {field}");
    }
    let pid = i32::try_from(proof["worker"].as_i64().unwrap()).unwrap();
    assert!(pid > 0 && !ready(&worker) && !ready(&cli));
    let proc_path = format!("/proc/{pid}");
    let inode = std::fs::metadata(&proc_path).unwrap().ino();
    let stat = std::fs::read_to_string(format!("{proc_path}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    assert!(start > 0);
    println!(
        "{}",
        serde_json::json!({
            "phase":"sealed original live worker before induced CLI failure",
            "control":mode,"worker":pid,"start":start,"inode":inode,
            "worker_pidfd_ready":false,"proof":proof
        })
    );
    assert_eq!(channel.send(&[1]).unwrap(), 1);
    let mut armed = [0u8; 1];
    channel
        .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
        .unwrap();
    assert_eq!(channel.recv(&mut armed).unwrap(), 1);
    assert_eq!(armed, [2], "specific pre-stop trigger was not armed");
    if mode == "containment-cli-owner-death" {
        assert!(!ready(&worker) && !ready(&cli));
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    cli.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            0
        );
    }
    let mut status = None;
    assert!(wait(deadline, || {
        status = child.try_wait().unwrap();
        status.is_some()
    }));
    let status = status.unwrap();
    if mode == "containment-early-assertion" {
        assert_eq!(
            status.code(),
            Some(101),
            "the intentional assertion must remain failed"
        );
    } else {
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
    assert!(wait(deadline, || ready(&worker)));
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe {
            libc::waitid(
                libc::P_PIDFD,
                worker.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        },
        0
    );
    assert_eq!(unsafe { info.si_pid() }, pid);
    assert_eq!(info.si_code, libc::CLD_EXITED);
    // Exit39 (absolute ceiling),40 (monitor refusal),37 (ordinary stop), or
    // any rescue signal must FAIL this supervisor-loss control.
    assert_eq!(unsafe { info.si_status() }, WORKER_SUPERVISOR_GONE);
    assert_eq!(std::fs::metadata(&proc_path).unwrap().ino(), inode);
    let final_stat = std::fs::read_to_string(format!("{proc_path}/stat")).unwrap();
    assert_eq!(
        final_stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        start
    );
    assert_eq!(
        unsafe {
            libc::waitid(
                libc::P_PIDFD,
                worker.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        },
        0
    );
    assert_eq!(unsafe { info.si_pid() }, pid);
    assert_eq!(info.si_code, libc::CLD_EXITED);
    assert_eq!(unsafe { info.si_status() }, WORKER_SUPERVISOR_GONE);
    assert!(!std::path::Path::new(&proc_path).exists());
    assert!(Instant::now() < deadline);
    println!(
        "{}",
        serde_json::json!({
            "phase":"containment control only; induced CLI failure remains failed",
            "control":mode,"worker":pid,"start":start,"inode":inode,
            "actual_worker_exit":WORKER_SUPERVISOR_GONE,"naturally_reaped":true,
            "original_cli_status":status.to_string(),"rescue_used":false
        })
    );
    0
}

fn monotonic_ns() -> u64 {
    let mut now = std::mem::MaybeUninit::<libc::timespec>::uninit();
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, now.as_mut_ptr()) },
        0
    );
    let now = unsafe { now.assume_init() };
    u64::try_from(now.tv_sec).unwrap() * 1_000_000_000 + u64::try_from(now.tv_nsec).unwrap()
}

/// Like the existing fault/activation/timeout controls, this is inert unless
/// explicitly selected. It is absent from ordinary help and CLI parsing; the
/// maintained integration test enters before any runtime/libtest threads.
pub(super) fn maybe_run() -> Option<i32> {
    if std::env::var_os("HERMIT_INTERNAL_CLI_LIFECYCLE").as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return None;
    }
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) != Some("__hermit-cli-lifecycle") {
        return None;
    }
    assert!(
        (4..=5).contains(&args.len()),
        "exact private lifecycle role arguments"
    );
    assert_eq!(
        std::fs::read_dir("/proc/self/task").unwrap().count(),
        1,
        "lifecycle role must start before runtime/libtest threads"
    );
    let end: u64 = args[3].parse().unwrap();
    let remaining = end
        .checked_sub(monotonic_ns())
        .expect("deadline expired before role start");
    let deadline = Instant::now() + Duration::from_nanos(remaining);
    if matches!(
        args[2].as_str(),
        "containment-early-assertion" | "containment-cli-owner-death"
    ) {
        assert_eq!(args.len(), 4);
        Some(containment_control(&args[2], deadline, end))
    } else if args[2].starts_with("gdb-") {
        Some(super::gdb_client::lifecycle::run(
            &args[2],
            deadline,
            end,
            args.get(4).map(String::as_str),
        ))
    } else {
        assert_eq!(args.len(), 4);
        Some(container_case(args[2].clone(), deadline, end))
    }
}
