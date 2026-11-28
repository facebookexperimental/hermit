/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `FS_IOC_GETVERSION` reads a file's inode generation, which the host picks
//! when it makes the inode (a random number on ext4, the transaction ID on
//! btrfs), so two runs that create the same file used to read different
//! values (https://github.com/facebookexperimental/hermit/issues/117).
//! Detcore answers it from the file's virtual inode number, the one `fstat`
//! shows, with the host's value written only to a page of its own, and
//! leaves every error to the kernel.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use detcore::Config;
use detcore::Detcore;
use reverie::Error;
use reverie::ExitStatus;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

/// The numbers that read or set an inode generation in Linux 7.1.3, the same
/// on x86_64 and aarch64 (`include/uapi/linux/fs.h`, `ext4.h`).
const FS_IOC_GETVERSION: u64 = 0x8008_7601;
const EXT4_IOC_GETVERSION: u64 = 0x8008_6603;
const FS_IOC32_GETVERSION: u64 = 0x8004_7601;
const EXT4_IOC32_GETVERSION: u64 = 0x8004_6603;
const FS_IOC_SETVERSION: u64 = 0x4008_7602;
const EXT4_IOC_SETVERSION: u64 = 0x4008_6604;
const FS_IOC32_SETVERSION: u64 = 0x4004_7602;
const EXT4_IOC32_SETVERSION: u64 = 0x4004_6604;

/// Every set number, by the label the guest prints for it.
const SETS: [(&str, u64); 4] = [
    ("set_fs", FS_IOC_SETVERSION),
    ("set_ext4", EXT4_IOC_SETVERSION),
    ("set_fs32", FS_IOC32_SETVERSION),
    ("set_ext4_32", EXT4_IOC32_SETVERSION),
];

/// `f_type` of ext2, ext3 and ext4 (`include/uapi/linux/magic.h`).
const EXT4_SUPER_MAGIC: i64 = 0xEF53;

/// A command register whose high 32 bits are set: Linux reads only the low
/// 32 (`ioctl`'s `unsigned int cmd`).
const HIGH_BITS: u64 = 0xdead_beef_0000_0000;

/// Where the files are made, unless `HERMIT_TEST_GENERATION_DIRECTORY` names
/// another directory (an ext4 or XFS mount, say). Linux keeps generations on
/// ext4, XFS and btrfs, but none on tmpfs: on fbsource's remote-execution
/// workers `/tmp` is btrfs on both ISAs.
const GENERATION_DIRECTORY: &str = "/tmp";

fn generation_parent() -> String {
    std::env::var("HERMIT_TEST_GENERATION_DIRECTORY")
        .unwrap_or_else(|_| GENERATION_DIRECTORY.to_owned())
}

/// A directory on a FUSE file system, such as an EdenFS checkout, for a row
/// that only a run naming one has; RE workers offer none.
const FUSE_DIRECTORY_VARIABLE: &str = "HERMIT_TEST_FUSE_DIRECTORY";

/// A tmpfs, which keeps no generation, on the same workers.
const TMPFS_DIRECTORY: &str = "/dev/shm";

/// What the guest's buffer holds where the call writes nothing.
const UNWRITTEN: u8 = 0xaa;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap()
}

/// `ioctl(fd, request, buffer)`, with the full 64-bit `request` in its
/// register, and an eight-byte `long` buffer filled with [`UNWRITTEN`]: the
/// buffer, or the errno.
fn version_ioctl(fd: i32, request: u64) -> Result<[u8; 8], i32> {
    let mut buffer = [UNWRITTEN; 8];
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd, request, buffer.as_mut_ptr()) };
    if rc == 0 { Ok(buffer) } else { Err(errno()) }
}

fn ino_of_fd(fd: i32) -> u64 {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0, "fstat({fd})");
    stat.st_ino
}

/// One line per call: the generation as the `int` Linux writes, the four
/// bytes after it in hex, and the descriptor's `st_ino`; or its errno.
fn report(label: &str, fd: i32, request: u64) -> String {
    match version_ioctl(fd, request) {
        Ok(buffer) => format!(
            "{label} version={} rest={:02x}{:02x}{:02x}{:02x} ino={}\n",
            i32::from_ne_bytes(buffer[..4].try_into().unwrap()),
            buffer[4],
            buffer[5],
            buffer[6],
            buffer[7],
            ino_of_fd(fd),
        ),
        Err(errno) => format!("{label} errno={errno}\n"),
    }
}

/// `FS_IOC_GETVERSION` into the last four bytes of a mapping, the page after
/// which is unmapped: Linux writes an `int`, so the call succeeds.
fn report_at_mapping_end(label: &str, fd: i32) -> String {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
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
    let buffer = unsafe { map.cast::<u8>().add(page - 4).cast::<i32>() };
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd, FS_IOC_GETVERSION, buffer) };
    let line = if rc == 0 {
        format!(
            "{label} version={} ino={}\n",
            unsafe { *buffer },
            ino_of_fd(fd)
        )
    } else {
        format!("{label} errno={}\n", errno())
    };
    unsafe { libc::munmap(map, page) };
    line
}

/// What backs the two pages an `int` straddles in [`report_straddling`].
#[derive(Clone, Copy)]
enum Straddle {
    /// Two private anonymous pages, readable and writable.
    Anonymous,
    /// A second page that maps an empty memfd, past its end: a store faults.
    PastEof,
    /// A second page made read-only.
    ReadOnlySecond,
    /// A second page made write-only.
    WriteOnlySecond,
    /// A first page made write-only.
    WriteOnlyFirst,
    /// A first page made read-only.
    ReadOnlyFirst,
    /// A first page that is not mapped.
    UnmappedFirst,
    /// A first page made write-only, and a second made read-only.
    WriteOnlyFirstReadOnlySecond,
}

/// `FS_IOC_GETVERSION` into an `int` that straddles two pages laid out as
/// `layout`, its bytes holding 0x5a. Linux publishes the `int` whole where
/// both pages take a store, or answers EFAULT and leaves the bytes alone: the
/// two on the first page are printed, or, where it is not mapped, the two on
/// the second.
fn report_straddling(label: &str, fd: i32, layout: Straddle) -> String {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
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
    let first = map.cast::<u8>();
    let second = unsafe { first.add(page) };
    let buffer = unsafe { second.sub(2) };
    unsafe { std::ptr::write_bytes(buffer, 0x5a, 4) };
    let protect = |at: *mut u8, prot| {
        assert_eq!(
            unsafe { libc::mprotect(at.cast(), page, prot) },
            0,
            "mprotect"
        );
    };
    match layout {
        Straddle::Anonymous => {}
        Straddle::PastEof => {
            let memfd = unsafe { libc::memfd_create(c"past-eof".as_ptr(), 0) };
            assert!(memfd >= 0, "memfd_create: {}", errno());
            let fixed = unsafe {
                libc::mmap(
                    second.cast(),
                    page,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    memfd,
                    0,
                )
            };
            assert_eq!(fixed, second.cast(), "mmap(MAP_FIXED): {}", errno());
            unsafe { libc::close(memfd) };
        }
        Straddle::ReadOnlySecond => protect(second, libc::PROT_READ),
        Straddle::WriteOnlySecond => protect(second, libc::PROT_WRITE),
        Straddle::WriteOnlyFirst => protect(first, libc::PROT_WRITE),
        Straddle::ReadOnlyFirst => protect(first, libc::PROT_READ),
        Straddle::UnmappedFirst => {
            assert_eq!(unsafe { libc::munmap(first.cast(), page) }, 0, "munmap");
        }
        Straddle::WriteOnlyFirstReadOnlySecond => {
            protect(first, libc::PROT_WRITE);
            protect(second, libc::PROT_READ);
        }
    }
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd, FS_IOC_GETVERSION, buffer) };
    let line = if rc == 0 {
        format!(
            "{label} version={} ino={}\n",
            unsafe { buffer.cast::<i32>().read_unaligned() },
            ino_of_fd(fd)
        )
    } else if let Straddle::UnmappedFirst = layout {
        format!(
            "{label} errno={} second={:02x}{:02x}\n",
            errno(),
            unsafe { *second },
            unsafe { *second.add(1) }
        )
    } else {
        format!(
            "{label} errno={} first={:02x}{:02x}\n",
            errno(),
            unsafe { *buffer },
            unsafe { *buffer.add(1) }
        )
    };
    unsafe { libc::munmap(map, 2 * page) };
    line
}

