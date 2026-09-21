#!/usr/bin/env -S rust-script --force
//! Refuse a checker that nothing schedules.
//!
//! THE GAP THIS CLOSES. `check.lint_checks` made a checker added to the Makefile's
//! `lint-checks` target gated by construction, which removed "someone must also
//! hand-write a DAG node" as a failure mode. It did NOT cover the other shape: a
//! checker that is added to no target at all. Measured 2026-08-25 at main
//! a5fef7ff7623, four checkers were reachable from nothing whatsoever --
//! ci/check-derived-current-after-merge.sh was referenced by ZERO files in the
//! repository, and check-detcore-backend-abstraction-test.sh was named only inside
//! a comment. A checker nothing runs is indistinguishable from a checker that
//! passes, so it is worse than absent: it reads as coverage.
//!
//! WHAT IT ASSERTS. Every tracked, executable checker entrypoint is reachable from
//! either a `cmd` in ci/dag/*.json or the Makefile's `lint-checks` recipe, directly
//! or through another reachable script. Anything else must be listed in ALLOWLIST
//! with a reason, so unscheduled checkers are visible debt rather than silence.
//!
//! ⚠️ TWO TRAPS THIS DELIBERATELY AVOIDS, both of which caught the manual sweep
//! that produced it:
//!
//!   1. A reference inside a COMMENT is not an invocation.
//!      scripts/check-detcore-backend-abstraction.sh:264 names
//!      check-detcore-backend-abstraction-test.sh in a comment, and a plain grep
//!      reports it as scheduled. Comments are stripped before matching.
//!   2. A reference in .github/workflows/ is not evidence of a local gate. The
//!      portable workflow's integration-branch run is supplemental evidence;
//!      every other workflow is manual, and none gate a PR. Workflows are NOT a
//!      reachability source here, by design.
//!
//! Reachability is a fixpoint, not one hop: check-git-pin-uniformity.rs has no DAG
//! node but ci/run-reverie-pin-check.sh calls it and that has two, so it IS
//! scheduled. Stopping at one hop would report it as an orphan.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// Checkers that are deliberately NOT scheduled, each with the reason.
///
/// This is a RATCHET, not an amnesty. Every entry is a real gap that someone
/// decided not to close today; adding to it is a deliberate, reviewed act, and the
/// list should shrink. It exists so this checker starts green on a tree that
/// already has four orphans rather than landing as an immediate red.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "scripts/check-default-build-warnings.sh",
        "Builds the workspace; its findings were never counted, so wiring it \
         untriaged could convert a silent gap into a standing red. Only \
         scripts/setup-hooks.sh references it, and only to chmod +x. Run it on a \
         quiet box, count what it finds, then schedule or delete it.",
    ),
    (
        "scripts/test-fail-closed.sh",
        "Builds the workspace; findings never counted, same reasoning as \
         check-default-build-warnings.sh. Reached only from \
         scripts/progress-report.sh, which is itself unscheduled.",
    ),
    (
        "ci/check-derived-current-after-merge.sh",
        "Referenced by ZERO files in the repository. Passes when run by hand \
         (measured 2026-08-25). Scheduling it is a judgement about whether it is \
         live or dead code, which should be made explicitly rather than by this \
         guard's default.",
    ),
    (
        "scripts/check-detcore-backend-abstraction-test.sh",
        "Named only inside a comment at check-detcore-backend-abstraction.sh:264. \
         Passes when run by hand. It looks like the self-test arm of that checker \
         and most likely wants to be invoked by it; that is a change to that \
         checker, not to this list.",
    ),
    (
        "scripts/stress-test.sh",
        "A stress FRAMEWORK, not a checker. It matches only because the suffix rule \
         adopted `-test.sh`, and over-inclusion by a convention is exactly what an \
         allowlist entry is for: the alternative is narrowing the suffix until it \
         stops seeing the real `-test.sh` checkers too.",
    ),
    (
        "ci/validate-timeout-layers-test.sh",
        "A LIVE, ON-DEMAND bracket, by its own first line. It exercises validate's \
         nested wall-time limits, so scheduling it in lint-checks would run a real \
         validate inside a lint node. Measured 2026-08-26: reached by nothing, and \
         that is intended rather than an oversight.",
    ),
    (
        "scripts/validate-env-block-test.sh",
        "An on-demand bracket that SPAWNS VALIDATE: measured 2026-08-26 at 1m18s wall \
         on an idle box, and it fails with `Could not execute cargo` wherever the \
         submodules are not initialised -- a setup condition that would read as a \
         product failure in a lint node. Waived rather than scheduled for that \
         reason, not because nothing should run it.",
    ),
];

/// Is this tracked path a checker entrypoint?
fn is_checker_path(path: &str) -> bool {
    CHECKER_PATHS.contains(&path)
        || CHECKER_PREFIXES.iter().any(|pre| path.starts_with(pre))
        || CHECKER_SUFFIXES.iter().any(|suf| path.ends_with(suf))
}

/// Checker entrypoints whose filenames do not match the repository conventions.
///
/// Keep this exact rather than treating every `*-probe.rs` as a checker: most probes
/// are tools, while `bisect-probe.rs --self-test` is the assertion-bearing entrypoint
/// that `lint-checks` must run.
const CHECKER_PATHS: &[&str] = &["scripts/bisect-probe.rs"];

/// Directory/prefix pairs that identify a checker entrypoint by convention.
const CHECKER_PREFIXES: &[&str] = &[
    "scripts/check-",
    "scripts/test-",
    "scripts/test_",
    "ci/check-",
    "ci/verify-",
    "ci/test-",
    "ci/test_",
];

/// Suffixes that identify a checker entrypoint by convention.
///
/// WARNING -- A CONVENTION EXPRESSED ONLY AS A PREFIX CANNOT SEE ITS OWN FAMILY.
/// `foo-test.sh` is a checker by every standard this file uses -- it asserts and
/// exits nonzero -- and five of them were outside the population entirely, so
/// nothing verified that anything runs them. Two turned out to be scheduled by
/// NOTHING, which is the condition this checker exists to report and could not.
const CHECKER_SUFFIXES: &[&str] = &["-test.sh"];

/// This checker's own tracked path.
const SELF_PATH: &str = "scripts/check-checker-scheduling.rs";

/// Whether a reached script's BODY may be read as a source of invocations.
///
/// ⚠️ THIS FILE IS REACHED BUT IS NOT AN INVOCATION SOURCE, AND THE DISTINCTION IS
/// LOAD-BEARING. `ALLOWLIST` names every allowlisted checker as a string literal,
/// and each of those entries is a DECLARATION THAT THE PATH IS NOT SCHEDULED --
/// the exact opposite of an invocation. Feeding this file's text back into the
/// fixpoint therefore marks every allowlisted checker as reached and reports it
/// as a stale allowlist entry.
///
/// It is self-defeating rather than merely wrong: the bug appears only once this
/// checker is itself added to the `lint-checks` recipe, which is what makes it
/// reachable, which is what makes it read its own body. Measured 2026-08-25 on
/// this branch rebased onto main: with the recipe line present, 5 of 5 allowlist
/// entries reported STALE and `make lint-checks` exited 1; with the line removed
/// and nothing else changed, 0 did. A gate that cannot be scheduled without
/// breaking itself is not a gate.
///
/// The exclusion is by path rather than by content: a heuristic that tried to
/// tell a declaration from an invocation inside arbitrary Rust would be the kind
/// of guess this checker exists to avoid.
fn expands_frontier(path: &str) -> bool {
    path != SELF_PATH
}

fn tracked_files() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files"])
        .output()
        .expect("git ls-files failed to spawn");
    assert!(out.status.success(), "git ls-files exited nonzero");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Strip comment text so a mention inside a comment is not read as an invocation.
