/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keep every `dagrun` library dependency on the commit the `agent-utils`
//! gitlink records.
//!
//! `dagrun` is a git dependency rather than a path into the `agent-utils`
//! submodule, so a clone without submodules can still load the root workspace
//! and build or install `hermit`. Validation still runs the `dagrun` executable
//! from the submodule checkout. The library that generates and reads the DAG and
//! the executable that runs it must therefore come from one commit; this test
//! refuses any declaration, or the lockfile entry, that names another one.
//!
//! Declarations are found by parsing every tracked `Cargo.toml` and every
//! rust-script ```` ```cargo ```` block as TOML, not by matching one spelling:
//! a table header, a renamed dependency or different spacing is still a
//! declaration. Rust-script headers have no lockfile, so for them this is the
//! only check. A manifest block that does not parse is refused rather than
//! skipped, because an unread block could hold a declaration.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const AGENT_UTILS_GIT: &str = "https://github.com/rrnewton/agent-utils.git";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ci/manifest-plan must sit two levels below the repository root")
        .to_path_buf()
}

/// Git exports these to hooks and `git rebase --exec` steps, and they override
/// the working directory (https://github.com/rrnewton/hermit/issues/3362).
fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(name);
    }
    let output = command
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("cannot run git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output must be UTF-8")
}

fn gitlink(ls_tree: &str) -> Result<String, String> {
    let (metadata, path) = ls_tree
        .trim_end()
        .split_once('\t')
        .ok_or_else(|| format!("malformed ls-tree output: {ls_tree:?}"))?;
    match metadata.split(' ').collect::<Vec<_>>()[..] {
        ["160000", "commit", sha] if path == "agent-utils" && sha.len() == 40 => Ok(sha.into()),
        _ => Err(format!(
            "HEAD does not record agent-utils as a gitlink: {ls_tree:?}"
        )),
    }
}

/// A `dagrun` dependency, or a manifest this test could not read.
#[derive(Clone, Debug)]
enum Found {
    Declaration {
        location: String,
        value: toml::Value,
    },
    Unreadable {
        location: String,
        reason: String,
    },
}

/// Every entry keyed `dagrun`, or renamed from it with `package = "dagrun"`, in
/// any table: dependencies, dev- and build-dependencies, target-specific
/// tables, `[workspace.dependencies]` and `[patch]` alike.
fn dagrun_entries(location: &str, value: &toml::Value, found: &mut Vec<Found>) {
    match value {
        toml::Value::Table(table) => {
            for (key, entry) in table {
                let renamed = entry.get("package").and_then(toml::Value::as_str) == Some("dagrun");
                if key == "dagrun" || renamed {
                    found.push(Found::Declaration {
                        location: format!("{location} [{key}]"),
                        value: entry.clone(),
                    });
                } else {
                    dagrun_entries(location, entry, found);
                }
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                dagrun_entries(location, item, found);
            }
        }
        _ => {}
    }
}

fn manifest_entries(location: &str, manifest: &str, found: &mut Vec<Found>) {
    match manifest.parse::<toml::Value>() {
        Ok(value) => dagrun_entries(location, &value, found),
        Err(e) => found.push(Found::Unreadable {
            location: location.into(),
            reason: format!("does not parse as TOML: {e}"),
        }),
    }
}

/// Strip the comment syntax rust-script accepts around a manifest line: `//!`,
/// `//`, or nothing inside a `/*! ... */` block.
fn manifest_line(line: &str) -> &str {
    let line = line.trim_start();
    line.strip_prefix("//!")
        .or_else(|| line.strip_prefix("//"))
        .unwrap_or(line)
}

