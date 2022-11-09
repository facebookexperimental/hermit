/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! misc syscall tests

use nix::unistd;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

/// Tests SYS_uname
#[test]
fn getrandom_intercepted() {
    detcore_testutils::det_test_fn(|| {
        let mut got: u64 = 0;
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_getrandom, &mut got as *const u64 as u64, 8, 0) },
            8
        );
        println!("SYS_getrandom 1st result: {}", got);

        let dev_urandom = b"/dev/urandom\0";
        let fd = unsafe { libc::open(dev_urandom[..].as_ptr() as *const _, libc::O_RDONLY, 0o644) };
        assert!(fd >= 0);

        assert_eq!(
            unsafe { libc::syscall(libc::SYS_read, fd, &mut got as *const u64 as u64, 8) },
            8
        );
        println!("/dev/urandom result: {}", got);
        assert!(unistd::close(fd).is_ok());

        let dev_random = b"/dev/random\0";
        let fd = unsafe { libc::open(dev_random[..].as_ptr() as *const _, libc::O_RDONLY, 0o644) };
        assert!(fd >= 0);

        assert_eq!(
            unsafe { libc::syscall(libc::SYS_read, fd, &mut got as *const u64 as u64, 8) },
            8
        );
        println!("/dev/random result: {}", got);
        assert!(unistd::close(fd).is_ok());
    })
}

#[test]
fn has_rdrand_without_detcore() {
    let cpuid = raw_cpuid::CpuId::new();
    let feature = cpuid.get_feature_info();
    assert!(feature.is_some());
    let feature = feature.unwrap();
    assert!(feature.has_rdrand());

    if let Some(feature_ext) = cpuid.get_extended_feature_info() {
        assert!(feature_ext.has_rdseed());
    }
}

#[test]
fn rdrand_rdseed_is_masked() {
    detcore_testutils::det_test_fn(|| {
        let cpuid = raw_cpuid::CpuId::new();
        let feature = cpuid.get_feature_info();
        assert!(feature.is_some());
        let feature = feature.unwrap();
        assert!(!feature.has_rdrand());

        if let Some(feature_ext) = cpuid.get_extended_feature_info() {
            assert!(!feature_ext.has_rdseed());
        }
    })
}
