// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Shared classification of captured output that shows an environmental block.

use serde::Deserialize;
use serde::Serialize;

/// The closed environmental-block values emitted by this classifier.
///
/// Consumers that turn an environmental observation into a retry decision use
/// this enum rather than matching the rendered string. Adding a classifier arm
/// therefore requires every typed consumer to handle the new value.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "kebab-case")]
pub enum EnvBlockClass {
    BpfjailerBanner,
    ProxyEgress,
    ThirdPartyBuild,
    ToolchainEperm,
    VcsFsDenial,
}

impl EnvBlockClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BpfjailerBanner => "bpfjailer-banner",
            Self::ProxyEgress => "proxy-egress",
            Self::ThirdPartyBuild => "third-party-build",
            Self::ToolchainEperm => "toolchain-eperm",
            Self::VcsFsDenial => "vcs-fs-denial",
        }
    }
}

/// Phrases that mean "the kernel/sandbox said no", in lowercase.
const DENIALS: &[&str] = &[
    "operation not permitted",
    "permission denied",
    "(os error 1)",
];

fn has_denial(line: &str) -> bool {
    DENIALS.iter().any(|d| line.contains(d))
}

fn has_any(line: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| line.contains(n))
}

/// Build-tool / compiler / linker phrasing for a Form-2 denial, in lowercase.
///
/// Split out of the old inline test so the same anchors can be applied per BLOCK
/// (see [`diagnostic_blocks`]) instead of per physical line.
fn has_toolchain_phrase(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("error: failed to write ")
        || trimmed.starts_with("rm: cannot remove ")
        || (line.contains("fatal error: ") && line.matches(':').count() >= 2)
        || line.contains("cmake error")
        || has_any(
            line,
            &[
                "cannot open",
                "error opening",
                "failed to open",
                "could not open",
                "opening dependency file",
                "could not write output to",
                "couldn't create the temp file",
                "can't create",
                "cannot execute",
                "could not create temporary file",
                "failed to build archive at",
                // MEASURED 2026-08-17, five phrasings of one host condition that
                // reached a node's output and were classified as product reds:
                // rustc's incremental link_or_copy step, which says "unable to
                // copy" where the sibling path says "could not write output to";
                "unable to copy",
                // A same-line cargo spawn denial retains the historical
                // classification. Cross-line matching is narrower and handled
                // by `has_cross_line_toolchain_phrase` below.
                "could not execute process",
                // `cp -a`/`ln -s` refusing to make a link, which "can't create"
                // above does not spell the same way.
                "cannot create symbolic link",
                // The rustc and coreutils anchors are handled above as
                // line-prefix checks. They MUST NOT use this substring matcher:
                // guest prose can quote the complete diagnostic text next to a
                // real EPERM, and that must remain a product red.
            ],
        )
}

/// Tool syntax proven safe to join with a denial elsewhere in the same Cargo
/// diagnostic block. Generic phrases such as "could not open" stay same-line
/// only: product prose can contain them above an indented guest EPERM.
fn has_cross_line_toolchain_phrase(line: &str) -> bool {
    line.trim_start().starts_with("could not execute process ")
}

/// Group `lower` into diagnostic blocks: one leading line plus its continuations.
///
/// WHY THIS EXISTS. Form 2 used to require the denial phrase and the build-tool
/// phrase on the SAME physical line. Cargo does not oblige. Measured verbatim:
///
/// ```text
/// error: failed to run custom build command for `libm v0.2.16`
///
/// Caused by:
///   could not execute process .../build-script-build (never executed)
///
/// Caused by:
///   Permission denied (os error 13)
/// ```
///
/// The line carrying the tool phrase has no denial and the line carrying the
/// denial has no tool phrase, so neither line satisfied both and a host denial
/// was recorded as a product failure.
///
/// WHY THIS DOES NOT WEAKEN THE SCOPING RULE. This does NOT widen the search to
/// the whole region — that is what would let an unrelated concurrent denial
/// excuse a real red, and it stays forbidden. A block ends at the next
/// UNINDENTED line, so a jail banner (`An action was blocked on this server…`,
/// unindented) always starts its own block and can never be absorbed into a
/// genuine compiler error's block. A gcc error keeps its indented source excerpt
/// and nothing else. Continuations are: blank lines, indented lines, and the
/// bare `Caused by:` header, which is exactly cargo's own error grouping.
fn diagnostic_blocks(lower: &str) -> Vec<Vec<&str>> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in lower.lines() {
        let trimmed = line.trim_start();
        let continues = trimmed.is_empty()
            || line.starts_with(' ')
            || line.starts_with('\t')
            || trimmed.starts_with("caused by:");
        if continues && !blocks.is_empty() {
            blocks.last_mut().expect("checked non-empty").push(line);
        } else {
            blocks.push(vec![line]);
        }
    }
    blocks
}

