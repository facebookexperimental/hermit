/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use clap::ValueEnum;

use crate::tests::runners::cargo_integration_runner::IntegrationRunner;
use crate::tests::runners::cargo_integration_runner::OutputFormat;
use crate::tests::runners::cargo_integration_runner::RunnerConfig;
use crate::tests::runners::matrix_discovery::HermitMode;

#[derive(Debug, Clone, ValueEnum)]
enum CliHermitMode {
    Default,
    Strict,
    Chaos,
    VirtualTime,
    VirtualRandom,
    Record,
    Replay,
    Verify,
}

impl From<CliHermitMode> for HermitMode {
    fn from(mode: CliHermitMode) -> Self {
        match mode {
            CliHermitMode::Default => HermitMode::Default,
            CliHermitMode::Strict => HermitMode::Strict,
            CliHermitMode::Chaos => HermitMode::Chaos,
            CliHermitMode::VirtualTime => HermitMode::VirtualTime,
            CliHermitMode::VirtualRandom => HermitMode::VirtualRandom,
            CliHermitMode::Record => HermitMode::Record,
            CliHermitMode::Replay => HermitMode::Replay,
            CliHermitMode::Verify => HermitMode::Verify,
        }
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum CliOutputFormat {
    Human,
    Json,
    Junit,
}

impl From<CliOutputFormat> for OutputFormat {
    fn from(format: CliOutputFormat) -> Self {
        match format {
            CliOutputFormat::Human => OutputFormat::Human,
            CliOutputFormat::Json => OutputFormat::Json,
            CliOutputFormat::Junit => OutputFormat::Junit,
        }
    }
}

/// Options for the integration test runner
#[derive(Debug, Parser)]
pub struct IntegrationOpts {
    /// Filter tests by name pattern
    #[arg(short, long)]
    pub filter: Option<String>,

    /// Filter tests by category (basic, determinism, threading, ipc, memory, flaky, stress, standalone, shell)
    #[arg(short = 'c', long)]
    pub category: Option<String>,

    /// Filter tests by Hermit mode
    #[arg(short = 'm', long, value_enum)]
    pub mode: Option<Vec<CliHermitMode>>,

    /// Maximum parallel test executions
    #[arg(short = 'j', long, default_value = "4")]
    pub parallel: usize,

    /// Override timeout for all tests (seconds)
    #[arg(long)]
    pub timeout: Option<u64>,

    /// Silently pass hardware-dependent tests instead of skipping them
    #[arg(long)]
    pub silently_pass_hardware_tests: bool,

    /// Output format
    #[arg(short = 'o', long, value_enum, default_value = "human")]
    pub output_format: CliOutputFormat,

    /// Write output to file instead of stdout
    #[arg(long)]
    pub output_file: Option<PathBuf>,

    /// Generate coverage manifest and exit
    #[arg(long)]
    pub generate_manifest: bool,

    /// Dry run - show what would be executed without running
    #[arg(long)]
    pub dry_run: bool,

    /// Run local validation only (quick sanity check)
    #[arg(long)]
    pub local_validation: bool,
}

impl IntegrationOpts {
    pub fn main(
        &self,
        _global: &crate::global_opts::GlobalOpts,
    ) -> Result<crate::ExitStatus, crate::Error> {
        let mut config = RunnerConfig {
            filter: self.filter.clone(),
            category_filter: self.category.clone(),
            mode_filter: self
                .mode
                .clone()
                .map(|modes| modes.into_iter().map(Into::into).collect()),
            max_parallel: self.parallel,
            timeout_override: self.timeout.map(Duration::from_secs),
            silently_pass_hardware_tests: self.silently_pass_hardware_tests,
            output_format: self.output_format.clone().into(),
            output_file: self.output_file.clone(),
            generate_manifest: self.generate_manifest,
            dry_run: self.dry_run,
        };

        let mut runner = IntegrationRunner::new(config);

        if self.local_validation {
            runner
                .run_local_validation()
                .map_err(|e| crate::Error::from(e))?;
            return Ok(crate::ExitStatus::Exited(0));
        }

        let results = runner.run().map_err(|e| crate::Error::from(e))?;

        // Exit with error code if any tests failed
        let has_failures = results.iter().any(|r| r.status.is_failure());
        if has_failures {
            Ok(crate::ExitStatus::Exited(1))
        } else {
            Ok(crate::ExitStatus::Exited(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_mode_conversion() {
        assert_eq!(
            HermitMode::from(CliHermitMode::Default),
            HermitMode::Default
        );
        assert_eq!(HermitMode::from(CliHermitMode::Strict), HermitMode::Strict);
        assert_eq!(HermitMode::from(CliHermitMode::Chaos), HermitMode::Chaos);
    }

    #[test]
    fn test_cli_output_format_conversion() {
        assert_eq!(
            OutputFormat::from(CliOutputFormat::Human),
            OutputFormat::Human
        );
        assert_eq!(
            OutputFormat::from(CliOutputFormat::Json),
            OutputFormat::Json
        );
        assert_eq!(
            OutputFormat::from(CliOutputFormat::Junit),
            OutputFormat::Junit
        );
    }
}