/// A tag for a pointer's top byte.
const TAG: usize = 0x2a << 56;

/// `FS_IOC_GETVERSION` on `fd` through a pointer with [`TAG`] in its top byte
/// to an otherwise writable `int`, its bytes holding 0x5a. Linux answers
/// EFAULT and writes nothing: `access_ok` refuses the pointer, past user
/// memory on x86-64 and tagged on an arm64 thread without the tagged-address
/// ABI.
fn report_tagged(label: &str, fd: i32) -> String {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(map, libc::MAP_FAILED);
    let buffer = map.cast::<u8>();
    unsafe { std::ptr::write_bytes(buffer, 0x5a, 4) };
    let tagged = buffer as usize | TAG;
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd, FS_IOC_GETVERSION, tagged) };
    let errno = if rc == 0 { 0 } else { errno() };
    let bytes = unsafe { std::slice::from_raw_parts(buffer, 4) };
    let line = format!(
        "{label} rc={rc} errno={errno} bytes={:02x}{:02x}{:02x}{:02x}\n",
        bytes[0], bytes[1], bytes[2], bytes[3]
    );
    unsafe { libc::munmap(map, page) };
    line
}

/// `PKEY_DISABLE_WRITE` (include/uapi/asm-generic/mman-common.h).
const PKEY_DISABLE_WRITE: usize = 0x2;

/// `FS_IOC_GETVERSION` on `fd` into an `int` on a page whose protection key
/// denies writes, its bytes holding 0x5a; or why no key could be allocated.
fn report_protection_key(label: &str, fd: i32) -> String {
    let key = unsafe { libc::syscall(libc::SYS_pkey_alloc, 0, PKEY_DISABLE_WRITE) };
    if key < 0 {
        return format!("{label} pkey_alloc_errno={}\n", errno());
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(map, libc::MAP_FAILED);
    let buffer = map.cast::<u8>();
    unsafe { std::ptr::write_bytes(buffer, 0x5a, 4) };
    let keyed = unsafe {
        libc::syscall(
            libc::SYS_pkey_mprotect,
            map,
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            key,
        )
    };
    assert_eq!(keyed, 0, "pkey_mprotect: {}", errno());
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd, FS_IOC_GETVERSION, buffer) };
    let line = if rc == 0 {
        format!("{label} version={}\n", unsafe {
            buffer.cast::<i32>().read_unaligned()
        })
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(buffer, 4) };
        format!(
            "{label} errno={} bytes={:02x}{:02x}{:02x}{:02x}\n",
            errno(),
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3]
        )
    };
    unsafe { libc::munmap(map, page) };
    unsafe { libc::syscall(libc::SYS_pkey_free, key) };
    line
}

/// A read before the guest allocates a protection key, one into a page whose
/// key denies writes, and one after.
fn guest_with_protection_keys(dir: &Path) {
    let file = std::fs::File::create(dir.join("keyed.txt")).unwrap();
    let mut out = report("before_keys", file.as_raw_fd(), FS_IOC_GETVERSION);
    out += &report_protection_key("keyed", file.as_raw_fd());
    out += &report("after_keys", file.as_raw_fd(), FS_IOC_GETVERSION);
    std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes()).unwrap();
}

/// `FS_IOC_GETVERSION` on a pidfd of the guest, labelled by whether the kernel
/// makes pidfds regular files: those of fbsource's 6.16 and 7.1.3 workers have
/// mode 0700, with no file type.
fn report_pidfd() -> String {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) } as i32;
    if pidfd < 0 {
        return format!("pidfd open_errno={}\n", errno());
    }
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(pidfd, &mut stat) }, 0);
    let label = if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
        "pidfd_reg"
    } else {
        "pidfd_untyped"
    };
    let line = report(label, pidfd, FS_IOC_GETVERSION);
    unsafe { libc::close(pidfd) };
    line
}

/// `FS_IOC_SETVERSION` with 12345, then `FS_IOC_GETVERSION`: a set that
/// succeeded would make the read disagree with the virtual value.
fn report_set_then_get(fd: i32) -> String {
    let value: libc::c_long = 12345;
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd, FS_IOC_SETVERSION, &value) };
    let set = if rc == 0 {
        "set_then_get set=ok\n".to_owned()
    } else {
        format!("set_then_get set_errno={}\n", errno())
    };
    set + &report("get_after_set", fd, FS_IOC_GETVERSION)
}

/// The guest: files it creates, as rr's `ioctl_fs` test creates one, and
/// their directory; one deleted and made again; every generation number; then
/// the calls whose errors Linux decides.
fn guest(dir: &Path) {
    let mut out = String::new();
    let first = std::fs::File::create(dir.join("dummy.txt")).unwrap();
    let second = std::fs::File::create(dir.join("second.txt")).unwrap();
    let directory = std::fs::File::open(dir).unwrap();
    out += &report("first", first.as_raw_fd(), FS_IOC_GETVERSION);
    out += &report("second", second.as_raw_fd(), FS_IOC_GETVERSION);
    out += &report("directory", directory.as_raw_fd(), FS_IOC_GETVERSION);
    out += &report(
        "high_bits",
        first.as_raw_fd(),
        HIGH_BITS | FS_IOC_GETVERSION,
    );
    out += &report("ext4", first.as_raw_fd(), EXT4_IOC_GETVERSION);
    out += &report("fs32", first.as_raw_fd(), FS_IOC32_GETVERSION);
    out += &report("ext4_32", first.as_raw_fd(), EXT4_IOC32_GETVERSION);
    out += &report_at_mapping_end("mapping_end", first.as_raw_fd());
    for (label, layout) in [
        ("straddle", Straddle::Anonymous),
        ("straddle_eof", Straddle::PastEof),
        ("straddle_ro", Straddle::ReadOnlySecond),
        ("straddle_wo", Straddle::WriteOnlySecond),
        ("straddle_wo_first", Straddle::WriteOnlyFirst),
        ("straddle_ro_first", Straddle::ReadOnlyFirst),
        ("straddle_unmapped_first", Straddle::UnmappedFirst),
        (
            "straddle_wo_first_ro",
            Straddle::WriteOnlyFirstReadOnlySecond,
        ),
    ] {
        out += &report_straddling(label, first.as_raw_fd(), layout);
    }

    let recreated = dir.join("recreated.txt");
    let before = std::fs::File::create(&recreated).unwrap();
    out += &report("recreated_before", before.as_raw_fd(), FS_IOC_GETVERSION);
    drop(before);
    std::fs::remove_file(&recreated).unwrap();
    let after = std::fs::File::create(&recreated).unwrap();
    out += &report("recreated_after", after.as_raw_fd(), FS_IOC_GETVERSION);

    let tmpfs_dir = tempfile::Builder::new()
        .prefix("fs-ioc-getversion-")
        .tempdir_in(TMPFS_DIRECTORY)
        .unwrap();
    let on_tmpfs = std::fs::File::create(tmpfs_dir.path().join("file")).unwrap();
    out += &report("tmpfs", on_tmpfs.as_raw_fd(), FS_IOC_GETVERSION);

    out += &report_pidfd();
    if let Ok(fuse_dir) = std::env::var(FUSE_DIRECTORY_VARIABLE) {
        let on_fuse = tempfile::tempfile_in(fuse_dir).unwrap();
        out += &report("fuse", on_fuse.as_raw_fd(), FS_IOC_GETVERSION);
    }

    let path = CString::new(dir.join("dummy.txt").as_os_str().as_bytes()).unwrap();
    let o_path = unsafe { libc::open(path.as_ptr(), libc::O_PATH) };
    assert!(o_path >= 0, "open(O_PATH): {}", errno());
    // Arguments at the very top of the address space: the last three bytes,
    // and the start of the last page.
    for (label, argument) in [
        ("top_minus_2", usize::MAX - 2),
        ("top_page", usize::MAX & !4095),
    ] {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_ioctl,
                first.as_raw_fd(),
                FS_IOC_GETVERSION,
                argument,
            )
        };
        out += &format!("{label} rc={rc} errno={}\n", errno());
    }
    out += &report_tagged("tagged", first.as_raw_fd());
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, first.as_raw_fd(), FS_IOC_GETVERSION, 0) };
    out += &format!("null rc={rc} errno={}\n", errno());
    out += &report("o_path", o_path, FS_IOC_GETVERSION);
    // The sets go to `second`, so that where one reaches a host that takes it,
    // `first` keeps the host's generation.
    for (label, set) in SETS {
        out += &report(label, second.as_raw_fd(), set);
        out += &report(
            &format!("{label}_high_bits"),
            second.as_raw_fd(),
            HIGH_BITS | set,
        );
        out += &report(&format!("{label}_o_path"), o_path, set);
    }
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    out += &report("set_pipe", pipe[0], FS_IOC_SETVERSION);
    out += &report_set_then_get(second.as_raw_fd());
    unsafe { libc::close(o_path) };
    std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes()).unwrap();
}