/// Classify a failed gate's output as an ENVIRONMENTAL block, naming the class.
///
/// Returns `None` for a genuine product/test failure. Misclassifying a real
/// failure as environmental is as harmful as the reverse, so every form-2
/// (banner-less) anchor is pinned to build-tool / VCS phrasing: ordinary GUEST
/// output that legitimately produces `EPERM` - `DETLOG ... madvise ... EPERM
/// (Operation not permitted)`, the `kcmp-eperm` fixture, a `context: Mount`
/// EPERM - must never trip it. The self-test brackets both directions with
/// counts.
///
/// The classes, and what each is evidenced by:
///
/// * `bpfjailer-banner` - the canonical jail banner. The agent sandbox is a
///   BPFJailer jail inherited by every descendant (`validate -> cargo ->
///   rustc/cmake/cc1/ld`), and its FS/EXEC/NET enforcers transiently deny an
///   `open`/`execve` for reasons unrelated to the code under test.
/// * `toolchain-eperm` - a raw `EPERM`/`EACCES` leaked to a build tool with NO
///   banner: `cc1 fatal error: .../stddef.h: Operation not permitted`, a CMake or
///   linker denial on a system path, `rustc error: could not write output to ...`.
///   Those files are world-readable `root:root -rw-r--r--` on this host, so a
///   *compiler* reporting it cannot open a header for a permission reason is
///   never legitimate product behaviour.
/// * `third-party-build` - the vendored DynamoRIO/elfutils build under
///   `reverie-dbi`. At `nproc=316` an unbounded dependency scan drives elfutils
///   into a concurrency-exposed `SIGABRT`; that is a HOST build flake, not a
///   Hermit defect (Hermit source is not what failed to compile).
/// * `proxy-egress` - **NEW.** Egress through `fwdproxy` failed, so a networked
///   gate could not reach GitHub. Verbatim from
///   `/tmp/hermit-validate.WUrHlJ.log` (2026-08-07 09:37): `Lookup error: git
///   ls-remote https://github.com/rrnewton/reverie.git refs/heads/main failed:
///   fatal: unable to access '...': Could not resolve proxy: fwdproxy`, which the
///   old regex did NOT match - that run recorded a PRODUCT red for the Reverie
///   pin gate (the log contains no `ENVIRONMENTAL block` line) when nothing about
///   the tree was wrong. `CONNECT tunnel failed, response 403` is the same class:
///   the proxy is a per-destination allowlist, so a 403 is an egress verdict, not
///   a Hermit result.
/// * `vcs-fs-denial` - **NEW, defence in depth.** A banner-less git FS denial,
///   e.g. a jail denying `git init`/`git config` inside
///   `check-reverie-pin.rs`'s `/tmp` fixture repository. NOTE, honestly: the one
///   occurrence found on disk (`/tmp/hermit-validate.H61gJP.log:1400-1419`,
///   `Enforcer: FS, Reason: FILE_OPEN` while running `git -C
///   /tmp/check-reverie-pin-stale-lock-... config user.name`) DID carry the
///   jail banner and WAS already classified and retried successfully
///   (`:1499 ENVIRONMENTAL block on attempt 1/3`). So this anchor is not fixing a
///   measured miss; it covers the same denial arriving without the banner, which
///   is how the FS enforcer surfaces when it denies a child that captures its own
///   stderr.
///
/// What ONE output showed about an environmental block, in THREE STATES.
///
/// ⚠️ NOT `EnvBlockVerdict`, WHICH IS A DIFFERENT AXIS. That type settles whether
/// a classification HYPOTHESIS survived a later re-execution (confirmed /
/// refuted / unconfirmed). This one reports what a single captured region
/// ACTUALLY CONTAINED. A run can be `Denied` here and `Refuted` there — observed
/// a denial, then passed on rerun — and the two must not be read as the same fact.
///
/// ⚠️ TWO STATES CANNOT CARRY THREE FACTS, AND THIS IS WHERE THE THIRD GOT LOST.
/// The old `Option<&str>` said `Some(class)` for a denial and `None` for
/// everything else — collapsing "we looked and there was no denial" together with
/// "there was nothing to look at". Those need opposite responses: the first is a
/// real product failure, the second is a run that produced no evidence either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvBlockObservation {
    /// A denial FIRED, and this names its form.
    Denied(EnvBlockClass),
    /// Evidence was present and carried no denial. A product failure here is real.
    NoDenial,
    /// There was nothing to evaluate. NOT a statement that no denial occurred.
    NothingObserved,
}

impl EnvBlockObservation {
    pub fn block_class(self) -> Option<EnvBlockClass> {
        match self {
            Self::Denied(class) => Some(class),
            Self::NoDenial | Self::NothingObserved => None,
        }
    }

