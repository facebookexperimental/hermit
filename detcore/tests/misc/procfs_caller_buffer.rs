/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A sanitized procfs read behaves like a native one toward the caller.
//!
//! The first read of a snapshotted procfs file captures the whole host file
//! and then hands the caller sanitized bytes. Each test uses a fresh
//! descriptor, so its call is the one that performs the capture.
//!
//! - `/proc/diskstats` sanitizes to fewer bytes than the host's file (each
//!   disk's counters become single digits), so after a first read the
//!   caller's buffer past the returned count must hold none of the host
//!   table the capture read. The write-only cases cover a destination the
//!   kernel may write but a tracer's protection-respecting read cannot see.
//!   (`/proc/modules`, which these tests used before, is now a file Detcore
//!   renders with no capture: https://github.com/rrnewton/hermit/issues/4266.)
//! - The copy to the caller checks the destination as the kernel's
//!   `copy_to_user` does: a read-only, inaccessible or unmapped destination
//!   gets `EFAULT` and is not written, and a destination that becomes
//!   inaccessible part way gets the bytes copied before that point.
//! - Like a seq_file read, a sequential read advances the shared offset only by
//!   the bytes it copied, so a read that faults leaves the data unread.
//! - Capturing `/proc/self/maps` does not list a mapping that is gone by the
//!   time the read returns.
//! - That capture does not need stack below the stack pointer: a stack pointer
//!   just above a guard page or unmapped memory still gets the listing, and
//!   neither the stack nor the caller's buffer beyond the output is changed.
//! - As in Linux, a requested range beyond the user address limit gets
//!   `EFAULT` even at end of file, where any other range gets 0. At the limit
//!   itself the answer is the kernel's own for a file it reads.

use std::fs::File;
use std::os::fd::AsRawFd;

/// Larger than the host `/proc/diskstats` on a typical machine, so the capture
/// takes the whole file in one read and would leave its tail behind.
const LEN: usize = 64 * 1024;
const SENTINEL: u8 = 0xa5;

/// The host's `/proc/diskstats`, which must have contents to leave behind.
fn require_host_disks() -> usize {
    let host = std::fs::read("/proc/diskstats").unwrap();
    assert!(
        !host.is_empty() && host.len() < LEN,
        "this host's /proc/diskstats has {} bytes; these tests need a nonempty file \
         that fits their {LEN}-byte buffer",
        host.len()
    );
    host.len()
}

/// A leak shows only past the bytes a read published and within what the
/// capture wrote: the host's file must be longer than the sanitized one.
fn assert_detectable(host: usize, returned: isize, call: &str) {
    assert!(
        returned > 0 && (returned as usize) < host,
        "{call} returned {returned} of a {host}-byte host file, so a leftover host byte \
         could not show"
    );
}

/// The bytes past the `returned` a read published are as the caller left
/// them.
fn assert_untouched(buf: &[u8], returned: usize, call: &str) {
    if let Some(at) = buf[returned..].iter().position(|&byte| byte != SENTINEL) {
        let at = returned + at;
        let end = buf.len().min(at + 64);
        panic!(
            "{call} of /proc/diskstats returned {returned} but changed the caller's buffer at \
             byte {at}: {:?}",
            String::from_utf8_lossy(&buf[at..end])
        );
    }
}

#[test]
fn procfs_read_leaves_no_host_bytes_in_the_caller_buffer() {
    let host = require_host_disks();
    super::det_test_fn_without_pmu(|| {
        let file = File::open("/proc/diskstats").unwrap();
        let mut buf = vec![SENTINEL; LEN];
        let n = unsafe { libc::read(file.as_raw_fd(), buf.as_mut_ptr().cast(), LEN) };
        assert_detectable(host, n, "read");
        assert_untouched(&buf, n as usize, "read");
    });
}

#[test]
fn procfs_pread_leaves_no_host_bytes_in_the_caller_buffer() {
    let host = require_host_disks();
    super::det_test_fn_without_pmu(|| {
        let file = File::open("/proc/diskstats").unwrap();
        let mut buf = vec![SENTINEL; LEN];
        let n = unsafe { libc::pread(file.as_raw_fd(), buf.as_mut_ptr().cast(), LEN, 0) };
        assert_detectable(host, n, "pread");
        assert_untouched(&buf, n as usize, "pread");
    });
}

