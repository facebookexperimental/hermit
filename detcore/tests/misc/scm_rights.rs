/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors received through `SCM_RIGHTS` control messages.
//!
//! Every message goes through a `socketpair`, whose sends Detcore queues with
//! the open file each descriptor names (`detcore::syscalls::scm_rights`), so
//! a received descriptor whose sender still holds it joins that open file as
//! an alias: it keeps that file's blocking state and an eventfd's kind, and
//! `MSG_CMSG_CLOEXEC` marks it alone. Most tests send a descriptor the process
//! keeps; one receives a pipe end from a child that keeps it, which no
//! descriptor of the receiving table names, and one follows the queue through
//! a peek, a truncated receive and a discarded message. Others follow it
//! through calls that take nothing (a peek through `recv`, a read of no
//! bytes, a receive that fails with `EFAULT` before it reads the socket, a
//! refused `sendfile`), calls that take a message Detcore cannot match or
//! that a datagram socketpair's queue, which holds every message, names (a
//! data-only read, a discard in a batch, a receive that fails with `EFAULT`
//! after it read the socket, a `preadv2`), a receipt that names a message
//! Linux cannot have delivered yet, and a peek past the first message
//! (`SO_PEEK_OFF`); and the port of a socket only a queued message holds,
//! freed with that message by a read, a purge or the exit of the receiving
//! socket's holder. Others receive from a child a random device and a
//! rendered procfs file. Descriptors from another process in general are
//! covered by `tests/c/scm_rights_received_descriptors.c`.
//!
//! Each run starts in a fresh PID namespace (`run_deterministically`).

use std::io;
use std::mem::size_of;
use std::mem::size_of_val;
use std::ptr;

use detcore::Config;
use detcore_testutils::det_test_fn_with_config_repetitions;
use libc::c_int;
use reverie::ExitStatus;

const RUNS: u64 = 3;

/// `recvmsg` takes a different Detcore path with and without virtual time.
fn configs() -> [(&'static str, Config); 2] {
    [
        (
            "virtual time",
            Config {
                sequentialize_threads: true,
                virtualize_time: true,
                ..Default::default()
            },
        ),
        (
            "host time",
            Config {
                sequentialize_threads: true,
                virtualize_time: false,
                virtualize_metadata: false,
                ..Default::default()
            },
        ),
    ]
}

/// Runs `guest` `RUNS` times under each configuration and requires every
/// run to exit 0 with the same stdout, stderr and schedule
/// (`det_test_fn_with_config_repetitions`).
///
/// Each run starts in a fresh PID namespace, as a `hermit run` container
/// does, so its guest's thread ids repeat. Detcore names some of an open
/// file's state by a thread id: a random device's stream by the task that
/// opened it or, for one it received, by the receiving table's owner
/// (`RandomDeviceStream`). In the test process's own PID namespace those ids
/// are host PIDs, new in every run, and a random device received from a child
/// read different bytes each time.
fn run_deterministically(guest: fn()) {
    for (name, config) in configs() {
        det_test_fn_with_config_repetitions(RUNS, true, guest, config, |output, _state| {
            assert_eq!(
                output.status,
                ExitStatus::Exited(0),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Every guest prints what it found. An empty stdout would compare
            // equal in every run, whatever the guest read.
            assert!(
                !output.stdout.is_empty(),
                "{name}: the guest printed nothing"
            );
        });
    }
}

fn check(result: c_int, what: &str) -> c_int {
    assert!(result >= 0, "{what}: {}", io::Error::last_os_error());
    result
}

fn pipe() -> (c_int, c_int) {
    let mut fds = [0; 2];
    check(unsafe { libc::pipe(fds.as_mut_ptr()) }, "pipe");
    (fds[0], fds[1])
}

fn socketpair(ty: c_int) -> (c_int, c_int) {
    let mut fds = [0; 2];
    check(
        unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) },
        "socketpair",
    );
    (fds[0], fds[1])
}

fn write_all(fd: c_int, bytes: &[u8]) {
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(
        written,
        bytes.len() as isize,
        "write to {fd}: {}",
        io::Error::last_os_error()
    );
}

fn read_exact(fd: c_int, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    let mut filled = 0;
    while filled < len {
        let read = unsafe { libc::read(fd, bytes[filled..].as_mut_ptr().cast(), len - filled) };
        assert!(
            read > 0,
            "read from {fd}: {read} ({})",
            io::Error::last_os_error()
        );
        filled += read as usize;
    }
    bytes
}

fn fcntl(fd: c_int, command: c_int) -> c_int {
    check(unsafe { libc::fcntl(fd, command) }, "fcntl")
}

/// Sends one byte carrying `fds` in a single `SCM_RIGHTS` message. It
/// allocates nothing, so a child forked from the test process can call it.
fn send_fds(socket: c_int, fds: &[c_int]) {
    let mut payload = *b"f";
    assert_eq!(send_fds_with(socket, fds, &mut payload), 1);
}

/// Sends `payload` carrying `fds` in a single `SCM_RIGHTS` message and
/// returns how many of its bytes Linux took.
fn send_fds_with(socket: c_int, fds: &[c_int], payload: &mut [u8]) -> usize {
    send_fds_with_name(socket, fds, payload, None)
}

/// As [`send_fds`], to the socket `address` names (`msg_name`).
fn send_fds_to(socket: c_int, fds: &[c_int], address: &(libc::sockaddr_un, u32)) {
    let mut payload = *b"f";
    assert_eq!(
        send_fds_with_name(socket, fds, &mut payload, Some(address)),
        1
    );
}

/// As [`send_fds_with`], naming the destination `name` where given.
fn send_fds_with_name(
    socket: c_int,
    fds: &[c_int],
    payload: &mut [u8],
    name: Option<&(libc::sockaddr_un, u32)>,
) -> usize {
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let data_len = size_of_val(fds) as u32;
    // `u64` storage gives the buffer the alignment `cmsghdr` needs: room for
    // the header and 28 descriptors.
    let mut control = [0u64; 16];
    assert!(unsafe { libc::CMSG_SPACE(data_len) } as usize <= size_of_val(&control));
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(data_len) } as _;
    if let Some((address, length)) = name {
        message.msg_name = (&raw const *address).cast_mut().cast();
        message.msg_namelen = *length;
    }
    // SAFETY: the control buffer has room for one message of `data_len` bytes.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(data_len) as _;
        ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(header).cast(), fds.len());
    }
    let sent = unsafe { libc::sendmsg(socket, &message, 0) };
    assert!(sent > 0, "sendmsg: {}", io::Error::last_os_error());
    sent as usize
}

/// Descriptors in the `SCM_RIGHTS` messages the kernel wrote into `message`.
fn received_fds(message: &libc::msghdr) -> Vec<c_int> {
    let mut fds = Vec::new();
    // SAFETY: walks the control messages the kernel wrote into `message`.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data_len = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(header).cast::<c_int>();
                fds.extend(
                    (0..data_len / size_of::<c_int>()).map(|i| data.add(i).read_unaligned()),
                );
            }
            header = libc::CMSG_NXTHDR(message, header);
        }
    }
    fds
}

/// Receives one byte and returns the descriptors that came with it.
fn recv_fds(socket: c_int, flags: c_int) -> Vec<c_int> {
    recv_fds_into(socket, flags, size_of::<[u64; 16]>()).0
}

/// Receives one byte into a control buffer of `room` bytes (none for 0), and
/// returns the descriptors that came with it and whether Linux set
/// `MSG_CTRUNC`.
fn recv_fds_into(socket: c_int, flags: c_int, room: usize) -> (Vec<c_int>, bool) {
    let mut payload = [0u8];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = [0u64; 16];
    assert!(room <= size_of_val(&control));
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if room > 0 {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = room as _;
    }
    let received = unsafe { libc::recvmsg(socket, &mut message, flags) };
    assert_eq!(received, 1, "recvmsg: {}", io::Error::last_os_error());
    (
        received_fds(&message),
        message.msg_flags & libc::MSG_CTRUNC != 0,
    )
}

/// Whether `fd` is blocking, as its open file's status flags say.
fn blocking(fd: c_int) -> bool {
    fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK == 0
}

/// A pipe end sent within the process names the open file the sender holds:
/// it keeps that file's blocking state and reaches the same pipe.
fn received_pipe_end_shares_the_open_file() {
    let (read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[write_end]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_ne!(received, write_end);

    assert_eq!(
        fcntl(received, libc::F_GETFL) & libc::O_NONBLOCK,
        0,
        "the sender's pipe end is blocking"
    );
    write_all(received, b"through the received end");
    assert_eq!(read_exact(read_end, 24), b"through the received end");
    println!("descriptor {received} names the open file of {write_end}");
}

/// Detcore keeps pipes nonblocking in the kernel. A blocking read through a
/// received end must still wait for the writer rather than fail with EAGAIN.
/// The writer is another process, not a thread: each guest descends, by
/// fork, from the multithreaded test process, and a Rust thread started there
/// can wait forever on a lock of the standard library's that another thread
/// of the test process held at the fork. A forked writer takes no such lock.
fn blocking_read_on_a_received_pipe_waits_for_the_writer() {
    let (read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[read_end]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };

    let writer = check(unsafe { libc::fork() }, "fork");
    if writer == 0 {
        // Only the parent reads.
        unsafe {
            libc::close(read_end);
            libc::close(received);
        }
        for _ in 0..3 {
            unsafe { libc::sched_yield() };
        }
        write_all(write_end, b"late");
        unsafe { libc::_exit(0) };
    }
    // The writer holds the only write end: if it fails, the read ends at the
    // end of file and the test fails, instead of waiting forever.
    check(unsafe { libc::close(write_end) }, "close");
    assert_eq!(read_exact(received, 4), b"late");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(writer, &mut status, 0) }, writer);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    println!("blocking read on {received} returned the writer's bytes");
}

