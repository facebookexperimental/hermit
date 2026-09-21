#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Guard the GitHub Pages compatibility-site publication contract.
//!
//! This checker owns orchestration facts. The Python publisher owns registry,
//! archive, tree, and append-only-history semantics and is exercised as a
//! black box here; those rules must not be reimplemented in this file.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const WORKFLOW: &str = ".github/workflows/docs.yml";
const RELEASE_PINS: &str = ".github/compatibility-site-releases.json";
const PUBLISHER: &str = ".github/scripts/publish-compatibility-site.py";
const LANDING_PAGE: &str = "docs/site/index.html";
const PUBLICATION_STEP: &str = "Add reviewed compatibility website";
const LANDING_ALIAS: &str = "href=\"compatibility/latest/\"";
const LANDING_WORDING: &str = "<strong>Compatibility snapshot:</strong>";

fn repository_root() -> PathBuf {
    let script = Path::new(file!());
    script
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn named_step(source: &str, name: &str) -> Result<String, String> {
    let lines = source.lines().collect::<Vec<_>>();
    let marker = format!("- name: {name}");
    let matches = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == marker)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "expected exactly one `{name}` step, found {}",
            matches.len()
        ));
    }
    let start = matches[0];
    let step_indent = indentation(lines[start]);
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            !line.trim().is_empty()
                && indentation(line) == step_indent
                && line.trim_start().starts_with("- ")
        })
        .map_or(lines.len(), |offset| start + 1 + offset);
    Ok(lines[start..end].join("\n"))
}

