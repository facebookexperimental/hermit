/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Tests stat related syscall functionality of detcore.

// Fend off the RUSTFIXDEPS dependency linter:
use detcore as _;
use detcore_testutils as _;
use libc as _;
use nix as _;
use rand as _;
use reverie as _;
use reverie_ptrace as _;
use tempfile as _;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

#[cfg(all(not(sanitized), test))]
mod non_sanitized_tests {
    use std::mem::MaybeUninit;

    use detcore::Detcore;
    use detcore_testutils::det_test_all_configs;
    use nix::sys::stat::FileStat;
    use nix::unistd;
    use reverie::Errno;
    use reverie_ptrace::testing::check_fn;

    const FILE_NON_EXIST: *const libc::c_char =
        "/this_file_definitely_dost_not_exist!#\0".as_ptr() as _;

    /// Needs a null terminated input
    fn stat_file(name: &str) -> FileStat {
        let file = name.as_ptr() as _;
        let mut maybe_stat: MaybeUninit<libc::stat> = MaybeUninit::uninit();
        let res = Errno::result(unsafe { libc::stat(file, maybe_stat.as_mut_ptr()) });
        if let Err(e) = res {
            eprintln!("Failed while attempting to stat file {:?}", name);
            panic!("libc:stat returned error {}", e);
        }
        unsafe { maybe_stat.assume_init() }
    }

    #[test]
    fn stat_blksize_sanity() {
        det_test_all_configs(
            |cfg| cfg.virtualize_metadata,
            |_| {
                let stat = stat_file("/etc/passwd\0");
                assert!(stat.st_blksize > 0, "blksize must be reasonable, not zero");
            },
            detcore_testutils::expect_success,
        );
    }

    #[test]
    fn stat_two_files() {
        det_test_all_configs(
            |cfg| cfg.virtualize_metadata,
            |cfg| {
                // For now we blithely assume every Linux host system has these two files:
                let stat1 = stat_file("/bin/sh\0");
                let stat2 = stat_file("/bin/ls\0");
                if cfg.virtualize_metadata {
                    // Expecting constant defaults in these fields under virtualize_metadata:
                    assert_eq!(stat1.st_atime, stat2.st_atime);
                    assert_eq!(stat1.st_atime_nsec, stat2.st_atime_nsec);
                    assert_eq!(stat1.st_ctime, stat2.st_ctime);
                    assert_eq!(stat1.st_ctime_nsec, stat2.st_ctime_nsec);
                    // Mtime should match because we start files off at the same starting point:
                    assert_eq!(stat1.st_mtime, stat2.st_mtime);
                    assert_eq!(stat1.st_mtime_nsec, stat2.st_mtime_nsec);
                    assert_ne!(stat1.st_ino, stat2.st_ino);
                    // TODO: assert same st_dev, gid, uid when virtualize_metadata hides those.
                }
            },
            detcore_testutils::expect_success,
        );
    }

    // TODO: FIXME. This test is returning mysterious ENOENT errors:
    //   thread 'non_sanitized_tests::stat_temp_file::middle_detcore' panicked at 'libc:stat returned error -2 ENOENT (No such file or directory)', hermetic_infra/detcore/tests/stats/mod.rs:33:13
    // It previously caused a problem here: D27721151
    mod stat_temp_file {
        use std::io::Write;

        use tempfile::Builder;

        fn run_test(uniq: &str) {
            let path = {
                let mut file = Builder::new()
                    .prefix(uniq)
                    .suffix(".txt")
                    .rand_bytes(5)
                    .tempfile()
                    .unwrap();
                let text = "Hello world";
                file.write_all(text.as_bytes()).unwrap();
                file.into_temp_path()
            };
            println!("Wrote to file at path: {:?}", path);
            let mut path_string: String = path.to_str().unwrap().to_string();
            path_string.push_str("\0");
            let stat = super::stat_file(path_string.as_str());
            println!("Stat result: {:?}", stat);
            // The TempPath will delete the file when it passes out of scope.
        }

        #[test]
        fn raw() {
            use rand::Rng;
            let mut prefix = "raw_".to_string();
            let num: u128 = rand::thread_rng().gen();
            prefix.push_str(&num.to_string());
            prefix.push_str("_");
            run_test(&prefix)
        }

