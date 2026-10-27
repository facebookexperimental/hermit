/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The guest reads Hermit's frozen copy of a property area
//! (`Config::property_snapshot`): a pure read of a file the snapshot copied,
//! by any name, opens the copy; an open that writes or truncates runs on the
//! live file, as natively, and never touches the copy; a file in the area
//! the snapshot does not hold is read live only where the run allows it.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

/// A live area with one file, a copy of it with other contents, and the
/// snapshot configuration that names them.
struct Area {
    live: tempfile::TempDir,
    _copies: tempfile::TempDir,
    snapshot: detcore_model::config::PropertySnapshot,
}

fn area() -> Area {
    let live = tempfile::tempdir().unwrap();
    let copies = tempfile::tempdir().unwrap();
    let name = "u:object_r:test_prop:s0";
    std::fs::write(live.path().join(name), [1, 2, 3, 4]).unwrap();
    std::fs::write(copies.path().join(name), [5, 6, 7, 8]).unwrap();
    let metadata = std::fs::metadata(live.path().join(name)).unwrap();
    let snapshot = detcore_model::config::PropertySnapshot {
        guest_dir: live.path().to_owned(),
        copy_dir: copies.path().to_owned(),
        live_files: vec![detcore_model::config::PropertyFile {
            dev: metadata.dev(),
            ino: metadata.ino(),
            name: name.to_owned(),
        }],
        uncopied_files: Vec::new(),
        consistent_cut: true,
    };
    Area {
        live,
        _copies: copies,
        snapshot,
    }
}

/// A read by its own name or through a symbolic link opens the copy; a file
/// the area gained after the snapshot is read live in a run that is not
/// fail-closed; `O_RDONLY | O_TRUNC` truncates the live file, as natively,
/// and later reads still get the copy.
#[test]
fn a_property_snapshot_serves_every_read_of_the_files_it_copied() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let link = area.live.path().join("..").join(format!(
        "{}-link",
        area.live.path().file_name().unwrap().to_string_lossy()
    ));
    std::os::unix::fs::symlink(&file, &link).unwrap();
    // A file the area gained after its snapshot was taken.
    let later = area.live.path().join("u:object_r:later_prop:s0");
    std::fs::write(&later, [9]).unwrap();
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let paths = (file.clone(), link.clone(), later.clone());
    let guest = move || {
        let (file, link, later): &(PathBuf, PathBuf, PathBuf) = &paths;
        assert_eq!(std::fs::read(file).unwrap(), [5, 6, 7, 8]);
        assert_eq!(std::fs::read(link).unwrap(), [5, 6, 7, 8]);
        assert_eq!(std::fs::read(later).unwrap(), [9]);
        let truncated = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path; the descriptor is closed below.
        let fd = unsafe { libc::open(truncated.as_ptr(), libc::O_RDONLY | libc::O_TRUNC) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        // SAFETY: `fd` is the descriptor just opened.
        unsafe { libc::close(fd) };
        assert_eq!(std::fs::read(file).unwrap(), [5, 6, 7, 8]);
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
    let _ = std::fs::remove_file(&link);
    // The guest's truncation reached the live file, never the copy.
    assert_eq!(std::fs::metadata(Path::new(&file)).unwrap().len(), 0);
}

/// The same reads with Hermit's metadata virtualization off, where the
/// descriptor check takes its own `fstat`.
#[test]
fn a_property_snapshot_serves_reads_with_metadata_virtualization_off() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        virtualize_metadata: false,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let guest = move || {
        assert_eq!(std::fs::read(&file).unwrap(), [5, 6, 7, 8]);
        // An unrelated read is checked against the snapshot and passes.
        assert!(std::fs::read("/proc/self/stat").is_ok());
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
}

/// In a fail-closed run, an anonymous mapping that names a descriptor of a
/// live property file (opened to write, which runs natively) succeeds, since
/// Linux ignores the descriptor there, and a file mapping of a descriptor that
/// is not open fails natively with `EBADF`: neither is refused.
#[test]
fn mappings_that_read_no_property_file_are_not_refused() {
    let area = area();
    let file = area.live.path().join("u:object_r:test_prop:s0");
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        panic_on_unsupported_syscalls: true,
        shutdown_on_unsupported_syscall: true,
        property_snapshot: Some(area.snapshot.clone()),
        ..Default::default()
    };
    let guest = move || {
        use std::os::fd::AsRawFd;
        let live = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file)
            .unwrap();
        // SAFETY: an anonymous private mapping, unmapped below.
        let anonymous = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                live.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            anonymous,
            libc::MAP_FAILED,
            "{}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(anonymous, 4096) };
        // A descriptor number this guest opened and closed, so it is not
        // open (not -1, which skips the check entirely).
        let closed_fd = std::fs::File::open("/dev/null").unwrap().as_raw_fd();
        // SAFETY: a file mapping of a descriptor that is not open fails.
        let closed = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                closed_fd,
                0,
            )
        };
        assert_eq!(closed, libc::MAP_FAILED);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    };
    detcore_testutils::det_test_fn_with_config(
        true,
        guest,
        config,
        detcore_testutils::expect_success,
    );
}