/// A read into pages the guest made PROT_WRITE-only, larger than the host's
/// file. The kernel's read may write them, but `process_vm_readv` cannot read
/// them, so a save of the caller's buffer would come back empty.
const WRITE_ONLY_COUNT: usize = LEN;

fn assert_write_only_destination_untouched(
    host: usize,
    call: &str,
    read: impl FnOnce(libc::c_int, *mut libc::c_void) -> isize,
) {
    let page = WRITE_ONLY_COUNT;
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    unsafe { std::ptr::write_bytes(mapping.cast::<u8>(), SENTINEL, page) };
    assert_eq!(
        unsafe { libc::mprotect(mapping, page, libc::PROT_WRITE) },
        0
    );
    let file = File::open("/proc/diskstats").unwrap();
    let n = read(file.as_raw_fd(), mapping);
    assert_eq!(
        unsafe { libc::mprotect(mapping, page, libc::PROT_READ | libc::PROT_WRITE) },
        0
    );
    let buf = unsafe { std::slice::from_raw_parts(mapping.cast::<u8>(), page) };
    assert_detectable(host, n, call);
    assert_untouched(buf, n as usize, call);
    assert_eq!(unsafe { libc::munmap(mapping, page) }, 0);
}

#[test]
fn procfs_read_leaves_no_host_bytes_in_a_write_only_buffer() {
    let host = require_host_disks();
    super::det_test_fn_without_pmu(|| {
        assert_write_only_destination_untouched(host, "write-only read", |fd, buf| unsafe {
            libc::read(fd, buf, WRITE_ONLY_COUNT)
        });
    });
}

#[test]
fn procfs_pread_leaves_no_host_bytes_in_a_write_only_buffer() {
    let host = require_host_disks();
    super::det_test_fn_without_pmu(|| {
        assert_write_only_destination_untouched(host, "write-only pread", |fd, buf| unsafe {
            libc::pread(fd, buf, WRITE_ONLY_COUNT, 0)
        });
    });
}

/// Sanitized `/proc/uptime` is a nonempty line longer than eight bytes, so an
/// eight-byte read publishes eight bytes.
const UPTIME_COUNT: usize = 8;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap()
}

/// Map `pages` read/write pages filled with the sentinel.
fn sentinel_pages(pages: usize) -> (*mut u8, usize) {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    unsafe { std::ptr::write_bytes(mapping.cast::<u8>(), SENTINEL, pages * page) };
    (mapping.cast(), page)
}

fn protect(address: *mut u8, len: usize, protection: libc::c_int) {
    assert_eq!(
        unsafe { libc::mprotect(address.cast(), len, protection) },
        0
    );
}

/// Read and pread of a fresh `/proc/uptime` into a page with `protection`,
/// checking the result, the shared offset and which page bytes changed.
fn read_uptime_into_page(protection: libc::c_int, expect_published: bool) {
    for positioned in [false, true] {
        let call = if positioned { "pread" } else { "read" };
        let (page_start, page) = sentinel_pages(1);
        protect(page_start, page, protection);
        let file = File::open("/proc/uptime").unwrap();
        let fd = file.as_raw_fd();
        let n = unsafe {
            if positioned {
                libc::pread(fd, page_start.cast(), UPTIME_COUNT, 0)
            } else {
                libc::read(fd, page_start.cast(), UPTIME_COUNT)
            }
        };
        let error = errno();
        let offset = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
        protect(page_start, page, libc::PROT_READ | libc::PROT_WRITE);
        let buf = unsafe { std::slice::from_raw_parts(page_start, page) };
        if expect_published {
            assert_eq!(
                n, UPTIME_COUNT as isize,
                "{call} with protection {protection}"
            );
            assert!(
                buf[UPTIME_COUNT..].iter().all(|&byte| byte == SENTINEL),
                "{call} of {UPTIME_COUNT} bytes changed the page past them"
            );
            assert_eq!(offset, if positioned { 0 } else { UPTIME_COUNT as i64 });
        } else {
            assert_eq!(
                (n, error),
                (-1, libc::EFAULT),
                "{call} into a page with protection {protection} must fail EFAULT"
            );
            assert!(
                buf.iter().all(|&byte| byte == SENTINEL),
                "{call} that failed EFAULT changed a page with protection {protection}"
            );
            assert_eq!(offset, 0, "{call} that failed EFAULT moved the offset");
        }
        assert_eq!(unsafe { libc::munmap(page_start.cast(), page) }, 0);
    }
}

