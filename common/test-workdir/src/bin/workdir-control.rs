/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Actual filesystem and network namespace controls, run in a fresh native process.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use hermit_test_workdir::BindMount;
use hermit_test_workdir::Isolation;
use hermit_test_workdir::with_isolated_workdir;
use hermit_test_workdir::with_isolation;

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

fn statfs_type(path: &str) -> i64 {
    let directory = File::open(path).unwrap();
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    assert_eq!(
        unsafe { libc::fstatfs(directory.as_raw_fd(), filesystem.as_mut_ptr()) },
        0
    );
    unsafe { filesystem.assume_init() }.f_type
}

/// One physical run with binds: a private, otherwise empty tmpfs /tmp holding
/// the preserved path and the binds, whose sources sit below the original /tmp.
fn check_binds() -> io::Result<()> {
    let pid = std::process::id();
    let source = std::path::PathBuf::from(format!("/tmp/hermit-workdir-control-src-{pid}"));
    let file_source = std::path::PathBuf::from(format!("/tmp/hermit-workdir-control-file-{pid}"));
    let keep = std::path::PathBuf::from(format!("/tmp/hermit-workdir-control-keep-{pid}"));
    let scratch = std::path::PathBuf::from(format!("/tmp/hermit-workdir-control-scratch-{pid}"));
    // Removed however the check ends, including a setup refusal or a panic.
    struct Cleanup(Vec<std::path::PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_dir_all(path).or_else(|_| std::fs::remove_file(path));
            }
        }
    }
    let _cleanup = Cleanup(vec![source.clone(), file_source.clone(), keep.clone()]);
    std::fs::create_dir(&source)?;
    std::fs::write(source.join("input"), b"bound input")?;
    std::fs::write(&file_source, b"bound file")?;
    std::fs::create_dir(&keep)?;
    std::fs::write(keep.join("records"), b"preserved")?;
    let parent_tmp = statfs_type("/tmp");
    let isolation = Isolation {
        local_networking: false,
        test_workdir: false,
        binds: vec![
            BindMount {
                source: source.clone(),
                target: "/tmp/e2e/bound".into(),
            },
            BindMount {
                source: file_source.clone(),
                target: "/tmp/e2e/file".into(),
            },
        ],
        preserve: vec![keep.clone()],
    };
    let (first, second) = (
        with_isolation(&isolation, || {
            assert_eq!(statfs_type("/tmp"), libc::TMPFS_MAGIC);
            let mut names = std::fs::read_dir("/tmp")
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            let mut expected = vec![
                "e2e".to_string(),
                keep.file_name().unwrap().to_str().unwrap().into(),
            ];
            expected.sort();
            assert_eq!(
                names, expected,
                "the private /tmp holds only the binds and the preserved path"
            );
            assert_eq!(
                std::fs::read("/tmp/e2e/bound/input").unwrap(),
                b"bound input"
            );
            assert_eq!(std::fs::read("/tmp/e2e/file").unwrap(), b"bound file");
            assert_eq!(std::fs::read(keep.join("records")).unwrap(), b"preserved");
            std::fs::write("/tmp/e2e/bound/output", b"written in the run").unwrap();
            std::fs::write(&scratch, b"private").unwrap();
            namespace()
        })?,
        with_isolation(&isolation, || {
            assert!(
                !scratch.exists(),
                "a second physical run starts with a fresh /tmp"
            );
            namespace()
        })?,
    );
    assert_ne!(inode(&first), inode(&second));
    assert_eq!(statfs_type("/tmp"), parent_tmp, "parent /tmp was replaced");
    assert_eq!(
        std::fs::read(source.join("output"))?,
        b"written in the run",
        "a write through the bind reaches the host source"
    );
    assert!(
        !scratch.exists(),
        "a file made in the private /tmp leaked to the parent"
    );
    let launched = AtomicBool::new(false);
    let missing = Isolation {
        binds: vec![BindMount {
            source: format!("/tmp/hermit-workdir-control-missing-{pid}").into(),
            target: "/tmp/e2e/bound".into(),
        }],
        ..Isolation::default()
    };
    let error = with_isolation(&missing, || launched.store(true, Ordering::SeqCst)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    assert!(
        !launched.load(Ordering::SeqCst),
        "a missing source launched the callback"
    );
    println!(
        "two runs with binds have private fresh tmpfs /tmp holding only the binds and the preserved path; sources below the original /tmp are reachable; writes reach the source; a missing source fails before launch"
    );
    Ok(())
}

fn network_namespace() -> io::Result<File> {
    File::open("/proc/thread-self/ns/net")
}

