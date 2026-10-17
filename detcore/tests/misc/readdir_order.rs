/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Directory enumeration order must not depend on the host filesystem.
//!
//! Linux returns entries in filesystem order -- creation order on tmpfs, a
//! per-filesystem-seeded name hash on ext4 -- and glibc reads them 32KiB at a
//! time. Sorting each of those buffers separately still left a directory
//! larger than one buffer as sorted runs whose boundaries depend on the host.
//! These tests create directories far larger than one buffer, in different
//! creation orders, and require one global order, stable offsets, and the
//! POSIX `seekdir`/`rewinddir` behaviour built on them.

use std::collections::BTreeSet;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

const RUNS: usize = 5;

/// Far more than one 32KiB glibc buffer holds: each name below takes a 32-byte
/// `linux_dirent64` record, so 3000 entries are about 94KiB.
const ENTRIES: usize = 3000;

fn run_five_times(guest: fn()) {
    run_five_times_with(guest, true)
}

fn run_five_times_with(guest: fn(), sequentialize_threads: bool) {
    run_five_times_on(|| (), |()| guest(), sequentialize_threads)
}

/// Runs `guest` five times, each on a new value from `setup`. The value is made
/// outside the guest and dropped after its run.
fn run_five_times_on<S: Sync>(
    setup: impl Fn() -> S,
    guest: impl Fn(&S) + Sync,
    sequentialize_threads: bool,
) {
    let config = Config {
        sequentialize_threads,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    run_five_times_under(setup, guest, config)
}

/// Like [`run_five_times`], also hashing the bytes each syscall returns into
/// the log, as `hermit run` does whenever its log is read: under `--verify`,
/// or at log level info or above.
fn run_five_times_hashing_buffers(guest: fn()) {
    // Detcore reads a call's bytes back only while its records are observed:
    // by a tracing subscriber at INFO, or by a record sink. The subscriber's
    // level is the caller's DETCORE_TEST_RUST_LOG, which GitHub's portable
    // workflow sets to "off", so without a sink nothing is hashed there and
    // these tests check nothing (portable run 37383177918). A sink that drops
    // every record, for every target, makes the hashing independent of the
    // log level.
    _ = detcore::detlog::set_forwarder(|_, _, _, _| {}, detcore::detlog::ForwardPolicy::all());
    assert!(detcore::detlog::forwards_target("detcore::io_buffers"));
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        detlog_io_buffers: true,
        ..Default::default()
    };
    run_five_times_under(|| (), |()| guest(), config)
}

/// Like [`run_five_times`], without hashing the bytes each syscall returns
/// into the log.
fn run_five_times_without_hashing_buffers(guest: fn()) {
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        detlog_io_buffers: false,
        ..Default::default()
    };
    run_five_times_under(|| (), |()| guest(), config)
}

fn run_five_times_under<S: Sync>(setup: impl Fn() -> S, guest: impl Fn(&S) + Sync, config: Config) {
    let mut expected = None;

    for run in 1..=RUNS {
        let value = setup();
        let (output, _state) = detcore_testutils::test_fn_with_config::<Detcore, _>(
            || guest(&value),
            config.clone(),
            true,
        )
        .unwrap_or_else(|error| panic!("readdir guest run {run} failed: {error:#}"));
        assert_eq!(
            output.status,
            ExitStatus::Exited(0),
            "guest run {run} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "guest run {run} printed no listing digest"
        );
        if let Some(expected) = &expected {
            assert_eq!(
                &output.stdout, expected,
                "directory listing diverged on run {run}"
            );
        } else {
            expected = Some(output.stdout);
        }
    }
}

fn name(index: usize) -> String {
    format!("entry-{index:06}")
}

/// Every name, in the order `readdir` must return them after `.` and `..`.
fn sorted_names() -> Vec<String> {
    (0..ENTRIES).map(name).collect()
}

/// Create the directory's entries in `order`.
fn populate(dir: &Path, order: impl Iterator<Item = usize>) {
    for index in order {
        File::create(dir.join(name(index))).unwrap();
    }
}

/// A creation order that is neither sorted nor reverse-sorted: 1297 is prime
/// and does not divide `ENTRIES`, so this visits every index once.
fn scrambled() -> impl Iterator<Item = usize> {
    (0..ENTRIES).map(|step| (step * 1297) % ENTRIES)
}

struct Listing {
    names: Vec<String>,
    offsets: Vec<i64>,
    digest: u64,
}

/// Read a whole directory through glibc's `opendir`/`readdir`.
fn read_listing(dir: *mut libc::DIR) -> Listing {
    let mut listing = Listing {
        names: Vec::new(),
        offsets: Vec::new(),
        digest: 0xcbf2_9ce4_8422_2325,
    };
    loop {
        let entry = unsafe { libc::readdir64(dir) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        for byte in name
            .bytes()
            .chain(entry.d_ino.to_ne_bytes())
            .chain(entry.d_off.to_ne_bytes())
            .chain([entry.d_type])
        {
            listing.digest = (listing.digest ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
        listing.names.push(name);
        listing.offsets.push(entry.d_off);
    }
    listing
}

// Guest code: a guest's own directory reads are what this exercises.
#[allow(clippy::disallowed_methods)]
fn open_dir(path: &Path) -> *mut libc::DIR {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let dir = unsafe { libc::opendir(path.as_ptr()) };
    assert!(!dir.is_null(), "opendir failed");
    dir
}

fn assert_whole_directory_sorted(listing: &Listing, expected: &[String]) {
    assert_eq!(listing.names[0], ".");
    assert_eq!(listing.names[1], "..");
    assert!(
        listing.names[2..] == *expected,
        "directory not returned in one global order: {} entries, first mismatch at {:?}",
        listing.names.len() - 2,
        listing.names[2..]
            .iter()
            .zip(expected)
            .position(|(got, want)| got != want)
    );
    // Each d_off names the position after its entry, counting from 1.
    let offsets: Vec<i64> = (1..=listing.names.len() as i64).collect();
    assert_eq!(listing.offsets, offsets);
}

/// A new directory holding every name, created in [`scrambled`] order.
///
/// Tests pass this to [`run_five_times_on`] so that the entries are created
/// before each traced run and removed after it, not inside it. Creating them
/// in the guest cost 3000 traced `openat` and `close` pairs and 3000 traced
/// `unlinkat` calls in each of the five runs, none of which the tests examine,
/// and their kernel cost grows with host load: in validation, three such tests
/// reached 19 to 22 CPU seconds against a 22-second limit, while
/// `creation_order_does_not_change_enumeration`, which creates twice as many
/// files the same way outside the guest, peaked at 10.2.
fn scrambled_directory() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    root
}

/// Two directories holding the same names, created in different orders.
fn creation_order_directories() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let forward = root.path().join("forward");
    let shuffled = root.path().join("shuffled");
    std::fs::create_dir(&forward).unwrap();
    std::fs::create_dir(&shuffled).unwrap();
    populate(&forward, 0..ENTRIES);
    populate(&shuffled, scrambled());
    root
}

// Guest code: a guest's own directory reads are what this exercises.
#[allow(clippy::disallowed_methods)]
fn creation_order_does_not_change_enumeration_guest(root: &tempfile::TempDir) {
    let forward = root.path().join("forward");
    let shuffled = root.path().join("shuffled");
    let expected = sorted_names();

    let dir = open_dir(&forward);
    let forward_listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    assert_whole_directory_sorted(&forward_listing, &expected);

    let dir = open_dir(&shuffled);
    let shuffled_listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    assert_whole_directory_sorted(&shuffled_listing, &expected);

    // std's iterator is a second, independent reader over the same syscall.
    let std_names: Vec<String> = std::fs::read_dir(&shuffled)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(std_names, expected);

    println!(
        "forward {:016x} shuffled {:016x}",
        forward_listing.digest, shuffled_listing.digest
    );
}

#[test]
fn creation_order_does_not_change_enumeration() {
    // The host's order depends on the order the files were created in, not
    // on which process created them. Creating 6000 files inside each traced
    // run cost most of the test's CPU limit, so they are created outside it,
    // in new directories for each run, so that the host's inode numbers still
    // differ between the runs being compared.
    run_five_times_on(
        creation_order_directories,
        creation_order_does_not_change_enumeration_guest,
        true,
    );
}

fn seekdir_and_rewinddir_guest(root: &tempfile::TempDir) {
    let dir = open_dir(root.path());

    // Stop part way through, well past the first 32KiB buffer.
    let mut before = Vec::new();
    for _ in 0..2000 {
        let entry = unsafe { libc::readdir64(dir) };
        assert!(!entry.is_null());
        before.push(unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned());
    }
    let mark = unsafe { libc::telldir(dir) };
    let rest = read_listing(dir);
    assert_eq!(before.len() + rest.names.len(), ENTRIES + 2);

    unsafe { libc::seekdir(dir, mark) };
    let again = read_listing(dir);
    assert_eq!(again.names, rest.names, "seekdir did not return to telldir");
    assert_eq!(again.digest, rest.digest);

    // rewinddir refers the stream to the directory's current contents.
    File::create(root.path().join("added-after-opendir")).unwrap();
    std::fs::remove_file(root.path().join(name(0))).unwrap();
    unsafe { libc::rewinddir(dir) };
    let refreshed = read_listing(dir);
    unsafe { libc::closedir(dir) };
    let names: BTreeSet<&str> = refreshed.names.iter().map(String::as_str).collect();
    assert!(names.contains("added-after-opendir"));
    assert!(!names.contains(name(0).as_str()));
    assert_eq!(refreshed.names.len(), ENTRIES + 2);
    assert_eq!(refreshed.names[2], "added-after-opendir");

    println!(
        "rest {:016x} refreshed {:016x}",
        rest.digest, refreshed.digest
    );
}

#[test]
fn seekdir_and_rewinddir() {
    run_five_times_on(scrambled_directory, seekdir_and_rewinddir_guest, true);
}

fn getdents64(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    let n = unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            fd,
            buf.as_mut_ptr(),
            buf.len() as libc::c_uint,
        )
    };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    } else {
        Ok(n as usize)
    }
}

fn getdents_legacy(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    let n = unsafe {
        libc::syscall(
            libc::SYS_getdents,
            fd,
            buf.as_mut_ptr(),
            buf.len() as libc::c_uint,
        )
    };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    } else {
        Ok(n as usize)
    }
}

/// The names in a raw `getdents` buffer; the legacy layout keeps `d_type` in
/// the last byte of the record, after the name.
fn record_names(buf: &[u8], name_offset: usize) -> Vec<String> {
    let mut names = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let reclen = u16::from_ne_bytes([buf[at + 16], buf[at + 17]]) as usize;
        let name = CStr::from_bytes_until_nul(&buf[at + name_offset..at + reclen]).unwrap();
        names.push(name.to_str().unwrap().to_owned());
        at += reclen;
    }
    names
}

fn raw_getdents_guest(root: &tempfile::TempDir) {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    for (call, name_offset) in [
        (getdents64 as fn(i32, &mut [u8]) -> Result<usize, i32>, 19),
        (getdents_legacy, 18),
    ] {
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        // A buffer smaller than the first record is EINVAL.
        assert_eq!(call(fd, &mut [0; 16]), Err(libc::EINVAL));

        // Alternate the descriptor and its dup: they share one stream. Small
        // buffers force many calls, each continuing where the last stopped.
        let alias = unsafe { libc::dup(fd) };
        assert!(alias >= 0);
        let mut names = Vec::new();
        let mut buf = [0u8; 200];
        let mut turn = 0;
        loop {
            let n = call(if turn % 2 == 0 { fd } else { alias }, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            names.extend(record_names(&buf[..n], name_offset));
            turn += 1;
        }
        assert_eq!(names, expected);
        assert!(turn > 100, "only {turn} calls: the buffer was not small");

        // A position is an entry index, so seeking to 5 resumes at entry 5.
        assert_eq!(unsafe { libc::lseek(alias, 5, libc::SEEK_SET) }, 5);
        let n = call(fd, &mut buf).unwrap();
        assert_eq!(record_names(&buf[..n], name_offset)[0], expected[5]);
        unsafe { libc::close(alias) };
    }

    // A regular file is not a directory, and asking must not move its offset.
    let file_path = root.path().join(name(7));
    std::fs::write(&file_path, b"0123456789").unwrap();
    let file = File::open(&file_path).unwrap();
    let fd = file.as_raw_fd();
    assert_eq!(getdents64(fd, &mut [0; 4096]), Err(libc::ENOTDIR));
    assert_eq!(unsafe { libc::lseek(fd, 4, libc::SEEK_SET) }, 4);
    assert_eq!(getdents64(fd, &mut [0; 4096]), Err(libc::ENOTDIR));
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }, 4);

    println!("raw getdents ok");
}

#[test]
fn raw_getdents_share_one_sorted_stream() {
    run_five_times_on(scrambled_directory, raw_getdents_guest, true);
}

/// Every name in the directory, read with `getdents64` on `fd` until the end.
fn drain_names(fd: i32) -> Vec<String> {
    let mut names = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = getdents64(fd, &mut buf).unwrap_or_else(|errno| {
            panic!(
                "getdents64 failed: {}",
                std::io::Error::from_raw_os_error(errno)
            )
        });
        if n == 0 {
            return names;
        }
        names.extend(record_names(&buf[..n], 19));
    }
}

