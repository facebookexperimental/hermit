/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--verify` must not claim determinism it did not check.
//!
//! Reverie types many syscall output buffers as bare pointers, so the compared
//! records show the buffer's ADDRESS and not its bytes. Two runs whose buffers
//! differ while their return values agree therefore produce character-identical
//! records and compare equal. Measured on a netlink `recvmsg` that returns a
//! stable `Ok(1468)` while four payload bytes vary: the same guest reported
//! `verdict: matched, bitwise_parity: true` and printed "Determinism verified",
//! while the same command with `--detlog-io-buffers` reported `diverged`.
//!
//! The coverage fix was to make that hashing the DEFAULT rather than an opt-in;
//! `--no-detlog-io-buffers` now selects the weaker comparison deliberately. This
//! test guards the other half: that when content was NOT compared, the verdict
//! says so instead of claiming determinism outright.

use std::path::Path;
use std::process::Command;

/// Run `/bin/true` under `--verify --verify-strict`, optionally with the
/// output-buffer hash, and return (stderr, parsed verify JSON).
///
/// THE SENSE OF THE FLAG INVERTED, so which branch needs an argument inverted
/// with it. Buffer hashing is ON BY DEFAULT since the io-buffer default flip,
/// and the positive `--detlog-io-buffers` spelling no longer parses at all, so
/// `with_io_buffers == true` is now the plain invocation and it is the FALSE
/// case that has to ask for the weaker comparison. Every assertion in both
/// tests below is unchanged; only the way the two cases are selected moved.
/// The `true` case is now strictly more valuable than before, because it
/// exercises the configuration an ordinary user actually gets.
fn verify(with_io_buffers: bool) -> (String, serde_json::Value) {
    let json = tempfile::NamedTempFile::new().expect("temp file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(["run", "--strict", "--verify", "--verify-strict"]);
    if !with_io_buffers {
        command.arg("--no-detlog-io-buffers");
    }
    command
        .arg("--verify-json")
        .arg(json.path())
        .args(["--", "/bin/true"]);
    let output = command.output().expect("failed to start hermit");
    let text = std::fs::read_to_string(json.path()).expect("verify json");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        serde_json::from_str(&text).expect("verify json parses"),
    )
}

/// Guard: `/bin/true` must actually verify, or neither assertion below means
/// anything.
fn assert_matched(report: &serde_json::Value) {
    assert_eq!(
        report["verdict"], "matched",
        "/bin/true should verify; this test cannot say anything about the wording of a \
         success message that was never printed"
    );
}

#[test]
fn a_verdict_without_buffer_content_does_not_claim_determinism() {
    let (stderr, report) = verify(false);
    assert_matched(&report);
    assert_eq!(report["bitwise_parity"], false);
    assert_eq!(
        report["comparison"]["compare_io_buffers"], false,
        "the envelope must record that buffer content did not participate, so a consumer can \
         require it rather than assume it"
    );
    // The "Determinism verified" marker itself is deliberately NOT removed --
    // ~110 files assert on that substring -- so what is asserted here is that
    // the claim is QUALIFIED, not that it is absent.
    assert!(
        stderr.contains("output-buffer CONTENT was not compared"),
        "reported success without comparing syscall output-buffer content and without saying \
         so. A divergence confined to a buffer whose length is stable is invisible to this \
         comparison, so an unqualified claim overstates what was \
         established.\nstderr:\n{stderr}"
    );
}

#[test]
fn a_verdict_with_buffer_content_may_claim_determinism() {
    // The converse, so the wording change is not just "never claim anything":
    // when content IS compared the strong sentence is earned and still printed.
    let (stderr, report) = verify(true);
    assert_matched(&report);
    assert_eq!(report["comparison"]["compare_io_buffers"], true);
    assert_eq!(report["bitwise_parity"], true);
    assert!(
        stderr.contains("Determinism verified"),
        "with buffer content compared the strong claim is earned and should be \
         printed.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("output-buffer CONTENT was not compared"),
        "the qualification must NOT appear when content WAS compared, or it is noise rather \
         than information.\nstderr:\n{stderr}"
    );
}

#[test]
fn the_guest_binary_this_test_relies_on_exists() {
    assert!(Path::new("/bin/true").exists(), "/bin/true is required");
}
