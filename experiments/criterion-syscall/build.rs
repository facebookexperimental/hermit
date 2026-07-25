/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let source = PathBuf::from("fixtures/syscall_server.c");
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("syscall-server");
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());

    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=CC");

    let status = Command::new(&compiler)
        .args([
            "-O3",
            "-std=c11",
            "-D_GNU_SOURCE",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-plt",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to invoke C compiler {}: {error}",
                PathBuf::from(&compiler).display()
            )
        });

    assert!(
        status.success(),
        "C compiler failed while building {}",
        source.display()
    );
    println!("cargo:rustc-env=SYSCALL_BENCH_HELPER={}", output.display());
}
