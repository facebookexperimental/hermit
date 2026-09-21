/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;

use nix::mount::MsFlags;
use nix::mount::mount;
use nix::mount::umount;
use reverie::process::Command as ReverieCommand;
use reverie::process::Mount;
use reverie::process::Namespace;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const RUNS: usize = 5;

fn compile_c(source: &Path, output: &Path) {
    let rendered = format!("cc -O0 -g {} -o {}", source.display(), output.display());
    let result = Command::new("cc")
        .args(["-O0", "-g"])
        .arg(source)
        .arg("-o")
        .arg(output)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        result.status.success(),
        "guest compilation failed: {rendered}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn compile_freestanding_c(source: &Path, output: &Path) {
    let args = [
        "-O0",
        "-g",
        "-nostdlib",
        "-static",
        "-fno-stack-protector",
        "-fno-pie",
        "-no-pie",
    ];
    let rendered = format!(
        "cc {} {} -o {}",
        args.join(" "),
        source.display(),
        output.display()
    );
    let result = Command::new("cc")
        .args(args)
        .arg(source)
        .arg("-o")
        .arg(output)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        result.status.success(),
        "guest compilation failed: {rendered}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_procfs(path: &str) -> Vec<u8> {
    read_procfs_at_epoch(path, None)
}

fn read_procfs_at_epoch(path: &str, epoch: Option<&str>) -> Vec<u8> {
    read_procfs_with(path, epoch, |_| {})
}

