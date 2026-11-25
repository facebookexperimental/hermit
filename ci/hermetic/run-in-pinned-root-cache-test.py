#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Exercise the actual wrapper's cache mounts, mostly without a container.

One live case (SeededOverlayLive) starts a real container where podman and
the pinned image are available, and skips by name where they are not.
"""

import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


WRAPPER = Path(__file__).with_name("run-in-pinned-root.sh")

# Wall bound for one run-in-pinned-root.sh invocation. Measured 2026-11-03
# on a quiet host at hermit 2baafbdf: the heaviest single test in this file
# (InheritedGitLocation.test_scratch_repository_tests_ignore_inherited_git_
# location_variables) used 27.15 CPU-s (9.58 user + 17.57 sys) in 7.77
# wall-s, and the full 30-test file used 41.65 CPU-s in 23.33 wall-s.
# Owner labelling: CPU cap about 1.5x the observed CPU (40 CPU-s, at most
# 60 CPU-s); wall cap 8x the CPU cap (320 s, at most 600 s). The old flat
# 10 s expired on every wrapper call in the 2026-11-03 buck-hybrid red run
# at 31.4x CPU/wall. Git fixture calls below keep their 10 s: they are
# local operations that did not expire in that run.
WRAPPER_TIMEOUT_S = 320

# Git exports these to hooks and to every `git rebase --exec` step, and they
# take precedence over `git -C`. Every command below names its repository
# explicitly, so it runs without them; otherwise a scratch `git init` rewrites
# the caller's core.bare and a scratch commit moves the caller's HEAD
# (https://github.com/rrnewton/hermit/issues/3362). GIT_CONFIG_COUNT and its
# companions carry this host's proxy rewrites and are kept.
REPOSITORY_LOCATION_VARIABLES = frozenset(
    (
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    )
)


def repository_neutral_env():
    """This process's environment without Git's repository-location variables."""
    return {
        name: value
        for name, value in os.environ.items()
        if name not in REPOSITORY_LOCATION_VARIABLES
    }


def signal_masks(status):
    """The SigBlk and SigIgn masks of a /proc/<pid>/status text, as integers."""
    masks = {}
    for line in status.splitlines():
        key, _, value = line.partition(":")
        if key in ("SigBlk", "SigIgn"):
            masks[key] = int(value.strip(), 16)
    return masks


class CargoCacheMounts(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="hermit-pinned-root-cache-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        for directory in (
            "source/agent-utils/rs/target",
            "source/agent-utils/rs/.agent-utils-locks",
            "source/agent-utils/rs/.agent-utils-snapshots",
            "tools",
            "cargo/bin",
            "cargo/registry",
            "cargo/git",
        ):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        (self.root / "cargo/config.toml").write_text(
            "host configuration must not be imported\n"
        )
        (self.root / "cargo/bin/cargo-clippy").write_text(
            "host executable must not be imported\n"
        )
        self.capture = self.root / "podman.jsonl"
        fake = self.root / "tools/podman"
        fake.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, signal, sys\n"
            "with open(os.environ['PINNED_ROOT_CAPTURE'], 'a') as out:\n"
            "    out.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "if sys.argv[1] == 'run' and os.environ.get('PINNED_ROOT_WRITE_AGENT_UTILS_STATE') == '1':\n"
            "    mounts = [sys.argv[i + 1] for i, arg in enumerate(sys.argv) if arg == '--mount']\n"
            "    by_destination = {dict(field.split('=', 1) for field in mount.split(','))['destination']: dict(field.split('=', 1) for field in mount.split(','))['source'] for mount in mounts}\n"
            "    for destination in ['/src/agent-utils/rs/target', '/src/agent-utils/rs/.agent-utils-locks', '/src/agent-utils/rs/.agent-utils-snapshots']:\n"
            "        pathlib.Path(by_destination[destination], 'container-write').write_text(destination + '\\n')\n"
            "if sys.argv[1] == 'run' and os.environ.get('PINNED_ROOT_EXEC_PAYLOAD') == '1':\n"
            "    os.sched_setaffinity(0, json.loads(os.environ['PINNED_ROOT_RUNTIME_AFFINITY']))\n"
            "    leftover = json.loads(os.environ.get('PINNED_ROOT_RUNTIME_SIGNALS', '{}'))\n"
            "    signal.pthread_sigmask(signal.SIG_BLOCK, leftover.get('block', []))\n"
            "    for number in leftover.get('ignore', []):\n"
            "        signal.signal(number, signal.SIG_IGN)\n"
            "    image = next(i for i, arg in enumerate(sys.argv) if arg.startswith('fixture@sha256:'))\n"
            "    os.execv(sys.argv[image + 1], sys.argv[image + 1:])\n"
            "if sys.argv[1:3] == ['image', 'inspect']: print('c' * 64); sys.exit(0)\n"
            "if sys.argv[1:3] == ['image', 'exists'] and os.environ.get('PINNED_ROOT_IMAGE_EXISTS_RC'): sys.exit(int(os.environ['PINNED_ROOT_IMAGE_EXISTS_RC']))\n"
            "sys.exit(0 if sys.argv[1:3] == ['image', 'exists'] or sys.argv[1] == 'run' else 90)\n"
        )
        fake.chmod(0o755)

    def invoke(
        self,
        cargo_home="cargo",
        run_state=None,
        source="source",
        output="output",
        proc_locks_runtime=None,
        calibration=False,
        git_location=None,
        command=None,
        caller_affinity=None,
        project=None,
        bundle_lower=None,
        hermit_bundle=None,
        extra_env=None,
        extra_forwarded=None,
        prefix=None,
        home=None,
    ):
        # The wrapper gets this process's environment, as it does in
        # production, so an inherited Git location variable reaches it.
        # `git_location` replaces those variables with exactly the given ones.
        env = os.environ.copy()
        if git_location is not None:
            env = repository_neutral_env()
            env.update(git_location)
        env["PATH"] = str(self.root / "tools") + os.pathsep + env["PATH"]
        env["PINNED_ROOT_CAPTURE"] = str(self.capture)
        forwarded = ["--nextest-calibration"] if calibration else []
        if project is not None:
            forwarded += project

        if proc_locks_runtime is not None:
            env["XDG_RUNTIME_DIR"] = str(proc_locks_runtime)
            forwarded.append("--proc-locks-runtime")
        if run_state is not None:
            env["VALIDATE_RUN_STATE"] = str(run_state)
            forwarded += ["--env", "VALIDATE_RUN_STATE"]
        if bundle_lower is not None:
            forwarded += ["--bundle-lower", str(bundle_lower)]
        if hermit_bundle is not None:
            forwarded += ["--hermit-bundle", str(hermit_bundle)]
        if extra_env:
            env.update(extra_env)
        if extra_forwarded:
            forwarded += extra_forwarded
        # `home` pins the HOME the wrapper sees: a path, or "" for unset.
        # Seeding reads the caller's HOME, so no seeded-mode case may
        # inherit this process's real one.
        if home is not None:
            if home == "":
                env.pop("HOME", None)
            else:
                env["HOME"] = str(home)
        cargo_args = [] if cargo_home is None else ["--cargo-home", cargo_home]
        result = subprocess.run(
            [
                *(prefix or []),
                "bash",
                str(WRAPPER),
                "--src",
                str(source),
                "--out",
                output,
                *cargo_args,
                "--digest",
                "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                *forwarded,
                "--",
                *(command or ["/not-executed/command", "literal argument"]),
            ],
            cwd=self.root,
            env=env,
            capture_output=True,
            text=True,
            timeout=WRAPPER_TIMEOUT_S,
            preexec_fn=(
                None
                if caller_affinity is None
                else lambda: os.sched_setaffinity(0, caller_affinity)
            ),
        )
        calls = [json.loads(line) for line in self.capture.read_text().splitlines()] if self.capture.exists() else []
        return result, calls

    def check_image(self, exists_rc):
        env = repository_neutral_env()
        env["PATH"] = str(self.root / "tools") + os.pathsep + env["PATH"]
        env["PINNED_ROOT_CAPTURE"] = str(self.capture)
        env["PINNED_ROOT_IMAGE_EXISTS_RC"] = str(exists_rc)
        result = subprocess.run(
            [
                "bash",
                str(WRAPPER),
                "--check-image",
                "--digest",
                "fixture@sha256:" + "0" * 64,
            ],
            cwd=self.root,
            env=env,
            capture_output=True,
            text=True,
            timeout=WRAPPER_TIMEOUT_S,
        )
        calls = [json.loads(line) for line in self.capture.read_text().splitlines()]
        return result, [c for c in calls if c[:2] == ["image", "exists"]]

    def test_check_image_timeout_gets_one_logged_retry_and_still_refuses(self):
        # The probe's WALL bound is generous (120 s) and a timeout status is
        # retried once, logged; admission still refuses afterwards. The fake
        # podman answers 124 at once, so no real 120 s elapses.
        result, exists = self.check_image(124)
        self.assertEqual(result.returncode, 124)
        self.assertEqual(len(exists), 2)
        self.assertIn("retrying once", result.stderr)
        self.assertIn("120s query bound", result.stderr)

    def test_check_image_missing_image_is_not_retried(self):
        result, exists = self.check_image(1)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(len(exists), 1)
        self.assertIn("is not present locally", result.stderr)
    def test_paired_project_keeps_original_core_paths_and_relocates_real_git_metadata(self):
        env=repository_neutral_env()
        env.update(GIT_CONFIG_GLOBAL=os.devnull,GIT_CONFIG_NOSYSTEM='1',GIT_OPTIONAL_LOCKS='0')
        def git(root,*args):
            result=subprocess.run(['git','-c','user.name=fixture','-c','user.email=fixture@example.invalid','-C',str(root),*args],env=env,capture_output=True,text=True,timeout=10)
            self.assertEqual(result.returncode,0,result.stderr)
            return result.stdout.strip()
        core=self.root/'source';git(core,'init','-q');(core/'file').write_text('core');git(core,'add','file');git(core,'commit','-qm','core')
        seed=self.root/'project-seed';seed.mkdir();git(seed,'init','-q');(seed/'file').write_text('project');git(seed,'add','file');git(seed,'commit','-qm','project')
        project=self.root/'project';git(seed,'worktree','add','--detach',str(project))
        descriptor=project/'ci/project.json';descriptor.parent.mkdir();graph=project/'ci/graph.json';graph.write_text('{"steps":[]}\n'+' '*65536)
        descriptor.write_text(json.dumps({'schema':1,'repository':'/project','tools_root':'/src','graph':'/project/ci/graph.json','shared_package':'hermit-run'}))
        git(project,'add','ci');git(project,'commit','-qm','project inputs')
        original_head=git(project,'rev-parse','HEAD');original_index=git(project,'ls-files','--stage')
        result,calls=self.invoke(project=['--project-src',str(project),'--project-out',str(self.root/'project-output'),'--project-descriptor',str(descriptor)])
        self.assertEqual(result.returncode,0,result.stderr);argv=calls[-1]
        mounts=[argv[i+1] for i,arg in enumerate(argv) if arg=='--mount']
        self.assertIn(f'type=bind,source={core},destination=/src,ro=true',mounts)
        self.assertIn(f'type=bind,source={self.root}/output/target,destination=/src/target',mounts)
        self.assertIn(f'type=bind,source={project},destination=/project,ro=true',mounts)
        self.assertIn(f'type=bind,source={self.root}/project-output/target,destination=/project/target',mounts)
        self.assertIn('CARGO_TARGET_DIR=/project/target',argv);self.assertEqual(argv[argv.index('-w')+1],'/project')
        overlays=[dict(p.split('=',1) for p in m.split(',')) for m in mounts if 'git-root-configs.' in m]
        # A linked worktree without extensions.worktreeConfig finds its root
        # through its .git file, so its shared metadata is mounted where it is
        # and no private config is bound over it (issue 3920).
        common=git(project,'rev-parse','--path-format=absolute','--git-common-dir')
        private=git(project,'rev-parse','--path-format=absolute','--git-dir')
        self.assertIn(f'type=bind,source={common},destination={common},ro=true',mounts)
        self.assertIn(f'type=bind,source={private},destination={private},ro=true',mounts)
        self.assertEqual(overlays,[])
        proof=json.loads(next((self.root/'project-output').glob('source-binding.*.json')).read_text())
        self.assertEqual(proof['sources'][1]['head'],original_head);self.assertEqual(proof['sources'][0]['guest'],'/src')
        self.assertEqual(git(project,'rev-parse','HEAD'),original_head);self.assertEqual(git(project,'ls-files','--stage'),original_index)
        # A tracked graph link does not give its untracked target source custody.
        graph.unlink();(graph.parent/'untracked.json').write_text('{"steps":[]}')
        graph.symlink_to('untracked.json');git(project,'add','ci/graph.json');git(project,'commit','-qm','tracked graph symlink')
        self.capture.unlink(missing_ok=True)
        result,calls=self.invoke(project=['--project-src',str(project),'--project-out',str(self.root/'project-output'),'--project-descriptor',str(descriptor)])
        self.assertEqual(result.returncode,2);self.assertIn('graph has a symlink component',result.stderr)
        self.assertFalse(any(call and call[0]=='run' for call in calls))

    def test_paired_project_refuses_partial_or_aliasing_inputs_before_any_payload(self):
        descriptor=self.root/'descriptor.json';descriptor.write_text('{}')
        for arguments in (['--project-src',str(self.root/'source')],['--project-out',str(self.root/'output')],['--project-src-rw'],['--project-src',str(self.root/'source'),'--project-out',str(self.root/'different-output'),'--project-descriptor',str(descriptor)]):
            with self.subTest(arguments=arguments):
                self.capture.unlink(missing_ok=True)
                result,calls=self.invoke(project=arguments)
                self.assertNotEqual(result.returncode,0)
                self.assertFalse(any(call and call[0]=='run' for call in calls))
                self.assertIn('distinct and non-overlapping' if len(arguments)>2 else 'required together',result.stderr)

        # Cross-owner ancestors are as unsafe as descendants. Same-owner
        # source/output layouts keep the original admission rules.
        nested=self.root/'output/project';nested.mkdir(parents=True)
        separate=self.root/'other-project';separate.mkdir()
        for source,project_source,output,project_output in [(self.root/'source',nested,self.root/'output',self.root/'other-output'),(self.root/'source',separate,self.root/'core-output',self.root)]:
            with self.subTest(project_source=project_source,project_output=project_output):
                self.capture.unlink(missing_ok=True)
                result,calls=self.invoke(source=str(source),output=str(output),project=['--project-src',str(project_source),'--project-out',str(project_output),'--project-descriptor',str(descriptor)])
                self.assertEqual(result.returncode,2);self.assertIn('distinct and non-overlapping',result.stderr)
                self.assertFalse(any(call and call[0]=='run' for call in calls))

    def prepare_capture_fixture(self, probe_status=0, resolve_status=0):
        source = self.root / "source"
        published = source / "target/ci/rust-scripts"
        published.mkdir(parents=True, exist_ok=True)
        (published / "manifest.tsv").write_text("fixture producer manifest\n")
        runner = source / "ci/rust-script-bin/rust-script"
        runner.parent.mkdir(parents=True, exist_ok=True)
        runner.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys\n"
            "manifest = pathlib.Path(os.environ.get('HERMIT_RUST_SCRIPT_ARTIFACT_ROOT', '.'), 'manifest.tsv')\n"
            "if sys.argv[1] == '--resolve-optional' and (not manifest.is_file() or manifest.is_symlink()): sys.exit(2)\n"
            f"if sys.argv[1] == '--resolve-optional': print(pathlib.Path(__file__).resolve()) if {resolve_status} == 0 else None; sys.exit({resolve_status})\n"
            f"if sys.argv[-1] == '--probe': print('nextest-launch-observation-v2'); sys.exit({probe_status})\n"
            "assert sys.argv[1:3] == ['capture-launch', '--pinned-image']\n"
            "assert sys.argv[4:6] == ['--image-id', 'c' * 64]\n"
            "assert sys.argv[6] == '--output'\n"
            "path = pathlib.Path(sys.argv[7])\n"
            "fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o400)\n"
            "with os.fdopen(fd, 'w') as out: json.dump({'fixture_only': True, 'image': sys.argv[3]}, out)\n"
        )
        runner.chmod(0o755)

    def test_calibration_opt_in_uses_unique_read_only_proofs_and_exact_inspected_image(
        self,
    ):
        self.prepare_capture_fixture()
        retained = []
        for _ in range(2):
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(calibration=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(calls), 3)
            self.assertEqual(
                calls[1], ["image", "inspect", "--format", "{{.Id}}", calls[0][-1]]
            )
            self.assertEqual(calls[2][-8], calls[0][-1])
            mounts = [
                calls[2][i + 1]
                for i, value in enumerate(calls[2])
                if value == "--mount"
            ]
            proof_mounts = [
                m for m in mounts if "destination=/run/hermit-nextest-launch.json" in m
            ]
            self.assertEqual(len(proof_mounts), 1)
            fields = dict(part.split("=", 1) for part in proof_mounts[0].split(","))
            self.assertEqual(fields["ro"], "true")
            path = Path(fields["source"])
            self.assertTrue(path.is_relative_to(self.root / "output"))
            self.assertEqual(path.stat().st_mode & 0o777, 0o400)
            self.assertEqual(json.loads(path.read_text())["image"], calls[0][-1])
            retained.append((path, path.read_bytes()))
        self.assertNotEqual(retained[0][0], retained[1][0])
        self.assertEqual(retained[0][0].read_bytes(), retained[0][1])

    def test_bootstrap_and_unavailable_capture_do_not_compile_or_claim_a_proof(self):
        # Bootstrap remains independent of a producer which may itself use this wrapper.
        for installed, status, opt_in, expected in [
            (False, 0, True, 0),
            (True, 23, False, 0),
            (True, 2, True, 0),
            (True, 126, True, 126),
            (True, 127, True, 127),
        ]:
            with self.subTest(installed=installed, status=status, opt_in=opt_in):
                if installed:
                    self.prepare_capture_fixture(probe_status=status)
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(calibration=opt_in)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(len(calls), 2 if expected == 0 else 1)
                self.assertFalse(
                    any(
                        "hermit-nextest-launch.json" in arg
                        for call in calls
                        for arg in call
                    )
                )

    def test_malformed_preparation_is_distinct_from_missing_optional_capture(self):
        for status in [2, 3]:
            self.prepare_capture_fixture(resolve_status=status)
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(calibration=True)
            self.assertEqual(result.returncode, 2 if status == 2 else 0, result.stderr)
            self.assertEqual(len(calls), 1 if status == 2 else 2)
            self.assertFalse(
                any(
                    "hermit-nextest-launch.json" in arg
                    for call in calls
                    for arg in call
                )
            )

    def test_existing_invalid_manifest_is_not_optional_absence(self):
        self.prepare_capture_fixture()
        manifest = self.root / "source/target/ci/rust-scripts/manifest.tsv"
        manifest.unlink()
        for kind in ["directory", "dangling-symlink"]:
            if kind == "directory":
                manifest.mkdir()
            else:
                manifest.symlink_to(manifest.with_name("absent"))
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(calibration=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertEqual(len(calls), 1)
            if kind == "directory":
                manifest.rmdir()
            else:
                manifest.unlink()

    def test_imports_only_registry_and_git_from_the_host_cargo_home(self):
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(
            calls[0],
            [
                "image",
                "exists",
                "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ],
        )
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
        # The explicit home stays a writable bind: it is a build-phase home
        # cargo extracts into, never the caller's live ~/.cargo.
        self.assertTrue(
            all("ro=true" not in mount and "readonly" not in mount for mount in imports),
            imports,
        )
        self.assertIn(
            f"type=bind,source={self.root}/source,destination=/src,ro=true", mounts
        )
        self.assertIn(
            f"type=bind,source={self.root}/output/target,destination=/src/target",
            mounts,
        )
        self.assertIn(
            f"type=bind,source={self.root}/output/agent-utils-rs/target,destination=/src/agent-utils/rs/target",
            mounts,
        )
        self.assertIn(
            f"type=bind,source={self.root}/output/agent-utils-rs/locks,destination=/src/agent-utils/rs/.agent-utils-locks",
            mounts,
        )
        self.assertIn(
            f"type=bind,source={self.root}/output/agent-utils-rs/snapshots,destination=/src/agent-utils/rs/.agent-utils-snapshots",
            mounts,
        )
        self.assertIn("CARGO_HOME=/build/.cargo", argv)
        self.assertEqual(argv.count("--cgroups=disabled"), 1)
        self.assertFalse(
            any(arg.startswith(("--cgroup-parent", "--cgroupns")) for arg in argv)
        )
        self.assertIn("--network=none", argv)
        self.assertIn("--http-proxy=false", argv)
        self.assertEqual(
            argv[-8:],
            [
                "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "/bin/env",
                "--default-signal",
                "/bin/taskset",
                "--cpu-list",
                ",".join(map(str, sorted(os.sched_getaffinity(0)))),
                "/not-executed/command",
                "literal argument",
            ],
        )

    def test_payload_takes_no_optional_git_locks(self):
        # A read-only `git status` in one node otherwise creates and deletes
        # .git/index.lock in a Cargo git checkout that another node is copying
        # (71e738ab's validation: "cp: cannot stat '.git/index.lock'").
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = calls[1]
        settings = [argv[index + 1] for index, arg in enumerate(argv) if arg == "-e"]
        self.assertIn("GIT_OPTIONAL_LOCKS=0", settings)
        image = argv.index(
            "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"
        )
        self.assertLess(argv.index("GIT_OPTIONAL_LOCKS=0"), image)

    def test_payload_is_not_the_container_init(self):
        # The payload is exec'd as PID 1 of the container's PID namespace
        # unless Podman runs an init above it, and every orphan in the
        # namespace is then re-parented to the payload, which waits only for
        # its own children: one zombie per KVM run under test-harness. --init
        # must be a `podman run` option, before the image; after it, it would
        # be a payload argument.
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = calls[1]
        self.assertEqual(argv[0], "run")
        self.assertEqual(argv.count("--init"), 1)
        image = argv.index(
            "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"
        )
        self.assertLess(argv.index("--init"), image)

    def test_payload_starts_with_default_signal_actions_and_none_blocked(self):
        # Under --init the payload is catatonit's child, and catatonit leaves
        # SIGTSTP, SIGTTIN and SIGTTOU blocked there. The blocked mask survives
        # execve, so every process the payload started inherited it, and the
        # cli tests that compare a zombie's /proc/<pid>/stat signal fields with
        # Linux's answer failed. The fake runtime leaves those three blocked,
        # and also leaves three actions ignored (being Python, it ignores
        # SIGPIPE and SIGXFSZ itself as well), before it execs the payload.
        # The payload must see no signal blocked and none of 1-31 or 34-64
        # ignored; for 32 and 33 it keeps exactly this test's own setting.
        # glibc reserves signals 32 and 33 for its threads library, and sigaction refuses to change them.
        reserved = (1 << 31) | (1 << 32)
        own = signal_masks(Path("/proc/self/status").read_text())
        result, calls = self.invoke(
            command=["cat", "/proc/self/status"],
            extra_env={
                "PINNED_ROOT_EXEC_PAYLOAD": "1",
                "PINNED_ROOT_RUNTIME_AFFINITY": json.dumps(
                    sorted(os.sched_getaffinity(0))
                ),
                "PINNED_ROOT_RUNTIME_SIGNALS": json.dumps(
                    {
                        "block": [signal.SIGTSTP, signal.SIGTTIN, signal.SIGTTOU],
                        "ignore": [signal.SIGPIPE, signal.SIGXFSZ, signal.SIGTERM],
                    }
                ),
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        masks = signal_masks(result.stdout)
        self.assertEqual(sorted(masks), ["SigBlk", "SigIgn"], result.stdout)
        ignored = masks["SigIgn"]
        self.assertEqual(
            {
                "SigBlk": f"{masks['SigBlk']:016x}",
                "SigIgn 1-31": f"{ignored & 0x7FFFFFFF:016x}",
                "SigIgn 32-33": f"{ignored & reserved:016x}",
                "SigIgn 34-64": f"{ignored >> 33:016x}",
            },
            {
                "SigBlk": f"{0:016x}",
                "SigIgn 1-31": f"{0:016x}",
                "SigIgn 32-33": f"{own['SigIgn'] & reserved:016x}",
                "SigIgn 34-64": f"{0:016x}",
            },
        )

    def test_payload_uses_cargo_checksum_freshness(self):
        # Every pinned-root container shares /src/target, and Cargo marks a
        # unit dirty when its prior build used checksum freshness and this
        # one does not, so the setting must reach every container.
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = calls[1]
        settings = [argv[index + 1] for index, arg in enumerate(argv) if arg == "-e"]
        self.assertIn("CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true", settings)
        image = argv.index(
            "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"
        )
        self.assertLess(argv.index("CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true"), image)

    def test_payload_keeps_caller_affinity_when_runtime_widens_it(self):
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 2:
            self.skipTest("affinity expansion requires two available CPUs")
        inherited = available[::2]
        os.environ["PINNED_ROOT_EXEC_PAYLOAD"] = "1"
        os.environ["PINNED_ROOT_RUNTIME_AFFINITY"] = json.dumps(available)
        self.addCleanup(os.environ.pop, "PINNED_ROOT_EXEC_PAYLOAD", None)
        self.addCleanup(os.environ.pop, "PINNED_ROOT_RUNTIME_AFFINITY", None)
        # The fake runtime really widens its own affinity before exec. The
        # original payload must inherit the wrapper's narrower set, preserve
        # its literal argument, and return its own nonzero status unchanged.
        result, calls = self.invoke(
            caller_affinity=inherited,
            command=[
                sys.executable,
                "-c",
                "import json, os, sys; "
                "print(json.dumps([sorted(os.sched_getaffinity(0)), sys.argv[1:]])); "
                "sys.exit(23)",
                "literal argument",
            ],
        )
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(json.loads(result.stdout), [inherited, ["literal argument"]])
        self.assertEqual(sorted(os.sched_getaffinity(0)), available)
        self.assertEqual(len(calls), 2)

    def test_agent_utils_writable_state_is_confined_to_the_pinned_output(self):
        host_state = self.root / "source/agent-utils/rs"
        sentinels = {}
        for relative in ("target", ".agent-utils-locks", ".agent-utils-snapshots"):
            directory = host_state / relative
            sentinel = directory / "host-sentinel"
            sentinel.write_bytes((relative + "\n").encode())
            sentinels[relative] = (
                sentinel.read_bytes(),
                sentinel.stat().st_mtime_ns,
                directory.stat().st_mtime_ns,
            )

        os.environ["PINNED_ROOT_WRITE_AGENT_UTILS_STATE"] = "1"
        self.addCleanup(os.environ.pop, "PINNED_ROOT_WRITE_AGENT_UTILS_STATE", None)
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)

        for relative, before in sentinels.items():
            directory = host_state / relative
            sentinel = directory / "host-sentinel"
            self.assertEqual(
                (
                    sentinel.read_bytes(),
                    sentinel.stat().st_mtime_ns,
                    directory.stat().st_mtime_ns,
                ),
                before,
                f"host agent-utils {relative} state changed",
            )
            self.assertFalse((directory / "container-write").exists())

        output_state = self.root / "output/agent-utils-rs"
        expected = {
            "target": "/src/agent-utils/rs/target\n",
            "locks": "/src/agent-utils/rs/.agent-utils-locks\n",
            "snapshots": "/src/agent-utils/rs/.agent-utils-snapshots\n",
        }
        for relative, contents in expected.items():
            marker = output_state / relative / "container-write"
            self.assertEqual(marker.read_text(), contents)
            self.assertTrue(marker.is_relative_to(self.root / "output"))

    def test_proc_locks_mount_reuses_the_native_host_inode(self):
        runtime = self.root / "runtime with spaces"
        runtime.mkdir(mode=0o700)
        lease = runtime / "hermit-proc-locks-determinism.lock"
        lease.write_bytes(b"preserve an existing host lease")
        lease.chmod(0o600)
        before = lease.stat()
        for output in ["first-container-output", "second-container-output"]:
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(output=output, proc_locks_runtime=runtime)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(calls), 2)
            argv = calls[1]
            self.assertIn(
                "/run/hermit-proc-locks:rw,nosuid,nodev,noexec,mode=0700", argv
            )
            self.assertIn(
                f"type=bind,source={lease},destination=/run/hermit-proc-locks/hermit-proc-locks-determinism.lock",
                argv,
            )
            self.assertIn("XDG_RUNTIME_DIR=/run/hermit-proc-locks", argv)
            self.assertIn(
                f"HERMIT_PROC_LOCKS_LEASE_ID={before.st_dev}:{before.st_ino}", argv
            )
            self.assertFalse(any(f"source={runtime}," in arg for arg in argv))
            self.assertEqual(lease.read_bytes(), b"preserve an existing host lease")
            after = lease.stat()
            self.assertEqual(
                (
                    after.st_dev,
                    after.st_ino,
                    after.st_uid,
                    after.st_mode,
                    after.st_mtime_ns,
                ),
                (
                    before.st_dev,
                    before.st_ino,
                    before.st_uid,
                    before.st_mode,
                    before.st_mtime_ns,
                ),
            )
        # The real wrapper and host file opens executed above; Podman was a
        # recorder. This is not a container UID-mapping or flock experiment.

    def test_proc_locks_mount_refuses_unsafe_inputs_before_podman_run(self):
        for case in [
            "missing",
            "public-directory",
            "directory-symlink",
            "public-file",
            "fifo",
            "file-symlink",
            "mount-delimiter",
        ]:
            with self.subTest(case=case):
                runtime = self.root / case
                runtime.mkdir(mode=0o700)
                lease = runtime / "hermit-proc-locks-determinism.lock"
                if case == "missing":
                    runtime.rmdir()
                elif case == "public-directory":
                    runtime.chmod(0o755)
                elif case == "directory-symlink":
                    target = self.root / "real-private-directory"
                    runtime.rename(target)
                    runtime.symlink_to(target, target_is_directory=True)
                elif case == "public-file":
                    lease.write_bytes(b"preserve unsafe bytes")
                    lease.chmod(0o666)
                elif case == "fifo":
                    os.mkfifo(lease, 0o600)
                elif case == "file-symlink":
                    target = runtime / "other-file"
                    target.write_bytes(b"preserve symlink target")
                    target.chmod(0o600)
                    lease.symlink_to(target)
                elif case == "mount-delimiter":
                    other = self.root / "invalid,mount"
                    runtime.rename(other)
                    runtime = other
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(proc_locks_runtime=runtime)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(
                    calls,
                    [
                        [
                            "image",
                            "exists",
                            "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                        ]
                    ],
                )

    def test_normalized_output_with_spaces_keeps_the_exact_private_home(self):
        result, calls = self.invoke(output="unused directory/../output with spaces")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        output = self.root / "output with spaces"
        private_home = output / "home"
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        self.assertIn(
            f"type=bind,source={output / 'target'},destination=/src/target", mounts
        )
        self.assertIn(f"type=bind,source={private_home},destination=/build", mounts)
        for cache in ("registry", "git"):
            self.assertTrue((private_home / ".cargo" / cache).is_dir(), cache)
            self.assertIn(
                f"type=bind,source={self.root / 'cargo' / cache},destination=/build/.cargo/{cache}",
                mounts,
            )
        self.assertFalse((self.root / "unused directory").exists())
        self.assertIn("HOME=/build", argv)
        self.assertIn("CARGO_HOME=/build/.cargo", argv)
        self.assertIn("--network=none", argv)
        self.assertIn("--http-proxy=false", argv)
        self.assertEqual(
            argv[-8:],
            [
                "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "/bin/env",
                "--default-signal",
                "/bin/taskset",
                "--cpu-list",
                ",".join(map(str, sorted(os.sched_getaffinity(0)))),
                "/not-executed/command",
                "literal argument",
            ],
        )

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
        self.assertEqual(
            calls,
            [
                [
                    "image",
                    "exists",
                    "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                ]
            ],
        )

    def make_home_with_caches(self):
        home = self.root / "home"
        for directory in (".cargo/registry", ".cargo/git", ".cargo/bin"):
            (home / directory).mkdir(parents=True, exist_ok=True)
        (home / ".cargo/config.toml").write_text(
            "host configuration must not be imported\n"
        )
        (home / ".cargo/bin/cargo-clippy").write_text(
            "host executable must not be imported\n"
        )
        return home

    def cargo_volumes(self, argv):
        return [
            argv[index + 1]
            for index, arg in enumerate(argv)
            if arg == "--volume" and ":/build/.cargo/" in argv[index + 1]
        ]

    def test_default_seeds_home_caches_under_a_private_overlay(self):
        home = self.make_home_with_caches()
        result, calls = self.invoke(cargo_home=None, home=home)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        self.assertEqual(
            self.cargo_volumes(argv),
            [
                f"{home}/.cargo/registry:/build/.cargo/registry:O",
                f"{home}/.cargo/git:/build/.cargo/git:O",
            ],
        )
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        self.assertFalse(any(f"source={home}" in mount for mount in mounts))
        self.assertFalse(any(".cargo/bin" in arg for arg in argv))
        self.assertIn(
            f"seeding cargo dependency caches from {home}/.cargo under a private overlay",
            result.stderr,
        )
        out = self.root / "output"
        pinned_home = out / "home"
        for cache in ("registry", "git"):
            self.assertTrue((pinned_home / ".cargo" / cache).is_dir(), cache)

    def test_default_without_home_caches_seeds_nothing(self):
        home = self.root / "empty-home"
        home.mkdir()
        result, calls = self.invoke(
            cargo_home=None, home=home, command=["cargo", "build"]
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        self.assertEqual(self.cargo_volumes(argv), [])
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        self.assertFalse(any(f"source={home}" in mount for mount in mounts))
        self.assertNotIn("seeding cargo dependency caches", result.stderr)
        self.assertIn("offline cargo builds need populated caches", result.stderr)
        self.assertNotIn("caches are mounted read-only", result.stderr)

    def test_cargo_home_none_seeds_nothing_even_with_home_caches(self):
        home = self.make_home_with_caches()
        result, calls = self.invoke(cargo_home="none", home=home)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        self.assertEqual(self.cargo_volumes(argv), [])
        mounts = [argv[index + 1] for index, arg in enumerate(argv) if arg == "--mount"]
        self.assertFalse(any(f"source={home}" in mount for mount in mounts))
        self.assertNotIn("seeding cargo dependency caches", result.stderr)

    def test_home_unset_seeds_nothing_and_names_it(self):
        result, calls = self.invoke(
            cargo_home=None, home="", command=["cargo", "build"]
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(self.cargo_volumes(calls[1]), [])
        self.assertIn("HOME is unset, so no cargo caches are seeded", result.stderr)

    def test_empty_cargo_home_value_is_refused_by_name(self):
        result, calls = self.invoke(cargo_home="")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--cargo-home was given an empty value", result.stderr)
        self.assertEqual(calls, [])

    def test_no_home_caches_with_a_non_cargo_payload_is_silent(self):
        home = self.root / "empty-home"
        home.mkdir()
        result, calls = self.invoke(cargo_home=None, home=home)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(self.cargo_volumes(calls[1]), [])
        self.assertNotIn("offline cargo builds", result.stderr)
        self.assertNotIn("seeding cargo dependency caches", result.stderr)

    def test_seeded_home_path_with_a_colon_is_refused_by_name(self):
        home = self.root / "ho:me"
        (home / ".cargo/registry").mkdir(parents=True)
        result, calls = self.invoke(cargo_home=None, home=home)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn(
            "an overlay volume cannot name a source path containing ':'",
            result.stderr,
        )
        self.assertEqual(
            calls,
            [
                [
                    "image",
                    "exists",
                    "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                ]
            ],
        )

    def test_buck_cells_and_fixture_stage_pin_no_cargo_caches(self):
        buck_e2e = WRAPPER.parent.parent / "buck-e2e"
        cell = (buck_e2e / "cell.sh").read_text()
        self.assertIn(
            '"$wrapper" --src "$R" --out "$W/pinned" --src-rw --cargo-home none',
            cell,
        )
        stage = (buck_e2e / "stage").read_text()
        self.assertIn("--src-rw --cargo-home none --", stage)

    def test_run_state_uses_the_same_host_directory_for_each_pinned_command(self):
        run_state = self.root / "run state"
        run_state.mkdir()
        (run_state / "fixture").write_bytes(b"existing fixture")
        result, calls = self.invoke(run_state=run_state)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        self.assertIn(
            f"type=bind,source={run_state},destination=/validate-run-state", argv
        )
        self.assertIn("VALIDATE_RUN_STATE=/validate-run-state", argv)
        self.assertEqual((run_state / "fixture").read_bytes(), b"existing fixture")
        self.assertEqual(argv[-2:], ["/not-executed/command", "literal argument"])

    def test_census_base_dir_is_mounted_and_rewritten_for_the_guest(self):
        # The queue supplies the stamped census base as host paths outside
        # the source mount; the guest must see the directory and the files
        # inside it at rewritten guest paths, or counted nodes refuse.
        census = self.root / "census base"
        census.mkdir()
        (census / "cli.listed-ids").write_text("pkg::bin$one\n")
        (census / "dropped.txt").write_text("")
        result, calls = self.invoke(
            extra_env={
                "HERMIT_CENSUS_BASE_COMMIT": "a" * 40,
                "HERMIT_CENSUS_BASE_DIR": str(census),
                "HERMIT_CENSUS_BASE_LISTED_IDS": str(census / "cli.listed-ids"),
                "HERMIT_CENSUS_DROPPED_FILE": str(census / "dropped.txt"),
            },
            extra_forwarded=[
                "--env", "HERMIT_CENSUS_BASE_COMMIT",
                "--env", "HERMIT_CENSUS_BASE_DIR",
                "--env", "HERMIT_CENSUS_BASE_LISTED_IDS",
                "--env", "HERMIT_CENSUS_DROPPED_FILE",
            ],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = calls[-1]
        self.assertIn(
            f"type=bind,source={census},destination=/census-base,ro=true", argv)
        self.assertIn("HERMIT_CENSUS_BASE_DIR=/census-base", argv)
        self.assertIn(
            "HERMIT_CENSUS_BASE_LISTED_IDS=/census-base/cli.listed-ids", argv)
        self.assertIn("HERMIT_CENSUS_DROPPED_FILE=/census-base/dropped.txt", argv)


    def hermit_bundle_fixture(self, kind="binary-only", tamper=False, install=False, binary=None):
        import hashlib as _hl
        hermit = binary if binary is not None else b"#!/bin/sh\n[ \"$1\" = --version ] && echo 'hermit fixture 0.0'\n"
        binary_hash = _hl.sha256(hermit).hexdigest()
        identity = _hl.sha256(f"{kind}\n{binary_hash}\nnone\nnone\n".encode()).hexdigest()
        bundle = self.root / identity
        bundle.mkdir(parents=True)
        (bundle / "kind").write_text(kind)
        hermit_path = bundle / "hermit"
        hermit_path.write_bytes(hermit + (b"# tampered\n" if tamper else b""))
        hermit_path.chmod(0o755)
        (bundle / "hermit.sha256").write_text(binary_hash)
        if install:
            (bundle / "install").mkdir()
        return bundle

    def test_hermit_bundle_is_mounted_verified_and_named(self):
        bundle = self.hermit_bundle_fixture()
        result, calls = self.invoke(hermit_bundle=bundle)
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = calls[-1]
        self.assertIn(
            f"type=bind,source={bundle},destination=/hermit-bundle,ro=true", argv
        )
        self.assertIn("HERMIT_BIN=/hermit-bundle/hermit", argv)
        self.assertFalse(any(arg.startswith("HERMIT_INSTALL_DIR=") for arg in argv))
        self.assertIn("hermit bundle verified:", result.stderr)

    def fake_elf_tools(self, missing=None):
        # Fake readelf/ldd, first on PATH through the test tools dir, so a
        # planted ELF reaches the wrapper's ABI preflight deterministically.
        readelf = self.root / "tools/readelf"
        readelf.write_text(
            f"#!{sys.executable}\n"
            "import sys\n"
            "if '-d' in sys.argv:\n"
            "    print('0x0000000000000001 (NEEDED)             Shared library: [libfixture.so.1]')\n"
            "else:\n"
            "    print('  Name: GLIBC_2.34  Flags: none  Version: 2')\n"
        )
        readelf.chmod(0o755)
        ldd = self.root / "tools/ldd"
        body = (
            "print('libmissing-fixture.so.1 => not found')\n"
            if missing
            else "print('libfixture.so.1 => /lib/libfixture.so.1 (0x00007f0000000000)')\n"
        )
        ldd.write_text(f"#!{sys.executable}\n{body}")
        ldd.chmod(0o755)

    def test_a_tampered_hermit_bundle_is_refused_before_launch(self):
        bundle = self.hermit_bundle_fixture(tamper=True)
        result, calls = self.invoke(hermit_bundle=bundle)
        self.assertEqual(result.returncode, 2)
        self.assertIn("failed artifact verification", result.stderr)
        self.assertIn("published Hermit hash mismatch", result.stderr)
        self.assertFalse(any(call and call[0] == "run" for call in calls))

    def test_a_complete_bundle_without_install_is_refused_before_launch(self):
        bundle = self.hermit_bundle_fixture(kind="complete")
        result, calls = self.invoke(hermit_bundle=bundle)
        self.assertEqual(result.returncode, 2)
        self.assertIn("failed artifact verification", result.stderr)
        self.assertIn("resource bundle has no rsrcs directory", result.stderr)
        self.assertFalse(any(call and call[0] == "run" for call in calls))

    def test_bundle_elf_with_an_unresolved_library_is_refused_by_name(self):
        self.fake_elf_tools(missing="libmissing-fixture.so.1")
        bundle = self.hermit_bundle_fixture(binary=b"\x7fELF planted fixture\n")
        result, calls = self.invoke(hermit_bundle=bundle)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("DT_NEEDED: libfixture.so.1", result.stderr)
        self.assertIn("libmissing-fixture.so.1 => not found", result.stderr)
        self.assertFalse(any(call and call[0] == "run" for call in calls))

    def test_bundle_binary_whose_version_fails_is_refused_before_launch(self):
        self.fake_elf_tools()
        bundle = self.hermit_bundle_fixture(binary=b"\x7fELF planted fixture\n")
        result, calls = self.invoke(hermit_bundle=bundle)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("--version exited nonzero", result.stderr)
        self.assertFalse(any(call and call[0] == "run" for call in calls))

    def test_empty_hermit_bundle_value_is_refused_by_name(self):
        result, calls = self.invoke(hermit_bundle="")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("--hermit-bundle was given an empty value", result.stderr)
        self.assertFalse(any(call and call[0] == "run" for call in calls))

    def test_check_image_refuses_hermit_bundle(self):
        env = repository_neutral_env()
        env["PATH"] = str(self.root / "tools") + os.pathsep + env["PATH"]
        env["PINNED_ROOT_CAPTURE"] = str(self.capture)
        result = subprocess.run(
            [
                "bash", str(WRAPPER),
                "--check-image",
                "--digest", "fixture@sha256:" + "0" * 64,
                "--hermit-bundle", str(self.hermit_bundle_fixture()),
            ],
            cwd=self.root, env=env,
            capture_output=True, text=True, timeout=WRAPPER_TIMEOUT_S,
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("--check-image accepts only optional --digest", result.stderr)

    def test_forwarded_hermit_install_dir_is_refused_with_a_bundle(self):
        bundle = self.hermit_bundle_fixture()
        result, calls = self.invoke(
            hermit_bundle=bundle,
            extra_env={"HERMIT_INSTALL_DIR": "/host/install"},
            extra_forwarded=["--env", "HERMIT_INSTALL_DIR"],
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("HERMIT_INSTALL_DIR is bundle-owned", result.stderr)
        self.assertFalse(any(call and call[0] == "run" for call in calls))

    def test_bundle_lower_is_a_read_only_bind_that_adds_no_link(self):
        # ci/buck-e2e/cell.sh shares one staged bundle across concurrent cells as
        # an overlay lower. A bind leaves its files' link counts alone; a guest
        # stat reports them, so a hard-linked copy per cell diverged run 1 from
        # run 2.
        bundle = self.root / "staged bundle"
        (bundle / "src").mkdir(parents=True)
        (bundle / "src/test.sh").write_text("#!/bin/sh\n")
        result, calls = self.invoke(bundle_lower=bundle)
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = calls[-1]
        self.assertEqual(argv[0], "run")
        self.assertIn(
            f"type=bind,source={bundle},destination=/bundle-lower,ro=true", argv
        )
        self.assertEqual((bundle / "src/test.sh").stat().st_nlink, 1)

    def test_no_bundle_lower_mounts_nothing_there(self):
        result, calls = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("/bundle-lower" in arg for arg in calls[-1]))

    def test_unusable_bundle_lower_refuses_before_container_launch(self):
        comma = self.root / "a,b"
        comma.mkdir()
        for lower, message in (
            (self.root / "missing", "is not a directory"),
            (comma, "cannot contain a comma"),
        ):
            with self.subTest(lower=lower):
                self.capture.write_text("")
                result, calls = self.invoke(bundle_lower=lower)
                self.assertEqual(result.returncode, 2)
                self.assertIn(message, result.stderr)
                self.assertFalse(any(call[0] == "run" for call in calls), calls)

    def kernel_header_mounts(self, release=None, prefix=None, fake_uname=True):
        """Run the wrapper, under a fake `uname -r` printing RELEASE when one
        is given, and return its result and every --mount under /lib/modules.
        Whatever is bound, the container is told where the running kernel's
        headers belong (HERMIT_RUNNING_KERNEL_UAPI_ASM), exactly once, so the
        syscall-table test reads only there. With PREFIX and fake_uname off,
        the release instead comes from the prefix itself (setarch)."""
        if release is not None and fake_uname:
            uname = self.root / "tools/uname"
            uname.write_text(f"#!/bin/sh\nprintf '%s\\n' '{release}'\n")
            uname.chmod(0o755)
        result, calls = self.invoke(prefix=prefix)
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = calls[-1]
        self.assertEqual(argv[0], "run")
        guest = f"/lib/modules/{release or os.uname().release}/build/arch/x86/include/generated/uapi/asm"
        named = [
            argv[i + 1]
            for i, arg in enumerate(argv[:-1])
            if arg == "-e" and argv[i + 1].startswith("HERMIT_RUNNING_KERNEL_UAPI_ASM=")
        ]
        self.assertEqual(named, [f"HERMIT_RUNNING_KERNEL_UAPI_ASM={guest}"])
        mounts = [argv[i + 1] for i, arg in enumerate(argv) if arg == "--mount"]
        headers = [m for m in mounts if "/lib/modules/" in m]
        # A bound run says so in one stderr line (the node's log, outside
        # any test output capture); an unbound run never claims a bind.
        said = [
            line for line in result.stderr.splitlines() if "bound host syscall header:" in line
        ]
        if headers:
            source = dict(field.split("=", 1) for field in headers[0].split(","))["source"]
            self.assertEqual(
                said,
                [
                    "run-in-pinned-root: bound host syscall header: "
                    f"{guest}/unistd_64.h (ro, from {source})"
                ],
            )
        else:
            self.assertEqual(said, [])
        return result, headers

    def test_the_running_kernel_header_directory_name_is_wrapper_owned(self):
        result, calls = self.invoke(
            extra_env={"HERMIT_RUNNING_KERNEL_UAPI_ASM": "/elsewhere"},
            extra_forwarded=["--env", "HERMIT_RUNNING_KERNEL_UAPI_ASM"],
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("HERMIT_RUNNING_KERNEL_UAPI_ASM is wrapper-owned", result.stderr)
        self.assertFalse(any(call[0] == "run" for call in calls), calls)

    def test_running_kernel_uapi_headers_are_bound_read_only_where_the_host_has_them(self):
        # Detcore's syscall-table test reads the running kernel's
        # unistd_64.h from this directory first. The container shares the
        # host's kernel, so the same release names the same path inside.
        release = os.uname().release
        guest = f"/lib/modules/{release}/build/arch/x86/include/generated/uapi/asm"
        result, headers = self.kernel_header_mounts()
        if os.path.isdir(guest):
            self.assertEqual(
                headers,
                [f"type=bind,source={os.path.realpath(guest)},destination={guest},ro=true"],
            )
        else:
            self.assertEqual(headers, [])
            self.assertIn(f"uapi headers are absent at {guest}", result.stderr)

    def test_a_host_without_uapi_headers_binds_none_and_says_so(self):
        release = "0.0.0-hermit-fixture-without-headers"
        result, headers = self.kernel_header_mounts(release)
        self.assertEqual(headers, [])
        self.assertIn(
            "uapi headers are absent at "
            f"/lib/modules/{release}/build/arch/x86/include/generated/uapi/asm;",
            result.stderr,
        )

    def test_uapi_headers_podman_cannot_name_exactly_are_not_bound(self):
        result, headers = self.kernel_header_mounts("0.0.0-hermit,fixture")
        self.assertEqual(headers, [])
        self.assertIn("cannot name /lib/modules/0.0.0-hermit,fixture/", result.stderr)

    def _other_release_prefix(self, fixture):
        """(release, prefix) running the wrapper so its `uname -r` reports a
        release that is not this host's, in a private mount namespace whose
        /lib/modules is a tmpfs holding only FIXTURE's trees: kernels/<R'>
        for R' and decoy/ for the running release. setarch --uname-2.6
        changes the uname(2) release too, so this holds even if the wrapper
        ever stops shelling out to uname(1). Skips (never counts) where the
        capability is refused."""
        for argv, capability in (
            (["unshare", "-rm", "/bin/true"], "unprivileged user+mount namespaces (unshare -rm)"),
            (["setarch", "x86_64", "--uname-2.6", "/bin/true"], "setarch x86_64 --uname-2.6 personality"),
        ):
            probe = subprocess.run(argv, capture_output=True, text=True, timeout=30)
            if probe.returncode != 0:
                self.skipTest(f"{capability} refused here: {probe.stderr.strip()}")
        other = subprocess.run(
            ["setarch", "x86_64", "--uname-2.6", "uname", "-r"],
            capture_output=True, text=True, timeout=30, check=True,
        ).stdout.strip()
        running = os.uname().release
        self.assertNotEqual(other, running)
        setup = (
            'mount -t tmpfs tmpfs /lib/modules || exit 125; '
            'mkdir -p "/lib/modules/$1" "/lib/modules/$3" || exit 125; '
            'ln -s "$2/kernels/$1" "/lib/modules/$1/build" || exit 125; '
            'ln -s "$2/decoy" "/lib/modules/$3/build" || exit 125; '
            'shift 3; exec "$@"'
        )
        return other, [
            "setarch", "x86_64", "--uname-2.6",
            "unshare", "-rm", "/bin/sh", "-c", setup, "sh",
            other, str(fixture), running,
        ]

    def _header_fixture(self, base, release, with_header=True):
        # The wrapper realpath()s the source, so compare against the
        # resolved fixture path (a symlinked temp dir would differ).
        base = Path(os.path.realpath(base))
        asm = base / "kernels" / release / "arch/x86/include/generated/uapi/asm"
        asm.mkdir(parents=True)
        if with_header:
            (asm / "unistd_64.h").write_text("#define __NR_read 0\n")
        decoy = base / "decoy/arch/x86/include/generated/uapi/asm"
        decoy.mkdir(parents=True)
        (decoy / "unistd_64.h").write_text("#define __NR_decoy 9999\n")
        return base

    def test_another_kernels_headers_are_bound_at_its_own_release(self):
        # The bound branch with a release that is not the running one: the
        # only /lib/modules trees in the namespace are R' (real) and the
        # running release's decoy, so a wrapper that used the running
        # release anywhere would bind the decoy and fail by name here.
        running = os.uname().release
        probe_root = self.root / "probe-fixture"
        other, _ = self._other_release_prefix(probe_root)
        fixture = self._header_fixture(self.root / "fixture", other)
        other, prefix = self._other_release_prefix(fixture)
        result, headers = self.kernel_header_mounts(
            other, prefix=prefix, fake_uname=False
        )
        guest = f"/lib/modules/{other}/build/arch/x86/include/generated/uapi/asm"
        self.assertEqual(
            headers,
            [
                "type=bind,"
                f"source={fixture}/kernels/{other}/arch/x86/include/generated/uapi/asm,"
                f"destination={guest},ro=true"
            ],
        )
        argv = self.capture.read_text()
        self.assertNotIn(running, argv)
        self.assertNotIn("decoy", argv)
        self.assertNotIn(running, result.stderr)
        self.assertNotIn("decoy", result.stderr)

    def test_another_kernel_without_headers_binds_none_and_says_so(self):
        fixture = self._header_fixture(self.root / "fixture", "unused", with_header=False)
        # with_header=False still makes kernels/unused; R' stays absent.
        other, prefix = self._other_release_prefix(fixture)
        # No kernels/<R'> tree exists under the fixture, only the decoy.
        result, headers = self.kernel_header_mounts(
            other, prefix=prefix, fake_uname=False
        )
        self.assertEqual(headers, [])
        self.assertIn(
            "run-in-pinned-root: the running kernel's uapi headers are absent at "
            f"/lib/modules/{other}/build/arch/x86/include/generated/uapi/asm;",
            result.stderr,
        )

    def test_a_comma_in_the_host_header_path_is_not_bound(self):
        # The existing comma case puts the comma in the release; this one
        # puts it in the resolved HOST path (the mount source).
        other, _ = self._other_release_prefix(self.root / "probe-fixture")
        fixture = self._header_fixture(self.root / "src,x", other)
        other, prefix = self._other_release_prefix(fixture)
        result, headers = self.kernel_header_mounts(
            other, prefix=prefix, fake_uname=False
        )
        self.assertEqual(headers, [])
        self.assertIn(
            f"cannot name /lib/modules/{other}/build/arch/x86/include/generated/uapi/asm "
            f"(host path {fixture}/kernels/{other}/arch/x86/include/generated/uapi/asm)",
            result.stderr,
        )

    def test_relative_run_state_refuses_before_container_launch(self):
        result, calls = self.invoke(run_state="relative-state")
        self.assertEqual(result.returncode, 2)
        self.assertIn("VALIDATE_RUN_STATE must be absolute", result.stderr)
        self.assertEqual(
            calls,
            [
                [
                    "image",
                    "exists",
                    "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                ]
            ],
        )

    def test_relocates_real_nested_submodule_configs_without_changing_host_metadata(
        self,
    ):
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = repository_neutral_env()
        git_env.update(
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_OPTIONAL_LOCKS="0",
        )

        def git(root, *args):
            result = subprocess.run(
                [
                    git_bin,
                    "-c",
                    "protocol.file.allow=always",
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-C",
                    str(root),
                    *args,
                ],
                env=git_env,
                capture_output=True,
                timeout=10,
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
                source = self.root / (
                    "separate-source" if separate_metadata else "plain-source"
                )
                options = (
                    ["--separate-git-dir", str(self.root / "super-metadata")]
                    if separate_metadata
                    else []
                )
                git(self.root, "clone", "-q", *options, str(superproject), str(source))
                git(source, "submodule", "update", "--init", "--recursive")
                paths = ["third-party/fixture", "third-party/fixture/nested child"]
                metadata = {}
                for path in paths:
                    directory = Path(
                        git(source / path, "rev-parse", "--absolute-git-dir")
                        .decode()
                        .strip()
                    )
                    metadata[path] = directory
                nested = metadata[paths[1]]
                original_worktree = (
                    git(source / paths[1], "config", "--get", "core.worktree")
                    .decode()
                    .strip()
                )
                git(source / paths[1], "config", "extensions.worktreeConfig", "true")
                git(
                    source / paths[1],
                    "config",
                    "--file",
                    str(nested / "config.worktree"),
                    "core.worktree",
                    original_worktree,
                )
                git(
                    source / paths[1],
                    "config",
                    "--file",
                    str(nested / "config.worktree"),
                    "fixture.value",
                    "preserve this value",
                )
                before = {
                    str(file): file.read_bytes()
                    for directory in metadata.values()
                    for file in (
                        directory / "config",
                        directory / "config.worktree",
                        directory / "index",
                    )
                    if file.exists()
                }
                heads = {
                    path: git(source / path, "rev-parse", "HEAD") for path in paths
                }
                indexes = {
                    path: git(source / path, "ls-files", "--stage", "-z")
                    for path in paths
                }
                objects = {
                    path: git(source / path, "show", "HEAD:payload") for path in paths
                }
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
                self.assertEqual(
                    len(overlays),
                    3,
                    "both nested configs and config.worktree must relocate",
                )
                for path, directory in metadata.items():
                    raw = (
                        (source / path / ".git")
                        .read_text()
                        .removeprefix("gitdir: ")
                        .strip()
                    )
                    guest_dir = os.path.normpath(os.path.join("/src", path, raw))
                    for name in ("config", "config.worktree"):
                        original = directory / name
                        if not original.exists():
                            continue
                        copied = overlays[guest_dir + "/" + name]
                        self.assertEqual(
                            git(
                                source,
                                "config",
                                "--file",
                                str(copied),
                                "--get",
                                "core.worktree",
                            )
                            .decode()
                            .strip(),
                            "/src/" + path,
                        )

                        def other_values(config, source=source):
                            values = git(
                                source,
                                "config",
                                "--file",
                                str(config),
                                "--null",
                                "--list",
                            ).split(b"\0")
                            return [
                                value
                                for value in values
                                if not value.startswith(b"core.worktree\n")
                            ]

                        self.assertEqual(other_values(copied), other_values(original))
                    self.assertEqual(
                        git(source / path, "rev-parse", "HEAD"), heads[path]
                    )
                    self.assertEqual(
                        git(source / path, "ls-files", "--stage", "-z"), indexes[path]
                    )
                    self.assertEqual(
                        git(source / path, "show", "HEAD:payload"), objects[path]
                    )
                for file, contents in before.items():
                    self.assertEqual(Path(file).read_bytes(), contents, file)

    def test_relocates_nested_linked_worktrees_with_external_common_metadata(self):
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = repository_neutral_env()
        git_env.update(
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_OPTIONAL_LOCKS="0",
        )

        def git(root, *args):
            result = subprocess.run(
                [
                    git_bin,
                    "-c",
                    "protocol.file.allow=always",
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-C",
                    str(root),
                    *args,
                ],
                env=git_env,
                capture_output=True,
                timeout=10,
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

        leaf = seed("external-leaf")
        child = seed("external-child")
        git(child, "submodule", "add", "-q", str(leaf), "nested leaf")
        git(child, "commit", "-qam", "nested fixture")
        product = seed("external-product")
        git(product, "submodule", "add", "-q", str(child), "nested child")
        git(product, "commit", "-qam", "product fixture")
        source = self.root / "linked-source"
        git(product, "worktree", "add", "--detach", str(source))
        paths = ["nested child", "nested child/nested leaf"]
        # These child worktrees share the external seed stores, not the root's
        # metadata tree. Their unchanged commondir files must resolve in /src.
        git(child, "worktree", "add", "--detach", str(source / paths[0]))
        git(leaf, "worktree", "add", "--detach", str(source / paths[1]))
        metadata = {}
        for path in paths:
            repo = source / path
            git(repo, "config", "extensions.worktreeConfig", "true")
            git(repo, "config", "--worktree", "core.worktree", str(repo))
            git(repo, "config", "--worktree", "fixture.value", "preserve child value")
            directory = Path(
                git(repo, "rev-parse", "--absolute-git-dir").decode().strip()
            )
            common = Path(
                git(repo, "rev-parse", "--path-format=absolute", "--git-common-dir")
                .decode()
                .strip()
            )
            self.assertNotEqual(directory, common)
            self.assertFalse(common.is_relative_to(product / ".git"))
            metadata[path] = (directory, common)
        self.assertEqual(
            git(source, "status", "--porcelain=v1", "--ignore-submodules=none"), b""
        )
        before = {
            str(f): f.read_bytes()
            for directory, common in metadata.values()
            for f in (
                directory / "HEAD",
                directory / "index",
                directory / "commondir",
                directory / "config.worktree",
                common / "config",
            )
            if f.is_file()
        }
        gitfiles = {path: (source / path / ".git").read_bytes() for path in paths}
        identities = {
            path: (
                git(source / path, "rev-parse", "HEAD"),
                git(source / path, "ls-files", "--stage", "-z"),
                git(source / path, "show", "HEAD:payload"),
            )
            for path in paths
        }
        result, calls = self.invoke(source=source)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        argv = calls[1]
        mounts = [
            dict(field.split("=", 1) for field in argv[i + 1].split(","))
            for i, arg in enumerate(argv)
            if arg == "--mount"
        ]
        destinations = {mount["destination"]: mount for mount in mounts}
        for path, (directory, common) in metadata.items():
            raw = gitfiles[path].decode().removeprefix("gitdir: ").strip()
            guest_dir = os.path.normpath(os.path.join("/src", path, raw))
            guest_common = os.path.normpath(
                os.path.join(guest_dir, (directory / "commondir").read_text().strip())
            )
            self.assertIn(
                guest_common,
                destinations,
                "nested commondir must resolve outside the root metadata mount",
            )
            for actual, destination in ((common, guest_common), (directory, guest_dir)):
                self.assertEqual(destinations[destination]["source"], str(actual))
                self.assertEqual(destinations[destination]["ro"], "true")
            for actual, destination in (
                (common / "config", guest_common + "/config"),
                (directory / "config.worktree", guest_dir + "/config.worktree"),
            ):
                overlay = destinations[destination]
                self.assertEqual(overlay["ro"], "true")
                copied = Path(overlay["source"])
                self.assertNotEqual(copied, actual)
                self.assertTrue(copied.is_relative_to(self.root / "output"))
                self.assertEqual(
                    git(
                        source,
                        "config",
                        "--file",
                        str(copied),
                        "--get",
                        "core.worktree",
                    )
                    .decode()
                    .strip(),
                    "/src/" + path,
                )

                def non_worktree(config):
                    values = git(
                        source, "config", "--file", str(config), "--null", "--list"
                    ).split(b"\0")
                    return [
                        value
                        for value in values
                        if not value.startswith(b"core.worktree\n")
                    ]

                self.assertEqual(non_worktree(copied), non_worktree(actual))
            self.assertEqual((source / path / ".git").read_bytes(), gitfiles[path])
            self.assertEqual(
                (
                    git(source / path, "rev-parse", "HEAD"),
                    git(source / path, "ls-files", "--stage", "-z"),
                    git(source / path, "show", "HEAD:payload"),
                ),
                identities[path],
            )
        for filename, data in before.items():
            self.assertEqual(Path(filename).read_bytes(), data, filename)
        self.assertFalse(
            any(
                value.startswith(("GIT_DIR=", "GIT_WORK_TREE=", "GIT_CONFIG_COUNT="))
                for value in argv
            )
        )

    def test_relocates_gitfile_roots_and_common_metadata_without_global_git_overrides(
        self,
    ):
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = repository_neutral_env()
        git_env.update(
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_OPTIONAL_LOCKS="0",
        )

        def git(root, *args):
            result = subprocess.run(
                [
                    git_bin,
                    "-c",
                    "protocol.file.allow=always",
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-C",
                    str(root),
                    *args,
                ],
                env=git_env,
                capture_output=True,
                timeout=10,
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

        leaf = seed("root-leaf-seed")
        product = seed("root-product-seed")
        git(product, "submodule", "add", "-q", str(leaf), "nested module")
        git(product, "commit", "-qam", "nested product fixture")
        parent = seed("root-parent-seed")
        git(parent, "submodule", "add", "-q", str(product), "hermit")
        git(parent, "commit", "-qam", "product submodule fixture")

        for topology in ("parent-submodule", "absolute-worktree", "relative-worktree"):
            with self.subTest(topology=topology):
                checkout = self.root / topology
                if topology == "parent-submodule":
                    # The failing production topology: Hermit itself is a submodule
                    # of a parent's linked worktree, with a relative root gitfile.
                    git(parent, "worktree", "add", "--detach", str(checkout))
                    git(checkout, "submodule", "update", "--init", "--recursive")
                    source = checkout / "hermit"
                else:
                    git(product, "worktree", "add", "--detach", str(checkout))
                    source = checkout
                    git(source, "submodule", "update", "--init", "--recursive")
                    if topology == "relative-worktree":
                        directory = (
                            git(source, "rev-parse", "--absolute-git-dir")
                            .decode()
                            .strip()
                        )
                        (source / ".git").write_text(
                            "gitdir: " + os.path.relpath(directory, source) + "\n"
                        )

                directory = Path(
                    git(source, "rev-parse", "--absolute-git-dir").decode().strip()
                )
                common = Path(
                    git(
                        source,
                        "rev-parse",
                        "--path-format=absolute",
                        "--git-common-dir",
                    )
                    .decode()
                    .strip()
                )
                git(source, "config", "extensions.worktreeConfig", "true")
                git(source, "config", "--worktree", "core.worktree", str(source))
                git(
                    source,
                    "config",
                    "--worktree",
                    "fixture.value",
                    "keep root-only value",
                )
                nested = source / "nested module"
                nested_dir = Path(
                    git(nested, "rev-parse", "--absolute-git-dir").decode().strip()
                )
                metadata_dirs = set((directory, common, nested_dir))
                before = {
                    str(f): f.read_bytes()
                    for d in metadata_dirs
                    for name in (
                        "config",
                        "config.worktree",
                        "HEAD",
                        "index",
                        "commondir",
                    )
                    if (f := d / name).is_file()
                }
                identities = {
                    str(repo): (
                        git(repo, "rev-parse", "HEAD"),
                        git(repo, "ls-files", "--stage", "-z"),
                        git(repo, "show", "HEAD:payload"),
                    )
                    for repo in (source, nested)
                }
                raw = (source / ".git").read_text().removeprefix("gitdir: ").strip()
                guest_dir = os.path.normpath(os.path.join("/src", raw))
                self.assertEqual(os.path.isabs(raw), topology == "absolute-worktree")
                if topology != "absolute-worktree":
                    self.assertNotEqual(guest_dir, str(directory))
                common_raw = (
                    (directory / "commondir").read_text().strip()
                    if (directory / "commondir").is_file()
                    else "."
                )
                guest_common = os.path.normpath(os.path.join(guest_dir, common_raw))
                self.assertEqual(directory == common, topology == "parent-submodule")
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(source=source)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(calls), 2)
                argv = calls[1]
                mounts = [
                    dict(field.split("=", 1) for field in argv[i + 1].split(","))
                    for i, arg in enumerate(argv)
                    if arg == "--mount"
                ]
                destinations = {m["destination"]: m for m in mounts}
                self.assertIn(
                    guest_dir,
                    destinations,
                    "root gitfile must resolve to the actual root metadata mount",
                )
                self.assertEqual(destinations[guest_dir]["source"], str(directory))
                self.assertEqual(destinations[guest_dir]["ro"], "true")
                self.assertIn(
                    guest_common,
                    destinations,
                    "relative commondir must resolve to the actual common metadata",
                )
                self.assertEqual(destinations[guest_common]["source"], str(common))
                self.assertEqual(destinations[guest_common]["ro"], "true")
                # The linked topologies share their common config with the
                # repository they were added from, which sets no core.worktree:
                # nothing may be bound over it, because a lock-and-rename write
                # to it would detach the bind mid-run
                # (https://github.com/rrnewton/hermit/issues/3920). Only the
                # submodule root, a repository of its own, needs its copy.
                overlays = [(directory / "config.worktree", guest_dir + "/config.worktree")]
                if topology == "parent-submodule":
                    overlays.append((common / "config", guest_common + "/config"))
                else:
                    self.assertNotIn(guest_common + "/config", destinations)
                    self.assertNotIn(str(common / "config"), destinations)
                for actual, destination in overlays:
                    overlay = destinations[destination]
                    self.assertEqual(overlay["ro"], "true")
                    copied = Path(overlay["source"])
                    self.assertNotEqual(copied, actual)
                    self.assertTrue(copied.is_relative_to(self.root / "output"))
                    self.assertEqual(
                        git(
                            source,
                            "config",
                            "--file",
                            str(copied),
                            "--get",
                            "core.worktree",
                        )
                        .decode()
                        .strip(),
                        "/src",
                    )

                    def non_worktree(config, source=source):
                        values = git(
                            source, "config", "--file", str(config), "--null", "--list"
                        ).split(b"\0")
                        return [
                            value
                            for value in values
                            if not value.startswith(b"core.worktree\n")
                        ]

                    self.assertEqual(non_worktree(copied), non_worktree(actual))
                nested_raw = (
                    (nested / ".git").read_text().removeprefix("gitdir: ").strip()
                )
                guest_nested = os.path.normpath(
                    os.path.join("/src/nested module", nested_raw)
                )
                self.assertEqual(destinations[guest_nested]["source"], str(nested_dir))
                nested_copy = destinations[guest_nested + "/config"]
                self.assertEqual(nested_copy["ro"], "true")
                self.assertEqual(
                    git(
                        source,
                        "config",
                        "--file",
                        nested_copy["source"],
                        "--get",
                        "core.worktree",
                    )
                    .decode()
                    .strip(),
                    "/src/nested module",
                )
                self.assertEqual(
                    argv[-8:],
                    [
                        "fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                        "/bin/env",
                        "--default-signal",
                        "/bin/taskset",
                        "--cpu-list",
                        ",".join(map(str, sorted(os.sched_getaffinity(0)))),
                        "/not-executed/command",
                        "literal argument",
                    ],
                )
                self.assertFalse(
                    any(
                        value.startswith(
                            ("GIT_DIR=", "GIT_WORK_TREE=", "GIT_CONFIG_COUNT=")
                        )
                        for value in argv
                    ),
                    "root Git overrides must not leak to nested Git",
                )
                for filename, data in before.items():
                    self.assertEqual(Path(filename).read_bytes(), data, filename)
                for repo in (source, nested):
                    self.assertEqual(
                        (
                            git(repo, "rev-parse", "HEAD"),
                            git(repo, "ls-files", "--stage", "-z"),
                            git(repo, "show", "HEAD:payload"),
                        ),
                        identities[str(repo)],
                    )

    def test_a_shared_config_rewrite_renames_over_no_mount_destination(self):
        """A Git config write in the shared checkout replaces no mounted file.

        Git writes config by lock-and-rename, and a rename over a bind-mount
        destination detaches the bind in every mount namespace, mid-run
        (https://github.com/rrnewton/hermit/issues/3920). This performs that
        write while the wrapper's mounts would be live: it maps every file
        mount destination back to the host file it covers, rewrites the shared
        common config the way any agent's `git config` does, and requires
        every covered file to keep its identity. A common config that sets
        core.worktree for every worktree, directly or through include.path,
        still gets its private copy; a core.worktree that the extension does
        not apply does not.
        """
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = repository_neutral_env()
        git_env.update(
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_OPTIONAL_LOCKS="0",
        )

        def git(root, *args):
            result = subprocess.run(
                [git_bin, "-c", "protocol.file.allow=always", "-c", "user.name=fixture",
                 "-c", "user.email=fixture@example.invalid", "-C", str(root), *args],
                env=git_env, capture_output=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            return result.stdout.decode().strip()

        leaf = self.root / "rewrite-leaf"
        leaf.mkdir()
        git(leaf, "init", "-q")
        (leaf / "payload").write_text("leaf\n")
        git(leaf, "add", "payload")
        git(leaf, "commit", "-qm", "leaf")
        shared = self.root / "rewrite-shared"
        shared.mkdir()
        git(shared, "init", "-q")
        (shared / "payload").write_text("shared\n")
        git(shared, "add", "payload")
        git(shared, "commit", "-qm", "shared")
        git(shared, "submodule", "add", "-q", str(leaf), "nested")
        git(shared, "commit", "-qam", "nested")
        # The production shared checkout: the worktree extension on, and no
        # core.worktree in its common config.
        git(shared, "config", "extensions.worktreeConfig", "true")

        # Each case: how the shared config is set for the run, and whether its
        # common config must still get a private copy.
        included = self.root / "rewrite-included.config"
        cases = {
            # Production: the extension on, no core.worktree.
            "no-common-worktree": ({}, False),
            # With the extension on, a common core.worktree applies to every
            # worktree. It points at the checkout so the wrapper's own
            # submodule walk still finds its working tree.
            "common-worktree": ({"core.worktree": "checkout"}, True),
            # Read as Git reads it, through include.path.
            "included-worktree": ({"include.path": str(included)}, True),
            # With the extension off, Git applies a common core.worktree to
            # the main worktree alone.
            "extension-off": (
                {"core.worktree": "checkout", "extensions.worktreeConfig": "false"},
                False,
            ),
        }
        for case, (settings, copied) in cases.items():
            with self.subTest(case=case):
                checkout = self.root / f"rewrite-{case}"
                git(shared, "worktree", "add", "--detach", str(checkout))
                git(checkout, "submodule", "update", "--init", "--recursive")
                common = Path(git(checkout, "rev-parse", "--path-format=absolute",
                                  "--git-common-dir"))
                included.write_text(f"[core]\n\tworktree = {checkout}\n")
                for key, value in settings.items():
                    value = str(checkout) if value == "checkout" else value
                    git(shared, "config", key, value)
                self.capture.unlink(missing_ok=True)
                result, calls = self.invoke(source=checkout)
                for key in settings:
                    git(shared, "config", "--unset", key)
                git(shared, "config", "extensions.worktreeConfig", "true")
                self.assertEqual(result.returncode, 0, result.stderr)
                argv = calls[1]
                mounts = [
                    dict(field.split("=", 1) for field in argv[i + 1].split(","))
                    for i, arg in enumerate(argv)
                    if arg == "--mount"
                ]
                directories = sorted(
                    ((m["destination"], m["source"]) for m in mounts
                     if Path(m["source"]).is_dir()),
                    key=lambda pair: len(pair[0]),
                    reverse=True,
                )

                def covered(destination):
                    # The host file a file-mount destination sits on: the
                    # destination as seen through the deepest directory mount
                    # that contains it, or the host path itself.
                    for guest, host in directories:
                        if destination.startswith(guest + "/"):
                            return Path(host) / destination[len(guest) + 1:]
                    return Path(destination)

                files = {
                    m["destination"]: covered(m["destination"])
                    for m in mounts
                    if Path(m["source"]).is_file() and covered(m["destination"]).exists()
                }
                bound_common = [d for d, f in files.items() if f == common / "config"]
                if copied:
                    self.assertEqual(len(bound_common), 1, files)
                    continue
                self.assertEqual(bound_common, [], "the shared common config is bound over")
                before = {d: f.stat().st_ino for d, f in files.items()}
                common_before = (common / "config").stat().st_ino
                git(shared, "config", "fixture.rewritten", "yes")
                self.assertNotEqual((common / "config").stat().st_ino, common_before,
                                    "git config no longer renames; the reproducer is void")
                after = {d: f.stat().st_ino for d, f in files.items()}
                self.assertEqual(after, before, "a mount destination was renamed over")

    def test_inherited_git_location_variables_leave_the_mounts_unchanged(self):
        # https://github.com/rrnewton/hermit/issues/3362: a `git rebase --exec`
        # step or a hook runs this wrapper with GIT_DIR, GIT_WORK_TREE or
        # GIT_INDEX_FILE naming some other repository. The wrapper names --src
        # and each submodule explicitly, so every case must produce the podman
        # invocation and private config copies of a run without them, and must
        # leave the named repository untouched.
        git_bin = shutil.which("git")
        self.assertIsNotNone(git_bin)
        git_env = repository_neutral_env()
        git_env.update(
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_OPTIONAL_LOCKS="0",
        )

        def git(root, *args, location=None):
            result = subprocess.run(
                [
                    git_bin,
                    "-c",
                    "protocol.file.allow=always",
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-C",
                    str(root),
                    *args,
                ],
                env={**git_env, **(location or {})},
                capture_output=True,
                timeout=10,
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

        # A linked-worktree root with a nested submodule reaches every git
        # lookup the wrapper makes: the root gitfile and common directory, the
        # recursive submodule walk, and each submodule's own metadata.
        leaf = seed("location-leaf")
        child = seed("location-child")
        git(child, "submodule", "add", "-q", str(leaf), "nested leaf")
        git(child, "commit", "-qam", "nested fixture")
        product = seed("location-product")
        git(product, "submodule", "add", "-q", str(child), "third-party/child")
        git(product, "commit", "-qam", "product fixture")
        source = self.root / "location-source"
        git(product, "worktree", "add", "-q", "--detach", str(source))
        git(source, "submodule", "update", "--init", "--recursive")
        # A worktree-specific core.worktree gives the root a private config
        # copy to relocate. A linked root's common config is no longer copied
        # unless it sets core.worktree for every worktree
        # (https://github.com/rrnewton/hermit/issues/3920), and the root copy
        # is what an inherited GIT_DIR once redirected.
        git(source, "config", "extensions.worktreeConfig", "true")
        git(source, "config", "--worktree", "core.worktree", str(source))
        metadata = [
            Path(
                git(repo, "rev-parse", "--path-format=absolute", option)
                .decode()
                .strip()
            )
            for repo in (
                source,
                source / "third-party/child",
                source / "third-party/child/nested leaf",
            )
            for option in ("--git-dir", "--git-common-dir")
        ]

        # The repository the variables name: a linked worktree, as in the
        # incident, whose gitdir is not called `.git`.
        other = seed("location-other")
        other_linked = self.root / "location-other-linked"
        git(other, "worktree", "add", "-q", "--detach", str(other_linked))
        other_git_dir = Path(
            git(other_linked, "rev-parse", "--absolute-git-dir").decode().strip()
        )
        values = {
            "GIT_DIR": other_git_dir,
            "GIT_WORK_TREE": other_linked,
            "GIT_INDEX_FILE": other_git_dir / "index",
        }
        controls = {
            "GIT_DIR": ("rev-parse", "--absolute-git-dir"),
            "GIT_WORK_TREE": ("rev-parse", "--show-toplevel"),
            "GIT_INDEX_FILE": ("rev-parse", "--git-path", "index"),
        }

        def other_state():
            files = sorted(
                path for path in (other / ".git").rglob("*") if path.is_file()
            )
            return {
                str(path): path.read_bytes() for path in [*files, other_linked / ".git"]
            }

        def podman_run(label, location):
            output = self.root / f"location-output-{label}"
            real_output = os.path.realpath(output)
            self.capture.unlink(missing_ok=True)
            result, calls = self.invoke(
                source=source, output=str(output), git_location=location
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(calls), 2)

            # Only the output directory and the mktemp suffixes may differ.
            def normalize(text):
                return re.sub(
                    r"/git-(root-)?configs\.[^/,]+",
                    r"/git-\1configs.*",
                    text.replace(real_output, "<output>"),
                )

            copies = {
                normalize(str(path)): path.read_bytes()
                for path in sorted(Path(real_output).glob("git-*configs.*/**/*"))
                if path.is_file()
            }
            return [normalize(arg) for arg in calls[1]], copies

        baseline_argv, baseline_copies = podman_run("none", {})
        mount_sources = [
            dict(field.split("=", 1) for field in baseline_argv[i + 1].split(","))[
                "source"
            ]
            for i, arg in enumerate(baseline_argv)
            if arg == "--mount"
        ]
        for directory in metadata:
            self.assertIn(
                str(directory),
                mount_sources,
                "the baseline must mount every metadata directory",
            )
        self.assertEqual(
            sum("/git-root-configs.*/" in path for path in mount_sources), 1
        )
        self.assertEqual(sum("/git-configs.*/" in path for path in mount_sources), 2)
        self.assertEqual(len(baseline_copies), 3)
        before = other_state()
        for names in (
            ("GIT_DIR",),
            ("GIT_WORK_TREE",),
            ("GIT_INDEX_FILE",),
            ("GIT_WORK_TREE", "GIT_INDEX_FILE"),
        ):
            with self.subTest(inherited=names):
                location = {name: str(values[name]) for name in names}
                for name in names:
                    # Control: the variable really redirects `git -C <src>`,
                    # so an equal result below is not vacuous.
                    steered = git(
                        source, *controls[name], location={name: location[name]}
                    )
                    self.assertEqual(
                        Path(steered.decode().strip()).resolve(),
                        values[name].resolve(),
                        f"control: {name} did not steer an unisolated git",
                    )
                argv, copies = podman_run("+".join(names), location)
                self.assertEqual(argv, baseline_argv)
                self.assertEqual(copies, baseline_copies)
                self.assertEqual(other_state(), before)


# The git repository-location variables the regression below exports into a
# child run. Listed separately from the helpers under test so that shrinking
# their list cannot also shrink this one.
INHERITED_LOCATION_VARIABLES = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
)


class InheritedGitLocation(unittest.TestCase):
    """https://github.com/rrnewton/hermit/issues/3362: the scratch-repository
    tests above must not act on a repository named by an inherited Git
    location variable.

    `git rebase --exec` exports GIT_DIR to the commands it runs, and hooks get
    GIT_DIR, GIT_WORK_TREE and GIT_INDEX_FILE. On hermit main 6be37a833d a
    `cargo test -p hermit-manifest-plan --lib` run under `rebase --exec` ran
    this script, whose fixtures then committed "fixture" commits adding
    `payload` onto a real worktree, and set `core.bare = true` in the shared
    hermit configuration.

    Each case re-runs the scratch-repository tests in a child process with one
    variable, or the GIT_WORK_TREE and GIT_INDEX_FILE pair that hooks get
    together, pointing into a throwaway repository. It then requires both that
    the child passed and that the throwaway is byte-identical afterwards. The
    variables go to the child only. The child's fixture git commands drop
    them, but `invoke` passes them on to the wrapper, so each case also runs
    the wrapper's own git lookups under them.
    """

    CHILD_TESTS = (
        "CargoCacheMounts.test_relocates_real_nested_submodule_configs_without_changing_host_metadata",
        "CargoCacheMounts.test_relocates_nested_linked_worktrees_with_external_common_metadata",
        "CargoCacheMounts.test_relocates_gitfile_roots_and_common_metadata_without_global_git_overrides",
    )

    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="hermit-git-location-throwaway-")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.git_bin = shutil.which("git")
        self.assertIsNotNone(self.git_bin)

    def bookkeeping_env(self):
        env = {
            k: v for k, v in os.environ.items() if k not in INHERITED_LOCATION_VARIABLES
        }
        env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        return env

    def bookkeeping_git(self, directory, *args, check=True):
        result = subprocess.run(
            [
                self.git_bin,
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-C",
                str(directory),
                *args,
            ],
            env=self.bookkeeping_env(),
            capture_output=True,
            timeout=10,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr.decode())
        return result

    def throwaway(self, name):
        """A repository with a linked worktree: GIT_DIR naming
        `.git/worktrees/<name>`, a directory not called `.git`, is what made
        `git init` record core.bare = true in the shared configuration."""
        main = self.root / name / "main"
        linked = self.root / name / "linked"
        main.mkdir(parents=True)
        self.bookkeeping_git(main, "init", "-q")
        (main / "tracked").write_text("throwaway\n")
        self.bookkeeping_git(main, "add", "tracked")
        self.bookkeeping_git(main, "commit", "-qm", "throwaway")
        self.bookkeeping_git(main, "worktree", "add", "-q", "--detach", str(linked))
        linked_git_dir = Path(
            self.bookkeeping_git(linked, "rev-parse", "--absolute-git-dir")
            .stdout.decode()
            .strip()
        )
        self.assertNotEqual(linked_git_dir.name, ".git")
        return main, linked, linked_git_dir

    def snapshot(self, main, linked, linked_git_dir):
        common = main / ".git"
        files = [
            common / "HEAD",
            common / "config",
            common / "index",
            common / "packed-refs",
            linked_git_dir / "HEAD",
            linked_git_dir / "index",
            linked_git_dir / "config.worktree",
            linked / ".git",
        ]
        files += sorted(path for path in (common / "refs").rglob("*") if path.is_file())
        state = {
            str(path): path.read_bytes() if path.exists() else b"<absent>"
            for path in files
        }
        bare = self.bookkeeping_git(main, "config", "--get", "core.bare", check=False)
        state["git config --get core.bare"] = (bare.returncode, bare.stdout)
        for directory in (main, linked):
            state[f"{directory}: git rev-parse HEAD"] = self.bookkeeping_git(
                directory, "rev-parse", "HEAD"
            ).stdout
        state["git for-each-ref"] = self.bookkeeping_git(main, "for-each-ref").stdout
        return state

    def test_scratch_repository_tests_ignore_inherited_git_location_variables(self):
        # An unrelated repository, used only to prove that each variable really
        # reaches git in the child's environment. Without it a misspelled
        # variable would make every case pass vacuously.
        probe = self.root / "probe"
        probe.mkdir()
        self.bookkeeping_git(probe, "init", "-q")
        controls = {
            "GIT_DIR": ("rev-parse", "--absolute-git-dir"),
            "GIT_WORK_TREE": ("rev-parse", "--show-toplevel"),
            "GIT_INDEX_FILE": ("rev-parse", "--git-path", "index"),
        }
        cases = []
        # Each variable alone, then the pair that hooks get together.
        for names in (
            ("GIT_DIR",),
            ("GIT_WORK_TREE",),
            ("GIT_INDEX_FILE",),
            ("GIT_WORK_TREE", "GIT_INDEX_FILE"),
        ):
            main, linked, linked_git_dir = self.throwaway("+".join(names))
            values = {
                "GIT_DIR": linked_git_dir,
                "GIT_WORK_TREE": linked,
                "GIT_INDEX_FILE": linked_git_dir / "index",
            }
            inherited = {name: values[name] for name in names}
            for name, value in inherited.items():
                live = subprocess.run(
                    [self.git_bin, "-C", str(probe), *controls[name]],
                    env={**self.bookkeeping_env(), name: str(value)},
                    capture_output=True,
                    timeout=10,
                )
                self.assertEqual(
                    Path(live.stdout.decode().strip()).resolve(),
                    value.resolve(),
                    f"control: {name} did not steer an unisolated git: {live.stderr!r}",
                )
            described = " ".join(f"{name}={value}" for name, value in inherited.items())
            before = self.snapshot(main, linked, linked_git_dir)
            # The cases run concurrently: each has its own throwaway.
            child = subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), *self.CHILD_TESTS],
                env={
                    **self.bookkeeping_env(),
                    **{name: str(value) for name, value in inherited.items()},
                },
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            self.addCleanup(lambda child=child: child.poll() is None and child.kill())
            cases.append((described, (main, linked, linked_git_dir), before, child))
        # Every case is judged before anything is asserted, so a failure names
        # each variable that still leaks rather than only the first.
        failures = []
        for described, repository, before, child in cases:
            stdout, stderr = child.communicate(timeout=300)
            after = self.snapshot(*repository)

            def show(data):
                if isinstance(data, bytes) and b"\0" in data:
                    return (
                        f"<{len(data)} bytes sha256 {hashlib.sha256(data).hexdigest()}>"
                    )
                return repr(data)

            changed = [
                f"  {key}: {show(before.get(key))} -> {show(after.get(key))}"
                for key in sorted(set(before) | set(after))
                if before.get(key) != after.get(key)
            ]
            if changed:
                failures.append(
                    f"with {described} the scratch-repository tests wrote "
                    "into the named repository:\n" + "\n".join(changed)
                )
            report = stderr.decode()
            if not (
                child.returncode == 0
                and f"Ran {len(self.CHILD_TESTS)} tests" in report
                and report.rstrip().endswith("OK")
            ):
                failures.append(
                    f"with {described} the scratch-repository tests did not all "
                    f"run and pass:\n{stdout.decode()}{report}"
                )
        if failures:
            self.fail("\n\n".join(failures))


class PinnedGuestPathContract(unittest.TestCase):
    def test_declared_guest_paths_cover_existing_liteinst_and_portable_consumers(self):
        here = Path(__file__).resolve().parent
        paths = (here / "guest-paths.txt").read_text().splitlines()
        self.assertEqual(len(paths), len(set(paths)), "duplicate guest paths")
        self.assertTrue(paths)
        for path in paths:
            # The manifest is the exact allowlist (see
            # assert-build-dependencies.sh): literal paths are admitted as
            # listed, including /usr/bin/[ and /usr/sbin/*; only canonical
            # absolute shape is required here.
            self.assertTrue(path.startswith("/usr/"), path)
            self.assertNotRegex(path, r"\s")
            self.assertNotIn("//", path)
            self.assertNotIn("/./", path)
            self.assertNotIn("/../", path)
            self.assertFalse(path.endswith("/.") or path.endswith("/.."), path)
        # The in-guest LiteInst program tests, run by test.cli in the pinned
        # root (formerly hermit-cli/tests/liteinst_advanced.rs).
        source = (
            here.parents[1] / "hermit-cli/tests/common/liteinst_in_guest_programs.rs"
        ).read_text()
        literals = set(re.findall(r'"(/usr/bin/[A-Za-z0-9_-]+)"', source))
        self.assertTrue(literals, "the source population must not disappear")
        self.assertEqual(
            literals - set(paths), set(), "missing existing LiteInst guest"
        )
        portable = {
            "/usr/bin/" + command
            for command in "bash date df du find git node nodejs nproc python3 sort stat tr".split()
        }
        self.assertEqual(portable - set(paths), set(), "lost existing portable guest")
        self.assertIn(
            "/usr/bin/printf", paths, "DBT ptrace argument-forwarding reference"
        )
        # test.record_replay's curl_recording_replays_offline_with_identical_logs
        # finds curl on PATH (https://github.com/rrnewton/hermit/issues/3786).
        self.assertIn("/usr/bin/curl", paths, "record_replay curl guest")


class SeededOverlayLive(unittest.TestCase):
    """The seeded default must never write the caller's cargo home.

    Builds a scratch HOME whose .cargo holds a registry crate that is
    cached but not extracted and a git dependency that is in the db but
    not checked out, runs the real wrapper on its default (seeded) path,
    and requires the scratch .cargo to be byte-identical afterwards:
    extractions and checkouts may only land in the container-private
    overlay upper layer. Skips by name where podman, the pinned image,
    or the fixture tools are unavailable.
    """

    def cargo_manifest(self, cargo_home):
        lines = []
        for path in sorted(p for p in cargo_home.rglob("*") if p.is_file()):
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            lines.append(f"{path.relative_to(cargo_home)} {path.stat().st_size} {digest}")
        return hashlib.sha256("\n".join(lines).encode()).hexdigest(), lines

    def run_fixture(self, args, cwd, env=None):
        return subprocess.run(
            args, cwd=cwd, env=env, capture_output=True, text=True, timeout=300
        )

    def test_seeded_default_build_leaves_host_cargo_home_byte_unchanged(self):
        if Path("/run/.containerenv").exists():
            self.skipTest("inside a container; nested podman is not used")
        if shutil.which("podman") is None:
            self.skipTest("podman not on PATH")
        image = WRAPPER.with_name("image.digest").read_text().strip()
        exists = subprocess.run(
            ["podman", "image", "exists", image],
            capture_output=True, text=True, timeout=120,
        )
        if exists.returncode != 0:
            self.skipTest(f"pinned image {image} not in the local podman store")
        # Past this point the case must run: missing tools are failures,
        # not skips a host could quietly sit on.
        for tool in ("cargo", "git"):
            if shutil.which(tool) is None:
                self.fail(
                    f"{tool} not on PATH although podman and the pinned image "
                    "exist; the seeded-overlay case must run here"
                )
        scratch = tempfile.TemporaryDirectory(prefix="seeded-overlay-live-")
        self.addCleanup(scratch.cleanup)
        root = Path(scratch.name)
        gitlib = root / "gitlib"
        (gitlib / "src").mkdir(parents=True)
        (gitlib / "Cargo.toml").write_text(
            '[package]\nname = "gitprobe-lib"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n'
        )
        (gitlib / "src/lib.rs").write_text("pub fn answer() -> i32 {\n    42\n}\n")
        neutral = repository_neutral_env()
        for args in (
            ["git", "init", "-b", "main", "-q"],
            ["git", "add", "Cargo.toml", "src/lib.rs"],
            ["git", "-c", "user.name=seeded-overlay-live",
             "-c", "user.email=seeded-overlay-live@invalid",
             "commit", "-qm", "gitprobe-lib"],
        ):
            done = self.run_fixture(args, gitlib, env=neutral)
            self.assertEqual(done.returncode, 0, done.stderr)
        src = root / "consumer"
        for directory in (
            "src", "agent-utils/rs/target",
            "agent-utils/rs/.agent-utils-locks",
            "agent-utils/rs/.agent-utils-snapshots",
        ):
            (src / directory).mkdir(parents=True, exist_ok=True)
        (src / "Cargo.toml").write_text(
            '[package]\nname = "seeded-overlay-live"\nversion = "0.1.0"\n'
            'edition = "2021"\n\n[dependencies]\nseededreg = "=0.1.0"\n'
            f'gitprobe-lib = {{ git = "file://{gitlib}" }}\n\n[workspace]\n'
        )
        (src / "src/main.rs").write_text(
            "fn main() {\n"
            '    println!("seeded-live reg={} answer={}", '
            "seededreg::val(), gitprobe_lib::answer());\n}\n"
        )
        home = root / "home"
        cargo_home = home / ".cargo"
        lock_env = {**os.environ, "CARGO_HOME": str(cargo_home)}
        # Prime the git db through a git-only crate: cargo --offline will
        # not clone a git dependency, so the db must exist before the real
        # lock step. The priming clone is a local file:// fetch, and the
        # checkout it makes is deleted again with the fixture clean-up.
        prime = root / "prime"
        (prime / "src").mkdir(parents=True)
        (prime / "Cargo.toml").write_text(
            '[package]\nname = "prime"\nversion = "0.1.0"\nedition = "2021"\n\n'
            f'[dependencies]\ngitprobe-lib = {{ git = "file://{gitlib}" }}\n\n[workspace]\n'
        )
        (prime / "src/main.rs").write_text(
            'fn main() { println!("{}", gitprobe_lib::answer()); }\n'
        )
        primed = self.run_fixture(
            ["cargo", "generate-lockfile"], prime, env=lock_env
        )
        if primed.returncode != 0:
            self.fail(
                "cannot prime the scratch git db "
                f"({primed.stderr.strip()[-200:]})"
            )
        # Self-contained registry fixture: package a tiny local crate and
        # write its .crate archive plus a synthesized sparse-index cache
        # entry (with the crate's sha256 as its checksum) into the scratch
        # registry. Nothing is read from the caller's real cargo home, so
        # the case runs identically on any host with podman and the image.
        regcrate = root / "regcrate"
        (regcrate / "src").mkdir(parents=True)
        (regcrate / "Cargo.toml").write_text(
            '[package]\nname = "seededreg"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\npath = "src/lib.rs"\n'
        )
        (regcrate / "src/lib.rs").write_text("pub fn val() -> i32 {\n    7\n}\n")
        packaged = self.run_fixture(
            ["cargo", "package", "--offline"], regcrate,
            env={**os.environ, "CARGO_HOME": str(root / "pkg-home/.cargo")},
        )
        if packaged.returncode != 0:
            self.fail(
                "cannot package the fixture registry crate "
                f"({packaged.stderr.strip()[-200:]})"
            )
        crate_path = regcrate / "target/package/seededreg-0.1.0.crate"
        if not crate_path.exists():
            self.fail(f"packaged fixture crate missing at {crate_path}")
        # The index directory name carries a cargo-computed hash; cargo's
        # own debug output names it, version-independent, on the offline
        # attempt that is expected to fail before the entry exists.
        probe = subprocess.run(
            ["cargo", "generate-lockfile", "--offline"], cwd=src,
            env={**lock_env, "CARGO_LOG": "debug"},
            capture_output=True, text=True, timeout=300,
        )
        found = re.search(r"index\.crates\.io-[0-9a-z]+", probe.stdout + probe.stderr)
        if found is None:
            self.fail("cannot discover the sparse index directory name from cargo debug output")
        index_id = found.group(0)
        cache_dst = cargo_home / "registry/cache" / index_id
        cache_dst.mkdir(parents=True)
        shutil.copyfile(crate_path, cache_dst / crate_path.name)
        index_root = cargo_home / "registry/index" / index_id
        (index_root / ".cache/se/ed").mkdir(parents=True)
        (index_root / "config.json").write_text(
            '{\n  "dl": "https://static.crates.io/crates",\n  "api": "https://crates.io"\n}\n'
        )
        meta = {
            "name": "seededreg", "vers": "0.1.0", "deps": [],
            "cksum": hashlib.sha256(crate_path.read_bytes()).hexdigest(),
            "features": {}, "yanked": False,
        }
        (index_root / ".cache/se/ed/seededreg").write_bytes(
            b"\x03" + (2).to_bytes(4, "little")
            + b'etag: "seeded-overlay-live"\x00' + b"0.1.0\x00"
            + json.dumps(meta, separators=(",", ":")).encode() + b"\x00"
        )
        locked = self.run_fixture(
            ["cargo", "generate-lockfile", "--offline"], src, env=lock_env
        )
        if locked.returncode != 0:
            self.fail(
                "cannot lock against the synthesized scratch registry "
                f"({locked.stderr.strip()[-200:]})"
            )
        for extracted in cargo_home.glob("registry/src/*/seededreg-0.1.0"):
            shutil.rmtree(extracted)
        for checkout in cargo_home.glob("git/checkouts/*"):
            shutil.rmtree(checkout)
        self.assertTrue(list(cargo_home.glob("git/db/*")), "git db populated")
        self.assertEqual(list(cargo_home.glob("registry/src/*/seededreg-0.1.0")), [])
        self.assertEqual(list(cargo_home.glob("git/checkouts/*")), [])
        before_hash, before = self.cargo_manifest(cargo_home)
        env = {**os.environ, "HOME": str(home)}
        # Swapping HOME would also move rootless podman's default store and
        # config (both derive from HOME when XDG is unset), making the
        # pinned image "absent" on a default-layout host. Pin both XDG
        # roots to the caller's real ones so podman looks exactly where it
        # did before the swap.
        real_home = Path.home()
        # An empty XDG value is unset for podman's path derivation too.
        env["XDG_DATA_HOME"] = (
            os.environ.get("XDG_DATA_HOME") or str(real_home / ".local/share")
        )
        env["XDG_CONFIG_HOME"] = (
            os.environ.get("XDG_CONFIG_HOME") or str(real_home / ".config")
        )
        result = subprocess.run(
            [
                "bash", str(WRAPPER),
                "--src", str(src), "--out", str(root / "out"), "--src-rw",
                "--", "bash", "-c",
                "cd /src && cargo build --locked --offline && "
                "./target/debug/seeded-overlay-live",
            ],
            env=env, capture_output=True, text=True, timeout=900,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("seeded-live reg=7 answer=42", result.stdout)
        self.assertIn("seeding cargo dependency caches", result.stderr)
        after_hash, after = self.cargo_manifest(cargo_home)
        changed = [line for line in before if line not in after][:5]
        added = [line for line in after if line not in before][:5]
        self.assertEqual(
            before_hash, after_hash,
            f"seeded run changed the host cargo home: changed={changed} added={added}",
        )
        self.assertEqual(list(cargo_home.glob("registry/src/*/seededreg-0.1.0")), [])
        self.assertEqual(list(cargo_home.glob("git/checkouts/*")), [])


if __name__ == "__main__":
    unittest.main()