fn sysfs_interfaces() -> io::Result<Vec<String>> {
    let mut names = std::fs::read_dir("/sys/class/net")?
        .map(|entry| {
            entry.map(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .expect("interface name is UTF-8")
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn kernel_interfaces() -> io::Result<Vec<String>> {
    let mut names = std::fs::read_to_string("/proc/thread-self/net/dev")?
        .lines()
        .skip(2)
        .map(|line| {
            line.split_once(':')
                .expect("kernel interface row")
                .0
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

fn check_network_entry(credentials: (libc::uid_t, libc::gid_t)) -> io::Result<File> {
    assert_eq!(
        unsafe { (libc::geteuid(), libc::getegid()) },
        credentials,
        "Local changed callback credentials"
    );
    let current = network_namespace()?;
    let expected = vec!["lo".to_string()];
    assert_eq!(
        kernel_interfaces()?,
        expected,
        "kernel network must contain only loopback"
    );
    assert_eq!(
        sysfs_interfaces()?,
        expected,
        "fresh sysfs must match the isolated network"
    );
    assert_eq!(statfs_type("/sys"), libc::SYSFS_MAGIC);

    // Real traffic demonstrates that loopback is up, rather than merely named lo.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let mut sender = std::net::TcpStream::connect(listener.local_addr()?)?;
    let (mut receiver, _) = listener.accept()?;
    sender.write_all(b"local network")?;
    let mut bytes = [0_u8; 13];
    receiver.read_exact(&mut bytes)?;
    assert_eq!(&bytes, b"local network");
    let inherited = std::thread::spawn(|| network_namespace().map(|file| inode(&file)))
        .join()
        .expect("namespace worker panicked")?;
    assert_eq!(
        inherited,
        inode(&current),
        "workers must inherit the run's netns"
    );
    // Holding the descriptor after return prevents sequential inode reuse.
    Ok(current)
}

fn check_networking() -> io::Result<()> {
    // This command runs in a freshly exec'd child, never in the threaded Rust
    // test harness. User namespace setup must precede the first worker.
    assert_eq!(std::fs::read_dir("/proc/self/task")?.count(), 1);
    if !hermit_test_workdir::has_cap_sys_admin()? {
        hermit_test_workdir::enter_root_user_namespace()?;
    }
    let caller = network_namespace()?;
    let caller_mount = namespace();
    let caller_sysfs = std::fs::metadata("/sys/class/net")?;
    let caller_sysfs_identity = (caller_sysfs.dev(), caller_sysfs.ino());
    let credentials = unsafe { (libc::geteuid(), libc::getegid()) };
    let unchanged = || -> io::Result<()> {
        assert_eq!(
            inode(&network_namespace()?),
            inode(&caller),
            "caller netns changed"
        );
        assert_eq!(
            inode(&namespace()),
            inode(&caller_mount),
            "caller mount namespace changed"
        );
        let sysfs = std::fs::metadata("/sys/class/net")?;
        assert_eq!(
            (sysfs.dev(), sysfs.ino()),
            caller_sysfs_identity,
            "caller sysfs changed"
        );
        assert_eq!(
            unsafe { (libc::geteuid(), libc::getegid()) },
            credentials,
            "caller credentials changed"
        );
        Ok(())
    };
    let host = Isolation::default();
    assert!(host.is_empty(), "Host without mounts needs no isolation");
    let host_before = with_isolation(&host, || -> io::Result<File> {
        let current = network_namespace()?;
        assert_eq!(inode(&current), inode(&caller), "Host netns was replaced");
        let sysfs = std::fs::metadata("/sys/class/net")?;
        assert_eq!(
            (sysfs.dev(), sysfs.ino()),
            caller_sysfs_identity,
            "Host sysfs was replaced"
        );
        Ok(current)
    })??;
    unchanged()?;

    let local = Isolation {
        local_networking: true,
        ..Isolation::default()
    };
    assert!(
        !local.is_empty(),
        "Local needs isolation even without filesystem mounts"
    );
    let first = with_isolation(&local, || check_network_entry(credentials))??;
    unchanged()?;
    let second = with_isolation(&local, || check_network_entry(credentials))??;
    unchanged()?;
    assert_ne!(
        inode(&first),
        inode(&caller),
        "first Local run shares caller netns"
    );
    assert_ne!(
        inode(&second),
        inode(&caller),
        "second Local run shares caller netns"
    );
    assert_ne!(inode(&first), inode(&second), "physical runs share netns");

    // The kernel's descriptor directory can never contain a descriptor -1.
    // This valid bind request fails after Local netns/sysfs/loopback setup.
    let missing_source = Isolation {
        local_networking: true,
        binds: vec![BindMount {
            source: "/proc/thread-self/fd/-1".into(),
            target: "/tmp/network-control-missing".into(),
        }],
        ..Isolation::default()
    };
    let launched = AtomicBool::new(false);
    let error =
        with_isolation(&missing_source, || launched.store(true, Ordering::SeqCst)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    assert!(
        !launched.load(Ordering::SeqCst),
        "Local setup failure launched the callback"
    );
    unchanged()?;

    let host_after = with_isolation(&host, network_namespace)??;
    assert_eq!(
        inode(&host_before),
        inode(&host_after),
        "Local changed later Host networking"
    );
    unchanged()?;
    println!(
        "networking: two physical Local runs use distinct netns with working loopback and matching fresh sysfs; caller and Host netns, sysfs, mounts and credentials remain unchanged"
    );
    Ok(())
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
        [command] if command == "binds" => check_binds()?,
        [command] if command == "networking" => check_networking()?,
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
                "usage: workdir-control check | binds | networking | expect-setup-error permission-denied|not-found",
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
