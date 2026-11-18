/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Skills imported from agent-utils (https://github.com/rrnewton/agent-utils),
//! which is the source of truth for every skill in `IMPORTED`.
//!
//! The copies at `.claude/skills/<name>/SKILL.md` are generated and never edited
//! here: each is the upstream `skills/<name>/SKILL.md` with one generated-from
//! line inserted after its frontmatter. `scripts/import-agent-utils-skills.rs`
//! regenerates them from an agent-utils checkout and records the source commit
//! and each upstream file's SHA-256 in `LOCK`. `check` (run by
//! `scripts/check-skill-discovery.rs`) requires every copy to reproduce its
//! recorded upstream bytes exactly and, when an agent-utils checkout holding the
//! recorded commit is available (`AGENT_UTILS_CHECKOUT`, or an initialized
//! `agent-utils` submodule), to equal a fresh regeneration from it. Without such
//! a checkout the check trusts the lock: a hand edit that also rewrites the
//! lock's hash passes there, and only the upstream comparison catches it. The
//! primary repository has no agent-utils checkout, so runs there are always in
//! that lock-only mode. To change one of these skills, change it in agent-utils
//! and rerun the import.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// The imported skills, in the order `LOCK` lists them.
pub const IMPORTED: &[&str] = &[
    "testing-principles",
    "testing-with-buck",
    "testing-with-dagrun",
];

/// The import record, relative to the repository root.
pub const LOCK: &str = "scripts/agent-utils-skills.lock";

const IMPORTER: &str = "scripts/import-agent-utils-skills.rs";

/// The recorded source commit and, per skill, the SHA-256 of its upstream file.
pub struct Lock {
    pub commit: String,
    pub skills: Vec<(String, String)>,
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn read_lock(root: &Path) -> Result<Lock, String> {
    let path = root.join(LOCK);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut commit = None;
    let mut skills = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["commit", hash] if commit.is_none() && is_hex(hash, 40) => {
                commit = Some((*hash).to_owned());
            }
            ["skill", name, hash] if is_hex(hash, 64) => {
                skills.push(((*name).to_owned(), (*hash).to_owned()));
            }
            _ => return Err(format!("{LOCK}:{}: unrecognized line: {line}", index + 1)),
        }
    }
    let commit = commit.ok_or_else(|| format!("{LOCK} records no commit"))?;
    Ok(Lock { commit, skills })
}

/// The generated-from line, with the blank line that separates it from the
/// frontmatter.
fn header(commit: &str, name: &str) -> String {
    format!(
        "\n<!-- Generated from agent-utils {commit} skills/{name}/SKILL.md; do not edit. \
         Change it in agent-utils, then run {IMPORTER}. -->\n"
    )
}

/// Byte offset just past the frontmatter's closing `---` line.
fn frontmatter_end(text: &str) -> Option<usize> {
    let rest = text.strip_prefix("---\n")?;
    Some("---\n".len() + rest.find("\n---\n")? + "\n---\n".len())
}

/// The imported copy of an upstream `SKILL.md`.
pub fn render(upstream: &str, commit: &str, name: &str) -> Result<String, String> {
    let end = frontmatter_end(upstream)
        .ok_or_else(|| format!("agent-utils skills/{name}/SKILL.md has no frontmatter"))?;
    Ok(format!(
        "{}{}{}",
        &upstream[..end],
        header(commit, name),
        &upstream[end..]
    ))
}

/// The upstream bytes an imported copy was generated from.
fn recover(copy: &str, commit: &str, name: &str) -> Result<String, String> {
    let end = frontmatter_end(copy).ok_or("has no frontmatter")?;
    let body = copy[end..]
        .strip_prefix(&header(commit, name))
        .ok_or_else(|| {
            format!("lacks the generated-from line for agent-utils {commit} after its frontmatter")
        })?;
    Ok(format!("{}{body}", &copy[..end]))
}

pub fn sha256(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run sha256sum: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("sha256sum has no standard input")?
        .write_all(bytes)
        .map_err(|error| format!("cannot write to sha256sum: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("sha256sum failed: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    match text.split_whitespace().next() {
        Some(hash) if output.status.success() && is_hex(hash, 64) => Ok(hash.to_owned()),
        _ => Err(format!("sha256sum printed no digest: {text}")),
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {} failed in {}",
            args.join(" "),
            dir.display()
        ))
    }
}

/// Upstream `skills/<name>/SKILL.md` at `commit` in an agent-utils checkout.
pub fn upstream_file(checkout: &Path, commit: &str, name: &str) -> Result<String, String> {
    String::from_utf8(git(
        checkout,
        &["show", &format!("{commit}:skills/{name}/SKILL.md")],
    )?)
    .map_err(|_| format!("agent-utils skills/{name}/SKILL.md is not UTF-8"))
}