fn read_procfs_with(
    path: &str,
    epoch: Option<&str>,
    configure: impl FnOnce(&mut Command),
) -> Vec<u8> {
    let mut command = Command::new(hermit_test::hermit_binary());
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
    configure(&mut command);
    command.args(["--", "/bin/cat", path]);
    hermit_test::configure_guest_execution(&mut command);
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

fn assert_runs_equal_while_host_mountinfo_stable<T: Eq + std::fmt::Debug>(
    mut run: impl FnMut() -> T,
    label: &str,
) -> T {
    for attempt in 1..=3 {
        let before_first = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        let first = run();
        let after_first = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        let before_second = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        let second = run();
        let after_second = fs::read("/proc/self/mountinfo").expect("read host mountinfo");
        if before_first == after_first
            && after_first == before_second
            && before_second == after_second
        {
            assert_eq!(
                first, second,
                "{label}: product output differed while the host mount table was stable"
            );
            return first;
        }
        let host_change = [
            ("before first", &before_first, "after first", &after_first),
            ("after first", &after_first, "before second", &before_second),
            (
                "before second",
                &before_second,
                "after second",
                &after_second,
            ),
        ]
        .into_iter()
        .find(|(_, left, _, right)| left != right)
        .map(|(left_label, left, right_label, right)| {
            format!(
                "{left_label} versus {right_label}: {}",
                first_mountinfo_row_difference(left, right)
            )
        });
        if attempt == 3 {
            panic!(
                "{label}: host /proc/self/mountinfo changed around all three independent-run pairs; last observed change: {}",
                host_change.as_deref().unwrap_or("unavailable")
            );
        }
    }
    unreachable!()
}

fn first_mountinfo_row_difference(left: &[u8], right: &[u8]) -> String {
    let mut left_rows = left.split(|byte| *byte == b'\n');
    let mut right_rows = right.split(|byte| *byte == b'\n');
    for row_index in 0.. {
        let left = left_rows.next();
        let right = right_rows.next();
        if left != right {
            return format!(
                "row {row_index}: {:?} -> {:?}",
                left.map(String::from_utf8_lossy),
                right.map(String::from_utf8_lossy)
            );
        }
    }
    unreachable!("different mountinfo byte strings must have a differing row")
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

fn first_hwmon_input() -> Option<PathBuf> {
    let mut hwmon_dirs = fs::read_dir("/sys/class/hwmon")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    hwmon_dirs.sort();
    for directory in hwmon_dirs {
        let mut inputs = fs::read_dir(directory)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_input"))
            })
            .collect::<Vec<_>>();
        inputs.sort();
        if let Some(input) = inputs.into_iter().next() {
            return Some(input);
        }
    }
    None
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
// TODO-HUMAN-REVIEW(PR-843): Review process and system accounting coverage.
#[test]
fn proc_system_cpu_accounting_is_deterministic() {
    assert_deterministic("/proc/stat", |contents| {
        let text = std::str::from_utf8(contents).expect("stat should be UTF-8");
        let cpu_lines = text
            .lines()
            .filter(|line| line.starts_with("cpu"))
            .collect::<Vec<_>>();
        let cpu_count = cpu_lines.len() - 1;
        for line in &cpu_lines {
            let mut fields = line.split_whitespace();
            let name = fields.next().expect("CPU line has no name");
            let counters = fields
                .map(|field| field.parse::<u64>().expect("CPU counter should be numeric"))
                .collect::<Vec<_>>();
            assert!(
                counters
                    .iter()
                    .enumerate()
                    .all(|(index, value)| index == 0 || *value == 0)
            );
            assert_eq!(
                counters[0],
                if name == "cpu" {
                    12_000 * cpu_count as u64
                } else {
                    12_000
                }
            );
        }
        assert!(text.contains("btime 1767225480\n"));
    });
}

#[test]
fn proc_vm_accounting_is_deterministic() {
    assert_deterministic("/proc/vmstat", |contents| {
        let text = std::str::from_utf8(contents).expect("vmstat should be UTF-8");
        assert!(
            text.lines()
                .all(|line| line.split_whitespace().nth(1) == Some("0"))
        );
    });
}

#[test]
fn proc_pid_stat_accounting_is_deterministic() {
    assert_deterministic("/proc/1/stat", |contents| {
        let text = std::str::from_utf8(contents).expect("process stat should be UTF-8");
        let comm_end = text.rfind(") ").expect("stat has no comm terminator");
        let fields = text[comm_end + 2..].split_whitespace().collect::<Vec<_>>();
        assert_eq!(fields[0], "S");
        assert_eq!(fields[23 - 3], "0");
        assert_eq!(fields[24 - 3], "0");
    });
}

#[test]
fn proc_pid_statm_accounting_is_deterministic() {
    assert_deterministic("/proc/1/statm", |contents| {
        assert_eq!(contents, b"0 0 0 0 0 0 0\n");
    });
}

#[test]
fn proc_pid_status_accounting_is_deterministic() {
    assert_deterministic("/proc/1/status", |contents| {
        let text = std::str::from_utf8(contents).expect("process status should be UTF-8");
        assert!(text.contains("VmSize:\t0 kB\n"));
        assert!(text.contains("VmRSS:\t0 kB\n"));
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
            frequencies
                .iter()
                .all(|line| *line == "cpu MHz\t\t: 1000.000"),
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-883): Review interrupt, softirq, and module snapshot coverage.
#[test]
fn proc_interrupt_accounting_is_deterministic() {
    for path in ["/proc/interrupts", "/proc/softirqs"] {
        assert_deterministic(path, |contents| {
            let text = std::str::from_utf8(contents).expect("interrupt table should be UTF-8");
            assert!(text.contains("CPU0"));
            for line in text.lines().filter(|line| line.contains(':')) {
                let (_, values) = line
                    .split_once(':')
                    .expect("interrupt row should have a label");
                for token in values.split_whitespace() {
                    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
                        break;
                    }
                    assert!(token.bytes().all(|byte| byte == b'0'));
                }
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

    let mut command = Command::new(hermit_test::hermit_binary());
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
    hermit_test::configure_guest_execution(&mut command);
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
fn proc_self_mountinfo_is_deterministic() {
    let _guard = hermit_run_lock();
    let host_tmpdir = tempfile::tempdir().expect("host TMPDIR");
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command.env("TMPDIR", host_tmpdir.path());
        })
    };
    let first = read();
    for run in 2..=RUNS {
        assert_eq!(first, read(), "mountinfo differed on run {run}");
    }
    {
        let contents = &first;
        let text = std::str::from_utf8(contents).expect("mountinfo should be UTF-8");
        assert!(!text.contains("/tmpvol/.tmp"));
        assert!(text.lines().all(|line| line.contains(" - ")));
        assert!(text.contains(" /tmpvol/.hermit/"));
    }
}

#[test]
fn proc_self_mountinfo_preserves_user_mount_with_tempfile_shape() {
    let _guard = hermit_run_lock();
    let mut user_group = tempfile::Builder::new()
        .prefix(".tmp")
        .rand_bytes(6)
        .tempfile_in("/tmp")
        .expect("create user-controlled tempfile-shaped group file");
    writeln!(user_group, "root:x:0:").expect("populate user group file");
    let contents = read_procfs_with("/proc/self/mountinfo", None, |command| {
        command.arg(format!(
            "--mount=type=bind,source={},target=/etc/group",
            user_group.path().display()
        ));
    });
    let text = std::str::from_utf8(&contents).expect("mountinfo should be UTF-8");
    let group_rows = text
        .lines()
        .filter(|line| line.split(' ').nth(4) == Some("/etc/group"))
        .collect::<Vec<_>>();
    assert!(!group_rows.is_empty(), "mountinfo must contain /etc/group");
    assert!(
        group_rows
            .iter()
            .all(|row| !row.contains("/tmpvol/.hermit/etc/group")),
        "a user-supplied mount must not be represented as Hermit-owned: {group_rows:?}"
    );
    assert!(
        group_rows
            .iter()
            .any(|row| row.contains(user_group.path().file_name().unwrap().to_str().unwrap())),
        "the user-supplied tempfile-shaped root must be preserved: {group_rows:?}"
    );
}

fn mountinfo_and_stat_proc_device(no_virtualize_metadata: bool, stat_first: bool) -> (u64, u64) {
    let script = if stat_first {
        "/usr/bin/stat -c '__STAT__ %d' /proc; cat /proc/self/mountinfo"
    } else {
        "cat /proc/self/mountinfo; /usr/bin/stat -c '__STAT__ %d' /proc"
    };
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    if no_virtualize_metadata {
        command.arg("--no-virtualize-metadata");
    }
    command.args(["--", "/bin/sh", "-c", script]);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        output.status.success(),
        "mountinfo/stat probe failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let text = std::str::from_utf8(&output.stdout).expect("probe output should be UTF-8");
    let stat_device = text
        .lines()
        .find_map(|line| line.strip_prefix("__STAT__ "))
        .expect("probe omitted stat device")
        .parse::<u64>()
        .expect("stat device should be decimal");
    let mount_devices = text
        .lines()
        .filter_map(|line| {
            let fields = line.split(' ').collect::<Vec<_>>();
            (fields.get(4) == Some(&"/proc")).then_some(fields)
        })
        .map(|fields| {
            let (major, minor) = fields[2]
                .split_once(':')
                .expect("mountinfo device should be major:minor");
            libc::makedev(
                major.parse().expect("mountinfo major should be decimal"),
                minor.parse().expect("mountinfo minor should be decimal"),
            )
        })
        .collect::<Vec<_>>();
    let mounted_device = *mount_devices
        .last()
        .expect("probe omitted the effective /proc mount row");
    // A mount namespace may retain covered lower rows. Linux reports them all
    // in stacking order; pathname lookup and stat observe the final/top row.
    (mounted_device, stat_device)
}

#[test]
fn mountinfo_device_agrees_with_stat_with_and_without_metadata_virtualization() {
    let _guard = hermit_run_lock();
    for no_virtualize_metadata in [false, true] {
        for stat_first in [false, true] {
            let (mountinfo_device, stat_device) =
                mountinfo_and_stat_proc_device(no_virtualize_metadata, stat_first);
            assert_eq!(
                mountinfo_device, stat_device,
                "mountinfo and stat disagreed when no_virtualize_metadata={no_virtualize_metadata}, \
                 stat_first={stat_first}"
            );
        }
    }
}

#[test]
fn proc_self_mountinfo_preserves_user_mount_over_private_tmp() {
    let _guard = hermit_run_lock();
    let user_tmp = tempfile::Builder::new()
        .prefix(".tmp")
        .rand_bytes(6)
        .tempdir_in("/tmp")
        .expect("create user-controlled tempfile-shaped tmp directory");
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command.arg(format!(
                "--mount=type=bind,source={},target=/tmp",
                user_tmp.path().display()
            ));
        })
    };
    let contents = assert_runs_equal_while_host_mountinfo_stable(
        read,
        "user mount at /tmp exposed Hermit's random staging mountpoint",
    );
    let text = std::str::from_utf8(&contents).expect("mountinfo should be UTF-8");
    let tmp_rows = text
        .lines()
        .filter(|line| line.split(' ').nth(4) == Some("/tmp"))
        .collect::<Vec<_>>();
    assert!(!tmp_rows.is_empty(), "mountinfo must expose /tmp");
    assert!(
        tmp_rows
            .iter()
            .all(|row| !row.contains("/tmpvol/.hermit/tmp")),
        "a user mount over /tmp must discard private-tmp provenance: {tmp_rows:?}"
    );
    let effective_tmp = tmp_rows
        .last()
        .expect("nonempty /tmp mount rows should have a top mount");
    assert!(
        effective_tmp.contains(user_tmp.path().file_name().unwrap().to_str().unwrap()),
        "the user-provided /tmp root must remain visible: {tmp_rows:?}"
    );
}

#[test]
fn proc_self_mountinfo_preserves_user_bind_over_private_tmp() {
    let _guard = hermit_run_lock();
    let user_tmp = tempfile::Builder::new()
        .prefix(".tmp")
        .rand_bytes(6)
        .tempdir_in("/tmp")
        .expect("create user-controlled bind source");
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command.arg(format!("--bind={}:/tmp", user_tmp.path().display()));
        })
    };
    let first =
        assert_runs_equal_while_host_mountinfo_stable(read, "exact /tmp bind changed across runs");
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    let effective_tmp = text
        .lines()
        .rfind(|line| line.split(' ').nth(4) == Some("/tmp"))
        .expect("mountinfo must expose /tmp");
    assert!(
        effective_tmp.contains(user_tmp.path().file_name().unwrap().to_str().unwrap()),
        "the exact /tmp bind root was not preserved: {effective_tmp}"
    );
    assert_eq!(
        effective_tmp.split(' ').nth(4),
        Some("/tmp"),
        "the exact /tmp bind exposed Hermit's random staging mountpoint"
    );
}