/// Send `fd` to this process over a Unix socket and return the descriptor
/// that arrives. Detcore does not track descriptors received this way.
fn receive_descriptor(fd: i32) -> i32 {
    /// Room for one `SCM_RIGHTS` message, aligned as a `cmsghdr` must be.
    #[repr(C, align(8))]
    struct Control([u8; 32]);

    fn message(byte: &mut u8, control: &mut Control) -> (libc::iovec, libc::msghdr) {
        let iov = libc::iovec {
            iov_base: (byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iovlen = 1;
        header.msg_control = control.0.as_mut_ptr().cast();
        header.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as usize;
        (iov, header)
    }

    let (sender, receiver) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut byte = b'x';
    let mut control = Control([0; 32]);
    let (mut iov, mut header) = message(&mut byte, &mut control);
    header.msg_iov = &mut iov;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(4) as usize;
        libc::CMSG_DATA(cmsg).cast::<i32>().write_unaligned(fd);
        assert_eq!(libc::sendmsg(sender.as_raw_fd(), &header, 0), 1);
    }

    let mut control = Control([0; 32]);
    let (mut iov, mut header) = message(&mut byte, &mut control);
    header.msg_iov = &mut iov;
    unsafe {
        assert_eq!(libc::recvmsg(receiver.as_raw_fd(), &mut header, 0), 1);
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        assert!(!cmsg.is_null(), "no descriptor received");
        assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
        libc::CMSG_DATA(cmsg).cast::<i32>().read_unaligned()
    }
}

fn received_descriptor_guest(root: &tempfile::TempDir) {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // A descriptor that arrives over a Unix socket has no open file
    // description in Detcore; it must still list the whole directory.
    let dir = File::open(root.path()).unwrap();
    let received = receive_descriptor(dir.as_raw_fd());
    drop(dir);

    let mut names = drain_names(received);
    assert_eq!(names.len(), ENTRIES + 2, "received descriptor lost entries");
    // Without a stream, each kernel buffer is sorted on its own, as on main,
    // so only the set of names is fixed. The length check above together with
    // set equality still rules out any repeated entry. The per-buffer order
    // is an older deviation of untracked descriptors, which an identity for
    // received descriptors removes
    // (https://github.com/rrnewton/hermit/issues/3387). No stream is shared
    // here, because `dir` is closed before any read, so the repeats of
    // https://github.com/rrnewton/hermit/issues/3722 cannot occur.
    names.sort();
    assert_eq!(names, expected);
    unsafe { libc::close(received) };

    println!("received descriptor ok");
}

/// Compares only the set of names, because an untracked descriptor still sorts
/// each buffer on its own (https://github.com/rrnewton/hermit/issues/3387);
/// see the guest.
#[test]
fn received_descriptor_lists_whole_directory() {
    run_five_times_on(scrambled_directory, received_descriptor_guest, true);
}

fn unsequentialized_threads_guest(root: &tempfile::TempDir) {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // Two threads read one open file description through two descriptors at
    // the same moment. Linux returns every entry exactly once in total.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let alias = unsafe { libc::dup(fd) };
    assert!(alias >= 0);
    let start = std::sync::Barrier::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            drain_names(fd)
        });
        let second = scope.spawn(|| {
            start.wait();
            drain_names(alias)
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    unsafe { libc::close(alias) };

    let mut names = first;
    names.extend(second);
    assert_eq!(
        names.len(),
        ENTRIES + 2,
        "concurrent readers lost or repeated entries"
    );
    names.sort();
    assert_eq!(names, expected);

    println!("concurrent readers ok");
}

#[test]
fn unsequentialized_threads_share_one_stream() {
    run_five_times_on(scrambled_directory, unsequentialized_threads_guest, false);
}

/// The name and `d_off` of each record in a raw `getdents64` buffer.
fn records(buf: &[u8]) -> Vec<(String, i64)> {
    let mut records = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let off = i64::from_ne_bytes(buf[at + 8..at + 16].try_into().unwrap());
        let reclen = u16::from_ne_bytes([buf[at + 16], buf[at + 17]]) as usize;
        let name = CStr::from_bytes_until_nul(&buf[at + 19..at + reclen]).unwrap();
        records.push((name.to_str().unwrap().to_owned(), off));
        at += reclen;
    }
    records
}

fn buffer_tail_guest() {
    // Names of many lengths, so a buffer of sorted records and a buffer of
    // host-ordered records fill to different lengths.
    let root = tempfile::tempdir().unwrap();
    for index in 0..300 {
        let name = format!("n{}{index:03}", "x".repeat(index * 7 % 40));
        File::create(root.path().join(name)).unwrap();
    }

    // Linux writes only the records it returns. Reading the host directory
    // must not leave anything after them, whatever the host order.
    for capacity in (150..=450).step_by(10) {
        let dir = File::open(root.path()).unwrap();
        let mut buf = vec![0xaa_u8; capacity + 64];
        let n = getdents64(dir.as_raw_fd(), &mut buf[..capacity]).unwrap();
        assert!(n > 0);
        assert!(
            buf[n..].iter().all(|&byte| byte == 0xaa),
            "a {capacity}-byte getdents64 returned {n} bytes and changed bytes after them"
        );
    }

    println!("buffer tail ok");
}

#[test]
fn buffer_tail_left_untouched() {
    run_five_times(buffer_tail_guest);
}

fn small_buffer_guest() {
    let root = tempfile::tempdir().unwrap();
    File::create(root.path().join("a-longer-filename")).unwrap();

    // A buffer too small for the first entry is EINVAL.
    let dir = File::open(root.path()).unwrap();
    assert_eq!(getdents64(dir.as_raw_fd(), &mut [0; 16]), Err(libc::EINVAL));

    // A 24-byte buffer holds `.` but not the 40-byte record of the long name;
    // Linux still returns the entries that fit, one call at a time.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut buf = [0u8; 24];
    let n = getdents64(fd, &mut buf).unwrap();
    assert_eq!(record_names(&buf[..n], 19), ["."]);
    let mut names = vec![".".to_owned()];
    names.extend(drain_names(fd));
    names.sort();
    assert_eq!(names, [".", "..", "a-longer-filename"]);

    println!("small buffer ok");
}

#[test]
fn small_buffer_returns_entries_that_fit() {
    run_five_times(small_buffer_guest);
}

fn seek_before_first_read_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..5 {
        File::create(root.path().join(name(index))).unwrap();
    }

    // Learn a real host position: a received descriptor has no stream, so
    // its records carry the kernel's own `d_off` cookies. `.` comes first on
    // Linux filesystems, and its cookie is the position after it.
    let probe = File::open(root.path()).unwrap();
    let received = receive_descriptor(probe.as_raw_fd());
    drop(probe);
    let mut buf = [0u8; 4096];
    let n = getdents64(received, &mut buf).unwrap();
    unsafe { libc::close(received) };
    let cookie = records(&buf[..n])
        .into_iter()
        .find_map(|(name, off)| (name == ".").then_some(off))
        .expect("no `.` entry");
    assert_ne!(cookie, 0);

    // Seeking a fresh descriptor there before its first read resumes after
    // `.`, as on Linux, rather than restarting the directory.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    assert_eq!(unsafe { libc::lseek(fd, cookie, libc::SEEK_SET) }, cookie);
    let mut names = drain_names(fd);
    names.sort();
    let mut expected = vec!["..".to_owned()];
    expected.extend((0..5).map(name));
    assert_eq!(names, expected);

    println!("seek before first read ok");
}

#[test]
fn seek_before_first_read_is_kept() {
    run_five_times(seek_before_first_read_guest);
}

fn rewind_after_host_order_guest(root: &tempfile::TempDir) {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // A descriptor seeked before its first read is read in host order (see
    // `seek_before_first_read_is_kept`). Seeking it back to 0 starts the
    // directory over, and the sorted stream with it.
    let probe = File::open(root.path()).unwrap();
    let received = receive_descriptor(probe.as_raw_fd());
    drop(probe);
    let mut buf = [0u8; 4096];
    let n = getdents64(received, &mut buf).unwrap();
    unsafe { libc::close(received) };
    let cookie = records(&buf[..n])
        .into_iter()
        .find_map(|(name, off)| (name == ".").then_some(off))
        .expect("no `.` entry");
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    assert_eq!(unsafe { libc::lseek(fd, cookie, libc::SEEK_SET) }, cookie);
    assert!(getdents64(fd, &mut buf).unwrap() > 0);
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(drain_names(fd), expected, "after seeking a host position");

    // A buffer with room for only the first entry gets it from the sorted
    // stream, and a rewind starts the stream over.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut small = [0u8; 24];
    let n = getdents64(fd, &mut small).unwrap();
    assert_eq!(record_names(&small[..n], 19), ["."]);
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(drain_names(fd), expected, "after a small buffer");

    println!("rewind after host order ok");
}

#[test]
fn rewind_after_host_order_sorts_again() {
    run_five_times_on(scrambled_directory, rewind_after_host_order_guest, true);
}

fn passed_on_descriptor_guest(root: &tempfile::TempDir) {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // Read part of the directory, then hand the open file to a descriptor
    // Detcore does not track. On Linux it reads the entries not yet returned;
    // it may also repeat some here, because the host order differs, but it
    // must not miss any. The repeats are a known regression, not the spec:
    // https://github.com/rrnewton/hermit/issues/3722.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut buf = [0u8; 512];
    let n = getdents64(fd, &mut buf).unwrap();
    let first = record_names(&buf[..n], 19);
    assert!(
        first.len() < expected.len(),
        "the first read was not partial"
    );
    let received = receive_descriptor(fd);
    let passed_on: BTreeSet<String> = drain_names(received).into_iter().collect();
    let missing: Vec<&String> = expected[first.len()..]
        .iter()
        .filter(|name| !passed_on.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "the passed-on descriptor missed {} entries not yet returned, first {:?}",
        missing.len(),
        missing.first()
    );

    // The stream itself continues where it stopped. This is NOT Linux
    // behaviour: the two descriptors share one position, so after the
    // passed-on descriptor read to the end, Linux returns nothing here. The
    // stream does not see the alias's reads and returns those entries again,
    // the second mechanism of https://github.com/rrnewton/hermit/issues/3722.
    // Flip this assertion when that issue is fixed.
    let mut names = first;
    names.extend(drain_names(fd));
    assert_eq!(names, expected);

    // At the end of the stream, the passed-on descriptor is at the end too.
    let mut buf = [0u8; 4096];
    assert_eq!(getdents64(received, &mut buf), Ok(0));

    // A rewind restarts both.
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(drain_names(received).len(), expected.len());
    unsafe { libc::close(received) };

    println!("passed-on descriptor ok");
}

/// Tolerates repeated entries on an untracked descriptor, a known regression
/// tracked in https://github.com/rrnewton/hermit/issues/3722; see the guest.
#[test]
fn passed_on_descriptor_misses_no_entry() {
    run_five_times_on(scrambled_directory, passed_on_descriptor_guest, true);
}

fn partly_mapped_buffer_guest(root: &tempfile::TempDir) {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // A buffer whose first page is mapped and whose next two are not. Linux
    // returns the records that fit in the mapped page.
    let page = 4096;
    let buf = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            3 * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(buf, libc::MAP_FAILED);
    assert_eq!(
        unsafe { libc::munmap(buf.cast::<u8>().add(page).cast(), 2 * page) },
        0
    );

    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf, 3 * page as libc::c_uint) };
    assert!(
        n > 0,
        "getdents64 into a partly mapped buffer failed: {}",
        std::io::Error::last_os_error()
    );
    let n = n as usize;
    assert!(n <= page, "{n} bytes returned from a {page}-byte mapping");
    let first = record_names(
        unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), n) },
        19,
    );
    unsafe { libc::munmap(buf, page) };

    let mut names = first;
    names.extend(drain_names(fd));
    assert_eq!(names, expected);

    println!("partly mapped buffer ok");
}

#[test]
fn partly_mapped_buffer_returns_entries_that_fit() {
    run_five_times_on(scrambled_directory, partly_mapped_buffer_guest, true);
}

fn long_name_past_writable_end_guest() {
    let root = tempfile::tempdir().unwrap();
    // A 200-byte name takes a 224-byte record.
    let long = "y".repeat(200);
    File::create(root.path().join(&long)).unwrap();

    // A 256-byte buffer of which only the first 64 bytes are mapped: `.` and
    // `..` fit, the long name does not. Linux returns the two that fit, and
    // then fails the call that reaches the long name.
    let page = 4096;
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            2 * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(map, libc::MAP_FAILED);
    assert_eq!(
        unsafe { libc::munmap(map.cast::<u8>().add(page).cast(), page) },
        0
    );
    let buf = unsafe { std::slice::from_raw_parts_mut(map.cast::<u8>().add(page - 64), 64) };
    buf.fill(0xaa);

    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let getdents = || unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            fd,
            map.cast::<u8>().add(page - 64),
            256,
        )
    };
    let n = getdents();
    assert_eq!(n, 48, "{}", std::io::Error::last_os_error());
    assert_eq!(record_names(&buf[..48], 19), [".", ".."]);
    assert!(
        buf[48..].iter().all(|&byte| byte == 0xaa),
        "bytes after the returned records changed: {:02x?}",
        &buf[48..]
    );
    assert_eq!(getdents(), -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EFAULT)
    );
    unsafe { libc::munmap(map, page) };
    assert_eq!(drain_names(fd), [long]);

    println!("long name past writable end ok");
}

#[test]
fn long_name_past_writable_end_returns_entries_before_it() {
    run_five_times(long_name_past_writable_end_guest);
}

fn seek_during_first_read_guest(root: &tempfile::TempDir) {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // One thread makes the first getdents64 call on a fresh descriptor, which
    // reads the whole host directory before it returns, while another
    // rewinds the same open file up to 100 times, stopping early when that
    // call returns. A rewind before the call has no effect and a rewind after
    // it restarts the stream; neither may land in the middle of reading the
    // host directory. (Unbounded rewinds landing there would keep the read
    // from ever reaching the end, so a broken lock would hang, not fail.)
    for trial in 0..20 {
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        let start = std::sync::Barrier::new(2);
        let done = std::sync::atomic::AtomicBool::new(false);
        let first = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                start.wait();
                let mut buf = [0u8; 512];
                let n = getdents64(fd, &mut buf).unwrap();
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                record_names(&buf[..n], 19)
            });
            scope.spawn(|| {
                start.wait();
                for _ in 0..100 {
                    if done.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
                }
            });
            reader.join().unwrap()
        });
        assert!(
            first == expected[..first.len()],
            "trial {trial}: the first call returned {first:?}, not the start of the sorted directory"
        );
        let rest = drain_names(fd);
        let mut continued = first;
        continued.extend(rest.iter().cloned());
        assert!(
            rest == expected || continued == expected,
            "trial {trial}: {} entries after the first {} did not continue or restart the directory",
            rest.len(),
            continued.len() - rest.len()
        );
    }

    println!("seek during first read ok");
}

#[test]
fn seek_during_first_read_leaves_stream_whole() {
    run_five_times_on(scrambled_directory, seek_during_first_read_guest, false);
}

/// A directory holding `count` names from `name`, created in both orders:
/// `forward` in sorted order and `reverse` in reverse sorted order.
fn both_orders(count: usize) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (order, indices) in [
        ("forward", (0..count).collect::<Vec<_>>()),
        ("reverse", (0..count).rev().collect()),
    ] {
        let dir = root.path().join(order);
        std::fs::create_dir(&dir).unwrap();
        populate(&dir, indices.into_iter());
    }
    root
}

/// `.`, `..` and then the first `count` names from `name`.
fn listing_of(count: usize) -> Vec<String> {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..count).map(name));
    expected
}

