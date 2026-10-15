// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! A small runner for detcore/tests/lit's fixtures, which internal Hermit runs
//! through LLVM's lit and FileCheck and which nothing here ran before.
//!
//! It implements the part of lit those fixtures use, and refuses the rest:
//!
//! - **Keywords.** `RUN:` lines, with `\` continuations, and `XFAIL: *`. Any
//!   other lit keyword (`REQUIRES:`, `UNSUPPORTED:`, `ALLOW_RETRIES:`, `END.`,
//!   `DEFINE:`, `REDEFINE:`, or an `XFAIL:` naming features) is refused.
//! - **Substitutions.** `%hermit`, `%me`, `%s` and `%%`, as the internal Buck
//!   rules define them (`%me` is the fixture's directory built as a program).
//!   Any other `%` name is refused.
//! - **The script.** lit's external-shell form: bash runs
//!   `set -o pipefail; { RUN1; } && { RUN2; } ...` in a fresh directory, and
//!   the test passes when it exits 0 and every FileCheck in it matched.
//! - **FileCheck.** Allowed only as the last command of a pipeline (after `|`
//!   or `|&`) with `%s` and an optional `--check-prefix`. The script records
//!   what each one reads, and [`filecheck`] checks it after the script ends.
//!   The verdict is the same as running FileCheck in place: a mismatch fails
//!   the test, and so does a FileCheck that never ran.
//! - **XFAIL.** `XFAIL: *` turns a failure into a pass and a pass into a
//!   failure (lit's XPASS). A refusal stays a refusal: it is never an expected
//!   failure.
//!
//! A refused fixture fails its test as unsupported. Nothing outside the subset
//! can pass by being misread.

#[path = "lit/filecheck.rs"]
mod filecheck;
#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// The verdict on one fixture.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail(String),
    /// The fixture uses something outside the supported subset.
    Unsupported(String),
}

/// One fixture's lit directives.
#[derive(Debug, PartialEq, Eq)]
struct LitTest {
    runs: Vec<String>,
    xfail: bool,
}

const REFUSED_KEYWORDS: [&str; 6] = [
    "REQUIRES:",
    "UNSUPPORTED:",
    "ALLOW_RETRIES:",
    "END.",
    "DEFINE:",
    "REDEFINE:",
];

/// TEXT's lit directives, or why they are outside the supported subset.
fn parse_test(text: &str) -> Result<LitTest, String> {
    let mut runs: Vec<String> = Vec::new();
    let mut xfail = false;
    let mut continuing = false;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if let Some(keyword) = REFUSED_KEYWORDS.iter().find(|k| line.contains(*k)) {
            return Err(format!(
                "line {number}: lit keyword {keyword} is not supported"
            ));
        }
        if let Some((_, rest)) = line.split_once("XFAIL:") {
            if rest.trim() != "*" {
                return Err(format!("line {number}: only `XFAIL: *` is supported"));
            }
            xfail = true;
            continue;
        }
        let Some((_, rest)) = line.split_once("RUN:") else {
            if continuing {
                return Err(format!(
                    "line {number}: a RUN: continuation has no RUN: line"
                ));
            }
            continue;
        };
        let rest = rest.trim();
        let (command, continues) = match rest.strip_suffix('\\') {
            Some(command) => (command.trim_end(), true),
            None => (rest, false),
        };
        if continuing {
            let last = runs.last_mut().unwrap();
            last.push(' ');
            last.push_str(command);
        } else {
            runs.push(command.to_owned());
        }
        continuing = continues;
    }
    if continuing {
        return Err("the last RUN: line ends with a continuation".into());
    }
    if runs.is_empty() {
        return Err("no RUN: line".into());
    }
    Ok(LitTest { runs, xfail })
}

/// One FileCheck in a RUN line, in the order the RUN lines name them.
#[derive(Debug, PartialEq, Eq)]
struct FileCheckCall {
    prefix: String,
}