/// The `dagrun` entries in a rust-script source's ```` ```cargo ```` blocks.
fn script_entries(path: &str, source: &str, found: &mut Vec<Found>) {
    let mut lines = source.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        if manifest_line(line).trim_start().starts_with("cargo-deps:") && line.contains("dagrun") {
            found.push(Found::Unreadable {
                location: format!("{path}:{}", index + 1),
                reason: "declares dagrun in a `cargo-deps:` line, which this test cannot check; \
                         use a ```cargo block"
                    .into(),
            });
        }
        if !manifest_line(line).trim_start().starts_with("```cargo") {
            continue;
        }
        let location = format!("{path}:{}", index + 1);
        let mut block = String::new();
        let mut closed = false;
        for (_, line) in lines.by_ref() {
            let line = manifest_line(line);
            if line.trim_start().starts_with("```") {
                closed = true;
                break;
            }
            block.push_str(line);
            block.push('\n');
        }
        if closed {
            manifest_entries(&location, &block, found);
        } else {
            found.push(Found::Unreadable {
                location,
                reason: "```cargo block has no closing fence".into(),
            });
        }
    }
}

/// Why `found` disagrees with `pin`, if it does.
fn refusal(pin: &str, found: &Found) -> Option<String> {
    let (location, value) = match found {
        Found::Unreadable { location, reason } => {
            return Some(format!("{location}: manifest {reason}"));
        }
        Found::Declaration { location, value } => (location, value),
    };
    let field = |name| value.get(name).and_then(toml::Value::as_str);
    let pinned = field("git") == Some(AGENT_UTILS_GIT)
        && field("rev") == Some(pin)
        && ["path", "branch", "tag", "registry"]
            .iter()
            .all(|key| value.get(key).is_none());
    (!pinned).then(|| {
        format!("{location}: `{value}` must be {{ git = \"{AGENT_UTILS_GIT}\", rev = \"{pin}\" }}")
    })
}

/// Every reason the declarations or the lockfile disagree with `pin`.
fn disagreements(pin: &str, found: &[Found], lock: &str) -> Vec<String> {
    let mut problems: Vec<String> = found.iter().filter_map(|f| refusal(pin, f)).collect();
    let lock: toml::Value = match lock.parse() {
        Ok(lock) => lock,
        Err(e) => return vec![format!("Cargo.lock does not parse: {e}")],
    };
    let sources: Vec<Option<&str>> = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some("dagrun"))
        .map(|package| package.get("source").and_then(toml::Value::as_str))
        .collect();
    let locked = format!("git+{AGENT_UTILS_GIT}?rev={pin}#{pin}");
    if sources != [Some(locked.as_str())] {
        problems.push(format!(
            "Cargo.lock must hold exactly one dagrun package, from `{locked}`; found {sources:?}"
        ));
    }
    problems
}

fn declarations(found: &[Found]) -> usize {
    found
        .iter()
        .filter(|f| matches!(f, Found::Declaration { .. }))
        .count()
}

#[test]
fn every_dagrun_dependency_uses_the_agent_utils_gitlink() {
    let root = repo_root();
    let pin = gitlink(&git(&root, &["ls-tree", "HEAD", "agent-utils"])).unwrap();
    let mut found = Vec::new();
    for path in git(
        &root,
        &[
            "ls-files",
            "--",
            "Cargo.toml",
            "*/Cargo.toml",
            ":!agent-utils",
        ],
    )
    .lines()
    {
        let manifest =
            std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"));
        manifest_entries(path, &manifest, &mut found);
    }
    let scripts = git(
        &root,
        &[
            "grep",
            "-l",
            "-e",
            "```cargo",
            "-e",
            "cargo-deps:",
            "--",
            "*.rs",
            ":!agent-utils",
        ],
    );
    for path in scripts.lines() {
        let source =
            std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"));
        script_entries(path, &source, &mut found);
    }
    assert!(
        found.iter().any(|f| matches!(
            f,
            Found::Declaration { location, .. } if location.starts_with("ci/manifest-plan/Cargo.toml ")
        )),
        "the search must find the workspace's own dagrun dependency; found {found:?}"
    );
    assert!(
        found.iter().any(|f| matches!(
            f,
            Found::Declaration { location, .. } if location.starts_with("scripts/validate.rs:")
        )),
        "the search must find the validation driver's dagrun dependency; found {found:?}"
    );
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");

    let problems = disagreements(&pin, &found, &lock);
    assert!(
        problems.is_empty(),
        "dagrun dependencies disagree with the agent-utils gitlink {pin}; bump each \
         `rev`, then run `cargo update -p dagrun`:\n{}",
        problems.join("\n")
    );

    // The same inputs must be refused against any other commit, and a
    // declaration that reverts to the submodule path must be refused too.
    let other = "0".repeat(40);
    assert_eq!(
        disagreements(&other, &found, &lock).len(),
        found.len() + 1,
        "a different gitlink must refuse every declaration and the lockfile"
    );
    let mut reverted = found.clone();
    reverted[0] = Found::Declaration {
        location: "reverted".into(),
        value: toml::toml! { path = "../../agent-utils/rs/dagrun" }.into(),
    };
    assert_eq!(
        disagreements(&pin, &reverted, &lock).len(),
        1,
        "a path declaration must be refused"
    );
}