/// A random device from another process is Detcore's random stream, as one
/// this process opened is: the same bytes in every run.
fn random_device_from_a_child_reads_the_deterministic_stream() {
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let child = check(unsafe { libc::fork() }, "fork");
    if child == 0 {
        let urandom = check(
            unsafe { libc::open(c"/dev/urandom".as_ptr(), libc::O_RDONLY) },
            "open /dev/urandom",
        );
        send_fds(left, &[urandom]);
        unsafe { libc::_exit(0) };
    }

    let [urandom] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let bytes = read_exact(urandom, 16);
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    println!("received /dev/urandom read {bytes:02x?}");
}

/// Reads `fd` to its end.
fn read_to_end(fd: c_int) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 256];
    loop {
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        assert!(read >= 0, "read from {fd}: {}", io::Error::last_os_error());
        if read == 0 {
            return bytes;
        }
        bytes.extend_from_slice(&buffer[..read as usize]);
    }
}

/// A procfs file Detcore renders, opened by another process, reads what the
/// file reads (the child shares the parent's cgroup, so the parent's own open
/// reads the same bytes), not the empty placeholder that stands in for it.
fn rendered_procfs_file_from_a_child_reads_as_the_file() {
    let own = check(
        unsafe { libc::open(c"/proc/self/cgroup".as_ptr(), libc::O_RDONLY) },
        "open /proc/self/cgroup",
    );
    let expected = read_to_end(own);
    assert!(!expected.is_empty());
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let (go_read, go_write) = pipe();
    let child = check(unsafe { libc::fork() }, "fork");
    if child == 0 {
        let cgroup = check(
            unsafe { libc::open(c"/proc/self/cgroup".as_ptr(), libc::O_RDONLY) },
            "open /proc/self/cgroup",
        );
        send_fds(left, &[cgroup]);
        // Live until the parent has read: a reaped task's file reads ESRCH.
        read_exact(go_read, 1);
        unsafe { libc::_exit(0) };
    }

    let [cgroup] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let received = read_to_end(cgroup);
    write_all(go_write, b"g");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert_eq!(
        String::from_utf8_lossy(&received),
        String::from_utf8_lossy(&expected)
    );
    println!(
        "received /proc/<child>/cgroup reads {:?}",
        String::from_utf8_lossy(&received)
    );
}

/// `recvmmsg` registers the descriptors of every message it delivers.
fn recvmmsg_registers_every_message() {
    let (first_read, first_write) = pipe();
    let (second_read, second_write) = pipe();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    send_fds(left, &[first_write]);
    send_fds(left, &[second_write]);

    let mut payloads = [[0u8; 1]; 2];
    let mut iovs = payloads.each_mut().map(|payload| libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    });
    let mut controls = [[0u64; 16]; 2];
    let mut messages: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
    for ((message, iov), control) in messages.iter_mut().zip(&mut iovs).zip(&mut controls) {
        message.msg_hdr.msg_iov = iov;
        message.msg_hdr.msg_iovlen = 1;
        message.msg_hdr.msg_control = control.as_mut_ptr().cast();
        message.msg_hdr.msg_controllen = size_of_val(control) as _;
    }
    let delivered = unsafe { libc::recvmmsg(right, messages.as_mut_ptr(), 2, 0, ptr::null_mut()) };
    assert_eq!(delivered, 2, "recvmmsg: {}", io::Error::last_os_error());

    let received: Vec<c_int> = messages
        .iter()
        .flat_map(|message| received_fds(&message.msg_hdr))
        .collect();
    let [first, second] = received[..] else {
        panic!("expected one descriptor per message, got {received:?}");
    };
    for fd in [first, second] {
        assert_eq!(
            fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK,
            0,
            "the sender's pipe ends are blocking"
        );
    }
    write_all(first, b"one");
    write_all(second, b"two");
    assert_eq!(read_exact(first_read, 3), b"one");
    assert_eq!(read_exact(second_read, 3), b"two");
    println!("recvmmsg delivered {first} and {second}");
}

/// `MSG_CMSG_CLOEXEC` sets close-on-exec on the received descriptor only.
fn msg_cmsg_cloexec_marks_received_descriptors() {
    let (_read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[write_end]);
    send_fds(left, &[write_end]);

    let [plain] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let [cloexec] = recv_fds(right, libc::MSG_CMSG_CLOEXEC)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(fcntl(plain, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
    assert_eq!(
        fcntl(cloexec, libc::F_GETFD) & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC
    );
    assert_eq!(fcntl(write_end, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
    write_all(plain, b"p");
    write_all(cloexec, b"c");
    println!("{cloexec} is close-on-exec, {plain} is not");
}

fn eventfd(initial: u32, flags: c_int) -> c_int {
    check(unsafe { libc::eventfd(initial, flags) }, "eventfd")
}

/// One `read` of an eventfd's eight bytes.
fn read_exact_once(fd: c_int) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    assert_eq!(read, 8, "read from {fd}: {}", io::Error::last_os_error());
    bytes
}

fn eventfd_write(fd: c_int, value: u64) {
    write_all(fd, &value.to_ne_bytes());
}

/// An eventfd sent within the process is the sender's eventfd: a count
/// written through either descriptor is read through the other, and an
/// `EFD_SEMAPHORE` eventfd hands out one unit per read through both.
fn received_eventfd_shares_the_counter() {
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let original = eventfd(0, 0);
    let semaphore = eventfd(2, libc::EFD_SEMAPHORE);
    send_fds(left, &[original, semaphore]);
    let [received, received_semaphore] = recv_fds(right, 0)[..] else {
        panic!("expected two descriptors");
    };
    eventfd_write(received, 5);
    let through_original = u64::from_ne_bytes(read_exact_once(original));
    eventfd_write(original, 7);
    let through_received = u64::from_ne_bytes(read_exact_once(received));
    let units = [
        u64::from_ne_bytes(read_exact_once(received_semaphore)),
        u64::from_ne_bytes(read_exact_once(semaphore)),
    ];
    assert_eq!((through_original, through_received), (5, 7));
    assert_eq!(units, [1, 1]);
    println!("eventfd {received} shares {original}'s counter, semaphore units {units:?}");
}

/// A pipe end received from another process that keeps it names the open
/// file the sender holds, which no descriptor of the receiving table names:
/// it keeps that file's blocking state and reaches the same pipe. Detcore
/// keeps the pipe physically nonblocking, so a description of its own, made
/// from the kernel's flags, would say nonblocking. The child calls only
/// functions that allocate nothing (see
/// `blocking_read_on_a_received_pipe_waits_for_the_writer`).
fn pipe_end_from_a_process_that_keeps_it_shares_the_open_file() {
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let sender = check(unsafe { libc::fork() }, "fork");
    if sender == 0 {
        unsafe { libc::close(right) };
        let (read_end, write_end) = pipe();
        send_fds(left, &[write_end]);
        // Keep the pipe until the parent has written through its end.
        let mut byte = [0u8];
        let read = unsafe { libc::read(left, byte.as_mut_ptr().cast(), 1) };
        let mut text = [0u8; 4];
        let got = unsafe { libc::read(read_end, text.as_mut_ptr().cast(), 4) };
        let read_back = read == 1 && got == 4 && &text == b"from";
        unsafe { libc::_exit(if read_back { 0 } else { 1 }) };
    }
    unsafe { libc::close(left) };
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received), "the sender's pipe end is blocking");
    write_all(received, b"from");
    write_all(right, b"k");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(sender, &mut status, 0) }, sender);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the sender did not read what was written through the received end"
    );
    println!("descriptor {received} names the open file its sender holds");
}

/// An abstract `AF_UNIX` address for `name`, which starts with its NUL:
/// nothing on the file system, freed when the run ends.
fn abstract_address(name: &[u8]) -> (libc::sockaddr_un, u32) {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(name) {
        *slot = *byte as libc::c_char;
    }
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + name.len()) as u32;
    (address, length)
}

fn unix_stream_socket() -> c_int {
    check(
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) },
        "socket",
    )
}

/// A connection a child process makes to its parent's listener carries open
/// files as one within a process does: the child's pipe end, sent while the
/// child keeps it, names the child's open file in the parent. The bind and
/// the accept run in the parent and the connect in the child, so they meet
/// only through the connections the run's tasks share. The child calls only
/// functions that allocate nothing.
fn descriptors_over_a_connection_from_a_child_share_their_open_files() {
    let (address, length) = abstract_address(b"\0hermit-detcore-scm-rights-child-connection");
    let at = (&raw const address).cast::<libc::sockaddr>();
    let listener = unix_stream_socket();
    check(unsafe { libc::bind(listener, at, length) }, "bind");
    check(unsafe { libc::listen(listener, 1) }, "listen");
    let child = check(unsafe { libc::fork() }, "fork");
    if child == 0 {
        unsafe { libc::close(listener) };
        let client = unix_stream_socket();
        if unsafe { libc::connect(client, at, length) } != 0 {
            unsafe { libc::_exit(2) };
        }
        let (read_end, write_end) = pipe();
        send_fds(client, &[write_end]);
        // Keep the pipe until the parent has written through its end.
        let mut byte = [0u8];
        let read = unsafe { libc::read(client, byte.as_mut_ptr().cast(), 1) };
        let mut text = [0u8; 4];
        let got = unsafe { libc::read(read_end, text.as_mut_ptr().cast(), 4) };
        let read_back = read == 1 && got == 4 && &text == b"kept";
        unsafe { libc::_exit(if read_back { 0 } else { 1 }) };
    }
    let server = check(
        unsafe { libc::accept(listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );
    let [received] = recv_fds(server, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received), "the child's pipe end is blocking");
    write_all(received, b"kept");
    write_all(server, b"k");
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the child did not read what was written through the received end"
    );
    println!("descriptor {received} from a child's connection names the child's open file");
}

