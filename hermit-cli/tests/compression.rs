/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

// Each harness attempt runs 15 strict Hermit processes, and a busy host
// sometimes delivers a PMU interrupt late enough that Hermit refuses the run
// (exit 122, cause=skid-overshoot). That measures the host, not the guest, so
// such an attempt runs again; nothing else does
// (https://github.com/rrnewton/hermit/issues/1845).
const SKID_ATTEMPTS: u32 = detcore_model::SKID_RETRY_MAX_ATTEMPTS;

#[test]
fn compression_tools_are_deterministic_under_strict_hermit() {
    let _guard = HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository root");
    let runner = repo_root.join("hermit-cli/tests/fixtures/compression/run.sh");
    let artifact_root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("compression-{}", std::process::id()));

    let (_, output, ()) = hermit_test::skid_overshoot::retry_typed_skid_refusal(
        "compression",
        SKID_ATTEMPTS,
        || {
            if artifact_root.exists() {
                fs::remove_dir_all(&artifact_root)
                    .expect("failed to remove stale compression artifacts");
            }
            let output = Command::new(&runner)
                .env("HERMIT_BIN", env!("CARGO_BIN_EXE_hermit"))
                // Staged test sources may not contain Git metadata.
                .env("COMPRESSION_SOURCE_IDENTITY", "content")
                // The script's strict runs take their epoch from this.
                .env("HERMIT_EPOCH", detcore_model::config::CANONICAL_TEST_EPOCH)
                .env("COMPRESSION_ARTIFACT_ROOT", &artifact_root)
                .env("COMPRESSION_INPUT_LINES", "12000")
                .output()
                .unwrap_or_else(|error| panic!("failed to start {}: {error}", runner.display()));
            (output, ())
        },
        &|_| {},
    );

    // The script's guests are the host's bzip2/gzip: a host-fact v3
    // refusal of one of its Hermit runs ends the case as a counted
    // skip here, before the success assertion can fail it.
    {
        let bzip2 = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("bzip2"))
                .find(|candidate| candidate.is_file())
        });
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        hermit_test::abort_on_host_fact_v3_refusal_for(
            "compression",
            bzip2.as_deref(),
            true,
            output.status.code(),
            &text,
        );
    }

    assert!(
        output.status.success(),
        "compression harness failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("runner stdout was not UTF-8");
    assert!(
        stdout.contains(
            "Compression determinism: 3/3 strict runs produced SHA-identical bzip2, gzip, and \
             bzip2recover output."
        ),
        "missing deterministic success marker:\n{stdout}"
    );

    let evidence = fs::read_dir(&artifact_root)
        .expect("compression artifact directory should exist")
        .next()
        .expect("compression runner should create one evidence directory")
        .expect("failed to read compression evidence entry")
        .path();
    let results = fs::read_to_string(evidence.join("results.tsv"))
        .expect("compression results.tsv should exist");
    assert_eq!(
        results.lines().count(),
        4,
        "expected header plus three runs"
    );
    for line in results.lines().skip(1) {
        let recovered_blocks = line
            .rsplit_once('\t')
            .expect("result row should contain a recovered-block count")
            .1
            .parse::<usize>()
            .expect("recovered-block count should be an integer");
        assert!(
            recovered_blocks >= 2,
            "expected a multi-block archive, got {recovered_blocks}"
        );
    }
    assert!(evidence.join("summary.txt").is_file());
}
