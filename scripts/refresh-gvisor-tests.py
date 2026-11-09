#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
"""Refresh the vendored gVisor syscall tests to a newer upstream commit.

    scripts/refresh-gvisor-tests.py --dry-run [COMMIT]   # report only
    scripts/refresh-gvisor-tests.py COMMIT               # apply
    scripts/refresh-gvisor-tests.py --check              # drift guard, no network
        # COMMIT defaults to origin/master of google/gvisor.

The vendored tree is third-party/gvisor-syscall-tests/upstream/ (byte-
identical upstream files) plus generated build files under build/.
Applying replaces upstream/ wholesale from `git archive` at COMMIT,
rewrites UPSTREAM_COMMIT, LICENSE, and the generated files. Nothing
under upstream/ is ever edited in place.

--check needs no network. Byte identity to upstream is established at
refresh time: apply copies from `git archive` at the pinned commit and
records every vendored file's sha256 in build/upstream.sha256. --check
verifies the on-disk tree against that manifest (missing, extra, or
one-byte-edited file fails) and regenerates the build files in memory,
failing if the committed copies differ. It proves the tree is
unchanged since the recorded refresh; it does not re-fetch upstream.
--self-test exercises the manifest check on a scratch tree, including
the one-byte-edit refusal.

Exit status: 0 no change / applied, 3 files would change (dry-run),
4 dependency-version drift in MODULE.bazel (apply refuses until the
consumer side -- hermetic image packages and shim/fbsource target
mapping -- has been updated deliberately).

In an fbsource checkout this tree is import output: run only
--check/--dry-run there; apply refreshes on the GitHub side.
"""
import argparse
import hashlib
import difflib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PKG = os.path.join(ROOT, "third-party/gvisor-syscall-tests")
UPSTREAM = os.path.join(PKG, "upstream")
BUILD = os.path.join(PKG, "build")
PATHS = ["test/syscalls/linux", "test/util", "MODULE.bazel"]
GVISOR_URL = "https://github.com/google/gvisor"
MANIFEST = os.path.join(BUILD, "upstream.sha256")

sys.path.insert(0, BUILD)
import gen_build  # noqa: E402


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw).stdout


def pinned():
    return open(os.path.join(PKG, "UPSTREAM_COMMIT")).read().strip()


def allowlist():
    return gen_build.read_allowlist(os.path.join(BUILD, "allowlist.txt"))


def generate(gvisor_dir):
    ir = gen_build.build_ir(gvisor_dir, allowlist())
    return ir, gen_build.all_outputs(ir, "both")


def vendored_files(extract, ir):
    """Relative paths copied from a full upstream extract: all of
    test/util, the BUILD files, MODULE.bazel, every linux header, and
    only the allowlisted targets' (and their closures'/helpers')
    sources. Vendor only what we run."""
    keep = set()
    for base, _dirs, names in os.walk(os.path.join(extract, "test/util")):
        for n in names:
            keep.add(os.path.relpath(os.path.join(base, n), extract))
    keep.add("MODULE.bazel")
    keep.add("test/syscalls/linux/BUILD")
    keep.add("test/syscalls/linux/rseq/BUILD")
    for base, _dirs, names in os.walk(os.path.join(extract, "test/syscalls/linux")):
        for n in names:
            if n.endswith(".h"):
                keep.add(os.path.relpath(os.path.join(base, n), extract))
    libs = ir["libraries"]
    for b in ir["tests"].values():
        keep.update(s for s in b["srcs"] + b["test_files"] if not s.startswith("support/"))
        for lab in b["lib_closure"]:
            keep.update(s for s in libs[lab]["srcs"] if not s.startswith("support/"))
    keep.update(s for s in libs["//test/util:test_main"]["srcs"]
                if not s.startswith("support/"))
    for h in ir["helpers"].values():
        keep.update(h.get("srcs", []))
    return sorted(keep)

def hash_manifest(tree):
    """sha256sum-format manifest of every file under `tree`."""
    lines = []
    for base, _dirs, names in os.walk(tree):
        for n in names:
            full = os.path.join(base, n)
            rel = os.path.relpath(full, tree)
            with open(full, "rb") as fh:
                lines.append("%s  %s" % (hashlib.sha256(fh.read()).hexdigest(), rel))
    return "\n".join(sorted(lines)) + "\n"