/// [`guest`], with a sibling thread that runs until it ends.
fn guest_beside_a_sibling(dir: &Path) {
    let stop = Arc::new(AtomicBool::new(false));
    let turns = Arc::new(AtomicUsize::new(0));
    let sibling = {
        let (stop, turns) = (Arc::clone(&stop), Arc::clone(&turns));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                turns.fetch_add(1, Ordering::SeqCst);
                unsafe { libc::sched_yield() };
            }
        })
    };
    while turns.load(Ordering::SeqCst) == 0 {
        unsafe { libc::sched_yield() };
    }
    guest(dir);
    stop.store(true, Ordering::SeqCst);
    sibling.join().unwrap();
    println!("sibling ran={}", turns.load(Ordering::SeqCst) > 0);
}

/// A fresh directory under [`generation_parent`], after checking that the
/// host keeps generations there: otherwise every run would read ENOTTY and
/// this test would show nothing, so it fails, naming the host limitation.
fn generation_directory() -> tempfile::TempDir {
    let parent = generation_parent();
    let dir = tempfile::Builder::new()
        .prefix("fs-ioc-getversion-")
        .tempdir_in(&parent)
        .unwrap();
    let probe = std::fs::File::create(dir.path().join("probe")).unwrap();
    if let Err(errno) = version_ioctl(probe.as_raw_fd(), FS_IOC_GETVERSION) {
        panic!(
            "host limitation: {parent} keeps no inode generations \
             (FS_IOC_GETVERSION failed with errno {errno}); this test needs a file \
             system that does, such as ext4, XFS or btrfs"
        );
    }
    std::fs::remove_file(dir.path().join("probe")).unwrap();
    dir
}

fn config(virtualize_metadata: bool, sequentialize_threads: bool) -> Config {
    // CPUID is not under test; leaving it to the host lets the test run
    // where CPUID faulting is unavailable. A refusal answers EOPNOTSUPP. The
    // tests run on ptrace, and say so, as Hermit does for a ptrace run.
    detcore_testutils::prepare_ptrace_timer_config(Config {
        backend: reverie::BackendCapabilities::PTRACE,
        sequentialize_threads,
        max_timeslice: None,
        virtualize_cpuid: false,
        virtualize_metadata,
        panic_on_unsupported_syscalls: false,
        ..Default::default()
    })
}

/// One run of `guest` under `Detcore<T>` in a fresh directory, returning its
/// stdout and the directory, which outlives the run.
fn run_with<T>(config: Config, guest: fn(&Path)) -> (String, Arc<tempfile::TempDir>)
where
    T: Tool<GlobalState = detcore::GlobalState> + 'static,
{
    let dir = Arc::new(generation_directory());
    let guest_dir = Arc::clone(&dir);
    let (output, _) = detcore_testutils::test_fn_with_config::<T, _>(
        move || guest(guest_dir.path()),
        config,
        true,
    )
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (stdout, dir)
}

fn run(config: Config) -> (String, Arc<tempfile::TempDir>) {
    run_with::<Detcore>(config, guest)
}

/// The value after `version=` and after `ino=` on the line starting with
/// `label`, and the four bytes after the `int` it wrote, if any.
fn version_line(stdout: &str, label: &str) -> (i32, Option<String>, u64) {
    let line = stdout
        .lines()
        .find(|line| line.starts_with(&format!("{label} ")))
        .unwrap_or_else(|| panic!("no {label} line in {stdout:?}"));
    let field = |name: &str| {
        line.split_whitespace()
            .find_map(|word| word.strip_prefix(name))
            .map(str::to_owned)
    };
    (
        field("version=")
            .unwrap_or_else(|| panic!("no version in {line:?}"))
            .parse()
            .unwrap(),
        field("rest="),
        field("ino=")
            .unwrap_or_else(|| panic!("no ino in {line:?}"))
            .parse()
            .unwrap(),
    )
}

fn has_line(stdout: &str, expected: &str) {
    assert!(
        stdout.lines().any(|line| line == expected),
        "no {expected:?} line\n{stdout}"
    );
}

/// The file system `dir` is on, by its `f_type`.
fn f_type(dir: &Path) -> i64 {
    let path = CString::new(dir.as_os_str().as_bytes()).unwrap();
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statfs(path.as_ptr(), &mut statfs) }, 0);
    let f_type: i64 = statfs.f_type;
    f_type
}

#[test]
fn a_new_files_generation_is_its_virtual_inode_number_in_every_run() {
    // A tagged pointer is EFAULT natively, with nothing written; the run
    // below must answer the same.
    let native_dir = generation_directory();
    let native_file = std::fs::File::create(native_dir.path().join("native.txt")).unwrap();
    let native_tagged = report_tagged("tagged", native_file.as_raw_fd());
    assert_eq!(
        native_tagged,
        format!("tagged rc=-1 errno={} bytes=5a5a5a5a\n", libc::EFAULT)
    );
    let (first_run, first_dir) = run(config(true, true));
    let (second_run, _second_dir) = run(config(true, true));
    assert_eq!(first_run, second_run, "two runs read different generations");
    // EXT4_IOC_GETVERSION is answered by ext4 alone.
    let on_ext4 = f_type(first_dir.path()) == EXT4_SUPER_MAGIC;
    let mut virtual_labels = vec![
        "first",
        "second",
        "directory",
        "high_bits",
        "mapping_end",
        "straddle",
        "straddle_wo",
        "straddle_wo_first",
        "recreated_before",
        "recreated_after",
    ];
    if on_ext4 {
        virtual_labels.push("ext4");
    } else {
        has_line(&first_run, &format!("ext4 errno={}", libc::ENOTTY));
    }
    for label in virtual_labels {
        let (version, rest, ino) = version_line(&first_run, label);
        assert_eq!(
            version, ino as u32 as i32,
            "{label}: the generation is not the low 32 bits of the virtual inode number \
             {ino}\n{first_run}"
        );
        if let Some(rest) = rest {
            assert_eq!(
                rest, "aaaaaaaa",
                "{label}: the call wrote past the int Linux writes\n{first_run}"
            );
        }
    }
    let version = |label| version_line(&first_run, label).0;
    assert_ne!(version("first"), version("second"), "{first_run}");
    // A file deleted and made again is a new file, with a new generation, as
    // Linux gives it: Detcore never gives a virtual inode number twice.
    assert_ne!(
        version("recreated_before"),
        version("recreated_after"),
        "{first_run}"
    );
    // A pidfd's generation is pidfs's 0 on a 64-bit kernel, from the kernel:
    // where pidfds are regular files, Detcore tells pidfs by its fstatfs and
    // sends the call on; elsewhere the call goes on before that.
    let pidfd = first_run
        .lines()
        .find(|line| line.starts_with("pidfd"))
        .unwrap_or_else(|| panic!("no pidfd line\n{first_run}"));
    assert!(
        pidfd.starts_with("pidfd_reg version=0 ")
            || pidfd.starts_with("pidfd_untyped version=0 ")
            || pidfd == format!("pidfd_untyped errno={}", libc::ENOTTY),
        "{first_run}"
    );
    // A FUSE daemon decides what the call writes, so Detcore refuses it.
    if std::env::var(FUSE_DIRECTORY_VARIABLE).is_ok() {
        has_line(&first_run, &format!("fuse errno={}", libc::EOPNOTSUPP));
    }
    // tmpfs keeps no generation, and no file system answers a 4-byte encoding
    // for a 64-bit task: their ENOTTY is the guest's answer. Linux decides
    // every other error: EFAULT for an argument it cannot write, EBADF for an
    // O_PATH descriptor.
    for expected in [
        format!("tmpfs errno={}", libc::ENOTTY),
        format!("fs32 errno={}", libc::ENOTTY),
        format!("ext4_32 errno={}", libc::ENOTTY),
        format!("null rc=-1 errno={}", libc::EFAULT),
        format!("top_minus_2 rc=-1 errno={}", libc::EFAULT),
        format!("top_page rc=-1 errno={}", libc::EFAULT),
        native_tagged.trim_end().to_owned(),
        format!("straddle_eof errno={} first=5a5a", libc::EFAULT),
        format!("straddle_ro errno={} first=5a5a", libc::EFAULT),
        format!("straddle_ro_first errno={} first=5a5a", libc::EFAULT),
        format!("straddle_unmapped_first errno={} second=5a5a", libc::EFAULT),
        format!("straddle_wo_first_ro errno={} first=5a5a", libc::EFAULT),
        format!("o_path errno={}", libc::EBADF),
    ] {
        has_line(&first_run, &expected);
    }
}

