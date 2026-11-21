/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Launch preflight for the host tools that selected host nodes need
//! (dh-63c2n3, option C).
//!
//! It runs inside the validation unit, after plan and cache returns and before
//! any graph execution, where it sees the exact selection (focused, `--only`,
//! `--tests`, selective, Buck) and the PATH the nodes inherit. ci-hub admission
//! sees only the launcher's environment, and a run once died because the
//! launcher's PATH and the node's PATH differed
//! (dev-hermit ci-hub/validate/e2e_runner_default.py).
//!
//! Scope: the host-side nodes in the selection that prepare guests, the
//! host `test-harness build` nodes and the Buck stage. A pinned-root node
//! prepares in the image, which carries its own tools. The tools a preparing
//! node needs are the manifest `requires:` tokens of the prepared-program
//! cells it prepares, resolved through [`TOOL_PROBES`], plus the needs no
//! manifest states ([`NAMED_HOST_PREPARERS`]).
//!
//! A missing tool REFUSES the run, naming each missing token and every
//! candidate it tried: COULD_NOT_RUN, never green, never a receipt or a cache
//! entry. It never removes a node and is never a HOST-INAPPLICABLE skip: the
//! plan is read, not edited.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use dagrun::DagConfig;
use dagrun::model::Step;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::Selection;

use crate::COULD_NOT_RUN_EXIT_CODE;
use crate::RUST_SCRIPT_COMMAND_PREFIX;
use crate::RunSummary;

const PINNED_ROOT_WRAPPER: &str = "./ci/hermetic/run-in-pinned-root.sh ";
const HOST_GUEST_BUILD: &str = "target/debug/test-harness build ";
const HARNESS_RUN: &str = "target/debug/test-harness run ";

/// Why a token is probed: an interpreter runs the guest's own program, so the
/// guest's `--prepare` must find it; a tool or compiler may be needed only to
/// run a guest, but some guest declaring it needs it to prepare.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolKind {
    Interpreter,
    Tool,
}

/// A manifest `requires:` token that names a host executable, and the
/// candidates any one of which satisfies it, exactly as the guests' own
/// preparation resolves them: a bare name is looked up on the node's PATH, an
/// absolute path is tested where it is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolProbe {
    pub(crate) token: &'static str,
    pub(crate) kind: ToolKind,
    pub(crate) candidates: &'static [&'static str],
}

/// The token-to-probe table. `requires:` is a capability vocabulary, not a list
/// of executables (`REQUIRES_VOCABULARY` in ci/manifest-plan/src/runner.rs):
/// `lua5.4` is satisfied by `lua5.4` or `lua`, as
/// tests/e2e/language-runtimes/lua-random.sh accepts either, and `cxx` by
/// `c++`. python3, git and node guests test absolute paths
/// (python-*.sh: `/usr/bin/python3`; git-repository-workflow.sh:
/// `/usr/bin/git`; node-v8-jit.sh: `/bin/node`, `/usr/bin/node`,
/// `/usr/bin/nodejs`), so their candidates are those paths. The `.c` and `.rs`
/// guests are compiled by the harness itself with `cc` and `rustc` from PATH
/// (runner.rs, `prepare_test_until`).
pub(crate) const TOOL_PROBES: &[ToolProbe] = &[
    ToolProbe {
        token: "ar",
        kind: ToolKind::Tool,
        candidates: &["ar"],
    },
    ToolProbe {
        token: "bash",
        kind: ToolKind::Interpreter,
        candidates: &["bash"],
    },
    ToolProbe {
        token: "cc",
        kind: ToolKind::Tool,
        candidates: &["cc"],
    },
    ToolProbe {
        token: "cxx",
        kind: ToolKind::Tool,
        candidates: &["c++"],
    },
    ToolProbe {
        token: "date",
        kind: ToolKind::Tool,
        candidates: &["date"],
    },
    ToolProbe {
        token: "du",
        kind: ToolKind::Tool,
        candidates: &["du"],
    },
    ToolProbe {
        token: "find",
        kind: ToolKind::Tool,
        candidates: &["find"],
    },
    ToolProbe {
        token: "gawk",
        kind: ToolKind::Interpreter,
        candidates: &["gawk"],
    },
    ToolProbe {
        token: "git",
        kind: ToolKind::Tool,
        candidates: &["/usr/bin/git"],
    },
    ToolProbe {
        token: "hexdump",
        kind: ToolKind::Tool,
        candidates: &["hexdump"],
    },
    ToolProbe {
        token: "jq",
        kind: ToolKind::Tool,
        candidates: &["jq"],
    },
    ToolProbe {
        token: "lua5.4",
        kind: ToolKind::Interpreter,
        candidates: &["lua5.4", "lua"],
    },
    ToolProbe {
        token: "m4",
        kind: ToolKind::Interpreter,
        candidates: &["m4"],
    },
    ToolProbe {
        token: "node",
        kind: ToolKind::Interpreter,
        candidates: &["/bin/node", "/usr/bin/node", "/usr/bin/nodejs"],
    },
    ToolProbe {
        token: "openssl",
        kind: ToolKind::Tool,
        candidates: &["openssl"],
    },
    ToolProbe {
        token: "perl",
        kind: ToolKind::Interpreter,
        candidates: &["perl"],
    },
    ToolProbe {
        token: "python3",
        kind: ToolKind::Interpreter,
        candidates: &["/usr/bin/python3"],
    },
    ToolProbe {
        token: "ruby",
        kind: ToolKind::Interpreter,
        candidates: &["ruby"],
    },
    ToolProbe {
        token: "rustc",
        kind: ToolKind::Tool,
        candidates: &["rustc"],
    },
    ToolProbe {
        token: "sqlite3",
        kind: ToolKind::Tool,
        candidates: &["sqlite3"],
    },
    ToolProbe {
        token: "tclsh",
        kind: ToolKind::Interpreter,
        candidates: &["tclsh"],
    },
    ToolProbe {
        token: "zstd",
        kind: ToolKind::Tool,
        candidates: &["zstd"],
    },
];

