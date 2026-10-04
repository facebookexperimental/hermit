/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression fixture for `rust_script_prelude::init` SIGPIPE handling.
//!
//! Compiled by `scripts/check-script-sigpipe.sh` with a plain `rustc` (no
//! rust-script needed), then run as `sigpipe_smoke | head` under `pipefail`.
//! After `init()`, writing far more lines than the downstream reader consumes
//! must terminate the producer cleanly (exit 0), not with a panic or exit 141.

#[path = "../rust_script_prelude.rs"]
mod rust_script_prelude;

fn main() {
    rust_script_prelude::init();
    // Write enough lines that a `head -n1` consumer closes the pipe long before
    // we finish, forcing a SIGPIPE on some later write.
    for i in 0..1_000_000 {
        println!("line {i}");
    }
}