#[test]
fn procfs_first_read_into_a_read_only_page_fails_efault() {
    super::det_test_fn_without_pmu(|| read_uptime_into_page(libc::PROT_READ, false));
}

#[test]
fn procfs_first_read_into_an_inaccessible_page_fails_efault() {
    super::det_test_fn_without_pmu(|| read_uptime_into_page(libc::PROT_NONE, false));
}

#[test]
fn procfs_first_read_into_writable_pages_publishes() {
    super::det_test_fn_without_pmu(|| {
        read_uptime_into_page(libc::PROT_READ | libc::PROT_WRITE, true);
        read_uptime_into_page(libc::PROT_WRITE, true);
    });
}

#[test]
fn procfs_first_read_into_an_unmapped_page_fails_efault() {
    super::det_test_fn_without_pmu(|| {
        let (page_start, page) = sentinel_pages(1);
        assert_eq!(unsafe { libc::munmap(page_start.cast(), page) }, 0);
        for positioned in [false, true] {
            let file = File::open("/proc/uptime").unwrap();
            let fd = file.as_raw_fd();
            let n = unsafe {
                if positioned {
                    libc::pread(fd, page_start.cast(), UPTIME_COUNT, 0)
                } else {
                    libc::read(fd, page_start.cast(), UPTIME_COUNT)
                }
            };
            assert_eq!((n, errno()), (-1, libc::EFAULT), "positioned: {positioned}");
            assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }, 0);
        }
    });
}

/// A destination whose second half is inaccessible gets the first half, and a
/// sequential read advances the offset by exactly that much.
#[test]
fn procfs_first_read_across_an_inaccessible_page_copies_a_prefix() {
    super::det_test_fn_without_pmu(|| {
        for positioned in [false, true] {
            let (mapping, page) = sentinel_pages(2);
            let second_page = unsafe { mapping.add(page) };
            protect(second_page, page, libc::PROT_NONE);
            let destination = unsafe { second_page.sub(UPTIME_COUNT / 2) };
            let file = File::open("/proc/uptime").unwrap();
            let fd = file.as_raw_fd();
            let n = unsafe {
                if positioned {
                    libc::pread(fd, destination.cast(), UPTIME_COUNT, 0)
                } else {
                    libc::read(fd, destination.cast(), UPTIME_COUNT)
                }
            };
            assert_eq!(n, (UPTIME_COUNT / 2) as isize, "positioned: {positioned}");
            let offset = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
            assert_eq!(
                offset,
                if positioned {
                    0
                } else {
                    (UPTIME_COUNT / 2) as i64
                }
            );
            protect(second_page, page, libc::PROT_READ | libc::PROT_WRITE);
            let tail = unsafe { std::slice::from_raw_parts(second_page, page) };
            assert!(tail.iter().all(|&byte| byte == SENTINEL));
            assert_eq!(unsafe { libc::munmap(mapping.cast(), 2 * page) }, 0);
        }
    });
}

/// A first read that faults consumes nothing, through the descriptor or an
/// alias that shares its offset.
#[test]
fn procfs_failed_first_read_leaves_the_content_unread() {
    super::det_test_fn_without_pmu(|| {
        let file = File::open("/proc/uptime").unwrap();
        let fd = file.as_raw_fd();
        let alias = unsafe { libc::dup(fd) };
        assert!(alias >= 0);
        let n = unsafe { libc::read(fd, std::ptr::null_mut(), UPTIME_COUNT) };
        assert_eq!((n, errno()), (-1, libc::EFAULT));
        assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }, 0);
        assert_eq!(unsafe { libc::lseek(alias, 0, libc::SEEK_CUR) }, 0);

        let mut whole = [0_u8; 256];
        let whole_len = unsafe { libc::pread(fd, whole.as_mut_ptr().cast(), whole.len(), 0) };
        assert!(whole_len > UPTIME_COUNT as isize);
        let mut read_back = [0_u8; 256];
        let read_len = unsafe { libc::read(alias, read_back.as_mut_ptr().cast(), read_back.len()) };
        assert_eq!(
            &read_back[..read_len as usize],
            &whole[..whole_len as usize],
            "the read after the failed one must see the whole file"
        );
        assert_eq!(unsafe { libc::close(alias) }, 0);
    });
}

