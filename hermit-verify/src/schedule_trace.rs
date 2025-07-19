/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::read_trace;
use detcore_model::pid::DetTid;
use detcore_model::schedule::SchedEvent;
use edit_distance::iterable_bubble_sort;

use crate::CommonOpts;

#[derive(Debug, Parser)]
pub enum SchedTraceOpts {
    /// Inspect and report statistics on one schedule.
    Inspect(InspectOpts),

    /// Just dump the raw schedule
    Print(PrintOpts),

    /// Difference between two preempt files
    Diff(DiffOpts),

    /// Run the interpolation between two preempt files
    Interpolate(InterpolateOpts),
}

impl SchedTraceOpts {
    pub fn main(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        match self {
            SchedTraceOpts::Diff(x) => x.run(common_args),
            SchedTraceOpts::Interpolate(x) => x.run(common_args),
            SchedTraceOpts::Inspect(x) => x.run(common_args),
            SchedTraceOpts::Print(x) => x.run(common_args),
        }
    }
}

#[derive(Debug, Parser)]
pub struct DiffOpts {
    /// First schedule file to compare.
    file_a: PathBuf,
    /// Second schedule file to compare.
    file_b: PathBuf,
}

impl DiffOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let first_schedule = read_trace(&self.file_a);
        let second_schedule = read_trace(&self.file_b);
        println!("first schedule length: {}", first_schedule.len());
        println!("second schedule length: {}", second_schedule.len());
        let bubbles = iterable_bubble_sort(&first_schedule, &second_schedule);
        eprintln!(
            ":: {}",
            format!(
                "Diffs found in schedules : Swap distance  = {}, Edit distance = {} ",
                bubbles.swap_distance(),
                bubbles.edit_distance()
            )
            .yellow()
            .bold()
        );
        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct InspectOpts {
    /// Schedule to inspect.
    pub sched_file: PathBuf,

    /// Print O(N) output proportional to the length of the schedule.
    #[clap(long, short = 'v')]
    pub verbose: bool,
}

/// Return all events which are right before a preemption.
fn preempt_events(sched: &Vec<SchedEvent>) -> Vec<SchedEvent> {
    let mut acc = Vec::new();
    for ix in 0..sched.len() - 1 {
        if sched[ix].dettid != sched[ix + 1].dettid {
            acc.push(sched[ix].clone());
        }
    }
    acc
}

/// Split apart an event series into a per-thread record.
fn per_thread_events(events: &Vec<SchedEvent>) -> BTreeMap<DetTid, Vec<SchedEvent>> {
    let mut acc = BTreeMap::new();
    for evt in events {
        let vec: &mut Vec<SchedEvent> = acc.entry(evt.dettid).or_default();
        vec.push(evt.clone());
    }
    acc
}

impl InspectOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let schedule = read_trace(&self.sched_file);
        println!("Schedule length: {}", schedule.len());

        let preempts = preempt_events(&schedule);
        println!("Context-switch/preemption events: {}", preempts.len());
        println!("Per thread pre-preemption events:");
        let per_thread_preempts = per_thread_events(&preempts);
        for (tid, vec) in &per_thread_preempts {
            println!("  tid {}: {} preemptions", tid, vec.len());
            if self.verbose {
                for evt in vec {
                    println!("    {}", evt);
                }
            }
        }

        if self.verbose {
            println!("Full context switch event list in order, with before/after events:");
            let mut count = 0;
            for ix in 0..schedule.len() - 1 {
                if schedule[ix].dettid != schedule[ix + 1].dettid {
                    count += 1;
                    println!(" {: >2}: {} => {}", count, schedule[ix], schedule[ix + 1]);
                }
            }
        }

        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct PrintOpts {
    /// Schedule to inspect.
    pub sched_file: PathBuf,

    /// Print the indices as well as the raw events.
    #[clap(long, short = 'i')]
    indices: bool,

    /// Print only events from the given tid, leaving a blank line anywhere the tid was preempted.
    #[clap(long)]
    tid: Option<i32>,

    /// Strip times before printing.
    #[clap(long)]
    strip_times: bool,
}

impl PrintOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let schedule = read_trace(&self.sched_file);
        let mut last_skipped = false;
        let mut thread_counts: BTreeMap<DetTid, u64> = BTreeMap::new();
        for (global_ix, ev) in schedule.iter().enumerate() {
            let entry = thread_counts.entry(ev.dettid).or_default();
            let current_thread_ix: u64 = *entry;
            *entry += 1;

            if let Some(tid) = self.tid {
                if ev.dettid.as_raw() == tid {
                    last_skipped = false;
                } else {
                    if !last_skipped {
                        println!();
                    }
                    last_skipped = true;
                    continue;
                }
            }

            let mut ev = ev.clone();
            if self.strip_times {
                ev.end_time = None;
            }

            if self.indices {
                println!(
                    "{: <9} {}",
                    format!("({},{})", global_ix, current_thread_ix),
                    ev
                );
            } else {
                println!("{}", ev);
            }
        }
        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct InterpolateOpts {
    /// Interpolate file a and b by how much percentage. Currently, it takes an integer between 0 and 100.
    interpolate: u8,

    /// First log to compare.
    file_a: PathBuf,

    /// Second log to compare.
    file_b: PathBuf,
}

impl InterpolateOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let first_schedule = read_trace(&self.file_a);
        let second_schedule = read_trace(&self.file_b);
        // let i = 0;
        let dir = tempfile::Builder::new()
            .prefix("hermit_internal")
            .tempdir()?;
        let tmpdir_path = dir.keep();
        eprintln!(":: Temp workspace: {}", tmpdir_path.display());
        let (_swap_dist, _edit_dist, requested_schedule) = {
            let mut bubbles = iterable_bubble_sort(&first_schedule, &second_schedule);
            (
                bubbles.swap_distance(),
                bubbles.edit_distance(),
                bubbles
                    .interpolate(self.interpolate)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let sched_path = tmpdir_path.join(format!("interpolate_{}.events", self.interpolate));
        let next_sched = PreemptionRecord::from_sched_events(requested_schedule);
        next_sched.write_to_disk(&sched_path).unwrap();

        Ok(true)
    }
}
