/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const RUNS: usize = 5;

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_procfs(path: &str) -> Vec<u8> {
    read_procfs_at_epoch(path, None)
}

fn read_procfs_at_epoch(path: &str, epoch: Option<&str>) -> Vec<u8> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    if let Some(epoch) = epoch {
        command.arg(format!("--epoch={epoch}"));
    }
    command.args(["--", "/bin/cat", path]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        output.status.success(),
        "procfs read failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

fn assert_deterministic(path: &str, validate: impl Fn(&[u8])) {
    let _guard = hermit_run_lock();
    let first = read_procfs(path);
    assert!(!first.is_empty(), "{path} unexpectedly returned no data");
    validate(&first);

    for run in 2..=RUNS {
        let output = read_procfs(path);
        assert_eq!(
            first,
            output,
            "{path} differed between run 1 and run {run}\nrun 1: {}\nrun {run}: {}",
            String::from_utf8_lossy(&first),
            String::from_utf8_lossy(&output),
        );
    }
}

#[test]
fn proc_self_maps_is_deterministic() {
    assert_deterministic("/proc/self/maps", |contents| {
        let text = std::str::from_utf8(contents).expect("maps should be UTF-8");
        let mut previous_start = 0;
        for line in text.lines() {
            let range = line.split_whitespace().next().expect("missing maps range");
            let (start, end) = range.split_once('-').expect("invalid maps range");
            let start = u64::from_str_radix(start, 16).expect("invalid maps start");
            let end = u64::from_str_radix(end, 16).expect("invalid maps end");
            assert!(start < end, "empty or reversed maps range");
            assert!(start >= previous_start, "maps are not address ordered");
            previous_start = start;
        }
    });
}

#[test]
fn proc_self_stat_is_deterministic() {
    assert_deterministic("/proc/self/stat", |contents| {
        let text = std::str::from_utf8(contents).expect("stat should be UTF-8");
        let comm_end = text.rfind(") ").expect("stat has no comm terminator");
        let fields = text[comm_end + 2..].split_whitespace().collect::<Vec<_>>();
        assert!(fields.len() >= 50, "stat has too few fields");
        for field in [10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 24, 39, 42, 43, 44] {
            assert_eq!(fields[field - 3], "0", "stat field {field} is volatile");
        }
    });
}

#[test]
fn proc_self_status_is_deterministic() {
    assert_deterministic("/proc/self/status", |contents| {
        let text = std::str::from_utf8(contents).expect("status should be UTF-8");
        let pid = text
            .lines()
            .find_map(|line| line.strip_prefix("Pid:\t"))
            .expect("status has no PID")
            .parse::<u32>()
            .expect("status PID should be numeric");
        assert!(pid > 0);
        assert!(text.contains("Cpus_allowed:\t00000000,00000000,00000000,00000001\n"));
        assert!(text.contains("Cpus_allowed_list:\t0\n"));
        assert!(text.contains("voluntary_ctxt_switches:\t0\n"));
        assert!(text.contains("nonvoluntary_ctxt_switches:\t0\n"));
    });
}

#[test]
fn proc_self_cmdline_is_deterministic() {
    assert_deterministic("/proc/self/cmdline", |contents| {
        assert!(contents.contains(&0), "cmdline should be NUL-delimited");
        assert!(
            contents
                .windows(b"/proc/self/cmdline".len())
                .any(|window| window == b"/proc/self/cmdline")
        );
    });
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-861): Review deterministic kernel I/O accounting coverage.
#[test]
fn proc_diskstats_uses_synthetic_counters() {
    assert_deterministic("/proc/diskstats", |contents| {
        let text = std::str::from_utf8(contents).expect("diskstats should be UTF-8");
        for line in text.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            assert!(fields.len() >= 4, "diskstats line has too few fields");
            for (index, value) in fields[3..].iter().enumerate() {
                let expected = match index {
                    0 | 4 => "1",
                    2 | 6 => "8",
                    _ => "0",
                };
                assert_eq!(*value, expected, "unexpected disk counter {index}");
            }
        }
    });
}

#[test]
fn proc_pid_io_uses_zero_counters() {
    assert_deterministic("/proc/1/io", |contents| {
        let text = std::str::from_utf8(contents).expect("process io should be UTF-8");
        assert!(text.lines().all(|line| line.ends_with(": 0")));
    });
}

#[test]
fn proc_cpuinfo_is_deterministic() {
    assert_deterministic("/proc/cpuinfo", |contents| {
        let text = std::str::from_utf8(contents).expect("cpuinfo should be UTF-8");
        assert!(text.contains("processor\t:"));
        let frequencies = text
            .lines()
            .filter(|line| line.starts_with("cpu MHz"))
            .collect::<Vec<_>>();
        assert!(
            frequencies.iter().all(|line| *line == "cpu MHz\t\t: 0.000"),
            "cpuinfo contains a volatile frequency"
        );
    });
}

#[test]
fn proc_loadavg_uses_virtual_values() {
    assert_deterministic("/proc/loadavg", |contents| {
        assert_eq!(contents, b"0.00 0.00 0.00 1/1 1\n");
    });
}

#[test]
fn proc_uptime_uses_virtual_time() {
    assert_deterministic("/proc/uptime", |contents| {
        assert_eq!(contents, b"120.00 0.00\n");
    });
}

#[test]
fn proc_entropy_available_is_deterministic() {
    assert_deterministic("/proc/sys/kernel/random/entropy_avail", |contents| {
        let _entropy = std::str::from_utf8(contents)
            .expect("entropy_avail should be UTF-8")
            .trim()
            .parse::<u32>()
            .expect("entropy_avail should be numeric");
    });
}