fn literal_block(step: &str, key: &str) -> Result<String, String> {
    let lines = step.lines().collect::<Vec<_>>();
    let marker = format!("{key}: |");
    let matches = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == marker)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "publication step must contain exactly one `{marker}`, found {}",
            matches.len()
        ));
    }
    let start = matches[0] + 1;
    let parent_indent = indentation(lines[matches[0]]);
    let end = lines[start..]
        .iter()
        .position(|line| !line.trim().is_empty() && indentation(line) <= parent_indent)
        .map_or(lines.len(), |offset| start + offset);
    let body = &lines[start..end];
    let body_indent = body
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indentation(line))
        .min()
        .ok_or_else(|| format!("publication `{key}` block is empty"))?;
    if body_indent <= parent_indent {
        return Err(format!("publication `{key}` block has invalid indentation"));
    }
    Ok(body
        .iter()
        .map(|line| line.get(body_indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn require_once(errors: &mut Vec<String>, scope: &str, source: &str, needle: &str) {
    let count = source.matches(needle).count();
    if count != 1 {
        errors.push(format!(
            "{scope} must contain `{needle}` exactly once, found {count}"
        ));
    }
}

fn validate_contract(workflow: &str, landing: &str) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    require_once(&mut errors, "checkout history", workflow, "fetch-depth: 0");
    let step = match named_step(workflow, PUBLICATION_STEP) {
        Ok(step) => step,
        Err(error) => return Err(vec![error]),
    };
    require_once(
        &mut errors,
        "publication step",
        &step,
        "if: github.ref == 'refs/heads/main'",
    );
    let shell = match literal_block(&step, "run") {
        Ok(shell) => shell,
        Err(error) => {
            errors.push(error);
            return Err(errors);
        }
    };

    require_once(
        &mut errors,
        "publication orchestration",
        &shell,
        "pins=.github/compatibility-site-releases.json",
    );
    require_once(
        &mut errors,
        "publication orchestration",
        &shell,
        "python3 .github/scripts/publish-compatibility-site.py validate \"$pins\" .",
    );
    require_once(
        &mut errors,
        "publication orchestration",
        &shell,
        "python3 .github/scripts/publish-compatibility-site.py extract \"$archive\"",
    );
    require_once(
        &mut errors,
        "publication orchestration",
        &shell,
        "python3 .github/scripts/publish-compatibility-site.py \\\n  finalize \"$pins\" target/doc/compatibility",
    );
    require_once(
        &mut errors,
        "release iteration",
        &shell,
        "release_count=$(jq -er '.releases | length' \"$pins\")",
    );
    require_once(
        &mut errors,
        "release iteration",
        &shell,
        "while (( index < release_count )); do",
    );
    require_once(
        &mut errors,
        "release metadata",
        &shell,
        ".tag_name != $tag or .name != $title",
    );
    require_once(
        &mut errors,
        "release metadata",
        &shell,
        ".draft != false or .prerelease != false",
    );
    require_once(
        &mut errors,
        "release metadata",
        &shell,
        ".state == \"uploaded\"",
    );
    require_once(
        &mut errors,
        "release metadata",
        &shell,
        ".digest == $digest",
    );
    let validate = shell.find("publish-compatibility-site.py validate");
    let network = shell.find("gh api");
    let extract = shell.find("publish-compatibility-site.py extract");
    let finalize = shell.find("publish-compatibility-site.py \\\n  finalize");
    if !matches!((validate, network), (Some(left), Some(right)) if left < right) {
        errors.push("registry validation must run before any release API request".into());
    }
    if !matches!((extract, finalize), (Some(left), Some(right)) if left < right) {
        errors.push("all pinned archives must be extracted before finalization".into());
    }

    for forbidden in [
        "keep_files:",
        "git fetch origin gh-pages",
        "git checkout gh-pages",
        "refs/heads/gh-pages",
        "cat > \"$publisher\"",
        "<<'PY'",
        "jq -e '",
    ] {
        if workflow.contains(forbidden) {
            errors.push(format!(
                "workflow must delegate publication semantics to {PUBLISHER}; found `{forbidden}`"
            ));
        }
    }

    require_once(&mut errors, "landing page", landing, LANDING_ALIAS);
    require_once(&mut errors, "landing page", landing, LANDING_WORDING);
    require_once(
        &mut errors,
        "deployment step",
        workflow,
        "publish_dir: ./target/doc",
    );
    require_once(
        &mut errors,
        "deployment step",
        workflow,
        "force_orphan: true",
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn read_inputs(root: &Path) -> Result<(String, String), String> {
    let workflow = fs::read_to_string(root.join(WORKFLOW))
        .map_err(|error| format!("cannot read {WORKFLOW}: {error}"))?;
    let landing = fs::read_to_string(root.join(LANDING_PAGE))
        .map_err(|error| format!("cannot read {LANDING_PAGE}: {error}"))?;
    for path in [RELEASE_PINS, PUBLISHER] {
        let metadata = fs::symlink_metadata(root.join(path))
            .map_err(|error| format!("cannot inspect {path}: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err(format!("{path} must be a regular file"));
        }
    }
    Ok((workflow, landing))
}

fn validate_registry(root: &Path) -> Result<String, String> {
    let output = Command::new("python3")
        .arg(root.join(PUBLISHER))
        .arg("validate")
        .arg(root.join(RELEASE_PINS))
        .arg(root)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot execute {PUBLISHER}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "publisher rejected current registry: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    rust_script_prelude::init();
    let root = repository_root();
    let result = read_inputs(&root).and_then(|(workflow, landing)| {
        validate_contract(&workflow, &landing)
            .map_err(|errors| errors.join("\n"))
            .and_then(|()| validate_registry(&root))
    });
    match result {
        Ok(registry) => {
            println!("docs Pages contract OK: release-pinned paths are reconstructed; {registry}")
        }
        Err(error) => {
            eprintln!("docs-pages-contract: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK_BOX_FIXTURE: &str = r#"
import copy
import hashlib
import io
import json
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile

helper = pathlib.Path(sys.argv[1]).resolve()
checked_in_registry = pathlib.Path(sys.argv[2]).resolve()

def run_helper(arguments, *, cwd=None, expected=None):
    result = subprocess.run(
        [sys.executable, str(helper), *map(str, arguments)],
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    text = (result.stdout + result.stderr).decode(errors="replace")
    if expected is None:
        assert result.returncode == 0, text
    else:
        assert result.returncode != 0, text
        assert expected in text, text
    return text

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()

def digest(value):
    return hashlib.sha256(canonical(value)).hexdigest()

def inventory(root):
    rows = []
    for path in [root, *sorted(root.rglob("*"))]:
        relative = "" if path == root else path.relative_to(root).as_posix()
        metadata = path.stat(follow_symlinks=False)
        mode = stat.S_IMODE(metadata.st_mode)
        if stat.S_ISDIR(metadata.st_mode):
            rows.append((relative, "d", mode, 0, ""))
        elif stat.S_ISREG(metadata.st_mode):
            data = path.read_bytes()
            rows.append((relative, "f", mode, len(data), hashlib.sha256(data).hexdigest()))
        else:
            rows.append((relative, "o", mode, 0, ""))
    return tuple(rows)

def measurements(root):
    rows = inventory(root)
    files = [row for row in rows if row[1] == "f"]
    content = hashlib.sha256()
    for relative, _, _, _, file_sha256 in files:
        content.update(file_sha256.encode())
        content.update(b"  ./")
        content.update(relative.encode())
        content.update(b"\n")
    mode_rows = [f"{relative}\t{kind}\t{mode:o}\n" for relative, kind, mode, _, _ in rows]
    identity_rows = [
        f"{relative}\t{kind}\t{mode:o}\t{file_sha256}\n"
        for relative, kind, mode, _, file_sha256 in rows
    ]
    return {
        "content_tree_sha256": content.hexdigest(),
        "file_bytes": sum(row[3] for row in files),
        "file_count": len(files),
        "mode_sha256": hashlib.sha256("".join(sorted(mode_rows)).encode()).hexdigest(),
        "recursive_identity_sha256": hashlib.sha256("".join(identity_rows).encode()).hexdigest(),
    }

def make_archive(base, identity, marker):
    source = base / f"source-{marker}"
    (source / "assets").mkdir(parents=True)
    artifact_bytes = {
        "assets/site.css": f"body {{{marker}}}\n".encode(),
        "index.html": f"<h1>{marker}</h1>\n".encode(),
    }
    artifacts = [
        {
            "bytes": len(data),
            "content_encoding": None,
            "content_type": "text/plain; charset=utf-8",
            "path": path,
            "sha256": hashlib.sha256(data).hexdigest(),
        }
        for path, data in sorted(artifact_bytes.items())
    ]
    grouped = {".": [artifacts[1]], "assets": [artifacts[0]]}
    directories = [
        {"artifacts_sha256": digest(grouped[path]), "file_count": len(grouped[path]), "path": path}
        for path in sorted(grouped)
    ]
    artifacts_sha256 = digest(artifacts)
    directories_sha256 = digest(directories)
    tree_sha256 = digest(
        {"artifacts_sha256": artifacts_sha256, "directories_sha256": directories_sha256}
    )
    manifest = {
        "artifacts": artifacts,
        "artifacts_sha256": artifacts_sha256,
        "counts": {"test_count": 1},
        "directories": directories,
        "directories_sha256": directories_sha256,
        "freshness_sha256": identity,
        "generator": {"fixture": True},
        "inputs": {"fixture": marker},
        "provenance": {"fixture": marker},
        "schema_version": 1,
        "tree_sha256": tree_sha256,
    }
    files = {"build.json": canonical(manifest) + b"\n", **artifact_bytes}
    for relative, data in files.items():
        path = source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        os.chmod(path, 0o444)
    for directory in (source / "assets", source):
        os.chmod(directory, 0o555)

    archive = base / f"compatibility-website-{identity}.tar.gz"
    with tarfile.open(archive, "w:gz") as output:
        for name in (".", "./assets"):
            member = tarfile.TarInfo(name)
            member.type = tarfile.DIRTYPE
            member.mode = 0o555
            output.addfile(member)
        for relative, data in sorted(files.items()):
            member = tarfile.TarInfo(f"./{relative}")
            member.size = len(data)
            member.mode = 0o444
            output.addfile(member, io.BytesIO(data))

    observed = measurements(source)
    pin = {
        "archive_bytes": archive.stat().st_size,
        "archive_member_count": 5,
        "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
        "artifacts_sha256": artifacts_sha256,
        "asset": archive.name,
        "build_sha256": hashlib.sha256(files["build.json"]).hexdigest(),
        "content_tree_sha256": observed["content_tree_sha256"],
        "counts": manifest["counts"],
        "directories": ["assets"],
        "file_bytes": observed["file_bytes"],
        "file_count": observed["file_count"],
        "identity": identity,
        "manifest_tree_sha256": tree_sha256,
        "mode_sha256": observed["mode_sha256"],
        "recursive_identity_sha256": observed["recursive_identity_sha256"],
        "release_title": f"Compatibility website snapshot {identity}",
        "tag": f"compatibility-website-{identity}",
    }
    return archive, pin

def write_registry(path, releases, latest, repository="fixture/repo"):
    value = {
        "latest_identity": latest,
        "release_repository": repository,
        "releases": sorted(releases, key=lambda pin: pin["identity"]),
        "schema_version": 1,
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n")
    return value

def install_landing(document_root):
    document_root.mkdir(parents=True)
    (document_root / "index.html").write_text(
        "<strong>Compatibility snapshot:</strong>\n"
        '        <a href="compatibility/latest/">Open the real-ledger compatibility website</a>'
    )

def git(repository, *arguments):
    subprocess.run(["git", *arguments], cwd=repository, check=True, stdout=subprocess.DEVNULL)

with tempfile.TemporaryDirectory(prefix="docs-pages-black-box-", dir="/tmp") as temporary:
    base = pathlib.Path(temporary)

    # Two independent publications: retained b sorts after new/latest a.
    retained_identity = "b" * 64
    latest_identity = "a" * 64
    retained_archive, retained_pin = make_archive(base, retained_identity, "retained")
    latest_archive, latest_pin = make_archive(base, latest_identity, "latest")

    first_registry_path = base / "first-registry.json"
    write_registry(first_registry_path, [retained_pin], retained_identity)
    first_document = base / "first-document"
    install_landing(first_document)
    run_helper(["extract", retained_archive, first_document / "compatibility" / retained_identity,
                first_registry_path, 0])
    run_helper(["finalize", first_registry_path, first_document / "compatibility"])
    retained_before = inventory(first_document / "compatibility" / retained_identity)
    assert inventory(first_document / "compatibility" / "latest") == retained_before

    second_registry_path = base / "second-registry.json"
    write_registry(second_registry_path, [retained_pin, latest_pin], latest_identity)
    second_document = base / "second-document"
    install_landing(second_document)
    sorted_archives = {latest_identity: latest_archive, retained_identity: retained_archive}
    for index, identity in enumerate((latest_identity, retained_identity)):
        run_helper(["extract", sorted_archives[identity], second_document / "compatibility" / identity,
                    second_registry_path, index])
    run_helper(["finalize", second_registry_path, second_document / "compatibility"])
    assert inventory(second_document / "compatibility" / retained_identity) == retained_before
    assert inventory(second_document / "compatibility" / latest_identity) == inventory(
        second_document / "compatibility" / "latest"
    )

    missing_document = base / "missing-document"
    install_landing(missing_document)
    run_helper(["extract", latest_archive, missing_document / "compatibility" / latest_identity,
                second_registry_path, 0])
    run_helper(["finalize", second_registry_path, missing_document / "compatibility"],
               expected="do not exactly match")

    unsafe_document = base / "unsafe-document"
    install_landing(unsafe_document)
    shutil.copytree(
        first_document / "compatibility" / retained_identity,
        unsafe_document / "compatibility" / retained_identity,
    )
    os.chmod(unsafe_document / "compatibility" / retained_identity, 0o755)
    os.symlink("index.html", unsafe_document / "compatibility" / retained_identity / "alias")
    run_helper(["finalize", first_registry_path, unsafe_document / "compatibility"],
               expected="link or special")

    # Validation reads first-parent history, with the first registry allowed to
    # have no predecessor and every committed row retained thereafter.
    checked = json.loads(checked_in_registry.read_text())
    added = copy.deepcopy(checked["releases"][-1])
    added_identity = "f" * 64
    added["identity"] = added_identity
    added["tag"] = f"compatibility-website-{added_identity}"
    added["asset"] = f"{added['tag']}.tar.gz"
    added["release_title"] = f"Compatibility website snapshot {added_identity}"
    expanded = copy.deepcopy(checked)
    expanded["latest_identity"] = added_identity
    expanded["releases"].append(added)

    repository = base / "history"
    repository.mkdir()
    git(repository, "init", "--quiet")
    git(repository, "config", "user.name", "Fixture")
    git(repository, "config", "user.email", "fixture@example.invalid")
    git(repository, "commit", "--quiet", "--allow-empty", "-m", "base")
    history_registry = repository / ".github" / "compatibility-site-releases.json"
    history_registry.parent.mkdir()
    history_registry.write_text(json.dumps(checked, sort_keys=True, indent=2) + "\n")
    git(repository, "add", ".github/compatibility-site-releases.json")
    git(repository, "commit", "--quiet", "-m", "first registry")
    run_helper(["validate", history_registry, repository], cwd=repository)

    history_registry.write_text(json.dumps(expanded, sort_keys=True, indent=2) + "\n")
    git(repository, "add", ".github/compatibility-site-releases.json")
    git(repository, "commit", "--quiet", "-m", "expanded registry")
    expanded_commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repository).decode().strip()
    run_helper(["validate", history_registry, repository], cwd=repository)

    shallow = base / "shallow"
    subprocess.run(["git", "clone", "--quiet", "--depth", "1", repository.as_uri(), str(shallow)], check=True)
    run_helper(["validate", shallow / ".github" / "compatibility-site-releases.json", shallow],
               cwd=shallow, expected="full git history is required")

    removed = copy.deepcopy(expanded)
    removed["releases"] = [pin for pin in removed["releases"] if pin["identity"] != checked["latest_identity"]]
    history_registry.write_text(json.dumps(removed, sort_keys=True, indent=2) + "\n")
    git(repository, "add", ".github/compatibility-site-releases.json")
    git(repository, "commit", "--quiet", "-m", "removed row")
    run_helper(["validate", history_registry, repository], cwd=repository,
               expected="removed published identity")

    git(repository, "checkout", "--quiet", "-B", "mutation", expanded_commit)
    mutated = copy.deepcopy(expanded)
    mutated_pin = next(
        pin for pin in mutated["releases"]
        if pin["identity"] == checked["latest_identity"]
    )
    mutated_pin["archive_bytes"] += 1
    history_registry.write_text(json.dumps(mutated, sort_keys=True, indent=2) + "\n")
    git(repository, "add", ".github/compatibility-site-releases.json")
    git(repository, "commit", "--quiet", "-m", "mutated row")
    run_helper(["validate", history_registry, repository], cwd=repository,
               expected="changed published pin")

    duplicate = base / "duplicate" / ".github" / "compatibility-site-releases.json"
    duplicate.parent.mkdir(parents=True)
    duplicate.write_text('{"schema_version":1,"schema_version":1}')
    duplicate_repository = duplicate.parents[1]
    git(duplicate_repository, "init", "--quiet")
    run_helper(["validate", duplicate, duplicate_repository], cwd=duplicate_repository,
               expected="duplicate object key")

    invalid_values = (
        ("repository-whitespace", "release_repository", "fixture repo/release",
         "release repository is invalid"),
        ("repository-control", "release_repository", "fixture/repo\tname",
         "release repository is invalid"),
        ("repository-unicode", "release_repository", "fixture/r\u00e9po",
         "release repository is invalid"),
        ("directory-whitespace", "directories", "asset files",
         "release directory inventory is invalid"),
        ("directory-control", "directories", "assets\nfiles",
         "release directory inventory is invalid"),
        ("directory-unicode", "directories", "d\u00e1ta",
         "release directory inventory is invalid"),
    )
    for label, field, value, expected in invalid_values:
        invalid = copy.deepcopy(checked)
        if field == "release_repository":
            invalid[field] = value
        else:
            invalid["releases"][0][field] = [value]
            invalid["releases"][0]["archive_member_count"] = (
                invalid["releases"][0]["file_count"] + 2
            )
        invalid_registry = base / label / ".github" / "compatibility-site-releases.json"
        invalid_registry.parent.mkdir(parents=True)
        invalid_registry.write_text(json.dumps(invalid, sort_keys=True, indent=2) + "\n")
        invalid_repository = invalid_registry.parents[1]
        git(invalid_repository, "init", "--quiet")
        run_helper(["validate", invalid_registry, invalid_repository],
                   cwd=invalid_repository, expected=expected)

    wording = copy.deepcopy(checked)
    latest = next(
        pin for pin in wording["releases"]
        if pin["identity"] == wording["latest_identity"]
    )
    latest["release_title"] = "Historical compatibility website snapshot"
    history_registry.write_text(json.dumps(wording, sort_keys=True, indent=2) + "\n")
    run_helper(["validate", history_registry, repository], cwd=repository,
               expected="must not use Historical wording")

    bootstrap = copy.deepcopy(checked)
    bootstrap_pin = next(
        pin for pin in bootstrap["releases"]
        if pin["identity"] == "8feef179bed2bb48c81dd0bc8186d81df47c255d8c015dc4b0eb139eab439edc"
    )
    bootstrap_pin["archive_bytes"] += 1
    history_registry.write_text(json.dumps(bootstrap, sort_keys=True, indent=2) + "\n")
    run_helper(["validate", history_registry, repository], cwd=repository,
               expected="changed bootstrap pin")
"#;

    fn actual() -> (String, String) {
        read_inputs(&repository_root()).expect("repository contract inputs should be readable")
    }

    fn assert_rejected(workflow: &str, landing: &str, expected: &str) {
        let errors = validate_contract(workflow, landing)
            .expect_err("weakened publication contract unexpectedly passed")
            .join("\n");
        assert!(errors.contains(expected), "unexpected errors: {errors}");
    }

    #[test]
    fn current_workflow_and_landing_satisfy_orchestration_contract() {
        let (workflow, landing) = actual();
        validate_contract(&workflow, &landing).expect("current Pages orchestration should pass");
    }

    #[test]
    fn publisher_passes_black_box_publication_and_history_cases() {
        let root = repository_root();
        let output = Command::new("python3")
            .args([
                "-c",
                BLACK_BOX_FIXTURE,
                root.join(PUBLISHER).to_str().unwrap(),
                root.join(RELEASE_PINS).to_str().unwrap(),
            ])
            .output()
            .expect("python3 should run black-box publisher fixtures");
        assert!(
            output.status.success(),
            "black-box publisher fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn malformed_literal_block_indentation_is_rejected() {
        assert!(literal_block("- name: fixture\n  run: |\nbody\n", "run").is_err());
    }

    #[test]
    fn mutable_pages_inheritance_and_shallow_checkout_are_rejected() {
        let (workflow, landing) = actual();
        for forbidden in ["keep_files:", "git fetch origin gh-pages"] {
            assert_rejected(
                &format!("{workflow}\n{forbidden}\n"),
                &landing,
                "delegate publication semantics",
            );
        }
        assert_rejected(
            &workflow.replacen("fetch-depth: 0", "fetch-depth: 1", 1),
            &landing,
            "fetch-depth: 0",
        );
    }

    #[test]
    fn workflow_must_validate_before_network_and_finalize_after_extract() {
        let (workflow, landing) = actual();
        let moved_validation = workflow.replacen(
            "python3 .github/scripts/publish-compatibility-site.py validate \"$pins\" .",
            "true",
            1,
        );
        assert_rejected(&moved_validation, &landing, "validate");
        let removed_extract = workflow.replacen(
            "python3 .github/scripts/publish-compatibility-site.py extract \"$archive\"",
            "true",
            1,
        );
        assert_rejected(&removed_extract, &landing, "extract");
    }

    #[test]
    fn landing_page_must_use_stable_alias() {
        let (workflow, landing) = actual();
        let weakened = landing.replacen(LANDING_ALIAS, "href=\"compatibility/missing/\"", 1);
        assert_rejected(&workflow, &weakened, LANDING_ALIAS);
    }
}