def verify_manifest(tree, manifest_path=MANIFEST):
    """Problems between `tree` and the recorded hash manifest."""
    if not os.path.exists(manifest_path):
        return ["hash manifest missing: %s" % manifest_path]
    recorded = {}
    for line in open(manifest_path):
        line = line.rstrip("\n")
        if line:
            digest, rel = line.split("  ", 1)
            recorded[rel] = digest
    problems = []
    on_disk = set()
    for base, _dirs, names in os.walk(tree):
        for n in names:
            on_disk.add(os.path.relpath(os.path.join(base, n), tree))
    for rel in sorted(on_disk - set(recorded)):
        problems.append("vendored file not in hash manifest: %s" % rel)
    for rel in sorted(set(recorded) - on_disk):
        problems.append("vendored file missing from tree: %s" % rel)
    for rel in sorted(set(recorded) & on_disk):
        with open(os.path.join(tree, rel), "rb") as fh:
            if hashlib.sha256(fh.read()).hexdigest() != recorded[rel]:
                problems.append("vendored file hash mismatch: %s" % rel)
    return problems


def self_test():
    tmp = tempfile.mkdtemp(prefix="gvisor-refresh-selftest-")
    try:
        tree = os.path.join(tmp, "upstream")
        os.makedirs(os.path.join(tree, "test"))
        victim = os.path.join(tree, "test", "x.cc")
        with open(victim, "wb") as fh:
            fh.write(b"int main() { return 0; }\n")
        manifest = os.path.join(tmp, "upstream.sha256")
        with open(manifest, "w") as fh:
            fh.write(hash_manifest(tree))
        assert verify_manifest(tree, manifest) == [], "clean tree must verify"
        with open(victim, "r+b") as fh:
            fh.write(b"X")
        problems = verify_manifest(tree, manifest)
        assert any("hash mismatch" in p for p in problems), problems
        with open(os.path.join(tree, "test", "extra.cc"), "wb") as fh:
            fh.write(b"// extra\n")
        problems = verify_manifest(tree, manifest)
        assert any("not in hash manifest" in p for p in problems), problems
        print("refresh-gvisor-tests self-test: ok (clean, one-byte edit, extra file)")
        return 0
    finally:
        shutil.rmtree(tmp)


def dep_versions(module_text):
    out = {}
    for name, ver in re.findall(r'bazel_dep\(name = "([^"]+)", version = "([^"]+)"', module_text):
        if name in ("abseil-cpp", "googletest", "google_benchmark"):
            out[name] = ver
    return out


