/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! An all-rust runner for some integration tests (no buck)

use std::path::Path;
use std::path::PathBuf;

// use std::process::Command;
use subprocess::Popen;
use subprocess::PopenConfig;
use subprocess::Redirection;

/// Should we do the strict-mode determinism test for this binary?
fn strict_determinism_test_filter(p: &Path) -> bool {
    // let name = p.file_name().unwrap().into_string().unwrap();
    let name = p.file_name().unwrap().to_str().unwrap();
    // let ext = p.extension().to_str().unwrap();
    if name.ends_with(".d")
    //    ext == "d"
    {
        // Generated from a debug file
        false
    } else {
        true
    }
}

fn raw_exec(pb: PathBuf) {
    let root = Path::new("../").canonicalize().unwrap();
    let bin = root.join(&pb);

    let mut p = Popen::create(
        &[bin],
        PopenConfig {
            stdout: Redirection::Pipe,
            stderr: Redirection::Merge,
            ..Default::default()
        },
    )
    .unwrap();

    let (out, _err) = p.communicate(None).unwrap();
    let status = p.wait().unwrap();
    println!(
        "Guest output:\n----------------------------------------\n{}",
        out.unwrap_or("".to_string()),
    );
    assert!(status.success());
}

#[cfg(test)]
mod integration {
    extern crate test_generator;
    use std::path::PathBuf;

    use test_generator::test_resources;

    #[test_resources("target/debug/rustbin_*")]
    fn raw(rsrc: &str) {
        let pb = PathBuf::from(rsrc);
        if super::strict_determinism_test_filter(&pb) {
            println!("Verifying strict run {}", rsrc);
            super::raw_exec(pb);
        } else {
            println!("Not running test for {}", rsrc);
        }
    }
}
