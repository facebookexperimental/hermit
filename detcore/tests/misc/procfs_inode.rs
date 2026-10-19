/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Procfs metadata the guest sees does not depend on the host's procfs.
//!
//! Linux numbers a `/proc/<pid>/fd/<n>` entry from a host-wide counter when it
//! builds the entry's dentry, and builds it again under a new number once the
//! dentry is gone. Host memory pressure can evict it at any time, so a second
//! listing of `/proc/<pid>/fd` showed new numbers in one `--verify` run and
//! the old ones in the other (compat/lsof). Here the guest forces the rebuild:
//! it closes a descriptor, looks up the descriptor's entry, which fails and
//! drops the dentry, and opens a file at the same number again.
//!
//! Detcore names an entry by its path within procfs, which it reads back from
//! a descriptor the guest holds or opens for it, so the guest may reach the
//! entry as `/proc/<pid>`, `/proc/self` or `/proc/thread-self`. When it
//! cannot name the entry it records a determinism loss, so verification
//! refuses to compare the run rather than trusting host numbering.

use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::time::Duration;

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
    under_detcore_with(config(), guest)
}

/// Run `guest` under Detcore configured by `config` and require that it
/// exits 0.
fn under_detcore_with(config: Config, guest: impl FnOnce() + Send) {
    let (output, _) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        guest,
        detcore_testutils::prepare_ptrace_timer_config(config),
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

/// Marks a run of this test binary that [`in_its_own_process`] started.
const OWN_PROCESS_ENV: &str = "HERMIT_PROCFS_INODE_TEST_INNER";

/// Run `test`, the body of the test `name` in this module, in a process of
/// its own: this binary run again on that test alone, as nextest runs every
/// test. Detcore keeps the first determinism loss a process records, and
/// records a procfs loss once per process, so a test that checks the loss
/// must not share its process with other tests, as `cargo test` runs them.
/// Nor must a test that closes a descriptor in this process and needs its
/// number again, which a descriptor another test opens could take.
fn in_its_own_process(name: &str, test: impl FnOnce()) {
    let ran = format!("procfs_inode: {name} ran in its own process");
    if std::env::var_os(OWN_PROCESS_ENV).is_some() {
        test();
        println!("{ran}");
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .arg(format!("procfs_inode::{name}"))
        .args(["--exact", "--nocapture", "--test-threads=1"])
        .env(OWN_PROCESS_ENV, "1")
        .output()
        .expect("run this test binary again");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{name} failed in its own process: {stdout}{stderr}"
    );
    assert!(
        stdout.contains(&ran),
        "{name} did not run in its own process: {stdout}{stderr}"
    );
}

/// This process's procfs directory, by PID rather than through `self`.
fn proc_dir() -> String {
    format!("/proc/{}", std::process::id())
}

/// The inode number `getdents64` lists for `name` in `directory`.
fn listed(directory: &str, name: &str) -> u64 {
    let directory = File::open(directory).unwrap();
    let mut buffer = vec![0u8; 32 * 1024];
    let mut found = None;
    loop {
        let read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory.as_raw_fd(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        assert!(read >= 0, "getdents64: {}", std::io::Error::last_os_error());
        if read == 0 {
            break;
        }
        let mut offset = 0;
        while offset < read as usize {
            let record = &buffer[offset..];
            let ino = u64::from_ne_bytes(record[0..8].try_into().unwrap());
            let length = usize::from(u16::from_ne_bytes(record[16..18].try_into().unwrap()));
            let entry = &record[19..length];
            let entry = &entry[..entry.iter().position(|byte| *byte == 0).unwrap()];
            if entry == name.as_bytes() {
                found = Some(ino);
            }
            offset += length;
        }
    }
    found.unwrap_or_else(|| panic!("{directory:?} lists no {name}"))
}

/// The inode number and link count `statx` reports for `path` relative to
/// `dirfd`.
fn statx(dirfd: i32, path: &str, flags: i32) -> std::io::Result<(u64, u32)> {
    let path = CString::new(path).unwrap();
    let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let mask = libc::STATX_INO | libc::STATX_NLINK;
    if unsafe { libc::statx(dirfd, path.as_ptr(), flags, mask, buf.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let buf = unsafe { buf.assume_init() };
    Ok((buf.stx_ino, buf.stx_nlink))
}

/// The inode numbers of one entry of the descriptor directory `fds` before
/// and after it is rebuilt: as `getdents64` lists it, and as `lstat`,
/// `fstatat` relative to the directory and `statx` report it, which see the
/// rebuilt entry first.
#[derive(Debug)]
struct Sightings {
    listed_before: u64,
    stat_before: u64,
    stat_after: [u64; 3],
    listed_after: u64,
}

fn rebuild_a_descriptor_entry(fds: &str) -> Sightings {
    let fd = File::open("/dev/null").unwrap().into_raw_fd();
    let name = fd.to_string();
    let entry = format!("{fds}/{name}");
    let lstat = || std::fs::symlink_metadata(&entry).map(|metadata| metadata.ino());
    let listed_before = listed(fds, &name);
    let stat_before = lstat().unwrap();
    assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
    // The lookup of a closed descriptor's entry fails and drops its dentry.
    assert!(lstat().is_err(), "{entry} exists after close");
    let again = File::open("/dev/null").unwrap().into_raw_fd();
    assert_eq!(again, fd, "the reopened file must take the closed number");
    let directory = File::open(fds).unwrap();
    let nofollow = libc::AT_SYMLINK_NOFOLLOW;
    let stat_after = [
        lstat().unwrap(),
        statx(directory.as_raw_fd(), &name, nofollow).unwrap().0,
        statx(libc::AT_FDCWD, &entry, nofollow).unwrap().0,
    ];
    let listed_after = listed(fds, &name);
    assert_eq!(unsafe { libc::close(again) }, 0, "close({again})");
    Sightings {
        listed_before,
        stat_before,
        stat_after,
        listed_after,
    }
}

/// Rebuilds an entry of each descriptor directory `fds` names, natively and
/// then under Detcore, and requires that the guest saw each keep its number.
fn rebuilt_entries_keep_their_numbers(fds: fn() -> Vec<String>) {
    // Natively the rebuilt entry gets a new number. Were the host to keep the
    // dentry, the guest below would keep its number at any Detcore revision.
    for fds in fds() {
        let native = rebuild_a_descriptor_entry(&fds);
        assert_ne!(
            native.listed_before, native.listed_after,
            "the host did not rebuild the entry in {fds}, so this test cannot tell: {native:?}"
        );
    }
    under_detcore(move || {
        for fds in fds() {
            let seen = rebuild_a_descriptor_entry(&fds);
            let number = seen.listed_before;
            assert!(
                seen.stat_before == number
                    && seen.stat_after.iter().all(|stat| *stat == number)
                    && seen.listed_after == number,
                "the guest saw the rebuilt entry in {fds} renumbered: {seen:?}"
            );
        }
    });
}

#[test]
fn a_rebuilt_proc_fd_entry_keeps_its_number() {
    in_its_own_process("a_rebuilt_proc_fd_entry_keeps_its_number", || {
        rebuilt_entries_keep_their_numbers(|| vec![format!("{}/fd", proc_dir())]);
    });
}

/// `/proc/self` and `/proc/thread-self` name the guest, not the tracer, when
/// Detcore names the entry a guest reached through them. The thread's own
/// `fd` directory is a second directory, whose entries are other inodes.
#[test]
fn a_rebuilt_entry_reached_through_self_keeps_its_number() {
    in_its_own_process(
        "a_rebuilt_entry_reached_through_self_keeps_its_number",
        || {
            rebuilt_entries_keep_their_numbers(|| {
                vec![
                    "/proc/self/fd".to_owned(),
                    "/proc/thread-self/fd".to_owned(),
                ]
            });
        },
    );
}

/// Detcore names an entry the guest reached by path by opening the path
/// again after the stat. A stat whose result buffer holds its own path has
/// overwritten the path by then (Linux reads the path before it writes the
/// result), so Detcore cannot name the entry. It records a determinism loss
/// instead of passing host numbering on silently, and the stat still
/// succeeds. Named by a later stat, the entry keeps the number it was given
/// unnamed, and keeps it when the host rebuilds the entry.
#[test]
fn an_entry_detcore_cannot_name_records_a_determinism_loss() {
    in_its_own_process(
        "an_entry_detcore_cannot_name_records_a_determinism_loss",
        || {
            under_detcore(|| {
                let fd = File::open("/dev/null").unwrap().into_raw_fd();
                let entry = format!("/proc/self/fd/{fd}");
                let lstat = || std::fs::symlink_metadata(&entry).map(|metadata| metadata.ino());
                let path = CString::new(entry.clone()).unwrap();
                let path = path.as_bytes_with_nul();
                let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
                assert!(path.len() <= std::mem::size_of::<libc::stat>());
                let buffer = stat.as_mut_ptr();
                unsafe { std::ptr::copy_nonoverlapping(path.as_ptr(), buffer.cast(), path.len()) };
                let flags = libc::AT_SYMLINK_NOFOLLOW;
                let result = unsafe {
                    libc::syscall(libc::SYS_newfstatat, libc::AT_FDCWD, buffer, buffer, flags)
                };
                assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
                let unnamed = unsafe { stat.assume_init() }.st_ino;
                assert_eq!(lstat().unwrap(), unnamed, "named, the entry was renumbered");
                assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
                assert!(lstat().is_err(), "{entry} exists after close");
                let again = File::open("/dev/null").unwrap().into_raw_fd();
                assert_eq!(again, fd, "the reopened file must take the closed number");
                assert_eq!(
                    lstat().unwrap(),
                    unnamed,
                    "the rebuilt entry was renumbered"
                );
            });
            let loss = detcore::detlog::determinism_loss();
            assert!(
                loss.as_deref()
                    .is_some_and(|loss| loss.starts_with("procfs inode numbers: ")),
                "no procfs determinism loss was recorded: {loss:?}"
            );
        },
    );
}

/// Run `guest` under Detcore without sequentialized threads, in a process of
/// its own as the test `name`, and require the one procfs determinism loss
/// that refuses naming in that mode. Another guest thread could then close
/// the descriptor Detcore opens to name an entry reached by path, and take
/// its number for one of its own, before Detcore reads and closes it; or
/// change what a descriptor or the working directory leads to between
/// Detcore's look at its identity and at its link. So Detcore names no
/// procfs entry then, whatever the guest named it by.
fn without_sequentialized_threads_nothing_is_named(name: &str, guest: fn()) {
    in_its_own_process(name, || {
        let config = Config {
            sequentialize_threads: false,
            ..config()
        };
        under_detcore_with(config, guest);
        assert_eq!(
            detcore::detlog::determinism_loss().as_deref(),
            Some(
                "procfs inode numbers: the guest's threads are not sequentialized, \
                 so Detcore does not name procfs files"
            )
        );
    });
}

/// By path, the stat still succeeds, and the guest's next descriptor takes
/// the number it would have taken without the stat: Detcore opened nothing.
#[test]
fn without_sequentialized_threads_a_path_is_not_opened_again() {
    without_sequentialized_threads_nothing_is_named(
        "without_sequentialized_threads_a_path_is_not_opened_again",
        || {
            let lowest_free = || {
                let fd = File::open("/dev/null").unwrap().into_raw_fd();
                assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
                fd
            };
            let free = lowest_free();
            std::fs::symlink_metadata("/proc/self/stat").unwrap();
            assert_eq!(lowest_free(), free, "the stat left a descriptor open");
        },
    );
}

#[test]
fn without_sequentialized_threads_a_descriptor_is_not_named() {
    without_sequentialized_threads_nothing_is_named(
        "without_sequentialized_threads_a_descriptor_is_not_named",
        || {
            let file = File::open("/proc/self/stat").unwrap();
            file.metadata().unwrap();
        },
    );
}

#[test]
fn without_sequentialized_threads_the_working_directory_is_not_named() {
    without_sequentialized_threads_nothing_is_named(
        "without_sequentialized_threads_the_working_directory_is_not_named",
        || {
            std::env::set_current_dir("/proc/self").unwrap();
            let (inode, _) = statx(libc::AT_FDCWD, "", libc::AT_EMPTY_PATH).unwrap();
            assert_ne!(inode, 0);
        },
    );
}

/// By the time Detcore could read a stat's path again, a result written
/// over it may name another entry. Here it does: x86_64's `struct stat`
/// holds `st_mode` at offset 24, so a path placed there reads "mA" once the
/// result for a directory with mode 0o40555 is written, and "mA" in the
/// guest's directory leads to `/proc/self/fd`. The stat reports
/// `/proc/self/task`, which Linux looked up before it wrote the result.
/// Detcore must not give it `/proc/self/fd`'s number; it records a loss.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_overwritten_path_does_not_name_the_entry_it_now_leads_to() {
    in_its_own_process(
        "an_overwritten_path_does_not_name_the_entry_it_now_leads_to",
        || {
            let links = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("/proc/self/task", links.path().join("orig")).unwrap();
            std::os::unix::fs::symlink("/proc/self/fd", links.path().join("mA")).unwrap();
            under_detcore(|| {
                let fds = std::fs::metadata("/proc/self/fd").unwrap().ino();
                let directory = File::open(links.path()).unwrap();
                let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
                let buffer = stat.as_mut_ptr().cast::<u8>();
                let path = b"orig\0";
                let at = unsafe { buffer.add(24) };
                unsafe { std::ptr::copy_nonoverlapping(path.as_ptr(), at, path.len()) };
                let result = unsafe {
                    libc::syscall(libc::SYS_newfstatat, directory.as_raw_fd(), at, buffer, 0)
                };
                assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
                let stat = unsafe { stat.assume_init() };
                assert_eq!(
                    stat.st_mode,
                    libc::S_IFDIR | 0o555,
                    "the result wrote no \"mA\""
                );
                assert_ne!(stat.st_ino, fds, "the entry took /proc/self/fd's number");
                let task = std::fs::metadata("/proc/self/task").unwrap().ino();
                assert_eq!(task, stat.st_ino, "named, the entry was renumbered");
            });
            assert_eq!(
                detcore::detlog::determinism_loss().as_deref(),
                Some("procfs inode numbers: the call's result overwrote the guest's path")
            );
        },
    );
}

/// `tcp` listed in `/proc/<pid>/net` and reached as
/// `/proc/<pid>/task/<pid>/net/tcp` is one host inode.
fn net_tcp_numbers() -> (u64, u64) {
    let pid = std::process::id();
    let listed = listed(&format!("/proc/{pid}/net"), "tcp");
    let alias = format!("/proc/{pid}/task/{pid}/net/tcp");
    (listed, std::fs::metadata(alias).unwrap().ino())
}

#[test]
fn a_procfs_entry_reached_by_two_paths_keeps_one_number() {
    let (listed, alias) = net_tcp_numbers();
    assert_eq!(listed, alias, "natively one inode");
    under_detcore(|| {
        let (listed, alias) = net_tcp_numbers();
        assert_eq!(listed, alias, "the guest saw two numbers for one entry");
    });
}

/// The link counts of `/proc` as `stat`, `fstat` and `statx` report them.
fn proc_root_link_counts() -> [u64; 3] {
    let root = File::open("/proc").unwrap();
    [
        std::fs::metadata("/proc").unwrap().nlink(),
        root.metadata().unwrap().nlink(),
        u64::from(statx(libc::AT_FDCWD, "/proc", 0).unwrap().1),
    ]
}

#[test]
fn the_proc_root_link_count_does_not_count_host_processes() {
    // Linux counts every process in the procfs's PID namespace.
    let native = proc_root_link_counts();
    assert!(native.iter().all(|count| *count > 2), "{native:?}");
    under_detcore(|| assert_eq!(proc_root_link_counts(), [1, 1, 1]));
}

/// The link count `statx(AT_FDCWD, path, AT_EMPTY_PATH)` reports for the
/// current directory, where `path` is empty or NULL.
fn cwd_link_count(path: Option<&CStr>) -> std::io::Result<u32> {
    let path = path.map_or(std::ptr::null(), CStr::as_ptr);
    let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let (flags, mask) = (libc::AT_EMPTY_PATH, libc::STATX_NLINK);
    if unsafe { libc::statx(libc::AT_FDCWD, path, flags, mask, buf.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { buf.assume_init() }.stx_nlink)
}

/// `statx` of the current directory, by an empty path or (from Linux 6.11)
/// a NULL one, reports `/proc`'s link count as `stat` of `/proc` does.
#[test]
fn the_proc_root_link_count_through_the_cwd_does_not_count_host_processes() {
    // Linux before 6.11 refuses a NULL path, and the guest must see what the
    // host's kernel does.
    let native = cwd_link_count(None)
        .map(drop)
        .map_err(|error| error.raw_os_error());
    under_detcore(|| {
        std::env::set_current_dir("/proc").unwrap();
        let empty = CString::new("").unwrap();
        assert_eq!(cwd_link_count(Some(&empty)).unwrap(), 1, "an empty path");
        match native {
            Ok(()) => assert_eq!(cwd_link_count(None).unwrap(), 1, "a NULL path"),
            Err(errno) => {
                let error = cwd_link_count(None).unwrap_err();
                assert_eq!(error.raw_os_error(), errno, "a NULL path");
            }
        }
    });
}

#[test]
fn a_write_to_a_procfs_file_moves_its_mtime() {
    under_detcore(|| {
        let mut comm = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("{}/comm", proc_dir()))
            .unwrap();
        let mtime = |file: &File| {
            let metadata = file.metadata().unwrap();
            (metadata.mtime(), metadata.mtime_nsec())
        };
        let before = mtime(&comm);
        std::thread::sleep(Duration::from_millis(10));
        comm.write_all(b"procfs-inode").unwrap();
        assert_ne!(mtime(&comm), before, "the write left the mtime unchanged");
    });
}

/// The guest lists a thread's descriptor directory in two calls, the thread
/// exits between them, and its directory loses its name: the second call
/// returns entries from the snapshot the first took, keyed as they were
/// then. A descriptor entry rebuilt before the listing keeps the number
/// `lstat` reported, and no determinism loss is recorded, where keying the
/// second call again found nothing to name the directory by.
#[test]
fn a_listing_keeps_its_keys_after_its_task_exits() {
    in_its_own_process("a_listing_keeps_its_keys_after_its_task_exits", || {
        under_detcore(|| {
            let (sender, receiver) = std::sync::mpsc::channel();
            let (finish, finished) = std::sync::mpsc::channel::<()>();
            let thread = std::thread::spawn(move || {
                sender.send(unsafe { libc::gettid() }).unwrap();
                finished.recv().unwrap();
            });
            let tid = receiver.recv().unwrap();
            let task = format!("{}/task/{tid}", proc_dir());
            let fds = format!("{task}/fd");
            let fd = File::open("/dev/null").unwrap().into_raw_fd();
            let entry = format!("{fds}/{fd}");
            let lstat = || std::fs::symlink_metadata(&entry).map(|metadata| metadata.ino());
            let stat_before = lstat().unwrap();
            assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
            // The lookup of a closed descriptor's entry fails and drops its
            // dentry, which the listing builds again.
            assert!(lstat().is_err(), "{entry} exists after close");
            let again = File::open("/dev/null").unwrap().into_raw_fd();
            assert_eq!(again, fd, "the reopened file must take the closed number");
            let directory = File::open(&fds).unwrap();
            // `.` and `..` take 24 bytes each, so the first call returns only
            // them, and takes the snapshot.
            let mut buffer = vec![0u8; 48];
            let read = unsafe {
                libc::syscall(
                    libc::SYS_getdents64,
                    directory.as_raw_fd(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                )
            };
            assert_eq!(read, 48, "getdents64: {}", std::io::Error::last_os_error());
            finish.send(()).unwrap();
            thread.join().unwrap();
            // The join returns when the thread clears its ID, before it is reaped.
            let mut tries = 0;
            while std::fs::symlink_metadata(&task).is_ok() {
                tries += 1;
                assert!(tries < 100_000, "{task} outlived its thread");
                unsafe { libc::sched_yield() };
            }
            let rest = rest_of_listing(&directory);
            assert_eq!(
                rest.get(&fd.to_string()).copied(),
                Some(stat_before),
                "{fds} listed {fd} under another number: {rest:?}"
            );
        });
        assert_eq!(detcore::detlog::determinism_loss(), None);
    });
}

/// The entries `getdents64` returns from `directory`'s position on, by name.
fn rest_of_listing(directory: &File) -> std::collections::HashMap<String, u64> {
    let mut buffer = vec![0u8; 32 * 1024];
    let mut entries = std::collections::HashMap::new();
    loop {
        let read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory.as_raw_fd(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        assert!(read >= 0, "getdents64: {}", std::io::Error::last_os_error());
        if read == 0 {
            return entries;
        }
        let mut offset = 0;
        while offset < read as usize {
            let record = &buffer[offset..];
            let ino = u64::from_ne_bytes(record[0..8].try_into().unwrap());
            let length = usize::from(u16::from_ne_bytes(record[16..18].try_into().unwrap()));
            let name = &record[19..length];
            let name = &name[..name.iter().position(|byte| *byte == 0).unwrap()];
            entries.insert(String::from_utf8_lossy(name).into_owned(), ino);
            offset += length;
        }
    }
}

/// Under SaBRe, Detcore shares libc, and so `errno`, with the guest thread
/// whose call it handles, so naming an entry must leave the guest's `errno`
/// as it was. These tests run Detcore under ptrace, in another process,
/// where it cannot change it, so this pins the guest's view; Detcore's unit
/// tests pin the guard itself.
#[test]
fn naming_an_entry_leaves_the_guests_errno() {
    in_its_own_process("naming_an_entry_leaves_the_guests_errno", || {
        under_detcore(|| {
            const SENTINEL: i32 = 4242;
            let path = CString::new("/proc/self/stat").unwrap();
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            unsafe { *libc::__errno_location() = SENTINEL };
            let result = unsafe {
                libc::syscall(
                    libc::SYS_newfstatat,
                    libc::AT_FDCWD,
                    path.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            let errno = unsafe { *libc::__errno_location() };
            assert_eq!(result, 0, "newfstatat failed");
            assert_eq!(errno, SENTINEL, "naming the entry changed errno");
        });
        assert_eq!(detcore::detlog::determinism_loss(), None);
    });
}

/// The descriptor the `O_PATH` open that names a procfs path gets in the
/// filter tests: the lowest free one once the guest fills those below it.
#[cfg(target_arch = "x86_64")]
const FILTERED_FD: i32 = 100;

/// Linux's `ERESTARTSYS`, which it does not export to user space.
#[cfg(target_arch = "x86_64")]
const ERESTARTSYS: u32 = 512;

/// `AUDIT_ARCH_X86_64`, the `arch` of an x86_64 system call in seccomp data.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

/// A BPF instruction that loads the 32-bit word at `offset` in seccomp data:
/// the system call number at 0, its `arch` at 4, and the low half of
/// argument `n` at 16 + 8n.
#[cfg(target_arch = "x86_64")]
fn load(offset: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
        jt: 0,
        jf: 0,
        k: offset,
    }
}

/// A BPF jump past `jt` instructions when `test` (`BPF_JEQ` or `BPF_JSET`)
/// holds for the loaded word and `value`, and past `jf` when it does not.
#[cfg(target_arch = "x86_64")]
fn jump(test: u32, value: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | test | libc::BPF_K) as u16,
        jt,
        jf,
        k: value,
    }
}

/// A BPF instruction that ends the filter with `action`.
#[cfg(target_arch = "x86_64")]
fn ret(action: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_RET | libc::BPF_K) as u16,
        jt: 0,
        jf: 0,
        k: action,
    }
}

/// Install `program` as a seccomp filter of this thread, which the threads
/// and processes it starts inherit, as a guest inherits a filter from
/// whatever started it. Detcore refuses a guest's own `seccomp(2)`.
#[cfg(target_arch = "x86_64")]
fn install_filter(program: &mut [libc::sock_filter]) {
    let fprog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    let installed = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &fprog as *const libc::sock_fprog,
        )
    };
    assert_eq!(installed, 0, "seccomp: {}", std::io::Error::last_os_error());
}

