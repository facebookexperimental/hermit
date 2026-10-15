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

#[path = "common/hermit_binary.rs"]
mod hermit_test;

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
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let text = std::fs::read_to_string(json.path()).expect("verify json");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        serde_json::from_str(&text).expect("verify json parses"),
    )
}

/// The console phrase that claims bitwise parity. It must appear exactly when
/// the published report says `bitwise_parity: true`.
const BITWISE_PARITY_CLAIM: &str = "bitwise parity established";

/// Run `/bin/true` under a plain `--verify` (the lossy `Stripped` comparison,
/// no `--verify-strict`) and return (stderr, parsed verify JSON).
fn verify_plain() -> (String, serde_json::Value) {
    let json = tempfile::NamedTempFile::new().expect("temp file");
    let output = hermit_test::hermit_output(
        "hermit run",
        Command::new(env!("CARGO_BIN_EXE_hermit"))
            .args(["run", "--strict", "--verify", "--verify-json"])
            .arg(json.path())
            .args(["--", "/bin/true"]),
    );
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
    assert!(
        !stderr.contains(BITWISE_PARITY_CLAIM),
        "the report says bitwise_parity is false, so the console must not claim it.\nstderr:\n{stderr}"
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
    assert!(
        stderr.contains(BITWISE_PARITY_CLAIM),
        "the report says bitwise_parity is true, so the console should say so.\nstderr:\n{stderr}"
    );
}

#[test]
fn a_plain_verify_says_its_match_is_not_bitwise() {
    // A plain `--verify` prints the same "Determinism verified" sentence as
    // `--verify-strict`, but its log comparison is the lossy Stripped one. The
    // line after the sentence must say so and must not claim bitwise parity.
    let (stderr, report) = verify_plain();
    assert_matched(&report);
    assert_eq!(report["bitwise_parity"], false);
    assert_eq!(report["comparison"]["strip_lines"], true);
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "the success sentence is kept verbatim for its consumers.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains(BITWISE_PARITY_CLAIM),
        "a Stripped match is not bitwise parity.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("under the lossy Stripped comparison")
            && stderr.contains("This is not a bitwise comparison; add --verify-strict for one."),
        "the console must say which comparison the match rests on.\nstderr:\n{stderr}"
    );
}

#[test]
fn the_guest_binary_this_test_relies_on_exists() {
    assert!(Path::new("/bin/true").exists(), "/bin/true is required");
}

// A divergence caused by a host file changing during a run.
//
// Hermit numbers inodes in the order it first sees them, so a host file that
// is replaced between two opens in one run gives that run one more inode, and
// every later inode number of that run is one higher. On 2026-10-05 a package
// update replacing `/etc/ld.so.cache` between the opens of `sar` and its child
// `sadc` made `compat/sar-resource-tables` diverge that way. `--verify` must
// name such a cause rather than report a bare divergence, and must not name
// one for a divergence that has a different cause.

#[path = "common/host_input.rs"]
mod host_input;

use host_input::HOST_INPUT_GUEST;
use host_input::SELF_REPLACING_GUEST;

/// [`host_input::verify_across_host_action`] on the default (ptrace) backend.
fn verify_across_host_action(
    name: &str,
    guest: &str,
    replace_in_run1: bool,
    lines: [&str; 2],
) -> (
    std::path::PathBuf,
    String,
    hermit::canonical_verdict::VerificationReport,
) {
    host_input::verify_across_host_action(
        Path::new(env!("CARGO_BIN_EXE_hermit")),
        &[],
        &[],
        name,
        guest,
        replace_in_run1,
        lines,
    )
}

/// [`verify_across_host_action`] under `--passthru-opt`, which needs
/// `--allow-unsupported-syscalls` and refuses `--strict`.
fn verify_across_host_action_passthru(
    name: &str,
    guest: &str,
    replace_in_run1: bool,
    lines: [&str; 2],
) -> (
    std::path::PathBuf,
    String,
    hermit::canonical_verdict::VerificationReport,
) {
    host_input::verify_across_host_action_with(
        Path::new(env!("CARGO_BIN_EXE_hermit")),
        &[],
        &["--allow-unsupported-syscalls", "--passthru-opt"],
        &[],
        name,
        guest,
        replace_in_run1,
        lines,
    )
}