///
/// Deliberately conservative: only whole-line comments are removed. A trailing
/// `# ...` on a live command line is rare in these files and dropping it risks
/// discarding a real invocation, which would produce a FALSE ORPHAN -- the
/// expensive direction of error here.
fn strip_comments(text: &str, path: &str) -> String {
    let rust_like = path.ends_with(".rs");
    text.lines()
        .filter(|line| {
            let t = line.trim_start();
            if t.is_empty() {
                return false;
            }
            if rust_like {
                !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*'))
            } else {
                !t.starts_with('#')
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn main() {
    rust_script_prelude::init();
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "check-checker-scheduling: refuse a tracked checker entrypoint that no DAG\n\
             node and no `make lint-checks` recipe line can reach, directly or\n\
             transitively. Workflows are not a reachability source: the integration-\n\
             branch portable run is supplemental and the others are manual.\n\
             Deliberate exceptions live in\n\
             ALLOWLIST with a reason each."
        );
        return;
    }
    if std::env::args().any(|a| a == "--self-test") {
        self_test();
        return;
    }

    let tracked = tracked_files();

    // Every checker entrypoint we expect to be scheduled.
    let checkers: BTreeSet<String> = tracked
        .iter()
        .filter(|p| is_checker_path(p))
        .filter(|p| p.ends_with(".sh") || p.ends_with(".rs") || p.ends_with(".py"))
        .filter(|p| is_executable(p) || p.ends_with(".py"))
        .cloned()
        .collect();
    // ⚠️ A FLOOR, NOT A ZERO CHECK, AND THE DIFFERENCE IS THE FAILURE THAT MATTERS.
    // `!is_empty()` catches discovery going to NOTHING. It cannot catch discovery
    // COLLAPSING -- and a collapse reports HEALTHY, because everyone left in the
    // shrunken population still passes. "OK, 7 checker entrypoints, 7 scheduled" is
    // rc=0 in exactly the confident tone this guard exists to avoid, and nobody reads
    // a green line for a number that used to be larger.
    //
    // Sized against real predicate slips rather than chosen, measured 2026-08-26 at
    // `origin/main` 96ca7b1d51 where the population is 32:
    //
    //   one prefix typo'd away (`scripts/check-`)   18   <- caught, rc=101
    //   prefix list emptied, suffix only             7   <- caught
    //   suffix arm dropped (pre-#2684 behaviour)    26   <- NOT caught
    //   `.py` arm dropped from the exec filter      29   <- NOT caught
    //
    // ⚠️ SO IT IS DELIBERATELY A BLUNT INSTRUMENT AND THE LIMIT IS STATED RATHER THAN
    // IMPLIED: it catches a category disappearing, not a narrowing. A 32 -> 26 slip
    // passes this and must be caught by the semantic assertions in `self_test`, which
    // pin named files rather than a count. Both are needed; neither subsumes the
    // other. 25 leaves room for seven genuine deletions before the guard refuses.
    const POPULATION_FLOOR: usize = 25;
    assert!(
        checkers.len() >= POPULATION_FLOOR,
        "discovered {} checker entrypoint(s), fewer than the floor of {POPULATION_FLOOR}; \
         the naming convention or this filter moved and the guard is now reading a \
         collapsed population -- which would otherwise report OK, because everything \
         left in it still passes",
        checkers.len()
    );
    // A rename must not silently re-enable the self-reference described on
    // `expands_frontier`. If SELF_PATH stops naming a tracked file the exclusion
    // matches nothing, every allowlist entry reads as stale again, and the only
    // symptom is a confident wrong answer.
    assert!(
        tracked.iter().any(|p| p == SELF_PATH),
        "SELF_PATH ({SELF_PATH}) is not tracked; update it to this file's path"
    );

    // Seed the reachable set from the two things that actually schedule work.
    //
    // ⚠️ ONLY THE `cmd` FIELDS, never the whole JSON. A DAG node's `description` is
    // prose and routinely NAMES checkers it does not run -- check.lint_checks's own
    // description lists six of them. Seeding from the raw file therefore counted a
    // checker mentioned in prose as scheduled, which is the false negative this guard
    // exists to prevent: it would report an orphan as covered because someone wrote
    // about it. Caught 2026-08-25 when a freshly ALLOWLISTED entry was reported STALE
    // ("now scheduled") purely because a node description mentioned it.
    //
    // Same defect class as counting a marker's TEXT instead of the binding: the
    // instrument must read the field that CAUSES execution, not the field that
    // discusses it.
    let mut seed = String::new();
    for dag in ["ci/dag/validate.json"] {
        if Path::new(dag).exists() {
            let raw = std::fs::read_to_string(dag).unwrap();
            let cmds = extract_cmds(&raw);
            assert!(
                !cmds.is_empty(),
                "{dag} yielded no `cmd` fields; the schema moved and this guard would \
                 otherwise report every checker as an orphan"
            );
            for c in cmds {
                seed.push_str(&c);
                seed.push('\n');
            }
        }
    }
    seed.push_str(&lint_checks_recipe(
        &std::fs::read_to_string("Makefile").expect("Makefile is unreadable"),
    ));

    // Fixpoint over EVERY tracked script, not only the checkers.
    //
    // ⚠️ Expanding only checkers is wrong and the first version of this guard did
    // it. The intermediate scripts are what carry reachability: ci/run-reverie-pin-
    // check.sh has two DAG nodes and calls both check-reverie-pin.rs and
    // check-git-pin-uniformity.rs, and scripts/validate.rs calls
    // ci/verify-hermit-e2e-artifact.sh. Neither intermediary matches a checker
    // prefix, so a checkers-only frontier stopped dead at the DAG and reported all
    // three as orphans. False orphans are the expensive direction: they train a
    // reader to allowlist a checker that was scheduled all along.
    let all_scripts: BTreeSet<String> = tracked
        .iter()
        .filter(|p| p.ends_with(".sh") || p.ends_with(".rs") || p.ends_with(".py"))
        .filter(|p| !p.starts_with("third-party/"))
        .cloned()
        .collect();

    let mut reached: BTreeSet<String> = BTreeSet::new();
    let mut frontier = vec![seed];
    while let Some(text) = frontier.pop() {
        for s in &all_scripts {
            if reached.contains(s) {
                continue;
            }
            if is_invoked(&text, s) {
                reached.insert(s.clone());
                if expands_frontier(s) {
                    if let Ok(body) = std::fs::read_to_string(s) {
                        frontier.push(strip_comments(&body, s));
                    }
                }
            }
        }
    }

    let allow: BTreeMap<&str, &str> = ALLOWLIST.iter().copied().collect();
    let mut unscheduled: Vec<&String> = checkers
        .iter()
        .filter(|c| !reached.contains(*c))
        .filter(|c| !allow.contains_key(c.as_str()))
        .collect();
    unscheduled.sort();

    // A stale allowlist is its own defect: an entry that IS now scheduled, or names
    // a file that no longer exists, quietly grants permission nobody needs.
    let mut stale: Vec<&str> = ALLOWLIST
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| reached.contains(*p) || !Path::new(p).exists())
        .collect();
    stale.sort();

    if unscheduled.is_empty() && stale.is_empty() {
        println!(
            "check-checker-scheduling: OK -- {} checker entrypoint(s), {} scheduled, \
             {} allowlisted with reasons.",
            checkers.len(),
            reached.iter().filter(|r| checkers.contains(*r)).count(),
            ALLOWLIST.len()
        );
        return;
    }

    if !unscheduled.is_empty() {
        eprintln!(
            "check-checker-scheduling: {} checker(s) are scheduled by NOTHING.",
            unscheduled.len()
        );
        eprintln!(
            "  A checker nothing runs is indistinguishable from a checker that passes."
        );
        eprintln!("  Add it to the Makefile's `lint-checks` recipe (which check.lint_checks");
        eprintln!("  runs, so no DAG edit is needed), or to a DAG node, or add it to");
        eprintln!("  ALLOWLIST in this file WITH A REASON.");
        for c in &unscheduled {
            eprintln!("    {c}");
        }
    }
    for p in &stale {
        eprintln!("check-checker-scheduling: STALE ALLOWLIST entry {p} -- now scheduled or absent; remove it.");
    }
    std::process::exit(exit_code_for(unscheduled.len(), stale.len()));
}

/// The process status for a finished run: 1 when anything is unscheduled or stale.
///
/// ⚠️ THIS EXISTS ONLY TO BE TESTABLE, and that is the point. The failure path ended in a
/// bare `std::process::exit(1)` that no test could reach: `agent(codex-rev-2683)` showed
/// that replacing it with `return` left `--self-test` and all twelve unit tests GREEN while
/// an orphan merely printed a warning and the guard exited 0 -- the loudest possible
/// finding delivered through a silent exit status.
fn exit_code_for(unscheduled: usize, stale: usize) -> i32 {
    if unscheduled > 0 || stale > 0 { 1 } else { 0 }
}

/// A runner, then any number of FLAGS, then the path: `rustc --edition=2021 <path>`.
///
/// Tokenised per line rather than matched as a substring, so `--edition=2021` between
/// the runner and its input does not hide the invocation. Stops at the first token that
/// is not a flag; see the note at the call site for why it deliberately does not scan
/// further.
fn runner_flag_path(text: &str, path: &str) -> bool {
    const RUNNERS: [&str; 5] = ["python3", "python", "bash", "sh", "rustc"];
    const PYTHON_NO_VALUE: [&str; 13] = [
        "-B", "-E", "-I", "-O", "-OO", "-s", "-S", "-u", "-v", "-b", "-d", "-q", "-x",
    ];
    const SHELL_NO_VALUE: [&str; 18] = [
        "-e", "-x", "-u", "-v", "-n", "-f", "-C", "-a", "-h", "-i", "-l", "-m", "-p",
        "-r", "-s", "-t", "-c", "-D",
    ];
    const RUSTC_NO_VALUE: [&str; 5] = ["-O", "-g", "-V", "-h", "-v"];

    // ⚠️ THE THIRD STATE. A flag is not just "takes a value" or "does not" -- some put the
    // runner into a mode where the named path IS NEVER EXECUTED, and a two-state table
    // called every one of them SCHEDULED. Found by `agent(codex-rev-2683)`, who ran each
    // case and checked for a marker the script writes:
    //
    //     bash -n ./check-z.sh      rc 0, marker ABSENT   syntax check only
    //     bash -o noexec ./x.sh     rc 0, marker ABSENT   noexec via -o's VALUE
    //     rustc -V ./check-z.rs     rc 0, marker ABSENT   prints a version, compiles nothing
    //     python3 -c pass ./x.py    rc 0, marker ABSENT   the path is argv, not the program
    //
    // A checker that is merely an ARGUMENT to a non-executing runner mode was counted as
    // coverage -- the silent direction, and exactly what this guard exists to prevent.
    // Erring loud: any of these before the candidate makes the occurrence an orphan.
    const PYTHON_NO_EXEC: [&str; 4] = ["-V", "--version", "-h", "--help"];
    const SHELL_NO_EXEC: [&str; 5] = ["-n", "-D", "--help", "--version", "-V"];
    const RUSTC_NO_EXEC: [&str; 4] = ["-V", "--version", "-h", "--help"];

    // Flags whose VALUE is the program, so a path AFTER that value is an argument.
    const PROGRAM_IS_THE_VALUE: [&str; 2] = ["-c", "-m"];

    // ⚠️ A THIRD ANSWER: UNKNOWN. The previous revision had two -- takes a value, or does
    // not -- with UNKNOWN folded into "takes a value" and called the loud direction. It is
    // not always loud, and `agent(hermit-020)` relaying codex showed why:
    //
    //     python3 --nonsense ignored scripts/check-z.sh   ->  SCHEDULED
    //
    // Skipping a presumed value moves the cursor PAST one token, and the token it lands on
    // may be an argument rather than the program. Skipping too far yields a false ORPHAN
    // only when the landing token is not the checker; when it happens to BE the checker,
    // the same over-skip yields a false SCHEDULED, which is silent. My claim that the
    // inverted default made omissions loud was therefore true only for the cases I tested.
    //
    // So an unrecognised flag now yields NO INVOCATION for that occurrence. Uncertainty
    // resolves to orphan -- loud -- instead of to a guess in either direction.
    const RUSTC_VALUE: [&str; 17] = [
        "-o", "--out-dir", "--emit", "--extern", "--target", "--edition", "--crate-name",
        "--crate-type", "--cfg", "--check-cfg", "-L", "-l", "-C", "-Z", "-W", "-A", "-D",
    ];
    const PYTHON_VALUE: [&str; 5] = ["-c", "-m", "-X", "-W", "-Q"];
    const SHELL_VALUE: [&str; 3] = ["-o", "-O", "--rcfile"];

    enum Arity {
        None,
        Value,
        Unknown,
    }

    fn arity(runner: &str, flag: &str) -> Arity {
        if flag.contains('=') || (!flag.starts_with("--") && flag.len() > 2) {
            return Arity::None;
        }
        let (booleans, valued): (&[&str], &[&str]) = match runner {
            "rustc" => (&RUSTC_NO_VALUE, &RUSTC_VALUE),
            "python" | "python3" => (&PYTHON_NO_VALUE, &PYTHON_VALUE),
            _ => (&SHELL_NO_VALUE, &SHELL_VALUE),
        };
        if booleans.contains(&flag) {
            Arity::None
        } else if valued.contains(&flag) {
            Arity::Value
        } else {
            Arity::Unknown
        }
    }

    fn no_exec(runner: &str, flag: &str) -> bool {
        let modes: &[&str] = match runner {
            "rustc" => &RUSTC_NO_EXEC,
            "python" | "python3" => &PYTHON_NO_EXEC,
            _ => &SHELL_NO_EXEC,
        };
        if modes.contains(&flag) {
            return true;
        }
        // Bash accepts `-D` inside a short-option bundle. Do not generalise this
        // to every character in SHELL_NO_EXEC: Bash also accepts single-hyphen
        // long options such as `-norc`, and those DO execute the following script.
        matches!(runner, "bash" | "sh")
            && flag.starts_with('-')
            && !flag.starts_with("--")
            && flag.len() > 2
            && flag[1..].contains('D')
    }

    let dotted = format!("./{path}");
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let mut cursor = 0usize;
        for (index, token) in tokens.iter().enumerate() {
            if let Some(off) = line[cursor..].find(token) {
                cursor += off;
            }
            let runner_at = cursor;
            cursor += token.len();
            if !RUNNERS.contains(token) {
                continue;
            }
            // ⚠️ THE RUNNER ITSELF MUST BE IN COMMAND POSITION, and this clause did not
            // check it while this file's own comment claimed every clause did. Measured
            // by `agent(codex-rev-2683)`: `echo python3 -X faulthandler scripts/check-z.py`
            // classified SCHEDULED, because the scan found `python3` anywhere on the line.
            // A guard that three of four clauses consult is one a clause still bypasses.
            if !in_command_position(&line[..runner_at]) {
                continue;
            }
            let mut next = index + 1;
            let mut executes = true;
            while next < tokens.len() && tokens[next].starts_with('-') {
                let flag = tokens[next];
                if no_exec(token, flag) {
                    executes = false;
                }
                if PROGRAM_IS_THE_VALUE.contains(&flag) {
                    // The shells RUN the value (`sh -c ./x.sh`); python treats it as code
                    // or a module. Either way what FOLLOWS the value is an argument, so
                    // only the value itself can be the invocation.
                    let value = next + 1;
                    let shellish = !matches!(*token, "python" | "python3" | "rustc");
                    if shellish
                        && value < tokens.len()
                        && (tokens[value] == path || tokens[value] == dotted)
                    {
                        return true;
                    }
                    executes = false;
                    break;
                }
                // `-o noexec` turns execution off through the flag's VALUE, not the flag.
                if flag == "-o" && next + 1 < tokens.len() && tokens[next + 1] == "noexec" {
                    executes = false;
                }
                match arity(token, flag) {
                    Arity::Value => next += 1,
                    Arity::Unknown => {
                        // Cannot tell where the program is. Refuse the occurrence.
                        executes = false;
                        break;
                    }
                    Arity::None => {}
                }
                next += 1;
            }
            if !executes {
                continue;
            }
            if next < tokens.len() && (tokens[next] == path || tokens[next] == dotted) {
                return true;
            }
        }
    }
    false
}