/// The `requires:` tokens that name no executable, each with what it names
/// instead. Listed by name in every preflight's output; never probed and never
/// passed silently.
pub(crate) const NO_PROBE_TOKENS: &[(&str, &str)] = &[
    (
        "cpuid",
        "a host capability (CPUID faulting), decided by the host-capability step",
    ),
    ("kvm", "a host capability (/dev/kvm)"),
    ("linux", "the host operating system"),
    ("ptrace", "a kernel facility"),
    ("userns", "a kernel facility (user namespaces)"),
    ("vdso-getrandom", "a kernel vDSO export (Linux 6.11+)"),
    ("x86_64", "the host architecture"),
];

/// A host node that prepares guests although it runs no `test-harness build`
/// of its own: the lanes it prepares and the tools no manifest states.
pub(crate) struct NamedHostPreparer {
    pub(crate) tag: &'static str,
    pub(crate) lanes: &'static [&'static str],
    pub(crate) tools: &'static [ToolProbe],
}

/// The Buck stage prepares both lanes' guests on the host (ci/buck-e2e/stage,
/// `test-harness build --ci-only --lane <lane>` for portable and privileged)
/// and sets the staged Hermit's rpath with patchelf.
pub(crate) const NAMED_HOST_PREPARERS: &[NamedHostPreparer] = &[NamedHostPreparer {
    tag: hermit_manifest_plan::validation_dag::BUCK_STAGE_TAG,
    lanes: &["portable", "privileged"],
    tools: &[ToolProbe {
        token: "patchelf",
        kind: ToolKind::Tool,
        candidates: &["patchelf"],
    }],
}];

/// Every way the table can disagree with the `requires:` vocabulary: a token
/// in neither list, in both, in a list but not the vocabulary, or a probe with
/// no candidate. The table's own test runs it; at run time an unlisted token a
/// selected node needs refuses as [`Resolution::Unknown`].
#[cfg(test)]
pub(crate) fn table_coverage_errors(
    vocabulary: &[&str],
    probes: &[ToolProbe],
    no_probe: &[(&str, &str)],
) -> Vec<String> {
    let mut errors = Vec::new();
    let probed = probes
        .iter()
        .map(|probe| probe.token)
        .collect::<BTreeSet<_>>();
    let unprobed = no_probe
        .iter()
        .map(|(token, _)| *token)
        .collect::<BTreeSet<_>>();
    if probed.len() != probes.len() || unprobed.len() != no_probe.len() {
        errors.push("a token is listed twice in one table".to_string());
    }
    for token in vocabulary {
        match (probed.contains(token), unprobed.contains(token)) {
            (false, false) => errors.push(format!(
                "requires token {token} is in neither the probe table nor the no-probe class"
            )),
            (true, true) => errors.push(format!(
                "requires token {token} is in both the probe table and the no-probe class"
            )),
            _ => {}
        }
    }
    for token in probed.iter().chain(unprobed.iter()) {
        if !vocabulary.contains(token) {
            errors.push(format!("{token} is not a requires token"));
        }
    }
    for probe in probes {
        if probe.candidates.is_empty() {
            errors.push(format!("{} lists no candidate", probe.token));
        }
    }
    errors
}

/// Whether `path` names an executable regular file, following symlinks.
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// The first candidate that resolves: an absolute candidate where it is, a
/// bare name on `path` in order.
pub(crate) fn resolve(candidates: &[&str], path: &OsStr) -> Option<PathBuf> {
    for candidate in candidates {
        if candidate.contains('/') {
            let absolute = PathBuf::from(candidate);
            if is_executable(&absolute) {
                return Some(absolute);
            }
            continue;
        }
        for dir in std::env::split_paths(path) {
            let found = dir.join(candidate);
            if !dir.as_os_str().is_empty() && is_executable(&found) {
                return Some(found);
            }
        }
    }
    None
}

/// One selected host node that prepares guests, and what it needs.
#[derive(Clone, Debug)]
pub(crate) struct NodeNeeds {
    pub(crate) tag: String,
    /// The PATH the node inherits, as its command will see it.
    pub(crate) path: OsString,
    /// Manifest tokens of the prepared-program cells it prepares.
    pub(crate) tokens: BTreeSet<String>,
    /// Needs no manifest states.
    pub(crate) tools: Vec<ToolProbe>,
}

/// The lane a host `test-harness build` command prepares, or `None` when the
/// command runs no such build. Any flag other than the committed build's
/// fails closed: its population would be unaccounted.
fn host_guest_build_lane(cmd: &str) -> Result<Option<String>, String> {
    let Some((_, rest)) = cmd.split_once(HOST_GUEST_BUILD) else {
        return Ok(None);
    };
    let words = rest.split_whitespace().collect::<Vec<_>>();
    let mut lane = None;
    let mut ci_only = false;
    let mut index = 0;
    while index < words.len() {
        match words[index] {
            "--lane" => {
                let value = words
                    .get(index + 1)
                    .ok_or_else(|| format!("host guest build without a lane value: {cmd}"))?;
                if lane.replace(value.to_string()).is_some() {
                    return Err(format!("host guest build names --lane twice: {cmd}"));
                }
                index += 1;
            }
            "--ci-only" => ci_only = true,
            "--allow-empty" => {}
            "--jobs" => index += 1,
            other => {
                return Err(format!(
                    "host guest build passes {other}, whose prepared population the host-tool preflight cannot account: {cmd}"
                ));
            }
        }
        index += 1;
    }
    match (lane, ci_only) {
        (Some(lane), true) => Ok(Some(lane)),
        _ => Err(format!(
            "host guest build without --lane and --ci-only, whose prepared population the host-tool preflight cannot account: {cmd}"
        )),
    }
}

/// The PATH `step` inherits: its own `PATH` if it sets one, else this
/// process's, with ci/rust-script-bin in front when its command prepends it.
pub(crate) fn inherited_path(root: &Path, step: &Step) -> OsString {
    let base = step
        .env
        .get("PATH")
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    if !step.cmd.starts_with(RUST_SCRIPT_COMMAND_PREFIX) {
        return base;
    }
    let mut dirs = vec![root.join("ci/rust-script-bin")];
    dirs.extend(std::env::split_paths(&base));
    std::env::join_paths(dirs).unwrap_or(base)
}

