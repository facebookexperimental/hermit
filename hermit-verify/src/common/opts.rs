/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

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
}
