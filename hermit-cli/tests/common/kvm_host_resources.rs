// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved. Licensed under the BSD-style license in LICENSE.

//! KVM refuses, by name, a run that its own process's host resources cannot
//! carry: at launch under a finite `RLIMIT_AS`, and at an exec whose memory
//! the host refuses. Neither reaches the guest as an errno.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;

use super::kvm_cancellation::bounded_command_polling;
use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The KVM backend's guest memory size under Hermit, which it reserves for
/// every guest process and once more for each exec in flight.
const GUEST_MEMORY: u64 = 3 * GIB;

/// The launch test's smallest limit lies below one reservation
/// ([`launch_refusal`]).
const _: () = assert!(2 * GIB < GUEST_MEMORY);

/// The launch refusal's reason, as it names it.
const RESERVATIONS: &str = "reserves 3 GiB (3221225472 bytes) of host address space for every \
     guest process, plus 3 GiB (3221225472 bytes) more for each exec in flight";

/// This process's soft and hard `RLIMIT_AS`.
fn address_space_limit() -> libc::rlimit {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only `limit`.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limit) },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    limit
}

/// A test's deadline: the time left of `budget` from `start`.
fn remaining(start: Instant, budget: Duration) -> Duration {
    budget
        .checked_sub(start.elapsed())
        .expect("the regression must fit its wall budget")
}

/// Under a finite soft `RLIMIT_AS`, `hermit --backend=kvm run` refuses to
/// start, by name and before the guest runs, and the refusal names both
/// remedies and the reason no finite limit can work. The same command without
/// the limit starts the guest, so the refusal is the limit's. One limit is
/// below a single 3 GiB reservation: had the check moved after the backend's
/// first reservation, that reservation would fail first, and the run would
/// end without the refusal.
pub(super) fn launch_refusal() {
    let _lock = super::hermit_run_guard();
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let start = Instant::now();
    let budget = Duration::from_secs(55);
    let inherited = address_space_limit();
    assert_eq!(
        inherited.rlim_cur,
        libc::RLIM_INFINITY,
        "this regression, like every KVM run, needs an unlimited soft RLIMIT_AS"
    );
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let root = tempfile::Builder::new()
        .prefix("kvm-launch-address-space-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .keep();
    eprintln!(
        "KVM launch address-space refusal artifacts: {}",
        root.display()
    );
    // The guest's first and only action: the run started it if this is on
    // stdout.
    let args = [
        "--backend=kvm",
        "run",
        "--strict",
        "--",
        "/bin/sh",
        "-c",
        "echo guest-started",
    ];

    let directory = root.join("unlimited");
    let status = bounded_command_with_timeout(
        &mut super::hermit_command(&args),
        &directory,
        remaining(start, budget),
    );
    let stderr = String::from_utf8(bounded_read(&directory.join("stderr"), 16 * MIB)).unwrap();
    // EXIT-CLASS: guest
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(
        bounded_read(&directory.join("stdout"), MIB),
        b"guest-started\n"
    );
    assert!(!stderr.contains("kvm_address_space_limit"), "{stderr}");

    // The hard limit this process runs under, as the refusal names it; only
    // an unlimited one is spelled out here.
    let inherited_hard = if inherited.rlim_max == libc::RLIM_INFINITY {
        "unlimited)"
    } else {
        ""
    };
    for (name, soft, hard, limits) in [
        (
            "soft-64-gib",
            64 * GIB,
            inherited.rlim_max,
            format!("(soft 64 GiB (68719476736 bytes), hard {inherited_hard}"),
        ),
        (
            "both-16-gib",
            16 * GIB,
            16 * GIB,
            "(soft 16 GiB (17179869184 bytes), hard 16 GiB (17179869184 bytes))".to_owned(),
        ),
        (
            "soft-2-gib",
            2 * GIB,
            inherited.rlim_max,
            format!("(soft 2 GiB (2147483648 bytes), hard {inherited_hard}"),
        ),
    ] {
        let directory = root.join(name);
        let mut command = super::hermit_command(&args);
        // SAFETY: the closure makes only a setrlimit call, which is
        // async-signal-safe, between fork and exec. It only lowers limits.
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: soft,
                    rlim_max: hard,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let status =
            bounded_command_with_timeout(&mut command, &directory, remaining(start, budget));
        let stderr = String::from_utf8(bounded_read(&directory.join("stderr"), 16 * MIB)).unwrap();
        // EXIT-CLASS: hermit
        assert_eq!(
            status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{name}: {stderr}"
        );
        assert!(
            stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "{name}: {stderr}"
        );
        for named in [
            format!(
                "kvm_address_space_limit: --backend=kvm refuses to start under a finite \
                 RLIMIT_AS {limits}"
            ),
            RESERVATIONS.to_owned(),
            "Hermit does not raise the limit itself".to_owned(),
            "`ulimit -v unlimited`".to_owned(),
            "bound its memory with a cgroup instead (memory.max".to_owned(),
            "the guest has not started".to_owned(),
        ] {
            assert!(
                stderr.contains(&named),
                "{name}: {named:?} missing: {stderr}"
            );
        }
        assert!(
            bounded_read(&directory.join("stdout"), MIB).is_empty(),
            "{name}: the guest must not have started"
        );
    }
}

/// Every process's parent, from `/proc/PID/stat` (the field after the
/// command name, which may itself hold spaces or parentheses). A kernel need
/// not offer `/proc/PID/task/TID/children`.
fn parents() -> Vec<(u32, u32)> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let pid = entry.ok()?.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (_, fields) = stat.rsplit_once(')')?;
            let parent = fields.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, parent))
        })
        .collect()
}