#[test]
fn ordered_nested_user_mounts_preserve_linux_stacking() {
    let _guard = hermit_run_lock();
    let parent_source = tempfile::tempdir().expect("create parent mount source");
    let child_source = tempfile::tempdir().expect("create child mount source");
    fs::create_dir(parent_source.path().join("child")).expect("create covered child path");
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command
                .arg(format!(
                    "--mount=type=bind,source={},target=/tmp/stack/child",
                    child_source.path().display()
                ))
                .arg(format!(
                    "--mount=type=bind,source={},target=/tmp/stack",
                    parent_source.path().display()
                ));
        })
    };
    let first = assert_runs_equal_while_host_mountinfo_stable(
        read,
        "ordered nested mounts changed across runs",
    );
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    for target in ["/tmp/stack/child", "/tmp/stack"] {
        assert!(
            text.lines()
                .any(|line| line.split(' ').nth(4) == Some(target)),
            "mount stacking dropped {target}:\n{text}"
        );
    }
}

#[test]
fn user_root_mount_keeps_the_later_private_tmp_provenance() {
    let _guard = hermit_run_lock();
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command.arg("--mount=type=bind,source=/,target=/");
        })
    };
    let first = assert_runs_equal_while_host_mountinfo_stable(
        read,
        "user root mount discarded the later private /tmp provenance",
    );
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    assert!(
        text.lines().any(|line| {
            line.split(' ').nth(4) == Some("/tmp") && line.contains("/tmpvol/.hermit/tmp")
        }),
        "the active private /tmp row was not canonicalized after a root bind:\n{text}"
    );
}