/// The sar divergence in miniature: a host file replaced between run 1's two
/// opens of it. `--verify` reports the divergence as a typed host input
/// change, naming the run, the file and both of its host identities, and
/// still fails.
#[test]
fn a_host_file_replaced_during_run_1_is_named_as_the_cause() {
    let (root, stderr, report) =
        verify_across_host_action("host-input-replaced", HOST_INPUT_GUEST, true, ["go", "go"]);
    assert!(!report.verified, "{stderr}");
    assert!(!report.bitwise_parity);
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::InfrastructureError,
        "{stderr}"
    );
    assert!(
        report.comparison.is_some(),
        "the comparison is kept as evidence"
    );
    assert!(
        report.first_divergent_record.is_some(),
        "and where it diverged"
    );
    let Some(hermit::canonical_verdict::InfrastructureError::HostInputChanged {
        run,
        path,
        before,
        after,
    }) = &report.infrastructure_error
    else {
        panic!(
            "no host input change named: {:?}\n{stderr}",
            report.infrastructure_error
        );
    };
    assert_eq!(*run, hermit::canonical_verdict::VerificationRun::Run1);
    assert_eq!(Path::new(path), root.join("F"));
    assert_ne!(before.ino, after.ino, "the replacement is a new host inode");
    assert!(
        stderr.contains("HERMIT_HOST_INPUT_CHANGED host input changed during run 1: "),
        "{stderr}"
    );
    assert!(stderr.contains("Failure: nondeterministic."), "{stderr}");
}

/// The control: the same guest diverging because the host sent each run a
/// different line, with no host file changed. That divergence is not
/// attributed to a host input change.
#[test]
fn a_divergence_with_no_host_file_change_names_no_host_input_change() {
    let (_root, stderr, report) = verify_across_host_action(
        "host-input-unchanged",
        HOST_INPUT_GUEST,
        false,
        ["one", "two"],
    );
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED"), "{stderr}");
}

/// The counterexample a pattern alone cannot refuse: the guest replaces `F`
/// itself, in run 1 only, because the runs read different lines. The runs'
/// patterns differ as a host replacement's would, but the divergence came
/// first, so it is not attributed to a host input change and stays a failure.
#[test]
fn a_guest_that_replaces_a_file_after_diverging_names_no_host_input_change() {
    let (_root, stderr, report) = verify_across_host_action(
        "host-input-self-replaced",
        SELF_REPLACING_GUEST,
        false,
        // The same length, since the shell reads its line a byte per syscall.
        ["moveA", "moveB"],
    );
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED"), "{stderr}");
}

/// The counterexample a log prefix cannot refuse: the guest replaces `F` with
/// raw calls whose differing effects the compared log does not show (see
/// [`host_input::rebinding_guest`]). The opens' identities differ as a host
/// replacement's would, and the first difference comes after the second
/// open, but the guest renamed onto `F`, so the change is not named and the
/// run stays a divergence.
#[test]
fn a_guest_that_rebinds_the_path_itself_names_no_host_input_change() {
    let guest = host_input::rebinding_guest(&host_input::python_interpreter());
    let (_root, stderr, report) =
        verify_across_host_action("host-input-rebound", &guest, false, ["go", "go"]);
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGE"), "{stderr}");
}

/// A syscall issued from the untraced stub of Reverie's private page passes
/// the ptrace backend unobserved, so a guest that rebinds `F` that way leaves
/// no record of it. Its forked child then takes the SIGILL of the `ud2` after
/// the stub, and Reverie ends the run there with its named failure
/// (`UntracedStubSyscall`, at 0x71000002), which hermit reports as a policy
/// refusal: exit 122 in run 1, with no run 2 and no host input change named
/// (https://github.com/rrnewton/reverie/issues/1064). Before that, Reverie
/// delivered the SIGILL and the run went on to a divergence.
#[test]
fn a_rebinding_through_reverie_private_page_ends_the_run_with_a_refusal() {
    let guest = host_input::private_page_rebinding_guest(&host_input::python_interpreter());
    let (status, stderr) = host_input::verify_ended_in_run_1(
        Path::new(env!("CARGO_BIN_EXE_hermit")),
        "host-input-private-page",
        &guest,
        "go",
    );
    assert_eq!(
        status,
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("untraced private-page stub") && stderr.contains("0x71000002"),
        "the refusal must name the stub and its address:\n{stderr}"
    );
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGE"), "{stderr}");
}