/// Each spelling below declares dagrun from the submodule path. The search must
/// find every one of them, and every one must be refused.
#[test]
fn every_spelling_of_a_dagrun_declaration_is_found_and_refused() {
    let pin = "d510a54892c6380bee1817f11c282777debd8801";
    let path = r#"path = "../agent-utils/rs/dagrun""#;
    let headers = [
        format!("//! ```cargo\n//! [dependencies]\n//! dagrun = {{ {path} }}\n//! ```\n"),
        format!("//! ```cargo\n//!dagrun={{ {path} }}\n//! ```\n"),
        format!("//! ```cargo\n//! [dependencies.dagrun]\n//! {path}\n//! ```\n"),
        format!(
            "//! ```cargo\n//! [dependencies]\n//! dag = {{ package = \"dagrun\", {path} }}\n//! ```\n"
        ),
        format!(
            "//! ```cargo\n//! [target.'cfg(unix)'.dev-dependencies]\n//! dagrun = {{ {path} }}\n//! ```\n"
        ),
        format!("/*!\n```cargo\n[dependencies]\ndagrun = {{ {path} }}\n```\n*/\n"),
        "//! ```cargo\n//! [dependencies]\n//! dagrun = \"0.15\"\n//! ```\n".to_string(),
        format!(
            "//! ```cargo\n//! [dependencies]\n//! dagrun = {{ git = \"{AGENT_UTILS_GIT}\", rev = \"{pin}\", {path} }}\n//! ```\n"
        ),
    ];
    for header in &headers {
        let mut found = Vec::new();
        script_entries("header.rs", header, &mut found);
        assert_eq!(
            declarations(&found),
            1,
            "not found in:\n{header}\nfound {found:?}"
        );
        assert!(refusal(pin, &found[0]).is_some(), "not refused:\n{header}");
    }

    let pinned = format!(
        "//! ```cargo\n//! [dependencies]\n//! dagrun = {{ version = \"0.15.0\", git = \"{AGENT_UTILS_GIT}\", rev = \"{pin}\" }}\n//! ```\n"
    );
    let mut found = Vec::new();
    script_entries("header.rs", &pinned, &mut found);
    assert_eq!(declarations(&found), 1, "{found:?}");
    assert_eq!(
        refusal(pin, &found[0]),
        None,
        "the canonical declaration must pass"
    );

    let mut manifest = Vec::new();
    manifest_entries(
        "Cargo.toml",
        &format!("[patch.\"{AGENT_UTILS_GIT}\"]\ndagrun = {{ {path} }}\n"),
        &mut manifest,
    );
    assert_eq!(
        declarations(&manifest),
        1,
        "a [patch] override must be found"
    );
    assert!(
        refusal(pin, &manifest[0]).is_some(),
        "a [patch] override must be refused"
    );

    for unreadable in [
        "//! ```cargo\n//! [dependencies\n//! ```\n".to_string(),
        format!("//! ```cargo\n//! dagrun = {{ {path} }}\n"),
        "// cargo-deps: dagrun=\"0.15\"\n".to_string(),
    ] {
        let mut found = Vec::new();
        script_entries("header.rs", &unreadable, &mut found);
        assert!(
            matches!(found[..], [Found::Unreadable { .. }]),
            "an unreadable manifest must be reported, not skipped:\n{unreadable}\nfound {found:?}"
        );
        assert!(refusal(pin, &found[0]).is_some());
    }
}