/// Does `text` INVOKE `path`, as opposed to merely naming it?
///
/// ⚠️ A bare substring match on the basename is not good enough, and this is the
/// second time that exact shortcut produced a wrong answer here. A filename appears
/// in plenty of live, non-comment text that does not run it -- most sharply, an
/// `echo` inside ci/lint-checks-node.sh names test_validate_stop_paths.py in a
/// diagnostic message, which read as an invocation and silently marked an
/// unschedulable checker as scheduled.
///
/// So require one of the shapes this repository actually uses to RUN a script:
///
///     ./scripts/foo.sh            $(SUBMODULE_PROXY) ./ci/foo.sh
///     python3 ./scripts/foo.py    python3 scripts/foo.py
///     bash scripts/foo.sh         sh scripts/foo.sh
///
/// The bare-path forms are deliberately restricted to an explicit interpreter prefix.
/// Erring toward a FALSE ORPHAN is the safe direction: it is loud and a human fixes
/// it in one line, whereas a false "scheduled" is silent and defeats the guard.
fn is_invoked(text: &str, path: &str) -> bool {
    // WARNING -- EVERY CLAUSE HERE GOES THROUGH `invokes_on_line`, AND THAT IS THE SHAPE
    // THAT MATTERS. This was a disjunction in which only the runner clause consulted a
    // guard and this one was a bare `text.contains`. Because the clauses are OR-ed the
    // WEAKEST decided the answer, so every hardening added to the flag scan --
    // hermit#2661, #2667, #2674, #2681 -- was reachable only when this clause had already
    // declined. Two characters defeated them all: `rustc -o ./scripts/x.rs` read as an
    // invocation. A guard one arm of an OR can bypass is not a guard.
    let dotted = format!("./{path}");
    if text.lines().any(|line| invokes_on_line(line, &dotted)) {
        return true;
    }
    // Repository shell helpers commonly resolve their own root and invoke a
    // sibling as `"$ROOT_DIR/ci/check-x.sh"`. Match that rooted spelling while
    // retaining the same command-position guard, so an assignment such as
    // `CHECKER="$ROOT_DIR/ci/check-x.sh"` remains declaration rather than reachability.
    // Include the quotes in the needle: single quotes would suppress ROOT_DIR expansion.
    for prefix in ["$ROOT_DIR/", "${ROOT_DIR}/"] {
        let quoted = format!("\"{prefix}{path}\"");
        if invokes_outside_quotes(text, &quoted) {
            return true;
        }
    }
    // `rustc` is a runner here too: ci/run-reverie-pin-check.sh COMPILES
    // scripts/check-reverie-pin.rs and runs the resulting binary, so the checker is
    // genuinely scheduled without ever being executed as a script.
    for runner in ["python3 ", "python ", "bash ", "sh ", "rustc "] {
        let needle = format!("{runner}{path}");
        // ⚠️ AND THE RUNNER PREFIX MUST NOT BE INSIDE A STRING LITERAL. Requiring
        // `python3 ` in front of the path was still not enough: `ci/lint-checks-node.sh`
        // carries `"python3 scripts/test_validate_stop_paths.py` as FIXTURE DATA -- a
        // quoted multi-line string handed to `check_run` to simulate make's output --
        // and that read as an invocation. Measured 2026-08-26: it turned
        // check-checker-scheduling RED on main immediately after #2622 merged, because
        // the entry #2622 correctly allowlisted was then reported STALE ("now
        // scheduled") on the strength of a test fixture.
        //
        // A real invocation begins a command: the runner is at the start of a line, or
        // follows a shell operator. It is never preceded by a quote. This is the THIRD
        // narrowing of this predicate, and each one has been the same lesson -- a
        // filename in live code is not evidence that the code runs it.
        if text.lines().any(|line| invokes_on_line(line, &needle)) {
            return true;
        }
    }
    // ⚠️ A FLAG MAY SIT BETWEEN THE RUNNER AND THE PATH, and requiring them adjacent
    // reported a genuinely scheduled checker as an orphan. Found by agent(hermit-004)
    // wiring check.exit_status_class: `rustc --edition=2021 scripts/check-exit-status-class.rs`
    // matched nothing -- the cmd has no `./<path>` and no line STARTS with the path,
    // so every clause missed and the node was called scheduled by NOTHING.
    //
    // ⚠️ ONLY LEADING FLAGS ARE SKIPPED, AND THE SCAN STOPS AT THE FIRST NON-FLAG TOKEN.
    // It does NOT search the whole line for the path, because `rustc a.rs -o <path>`
    // would then read an OUTPUT as an invocation -- a false "scheduled", which is silent
    // and defeats the guard. Erring to a FALSE ORPHAN is the direction this file already
    // chose: loud, and a human fixes it in one line.
    //
    // The known cost of stopping at the first non-flag token: `rustc -o out <path>`, where
    // a flag takes a SEPARATE value, still misses. Skipping that would require knowing
    // which flags consume the next token, which is a guess -- and a wrong guess here fails
    // in the silent direction.
    if runner_flag_path(text, path) {
        return true;
    }
    // A path alone at the start of a line is a shell line-continuation argument --
    // the form run-reverie-pin-check.sh uses for both pin checkers. Anchoring to the
    // line start is what keeps this from matching a filename quoted mid-sentence.
    text.lines().any(|line| invokes_on_line(line, path))
}

