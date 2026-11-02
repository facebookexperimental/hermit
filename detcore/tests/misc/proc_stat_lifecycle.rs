/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The life of a descriptor of a process's own `/proc/<pid>/stat`, which
//! Detcore renders (package K slice D4c-1), under a real tracer: a snapshot
//! taken at the first read and kept by sequential reads, taken again by a
//! `pread` elsewhere, a rewind and a seek, as Linux's `seq_file` does; a
//! descriptor opened before its process is reparented, shared with
//! `SCM_RIGHTS`, and inherited by a forked child; and its answers to a mode
//! change, `syncfs` and a mapping, which these tests compare with the
//! kernel's own file read natively.
//!
//! These scenarios were the KVM executor's own stat tests
//! (`process_signal_publication.rs`, executor.rs and syncfs_tests.rs in
//! reverie-kvm) until Detcore took the file over and the executor's copy was
//! deleted.

use std::os::fd::RawFd;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

fn config() -> Config {
    Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    }
}

/// Run `guest` under Detcore and require that it exits 0.
fn under_detcore(guest: impl FnOnce() + Send) {
    let (output, _) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        guest,
        detcore_testutils::prepare_ptrace_timer_config(config()),
        true,
    )
    .unwrap();
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn open_stat(path: &str) -> RawFd {
    let path = std::ffi::CString::new(path).unwrap();
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    assert!(fd >= 0, "open: {}", std::io::Error::last_os_error());
    fd
}

/// One `read` of up to `count` bytes.
fn read_some(fd: RawFd, count: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; count];
    // SAFETY: the buffer is writable for its length.
    let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), count) };
    assert!(read >= 0, "read: {}", std::io::Error::last_os_error());
    bytes.truncate(read as usize);
    bytes
}

/// Every byte from the descriptor's position to the end.
fn read_rest(fd: RawFd) -> Vec<u8> {
    let mut all = Vec::new();
    loop {
        let chunk = read_some(fd, 4096);
        if chunk.is_empty() {
            return all;
        }
        all.extend(chunk);
    }
}

fn pread_all(fd: RawFd, offset: i64) -> Vec<u8> {
    let mut bytes = vec![0_u8; 4096];
    // SAFETY: the buffer is writable for its length.
    let read = unsafe { libc::pread(fd, bytes.as_mut_ptr().cast(), bytes.len(), offset) };
    assert!(read >= 0, "pread: {}", std::io::Error::last_os_error());
    bytes.truncate(read as usize);
    bytes
}

fn seek(fd: RawFd, offset: i64) {
    // SAFETY: a plain lseek.
    assert_eq!(unsafe { libc::lseek(fd, offset, libc::SEEK_SET) }, offset);
}

/// The fields of a whole `stat` line: the pid, the name in parentheses, then
/// fields 3 to 52.
fn fields(line: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(line).into_owned();
    assert!(text.ends_with('\n'), "not a whole line: {text:?}");
    let open = text.find(" (").expect("no name");
    let close = text.rfind(") ").expect("no name");
    let mut fields = vec![text[..open].to_owned(), text[open + 1..=close].to_owned()];
    fields.extend(text[close + 2..].split_whitespace().map(str::to_owned));
    fields
}

fn field(line: &[u8], number: usize) -> String {
    fields(line)[number - 1].clone()
}

/// Field `number` (3 or later) of the tail of a `stat` line read from a
/// position inside its pid or name: the fields after the name's `") "`.
fn tail_field(tail: &[u8], number: usize) -> String {
    let text = String::from_utf8_lossy(tail).into_owned();
    let close = text.rfind(") ").expect("no end of the name in the tail");
    text[close + 2..]
        .split_whitespace()
        .nth(number - 3)
        .expect("too few fields")
        .to_owned()
}

/// Require that `tail`, read from byte `offset` (inside the pid) of a new
/// rendering of the `stat` record `whole` was an earlier rendering of, is
/// `whole` from `offset` on, except for the CPU time `show` reads afresh:
/// utime and stime (fields 14 and 15, `do_task_stat`'s
/// `thread_group_cputime_adjusted`, fs/proc/array.c:566 and 598-599 at
/// v7.1.3) may have grown since `whole`, and never shrink (`cputime_adjust`
/// keeps both monotonic, kernel/sched/cputime.c:599-613). Every other byte
/// matches: the pid's remainder and the name exactly, the other fields one
/// by one.
fn assert_rendered_again_from(whole: &[u8], offset: usize, tail: &[u8]) {
    let text = String::from_utf8_lossy(whole).into_owned();
    let pid_end = text.find(' ').expect("no end of the pid");
    assert!(offset < pid_end, "offset {offset} is not inside the pid");
    let name_end = text.rfind(") ").expect("no end of the name") + 2;
    let prefix = name_end - offset;
    assert!(tail.len() > prefix, "short tail: {tail:?}");
    assert_eq!(tail[..prefix], whole[offset..name_end], "{tail:?}");
    let tail_text = String::from_utf8_lossy(&tail[prefix..]).into_owned();
    assert!(tail_text.ends_with('\n'), "not a whole line: {tail_text:?}");
    let later: Vec<&str> = tail_text.split_whitespace().collect();
    let earlier = &fields(whole)[2..];
    assert_eq!(later.len(), earlier.len(), "{tail_text:?}");
    for (index, (later, earlier)) in later.iter().zip(earlier).enumerate() {
        let number = index + 3;
        if number == 14 || number == 15 {
            let later: u64 = later.parse().expect("CPU time");
            let earlier: u64 = earlier.parse().expect("CPU time");
            assert!(
                later >= earlier,
                "field {number} went back: {earlier} -> {later}"
            );
        } else {
            assert_eq!(later, earlier, "field {number}: {tail_text:?}");
        }
    }
}

/// Run `test` natively in a forked child of this process, which has one
/// thread (this test process may have many) and exits 0 when `test` returns.
fn natively(test: impl FnOnce()) {
    // SAFETY: the child runs `test` and exits without returning.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
    if child == 0 {
        let passed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test)).is_ok();
        // SAFETY: the forked child ends here.
        unsafe { libc::_exit(if passed { 0 } else { 1 }) };
    }
    let mut status = 0;
    // SAFETY: waiting for the child forked above.
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the native run failed: status {status:#x}"
    );
}