/// A connection the process made to its own listener carries open files as
/// a `socketpair` does: a pipe write end the connecting socket sends names
/// the sender's blocking open file, and an eventfd the accepted socket sends
/// back shares the sender's counter. Detcore pairs the accepted connection
/// with the socket whose `connect` it took.
fn descriptors_over_a_connection_to_an_own_listener_share_their_open_files() {
    let (address, length) = abstract_address(b"\0hermit-detcore-scm-rights-connected");
    let listener = unix_stream_socket();
    let at = (&raw const address).cast::<libc::sockaddr>();
    check(unsafe { libc::bind(listener, at, length) }, "bind");
    check(unsafe { libc::listen(listener, 1) }, "listen");
    let client = unix_stream_socket();
    check(unsafe { libc::connect(client, at, length) }, "connect");
    let server = check(
        unsafe { libc::accept(listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );

    let (read_end, write_end) = pipe();
    send_fds(client, &[write_end]);
    let [received_end] = recv_fds(server, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received_end), "the sender's pipe end is blocking");
    write_all(received_end, b"over");
    assert_eq!(read_exact(read_end, 4), b"over");

    let counter = eventfd(0, 0);
    send_fds(server, &[counter]);
    let [received_counter] = recv_fds(client, 0)[..] else {
        panic!("expected one descriptor");
    };
    eventfd_write(received_counter, 9);
    let count = u64::from_ne_bytes(read_exact_once(counter));
    assert_eq!(count, 9);
    println!(
        "pipe end {received_end} and eventfd {received_counter} name their senders' open files"
    );
}

/// The status flags of descriptor `received`, an eventfd, as a line to
/// print: Linux gives the sender's blocking open file, and Detcore, where it
/// cannot tell which eventfd it is, refuses the descriptor (`F_GETFL` fails
/// with EBADF, as main fails it); what it must never do is give another
/// sender's nonblocking one.
fn blocking_eventfd_status(received: c_int, what: &str) -> String {
    let status = unsafe { libc::fcntl(received, libc::F_GETFL) };
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    assert!(
        status < 0 || status & libc::O_NONBLOCK == 0,
        "{what}: the received eventfd took another sender's nonblocking open file"
    );
    format!(
        "{what}: F_GETFL={} errno={}",
        status.max(-1),
        if status < 0 { errno } else { 0 }
    )
}

/// A connection made through a relative path reaches the listener Detcore
/// knows by its absolute path without Detcore seeing it queued there. Linux
/// gives the first `accept` that connection (the oldest,
/// `unix_stream_connect` and `unix_accept`), so a blocking eventfd its
/// connecting socket sends arrives there; the nonblocking eventfd the
/// connection through the absolute path sends must not be taken for it.
/// Detcore leaves the listener's pairing unknown instead of pairing the
/// first accept with the absolute connect.
fn a_connection_through_a_relative_path_does_not_shift_the_pairing() {
    let directory = c"/tmp/hermit-detcore-scm-rights-relative";
    let path = c"/tmp/hermit-detcore-scm-rights-relative/listener";
    unsafe {
        libc::mkdir(directory.as_ptr(), 0o700);
        libc::unlink(path.as_ptr());
    }
    let address_of = |path: &[u8]| {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in address.sun_path.iter_mut().zip(path) {
            *slot = *byte as libc::c_char;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len()) as u32;
        (address, length)
    };
    let absolute = address_of(path.to_bytes());
    let relative = address_of(b"listener");
    let at = |(address, _): &(libc::sockaddr_un, u32)| (&raw const *address).cast();
    let listener = unix_stream_socket();
    check(
        unsafe { libc::bind(listener, at(&absolute), absolute.1) },
        "bind",
    );
    check(unsafe { libc::listen(listener, 2) }, "listen");
    check(unsafe { libc::chdir(directory.as_ptr()) }, "chdir");
    let through_relative = unix_stream_socket();
    check(
        unsafe { libc::connect(through_relative, at(&relative), relative.1) },
        "connect through the relative path",
    );
    let through_absolute = unix_stream_socket();
    check(
        unsafe { libc::connect(through_absolute, at(&absolute), absolute.1) },
        "connect through the absolute path",
    );
    let accept = || {
        check(
            unsafe { libc::accept(listener, ptr::null_mut(), ptr::null_mut()) },
            "accept",
        )
    };
    let (first, second) = (accept(), accept());
    send_fds(through_absolute, &[eventfd(0, libc::EFD_NONBLOCK)]);
    send_fds(through_relative, &[eventfd(0, 0)]);
    let [received] = recv_fds(first, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!(
        "{}",
        blocking_eventfd_status(received, "first accepted connection")
    );
    let [_] = recv_fds(second, 0)[..] else {
        panic!("expected one descriptor");
    };
    unsafe {
        libc::unlink(path.as_ptr());
        libc::chdir(c"/".as_ptr());
        libc::rmdir(directory.as_ptr());
    }
}

/// A child that changed its root with `chroot` connects through the same
/// absolute path that names the parent's first listener outside, and reaches
/// the listener at that path inside its root, the parent's second. Linux
/// gives the first listener's `accept` the parent's own later connection,
/// so the blocking eventfd that connection sends arrives there; the
/// nonblocking eventfd the child sends over its connection must not be taken
/// for it. Detcore leaves pairing unknown after a connect through an
/// absolute path from a changed root. The child calls only functions that
/// allocate nothing.
fn a_connection_from_a_chroot_does_not_shift_the_pairing() {
    let jail = c"/tmp/hermit-detcore-scm-rights-jail";
    let jail_tmp = c"/tmp/hermit-detcore-scm-rights-jail/tmp";
    let outside = c"/tmp/hermit-detcore-scm-rights-chroot-listener";
    let inside =
        c"/tmp/hermit-detcore-scm-rights-jail/tmp/hermit-detcore-scm-rights-chroot-listener";
    unsafe {
        libc::mkdir(jail.as_ptr(), 0o755);
        libc::mkdir(jail_tmp.as_ptr(), 0o755);
        libc::unlink(outside.as_ptr());
        libc::unlink(inside.as_ptr());
    }
    let address_of = |path: &[u8]| {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in address.sun_path.iter_mut().zip(path) {
            *slot = *byte as libc::c_char;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len()) as u32;
        (address, length)
    };
    let (outer, inner) = (
        address_of(outside.to_bytes()),
        address_of(inside.to_bytes()),
    );
    let at = |(address, _): &(libc::sockaddr_un, u32)| (&raw const *address).cast();
    let listen_at = |address: &(libc::sockaddr_un, u32), path: &std::ffi::CStr| {
        let listener = unix_stream_socket();
        check(
            unsafe { libc::bind(listener, at(address), address.1) },
            "bind",
        );
        // The child's user namespace maps no user, so it needs the others bits.
        check(unsafe { libc::chmod(path.as_ptr(), 0o777) }, "chmod");
        check(unsafe { libc::listen(listener, 2) }, "listen");
        listener
    };
    let first_listener = listen_at(&outer, outside);
    let second_listener = listen_at(&inner, inside);
    let (sent, go) = (pipe(), pipe());
    // A fork into a user namespace of its own, where the child may chroot.
    let child = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong,
            0,
            0,
            0,
            0,
        )
    };
    assert!(child >= 0, "clone: {}", io::Error::last_os_error());
    if child == 0 {
        // Only the parent writes `go`, so the child sees its end of file if
        // the parent stops early.
        unsafe {
            libc::close(go.1);
            libc::close(sent.0);
        }
        let ok = unsafe { libc::chroot(jail.as_ptr()) } == 0
            && unsafe { libc::chdir(c"/".as_ptr()) } == 0;
        let client = unix_stream_socket();
        // The outside listener's path, which names the inside one here.
        if !ok || unsafe { libc::connect(client, at(&outer), outer.1) } != 0 {
            unsafe { libc::_exit(2) };
        }
        let nonblocking = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
        write_all(sent.1, b"c");
        // Send only once the parent's own connection is queued and accepted.
        let mut byte = [0u8];
        unsafe { libc::read(go.0, byte.as_mut_ptr().cast(), 1) };
        send_fds(client, &[nonblocking]);
        // Keep the eventfd until the parent has received.
        unsafe { libc::read(go.0, byte.as_mut_ptr().cast(), 1) };
        unsafe { libc::_exit(0) };
    }
    unsafe {
        libc::close(go.0);
        libc::close(sent.1);
    }
    assert_eq!(read_exact(sent.0, 1), b"c");
    let own = unix_stream_socket();
    check(
        unsafe { libc::connect(own, at(&outer), outer.1) },
        "connect",
    );
    let accepted = check(
        unsafe { libc::accept(first_listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );
    write_all(go.1, b"s");
    let child_side = check(
        unsafe { libc::accept(second_listener, ptr::null_mut(), ptr::null_mut()) },
        "accept",
    );
    let [_] = recv_fds(child_side, 0)[..] else {
        panic!("expected one descriptor from the child");
    };
    send_fds(own, &[eventfd(0, 0)]);
    let [received] = recv_fds(accepted, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!(
        "{}",
        blocking_eventfd_status(received, "outside listener's connection")
    );
    write_all(go.1, b"e");
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child as libc::pid_t, &mut status, 0) },
        child as libc::pid_t
    );
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the child could not chroot and connect"
    );
    unsafe {
        libc::unlink(outside.as_ptr());
        libc::unlink(inside.as_ptr());
        libc::rmdir(jail_tmp.as_ptr());
        libc::rmdir(jail.as_ptr());
    }
}