#[test]
fn user_var_mount_does_not_shadow_the_run_nscd_alias() {
    let _guard = hermit_run_lock();
    let user_var = tempfile::tempdir().expect("create user /var source");
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command.arg(format!(
                "--mount=type=bind,source={},target=/var",
                user_var.path().display()
            ));
        })
    };
    let first =
        assert_runs_equal_while_host_mountinfo_stable(read, "user /var mount changed across runs");
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    assert!(
        text.lines()
            .any(|line| line.split(' ').nth(4) == Some("/var")),
        "the user /var mount is absent:\n{text}"
    );
    if PathBuf::from("/var/run/nscd").is_dir()
        && fs::canonicalize("/var/run").ok() == fs::canonicalize("/run").ok()
    {
        assert!(
            text.lines().any(|line| {
                line.split(' ').nth(4) == Some("/run/nscd")
                    && line.contains("/tmpvol/.hermit/run/nscd")
            }),
            "a user /var mount incorrectly discarded the active /run/nscd hardening mount:\n{text}"
        );
    }
}

#[test]
fn ordered_var_then_nscd_mount_keeps_the_run_nscd_hardening_mount() {
    let _guard = hermit_run_lock();
    if !PathBuf::from("/var/run/nscd").is_dir()
        || fs::canonicalize("/var/run").ok() != fs::canonicalize("/run").ok()
    {
        return;
    }

    let user_var = tempfile::tempdir().expect("create user /var source");
    fs::create_dir_all(user_var.path().join("run/nscd")).expect("create user /var nscd path");
    fs::write(user_var.path().join("run/nscd/from-var"), b"from-var\n").expect("write /var marker");
    let later_nscd = tempfile::tempdir().expect("create later nscd source");
    fs::write(later_nscd.path().join("from-later"), b"from-later\n").expect("write later marker");
    let build = tempfile::tempdir().expect("create guest build directory");
    let guest = build.path().join("mount-nscd-order");
    compile_c(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli must be inside the repository")
            .join("tests/c/mount_nscd_order.c"),
        &guest,
    );

    let run = || {
        let mut command = Command::new(hermit_test::hermit_binary());
        command
            .args([
                "--log=error",
                "run",
                "--base-env=minimal",
                "--no-virtualize-cpuid",
                "--max-timeslice=disabled",
                "--tmp=/tmp",
            ])
            .arg(format!(
                "--mount=type=bind,source={},target=/var",
                user_var.path().display()
            ))
            .arg(format!(
                "--mount=type=bind,source={},target=/var/run/nscd",
                later_nscd.path().display()
            ))
            .arg("--")
            .arg(&guest);
        hermit_test::configure_guest_execution(&mut command);
        let rendered = format!("{command:?}");
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
        assert!(
            output.status.success(),
            "ordered mount run failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let first = assert_runs_equal_while_host_mountinfo_stable(
        run,
        "ordered /var then /var/run/nscd mounts changed across runs",
    );
    let text = std::str::from_utf8(&first).expect("guest output should be UTF-8");
    assert!(
        text.starts_with("from-later\n"),
        "later user mount was absent: {text}"
    );
    assert!(
        text.lines().any(|line| {
            line.split(' ').nth(4) == Some("/run/nscd") && line.contains("/tmpvol/.hermit/run/nscd")
        }),
        "ordered user mounts incorrectly removed the /run/nscd hardening mount:\n{text}"
    );
}

#[test]
fn user_run_mount_shadows_the_run_nscd_identity_mount() {
    let _guard = hermit_run_lock();
    let user_run = tempfile::tempdir().expect("create user /run source");
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command.arg(format!(
                "--mount=type=bind,source={},target=/run",
                user_run.path().display()
            ));
        })
    };
    let first =
        assert_runs_equal_while_host_mountinfo_stable(read, "user /run mount changed across runs");
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    assert!(
        text.lines()
            .any(|line| line.split(' ').nth(4) == Some("/run")),
        "the user /run mount is absent:\n{text}"
    );
    assert!(
        !text.lines().any(|line| {
            line.split(' ').nth(4) == Some("/run/nscd") && line.contains("/tmpvol/.hermit/run/nscd")
        }),
        "the identity-hardening nscd mount survived a later /run mount:\n{text}"
    );
}