/// What `answer` returns, run natively in a forked child of this process
/// (see [`natively`]), carried back through a pipe.
fn natively_answering(answer: impl FnOnce() -> String) -> String {
    let ends = pipe();
    natively(move || {
        let text = answer();
        write_all(ends[1], text.as_bytes());
    });
    // SAFETY: this process's copy of the write end, so the read below ends.
    unsafe { libc::close(ends[1]) };
    let mut text = Vec::new();
    loop {
        let chunk = read_some(ends[0], 4096);
        if chunk.is_empty() {
            break;
        }
        text.extend(chunk);
    }
    // SAFETY: the read end, read to its end.
    unsafe { libc::close(ends[0]) };
    String::from_utf8(text).unwrap()
}

/// Run `test` natively and under Detcore: the same checks hold for both.
fn natively_and_under_detcore(test: impl Fn() + Send + Sync + Copy) {
    natively(test);
    under_detcore(test);
}

/// The number of tasks a fresh rendering of this process's own `stat`
/// counts (field 20).
fn own_thread_count() -> String {
    let fd = open_stat("/proc/self/stat");
    let line = read_rest(fd);
    // SAFETY: a descriptor this guest opened.
    unsafe { libc::close(fd) };
    field(&line, 20)
}

/// Start a thread that waits until the returned sender is used or dropped,
/// and its handle.
fn a_waiting_thread() -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (started, release) = std::sync::mpsc::channel::<()>();
    let (keep, held) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        started.send(()).unwrap();
        let _ = held.recv();
    });
    release.recv().unwrap();
    (keep, thread)
}

/// Send `fd` over the Unix socket `socket` with `SCM_RIGHTS`.
fn send_fd(socket: RawFd, fd: RawFd) {
    // SAFETY: arithmetic on a length.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    // A cmsghdr's alignment: words, not bytes.
    let mut control = vec![0_u64; space.div_ceil(8)];
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&raw mut byte).cast(),
        iov_len: 1,
    };
    // SAFETY: a message built in buffers this guest owns.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space;
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        assert_eq!(libc::sendmsg(socket, &message, 0), 1);
    }
}

/// Receive one descriptor sent with `SCM_RIGHTS` on `socket`.
fn recv_fd(socket: RawFd) -> RawFd {
    // SAFETY: arithmetic on a length.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0_u64; space.div_ceil(8)];
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&raw mut byte).cast(),
        iov_len: 1,
    };
    // SAFETY: a message built in buffers this guest owns.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space;
        assert_eq!(libc::recvmsg(socket, &mut message, 0), 1);
        let header = libc::CMSG_FIRSTHDR(&message);
        assert!(!header.is_null(), "no descriptor received");
        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>())
    }
}

/// A Unix datagram socket pair.
fn socket_pair() -> [RawFd; 2] {
    let mut pair = [0; 2];
    // SAFETY: a socketpair into an array this guest owns.
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, pair.as_mut_ptr()) },
        0
    );
    pair
}

/// A pipe.
fn pipe() -> [RawFd; 2] {
    let mut ends = [0; 2];
    // SAFETY: a pipe into an array this guest owns.
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
    ends
}

fn write_all(fd: RawFd, bytes: &[u8]) {
    // SAFETY: a buffer this guest owns.
    assert_eq!(
        unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) },
        bytes.len() as isize
    );
}

/// Wait for one byte on `fd`.
fn wait_byte(fd: RawFd) -> u8 {
    let mut byte = 0_u8;
    // SAFETY: a one-byte buffer this guest owns.
    assert_eq!(unsafe { libc::read(fd, (&raw mut byte).cast(), 1) }, 1);
    byte
}

/// Fork a child that runs `child` and exits 0 when it returns, 1 when it
/// panics; the child's pid.
fn fork_running(child: impl FnOnce()) -> libc::pid_t {
    // SAFETY: the child runs `child` and exits without returning.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let passed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)).is_ok();
        // SAFETY: the forked child ends here.
        unsafe { libc::_exit(if passed { 0 } else { 1 }) };
    }
    pid
}

/// Wait for the child `pid` and require that it exited 0.
fn reap_passing(pid: libc::pid_t) {
    let mut status = 0;
    // SAFETY: waiting for a child of this guest.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child {pid} failed: status {status:#x}"
    );
}

/// The rendered file has Linux's 52 fields, with the caller's pid and the
/// parent its getppid answers, by every spelling of its own process.
#[test]
fn a_rendered_stat_has_52_fields_and_the_callers_identity() {
    under_detcore(|| {
        // SAFETY: plain identity calls.
        let (pid, tid, ppid) = unsafe { (libc::getpid(), libc::gettid(), libc::getppid()) };
        for (path, task) in [
            ("/proc/self/stat".to_owned(), pid),
            (format!("/proc/{pid}/stat"), pid),
            ("/proc/thread-self/stat".to_owned(), tid),
            (format!("/proc/self/task/{tid}/stat"), tid),
        ] {
            let fd = open_stat(&path);
            let line = read_rest(fd);
            assert_eq!(fields(&line).len(), 52, "{path}: {line:?}");
            assert_eq!(field(&line, 1), task.to_string(), "{path}");
            assert_eq!(field(&line, 3), "R", "{path}");
            assert_eq!(field(&line, 4), ppid.to_string(), "{path}");
            assert_eq!(field(&line, 20), "1", "{path}: one thread");
            // SAFETY: a descriptor this guest opened.
            unsafe { libc::close(fd) };
        }
    });
}

/// A read that continues the sequence keeps the record the first read
/// rendered; a `pread` elsewhere, a rewind and a seek render it again, as
/// Linux's `seq_read_iter` and `seq_lseek` do (`traverse`). The thread count
/// tells the renderings apart.
#[test]
fn a_started_stat_read_keeps_its_record_and_pread_or_seek_renders_again() {
    under_detcore(|| {
        let fd = open_stat("/proc/self/stat");
        let head = read_some(fd, 5);
        assert_eq!(head.len(), 5);
        let (started, release) = std::sync::mpsc::channel::<()>();
        let (keep, held) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            held.recv().unwrap();
        });
        release.recv().unwrap();
        // The rest of the first record: one thread when it was rendered.
        let mut line = head.clone();
        line.extend(read_rest(fd));
        assert_eq!(field(&line, 20), "1", "{line:?}");
        // A pread at 0, where the sequence is not, renders it again.
        let again = pread_all(fd, 0);
        assert_eq!(field(&again, 20), "2", "{again:?}");
        // A rewind renders it again too.
        seek(fd, 0);
        let rewound = read_rest(fd);
        assert_eq!(field(&rewound, 20), "2", "{rewound:?}");
        // A seek elsewhere renders it again, and reads on from there: the
        // same record, but for the CPU time the new rendering takes again.
        seek(fd, 3);
        let tail = read_rest(fd);
        assert_rendered_again_from(&rewound, 3, &tail);
        keep.send(()).unwrap();
        thread.join().unwrap();
        // SAFETY: a descriptor this guest opened.
        unsafe { libc::close(fd) };
    });
}

