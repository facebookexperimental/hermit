/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Test that --stacktrace-event actually prints the right event.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use regex::Regex;
use tempfile::NamedTempFile;

fn main() {
    let allargs: Vec<String> = std::env::args().collect();
    let hermit = &allargs[1];
    let prog = &allargs[2];

    let sched_file = NamedTempFile::new().unwrap();
    let sched_path = sched_file.path();
    let events = {
        eprintln!(":: Recording schedule to {}", sched_path.display());
        eprintln!(
            ":: Running program under hermit to record schedule: {}",
            prog
        );
        let output = Command::new(hermit)
            .arg("run")
            .arg("--base-env=minimal")
            .arg("--tmp=/tmp")
            .arg("--record-preemptions-to")
            .arg(sched_path)
            .arg(prog)
            .output()
            .expect("hermit run to succeed");
        eprintln!(":: First run exited with status: {:?}", output.status);

        let preempts: PreemptionRecord = PreemptionReader::new(sched_path).load_all();
        preempts.into_global()
    };
    eprintln!(":: Loaded schedule, length {}", events.len());

    // Points where the event after is a different thread:
    let mut switch_points: Vec<usize> = Vec::new();
    {
        let mut expected_pairs: Vec<(i32, i32)> = Vec::new();
        for ix in 0..events.len() - 1 {
            let tid1 = events[ix].dettid;
            let tid2 = events[ix + 1].dettid;
            // Backtraces are not available for events without rips, so exclude switch points
            // to and from such events. TODO(T138829292): Perhaps this test should instead not
            // expect backtraces?
            if tid1 != tid2 && events[ix].end_rip.is_some() && events[ix + 1].end_rip.is_some() {
                switch_points.push(ix);
                expected_pairs.push((tid1.as_raw(), tid2.as_raw()));
            }
        }
        eprintln!(
            ":: Context switch points, at event indices: {:?}",
            switch_points
        );
        eprintln!(":: Tids before/after switch points: {:?}", expected_pairs);
    }

    let mut before_after_switch: Vec<usize> = Vec::new();
    for ix in &switch_points {
        before_after_switch.push(*ix);
        before_after_switch.push(*ix + 1);
    }
    before_after_switch.dedup();

    eprintln!(
        ":: Indices of interest, before/after switches, with dedup: {:?}",
        before_after_switch
    );
    let expected_tids: Vec<i32> = before_after_switch
        .iter()
        .map(|ix| events[*ix].dettid.as_raw())
        .collect();

    let go = |args: Vec<&str>| {
        let stacktrace_paths: Vec<PathBuf> = before_after_switch
            .iter()
            .map(|_ix| NamedTempFile::new().unwrap().path().into())
            .collect();
        let output = {
            let mut rerun_cmd = Command::new(hermit);
            let mut rerun = &mut rerun_cmd;
            rerun = rerun
                .arg("--log=info")
                .arg("run")
                .arg("--base-env=minimal")
                .arg("--tmp=/tmp")
                .args(args);
            for (i, ix) in before_after_switch.iter().enumerate() {
                rerun = rerun.arg(format!(
                    "--stacktrace-event={},{}",
                    ix,
                    stacktrace_paths[i].display()
                ));
            }
            eprintln!(":: Running command: {:?}", &rerun);
            rerun.arg(prog).output().expect("hermit run to succeed")
        };
        eprintln!(":: Final output status: {:?}", output.status);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let event_ixs: Vec<usize> = {
            let re = Regex::new(r"Now output stack trace for scheduled event #(\d+)").unwrap();
            stderr
                .lines()
                .filter(|l| re.is_match(l))
                .map(|l| {
                    let cap = re.captures(l);
                    cap.unwrap()
                        .get(1)
                        .unwrap()
                        .as_str()
                        .parse::<usize>()
                        .unwrap()
                })
                .collect()
        };

        let tids: Vec<i32> = stacktrace_paths
            .iter()
            .filter_map(|path| {
                let stack = fs::read_to_string(path).ok()?;
                let re = Regex::new(r#""thread_id":(\d+)"#).unwrap();
                re.captures(&stack)
                    .and_then(|captures| captures.get(1))
                    .and_then(|tid| tid.as_str().parse::<i32>().ok())
            })
            .collect();

        eprintln!(
            ":: Sched event indices successfully printed ({}): {:?}",
            event_ixs.len(),
            event_ixs
        );
        eprintln!(":: Tids at stack traces ({}): {:?}", tids.len(), tids);

        assert_eq!(
            event_ixs, before_after_switch,
            "Printed stacktraces should be from the expected indices"
        );
        assert_eq!(tids, expected_tids, "Tids during expected events matched.");
    };
    eprintln!("\n:: Running again, with recording, for stack traces...");
    go(vec!["--record-preemptions"]);

    eprintln!("\n:: Running again, with replay to print stack traces again...");
    go(vec!["--replay-schedule-from", sched_path.to_str().unwrap()]);

    eprintln!(":: Test Succeeded!");
}