/// Make `close(FILTERED_FD)` return `-errno` without running.
#[cfg(target_arch = "x86_64")]
fn filter_close(errno: u32) {
    install_filter(&mut [
        load(4),
        jump(libc::BPF_JEQ, AUDIT_ARCH_X86_64, 1, 0),
        ret(libc::SECCOMP_RET_ALLOW),
        load(0),
        jump(libc::BPF_JEQ, libc::SYS_close as u32, 0, 3),
        load(16),
        jump(libc::BPF_JEQ, FILTERED_FD as u32, 0, 1),
        ret(libc::SECCOMP_RET_ERRNO | errno),
        ret(libc::SECCOMP_RET_ALLOW),
    ]);
}

/// Make `close(FILTERED_FD)` fail with `EPERM` without running, and every
/// `statx` that does not follow a final link, as `std::fs::symlink_metadata`
/// stats, fail with `ENOENT`. Both block a call; neither reports a result
/// for a call that did not run.
#[cfg(target_arch = "x86_64")]
fn filter_close_and_lstat() {
    install_filter(&mut [
        load(4),
        jump(libc::BPF_JEQ, AUDIT_ARCH_X86_64, 1, 0),
        ret(libc::SECCOMP_RET_ALLOW),
        load(0),
        jump(libc::BPF_JEQ, libc::SYS_close as u32, 0, 3),
        load(16),
        jump(libc::BPF_JEQ, FILTERED_FD as u32, 0, 5),
        ret(libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
        jump(libc::BPF_JEQ, libc::SYS_statx as u32, 0, 3),
        load(32),
        jump(libc::BPF_JSET, libc::AT_SYMLINK_NOFOLLOW as u32, 0, 1),
        ret(libc::SECCOMP_RET_ERRNO | libc::ENOENT as u32),
        ret(libc::SECCOMP_RET_ALLOW),
    ]);
}

/// Install the seccomp filter `filter` installs, then run a guest that fills
/// the descriptors below `FILTERED_FD`, so that the `O_PATH` descriptor
/// Detcore opens to name a procfs path takes it, and then stats a procfs
/// path, following the link as `std::fs::metadata` does. The filter leaves
/// that descriptor open in the guest, so the run must record a determinism
/// loss, and must end.
#[cfg(target_arch = "x86_64")]
fn stat_a_procfs_path_with_close_filtered(filter: impl FnOnce()) -> Option<String> {
    filter();
    under_detcore(|| {
        let open = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
        assert!(!open(FILTERED_FD), "{FILTERED_FD} is open from the start");
        let null = File::open("/dev/null").unwrap().into_raw_fd();
        let mut last = null;
        while last < FILTERED_FD - 1 {
            last = unsafe { libc::dup(null) };
            assert!(last >= 0, "dup: {}", std::io::Error::last_os_error());
        }
        assert_eq!(
            last,
            FILTERED_FD - 1,
            "a descriptor below {FILTERED_FD} was free"
        );
        std::fs::metadata("/proc/self/stat").unwrap();
        assert!(open(FILTERED_FD), "Detcore's O_PATH descriptor was closed");
    });
    detcore::detlog::determinism_loss()
}

/// A close that reports a restart every time has not run. Detcore tries it a
/// bounded number of times, where it had tried for ever, and records that
/// the descriptor stays open.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_close_that_never_runs_ends_in_a_determinism_loss() {
    in_its_own_process("a_close_that_never_runs_ends_in_a_determinism_loss", || {
        let loss = stat_a_procfs_path_with_close_filtered(|| filter_close(ERESTARTSYS));
        assert_eq!(
            loss.as_deref(),
            Some(
                "procfs inode numbers: close of the guest's O_PATH descriptor did not run in 8 attempts"
            )
        );
    });
}