/// A datagram the first socket of a `socketpair` sends to a socket it names
/// (`msg_name`) reaches that socket, not its peer (`unix_dgram_sendmsg`):
/// the nonblocking eventfd it carried must not be taken for the blocking one
/// the next datagram brings the peer.
fn an_addressed_datagram_does_not_take_the_peers_queue() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let named = abstract_address(b"\0hermit-detcore-scm-rights-addressed");
    let other = check(
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) },
        "socket",
    );
    check(
        unsafe { libc::bind(other, (&raw const named.0).cast(), named.1) },
        "bind",
    );
    send_fds_to(left, &[eventfd(0, libc::EFD_NONBLOCK)], &named);
    send_fds(left, &[eventfd(0, 0)]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!("{}", blocking_eventfd_status(received, "peer"));
    let [_] = recv_fds(other, 0)[..] else {
        panic!("expected one descriptor");
    };
}

/// A `socketpair` datagram socket that connects to another socket sends to
/// it (`unix_dgram_connect`); connected back to its old peer, it reaches the
/// peer again. The nonblocking eventfd it sent while connected elsewhere
/// must not be taken for the blocking one it sends the peer afterwards.
fn a_reconnected_datagram_socket_does_not_take_the_peers_queue() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let right_name = abstract_address(b"\0hermit-detcore-scm-rights-reconnected-peer");
    let other_name = abstract_address(b"\0hermit-detcore-scm-rights-reconnected-other");
    check(
        unsafe { libc::bind(right, (&raw const right_name.0).cast(), right_name.1) },
        "bind",
    );
    let other = check(
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) },
        "socket",
    );
    check(
        unsafe { libc::bind(other, (&raw const other_name.0).cast(), other_name.1) },
        "bind",
    );
    check(
        unsafe { libc::connect(left, (&raw const other_name.0).cast(), other_name.1) },
        "connect to another socket",
    );
    send_fds(left, &[eventfd(0, libc::EFD_NONBLOCK)]);
    check(
        unsafe { libc::connect(left, (&raw const right_name.0).cast(), right_name.1) },
        "connect back to the peer",
    );
    send_fds(left, &[eventfd(0, 0)]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    println!("{}", blocking_eventfd_status(received, "peer"));
    let [_] = recv_fds(other, 0)[..] else {
        panic!("expected one descriptor");
    };
}

/// An eventfd received after a receive with no room released an earlier
/// eventfd's message: every eventfd has one file identity, so only order
/// could tell the two apart, and the socket's queue no longer follows its
/// messages one for one. The later, blocking eventfd must never take the
/// released, nonblocking one's open file. Detcore refuses it instead, as main
/// refuses an eventfd it cannot classify, so `F_GETFL` through it fails with
/// EBADF where Linux reports a blocking descriptor.
fn eventfd_after_a_released_one_does_not_join_it() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let released = eventfd(0, libc::EFD_NONBLOCK);
    send_fds(left, &[released]);
    let (none, ctrunc) = recv_fds_into(right, 0, 0);
    assert!(none.is_empty() && ctrunc, "Linux releases the descriptor");
    let kept = eventfd(5, 0);
    send_fds(left, &[kept]);
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let status = unsafe { libc::fcntl(received, libc::F_GETFL) };
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    assert!(
        status < 0 || status & libc::O_NONBLOCK == 0,
        "the received eventfd took the released eventfd's nonblocking open file"
    );
    println!(
        "received eventfd: F_GETFL={} errno={}",
        status.max(-1),
        if status < 0 { errno } else { 0 }
    );
}

/// `recvmmsg` with `MSG_PEEK` peeks the first datagram for every entry, as
/// Linux does: each entry's descriptor names the first message's pipe end,
/// and both messages are still there to receive, in order.
fn recvmmsg_peek_names_the_first_message_for_every_entry() {
    let pipes: Vec<(c_int, c_int)> = (0..2).map(|_| pipe()).collect();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    send_fds(left, &[pipes[0].1]);
    send_fds(left, &[pipes[1].1]);

    let mut payloads = [[0u8; 1]; 2];
    let mut iovs = payloads.each_mut().map(|payload| libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    });
    let mut controls = [[0u64; 16]; 2];
    let mut messages: [libc::mmsghdr; 2] = unsafe { std::mem::zeroed() };
    for ((message, iov), control) in messages.iter_mut().zip(&mut iovs).zip(&mut controls) {
        message.msg_hdr.msg_iov = iov;
        message.msg_hdr.msg_iovlen = 1;
        message.msg_hdr.msg_control = control.as_mut_ptr().cast();
        message.msg_hdr.msg_controllen = size_of_val(control) as _;
    }
    let peeked = unsafe {
        libc::recvmmsg(
            right,
            messages.as_mut_ptr(),
            2,
            libc::MSG_PEEK,
            ptr::null_mut(),
        )
    };
    assert!(peeked >= 1, "recvmmsg: {}", io::Error::last_os_error());
    for message in &messages[..peeked as usize] {
        let [fd] = received_fds(&message.msg_hdr)[..] else {
            panic!("expected one descriptor per peeked entry");
        };
        assert!(blocking(fd), "the first message's pipe end is blocking");
        write_all(fd, b"p");
        assert_eq!(read_exact(pipes[0].0, 1), b"p");
    }
    for (index, (read_end, _)) in pipes.iter().enumerate() {
        let [fd] = recv_fds(right, 0)[..] else {
            panic!("expected one descriptor");
        };
        assert!(blocking(fd), "pipe {index}'s write end is blocking");
        write_all(fd, b"r");
        assert_eq!(read_exact(*read_end, 1), b"r");
    }
    println!("{peeked} peeked entries named the first message");
}

/// A stream message long enough for Linux to queue in several buffers
/// carries its descriptors in the first only: a receive of one byte takes
/// them all, and the rest of the bytes, read without a control buffer, carry
/// none. The socket's queue still follows its messages, so the next
/// message's eventfd joins its sender's.
fn descriptors_of_a_long_stream_message_come_with_its_first_byte() {
    // Above half the default send buffer, so Linux splits it.
    const LENGTH: usize = 150_000;
    let (left, right) = socketpair(libc::SOCK_STREAM);
    let (read_end, write_end) = pipe();
    let mut payload = vec![b'x'; LENGTH];
    let mut sent = send_fds_with(left, &[write_end], &mut payload);
    while sent < LENGTH {
        let written = unsafe { libc::write(left, payload[sent..].as_ptr().cast(), LENGTH - sent) };
        assert!(written > 0, "write: {}", io::Error::last_os_error());
        sent += written as usize;
    }
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(received), "the sender's pipe end is blocking");
    write_all(received, b"w");
    assert_eq!(read_exact(read_end, 1), b"w");
    assert!(
        read_exact(right, LENGTH - 1)
            .iter()
            .all(|&byte| byte == b'x')
    );

    let counter = eventfd(0, 0);
    send_fds(left, &[counter]);
    let [received_counter] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    eventfd_write(received_counter, 3);
    assert_eq!(u64::from_ne_bytes(read_exact_once(counter)), 3);
    println!("the long message's descriptor came with its first byte; the next eventfd joined");
}

/// The open files a socket's messages carry are matched in order: a
/// `MSG_PEEK` receive and the receive after it both name the sent pipe end; a
/// receive with room for one of two descriptors names the first (Linux sets
/// `MSG_CTRUNC` and releases the second); and after a receive with no control
/// buffer discards a message, the next receive names its own message's pipe
/// end, not the discarded one.
fn queued_rights_follow_peek_truncation_and_discards() {
    let pipes: Vec<(c_int, c_int)> = (0..4).map(|_| pipe()).collect();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    // Whether `received` is the write end of pipe `index`.
    let reaches = |received: c_int, index: usize| {
        assert!(blocking(received), "pipe {index}'s write end is blocking");
        write_all(received, b"w");
        assert_eq!(read_exact(pipes[index].0, 1), b"w");
    };

    send_fds(left, &[pipes[0].1]);
    let [peeked] = recv_fds(right, libc::MSG_PEEK)[..] else {
        panic!("expected one peeked descriptor");
    };
    let [taken] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_ne!(peeked, taken);
    reaches(peeked, 0);
    reaches(taken, 0);

    send_fds(left, &[pipes[1].1, pipes[2].1]);
    // CMSG_LEN(sizeof(int)): room for one descriptor (CMSG_SPACE's padding
    // would hold a second).
    let room = unsafe { libc::CMSG_LEN(size_of::<c_int>() as u32) } as usize;
    let (truncated, ctrunc) = recv_fds_into(right, 0, room);
    assert!(ctrunc, "Linux sets MSG_CTRUNC");
    let [first] = truncated[..] else {
        panic!("expected one of two descriptors, got {truncated:?}");
    };
    reaches(first, 1);

    send_fds(left, &[pipes[2].1]);
    let (discarded, ctrunc) = recv_fds_into(right, 0, 0);
    assert!(
        discarded.is_empty() && ctrunc,
        "the message's descriptors are discarded"
    );
    send_fds(left, &[pipes[3].1]);
    let [last] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    reaches(last, 3);
    println!("peeked {peeked}, taken {taken}, truncated {first}, after a discard {last}");
}

