/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// Deny flags-zero mount requests with EPERM, allowing the MS_RDONLY retry.
/// Install only in a test child: seccomp filters cannot be removed.
/// This also denies flags-zero tmpfs/sysfs mounts, so callers must avoid them.
pub fn deny_writable_mounts() -> std::io::Result<()> {
    let mut filter = [
        libc::sock_filter {
            code: 0x20, // BPF_LD | BPF_W | BPF_ABS
            jt: 0,
            jf: 0,
            k: 0, // seccomp_data.nr
        },
        libc::sock_filter {
            code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
            jt: 0,
            jf: 3,
            k: libc::SYS_mount as u32,
        },
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 40, // low word of seccomp_data.args[3] (mount flags) on x86_64
        },
        libc::sock_filter {
            code: 0x15,
            jt: 0,
            jf: 1,
            k: 0,
        },
        libc::sock_filter {
            code: 0x06, // BPF_RET | BPF_K
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: prctl copies this live, stack-allocated filter before returning.
    // Both calls are async-signal-safe, including use in a pre_exec callback.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1
            || libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Preserve the proc visibility and namespace checks from D121205913, while
/// requiring the forced fallback to have produced a read-only mount.
pub fn assert_readonly_proc(mounts: &str, status: &str, expected_pid: u32) {
    let options = mounts
        .lines()
        // /proc/mounts also lists the covered host mount. The newly stacked
        // proc mount is appended after it; inspect that mount, not its parent.
        .rev()
        .find_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.get(1) == Some(&"/proc") && fields.get(2) == Some(&"proc")).then(|| fields[3])
        })
        .expect("guest has no proc filesystem mounted at /proc");
    assert!(options.split(',').any(|option| option == "ro"), "{options}");
    for key in ["Pid", "NSpid"] {
        let value = status
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| (name == key).then_some(value.trim()))
            .unwrap_or_else(|| panic!("missing {key} in proc status: {status}"));
        assert_eq!(value, expected_pid.to_string(), "{key}: {status}");
    }
}
