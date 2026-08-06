#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Verify the cross-client skill layout without duplicating instruction bodies.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const PACKAGED_SKILLS: &[&str] = &[
    "benchmark",
    "backend-reality-reviewer",
    "ci-debugging",
    "continuous-virtual-time-is-sacred",
    "deadlock-debugging",
    "determinism-regression-debugging",
    "fabler",
    "hermit-debugging",
    "post-facto-review",
    "presenting-quantitative-data",
    "progress-rubric",
    "repo-cleanliness",
    "test-shrink-optimization",
    "ux-tester",
];

const PARENT_ONLY_ROLES: &[&str] = &[
    "hermit-ci",
    "hermit-coord",
    "hermit-dbt",
    "hermit-kvm",
    "hermit-lander",
    "hermit-liteinst",
    "hermit-opt",
    "hermit-sabre",
];

fn git_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("could not run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn require_symlink(path: &Path, expected: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_symlink() {
        return Err(format!("{} must be a symlink", path.display()));
    }
    let actual =
        fs::read_link(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if actual != expected {
        return Err(format!(
            "{} points to {:?}, expected {:?}",
            path.display(),
            actual,
            expected
        ));
    }
    Ok(())
}

fn frontmatter<'a>(contents: &'a str, path: &Path) -> Result<&'a str, String> {
    let rest = contents
        .strip_prefix("---\n")
        .ok_or_else(|| format!("{} lacks YAML frontmatter", path.display()))?;
    let closing = rest
        .find("\n---\n")
        .ok_or_else(|| format!("{} has unterminated YAML frontmatter", path.display()))?;
    Ok(&contents[..4 + closing + 5])
}

fn checked_frontmatter<'a>(
    contents: &'a str,
    path: &Path,
    expected_name: &str,
) -> Result<&'a str, String> {
    let metadata = frontmatter(contents, path)?;
    let body = metadata
        .strip_prefix("---\n")
        .and_then(|value| value.strip_suffix("---\n"))
        .ok_or_else(|| format!("{} has malformed YAML delimiters", path.display()))?;
    let mut lines = body.lines();
    let name = lines
        .next()
        .and_then(|line| line.strip_prefix("name: "))
        .ok_or_else(|| {
            format!(
                "{} frontmatter must begin with exactly `name: <slug>`",
                path.display()
            )
        })?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "{} frontmatter name {:?} is not a lowercase-hyphenated slug",
            path.display(),
            name
        ));
    }
    if name != expected_name {
        return Err(format!(
            "{} declares name {:?}, expected {:?}",
            path.display(),
            name,
            expected_name
        ));
    }
    let description = lines
        .next()
        .and_then(|line| line.strip_prefix("description: "))
        .and_then(|value| value.strip_prefix('"'))
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| {
            format!(
                "{} frontmatter description must be one nonempty double-quoted scalar",
                path.display()
            )
        })?;
    if description.trim().is_empty() || description.contains(['"', '\\']) {
        return Err(format!(
            "{} frontmatter description must be one nonempty double-quoted scalar without escapes",
            path.display()
        ));
    }
    if lines.next().is_some() {
        return Err(format!(
            "{} frontmatter contains unsupported or duplicate fields",
            path.display()
        ));
    }
    let instructions = contents
        .strip_prefix(metadata)
        .ok_or_else(|| format!("{} frontmatter boundary is inconsistent", path.display()))?;
    if instructions.trim().is_empty() {
        return Err(format!(
            "{} has metadata but no skill instructions",
            path.display()
        ));
    }
    Ok(metadata)
}

fn parser_regression_tests() -> Result<(), String> {
    let path = Path::new("<skill-frontmatter-fixture>");
    let valid = "---\nname: fixture-skill\ndescription: \"Useful guidance.\"\n---\n# Body\n";
    checked_frontmatter(valid, path, "fixture-skill")?;

    let invalid = [
        (
            "duplicate name",
            "---\nname: fixture-skill\nname: other\ndescription: \"Useful.\"\n---\n# Body\n",
        ),
        (
            "duplicate description",
            "---\nname: fixture-skill\ndescription: \"Useful.\"\ndescription: \"Other.\"\n---\n# Body\n",
        ),
        (
            "null description",
            "---\nname: fixture-skill\ndescription: null\n---\n# Body\n",
        ),
        (
            "empty block description",
            "---\nname: fixture-skill\ndescription: |\n---\n# Body\n",
        ),
        (
            "empty quoted description",
            "---\nname: fixture-skill\ndescription: \"\"\n---\n# Body\n",
        ),
        (
            "unterminated quote",
            "---\nname: fixture-skill\ndescription: \"Useful.\n---\n# Body\n",
        ),
        (
            "metadata-only skill",
            "---\nname: fixture-skill\ndescription: \"Useful.\"\n---\n",
        ),
    ];
    for (case, contents) in invalid {
        if checked_frontmatter(contents, path, "fixture-skill").is_ok() {
            return Err(format!(
                "parser regression fixture unexpectedly accepted {case}"
            ));
        }
    }
    Ok(())
}