/// A descriptor opened before its process is reparented shows the new
/// parent at its first read, as Linux's `show` runs at the read.
#[test]
fn a_stat_opened_before_reparenting_shows_the_new_parent() {
    under_detcore(|| {
        let mut ready = [0; 2];
        let mut verdict = [0; 2];
        // SAFETY: pipes and forks this guest owns; the children only make
        // async-signal-safe calls before they exit.
        unsafe {
            assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
            assert_eq!(libc::pipe(verdict.as_mut_ptr()), 0);
            let middle = libc::fork();
            assert!(middle >= 0);
            if middle == 0 {
                let grandchild = libc::fork();
                if grandchild == 0 {
                    let original = libc::getppid();
                    let fd = open_stat("/proc/self/stat");
                    libc::write(ready[1], b"r".as_ptr().cast(), 1);
                    let mut spins = 0;
                    while libc::getppid() == original && spins < 100_000 {
                        libc::sched_yield();
                        spins += 1;
                    }
                    let line = read_rest(fd);
                    let ok = libc::getppid() != original
                        && field(&line, 4) == libc::getppid().to_string();
                    libc::write(verdict[1], if ok { b"k" } else { b"f" }.as_ptr().cast(), 1);
                    libc::_exit(0);
                }
                let mut byte = 0_u8;
                libc::read(ready[0], (&raw mut byte).cast(), 1);
                libc::_exit(0);
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(middle, &mut status, 0), middle);
            let mut byte = 0_u8;
            assert_eq!(libc::read(verdict[0], (&raw mut byte).cast(), 1), 1);
            assert_eq!(byte, b'k', "the reparented reader saw a stale parent");
        }
    });
}

/// A forked child that reads its parent's descriptor, opened before the
/// fork, sees the parent's own record, its parent included: the run's root
/// process's parent is the getppid answer Detcore recorded when the root
/// opened its stat.
#[test]
fn an_inherited_stat_descriptor_shows_the_openers_parent() {
    under_detcore(|| {
        // SAFETY: plain identity calls, a pipe and a fork this guest owns.
        unsafe {
            let (pid, ppid) = (libc::getpid(), libc::getppid());
            let fd = open_stat(&format!("/proc/{pid}/stat"));
            let mut verdict = [0; 2];
            assert_eq!(libc::pipe(verdict.as_mut_ptr()), 0);
            let child = libc::fork();
            assert!(child >= 0);
            if child == 0 {
                let line = read_rest(fd);
                let ok = field(&line, 1) == pid.to_string() && field(&line, 4) == ppid.to_string();
                libc::write(verdict[1], if ok { b"k" } else { b"f" }.as_ptr().cast(), 1);
                libc::_exit(0);
            }
            let mut byte = 0_u8;
            assert_eq!(libc::read(verdict[0], (&raw mut byte).cast(), 1), 1);
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert_eq!(byte, b'k', "the child saw another pid or parent");
        }
    });
}

/// What a mode change, `syncfs`, a mapping and `fstat` answer for a
/// descriptor of the caller's own `stat`, as a tuple of errnos and values.
fn backing_answers() -> (i32, i32, i32, i32, u32, i64) {
    let fd = open_stat("/proc/self/stat");
    let errno = |result: i64| {
        if result < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap()
        } else {
            0
        }
    };
    let empty = std::ffi::CString::new("").unwrap();
    // SAFETY: calls on a descriptor this guest opened; a failed mapping maps
    // nothing, and a successful one is unmapped.
    unsafe {
        let fchmod = errno(libc::fchmod(fd, 0o400) as i64);
        let fchmodat2 = errno(libc::syscall(
            452, // fchmodat2
            fd,
            empty.as_ptr(),
            0o400,
            libc::AT_EMPTY_PATH,
        ));
        let syncfs = errno(libc::syncfs(fd) as i64);
        let mapped = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd,
            0,
        );
        let mmap = if mapped == libc::MAP_FAILED {
            std::io::Error::last_os_error().raw_os_error().unwrap()
        } else {
            libc::munmap(mapped, 4096);
            0
        };
        let mut stat: libc::stat = std::mem::zeroed();
        assert_eq!(libc::fstat(fd, &mut stat), 0);
        libc::close(fd);
        (
            fchmod,
            fchmodat2,
            syncfs,
            mmap,
            stat.st_mode & 0o7777,
            stat.st_size,
        )
    }
}

/// The rendered file answers a mode change, `syncfs`, a private mapping and
/// `fstat` as the kernel's own file does natively.
#[test]
fn a_rendered_stat_answers_mode_changes_syncfs_and_mappings_as_the_kernel() {
    let native = backing_answers();
    under_detcore(move || {
        assert_eq!(backing_answers(), native);
    });
}

/// A seek to a position inside the record renders it at the seek, as
/// Linux's `seq_lseek` `traverse`s there: a thread started between the seek
/// and the read does not reach the bytes the read returns.
#[test]
fn a_seek_renders_the_record_at_the_seek() {
    natively_and_under_detcore(|| {
        let before = own_thread_count();
        let fd = open_stat("/proc/self/stat");
        seek(fd, 3);
        let (keep, thread) = a_waiting_thread();
        let tail = read_rest(fd);
        assert_eq!(tail_field(&tail, 20), before, "{tail:?}");
        assert_ne!(
            own_thread_count(),
            before,
            "a new rendering counts the thread"
        );
        drop(keep);
        thread.join().unwrap();
        // SAFETY: a descriptor this guest opened.
        unsafe { libc::close(fd) };
    });
}

