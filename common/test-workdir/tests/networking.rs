/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[test]
fn physical_runs_get_local_networking_without_changing_the_caller() {
    // USER unshare must run before any child worker exists. Exec the production
    // control binary rather than attempting namespace setup in this harness.
    let caller = std::fs::read_link("/proc/thread-self/ns/net").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_workdir-control"))
        .arg("networking")
        .output()
        .expect("launch native production-helper regression");
    assert!(
        output.status.success(),
        "native helper regression failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_link("/proc/thread-self/ns/net").unwrap(),
        caller,
        "native child changed the test harness netns"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("networking: two physical Local runs")
    );
}