/// Process `pid`'s address-space size (`VmSize`), in bytes; 0 once it is gone.
fn address_space_bytes(pid: u32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmSize:"))
                .and_then(|value| value.trim().strip_suffix("kB"))
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .map_or(0, |kib| kib * 1024)
}

/// The "Max address space" line of process `pid`'s `/proc/PID/limits`.
fn address_space_limits_line(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/limits"))
        .unwrap()
        .lines()
        .find(|line| line.starts_with("Max address space"))
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .expect("/proc/PID/limits names the address-space limit")
}

/// The Hermit process that holds the KVM guest's memory: the one descendant of
/// this test process that runs the Hermit binary with at least one guest
/// memory reserved.
fn kvm_supervisor() -> u32 {
    let hermit = fs::metadata(super::hermit_test::hermit_binary()).unwrap();
    let parents = parents();
    let children = |parent: u32| {
        parents
            .iter()
            .filter(move |(_, of)| *of == parent)
            .map(|(pid, _)| *pid)
    };
    let mut pending = children(std::process::id()).collect::<Vec<_>>();
    let mut found = Vec::new();
    let mut seen = Vec::new();
    while let Some(pid) = pending.pop() {
        pending.extend(children(pid));
        let executable = fs::metadata(format!("/proc/{pid}/exe"));
        let size = address_space_bytes(pid);
        seen.push(format!("{pid}: {executable:?} VmSize {size}"));
        if executable.is_ok_and(|executable| {
            (executable.dev(), executable.ino()) == (hermit.dev(), hermit.ino())
        }) && size >= GUEST_MEMORY
        {
            found.push(pid);
        }
    }
    let [supervisor] = found[..] else {
        panic!("expected one KVM supervisor process, found {found:?} among {seen:#?}");
    };
    supervisor
}

