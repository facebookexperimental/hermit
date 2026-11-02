// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Where a wedged run waited: a test that kills a run at a deadline can
//! first record the kernel's view of the run's process tree.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// The kernel's view of process `root` and every process below it: each
/// thread's name, state, wait channel and current syscall number, from
/// `/proc`. A run that hangs without using CPU shows here which of
/// Hermit's threads and which guest threads were waiting, and in what.
/// Descendants are found by each process's parent in `/proc/PID/stat`,
/// because kernels without CONFIG_PROC_CHILDREN have no `children` file.
/// Whatever `/proc` refuses to show is left blank.
pub(crate) fn process_tree_snapshot(root: u32) -> String {
    let read = |path: &Path| fs::read_to_string(path).unwrap_or_default();
    // The fields after the parenthesized command name, which may itself
    // contain spaces or parentheses: the state, then the parent's PID.
    let after_name = |stat: &str| {
        stat.rsplit_once(") ")
            .map(|(_, rest)| rest.split(' ').map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let parents: Vec<(u32, u32)> = fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
                .filter_map(|pid| {
                    let fields = after_name(&read(&PathBuf::from(format!("/proc/{pid}/stat"))));
                    Some((pid, fields.get(1)?.parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut snapshot = String::new();
    let mut pending = vec![root];
    // The parents come from one scan that is not atomic, so a reused PID
    // could close a loop; visit each PID once.
    let mut visited = std::collections::HashSet::new();
    while let Some(pid) = pending.pop() {
        if !visited.insert(pid) {
            continue;
        }
        let process = PathBuf::from(format!("/proc/{pid}"));
        snapshot.push_str(&format!(
            "pid {pid} {}\n",
            read(&process.join("cmdline")).replace('\0', " ").trim_end()
        ));
        if let Ok(tasks) = fs::read_dir(process.join("task")) {
            let mut tasks: Vec<_> = tasks.flatten().map(|task| task.path()).collect();
            tasks.sort();
            for task in tasks {
                let fields = after_name(&read(&task.join("stat")));
                snapshot.push_str(&format!(
                    "  tid {} {} state={} wchan={} syscall={}\n",
                    task.file_name().unwrap_or_default().to_string_lossy(),
                    read(&task.join("comm")).trim(),
                    fields.first().map_or("?", String::as_str),
                    read(&task.join("wchan")).trim(),
                    read(&task.join("syscall"))
                        .split(' ')
                        .next()
                        .unwrap_or("")
                        .trim(),
                ));
            }
        }
        pending.extend(
            parents
                .iter()
                .filter(|(_, parent)| *parent == pid)
                .map(|(child, _)| *child),
        );
    }
    snapshot
}