/// Is `needle` used as a COMMAND on this line, rather than quoted inside a string?
///
/// The discriminator is the character immediately before the match: a command starts
/// a line or follows a shell operator (`;`, `&`, `|`, `(`, backtick) or `$(`. A quote
/// before it means the text is data -- an echoed diagnostic, or a fixture standing in
/// for output. Erring toward FALSE ORPHAN stays the safe direction.
fn invokes_on_line(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(hit) = line[from..].find(needle) {
        let at = from + hit;
        if invocation_at(line, at, needle) {
            return true;
        }
        from = at + 1;
    }
    false
}

fn invocation_at(line: &str, at: usize, needle: &str) -> bool {
    // ⚠️ THE MATCH MUST END AT A WORD BOUNDARY TOO. Without this,
    // `./scripts/check-z.sh.disabled` schedules `scripts/check-z.sh`: a longer
    // path CONTAINING the checker as a prefix satisfies the guard. Renaming a
    // checker to `.disabled` is a normal way to retire one, so this fires by
    // ACCIDENT rather than by contrivance -- reported by agent(hermit-020)
    // relaying codex, and reproduced here.
    let after = line[at + needle.len()..].chars().next();
    let ends_cleanly = after
        .map(|c| c.is_whitespace() || matches!(c, ';' | '&' | '|' | ')' | '"' | '\'' | ','))
        .unwrap_or(true);
    ends_cleanly && in_command_position(&line[..at])
}

/// Find an invocation while carrying quote state across line boundaries.
///
/// Command position remains a line-local question, but shell and fixture strings may
/// span lines. Resetting quote state at each newline lets an inert second line look
/// executable.
fn invokes_outside_quotes(text: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(hit) = text[from..].find(needle) {
        let at = from + hit;
        if is_active_shell_position(text, at) {
            let line_start = text[..at].rfind('\n').map_or(0, |index| index + 1);
            let line_end = text[at..].find('\n').map_or(text.len(), |index| at + index);
            if invocation_at(&text[line_start..line_end], at - line_start, needle) {
                return true;
            }
        }
        from = at + 1;
    }
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuoteState {
    Unquoted,
    Single,
    AnsiC,
    Double,
}

struct QuoteLexer {
    state: QuoteState,
    escaped: bool,
    effective_dollar: bool,
}

impl QuoteLexer {
    fn new() -> Self {
        Self {
            state: QuoteState::Unquoted,
            escaped: false,
            effective_dollar: false,
        }
    }

    /// Consume shell text while retaining only the lexical state needed to decide
    /// whether a later byte is executable. This is deliberately not a shell parser.
    fn scan(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if self.escaped {
                self.escaped = false;
                if byte != b'\n' {
                    self.effective_dollar = false;
                }
                index += 1;
                continue;
            }
            match self.state {
                QuoteState::Unquoted => match byte {
                    b'\\' => self.escaped = true,
                    b'$' => self.effective_dollar = true,
                    b'\'' => {
                        self.state = if self.effective_dollar {
                            QuoteState::AnsiC
                        } else {
                            QuoteState::Single
                        };
                        self.effective_dollar = false;
                    }
                    b'"' => {
                        self.state = QuoteState::Double;
                        self.effective_dollar = false;
                    }
                    _ => self.effective_dollar = false,
                },
                QuoteState::Single => {
                    if byte == b'\'' {
                        self.state = QuoteState::Unquoted;
                    }
                    self.effective_dollar = false;
                }
                QuoteState::AnsiC => match byte {
                    b'\\' => self.escaped = true,
                    b'\'' => self.state = QuoteState::Unquoted,
                    _ => {}
                },
                QuoteState::Double => match byte {
                    b'\\'
                        if matches!(
                            bytes.get(index + 1),
                            Some(b'$' | b'`' | b'"' | b'\\' | b'\n')
                        ) =>
                    {
                        self.escaped = true;
                    }
                    b'"' => self.state = QuoteState::Unquoted,
                    _ => {}
                },
            }
            if self.state != QuoteState::Unquoted {
                self.effective_dollar = false;
            }
            index += 1;
        }
    }

    fn is_unquoted(&self) -> bool {
        self.state == QuoteState::Unquoted && !self.escaped
    }
}

/// Does `text` end inside shell-like single, ANSI-C, or double quotes?
fn has_open_quote(text: &str) -> bool {
    let mut lexer = QuoteLexer::new();
    lexer.scan(text);
    !lexer.is_unquoted()
}

#[derive(Clone, Debug)]
struct HereDoc {
    delimiter: String,
    strip_tabs: bool,
}

fn is_shell_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace() || b";&|()<>".contains(&byte)
}

/// Parse the deliberately small heredoc grammar this reachability check accepts.
/// Only wholly single- or double-quoted static delimiters make the body inert: an
/// unquoted heredoc expands command substitutions. Bare, escaped, concatenated,
/// dynamic, and multiple heredocs are refused, preserving the safe false-orphan
/// direction instead of claiming their bodies are data.
fn parse_heredoc(after_operator: &str) -> Result<HereDoc, ()> {
    let mut tail = after_operator;
    let strip_tabs = tail.starts_with('-');
    if strip_tabs {
        tail = &tail[1..];
    }
    tail = tail.trim_start_matches([' ', '\t']);
    let bytes = tail.as_bytes();
    let Some(&first) = bytes.first() else {
        return Err(());
    };

    if !matches!(first, b'\'' | b'"') {
        return Err(());
    }
    let quote = first;
    let Some(end) = bytes[1..].iter().position(|byte| *byte == quote) else {
        return Err(());
    };
    let delimiter = &tail[1..end + 1];
    if delimiter.is_empty()
        || delimiter
            .bytes()
            .any(|byte| matches!(byte, b'$' | b'`' | b'\\'))
    {
        return Err(());
    }
    let consumed = end + 2;
    if bytes
        .get(consumed)
        .is_some_and(|byte| !is_shell_boundary(*byte))
    {
        return Err(());
    }
    Ok(HereDoc {
        delimiter: delimiter.to_string(),
        strip_tabs,
    })
}

struct ShellLexer {
    quote: QuoteLexer,
    pending_heredoc: Option<HereDoc>,
    heredoc: Option<HereDoc>,
    unsupported: bool,
}

impl ShellLexer {
    fn new() -> Self {
        Self {
            quote: QuoteLexer::new(),
            pending_heredoc: None,
            heredoc: None,
            unsupported: false,
        }
    }

    fn scan_code_line(&mut self, line: &str) {
        let mut start = 0;
        while let Some(hit) = line[start..].find("<<") {
            let index = start + hit;
            self.quote.scan(&line[start..index]);
            let operator_len = if line.as_bytes().get(index + 2) == Some(&b'<') {
                3
            } else {
                2
            };
            if self.quote.is_unquoted() && operator_len == 2 {
                if self.pending_heredoc.is_some() {
                    self.unsupported = true;
                    return;
                }
                match parse_heredoc(&line[index + 2..]) {
                    Ok(spec) => self.pending_heredoc = Some(spec),
                    Err(()) => {
                        self.unsupported = true;
                        return;
                    }
                }
            }
            self.quote.scan(&line[index..index + operator_len]);
            start = index + operator_len;
        }
        self.quote.scan(&line[start..]);
    }

    fn scan_line(&mut self, line: &str, complete: bool) {
        if self.unsupported {
            return;
        }
        if let Some(spec) = &self.heredoc {
            let candidate = if spec.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if complete && candidate.trim_end_matches('\r') == spec.delimiter {
                self.heredoc = None;
            }
            return;
        }

        self.scan_code_line(line);
        if !complete || self.unsupported {
            return;
        }
        let continued = self.quote.escaped;
        self.quote.scan("\n");
        if let Some(spec) = self.pending_heredoc.take() {
            if continued || !self.quote.is_unquoted() {
                self.unsupported = true;
            } else {
                self.heredoc = Some(spec);
            }
        }
    }

