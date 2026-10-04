/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression guest for the Landlock sandbox syscalls (KVM ratchet round 14).
//!
//! `landlock_create_ruleset`, `landlock_add_rule`, and `landlock_restrict_self`
//! were classified Unsupported, which fail-closes under `--strict` and aborts
//! any program that probes or installs a Landlock sandbox. Landlock is an LSM
//! whose availability and ABI version depend on the host kernel build
//! (`CONFIG_SECURITY_LANDLOCK`) and runtime LSM stacking, so forwarding these to
//! the host is nondeterministic. Detcore now determinizes them to a fixed
//! `ENOSYS` -- the errno a kernel built without Landlock returns -- so a guest
//! sees a consistent "sandbox unavailable" answer independent of the host.
//!
//! glibc does not wrap these syscalls, so this guest issues them raw via
//! `libc::syscall`. It must observe `ENOSYS` for each. Runs deterministically
//! under `hermit run --strict --verify`.

use std::io::Error;

/// Issue a raw syscall and return the errno it failed with (asserting it did
/// fail with -1, which is how `libc::syscall` reports a negative kernel return).
fn expect_enosys(name: &str, ret: libc::c_long) {
    assert_eq!(ret, -1, "{name} should fail (-1) under hermit, got {ret}");
    let errno = Error::last_os_error().raw_os_error().unwrap();
    assert_eq!(
        errno,
        libc::ENOSYS,
        "{name} should be determinized to ENOSYS, got errno {errno}"
    );
}

fn main() {
    // 1. The canonical Landlock ABI-version probe:
    //    landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION).
    //    On a Landlock kernel this returns the ABI version (>0); Detcore
    //    determinizes it to ENOSYS.
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    let ver = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    expect_enosys("landlock_create_ruleset(version probe)", ver);

    // 2. landlock_add_rule against a bogus ruleset fd: must also refuse ENOSYS
    //    before any argument validation (the syscall itself is unavailable).
    let add = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            -1i32,
            0u32,
            std::ptr::null::<libc::c_void>(),
            0u32,
        )
    };
    expect_enosys("landlock_add_rule", add);

    // 3. landlock_restrict_self against a bogus ruleset fd: same deterministic
    //    ENOSYS.
    let restrict = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, -1i32, 0u32) };
    expect_enosys("landlock_restrict_self", restrict);

    println!("Landlock syscalls determinized to ENOSYS. Test complete.");
}