/// A close that a filter refuses, as a filter that blocks a call does, leaves
/// the descriptor open, which Detcore learns from the error close returns,
/// where it had taken the descriptor as closed whatever close returned.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_close_that_a_filter_refuses_ends_in_a_determinism_loss() {
    in_its_own_process(
        "a_close_that_a_filter_refuses_ends_in_a_determinism_loss",
        || {
            let loss = stat_a_procfs_path_with_close_filtered(|| filter_close(libc::EPERM as u32));
            assert_eq!(
                loss.as_deref(),
                Some("procfs inode numbers: close of the guest's O_PATH descriptor returned EPERM")
            );
        },
    );
}

/// A close that a filter refuses leaves the descriptor open, whatever a
/// later look at the guest's descriptors finds. Here a filter also makes a
/// no-follow `statx` report that nothing is there. The tracer's threads start
/// under it, as Detcore's own calls run under the guest's filter under SaBRe,
/// and a look at `/proc/<tid>/fd/<n>` after the refused close had then taken
/// the descriptor as closed and recorded no loss.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_refused_close_ends_in_a_determinism_loss_whatever_a_lookup_says() {
    in_its_own_process(
        "a_refused_close_ends_in_a_determinism_loss_whatever_a_lookup_says",
        || {
            let loss = stat_a_procfs_path_with_close_filtered(filter_close_and_lstat);
            assert_eq!(
                loss.as_deref(),
                Some("procfs inode numbers: close of the guest's O_PATH descriptor returned EPERM")
            );
        },
    );
}