/// Marks a run of this test binary that a refusal test starts for its
/// guest: the refusal ends the tool's process, which in a test is the test
/// binary, so the guest runs in a binary of its own.
const REFUSAL_ENV: &str = "HERMIT_PROPERTY_SNAPSHOT_REFUSAL_INNER";

/// Whether this process is the inner run of the refusal test `test`. If it
/// is not, runs `test` again in a binary of its own and checks that its
/// guest's run ended with the policy-refusal exit, the live-area warning and
/// `why`.
fn is_inner_refusal_run(test: &str, why: &str) -> bool {
    if std::env::var_os(REFUSAL_ENV).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("property_snapshot::{test}"))
        .args(["--exact", "--nocapture", "--test-threads=1"])
        .env(REFUSAL_ENV, "1")
        .output()
        .expect("run this test binary again");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_PROPERTY_AREA_LIVE") && stderr.contains(why),
        "{stderr}"
    );
    false
}

/// Runs `guest` in a fail-closed run under `snapshot`; returns only if the
/// run was not refused.
fn run_fail_closed(
    snapshot: detcore_model::config::PropertySnapshot,
    guest: impl FnOnce() + Send + 'static,
) {
    let config = detcore::Config {
        max_timeslice: None,
        sequentialize_threads: true,
        panic_on_unsupported_syscalls: true,
        shutdown_on_unsupported_syscall: true,
        property_snapshot: Some(snapshot),
        ..Default::default()
    };
    // On a thread of its own with a large stack: Detcore's futures outgrow
    // the test harness thread's.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let _ = detcore_testutils::test_fn_with_config::<detcore::Detcore, _>(
                move || {
                    guest();
                    std::process::exit(0);
                },
                detcore_testutils::prepare_ptrace_timer_config(config),
                true,
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

/// In a fail-closed run, a read of a file the area gained after its snapshot
/// was taken ends the run with the policy-refusal exit, naming why.
#[test]
fn a_file_the_area_gained_after_its_snapshot_is_refused() {
    if !is_inner_refusal_run(
        "a_file_the_area_gained_after_its_snapshot_is_refused",
        "is in the property area but not in its snapshot",
    ) {
        return;
    }
    let area = area();
    let later = area.live.path().join("u:object_r:later_prop:s0");
    std::fs::write(&later, [9]).unwrap();
    run_fail_closed(area.snapshot.clone(), move || {
        let _ = std::fs::read(&later);
    });
    panic!("the run was not refused");
}

/// A file the capture saw but could not copy (it stopped being readable
/// mid-capture) is known by device and inode: in a fail-closed run, a read
/// of it through a hard link outside the area, a name the snapshot never
/// saw, is refused rather than read live unnoticed.
#[test]
fn a_file_without_a_copy_is_refused_through_an_outside_alias() {
    if !is_inner_refusal_run(
        "a_file_without_a_copy_is_refused_through_an_outside_alias",
        "stopped being readable while the snapshot was taken",
    ) {
        return;
    }
    let mut area = area();
    let gone = area.live.path().join("u:object_r:gone_prop:s0");
    std::fs::write(&gone, [9]).unwrap();
    let metadata = std::fs::metadata(&gone).unwrap();
    area.snapshot
        .uncopied_files
        .push(detcore_model::config::PropertyFile {
            dev: metadata.dev(),
            ino: metadata.ino(),
            name: "u:object_r:gone_prop:s0".to_owned(),
        });
    let outside = tempfile::tempdir_in(area.live.path().parent().unwrap()).unwrap();
    let alias = outside.path().join("alias");
    std::fs::hard_link(&gone, &alias).unwrap();
    run_fail_closed(area.snapshot.clone(), move || {
        let _ = std::fs::read(&alias);
    });
    panic!("the run was not refused");
}