/// Design decision: every set number on a regular file fails with ENOTTY, as
/// `FS_IOC_SETVERSION` does on btrfs or XFS, whatever the host would do, so a
/// later read never answers a value Linux would not after a set; an `O_PATH`
/// descriptor gets EBADF first. On a host where a set succeeds (ext2, or an
/// ext4 without `metadata_csum`), a set that reached it would show here as
/// `set=ok`, and the read after it would disagree with the virtual value.
#[test]
fn setversion_fails_with_enotty_by_design() {
    let (stdout, _dir) = run(config(true, true));
    for (label, _) in SETS {
        has_line(&stdout, &format!("{label} errno={}", libc::ENOTTY));
        has_line(
            &stdout,
            &format!("{label}_high_bits errno={}", libc::ENOTTY),
        );
        has_line(&stdout, &format!("{label}_o_path errno={}", libc::EBADF));
    }
    has_line(&stdout, &format!("set_then_get set_errno={}", libc::ENOTTY));
    let (version, _, ino) = version_line(&stdout, "get_after_set");
    assert_eq!(version, ino as u32 as i32, "{stdout}");
    // A pipe is not a file: the kernel answers.
    has_line(&stdout, &format!("set_pipe errno={}", libc::ENOTTY));
}

#[test]
fn without_virtual_metadata_the_hosts_generation_passes_through() {
    let (stdout, dir) = run(config(false, true));
    let (version, rest, _) = version_line(&stdout, "first");
    let file = std::fs::File::open(dir.path().join("dummy.txt")).unwrap();
    let host = version_ioctl(file.as_raw_fd(), FS_IOC_GETVERSION).unwrap();
    assert_eq!(
        version,
        i32::from_ne_bytes(host[..4].try_into().unwrap()),
        "the guest did not read the host's generation\n{stdout}"
    );
    assert_eq!(rest.as_deref(), Some("aaaaaaaa"), "{stdout}");
}

/// Natively, a page whose protection key denies the thread writes takes no
/// byte of the `int`: EFAULT. Detcore's store does not see protection keys,
/// so once the address space has allocated one, every generation read is
/// refused by name (EOPNOTSUPP where unsupported operations are allowed)
/// before any store, and that page keeps its bytes too. A read before the
/// first key is answered. On a host without protection keys `pkey_alloc`
/// fails natively and under Detcore with the same errno, every read is
/// answered, and the test says so.
#[test]
fn a_read_in_an_address_space_with_protection_keys_is_refused_before_any_store() {
    let native_dir = generation_directory();
    let native_file = std::fs::File::create(native_dir.path().join("native.txt")).unwrap();
    let native = report_protection_key("keyed", native_file.as_raw_fd());
    let keys = !native.contains("pkey_alloc_errno=");
    if keys {
        assert_eq!(
            native,
            format!("keyed errno={} bytes=5a5a5a5a\n", libc::EFAULT)
        );
    } else {
        eprintln!("host limitation: no protection keys here ({native:?})");
    }
    let (stdout, _dir) = run_with::<Detcore>(config(true, true), guest_with_protection_keys);
    let (version, _, ino) = version_line(&stdout, "before_keys");
    assert_eq!(version, ino as u32 as i32, "{stdout}");
    if keys {
        has_line(
            &stdout,
            &format!("keyed errno={} bytes=5a5a5a5a", libc::EOPNOTSUPP),
        );
        has_line(&stdout, &format!("after_keys errno={}", libc::EOPNOTSUPP));
    } else {
        has_line(&stdout, native.trim_end());
        let (version, _, ino) = version_line(&stdout, "after_keys");
        assert_eq!(version, ino as u32 as i32, "{stdout}");
    }
}

/// An EINTR from the generation read itself, which ran, is the guest's
/// answer, as Linux returns it: Detcore does not run the read again.
/// [`InterruptedReadProbe`] answers the first staged read with EINTR; the
/// guest's next read is answered.
#[test]
fn an_interrupted_read_is_the_guests_answer() {
    let (stdout, _dir) = run_with::<Detcore<InterruptedReadProbe>>(config(true, true), guest);
    has_line(&stdout, &format!("first errno={}", libc::EINTR));
    let (version, _, ino) = version_line(&stdout, "second");
    assert_eq!(version, ino as u32 as i32, "{stdout}");
}

/// A restart code from the staged read does not say whether the call ran,
/// on any file system (a security hook may return one before the handler):
/// Detcore refuses the call by name and does not run it again, and the guest
/// sees no restart code. [`NotRunReadProbe`] answers the first staged read,
/// on the generation directory, with ERESTARTSYS; the read after it is
/// answered, and the probe sees two reads in all.
#[test]
fn a_restart_code_on_an_audited_file_system_is_refused_without_a_second_run() {
    let (stdout, _dir) =
        run_with::<Detcore<NotRunReadProbe>>(config(true, true), audited_restart_guest);
    has_line(&stdout, &format!("audited errno={}", libc::EOPNOTSUPP));
    let (version, _, ino) = version_line(&stdout, "after");
    assert_eq!(version, ino as u32 as i32, "{stdout}");
    assert_eq!(NOT_RUN_READS.load(Ordering::SeqCst), 2, "{stdout}");
}

/// Two reads on files in the generation directory.
fn audited_restart_guest(dir: &Path) {
    let audited = std::fs::File::create(dir.join("audited.txt")).unwrap();
    let mut out = report("audited", audited.as_raw_fd(), FS_IOC_GETVERSION);
    let after = std::fs::File::create(dir.join("after.txt")).unwrap();
    out += &report("after", after.as_raw_fd(), FS_IOC_GETVERSION);
    std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes()).unwrap();
}

/// A read a signal stops before it runs every time is refused by name each
/// time, never answered with a backend's code, and never run a second time:
/// [`NeverRunReadProbe`] answers every staged read with ERESTARTSYS, and sees
/// exactly as many as the same guest makes when no read is interrupted
/// ([`CountingReadProbe`]).
#[test]
fn a_read_stopped_before_it_ran_every_time_is_refused() {
    let (stdout, _dir) = run_with::<Detcore<NeverRunReadProbe>>(config(true, true), guest);
    for label in ["first", "second", "directory", "high_bits"] {
        has_line(&stdout, &format!("{label} errno={}", libc::EOPNOTSUPP));
    }
    let (counted, _dir) = run_with::<Detcore<CountingReadProbe>>(config(true, true), guest);
    let (version, _, ino) = version_line(&counted, "first");
    assert_eq!(version, ino as u32 as i32, "{counted}");
    let (interrupted, uninterrupted) = (
        NEVER_RUN_READS.load(Ordering::SeqCst),
        COUNTED_READS.load(Ordering::SeqCst),
    );
    assert!(uninterrupted > 0, "{counted}");
    assert_eq!(
        interrupted, uninterrupted,
        "a refused read ran a second time\n{stdout}"
    );
}

