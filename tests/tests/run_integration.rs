/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

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

fn hermit_strict(pb: PathBuf) {
    let root = Path::new("../").canonicalize().unwrap();
    // let stat = Command::new("pwd").status().unwrap();
    let bin = root.join(&pb);

    let mut p = Popen::create(
        &[bin],
        PopenConfig {
            stdout: Redirection::Pipe,
            stderr: Redirection::Merge,
            ..Default::default()
        },
    );

    let (out, err) = p.communicate(None).unwrap();

    println!(
        "Guest output:\n----------------------------------------\n{}",
        String::from_utf8_lossy(&out),
    );

    /*
        // let stat = Command::new(&bin).status().unwrap_or_else(|err| {
        let out = Command::new(&bin).output().unwrap_or_else(|err| {
            panic!(
                "Failed to execute test binary at path: {}\nError: {}",
                bin.display(),
                err
            )
        });
        // assert!(stat.success());

        println!(
            "Guest stdout:\n----------------------------------------\n{}",
            String::from_utf8_lossy(&out.stdout),
        );
        println!(
            "Guest stderr:\n----------------------------------------\n{}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(out.status.success());
    */

    todo!()
}

#[cfg(test)]
mod integration {
    extern crate test_generator;
    use std::path::PathBuf;

    use test_generator::test_resources;

    #[test]
    fn my_unit() {
        panic!("bad");
    }

    #[test_resources("target/debug/rustbin_*")]
    fn hermit_strict(rsrc: &str) {
        let pb = PathBuf::from(rsrc);
        if super::strict_determinism_test_filter(&pb) {
            println!("Verifying strict run {}", rsrc);
            super::hermit_strict(pb);
        } else {
            println!("Not running test for {}", rsrc);
        }
    }
}
