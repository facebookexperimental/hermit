#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Split backend-private test debt from the landable code in a pull request.

The matrix-symmetry guard rejects growth of backend-private test corpora. Older
pull requests predate that guard and often weld product code to a legacy
``tests/backend-parity`` fixture/registration. This tool partitions such a PR
without interpreting or rewriting either side:

* code paths are replayed onto current ``origin/main``;
* asymmetric test paths are replayed separately from the source PR's merge
  base, preserving the deferred test patch exactly;
* the source patch is replayed as code + tests and its resulting Git tree must
  equal the original PR head tree byte-for-byte.

Every changed file, and therefore every diff hunk, belongs to exactly one
partition. Ambiguous inventory edits, deleted private tests, apply conflicts,
and unknown asymmetry shapes fail closed for human review.

Dry-run planning is the default and creates no branch refs or GitHub state::

    tests/backend-parity/split_asymmetric_pr.py --pr 1474

Publishing is explicit. Mixed PRs become a code-only PR and a labeled,
cross-linked deferred-test PR; only after both exist is the welded source PR
closed. A test-only source PR is simply labeled, made draft, and given the
tracked next-action checklist::

    tests/backend-parity/split_asymmetric_pr.py --pr 1474 --publish \
      --role-tag '[impl agent, gpt-5.6-sol]'

The temporary rise in open PR count for mixed PRs is intentional: one blocked
PR becomes one landable code PR plus one explicitly tracked test-debt PR.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
DEFAULT_REPO = "rrnewton/hermit"
DEFERRED_LABEL = "matrix-asymmetric-tests-deferred"
INVENTORY = "tests/e2e/manifests/inventory/test-files.json"
SYMMETRY_BASELINE = "ci/matrix-symmetry-baseline.json"
BACKEND_TOKENS = {"ptrace", "dbi", "dynamorio", "kvm", "sabre", "e9patch"}


class SplitError(RuntimeError):
    """A split that is not mechanically lossless and requires a human."""


@dataclass(frozen=True)
class Change:
    status: str
    path: str


@dataclass(frozen=True)
class PullRequest:
    number: int
    title: str
    url: str
    author: str
    state: str
    base_ref: str
    base_oid: str
    head_ref: str
    head_oid: str
    labels: tuple[str, ...]
    body: str


@dataclass(frozen=True)
class Partition:
    code: tuple[Change, ...]
    tests: tuple[Change, ...]


@dataclass(frozen=True)
class SplitObjects:
    source_tree: str
    replayed_tree: str
    code_tree: str
    test_tree: str
    code_commit: str | None
    test_commit: str


def _run(
    args: list[str],
    *,
    cwd: Path = REPO_ROOT,
    data: bytes | None = None,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[bytes]:
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)
    proc = subprocess.run(
        args,
        cwd=cwd,
        input=data,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=merged_env,
    )
    if check and proc.returncode != 0:
        stderr = proc.stderr.decode("utf-8", "replace").strip()
        raise SplitError(f"{' '.join(args)} failed ({proc.returncode}): {stderr}")
    return proc


def _text(args: list[str], **kwargs) -> str:
    return _run(args, **kwargs).stdout.decode("utf-8", "replace").strip()


def _git(repo: Path, *args: str, data: bytes | None = None) -> bytes:
    return _run(["git", *args], cwd=repo, data=data).stdout


def _git_text(repo: Path, *args: str) -> str:
    return _git(repo, *args).decode("utf-8", "replace").strip()


def _gh(repo_name: str, *args: str) -> str:
    return _text(["with-proxy", "gh", *args, "-R", repo_name])


def read_pr(repo_name: str, number: int) -> PullRequest:
    raw = _gh(
        repo_name,
        "pr",
        "view",
        str(number),
        "--json",
        (
            "number,title,url,author,state,baseRefName,baseRefOid,"
            "headRefName,headRefOid,labels,body"
        ),
    )
    value = json.loads(raw)
    return PullRequest(
        number=value["number"],
        title=value["title"],
        url=value["url"],
        author=value["author"]["login"],
        state=value["state"],
        base_ref=value["baseRefName"],
        base_oid=value["baseRefOid"],
        head_ref=value["headRefName"],
        head_oid=value["headRefOid"],
        labels=tuple(label["name"] for label in value["labels"]),
        body=value["body"] or "",
    )