/// Panic unless `names` contains every name in `expected` that is not in
/// `returned`. Extra names, including repeats of returned ones, pass: Linux
/// never repeats an entry, so this tolerates a known regression rather than
/// stating the spec (https://github.com/rrnewton/hermit/issues/3722).
fn assert_contains_rest(names: &[String], expected: &[String], returned: &[String], context: &str) {
    let names: BTreeSet<&String> = names.iter().collect();
    let wanted: Vec<&String> = expected
        .iter()
        .filter(|name| !returned.contains(name))
        .collect();
    let missing: Vec<&&String> = wanted
        .iter()
        .filter(|name| !names.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "{context}: {} of {} entries missing, first {:?}",
        missing.len(),
        wanted.len(),
        missing.first()
    );
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn alias_after_rewinds_guest() {
    let root = both_orders(300);
    let expected = listing_of(300);

    for order in ["forward", "reverse"] {
        let path = root.path().join(order);

        // Rewinding the stream twice, with the passed-on descriptor reading
        // the whole directory in between, leaves the open file at the start
        // each time: the passed-on descriptor then reads every entry again.
        let dir = File::open(&path).unwrap();
        let fd = dir.as_raw_fd();
        let mut buf = [0u8; 512];
        let n = getdents64(fd, &mut buf).unwrap();
        assert!(record_names(&buf[..n], 19).len() < expected.len());
        let received = receive_descriptor(fd);
        assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
        assert_eq!(
            sorted(drain_names(received)),
            sorted(expected.clone()),
            "{order}: after the first rewind"
        );
        assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
        drop(dir);
        assert_eq!(
            sorted(drain_names(received)),
            sorted(expected.clone()),
            "{order}: after the second rewind"
        );
        unsafe { libc::close(received) };

        // Seeking the stream back to where it already was, after the
        // passed-on descriptor read to the end, leaves the open file where
        // every entry not yet returned follows.
        let dir = File::open(&path).unwrap();
        let fd = dir.as_raw_fd();
        let n = getdents64(fd, &mut buf).unwrap();
        let first = record_names(&buf[..n], 19);
        let received = receive_descriptor(fd);
        let position = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
        assert!(position > 0);
        assert_contains_rest(
            &drain_names(received),
            &expected,
            &first,
            &format!("{order}: after seeking to the current position"),
        );
        assert_eq!(
            unsafe { libc::lseek(fd, position, libc::SEEK_SET) },
            position
        );
        drop(dir);
        assert_contains_rest(
            &drain_names(received),
            &expected,
            &first,
            &format!("{order}: after seeking back to {position}"),
        );
        unsafe { libc::close(received) };
    }

    println!("alias after rewinds ok");
}

/// Tolerates repeated entries on an untracked descriptor, a known regression
/// tracked in https://github.com/rrnewton/hermit/issues/3722; see
/// `assert_contains_rest`.
#[test]
fn passed_on_descriptor_follows_repeated_seeks() {
    run_five_times(alias_after_rewinds_guest);
}

/// A mapping of `pages` pages filled with 0xaa, of which the first `writable`
/// stay readable and writable and the rest get `protection`.
fn guarded_pages(pages: usize, writable: usize, protection: i32) -> *mut u8 {
    let page = 4096;
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(map, libc::MAP_FAILED);
    let map = map.cast::<u8>();
    unsafe { std::ptr::write_bytes(map, 0xaa, pages * page) };
    assert_eq!(
        unsafe {
            libc::mprotect(
                map.add(writable * page).cast(),
                (pages - writable) * page,
                protection,
            )
        },
        0
    );
    map
}

fn raw_getdents64(fd: i32, buf: *mut u8, count: libc::c_uint) -> Result<usize, i32> {
    let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf, count) };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    } else {
        Ok(n as usize)
    }
}

/// The `room` bytes Linux leaves where it began to store `record`, a
/// `getdents64` record that it could not finish, into a buffer that held
/// `0xaa`, for a `call` of `SYS_getdents64` or `SYS_getdents`. The two
/// formats' records of a name have the same length. `filldir64`
/// (fs/readdir.c) stores the record's `d_ino`, `d_reclen` and `d_type`, each
/// at once, so only if it fits whole, then the NUL after the name, then the
/// name. `filldir` stores `d_type` in the record's last byte, so if it fits,
/// so does the whole record. Neither stores the record's own `d_off` before
/// the next record or the end of the call.
fn left_by_linux(record: &[u8], room: usize, call: libc::c_long) -> Vec<u8> {
    let name_len = record[19..].iter().position(|&byte| byte == 0).unwrap();
    let reclen = usize::from(u16::from_ne_bytes([record[16], record[17]]));
    let type_at = if call == libc::SYS_getdents {
        assert!(room < reclen, "the record fits in {room} bytes");
        reclen - 1
    } else {
        assert!(
            19 + name_len >= room,
            "the record's NUL fits in {room} bytes"
        );
        18
    };
    let mut left = vec![0xaa; room];
    for (at, field) in [(0, 0..8), (16, 16..18), (type_at, 18..19)] {
        if at + field.len() <= room {
            left[at..at + field.len()].copy_from_slice(&record[field]);
        }
    }
    left
}

fn first_record_short_of_writable_end_guest() {
    // Two-character names take 24-byte records, of which Linux writes the
    // first 22: it does not write the padding after the name.
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // With 22 bytes writable before a read-only page, `.` still fits
    // (it needs 21), and the call returns its whole 24-byte record without
    // touching its padding or the two read-only bytes. With 20, nothing fits
    // and the call fails. Linux first stores the entry's position, 0, into
    // `.`'s `d_off`, then its `d_ino`, `d_reclen` and `d_type`, and fails at
    // the NUL after the name, byte 20, leaving the first 19 bytes written.
    // Either way a descriptor the open file is passed on to then finds every
    // entry not yet returned; without one, the stream continues. (After the
    // passed-on descriptor reads to the end, Linux gives the first descriptor
    // nothing more, so each is checked on its own open file.)
    for (writable, returned) in [(22, Ok(24)), (20, Err(libc::EFAULT))] {
        for pass_on in [true, false] {
            let context = format!("{writable} bytes writable, passed on {pass_on}");
            let map = guarded_pages(2, 1, libc::PROT_READ);
            let buf = unsafe { map.add(page - writable) };
            let dir = File::open(root.path()).unwrap();
            let fd = dir.as_raw_fd();
            let result = raw_getdents64(fd, buf, 256);
            assert_eq!(result, returned, "{context}");
            let bytes = unsafe { std::slice::from_raw_parts(buf, 24) };
            let first = match result {
                Ok(n) => record_names(&bytes[..n], 19),
                Err(_) => Vec::new(),
            };
            assert_eq!(first, expected[..first.len()], "{context}");
            assert_eq!(&bytes[21..], &[0xaa; 24][21..], "{context}");
            if result.is_err() {
                // `.`'s `d_ino`, as a descriptor of its own reads it.
                let mut dot = [0u8; 4096];
                let own = File::open(root.path()).unwrap();
                getdents64(own.as_raw_fd(), &mut dot).unwrap();
                assert_eq!(bytes[..8], dot[..8], "{context}");
                assert_eq!(&bytes[8..16], &0i64.to_ne_bytes(), "{context}");
                assert_eq!(&bytes[16..18], &24u16.to_ne_bytes(), "{context}");
                assert_eq!(bytes[18], libc::DT_DIR, "{context}");
                assert_eq!(bytes[19..21], [0xaa; 2], "{context}");
            }
            if pass_on {
                let received = receive_descriptor(fd);
                assert_contains_rest(&drain_names(received), &expected, &first, &context);
                unsafe { libc::close(received) };
            } else {
                let mut next = [0u8; 4096];
                let len = getdents64(fd, &mut next).unwrap();
                if result.is_err() {
                    // `.` comes first again, with the `d_ino` stored before.
                    assert_eq!(bytes[..8], next[..8], "{context}");
                }
                let mut names = first;
                names.extend(record_names(&next[..len], 19));
                names.extend(drain_names(fd));
                assert_eq!(names, expected, "{context}");
            }
            unsafe { libc::munmap(map.cast(), 2 * page) };
        }
    }

    println!("first record short of writable end ok");
}

#[test]
fn first_record_short_of_writable_end_leaves_stream_whole() {
    run_five_times(first_record_short_of_writable_end_guest);
}

fn record_into_inaccessible_page_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // 88 bytes before a page the guest cannot touch: three 24-byte records
    // fit, and the fourth would end 8 bytes inside that page. Linux returns
    // the three and never writes the inaccessible page. It does store the
    // fourth record's `d_ino` before failing on its length, so the 16 bytes
    // after the three records hold the `d_ino` of the entry the next call
    // returns first. The first call reads a fresh descriptor, the second
    // continues the stream.
    let map = guarded_pages(2, 1, libc::PROT_NONE);
    let buf = unsafe { map.add(page - 88) };
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut names = Vec::new();
    let mut left: Option<Vec<u8>> = None;
    for call in 0..2 {
        let n = raw_getdents64(fd, buf, 256).unwrap();
        assert_eq!(n, 72, "call {call}");
        let bytes = unsafe { std::slice::from_raw_parts(buf, 88) };
        names.extend(record_names(&bytes[..n], 19));
        if let Some(left) = left {
            assert_eq!(
                left,
                left_by_linux(&bytes[..24], 16, libc::SYS_getdents64),
                "call {call}"
            );
        }
        left = Some(bytes[n..].to_vec());
        assert_eq!(
            unsafe {
                libc::mprotect(
                    map.add(page).cast(),
                    page,
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            },
            0
        );
        let guard = unsafe { std::slice::from_raw_parts(map.add(page), page) };
        assert!(
            guard.iter().all(|&byte| byte == 0xaa),
            "call {call} wrote the inaccessible page"
        );
        assert_eq!(
            unsafe { libc::mprotect(map.add(page).cast(), page, libc::PROT_NONE) },
            0
        );
        unsafe { std::ptr::write_bytes(buf, 0xaa, 88) };
    }
    let mut next = [0u8; 4096];
    let len = getdents64(fd, &mut next).unwrap();
    assert_eq!(
        left.unwrap(),
        left_by_linux(&next[..24], 16, libc::SYS_getdents64),
        "after call 1"
    );
    names.extend(record_names(&next[..len], 19));
    names.extend(drain_names(fd));
    assert_eq!(names, expected);
    unsafe { libc::munmap(map.cast(), 2 * page) };

    println!("record into inaccessible page ok");
}

#[test]
fn record_into_inaccessible_page_is_not_returned() {
    run_five_times(record_into_inaccessible_page_guest);
}

fn large_buffer_tail_guest(root: &tempfile::TempDir) {
    let expected = listing_of(ENTRIES);
    let page = 4096;

    // A count of 100000 whose first 70001 bytes are writable, above the
    // 64KiB Detcore reads the host directory with. The records, `.` and
    // `..` at 24 bytes and the rest at 32, fill exactly 70000 bytes on a
    // fresh descriptor, and fewer after a first read of 4096. After them,
    // Linux leaves what it stored of the record that does not fit (see
    // [`left_by_linux`]): on a fresh descriptor nothing, because its `d_ino`
    // crosses into the inaccessible page and is stored at once.
    let writable: usize = 70001;
    let pages = (writable + 100000) / page + 2;
    let usable = writable.div_ceil(page);
    for warm in [false, true] {
        let map = guarded_pages(pages, usable, libc::PROT_NONE);
        let buf = unsafe { map.add(usable * page - writable) };
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        let mut names = Vec::new();
        if warm {
            let mut first = [0u8; 4096];
            let n = getdents64(fd, &mut first).unwrap();
            names = record_names(&first[..n], 19);
        }
        let n = raw_getdents64(fd, buf, 100000).unwrap();
        let bytes = unsafe { std::slice::from_raw_parts(buf, writable) };
        if !warm {
            assert_eq!(n, 70000);
        }
        assert!(n > 60000 && n <= writable, "{n} bytes returned");
        names.extend(record_names(&bytes[..n], 19));
        let mut next = [0u8; 4096];
        let len = getdents64(fd, &mut next).unwrap();
        assert_eq!(
            bytes[n..],
            left_by_linux(&next[..32], writable - n, libc::SYS_getdents64)[..],
            "warm {warm}: {n} bytes returned, and the bytes after them"
        );
        if !warm {
            assert!(bytes[n..].iter().all(|&byte| byte == 0xaa));
        }
        names.extend(record_names(&next[..len], 19));
        names.extend(drain_names(fd));
        assert_eq!(names, expected, "warm {warm}");
        unsafe { libc::munmap(map.cast(), pages * page) };
    }

    println!("large buffer tail ok");
}

#[test]
fn large_buffer_tail_left_untouched() {
    run_five_times_on(scrambled_directory, large_buffer_tail_guest, true);
}

fn count_above_int_max_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);

    // Linux keeps the count in an `int`, where these are negative: no entry
    // fits, so the call fails with EINVAL while entries remain and returns 0
    // at the end, and moves nothing. The buffer is on the heap, far enough
    // below the top of user memory for the whole range to be valid.
    let mut buf = vec![0xaa_u8; 4096];
    for count in [0x8000_0000_u32, 0xffff_ffff] {
        for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
            let getdents = |fd: i32, buf: &mut [u8], count: u32| {
                let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), count) };
                if n < 0 {
                    Err(std::io::Error::last_os_error().raw_os_error().unwrap())
                } else {
                    Ok(n as usize)
                }
            };
            let context = format!("syscall {call}, count {count:#x}");
            let dir = File::open(root.path()).unwrap();
            let fd = dir.as_raw_fd();
            // Fresh.
            assert_eq!(
                getdents(fd, &mut buf, count),
                Err(libc::EINVAL),
                "{context}"
            );
            assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
            // After part of the directory.
            let n = getdents(fd, &mut buf[..200], 200).unwrap();
            let mut names = record_names(&buf[..n], name_offset);
            buf.fill(0xaa);
            assert_eq!(
                getdents(fd, &mut buf, count),
                Err(libc::EINVAL),
                "{context}"
            );
            assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
            loop {
                let n = getdents(fd, &mut buf, 4096).unwrap();
                if n == 0 {
                    break;
                }
                names.extend(record_names(&buf[..n], name_offset));
            }
            assert_eq!(names, expected, "{context}");
            // At the end.
            buf.fill(0xaa);
            assert_eq!(getdents(fd, &mut buf, count), Ok(0), "{context}");
            assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
        }
    }

    println!("count above int max ok");
}

#[test]
fn count_above_int_max_matches_linux() {
    run_five_times(count_above_int_max_guest);
}