fn assert_mountinfo_target_under_private_tmp_is_stable(
    option: impl Fn(&mut Command) + Copy,
    target: &str,
) {
    let read = || read_procfs_with("/proc/self/mountinfo", None, option);
    let first = assert_runs_equal_while_host_mountinfo_stable(
        read,
        &format!("mountinfo changed across runs for user target {target}"),
    );
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    let rows = text
        .lines()
        .filter(|line| line.split(' ').nth(4) == Some(target))
        .collect::<Vec<_>>();
    assert!(
        !rows.is_empty(),
        "missing canonical mountpoint row for {target}: {text}"
    );
    assert!(
        rows.iter()
            .all(|row| !row.split(' ').nth(3).unwrap().contains("/.tmp")),
        "a mount under the proven private /tmp retained a random backing root: {rows:?}"
    );
}

#[test]
fn user_mount_under_private_tmp_has_a_stable_guest_mountpoint() {
    let _guard = hermit_run_lock();
    let option = |command: &mut Command| {
        command.arg("--mount=type=bind,source=/etc/hostname,target=/tmp/user-mount");
    };
    assert_mountinfo_target_under_private_tmp_is_stable(option, "/tmp/user-mount");
}

#[test]
fn user_bind_under_private_tmp_has_a_stable_guest_mountpoint() {
    let _guard = hermit_run_lock();
    let option = |command: &mut Command| {
        command.arg("--bind=/etc/hostname:/tmp/user-bind");
    };
    let first = assert_runs_equal_while_host_mountinfo_stable(
        || read_procfs_with("/proc/self/mountinfo", None, option),
        "bind mountinfo changed across runs",
    );
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    let rows = text
        .lines()
        .filter(|line| line.split(' ').nth(4) == Some("/tmp/user-bind"))
        .collect::<Vec<_>>();
    assert!(!rows.is_empty(), "missing canonical bind row: {text}");
    assert!(
        rows.iter()
            .all(|row| !row.split(' ').nth(3).unwrap().contains("/.tmp")),
        "a bind under the proven private /tmp retained a random backing root: {rows:?}"
    );
}

#[test]
fn ignored_bind_outside_tmp_does_not_discard_private_mount_provenance() {
    let _guard = hermit_run_lock();
    let ignored_source = tempfile::NamedTempFile::new().expect("ignored bind source");
    let read = || {
        read_procfs_with("/proc/self/mountinfo", None, |command| {
            command.arg(format!(
                "--bind={}:{}",
                ignored_source.path().display(),
                "/etc/group"
            ));
        })
    };
    let first = assert_runs_equal_while_host_mountinfo_stable(
        read,
        "ignored outside-/tmp bind destabilized private provenance",
    );
    let text = std::str::from_utf8(&first).expect("mountinfo should be UTF-8");
    assert!(
        text.lines().any(|line| {
            line.split(' ').nth(4) == Some("/etc/group")
                && line.contains("/tmpvol/.hermit/etc/group")
        }),
        "ignored bind incorrectly removed the active private /etc/group provenance"
    );
}

fn fdinfo_and_mountinfo_ids() -> (u64, u64, u64, u64) {
    let script = "exec 3</; exec 4</proc; \
                  printf '__ROOT_FD__ '; sed -n 's/^mnt_id:[[:space:]]*//p' /proc/self/fdinfo/3; \
                  printf '__PROC_FD__ '; sed -n 's/^mnt_id:[[:space:]]*//p' /proc/self/fdinfo/4; \
                  cat /proc/self/mountinfo";
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--",
        "/bin/sh",
        "-c",
        script,
    ]);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        output.status.success(),
        "fdinfo/mountinfo probe failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let text = std::str::from_utf8(&output.stdout).expect("probe output should be UTF-8");
    let tagged = |prefix: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("missing {prefix} in:\n{text}"))
            .parse::<u64>()
            .unwrap_or_else(|error| panic!("invalid {prefix}: {error}"))
    };
    let top_mount = |target: &str| {
        text.lines()
            .filter_map(|line| {
                let fields = line.split(' ').collect::<Vec<_>>();
                (fields.get(4) == Some(&target)).then(|| {
                    fields[0]
                        .parse::<u64>()
                        .expect("mountinfo ID should be decimal")
                })
            })
            .next_back()
            .unwrap_or_else(|| panic!("missing mountinfo target {target} in:\n{text}"))
    };
    (
        tagged("__ROOT_FD__ "),
        tagged("__PROC_FD__ "),
        top_mount("/"),
        top_mount("/proc"),
    )
}