/// The descriptor whose number is `openat`'s own system call number.
#[cfg(target_arch = "x86_64")]
const OPENAT_OWN_NUMBER: i32 = libc::SYS_openat as i32;

/// The `O_PATH` descriptor Detcore opens to name a procfs path can take the
/// number of `openat`'s own system call, which the result register holds
/// until the call runs. Reverie reports an `openat` that a signal stopped
/// before it ran as a restart, never as that number, so the number is the
/// call's descriptor: Detcore names the entry, records no loss, and closes
/// the descriptor, rather than opening another and leaving this one open.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_descriptor_numbered_as_openat_is_detcores_when_it_was_free() {
    in_its_own_process(
        "a_descriptor_numbered_as_openat_is_detcores_when_it_was_free",
        || {
            under_detcore(|| {
                let open = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
                assert!(
                    !open(OPENAT_OWN_NUMBER),
                    "{OPENAT_OWN_NUMBER} is open from the start"
                );
                let null = File::open("/dev/null").unwrap().into_raw_fd();
                let mut last = null;
                while last < OPENAT_OWN_NUMBER - 1 {
                    last = unsafe { libc::dup(null) };
                    assert!(last >= 0, "dup: {}", std::io::Error::last_os_error());
                }
                assert_eq!(
                    last,
                    OPENAT_OWN_NUMBER - 1,
                    "a descriptor below {OPENAT_OWN_NUMBER} was free"
                );
                std::fs::metadata("/proc/self/stat").unwrap();
                assert!(
                    !open(OPENAT_OWN_NUMBER),
                    "Detcore's O_PATH descriptor stayed open"
                );
                assert!(!open(OPENAT_OWN_NUMBER + 1), "Detcore opened another");
            });
            assert_eq!(detcore::detlog::determinism_loss(), None);
        },
    );
}