fn padding_in_inaccessible_page_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // Four 24-byte records take 96 bytes, but Linux writes only the first 22
    // of the fourth: it does not write the padding after the name. With 94 or
    // 95 bytes before a page the guest cannot touch, all four fit and the
    // call returns 96 without touching that page, and hashing the returned
    // bytes into the log must not fail on the two it cannot read. The page is
    // either inaccessible or not mapped at all; ptrace can read the first
    // kind. On a fresh descriptor the records start at `.`; after reading `.`
    // alone they start at `..`. A received descriptor, which Detcore does not
    // track, gets each buffer sorted on its own instead.
    let states = [(false, false), (false, true), (true, false), (true, true)];
    for unmapped in [false, true] {
        for writable in [94, 95] {
            for (warm, received) in states {
                let context = format!(
                    "{writable} bytes writable, unmapped {unmapped}, warm {warm}, received {received}"
                );
                let map = guarded_pages(2, 1, libc::PROT_NONE);
                let guard = unsafe { map.add(page) };
                if unmapped {
                    assert_eq!(unsafe { libc::munmap(guard.cast(), page) }, 0);
                }
                let buf = unsafe { guard.sub(writable) };
                let dir = File::open(root.path()).unwrap();
                let fd = if received {
                    receive_descriptor(dir.as_raw_fd())
                } else {
                    dir.as_raw_fd()
                };
                let mut names = Vec::new();
                if warm {
                    let mut first = [0u8; 24];
                    assert_eq!(getdents64(fd, &mut first), Ok(24), "{context}");
                    names = record_names(&first, 19);
                }
                let returned = raw_getdents64(fd, buf, 256);
                assert_eq!(returned, Ok(96), "{context}");
                let mut bytes = [0u8; 96];
                bytes[..writable]
                    .copy_from_slice(unsafe { std::slice::from_raw_parts(buf, writable) });
                let batch = record_names(&bytes, 19);
                if received {
                    assert!(batch.is_sorted(), "{context}: {batch:?}");
                } else {
                    assert_eq!(batch, expected[names.len()..names.len() + 4], "{context}");
                }
                names.extend(batch);
                if unmapped {
                    // Still nothing there to write.
                    assert_eq!(
                        unsafe { libc::msync(guard.cast(), page, libc::MS_ASYNC) },
                        -1,
                        "{context}"
                    );
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ENOMEM),
                        "{context}"
                    );
                } else {
                    assert_eq!(
                        unsafe {
                            libc::mprotect(guard.cast(), page, libc::PROT_READ | libc::PROT_WRITE)
                        },
                        0
                    );
                    let guard = unsafe { std::slice::from_raw_parts(guard, page) };
                    assert!(
                        guard.iter().all(|&byte| byte == 0xaa),
                        "{context}: the inaccessible page was written"
                    );
                }
                names.extend(drain_names(fd));
                if received {
                    assert_eq!(sorted(names), expected, "{context}");
                    unsafe { libc::close(fd) };
                } else {
                    assert_eq!(names, expected, "{context}");
                }
                unsafe { libc::munmap(map.cast(), 2 * page) };
            }
        }
    }

    println!("padding in inaccessible page ok");
}

#[test]
fn padding_in_inaccessible_page_is_returned() {
    run_five_times_hashing_buffers(padding_in_inaccessible_page_guest);
}

fn write_only_prefix_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // A page the guest can write but not read, then one it cannot touch.
    // After `.` and `..`, each record is 24 bytes. With 8 bytes before the
    // inaccessible page no record fits and the call fails, writing nothing:
    // Linux's first write to the first record of a call is its `d_off`, 8
    // bytes in. With 32, `00` fits and `01` does not. Linux stores `01`'s
    // `d_ino` and fails at its `d_reclen`, so the 8 bytes after `00` hold
    // the `d_ino` of `01` (see [`left_by_linux`]). With 40, `01`'s own
    // `d_off` is still not written. With 45, `getdents64` also stores `01`'s
    // `d_reclen` and `d_type` and fails at the NUL after its name, while
    // `getdents` fails at `d_type`, in the record's last byte, after
    // `d_reclen`. The stream continues after the returned records.
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        for (writable, returned) in [
            (8, Err(libc::EFAULT)),
            (32, Ok(24)),
            (40, Ok(24)),
            (45, Ok(24)),
        ] {
            let context = format!("syscall {call}, {writable} bytes writable");
            let map = guarded_pages(2, 1, libc::PROT_NONE);
            assert_eq!(
                unsafe { libc::mprotect(map.cast(), page, libc::PROT_WRITE) },
                0
            );
            let buf = unsafe { map.add(page - writable) };
            let dir = File::open(root.path()).unwrap();
            let fd = dir.as_raw_fd();
            let mut first = [0u8; 48];
            assert_eq!(getdents64(fd, &mut first), Ok(48), "{context}");
            let mut names = record_names(&first, 19);
            let result = unsafe { libc::syscall(call, fd, buf, 256) };
            let result = if result < 0 {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap())
            } else {
                Ok(result as usize)
            };
            assert_eq!(result, returned, "{context}");
            assert_eq!(
                unsafe { libc::mprotect(map.cast(), 2 * page, libc::PROT_READ | libc::PROT_WRITE) },
                0
            );
            let bytes = unsafe { std::slice::from_raw_parts(buf, writable) };
            let n = result.unwrap_or(0);
            names.extend(record_names(&bytes[..n], name_offset));
            let mut next = [0u8; 4096];
            let len = getdents64(fd, &mut next).unwrap();
            let left = if n > 0 {
                left_by_linux(&next[..24], writable - n, call)
            } else {
                vec![0xaa; writable]
            };
            assert_eq!(
                bytes[n..],
                left[..],
                "{context}: {n} bytes returned, and the bytes after them"
            );
            let guard = unsafe { std::slice::from_raw_parts(map.add(page), page) };
            assert!(
                guard.iter().all(|&byte| byte == 0xaa),
                "{context}: the inaccessible page was written"
            );
            names.extend(record_names(&next[..len], 19));
            names.extend(drain_names(fd));
            assert_eq!(names, expected, "{context}");
            unsafe { libc::munmap(map.cast(), 2 * page) };
        }
    }

    println!("write-only prefix ok");
}

/// The write-only tests run without hashing the bytes each syscall returns
/// into the log; `write_only_buffer_is_filled_while_hashing_buffers` covers a
/// write-only buffer with that hashing.
#[test]
fn write_only_prefix_matches_linux() {
    run_five_times_without_hashing_buffers(write_only_prefix_guest);
}

/// Rewind, seek, then call `getdents` and `getdents64` with each of `counts`,
/// none of which any entry fits.
fn count_after_rewind_and_seek(counts: &[u64]) {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);

    // A rewind drops what Detcore knows of the stream, and a seek after it
    // sets a position the host directory has not been read to. A count too
    // small for any entry must still fail with EINVAL while entries remain
    // after that position and return 0 at or past the end, moving nothing
    // and writing nothing; the next call continues from the position. (On a
    // descriptor never read, a position is a host cookie, as on Linux, so
    // each descriptor first lists the whole directory.)
    let mut buf = vec![0xaa_u8; 4096];
    for &count in counts {
        for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
            let getdents = |fd: i32, buf: &mut [u8], count: u64| {
                let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), count) };
                if n < 0 {
                    Err(std::io::Error::last_os_error().raw_os_error().unwrap())
                } else {
                    Ok(n as usize)
                }
            };
            for position in [5_i64, 22, 1_000_000] {
                let context = format!("syscall {call}, count {count:#x}, position {position}");
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                while getdents(fd, &mut buf, 4096).unwrap() > 0 {}
                assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
                assert_eq!(
                    unsafe { libc::lseek(fd, position, libc::SEEK_SET) },
                    position,
                    "{context}"
                );
                buf.fill(0xaa);
                let rest = &expected[(position as usize).min(expected.len())..];
                let answer = if rest.is_empty() {
                    Ok(0)
                } else {
                    Err(libc::EINVAL)
                };
                assert_eq!(getdents(fd, &mut buf, count), answer, "{context}");
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                assert_eq!(
                    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) },
                    position,
                    "{context}"
                );
                let mut names = Vec::new();
                loop {
                    let n = getdents(fd, &mut buf, 4096).unwrap();
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n], name_offset));
                }
                assert_eq!(names, rest, "{context}");
            }
        }
    }

    println!("count after rewind and seek ok");
}

fn count_above_int_max_after_rewind_guest() {
    // Negative as an `int`.
    count_after_rewind_and_seek(&[0x8000_0000, 0xffff_ffff]);
}

#[test]
fn count_above_int_max_after_rewind_matches_linux() {
    run_five_times(count_above_int_max_after_rewind_guest);
}

fn count_too_small_after_rewind_guest() {
    // Smaller than the 24 bytes of the shortest record. The count is an
    // `unsigned int`, so the last is 0.
    count_after_rewind_and_seek(&[0, 1, 10, 23, 1 << 32]);
}

#[test]
fn count_too_small_after_rewind_matches_linux() {
    run_five_times(count_too_small_after_rewind_guest);
}

/// Make `call` with `rsp` at `stack` instead of on this thread's stack.
fn syscall_on_stack(
    call: libc::c_long,
    fd: i32,
    buf: *mut u8,
    count: u64,
    stack: *mut u8,
) -> Result<usize, i32> {
    let result: i64;
    unsafe {
        std::arch::asm!(
            "mov {saved}, rsp",
            "mov rsp, {stack}",
            "syscall",
            "mov rsp, {saved}",
            stack = in(reg) stack,
            saved = out(reg) _,
            inlateout("rax") call => result,
            in("rdi") fd as i64,
            in("rsi") buf,
            in("rdx") count,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if result < 0 {
        Err(-result as i32)
    } else {
        Ok(result as usize)
    }
}

fn negative_count_stack_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let page = 4096;

    // After a rewind and a seek, Detcore must read the host directory to
    // answer a count that is negative as an `int`. Linux writes nothing to
    // answer it, so neither may Detcore: not to the buffer, and not below
    // the stack pointer, where the guest could later read what the host
    // returned. The stack is 256 bytes above a page the guest cannot touch,
    // or far from it.
    let pages = 16;
    let map = guarded_pages(pages, pages, libc::PROT_NONE);
    assert_eq!(
        unsafe { libc::mprotect(map.cast(), page, libc::PROT_NONE) },
        0
    );
    let stack = unsafe { std::slice::from_raw_parts(map.add(page), (pages - 1) * page) };
    let mut buf = vec![0xaa_u8; 4096];
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        for above in [256, 12 * page] {
            for position in [5_i64, 22] {
                let context = format!(
                    "syscall {call}, stack {above} bytes above the guard, position {position}"
                );
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                while getdents64(fd, &mut buf).unwrap() > 0 {}
                assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
                assert_eq!(
                    unsafe { libc::lseek(fd, position, libc::SEEK_SET) },
                    position
                );
                buf.fill(0xaa);
                let answer = if position < 22 {
                    Err(libc::EINVAL)
                } else {
                    Ok(0)
                };
                let result = syscall_on_stack(call, fd, buf.as_mut_ptr(), 0x8000_0000, unsafe {
                    map.add(page + above)
                });
                assert_eq!(result, answer, "{context}");
                assert!(
                    buf.iter().all(|&byte| byte == 0xaa),
                    "{context}: the buffer was written"
                );
                let changed = stack.iter().filter(|&&byte| byte != 0xaa).count();
                assert_eq!(
                    changed, 0,
                    "{context}: {changed} bytes of the stack were written"
                );
                assert_eq!(
                    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) },
                    position,
                    "{context}"
                );
                let mut names = Vec::new();
                loop {
                    let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), 4096) };
                    assert!(n >= 0, "{context}");
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n as usize], name_offset));
                }
                assert_eq!(names, listing_of(20)[position as usize..], "{context}");
            }
        }
    }
    unsafe { libc::munmap(map.cast(), pages * page) };

    println!("negative count stack ok");
}

#[test]
fn negative_count_writes_nothing_below_the_stack_pointer() {
    run_five_times(negative_count_stack_guest);
}

fn record_before_writable_page_guest() {
    let page = 4096;

    // The buffer starts 16 bytes before a writable page, in a page the guest
    // cannot write. The record for `..` crosses into the writable page, but
    // Linux's first write to it, its `d_off`, fails, so the call fails and
    // writes nothing; the stream stays at `..`. Both with entries after `..`
    // and with `..` the last entry, in an empty directory.
    for count in [20, 0] {
        let root = tempfile::tempdir().unwrap();
        for index in 0..count {
            File::create(root.path().join(name(index))).unwrap();
        }
        let expected = listing_of(count);
        for protection in [libc::PROT_NONE, libc::PROT_READ] {
            for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
                let context =
                    format!("{count} files, syscall {call}, first page protection {protection}");
                let map = guarded_pages(2, 2, libc::PROT_NONE);
                assert_eq!(unsafe { libc::mprotect(map.cast(), page, protection) }, 0);
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                let mut first = [0u8; 24];
                assert_eq!(getdents64(fd, &mut first), Ok(24), "{context}");
                let mut names = record_names(&first, 19);
                let result = unsafe { libc::syscall(call, fd, map.add(page - 16), 256) };
                assert_eq!(result, -1, "{context}");
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EFAULT),
                    "{context}"
                );
                assert_eq!(
                    unsafe { libc::mprotect(map.cast(), page, libc::PROT_READ | libc::PROT_WRITE) },
                    0
                );
                let bytes = unsafe { std::slice::from_raw_parts(map, 2 * page) };
                let changed = bytes.iter().filter(|&&byte| byte != 0xaa).count();
                assert_eq!(changed, 0, "{context}: {changed} bytes were written");
                let mut buf = [0u8; 4096];
                loop {
                    let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), 4096) };
                    assert!(n >= 0, "{context}");
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n as usize], name_offset));
                }
                assert_eq!(names, expected, "{context}");
                unsafe { libc::munmap(map.cast(), 2 * page) };
            }
        }
    }

    println!("record before writable page ok");
}

#[test]
fn record_starting_in_unwritable_page_leaves_next_page_untouched() {
    run_five_times(record_before_writable_page_guest);
}

