/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;
use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use hermit::Error;
use hermit::ExitStatus;
use hermit::instruction_map::CacheStatus;
use hermit::instruction_map::default_cache_dir;
use hermit::instruction_map::load_or_generate;

use super::global_opts::GlobalOpts;

/// Generate a JSON map of nondeterministic instructions in an ELF binary.
#[derive(Debug, Parser)]
pub struct InstructionMapOpts {
    /// ELF binary to inspect.
    #[clap(value_name = "BINARY")]
    binary: PathBuf,

    /// Directory for cached instruction maps.
    #[clap(long, value_name = "DIR", env = "HERMIT_INSTRUCTION_MAP_CACHE_DIR")]
    cache_dir: Option<PathBuf>,
}

impl InstructionMapOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();
        let cache_dir = self.cache_dir.clone().unwrap_or_else(default_cache_dir);
        let result = load_or_generate(&self.binary, &cache_dir)?;
        match result.cache_status {
            CacheStatus::Hit => tracing::debug!(
                cache = %result.cache_path.display(),
                "loaded cached instruction map"
            ),
            CacheStatus::Miss => tracing::debug!(
                cache = %result.cache_path.display(),
                sites = result.map.sites.len(),
                "generated instruction map"
            ),
        }

        let stdout = io::stdout();
        let mut output = stdout.lock();
        serde_json::to_writer_pretty(&mut output, &result.map)?;
        output.write_all(b"\n")?;
        Ok(ExitStatus::SUCCESS)
    }
}