def fetch_pr(repo: Path, pr: PullRequest, target_ref: str) -> str:
    _run(
        ["with-proxy", "git", "fetch", "origin", f"refs/pull/{pr.number}/head"],
        cwd=repo,
    )
    actual_head = _git_text(repo, "rev-parse", "FETCH_HEAD")
    if actual_head != pr.head_oid:
        raise SplitError(
            f"PR #{pr.number} moved while planning: API={pr.head_oid}, fetch={actual_head}"
        )
    _run(
        [
            "with-proxy",
            "git",
            "fetch",
            "origin",
            f"refs/heads/{pr.base_ref}:refs/remotes/origin/{pr.base_ref}",
        ],
        cwd=repo,
    )
    if target_ref.startswith("origin/"):
        target_branch = target_ref.removeprefix("origin/")
        if target_branch != pr.base_ref:
            _run(
                [
                    "with-proxy",
                    "git",
                    "fetch",
                    "origin",
                    f"refs/heads/{target_branch}:refs/remotes/origin/{target_branch}",
                ],
                cwd=repo,
            )
    return _git_text(repo, "rev-parse", target_ref)


def changed_paths(repo: Path, base: str, head: str) -> tuple[Change, ...]:
    raw = _git(repo, "diff", "--no-renames", "--name-status", "-z", base, head)
    fields = raw.decode("utf-8", "surrogateescape").split("\0")
    if fields and fields[-1] == "":
        fields.pop()
    if len(fields) % 2:
        raise SplitError("unexpected git --name-status -z output")
    changes = []
    for index in range(0, len(fields), 2):
        status, path = fields[index : index + 2]
        if status not in {"A", "M", "D", "T"}:
            raise SplitError(f"unsupported change status {status!r} for {path}")
        changes.append(Change(status, path))
    if not changes:
        raise SplitError("source PR has no patch relative to its merge base")
    return tuple(changes)


def _show_json(repo: Path, revision: str, path: str) -> dict | None:
    proc = _run(["git", "show", f"{revision}:{path}"], cwd=repo, check=False)
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as error:
        raise SplitError(f"{revision}:{path} is invalid JSON: {error}") from error


def _names_backend(value: str) -> bool:
    tokens = re.split(r"[^A-Za-z0-9]+", value.lower())
    return any(
        token in BACKEND_TOKENS or token.startswith("liteinst") for token in tokens
    )


def _backend_private(entry: dict) -> bool:
    if entry.get("disposition") != "guest-fixture":
        return False
    path = entry.get("path", "")
    runner = entry.get("runner", "")
    return (
        path.startswith("tests/backend-parity/")
        or "tests/backend-parity/" in runner
        or _names_backend(path)
        or _names_backend(runner)
    )


def _inventory_entries(value: dict | None) -> dict[str, dict]:
    if value is None:
        return {}
    files = value.get("files")
    if not isinstance(files, list):
        raise SplitError(f"{INVENTORY}: expected a files array")
    result = {}
    for entry in files:
        if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
            raise SplitError(f"{INVENTORY}: malformed entry")
        result[entry["path"]] = entry
    return result


def _path_patch(repo: Path, base: str, head: str, paths: Iterable[str]) -> bytes:
    selected = tuple(sorted(paths))
    if not selected:
        return b""
    return _git(
        repo,
        "diff",
        "--no-renames",
        "--binary",
        "--full-index",
        base,
        head,
        "--",
        *selected,
    )