fn store_crossing_into_write_only_page_guest() {
    let page = 4096;
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);

    // A page the guest cannot write, then a write-only page; the buffer starts
    // `before` bytes before the second page. Linux's first store is the first
    // record's placeholder `d_off`, 8 bytes in, then its `d_ino` at the start,
    // each one store, which writes nothing if any byte of it faults. 4 bytes
    // before, the `d_off` lands in the second page, holding 0, the position of
    // `.`, and the `d_ino` faults: the call fails with only the `d_off`
    // stored. 12 bytes before, the `d_off` itself faults and nothing is
    // stored. A copy that writes a crossing store's second-page part first
    // must put that part back, though no vectored copy can read a write-only
    // page.
    for protection in [libc::PROT_NONE, libc::PROT_READ] {
        for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
            for (before, stored) in [(4usize, Some(4..12)), (12, None)] {
                let context = format!(
                    "syscall {call}, first page protection {protection}, \
                     {before} bytes before the second page"
                );
                let map = guarded_pages(2, 2, libc::PROT_NONE);
                assert_eq!(unsafe { libc::mprotect(map.cast(), page, protection) }, 0);
                assert_eq!(
                    unsafe { libc::mprotect(map.add(page).cast(), page, libc::PROT_WRITE) },
                    0
                );
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                let result = unsafe { libc::syscall(call, fd, map.add(page - before), 256) };
                assert_eq!(result, -1, "{context}");
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EFAULT),
                    "{context}"
                );
                assert_eq!(
                    unsafe {
                        libc::mprotect(map.cast(), 2 * page, libc::PROT_READ | libc::PROT_WRITE)
                    },
                    0
                );
                let bytes = unsafe { std::slice::from_raw_parts(map, 2 * page) };
                let mut left = vec![0xaa; 2 * page];
                if let Some(stored) = stored {
                    left[page + stored.start..page + stored.end]
                        .copy_from_slice(&0i64.to_ne_bytes());
                }
                let changed: Vec<usize> =
                    (0..2 * page).filter(|&at| bytes[at] != left[at]).collect();
                assert!(
                    changed.is_empty(),
                    "{context}: bytes {changed:?} differ from what Linux leaves"
                );
                let mut names = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), 4096) };
                    assert!(n >= 0, "{context}");
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n as usize], name_offset));
                }
                assert_eq!(names, expected, "{context}");
                unsafe { libc::munmap(map.cast(), 2 * page) };
            }
        }
    }

    println!("store crossing into write-only page ok");
}

#[test]
fn store_crossing_into_write_only_page_leaves_what_linux_leaves() {
    run_five_times_without_hashing_buffers(store_crossing_into_write_only_page_guest);
}

fn fresh_write_only_buffer_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);
    let page = 4096;

    // The first call on a descriptor, into a buffer the guest can write but
    // not read. Linux fills it; Detcore must not need to read it back.
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        let context = format!("syscall {call}");
        let map = guarded_pages(1, 0, libc::PROT_WRITE);
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        let result = unsafe { libc::syscall(call, fd, map, 256) };
        // `.`, `..` and six 32-byte records.
        assert_eq!(result, 240, "{context}");
        assert_eq!(
            unsafe { libc::mprotect(map.cast(), page, libc::PROT_READ | libc::PROT_WRITE) },
            0
        );
        let bytes = unsafe { std::slice::from_raw_parts(map, page) };
        let mut names = record_names(&bytes[..240], name_offset);
        assert!(bytes[240..].iter().all(|&byte| byte == 0xaa), "{context}");
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), 4096) };
            assert!(n >= 0, "{context}");
            if n == 0 {
                break;
            }
            names.extend(record_names(&buf[..n as usize], name_offset));
        }
        assert_eq!(names, expected, "{context}");
        unsafe { libc::munmap(map.cast(), page) };
    }

    println!("fresh write-only buffer ok");
}

#[test]
fn fresh_write_only_buffer_is_filled() {
    run_five_times_without_hashing_buffers(fresh_write_only_buffer_guest);
}

/// With the bytes each syscall returns hashed into the log, as `hermit run`
/// does under `--verify`, Detcore reads them back from a buffer the guest can
/// write but not read one aligned word at a time, so the call still returns
/// what Linux returns (https://github.com/rrnewton/hermit/issues/3823).
#[test]
fn write_only_buffer_is_filled_while_hashing_buffers() {
    run_five_times_hashing_buffers(fresh_write_only_buffer_guest);
}

/// `pages` pages, each mapping one of `views` (protection, offset into a
/// fresh `memfd` of two pages), and any page after them inaccessible. Views
/// of the same offset are the same memory.
fn aliased_pages(pages: usize, views: &[(i32, usize)]) -> *mut u8 {
    let page = 4096;
    let backing = unsafe { libc::memfd_create(c"readdir-order".as_ptr(), 0) };
    assert!(backing >= 0);
    assert_eq!(
        unsafe { libc::ftruncate(backing, 2 * page as libc::off_t) },
        0
    );
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * page,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(map, libc::MAP_FAILED);
    let map = map.cast::<u8>();
    for (index, &(protection, offset)) in views.iter().enumerate() {
        let at = unsafe { map.add(index * page) };
        let view = unsafe {
            libc::mmap(
                at.cast(),
                page,
                protection,
                libc::MAP_SHARED | libc::MAP_FIXED,
                backing,
                offset as libc::off_t,
            )
        };
        assert_eq!(view, at.cast());
    }
    unsafe { libc::close(backing) };
    map
}

fn aliased_padding() {
    let page = 4096;

    // After `.` and `..`, `a` takes the first 24 bytes of the buffer and
    // each nine-byte name `bNNNXuvYZ` a 32-byte record, so the record of
    // `b127` starts 4088 bytes in, and the sixth and seventh bytes of its
    // name 4112 bytes in. The buffer's first page can be read; its second
    // is a write-only view of the same memory, so those two bytes land on
    // `a`'s `d_reclen`. In `b127`'s name they are the length of the whole
    // call, 0x1738 or 0x2538 bytes, so `a`'s record claims every byte after
    // it: in the second layout, too, whose third page is write-only other
    // memory. (Both lengths leave the names valid UTF-8.) Linux does
    // not look at what it wrote, and returns every record. Hashing the
    // returned bytes reads the write-only pages too, one aligned word at a
    // time, so it hashes the bytes the guest holds and never takes the rest
    // for padding after `a`'s name; the call returns what Linux returns,
    // with or without hashing.
    for (pages, names, second_view) in [(2, 185, None), (3, 297, Some(page))] {
        let count = 24 + names * 32;
        let context = format!("{pages} pages");
        let root = tempfile::tempdir().unwrap();
        File::create(root.path().join("a")).unwrap();
        for index in 0..names {
            let mut name = format!("b{index:03}XuvYZ").into_bytes();
            if index == 127 {
                name[5..7].copy_from_slice(&(count as u16).to_ne_bytes());
            }
            File::create(root.path().join(std::ffi::OsStr::from_bytes(&name))).unwrap();
        }
        let mut views = vec![
            (libc::PROT_READ | libc::PROT_WRITE, 0),
            (libc::PROT_WRITE, 0),
        ];
        views.extend(second_view.map(|offset| (libc::PROT_WRITE, offset)));
        let map = aliased_pages(pages, &views);
        unsafe { std::ptr::write_bytes(map, 0xaa, page) };
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        let mut warm = [0u8; 48];
        assert_eq!(getdents64(fd, &mut warm), Ok(48), "{context}");
        let result = raw_getdents64(fd, map, count as libc::c_uint);
        assert_eq!(result, Ok(count), "{context}");
        let bytes = unsafe { std::slice::from_raw_parts(map, page) };
        assert_eq!(
            bytes[16..18],
            (count as u16).to_ne_bytes(),
            "{context}: `a`'s record length"
        );
        // The last byte of `b127`'s name and the NUL after it.
        assert_eq!(bytes[19..21], [b'Z', 0], "{context}");
        assert_eq!(drain_names(fd), Vec::<String>::new(), "{context}");
        unsafe { libc::munmap(map.cast(), pages * page) };
    }

    println!("aliased padding ok");
}

#[test]
fn aliased_padding_is_returned_while_hashing_buffers() {
    run_five_times_hashing_buffers(aliased_padding);
}

#[test]
fn aliased_padding_is_returned_without_hashing_buffers() {
    run_five_times_without_hashing_buffers(aliased_padding);
}

fn aliased_record_not_copied_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..300 {
        File::create(root.path().join(format!("c{index:03}xyz"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..300).map(|index| format!("c{index:03}xyz")));
    let page = 4096;

    // Two readable views of the same memory, then an inaccessible page, and
    // a buffer 28 bytes in. After `.` and `..`, each seven-byte name takes a
    // 32-byte record, so 255 records fill 8160 bytes. A count of 8192 has
    // room for a 256th, whose `d_ino` Linux stores at once, starting 8188
    // bytes in: four bytes before the inaccessible page, and in the same
    // memory as the first half of the 128th record's `d_ino`. That store
    // faults, so the call returns the same 255 records as with a count of
    // 8160, and leaves every byte the same, the 128th record's `d_ino`
    // included, although writing the other records has since changed the
    // bytes of that memory.
    for call in [libc::SYS_getdents64, libc::SYS_getdents] {
        let mut left = Vec::new();
        for count in [8160, 8192] {
            let context = format!("syscall {call}, count {count}");
            let rw = libc::PROT_READ | libc::PROT_WRITE;
            let map = aliased_pages(3, &[(rw, 0), (rw, 0)]);
            unsafe { map.add(page + 17).write_volatile(0x5a) };
            assert_eq!(
                unsafe { map.add(17).read_volatile() },
                0x5a,
                "{context}: the views are not the same memory"
            );
            unsafe { std::ptr::write_bytes(map, 0xaa, page) };
            let dir = File::open(root.path()).unwrap();
            let fd = dir.as_raw_fd();
            let mut warm = [0u8; 48];
            assert_eq!(getdents64(fd, &mut warm), Ok(48), "{context}");
            let result = unsafe { libc::syscall(call, fd, map.add(28), count) };
            assert_eq!(result, 8160, "{context}");
            left.push(unsafe { std::slice::from_raw_parts(map, 2 * page) }.to_vec());
            assert_eq!(drain_names(fd), expected[2 + 255..], "{context}");
            unsafe { libc::munmap(map.cast(), 3 * page) };
        }
        assert!(
            left[0] == left[1],
            "syscall {call}: a store that faulted changed the bytes Linux left"
        );
    }

    println!("aliased record not copied ok");
}

#[test]
fn aliased_record_not_copied_is_left_as_linux_leaves_it() {
    run_five_times_hashing_buffers(aliased_record_not_copied_guest);
}

/// Map every free range of the address space, largest first, so that Detcore
/// can map nothing in the guest to read a directory into. Nothing may
/// allocate until the ranges are unmapped again.
fn occupy_address_space() -> Vec<(*mut libc::c_void, usize)> {
    let mut maps = Vec::with_capacity(4096);
    let mut len = 1usize << 46;
    while len >= 4096 {
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            len /= 2;
        } else {
            assert!(maps.len() < maps.capacity());
            maps.push((map, len));
        }
    }
    maps
}

/// Unmap the smallest of `maps`, returning its length. That leaves room for
/// less than the 64KiB Detcore would map, but for the one page it maps
/// instead, which the whole directory is read through.
fn free_smallest(maps: &mut Vec<(*mut libc::c_void, usize)>) -> usize {
    let smallest = (0..maps.len()).min_by_key(|&index| maps[index].1).unwrap();
    let (map, freed) = maps.swap_remove(smallest);
    unsafe { libc::munmap(map, freed) };
    freed
}

fn release(maps: Vec<(*mut libc::c_void, usize)>) {
    for &(map, len) in &maps {
        unsafe { libc::munmap(map, len) };
    }
}

fn address_space_full_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut buf = vec![0u8; 4096];

    // With the address space full, a count that is negative as an `int`
    // still gets Linux's answer on a stream never read, which needs no
    // snapshot: EINVAL, as entries remain. The first read then fails with
    // `ENOMEM` and reads nothing.
    let mut maps = occupy_address_space();
    let negative = raw_getdents64(fd, buf.as_mut_ptr(), 0x8000_0000);
    let full = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    let freed = free_smallest(&mut maps);
    let first = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    release(maps);

    assert_eq!(negative, Err(libc::EINVAL));
    assert_eq!(full, Err(libc::ENOMEM));
    assert!(freed < 64 * 1024, "{freed} bytes freed");
    let n = first.unwrap();
    let mut names = record_names(&buf[..n], 19);
    names.extend(drain_names(fd));
    assert_eq!(names, expected);

    // Rewound and then seeked past the start, the stream needs a snapshot
    // again, to know how many entries the directory has even for a negative
    // count. With none possible, both calls fail with `ENOMEM`, writing and
    // moving nothing; with a page free, the stream goes on from where it was
    // seeked to.
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(unsafe { libc::lseek(fd, 3, libc::SEEK_SET) }, 3);
    buf.fill(0xaa);
    let mut maps = occupy_address_space();
    let negative = raw_getdents64(fd, buf.as_mut_ptr(), 0x8000_0000);
    let full = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    let position = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
    let untouched = buf.iter().all(|&byte| byte == 0xaa);
    free_smallest(&mut maps);
    let first = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    release(maps);

    assert_eq!(negative, Err(libc::ENOMEM));
    assert_eq!(full, Err(libc::ENOMEM));
    assert_eq!(position, 3);
    assert!(untouched);
    let n = first.unwrap();
    let mut names = record_names(&buf[..n], 19);
    names.extend(drain_names(fd));
    assert_eq!(names, expected[3..]);

    println!("address space full ok");
}

/// Detcore reads the whole directory into a mapping it makes in the guest
/// before returning the first entries. When no mapping can be made, the call
/// fails with `ENOMEM` (Linux needs no memory here) and leaves the stream
/// where it was, rather than return entries in the host's order; a later
/// call with a page free lists the directory in order. So does a count that
/// is negative as an `int` on a stream that needs a snapshot to answer it.
#[test]
fn full_address_space_fails_without_losing_order() {
    run_five_times(address_space_full_guest);
}