#[test]
fn proc_pressure_uses_virtual_zero_values() {
    for path in [
        "/proc/pressure/cpu",
        "/proc/pressure/io",
        "/proc/pressure/memory",
    ] {
        assert_deterministic(path, |contents| {
            let text = std::str::from_utf8(contents).expect("pressure data should be UTF-8");
            assert!(text.lines().next().is_some());
            for line in text.lines() {
                let mut fields = line.split_whitespace();
                assert!(matches!(fields.next(), Some("some" | "full")));
                assert_eq!(fields.next(), Some("avg10=0.00"));
                assert_eq!(fields.next(), Some("avg60=0.00"));
                assert_eq!(fields.next(), Some("avg300=0.00"));
                assert_eq!(fields.next(), Some("total=0"));
                assert_eq!(fields.next(), None);
            }
        });
    }
}

#[test]
fn proc_schedstat_uses_virtual_zero_values() {
    assert_deterministic("/proc/schedstat", |contents| {
        let text = std::str::from_utf8(contents).expect("schedstat should be UTF-8");
        let mut saw_timestamp = false;
        let mut saw_cpu = false;
        let mut saw_domain = false;

        for line in text.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            match fields.first().copied() {
                Some("version") => {
                    assert_eq!(fields.len(), 2);
                    fields[1].parse::<u32>().expect("invalid schedstat version");
                }
                Some("timestamp") => {
                    assert_eq!(fields, ["timestamp", "0"]);
                    saw_timestamp = true;
                }
                Some(label) if is_numbered_label(label, "cpu") => {
                    assert!(fields[1..].iter().all(|field| *field == "0"));
                    saw_cpu = true;
                }
                Some(label) if is_numbered_label(label, "domain") => {
                    assert!(fields.len() >= 3);
                    assert!(fields[3..].iter().all(|field| *field == "0"));
                    saw_domain = true;
                }
                Some(label) => panic!("unexpected schedstat row {label}: {line}"),
                None => {}
            }
        }

        assert!(saw_timestamp);
        assert!(saw_cpu);
        assert!(saw_domain);
    });
}

fn is_numbered_label(label: &str, prefix: &str) -> bool {
    label.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[test]
fn proc_zoneinfo_uses_virtual_zero_values() {
    assert_deterministic("/proc/zoneinfo", |contents| {
        let text = std::str::from_utf8(contents).expect("zoneinfo should be UTF-8");
        let mut saw_node = false;
        let mut saw_cpu = false;
        let mut saw_accounting = false;

        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Node ") {
                assert!(trimmed.contains(", zone"));
                saw_node = true;
            } else if let Some(cpu) = trimmed.strip_prefix("cpu: ") {
                cpu.parse::<u32>().expect("invalid zoneinfo CPU label");
                saw_cpu = true;
            } else {
                assert!(
                    trimmed
                        .bytes()
                        .filter(u8::is_ascii_digit)
                        .all(|byte| byte == b'0'),
                    "zoneinfo retained a nonzero host quantity: {line}"
                );
                saw_accounting |= trimmed.starts_with("nr_inactive_anon ");
            }
        }

        assert!(saw_node);
        assert!(saw_cpu);
        assert!(saw_accounting);
    });
}

#[test]
fn proc_rtc_tracks_custom_epoch_and_virtual_time() {
    let _guard = hermit_run_lock();
    let epoch = "2000-12-31T23:59:59+00:00";
    let initial = read_procfs_at_epoch("/proc/driver/rtc", Some(epoch));
    let initial = std::str::from_utf8(&initial).expect("rtc should be UTF-8");
    assert!(initial.contains("rtc_time\t: 23:59:59\n"));
    assert!(initial.contains("rtc_date\t: 2000-12-31\n"));
    assert!(initial.contains("alarm_IRQ\t: no\n"));

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--epoch=2000-12-31T23:59:59+00:00",
        "--",
        "/usr/bin/python3",
        "-c",
        "import time; time.sleep(2); print(open('/proc/driver/rtc').read(), end='')",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        output.status.success(),
        "RTC virtual-time probe failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let advanced = String::from_utf8(output.stdout).expect("rtc should be UTF-8");
    let advanced_time = advanced
        .lines()
        .find_map(|line| line.strip_prefix("rtc_time\t: "))
        .expect("RTC output omitted rtc_time");
    assert_ne!(
        advanced_time, "23:59:59",
        "RTC did not advance with virtual time:\n{advanced}"
    );
    assert!(
        advanced.contains("rtc_date\t: 2001-01-01\n"),
        "RTC did not cross the configured epoch day:\n{advanced}"
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-873): Review mountinfo and UUID snapshots.
#[test]
fn proc_self_mountinfo_hides_private_temp_roots() {
    fn private_mount_records() -> Vec<String> {
        let contents = read_procfs("/proc/self/mountinfo");
        let text = std::str::from_utf8(&contents).expect("mountinfo should be UTF-8");
        assert!(!text.contains("/tmpvol/.tmp"));
        text.lines()
            .filter(|line| line.contains("/tmpvol/.hermit/"))
            .filter_map(|line| line.split_once(" /tmpvol/.hermit/"))
            .map(|(_, stable)| format!("/tmpvol/.hermit/{stable}"))
            .collect()
    }

    let _guard = hermit_run_lock();
    let first = private_mount_records();
    for run in 2..=RUNS {
        assert_eq!(
            first,
            private_mount_records(),
            "private mount records differed between run 1 and run {run}"
        );
    }
}

#[test]
fn proc_random_uuid_is_deterministic() {
    assert_deterministic("/proc/sys/kernel/random/uuid", |contents| {
        assert_eq!(contents, b"00000000-0000-4000-8000-000000000000\n");
    });
}