/// `recv` with `MSG_PEEK` takes nothing, and a `read` of no bytes returns
/// at once (`sock_read_iter`): the message stays queued with the open file
/// it carries, and the receive after them names it, on a stream and on a
/// datagram socket.
fn a_peek_or_a_read_of_no_bytes_leaves_the_message() {
    let pipes: Vec<(c_int, c_int)> = (0..2).map(|_| pipe()).collect();
    for (index, ty) in [libc::SOCK_STREAM, libc::SOCK_DGRAM]
        .into_iter()
        .enumerate()
    {
        let (left, right) = socketpair(ty);
        send_fds(left, &[pipes[index].1]);
        let mut byte = [0u8; 1];
        let peeked = unsafe { libc::recv(right, byte.as_mut_ptr().cast(), 1, libc::MSG_PEEK) };
        assert_eq!(peeked, 1, "recv: {}", io::Error::last_os_error());
        let read = unsafe { libc::read(right, byte.as_mut_ptr().cast(), 0) };
        assert_eq!(read, 0, "read: {}", io::Error::last_os_error());
        let [taken] = recv_fds(right, 0)[..] else {
            panic!("expected one descriptor");
        };
        assert!(blocking(taken), "pipe {index}'s write end is blocking");
        write_all(taken, b"w");
        assert_eq!(read_exact(pipes[index].0, 1), b"w");
        println!("socket type {ty}: the peeked message named pipe {index}");
    }
}

/// A datagram `read` takes the oldest message, here one that carried no
/// descriptor, and Linux delivers the next with its descriptor: the queue
/// keeps that one, and its receipt names the sender's open file.
fn a_data_only_read_leaves_the_next_message() {
    let (read_end, write_end) = pipe();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    write_all(left, b"d");
    send_fds(left, &[write_end]);
    assert_eq!(read_exact(right, 1), b"d");
    let [taken] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert!(blocking(taken), "the write end is blocking");
    write_all(taken, b"w");
    assert_eq!(read_exact(read_end, 1), b"w");
    println!("after a data-only read, descriptor {taken} named the write end");
}

/// Both ends of a pipe have one file identity. Sent read end first and
/// write end after, around a peek, each receipt names its own end, in order:
/// the read end stays blocking although the write end is not.
fn the_ends_of_a_pipe_arrive_in_order_around_a_peek() {
    let (read_end, write_end) = pipe();
    check(
        unsafe { libc::fcntl(write_end, libc::F_SETFL, libc::O_NONBLOCK) },
        "F_SETFL",
    );
    let (left, right) = socketpair(libc::SOCK_STREAM);
    send_fds(left, &[read_end]);
    let mut byte = [0u8; 1];
    let peeked = unsafe { libc::recv(right, byte.as_mut_ptr().cast(), 1, libc::MSG_PEEK) };
    assert_eq!(peeked, 1, "recv: {}", io::Error::last_os_error());
    send_fds(left, &[write_end]);
    let [first] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let [second] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(
        fcntl(first, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    assert_eq!(
        fcntl(second, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_WRONLY
    );
    assert!(blocking(first), "the read end is blocking");
    assert!(
        !blocking(second),
        "the write end is the sender's, nonblocking"
    );
    write_all(second, b"x");
    assert_eq!(read_exact(first, 1), b"x");
    println!("read end {first}, then write end {second}");
}

/// A `recvmmsg` whose first entry has room for one of its message's two
/// descriptors and whose second has no control buffer: Linux installs the
/// first descriptor and releases the second (`MSG_CTRUNC`), and releases the
/// second message's descriptor unseen. The third entry's message, still
/// queued, names its own pipe end.
fn a_batch_that_truncates_and_discards_leaves_the_later_message() {
    let pipes: Vec<(c_int, c_int)> = (0..4).map(|_| pipe()).collect();
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    send_fds(left, &[pipes[0].1, pipes[1].1]);
    send_fds(left, &[pipes[2].1]);
    send_fds(left, &[pipes[3].1]);

    let mut payloads = [[0u8; 1]; 3];
    let mut iovs = payloads.each_mut().map(|payload| libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    });
    let mut controls = [[0u64; 16]; 3];
    // CMSG_LEN(sizeof(int)): room for one descriptor.
    let one = unsafe { libc::CMSG_LEN(size_of::<c_int>() as u32) } as usize;
    let rooms = [one, 0, size_of::<[u64; 16]>()];
    let mut messages: [libc::mmsghdr; 3] = unsafe { std::mem::zeroed() };
    for (((message, iov), control), room) in messages
        .iter_mut()
        .zip(&mut iovs)
        .zip(&mut controls)
        .zip(rooms)
    {
        message.msg_hdr.msg_iov = iov;
        message.msg_hdr.msg_iovlen = 1;
        if room > 0 {
            message.msg_hdr.msg_control = control.as_mut_ptr().cast();
            message.msg_hdr.msg_controllen = room as _;
        }
    }
    let received = unsafe { libc::recvmmsg(right, messages.as_mut_ptr(), 3, 0, ptr::null_mut()) };
    assert_eq!(received, 3, "recvmmsg: {}", io::Error::last_os_error());
    let truncated = |message: &libc::mmsghdr| message.msg_hdr.msg_flags & libc::MSG_CTRUNC != 0;
    assert!(truncated(&messages[0]) && truncated(&messages[1]));
    let [first] = received_fds(&messages[0].msg_hdr)[..] else {
        panic!("expected one of the first message's two descriptors");
    };
    assert!(received_fds(&messages[1].msg_hdr).is_empty());
    let [last] = received_fds(&messages[2].msg_hdr)[..] else {
        panic!("expected the third message's descriptor");
    };
    for (fd, index) in [(first, 0), (last, 3)] {
        assert!(blocking(fd), "pipe {index}'s write end is blocking");
        write_all(fd, b"w");
        assert_eq!(read_exact(pipes[index].0, 1), b"w");
    }
    println!("truncated {first}, after a discard in the same batch {last}");
}

/// A datagram receive that fails with `EFAULT` copying its byte has taken
/// its message, and Linux freed it, descriptor and all
/// (`__unix_dgram_recvmsg`). The nonblocking eventfd it carried must not be
/// taken for the blocking one the next message brings, although only the
/// queued messages still held the two. The socketpair's queue holds every
/// message, so the failed receive gives up the oldest, and the next receipt
/// joins the kept eventfd: blocking, as Linux reports it (`O_RDWR`, 2).
fn an_efault_receive_does_not_leave_its_eventfd_to_the_next() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let released = eventfd(0, libc::EFD_NONBLOCK);
    let kept = eventfd(5, 0);
    send_fds(left, &[released]);
    send_fds(left, &[kept]);
    unsafe {
        libc::close(released);
        libc::close(kept);
    }
    let mut iov = libc::iovec {
        // No page is mapped at address 1.
        iov_base: ptr::without_provenance_mut(1),
        iov_len: 1,
    };
    let mut control = [0u64; 16];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control) as _;
    let failed = unsafe { libc::recvmsg(right, &mut message, 0) };
    let errno = io::Error::last_os_error().raw_os_error();
    assert_eq!((failed, errno), (-1, Some(libc::EFAULT)));
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let status = unsafe { libc::fcntl(received, libc::F_GETFL) };
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    assert_eq!(
        (status, if status < 0 { errno } else { 0 }),
        (libc::O_RDWR, 0),
        "the received eventfd is the kept one, blocking"
    );
    assert_eq!(u64::from_ne_bytes(read_exact_once(received)), 5);
    println!("after an EFAULT receive: F_GETFL={status}");
}

/// `preadv2` at the current offset (-1) reads a socket as `readv` does: on a
/// datagram socket it takes the oldest message, and Linux releases the
/// descriptor that message carried unseen. The nonblocking eventfd it
/// carried must not be taken for the blocking one the next message brings,
/// as after an `EFAULT` receive: the next receipt joins the kept eventfd.
fn a_preadv2_at_the_current_offset_takes_its_message() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let released = eventfd(0, libc::EFD_NONBLOCK);
    let kept = eventfd(5, 0);
    send_fds(left, &[released]);
    send_fds(left, &[kept]);
    unsafe {
        libc::close(released);
        libc::close(kept);
    }
    let mut byte = [0u8; 1];
    let iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let taken = unsafe { libc::preadv2(right, &iov, 1, -1, 0) };
    assert_eq!(taken, 1, "preadv2: {}", io::Error::last_os_error());
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let status = unsafe { libc::fcntl(received, libc::F_GETFL) };
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    assert_eq!(
        (status, if status < 0 { errno } else { 0 }),
        (libc::O_RDWR, 0),
        "the received eventfd is the kept one, blocking"
    );
    assert_eq!(u64::from_ne_bytes(read_exact_once(received)), 5);
    println!("after a preadv2 at the current offset: F_GETFL={status}");
}