        fn detcore(_mode: &str, cfg: &detcore::Config) {
            use rand::Rng;
            // If this runs deterministically, it will collide, so a unique value is
            // generated outside the deterministic section:
            let mut prefix = "detcore_".to_string();
            let num: u128 = rand::thread_rng().gen();
            prefix.push_str(&num.to_string());
            prefix.push_str("_");
            detcore_testutils::det_test_fn_with_config(
                cfg.virtualize_metadata,
                || run_test(&prefix),
                cfg.clone(),
                detcore_testutils::expect_success,
            );
        }

        // Boilerplate:
        // #[test]
        // fn bottom_detcore() {
        //     detcore("bottom", &detcore_testutils::BOTTOM_CFG);
        // }

        // #[test]
        // fn middle_detcore() {
        //     detcore("middle", &detcore_testutils::MIDDLE_CFG);
        // }

        #[test]
        fn default_detcore() {
            detcore("default", &Default::default());
        }

        #[test]
        fn top_detcore() {
            detcore("top", &detcore_testutils::TOP_CFG);
        }
    }

    #[test]
    fn stat_with_nullptr_errno_sanity() {
        check_fn::<Detcore, _>(move || {
            let res = Errno::result(unsafe {
                libc::stat("/dev/null\0".as_ptr() as _, std::ptr::null_mut())
            });
            assert_eq!(res, Err(Errno::EFAULT));

            let res = Errno::result(unsafe { libc::stat(FILE_NON_EXIST, std::ptr::null_mut()) });
            assert_eq!(res, Err(Errno::ENOENT));
        });
    }

    #[test]
    fn fstat_with_nullptr_errno_sanity() {
        check_fn::<Detcore, _>(move || {
            let res = Errno::result(unsafe { libc::fstat(0, std::ptr::null_mut()) });
            assert_eq!(res, Err(Errno::EFAULT));

            let _ = unistd::close(255);
            let res = Errno::result(unsafe { libc::fstat(255, std::ptr::null_mut()) });
            assert_eq!(res, Err(Errno::EBADF));
        });
    }

    #[test]
    fn stat_nonexist_file() {
        check_fn::<Detcore, _>(move || {
            let current_dir = ".\0".as_ptr() as _;
            let res = Errno::result(unsafe { libc::stat(current_dir, std::ptr::null_mut()) });
            assert_eq!(res, Err(Errno::EFAULT));

            let res = Errno::result(unsafe { libc::stat(FILE_NON_EXIST, std::ptr::null_mut()) });
            assert_eq!(res, Err(Errno::ENOENT));

            let mut stat: MaybeUninit<libc::stat> = MaybeUninit::uninit();
            let res = Errno::result(unsafe { libc::stat(FILE_NON_EXIST, stat.as_mut_ptr()) });
            assert_eq!(res, Err(Errno::ENOENT));
        });
    }

    #[test]
    fn stat_lstat_symlink() {
        check_fn::<Detcore, _>(move || {
            let this_exe = "/proc/self/exe\0"; // .as_ptr() as _;

            let stat = stat_file(this_exe);
            assert_ne!(stat.st_mode & libc::S_IFMT, libc::S_IFLNK);

            let mut maybe_stat: MaybeUninit<libc::stat> = MaybeUninit::uninit();
            let res = Errno::result(unsafe {
                libc::lstat(this_exe.as_ptr() as _, maybe_stat.as_mut_ptr())
            });
            assert!(res.is_ok());

            let stat = unsafe { maybe_stat.assume_init() };
            assert_eq!(stat.st_mode & libc::S_IFMT, libc::S_IFLNK);
        });
    }

    #[test]
    fn stat_fstat_is_consistent() {
        check_fn::<Detcore, _>(move || {
            let this_exe = "/proc/self/exe\0".as_ptr() as _;

            let mut maybe_stat: MaybeUninit<libc::stat> = MaybeUninit::uninit();
            let res = Errno::result(unsafe { libc::stat(this_exe, maybe_stat.as_mut_ptr()) });
            assert!(res.is_ok());

            let stat1 = unsafe { maybe_stat.assume_init() };

            let fd = unsafe { libc::openat(libc::AT_FDCWD, this_exe, 0) };
            assert!(fd > 0);

            let res = Errno::result(unsafe { libc::fstat(fd, maybe_stat.as_mut_ptr()) });
            assert!(res.is_ok());

            let stat2 = unsafe { maybe_stat.assume_init() };
            let res = Errno::result(unsafe { libc::close(fd) });
            assert!(res.is_ok());

            assert_eq!(stat1.st_ino, stat2.st_ino);
        });
    }
}