/// A read whose copy faults has still traversed: the record stays buffered
/// where it started, and a retry there reads it, not a new rendering, as
/// Linux's `seq_read_iter` sets the sequence before it copies.
#[test]
fn a_read_that_faults_keeps_the_record_its_traversal_rendered() {
    natively_and_under_detcore(|| {
        let before = own_thread_count();
        let fd = open_stat("/proc/self/stat");
        // SAFETY: an anonymous page nobody may write, unmapped below.
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(page, libc::MAP_FAILED);
        // SAFETY: the kernel checks the buffer; nothing is written.
        let faulted = unsafe { libc::pread(fd, page, 64, 3) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!((faulted, errno), (-1, Some(libc::EFAULT)));
        let (keep, thread) = a_waiting_thread();
        let tail = pread_all(fd, 3);
        assert_eq!(tail_field(&tail, 20), before, "{tail:?}");
        drop(keep);
        thread.join().unwrap();
        // SAFETY: the page mapped and the descriptor opened above.
        unsafe {
            libc::munmap(page, 4096);
            libc::close(fd);
        }
    });
}

/// A descriptor sent with `SCM_RIGHTS` to another process is the same open
/// file: after the sender closes its every alias and starts a thread, the
/// receiver's read continues the record the sender's first read rendered
/// (one thread), as Linux's one `seq_file` per open file does.
#[test]
fn a_stat_descriptor_sent_to_another_process_continues_the_senders_record() {
    natively_and_under_detcore(|| {
        let pair = socket_pair();
        let (ack, go) = (pipe(), pipe());
        let before = own_thread_count();
        // SAFETY: a plain identity call.
        let pid = unsafe { libc::getpid() };
        let receiver = fork_running(move || {
            let received = recv_fd(pair[1]);
            // The receiver's first call naming the descriptor, while the
            // sender still has its own: Detcore joins the description here
            // (`Detcore::adopt_received_placeholders`).
            // SAFETY: a stat buffer this guest owns.
            unsafe {
                let mut stat: libc::stat = std::mem::zeroed();
                assert_eq!(libc::fstat(received, &mut stat), 0);
            }
            write_all(ack[1], b"r");
            wait_byte(go[0]);
            let tail = read_rest(received);
            assert_eq!(tail_field(&tail, 20), before, "{tail:?}");
        });
        let fd = open_stat(&format!("/proc/{pid}/stat"));
        assert_eq!(read_some(fd, 4).len(), 4);
        send_fd(pair[0], fd);
        wait_byte(ack[0]);
        // SAFETY: the sender's only alias.
        unsafe { libc::close(fd) };
        let (keep, thread) = a_waiting_thread();
        write_all(go[1], b"g");
        reap_passing(receiver);
        drop(keep);
        thread.join().unwrap();
    });
}

/// DECLARED DIFFERENCE. A descriptor that reaches another process with
/// `SCM_RIGHTS` joins Detcore's state of its open file at the receiver's
/// first call naming it (`Detcore::adopt_received_placeholders`). If the
/// sender has closed its every alias before then (the descriptor was still
/// queued, or received and not yet used), no descriptor Detcore models
/// refers to the open file, so its state (the buffered record) is gone, as
/// for every rendered file (`fd::OpenFileHandle`): the receiver rebuilds it
/// from its placeholder at the shared cursor and renders the record again
/// (two threads), where Linux, whose queued file keeps its `seq_file`,
/// continues the sender's record (one thread).
#[test]
fn a_queued_stat_descriptor_whose_sender_closed_it_renders_again_under_detcore() {
    let check = |buffered: bool| {
        move || {
            let pair = socket_pair();
            let go = pipe();
            let before: i32 = own_thread_count().parse().unwrap();
            // SAFETY: a plain identity call.
            let pid = unsafe { libc::getpid() };
            let receiver = fork_running(move || {
                wait_byte(go[0]);
                let received = recv_fd(pair[1]);
                let tail = read_rest(received);
                let expected = if buffered { before } else { before + 1 };
                assert_eq!(tail_field(&tail, 20), expected.to_string(), "{tail:?}");
            });
            let fd = open_stat(&format!("/proc/{pid}/stat"));
            assert_eq!(read_some(fd, 4).len(), 4);
            send_fd(pair[0], fd);
            // SAFETY: the sender's only alias, while the message is queued.
            unsafe { libc::close(fd) };
            let (keep, thread) = a_waiting_thread();
            write_all(go[1], b"g");
            reap_passing(receiver);
            drop(keep);
            thread.join().unwrap();
        }
    };
    natively(check(true));
    under_detcore(check(false));
}

/// A forked child reading its parent's `stat` through an inherited
/// descriptor sees the name the parent gave itself through its kernel `comm`
/// file: the tool reads the described task's own `comm`, not the reader's.
#[test]
fn an_inherited_stat_descriptor_shows_the_name_its_task_wrote_to_its_comm_file() {
    natively_and_under_detcore(|| {
        let comm = std::ffi::CString::new("/proc/self/comm").unwrap();
        // SAFETY: a NUL-terminated path, and a buffer this guest owns.
        unsafe {
            let fd = libc::open(comm.as_ptr(), libc::O_WRONLY);
            assert!(fd >= 0, "open comm: {}", std::io::Error::last_os_error());
            write_all(fd, b"renamed");
            libc::close(fd);
        }
        // SAFETY: a plain identity call.
        let pid = unsafe { libc::getpid() };
        let fd = open_stat(&format!("/proc/{pid}/stat"));
        let child = fork_running(move || {
            let line = read_rest(fd);
            assert_eq!(field(&line, 1), pid.to_string(), "{line:?}");
            assert_eq!(field(&line, 2), "(renamed)", "{line:?}");
        });
        reap_passing(child);
    });
}

/// A process this one creates with `CLONE_PARENT` shares this one's parent,
/// which is outside the guest's process tree: its `stat`, read by its own
/// child through an inherited descriptor, shows the parent its `getppid`
/// answers, not 0.
#[test]
fn a_clone_parent_childs_stat_read_by_its_child_shows_its_outside_parent() {
    natively_and_under_detcore(|| {
        let verdict = pipe();
        // SAFETY: a raw clone with no new stack forks this process; the
        // child makes only these calls and exits.
        let sibling = unsafe {
            libc::syscall(
                libc::SYS_clone,
                (libc::CLONE_PARENT | libc::SIGCHLD) as libc::c_ulong,
                0,
                0,
                0,
                0,
            )
        };
        assert!(sibling >= 0, "clone: {}", std::io::Error::last_os_error());
        if sibling == 0 {
            let passed = std::panic::catch_unwind(|| {
                // SAFETY: plain identity calls.
                let (pid, ppid) = unsafe { (libc::getpid(), libc::getppid()) };
                let fd = open_stat(&format!("/proc/{pid}/stat"));
                let child = fork_running(move || {
                    let line = read_rest(fd);
                    assert_eq!(field(&line, 4), ppid.to_string(), "{line:?}");
                });
                reap_passing(child);
            })
            .is_ok();
            write_all(verdict[1], if passed { b"k" } else { b"f" });
            // SAFETY: the cloned child ends here.
            unsafe { libc::_exit(0) };
        }
        assert_eq!(
            wait_byte(verdict[0]),
            b'k',
            "the sibling's child saw another parent"
        );
    });
}

/// `rsslim` (field 25) is the described process's soft `RLIMIT_RSS`, not
/// the reader's: a child forked before its parent tried to lower its limit
/// reads, through an inherited descriptor, the limit the parent's
/// `getrlimit` then answers. (Linux accepts the lowering; Detcore refuses any
/// change to `RLIMIT_RSS` with EPERM, so there both are `RLIM_INFINITY`.)
#[test]
fn an_inherited_stat_descriptor_shows_its_processes_rss_limit() {
    natively_and_under_detcore(|| {
        let fd = open_stat("/proc/self/stat");
        let go = pipe();
        let child = fork_running(move || {
            let mut bytes = [0_u8; 8];
            // SAFETY: a buffer this guest owns.
            assert_eq!(
                unsafe { libc::read(go[0], bytes.as_mut_ptr().cast(), 8) },
                8
            );
            let line = read_rest(fd);
            assert_eq!(
                field(&line, 25),
                u64::from_le_bytes(bytes).to_string(),
                "{line:?}"
            );
        });
        // SAFETY: rlimit structures this guest owns.
        let limit = unsafe {
            let mut limit: libc::rlimit = std::mem::zeroed();
            assert_eq!(libc::getrlimit(libc::RLIMIT_RSS, &mut limit), 0);
            let lowered = libc::rlimit {
                rlim_cur: 64 << 20,
                rlim_max: limit.rlim_max,
            };
            let _ = libc::setrlimit(libc::RLIMIT_RSS, &lowered);
            assert_eq!(libc::getrlimit(libc::RLIMIT_RSS, &mut limit), 0);
            limit.rlim_cur
        };
        write_all(go[1], &limit.to_le_bytes());
        reap_passing(child);
    });
}

/// Marks a run of this test binary that `syncfs_after_exec` started, with
/// the descriptor to `syncfs`.
const SYNCFS_FD_ENV: &str = "HERMIT_PROC_STAT_SYNCFS_FD";

/// The image `syncfs_after_exec` executes: this binary on this test alone,
/// which exits with `syncfs`'s errno (0 for success) on the descriptor the
/// environment names, and does nothing in any other run.
#[test]
fn a_syncfs_run_after_an_exec() {
    if let Some(fd) = std::env::var_os(SYNCFS_FD_ENV) {
        let fd: RawFd = fd.to_str().unwrap().parse().unwrap();
        // SAFETY: a descriptor this image inherited.
        let result = unsafe { libc::syncfs(fd) };
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap();
        std::process::exit(if result == 0 { 0 } else { errno });
    }
}

/// `syncfs`'s errno (0 for success) on `fd` in a forked child that first
/// executes this binary on [`a_syncfs_run_after_an_exec`].
fn syncfs_after_exec(fd: RawFd) -> i32 {
    let exe = std::ffi::CString::new(
        std::env::current_exe()
            .unwrap()
            .into_os_string()
            .into_encoded_bytes(),
    )
    .unwrap();
    let test = c"proc_stat_lifecycle::a_syncfs_run_after_an_exec";
    let env = std::ffi::CString::new(format!("{SYNCFS_FD_ENV}={fd}")).unwrap();
    let null = c"/dev/null";
    // SAFETY: the child redirects its output and executes; on failure it
    // exits 127.
    unsafe {
        let child = libc::fork();
        assert!(child >= 0);
        if child == 0 {
            let quiet = libc::open(null.as_ptr(), libc::O_WRONLY);
            libc::dup2(quiet, 1);
            libc::dup2(quiet, 2);
            let argv = [
                exe.as_ptr(),
                test.as_ptr(),
                c"--exact".as_ptr(),
                c"--test-threads=1".as_ptr(),
                std::ptr::null(),
            ];
            let envp = [env.as_ptr(), std::ptr::null()];
            libc::execve(exe.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
        let mut status = 0;
        assert_eq!(libc::waitpid(child, &mut status, 0), child);
        assert!(libc::WIFEXITED(status), "status {status:#x}");
        libc::WEXITSTATUS(status)
    }
}

/// `syncfs`'s errno (0 for success) on `fd` in a forked child.
fn syncfs_in_a_child(fd: RawFd) -> i32 {
    // SAFETY: the child makes one call and exits with its errno.
    unsafe {
        let child = libc::fork();
        assert!(child >= 0);
        if child == 0 {
            let result = libc::syncfs(fd);
            let errno = *libc::__errno_location();
            libc::_exit(if result == 0 { 0 } else { errno });
        }
        let mut status = 0;
        assert_eq!(libc::waitpid(child, &mut status, 0), child);
        libc::WEXITSTATUS(status)
    }
}

fn errno_of(result: i64) -> i32 {
    if result < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap()
    } else {
        0
    }
}

/// What a descriptor of the caller's own `stat` answers once it has moved:
/// a mode change through an `O_PATH` descriptor; a reopen through its
/// descriptor link, and whether that names the same inode; and `syncfs`
/// after it is sent with `SCM_RIGHTS`, after the sender's number is given to
/// another file, through a `dup`, in a forked child and after an exec.
fn transfer_answers() -> Vec<(&'static str, i64)> {
    let mut answers = Vec::new();
    let path = std::ffi::CString::new("/proc/self/stat").unwrap();
    let empty = std::ffi::CString::new("").unwrap();
    // SAFETY: calls on paths and descriptors this guest owns.
    unsafe {
        let o_path = libc::open(path.as_ptr(), libc::O_PATH);
        assert!(o_path >= 0);
        answers.push((
            "O_PATH fchmodat2",
            i64::from(errno_of(libc::syscall(
                452, // fchmodat2
                o_path,
                empty.as_ptr(),
                0o400,
                libc::AT_EMPTY_PATH,
            ))),
        ));
        libc::close(o_path);

        let fd = open_stat("/proc/self/stat");
        let link = std::ffi::CString::new(format!("/proc/self/fd/{fd}")).unwrap();
        let reopened = libc::open(link.as_ptr(), libc::O_RDONLY);
        answers.push(("reopen errno", i64::from(errno_of(i64::from(reopened)))));
        let mut by_fd: libc::stat = std::mem::zeroed();
        let mut by_link: libc::stat = std::mem::zeroed();
        let mut by_path: libc::stat = std::mem::zeroed();
        assert_eq!(libc::fstat(fd, &mut by_fd), 0);
        assert_eq!(libc::fstat(reopened, &mut by_link), 0);
        assert_eq!(libc::stat(path.as_ptr(), &mut by_path), 0);
        answers.push((
            "reopen is the same inode",
            i64::from(by_fd.st_ino == by_link.st_ino && by_fd.st_dev == by_link.st_dev),
        ));
        answers.push((
            "the path is the same inode",
            i64::from(by_fd.st_ino == by_path.st_ino && by_fd.st_dev == by_path.st_dev),
        ));
        answers.push((
            "the reopened file's fields",
            fields(&read_rest(reopened)).len() as i64,
        ));
        libc::close(reopened);

        let pair = socket_pair();
        send_fd(pair[0], fd);
        let received = recv_fd(pair[1]);
        answers.push((
            "syncfs of the received descriptor",
            i64::from(errno_of(i64::from(libc::syncfs(received)))),
        ));
        libc::close(fd);
        let null = c"/dev/null";
        let reused = libc::open(null.as_ptr(), libc::O_RDONLY);
        answers.push((
            "syncfs after the source number is reused",
            i64::from(errno_of(i64::from(libc::syncfs(received)))),
        ));
        let duplicate = libc::dup(received);
        answers.push((
            "syncfs of a dup",
            i64::from(errno_of(i64::from(libc::syncfs(duplicate)))),
        ));
        answers.push((
            "syncfs in a forked child",
            i64::from(syncfs_in_a_child(duplicate)),
        ));
        answers.push((
            "syncfs after an exec",
            i64::from(syncfs_after_exec(duplicate)),
        ));
        for fd in [reused, duplicate, received, pair[0], pair[1]] {
            libc::close(fd);
        }
    }
    answers
}

/// A descriptor of the caller's own `stat` answers a mode change, a reopen
/// through its descriptor link and `syncfs` after it moves (sent, its
/// source number reused, duplicated, inherited by a fork and an exec) as
/// the kernel's own file does natively.
#[test]
fn a_moved_stat_descriptor_answers_as_the_kernels_file() {
    let native = natively_answering(|| format!("{:?}", transfer_answers()));
    assert!(native.contains("syncfs after an exec"), "{native}");
    println!("native answers: {native}");
    under_detcore(move || assert_eq!(format!("{:?}", transfer_answers()), native));
}

/// DECLARED DIFFERENCE. A rendered `stat` is no `sendfile` source under
/// Detcore: the shared refusal contract for every procfs input
/// (`handle_sendfile`, ENOSYS, so glibc's callers fall back to `read`).
/// Linux refuses it too, with EINVAL: `proc_single_file_operations` has no
/// `splice_read` (fs/proc/base.c:823-828 in 6.17).
#[test]
fn a_rendered_stat_is_refused_as_a_sendfile_source() {
    let check = |expected: i32| {
        move || {
            let fd = open_stat("/proc/self/stat");
            let out = pipe();
            // SAFETY: descriptors this guest owns.
            let sent = unsafe { libc::sendfile(out[1], fd, std::ptr::null_mut(), 64) };
            let errno = std::io::Error::last_os_error().raw_os_error();
            assert_eq!((sent, errno), (-1, Some(expected)));
        }
    };
    natively(check(libc::EINVAL));
    under_detcore(check(libc::ENOSYS));
}

/// A rendering that fails resets the sequence and keeps the file cursor, as
/// Linux's `seq_read_iter` does after a failed `traverse`: once a thread's
/// task is gone, a pread off the sequence is ESRCH, and a later seek into
/// the record traverses again, is ESRCH too, and leaves the cursor at 0
/// (`seq_lseek`).
#[test]
fn a_failed_rendering_resets_the_sequence_and_a_seek_fails_again() {
    natively_and_under_detcore(|| {
        let (to_worker, worker_waits) = std::sync::mpsc::channel::<()>();
        let (tid_sender, tid) = std::sync::mpsc::channel::<libc::pid_t>();
        let worker = std::thread::spawn(move || {
            // SAFETY: a plain identity call.
            tid_sender.send(unsafe { libc::gettid() }).unwrap();
            let _ = worker_waits.recv();
        });
        let tid = tid.recv().unwrap();
        let fd = open_stat(&format!("/proc/self/task/{tid}/stat"));
        assert_eq!(read_some(fd, 5).len(), 5);
        drop(to_worker);
        worker.join().unwrap();
        // Natively the joined thread may linger for a moment before the
        // kernel releases it; under Detcore it is gone at once.
        let mut buffer = [0_u8; 64];
        let mut failed = None;
        for _ in 0..100_000 {
            // SAFETY: a buffer this guest owns.
            let read = unsafe { libc::pread(fd, buffer.as_mut_ptr().cast(), 64, 3) };
            if read < 0 {
                failed = std::io::Error::last_os_error().raw_os_error();
                break;
            }
            // SAFETY: a plain yield.
            unsafe { libc::sched_yield() };
        }
        assert_eq!(failed, Some(libc::ESRCH));
        let seek = |offset: i64, whence: libc::c_int| {
            // SAFETY: a seek on a descriptor this guest opened.
            let result = unsafe { libc::lseek(fd, offset, whence) };
            (
                result,
                (result < 0)
                    .then(|| std::io::Error::last_os_error().raw_os_error())
                    .flatten(),
            )
        };
        // The failed read kept the cursor (5) and reset the sequence (0), so
        // even a seek to the cursor traverses, fails, and resets the cursor.
        assert_eq!(seek(0, libc::SEEK_CUR), (-1, Some(libc::ESRCH)));
        assert_eq!(seek(0, libc::SEEK_CUR), (0, None), "seq_lseek reset it");
        // A seek into the record traverses, fails again, and resets.
        assert_eq!(seek(3, libc::SEEK_SET), (-1, Some(libc::ESRCH)));
        assert_eq!(seek(0, libc::SEEK_CUR), (0, None));
        // SAFETY: a descriptor this guest opened.
        unsafe { libc::close(fd) };
    });
}

/// A read that Linux refuses before it reaches the seq_file (a destination
/// beyond the user address range, EFAULT; a position plus count past
/// LLONG_MAX, EINVAL) leaves the buffered record alone: the original read,
/// continued after a thread is started, still returns the record its first
/// read rendered.
#[test]
fn a_refused_read_leaves_the_buffered_record() {
    natively_and_under_detcore(|| {
        let before = own_thread_count();
        let fd = open_stat("/proc/self/stat");
        assert_eq!(read_some(fd, 4).len(), 4);
        let kernel_half = 0xffff_8000_0000_0000_u64 as *mut libc::c_void;
        let mut byte = 0_u8;
        // SAFETY: the kernel checks both calls' arguments; nothing is written.
        let (faulted, overflowed) = unsafe {
            let faulted = libc::pread(fd, kernel_half, 16, 9);
            let faulted_errno = std::io::Error::last_os_error().raw_os_error();
            let overflowed = libc::pread(fd, (&raw mut byte).cast(), 1, i64::MAX);
            let overflowed_errno = std::io::Error::last_os_error().raw_os_error();
            ((faulted, faulted_errno), (overflowed, overflowed_errno))
        };
        assert_eq!(faulted, (-1, Some(libc::EFAULT)));
        assert_eq!(overflowed, (-1, Some(libc::EINVAL)));
        let (keep, thread) = a_waiting_thread();
        let tail = read_rest(fd);
        assert_eq!(tail_field(&tail, 20), before, "{tail:?}");
        drop(keep);
        thread.join().unwrap();
        // SAFETY: a descriptor this guest opened.
        unsafe { libc::close(fd) };
    });
}

/// Write `name` to this process's own kernel `comm` file.
fn rename_through_comm_file(name: &[u8]) {
    let comm = std::ffi::CString::new("/proc/self/comm").unwrap();
    // SAFETY: a NUL-terminated path, and a buffer this guest owns.
    unsafe {
        let fd = libc::open(comm.as_ptr(), libc::O_WRONLY);
        assert!(fd >= 0, "open comm: {}", std::io::Error::last_os_error());
        write_all(fd, name);
        libc::close(fd);
    }
}

/// Run `child` in a child cloned with `flags` (a new user namespace, where
/// it may `chroot`, and optionally a new PID namespace), which reports
/// whether `child` returned through a pipe; require that it did.
fn in_a_cloned_child(flags: libc::c_int, child: impl FnOnce()) {
    let verdict = pipe();
    // SAFETY: a raw clone with no new stack forks this process; the child
    // runs `child`, reports and exits.
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (flags | libc::SIGCHLD) as libc::c_ulong,
            0,
            0,
            0,
            0,
        )
    };
    assert!(pid >= 0, "clone: {}", std::io::Error::last_os_error());
    if pid == 0 {
        let passed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)).is_ok();
        write_all(verdict[1], if passed { b"k" } else { b"f" });
        // SAFETY: the cloned child ends here.
        unsafe { libc::_exit(0) };
    }
    assert_eq!(
        wait_byte(verdict[0]),
        b'k',
        "the cloned child's checks failed"
    );
    let mut status = 0;
    // SAFETY: waiting for the child cloned above.
    assert_eq!(
        unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) },
        pid as libc::pid_t
    );
}