/// A loopback address of this test process's own for the port tests: the
/// 22 bits of its PID in 127.64.0.0/10, so that the ports Detcore hands out
/// are free there whatever else the host binds, as in
/// `bound_port_survives_closing_dup_alias` (which takes 127.128.0.0/10).
/// Set before the guest forks, which inherits it: the guest's own PID is in
/// a namespace of its own (`run_deterministically`).
static LOOPBACK: std::sync::OnceLock<[u8; 4]> = std::sync::OnceLock::new();

fn loopback() -> [u8; 4] {
    *LOOPBACK.get_or_init(|| {
        let pid = std::process::id();
        assert!(pid < 1 << 22, "PID {pid} exceeds Linux's 22-bit limit");
        [127, 0x40 | (pid >> 16) as u8, (pid >> 8) as u8, pid as u8]
    })
}

/// A UDP socket bound to port 0 of [`loopback`], and its port, or the
/// `bind` errno.
fn bound_udp_socket() -> (c_int, Result<u16, i32>) {
    let socket = check(
        unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) },
        "socket",
    );
    let mut address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(loopback()),
        },
        sin_zero: [0; 8],
    };
    let length = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    if unsafe { libc::bind(socket, (&raw const address).cast(), length) } != 0 {
        return (
            socket,
            Err(io::Error::last_os_error().raw_os_error().unwrap_or(0)),
        );
    }
    let mut named = length;
    check(
        unsafe { libc::getsockname(socket, (&raw mut address).cast(), &mut named) },
        "getsockname",
    );
    (socket, Ok(u16::from_be(address.sin_port)))
}

/// A socket in flight is still bound, as Linux's in-flight reference keeps
/// it: a later `bind` to port 0 succeeds with another port, although the
/// sender closed its descriptor.
fn a_socket_in_flight_keeps_its_port() {
    let (sent, port) = bound_udp_socket();
    let port = port.expect("bind the first socket");
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    send_fds(left, &[sent]);
    check(unsafe { libc::close(sent) }, "close");
    let (other, other_port) = bound_udp_socket();
    let other_port = other_port.expect("a bind to port 0 while a bound socket is in flight");
    assert_ne!(other_port, port, "the socket in flight keeps its port");
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    unsafe {
        libc::close(received);
        libc::close(other);
    }
    println!("the socket in flight kept its port; another bind took the next");
}

/// A socket only a queued message holds is released, port and all, when
/// Linux frees the message: by the close of the receiving socket's last
/// descriptor (`unix_release_sock`), or by a send through a datagram socket
/// whose peer has gone (`ECONNREFUSED`, `unix_dgram_disconnected`), which
/// purges that socket's own queue. Detcore hands the next `bind` to port 0
/// the port it freed last, so each comes back at once.
fn a_freed_message_frees_its_sockets_port() {
    let (sent, port) = bound_udp_socket();
    let port = port.expect("bind the first socket");
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    send_fds(left, &[sent]);
    check(unsafe { libc::close(sent) }, "close");
    check(unsafe { libc::close(right) }, "close the receiving socket");
    let (reused, reused_port) = bound_udp_socket();
    assert_eq!(
        reused_port,
        Ok(port),
        "the receiving socket's close freed the port"
    );
    check(unsafe { libc::close(reused) }, "close");

    let (sent, again) = bound_udp_socket();
    assert_eq!(again, Ok(port));
    let (receiver, sender) = socketpair(libc::SOCK_DGRAM);
    send_fds(sender, &[sent]);
    check(unsafe { libc::close(sent) }, "close");
    check(unsafe { libc::close(sender) }, "close the peer");
    let refused = unsafe { libc::send(receiver, b"x".as_ptr().cast(), 1, 0) };
    let errno = io::Error::last_os_error().raw_os_error();
    assert_eq!((refused, errno), (-1, Some(libc::ECONNREFUSED)));
    let (reused, reused_port) = bound_udp_socket();
    assert_eq!(reused_port, Ok(port), "the purge freed the port");
    unsafe {
        libc::close(reused);
        libc::close(left);
        libc::close(receiver);
    }
    println!("each freed message freed port {port}");
}

/// The `fcntl(F_GETFL)` of `fd`, or `-1 - errno` where it fails.
fn status_or_errno(fd: c_int) -> c_int {
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status < 0 {
        -1 - io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        status
    }
}

/// A `read` takes a datagram or seqpacket socket's oldest message, or a
/// stream socket's oldest bytes, and Linux releases the descriptors the
/// message carried, or those of the message whose bytes the read reached: a
/// bound socket only that message held is closed, so its port is free again,
/// with the receiving socket still open
/// (https://github.com/rrnewton/hermit/issues/4930). A message of data alone
/// queued before it is read first and releases nothing: a bind to port 0
/// after that read gets another port. Detcore hands the next bind to port 0
/// the port it allocated last when that one is free again, so the bind after
/// the discarding read, with no bind between, gets the discarded socket's
/// port. One stream read of both messages' bytes releases the socket too.
/// Linux 7.1.3 refuses the exact port (`EADDRINUSE`) after the read of data
/// alone, and binds it after the discarding read, on all three types.
fn a_plain_read_frees_the_port_of_the_socket_it_discards() {
    for ty in [libc::SOCK_DGRAM, libc::SOCK_SEQPACKET, libc::SOCK_STREAM] {
        for bind_between in [true, false] {
            let (sent, port) = bound_udp_socket();
            let port = port.expect("bind the first socket");
            let (left, right) = socketpair(ty);
            write_all(left, b"data");
            send_fds(left, &[sent]);
            check(unsafe { libc::close(sent) }, "close");
            let mut buffer = [0u8; 8];
            let read = unsafe { libc::read(right, buffer.as_mut_ptr().cast(), 4) };
            assert_eq!(read, 4, "read: {}", io::Error::last_os_error());
            let held = bind_between.then(|| {
                let (held, held_port) = bound_udp_socket();
                assert_ne!(
                    held_port,
                    Ok(port),
                    "socket type {ty}: the read of data alone released nothing"
                );
                held
            });
            let read = unsafe { libc::read(right, buffer.as_mut_ptr().cast(), buffer.len()) };
            assert_eq!(read, 1, "read: {}", io::Error::last_os_error());
            if let Some(held) = held {
                check(unsafe { libc::close(held) }, "close");
            } else {
                let (reused, reused_port) = bound_udp_socket();
                assert_eq!(
                    reused_port,
                    Ok(port),
                    "socket type {ty}: the read released the socket its message carried"
                );
                check(unsafe { libc::close(reused) }, "close");
            }
            unsafe {
                libc::close(left);
                libc::close(right);
            }
        }
        println!("socket type {ty}: the discarding read freed the port, the data read did not");
    }
    let (sent, port) = bound_udp_socket();
    let port = port.expect("bind the first socket");
    let (left, right) = socketpair(libc::SOCK_STREAM);
    write_all(left, b"data");
    send_fds(left, &[sent]);
    check(unsafe { libc::close(sent) }, "close");
    let mut buffer = [0u8; 8];
    let read = unsafe { libc::read(right, buffer.as_mut_ptr().cast(), buffer.len()) };
    assert_eq!(read, 5, "read: {}", io::Error::last_os_error());
    let (reused, reused_port) = bound_udp_socket();
    assert_eq!(
        reused_port,
        Ok(port),
        "one stream read of both messages released the socket"
    );
    unsafe {
        libc::close(reused);
        libc::close(left);
        libc::close(right);
    }
    println!("one stream read of data and the message freed the port");
}

