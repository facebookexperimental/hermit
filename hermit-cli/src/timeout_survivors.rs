/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Which guest processes were still running when `--timeout` fired, for the
//! timeout report.
//!
//! A run lasts until every guest process has exited, not only the first one.
//! A guest whose root process crashed or exited while a child it never reaped
//! sat blocked (in a binder read, a FIFO open, a wait on a peer outside the
//! container) therefore runs to the bound, and the report used to name only
//! the bound. This names the processes that were left: their ID, command name,
//! parent, and the system call each was in.
//!
//! The processes are found by tracing relation rather than by ancestry: an
//! orphan is re-parented (to the host's init or a subreaper, or to Hermit
//! itself when Hermit is the container's init), but its tracer is still this
//! process. Host-side and read once, after the bound fired, so nothing here
//! can reach the guest. A process shown "waiting in ptrace_stop" or "stopped
//! outside a system call" is usually held by Hermit itself, at a stop Hermit
//! has not resumed, rather than by the kernel.

use std::collections::BTreeSet;
use std::fs;

/// The most processes one report names.
const LISTED: usize = 8;

/// One traced guest process, as `/proc` showed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Survivor {
    pub pid: i32,
    pub comm: String,
    pub ppid: i32,
    /// The first field of `/proc/<pid>/syscall`: a system call number,
    /// `running`, or `-1` (stopped, but not in a system call).
    pub syscall: String,
    /// `/proc/<pid>/wchan`, or empty when the kernel shows none.
    pub wchan: String,
}

/// The guest processes this process traces now, described in one line for a
/// timeout report, or `None` when there are none or `/proc` cannot be read.
/// `root` is the run's root guest process, when known.
pub(crate) fn traced_processes_now(root: Option<i32>) -> Option<String> {
    let tracers = task_ids("/proc/self/task");
    if tracers.is_empty() {
        return None;
    }
    let mut survivors: Vec<Survivor> = task_ids("/proc")
        .into_iter()
        .filter_map(|pid| read_survivor(pid, &tracers))
        .collect();
    survivors.sort_by_key(|survivor| survivor.pid);
    describe(&survivors, std::process::id() as i32, root)
}

fn task_ids(dir: &str) -> BTreeSet<i32> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .collect()
}

/// `status` field `key` (`TracerPid`, `PPid`) as a number.
fn status_field(status: &str, key: &str) -> Option<i32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|value| value.trim().parse().ok())
}

fn read_survivor(pid: i32, tracers: &BTreeSet<i32>) -> Option<Survivor> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let tracer = status_field(&status, "TracerPid")?;
    if !tracers.contains(&tracer) {
        return None;
    }
    let read = |name: &str| {
        fs::read_to_string(format!("/proc/{pid}/{name}"))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    Some(Survivor {
        pid,
        comm: read("comm"),
        ppid: status_field(&status, "PPid").unwrap_or(0),
        syscall: read("syscall")
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
        wchan: read("wchan"),
    })
}

/// What a survivor is doing, from its `syscall` and `wchan`.
fn activity(survivor: &Survivor) -> String {
    let call = match survivor.syscall.as_str() {
        "" => "in an unknown state".to_owned(),
        "running" => "running".to_owned(),
        "-1" => "stopped outside a system call".to_owned(),
        number => match number.parse::<i32>() {
            Ok(number) => format!("in {}", reverie::syscalls::Sysno::from(number)),
            Err(_) => format!("in system call {number}"),
        },
    };
    match survivor.wchan.as_str() {
        "" | "0" => call,
        wchan => format!("{call} (waiting in {wchan})"),
    }
}

/// The report line for `survivors`. A survivor was orphaned (its parent exited,
/// which is how a run outlives its root) when its parent is neither another
/// survivor nor `this_process`, or when its parent is `this_process` but it is
/// not the run's `root`: Hermit started only the root, so any other child of
/// Hermit was re-parented to it.
/// The name the report prints for a survivor. Linux names an exec'd task by
/// the basename of its image (`begin_new_exec` stores `kbasename(filename)`
/// in `comm`), so a `comm` carrying a directory prefix came from the
/// executor's naming of the host process, not from the guest's own name.
/// Print the basename, as the kernel's own `comm` would read for that exec.
fn display_comm(comm: &str) -> &str {
    comm.rsplit('/').next().unwrap_or(comm)
}