def partition_changes(
    repo: Path, base: str, head: str, changes: tuple[Change, ...]
) -> Partition:
    by_path = {change.path: change for change in changes}
    inventory_before = _inventory_entries(_show_json(repo, base, INVENTORY))
    inventory_after = _inventory_entries(_show_json(repo, head, INVENTORY))
    quarantine = {
        change.path
        for change in changes
        if change.path.startswith("tests/backend-parity/")
    }
    for change in changes:
        entry = inventory_after.get(change.path) or inventory_before.get(change.path)
        if entry is not None and _backend_private(entry):
            quarantine.add(change.path)
    if any(by_path[path].status == "D" for path in quarantine):
        deleted = sorted(path for path in quarantine if by_path[path].status == "D")
        raise SplitError(
            "backend-private deletions are cleanup, not new asymmetric debt; "
            f"human disposition required: {deleted}"
        )

    # Legacy matrix registrations often point at tests/c fixtures. Associate
    # them by exact path, basename, or stem references in the parity-side patch.
    parity_patch = _path_patch(repo, base, head, quarantine).decode("utf-8", "replace")
    for change in changes:
        path = change.path
        if path in quarantine or not path.startswith("tests/"):
            continue
        if path == INVENTORY or path.startswith("tests/e2e/manifests/"):
            continue
        name = Path(path).name
        stem = Path(path).stem
        if path in parity_patch or name in parity_patch or stem in parity_patch:
            if change.status == "D":
                raise SplitError(
                    f"referenced private test deletion needs review: {path}"
                )
            quarantine.add(path)

    if INVENTORY in by_path:
        entry_paths = {
            path
            for path in inventory_before.keys() | inventory_after.keys()
            if inventory_before.get(path) != inventory_after.get(path)
        }
        if not entry_paths:
            raise SplitError(f"{INVENTORY} changed without semantic entry changes")
        unrelated = []
        for path in sorted(entry_paths):
            entry = inventory_after.get(path) or inventory_before.get(path) or {}
            if path in quarantine or _backend_private(entry):
                if path in by_path:
                    quarantine.add(path)
            else:
                unrelated.append(path)
        if unrelated:
            raise SplitError(
                f"{INVENTORY} mixes asymmetric debt with unrelated entries: {unrelated}"
            )
        quarantine.add(INVENTORY)

    if SYMMETRY_BASELINE in by_path:
        quarantine.add(SYMMETRY_BASELINE)

    if not quarantine:
        raise SplitError(
            "no known matrix-asymmetry shape found; do not guess at a partition"
        )
    tests = tuple(change for change in changes if change.path in quarantine)
    code = tuple(change for change in changes if change.path not in quarantine)
    if len(code) + len(tests) != len(changes):
        raise SplitError("internal error: changed path was not assigned exactly once")
    if {item.path for item in code} & {item.path for item in tests}:
        raise SplitError("internal error: path assigned to both partitions")
    return Partition(code=code, tests=tests)


def _diff_units(repo: Path, base: str, head: str, paths: Iterable[str]) -> int:
    patch = _path_patch(repo, base, head, paths).decode("utf-8", "replace")
    units = 0
    in_file = False
    file_has_hunk = False
    for line in patch.splitlines():
        if line.startswith("diff --git "):
            if in_file and not file_has_hunk:
                units += 1
            in_file = True
            file_has_hunk = False
        elif line.startswith("@@ "):
            units += 1
            file_has_hunk = True
    if in_file and not file_has_hunk:
        units += 1
    return units


def assert_hunk_partition(
    repo: Path, base: str, head: str, partition: Partition
) -> tuple[int, int, int]:
    all_paths = [item.path for item in (*partition.code, *partition.tests)]
    total = _diff_units(repo, base, head, all_paths)
    code = _diff_units(repo, base, head, (item.path for item in partition.code))
    tests = _diff_units(repo, base, head, (item.path for item in partition.tests))
    if total != code + tests:
        raise SplitError(
            f"hunk accounting failed: total={total}, code={code}, tests={tests}"
        )
    return total, code, tests


def _tree_after_patches(
    repo: Path, base: str, patches: Iterable[bytes], *, three_way: bool
) -> str:
    with tempfile.TemporaryDirectory(prefix="split-asymmetric-index-") as tmp:
        index = Path(tmp) / "index"
        env = {"GIT_INDEX_FILE": str(index)}
        _run(["git", "read-tree", base], cwd=repo, env=env)
        for patch in patches:
            if not patch:
                continue
            args = ["git", "apply", "--cached", "--binary", "--whitespace=nowarn"]
            if three_way:
                args.append("--3way")
            _run(args, cwd=repo, data=patch, env=env)
        return _text(["git", "write-tree"], cwd=repo, env=env)