/// The same on tmpfs, whose (absent) handler Detcore has audited to answer
/// only ENOTTY: a restart code from the staged read is refused by name with
/// no second run. [`OtherRestartProbe`] answers the first staged read, the
/// tmpfs one, with ERESTARTSYS; the read after it, on the generation
/// directory, is answered, and the probe sees two reads in all.
#[test]
fn a_restart_code_on_a_file_system_without_a_handler_is_refused_without_a_second_run() {
    let (stdout, _dir) =
        run_with::<Detcore<OtherRestartProbe>>(config(true, true), no_handler_restart_guest);
    has_line(&stdout, &format!("tmpfs errno={}", libc::EOPNOTSUPP));
    let (version, _, ino) = version_line(&stdout, "after");
    assert_eq!(version, ino as u32 as i32, "{stdout}");
    assert_eq!(OTHER_RESTART_READS.load(Ordering::SeqCst), 2, "{stdout}");
}

/// One off-allowlist probe of the test below: its name, its run, and the
/// count of generation ioctls it sent.
type MagicProbeRun = (&'static str, fn() -> String, &'static AtomicUsize);

/// A read on a file system off Detcore's allowlist is refused by name, and
/// the generation ioctl is never sent; Detcore's `fstat` and `fstatfs` probes
/// still reach the file system. Here eCryptfs (whose handler forwards the call
/// to the file system below it and copies attributes back even on an error),
/// overlayfs, and an unknown magic: each probe rewrites the `f_type` of
/// Detcore's `fstatfs` to its magic and counts generation ioctls sent: none.
#[test]
fn a_read_on_a_file_system_off_the_allowlist_is_refused_before_it_runs() {
    let runs: [MagicProbeRun; 3] = [
        (
            "eCryptfs",
            || run_with::<Detcore<EcryptfsMagicProbe>>(config(true, true), audited_restart_guest).0,
            &ECRYPTFS_READS,
        ),
        (
            "overlayfs",
            || {
                run_with::<Detcore<OverlayfsMagicProbe>>(config(true, true), audited_restart_guest)
                    .0
            },
            &OVERLAYFS_READS,
        ),
        (
            "an unknown magic",
            || run_with::<Detcore<UnknownMagicProbe>>(config(true, true), audited_restart_guest).0,
            &UNKNOWN_MAGIC_READS,
        ),
    ];
    for (name, run, reads) in runs {
        let stdout = run();
        for label in ["audited", "after"] {
            has_line(&stdout, &format!("{label} errno={}", libc::EOPNOTSUPP));
        }
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "a generation read ran on {name}\n{stdout}"
        );
    }
}

/// A probe that rewrites the `f_type` Detcore's `fstatfs` reads into its
/// staging page to `$magic` and counts the generation reads into the page
/// that reach the kernel.
macro_rules! magic_probe {
    ($probe:ident, $seen:ident, $page:ident, $magic:expr) => {
        static $seen: AtomicUsize = AtomicUsize::new(0);
        static $page: AtomicUsize = AtomicUsize::new(0);

        #[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
        struct $probe;

        #[reverie::tool]
        impl Tool for $probe {
            type GlobalState = detcore::GlobalState;
            type ThreadState = ();

            fn subscriptions(_config: &detcore::Config) -> Subscription {
                Subscription::none()
            }

            async fn handle_syscall_event<G: Guest<Self>>(
                &self,
                guest: &mut G,
                call: Syscall,
            ) -> Result<i64, Error> {
                let (_, args) = call.into_parts();
                let page = $page.load(Ordering::SeqCst);
                let statfs_at = page + std::mem::size_of::<libc::stat>();
                match call {
                    Syscall::Mmap(_) if args.arg1 == STAGING_LEN => {
                        let mapped = guest.inject(call).await?;
                        $page.store(mapped as usize, Ordering::SeqCst);
                        Ok(mapped)
                    }
                    Syscall::Fstatfs(_) if page != 0 && args.arg1 == statfs_at => {
                        let result = guest.inject(call).await?;
                        let at =
                            reverie::syscalls::AddrMut::<libc::__fsword_t>::from_raw(statfs_at)
                                .unwrap();
                        guest.memory().write_value(at, &$magic)?;
                        Ok(result)
                    }
                    Syscall::Ioctl(_)
                        if page != 0
                            && args.arg2 == page + STAGED_OUTPUT
                            && is_generation_number(args.arg1 as u32) =>
                    {
                        $seen.fetch_add(1, Ordering::SeqCst);
                        Ok(guest.inject(call).await?)
                    }
                    _ => Ok(guest.inject(call).await?),
                }
            }
        }
    };
}

magic_probe!(EcryptfsMagicProbe, ECRYPTFS_READS, ECRYPTFS_PAGE, 0xF15F);
magic_probe!(
    OverlayfsMagicProbe,
    OVERLAYFS_READS,
    OVERLAYFS_PAGE,
    0x794C_7630
);
magic_probe!(
    UnknownMagicProbe,
    UNKNOWN_MAGIC_READS,
    UNKNOWN_MAGIC_PAGE,
    0x1234_5678
);

/// A read on a tmpfs file, then one on a file in the generation directory.
fn no_handler_restart_guest(dir: &Path) {
    let tmpfs_dir = tempfile::Builder::new()
        .prefix("fs-ioc-getversion-")
        .tempdir_in(TMPFS_DIRECTORY)
        .unwrap();
    let on_tmpfs = std::fs::File::create(tmpfs_dir.path().join("file")).unwrap();
    let mut out = report("tmpfs", on_tmpfs.as_raw_fd(), FS_IOC_GETVERSION);
    let after = std::fs::File::create(dir.join("after.txt")).unwrap();
    out += &report("after", after.as_raw_fd(), FS_IOC_GETVERSION);
    std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes()).unwrap();
}

/// An interruption of Detcore's own `munmap` of the staging page is no
/// failure of the cleanup: Detcore runs it again, and the run goes on.
/// [`InterruptedUnmapProbe`] answers the first two with EINTR and
/// ERESTARTSYS.
#[test]
fn an_interrupted_unmap_of_the_staging_page_is_run_again() {
    let (stdout, _dir) = run_with::<Detcore<InterruptedUnmapProbe>>(config(true, true), guest);
    let (version, _, ino) = version_line(&stdout, "first");
    assert_eq!(version, ino as u32 as i32, "{stdout}");
    has_line(&stdout, &format!("set_pipe errno={}", libc::ENOTTY));
    assert!(INTERRUPTED_UNMAPS.load(Ordering::SeqCst) >= 3, "{stdout}");
}

/// Without sequentialized threads a sibling thread runs beside every call
/// Detcore would make into the guest's memory, so every generation call is
/// refused by name (EOPNOTSUPP where unsupported operations are allowed)
/// before any of them, whatever the descriptor. A sibling thread runs
/// throughout, and [`UnsequentializedRaceProbe`] does at the worst moment what
/// it could do to a staging page: it finds none to read or change, and no
/// generation call reaches the kernel.
#[test]
fn without_sequentialized_threads_every_generation_call_is_refused_before_staging() {
    UNSEQUENTIALIZED_RACES.lock().unwrap().clear();
    let (stdout, _dir) =
        run_with::<Detcore<UnsequentializedRaceProbe>>(config(true, false), guest_beside_a_sibling);
    let races = UNSEQUENTIALIZED_RACES.lock().unwrap().clone();
    assert!(races.is_empty(), "{races:#?}\n{stdout}");
    for label in ["first", "directory", "ext4", "fs32", "tmpfs", "o_path"] {
        has_line(&stdout, &format!("{label} errno={}", libc::EOPNOTSUPP));
    }
    for (label, _) in SETS {
        has_line(&stdout, &format!("{label} errno={}", libc::EOPNOTSUPP));
        has_line(
            &stdout,
            &format!("{label}_high_bits errno={}", libc::EOPNOTSUPP),
        );
        has_line(
            &stdout,
            &format!("{label}_o_path errno={}", libc::EOPNOTSUPP),
        );
    }
    has_line(
        &stdout,
        &format!("set_then_get set_errno={}", libc::EOPNOTSUPP),
    );
    has_line(&stdout, &format!("set_pipe errno={}", libc::EOPNOTSUPP));
    has_line(&stdout, "sibling ran=true");
}