fn negative_count_untracked_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }

    // A descriptor Detcore does not track, and one it reads in host order
    // because it was seeked to a host position before its first read: a
    // count that is negative as an `int` fails with EINVAL while entries
    // remain and returns 0 at the end, writing nothing and moving nothing.
    let mut buf = vec![0xaa_u8; 4096];
    let getdents = |call: libc::c_long, fd: i32, buf: &mut [u8], count: u64| {
        let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), count) };
        if n < 0 {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap())
        } else {
            Ok(n as usize)
        }
    };
    for call in [libc::SYS_getdents64, libc::SYS_getdents] {
        for host_order in [false, true] {
            let context = format!("syscall {call}, host order {host_order}");
            let probe = File::open(root.path()).unwrap();
            let fd = receive_descriptor(probe.as_raw_fd());
            drop(probe);
            if host_order {
                let n = getdents64(fd, &mut buf).unwrap();
                let cookie = records(&buf[..n])
                    .into_iter()
                    .find_map(|(name, off)| (name == ".").then_some(off))
                    .expect("no `.` entry");
                unsafe { libc::close(fd) };
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                assert_eq!(unsafe { libc::lseek(fd, cookie, libc::SEEK_SET) }, cookie);
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0x8000_0000),
                    Err(libc::EINVAL),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                assert_eq!(
                    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) },
                    cookie,
                    "{context}"
                );
                while getdents(call, fd, &mut buf, 4096).unwrap() > 0 {}
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0xffff_ffff),
                    Ok(0),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
            } else {
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0x8000_0000),
                    Err(libc::EINVAL),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                // Every entry still follows, so the position is still 0.
                // (Asked with `lseek`, Hermit answers EBADF on the ptrace,
                // KVM and LiteInst backends, as for every call on a
                // descriptor received through `SCM_RIGHTS`: Detcore does not
                // track it.)
                let name_offset = if call == libc::SYS_getdents64 { 19 } else { 18 };
                let mut names = Vec::new();
                loop {
                    let n = getdents(call, fd, &mut buf, 4096).unwrap();
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n], name_offset));
                }
                let mut expected: Vec<String> = (0..20).map(name).collect();
                expected.extend([".".to_owned(), "..".to_owned()]);
                assert_eq!(sorted(names), sorted(expected), "{context}");
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0xffff_ffff),
                    Ok(0),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                unsafe { libc::close(fd) };
            }
        }
    }

    // A descriptor Detcore tracks, rewound to 0 after it was read to the
    // end, while a descriptor it does not track, for the same open file,
    // leaves the kernel's position at the end. Linux keeps one position for
    // both, so there nothing follows and a negative count returns 0.
    // Detcore keeps the tracked stream's own position, from which every
    // entry follows. Either way the answer must agree with what follows:
    // EINVAL while entries remain, 0 at the end.
    let mut all: Vec<String> = (0..20).map(name).collect();
    all.extend([".".to_owned(), "..".to_owned()]);
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        let context = format!("syscall {call}, rewound with an untracked alias at the end");
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        while getdents(call, fd, &mut buf, 4096).unwrap() > 0 {}
        assert_eq!(
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) },
            0,
            "{context}"
        );
        let alias = receive_descriptor(fd);
        while getdents(call, alias, &mut buf, 4096).unwrap() > 0 {}
        unsafe { libc::close(alias) };
        buf.fill(0xaa);
        let answer = getdents(call, fd, &mut buf, 0x8000_0000);
        assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
        let mut names = Vec::new();
        loop {
            let n = getdents(call, fd, &mut buf, 4096).unwrap();
            if n == 0 {
                break;
            }
            names.extend(record_names(&buf[..n], name_offset));
        }
        let followed = names.len();
        let expected = if names.is_empty() {
            Ok(0)
        } else {
            assert_eq!(sorted(names), sorted(all.clone()), "{context}");
            Err(libc::EINVAL)
        };
        assert_eq!(answer, expected, "{context}: {followed} entries followed");
    }

    println!("negative count untracked ok");
}

#[test]
fn negative_count_on_untracked_and_host_order_descriptors() {
    run_five_times(negative_count_untracked_guest);
}

/// Whether a host inode is new to Detcore depends on host state the guest does
/// not control: a file another process replaced, an inode the filesystem
/// reused, or, as here, a hard link. Even runs stat two names that are one
/// host file and odd runs two names that are two host files, then list a
/// third directory. Only the two stat'd names may tell the runs apart; the
/// listing's `d_ino` values must not
/// (https://github.com/rrnewton/hermit/issues/2897, where one replaced file
/// renumbered every entry of a later `/sys` listing).
fn host_dependent_inode_sighting_guest(root: &tempfile::TempDir) {
    std::fs::metadata(root.path().join("a")).unwrap();
    std::fs::metadata(root.path().join("b")).unwrap();
    let dir = open_dir(&root.path().join("listed"));
    let listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    assert_eq!(listing.names, [".", "..", "x", "y", "z"]);
    println!("listed {:016x}", listing.digest);
}

#[test]
fn host_dependent_inode_sighting_does_not_renumber_a_later_listing() {
    let runs = std::sync::atomic::AtomicUsize::new(0);
    run_five_times_on(
        || {
            let root = tempfile::tempdir().unwrap();
            File::create(root.path().join("a")).unwrap();
            if runs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .is_multiple_of(2)
            {
                std::fs::hard_link(root.path().join("a"), root.path().join("b")).unwrap();
            } else {
                File::create(root.path().join("b")).unwrap();
            }
            let listed = root.path().join("listed");
            std::fs::create_dir(&listed).unwrap();
            for name in ["x", "y", "z"] {
                File::create(listed.join(name)).unwrap();
            }
            root
        },
        host_dependent_inode_sighting_guest,
        true,
    );
}

/// A `/proc/self/maps` read numbers the file behind each file-backed line.
/// Even runs map two names that are one host file (as a hard-linked library in
/// an optimised Nix store is), odd runs two names that are two host files.
/// Each run fstats and maps both, reads its maps, and then lists a third
/// directory. The fstats number both files first, so the maps read mints
/// nothing in either case. The listing's `d_ino` values must not tell the runs
/// apart: how many numbering requests a maps read makes must follow from the
/// guest's mappings, not from whether two of them are one host file.
fn mapped_hard_link_guest(root: &tempfile::TempDir) {
    let mut mappings = Vec::new();
    for name in ["a", "b"] {
        let file = File::open(root.path().join(name)).unwrap();
        file.metadata().unwrap();
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        mappings.push(addr);
    }
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    assert!(maps.contains(&*root.path().join("a").to_string_lossy()));
    let dir = open_dir(&root.path().join("listed"));
    let listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    for addr in mappings {
        assert_eq!(unsafe { libc::munmap(addr, 4096) }, 0);
    }
    assert_eq!(listing.names, [".", "..", "x", "y", "z"]);
    println!("listed {:016x}", listing.digest);
}

#[test]
fn mapped_hard_link_does_not_renumber_a_later_listing() {
    let runs = std::sync::atomic::AtomicUsize::new(0);
    run_five_times_on(
        || {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("a"), [7u8; 4096]).unwrap();
            if runs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .is_multiple_of(2)
            {
                std::fs::hard_link(root.path().join("a"), root.path().join("b")).unwrap();
            } else {
                std::fs::write(root.path().join("b"), [7u8; 4096]).unwrap();
            }
            let listed = root.path().join("listed");
            std::fs::create_dir(&listed).unwrap();
            for name in ["x", "y", "z"] {
                File::create(listed.join(name)).unwrap();
            }
            root
        },
        mapped_hard_link_guest,
        true,
    );
}

/// The test process's stdin, replaced by a pipe for one run and restored when
/// the run's value is dropped. Detcore takes the guest's stdio identities from
/// that descriptor.
struct StdinAliasSetup {
    root: tempfile::TempDir,
    /// The read end of a pipe the guest inherits above fd 2.
    held: std::os::fd::OwnedFd,
    /// The pipe now on fd 0: `held`'s own pipe on even runs, another on odd.
    _stdin_pipe: Vec<std::os::fd::OwnedFd>,
    saved_stdin: std::os::fd::OwnedFd,
    /// The test process, which holds `held` outside the guest.
    test_pid: u32,
}

impl Drop for StdinAliasSetup {
    fn drop(&mut self) {
        assert_eq!(
            unsafe { libc::dup2(self.saved_stdin.as_raw_fd(), libc::STDIN_FILENO) },
            libc::STDIN_FILENO
        );
    }
}

fn host_pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    }
}

/// A readlink of another guest process's `pipe:[N]` descriptor numbers that pipe,
/// and renders the fixed stdio number instead when the pipe is the reader's
/// inherited stdin. Even runs give the guest a descriptor that is its stdin's
/// pipe, odd runs one that is another pipe. The guest fstats it (numbering it
/// in both cases), forks a child that keeps it, reads the child's link, and
/// then lists a third directory. The listing's `d_ino` values must not tell
/// the runs apart: whether the link's pipe is the reader's stdin must not
/// decide how many numbering requests the readlink makes.
fn stdin_alias_readlink_guest(setup: &StdinAliasSetup) {
    // Detcore's descriptor table admits only fds 0-2 from the tool, so the
    // guest opens the pipe through the test process's procfs entry to get a
    // descriptor it tracks.
    let reopened = File::open(format!(
        "/proc/{}/fd/{}",
        setup.test_pid,
        setup.held.as_raw_fd()
    ))
    .unwrap();
    let held = reopened.as_raw_fd();
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    assert_eq!(unsafe { libc::fstat(held, &mut stat) }, 0);
    let mut sync = [0; 2];
    assert_eq!(unsafe { libc::pipe(sync.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe {
            libc::close(sync[1]);
            let mut byte = 0_u8;
            let n = libc::read(sync[0], (&raw mut byte).cast(), 1);
            libc::_exit(if n == 0 { 0 } else { 1 });
        }
    }
    unsafe { libc::close(sync[0]) };
    let link = std::fs::read_link(format!("/proc/{child}/fd/{held}")).unwrap();
    assert!(
        link.as_os_str().as_bytes().starts_with(b"pipe:["),
        "unexpected link {link:?}"
    );
    unsafe { libc::close(sync[1]) };
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    let dir = open_dir(&setup.root.path().join("listed"));
    let listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    assert_eq!(listing.names, [".", "..", "x", "y", "z"]);
    drop(reopened);
    println!("listed {:016x}", listing.digest);
}

#[test]
fn stdin_alias_does_not_renumber_a_later_listing() {
    let runs = std::sync::atomic::AtomicUsize::new(0);
    run_five_times_on(
        || {
            let root = tempfile::tempdir().unwrap();
            let listed = root.path().join("listed");
            std::fs::create_dir(&listed).unwrap();
            for name in ["x", "y", "z"] {
                File::create(listed.join(name)).unwrap();
            }
            let saved_stdin = std::io::stdin().as_fd().try_clone_to_owned().unwrap();
            let (held, held_write) = host_pipe();
            let stdin_pipe = if runs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .is_multiple_of(2)
            {
                assert_eq!(
                    unsafe { libc::dup2(held.as_raw_fd(), libc::STDIN_FILENO) },
                    libc::STDIN_FILENO
                );
                vec![held_write]
            } else {
                let (other, other_write) = host_pipe();
                assert_eq!(
                    unsafe { libc::dup2(other.as_raw_fd(), libc::STDIN_FILENO) },
                    libc::STDIN_FILENO
                );
                vec![held_write, other, other_write]
            };
            StdinAliasSetup {
                root,
                held,
                _stdin_pipe: stdin_pipe,
                saved_stdin,
                test_pid: std::process::id(),
            }
        },
        stdin_alias_readlink_guest,
        true,
    );
}

/// A fresh `memfd` holding one page. A mapping of a memfd names the same
/// device in `/proc/self/maps` as `fstat` reports, unlike a file on btrfs or
/// overlayfs, so Detcore's stdio identities match its maps line on every host.
fn one_page_memfd(name: &CStr) -> File {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    assert!(fd >= 0);
    assert_eq!(unsafe { libc::ftruncate(fd, 4096) }, 0);
    unsafe { File::from_raw_fd(fd) }
}

/// The test process's stdin, replaced by a memfd for one run and restored
/// when the run's value is dropped. Detcore takes the guest's stdio identities
/// from that descriptor.
struct StdinMemfdSetup {
    root: tempfile::TempDir,
    /// The memfd the guest maps, held by the test process above fd 2.
    mapped: File,
    /// The memfd now on fd 0: `mapped` on even runs, another on odd.
    _stdin: File,
    saved_stdin: std::os::fd::OwnedFd,
    /// The test process, which holds `mapped` outside the guest.
    test_pid: u32,
}

impl Drop for StdinMemfdSetup {
    fn drop(&mut self) {
        assert_eq!(
            unsafe { libc::dup2(self.saved_stdin.as_raw_fd(), libc::STDIN_FILENO) },
            libc::STDIN_FILENO
        );
    }
}

/// Put a memfd on the test process's fd 0: `mapped` itself on even runs
/// (counted by `runs`), another memfd on odd runs.
fn stdin_memfd_setup(runs: &std::sync::atomic::AtomicUsize) -> StdinMemfdSetup {
    let root = tempfile::tempdir().unwrap();
    let listed = root.path().join("listed");
    std::fs::create_dir(&listed).unwrap();
    for name in ["x", "y", "z"] {
        File::create(listed.join(name)).unwrap();
    }
    let saved_stdin = std::io::stdin().as_fd().try_clone_to_owned().unwrap();
    let mapped = one_page_memfd(c"mapped");
    let stdin = if runs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .is_multiple_of(2)
    {
        mapped.try_clone().unwrap()
    } else {
        one_page_memfd(c"other")
    };
    assert_eq!(
        unsafe { libc::dup2(stdin.as_raw_fd(), libc::STDIN_FILENO) },
        libc::STDIN_FILENO
    );
    StdinMemfdSetup {
        root,
        mapped,
        _stdin: stdin,
        saved_stdin,
        test_pid: std::process::id(),
    }
}

/// Map one page of `fd` read-only.
fn map_page(fd: i32) -> *mut libc::c_void {
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd,
            0,
        )
    };
    assert_ne!(addr, libc::MAP_FAILED);
    addr
}

/// A `/proc/self/maps` line backed by the reader's stdin renders the fixed
/// stdio inode, but its numbering request is still made. The guest maps the
/// memfd `mapped`, reads its maps and lists a third
/// directory. On even runs `mapped` is also the guest's stdin, so its maps
/// line takes the stdio inode; on odd runs it is numbered by the pool. The
/// listing's `d_ino` values must not tell the runs apart: whether a mapped
/// file is the reader's stdin must not decide how many numbering requests a
/// maps read makes (https://github.com/rrnewton/hermit/issues/2897).
fn mapped_stdin_guest(setup: &StdinMemfdSetup) {
    // Detcore's descriptor table admits only fds 0-2 from the tool, so the
    // guest opens the memfd through the test process's procfs entry to get a
    // descriptor it tracks.
    let reopened = File::open(format!(
        "/proc/{}/fd/{}",
        setup.test_pid,
        setup.mapped.as_raw_fd()
    ))
    .unwrap();
    let addr = map_page(reopened.as_raw_fd());
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    assert!(maps.contains("/memfd:mapped"));
    let dir = open_dir(&setup.root.path().join("listed"));
    let listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    assert_eq!(unsafe { libc::munmap(addr, 4096) }, 0);
    assert_eq!(listing.names, [".", "..", "x", "y", "z"]);
    println!("listed {:016x}", listing.digest);
}

