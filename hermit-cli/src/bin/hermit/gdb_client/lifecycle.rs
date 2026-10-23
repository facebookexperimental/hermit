/* Copyright (c) Meta Platforms, Inc. and affiliates. */
//! Explicit private CLI regression roles; normal watcher operations never enter here.
use std::mem::ManuallyDrop;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use super::GdbClientWatch;
use super::helper;
use crate::cli_owned_lifecycle::pidfd;
use crate::cli_owned_lifecycle::ready;
use crate::cli_owned_lifecycle::receive_fd;
use crate::cli_owned_lifecycle::transfer_fd;
use crate::cli_owned_lifecycle::wait;

struct Rescue(Vec<OwnedFd>);
impl Drop for Rescue {
    fn drop(&mut self) {
        // Exact retained handles only. These are emergency actions after a
        // failed predicate, never evidence that its original deadline was met.
        for fd in &self.0 {
            if !ready(fd) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
    }
}
fn duplicate(fd: &OwnedFd) -> OwnedFd {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    assert!(raw >= 0);
    unsafe { OwnedFd::from_raw_fd(raw) }
}
fn wait_child(pid: i32, deadline: Instant, expected: i32) {
    let mut status = 0;
    let mut got = 0;
    let mut error = None;
    assert!(
        wait(deadline, || {
            got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            error = (got < 0)
                .then(|| std::io::Error::last_os_error().raw_os_error())
                .flatten();
            got != 0
        }),
        "actual wait deadline for {pid}"
    );
    assert_eq!(got, pid, "wait errno={error:?}");
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), expected);
}
fn already_reaped(pid: i32) {
    let mut status = 0;
    let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    let error = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(
        (got, error),
        (-1, Some(libc::ECHILD)),
        "original wait already consumed"
    );
}
fn client_command(directory: &Path, end: u64) -> std::process::Command {
    let mut command = std::process::Command::new("/proc/self/exe");
    command.args([
        "__hermit-cli-lifecycle",
        "gdb-client",
        &end.to_string(),
        directory.to_str().unwrap(),
    ]);
    command
}
fn client_ready(directory: &Path, deadline: Instant) -> i32 {
    let file = directory.join("client-ready");
    assert!(wait(deadline, || file.exists()));
    // Rename publishes the complete PID, not a partially written file.
    std::fs::read_to_string(file).unwrap().parse().unwrap()
}
fn release_client(directory: &Path) {
    std::fs::write(directory.join("client-exit"), b"exit").unwrap();
}
fn listener() -> TcpListener {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}
fn connection(listener: &TcpListener, deadline: Instant) {
    assert!(
        wait(deadline, || match listener.accept() {
            Ok((stream, _)) => {
                drop(stream);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("accept: {error}"),
        }),
        "actual helper release connection missing"
    );
}

pub(crate) fn run(mode: &str, deadline: Instant, end: u64, client_path: Option<&str>) -> i32 {
    if mode == "gdb-client" {
        let directory = Path::new(client_path.unwrap());
        std::fs::write(
            directory.join("client-ready-tmp"),
            unsafe { libc::getpid() }.to_string(),
        )
        .unwrap();
        std::fs::rename(
            directory.join("client-ready-tmp"),
            directory.join("client-ready"),
        )
        .unwrap();
        assert!(wait(deadline, || directory.join("client-exit").exists()));
        return 17;
    }
    match mode {
        "gdb-foreign-copy" => foreign_copy(deadline, end),
        "gdb-parent-death-alias" => parent_death_alias(deadline, end),
        "gdb-creator-thread-exit" => creator_thread_exit(deadline, end),
        _ => panic!("unknown GDB lifecycle case"),
    }
    0
}

fn foreign_copy(deadline: Instant, end: u64) {
    let directory = tempfile::tempdir().unwrap();
    let listener = listener();
    let port = listener.local_addr().unwrap().port();
    let mut watch = GdbClientWatch::spawn(client_command(directory.path(), end), port).unwrap();
    let client = client_ready(directory.path(), deadline);
    let helper_pid = watch.helper.as_ref().unwrap().id() as i32;
    let client_fd = pidfd(client);
    let helper_fd = pidfd(helper_pid);
    let _rescue = Rescue(vec![duplicate(&client_fd), duplicate(&helper_fd)]);
    assert_eq!(std::fs::read_dir("/proc/self/task").unwrap().count(), 1);
    let copied = unsafe { libc::fork() };
    assert!(copied >= 0);
    if copied == 0 {
        assert!(!watch.owner.is_current().unwrap());
        assert!(
            watch
                .finish()
                .unwrap_err()
                .to_string()
                .contains("outside its owning process")
        );
        drop(watch);
        assert_eq!(
            std::fs::read_dir("/proc/self/task").unwrap().count(),
            1,
            "foreign Drop must not create a reaper thread"
        );
        unsafe { libc::_exit(0) }
    }
    wait_child(copied, deadline, 0);
    assert!(!ready(&helper_fd) && !ready(&client_fd));
    assert!(!watch.done_sent);
    release_client(directory.path());
    connection(&listener, deadline);
    assert!(
        watch.finish().unwrap(),
        "real release was observed before original-owner finish"
    );
    assert!(ready(&helper_fd) && ready(&client_fd));
    already_reaped(helper_pid);
    assert!(Instant::now() < deadline);
    println!(
        "GDB foreign-copy: refused finish, inert Drop, actual later release, original helper reaped"
    );
}

fn creator_thread_exit(deadline: Instant, end: u64) {
    let directory = tempfile::tempdir().unwrap();
    let listener = listener();
    let port = listener.local_addr().unwrap().port();
    let command = client_command(directory.path(), end);
    let creator = std::thread::spawn(move || GdbClientWatch::spawn(command, port).unwrap());
    let mut watch = creator.join().unwrap();
    let client = client_ready(directory.path(), deadline);
    let helper_pid = watch.helper.as_ref().unwrap().id() as i32;
    let client_fd = pidfd(client);
    let helper_fd = pidfd(helper_pid);
    let _rescue = Rescue(vec![duplicate(&client_fd), duplicate(&helper_fd)]);
    assert!(watch.owner.is_current().unwrap());
    assert!(
        !ready(&watch.owner.pidfd),
        "creator-thread exit is not process exit"
    );
    release_client(directory.path());
    connection(&listener, deadline);
    assert!(watch.finish().unwrap());
    assert!(ready(&helper_fd) && ready(&client_fd));
    already_reaped(helper_pid);
    assert!(Instant::now() < deadline);
    println!(
        "GDB creator-thread-exit: original process pidfd live, supervision continued, helper reaped"
    );
}

fn parent_death_alias(deadline: Instant, end: u64) {
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let directory = tempfile::tempdir().unwrap();
    let listener = listener();
    let port = listener.local_addr().unwrap().port();
    let (receiver, sender) = UnixDatagram::pair().unwrap();
    let parent = unsafe { libc::fork() };
    assert!(parent >= 0);
    if parent == 0 {
        let watch = GdbClientWatch::spawn(client_command(directory.path(), end), port).unwrap();
        let client = client_ready(directory.path(), deadline);
        let helper_pid = watch.helper.as_ref().unwrap().id() as i32;
        let client_fd = pidfd(client);
        let helper_fd = pidfd(helper_pid);
        let alias = unsafe { libc::fork() };
        assert!(alias >= 0);
        if alias == 0 {
            let _watch = ManuallyDrop::new(watch);
            std::fs::write(directory.path().join("alias-ready"), b"socket retained").unwrap();
            assert!(wait(deadline, || directory
                .path()
                .join("alias-exit")
                .exists()));
            unsafe { libc::_exit(29) }
        }
        let alias_fd = pidfd(alias);
        assert!(wait(deadline, || directory
            .path()
            .join("alias-ready")
            .exists()));
        for fd in [
            client_fd.as_raw_fd(),
            helper_fd.as_raw_fd(),
            alias_fd.as_raw_fd(),
            watch.stream.as_ref().unwrap().as_raw_fd(),
        ] {
            transfer_fd(sender.as_raw_fd(), fd);
        }
        let meta = serde_json::json!({"client":client,"helper":helper_pid,"alias":alias});
        std::fs::write(directory.path().join("meta-tmp"), meta.to_string()).unwrap();
        std::fs::rename(
            directory.path().join("meta-tmp"),
            directory.path().join("meta"),
        )
        .unwrap();
        assert!(wait(deadline, || directory
            .path()
            .join("parent-exit")
            .exists()));
        // No watcher Drop/Done. A live descendant AND this test's retained IPC
        // alias prevent EOF. Only the original parent pidfd proves its death.
        unsafe { libc::_exit(19) }
    }
    let parent_fd = pidfd(parent);
    let mut rescue = Rescue(vec![duplicate(&parent_fd)]);
    assert!(wait(deadline, || directory.path().join("meta").exists()));
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("meta")).unwrap()).unwrap();
    let client = meta["client"].as_i64().unwrap() as i32;
    let helper_pid = meta["helper"].as_i64().unwrap() as i32;
    let alias = meta["alias"].as_i64().unwrap() as i32;
    let client_fd = receive_fd(receiver.as_raw_fd());
    let helper_fd = receive_fd(receiver.as_raw_fd());
    let alias_fd = receive_fd(receiver.as_raw_fd());
    let control_fd = receive_fd(receiver.as_raw_fd());
    use std::os::fd::IntoRawFd;
    let mut control = unsafe { UnixStream::from_raw_fd(control_fd.into_raw_fd()) };
    control.set_nonblocking(true).unwrap();
    rescue.0.extend([
        duplicate(&client_fd),
        duplicate(&helper_fd),
        duplicate(&alias_fd),
    ]);
    assert!(!ready(&client_fd) && !ready(&helper_fd) && !ready(&alias_fd));
    std::fs::write(directory.path().join("parent-exit"), b"exit").unwrap();
    wait_child(parent, deadline, 19);
    assert!(ready(&parent_fd));
    assert!(!ready(&alias_fd) && !ready(&client_fd) && !ready(&helper_fd));
    release_client(directory.path());
    let mut statuses = helper::StatusReader::default();
    assert!(wait(deadline, || {
        match statuses.next(&mut control).unwrap() {
            None => false,
            Some(status) => {
                assert_eq!(
                    status,
                    helper::Status {
                        kind: helper::FINISHED,
                        value: 0
                    },
                    "parent death must stop release probes despite open aliases"
                );
                true
            }
        }
    }));
    wait_child(helper_pid, deadline, 0);
    assert!(ready(&helper_fd) && ready(&client_fd) && !ready(&alias_fd));
    already_reaped(client);
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    std::fs::write(directory.path().join("alias-exit"), b"exit").unwrap();
    wait_child(alias, deadline, 29);
    assert!(ready(&alias_fd));
    assert!(Instant::now() < deadline);
    println!(
        "GDB parent-death-alias: parent19 reaped, live socket alias retained, no probe, helper0/client reaped, alias29"
    );
}