/// On a backend where Detcore has not proven this route (KVM's, whose
/// `host_task_reads_reach_guest_threads` is false), every generation call is
/// refused by name, before any call: none reaches the kernel, and no staging
/// page is mapped.
#[test]
fn on_a_backend_without_the_proven_route_every_generation_call_is_refused() {
    ROUTE_RACES.lock().unwrap().clear();
    let mut config = config(true, true);
    config.backend.host_task_reads_reach_guest_threads = false;
    let (stdout, _dir) = run_with::<Detcore<RouteRaceProbe>>(config, guest);
    for label in ["first", "directory", "ext4", "fs32", "tmpfs", "o_path"] {
        has_line(&stdout, &format!("{label} errno={}", libc::EOPNOTSUPP));
    }
    for (label, _) in SETS {
        has_line(&stdout, &format!("{label} errno={}", libc::EOPNOTSUPP));
        has_line(
            &stdout,
            &format!("{label}_o_path errno={}", libc::EOPNOTSUPP),
        );
    }
    has_line(&stdout, &format!("set_pipe errno={}", libc::EOPNOTSUPP));
    let races = ROUTE_RACES.lock().unwrap().clone();
    assert!(races.is_empty(), "{races:#?}\n{stdout}");
}

/// The generation reads, by the label the probe's guest prints for them.
const GETS: [(&str, u64); 4] = [
    ("get_fs", FS_IOC_GETVERSION),
    ("get_ext4", EXT4_IOC_GETVERSION),
    ("get_fs32", FS_IOC32_GETVERSION),
    ("get_ext4_32", EXT4_IOC32_GETVERSION),
];

fn is_generation_number(command: u32) -> bool {
    GETS.iter()
        .chain(SETS.iter())
        .any(|(_, number)| *number == u64::from(command))
}

/// A layer below Detcore, which issues the calls Detcore hands to the kernel,
/// and notes in `$calls` every generation read or set among them, as
/// (command, argument).
macro_rules! generation_call_probe {
    ($probe:ident, $calls:ident) => {
        static $calls: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

        #[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
        struct $probe;

        #[reverie::tool]
        impl Tool for $probe {
            type GlobalState = detcore::GlobalState;
            type ThreadState = ();

            fn subscriptions(_config: &detcore::Config) -> Subscription {
                Subscription::none()
            }

            async fn handle_syscall_event<G: Guest<Self>>(
                &self,
                guest: &mut G,
                call: Syscall,
            ) -> Result<i64, Error> {
                if let Syscall::Ioctl(_) = call {
                    let (_, args) = call.into_parts();
                    let command = args.arg1 as u32;
                    if is_generation_number(command) {
                        $calls.lock().unwrap().push((command, args.arg2));
                    }
                }
                Ok(guest.inject(call).await?)
            }
        }
    };
}

generation_call_probe!(GenerationCallProbe, PHYSICAL_GENERATION_CALLS);

/// A layer below Detcore that does, at the worst moment, what a sibling
/// thread running beside a call could do to a staging page in the guest's
/// memory, and notes in `$races` what it did: when Detcore maps the page, it
/// notes it (`$page`); after an `fstat` writes into it, it reads the host's
/// `stat` there and rewrites its file type as a FIFO. It also notes every
/// generation call that reaches the kernel.
macro_rules! race_probe {
    ($probe:ident, $races:ident, $page:ident) => {
        static $races: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static $page: AtomicUsize = AtomicUsize::new(0);

        #[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
        struct $probe;

        #[reverie::tool]
        impl Tool for $probe {
            type GlobalState = detcore::GlobalState;
            type ThreadState = ();

            fn subscriptions(_config: &detcore::Config) -> Subscription {
                Subscription::none()
            }

            async fn handle_syscall_event<G: Guest<Self>>(
                &self,
                guest: &mut G,
                call: Syscall,
            ) -> Result<i64, Error> {
                let (_, args) = call.into_parts();
                let page = $page.load(Ordering::SeqCst);
                match call {
                    Syscall::Mmap(_) if args.arg1 == STAGING_LEN => {
                        let mapped = guest.inject(call).await?;
                        $page.store(mapped as usize, Ordering::SeqCst);
                        $races
                            .lock()
                            .unwrap()
                            .push(format!("a staging page was mapped at {mapped:#x}"));
                        Ok(mapped)
                    }
                    Syscall::Fstat(_) if page != 0 && args.arg1 == page => {
                        let result = guest.inject(call).await?;
                        let mut memory = guest.memory();
                        let at = reverie::syscalls::Addr::<libc::stat>::from_raw(page).unwrap();
                        let mut stat = memory.read_value(at)?;
                        $races.lock().unwrap().push(format!(
                            "the host's stat (st_ino {}) was read in the staging page",
                            stat.st_ino
                        ));
                        stat.st_mode = libc::S_IFIFO | 0o600;
                        let at = reverie::syscalls::AddrMut::<libc::stat>::from_raw(page).unwrap();
                        memory.write_value(at, &stat)?;
                        Ok(result)
                    }
                    Syscall::Ioctl(_) if is_generation_number(args.arg1 as u32) => {
                        $races.lock().unwrap().push(format!(
                            "generation call {:#x} reached the kernel",
                            args.arg1 as u32
                        ));
                        Ok(guest.inject(call).await?)
                    }
                    _ => Ok(guest.inject(call).await?),
                }
            }
        }
    };
}

race_probe!(
    UnsequentializedRaceProbe,
    UNSEQUENTIALIZED_RACES,
    UNSEQUENTIALIZED_RACE_PAGE
);
race_probe!(RouteRaceProbe, ROUTE_RACES, ROUTE_RACE_PAGE);

/// The guest of [`the_host_generation_never_reaches_guest_memory`]: every
/// generation read and set on a file, each with a buffer of its own, printing
/// the address of each buffer it gave a read.
fn staging_guest(dir: &Path) {
    let file = std::fs::File::create(dir.join("staged.txt")).unwrap();
    let mut buffers = [[UNWRITTEN; 8]; 8];
    let (gets, sets) = buffers.split_at_mut(4);
    for ((label, number), buffer) in GETS.iter().zip(gets.iter_mut()) {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_ioctl,
                file.as_raw_fd(),
                *number,
                buffer.as_mut_ptr(),
            )
        };
        println!("{label} rc={rc} buffer={}", buffer.as_ptr() as usize);
    }
    for ((label, number), buffer) in SETS.iter().zip(sets.iter_mut()) {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_ioctl,
                file.as_raw_fd(),
                *number,
                buffer.as_mut_ptr(),
            )
        };
        println!("{label} rc={rc}");
    }
}

/// The kernel writes the host's generation only into Detcore's own page, never
/// into a guest buffer, and no set reaches it.
#[test]
fn the_host_generation_never_reaches_guest_memory() {
    PHYSICAL_GENERATION_CALLS.lock().unwrap().clear();
    let (stdout, _dir) =
        run_with::<Detcore<GenerationCallProbe>>(config(true, true), staging_guest);
    let get_buffers: Vec<usize> = stdout
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("buffer="))
        .map(|buffer| buffer.parse().unwrap())
        .collect();
    assert_eq!(get_buffers.len(), GETS.len(), "{stdout}");
    has_line(&stdout, &format!("get_fs rc=0 buffer={}", get_buffers[0]));
    let calls = PHYSICAL_GENERATION_CALLS.lock().unwrap().clone();
    for (label, number) in GETS {
        assert!(
            calls
                .iter()
                .any(|(command, _)| u64::from(*command) == number),
            "{label} never reached the kernel: {calls:?}"
        );
    }
    assert!(
        calls
            .iter()
            .all(|(_, argument)| !get_buffers.contains(argument)),
        "the kernel wrote a host generation into a guest buffer {get_buffers:x?}: \
         {calls:x?}\n{stdout}"
    );
    assert!(
        calls
            .iter()
            .all(|(command, _)| SETS.iter().all(|(_, set)| u64::from(*command) != *set)),
        "a set reached the kernel: {calls:x?}\n{stdout}"
    );
}

/// The bytes of Detcore's staging page: an `fstat` result, an `fstatfs`
/// result and eight more.
const STAGING_LEN: usize =
    std::mem::size_of::<libc::stat>() + std::mem::size_of::<libc::statfs>() + 8;