/// The selected host nodes that prepare guests. Lanes are resolved against the
/// manifests only when some node prepares any, so a selection with none never
/// reads them.
pub(crate) fn host_tool_needs(
    root: &Path,
    cfgs: &[&DagConfig],
    path_of: &dyn Fn(&Step) -> OsString,
) -> Result<Vec<NodeNeeds>, String> {
    let mut preparers = Vec::new();
    for step in cfgs.iter().flat_map(|cfg| cfg.steps.iter()) {
        if step.skip_reason.is_some() || step.cmd.starts_with(PINNED_ROOT_WRAPPER) {
            continue;
        }
        let tag = step.tag();
        if step.cmd.contains(HARNESS_RUN)
            && !step.cmd.contains("--prebuilt")
            && !step.cmd.contains("E2E_IMPORT_RESULTS=")
        {
            return Err(format!(
                "host node {tag} runs test-harness without --prebuilt, so it prepares guests in-node; the host-tool preflight cannot account its tools"
            ));
        }
        let mut lanes = Vec::new();
        let mut tools = Vec::new();
        if let Some(lane) = host_guest_build_lane(&step.cmd)? {
            lanes.push(lane);
        }
        if let Some(named) = NAMED_HOST_PREPARERS.iter().find(|named| named.tag == tag) {
            lanes.extend(named.lanes.iter().map(|lane| lane.to_string()));
            tools.extend(named.tools.iter().copied());
        }
        if lanes.is_empty() && tools.is_empty() {
            continue;
        }
        preparers.push((step, lanes, tools));
    }
    if preparers.is_empty() {
        return Ok(Vec::new());
    }
    let manifests = ManifestSet::load(root)?;
    let mut by_lane = BTreeMap::<String, BTreeSet<String>>::new();
    let mut needs = Vec::new();
    for (step, lanes, tools) in preparers {
        let mut tokens = BTreeSet::new();
        for lane in lanes {
            if !by_lane.contains_key(&lane) {
                let cells = manifests.select(&Selection {
                    lane: Some(lane.clone()),
                    population: Some(Population::Required),
                    ..Selection::default()
                })?;
                let lane_tokens = cells
                    .iter()
                    .filter(|cell| cell.test.program.is_some())
                    .flat_map(|cell| cell.test.requires.iter().cloned())
                    .collect();
                by_lane.insert(lane.clone(), lane_tokens);
            }
            tokens.extend(by_lane[&lane].iter().cloned());
        }
        needs.push(NodeNeeds {
            tag: step.tag(),
            path: path_of(step),
            tokens,
            tools,
        });
    }
    Ok(needs)
}

/// How one needed token resolved on one node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Resolution {
    Found(PathBuf),
    Missing(Vec<String>),
    NoProbe,
    /// In neither list: the table's own test refuses this at commit time.
    Unknown,
}

pub(crate) fn resolve_token(token: &str, probes: &[ToolProbe], path: &OsStr) -> Resolution {
    if let Some(probe) = probes.iter().find(|probe| probe.token == token) {
        return match resolve(probe.candidates, path) {
            Some(found) => Resolution::Found(found),
            None => Resolution::Missing(probe.candidates.iter().map(|c| c.to_string()).collect()),
        };
    }
    if NO_PROBE_TOKENS.iter().any(|(name, _)| *name == token) {
        return Resolution::NoProbe;
    }
    Resolution::Unknown
}

/// What the preflight concluded: its printed lines, and the refusal detail
/// when anything a selected node needs is missing or unknown.
#[derive(Debug)]
pub(crate) struct PreflightVerdict {
    pub(crate) lines: Vec<String>,
    pub(crate) missing: Vec<String>,
}

fn kind_of(token: &str, probes: &[ToolProbe]) -> &'static str {
    match probes.iter().find(|probe| probe.token == token) {
        Some(ToolProbe {
            kind: ToolKind::Interpreter,
            ..
        }) => "interpreter",
        _ => "tool",
    }
}