def _authors(repo: Path, base: str, head: str) -> tuple[tuple[str, str], ...]:
    raw = _git(repo, "log", "--format=%aN%x00%aE", f"{base}..{head}")
    fields = raw.decode("utf-8", "replace").splitlines()
    result = []
    seen = set()
    for field in fields:
        if "\0" not in field:
            continue
        name, email = field.split("\0", 1)
        key = (name, email)
        if key not in seen:
            seen.add(key)
            result.append(key)
    if not result:
        raw_head = _git_text(repo, "show", "-s", "--format=%aN%x00%aE", head)
        name, email = raw_head.split("\0", 1)
        result.append((name, email))
    return tuple(result)


def _commit_tree(
    repo: Path,
    tree: str,
    parent: str,
    message: str,
    author: tuple[str, str],
    author_date: str,
) -> str:
    env = {
        "GIT_AUTHOR_NAME": author[0],
        "GIT_AUTHOR_EMAIL": author[1],
        "GIT_AUTHOR_DATE": author_date,
    }
    return _text(
        ["git", "commit-tree", tree, "-p", parent],
        cwd=repo,
        data=(message.rstrip() + "\n").encode(),
        env=env,
    )


def build_split_objects(
    repo: Path,
    pr: PullRequest,
    source_base: str,
    target_base: str,
    partition: Partition,
) -> SplitObjects:
    code_patch = _path_patch(
        repo, source_base, pr.head_oid, (c.path for c in partition.code)
    )
    test_patch = _path_patch(
        repo, source_base, pr.head_oid, (c.path for c in partition.tests)
    )
    replayed_tree = _tree_after_patches(
        repo, source_base, (code_patch, test_patch), three_way=False
    )
    source_tree = _git_text(repo, "rev-parse", f"{pr.head_oid}^{{tree}}")
    if replayed_tree != source_tree:
        raise SplitError(
            "losslessness check failed: code + test patches do not reproduce "
            f"source tree ({replayed_tree} != {source_tree})"
        )

    authors = _authors(repo, source_base, pr.head_oid)
    primary = authors[0]
    author_date = _git_text(repo, "show", "-s", "--format=%aI", pr.head_oid)
    coauthors = "\n".join(
        f"Co-authored-by: {name} <{email}>" for name, email in authors[1:]
    )
    provenance = f"Original-PR: {pr.url}\nOriginal-Head: {pr.head_oid}\n" + (
        f"{coauthors}\n" if coauthors else ""
    )

    code_commit = None
    if partition.code:
        code_tree = _tree_after_patches(
            repo, target_base, (code_patch,), three_way=True
        )
        code_message = (
            f"Split landable code from #{pr.number}: {pr.title}\n\n{provenance}"
        )
        code_commit = _commit_tree(
            repo, code_tree, target_base, code_message, primary, author_date
        )
    else:
        code_tree = _git_text(repo, "rev-parse", f"{target_base}^{{tree}}")

    test_tree = _tree_after_patches(repo, source_base, (test_patch,), three_way=False)
    test_message = (
        f"Quarantine asymmetric tests from #{pr.number}: {pr.title}\n\n{provenance}"
    )
    test_commit = _commit_tree(
        repo, test_tree, source_base, test_message, primary, author_date
    )
    return SplitObjects(
        source_tree=source_tree,
        replayed_tree=replayed_tree,
        code_tree=code_tree,
        test_tree=test_tree,
        code_commit=code_commit,
        test_commit=test_commit,
    )


def _ensure_label(repo_name: str) -> None:
    _gh(
        repo_name,
        "label",
        "create",
        DEFERRED_LABEL,
        "--color",
        "B60205",
        "--description",
        "Backend-private tests deferred for shared-manifest evaluation",
        "--force",
    )


def _existing_pr(repo_name: str, branch: str) -> dict | None:
    raw = _gh(
        repo_name,
        "pr",
        "list",
        "--state",
        "all",
        "--head",
        branch,
        "--json",
        "number,url,state",
    )
    values = json.loads(raw)
    return values[0] if values else None


def _create_or_reuse_pr(
    repo_name: str,
    branch: str,
    base: str,
    title: str,
    body: str,
    *,
    draft: bool,
) -> dict:
    existing = _existing_pr(repo_name, branch)
    if existing:
        if existing["state"] != "OPEN":
            raise SplitError(f"branch {branch} already has non-open PR {existing}")
        return existing
    args = ["pr", "create", "--base", base, "--head", branch, "--title", title]
    if draft:
        args.append("--draft")
    with tempfile.NamedTemporaryFile(
        "w", prefix="split-pr-body-", delete=False
    ) as body_file:
        body_file.write(body)
        body_path = body_file.name
    try:
        url = _gh(repo_name, *args, "--body-file", body_path).strip()
    finally:
        Path(body_path).unlink(missing_ok=True)
    number = int(url.rstrip("/").rsplit("/", 1)[1])
    return {"number": number, "url": url, "state": "OPEN"}