/// Where the generation read writes in Detcore's staging page.
const STAGED_OUTPUT: usize =
    std::mem::size_of::<libc::stat>() + std::mem::size_of::<libc::statfs>();

/// The address of the last staging page Detcore mapped.
static STAGING_PAGE: AtomicUsize = AtomicUsize::new(0);

/// When [`FailingCleanupProbe`] makes the staging page read-only, so that
/// Detcore cannot zero it: never, after the generation read writes it, or
/// after the `fstat` that classifies the descriptor writes it.
static PROTECT_STAGING_PAGE: AtomicUsize = AtomicUsize::new(PROTECT_NEVER);
const PROTECT_NEVER: usize = 0;
const PROTECT_AFTER_READ: usize = 1;
const PROTECT_AFTER_FSTAT: usize = 2;

/// A layer below Detcore that notes Detcore's staging page when it is mapped
/// and fails its `munmap` with EINVAL, without running it, after printing on
/// stderr whether the page holds only zeros; with [`PROTECT_STAGING_PAGE`],
/// it first makes the page read-only.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct FailingCleanupProbe;

#[reverie::tool]
impl Tool for FailingCleanupProbe {
    type GlobalState = detcore::GlobalState;
    type ThreadState = ();

    fn subscriptions(_config: &detcore::Config) -> Subscription {
        Subscription::none()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let (_, args) = call.into_parts();
        let page = STAGING_PAGE.load(Ordering::SeqCst);
        let protect_after = match call {
            Syscall::Mmap(_) if args.arg1 == STAGING_LEN => {
                let mapped = guest.inject(call).await?;
                STAGING_PAGE.store(mapped as usize, Ordering::SeqCst);
                return Ok(mapped);
            }
            Syscall::Munmap(_) if page != 0 && args.arg0 == page && args.arg1 == STAGING_LEN => {
                let mut bytes = vec![0xff; STAGING_LEN];
                let at = reverie::syscalls::Addr::<u8>::from_raw(page).unwrap();
                guest.memory().read_exact(at, &mut bytes)?;
                let zeroed = bytes.iter().all(|byte| *byte == 0);
                eprintln!("staging page at its failed munmap: zeroed={zeroed}");
                return Err(Errno::EINVAL.into());
            }
            Syscall::Ioctl(_) if page != 0 && args.arg2 == page + STAGED_OUTPUT => {
                PROTECT_AFTER_READ
            }
            Syscall::Fstat(_) if page != 0 && args.arg1 == page => PROTECT_AFTER_FSTAT,
            _ => PROTECT_NEVER,
        };
        let result = guest.inject(call).await;
        if protect_after != PROTECT_NEVER
            && PROTECT_STAGING_PAGE.load(Ordering::SeqCst) == protect_after
        {
            let protect = Syscall::Mprotect(
                reverie::syscalls::Mprotect::new()
                    .with_addr(reverie::syscalls::AddrMut::from_raw(page))
                    .with_len(STAGING_LEN)
                    .with_protection(reverie::syscalls::ProtFlags::PROT_READ),
            );
            guest.inject(protect).await?;
        }
        Ok(result?)
    }
}

/// Which of Detcore's calls an interrupting probe answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Interrupting {
    /// The guest's generation read into the staging page.
    Read,
    /// The staging page's `munmap`.
    Unmap,
}

/// A layer below Detcore that notes Detcore's staging page when it is mapped
/// and answers the `n`th call (from 0) of kind `$which` with `$answer(n)`'s
/// errno, without running it, as a signal interrupts a call, or runs it where
/// that is `None`. `$seen` counts those calls.
macro_rules! interrupting_probe {
    ($probe:ident, $seen:ident, $page:ident, $which:expr, $answer:expr) => {
        static $seen: AtomicUsize = AtomicUsize::new(0);
        static $page: AtomicUsize = AtomicUsize::new(0);

        #[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
        struct $probe;

        #[reverie::tool]
        impl Tool for $probe {
            type GlobalState = detcore::GlobalState;
            type ThreadState = ();

            fn subscriptions(_config: &detcore::Config) -> Subscription {
                Subscription::none()
            }

            async fn handle_syscall_event<G: Guest<Self>>(
                &self,
                guest: &mut G,
                call: Syscall,
            ) -> Result<i64, Error> {
                let (_, args) = call.into_parts();
                let page = $page.load(Ordering::SeqCst);
                let chosen = match call {
                    Syscall::Mmap(_) if args.arg1 == STAGING_LEN => {
                        let mapped = guest.inject(call).await?;
                        $page.store(mapped as usize, Ordering::SeqCst);
                        return Ok(mapped);
                    }
                    Syscall::Ioctl(_) => {
                        $which == Interrupting::Read
                            && page != 0
                            && args.arg2 == page + STAGED_OUTPUT
                            && is_generation_number(args.arg1 as u32)
                    }
                    Syscall::Munmap(_) => {
                        $which == Interrupting::Unmap
                            && page != 0
                            && args.arg0 == page
                            && args.arg1 == STAGING_LEN
                    }
                    _ => false,
                };
                if chosen {
                    let answer: fn(usize) -> Option<Errno> = $answer;
                    if let Some(errno) = answer($seen.fetch_add(1, Ordering::SeqCst)) {
                        return Err(errno.into());
                    }
                }
                Ok(guest.inject(call).await?)
            }
        }
    };
}

interrupting_probe!(
    InterruptedReadProbe,
    INTERRUPTED_READS,
    INTERRUPTED_READ_PAGE,
    Interrupting::Read,
    |n| (n == 0).then_some(Errno::EINTR)
);
interrupting_probe!(
    NotRunReadProbe,
    NOT_RUN_READS,
    NOT_RUN_READ_PAGE,
    Interrupting::Read,
    |n| (n == 0).then_some(Errno::ERESTARTSYS)
);
interrupting_probe!(
    NeverRunReadProbe,
    NEVER_RUN_READS,
    NEVER_RUN_READ_PAGE,
    Interrupting::Read,
    |_| Some(Errno::ERESTARTSYS)
);
interrupting_probe!(
    CountingReadProbe,
    COUNTED_READS,
    COUNTED_READ_PAGE,
    Interrupting::Read,
    |_| None
);
interrupting_probe!(
    OtherRestartProbe,
    OTHER_RESTART_READS,
    OTHER_RESTART_READ_PAGE,
    Interrupting::Read,
    |n| (n == 0).then_some(Errno::ERESTARTSYS)
);
interrupting_probe!(
    InterruptedUnmapProbe,
    INTERRUPTED_UNMAPS,
    INTERRUPTED_UNMAP_PAGE,
    Interrupting::Unmap,
    |n| [Some(Errno::EINTR), Some(Errno::ERESTARTSYS)]
        .get(n)
        .copied()
        .flatten()
);
interrupting_probe!(
    AlwaysInterruptedUnmapProbe,
    ALWAYS_INTERRUPTED_UNMAPS,
    ALWAYS_INTERRUPTED_UNMAP_PAGE,
    Interrupting::Unmap,
    |_| Some(Errno::EINTR)
);

/// Marks a run of this test binary that a run-ending test starts for its
/// guest: the end of the run ends the tool's process, which in a test is the
/// test binary, so the guest runs in a binary of its own.
const ENDED_RUN_ENV: &str = "HERMIT_FS_IOC_GETVERSION_ENDED_RUN_INNER";

/// Whether this process is the inner run of the run-ending test `test`. If it
/// is not, runs `test` again in a binary of its own and checks that its run
/// ended with the policy-refusal exit and that its stderr names each of
/// `needles`.
fn is_inner_ended_run(test: &str, needles: &[&str]) -> bool {
    if std::env::var_os(ENDED_RUN_ENV).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("fs_ioc_getversion::{test}"))
        .args(["--exact", "--nocapture", "--test-threads=1"])
        .env(ENDED_RUN_ENV, "1")
        .output()
        .expect("run this test binary again");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    for needle in needles {
        assert!(stderr.contains(needle), "no {needle:?} in\n{stderr}");
    }
    false
}

