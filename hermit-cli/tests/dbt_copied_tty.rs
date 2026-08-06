/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn copied_child_tiocgpgrp_verifies_under_dbt_strict() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-copied-tiocgpgrp");
    fs::create_dir_all(&build_root).expect("failed to create DBT TIOCGPGRP guest directory");
    let guest = build_root.join("dbt_copied_tiocgpgrp");
    let compile = Command::new("cc")
        .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/dbt_copied_tiocgpgrp.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile DBT TIOCGPGRP guest");
    assert!(
        compile.status.success(),
        "DBT TIOCGPGRP guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let output = Command::new("timeout")
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--backend=dbt",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .output()
        .expect("failed to run DBT TIOCGPGRP guest");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "DBT copied-child TIOCGPGRP verification failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(stdout, "dbt-copied-tiocgpgrp-ok\n");
    assert!(
        stderr.contains("Determinism verified"),
        "DBT verification omitted its success marker:\n{stderr}"
    );
}