def _next_action(role_tag: str, source: PullRequest, code_url: str | None) -> str:
    code_line = f"Landable code: {code_url}\n\n" if code_url else ""
    return (
        f"{role_tag}\n\n"
        f"Deferred asymmetric tests from {source.url}.\n\n"
        f"{code_line}"
        "## Tracked Next Action\n"
        "After validate and GitHub DAG-runner capacity is sufficient to evaluate "
        "this workload, record evidence here and choose exactly one disposition:\n\n"
        "- [ ] establish it on ptrace and add a complete shared-manifest row;\n"
        "- [ ] rewrite or minimize it, then establish the reduced case on ptrace;\n"
        "- [ ] reject it and record the technical reason.\n\n"
        f"Keep `{DEFERRED_LABEL}` until one disposition is complete."
    )


def _human_review_section(pr: PullRequest) -> str:
    if "post-facto-human-review" not in pr.labels:
        return ""
    match = re.search(r"(?ms)^## Human Review Required\s*\n(.*?)(?=^##\s|\Z)", pr.body)
    if match is None or not match.group(1).strip():
        raise SplitError(
            f"PR #{pr.number} has post-facto-human-review but no auditable "
            "Human Review Required section"
        )
    return "\n\n## Human Review Required\n" + match.group(1).strip()


def publish(
    repo: Path,
    repo_name: str,
    pr: PullRequest,
    source_base: str,
    partition: Partition,
    objects: SplitObjects,
    role_tag: str,
) -> dict:
    _ensure_label(repo_name)
    if not partition.code:
        _gh(repo_name, "pr", "edit", str(pr.number), "--add-label", DEFERRED_LABEL)
        _run(
            [
                "with-proxy",
                "gh",
                "pr",
                "ready",
                str(pr.number),
                "--undo",
                "-R",
                repo_name,
            ],
            check=False,
        )
        _gh(
            repo_name,
            "pr",
            "comment",
            str(pr.number),
            "--body",
            _next_action(role_tag, pr, None),
        )
        return {"source": pr.url, "code": None, "tests": pr.url, "source_closed": False}

    code_branch = f"split/pr-{pr.number}-code"
    test_branch = f"split/pr-{pr.number}-matrix-asymmetric-tests"
    for branch, commit in (
        (code_branch, objects.code_commit),
        (test_branch, objects.test_commit),
    ):
        assert commit is not None
        local_ref = f"refs/heads/{branch}"
        existing = _run(
            ["git", "rev-parse", "--verify", local_ref], cwd=repo, check=False
        )
        if existing.returncode == 0:
            current = existing.stdout.decode().strip()
            if current != commit:
                raise SplitError(f"{local_ref} exists at {current}, expected {commit}")
        else:
            _git(repo, "update-ref", local_ref, commit)
        _run(
            ["with-proxy", "git", "push", "origin", f"{commit}:{local_ref}"],
            cwd=repo,
        )

    attribution = (
        f"Original PR: {pr.url} by @{pr.author}; original head `{pr.head_oid}`."
    )
    human_review = _human_review_section(pr)
    code_body = (
        f"{role_tag}\n\n## Summary\n"
        "This is the landable non-asymmetric portion of the original PR. "
        "Backend-private test changes are intentionally excluded and tracked in "
        "a separate deferred PR.\n\n"
        f"{attribution}\n\n"
        "## Determinism\nThe source hunks are replayed without semantic rewriting. "
        "The splitter proves that the code and test partitions reproduce the "
        "original PR head tree byte-for-byte.\n\n"
        "## Validation\nThe mechanical losslessness proof passed. Product validation "
        "must run on this code-only PR before landing.\n\n"
        "## Relationship to gVisor\nThis mechanical split does not change the "
        "source PR's behavior or its relationship to gVisor. Any required "
        "behavioral comparison remains part of code review for this PR."
        f"{human_review}"
    )
    code_pr = _create_or_reuse_pr(
        repo_name,
        code_branch,
        "main",
        f"Code from #{pr.number}: {pr.title}",
        code_body,
        draft=True,
    )
    test_body = (
        f"{_next_action(role_tag, pr, code_pr['url'])}\n\n"
        "## Determinism\nThis PR preserves the original asymmetric test hunks exactly; "
        "it does not claim that the tests are valid or ready to land.\n\n"
        f"## Validation\n{attribution} The split losslessness proof passed."
    )
    test_pr = _create_or_reuse_pr(
        repo_name,
        test_branch,
        pr.base_ref,
        f"Deferred asymmetric tests from #{pr.number}: {pr.title}",
        test_body,
        draft=True,
    )
    if "post-facto-human-review" in pr.labels:
        _gh(
            repo_name,
            "pr",
            "edit",
            str(code_pr["number"]),
            "--add-label",
            "post-facto-human-review",
        )
    _gh(repo_name, "pr", "edit", str(test_pr["number"]), "--add-label", DEFERRED_LABEL)
    _gh(
        repo_name,
        "pr",
        "comment",
        str(code_pr["number"]),
        "--body",
        f"{role_tag}\n\nDeferred asymmetric tests: {test_pr['url']}",
    )
    _gh(
        repo_name,
        "pr",
        "comment",
        str(pr.number),
        "--body",
        (
            f"{role_tag}\n\nLossless split completed:\n\n"
            f"- landable code: {code_pr['url']}\n"
            f"- deferred asymmetric tests: {test_pr['url']}\n\n"
            "Closing this welded source PR only because both replacement PRs exist."
        ),
    )
    _gh(repo_name, "pr", "close", str(pr.number))
    return {
        "source": pr.url,
        "code": code_pr["url"],
        "tests": test_pr["url"],
        "source_closed": True,
    }