#[test]
fn fdinfo_mount_ids_match_mountinfo_without_aliasing() {
    let _guard = hermit_run_lock();
    let first = assert_runs_equal_while_host_mountinfo_stable(
        fdinfo_and_mountinfo_ids,
        "fdinfo/mountinfo identities changed across runs",
    );
    assert_eq!(first.0, first.2, "root fdinfo disagreed with mountinfo");
    assert_eq!(first.1, first.3, "proc fdinfo disagreed with mountinfo");
    assert_ne!(first.0, first.1, "distinct mounts collapsed to one mnt_id");
}

#[test]
fn chroot_mountinfo_subset_keeps_fdinfo_identity_consistent() {
    const INNER: &str = "HERMIT_CHROOT_MOUNTINFO_SUBSET_INNER";
    if std::env::var_os(INNER).is_none() {
        let mut command = ReverieCommand::new(std::env::current_exe().expect("find test binary"));
        command
            .args([
                "--exact",
                "chroot_mountinfo_subset_keeps_fdinfo_identity_consistent",
                "--nocapture",
            ])
            .env(INNER, "1")
            .map_root()
            .unshare(Namespace::MOUNT | Namespace::PID)
            .mount(Mount::proc());
        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build chroot namespace test runtime")
            .block_on(command.output())
            .expect("launch chroot namespace test");
        assert_eq!(
            output.status,
            reverie::process::ExitStatus::Exited(0),
            "chroot mountinfo test failed in its private namespace:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _guard = hermit_run_lock();
    let root = tempfile::tempdir().expect("create chroot");
    let build = tempfile::tempdir().expect("create guest build directory");
    let controller_program = build.path().join("chroot-mountinfo-fdinfo");
    compile_freestanding_c(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli must be inside the repository")
            .join("tests/c/chroot_mountinfo_fdinfo.c"),
        &controller_program,
    );

    // TracerBuilder validates the host pathname before entering the chroot,
    // while exec resolves that same absolute pathname inside the chroot.
    let relative_program = controller_program
        .strip_prefix("/")
        .expect("temporary guest path should be absolute");
    let chroot_program = root.path().join(relative_program);
    fs::create_dir_all(chroot_program.parent().expect("guest must have a parent"))
        .expect("create chroot guest parent");
    fs::copy(&controller_program, &chroot_program).expect("copy guest into chroot");

    let proc_target = root.path().join("proc");
    fs::create_dir(&proc_target).expect("create chroot proc mountpoint");
    mount(
        Some("proc"),
        &proc_target,
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .expect("mount procfs inside chroot");

    let captured_mount_ids =
        hermit::capture_mountinfo_identity_order().expect("capture producer mount identity order");
    let raw_proc_mount_id = fs::read_to_string("/proc/self/mountinfo")
        .expect("read producer mountinfo")
        .lines()
        .find_map(|row| {
            let mut fields = row.split(' ');
            let raw_mount_id = fields.next()?.parse::<u64>().ok()?;
            let _parent = fields.next()?;
            let _device = fields.next()?;
            let _root = fields.next()?;
            (fields.next()? == proc_target.to_str()?).then_some(raw_mount_id)
        })
        .expect("find chroot proc mount in producer mountinfo");
    let expected_proc_mount_id = captured_mount_ids
        .iter()
        .position(|raw| *raw == raw_proc_mount_id)
        .map(|index| index as u64 + 1)
        .expect("proc mount must be present in captured identity order");

    let config = hermit::DetConfig {
        mountinfo_mount_ids: captured_mount_ids,
        mountinfo_mount_ids_captured: true,
        ..Default::default()
    };
    let mut command = ReverieCommand::new(&controller_program);
    command.chroot(root.path()).current_dir("/");
    let output = hermit::run_with_output(command, config, false, &None);
    umount(&proc_target).expect("unmount chroot procfs");
    let output = output.expect("run chroot guest");
    assert_eq!(
        output.status,
        reverie::process::ExitStatus::Exited(0),
        "chroot guest failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let text = std::str::from_utf8(&output.stdout).expect("procfs output should be UTF-8");
    let (mountinfo, fdinfo) = text
        .strip_prefix("__MOUNTINFO__\n")
        .and_then(|text| text.split_once("__FDINFO__\n"))
        .expect("guest output must separate mountinfo and fdinfo");
    let visible_proc_mount_id = mountinfo
        .lines()
        .find_map(|row| {
            let fields = row.split(' ').collect::<Vec<_>>();
            (fields.get(4) == Some(&"/proc")).then(|| {
                fields[0]
                    .parse::<u64>()
                    .expect("mountinfo ID should be decimal")
            })
        })
        .expect("chroot mountinfo must expose /proc");
    let fdinfo_mount_id = detcore_model::procfs::parse_fdinfo_mount_id(fdinfo.as_bytes())
        .expect("fdinfo must contain one numeric mnt_id");
    assert_eq!(visible_proc_mount_id, expected_proc_mount_id);
    assert_eq!(fdinfo_mount_id, expected_proc_mount_id);
    assert!(
        !mountinfo.contains(root.path().to_str().expect("UTF-8 chroot path")),
        "chroot mountinfo leaked the host-side chroot path:\n{mountinfo}"
    );
}

fn redirected_regular_stdio_fdinfo() -> Vec<u8> {
    let mut input = tempfile::NamedTempFile::new().expect("redirected stdin file");
    writeln!(input, "unused input").expect("populate redirected stdin");
    let output = tempfile::NamedTempFile::new().expect("redirected stdout file");

    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--",
        "/bin/cat",
        "/proc/self/fdinfo/0",
        "/proc/self/fdinfo/1",
    ]);
    hermit_test::configure_guest_execution(&mut command);
    command
        .stdin(Stdio::from(
            input.reopen().expect("reopen redirected stdin"),
        ))
        .stdout(Stdio::from(
            output.reopen().expect("reopen redirected stdout"),
        ));
    let rendered = format!("{command:?}");
    let result = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        result.status.success(),
        "redirected stdio fdinfo failed: {rendered}\nstatus: {}\nstderr:\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr),
    );
    let contents = fs::read(output.path()).expect("read redirected fdinfo output");
    assert_eq!(
        contents
            .split(|byte| *byte == b'\n')
            .filter(|line| line.starts_with(b"mnt_id:"))
            .count(),
        2,
        "both redirected stdin and stdout must retain fdinfo mount identities"
    );
    contents
}

fn compile_fdinfo_mount_classes_guest() -> PathBuf {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
        .to_path_buf();
    let output = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("proc-fdinfo-mount-classes");
    let compile = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/proc_fdinfo_mount_classes.c"))
        .arg("-o")
        .arg(&output)
        .output()
        .expect("compile fdinfo mount-class guest");
    assert!(
        compile.status.success(),
        "failed to compile fdinfo mount-class guest:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    output
}

fn fdinfo_mount_classes_with_stdio(guest: &PathBuf, regular_stdin_and_stderr: bool) -> Vec<u8> {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--tmp=/tmp",
        "--",
    ]);
    command.arg(guest);
    hermit_test::configure_guest_execution(&mut command);
    if regular_stdin_and_stderr {
        let stdin = tempfile::NamedTempFile::new().expect("create regular stdin");
        let stderr_file = tempfile::NamedTempFile::new().expect("create regular stderr");
        command.stdin(Stdio::from(stdin.reopen().expect("reopen regular stdin")));
        command.stderr(Stdio::from(
            stderr_file.reopen().expect("reopen regular stderr"),
        ));
    }
    let output = command.output().expect("run fdinfo mount-class guest");
    assert!(
        output.status.success(),
        "fdinfo mount-class guest failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn guest_pipe_socket_and_anon_fdinfo_ignore_hermit_stdio_shape() {
    let _guard = hermit_run_lock();
    let guest = compile_fdinfo_mount_classes_guest();
    let pair = assert_runs_equal_while_host_mountinfo_stable(
        || {
            (
                fdinfo_mount_classes_with_stdio(&guest, false),
                fdinfo_mount_classes_with_stdio(&guest, true),
            )
        },
        "fdinfo class outputs changed across repeated stdio-shape pairs",
    );
    let (piped, regular) = pair;
    assert_eq!(
        piped, regular,
        "guest fdinfo changed with Hermit stdio shape"
    );
    let text = std::str::from_utf8(&piped).expect("fdinfo output should be UTF-8");
    for label in ["[pipe]", "[socket]", "[eventfd]", "[mount-namespace]"] {
        assert!(text.contains(label), "missing {label} in:\n{text}");
    }
    // SAFETY: pidfd_open has no pointer arguments. On a kernel that supports
    // it, the returned descriptor is owned here and closed immediately.
    let host_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
    if host_pidfd >= 0 {
        // SAFETY: host_pidfd is a live descriptor returned by pidfd_open above.
        unsafe { libc::close(host_pidfd as i32) };
        assert!(
            text.contains("[pidfd]"),
            "the host supports pidfd_open but the guest omitted pidfs coverage:\n{text}"
        );
    }
    let mount_ids = text
        .lines()
        .filter_map(|line| line.strip_prefix("mnt_id:"))
        .map(|value| value.trim().parse::<u64>().expect("decimal mnt_id"))
        .collect::<Vec<_>>();
    let expected = if text.contains("[pidfd]") { 5 } else { 4 };
    assert_eq!(
        mount_ids.len(),
        expected,
        "each descriptor must retain one mnt_id field:\n{text}"
    );
    assert_eq!(
        mount_ids.iter().copied().collect::<BTreeSet<_>>().len(),
        expected,
        "pipefs, sockfs, anon_inodefs, nsfs, and pidfs must remain distinct:\n{text}"
    );
    assert!(
        text.contains("flags:"),
        "fdinfo flags were dropped:\n{text}"
    );
    assert!(
        text.contains("scm_fds:"),
        "socket fdinfo fields were dropped:\n{text}"
    );
    assert!(
        text.contains("eventfd-count:"),
        "eventfd fields were dropped:\n{text}"
    );
}

fn mount_namespace_fdinfo(no_namespace: bool) -> Vec<u8> {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    if no_namespace {
        command.arg("--no-namespace");
    }
    command.args([
        "--",
        "/bin/sh",
        "-c",
        "exec 3</proc/self/ns/mnt; cat /proc/self/fdinfo/3",
    ]);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
    assert!(
        output.status.success(),
        "mount namespace fdinfo failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        detcore_model::procfs::parse_fdinfo_mount_id(&output.stdout).is_some(),
        "mount namespace fdinfo omitted a valid mnt_id: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    output.stdout
}

#[test]
fn mount_namespace_fdinfo_is_stable_with_and_without_namespace_setup() {
    let _guard = hermit_run_lock();
    for no_namespace in [false, true] {
        assert_runs_equal_while_host_mountinfo_stable(
            || mount_namespace_fdinfo(no_namespace),
            &format!("nsfs fdinfo changed across runs when no_namespace={no_namespace}"),
        );
    }
}

#[test]
fn no_namespace_mountinfo_and_fdinfo_share_one_mount_identity_map() {
    let _guard = hermit_run_lock();
    let run = || {
        let mut command = Command::new(hermit_test::hermit_binary());
        command.args([
            "--log=error",
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--no-namespace",
            "--",
            "/bin/sh",
            "-c",
            "exec 3</bin/sh; cat /proc/self/mountinfo; printf '__FDINFO__\\n'; cat /proc/self/fdinfo/3",
        ]);
        hermit_test::configure_guest_execution(&mut command);
        let rendered = format!("{command:?}");
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"));
        assert!(
            output.status.success(),
            "no-namespace mountinfo/fdinfo read failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output.stdout
    };
    let output = assert_runs_equal_while_host_mountinfo_stable(
        run,
        "no-namespace mountinfo/fdinfo identities changed across runs",
    );
    let text = std::str::from_utf8(&output).expect("procfs output should be UTF-8");
    let (mountinfo, fdinfo) = text
        .split_once("__FDINFO__\n")
        .expect("guest output must separate mountinfo and fdinfo");
    let fdinfo_mount_id = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .expect("namespace fdinfo must contain mnt_id");
    assert!(
        mountinfo.lines().any(|line| {
            line.split_once(' ')
                .is_some_and(|(mount_id, _)| mount_id == fdinfo_mount_id)
        }),
        "no-namespace fdinfo mnt_id {fdinfo_mount_id} is absent from mountinfo:\n{text}"
    );
}

#[test]
fn redirected_regular_stdin_and_stdout_fdinfo_are_stable() {
    let _guard = hermit_run_lock();
    assert_runs_equal_while_host_mountinfo_stable(
        redirected_regular_stdio_fdinfo,
        "redirected regular stdio fdinfo changed across runs",
    );
}

#[test]
fn proc_random_uuid_is_deterministic() {
    assert_deterministic("/proc/sys/kernel/random/uuid", |contents| {
        let uuid = contents
            .strip_suffix(b"\n")
            .expect("random UUID should end with a newline");
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid[14], b'4');
        assert!(matches!(uuid[19], b'8' | b'9' | b'a' | b'b'));
        for (index, byte) in uuid.iter().copied().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                assert_eq!(byte, b'-');
            } else {
                assert!(byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
            }
        }
    });
}

#[test]
fn proc_modules_are_deterministic() {
    assert_deterministic("/proc/modules", |contents| {
        let text = std::str::from_utf8(contents).expect("modules should be UTF-8");
        for line in text.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            assert!(fields.len() >= 4, "malformed module row: {line}");
            let expected = if fields[3] == "-" {
                0
            } else {
                fields[3]
                    .split(',')
                    .filter(|holder| !holder.is_empty())
                    .count()
            };
            assert_eq!(fields[2].parse::<usize>().unwrap(), expected);
        }
    });
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-865): Review NUMA and hwmon snapshot coverage.
#[test]
fn sysfs_numa_accounting_is_deterministic() {
    assert_deterministic("/sys/devices/system/node/node0/numastat", |contents| {
        let text = std::str::from_utf8(contents).expect("numastat should be UTF-8");
        assert!(text.lines().all(|line| line.ends_with(" 0")));
    });
    assert_deterministic("/sys/devices/system/node/node0/meminfo", |contents| {
        let text = std::str::from_utf8(contents).expect("node meminfo should be UTF-8");
        assert!(text.contains("MemTotal: 1048576 kB\n"));
        assert!(text.contains("MemFree: 1048576 kB\n"));
    });
}

#[test]
fn sysfs_hwmon_input_is_deterministic_when_available() {
    let Some(path) = first_hwmon_input() else {
        return;
    };
    let path = path.to_str().expect("hwmon path should be UTF-8");
    assert_deterministic(path, |contents| assert_eq!(contents, b"0\n"));
}