#[test]
fn mapped_stdin_does_not_renumber_a_later_listing() {
    let runs = std::sync::atomic::AtomicUsize::new(0);
    run_five_times_on(|| stdin_memfd_setup(&runs), mapped_stdin_guest, true);
}

/// A mapping of the guest's stdin reports, in `/proc/self/maps`, the inode
/// that `/proc/self/fdinfo/0` and `fstat(0)` report for that descriptor.
/// Detcore gives fds 0, 1 and 2 the stat of its own stdin, so all three share
/// one host identity; the maps line must take fd 0's number, not the number
/// of whichever descriptor came last.
fn mapped_stdin_identity_guest(_setup: &StdinMemfdSetup) {
    let addr = map_page(libc::STDIN_FILENO);
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let line = maps
        .lines()
        .find(|line| line.contains("/memfd:mapped"))
        .unwrap_or_else(|| panic!("no maps line for the stdin memfd in:\n{maps}"));
    let maps_inode: u64 = line.split_whitespace().nth(4).unwrap().parse().unwrap();
    let fdinfo = std::fs::read_to_string("/proc/self/fdinfo/0").unwrap();
    let fdinfo_inode: u64 = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("ino:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    assert_eq!(unsafe { libc::fstat(libc::STDIN_FILENO, &mut stat) }, 0);
    assert_eq!(unsafe { libc::munmap(addr, 4096) }, 0);
    assert_eq!(
        (maps_inode, stat.st_ino),
        (fdinfo_inode, fdinfo_inode),
        "maps, fstat and fdinfo disagree on stdin's inode"
    );
    println!("stdin inode {fdinfo_inode}");
}

#[test]
fn mapped_stdin_reports_the_inode_fdinfo_reports() {
    // Every run maps the memfd that is its stdin, so count the runs as even.
    let runs = std::sync::atomic::AtomicUsize::new(0);
    run_five_times_on(
        || {
            runs.store(0, std::sync::atomic::Ordering::Relaxed);
            stdin_memfd_setup(&runs)
        },
        mapped_stdin_identity_guest,
        true,
    );
}

/// The inode and mtime one raw stat syscall reports. `fstat` takes only
/// `dirfd`; `newfstatat` and `statx` also take `path` and `flags`.
fn raw_stat(sysno: libc::c_long, dirfd: i32, path: &CStr, flags: i32) -> (u64, i64, i64) {
    let path = path.as_ptr();
    if sysno == libc::SYS_statx {
        let mut stx = unsafe { std::mem::zeroed::<libc::statx>() };
        let mask = libc::STATX_INO | libc::STATX_MTIME;
        let rc = unsafe { libc::syscall(sysno, dirfd, path, flags, mask, &raw mut stx) };
        assert_eq!(rc, 0, "statx on dirfd {dirfd} failed");
        return (
            stx.stx_ino,
            stx.stx_mtime.tv_sec,
            stx.stx_mtime.tv_nsec.into(),
        );
    }
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let rc = if sysno == libc::SYS_fstat {
        unsafe { libc::syscall(sysno, dirfd, &raw mut stat) }
    } else {
        unsafe { libc::syscall(sysno, dirfd, path, &raw mut stat, flags) }
    };
    assert_eq!(rc, 0, "stat syscall {sysno} on dirfd {dirfd} failed");
    (stat.st_ino, stat.st_mtime, stat.st_mtime_nsec)
}

/// Every stat syscall that describes an inherited stdio descriptor by itself
/// reports the inode fdinfo reports, and one mtime. glibc's `fstat()` issues
/// `fstat` or `newfstatat(fd, "", AT_EMPTY_PATH)` depending on the build, and
/// Rust's `File::metadata()` issues `statx`, so the guest's libc must not
/// decide which number it sees. A call that names a path describes that path
/// instead, even with a stdio `dirfd` and `AT_EMPTY_PATH`: an absolute path
/// ignores `dirfd`. Raw syscalls keep this test independent of the host libc.
fn stdio_empty_path_stat_guest(_setup: &StdinMemfdSetup) {
    let empty = libc::AT_EMPTY_PATH;
    for fd in 0..=2 {
        let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap();
        let fdinfo_inode: u64 = fdinfo
            .lines()
            .find_map(|line| line.strip_prefix("ino:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let fstat = raw_stat(libc::SYS_fstat, fd, c"", 0);
        let newfstatat = raw_stat(libc::SYS_newfstatat, fd, c"", empty);
        let statx = raw_stat(libc::SYS_statx, fd, c"", empty);
        assert_eq!(
            (fstat.0, newfstatat, statx),
            (fdinfo_inode, fstat, fstat),
            "fd {fd}: fstat, newfstatat and statx (inode, mtime) disagree with fdinfo inode {fdinfo_inode}"
        );
        let root = raw_stat(libc::SYS_newfstatat, libc::AT_FDCWD, c"/", 0).0;
        let root_via_fd = [
            raw_stat(libc::SYS_newfstatat, fd, c"/", 0).0,
            raw_stat(libc::SYS_newfstatat, fd, c"/", empty).0,
            raw_stat(libc::SYS_statx, fd, c"/", empty).0,
        ];
        assert_eq!(
            root_via_fd, [root; 3],
            "fd {fd}: a stat of \"/\" with dirfd {fd} took the descriptor's inode {fdinfo_inode}"
        );
        assert_ne!(root, fdinfo_inode);
        println!("fd {fd} inode {fdinfo_inode}, / inode {root}");
    }
}

#[test]
fn stdio_empty_path_stat_reports_the_inode_fdinfo_reports() {
    let runs = std::sync::atomic::AtomicUsize::new(0);
    run_five_times_on(
        || {
            runs.store(0, std::sync::atomic::Ordering::Relaxed);
            stdin_memfd_setup(&runs)
        },
        stdio_empty_path_stat_guest,
        true,
    );
}

/// The test process's stdin, replaced by a pipe for one run and restored
/// when the run's value is dropped.
struct StdinPipeSetup {
    _stdin_pipe: [std::os::fd::OwnedFd; 2],
    saved_stdin: std::os::fd::OwnedFd,
}

impl Drop for StdinPipeSetup {
    fn drop(&mut self) {
        assert_eq!(
            unsafe { libc::dup2(self.saved_stdin.as_raw_fd(), libc::STDIN_FILENO) },
            libc::STDIN_FILENO
        );
    }
}

/// Another process's inherited stdin pipe reads as the same `pipe:[N]` link
/// as the reader's own: the guest forks a child that keeps its stdin, and
/// reads both `/proc/self/fd/0` and the child's `/proc/<pid>/fd/0`.
fn stdin_pipe_link_guest(_setup: &StdinPipeSetup) {
    let mut sync = [0; 2];
    assert_eq!(unsafe { libc::pipe(sync.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe {
            libc::close(sync[1]);
            let mut byte = 0_u8;
            let n = libc::read(sync[0], (&raw mut byte).cast(), 1);
            libc::_exit(if n == 0 { 0 } else { 1 });
        }
    }
    unsafe { libc::close(sync[0]) };
    let own = std::fs::read_link("/proc/self/fd/0").unwrap();
    let childs = std::fs::read_link(format!("/proc/{child}/fd/0")).unwrap();
    unsafe { libc::close(sync[1]) };
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    assert_eq!(own, childs, "one stdin pipe, two links");
    println!("stdin link {own:?}");
}

#[test]
fn another_process_stdin_pipe_reads_as_the_readers_own() {
    run_five_times_on(
        || {
            let saved_stdin = std::io::stdin().as_fd().try_clone_to_owned().unwrap();
            let (read, write) = host_pipe();
            assert_eq!(
                unsafe { libc::dup2(read.as_raw_fd(), libc::STDIN_FILENO) },
                libc::STDIN_FILENO
            );
            StdinPipeSetup {
                _stdin_pipe: [read, write],
                saved_stdin,
            }
        },
        stdin_pipe_link_guest,
        true,
    );
}

/// The `d_ino` that one `getdents64` listing of `dir` gives `name`, if listed.
fn listed_inode(dir: &str, name: &str) -> Option<u64> {
    let dir = CString::new(dir).unwrap();
    let fd = unsafe { libc::open(dir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    assert!(fd >= 0, "open {dir:?}");
    let mut buf = [0u8; 8192];
    let mut found = None;
    loop {
        let n = getdents64(fd, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        let mut offset = 0;
        while offset < n {
            let record = &buf[offset..n];
            let d_ino = u64::from_ne_bytes(record[..8].try_into().unwrap());
            let reclen = u16::from_ne_bytes(record[16..18].try_into().unwrap()) as usize;
            let entry = CStr::from_bytes_until_nul(&record[19..reclen]).unwrap();
            if entry.to_bytes() == name.as_bytes() {
                found = Some(d_ino);
            }
            offset += reclen;
        }
    }
    unsafe { libc::close(fd) };
    found
}

/// The inodes Detcore reports for descriptor `fd`'s procfs entries, in this
/// order: the `getdents64` records of `/proc/self/fd` and `/proc/<pid>/fd`,
/// `newfstatat` and `statx` of `/proc/self/fd/<fd>` without following it, the
/// `getdents64` record of `/proc/self/fdinfo`, and `newfstatat` of
/// `/proc/self/fdinfo/<fd>`.
fn descriptor_entry_inodes(fd: i32) -> [u64; 6] {
    let name = fd.to_string();
    let pid = unsafe { libc::getpid() };
    let link = CString::new(format!("/proc/self/fd/{fd}")).unwrap();
    let info = CString::new(format!("/proc/self/fdinfo/{fd}")).unwrap();
    let nofollow = libc::AT_SYMLINK_NOFOLLOW;
    [
        listed_inode("/proc/self/fd", &name).expect("fd not listed"),
        listed_inode(&format!("/proc/{pid}/fd"), &name).expect("fd not listed by pid"),
        raw_stat(libc::SYS_newfstatat, libc::AT_FDCWD, &link, nofollow).0,
        raw_stat(libc::SYS_statx, libc::AT_FDCWD, &link, nofollow).0,
        listed_inode("/proc/self/fdinfo", &name).expect("fdinfo not listed"),
        raw_stat(libc::SYS_newfstatat, libc::AT_FDCWD, &info, 0).0,
    ]
}

/// Linux gives a `/proc/<pid>/fd/<n>` or `fdinfo/<n>` entry a fresh inode
/// number each time it instantiates the entry's dentry, so the number a
/// listing or `lstat` reports changes whenever the host drops the cached
/// dentry, which memory pressure does at a time no guest chooses
/// (https://github.com/rrnewton/hermit/issues/3873). Here the guest forces
/// that: it closes the descriptor, looks the entry up (which drops the
/// dentry), and opens the descriptor again; natively both numbers change.
/// Detcore must report the same numbers before and after, the same for
/// `/proc/self` and `/proc/<pid>`, and the same in a listing as in a stat
/// that does not follow the link.
fn descriptor_entry_guest() {
    const FD: i32 = 40;
    let reopen = || {
        let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        assert_eq!(unsafe { libc::dup2(null, FD) }, FD);
        assert_eq!(unsafe { libc::close(null) }, 0);
    };
    reopen();
    let before = descriptor_entry_inodes(FD);
    assert_eq!(unsafe { libc::close(FD) }, 0);
    for path in [c"/proc/self/fd/40", c"/proc/self/fdinfo/40"] {
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        assert_eq!(unsafe { libc::lstat(path.as_ptr(), &mut stat) }, -1);
    }
    assert_eq!(listed_inode("/proc/self/fd", "40"), None);
    reopen();
    let after = descriptor_entry_inodes(FD);
    assert_eq!(unsafe { libc::close(FD) }, 0);
    assert_eq!(
        before[..4],
        [before[0]; 4],
        "fd entry: listings and nofollow stats disagree"
    );
    assert_eq!(
        before[4], before[5],
        "fdinfo entry: listing and stat disagree"
    );
    assert_eq!(
        after, before,
        "the reopened descriptor's entries were renumbered"
    );
    println!("fd entry {} fdinfo entry {}", before[0], before[4]);
}

#[test]
fn descriptor_entries_keep_their_inodes_when_procfs_recreates_them() {
    run_five_times(descriptor_entry_guest);
}

/// `(st_dev, st_ino)` from raw `newfstatat(dirfd, path, flags)`.
fn at_identity(dirfd: i32, path: &CStr, flags: i32) -> (u64, u64) {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_newfstatat,
            dirfd,
            path.as_ptr(),
            &raw mut stat,
            flags,
        )
    };
    assert_eq!(rc, 0, "newfstatat {path:?} relative to {dirfd}");
    (stat.st_dev, stat.st_ino)
}

/// `(st_dev, st_ino)` from raw `fstat(fd)`.
fn fd_identity(fd: i32) -> (u64, u64) {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    assert_eq!(
        unsafe { libc::syscall(libc::SYS_fstat, fd, &raw mut stat) },
        0,
        "fstat {fd}"
    );
    (stat.st_dev, stat.st_ino)
}

/// Opens `/dev/null` as descriptor 40.
fn open_slot_40() {
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert_eq!(unsafe { libc::dup2(null, 40) }, 40);
    assert_eq!(unsafe { libc::close(null) }, 0);
}

/// The `d_ino` of `name` in a fresh listing of the directory `dirfd` holds.
fn listed_in(dirfd: i32, name: &str) -> u64 {
    assert_eq!(unsafe { libc::lseek(dirfd, 0, libc::SEEK_SET) }, 0);
    let mut buf = [0u8; 8192];
    loop {
        let n = getdents64(dirfd, &mut buf).unwrap();
        assert_ne!(n, 0, "{name} not listed");
        let mut offset = 0;
        while offset < n {
            let record = &buf[offset..n];
            let reclen = u16::from_ne_bytes(record[16..18].try_into().unwrap()) as usize;
            let entry = CStr::from_bytes_until_nul(&record[19..reclen]).unwrap();
            if entry.to_bytes() == name.as_bytes() {
                return u64::from_ne_bytes(record[..8].try_into().unwrap());
            }
            offset += reclen;
        }
    }
}

/// An opened `/proc/self/fdinfo/40` is the entry its path names: `fstat`
/// and `newfstatat(fd, "", AT_EMPTY_PATH)` report the identity a stat of the
/// path reports (codex review of
/// https://github.com/rrnewton/hermit/pull/3972, P2-1).
fn fdinfo_descriptor_identity_guest() {
    open_slot_40();
    let path = at_identity(libc::AT_FDCWD, c"/proc/self/fdinfo/40", 0);
    let fd = unsafe { libc::open(c"/proc/self/fdinfo/40".as_ptr(), libc::O_RDONLY) };
    assert!(fd >= 0);
    let actual = fd_identity(fd);
    let empty = at_identity(fd, c"", libc::AT_EMPTY_PATH);
    assert_eq!(
        (actual, empty),
        (path, path),
        "fdinfo: path, fstat and empty path"
    );
    assert_eq!(unsafe { libc::close(fd) }, 0);
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("fdinfo entry {}", path.1);
}

#[test]
fn an_opened_fdinfo_entry_has_the_identity_its_path_has() {
    run_five_times(fdinfo_descriptor_identity_guest);
}

/// `/proc/self/fd/40` opened with `O_PATH|O_NOFOLLOW` is the link itself:
/// `fstat` and an empty-path stat report what `lstat` of the path reports
/// (P2-1 of the same review).
fn opath_proc_link_identity_guest() {
    open_slot_40();
    let link = c"/proc/self/fd/40";
    let path = at_identity(libc::AT_FDCWD, link, libc::AT_SYMLINK_NOFOLLOW);
    let fd = unsafe { libc::open(link.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW) };
    assert!(fd >= 0);
    let actual = fd_identity(fd);
    let empty = at_identity(fd, c"", libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW);
    assert_eq!(
        (actual, empty),
        (path, path),
        "fd link: lstat, fstat and empty path"
    );
    assert_eq!(unsafe { libc::close(fd) }, 0);
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("fd link {}", path.1);
}

#[test]
fn an_opath_proc_fd_link_has_the_identity_its_path_has() {
    run_five_times(opath_proc_link_identity_guest);
}

/// A descriptor table directory a parent opened as `/proc/self/<table>` and
/// a child inherited still holds the parent's table: the child's listing
/// and its stat of `"40"` relative to it give the parent's entry (P2-2).
fn inherited_table_guest(table: &str) {
    open_slot_40();
    let path = CString::new(format!("/proc/self/{table}")).unwrap();
    let dir = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    assert!(dir >= 0);
    let parent = listed_in(dir, "40");
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        let listed = listed_in(dir, "40");
        let relative = at_identity(dir, c"40", libc::AT_SYMLINK_NOFOLLOW).1;
        let same = listed == parent && relative == parent;
        unsafe { libc::_exit(if same { 0 } else { 42 }) };
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "{table}: the child's listing or relative stat named another entry: {status:#x}"
    );
    assert_eq!(unsafe { libc::close(dir) }, 0);
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("{table} entry {parent}");
}

#[test]
fn an_inherited_fd_table_names_the_parents_entries() {
    run_five_times(|| inherited_table_guest("fd"));
}

#[test]
fn an_inherited_fdinfo_table_names_the_parents_entries() {
    run_five_times(|| inherited_table_guest("fdinfo"));
}

/// With `/proc/self/fd` as the working directory, a stat of `"40"` relative
/// to it gives the entry a listing gives (P2-2).
fn cwd_table_guest() {
    open_slot_40();
    let dir = unsafe {
        libc::open(
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
    };
    assert!(dir >= 0);
    assert_eq!(unsafe { libc::fchdir(dir) }, 0);
    let listed = listed_in(dir, "40");
    let relative = at_identity(libc::AT_FDCWD, c"40", libc::AT_SYMLINK_NOFOLLOW).1;
    assert_eq!(relative, listed, "cwd-relative stat and listing");
    assert_eq!(unsafe { libc::close(dir) }, 0);
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("cwd entry {listed}");
}

#[test]
fn a_cwd_relative_stat_names_the_listed_entry() {
    run_five_times(cwd_table_guest);
}

/// Following `/proc/self/fd/40`, when descriptor 40 holds an ordinary
/// symlink opened with `O_PATH|O_NOFOLLOW`, describes that symlink: the stat
/// keeps the symlink's identity, which `lstat` of its name and `fstat(40)`
/// report, while `lstat` of the proc link describes the link (P2-3).
fn followed_opath_symlink_guest() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    std::fs::write(&target, b"x").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let link = CString::new(link.as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(link.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW) };
    assert_eq!(unsafe { libc::dup2(fd, 40) }, 40);
    assert_eq!(unsafe { libc::close(fd) }, 0);
    let original = at_identity(libc::AT_FDCWD, &link, libc::AT_SYMLINK_NOFOLLOW);
    let actual = fd_identity(40);
    let followed = at_identity(libc::AT_FDCWD, c"/proc/self/fd/40", 0);
    let proc_link = at_identity(
        libc::AT_FDCWD,
        c"/proc/self/fd/40",
        libc::AT_SYMLINK_NOFOLLOW,
    );
    assert_eq!(
        (actual, followed),
        (original, original),
        "the symlink's identity"
    );
    assert_ne!(proc_link, original, "the proc link is a separate object");
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("symlink {} proc link {}", original.1, proc_link.1);
}

#[test]
fn a_followed_fd_link_to_an_opath_symlink_keeps_the_symlinks_identity() {
    run_five_times(followed_opath_symlink_guest);
}

/// Restores the soft `RLIMIT_NOFILE` it lowered when dropped.
struct LoweredNofile(libc::rlimit);

impl Drop for LoweredNofile {
    fn drop(&mut self) {
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
    }
}

/// Lowers this process's soft `RLIMIT_NOFILE` to `soft` until the result is
/// dropped, so that a guest forked from it inherits a physical table it can
/// fill. Detcore keeps a guest's own `setrlimit` virtual.
fn lower_nofile(soft: libc::rlim_t) -> LoweredNofile {
    let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    let lowered = libc::rlimit {
        rlim_cur: soft.min(limit.rlim_cur),
        rlim_max: limit.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);
    LoweredNofile(limit)
}

/// With the guest's descriptor table full, a stat of `/proc/self/fdinfo/40`
/// still reports the identity `fstat` of an opened descriptor for it does:
/// recovering the entry's identity must not need a free guest descriptor
/// (codex re-check of https://github.com/rrnewton/hermit/pull/3972, P2-1).
fn full_table_guest(_lowered: &LoweredNofile) {
    open_slot_40();
    let info = unsafe { libc::open(c"/proc/self/fdinfo/40".as_ptr(), libc::O_RDONLY) };
    assert!(info >= 0);
    let mut filler = Vec::new();
    let errno = loop {
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            break std::io::Error::last_os_error().raw_os_error();
        }
        filler.push(fd);
        assert!(filler.len() < 4096, "the descriptor table never filled");
    };
    assert_eq!(errno, Some(libc::EMFILE));
    let path = at_identity(libc::AT_FDCWD, c"/proc/self/fdinfo/40", 0);
    let actual = fd_identity(info);
    for fd in filler {
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }
    assert_eq!(path, actual, "path and descriptor stats with a full table");
    assert_eq!(unsafe { libc::close(info) }, 0);
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("fdinfo entry {}", path.1);
}

#[test]
fn a_stat_with_a_full_descriptor_table_keeps_the_entrys_identity() {
    run_five_times_on(|| lower_nofile(256), full_table_guest, true);
}

/// Storage for a pathname that the stat result then overwrites, which Linux
/// allows: it copies the pathname in before it copies the result out.
#[repr(C)]
union Overlapping<T: Copy> {
    result: T,
    path: [u8; 256],
}

/// A stat whose pathname shares its storage with the result reports the
/// identity `fstat` of an opened descriptor for the same entry does, for
/// `newfstatat` and `statx` (P2-2 of the same re-check).
fn overlapping_path_guest() {
    open_slot_40();
    let info = unsafe { libc::open(c"/proc/self/fdinfo/40".as_ptr(), libc::O_RDONLY) };
    assert!(info >= 0);
    let actual = fd_identity(info);
    let path = c"/proc/self/fdinfo/40".to_bytes_with_nul();

    let mut stat = Overlapping::<libc::stat> { path: [0; 256] };
    unsafe { stat.path[..path.len()].copy_from_slice(path) };
    let rc = unsafe {
        let buffer = &raw mut stat;
        libc::syscall(
            libc::SYS_newfstatat,
            libc::AT_FDCWD,
            buffer.cast::<u8>(),
            buffer.cast::<libc::stat>(),
            0,
        )
    };
    assert_eq!(rc, 0);
    let by_newfstatat = unsafe { (stat.result.st_dev, stat.result.st_ino) };

    let mut statx = Overlapping::<libc::statx> { path: [0; 256] };
    unsafe { statx.path[..path.len()].copy_from_slice(path) };
    let rc = unsafe {
        let buffer = &raw mut statx;
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            buffer.cast::<u8>(),
            0,
            libc::STATX_INO,
            buffer.cast::<libc::statx>(),
        )
    };
    assert_eq!(rc, 0);
    let result = unsafe { statx.result };
    let by_statx = (
        libc::makedev(result.stx_dev_major, result.stx_dev_minor),
        result.stx_ino,
    );

    assert_eq!(
        (by_newfstatat, by_statx),
        (actual, actual),
        "overlapping-path stats"
    );
    assert_eq!(unsafe { libc::close(info) }, 0);
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("fdinfo entry {}", actual.1);
}

#[test]
fn a_stat_whose_path_overlaps_its_result_keeps_the_entrys_identity() {
    run_five_times(overlapping_path_guest);
}

/// A directory, made outside the guest, whose name is not UTF-8 and which
/// holds `entry`, a symlink to `/proc/self/fdinfo/40`. Made outside because
/// the test harness's RPC validation cannot carry a non-UTF-8 path for the
/// guest's own `mkdir`; the guest only resolves it.
fn non_utf8_directory() -> tempfile::TempDir {
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    let directory = root
        .path()
        .join(std::ffi::OsStr::from_bytes(b"nonutf8-\xff"));
    std::fs::create_dir(&directory).unwrap();
    std::os::unix::fs::symlink("/proc/self/fdinfo/40", directory.join("entry")).unwrap();
    root
}

/// A stat that reaches `/proc/self/fdinfo/40` through a directory whose name
/// is not UTF-8 reports the identity `fstat` of an opened descriptor for the
/// entry does: a pathname is bytes, and every component of it is legal
/// (codex's second re-check of https://github.com/rrnewton/hermit/pull/3972,
/// P2-1).
fn non_utf8_path_guest(root: &tempfile::TempDir) {
    use std::os::unix::ffi::OsStrExt;
    open_slot_40();
    let info = unsafe { libc::open(c"/proc/self/fdinfo/40".as_ptr(), libc::O_RDONLY) };
    assert!(info >= 0);
    let alias = root
        .path()
        .join(std::ffi::OsStr::from_bytes(b"nonutf8-\xff"))
        .join("entry");
    let alias = CString::new(alias.as_os_str().as_bytes()).unwrap();
    let path = at_identity(libc::AT_FDCWD, &alias, 0);
    let actual = fd_identity(info);
    assert_eq!(
        path, actual,
        "a non-UTF-8 directory on the way to the entry"
    );
    assert_eq!(unsafe { libc::close(info) }, 0);
    assert_eq!(unsafe { libc::close(40) }, 0);
    println!("fdinfo entry {}", actual.1);
}

#[test]
fn a_non_utf8_path_to_an_entry_keeps_the_entrys_identity() {
    run_five_times_on(non_utf8_directory, non_utf8_path_guest, true);
}

/// Following `/proc/self/fd/40`, when descriptor 40 holds the
/// `/proc/self/fd/41` link itself (opened with `O_PATH|O_NOFOLLOW`), jumps
/// to that link object and stops: `fstat(40)`, the followed stat and a
/// non-following stat of `/proc/self/fd/41` describe one object (codex's
/// third re-check of https://github.com/rrnewton/hermit/pull/3972, P2-1).
fn held_proc_link_guest() {
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert_eq!(unsafe { libc::dup2(null, 41) }, 41);
    assert_eq!(unsafe { libc::close(null) }, 0);
    let link = unsafe {
        libc::open(
            c"/proc/self/fd/41".as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW,
        )
    };
    assert_eq!(unsafe { libc::dup2(link, 40) }, 40);
    assert_eq!(unsafe { libc::close(link) }, 0);
    let held = fd_identity(40);
    let followed = at_identity(libc::AT_FDCWD, c"/proc/self/fd/40", 0);
    let original = at_identity(
        libc::AT_FDCWD,
        c"/proc/self/fd/41",
        libc::AT_SYMLINK_NOFOLLOW,
    );
    assert_eq!((followed, original), (held, held), "the held proc link");
    assert_eq!(unsafe { libc::close(40) }, 0);
    assert_eq!(unsafe { libc::close(41) }, 0);
    println!("held link {}", held.1);
}

#[test]
fn a_followed_fd_link_to_a_held_proc_link_stops_at_that_link() {
    run_five_times(held_proc_link_guest);
}

/// A 64-byte symlink to `/proc/self/fdinfo/41` (the slashes are redundant)
/// on tmpfs, whose device has major number 0, so that it passes the cheap
/// gate a procfs fd link passes.
fn ordinary_64_byte_link() -> tempfile::TempDir {
    let root = tempfile::tempdir_in("/dev/shm").unwrap();
    let target = format!("{}proc/self/fdinfo/41", "/".repeat(45));
    assert_eq!(target.len(), 64);
    std::os::unix::fs::symlink(target, root.path().join("link")).unwrap();
    root
}

/// Following `/proc/self/fd/40`, when descriptor 40 holds an ordinary
/// symlink (opened with `O_PATH|O_NOFOLLOW`) whose 64-byte text names a
/// procfs entry, stops at that symlink: `fstat(40)`, the followed stat and
/// `lstat` of the symlink describe one object, not the entry its text names
/// (P2-1 of the same re-check).
fn held_ordinary_link_guest(root: &tempfile::TempDir) {
    use std::os::unix::ffi::OsStrExt;
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert_eq!(unsafe { libc::dup2(null, 41) }, 41);
    assert_eq!(unsafe { libc::close(null) }, 0);
    let path = CString::new(root.path().join("link").as_os_str().as_bytes()).unwrap();
    let link = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW) };
    assert_eq!(unsafe { libc::dup2(link, 40) }, 40);
    assert_eq!(unsafe { libc::close(link) }, 0);
    let held = fd_identity(40);
    let followed = at_identity(libc::AT_FDCWD, c"/proc/self/fd/40", 0);
    let direct = at_identity(libc::AT_FDCWD, &path, libc::AT_SYMLINK_NOFOLLOW);
    assert_eq!(
        (followed, direct),
        (held, held),
        "the held ordinary symlink"
    );
    assert_eq!(unsafe { libc::close(40) }, 0);
    assert_eq!(unsafe { libc::close(41) }, 0);
    println!("held symlink {}", held.1);
}

#[test]
fn a_followed_fd_link_to_a_held_ordinary_link_stops_at_that_link() {
    run_five_times_on(ordinary_64_byte_link, held_ordinary_link_guest, true);
}