def plan_one(
    repo: Path,
    repo_name: str,
    number: int,
    target_ref: str,
    publish_changes: bool,
    role_tag: str | None,
) -> dict:
    pr = read_pr(repo_name, number)
    if pr.state != "OPEN":
        raise SplitError(f"PR #{number} is {pr.state}, expected OPEN")
    target_base = fetch_pr(repo, pr, target_ref)
    source_base = _git_text(repo, "merge-base", pr.base_oid, pr.head_oid)
    changes = changed_paths(repo, source_base, pr.head_oid)
    partition = partition_changes(repo, source_base, pr.head_oid, changes)
    total_hunks, code_hunks, test_hunks = assert_hunk_partition(
        repo, source_base, pr.head_oid, partition
    )
    objects = build_split_objects(repo, pr, source_base, target_base, partition)
    result = {
        "pr": number,
        "url": pr.url,
        "source_base": source_base,
        "source_head": pr.head_oid,
        "target_base": target_base,
        "code_paths": [item.path for item in partition.code],
        "test_paths": [item.path for item in partition.tests],
        "hunks": {"total": total_hunks, "code": code_hunks, "tests": test_hunks},
        "source_tree": objects.source_tree,
        "replayed_tree": objects.replayed_tree,
        "lossless": objects.source_tree == objects.replayed_tree,
        "code_commit": objects.code_commit,
        "test_commit": objects.test_commit,
        "published": None,
    }
    if publish_changes:
        if role_tag is None:
            raise SplitError("--publish requires --role-tag")
        result["published"] = publish(
            repo,
            repo_name,
            pr,
            source_base,
            partition,
            objects,
            role_tag,
        )
    return result