/// [`a_rebinding_through_reverie_private_page_ends_the_run_with_a_refusal`]
/// with the page changed first, as `rewrite` says, so that the stub returns
/// and no SIGILL occurs. Detcore reports the change to the page, so no host
/// input change is named and the run stays a divergence.
fn assert_a_rewritten_private_page_names_no_host_input_change(
    name: &str,
    rewrite: host_input::PrivatePageRewrite,
) {
    let guest =
        host_input::private_page_rewriting_guest(&host_input::python_interpreter(), rewrite);
    let (_root, stderr, report) = verify_across_host_action(name, &guest, false, ["go", "go"]);
    // In both runs the child returned from the stub and exited 9 (wait
    // status 2304), so the untraced rename ran and nothing faulted: neither
    // run's SIGILL rule can be what keeps the change from being named.
    assert_eq!(
        stderr.matches("child wait status 2304").count(),
        2,
        "{rewrite:?}: {stderr}"
    );
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{rewrite:?}: {stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{rewrite:?}: {stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED"), "{stderr}");
}

#[test]
fn a_private_page_made_writable_names_no_host_input_change() {
    assert_a_rewritten_private_page_names_no_host_input_change(
        "host-input-private-page-mprotect",
        host_input::PrivatePageRewrite::Mprotect,
    );
}

#[test]
fn a_page_mapped_over_the_private_page_names_no_host_input_change() {
    assert_a_rewritten_private_page_names_no_host_input_change(
        "host-input-private-page-map-fixed",
        host_input::PrivatePageRewrite::MapFixed,
    );
}

#[test]
fn a_private_page_written_through_proc_self_mem_names_no_host_input_change() {
    assert_a_rewritten_private_page_names_no_host_input_change(
        "host-input-private-page-proc-mem",
        host_input::PrivatePageRewrite::ProcSelfMem,
    );
}

/// hermit-cli hard-codes Reverie's private page as the range Detcore guards
/// (`REVERIE_PTRACE_PRIVATE_PAGE` in run.rs), because reverie-ptrace keeps
/// its constants private. A guest's own memory map under the ptrace backend
/// shows it there, one executable page, so a move in Reverie fails here.
#[test]
fn reverie_private_page_is_the_range_hermit_guards() {
    let output = hermit_test::hermit_output(
        "hermit run",
        Command::new(env!("CARGO_BIN_EXE_hermit")).args([
            "run",
            "--strict",
            "--",
            "/bin/cat",
            "/proc/self/maps",
        ]),
    );
    let maps = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    let page: Vec<&str> = maps
        .lines()
        .filter(|line| line.starts_with("71000000-"))
        .collect();
    assert_eq!(page.len(), 1, "{maps}");
    assert!(page[0].starts_with("71000000-71001000 r-xp "), "{maps}");
}

/// Under `--passthru-opt` every syscall outside Detcore's short allow-list
/// bypasses it, `rename` and `link` among them, so the guest's own rebinding
/// is never reported. The opens still are, and they show the guest's
/// replacement of `F` exactly as a host replacement would. A host input change
/// is therefore never named there: it is reported as unattributed, and the run
/// stays a divergence.
#[test]
fn under_passthru_opt_an_unobserved_rebinding_names_no_host_input_change() {
    let guest = host_input::rebinding_guest(&host_input::python_interpreter());
    let (_root, stderr, report) = verify_across_host_action_passthru(
        "host-input-rebound-passthru",
        &guest,
        false,
        ["go", "go"],
    );
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED"), "{stderr}");
    assert!(
        stderr.contains("HERMIT_HOST_INPUT_CHANGE_UNATTRIBUTED")
            && stderr.contains("--passthru-opt"),
        "{stderr}"
    );
}

/// The same refusal for a genuine host replacement under `--passthru-opt`:
/// Hermit cannot tell it from an unobserved guest rebinding, so it is
/// reported, with the run and the file, and not named as the cause.
#[test]
fn under_passthru_opt_a_host_replacement_is_reported_but_not_named() {
    let (root, stderr, report) = verify_across_host_action_passthru(
        "host-input-replaced-passthru",
        host_input::HOST_INPUT_GUEST,
        true,
        ["go", "go"],
    );
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED"), "{stderr}");
    let unattributed = stderr
        .lines()
        .find(|line| line.starts_with("HERMIT_HOST_INPUT_CHANGE_UNATTRIBUTED"))
        .unwrap_or_else(|| panic!("no unattributed report:\n{stderr}"));
    assert!(unattributed.contains("run 1"), "{unattributed}");
    assert!(
        unattributed.contains(&root.join("F").display().to_string()),
        "{unattributed}"
    );
    assert!(unattributed.contains("--passthru-opt"), "{unattributed}");
}

/// The guest's namespace changes are recorded before they run, whatever their
/// outcome, so that a change the kernel commits is never missing from the
/// record, even when the run ends before Hermit sees the result. A host
/// replacement of `F` in run 1 is therefore not named when the guest also
/// tried to rename onto `F`, though that rename failed and changed nothing.
#[test]
fn a_replacement_where_the_guest_tried_to_rebind_the_path_names_no_host_input_change() {
    let guest = host_input::failed_rename_guest(&host_input::python_interpreter());
    let (_root, stderr, report) =
        verify_across_host_action("host-input-failed-rename", &guest, true, ["go", "go"]);
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGE"), "{stderr}");
}