pub(crate) fn evaluate(
    needs: &[NodeNeeds],
    probes: &[ToolProbe],
    selected: &BTreeSet<String>,
) -> PreflightVerdict {
    let mut lines = vec![match needs.len() {
        0 => "Host-tool preflight: 0 selected host nodes declare tool needs".to_string(),
        n => format!("Host-tool preflight: {n} selected host node(s) declare tool needs"),
    }];
    let mut missing = Vec::new();
    for node in needs {
        let mut found = Vec::new();
        for (token, resolution) in node
            .tokens
            .iter()
            .map(|token| (token.clone(), resolve_token(token, probes, &node.path)))
            .chain(node.tools.iter().map(|tool| {
                (
                    tool.token.to_string(),
                    resolve_token(tool.token, std::slice::from_ref(tool), &node.path),
                )
            }))
        {
            match resolution {
                Resolution::Found(path) => found.push(format!("{token}={}", path.display())),
                Resolution::Missing(tried) => missing.push(format!(
                    "{} needs {} {token}: none of [{}] is on the PATH it inherits",
                    node.tag,
                    kind_of(&token, probes),
                    tried.join(", ")
                )),
                Resolution::NoProbe => {}
                Resolution::Unknown => missing.push(format!(
                    "{} needs {token}, which is in neither the probe table nor the no-probe class",
                    node.tag
                )),
            }
        }
        lines.push(format!("  {}: found {}", node.tag, found.join(", ")));
    }
    lines.extend(missing.iter().map(|line| format!("  MISSING: {line}")));
    lines.push(format!(
        "Host-tool preflight: never probed, by name (they name no executable): {}",
        NO_PROBE_TOKENS
            .iter()
            .map(|(token, what)| format!("{token} ({what})"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let compat = hermit_manifest_plan::validation_dag::HOST_MANIFEST_RUNS;
    let selected_compat = compat
        .iter()
        .filter(|tag| selected.contains(**tag))
        .copied()
        .collect::<Vec<_>>();
    lines.push(format!(
        "Host-tool preflight: does not probe the host compat run types {} (their host needs lie outside the manifest requires vocabulary); selected here: {}",
        compat.join(", "),
        if selected_compat.is_empty() {
            "none".to_string()
        } else {
            selected_compat.join(", ")
        }
    ));
    PreflightVerdict { lines, missing }
}

pub(crate) fn refusal(profile: &str, missing: &[String]) -> Box<RunSummary> {
    let mut why = vec![format!(
        "{} tool(s) a selected host node needs are missing; each line names the token and every candidate tried:",
        missing.len()
    )];
    why.extend(missing.iter().map(|line| format!("  {line}")));
    refused(profile, why)
}

/// The refusal when the preflight cannot work out what the selected host
/// nodes need ([`host_tool_needs`] failed: a host node whose preparation it
/// cannot account, or manifests it cannot read). Nothing was probed, so it
/// names no missing tool.
pub(crate) fn planning_refusal(profile: &str, error: &str) -> Box<RunSummary> {
    refused(
        profile,
        vec![format!(
            "the host-tool preflight could not plan the selected host nodes: {error}"
        )],
    )
}

fn refused(profile: &str, mut why: Vec<String>) -> Box<RunSummary> {
    why.push(
        "No DAG node or product test was executed; no node was removed and none was recorded as host-inapplicable; no passing receipt was produced."
            .into(),
    );
    let mut summary = RunSummary::refused(
        COULD_NOT_RUN_EXIT_CODE,
        profile,
        "the host-tool preflight",
        why,
    );
    summary.executed_tests = Some(0);
    summary.passed_tests = Some(0);
    summary.diagnostic_failed_tests = Some(0);
    summary.skipped_tests = Some(0);
    Box::new(summary)
}

/// The preflight over the bound selection: print what it found, refuse on
/// anything missing, and refuse under its own header when it cannot plan.
pub(crate) fn admit(
    root: &Path,
    cfg: &DagConfig,
    second: Option<&DagConfig>,
    profile: &str,
) -> Result<(), Box<RunSummary>> {
    let cfgs = std::iter::once(cfg).chain(second).collect::<Vec<_>>();
    let selected = cfgs
        .iter()
        .flat_map(|cfg| cfg.steps.iter().map(Step::tag))
        .collect::<BTreeSet<_>>();
    let needs = host_tool_needs(root, &cfgs, &|step| inherited_path(root, step))
        .map_err(|error| planning_refusal(profile, &error))?;
    let verdict = evaluate(&needs, TOOL_PROBES, &selected);
    for line in &verdict.lines {
        println!("{line}");
    }
    if verdict.missing.is_empty() {
        Ok(())
    } else {
        Err(refusal(profile, &verdict.missing))
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    use hermit_manifest_plan::runner::REQUIRES_VOCABULARY;
    use hermit_manifest_plan::service_result::FinalValidateStatus;

    use super::*;

    fn root() -> PathBuf {
        let root = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("scripts/lib/validate_host_tools.rs sits two levels below the root")
            .to_path_buf();
        assert!(
            root.join("ci/dag/validate.json").is_file(),
            "{}",
            root.display()
        );
        root
    }

    fn vocabulary() -> Vec<&'static str> {
        REQUIRES_VOCABULARY
            .iter()
            .map(|(token, _)| *token)
            .collect()
    }

    #[test]
    fn table_covers_the_requires_vocabulary_exactly() {
        let errors = table_coverage_errors(&vocabulary(), TOOL_PROBES, NO_PROBE_TOKENS);
        assert!(errors.is_empty(), "{errors:#?}");
        // RED CONTROL: a token in neither list fails the table's own test.
        let mut extended = vocabulary();
        extended.push("frobnicate");
        let errors = table_coverage_errors(&extended, TOOL_PROBES, NO_PROBE_TOKENS);
        assert_eq!(
            errors,
            ["requires token frobnicate is in neither the probe table nor the no-probe class"]
        );
        // RED CONTROL: a probed name the vocabulary does not have.
        let mut probes = TOOL_PROBES.to_vec();
        probes.push(ToolProbe {
            token: "lua",
            kind: ToolKind::Interpreter,
            candidates: &["lua"],
        });
        let errors = table_coverage_errors(&vocabulary(), &probes, NO_PROBE_TOKENS);
        assert_eq!(errors, ["lua is not a requires token"]);
        // The incident's two interpreters, with the alternates their guests
        // accept.
        let lua = TOOL_PROBES.iter().find(|p| p.token == "lua5.4").unwrap();
        assert_eq!(lua.candidates, ["lua5.4", "lua"]);
        let ruby = TOOL_PROBES.iter().find(|p| p.token == "ruby").unwrap();
        assert_eq!(ruby.candidates, ["ruby"]);
    }

    /// Scratch directories for stub PATHs, removed on drop.
    struct Scratch(tempfile::TempDir);

    impl Scratch {
        fn new() -> Self {
            Scratch(tempfile::tempdir().unwrap())
        }

        /// A directory holding a symlink to every executable name on the real
        /// PATH (first occurrence wins), except `excluded`.
        fn shadow(&self, name: &str, excluded: &[&str]) -> PathBuf {
            let dir = self.0.path().join(format!("shadow-{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut seen = BTreeSet::new();
            let real = std::env::var_os("PATH").unwrap_or_default();
            for source in std::env::split_paths(&real) {
                let Ok(entries) = std::fs::read_dir(&source) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let file_name = entry.file_name();
                    let Some(text) = file_name.to_str() else {
                        continue;
                    };
                    if excluded.contains(&text) || !seen.insert(text.to_string()) {
                        continue;
                    }
                    let _ = std::os::unix::fs::symlink(entry.path(), dir.join(&file_name));
                }
            }
            dir
        }

        /// A directory holding only `name`: a fake executable that exits 0
        /// when `real` is false, else a symlink to the real one on PATH.
        fn alone(&self, name: &str, real: bool) -> Option<PathBuf> {
            let dir = self.0.path().join(format!(
                "alone-{name}-{}",
                if real { "real" } else { "fake" }
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let target = dir.join(name);
            if real {
                let source = resolve(&[name], &std::env::var_os("PATH").unwrap_or_default())?;
                std::os::unix::fs::symlink(source, &target).unwrap();
            } else {
                std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Some(dir)
        }

        fn fixture(&self, label: &str) -> PathBuf {
            let dir = self.0.path().join(format!("fixture-{label}"));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    }

    fn join(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    /// Run `program --prepare` as the harness does (from the root, with the
    /// cell's fixture and home directories), under `path`. Returns whether it
    /// exited 0, and its stderr.
    fn prepare(root: &Path, program: &str, path: &OsStr, fixture: &Path) -> (bool, String) {
        for name in ["fixtures", "home", "tmp", "xdg-config"] {
            std::fs::create_dir_all(fixture.join(name)).unwrap();
        }
        let mut child = Command::new(root.join(program))
            .arg("--prepare")
            .current_dir(root)
            .env("PATH", path)
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env("HOME", fixture.join("home"))
            .env("XDG_CONFIG_HOME", fixture.join("xdg-config"))
            .env("E2E_TMPDIR", fixture.join("tmp"))
            .env("E2E_FIXTURE_DIR", fixture.join("fixtures"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("cannot start {program}: {error}"));
        // A WALL bound on the child, polled so a hung --prepare fails the
        // test instead of hanging it.
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{program} --prepare exceeded its 120 s WALL bound");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        (status.success(), stderr)
    }

    /// GREEN and RED controls of the incident: with only `lua` on PATH,
    /// lua-random's --prepare and the preflight both pass; with neither Lua
    /// interpreter, both refuse, and the preflight names lua5.4 and lua.
    #[test]
    fn lua_alternate_green_and_red_controls() {
        let root = root();
        let scratch = Scratch::new();
        let shadow = scratch.shadow("lua", &["lua5.4", "lua"]);
        let only_lua = scratch.alone("lua", false).unwrap();
        let program = "tests/e2e/language-runtimes/lua-random.sh";
        let node = |path: OsString| NodeNeeds {
            tag: "build.manifest_guests".into(),
            path,
            tokens: BTreeSet::from(["lua5.4".to_string(), "linux".to_string()]),
            tools: Vec::new(),
        };

        let green = join(&[&only_lua, &shadow]);
        let (prepared, stderr) = prepare(&root, program, &green, &scratch.fixture("green"));
        assert!(prepared, "lua-random refused a PATH with lua: {stderr}");
        let verdict = evaluate(&[node(green)], TOOL_PROBES, &BTreeSet::new());
        assert!(verdict.missing.is_empty(), "{:?}", verdict.lines);

        let red = join(&[&shadow]);
        let (prepared, stderr) = prepare(&root, program, &red, &scratch.fixture("red"));
        assert!(!prepared, "lua-random accepted a PATH with neither Lua");
        assert!(stderr.contains("tried: lua5.4, lua"), "{stderr}");
        let verdict = evaluate(&[node(red)], TOOL_PROBES, &BTreeSet::new());
        assert_eq!(
            verdict.missing,
            [
                "build.manifest_guests needs interpreter lua5.4: none of [lua5.4, lua] is on the PATH it inherits"
            ]
        );
        let summary = refusal("hosted-portable", &verdict.missing);
        assert_eq!(summary.verdict, crate::Verdict::Refused);
        assert_eq!(summary.exit_code, COULD_NOT_RUN_EXIT_CODE);
        assert_eq!(
            crate::final_validate_status(summary.verdict),
            Some(FinalValidateStatus::CouldNotRun)
        );
        assert!(
            summary
                .detail
                .iter()
                .any(|line| line.contains("lua5.4") && line.contains("lua]")),
            "{:?}",
            summary.detail
        );
    }

    /// The guests of each interpreter token: CI-required cells whose program is
    /// a script the harness runs with `--prepare`.
    fn interpreter_guests(root: &Path) -> BTreeMap<&'static str, BTreeSet<String>> {
        let cells = ManifestSet::load(root)
            .unwrap()
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        let mut guests = BTreeMap::<&'static str, BTreeSet<String>>::new();
        for probe in TOOL_PROBES
            .iter()
            .filter(|probe| probe.kind == ToolKind::Interpreter)
        {
            for cell in &cells {
                if let Some(program) = &cell.test.program {
                    if program.ends_with(".sh")
                        && cell.test.requires.iter().any(|token| token == probe.token)
                    {
                        guests
                            .entry(probe.token)
                            .or_default()
                            .insert(program.clone());
                    }
                }
            }
        }
        guests
    }

    /// What the agreement check found.
    #[derive(Debug, Default)]
    struct Agreement {
        guests: BTreeSet<String>,
        alternates: usize,
        /// Tokens whose candidates are absolute paths, which a stub PATH
        /// cannot hide: checked against this host's filesystem only.
        absolute: BTreeSet<&'static str>,
        /// Guests whose preparation passes with an empty PATH (an absolute
        /// #! interpreter): checked against this host's filesystem only.
        path_independent: BTreeSet<String>,
        disagreements: Vec<String>,
    }

    /// Run each interpreter token's guests' --prepare under stub PATHs: with
    /// each single alternate alone (the union of `probes`' and `also`'s
    /// candidates for the token) prepare and the preflight must agree, and
    /// with none, both must refuse.
    fn agreement(
        root: &Path,
        probes: &[ToolProbe],
        also: &[ToolProbe],
        guests: &BTreeMap<&'static str, BTreeSet<String>>,
        scratch: &Scratch,
    ) -> Agreement {
        let mut found = Agreement::default();
        for (token, programs) in guests {
            let listed = probes
                .iter()
                .chain(also)
                .filter(|probe| probe.token == *token)
                .flat_map(|probe| probe.candidates.iter().copied())
                .collect::<BTreeSet<_>>();
            let path_names = listed
                .iter()
                .copied()
                .filter(|candidate| !candidate.contains('/'))
                .collect::<Vec<_>>();
            if path_names.is_empty() {
                // Absolute candidates: agree on this host's filesystem.
                found.absolute.insert(token);
                let real = std::env::var_os("PATH").unwrap_or_default();
                let preflight = matches!(resolve_token(token, probes, &real), Resolution::Found(_));
                for program in programs {
                    let (prepared, stderr) = prepare(
                        root,
                        program,
                        &real,
                        &scratch.fixture(&format!("abs-{token}")),
                    );
                    found.guests.insert(program.clone());
                    if prepared != preflight {
                        found.disagreements.push(format!(
                            "{program} --prepare {} on this host but the preflight {} {token}: {stderr}",
                            if prepared { "passes" } else { "fails" },
                            if preflight { "finds" } else { "misses" }
                        ));
                    }
                }
                continue;
            }
            let shadow = scratch.shadow(token, &path_names);
            for program in programs {
                found.guests.insert(program.clone());
                for candidate in &path_names {
                    found.alternates += 1;
                    let mut accepted = false;
                    let mut last = String::new();
                    for real in [false, true] {
                        let Some(alone) = scratch.alone(candidate, real) else {
                            continue;
                        };
                        let path = join(&[&alone, &shadow]);
                        let (prepared, stderr) = prepare(
                            root,
                            program,
                            &path,
                            &scratch.fixture(&format!("{token}-{candidate}-{real}")),
                        );
                        last = stderr;
                        if prepared {
                            accepted = true;
                            let preflight = resolve_token(token, probes, &path);
                            if !matches!(preflight, Resolution::Found(_)) {
                                found.disagreements.push(format!(
                                    "{program} --prepare accepts {candidate} alone, but the preflight finds no {token} ({preflight:?})"
                                ));
                            }
                            break;
                        }
                    }
                    if !accepted {
                        found.disagreements.push(format!(
                            "{program} --prepare does not accept {candidate} alone, an alternate the table lists for {token}: {last}"
                        ));
                    }
                }
                let none = join(&[&shadow]);
                let (prepared, _) = prepare(
                    root,
                    program,
                    &none,
                    &scratch.fixture(&format!("{token}-none")),
                );
                let preflight = resolve_token(token, probes, &none);
                if !matches!(preflight, Resolution::Missing(_)) {
                    found.disagreements.push(format!(
                        "with none of {path_names:?} on PATH, the preflight resolves {token} as {preflight:?}; it must refuse"
                    ));
                }
                if prepared {
                    // A guest whose preparation passes even with an EMPTY PATH
                    // resolves nothing through PATH (an absolute #! line), so
                    // a stub PATH cannot hide its interpreter: it is named and
                    // checked on this host's filesystem instead. Any guest
                    // that needs PATH at all and still passes here disagrees.
                    let empty = scratch.fixture(&format!("{token}-empty-path"));
                    let (independent, _) = prepare(
                        root,
                        program,
                        empty.as_os_str(),
                        &scratch.fixture(&format!("{token}-empty")),
                    );
                    if !independent {
                        found.disagreements.push(format!(
                            "with none of {path_names:?} on PATH, {program} --prepare passes and the preflight refuses {token}; both must refuse"
                        ));
                    } else {
                        found.path_independent.insert(program.clone());
                        let real = std::env::var_os("PATH").unwrap_or_default();
                        let (prepared, stderr) = prepare(
                            root,
                            program,
                            &real,
                            &scratch.fixture(&format!("{token}-real")),
                        );
                        let preflight =
                            matches!(resolve_token(token, probes, &real), Resolution::Found(_));
                        if prepared != preflight {
                            found.disagreements.push(format!(
                                "{program} --prepare {} on this host but the preflight {} {token}: {stderr}",
                                if prepared { "passes" } else { "fails" },
                                if preflight { "finds" } else { "misses" }
                            ));
                        }
                    }
                }
            }
        }
        found
    }

    /// [v3 R6] The table agrees with the guests by RUNNING each interpreter
    /// token guest's --prepare, never by reading its script. Fails when it
    /// checks no guest, when the lua alternate is dropped from the table, and
    /// when a guest's --prepare stops accepting an alternate the table lists.
    #[test]
    fn table_agrees_with_each_interpreter_guests_prepare() {
        let root = root();
        let guests = interpreter_guests(&root);
        let scratch = Scratch::new();
        let found = agreement(&root, TOOL_PROBES, &[], &guests, &scratch);
        eprintln!(
            "table agreement: {} interpreter-token guest(s), {} alternate(s) stub-checked; absolute-path candidates checked on this host's filesystem only: {:?}; guests resolving nothing through PATH (absolute #! line), checked on this host's filesystem only: {:?}; tools and compilers probed but not agreement-tested: {:?}",
            found.guests.len(),
            found.alternates,
            found.absolute,
            found.path_independent,
            TOOL_PROBES
                .iter()
                .filter(|probe| probe.kind == ToolKind::Tool)
                .map(|probe| probe.token)
                .collect::<Vec<_>>()
        );
        assert!(found.disagreements.is_empty(), "{:#?}", found.disagreements);
        assert!(!found.guests.is_empty(), "checked zero guests");
        assert!(found.alternates > 0, "checked zero alternates");
        assert!(
            found
                .guests
                .contains("tests/e2e/language-runtimes/lua-random.sh")
        );
        assert!(
            found
                .guests
                .contains("tests/e2e/language-runtimes/ruby-random.sh")
        );

        // RED CONTROL: zero guests is a failure, not a vacuous pass.
        let none = agreement(&root, TOOL_PROBES, &[], &BTreeMap::new(), &scratch);
        assert!(none.guests.is_empty());

        // RED CONTROL: the table with the lua alternate dropped. lua-random
        // still accepts `lua` alone, and the mutated table does not.
        let lua_only = TOOL_PROBES
            .iter()
            .map(|probe| {
                if probe.token == "lua5.4" {
                    ToolProbe {
                        candidates: &["lua5.4"],
                        ..*probe
                    }
                } else {
                    *probe
                }
            })
            .collect::<Vec<_>>();
        let lua_guests = BTreeMap::from([(
            "lua5.4",
            BTreeSet::from(["tests/e2e/language-runtimes/lua-random.sh".to_string()]),
        )]);
        let dropped = agreement(&root, &lua_only, TOOL_PROBES, &lua_guests, &scratch);
        assert!(
            dropped
                .disagreements
                .iter()
                .any(|line| line.contains("accepts lua alone, but the preflight finds no lua5.4")),
            "{:#?}",
            dropped.disagreements
        );

        // RED CONTROL: a guest whose --prepare stops accepting `lua`.
        let narrowed = scratch.fixture("narrowed-guest").join("lua-random.sh");
        let text = std::fs::read_to_string(root.join("tests/e2e/language-runtimes/lua-random.sh"))
            .unwrap();
        let fallback = "    elif command -v lua >/dev/null 2>&1; then\n        echo lua\n";
        assert_eq!(
            text.matches(fallback).count(),
            1,
            "lua-random's fallback moved"
        );
        std::fs::write(&narrowed, text.replacen(fallback, "", 1)).unwrap();
        std::fs::set_permissions(&narrowed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let narrowed_guests = BTreeMap::from([(
            "lua5.4",
            BTreeSet::from([narrowed.to_string_lossy().into_owned()]),
        )]);
        let refused = agreement(&root, TOOL_PROBES, &[], &narrowed_guests, &scratch);
        assert!(
            refused
                .disagreements
                .iter()
                .any(|line| line.contains("does not accept lua alone")),
            "{:#?}",
            refused.disagreements
        );

        // RED CONTROL: a guest that needs PATH, but not its declared
        // interpreter, passes with none of the candidates. It is not exempt as
        // PATH-independent, because it fails with an empty PATH.
        let unchecked = scratch.fixture("unchecked-guest").join("ignores-lua.sh");
        std::fs::write(
            &unchecked,
            "#!/usr/bin/env bash\ncase ${1:-} in --prepare) command -v sh >/dev/null ;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&unchecked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let unchecked_guests = BTreeMap::from([(
            "lua5.4",
            BTreeSet::from([unchecked.to_string_lossy().into_owned()]),
        )]);
        let ignored = agreement(&root, TOOL_PROBES, &[], &unchecked_guests, &scratch);
        assert!(ignored.path_independent.is_empty());
        assert!(
            ignored
                .disagreements
                .iter()
                .any(|line| line.contains("--prepare passes and the preflight refuses lua5.4")),
            "{:#?}",
            ignored.disagreements
        );
    }

    fn committed() -> DagConfig {
        crate::validate_plan::validation_config(&root()).unwrap()
    }

    fn label(cfg: &DagConfig, label: &str) -> DagConfig {
        dagrun::select_steps_by_labels(cfg, &[label.to_string()]).unwrap()
    }

    fn real_path(_: &Step) -> OsString {
        std::env::var_os("PATH").unwrap_or_default()
    }

    /// After (B) the cargo full, portable and privileged selections hold no
    /// host node that prepares guests, so the preflight says so by count; the
    /// hosted selections and the Buck composition do, and are probed.
    #[test]
    fn preflight_scope_follows_the_committed_selections() {
        let root = root();
        let committed = committed();
        for local in ["full", "portable", "privileged"] {
            let cfg = label(&committed, local);
            let needs = host_tool_needs(&root, &[&cfg], &real_path).unwrap();
            assert!(needs.is_empty(), "{local}: {needs:?}");
            let verdict = evaluate(&needs, TOOL_PROBES, &BTreeSet::new());
            assert_eq!(
                verdict.lines[0],
                "Host-tool preflight: 0 selected host nodes declare tool needs"
            );
            assert!(verdict.missing.is_empty());
        }
        let hosted = label(&committed, "hosted-portable");
        let needs = host_tool_needs(&root, &[&hosted], &real_path).unwrap();
        assert_eq!(
            needs
                .iter()
                .map(|node| node.tag.as_str())
                .collect::<Vec<_>>(),
            ["build.manifest_guests"]
        );
        for token in ["lua5.4", "ruby", "cc", "linux"] {
            assert!(
                needs[0].tokens.contains(token),
                "{token}: {:?}",
                needs[0].tokens
            );
        }
        let privileged = label(&committed, "hosted-privileged");
        let needs = host_tool_needs(&root, &[&privileged], &real_path).unwrap();
        assert_eq!(
            needs
                .iter()
                .map(|node| node.tag.as_str())
                .collect::<Vec<_>>(),
            ["privileged-build.manifest_guests_on_host"]
        );
        let buck = hermit_manifest_plan::validation_dag::buck_e2e_selection(&committed).unwrap();
        let needs = host_tool_needs(&root, &[&buck], &real_path).unwrap();
        assert_eq!(
            needs
                .iter()
                .map(|node| node.tag.as_str())
                .collect::<Vec<_>>(),
            [hermit_manifest_plan::validation_dag::BUCK_STAGE_TAG]
        );
        assert_eq!(needs[0].tools.len(), 1);
        assert_eq!(needs[0].tools[0].candidates, ["patchelf"]);
        assert!(needs[0].tokens.contains("ruby") && needs[0].tokens.contains("python3"));
        // The PATH a rust-script-prefixed node inherits starts with
        // ci/rust-script-bin.
        let stage = buck
            .steps
            .iter()
            .find(|step| step.tag() == hermit_manifest_plan::validation_dag::BUCK_STAGE_TAG)
            .unwrap();
        let path = inherited_path(&root, stage);
        assert_eq!(
            std::env::split_paths(&path).next(),
            Some(root.join("ci/rust-script-bin"))
        );
    }

    /// A hosted-portable selection on a PATH without ruby refuses by name;
    /// the refusal is never green and leaves the plan untouched.
    #[test]
    fn preflight_refuses_a_hosted_selection_missing_ruby_by_name() {
        let root = root();
        let committed = committed();
        let hosted = label(&committed, "hosted-portable");
        let before = dagrun::io::dag_to_json(&hosted);
        let scratch = Scratch::new();
        let shadow = scratch.shadow("no-ruby", &["ruby"]);
        // Fakes for every other PATH-resolved candidate, so ruby is the one
        // PATH token the node can miss.
        let fakes = scratch.0.path().join("fakes");
        std::fs::create_dir_all(&fakes).unwrap();
        for probe in TOOL_PROBES.iter().filter(|probe| probe.token != "ruby") {
            for candidate in probe.candidates.iter().filter(|c| !c.contains('/')) {
                let target = fakes.join(candidate);
                std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let path = join(&[&fakes, &shadow]);
        let needs = host_tool_needs(&root, &[&hosted], &|_| path.clone()).unwrap();
        let verdict = evaluate(&needs, TOOL_PROBES, &BTreeSet::new());
        assert!(
            verdict.missing.iter().any(|line| line
                == "build.manifest_guests needs interpreter ruby: none of [ruby] is on the PATH it inherits"),
            "{:?}",
            verdict.missing
        );
        assert!(
            !verdict.missing.iter().any(|line| line.contains("lua5.4")),
            "{:?}",
            verdict.missing
        );
        let summary = refusal("hosted-portable", &verdict.missing);
        assert_eq!(summary.verdict, crate::Verdict::Refused);
        assert_eq!(summary.executed_tests, Some(0));
        assert_eq!(
            dagrun::io::dag_to_json(&hosted),
            before,
            "the preflight edited the plan"
        );
    }

    /// The parser fails closed on a host build whose population it cannot
    /// account, and accepts the committed shape.
    #[test]
    fn host_guest_build_parser_accepts_only_the_committed_shape() {
        assert_eq!(
            host_guest_build_lane(
                "export X=1; target/debug/test-harness build --lane portable --ci-only --allow-empty"
            )
            .unwrap(),
            Some("portable".to_string())
        );
        assert_eq!(host_guest_build_lane("cargo build").unwrap(), None);
        for refused in [
            "target/debug/test-harness build --lane portable --allow-empty",
            "target/debug/test-harness build --ci-only",
            "target/debug/test-harness build --lane portable --ci-only --include-manual",
            "target/debug/test-harness build --lane portable --lane privileged --ci-only",
        ] {
            assert!(
                host_guest_build_lane(refused).is_err(),
                "accepted {refused}"
            );
        }
    }

    /// The committed host guest build of hosted-portable, alone in a plan.
    fn host_guest_build_alone(edit: impl FnOnce(&mut Step)) -> DagConfig {
        let mut step = committed()
            .steps
            .into_iter()
            .find(|step| step.tag() == "build.manifest_guests")
            .unwrap();
        step.deps.clear();
        edit(&mut step);
        DagConfig {
            steps: vec![step],
            ..Default::default()
        }
    }

    /// dh-63c2n3 v5: when the preflight cannot plan the selection, here a host
    /// build whose population it cannot account, the refusal says so and names
    /// no missing tool. v4 headed it "1 tool(s) a selected host node needs are
    /// missing".
    #[test]
    fn admit_heads_a_planning_error_as_a_planning_error() {
        let root = root();
        let cfg = host_guest_build_alone(|step| {
            step.cmd = step.cmd.replace(" --ci-only", "");
        });
        let cmd = cfg.steps[0].cmd.clone();
        assert!(cmd.contains("test-harness build --lane portable --allow-empty"));
        let summary = admit(&root, &cfg, None, "hosted-portable").unwrap_err();
        assert_eq!(
            summary.detail,
            [
                "refused by: the host-tool preflight".to_string(),
                format!(
                    "the host-tool preflight could not plan the selected host nodes: host guest build without --lane and --ci-only, whose prepared population the host-tool preflight cannot account: {cmd}"
                ),
                "No DAG node or product test was executed; no node was removed and none was recorded as host-inapplicable; no passing receipt was produced."
                    .to_string(),
            ]
        );
        assert_eq!(summary.verdict, crate::Verdict::Refused);
        assert_eq!(summary.exit_code, COULD_NOT_RUN_EXIT_CODE);
        assert_eq!(summary.executed_tests, Some(0));
    }

    /// The missing-tools header stays for real missing tools: the same node,
    /// planned, on a PATH holding no probed tool, refuses through `admit`
    /// with the count of the lines that follow.
    #[test]
    fn admit_heads_missing_tools_as_missing_tools() {
        let root = root();
        let scratch = Scratch::new();
        let empty = scratch.0.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let cfg = host_guest_build_alone(|step| {
            step.env
                .insert("PATH".into(), empty.to_str().unwrap().to_string());
        });
        let summary = admit(&root, &cfg, None, "hosted-portable").unwrap_err();
        let missing = summary
            .detail
            .iter()
            .filter(|line| line.starts_with("  build.manifest_guests needs "))
            .count();
        assert!(missing >= 2, "{:#?}", summary.detail);
        assert_eq!(summary.detail[0], "refused by: the host-tool preflight");
        assert_eq!(
            summary.detail[1],
            format!(
                "{missing} tool(s) a selected host node needs are missing; each line names the token and every candidate tried:"
            )
        );
        for line in [
            "  build.manifest_guests needs interpreter lua5.4: none of [lua5.4, lua] is on the PATH it inherits",
            "  build.manifest_guests needs interpreter ruby: none of [ruby] is on the PATH it inherits",
        ] {
            assert!(
                summary.detail.iter().any(|detail| detail == line),
                "{line}: {:#?}",
                summary.detail
            );
        }
        assert!(
            !summary
                .detail
                .iter()
                .any(|line| line.contains("could not plan")),
            "{:#?}",
            summary.detail
        );
        assert_eq!(summary.verdict, crate::Verdict::Refused);
        assert_eq!(summary.exit_code, COULD_NOT_RUN_EXIT_CODE);
    }
}