/// Whether every page of `[start, end)` is mapped. `msync(MS_ASYNC)` fails
/// with `ENOMEM` at the first unmapped page and, unlike `mincore`, needs no
/// buffer sized to the range: one maps row can reserve terabytes.
fn range_is_mapped(start: usize, end: usize) -> Result<(), i32> {
    let rc = unsafe { libc::msync(start as *mut libc::c_void, end - start, libc::MS_ASYNC) };
    if rc == 0 { Ok(()) } else { Err(errno()) }
}

/// Require every row of `/proc/self/maps` text other than `[vsyscall]` to be
/// mapped now, and return each row's range with its line.
fn assert_rows_mapped(text: &str) -> Vec<(usize, usize, String)> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.ends_with("[vsyscall]") {
            continue;
        }
        let range = line.split_whitespace().next().unwrap();
        let (start, end) = range.split_once('-').unwrap();
        let start = usize::from_str_radix(start, 16).unwrap();
        let end = usize::from_str_radix(end, 16).unwrap();
        if let Err(errno) = range_is_mapped(start, end) {
            panic!("row {line:?} is not mapped: errno {errno}");
        }
        rows.push((start, end, line.to_owned()));
    }
    rows
}

/// Every row of a freshly read `/proc/self/maps` is still mapped when the read
/// returns. A capture mapping created and removed inside the read would show
/// either as its own row or merged into a neighbouring row, and in both cases
/// part of that row's range is gone, so `msync` fails with `ENOMEM`. A
/// guarded read/write page must also keep a row of exactly its own extent.
#[test]
fn procfs_self_maps_lists_no_capture_mapping() {
    super::det_test_fn_without_pmu(|| {
        let file = File::open("/proc/self/maps").unwrap();
        let fd = file.as_raw_fd();
        let mut contents: Vec<u8> = Vec::with_capacity(1 << 20);
        // A read/write page between two PROT_NONE pages cannot merge with
        // an existing mapping, so its row is exactly one page.
        let (guarded, page) = sentinel_pages(3);
        protect(guarded, page, libc::PROT_NONE);
        let neighbour = unsafe { guarded.add(page) };
        protect(unsafe { neighbour.add(page) }, page, libc::PROT_NONE);

        loop {
            let spare = contents.capacity() - contents.len();
            assert!(spare > 0, "/proc/self/maps outgrew the buffer");
            let n =
                unsafe { libc::read(fd, contents.as_mut_ptr().add(contents.len()).cast(), spare) };
            assert!(n >= 0, "read failed: {}", errno());
            if n == 0 {
                break;
            }
            unsafe { contents.set_len(contents.len() + n as usize) };
        }

        let text = String::from_utf8(contents).unwrap();
        let (start, end, line) = assert_rows_mapped(&text)
            .into_iter()
            .find(|(start, end, _)| (*start..*end).contains(&(neighbour as usize)))
            .expect("the guarded page has no row");
        assert_eq!(
            (start, end),
            (neighbour as usize, neighbour as usize + page),
            "the guarded page's row is {line:?}"
        );
        // The probe sees a hole: with the middle page gone, the three-page
        // range is no longer mapped.
        let guarded_start = guarded as usize;
        assert_eq!(
            range_is_mapped(guarded_start, guarded_start + 3 * page),
            Ok(())
        );
        assert_eq!(unsafe { libc::munmap(neighbour.cast(), page) }, 0);
        assert_eq!(
            range_is_mapped(guarded_start, guarded_start + 3 * page),
            Err(libc::ENOMEM)
        );
        assert_eq!(unsafe { libc::munmap(guarded.cast(), 3 * page) }, 0);
    });
}