/// An exec whose memory the host refuses ends the run as a policy refusal,
/// and the guest never sees an ENOMEM. A KVM exec first loads the new image
/// into a validation memory as large as the guest's (3 GiB), a reservation of
/// Hermit's own process. Launch refuses any finite `RLIMIT_AS`, so this
/// lowers the supervisor's soft limit from outside once the guest is
/// running, to 1 GiB above what the supervisor uses: room for anything it
/// allocates in the meantime, but not for the reservation. The guest, a bash
/// with `execfail` set so that a failed exec would let it go on, then execs
/// /bin/true. The run must exit 122 with the backend's diagnostic naming the
/// reservation and the lowered limit; the guest must print neither bash's
/// "Cannot allocate memory" nor its line after the exec. The same run left
/// unlimited execs /bin/true and exits 0.
pub(super) fn exec_refusal() {
    let _lock = super::hermit_run_guard();
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let start = Instant::now();
    let budget = Duration::from_secs(55);
    assert_eq!(
        address_space_limit().rlim_cur,
        libc::RLIM_INFINITY,
        "this regression, like every KVM run, needs an unlimited soft RLIMIT_AS"
    );
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let root = tempfile::Builder::new()
        .prefix("kvm-exec-host-resources-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .keep();
    eprintln!(
        "KVM exec host-resources refusal artifacts: {}",
        root.display()
    );
    for lowered in [false, true] {
        let directory = root.join(if lowered { "lowered" } else { "unlimited" });
        fs::create_dir_all(&directory).unwrap();
        let gate = directory.join("gate");
        let script = format!(
            "printf 'READY\\n'; while [ ! -e '{}' ]; do :; done; shopt -s execfail; \
             exec /bin/true; echo \"exec-returned $?\"",
            gate.display()
        );
        let args = [
            "--backend=kvm",
            "run",
            "--strict",
            "--",
            "/bin/bash",
            "-c",
            script.as_str(),
        ];
        let mut released = false;
        let mut lowered_limit = None;
        let status = bounded_command_polling(
            &mut super::hermit_command(&args),
            &directory,
            remaining(start, budget),
            || {
                if released
                    || !fs::read(directory.join("stdout"))
                        .is_ok_and(|stdout| stdout.starts_with(b"READY\n"))
                {
                    return;
                }
                if lowered {
                    let supervisor = kvm_supervisor();
                    let before = address_space_limits_line(supervisor);
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    // SAFETY: prlimit reads `limit` and writes nothing back.
                    let current = unsafe {
                        libc::prlimit(
                            supervisor as libc::pid_t,
                            libc::RLIMIT_AS,
                            std::ptr::null(),
                            &mut limit,
                        )
                    };
                    assert_eq!(current, 0, "{}", std::io::Error::last_os_error());
                    let soft = (address_space_bytes(supervisor) + GIB).min(limit.rlim_max);
                    limit.rlim_cur = soft;
                    // SAFETY: as above; only the soft limit drops.
                    let set = unsafe {
                        libc::prlimit(
                            supervisor as libc::pid_t,
                            libc::RLIMIT_AS,
                            &limit,
                            std::ptr::null_mut(),
                        )
                    };
                    assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
                    fs::write(
                        directory.join("supervisor.txt"),
                        format!(
                            "pid={supervisor}\nbefore: {before}\nafter: {}\n",
                            address_space_limits_line(supervisor)
                        ),
                    )
                    .unwrap();
                    lowered_limit = Some(soft);
                }
                fs::write(&gate, b"").unwrap();
                released = true;
            },
        );
        assert!(released, "the guest never said it was ready");
        let stdout = String::from_utf8(bounded_read(&directory.join("stdout"), MIB)).unwrap();
        let stderr = String::from_utf8(bounded_read(&directory.join("stderr"), 16 * MIB)).unwrap();
        // Neither a guest that saw the exec fail nor one that ran on after it.
        assert_eq!(stdout, "READY\n", "{stderr}");
        assert!(
            !stderr.contains("/bin/true: Cannot allocate memory"),
            "the guest saw ENOMEM: {stderr}"
        );
        let Some(soft) = lowered_limit else {
            // EXIT-CLASS: guest
            assert_eq!(status.code(), Some(0), "{stderr}");
            assert!(!stderr.contains("HERMIT_POLICY_REFUSAL"), "{stderr}");
            continue;
        };
        // EXIT-CLASS: hermit
        assert_eq!(status.code(), Some(HERMIT_POLICY_REFUSAL_EXIT), "{stderr}");
        assert!(
            stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "{stderr}"
        );
        for named in [
            "KVM exec: host address space ran out: reserving 3221225472 bytes of host address \
             space for the guest memory that validates the new image failed"
                .to_owned(),
            format!("under this process's RLIMIT_AS (soft {soft} bytes, "),
        ] {
            assert!(stderr.contains(&named), "{named:?} missing: {stderr}");
        }
    }
}
