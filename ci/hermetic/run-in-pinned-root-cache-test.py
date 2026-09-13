#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Exercise the actual wrapper's cache mounts without starting a container."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


WRAPPER = Path(__file__).with_name("run-in-pinned-root.sh")


class CargoCacheMounts(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="hermit-pinned-root-cache-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        for directory in ("source", "tools", "cargo/bin", "cargo/registry", "cargo/git"):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        (self.root / "cargo/config.toml").write_text("host configuration must not be imported\n")
        (self.root / "cargo/bin/cargo-clippy").write_text("host executable must not be imported\n")
        self.capture = self.root / "podman.jsonl"
        fake = self.root / "tools/podman"
        fake.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "with open(os.environ['PINNED_ROOT_CAPTURE'], 'a') as out:\n"
            "    out.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "sys.exit(0 if sys.argv[1:3] == ['image', 'exists'] or sys.argv[1] == 'run' else 90)\n"
        )
        fake.chmod(0o755)

    def invoke(self, cargo_home="cargo", run_state=None, source="source"):
        env = os.environ.copy()
        env["PATH"] = str(self.root / "tools") + os.pathsep + env["PATH"]
        env["PINNED_ROOT_CAPTURE"] = str(self.capture)
        forwarded = []
        if run_state is not None:
            env["VALIDATE_RUN_STATE"] = str(run_state)
            forwarded = ["--env", "VALIDATE_RUN_STATE"]
        result = subprocess.run(
            [
                "bash", str(WRAPPER), "--src", str(source), "--out", "output",
                "--cargo-home", cargo_home, "--digest", "fixture@sha256:unused",
                *forwarded,
                "--", "/not-executed/command", "literal argument",
            ],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=10,
        )
        calls = [json.loads(line) for line in self.capture.read_text().splitlines()]
        return result, calls

    def test_imports_only_registry_and_git_from_the_host_cargo_home(self):
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0], ["image", "exists", "fixture@sha256:unused"])
        argv = calls[1]
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        imports = [mount for mount in mounts if f"source={self.root}/cargo" in mount]
        self.assertEqual(
            imports,
            [
                f"type=bind,source={self.root}/cargo/registry,destination=/build/.cargo/registry",
                f"type=bind,source={self.root}/cargo/git,destination=/build/.cargo/git",
            ],
        )
        self.assertIn(f"type=bind,source={self.root}/source,destination=/src,ro=true", mounts)
        self.assertIn(f"type=bind,source={self.root}/output/target,destination=/src/target", mounts)
        self.assertIn("CARGO_HOME=/build/.cargo", argv)
        self.assertEqual(argv.count("--cgroups=disabled"), 1)
        self.assertFalse(any(arg.startswith(("--cgroup-parent", "--cgroupns")) for arg in argv))
        self.assertIn("--network=none", argv)
        self.assertIn("--http-proxy=false", argv)
        self.assertEqual(argv[-3:], ["fixture@sha256:unused", "/not-executed/command", "literal argument"])

    def test_absent_dependency_cache_is_not_replaced_by_a_whole_home_mount(self):
        (self.root / "cargo/registry").rmdir()
        (self.root / "cargo/git").rmdir()
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        self.assertFalse(any(f"source={self.root}/cargo" in mount for mount in mounts))
        self.assertIn("CARGO_HOME=/build/.cargo", argv)

    def test_missing_cargo_home_refuses_before_container_launch(self):
        result, calls = self.invoke("missing")
        self.assertEqual(result.returncode, 2)
        self.assertIn("is not a directory", result.stderr)
        self.assertEqual(calls, [["image", "exists", "fixture@sha256:unused"]])

    def test_run_state_uses_the_same_host_directory_for_each_pinned_command(self):
        run_state = self.root / "run state"
        run_state.mkdir()
        (run_state / "fixture").write_bytes(b"existing fixture")
        result, calls = self.invoke(run_state=run_state)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        self.assertIn(f"type=bind,source={run_state},destination=/validate-run-state", argv)
        self.assertIn("VALIDATE_RUN_STATE=/validate-run-state", argv)
        self.assertEqual((run_state / "fixture").read_bytes(), b"existing fixture")
        self.assertEqual(argv[-2:], ["/not-executed/command", "literal argument"])

    def test_relative_run_state_refuses_before_container_launch(self):
        result, calls = self.invoke(run_state="relative-state")
        self.assertEqual(result.returncode, 2)
        self.assertIn("VALIDATE_RUN_STATE must be absolute", result.stderr)
        self.assertEqual(calls, [["image", "exists", "fixture@sha256:unused"]])

    def test_relocates_real_nested_submodule_configs_without_changing_host_metadata(self):
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = os.environ.copy()
        git_env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                       GIT_OPTIONAL_LOCKS="0")

        def git(root, *args):
            result = subprocess.run(
                [git_bin, "-c", "protocol.file.allow=always", "-c", "user.name=fixture",
                 "-c", "user.email=fixture@example.invalid", "-C", str(root), *args],
                env=git_env, capture_output=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            return result.stdout

        def seed(name):
            root = self.root / name
            root.mkdir()
            git(root, "init", "-q")
            (root / "payload").write_text(name + "\n")
            git(root, "add", "payload")
            git(root, "commit", "-qm", "fixture")
            return root

        leaf = seed("leaf-seed")
        child = seed("child-seed")
        git(child, "submodule", "add", "-q", str(leaf), "nested child")
        git(child, "commit", "-qam", "nested fixture")
        superproject = seed("super-seed")
        git(superproject, "submodule", "add", "-q", str(child), "third-party/fixture")
        git(superproject, "commit", "-qam", "submodule fixture")

        for separate_metadata in (False, True):
            with self.subTest(separate_metadata=separate_metadata):
                source = self.root / ("separate-source" if separate_metadata else "plain-source")
                options = (["--separate-git-dir", str(self.root / "super-metadata")]
                           if separate_metadata else [])
                git(self.root, "clone", "-q", *options, str(superproject), str(source))
                git(source, "submodule", "update", "--init", "--recursive")
                paths = ["third-party/fixture", "third-party/fixture/nested child"]
                metadata = {}
                for path in paths:
                    directory = Path(git(source / path, "rev-parse", "--absolute-git-dir").decode().strip())
                    metadata[path] = directory
                nested = metadata[paths[1]]
                original_worktree = git(source / paths[1], "config", "--get", "core.worktree").decode().strip()
                git(source / paths[1], "config", "extensions.worktreeConfig", "true")
                git(source / paths[1], "config", "--file", str(nested / "config.worktree"),
                    "core.worktree", original_worktree)
                git(source / paths[1], "config", "--file", str(nested / "config.worktree"),
                    "fixture.value", "preserve this value")
                before = {
                    str(file): file.read_bytes()
                    for directory in metadata.values()
                    for file in (directory / "config", directory / "config.worktree", directory / "index")
                    if file.exists()
                }
                heads = {path: git(source / path, "rev-parse", "HEAD") for path in paths}
                indexes = {path: git(source / path, "ls-files", "--stage", "-z") for path in paths}
                objects = {path: git(source / path, "show", "HEAD:payload") for path in paths}
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(source=source)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(calls), 2)
                argv = calls[1]
                mounts = [argv[i + 1] for i, arg in enumerate(argv) if arg == "--mount"]
                overlays = {}
                for mount in mounts:
                    fields = dict(part.split("=", 1) for part in mount.split(","))
                    if "/git-configs." in fields.get("source", ""):
                        self.assertEqual(fields["ro"], "true")
                        overlays[fields["destination"]] = Path(fields["source"])
                self.assertEqual(len(overlays), 3, "both nested configs and config.worktree must relocate")
                for path, directory in metadata.items():
                    raw = (source / path / ".git").read_text().removeprefix("gitdir: ").strip()
                    guest_dir = os.path.normpath(os.path.join("/src", path, raw))
                    for name in ("config", "config.worktree"):
                        original = directory / name
                        if not original.exists():
                            continue
                        copied = overlays[guest_dir + "/" + name]
                        self.assertEqual(
                            git(source, "config", "--file", str(copied), "--get", "core.worktree").decode().strip(),
                            "/src/" + path,
                        )
                        def other_values(config):
                            values = git(source, "config", "--file", str(config), "--null", "--list").split(b"\0")
                            return [value for value in values if not value.startswith(b"core.worktree\n")]
                        self.assertEqual(other_values(copied), other_values(original))
                    self.assertEqual(git(source / path, "rev-parse", "HEAD"), heads[path])
                    self.assertEqual(git(source / path, "ls-files", "--stage", "-z"), indexes[path])
                    self.assertEqual(git(source / path, "show", "HEAD:payload"), objects[path])
                for file, contents in before.items():
                    self.assertEqual(Path(file).read_bytes(), contents, file)


if __name__ == "__main__":
    unittest.main()