/// Issue `read(fd, buf, count)`, or `pread64(fd, buf, count, 0)` when
/// `positioned`, with `rsp` switched to `stack` for the `syscall` instruction.
///
/// # Safety
///
/// As for the tight-stack `openat`: nothing is pushed to `stack`.
unsafe fn raw_read_on_stack(
    stack: *mut u8,
    fd: libc::c_int,
    buf: *mut u8,
    count: usize,
    positioned: bool,
) -> i64 {
    let number = if positioned {
        libc::SYS_pread64
    } else {
        libc::SYS_read
    };
    let result: i64;
    // SAFETY: rsp is swapped with r12 and swapped back before the block ends;
    // `syscall` itself does not touch the user stack. rcx and r11 are the
    // registers `syscall` clobbers.
    unsafe {
        std::arch::asm!(
            "xchg rsp, r12",
            "syscall",
            "xchg rsp, r12",
            inout("r12") stack => _,
            inlateout("rax") number => result,
            in("rdi") i64::from(fd),
            in("rsi") buf,
            in("rdx") count,
            in("r10") 0_i64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// The first read of `/proc/self/maps` needs no stack, so a stack pointer just
/// above a guard page or unmapped memory must not fail it. The stack page and
/// the caller's buffer beyond the output keep their bytes, and the listing
/// still has no capture mapping.
#[test]
fn procfs_self_maps_first_read_succeeds_without_writable_stack_below_rsp() {
    use super::tight_stack_openat::BelowStack;
    super::det_test_fn_sequential_without_pmu(|| {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        // 1024 bytes leave room below the red zone for the whole scratch and
        // are the control; 512, 192 and 0 do not.
        for (below, writable_bytes) in [
            (BelowStack::GuardPage, 1024),
            (BelowStack::GuardPage, 512),
            (BelowStack::GuardPage, 192),
            (BelowStack::GuardPage, 0),
            (BelowStack::Unmapped, 192),
            (BelowStack::Unmapped, 0),
        ] {
            for positioned in [false, true] {
                let (stack, mapping, mapping_len) =
                    super::tight_stack_openat::tight_stack(below, writable_bytes);
                let stack_page = unsafe { stack.sub(writable_bytes) };
                unsafe { std::ptr::write_bytes(stack_page, SENTINEL, page) };
                let file = File::open("/proc/self/maps").unwrap();
                let mut contents = vec![SENTINEL; 1 << 20];
                let n = unsafe {
                    raw_read_on_stack(
                        stack,
                        file.as_raw_fd(),
                        contents.as_mut_ptr(),
                        contents.len(),
                        positioned,
                    )
                };
                let case = format!(
                    "{} with {writable_bytes} writable bytes above a {below:?} region",
                    if positioned { "pread" } else { "read" }
                );
                assert!(n > 0, "{case} returned {n}");
                assert!(
                    (n as usize) < contents.len(),
                    "/proc/self/maps outgrew the buffer"
                );
                let stack_bytes = unsafe { std::slice::from_raw_parts(stack_page, page) };
                assert!(
                    stack_bytes.iter().all(|&byte| byte == SENTINEL),
                    "{case} changed the stack page"
                );
                assert!(
                    contents[n as usize..].iter().all(|&byte| byte == SENTINEL),
                    "{case} left bytes beyond the output in the caller's buffer"
                );
                let text = std::str::from_utf8(&contents[..n as usize]).unwrap();
                assert_rows_mapped(text);
                assert_eq!(unsafe { libc::munmap(mapping, mapping_len) }, 0);
            }
        }
    });
}

/// With no stack scratch, the caller's destination is checked the way Linux
/// checks it: a destination that faults part way gets the bytes before the
/// fault, and a read-only one gets `EFAULT` and is not written.
#[test]
fn procfs_self_maps_first_read_on_a_tight_stack_checks_the_destination() {
    use super::tight_stack_openat::BelowStack;
    super::det_test_fn_sequential_without_pmu(|| {
        for positioned in [false, true] {
            let (stack, stack_mapping, stack_len) =
                super::tight_stack_openat::tight_stack(BelowStack::GuardPage, 0);
            let (mapping, page) = sentinel_pages(2);
            let second_page = unsafe { mapping.add(page) };
            protect(second_page, page, libc::PROT_NONE);
            let file = File::open("/proc/self/maps").unwrap();
            let n = unsafe {
                raw_read_on_stack(stack, file.as_raw_fd(), second_page.sub(4), 512, positioned)
            };
            assert_eq!(n, 4, "positioned: {positioned}");
            let offset = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
            assert_eq!(offset, if positioned { 0 } else { 4 });
            protect(second_page, page, libc::PROT_READ | libc::PROT_WRITE);
            let tail = unsafe { std::slice::from_raw_parts(second_page, page) };
            assert!(tail.iter().all(|&byte| byte == SENTINEL));

            // The prefix above went to the end of this page.
            unsafe { std::ptr::write_bytes(mapping, SENTINEL, page) };
            protect(mapping, page, libc::PROT_READ);
            let file = File::open("/proc/self/maps").unwrap();
            let n = unsafe { raw_read_on_stack(stack, file.as_raw_fd(), mapping, 512, positioned) };
            assert_eq!(n, -i64::from(libc::EFAULT), "positioned: {positioned}");
            assert_eq!(
                unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) },
                0
            );
            let head = unsafe { std::slice::from_raw_parts(mapping, page) };
            assert!(head.iter().all(|&byte| byte == SENTINEL));
            assert_eq!(unsafe { libc::munmap(mapping.cast(), 2 * page) }, 0);
            assert_eq!(unsafe { libc::munmap(stack_mapping, stack_len) }, 0);
        }
    });
}

/// With no stack scratch, a valid destination of 1 or 4 bytes that ends right
/// before an unmapped page gets those bytes, as natively. Neither the capture,
/// which saves and reads back those bytes, nor the digest of the moved bytes
/// in the log may fail on the unmapped page, as an eight-byte
/// `PTRACE_PEEKDATA` there does. Eight bytes is the control.
#[test]
fn procfs_self_maps_short_read_on_a_tight_stack_before_an_unmapped_page() {
    use super::tight_stack_openat::BelowStack;
    super::det_test_fn_sequential_without_pmu(|| {
        let mut expected = [0_u8; 8];
        let file = File::open("/proc/self/maps").unwrap();
        assert_eq!(
            unsafe { libc::read(file.as_raw_fd(), expected.as_mut_ptr().cast(), 8) },
            8
        );
        for count in [1, 4, 8] {
            for positioned in [false, true] {
                let (stack, stack_mapping, stack_len) =
                    super::tight_stack_openat::tight_stack(BelowStack::GuardPage, 0);
                let (mapping, page) = sentinel_pages(2);
                let second_page = unsafe { mapping.add(page) };
                assert_eq!(unsafe { libc::munmap(second_page.cast(), page) }, 0);
                let destination = unsafe { second_page.sub(count) };
                let file = File::open("/proc/self/maps").unwrap();
                let n = unsafe {
                    raw_read_on_stack(stack, file.as_raw_fd(), destination, count, positioned)
                };
                let case = format!(
                    "{} of {count} bytes",
                    if positioned { "pread" } else { "read" }
                );
                assert_eq!(n, count as i64, "{case}");
                let got = unsafe { std::slice::from_raw_parts(destination, count) };
                assert_eq!(got, &expected[..count], "{case}");
                let before = unsafe { std::slice::from_raw_parts(mapping, page - count) };
                assert!(before.iter().all(|&byte| byte == SENTINEL), "{case}");
                let offset = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
                assert_eq!(offset, if positioned { 0 } else { count as i64 }, "{case}");
                assert_eq!(unsafe { libc::munmap(mapping.cast(), page) }, 0);
                assert_eq!(unsafe { libc::munmap(stack_mapping, stack_len) }, 0);
            }
        }
    });
}

/// Linux checks the whole requested range against the user address limit
/// before reading, so a range beyond it is `EFAULT` even at EOF. A range
/// within the limit gets 0 at EOF whether or not its pages are mapped.
fn read_at_eof(fd: libc::c_int, positioned: bool, buf: usize, count: usize) -> (isize, i32) {
    let n = unsafe {
        if positioned {
            libc::pread(fd, buf as *mut libc::c_void, count, 0)
        } else {
            libc::read(fd, buf as *mut libc::c_void, count)
        }
    };
    (n, if n < 0 { errno() } else { 0 })
}

/// The running kernel's user address limit, found from its own reads of
/// `null`, a `/dev/null` descriptor, which is always at end of file: the
/// highest address at which it accepts an empty read.
fn kernel_user_address_limit(null: libc::c_int) -> usize {
    let accepts = |buf| match read_at_eof(null, false, buf, 0) {
        (0, 0) => true,
        (-1, libc::EFAULT) => false,
        other => panic!("empty read of /dev/null at {buf:#x}: {other:?}"),
    };
    let (mut accepted, mut refused) = (0_usize, usize::MAX);
    assert!(accepts(accepted));
    assert!(!accepts(refused));
    while refused - accepted > 1 {
        let middle = accepted + (refused - accepted) / 2;
        if accepts(middle) {
            accepted = middle;
        } else {
            refused = middle;
        }
    }
    accepted
}

fn assert_eof_ranges(fd: libc::c_int, positioned: bool, offset: i64) {
    let call = if positioned { "pread" } else { "read" };
    // Within the limit: no page is mapped at 4 KiB, and NULL has no bytes to take.
    assert_eq!(
        read_at_eof(fd, positioned, 0x1000, 1 << 20),
        (0, 0),
        "{call}"
    );
    assert_eq!(read_at_eof(fd, positioned, 0, 8), (0, 0), "{call}");
    // Beyond it: the start, the end, or an end that wraps around.
    for (buf, count) in [
        (usize::MAX - 0xfff, 1),
        (0x1000, 1 << 62),
        (0x1000, usize::MAX - 0xfff),
    ] {
        assert_eq!(
            read_at_eof(fd, positioned, buf, count),
            (-1, libc::EFAULT),
            "{call} of {count:#x} bytes at {buf:#x} at EOF"
        );
    }
    // At the limit: each read gets what the kernel gives the same read of a
    // file it reads itself, and the kernel accepts the empty range at the
    // limit and refuses the byte there.
    let null = File::open("/dev/null").unwrap();
    let limit = kernel_user_address_limit(null.as_raw_fd());
    #[cfg(target_arch = "x86_64")]
    assert!(limit >= (1 << 47) - 4096, "{limit:#x}");
    assert_eq!(read_at_eof(null.as_raw_fd(), false, limit, 0), (0, 0));
    assert_eq!(
        read_at_eof(null.as_raw_fd(), false, limit, 1),
        (-1, libc::EFAULT)
    );
    for (buf, count) in [
        (limit, 0),
        (limit + 1, 0),
        (limit - 1, 1),
        (limit, 1),
        (limit - 0x1000, 0x1000),
        (limit - 0xfff, 0x1000),
        // Past MAX_RW_COUNT, which `read` checks uncapped.
        (0x1000, limit - 0x1000),
        (0x1000, limit - 0xfff),
    ] {
        assert_eq!(
            read_at_eof(fd, positioned, buf, count),
            read_at_eof(null.as_raw_fd(), false, buf, count),
            "{call} of {count:#x} bytes at {buf:#x} at EOF, limit {limit:#x}"
        );
    }
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }, offset);
}

#[test]
fn procfs_read_beyond_the_user_address_limit_fails_efault_at_eof() {
    super::det_test_fn_without_pmu(|| {
        // /proc/modules, which Detcore renders, is empty, so its first read
        // is at EOF.
        for positioned in [false, true] {
            let file = File::open("/proc/modules").unwrap();
            assert_eof_ranges(file.as_raw_fd(), positioned, 0);
        }
        // /proc/uptime after reading all of it.
        let file = File::open("/proc/uptime").unwrap();
        let fd = file.as_raw_fd();
        let mut whole = [0_u8; 256];
        let len = unsafe { libc::read(fd, whole.as_mut_ptr().cast(), whole.len()) };
        assert!(len > 0);
        assert_eq!(
            unsafe { libc::read(fd, whole.as_mut_ptr().cast(), whole.len()) },
            0
        );
        assert_eof_ranges(fd, false, len as i64);
    });
}
