/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use detcore::preemptions::read_trace;
use detcore::preemptions::PreemptionRecord;
use edit_distance::iterable_bubble_sort;

use crate::CommonOpts;

#[derive(Debug, Parser)]
pub enum SchedTraceOpts {
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
        }
    }
}

#[derive(Debug, Parser)]
pub struct DiffOpts {
    /// First log to compare.
    file_a: PathBuf,
    /// Second log to compare.
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
            format!("Diffs found in schedules {} ", bubbles.swap_distance())
                .yellow()
                .bold()
        );
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
        let tmpdir_path = dir.into_path();
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