def check():
    problems = verify_manifest(UPSTREAM)
    if problems:
        for problem in problems:
            print(problem)
        print("vendored tree differs from build/upstream.sha256; a hand-edit"
              " (or a refresh without regenerating the manifest) is the"
              " usual cause. Re-run the refresh at %s." % pinned())
        return 1
    ir, files = generate(UPSTREAM)
    bad = []
    for name, text in files.items():
        p = os.path.join(BUILD, name)
        if not os.path.exists(p) or open(p).read() != text:
            bad.append(name)
    on_disk = {"combined_only/" + f for f in os.listdir(os.path.join(BUILD, "combined_only"))}
    bad += sorted(on_disk - set(files))
    if bad:
        print("generated files are stale: %s" % ", ".join(bad))
        print("regenerate with: scripts/refresh-gvisor-tests.py %s" % pinned())
        return 1
    print("gvisor vendored tree matches its hash manifest and is up to date at %s (%d tests)"
          % (pinned(), len(ir["tests"])))
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("commit", nargs="?")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--write-manifest", action="store_true",
                    help="rewrite build/upstream.sha256 from the on-disk tree")
    ap.add_argument("--gvisor-clone", help="existing gVisor clone to fetch from "
                    "(default: fresh blobless clone in a temp dir)")
    a = ap.parse_args()
    if a.self_test:
        return self_test()
    if a.write_manifest:
        with open(MANIFEST, "w") as fh:
            fh.write(hash_manifest(UPSTREAM))
        print("wrote %s" % MANIFEST)
        return 0
    if a.check:
        return check()

    tmp = tempfile.mkdtemp(prefix="gvisor-refresh-")
    try:
        clone = a.gvisor_clone
        if not clone:
            clone = os.path.join(tmp, "gvisor")
            run(["git", "clone", "--filter=blob:none", "--no-checkout", GVISOR_URL, clone])
        commit = run(["git", "-C", clone, "rev-parse",
                      (a.commit or "origin/master") + "^{commit}"]).strip()
        subj = run(["git", "-C", clone, "log", "-1", "--format=%cs %s", commit]).strip()
        print("pinned:    %s\ncandidate: %s (%s)" % (pinned(), commit, subj))

        extract = os.path.join(tmp, "extract")
        os.makedirs(extract)
        tar = subprocess.run(["git", "-C", clone, "archive", commit] + PATHS,
                             check=True, capture_output=True).stdout
        subprocess.run(["tar", "-x", "-C", extract], input=tar, check=True)
        license_text = run(["git", "-C", clone, "show", commit + ":LICENSE"])

        ir, files = generate(extract)
        old_ir = json.load(open(os.path.join(BUILD, "targets.json")))
        changed = [n for n, t in sorted(files.items())
                   if not os.path.exists(os.path.join(BUILD, n))
                   or open(os.path.join(BUILD, n)).read() != t]
        on_disk = {"combined_only/" + f for f in os.listdir(os.path.join(BUILD, "combined_only"))}
        removed = sorted(on_disk - set(files))
        stage = os.path.join(tmp, "stage")
        if allowlist() is None:
            shutil.copytree(extract, stage)
        else:
            for rel in vendored_files(extract, ir):
                dst = os.path.join(stage, rel)
                os.makedirs(os.path.dirname(dst), exist_ok=True)
                shutil.copy2(os.path.join(extract, rel), dst)
        vendored_changed = subprocess.run(
            ["diff", "-rq", stage, UPSTREAM], capture_output=True).returncode != 0
        old_t, new_t = set(old_ir["tests"]), set(ir["tests"])
        print("tests: %d -> %d (added %s, removed %s)" % (
            len(old_t), len(new_t), sorted(new_t - old_t) or "none",
            sorted(old_t - new_t) or "none"))
        for key in ("flag_collisions", "suite_collisions", "instantiation_collisions"):
            o, n = set(old_ir.get(key, {})), set(ir[key])
            print("%s: %d -> %d%s" % (key, len(o), len(n),
                  "" if o == n else " (new %s, gone %s)" % (sorted(n - o), sorted(o - n))))
        for w in ir["warnings"]:
            if w not in old_ir["warnings"]:
                print("new warning:", w)
        print("vendored files changed: %s" % ("yes" if vendored_changed or commit != pinned() else "no"))
        print("generated files that would change: %s"
              % (", ".join(changed + removed) or "none"))
        for name in changed[:2]:
            p = os.path.join(BUILD, name)
            old = open(p).read().splitlines() if os.path.exists(p) else []
            d = list(difflib.unified_diff(old, files[name].splitlines(),
                                           "a/" + name, "b/" + name, lineterm="", n=1))
            print("\n".join(d[:30]) + ("\n... (%d more diff lines)" % (len(d) - 30) if len(d) > 30 else ""))

        old_deps = dep_versions(open(os.path.join(UPSTREAM, "MODULE.bazel")).read())
        new_deps = dep_versions(open(os.path.join(extract, "MODULE.bazel")).read())
        drift = ["%s: %s -> %s" % (k, old_deps.get(k), new_deps.get(k))
                 for k in sorted(set(old_deps) | set(new_deps)) if old_deps.get(k) != new_deps.get(k)]
        for d in drift:
            print("dependency drift:", d)
        any_change = bool(changed or removed or vendored_changed or commit != pinned())
        if a.dry_run:
            print("dry run: nothing modified")
            return 4 if drift else (3 if any_change else 0)
        if drift:
            print("refusing: update the hermetic image packages and the"
                  " shim/fbsource dependency mapping first, then re-run")
            return 4

        shutil.rmtree(UPSTREAM)
        shutil.copytree(stage, UPSTREAM)
        with open(MANIFEST, "w") as fh:
            fh.write(hash_manifest(UPSTREAM))
        with open(os.path.join(PKG, "UPSTREAM_COMMIT"), "w") as fh:
            fh.write(commit + "\n")
        with open(os.path.join(PKG, "LICENSE"), "w") as fh:
            fh.write(license_text)
        for name in removed:
            os.remove(os.path.join(BUILD, name))
        for name, text in files.items():
            os.makedirs(os.path.dirname(os.path.join(BUILD, name)), exist_ok=True)
            with open(os.path.join(BUILD, name), "w") as fh:
                fh.write(text)
        print("vendored %s; review `git diff`, rebuild, re-run the gvisor"
              " bucket, then open the PR." % commit)
        return 0
    finally:
        shutil.rmtree(tmp)


if __name__ == "__main__":
    sys.exit(main())