    /// The legacy two-state view, for callers that genuinely only ask "was it
    /// blocked". `NothingObserved` maps to `None` — correct for that question,
    /// and the reason the three-state form exists for callers asking anything else.
    pub fn class(self) -> Option<&'static str> {
        self.block_class().map(EnvBlockClass::as_str)
    }
}

/// Preserved two-state entry point. Every existing caller keeps working; anything
/// that must tell "no denial" from "no evidence" calls
/// [`environmental_block_observation`] instead.
pub fn environmental_block_class(output: &str) -> Option<&'static str> {
    environmental_block_observation(output).class()
}

pub fn environmental_block_observation(output: &str) -> EnvBlockObservation {
    // NOTHING TO EVALUATE IS ITS OWN ANSWER. An empty region means the step
    // produced no output, which is not evidence that it ran unblocked.
    if output.trim().is_empty() {
        return EnvBlockObservation::NothingObserved;
    }
    let lower = output.to_ascii_lowercase();
    // Form 1: the canonical jail banner, anywhere in the region.
    //
    // ⚠️ `lower.contains("bpfjailer")` IS DELIBERATELY UNANCHORED, AND I TRIED TO
    // ANCHOR IT AND WAS WRONG. A bare mention does read as DENIED under it —
    // measured on three shapes, including a diagnostic that says "no bpfjailer
    // denial was observed this run" — and a `Denied` verdict is EXCUSING, so a
    // false positive is silent absolution rather than a loud false red. That
    // argument is real but it is not new information: requiring a denial word on
    // the same line breaks a case `self_test` ASSERTS must be accepted,
    //     "[e2e.metadata] Bunnylol `scuba bpfjailer_enforce` for more details"
    // which is a genuine jailer output line carrying no denial word of its own.
    // The unanchored form is a DECLARED TRADE, recorded in the self-test: accept
    // some mentions rather than miss the jailer's own pointer line. Narrowing it
    // needs a replacement that keeps that case, and that is an owner decision
    // about which error to prefer, not a repair.
    if lower.contains("blocked on this server based on a security policy")
        || lower.contains("enforcer: fs, reason:")
        || lower.contains("enforcer: exec, reason:")
        || lower.contains("enforcer: net, reason:")
        || lower.contains("bpfjailer")
    {
        return EnvBlockObservation::Denied(EnvBlockClass::BpfjailerBanner);
    }
    // Form 4 (checked before the per-line scan because it is a whole-region
    // signature): the vendored third-party build script.
    if (lower.contains("failed to run custom build command for") && lower.contains("reverie-dbi"))
        || (lower.contains("panicked at") && lower.contains("reverie-dbi/build.rs"))
    {
        return EnvBlockObservation::Denied(EnvBlockClass::ThirdPartyBuild);
    }
    let mut vcs_hit = false;
    for line in lower.lines() {
        // Form 3 (NEW): egress through the forward proxy failed.
        if has_any(
            line,
            &[
                "could not resolve proxy",
                "could not resolve host",
                "connect tunnel failed, response 403",
                "proxy connect aborted",
                "failed to connect to fwdproxy",
            ],
        ) {
            return EnvBlockObservation::Denied(EnvBlockClass::ProxyEgress);
        }
        // Form 2a: legacy same-line toolchain denial. The conjunction remains
        // same-line so generic product prose cannot borrow a denial from a later
        // indented line in its block.
        if has_denial(line) && has_toolchain_phrase(line) {
            return EnvBlockObservation::Denied(EnvBlockClass::ToolchainEperm);
        }
        // Form 5 (NEW): a banner-less git FS denial. Requires BOTH a git-fatal
        // shape and a denial on the same line, so a guest test that merely prints
        // "permission denied" cannot trip it.
        if (line.starts_with("fatal:") || line.contains(" fatal: ") || line.contains(".git/"))
            && has_any(
                line,
                &[
                    "cannot mkdir",
                    "could not create work tree dir",
                    "could not create leading directories",
                    "unable to create",
                    "unable to write",
                    "unable to access",
                    "could not lock",
                    "chmod on",
                    ".git/",
                ],
            )
        {
            vcs_hit = true;
        }
    }
    // Form 2b: the measured Cargo spawn diagnostic puts its denial in a
    // continuation line. Only that structural phrase may join evidence across
    // one diagnostic block; generic legacy phrases remain same-line above.
    if diagnostic_blocks(&lower).iter().any(|block| {
        block.iter().any(|line| has_denial(line))
            && block
                .iter()
                .any(|line| has_cross_line_toolchain_phrase(line))
    }) {
        return EnvBlockObservation::Denied(EnvBlockClass::ToolchainEperm);
    }
    if vcs_hit {
        return EnvBlockObservation::Denied(EnvBlockClass::VcsFsDenial);
    }
    EnvBlockObservation::NoDenial
}