fn cleanup_failure_guest(dir: &Path) {
    let file = std::fs::File::create(dir.join("leaked.txt")).unwrap();
    print!("{}", report("get_fs", file.as_raw_fd(), FS_IOC_GETVERSION));
}

/// A set, for which Detcore's only calls are those that classify the
/// descriptor.
fn classification_cleanup_failure_guest(dir: &Path) {
    let file = std::fs::File::create(dir.join("classified.txt")).unwrap();
    print!("{}", report("set_fs", file.as_raw_fd(), FS_IOC_SETVERSION));
}

/// The inner half of a run-ending test: `guest` under `Detcore<T>`, where
/// unsupported operations are allowed, so that only the end of the run stops
/// it. Never returns where the run ends.
fn run_to_its_end<T>(guest: fn(&Path))
where
    T: Tool<GlobalState = detcore::GlobalState> + 'static,
{
    let dir = generation_directory();
    let path = dir.path().to_owned();
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let _ = detcore_testutils::test_fn_with_config::<T, _>(
                move || guest(&path),
                config(true, true),
                true,
            );
        })
        .unwrap()
        .join()
        .unwrap();
    panic!("the run did not end");
}

/// A staging page Detcore cannot unmap ends the run, whatever the
/// unsupported-operation policy, after Detcore has zeroed it.
#[test]
fn a_staging_page_that_cannot_be_unmapped_ends_the_run() {
    if !is_inner_ended_run(
        "a_staging_page_that_cannot_be_unmapped_ends_the_run",
        &[
            "staging page at its failed munmap: zeroed=true",
            "an inode generation call, on descriptor",
            "could not unmap the page the call wrote into",
        ],
    ) {
        return;
    }
    run_to_its_end::<Detcore<FailingCleanupProbe>>(cleanup_failure_guest);
}

/// A `munmap` of the staging page that a signal interrupts every time ends the
/// run after Detcore's bounded attempts, saying so.
#[test]
fn an_unmap_interrupted_every_time_ends_the_run() {
    if !is_inner_ended_run(
        "an_unmap_interrupted_every_time_ends_the_run",
        &[
            "could not unmap the page the call wrote into",
            "a signal interrupted it 64 times",
        ],
    ) {
        return;
    }
    run_to_its_end::<Detcore<AlwaysInterruptedUnmapProbe>>(cleanup_failure_guest);
}

/// A staging page Detcore can neither zero nor unmap ends the run too.
#[test]
fn a_staging_page_that_can_be_neither_zeroed_nor_unmapped_ends_the_run() {
    if !is_inner_ended_run(
        "a_staging_page_that_can_be_neither_zeroed_nor_unmapped_ends_the_run",
        &[
            "staging page at its failed munmap: zeroed=false",
            "could neither zero",
        ],
    ) {
        return;
    }
    PROTECT_STAGING_PAGE.store(PROTECT_AFTER_READ, Ordering::SeqCst);
    run_to_its_end::<Detcore<FailingCleanupProbe>>(cleanup_failure_guest);
}

/// The page that holds the `fstat` classifying a descriptor is released in the
/// same way: a set, which makes no other call, ends the run when Detcore
/// cannot unmap it.
#[test]
fn a_classification_page_that_cannot_be_unmapped_ends_the_run() {
    if !is_inner_ended_run(
        "a_classification_page_that_cannot_be_unmapped_ends_the_run",
        &[
            "staging page at its failed munmap: zeroed=true",
            "ioctl 0x40087602, an inode generation call, on descriptor",
            "could not unmap the page the call wrote into",
        ],
    ) {
        return;
    }
    run_to_its_end::<Detcore<FailingCleanupProbe>>(classification_cleanup_failure_guest);
}

/// ... and when Detcore can neither zero nor unmap it.
#[test]
fn a_classification_page_that_can_be_neither_zeroed_nor_unmapped_ends_the_run() {
    if !is_inner_ended_run(
        "a_classification_page_that_can_be_neither_zeroed_nor_unmapped_ends_the_run",
        &[
            "staging page at its failed munmap: zeroed=false",
            "ioctl 0x40087602, an inode generation call, on descriptor",
            "could neither zero",
        ],
    ) {
        return;
    }
    PROTECT_STAGING_PAGE.store(PROTECT_AFTER_FSTAT, Ordering::SeqCst);
    run_to_its_end::<Detcore<FailingCleanupProbe>>(classification_cleanup_failure_guest);
}

/// The descriptor [`FailingFstatProbe`]'s guest asks about.
const ADMITTED_FD: i32 = 777;

/// The `ioctl` command, unknown to Linux, by which [`FailingFstatProbe`]'s
/// guest arms it.
const ARM_COMMAND: u64 = 0x4d41_524b;

/// Whether [`FailingFstatProbe`] fails the next `fstat` of [`ADMITTED_FD`].
static FSTAT_ARMED: AtomicUsize = AtomicUsize::new(0);

/// The `fstat`s of [`ADMITTED_FD`] [`FailingFstatProbe`] failed.
static FSTATS_FAILED: AtomicUsize = AtomicUsize::new(0);

/// A layer below Detcore that, once armed by an `ioctl` with [`ARM_COMMAND`],
/// fails the next `fstat` of [`ADMITTED_FD`] with EIO, without running it,
/// and notes in [`ADMISSION_CALLS`] every generation call that reaches the
/// kernel.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct FailingFstatProbe;

static ADMISSION_CALLS: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

#[reverie::tool]
impl Tool for FailingFstatProbe {
    type GlobalState = detcore::GlobalState;
    type ThreadState = ();

    fn subscriptions(_config: &detcore::Config) -> Subscription {
        Subscription::none()
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let (_, args) = call.into_parts();
        match call {
            Syscall::Ioctl(_) if args.arg1 as u64 == ARM_COMMAND => {
                FSTAT_ARMED.store(1, Ordering::SeqCst);
            }
            Syscall::Ioctl(_) if is_generation_number(args.arg1 as u32) => {
                ADMISSION_CALLS
                    .lock()
                    .unwrap()
                    .push((args.arg1 as u32, args.arg2));
            }
            Syscall::Fstat(_)
                if args.arg0 == ADMITTED_FD as usize
                    && FSTAT_ARMED.swap(0, Ordering::SeqCst) == 1 =>
            {
                FSTATS_FAILED.fetch_add(1, Ordering::SeqCst);
                return Err(Errno::EIO.into());
            }
            _ => {}
        }
        Ok(guest.inject(call).await?)
    }
}

fn admission_guest(dir: &Path) {
    let file = std::fs::File::create(dir.join("admitted.txt")).unwrap();
    assert_eq!(
        unsafe { libc::dup2(file.as_raw_fd(), ADMITTED_FD) },
        ADMITTED_FD
    );
    let mut out = String::new();
    for (label, number) in [("set", FS_IOC_SETVERSION), ("get", FS_IOC_GETVERSION)] {
        let _ = unsafe { libc::syscall(libc::SYS_ioctl, ADMITTED_FD, ARM_COMMAND, 0) };
        out += &report(label, ADMITTED_FD, number);
    }
    unsafe { libc::close(ADMITTED_FD) };
    out += &report("set_closed", ADMITTED_FD, FS_IOC_SETVERSION);
    print!("{out}");
}

/// A descriptor Detcore could not classify is refused by name, never sent on
/// to the kernel; one that `fstat` says is not open gets Linux's EBADF without
/// a call.
#[test]
fn a_descriptor_detcore_cannot_classify_is_refused() {
    FSTATS_FAILED.store(0, Ordering::SeqCst);
    ADMISSION_CALLS.lock().unwrap().clear();
    let (stdout, _dir) =
        run_with::<Detcore<FailingFstatProbe>>(config(true, true), admission_guest);
    has_line(&stdout, &format!("set errno={}", libc::EOPNOTSUPP));
    has_line(&stdout, &format!("get errno={}", libc::EOPNOTSUPP));
    has_line(&stdout, &format!("set_closed errno={}", libc::EBADF));
    assert_eq!(FSTATS_FAILED.load(Ordering::SeqCst), 2, "{stdout}");
    let calls = ADMISSION_CALLS.lock().unwrap().clone();
    assert!(
        calls.is_empty(),
        "a generation call reached the kernel: {calls:x?}\n{stdout}"
    );
}
