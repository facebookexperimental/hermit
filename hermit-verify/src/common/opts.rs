/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::SystemTime;

use clap::Parser;

#[derive(Parser, Debug)]
pub struct CommonOpts {
    /// Specify hermit binary path.
    #[clap(long, env = "HERMIT_BIN", default_value = "hermit")]
    pub hermit_bin: PathBuf,

    // In test environment allows to specify root directory with produced artifacts
    #[clap(long, env = "TEST_RESULT_ARTIFACTS_DIR")]
    pub test_result_artifact_dir: Option<PathBuf>,

    // Allows to ignore TEST_RESULT_ARTIFACTS_DIR when running in test environment
    #[clap(long, default_value = "false")]
    pub ignore_test_result_artifacts_dir: bool,

    // Specify root directory with produced artifacts
    #[clap(long)]
    pub temp_dir_path: Option<PathBuf>,

    /// Allow the run itself to exit with a nonzero code, but still count the
    /// verification as successful if all runs match each other.
    #[clap(long)]
    pub allow_nonzero_exit: bool,

    /// Print extra information while running.
    #[clap(long, short = 'v')]
    pub verbose: bool,

    /// The virtual-time epoch given to every hermit run of this verification.
    ///
    /// `hermit run` samples the host clock for its epoch when none is supplied,
    /// so two separate runs would otherwise start their virtual clocks at two
    /// different instants and every compared time would differ
    /// (<https://github.com/rrnewton/hermit/issues/3411>). `None` when
    /// `HERMIT_EPOCH` is set: each run then inherits that one explicit value.
    #[clap(skip)]
    pub comparison_epoch: Option<String>,
}

impl CommonOpts {
    /// Resolve [`Self::comparison_epoch`] once, before the first run.
    pub fn resolve_comparison_epoch(
        &mut self,
        environment_epoch: Option<OsString>,
        capture_now: impl FnOnce() -> SystemTime,
    ) {
        self.comparison_epoch = match environment_epoch {
            Some(_) => None,
            None => Some(detcore_model::config::epoch_from_host_time(capture_now()).to_rfc3339()),
        };
    }

    /// Give a `hermit ... run ...` argument vector this verification's epoch,
    /// unless the caller's own `--hermit-arg` already chose one.
    pub fn pin_comparison_epoch(&self, mut hermit_args: Vec<String>) -> Vec<String> {
        let Some(epoch) = &self.comparison_epoch else {
            return hermit_args;
        };
        let Some(run) = hermit_args.iter().position(|arg| arg == "run") else {
            return hermit_args;
        };
        let run_options = hermit_args[run + 1..]
            .iter()
            .take_while(|arg| arg.as_str() != "--");
        if run_options
            .clone()
            .any(|arg| arg == "--epoch" || arg.starts_with("--epoch="))
        {
            return hermit_args;
        }
        hermit_args.insert(run + 1, format!("--epoch={epoch}"));
        hermit_args
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn opts() -> CommonOpts {
        CommonOpts::parse_from(["hermit-verify"])
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn one_captured_epoch_is_given_to_every_run() {
        let mut opts = opts();
        opts.resolve_comparison_epoch(None, || {
            UNIX_EPOCH + Duration::new(978_307_199, 123_456_789)
        });
        let args = strings(&["--log=trace", "run", "--chaos", "--", "/bin/true", "run"]);
        let expected = strings(&[
            "--log=trace",
            "run",
            "--epoch=2000-12-31T23:59:59.123456789+00:00",
            "--chaos",
            "--",
            "/bin/true",
            "run",
        ]);
        assert_eq!(opts.pin_comparison_epoch(args.clone()), expected);
        assert_eq!(opts.pin_comparison_epoch(args), expected);
    }

    #[test]
    fn an_explicit_epoch_is_never_replaced_or_duplicated() {
        let mut opts = opts();
        opts.resolve_comparison_epoch(None, || UNIX_EPOCH);
        for explicit in [
            strings(&["run", "--epoch=2026-01-01T00:00:00Z", "--", "/bin/true"]),
            strings(&["run", "--epoch", "2026-01-01T00:00:00Z", "--", "/bin/true"]),
        ] {
            assert_eq!(opts.pin_comparison_epoch(explicit.clone()), explicit);
        }
        // A guest argument spelled like the option does not count.
        assert_eq!(
            opts.pin_comparison_epoch(strings(&["run", "--", "/bin/echo", "--epoch=x"])),
            strings(&[
                "run",
                "--epoch=1970-01-01T00:00:00+00:00",
                "--",
                "/bin/echo",
                "--epoch=x"
            ]),
        );
    }

    #[test]
    fn an_environment_epoch_is_inherited_without_reading_the_clock() {
        let mut opts = opts();
        opts.resolve_comparison_epoch(Some("2026-01-01T00:00:00Z".into()), || {
            panic!("an explicit environment epoch must not sample the clock")
        });
        assert_eq!(opts.comparison_epoch, None);
        let args = strings(&["run", "--", "/bin/true"]);
        assert_eq!(opts.pin_comparison_epoch(args.clone()), args);
    }
}