def self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="split-asymmetric-self-test-") as tmp:
        repo = Path(tmp)
        _run(["git", "init", "-q", "-b", "main"], cwd=repo)
        _run(["git", "config", "user.name", "Splitter Test"], cwd=repo)
        _run(["git", "config", "user.email", "splitter@example.com"], cwd=repo)
        (repo / "src").mkdir()
        (repo / "tests/backend-parity").mkdir(parents=True)
        (repo / "tests/e2e/manifests/inventory").mkdir(parents=True)
        (repo / "src/lib.rs").write_text("pub fn value() -> u8 { 1 }\n")
        (repo / "tests/backend-parity/matrix.tsv").write_text("test_name\tptrace\n")
        (repo / INVENTORY).write_text(json.dumps({"schema": 2, "files": []}) + "\n")
        _run(["git", "add", "."], cwd=repo)
        _run(["git", "commit", "-q", "-m", "base"], cwd=repo)
        base = _git_text(repo, "rev-parse", "HEAD")

        (repo / "src/lib.rs").write_text("pub fn value() -> u8 { 2 }\n")
        (repo / "tests/c").mkdir()
        fixture = "tests/c/private_probe.c"
        (repo / fixture).write_text("int main(void) { return 0; }\n")
        with (repo / "tests/backend-parity/matrix.tsv").open("a") as matrix:
            matrix.write("private_probe\tpass\n")
        inventory = {
            "schema": 2,
            "files": [
                {
                    "path": fixture,
                    "disposition": "guest-fixture",
                    "runner": "tests/backend-parity/run_matrix.py",
                    "why": "self-test",
                }
            ],
        }
        (repo / INVENTORY).write_text(json.dumps(inventory, indent=2) + "\n")
        _run(["git", "add", "."], cwd=repo)
        _run(["git", "commit", "-q", "-m", "mixed change"], cwd=repo)
        head = _git_text(repo, "rev-parse", "HEAD")
        changes = changed_paths(repo, base, head)
        partition = partition_changes(repo, base, head, changes)
        assert [item.path for item in partition.code] == ["src/lib.rs"]
        assert {item.path for item in partition.tests} == {
            INVENTORY,
            "tests/backend-parity/matrix.tsv",
            fixture,
        }
        total, code, tests = assert_hunk_partition(repo, base, head, partition)
        assert total == code + tests and code > 0 and tests > 0
        fake_pr = PullRequest(
            number=1,
            title="self test",
            url="https://example.invalid/pr/1",
            author="splitter",
            state="OPEN",
            base_ref="main",
            base_oid=base,
            head_ref="feature",
            head_oid=head,
            labels=(),
            body="",
        )
        objects = build_split_objects(repo, fake_pr, base, base, partition)
        assert objects.source_tree == objects.replayed_tree
        assert set(
            _git_text(
                repo, "diff", "--name-only", base, objects.code_commit
            ).splitlines()
        ) == {"src/lib.rs"}
        assert set(
            _git_text(
                repo, "diff", "--name-only", base, objects.test_commit
            ).splitlines()
        ) == {INVENTORY, "tests/backend-parity/matrix.tsv", fixture}

        (repo / fixture).unlink()
        _run(["git", "add", "-u"], cwd=repo)
        _run(["git", "commit", "-q", "-m", "delete private test"], cwd=repo)
        deletion = _git_text(repo, "rev-parse", "HEAD")
        try:
            partition_changes(repo, head, deletion, changed_paths(repo, head, deletion))
        except SplitError as error:
            assert "deletions are cleanup" in str(error)
        else:
            raise AssertionError("backend-private deletion did not fail closed")
    print("PASS: splitter assigns every hunk once and reproduces the source tree")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pr", type=int, action="append", help="source PR (repeatable)"
    )
    parser.add_argument("--repo", default=DEFAULT_REPO, help="GitHub owner/repository")
    parser.add_argument(
        "--target-ref", default="origin/main", help="fresh code replay base"
    )
    parser.add_argument(
        "--publish", action="store_true", help="push branches and update PRs"
    )
    parser.add_argument(
        "--role-tag",
        help="required PR/comment prefix when publishing, e.g. [impl agent, MODEL]",
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run local losslessness tests"
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not args.pr:
        parser.error("at least one --pr is required (or use --self-test)")
    if args.publish and not args.role_tag:
        parser.error("--publish requires --role-tag")
    if args.role_tag and not re.fullmatch(
        r"\[(impl agent|adversarial-reviewer agent|coordinator), [^]]+\]",
        args.role_tag,
    ):
        parser.error("--role-tag does not follow the repository comment convention")
    for number in args.pr:
        try:
            result = plan_one(
                REPO_ROOT,
                args.repo,
                number,
                args.target_ref,
                args.publish,
                args.role_tag,
            )
        except SplitError as error:
            print(f"ERROR: PR #{number}: {error}", file=sys.stderr)
            return 1
        print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