pub(crate) fn describe(
    survivors: &[Survivor],
    this_process: i32,
    root: Option<i32>,
) -> Option<String> {
    if survivors.is_empty() {
        return None;
    }
    let guests: BTreeSet<i32> = survivors.iter().map(|survivor| survivor.pid).collect();
    let mut parts: Vec<String> = survivors
        .iter()
        .take(LISTED)
        .map(|survivor| {
            let orphaned = if survivor.ppid == this_process {
                root.is_some_and(|root| root != survivor.pid)
            } else {
                !guests.contains(&survivor.ppid)
            };
            format!(
                "pid {} \"{}\" (parent {}{}) {}",
                survivor.pid,
                display_comm(&survivor.comm),
                survivor.ppid,
                if orphaned {
                    ", orphaned: its parent exited"
                } else {
                    ""
                },
                activity(survivor)
            )
        })
        .collect();
    if survivors.len() > LISTED {
        parts.push(format!("and {} more", survivors.len() - LISTED));
    }
    Some(format!(
        "{} guest process{} still running: {}",
        survivors.len(),
        if survivors.len() == 1 {
            " was"
        } else {
            "es were"
        },
        parts.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn survivor(pid: i32, ppid: i32, syscall: &str, wchan: &str) -> Survivor {
        Survivor {
            pid,
            comm: "guest".to_owned(),
            ppid,
            syscall: syscall.to_owned(),
            wchan: wchan.to_owned(),
        }
    }

    /// An orphan blocked in a system call is named with the call and where the
    /// kernel holds it; a child of another survivor, or of this process, is not
    /// called orphaned.
    #[test]
    fn a_timeout_report_names_orphans_and_the_call_each_is_in() {
        let ioctl = libc::SYS_ioctl.to_string();
        let line = describe(
            &[
                survivor(20485, 1, &ioctl, "binder_thread_read"),
                survivor(20490, 20485, "running", "0"),
                survivor(20491, 77, "-1", ""),
            ],
            77,
            Some(20491),
        )
        .expect("three survivors");
        assert_eq!(
            line,
            "3 guest processes were still running: \
             pid 20485 \"guest\" (parent 1, orphaned: its parent exited) in \
             ioctl (waiting in binder_thread_read); \
             pid 20490 \"guest\" (parent 20485) running; \
             pid 20491 \"guest\" (parent 77) stopped outside a system call"
        );
        assert_eq!(describe(&[], 77, None), None);
    }

    /// An executor may name the host process for an exec'd guest by its full
    /// image path; the report still names the guest by its comm, which Linux
    /// stores as the image's basename.
    #[test]
    fn a_timeout_report_names_an_execd_guest_by_its_basename() {
        let mut named = survivor(9, 1, "-1", "");
        named.comm = "/bin/cat".to_owned();
        let line = describe(&[named], 77, None).expect("one survivor");
        assert!(line.contains("pid 9 \"cat\""), "{line}");
    }

    /// Where Hermit is the container's init, an orphan is re-parented to Hermit
    /// itself: a child of Hermit that is not the root is still called orphaned.
    #[test]
    fn a_child_of_hermit_that_is_not_the_root_is_orphaned() {
        let line = describe(
            &[survivor(3, 1, "-1", ""), survivor(7, 1, "-1", "")],
            1,
            Some(3),
        )
        .expect("two survivors");
        assert!(
            line.contains("pid 3 \"guest\" (parent 1) stopped"),
            "{line}"
        );
        assert!(
            line.contains("pid 7 \"guest\" (parent 1, orphaned: its parent exited)"),
            "{line}"
        );
    }

    #[test]
    fn a_timeout_report_names_at_most_eight_processes() {
        let survivors: Vec<_> = (1..=10).map(|pid| survivor(pid, 0, "", "")).collect();
        let line = describe(&survivors, 99, None).expect("ten survivors");
        assert!(
            line.starts_with("10 guest processes were still running: "),
            "{line}"
        );
        assert!(line.ends_with("; and 2 more"), "{line}");
        assert_eq!(line.matches("pid ").count(), 8, "{line}");
    }

    #[test]
    fn status_fields_parse_as_proc_prints_them() {
        let status = "Name:\tcat\nPPid:\t1\nTracerPid:\t4242\n";
        assert_eq!(status_field(status, "TracerPid"), Some(4242));
        assert_eq!(status_field(status, "PPid"), Some(1));
        assert_eq!(status_field(status, "Uid"), None);
    }
}