/// A message sent to the receiving socket's name reaches it unqueued (an
/// addressed datagram), naming the same file as a later queued one (the two
/// ends of a pipe share its identity). Its receipt matches that later
/// message, which Linux cannot have delivered yet, so it joins nothing and
/// takes nothing: the messages queued up to it stay, and their receipts join
/// their own ends, blocking, as Linux shows them. The first receipt, of a
/// message Detcore never queued, records the received descriptor from the
/// kernel's facts, where Detcore keeps the pipe nonblocking. Linux 7.1.3
/// prints nonblock 0 for all three.
fn a_receipt_that_names_a_later_message_keeps_the_queue() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let name = abstract_address(b"\0scm-rights-unqueued");
    check(
        unsafe { libc::bind(right, (&raw const name.0).cast(), name.1) },
        "bind",
    );
    let (p_read, p_write) = pipe();
    let (_q_read, q_write) = pipe();
    send_fds_to(left, &[p_read], &name);
    send_fds(left, &[q_write]);
    send_fds(left, &[p_write]);
    unsafe {
        libc::close(p_read);
        libc::close(q_write);
        libc::close(p_write);
    }
    let [first] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let [second] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let [third] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(
        fcntl(first, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    for fd in [second, third] {
        assert_eq!(fcntl(fd, libc::F_GETFL) & libc::O_ACCMODE, libc::O_WRONLY);
        assert!(
            blocking(fd),
            "a queued pipe end joins its sender's, blocking"
        );
    }
    println!(
        "first nonblock={}, second and third blocking",
        u8::from(!blocking(first))
    );
}

/// A receive that took its message and then failed copying an output out
/// (`EFAULT`): a `recvfrom` whose address length is on a read-only page
/// (`move_addr_to_user`), and a `recvmsg` whose header is on one, with no
/// control buffer (the header's write-back). Linux released the nonblocking
/// eventfd that message carried, and the next receipt brings the blocking
/// one (Linux 7.1.3: `F_GETFL` 2 in all four). A socketpair's queue holds
/// every message, so the next receipt joins the kept eventfd, as Linux shows
/// it: a datagram receive took the oldest message, and a stream receive,
/// which reports no count, took the byte it asked for, which the memory map
/// shows writable, and detached the first eventfd with it. Counted as
/// leaving that message, a stream receive would take the released
/// nonblocking eventfd's open file (`F_GETFL` 2050); counted as one that may
/// have taken it, the receipt could be either eventfd's, joins neither, and
/// is refused (`F_GETFL` fails with `EBADF`).
fn a_receive_that_fails_copying_out_took_its_message() {
    for ty in [libc::SOCK_STREAM, libc::SOCK_DGRAM] {
        for via_recvmsg in [false, true] {
            let (left, right) = socketpair(ty);
            let released = eventfd(0, libc::EFD_NONBLOCK);
            let kept = eventfd(5, 0);
            send_fds(left, &[released]);
            send_fds(left, &[kept]);
            unsafe {
                libc::close(released);
                libc::close(kept);
            }
            let page = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(page, libc::MAP_FAILED);
            let mut byte = [0u8; 1];
            let mut iov = libc::iovec {
                iov_base: byte.as_mut_ptr().cast(),
                iov_len: 1,
            };
            let failed = if via_recvmsg {
                let header = page.cast::<libc::msghdr>();
                unsafe {
                    header.write(std::mem::zeroed());
                    (*header).msg_iov = &mut iov;
                    (*header).msg_iovlen = 1;
                    check(libc::mprotect(page, 4096, libc::PROT_READ), "mprotect");
                    libc::recvmsg(right, header, 0)
                }
            } else {
                let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
                let length = page.cast::<libc::socklen_t>();
                unsafe {
                    length.write(size_of::<libc::sockaddr_un>() as libc::socklen_t);
                    check(libc::mprotect(page, 4096, libc::PROT_READ), "mprotect");
                    libc::recvfrom(
                        right,
                        byte.as_mut_ptr().cast(),
                        1,
                        0,
                        (&raw mut address).cast(),
                        length,
                    )
                }
            };
            let errno = io::Error::last_os_error().raw_os_error();
            assert_eq!((failed, errno), (-1, Some(libc::EFAULT)));
            let [received] = recv_fds(right, 0)[..] else {
                panic!("expected one descriptor");
            };
            let status = status_or_errno(received);
            assert_eq!(status, libc::O_RDWR, "the kept eventfd, blocking");
            assert_eq!(u64::from_ne_bytes(read_exact_once(received)), 5);
            unsafe {
                libc::munmap(page, 4096);
                libc::close(left);
                libc::close(right);
            }
            println!(
                "socket type {ty}, {}: F_GETFL={status}",
                if via_recvmsg { "recvmsg" } else { "recvfrom" }
            );
        }
    }
}

/// Receives that fail with `EFAULT` before they read the socket take
/// nothing: a `readv` whose iovec array cannot be read (`__import_iovec`), a
/// `recvmsg` whose header cannot be read (`recvmsg_copy_msghdr`) and a `read`
/// into a kernel-range buffer (`import_ubuf`'s `access_ok`). The next receipt
/// is the first message's, the pipe's read end, and joins it, blocking; the
/// one after is the write end's. A `read` into an unmapped user buffer takes
/// the first message, which Linux frees: the next receipt is the write
/// end's. Linux 7.1.3 prints the same.
fn a_receive_that_fails_before_the_socket_takes_nothing() {
    for how in ["readv", "recvmsg", "kernel", "unmapped"] {
        let (left, right) = socketpair(libc::SOCK_DGRAM);
        let (read_end, write_end) = pipe();
        send_fds(left, &[read_end]);
        send_fds(left, &[write_end]);
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
        let failed = unsafe {
            match how {
                "readv" => libc::readv(right, ptr::without_provenance(8), 1),
                "recvmsg" => libc::recvmsg(right, ptr::without_provenance_mut(8), 0),
                "kernel" => {
                    libc::read(right, ptr::without_provenance_mut(0xffff_8880_0000_0000), 1)
                }
                _ => libc::read(right, ptr::without_provenance_mut(8), 1),
            }
        };
        let errno = io::Error::last_os_error().raw_os_error();
        assert_eq!((failed, errno), (-1, Some(libc::EFAULT)), "{how}");
        let [first] = recv_fds(right, 0)[..] else {
            panic!("expected one descriptor");
        };
        let access = fcntl(first, libc::F_GETFL) & libc::O_ACCMODE;
        assert!(
            blocking(first),
            "{how}: the receipt joins its pipe end, blocking"
        );
        if how == "unmapped" {
            assert_eq!(
                access,
                libc::O_WRONLY,
                "{how}: the failed read took the read end"
            );
        } else {
            assert_eq!(
                access,
                libc::O_RDONLY,
                "{how}: the failed call took nothing"
            );
            let [second] = recv_fds(right, 0)[..] else {
                panic!("expected one descriptor");
            };
            assert_eq!(
                fcntl(second, libc::F_GETFL) & libc::O_ACCMODE,
                libc::O_WRONLY
            );
            assert!(blocking(second), "{how}: the write end joins its sender's");
        }
        unsafe {
            libc::close(left);
            libc::close(right);
        }
        println!(
            "{how}: the next receipt is the {}",
            if access == libc::O_RDONLY {
                "read end"
            } else {
                "write end"
            }
        );
    }
}

/// A `write`, `writev` or `pwritev2` at the current offset through a
/// datagram socket whose peer has gone fails with `ECONNREFUSED` and purges
/// the socket's own queue (`unix_dgram_sendmsg`), freeing a bound socket only
/// that queue held, as a `sendto` does (`a_freed_message_frees_its_sockets_port`).
/// Linux 7.1.3 rebinds the exact port after each.
fn a_write_to_a_gone_peer_frees_the_queued_sockets_port() {
    for how in ["write", "writev", "pwritev2"] {
        let (sent, port) = bound_udp_socket();
        let port = port.expect("bind the first socket");
        let (receiver, sender) = socketpair(libc::SOCK_DGRAM);
        send_fds(sender, &[sent]);
        check(unsafe { libc::close(sent) }, "close");
        check(unsafe { libc::close(sender) }, "close the peer");
        let byte = *b"x";
        let iov = libc::iovec {
            iov_base: byte.as_ptr().cast_mut().cast(),
            iov_len: 1,
        };
        let refused = unsafe {
            match how {
                "write" => libc::write(receiver, byte.as_ptr().cast(), 1),
                "writev" => libc::writev(receiver, &iov, 1),
                _ => libc::pwritev2(receiver, &iov, 1, -1, 0),
            }
        };
        let errno = io::Error::last_os_error().raw_os_error();
        assert_eq!((refused, errno), (-1, Some(libc::ECONNREFUSED)), "{how}");
        let (reused, reused_port) = bound_udp_socket();
        assert_eq!(reused_port, Ok(port), "{how}: the purge freed the port");
        unsafe {
            libc::close(reused);
            libc::close(receiver);
        }
        println!("{how}: the purge freed port {port}");
    }
}

/// Detcore refuses a `sendfile` through a socket with `ENOSYS`
/// (`Detcore::handle_sendfile` mediates regular files and memfds only), so
/// it takes no message: the next receipt is the first message's, and joins
/// it, blocking. On Linux 7.1.3 a `sendfile` from a datagram socket into a
/// pipe reads it once and takes that message (`splice_file_to_pipe`).
fn a_sendfile_through_a_socket_takes_nothing() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let (x_read, _x_write) = pipe();
    let (_y_read, y_write) = pipe();
    send_fds(left, &[x_read]);
    send_fds(left, &[y_write]);
    unsafe {
        libc::close(x_read);
        libc::close(y_write);
    }
    let (into_read, into_write) = pipe();
    let moved = unsafe { libc::sendfile(into_write, right, ptr::null_mut(), 1) };
    let errno = io::Error::last_os_error().raw_os_error();
    assert_eq!((moved, errno), (-1, Some(libc::ENOSYS)));
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(
        fcntl(received, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    assert!(
        blocking(received),
        "the first message's pipe end joins its sender's"
    );
    unsafe {
        libc::close(into_read);
        libc::close(into_write);
    }
    println!("after a refused sendfile, descriptor {received} named x's read end");
}

/// With `SO_PEEK_OFF`, each peek reaches the message after the one before
/// (`__skb_try_recv_datagram` skips the bytes already peeked): the second
/// peek names the second message, which no other queued message names, so
/// its receipt joins it, blocking. The receives that follow take both in
/// order. Linux 7.1.3 prints the same.
fn a_peek_past_the_first_message_names_it() {
    let (left, right) = socketpair(libc::SOCK_DGRAM);
    let zero: c_int = 0;
    check(
        unsafe {
            libc::setsockopt(
                right,
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&raw const zero).cast(),
                size_of::<c_int>() as libc::socklen_t,
            )
        },
        "SO_PEEK_OFF",
    );
    let (x_read, _x_write) = pipe();
    let (_y_read, y_write) = pipe();
    send_fds(left, &[x_read]);
    send_fds(left, &[y_write]);
    let [first] = recv_fds(right, libc::MSG_PEEK)[..] else {
        panic!("expected one descriptor");
    };
    let [second] = recv_fds(right, libc::MSG_PEEK)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(
        fcntl(first, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    assert_eq!(
        fcntl(second, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_WRONLY
    );
    assert!(
        blocking(first) && blocking(second),
        "each peek joins its pipe end"
    );
    let [taken_first] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    let [taken_second] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(
        fcntl(taken_first, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    assert_eq!(
        fcntl(taken_second, libc::F_GETFL) & libc::O_ACCMODE,
        libc::O_WRONLY
    );
    println!("peeked {first} then {second}; took {taken_first} then {taken_second}");
}

/// A bound socket rides a message queued to a socket whose only holder, a
/// child, exits without receiving it; the parent then closes its own
/// descriptor of the bound socket, the last one once the child's exit freed
/// the queue, and its port is free again. With `shared`, the parent keeps
/// the receiving socket too, and the port is freed when the parent closes
/// that after the child's exit. Linux 7.1.3 frees the port at the child's
/// exit, and at the parent's close of the receiving socket with `shared`.
/// Whether the backend has torn the child's state down by then decides
/// nothing (https://github.com/rrnewton/hermit/issues/4930).
fn the_queue_of_an_exited_childs_socket_goes_with_it() {
    for shared in [false, true] {
        let (left, right) = socketpair(libc::SOCK_DGRAM);
        let (go_read, go_write) = pipe();
        let child = check(unsafe { libc::fork() }, "fork");
        if child == 0 {
            unsafe {
                libc::close(left);
                libc::close(go_write);
            }
            let mut byte = [0u8];
            let read = unsafe { libc::read(go_read, byte.as_mut_ptr().cast(), 1) };
            unsafe { libc::_exit(if read == 1 { 0 } else { 1 }) };
        }
        unsafe { libc::close(go_read) };
        if !shared {
            check(unsafe { libc::close(right) }, "close");
        }
        let (sent, port) = bound_udp_socket();
        let port = port.expect("bind the first socket");
        send_fds(left, &[sent]);
        check(unsafe { libc::close(sent) }, "close");
        write_all(go_write, b"g");
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        if shared {
            check(unsafe { libc::close(right) }, "close the receiving socket");
        }
        let (reused, reused_port) = bound_udp_socket();
        assert_eq!(
            reused_port,
            Ok(port),
            "shared={shared}: the receiving socket's last reference freed its queue"
        );
        unsafe {
            libc::close(reused);
            libc::close(left);
            libc::close(go_write);
        }
        println!("shared={shared}: port {port} free again");
    }
}

/// A message of data alone, then one carrying an eventfd holding 5, whose
/// sender keeps it; the data is read with `preadv2` at the current offset
/// (https://github.com/rrnewton/hermit/issues/4930, Codex's case). The
/// receipt joins the sender's eventfd, which reads 8 bytes holding 5, as on
/// Linux 7.1.3, through a datagram and through a stream socketpair.
fn data_read_with_preadv2_leaves_the_next_eventfd() {
    for ty in [libc::SOCK_DGRAM, libc::SOCK_STREAM] {
        data_read_with_preadv2_leaves_the_next_eventfd_on(ty);
    }
}

fn data_read_with_preadv2_leaves_the_next_eventfd_on(ty: c_int) {
    let (left, right) = socketpair(ty);
    let counter = eventfd(5, 0);
    write_all(left, b"d");
    send_fds(left, &[counter]);
    let mut byte = [0u8; 1];
    let iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let data = unsafe { libc::preadv2(right, &iov, 1, -1, 0) };
    assert_eq!(data, 1, "preadv2: {}", io::Error::last_os_error());
    let [received] = recv_fds(right, 0)[..] else {
        panic!("expected one descriptor");
    };
    assert_eq!(u64::from_ne_bytes(read_exact_once(received)), 5);
    println!("socket type {ty}: data=1; the received eventfd read 5");
}

#[test]
fn scm_rights_pipe_end_shares_the_open_file() {
    run_deterministically(received_pipe_end_shares_the_open_file);
}

#[test]
fn scm_rights_pipe_end_from_a_process_that_keeps_it() {
    run_deterministically(pipe_end_from_a_process_that_keeps_it_shares_the_open_file);
}

#[test]
fn scm_rights_over_a_connection_to_an_own_listener() {
    run_deterministically(descriptors_over_a_connection_to_an_own_listener_share_their_open_files);
}

#[test]
fn scm_rights_over_a_connection_from_a_child() {
    run_deterministically(descriptors_over_a_connection_from_a_child_share_their_open_files);
}

#[test]
fn scm_rights_connection_through_a_relative_path() {
    run_deterministically(a_connection_through_a_relative_path_does_not_shift_the_pairing);
}

#[test]
fn scm_rights_connection_from_a_chroot() {
    run_deterministically(a_connection_from_a_chroot_does_not_shift_the_pairing);
}

#[test]
fn scm_rights_addressed_datagram() {
    run_deterministically(an_addressed_datagram_does_not_take_the_peers_queue);
}

#[test]
fn scm_rights_reconnected_datagram_socket() {
    run_deterministically(a_reconnected_datagram_socket_does_not_take_the_peers_queue);
}

#[test]
fn scm_rights_eventfd_after_a_released_one() {
    run_deterministically(eventfd_after_a_released_one_does_not_join_it);
}

#[test]
fn scm_rights_recvmmsg_peek() {
    run_deterministically(recvmmsg_peek_names_the_first_message_for_every_entry);
}

#[test]
fn scm_rights_long_stream_message() {
    run_deterministically(descriptors_of_a_long_stream_message_come_with_its_first_byte);
}

#[test]
fn scm_rights_queue_follows_peek_truncation_and_discards() {
    run_deterministically(queued_rights_follow_peek_truncation_and_discards);
}

#[test]
fn scm_rights_eventfd_shares_the_counter() {
    run_deterministically(received_eventfd_shares_the_counter);
}

#[test]
fn scm_rights_blocking_read_waits_for_the_writer() {
    run_deterministically(blocking_read_on_a_received_pipe_waits_for_the_writer);
}

#[test]
fn scm_rights_random_device_from_a_child_process() {
    run_deterministically(random_device_from_a_child_reads_the_deterministic_stream);
}

#[test]
fn scm_rights_rendered_procfs_file_from_a_child_process() {
    run_deterministically(rendered_procfs_file_from_a_child_reads_as_the_file);
}

#[test]
fn scm_rights_through_recvmmsg() {
    run_deterministically(recvmmsg_registers_every_message);
}

#[test]
fn scm_rights_with_msg_cmsg_cloexec() {
    run_deterministically(msg_cmsg_cloexec_marks_received_descriptors);
}

#[test]
fn scm_rights_peek_and_empty_read_leave_the_message() {
    run_deterministically(a_peek_or_a_read_of_no_bytes_leaves_the_message);
}

#[test]
fn scm_rights_data_only_read_leaves_the_next_message() {
    run_deterministically(a_data_only_read_leaves_the_next_message);
}

#[test]
fn scm_rights_pipe_ends_arrive_in_order_around_a_peek() {
    run_deterministically(the_ends_of_a_pipe_arrive_in_order_around_a_peek);
}

#[test]
fn scm_rights_batch_truncation_and_discard_leave_the_later_message() {
    run_deterministically(a_batch_that_truncates_and_discards_leaves_the_later_message);
}

#[test]
fn scm_rights_efault_receive_does_not_leave_its_eventfd() {
    run_deterministically(an_efault_receive_does_not_leave_its_eventfd_to_the_next);
}

#[test]
fn scm_rights_preadv2_at_the_current_offset_takes_its_message() {
    run_deterministically(a_preadv2_at_the_current_offset_takes_its_message);
}

#[test]
fn scm_rights_socket_in_flight_keeps_its_port() {
    loopback();
    run_deterministically(a_socket_in_flight_keeps_its_port);
}

#[test]
fn scm_rights_freed_message_frees_its_sockets_port() {
    loopback();
    run_deterministically(a_freed_message_frees_its_sockets_port);
}

#[test]
fn scm_rights_plain_read_frees_the_discarded_sockets_port() {
    loopback();
    run_deterministically(a_plain_read_frees_the_port_of_the_socket_it_discards);
}

#[test]
fn scm_rights_receipt_naming_a_later_message_keeps_the_queue() {
    run_deterministically(a_receipt_that_names_a_later_message_keeps_the_queue);
}

#[test]
fn scm_rights_receive_failing_copying_out_took_its_message() {
    run_deterministically(a_receive_that_fails_copying_out_took_its_message);
}

#[test]
fn scm_rights_receive_failing_before_the_socket_takes_nothing() {
    run_deterministically(a_receive_that_fails_before_the_socket_takes_nothing);
}

#[test]
fn scm_rights_write_to_a_gone_peer_frees_the_queued_port() {
    loopback();
    run_deterministically(a_write_to_a_gone_peer_frees_the_queued_sockets_port);
}

#[test]
fn scm_rights_sendfile_through_a_socket_takes_nothing() {
    run_deterministically(a_sendfile_through_a_socket_takes_nothing);
}

#[test]
fn scm_rights_peek_past_the_first_message_names_it() {
    run_deterministically(a_peek_past_the_first_message_names_it);
}

#[test]
fn scm_rights_queue_of_an_exited_childs_socket_goes_with_it() {
    loopback();
    run_deterministically(the_queue_of_an_exited_childs_socket_goes_with_it);
}

#[test]
fn scm_rights_preadv2_data_read_leaves_the_next_eventfd() {
    run_deterministically(data_read_with_preadv2_leaves_the_next_eventfd);
}