/// A stat descriptor opened before a `chroot("/")` and read after it shows
/// the name its task gave itself through its kernel `comm` file: the
/// genuine procfs is still reachable, and the tool reads the described
/// task's own name.
#[test]
fn a_stat_read_after_chroot_to_the_root_shows_the_kernels_name() {
    natively_and_under_detcore(|| {
        in_a_cloned_child(libc::CLONE_NEWUSER, || {
            let fd = open_stat("/proc/self/stat");
            rename_through_comm_file(b"renamed");
            let root = std::ffi::CString::new("/").unwrap();
            // SAFETY: a NUL-terminated path.
            assert_eq!(unsafe { libc::chroot(root.as_ptr()) }, 0, "chroot");
            let line = read_rest(fd);
            assert_eq!(field(&line, 2), "(renamed)", "{line:?}");
        });
    });
}

/// A stat descriptor opened before a `chroot` into a tree whose /proc holds
/// ordinary files, a forged `comm` among them, still names its task, as
/// Linux's open file does. Under a tracer the tool reads its own /proc,
/// which the guest's chroot does not change, so the kernel's name shows.
/// Where the tool shares the guest's root (DBT, SaBRe, LiteInst), the read
/// after the guest's chroot is the named refusal, whatever the tree holds
/// (the unit test `a_comm_is_not_read_through_a_changed_root_the_tool_shares`).
#[test]
fn a_stat_read_after_chroot_into_a_forged_proc_shows_the_kernels_name() {
    natively_and_under_detcore(|| {
        // SAFETY: a plain identity call.
        let pid = unsafe { libc::getpid() };
        let tree = std::env::temp_dir().join(format!(
            "proc-stat-forged-{pid}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let task = tree.join(format!("proc/{pid}/task/{pid}"));
        std::fs::create_dir_all(&task).unwrap();
        let comms = [tree.join(format!("proc/{pid}/comm")), task.join("comm")];
        for comm in &comms {
            std::fs::write(comm, b"forged\n").unwrap();
        }
        let root = tree.clone();
        in_a_cloned_child(libc::CLONE_NEWUSER, move || {
            let fd = open_stat(&format!("/proc/{pid}/stat"));
            let path = std::ffi::CString::new(root.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a NUL-terminated path.
            assert_eq!(unsafe { libc::chroot(path.as_ptr()) }, 0, "chroot");
            let line = read_rest(fd);
            assert_ne!(field(&line, 2), "(forged)", "{line:?}");
            assert_eq!(field(&line, 1), pid.to_string(), "{line:?}");
        });
        // The tree this run made, file by file.
        for comm in &comms {
            std::fs::remove_file(comm).unwrap();
        }
        for directory in [
            task.clone(),
            tree.join(format!("proc/{pid}/task")),
            tree.join(format!("proc/{pid}")),
            tree.join("proc"),
            tree.clone(),
        ] {
            std::fs::remove_dir(directory).unwrap();
        }
    });
}

/// A child in a new PID namespace reads its parent's stat through an
/// inherited descriptor: the parent's number, its kernel name and its own
/// getppid answer, as the procfs the parent opened it in numbers them.
#[test]
fn a_pid_namespace_child_reads_its_parents_inherited_stat() {
    natively_and_under_detcore(|| {
        // SAFETY: plain identity calls.
        let (pid, ppid) = unsafe { (libc::getpid(), libc::getppid()) };
        let fd = open_stat(&format!("/proc/{pid}/stat"));
        rename_through_comm_file(b"ns-parent");
        in_a_cloned_child(libc::CLONE_NEWUSER | libc::CLONE_NEWPID, move || {
            let line = read_rest(fd);
            assert_eq!(field(&line, 1), pid.to_string(), "{line:?}");
            assert_eq!(field(&line, 2), "(ns-parent)", "{line:?}");
            assert_eq!(field(&line, 4), ppid.to_string(), "{line:?}");
        });
    });
}

/// Under a tracer every spelling of the caller's own `comm` stays the
/// kernel's, as before package K slice A+, which renders a `comm` from
/// Detcore's record only where that record is the name's source (KVM): a
/// write through any spelling renames the task, a `pread` at 0 of the same
/// descriptor reads the new name back, and every spelling, `PR_GET_NAME`
/// and the rendered `stat` show it; the file's inode is the kernel's, not one
/// of Detcore's per-process numbers (bit 63 set).
#[test]
fn every_comm_spelling_stays_the_kernels_under_a_tracer() {
    natively_and_under_detcore(|| {
        // SAFETY: plain identity calls.
        let (pid, tid) = unsafe { (libc::getpid(), libc::syscall(libc::SYS_gettid)) };
        let spellings = [
            "/proc/self/comm".to_owned(),
            "/proc/thread-self/comm".to_owned(),
            format!("/proc/{pid}/comm"),
            format!("/proc/self/task/{tid}/comm"),
            format!("/proc/{pid}/task/{tid}/comm"),
        ];
        for (index, spelling) in spellings.iter().enumerate() {
            let name = format!("kernel-name-{index}");
            let path = std::ffi::CString::new(spelling.as_str()).unwrap();
            // SAFETY: a NUL-terminated path.
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
            assert!(
                fd >= 0,
                "open {spelling} for writing: {}",
                std::io::Error::last_os_error()
            );
            write_all(fd, name.as_bytes());
            let line = format!("{name}\n").into_bytes();
            assert_eq!(pread_all(fd, 0), line, "{spelling}");
            // SAFETY: a stat buffer this guest owns.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
            assert_eq!(stat.st_ino >> 63, 0, "{spelling}: {:#x}", stat.st_ino);
            // SAFETY: the descriptor opened above.
            unsafe { libc::close(fd) };
            for other in &spellings {
                let fd = open_stat(other);
                assert_eq!(read_rest(fd), line, "{other} after {spelling}");
                // SAFETY: the descriptor opened just above.
                unsafe { libc::close(fd) };
            }
            let mut named = [0_u8; 16];
            // SAFETY: PR_GET_NAME writes 16 bytes into a buffer this guest
            // owns.
            assert_eq!(
                unsafe { libc::prctl(libc::PR_GET_NAME, named.as_mut_ptr()) },
                0
            );
            assert_eq!(&named[..name.len()], name.as_bytes(), "{spelling}");
            let stat = open_stat("/proc/self/stat");
            assert_eq!(field(&read_rest(stat), 2), format!("({name})"));
            // SAFETY: the descriptor opened just above.
            unsafe { libc::close(stat) };
        }
    });
}

/// A process's `stat` and `cgroup` (`ONE()` entries,
/// `proc_single_file_operations`) and a task's `comm`
/// (`proc_pid_set_comm_operations`) have `.read = seq_read` and no
/// `.read_iter`, so a vectored read goes through `do_loop_readv_writev`:
/// `preadv2` takes flags 0 and `RWF_HIPRI` and refuses `RWF_DSYNC`,
/// `RWF_SYNC` and `RWF_APPEND` with EOPNOTSUPP, natively and under Detcore,
/// whose dispatch serves the rendered `stat` and `cgroup` here (the `comm`
/// files stay the kernel's under a tracer).
#[test]
fn vectored_reads_of_stat_cgroup_and_comm_take_only_rwf_hipri() {
    natively_and_under_detcore(|| {
        for path in [
            "/proc/self/stat",
            "/proc/thread-self/stat",
            "/proc/self/cgroup",
            "/proc/self/comm",
            "/proc/thread-self/comm",
        ] {
            let fd = open_stat(path);
            for (flags, refused) in [
                (0, false),
                (libc::RWF_HIPRI, false),
                (libc::RWF_DSYNC, true),
                (libc::RWF_SYNC, true),
                (libc::RWF_APPEND, true),
            ] {
                let mut buffer = [0_u8; 1024];
                let vector = libc::iovec {
                    iov_base: buffer.as_mut_ptr().cast(),
                    iov_len: buffer.len(),
                };
                // SAFETY: one vector over a buffer this guest owns, at
                // position 0 (pos_l, pos_h).
                let got = unsafe { libc::syscall(libc::SYS_preadv2, fd, &vector, 1, 0, 0, flags) };
                let errno = std::io::Error::last_os_error().raw_os_error();
                if refused {
                    assert_eq!(
                        (got, errno),
                        (-1, Some(libc::EOPNOTSUPP)),
                        "{path} flags {flags:#x}"
                    );
                } else {
                    assert!(got > 0, "{path} flags {flags:#x}: {got} {errno:?}");
                }
            }
            // SAFETY: the descriptor opened above.
            unsafe { libc::close(fd) };
        }
    });
}