/// RUN with each FileCheck replaced by a call that records its input, the
/// FileChecks it names appended to CALLS.
fn rewrite_filechecks(run: &str, calls: &mut Vec<FileCheckCall>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = run;
    while let Some(at) = find_word(rest, "FileCheck") {
        let before = &rest[..at];
        let piped = before.trim_end();
        let after_pipe = piped.strip_suffix("|&").or_else(|| piped.strip_suffix('|'));
        if after_pipe.is_none_or(|left| left.ends_with('|')) {
            return Err(format!(
                "FileCheck must be the last command of a pipeline in `{run}`"
            ));
        }
        let tail = &rest[at + "FileCheck".len()..];
        let end = tail.find([';', '&', '|']).unwrap_or(tail.len());
        let mut prefix = None;
        let mut check_file = false;
        let mut words = tail[..end].split_whitespace();
        while let Some(word) = words.next() {
            let value = match word {
                "%s" if !check_file => {
                    check_file = true;
                    continue;
                }
                "--check-prefix" | "-check-prefix" => words.next(),
                _ => word
                    .strip_prefix("--check-prefix=")
                    .or_else(|| word.strip_prefix("-check-prefix=")),
            };
            match (value, prefix) {
                (Some(value), None) => prefix = Some(value.to_owned()),
                _ => {
                    return Err(format!(
                        "FileCheck argument {word:?} is not supported in `{run}`"
                    ));
                }
            }
        }
        if !check_file {
            return Err(format!("FileCheck must read %s in `{run}`"));
        }
        out.push_str(before);
        out.push_str(&format!("__lit_filecheck {}", calls.len()));
        calls.push(FileCheckCall {
            prefix: prefix.unwrap_or_else(|| "CHECK".to_owned()),
        });
        rest = &tail[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The byte offset of WORD in TEXT where it stands as a whole word.
fn find_word(text: &str, word: &str) -> Option<usize> {
    let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
    let mut start = 0;
    while let Some(offset) = text[start..].find(word) {
        let at = start + offset;
        let end = at + word.len();
        if boundary(text[..at].chars().next_back()) && boundary(text[end..].chars().next()) {
            return Some(at);
        }
        start = end;
    }
    None
}

/// RUN with each `%name` replaced by its shell-quoted value from SUBSTITUTIONS.
fn substitute(run: &str, substitutions: &[(&str, &Path)]) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = run;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(after) = after.strip_prefix('%') {
            out.push('%');
            rest = after;
            continue;
        }
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_len];
        let (_, value) = substitutions
            .iter()
            .find(|(key, _)| *key == name)
            .ok_or_else(|| format!("substitution %{name} is not supported in `{run}`"))?;
        out.push_str(&shell_words::quote(value.to_str().unwrap()));
        rest = &after[name_len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The bash script lit's external shell would run for TEST, with the
/// FileChecks it records.
fn script(
    test: &LitTest,
    substitutions: &[(&str, &Path)],
) -> Result<(String, Vec<FileCheckCall>), String> {
    let mut calls = Vec::new();
    let mut commands = Vec::new();
    for run in &test.runs {
        let rewritten = rewrite_filechecks(run, &mut calls)?;
        commands.push(substitute(&rewritten, substitutions)?);
    }
    let script = format!(
        "set -o pipefail\n\
         __lit_filecheck() {{\n  \
           if [ -e \"$LIT_FILECHECK_DIR/$1\" ]; then\n    \
             echo \"lit: FileCheck $1 ran twice\" >&2\n    \
             return 2\n  \
           fi\n  \
           cat > \"$LIT_FILECHECK_DIR/$1\"\n\
         }}\n\
         {{ {}; }}\n",
        commands.join("; } &&\n{ ")
    );
    Ok((script, calls))
}

/// Run the fixture at PATH with SUBSTITUTIONS (`s` is added here).
fn run_fixture(path: &Path, substitutions: &[(&str, &Path)]) -> Outcome {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => return Outcome::Fail(format!("{}: {error}", path.display())),
    };
    let test = match parse_test(&text) {
        Ok(test) => test,
        Err(error) => return Outcome::Unsupported(error),
    };
    let mut all: Vec<(&str, &Path)> = substitutions.to_vec();
    all.push(("s", path));
    let (script, calls) = match script(&test, &all) {
        Ok(prepared) => prepared,
        Err(error) => return Outcome::Unsupported(error),
    };
    let mut checks = Vec::new();
    for call in &calls {
        match filecheck::parse(&text, &call.prefix) {
            Ok(directives) => checks.push(directives),
            Err(error) => return Outcome::Unsupported(error),
        }
    }

    let label = format!("lit fixture {}", path.display());
    let (output, recorded) = hermit_test::hermit_output_with(&label, || {
        let scratch = tempfile::tempdir().expect("failed to create the fixture's directory");
        let recorded = scratch.path().join("filecheck");
        fs::create_dir(&recorded).unwrap();
        let work = scratch.path().join("work");
        fs::create_dir(&work).unwrap();
        let output = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .current_dir(&work)
            .env("LIT_FILECHECK_DIR", &recorded)
            .output()
            .expect("failed to run bash");
        let inputs: Vec<Option<String>> = (0..calls.len())
            .map(|index| fs::read_to_string(recorded.join(index.to_string())).ok())
            .collect();
        (output, inputs)
    });
    let outcome = if !output.status.success() {
        Outcome::Fail(format!(
            "the script exited with {}\nscript:\n{script}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    } else {
        calls
            .iter()
            .zip(&checks)
            .zip(&recorded)
            .enumerate()
            .find_map(|(index, ((call, directives), input))| match input {
                None => Some(Outcome::Fail(format!(
                    "FileCheck {index} (prefix {}) never ran\nscript:\n{script}",
                    call.prefix
                ))),
                Some(input) => filecheck::check(directives, input).err().map(|error| {
                    Outcome::Fail(format!(
                        "FileCheck {index} (prefix {}): {error}",
                        call.prefix
                    ))
                }),
            })
            .unwrap_or(Outcome::Pass)
    };
    match (test.xfail, outcome) {
        (true, Outcome::Fail(_)) => Outcome::Pass,
        (true, Outcome::Pass) => Outcome::Fail("XPASS: an XFAIL fixture passed".into()),
        (_, outcome) => outcome,
    }
}

/// The repository's detcore/tests/lit.
fn lit_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../detcore/tests/lit")
}

/// The fixtures under ROOT, sorted: the files the internal BUCK file's globs
/// name as lit tests (`*.test`, `scheduler_strategies/*.sh`,
/// `*/main.{c,cpp,rs,go}` and `*/*.lit`).
fn fixtures(root: &Path) -> Vec<PathBuf> {
    let entries = |directory: &Path| {
        let mut paths: Vec<PathBuf> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        paths
    };
    let extension = |path: &Path| path.extension().and_then(|e| e.to_str()).map(str::to_owned);
    let mut found = Vec::new();
    for path in entries(root) {
        if path.is_dir() {
            let scheduler = path.file_name().unwrap() == "scheduler_strategies";
            for inner in entries(&path) {
                let name = inner.file_name().unwrap().to_str().unwrap();
                let wanted = if scheduler {
                    extension(&inner).as_deref() == Some("sh")
                } else {
                    ["main.c", "main.cpp", "main.rs", "main.go"].contains(&name)
                        || extension(&inner).as_deref() == Some("lit")
                };
                if wanted && inner.is_file() {
                    found.push(inner);
                }
            }
        } else if extension(&path).as_deref() == Some("test") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// Every fixture is within the supported subset: its lit directives parse,
/// its RUN lines name only supported substitutions and FileCheck forms, and
/// each FileCheck's prefix has directives the matcher accepts. Nothing here
/// runs a fixture; it proves no fixture would be refused as unsupported.
#[test]
fn every_lit_fixture_is_within_the_supported_subset() {
    let root = lit_root();
    let fixtures = fixtures(&root);
    // 26 main.{rs,c,go} files, 46 .lit, 6 .test and 2 scheduler scripts.
    assert_eq!(fixtures.len(), 80, "{fixtures:#?}");
    let placeholder = Path::new("/placeholder");
    let substitutions = [
        ("hermit", placeholder),
        ("me", placeholder),
        ("s", placeholder),
    ];
    let mut checked = 0;
    for path in &fixtures {
        let text = fs::read_to_string(path).unwrap();
        let test = parse_test(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let (_, calls) = script(&test, &substitutions)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        for call in &calls {
            filecheck::parse(&text, &call.prefix)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            checked += 1;
        }
    }
    // `grep -o '\bFileCheck\b'` over the same files' RUN: lines also counts 36.
    assert_eq!(checked, 36);
}

#[cfg(test)]
mod runner {
    use super::*;

    /// Run fixture TEXT, written to a temporary file, with `%me` naming ME.
    fn run(text: &str, me: &Path) -> Outcome {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.lit");
        fs::write(&path, text).unwrap();
        run_fixture(&path, &[("me", me), ("hermit", Path::new("/bin/false"))])
    }

    fn echo() -> &'static Path {
        Path::new("/bin/echo")
    }

    #[test]
    fn a_matching_pipeline_passes() {
        let text = "RUN: printf 'a\\nb\\n\\nc\\n' | FileCheck %s\n\
                    CHECK: a\nCHECK-NEXT: b\nCHECK-EMPTY:\nCHECK-NEXT: c\n";
        assert_eq!(run(text, echo()), Outcome::Pass);
    }

    #[test]
    fn a_mismatch_fails() {
        let text = "RUN: printf 'a\\nc\\n' | FileCheck %s\nCHECK: a\nCHECK-NEXT: b\n";
        assert!(matches!(run(text, echo()), Outcome::Fail(_)));
    }

    #[test]
    fn substitutions_reach_the_shell_quoted() {
        let directory = tempfile::tempdir().unwrap();
        let me = directory.path().join("a program");
        fs::write(&me, "#!/bin/sh\necho \"me ran: $1\"\n").unwrap();
        let executable = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        fs::set_permissions(&me, executable).unwrap();
        let text = "RUN: %me 100%% | FileCheck %s\nCHECK: me ran: 100%\n";
        assert_eq!(run(text, &me), Outcome::Pass);
    }

    #[test]
    fn a_failing_command_fails_even_before_a_matching_filecheck() {
        // pipefail: the producer's status is the pipeline's.
        let text = "RUN: { echo a; false; } | FileCheck %s\nCHECK: a\n";
        assert!(matches!(run(text, echo()), Outcome::Fail(_)));
        let text = "RUN: false\nRUN: echo a | FileCheck %s\nCHECK: a\n";
        assert!(matches!(run(text, echo()), Outcome::Fail(_)));
        let text = "RUN: sh -c 'exit 42'; [ \"$?\" == 42 ]\n";
        assert_eq!(run(text, echo()), Outcome::Pass);
    }

    #[test]
    fn prefixes_select_their_own_directives() {
        let text = "RUN: echo one | FileCheck --check-prefix=FIRST %s\n\
                    RUN: echo two |& FileCheck %s --check-prefix SECOND\n\
                    FIRST: one\nSECOND: two\n";
        assert_eq!(run(text, echo()), Outcome::Pass);
        let swapped = "RUN: echo two | FileCheck --check-prefix=FIRST %s\nFIRST: one\n";
        assert!(matches!(run(swapped, echo()), Outcome::Fail(_)));
    }

    #[test]
    fn continuations_join_run_lines() {
        let text = "RUN: echo \\\nRUN:   joined | FileCheck %s\nCHECK: joined\n";
        assert_eq!(run(text, echo()), Outcome::Pass);
    }

    #[test]
    fn xfail_inverts_pass_and_fail_but_not_a_refusal() {
        assert_eq!(run("XFAIL: *\nRUN: false\n", echo()), Outcome::Pass);
        assert!(matches!(
            run("XFAIL: *\nRUN: true\n", echo()),
            Outcome::Fail(message) if message.contains("XPASS")
        ));
        assert!(matches!(
            run("XFAIL: *\nRUN: echo %t\n", echo()),
            Outcome::Unsupported(_)
        ));
    }

    #[test]
    fn a_filecheck_that_never_runs_fails() {
        let text = "RUN: true || echo a | FileCheck %s\nCHECK: a\n";
        assert!(matches!(
            run(text, echo()),
            Outcome::Fail(message) if message.contains("never ran")
        ));
    }

    #[test]
    fn syntax_outside_the_subset_is_refused() {
        for text in [
            "RUN: echo %t\n",
            "RUN: echo %{x}\n",
            "REQUIRES: x86\nRUN: true\n",
            "UNSUPPORTED: linux\nRUN: true\n",
            "XFAIL: linux\nRUN: true\n",
            "ALLOW_RETRIES: 2\nRUN: true\n",
            "RUN: true \\\n",
            "no run line\n",
            "RUN: FileCheck %s < /dev/null\nCHECK: a\n",
            "RUN: echo a || FileCheck %s\nCHECK: a\n",
            "RUN: echo a | FileCheck %s --strict-whitespace\nCHECK: a\n",
            "RUN: echo a | FileCheck other.txt\nCHECK: a\n",
            "RUN: echo a | FileCheck %s\nCHECK: a\nCHECK-NOT: b\n",
            "RUN: echo a | FileCheck %s --check-prefix=NONE\nCHECK: a\n",
        ] {
            assert!(
                matches!(run(text, echo()), Outcome::Unsupported(_)),
                "{text:?} must be refused"
            );
        }
    }

    #[test]
    fn find_word_ignores_longer_words() {
        assert_eq!(find_word("xFileCheck FileCheck", "FileCheck"), Some(11));
        assert_eq!(find_word("FileChecker", "FileCheck"), None);
    }
}
