/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit rewrite-exclusives`: report, and optionally write, the LSE rewrite
//! of an aarch64 ELF file's exclusive retry loops.
//!
//! The same code `hermit run --rewrite-exclusives` uses, exposed on its own so
//! a binary can be inspected before anyone runs it: which loops the rewriter
//! recognises, which it leaves alone and why.

use std::path::PathBuf;

use clap::Parser;
use hermit::Error;
use hermit::ExitStatus;
use hermit::aarch64_lse::elf;

#[derive(Debug, Parser)]
pub struct RewriteExclusivesOpts {
    /// ELF files to inspect.
    #[clap(value_name = "FILE", required = true)]
    files: Vec<PathBuf>,

    /// Write each rewritten file into this directory, under its own name,
    /// instead of only reporting what the rewrite would do.
    #[clap(long, value_name = "DIR")]
    output_dir: Option<PathBuf>,

    /// List every loop left alone, not just the first few per file.
    #[clap(long)]
    list_refusals: bool,
}

impl RewriteExclusivesOpts {
    pub fn main(&self) -> Result<ExitStatus, Error> {
        if let Some(dir) = &self.output_dir {
            std::fs::create_dir_all(dir)?;
        }
        let mut incomplete = false;
        for file in &self.files {
            let report = match &self.output_dir {
                Some(dir) => {
                    let name = file
                        .file_name()
                        .ok_or_else(|| Error::msg(format!("{} has no file name", file.display())))?;
                    elf::rewrite_file(file, dir.join(name))?
                }
                None => elf::rewrite_bytes(&mut std::fs::read(file)?)?,
            };
            let Some(report) = report else {
                println!("{}: not an aarch64 ELF image", file.display());
                incomplete = true;
                continue;
            };
            println!(
                "{}: exclusive_instructions={} rewritten={} left_alone={}",
                file.display(),
                report.exclusive_instructions,
                report.patched_total(),
                report.refused_total(),
            );
            for (idiom, count) in &report.patched {
                println!("  rewritten {idiom}: {count}");
            }
            for (reason, count) in &report.refused {
                println!("  left alone {reason}: {count}");
            }
            for segment in &report.skipped_segments {
                println!("  skipped {segment}");
            }
            if self.list_refusals {
                for (addr, why) in &report.refused_examples {
                    println!("  at {addr:#x}: {why}");
                }
            }
            incomplete |= !report.complete();
        }
        // A nonzero status says the files are not free of exclusive loops, so
        // a caller that needs that property can test for it.
        Ok(ExitStatus::Exited(if incomplete { 1 } else { 0 }))
    }
}
