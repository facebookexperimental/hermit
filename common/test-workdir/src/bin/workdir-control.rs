/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Actual mount-namespace controls, run explicitly inside the pinned root.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use hermit_test_workdir::with_isolated_workdir;

fn namespace() -> File {
    File::open("/proc/thread-self/ns/mnt").unwrap()
}

fn inode(namespace: &File) -> u64 {
    namespace.metadata().unwrap().ino()
}

fn check_entry(barrier: Option<&Barrier>, transport: &std::path::Path) -> File {
    let current_namespace = namespace();
    let directory = File::open("/test").unwrap();
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    assert_eq!(
        unsafe { libc::fstatfs(directory.as_raw_fd(), filesystem.as_mut_ptr()) },
        0
    );
    assert_eq!(
        unsafe { filesystem.assume_init() }.f_type,
        libc::TMPFS_MAGIC
    );
    assert_eq!(std::fs::read(transport).unwrap(), b"transport visible");
    assert_eq!(
        std::fs::read_dir("/test").unwrap().count(),
        0,
        "physical run must start empty"
    );
    std::env::set_current_dir("/test").unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open("same-name")
        .unwrap();
    file.write_all(b"owned by this run").unwrap();
    if let Some(barrier) = barrier {
        barrier.wait();
    }
    assert_eq!(std::fs::read("same-name").unwrap(), b"owned by this run");
    // New workers inherit this namespace. Returning the open namespace handle
    // prevents sequential namespace-inode reuse from making the check vacuous.
    let worker_inode = std::thread::spawn(|| inode(&namespace())).join().unwrap();
    assert_eq!(worker_inode, inode(&current_namespace));
    current_namespace
}

fn main() -> io::Result<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let parent = namespace();
    let parent_cwd = std::env::current_dir()?;
    let parent_marker = std::env::var_os(hermit_test_workdir::REQUEST_ENV);
    match arguments.as_slice() {
        [command] if command == "check" => {
            let transport_path =
                std::env::temp_dir().join(format!("hermit-workdir-control-{}", std::process::id()));
            let mut transport = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&transport_path)?;
            transport.write_all(b"transport visible")?;
            let marker =
                std::path::PathBuf::from(format!("/test/parent-only-{}", std::process::id()));
            let mut marker_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)?;
            marker_file.write_all(b"parent filesystem must remain visible")?;
            let parent_directory = std::fs::metadata("/test")?;
            let parent_identity = (parent_directory.dev(), parent_directory.ino());
            println!(
                "parent namespace={} /test dev={} inode={}",
                inode(&parent),
                parent_identity.0,
                parent_identity.1
            );
            let check_parent = || {
                let directory = std::fs::metadata("/test").unwrap();
                assert_eq!(
                    (directory.dev(), directory.ino()),
                    parent_identity,
                    "parent /test filesystem was replaced"
                );
                assert_eq!(
                    std::fs::read(&marker).unwrap(),
                    b"parent filesystem must remain visible",
                    "parent /test marker was hidden or changed"
                );
            };
            std::thread::scope(|scope| -> io::Result<()> {
                // This sibling keeps the original namespace and independently
                // resolves /test after every completed physical-run control.
                // Both channels are scoped here: early return/panic drops the
                // sender before scope joins the sibling, so it cannot strand it.
                let (requests, receive) = std::sync::mpsc::channel::<()>();
                let (acknowledge, acknowledgements) = std::sync::mpsc::channel::<()>();
                let sibling = scope.spawn(move || {
                    while receive.recv().is_ok() {
                        check_parent();
                        acknowledge.send(()).unwrap();
                    }
                });
                let unchanged = || {
                    check_parent();
                    requests.send(()).unwrap();
                    acknowledgements.recv().unwrap();
                };
                let first = with_isolated_workdir(|| check_entry(None, &transport_path))?;
                unchanged();
                println!("sequential run 1 namespace={}", inode(&first));
                let second = with_isolated_workdir(|| check_entry(None, &transport_path))?;
                unchanged();
                println!("sequential run 2 namespace={}", inode(&second));
                assert_ne!(inode(&first), inode(&second));
                assert_ne!(inode(&first), inode(&parent));
                assert_ne!(inode(&second), inode(&parent));
                let barrier = Barrier::new(2);
                let (third, fourth) = std::thread::scope(|scope| {
                    let run = || {
                        with_isolated_workdir(|| check_entry(Some(&barrier), &transport_path))
                            .unwrap()
                    };
                    let third = scope.spawn(run);
                    let fourth = scope.spawn(run);
                    (third.join().unwrap(), fourth.join().unwrap())
                });
                unchanged();
                println!("concurrent namespaces={} {}", inode(&third), inode(&fourth));
                assert_ne!(inode(&third), inode(&fourth));
                assert_ne!(inode(&third), inode(&parent));
                assert_ne!(inode(&fourth), inode(&parent));
                let failure = with_isolated_workdir(|| Err::<(), _>("callback failure"))?;
                assert_eq!(failure, Err("callback failure"));
                unchanged();
                let panic = std::panic::catch_unwind(|| {
                    with_isolated_workdir(|| std::panic::panic_any("callback panic")).unwrap();
                })
                .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"callback panic"));
                unchanged();
                drop(requests);
                sibling.join().unwrap();
                Ok(())
            })?;
            std::fs::remove_file(marker)?;
            std::fs::remove_file(transport_path)?;
            println!(
                "two sequential and two concurrent runs have private empty tmpfs; parent and sibling filesystems stay unchanged; workers inherit the private namespace; callback error and panic remain failures"
            );
        }
        [command, kind] if command == "expect-setup-error" => {
            let expected = match kind.as_str() {
                "permission-denied" => io::ErrorKind::PermissionDenied,
                "not-found" => io::ErrorKind::NotFound,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unknown error kind",
                    ));
                }
            };
            let launched = AtomicBool::new(false);
            let error =
                with_isolated_workdir(|| launched.store(true, Ordering::SeqCst)).unwrap_err();
            assert_eq!(error.kind(), expected, "{error}");
            assert!(
                !launched.load(Ordering::SeqCst),
                "setup failure launched the callback"
            );
            println!("expected setup failure before launch: {error}");
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: workdir-control check | expect-setup-error permission-denied|not-found",
            ));
        }
    }
    assert_eq!(
        inode(&namespace()),
        inode(&parent),
        "parent namespace changed"
    );
    assert_eq!(std::env::current_dir()?, parent_cwd, "parent cwd changed");
    assert_eq!(
        std::env::var_os(hermit_test_workdir::REQUEST_ENV),
        parent_marker,
        "parent marker changed"
    );
    Ok(())
}
