// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Parent thread loops sched_yield and child thread tries to exit_group.

fn main() {
    if matches!(std::env::var("HERMIT_MODE"), Ok(x) if x == "chaos" || x == "chaosreplay" || x == "tracereplay")
    {
        // This test is prone to deadlocking in the current chaos mode because
        // 1. There are no branches to cause preemption points.
        // 2. Timeouts are reset by syscalls.
        // TODO(T100400409) enable this test by (1) adding branches when (2) is fixed
        eprintln!("Skipping test in unsupported mode.");
        return;
    }
    let _ = std::thread::spawn(move || {
        loop {
            let _ = unsafe { libc::syscall(libc::SYS_exit_group, 0) };
        }
    });
    loop {
        let _ = unsafe { libc::syscall(libc::SYS_sched_yield, 0) };
    }
}