    fn scan(&mut self, text: &str) {
        let mut remainder = text;
        while let Some(newline) = remainder.find('\n') {
            self.scan_line(&remainder[..newline], true);
            remainder = &remainder[newline + 1..];
        }
        if !remainder.is_empty() {
            self.scan_line(remainder, false);
        }
    }

    fn is_executable(&self) -> bool {
        !self.unsupported
            && self.heredoc.is_none()
            && self.quote.is_unquoted()
            && !self.quote.effective_dollar
    }
}

/// Is `at` executable shell text rather than quoted or heredoc fixture data?
fn is_active_shell_position(text: &str, at: usize) -> bool {
    let mut lexer = ShellLexer::new();
    lexer.scan(&text[..at]);
    lexer.is_executable()
}

/// Is a match starting right after `before` THE FIRST WORD OF A COMMAND?
///
/// WARNING -- THIS USED TO ASK WHICH CHARACTER PRECEDED THE MATCH, which is the same
/// mistake as every other defect in this file: a semantic question answered by
/// enumerating syntax. The old rule accepted nothing, `;`, `&`, `|`, `(` or a backtick.
/// Two real invocations here are preceded by neither -- `$(SUBMODULE_PROXY) ./scripts/x.rs`
/// in the Makefile and `VAR=value ./ci/node.sh` in a DAG command -- so funnelling the
/// other clauses through the old guard reported 2 checkers as scheduled by NOTHING.
/// Adding `)` and `=` to the list would have fixed those two and left the third shape.
fn in_command_position(before: &str) -> bool {
    let mut before = before;
    // A quote directly before the match may be a string DELIMITER. Commands reach this
    // file as shell text, as JSON (decoded upstream) and as RUST literals in
    // scripts/validate.rs (`node.cmd = "./ci/verify-x.sh ..."`).
    //
    // ⚠️ ONLY WHEN IT OPENS ONE. agent(hermit-005) refused an earlier revision for
    // stripping ANY trailing quote, which readmitted the shape hermit#2656 excluded: a
    // runner prefix carried as FIXTURE DATA, where the quote is preceded by nothing but
    // indentation. In a genuine embedded command it is preceded by the operator that
    // introduces the literal, so that is what is required.
    if let Some(rest) = before.strip_suffix('"').or_else(|| before.strip_suffix('\'')) {
        let opener = rest.trim_end();
        if opener.is_empty() || !opener.ends_with(['=', '(', ',']) {
            return false;
        }
        before = rest;
    }
    if has_open_quote(before) {
        return false;
    }
    // The match must START A WORD. `OUT=./scripts/check-x.sh` is an assignment's VALUE.
    if let Some(last) = before.chars().last() {
        if !last.is_whitespace() && !matches!(last, ';' | '&' | '|' | '(' | '`') {
            return false;
        }
    }
    let trimmed = before.trim_end();
    if trimmed.is_empty() {
        return true;
    }
    for token in trimmed.split_whitespace().rev() {
        if is_command_boundary(token) {
            return true;
        }
        if is_assignment(token) || is_expansion(token) {
            continue;
        }
        return false;
    }
    true
}

/// `NAME=value` -- an environment prefix, which precedes a command.
fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `$(VAR)`, `${VAR}` or `$VAR` -- may expand to a command prefix.
fn is_expansion(token: &str) -> bool {
    token.starts_with('$')
}

/// A token that ENDS the previous command, so what follows begins a new one.
fn is_command_boundary(token: &str) -> bool {
    // A BARE `=` is an assignment operator with spaces around it, which shell
    // assignments never have. It occurs only in source that BUILDS a command string.
    matches!(token, ";" | "&&" | "||" | "|" | "&" | "(" | "`" | "=")
        || token.ends_with(';')
        || token.ends_with("&&")
        || token.ends_with("||")
        || token.ends_with('|')
        || token.ends_with('`')
}
/// Decode a JSON string literal into the text a shell would see.
///
/// WARNING -- WITHOUT THIS THE QUOTE GUARD IS MEANINGLESS ON DAG COMMANDS, and that was
/// a live defect. `extract_cmds` returned everything after `"cmd":` verbatim, so the JSON
/// delimiter stayed on the front. The guard rejects a match preceded by a quote -- meaning
/// SHELL quoting -- and cannot tell the two apart, so
///
///     "cmd": "python3 scripts/check-x.py --gate"
///
/// was rejected as quoted and read as an ORPHAN. Measured before this change: that shape
/// returned false while the `./` shape returned true, and it returned true only because
/// the `./` clause consulted no guard at all. The unguarded clause was LOAD-BEARING --
/// guarding it without decoding first turns every DAG-scheduled checker into a false
/// orphan at once, which is exactly what happened when I tried.
fn decode_json_string(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix('"') else {
        return raw.to_string();
    };
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}


/// Pull the value of every `"cmd":` field out of a DAG file.
///
/// Deliberately textual rather than a JSON parse: these scripts take no third-party
/// dependencies, and the field is emitted one-per-line by the generator. It fails
/// closed at the call site if the result is empty, so a schema move is loud rather
/// than silently reporting every checker as an orphan.
fn extract_cmds(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some(rest) = line.split_once("\"cmd\"") else {
            continue;
        };
        let Some(after_colon) = rest.1.split_once(':') else {
            continue;
        };
        out.push(decode_json_string(after_colon.1.trim()));
    }
    out
}

/// Extract just the `lint-checks` recipe. Using the whole Makefile would count a
/// checker named anywhere in it -- including in `lint-cargo`, in a comment, or in
/// an unrelated target -- as scheduled, which is the false-negative this guard
/// exists to prevent.
fn lint_checks_recipe(makefile: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in makefile.lines() {
        if line.starts_with("lint-checks:") {
            inside = true;
            continue;
        }
        if inside {
            // A recipe line is TAB-indented; anything else ends the recipe.
            if line.starts_with('\t') {
                let t = line.trim_start();
                if !t.starts_with('#') {
                    out.push_str(line);
                    out.push('\n');
                }
            } else if !line.trim().is_empty() {
                break;
            }
        }
    }
    assert!(
        !out.is_empty(),
        "could not find a `lint-checks:` recipe in the Makefile; this guard would \
         otherwise report every lint checker as an orphan"
    );
    out
}