fn require_real_dir(path: &Path, purpose: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "{} must be a real {purpose} directory",
            path.display()
        ));
    }
    Ok(())
}

fn require_regular_file(path: &Path, purpose: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "{} must be a regular {purpose} file",
            path.display()
        ));
    }
    Ok(())
}

fn require_contained(root: &Path, path: &Path, purpose: &str) -> Result<PathBuf, String> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve repository root {}: {error}", root.display()))?;
    let canonical_path = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    canonical_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "{} resolves outside repository root {} ({purpose})",
            path.display(),
            canonical_root.display()
        )
    })?;
    Ok(canonical_path)
}

fn require_internal_symlink(
    root: &Path,
    path: &Path,
    expected: &Path,
    purpose: &str,
) -> Result<PathBuf, String> {
    require_symlink(path, expected)?;
    require_contained(root, path, purpose)
}

struct TempFixture(PathBuf);

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn containment_regression_test() -> Result<(), String> {
    use std::os::unix::fs::symlink;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("fixture clock failed: {error}"))?
        .as_nanos();
    let fixture = TempFixture(env::temp_dir().join(format!(
        "hermit-skill-containment-{}-{nonce}",
        std::process::id()
    )));
    let root = fixture.0.join("repo");
    let outside = fixture.0.join("outside");
    fs::create_dir_all(&root)
        .map_err(|error| format!("create fixture root {}: {error}", root.display()))?;
    fs::create_dir_all(outside.join("skills")).map_err(|error| {
        format!(
            "create fixture escape target {}: {error}",
            outside.display()
        )
    })?;
    fs::write(root.join("AGENTS.md"), "fixture policy\n")
        .map_err(|error| format!("write positive fixture: {error}"))?;
    symlink("AGENTS.md", root.join("CLAUDE.md"))
        .map_err(|error| format!("create positive fixture symlink: {error}"))?;
    require_internal_symlink(
        &root,
        &root.join("CLAUDE.md"),
        Path::new("AGENTS.md"),
        "positive fixture",
    )?;
    symlink("missing", root.join("dangling"))
        .map_err(|error| format!("create dangling fixture symlink: {error}"))?;
    if require_internal_symlink(
        &root,
        &root.join("dangling"),
        Path::new("missing"),
        "dangling fixture",
    )
    .is_ok()
    {
        return Err("dangling discovery-link fixture passed containment".to_owned());
    }
    fs::write(outside.join("target"), "outside\n")
        .map_err(|error| format!("write escaping fixture: {error}"))?;
    symlink("../outside/target", root.join("escaping"))
        .map_err(|error| format!("create escaping fixture symlink: {error}"))?;
    if require_internal_symlink(
        &root,
        &root.join("escaping"),
        Path::new("../outside/target"),
        "escaping fixture",
    )
    .is_ok()
    {
        return Err("escaping discovery-link fixture passed containment".to_owned());
    }
    symlink(&outside, root.join(".claude"))
        .map_err(|error| format!("create fixture ancestor symlink: {error}"))?;

    if require_real_dir(&root.join(".claude"), "canonical ancestor").is_ok() {
        return Err("ancestor-symlink fixture passed the real-directory check".to_owned());
    }
    if require_contained(
        &root,
        &root.join(".claude/skills"),
        "ancestor-symlink fixture",
    )
    .is_ok()
    {
        return Err("ancestor-symlink fixture escaped repository containment".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn containment_regression_test() -> Result<(), String> {
    Err("skill containment checker requires Unix symlink semantics".to_owned())
}

fn entry_names(path: &Path) -> Result<BTreeSet<String>, String> {
    fs::read_dir(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .map(|entry| {
            let entry = entry.map_err(|error| format!("cannot read directory entry: {error}"))?;
            entry
                .file_name()
                .into_string()
                .map_err(|name| format!("non-UTF-8 skill entry: {name:?}"))
        })
        .collect()
}

fn check_package_tree(
    root: &Path,
    canonical_dir: &Path,
    canonical_path: &Path,
    codex_dir: &Path,
    llms_dir: &Path,
) -> Result<(), String> {
    for entry in fs::read_dir(canonical_path)
        .map_err(|error| format!("cannot read {}: {error}", canonical_path.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read package entry: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        let resolved = require_contained(root, &path, "canonical package resource")?;
        let relative = path.strip_prefix(canonical_dir).map_err(|_| {
            format!(
                "{} is not beneath canonical package {}",
                path.display(),
                canonical_dir.display()
            )
        })?;
        for client_path in [codex_dir.join(relative), llms_dir.join(relative)] {
            let client_resolved = fs::canonicalize(&client_path).map_err(|error| {
                format!(
                    "cannot resolve shared package resource {}: {error}",
                    client_path.display()
                )
            })?;
            if client_resolved != resolved {
                return Err(format!(
                    "{} resolves to {}, expected {}",
                    client_path.display(),
                    client_resolved.display(),
                    resolved.display()
                ));
            }
        }
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            check_package_tree(root, canonical_dir, &path, codex_dir, llms_dir)?;
        } else if !metadata.is_file() && !metadata.file_type().is_symlink() {
            return Err(format!(
                "{} is neither a regular package entry nor a symlink",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn package_resource_regression_test() -> Result<(), String> {
    use std::os::unix::fs::symlink;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("resource fixture clock failed: {error}"))?
        .as_nanos();
    let fixture = TempFixture(env::temp_dir().join(format!(
        "hermit-skill-resources-{}-{nonce}",
        std::process::id()
    )));
    let root = fixture.0.join("repo");
    let canonical = root.join(".claude/skills/example");
    let codex = root.join(".agents/skills/example");
    let llms = root.join(".llms/skills/example");
    let references = canonical.join("references");
    let outside = fixture.0.join("outside");
    fs::create_dir_all(&references)
        .map_err(|error| format!("create canonical resource fixture: {error}"))?;
    fs::create_dir_all(root.join(".agents/skills"))
        .map_err(|error| format!("create Codex resource fixture: {error}"))?;
    fs::create_dir_all(root.join(".llms"))
        .map_err(|error| format!("create Claude resource fixture: {error}"))?;
    fs::create_dir_all(&outside)
        .map_err(|error| format!("create escaping resource fixture: {error}"))?;
    fs::write(canonical.join("SKILL.md"), "fixture instructions\n")
        .map_err(|error| format!("write canonical skill fixture: {error}"))?;
    fs::write(references.join("inside.md"), "inside\n")
        .map_err(|error| format!("write internal resource fixture: {error}"))?;
    symlink("../../.claude/skills/example", &codex)
        .map_err(|error| format!("create Codex package fixture: {error}"))?;
    symlink("../.claude/skills", root.join(".llms/skills"))
        .map_err(|error| format!("create Claude root fixture: {error}"))?;
    check_package_tree(&root, &canonical, &canonical, &codex, &llms)?;

    symlink("missing.md", references.join("dangling.md"))
        .map_err(|error| format!("create dangling resource fixture: {error}"))?;
    if check_package_tree(&root, &canonical, &canonical, &codex, &llms).is_ok() {
        return Err("dangling package-resource fixture passed containment".to_owned());
    }
    fs::remove_file(references.join("dangling.md"))
        .map_err(|error| format!("remove exact dangling fixture: {error}"))?;

    fs::write(outside.join("target.md"), "outside\n")
        .map_err(|error| format!("write escaping resource target: {error}"))?;
    symlink(&outside.join("target.md"), references.join("escaping.md"))
        .map_err(|error| format!("create escaping resource fixture: {error}"))?;
    if check_package_tree(&root, &canonical, &canonical, &codex, &llms).is_ok() {
        return Err("escaping package-resource fixture passed containment".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn package_resource_regression_test() -> Result<(), String> {
    Err("skill resource checker requires Unix symlink semantics".to_owned())
}

fn check(root: &Path) -> Result<(), String> {
    require_contained(root, root, "repository root")?;
    require_real_dir(&root.join(".agents"), "stock-Codex ancestor")?;
    require_contained(root, &root.join(".agents"), "stock-Codex ancestor")?;
    require_real_dir(&root.join(".claude"), "canonical ancestor")?;
    require_contained(root, &root.join(".claude"), "canonical ancestor")?;
    require_real_dir(&root.join(".llms"), "Claude compatibility ancestor")?;
    require_contained(root, &root.join(".llms"), "Claude compatibility ancestor")?;
    require_internal_symlink(
        root,
        &root.join("CLAUDE.md"),
        Path::new("AGENTS.md"),
        "Claude policy link",
    )?;
    let llms_root = root.join(".llms/skills");
    require_internal_symlink(
        root,
        &llms_root,
        Path::new("../.claude/skills"),
        "Claude skill-root link",
    )?;

    let codex_root = root.join(".agents/skills");
    require_real_dir(&codex_root, "stock-Codex skill")?;
    require_contained(root, &codex_root, "stock-Codex skill directory")?;

    let mut expected_entries = BTreeSet::from(["README.md".to_owned()]);
    expected_entries.extend(PACKAGED_SKILLS.iter().map(|name| (*name).to_owned()));
    let actual_entries = entry_names(&codex_root)?;
    if actual_entries != expected_entries {
        return Err(format!(
            "stock-Codex skill entries differ:\n  actual: {actual_entries:?}\n  expected: {expected_entries:?}"
        ));
    }

    let mut expected_canonical = BTreeSet::new();
    expected_canonical.extend(PACKAGED_SKILLS.iter().map(|name| (*name).to_owned()));
    let canonical_root = root.join(".claude/skills");
    require_real_dir(&canonical_root, "canonical skill")?;
    let resolved_canonical_root =
        require_contained(root, &canonical_root, "canonical skill directory")?;
    if fs::canonicalize(&llms_root)
        .map_err(|error| format!("cannot resolve {}: {error}", llms_root.display()))?
        != resolved_canonical_root
    {
        return Err(format!(
            "{} does not resolve to canonical skill root {}",
            llms_root.display(),
            canonical_root.display()
        ));
    }
    let actual_canonical = entry_names(&canonical_root)?;
    if actual_canonical != expected_canonical {
        return Err(format!(
            "canonical skill entries differ:\n  actual: {actual_canonical:?}\n  expected: {expected_canonical:?}"
        ));
    }

    for name in PACKAGED_SKILLS {
        let canonical_dir = canonical_root.join(name);
        require_real_dir(&canonical_dir, "canonical packaged skill")?;
        require_contained(root, &canonical_dir, "canonical packaged skill")?;
        let canonical_skill = canonical_dir.join("SKILL.md");
        require_regular_file(&canonical_skill, "canonical packaged skill")?;
        let contents = fs::read_to_string(&canonical_skill)
            .map_err(|error| format!("cannot read {}: {error}", canonical_skill.display()))?;
        checked_frontmatter(&contents, &canonical_skill, name)?;

        let entry = codex_root.join(name);
        let resolved_entry = require_internal_symlink(
            root,
            &entry,
            &PathBuf::from(format!("../../.claude/skills/{name}")),
            "stock-Codex package link",
        )?;
        let resolved_canonical = fs::canonicalize(&canonical_dir)
            .map_err(|error| format!("cannot resolve {}: {error}", canonical_dir.display()))?;
        if resolved_entry != resolved_canonical {
            return Err(format!(
                "{} does not resolve to canonical package {}",
                entry.display(),
                canonical_dir.display()
            ));
        }
        require_regular_file(&entry.join("SKILL.md"), "resolved packaged skill")?;
        require_contained(root, &entry.join("SKILL.md"), "resolved packaged skill")?;
        check_package_tree(
            root,
            &canonical_dir,
            &canonical_dir,
            &entry,
            &llms_root.join(name),
        )?;
    }

    for name in PARENT_ONLY_ROLES {
        for path in [
            canonical_root.join(name),
            canonical_root.join(format!("{name}.md")),
        ] {
            if fs::symlink_metadata(&path).is_ok() {
                return Err(format!(
                    "parent coordinator role leaked into product skills: {}",
                    path.display()
                ));
            }
        }
    }

    Ok(())
}

fn main() {
    rust_script_prelude::init();
    if let Err(error) = parser_regression_tests()
        .and_then(|_| containment_regression_test())
        .and_then(|_| package_resource_regression_test())
    {
        eprintln!("check-skill-discovery: ERROR: {error}");
        std::process::exit(1);
    }
    let root = match env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => match git_root() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("check-skill-discovery: ERROR: {error}");
                std::process::exit(1);
            }
        },
    };
    if let Err(error) = check(&root) {
        eprintln!("check-skill-discovery: ERROR: {error}");
        std::process::exit(1);
    }
    println!(
        "check-skill-discovery: PASS ({} canonical packages; Claude root and Codex package links verified)",
        PACKAGED_SKILLS.len()
    );
}
