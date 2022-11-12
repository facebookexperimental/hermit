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

    /// Allow the run itself to exit with a nonzero code, but still count the
    /// verification as successful if all runs match each other.
    #[clap(long)]
    pub allow_nonzero_exit: bool,
}