fn self_test() {
    // ⚠️ PINS THE EXIT STATUS ITSELF. Reported by `agent(codex-rev-2683)`: the failure path
    // was a bare `exit(1)` no test reached, so swapping it for `return` kept every suite
    // green while orphans were admitted at rc=0.
    assert_eq!(exit_code_for(1, 0), 1, "an unscheduled checker must exit nonzero");
    assert_eq!(exit_code_for(0, 1), 1, "a stale allowlist entry must exit nonzero");
    assert_eq!(exit_code_for(3, 2), 1, "both together must exit nonzero");
    assert_eq!(exit_code_for(0, 0), 0, "a clean run must exit zero");

    // ⚠️ THE SHARED CONTROL. Several matcher defects in one night shared a SIGNATURE and
    // not a cause: each inferred a semantic fact from an unverified syntactic position,
    // and each failed DOWNWARD, where a smaller count reads as fewer problems. A shared
    // signature buys no shared patch -- the fixes are independent -- but it buys this: a
    // block of cases that must be FALSE, so a narrowing cannot pass as green. Add to it
    // whenever a clause is hardened; that is what the earlier fixes did not do, which is
    // why each was defeated by the next input shape.
    for (text, path, why) in [
        (
            "rustc -o ./scripts/check-z.rs in.rs",
            "scripts/check-z.rs",
            "a dotted -o OUTPUT read as an invocation -- this bypassed the flag scan",
        ),
        (
            "python3 -c ./scripts/check-z.py x.py",
            "scripts/check-z.py",
            "a dotted -c VALUE read as an invocation",
        ),
        (
            "sh -c \"echo ./scripts/check-z.rs\"",
            "scripts/check-z.rs",
            "a path inside a shell-quoted string read as an invocation",
        ),
        (
            "OUT=./scripts/check-z.rs",
            "scripts/check-z.rs",
            "an assignment VALUE read as an invocation",
        ),
        (
            "echo python3 -X faulthandler scripts/check-z.py",
            "scripts/check-z.py",
            "a runner named as an ARGUMENT to another command read as an invocation -- \
             the flag-scan clause skipped command-position validation while this file's \
             own comment claimed every clause performed it (agent(codex-rev-2683))",
        ),
        (
            "bash -n ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "bash -n is a syntax check: rc 0 and the script's marker ABSENT, measured",
        ),
        (
            "bash -D -x ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "bash -D implies -n and takes no value; a following flag must not make the \
             script look executed",
        ),
        (
            "bash -Dx ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "bash accepts bundled short options, and -D still prevents execution when \
             bundled with -x",
        ),
        (
            "bash -o noexec ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "noexec arrives through -o's VALUE, not the flag, so the flag table alone \
             cannot see it",
        ),
        (
            "python3 -c pass ./scripts/check-z.py",
            "scripts/check-z.py",
            "python -c makes the VALUE the program; the path after it is argv and is \
             never run",
        ),
        (
            "python3 -m noopmod ./scripts/check-z.py",
            "scripts/check-z.py",
            "same for -m: the module is the program",
        ),
        (
            "rustc -V ./scripts/check-z.rs",
            "scripts/check-z.rs",
            "rustc -V prints a version and compiles nothing, yet -V is a NO-VALUE flag \
             -- which is why a two-state table classified it SCHEDULED",
        ),
        (
            "./scripts/check-z.rs.disabled",
            "scripts/check-z.rs",
            "a LONGER path containing the checker as a prefix scheduled it -- renaming \
             a checker to .disabled is a normal way to retire one, so this fires by \
             ACCIDENT rather than by contrivance (agent(hermit-020) relaying codex)",
        ),
        (
            "python3 --nonsense ignored scripts/check-z.py",
            "scripts/check-z.py",
            "an UNRECOGNISED flag: skipping its presumed value can land the cursor on \
             the checker itself, so the inverted default was loud only for the cases \
             I happened to test -- uncertainty now resolves to orphan",
        ),
        (
            "see ./scripts/check-z.rs for details",
            "scripts/check-z.rs",
            "a mid-sentence mention read as an invocation",
        ),
        (
            "        \"python3 scripts/test_validate_stop_paths.py",
            "scripts/test_validate_stop_paths.py",
            "a runner prefix carried as FIXTURE DATA -- hermit#2656, the P0 that turned \
             this checker red on main, reverted once by stripping any trailing quote",
        ),
        (
            r#"fixture='bundle=$("$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh" "$pointer")'"#,
            "ci/verify-hermit-e2e-artifact.sh",
            "a rooted invocation inside a single-quoted shell fixture is dead text",
        ),
    ] {
        assert!(!is_invoked(text, path), "false SCHEDULED (silent direction): {why}");
    }

    // ...and the other half. Every one is a REAL invocation in this repository; if the
    // guard above is tightened until one fails, scheduled checkers become orphans.
    for (text, path, why) in [
        (
            "\t$(SUBMODULE_PROXY) ./scripts/check-n.rs",
            "scripts/check-n.rs",
            "a make variable expansion before the command",
        ),
        (
            "FOO=bar ./ci/node.sh",
            "ci/node.sh",
            "an environment assignment before the command",
        ),
        (
            "python3 scripts/check-x.py --gate",
            "scripts/check-x.py",
            "a DAG command in runner form, after JSON decoding",
        ),
        (
            "n.cmd = \"./ci/verify-x.sh p\".to_string();",
            "ci/verify-x.sh",
            "a command built as a Rust string literal in scripts/validate.rs",
        ),
        (
            "sh -c ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "the SHELLS execute the token after -c, so that token IS an invocation -- \
             the mirror of the python case above, and why the table is per-runner",
        ),
        (
            "bash -norc ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "-norc is a single-hyphen long option, not a bundle containing -n",
        ),
        (
            "bash -noprofile ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "-noprofile is a single-hyphen long option and still executes the script",
        ),
        (
            "bash -login ./scripts/check-z.sh",
            "scripts/check-z.sh",
            "-login is a single-hyphen long option and still executes the script",
        ),
        (
            "cargo build && scripts/check-y.sh --gate",
            "scripts/check-y.sh",
            "a checker CHAINED after another command -- the bare-path clause used to \
             require the START OF A LINE",
        ),
        (
            r#"bundle=$("$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh" "$pointer")"#,
            "ci/verify-hermit-e2e-artifact.sh",
            "the live artifact consumer invokes the verifier through its checkout root",
        ),
    ] {
        assert!(is_invoked(text, path), "false ORPHAN (loud direction): {why}");
    }

    // Pins JSON DECODING, which nothing else reaches: every other case calls is_invoked
    // on text that is already decoded, so dropping the decoder leaves them all green
    // while every DAG command silently changes shape.
    assert_eq!(
        extract_cmds("      \"cmd\": \"python3 scripts/check-x.py --gate\","),
        vec!["python3 scripts/check-x.py --gate".to_string()],
        "a DAG command must reach the matcher as shell text, not as a JSON literal"
    );

    // ⚠️ PINS THE POPULATION RULE, AND THE ASYMMETRY THAT MOTIVATED IT. A convention
    // expressed only as a PREFIX cannot see its own family: `foo-test.sh` asserts and
    // exits nonzero by every standard this file uses, and five were outside the
    // population entirely, so nothing verified that anything ran them -- three were
    // scheduled by NOTHING and are wired into `lint-checks` by this change. The prefix
    // list was also asymmetric: it carried `scripts/test_` but not `ci/test_`, so a
    // Python checker under ci/ was invisible for no reason anyone chose.
    assert!(
        is_checker_path("scripts/core-review-protocol-lint-test.sh"),
        "a -test.sh checker must be in the population"
    );
    assert!(
        is_checker_path("ci/test_audit_test_binary_registration.py"),
        "ci/test_ must be recognised, as scripts/test_ already was"
    );
    assert!(
        is_checker_path("scripts/bisect-probe.rs"),
        "bisect-probe --self-test must stay in the checker population"
    );
    // ⚠️ AND THE LIMIT, MEASURED RATHER THAN ASSUMED. Widening the population to every
    // tracked executable under scripts/ and ci/ was measured 2026-08-26 at 32 checkers
    // scheduled by NOTHING -- runners, libraries and probes, not checkers. That is a
    // standing red, which is how a gate gets switched off. The convention stays a
    // convention; what changed is that it no longer misses its own families.
    assert!(
        !is_checker_path("scripts/lib/helper.sh"),
        "the population must not swallow libraries"
    );
    assert!(
        !is_checker_path("scripts/other-probe.rs"),
        "one self-testing probe must not make every probe a checker"
    );

    // Comment stripping is what separates a real invocation from a mention.
    let sh = "# ./scripts/check-a.sh mentioned in a comment\n./scripts/check-b.sh\n";
    let stripped = strip_comments(sh, "x.sh");
    assert!(!stripped.contains("check-a.sh"), "commented-out shell line survived");
    assert!(stripped.contains("check-b.sh"), "live shell line was dropped");

    let rs = "// see scripts/check-c.rs for the equivalence proof\nrun(\"scripts/check-d.rs\");\n";
    let stripped = strip_comments(rs, "x.rs");
    assert!(!stripped.contains("check-c.rs"), "commented-out rust line survived");
    assert!(stripped.contains("check-d.rs"), "live rust line was dropped");

    // The real case that made this necessary.
    let real = "# Equivalence is TESTED, not asserted -- see check-detcore-backend-abstraction-test.sh:\n";
    assert!(
        strip_comments(real, "check-detcore-backend-abstraction.sh").is_empty(),
        "the comment-only reference that fooled the manual sweep still reads as an invocation"
    );

    // The recipe extractor must take lint-checks and stop before lint-cargo.
    let mk = "lint: lint-checks lint-cargo\n\nlint-checks:\n\t./scripts/check-x.sh\n\t@git diff --check\n\nlint-cargo:\n\t$(CARGO) clippy\n";
    let recipe = lint_checks_recipe(mk);
    assert!(recipe.contains("check-x.sh"), "recipe line missing");
    assert!(!recipe.contains("clippy"), "lint-cargo leaked into the lint-checks recipe");

    // This checker's own ALLOWLIST must not count as scheduling evidence.
    assert!(!expands_frontier(SELF_PATH), "this file's body is being read as invocations");
    assert!(expands_frontier("ci/run-reverie-pin-check.sh"), "a real intermediary stopped expanding");
    assert!(
        ALLOWLIST.iter().all(|(p, _)| *p != SELF_PATH),
        "this checker allowlisting itself would hide the very orphan it reports"
    );

    // ⚠️ A FLAG BETWEEN THE RUNNER AND THE PATH. The real cmd from check.exit_status_class
    // (hermit#2655), which this guard called an orphan while it was genuinely scheduled.
    let flagged = "mkdir -p target/ci && RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
scripts/check-exit-status-class.rs -o target/ci/check-exit-status-class && \
target/ci/check-exit-status-class --gate";
    assert!(
        is_invoked(flagged, "scripts/check-exit-status-class.rs"),
        "a flag between the runner and the path hid a scheduled checker"
    );
    assert!(
        is_invoked("rustc scripts/check-x.rs", "scripts/check-x.rs"),
        "the adjacent form regressed"
    );
    assert!(
        is_invoked("python3 -B scripts/check-y.py", "scripts/check-y.py"),
        "a flag before a python path hid a scheduled checker"
    );
    // ⚠️ THE KNOWN GAP, PINNED RATHER THAN LEFT TO BE REDISCOVERED. A flag taking a
    // SEPARATE value (`-X faulthandler`, `-o out`) stops the scan at the value, so this
    // form still reads as an orphan. Closing it needs a table of which flags consume the
    // next token; a wrong entry there fails SILENTLY, by reading an output as an input.
    // The false orphan is loud and costs one line to fix, so it is the side to be on.
    // If you widen this, keep the `-o` control below passing.
    // ⚠️ THIS PIN IS INVERTED, AS ITS OWN MESSAGE ASKED. It asserted that
    // `-X faulthandler <path>` reads as an ORPHAN and said to update it if the gap ever
    // closed. It has: the scan no longer needs to know that `-X` takes a value, because
    // an unrecognised flag is now ASSUMED to take one.
    assert!(
        is_invoked("python3 -X faulthandler scripts/check-y.py", "scripts/check-y.py"),
        "an unrecognised separate-value flag must no longer hide the script after it"
    );
    // The COST of the loud default, pinned so it is a choice and not a surprise: a
    // boolean missing from a runner's list is assumed to take a value, so the scan steps
    // past the script and reports a FALSE ORPHAN. The remedy is to add the flag, not to
    // widen the default back.
    assert!(
        !is_invoked("python3 --nonsense-bool scripts/check-y.py", "scripts/check-y.py"),
        "an unknown boolean is assumed to take a value; add it to the runner's list"
    );
    // ⚠️ BOTH DIRECTIONS OF ONE FLAG, WHICH IS WHY THE TABLE IS PER-RUNNER (hermit#2681)
    // AND WHY THE DEFAULT IS LOUD. `-O` is a boolean under python and rustc and takes a
    // shell option name under bash. Found by agent(hermit-007), measured by
    // agent(hermit-005).
    assert!(
        !is_invoked("bash -O scripts/check-x.sh", "scripts/check-x.sh"),
        "bash -O takes a shell option name, so the script is not the next token"
    );
    assert!(
        is_invoked("bash -O extglob scripts/check-x.sh", "scripts/check-x.sh"),
        "with its value present the script IS the next token"
    );
    assert!(
        is_invoked("python3 -O scripts/check-y.py", "scripts/check-y.py"),
        "python -O is a boolean, so the same flag differs per runner"
    );
    assert!(
        is_invoked("rustc -O scripts/check-x.rs", "scripts/check-x.rs"),
        "rustc -O is also a boolean (opt-level=2)"
    );
    // ⚠️ THE ORDERING THE ORIGINAL CONTROL MISSED, AND IT WAS A LIVE FALSE
    // "SCHEDULED". `-o` directly after the runner was skipped as an ordinary flag,
    // so the scan landed on its VALUE and read an OUTPUT as an invocation. The
    // control below uses the input-first ordering, where the scan stops at
    // `other.rs` and never reaches `-o`, so it passed while this did not.
    assert!(
        !is_invoked("rustc -o scripts/check-z.rs other.rs", "scripts/check-z.rs"),
        "an output path directly after -o read as an invocation -- a false SCHEDULED, \
         which is the silent direction this guard must never fail in"
    );
    // ⚠️ THE RUNNERS GENUINELY DISAGREE, WHICH IS WHY THE TABLES ARE SPLIT. Each
    // pair below was measured by running the runner against a real script, not
    // inferred: the shells EXECUTE the token after `-c`, python treats it as code
    // and never runs it. One flag, opposite correctness, so one table cannot hold
    // both.
    assert!(
        is_invoked("bash -c scripts/check-z.sh", "scripts/check-z.sh"),
        "bash -c RUNS the token after it (measured: prints RAN), so it is an \
         invocation -- marking -c value-taking for the shells is a false ORPHAN"
    );
    assert!(
        is_invoked("sh -c scripts/check-z.sh", "scripts/check-z.sh"),
        "sh -c RUNS the token after it, exactly as bash does"
    );
    assert!(
        !is_invoked("python3 -c scripts/check-z.py", "scripts/check-z.py"),
        "python3 -c is CODE (measured: SyntaxError, never runs the file), so the \
         token after it is not an invocation -- the opposite of the shells"
    );
    // ⚠️ AND THE REGRESSION THIS SPLIT FIXES, live on main until now: `-l` is a
    // login shell for bash and sh and takes NO value, while rustc's `-l` links a
    // library and DOES. The shared table skipped the script and reported a false
    // ORPHAN.
    assert!(
        is_invoked("bash -l scripts/check-z.sh", "scripts/check-z.sh"),
        "bash -l takes no value (measured: prints RAN), so the script after it \
         must still be seen"
    );
    assert!(
        is_invoked("sh -l scripts/check-z.sh", "scripts/check-z.sh"),
        "sh -l takes no value either"
    );
    assert!(
        is_invoked("rustc -l foo scripts/check-z.rs", "scripts/check-z.rs"),
        "rustc -l DOES take a value, so `foo` must be skipped and the script AFTER \
         it still seen -- that is what the table is for, and the split must not \
         lose it"
    );
    // The shell entries that ARE value-taking, each measured: the path is eaten as
    // the value and never runs, so reporting it scheduled would be a false
    // SCHEDULED -- the silent direction.
    assert!(
        !is_invoked("bash -o scripts/check-z.sh", "scripts/check-z.sh"),
        "bash -o consumes the next token (measured: 'invalid option name')"
    );
    assert!(
        !is_invoked("bash -D scripts/check-z.sh", "scripts/check-z.sh"),
        "bash -D exits after dumping strings without running the script"
    );

    // A value-taking flag must not hide the script that follows its value. Without
    // the table this stops on `2021` and reports a false ORPHAN.
    assert!(
        is_invoked("rustc --edition 2021 scripts/check-x.rs", "scripts/check-x.rs"),
        "a separate-value flag hid the script after it"
    );
    assert!(
        is_invoked("rustc -C opt-level=3 scripts/check-x.rs", "scripts/check-x.rs"),
        "a -C value hid the script after it"
    );
    // ⚠️ THE CONTROL FOR THE SILENT DIRECTION. An OUTPUT path must not read as an
    // invocation: matching it would report an unscheduled checker as scheduled, which is
    // the failure this whole guard exists to prevent and is invisible when wrong.
    assert!(
        !is_invoked("rustc other.rs -o scripts/check-z.rs", "scripts/check-z.rs"),
        "an -o output path was read as an invocation"
    );
    assert!(
        !is_invoked("see scripts/check-w.sh for details", "scripts/check-w.sh"),
        "a mid-sentence mention was read as an invocation"
    );

    println!("PASS: check-checker-scheduling strips comments, keeps invocations, scopes the recipe, and does not read its own allowlist as scheduling");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commented_reference_is_not_an_invocation() {
        let s = strip_comments("# ./scripts/check-a.sh\n./scripts/check-b.sh\n", "x.sh");
        assert!(!s.contains("check-a.sh"));
        assert!(s.contains("check-b.sh"));
    }

    #[test]
    fn rust_line_comments_are_stripped_too() {
        let s = strip_comments("// scripts/check-c.rs\nrun(\"check-d.rs\");\n", "x.rs");
        assert!(!s.contains("check-c.rs"));
        assert!(s.contains("check-d.rs"));
    }

    /// ⚠️ THE FIXTURE CASE, WHICH IS THE ONE THAT REDDENED MAIN. `ci/lint-checks-node.sh`
    /// hands `check_run` a quoted multi-line string standing in for make's output, and
    /// its first line is `"python3 scripts/test_validate_stop_paths.py`. Requiring the
    /// `python3 ` prefix was not enough -- the fixture has it. Measured 2026-08-26:
    /// this exact line made a correctly-allowlisted checker report STALE on main.
    #[test]
    fn a_runner_prefix_inside_a_string_literal_is_not_an_invocation() {
        let fixture = "        \"python3 scripts/test_validate_stop_paths.py";
        assert!(
            !is_invoked(fixture, "scripts/test_validate_stop_paths.py"),
            "a runner prefix quoted as fixture data must not read as an invocation"
        );
    }

    /// ...and the real shapes must still fire, so the guard cannot be over-tightened
    /// into a permanent pass. Each of these is a form this repository actually uses.
    #[test]
    fn genuine_invocations_still_fire_after_the_quote_guard() {
        for real in [
            "\tpython3 scripts/test_validate_stop_paths.py",
            "python3 scripts/test_validate_stop_paths.py",
            "\t./scripts/check-x.sh",
            "\tcd foo && python3 scripts/test_validate_stop_paths.py",
            "\t$(SUBMODULE_PROXY) ./ci/verify-submodules.sh",
        ] {
            let path = if real.contains("check-x") {
                "scripts/check-x.sh"
            } else if real.contains("verify-submodules") {
                "ci/verify-submodules.sh"
            } else {
                "scripts/test_validate_stop_paths.py"
            };
            assert!(is_invoked(real, path), "must still fire: {real}");
        }
    }

    #[test]
    fn recipe_extraction_stops_at_the_next_target() {
        let mk = "lint-checks:\n\t./scripts/check-x.sh\n\nlint-cargo:\n\t$(CARGO) clippy\n";
        let r = lint_checks_recipe(mk);
        assert!(r.contains("check-x.sh"));
        assert!(!r.contains("clippy"));
    }

    #[test]
    fn a_makefile_without_the_recipe_is_an_error_not_an_empty_pass() {
        let r = std::panic::catch_unwind(|| lint_checks_recipe("lint:\n\techo hi\n"));
        assert!(r.is_err(), "a missing lint-checks recipe must refuse, not return empty");
    }

    /// The regression that scheduling this checker introduced: its own ALLOWLIST
    /// literals were read as invocations, so all five allowlisted checkers
    /// reported STALE and `make lint-checks` exited 1.
    #[test]
    fn this_checkers_own_body_is_not_an_invocation_source() {
        assert!(!expands_frontier(SELF_PATH));
        assert!(expands_frontier("scripts/validate.rs"));
        assert!(expands_frontier("ci/run-reverie-pin-check.sh"));
    }

    /// Excluding the body must not become excluding the file: it is a checker
    /// like any other and still has to be scheduled by something.
    #[test]
    fn this_checker_does_not_allowlist_itself() {
        assert!(ALLOWLIST.iter().all(|(p, _)| *p != SELF_PATH));
    }

    #[test]
    fn every_allowlist_entry_carries_a_nonempty_reason() {
        for (path, reason) in ALLOWLIST {
            assert!(!path.is_empty());
            assert!(
                reason.len() > 40,
                "allowlist entry {path} needs a real reason, not a placeholder"
            );
        }
    }

    // The regression that motivated is_invoked: a filename quoted inside a live
    // (non-comment) diagnostic message is NOT an invocation. ci/lint-checks-node.sh
    // echoes exactly this shape, and a substring match read it as scheduling.
    #[test]
    fn a_name_quoted_mid_sentence_is_not_an_invocation() {
        let echoed = "        echo '  contract in scripts/test_validate_stop_paths.py exercises the REAL parent' >&2";
        assert!(!is_invoked(echoed, "scripts/test_validate_stop_paths.py"));
    }

    #[test]
    fn the_shapes_that_do_run_a_script_all_count() {
        assert!(is_invoked("\t./scripts/check-x.sh", "scripts/check-x.sh"));
        assert!(is_invoked(
            "\tpython3 scripts/test_x.py",
            "scripts/test_x.py"
        ));
        assert!(is_invoked(
            "\t$(SUBMODULE_PROXY) ./ci/run-x.sh",
            "ci/run-x.sh"
        ));
        assert!(is_invoked(
            "bundle=$(\"$ROOT_DIR/ci/verify-x.sh\" \"$pointer\")",
            "ci/verify-x.sh"
        ));
        assert!(is_invoked(
            "bundle=$(\"${ROOT_DIR}/ci/verify-x.sh\" \"$pointer\")",
            "ci/verify-x.sh"
        ));
        assert!(is_invoked(
            "printf '%s' 'fixture'; bundle=$(\"$ROOT_DIR/ci/verify-x.sh\" \"$pointer\")",
            "ci/verify-x.sh"
        ));
        assert!(is_invoked(
            "printf \\'; bundle=$(\"$ROOT_DIR/ci/verify-x.sh\" \"$pointer\")",
            "ci/verify-x.sh"
        ));
        assert!(
            !is_invoked(
                "VERIFY=\"$ROOT_DIR/ci/verify-x.sh\"",
                "ci/verify-x.sh"
            ),
            "assigning a rooted checker path is not execution"
        );
        assert!(
            !is_invoked(
                r##"let fixture = r#"bundle=$("$ROOT_DIR/ci/verify-x.sh" "$pointer")"#;"##,
                "ci/verify-x.sh"
            ),
            "a rooted invocation inside Rust fixture data is not execution"
        );
        let commented = strip_comments(
            "# bundle=$(\"$ROOT_DIR/ci/verify-x.sh\" \"$pointer\")",
            "fixture.sh",
        );
        assert!(
            !is_invoked(&commented, "ci/verify-x.sh"),
            "a commented-out rooted command is not execution"
        );
        let mutated_wrapper = r#"
fixture='
bundle=$("$ROOT_DIR/ci/verify-x.sh" "$pointer")'
bundle=/nonexistent
"#;
        assert!(
            !is_invoked(mutated_wrapper, "ci/verify-x.sh"),
            "dead single-quoted fixture text cannot replace the real consumer call"
        );
        let ansi_c_fixture = r#"
fixture=$'escaped apostrophe: \'
bundle=$("$ROOT_DIR/ci/verify-x.sh" "$pointer")'
bundle=/nonexistent
"#;
        assert!(
            !is_invoked(ansi_c_fixture, "ci/verify-x.sh"),
            "an escaped quote inside multiline ANSI-C fixture data must not end the quote"
        );
        let split_ansi_c_fixture = r#"
fixture=$\
'escaped apostrophe: \'
bundle=$("$ROOT_DIR/ci/verify-x.sh" "$pointer")'
bundle=/nonexistent
"#;
        assert!(
            !is_invoked(split_ansi_c_fixture, "ci/verify-x.sh"),
            "a backslash-newline may split the `$'` ANSI-C quote opener"
        );
        let split_locale_fixture = r#"
fixture=$\
"$ROOT_DIR/ci/verify-x.sh"
bundle=/nonexistent
"#;
        assert!(
            !is_invoked(split_locale_fixture, "ci/verify-x.sh"),
            "a backslash-newline may split the `$\"` locale-string quote opener"
        );
        let double_quoted_fixture = r#"
fixture="escaped quote: \"
bundle=\$(\"$ROOT_DIR/ci/verify-x.sh\" \"$pointer\")"
bundle=/nonexistent
"#;
        assert!(
            !is_invoked(double_quoted_fixture, "ci/verify-x.sh"),
            "an escaped quote inside multiline double-quoted fixture data must not end the quote"
        );
        let single_quoted_heredoc = r#"
cat <<'FIXTURE'
bundle=$("$ROOT_DIR/ci/verify-x.sh" "$pointer")
FIXTURE
bundle=$("$ROOT_DIR/ci/verify-y.sh" "$pointer")
"#;
        assert!(
            !is_invoked(single_quoted_heredoc, "ci/verify-x.sh"),
            "a rooted command-shaped line in a single-quoted heredoc is inert data"
        );
        assert!(
            is_invoked(single_quoted_heredoc, "ci/verify-y.sh"),
            "a single-quoted heredoc terminator must restore executable shell context"
        );
        let double_quoted_heredoc = r#"
cat <<"FIXTURE"
bundle=$("$ROOT_DIR/ci/verify-x.sh" "$pointer")
FIXTURE
bundle=$("$ROOT_DIR/ci/verify-y.sh" "$pointer")
"#;
        assert!(
            !is_invoked(double_quoted_heredoc, "ci/verify-x.sh"),
            "a rooted command-shaped line in a double-quoted heredoc is inert data"
        );
        assert!(
            is_invoked(double_quoted_heredoc, "ci/verify-y.sh"),
            "a double-quoted heredoc terminator must restore executable shell context"
        );
        let unsupported_bare_heredoc = r#"
cat <<FIXTURE
ignored
FIXTURE
bundle=$("$ROOT_DIR/ci/verify-x.sh" "$pointer")
"#;
        assert!(
            !is_invoked(unsupported_bare_heredoc, "ci/verify-x.sh"),
            "an unsupported bare heredoc must fail closed rather than guess"
        );
        assert!(
            !is_invoked(
                "bundle=$('$ROOT_DIR/ci/verify-x.sh' \"$pointer\")",
                "ci/verify-x.sh"
            ),
            "single quotes prevent ROOT_DIR expansion"
        );
        // Compiled rather than interpreted, and split across a continuation line --
        // how run-reverie-pin-check.sh reaches both pin checkers.
        assert!(is_invoked(
            "    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \\\n        scripts/check-reverie-pin.rs -o \"$checker\"",
            "scripts/check-reverie-pin.rs"
        ));
        // ⚠️ THE PER-RUNNER SPLIT ITSELF, PINNED HERE RATHER THAN ONLY IN
        // `self_test`. Re-merging the tables leaves `self_test` red but left THIS
        // suite green, so the change had no pin in the harness most people run.
        // Found by agent(hermit-004), who mutated `-l` back into
        // SHELL_VALUE_FLAGS and got "12 passed; 0 failed".
        //
        // A shell flag that takes NO value: the path after it IS the script.
        // Measured by execution -- `bash -l ./x.sh` and `sh -l ./x.sh` print RAN.
        assert!(is_invoked("\tbash -l scripts/check-x.sh", "scripts/check-x.sh"));
        assert!(is_invoked("\tsh -l scripts/check-x.sh", "scripts/check-x.sh"));
        // ...and the value-taking side, so the shell table cannot simply be
        // EMPTIED to satisfy the two above -- that would trade the loud failure
        // (false orphan) for the silent one (false scheduled).
        // Measured: `bash -o ./x.sh` answers "invalid option name", the path was
        // eaten as the value; `python3 -c ./x.sh` is a SyntaxError, never run.
        assert!(!is_invoked("\tbash -o scripts/check-x.sh", "scripts/check-x.sh"));
        assert!(!is_invoked("\tpython3 -c scripts/check-x.py", "scripts/check-x.py"));
    }

    #[test]
    fn ansi_c_quote_openers_follow_shell_line_continuation_rules() {
        let mut split = QuoteLexer::new();
        split.scan("$\\");
        split.scan("\n");
        split.scan("'escaped apostrophe: \\'");
        assert_eq!(split.state, QuoteState::AnsiC);

        let mut intervening_escape = QuoteLexer::new();
        intervening_escape.scan("$\\x'escaped apostrophe: \\'");
        assert_eq!(intervening_escape.state, QuoteState::Unquoted);
    }

    // A DAG `description` is prose and routinely names checkers it does not run.
    #[test]
    fn only_cmd_fields_seed_scheduling_not_descriptions() {
        let dag = r#"{"steps":[
          {"job":"a","description":"this node supersedes scripts/check-ghost.sh entirely",
           "cmd": "./scripts/check-real.sh"}]}"#;
        let cmds = extract_cmds(dag);
        assert_eq!(cmds.len(), 1, "one cmd expected, got {cmds:?}");
        assert!(cmds[0].contains("check-real.sh"));
        assert!(
            !cmds[0].contains("check-ghost.sh"),
            "a checker named only in prose must not read as scheduled"
        );
    }
}