/// The files of upstream `skills/<name>/` at `commit`.
pub fn upstream_entries(checkout: &Path, commit: &str, name: &str) -> Result<Vec<String>, String> {
    let listing = git(
        checkout,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            commit,
            &format!("skills/{name}/"),
        ],
    )?;
    Ok(String::from_utf8_lossy(&listing)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn holds_commit(checkout: &Path, commit: &str) -> bool {
    checkout.join(".git").exists()
        && git(
            checkout,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )
        .is_ok()
}

/// Verify every imported copy against `LOCK`, and against agent-utils itself
/// when a checkout holding the recorded commit is available: `explicit`, else
/// `AGENT_UTILS_CHECKOUT`, else the `agent-utils` submodule. Returns a summary.
pub fn check(root: &Path, explicit: Option<&Path>) -> Result<String, String> {
    let lock = read_lock(root)?;
    let names: Vec<&str> = lock.skills.iter().map(|(name, _)| name.as_str()).collect();
    if names != IMPORTED {
        return Err(format!(
            "{LOCK} lists {names:?}, expected {IMPORTED:?}; rerun {IMPORTER}"
        ));
    }
    // A checkout someone named (the importer's argument, or AGENT_UTILS_CHECKOUT)
    // must hold the commit; only the submodule fallback may be absent.
    let named = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => env::var_os("AGENT_UTILS_CHECKOUT")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    };
    let upstream = match named {
        Some(path) if holds_commit(&path, &lock.commit) => Some(path),
        Some(path) => {
            return Err(format!(
                "{} does not hold agent-utils {}",
                path.display(),
                lock.commit
            ));
        }
        None => Some(root.join("agent-utils")).filter(|path| holds_commit(path, &lock.commit)),
    };
    for (name, recorded) in &lock.skills {
        let dir = root.join(".claude/skills").join(name);
        let mut entries: Vec<String> = fs::read_dir(&dir)
            .map_err(|error| format!("cannot read {}: {error}", dir.display()))?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()
            .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        entries.sort();
        if entries != ["SKILL.md"] {
            return Err(format!(
                "{} holds {entries:?}; an imported skill holds only its generated SKILL.md",
                dir.display()
            ));
        }
        let path = dir.join("SKILL.md");
        let copy = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let original = recover(&copy, &lock.commit, name)
            .map_err(|error| format!("{} {error}", path.display()))?;
        let actual = sha256(original.as_bytes())?;
        if &actual != recorded {
            return Err(format!(
                "{} was edited by hand: its upstream part hashes to {actual}, but {LOCK} records \
                 {recorded}. Change the skill in agent-utils, then run {IMPORTER}.",
                path.display()
            ));
        }
        if let Some(checkout) = &upstream {
            let listed = upstream_entries(checkout, &lock.commit, name)?;
            if listed != [format!("skills/{name}/SKILL.md")] {
                return Err(format!(
                    "agent-utils {} skills/{name}/ holds {listed:?}; only a lone SKILL.md is \
                     imported",
                    lock.commit
                ));
            }
            let fresh = render(
                &upstream_file(checkout, &lock.commit, name)?,
                &lock.commit,
                name,
            )?;
            if fresh != copy {
                return Err(format!(
                    "{} differs from agent-utils {} skills/{name}/SKILL.md; run {IMPORTER}",
                    path.display(),
                    lock.commit
                ));
            }
        }
    }
    Ok(match upstream {
        Some(checkout) => format!(
            "{} imported agent-utils skills equal agent-utils {} in {}",
            IMPORTED.len(),
            lock.commit,
            checkout.display()
        ),
        None => format!(
            "{} imported agent-utils skills match {LOCK}; the comparison with agent-utils itself \
             was skipped because no checkout holds {} (set AGENT_UTILS_CHECKOUT)",
            IMPORTED.len(),
            lock.commit
        ),
    })
}

/// Round-trip and tamper cases for `render` and `recover`.
pub fn regression_test() -> Result<(), String> {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let upstream = "---\nname: x\ndescription: \"d\"\n---\n\n# X\n\nBody.\n";
    let copy = render(upstream, commit, "x")?;
    if recover(&copy, commit, "x")? != upstream {
        return Err("agent-utils import: render and recover do not round-trip".to_owned());
    }
    let other = "fedcba9876543210fedcba9876543210fedcba98";
    if recover(&copy, other, "x").is_ok() || recover(upstream, commit, "x").is_ok() {
        return Err("agent-utils import: recover accepted a missing or stale header".to_owned());
    }
    if render("# no frontmatter\n", commit, "x").is_ok() {
        return Err("agent-utils import: render accepted a file without frontmatter".to_owned());
    }
    Ok(())
}
