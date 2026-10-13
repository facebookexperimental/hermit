#!/usr/bin/env python3
"""Scenario tests for ci/buck-e2e/cell.sh and the container choice in ci/buck-e2e/defs.bzl.

CellTest runs cell.sh against a scratch bundle whose test-harness and
ci/hermetic/run-in-pinned-root.sh are stand-ins that record how they were called and
write the outputs a harness would. It checks that a cell marked
HERMIT_E2E_CONTAINER=pinned-root runs the harness only through the wrapper, with the
bundle at /src/bundle, outputs under the /results mount and a /test workdir, and that a
missing image, a non-local route or an unknown container value are reported as an
ERROR rather than run on the host. In the container, the private bundle copy's DBT
client directory gets a link to each library the image's loader (a stand-in ldd)
resolves outside the bundle, never replacing one the bundle ships, and a library the
image lacks is an ERROR. The wrapper and the harness each get a deadline that leaves
the next one out time to stop them. A cell the harness withholds as HOST-INAPPLICABLE
is reported skipped, with the harness's reason, on a local route only; on RE, or with any
other run failure, it stays failed. Nothing here needs podman, /test or capabilities.

A diverging verify cell's artifacts also go through ci/buck-e2e/ingest.py, as Tpx
would list them, to check that both run logs come back to this host from a cell that
ran remotely (fetched with testx) and from one that ran locally (read from buck-out).

ContainerChoiceTest evaluates defs.bzl's hermit_e2e_cells over the real
ci/expected-e2e-plan.json with stand-ins for the Buck builtins and checks which cells
get the pinned-root container in hybrid and local routing. ParityRouteTest and
InGuestRouteTest evaluate the same plan for each candidate backend's route: every
liteinst and in-guest-trap cell runs locally, and each parity candidate shares its
ptrace reference's route.
"""

from __future__ import annotations

import fcntl
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

import test_buck_e2e_ingest as ingest_test


CI = Path(__file__).resolve().parent
CELL_SH = CI / "buck-e2e" / "cell.sh"
DEFS = CI / "buck-e2e" / "defs.bzl"
PLAN = CI / "expected-e2e-plan.json"
RE_EXCLUSIONS = CI / "buck-e2e" / "re_exclusions.json"
TEST, MODE, BACKEND = "c-programs/cpuid-probe", "verify", "dbt"
CELL = f"{TEST}/{MODE}@{BACKEND}"
SLUG = "c-programs-cpuid-probe-verify-dbt"

# test-harness, for `run ... --results F --junit F --tpx-json F`: records its argv and
# the environment cell.sh gives it, then writes one PASS row, its test_done and the
# verify evidence a passing verify cell must return (see FAKE_VERDICT); FAKE_OUTCOME=FAIL writes a FAIL row and
# exits 1, as the harness does. HARNESS_ROOT_MAP maps a container
# path prefix to the host directory behind it, the way the fake wrapper's mounts would.
FAKE_HARNESS = r"""#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
flag = lambda name: args[args.index(name) + 1]
mapping = json.loads(os.environ.get("HARNESS_ROOT_MAP") or "{}")
def host(path):
    for prefix, target in mapping.items():
        if path == prefix or path.startswith(prefix + "/"):
            return target + path[len(prefix):]
    return path
keys = ("HERMIT_E2E_EMPTY_WORKDIR", "E2E_RESULT_ROOT", "E2E_BUILD_ROOT", "VALIDATE_RUN_STATE",
        "E2E_RUN_ID", "HERMIT_BIN", "HERMIT_INSTALL_DIR", "E2E_KEEP_VERIFY_LOGS", "E2E_PARITY_POST_PASS",
        "HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN", "HERMIT_EPOCH")
# The client directory the DBT backend would load from: name -> link target, or None.
rsrcs = os.path.join(host(os.environ.get("HERMIT_INSTALL_DIR", "/nonexistent")), "rsrcs")
links = {n: os.readlink(os.path.join(rsrcs, n)) if os.path.islink(os.path.join(rsrcs, n)) else None
         for n in sorted(os.listdir(rsrcs))} if os.path.isdir(rsrcs) else None
# With FAKE_KVM_SLOT_DIR: which KVM slot files someone holds locked while the harness runs,
# and the files behind the descriptors the harness inherited.
slots = None
if os.environ.get("FAKE_KVM_SLOT_DIR"):
    import fcntl
    slots = {}
    for n in sorted(os.listdir(os.environ["FAKE_KVM_SLOT_DIR"])):
        with open(os.path.join(os.environ["FAKE_KVM_SLOT_DIR"], n), "a") as f:
            try:
                fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
                slots[n] = "free"
            except BlockingIOError:
                slots[n] = "locked"
fd_targets = []
for fd in os.listdir("/proc/self/fd"):
    try:
        fd_targets.append(os.readlink("/proc/self/fd/" + fd))
    except OSError:
        pass
with open(os.environ["FAKE_CALLS"], "a") as calls:
    calls.write(json.dumps({"who": "harness", "argv": args, "env": {k: os.environ.get(k) for k in keys},
                            "rsrcs": links, "slots": slots, "fd_targets": fd_targets}) + "\n")
# Like hermit, which writes its private verify summary into its working directory (the
# repository root when no enclosing checkout ignores `ignored/`), and a write through an
# existing bundle file, which a hard-linked copy would carry back into the bundle.
repo = host(flag("--repo-root"))
with open(os.path.join(repo, ".hermit-verify-summary-fake"), "w") as f:
    f.write("summary\n")
if os.environ.get("HERMIT_BIN", "").startswith("/src/"):
    with open(host(os.environ["HERMIT_BIN"]), "a") as f:
        f.write("written by the cell\n")
outcome = os.environ.get("FAKE_OUTCOME", "PASS")
results, tpx = host(flag("--results")), host(flag("--tpx-json"))
os.makedirs(os.path.dirname(results), exist_ok=True)
test, mode, backend = flag("--test"), flag("--mode"), flag("--backend")
if outcome == "HOST-INAPPLICABLE":
    # Like the harness withholding its one cell: no row, the cell's test_done skipped, and
    # a failed run-verdict record because the run executed nothing. FAKE_RUN_FAILURES adds
    # further run failures; FAKE_RUN_FAILURES_ONLY replaces them all. FAKE_WITHHELD_TEST,
    # FAKE_WITHHELD_STATUS and FAKE_WITHHELD_OUTCOME change the withheld record.
    open(results, "w").close()
    with open(host(flag("--junit")), "w") as f:
        f.write("<testsuite/>\n")
    failures = ["every one of the 1 selected cell(s) was host-inapplicable; "
                "a run that executed no cell is not a pass"]
    failures += json.loads(os.environ.get("FAKE_RUN_FAILURES") or "[]")
    if os.environ.get("FAKE_RUN_FAILURES_ONLY"):
        failures = json.loads(os.environ["FAKE_RUN_FAILURES_ONLY"])
    with open(tpx, "w") as f:
        f.write(json.dumps({"op": "test_done",
                            "test": os.environ.get("FAKE_WITHHELD_TEST") or test + "/" + mode + "@" + backend,
                            "status": os.environ.get("FAKE_WITHHELD_STATUS", "skipped"),
                            "details": json.dumps({"outcome": os.environ.get("FAKE_WITHHELD_OUTCOME", "HOST-INAPPLICABLE"),
                                                   "reason": "NOT RUN, NOT a pass, no coverage: this machine lacks exact-branch-counter"})}) + "\n")
        f.write(json.dumps({"op": "test_done", "test": "test-harness/run-verdict", "status": "failed",
                            "details": json.dumps({"run_failures": failures})}) + "\n")
        f.write(json.dumps({"op": "all_done"}) + "\n")
    sys.exit(1)
with open(results, "w") as f:
    f.write(json.dumps({"test": test, "mode": mode, "backend": backend, "outcome": outcome,
                        "run_id": os.environ["E2E_RUN_ID"]}) + "\n")
with open(host(flag("--junit")), "w") as f:
    f.write("<testsuite/>\n")
status = "passed" if outcome == "PASS" else "failed"
with open(tpx, "w") as f:
    f.write(json.dumps({"op": "test_done", "test": test + "/" + mode + "@" + backend, "status": status,
                        "details": json.dumps({"outcome": outcome})}) + "\n")
celldir = os.path.join(host(os.environ["E2E_RESULT_ROOT"]), "runs", os.environ["E2E_RUN_ID"],
                       os.environ["FAKE_SLUG"])
logs = os.path.join(celldir, "verify-logs", "verify-1")
os.makedirs(logs, exist_ok=True)
# Like hermit --keep-logs: a matched verify keeps only run 1's (golden) log, anything else
# keeps both. FAKE_VERDICT overrides the verdict, FAKE_DETLOGS ("1", "2", "12" or "")
# which logs survive, FAKE_VERIFY_JSON=missing|garbage the verdict file itself, and
# FAKE_DETLOG_LINES how many lines each log has (default 1).
verdict = os.environ.get("FAKE_VERDICT", "matched")
verify_json = os.environ.get("FAKE_VERIFY_JSON", "")
if verify_json != "missing":
    with open(os.path.join(celldir, "verify-1.json"), "w") as f:
        f.write("not json\n" if verify_json == "garbage" else json.dumps({"verdict": verdict}) + "\n")
for n in os.environ.get("FAKE_DETLOGS", "1" if verdict == "matched" else "12"):
    with open(os.path.join(logs, "run%s_log_detlog" % n), "w") as f:
        f.write("".join("run %s detlog line %d\n" % (n, i) for i in range(int(os.environ.get("FAKE_DETLOG_LINES", "1")))))
sys.exit(0 if outcome == "PASS" else 1)
"""

# ci/hermetic/run-in-pinned-root.sh. `--check-image` exits FAKE_IMAGE_RC. A run records
# its options, the --env values it was asked to forward, and the mountpoints present in
# --src, then runs the command after `--` with /src, /results and /validate-run-state
# mapped to --src, $E2E_RESULT_ROOT and $VALIDATE_RUN_STATE, the way podman would.
FAKE_WRAPPER = r"""#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
calls = os.environ["FAKE_CALLS"]
if args[:1] == ["--check-image"]:
    with open(calls, "a") as f:
        f.write(json.dumps({"who": "check-image", "argv": args}) + "\n")
    rc = int(os.environ.get("FAKE_IMAGE_RC", "0"))
    if rc:
        print("run-in-pinned-root: image localhost/x@sha256:00 is not present", file=sys.stderr)
    sys.exit(rc)
split = args.index("--")
opts, command = args[:split], args[split + 1:]
src = opts[opts.index("--src") + 1]
# Like run-in-pinned-root.sh, which skips a named variable that is unset.
forwarded = {n: os.environ[n] for n in (opts[i + 1] for i, o in enumerate(opts) if o == "--env")
             if n in os.environ}
mountpoints = sorted(p for p in ("target", "agent-utils/rs/target", "agent-utils/rs/.agent-utils-locks",
                                 "agent-utils/rs/.agent-utils-snapshots") if os.path.isdir(os.path.join(src, p)))
with open(calls, "a") as f:
    f.write(json.dumps({"who": "wrapper", "opts": opts, "command": command, "forwarded": forwarded,
                        "mountpoints": mountpoints}) + "\n")
mapping = {"/src": src, "/results": os.environ["E2E_RESULT_ROOT"],
           "/validate-run-state": os.environ["VALIDATE_RUN_STATE"]}
env = {k: v for k, v in os.environ.items() if k not in forwarded}
env.update({"E2E_RESULT_ROOT": "/results", "VALIDATE_RUN_STATE": "/validate-run-state",
            "HARNESS_ROOT_MAP": json.dumps(mapping)})
env.update({k: v for k, v in forwarded.items() if k not in ("E2E_RESULT_ROOT", "VALIDATE_RUN_STATE")})
# The command is `sh -c SCRIPT NAME /src/bundle/.../rsrcs env NAME=VALUE... timeout ...
# /src/bundle/bin/test-harness ...`: run it with the container paths among its arguments
# mapped to the host.
command = [mapping["/src"] + c[len("/src"):] if c.startswith("/src/bundle/") else c for c in command]
sys.exit(subprocess.run(command, env=env).returncode)
"""


# ldd, for the DBT client and libdetcore_dbt.so in RSRCS: what the image's loader prints
# for them, with RSRCS where /src/bundle/hermit/install/rsrcs is, plus FAKE_LDD_EXTRA
# (a line for the client) and an exit status of FAKE_LDD_RC. Given only the client, it
# prints only the client's block.
FAKE_LDD = r"""#!/bin/sh
d=$(dirname "$1")
printf '%s:\n' "$1"
printf '\tlinux-vdso.so.1 (0x00007f68c1e4e000)\n'
printf '\tlibdrx.so => %s/dynamorio/ext/lib64/release/libdrx.so (0x0000000077000000)\n' "$d"
printf '\tlibc.so.6 => /nix/store/g-glibc-2.42/lib/libc.so.6 (0x00007f68c1200000)\n'
printf '\t/nix/store/g-glibc-2.42/lib64/ld-linux-x86-64.so.2 (0x00007f68c1e50000)\n'
printf '\tlibgcc_s.so.1 => /nix/store/x-libgcc/lib/libgcc_s.so.1 (0x00007f68c1e27000)\n'
printf '\tlibshipped.so => /nix/store/s-shipped/lib/libshipped.so (0x00007f68c1e00000)\n'
[ -z "${FAKE_LDD_EXTRA:-}" ] || printf '\t%s\n' "$FAKE_LDD_EXTRA"
[ -n "${2:-}" ] || exit "${FAKE_LDD_RC:-0}"
printf '%s:\n' "$2"
printf '\tlibc.so.6 => /nix/store/g-glibc-2.42/lib/libc.so.6 (0x00007f68c1200000)\n'
printf '\tlibm.so.6 => /nix/store/g-glibc-2.42/lib/libm.so.6 (0x00007f68c1523000)\n'
printf '\tlibgcc_s.so.1 => /nix/store/x-libgcc/lib/libgcc_s.so.1 (0x00007f68c1e27000)\n'
exit "${FAKE_LDD_RC:-0}"
"""

# timeout: records its argv, then runs the real one.
FAKE_TIMEOUT = r"""#!/usr/bin/env python3
import json, os, sys
with open(os.environ["FAKE_CALLS"], "a") as f:
    f.write(json.dumps({"who": "timeout", "argv": sys.argv[1:]}) + "\n")
os.execv("/usr/bin/timeout", ["timeout"] + sys.argv[1:])
"""

# The links the container's image libraries get in the private copy's client directory.
IMAGE_LINKS = {
    "libc.so.6": "/nix/store/g-glibc-2.42/lib/libc.so.6",
    "libm.so.6": "/nix/store/g-glibc-2.42/lib/libm.so.6",
    "ld-linux-x86-64.so.2": "/nix/store/g-glibc-2.42/lib64/ld-linux-x86-64.so.2",
    "libgcc_s.so.1": "/nix/store/x-libgcc/lib/libgcc_s.so.1",
}


def write_exe(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class CellTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="buck-e2e-cell-test."))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.bundle = self.tmp / "bundle"
        write_exe(self.bundle / "bin" / "test-harness", FAKE_HARNESS)
        write_exe(self.bundle / "src" / "ci" / "hermetic" / "run-in-pinned-root.sh", FAKE_WRAPPER)
        (self.bundle / "build").mkdir()
        rsrcs = self.bundle / "hermit" / "install" / "rsrcs"
        rsrcs.mkdir(parents=True)
        for lib in ("libreverie_dbt_client.so", "libdetcore_dbt.so", "libshipped.so"):
            (rsrcs / lib).write_text(lib + "\n")
        write_exe(self.tmp / "image-bin" / "ldd", FAKE_LDD)
        write_exe(self.tmp / "image-bin" / "timeout", FAKE_TIMEOUT)
        (self.bundle / "hermit" / "hermit").write_text("hermit\n")
        (self.bundle / "run-state").mkdir()
        (self.bundle / "SOURCE_SHA").write_text("a" * 40 + "\n")
        self.before = sorted(str(p.relative_to(self.bundle)) for p in self.bundle.rglob("*"))
        self.calls = self.tmp / "calls.jsonl"
        self.calls.touch()

    def run_cell(self, backend: str = BACKEND, cell_test: str = TEST, cell_mode: str = MODE,
                 **env: str) -> tuple[dict, dict | None]:
        artifacts = self.tmp / "tpx" / "artifacts"
        annotations = self.tmp / "tpx" / "annotations"
        shutil.rmtree(self.tmp / "tpx", ignore_errors=True)
        base = {
            "PATH": f"{self.tmp / 'image-bin'}:{os.environ['PATH']}",
            "HOME": str(self.tmp),
            "TEST_RESULT_ARTIFACTS_DIR": str(artifacts),
            "TEST_RESULT_ARTIFACT_ANNOTATIONS_DIR": str(annotations),
            "HERMIT_E2E_BUNDLE": str(self.bundle),
            "HERMIT_E2E_ROUTE": "local",
            "FAKE_CALLS": str(self.calls),
            "FAKE_SLUG": "{}-{}-{}".format(cell_test.replace("/", "-"), cell_mode, backend),
        }
        base.update(env)
        proc = subprocess.run(["bash", str(CELL_SH), cell_test, cell_mode, backend], env=base,
                              capture_output=True, text=True, timeout=120)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = [json.loads(l) for l in proc.stdout.splitlines() if l.strip()]
        self.assertEqual(lines[-1], {"op": "all_done"}, proc.stdout)
        self.assertEqual(len(lines), 2, proc.stdout)
        result = artifacts / "result.json"
        return lines[0], (json.loads(result.read_text()) if result.exists() else None)

    def calls_by(self, who: str) -> list[dict]:
        return [c for c in map(json.loads, self.calls.read_text().splitlines()) if c["who"] == who]

    def assert_error(self, done: dict, *needles: str) -> None:
        self.assertEqual(done["status"], "failed")
        details = json.loads(done["details"])
        self.assertEqual(details["outcome"], "ERROR")
        for needle in needles:
            self.assertIn(needle, details["reason"])
        self.assertEqual(self.calls_by("harness"), [], "the harness must not run")

    def test_pinned_root_runs_the_harness_only_through_the_wrapper(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root",
                                     HERMIT_E2E_CONTAINER_REASON="privileged lane")
        self.assertEqual(done["status"], "passed", done)
        self.assertEqual(json.loads(done["details"])["outcome"], "PASS")
        self.assertEqual(len(self.calls_by("check-image")), 1)
        [wrapper] = self.calls_by("wrapper")
        opts, command = wrapper["opts"], wrapper["command"]
        self.assertIn("--src-rw", opts, "hermit writes its verify summary into /src/bundle/src")
        self.assertEqual(wrapper["mountpoints"], ["agent-utils/rs/.agent-utils-locks",
                                                  "agent-utils/rs/.agent-utils-snapshots",
                                                  "agent-utils/rs/target", "target"])
        self.assertEqual(wrapper["forwarded"]["HERMIT_E2E_EMPTY_WORKDIR"], "/test")
        for name in ("E2E_RESULT_ROOT", "VALIDATE_RUN_STATE", "E2E_RUN_ID"):
            self.assertTrue(wrapper["forwarded"].get(name), name)
        self.assertEqual(wrapper["forwarded"]["E2E_KEEP_VERIFY_LOGS"], "1")
        self.assertEqual(wrapper["forwarded"]["E2E_PARITY_POST_PASS"], "0")
        self.assertNotIn("E2E_BUILD_ROOT", wrapper["forwarded"],
                         "the wrapper would replace E2E_BUILD_ROOT with /src/target/e2e-build")
        self.assertNotIn("HERMIT_EPOCH", wrapper["forwarded"], "an unset HERMIT_EPOCH is not forwarded")
        self.assertEqual(command[:2], ["sh", "-c"])
        self.assertEqual(command[3:6], ["link-dbt-runtime", "/src/bundle/hermit/install/rsrcs", "env"])
        for assignment in ("E2E_BUILD_ROOT=/src/bundle/build", "HERMIT_BIN=/src/bundle/hermit/hermit",
                           "HERMIT_INSTALL_DIR=/src/bundle/hermit/install"):
            self.assertIn(assignment, command)
        self.assertIn("/src/bundle/bin/test-harness", command)
        flag = lambda name: command[command.index(name) + 1]
        self.assertEqual(flag("--repo-root"), "/src/bundle/src")
        self.assertEqual(flag("--results"), "/results/buck-cell-out/results.jsonl")
        self.assertEqual(flag("--tpx-json"), "/results/buck-cell-out/tpx.jsonl")
        self.assertEqual((flag("--test"), flag("--mode"), flag("--backend")), (TEST, MODE, BACKEND))
        self.assertTrue(all(not c.startswith(str(self.tmp)) for c in command),
                        "the in-container command must not name host paths")
        [harness] = self.calls_by("harness")
        self.assertEqual(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"], "/test")
        self.assertEqual(harness["env"]["E2E_RESULT_ROOT"], "/results")
        self.assertEqual(harness["env"]["E2E_BUILD_ROOT"], "/src/bundle/build")
        # The image's libc, libm, libgcc_s and its ld.so, from the same glibc, beside the
        # client; nothing for the vdso, a library the bundle resolves itself, or one it ships.
        expected = dict(IMAGE_LINKS, **{n: None for n in ("libreverie_dbt_client.so", "libdetcore_dbt.so",
                                                          "libshipped.so")})
        self.assertEqual(harness["rsrcs"], expected)
        self.assertEqual(result["container"], "pinned-root")
        self.assertEqual(result["container_reason"], "privileged lane")
        self.assertEqual(result["empty_workdir"], "HERMIT_E2E_EMPTY_WORKDIR=/test")
        self.assertTrue(result["evidence_complete"], result)
        self.assertEqual(result["outcome"], "PASS")
        after = sorted(str(p.relative_to(self.bundle)) for p in self.bundle.rglob("*"))
        self.assertEqual(after, self.before, "cell.sh must not write into the bundle")
        self.assertEqual((self.bundle / "hermit" / "hermit").read_text(), "hermit\n",
                         "a write in the container reached the bundle: the copy shares its inodes")
        self.assertEqual(list(self.tmp.joinpath("tpx").glob("hermit-cell.*")), [], "scratch left behind")

    def test_hermit_epoch_reaches_the_harness_in_and_out_of_the_container(self) -> None:
        # Backend parity compares a ptrace reference with its candidates only under one guest
        # epoch, so the run's HERMIT_EPOCH (defs.bzl, from validate-node) must reach every
        # cell's harness: through the wrapper's --env in the pinned root, and inherited outside it.
        epoch = "2026-10-05T11:07:23+00:00"
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", HERMIT_EPOCH=epoch)
        self.assertEqual(done["status"], "passed", done)
        [wrapper] = self.calls_by("wrapper")
        self.assertEqual(wrapper["forwarded"]["HERMIT_EPOCH"], epoch)
        [harness] = self.calls_by("harness")
        self.assertEqual(harness["env"]["HERMIT_EPOCH"], epoch)
        done, _ = self.run_cell(HERMIT_EPOCH=epoch)
        self.assertEqual(done["status"], "passed", done)
        self.assertEqual(len(self.calls_by("wrapper")), 1, "outside the container, no wrapper runs")
        self.assertEqual(self.calls_by("harness")[-1]["env"]["HERMIT_EPOCH"], epoch)

    def test_library_missing_from_the_image_is_an_error(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root",
                                     FAKE_LDD_EXTRA="libzstd.so.1 => not found")
        self.assert_error(done, "harness rc=125")
        stderr = (self.tmp / "tpx" / "artifacts" / "harness.stderr").read_text()
        self.assertIn("the pinned-root image has no libzstd.so.1, which the DBT client needs", stderr)
        self.assertEqual(result["harness_rc"], 125)

    def test_interpreter_named_by_its_path_is_linked_by_its_name(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root",
                                FAKE_LDD_EXTRA="/lib64/ld-other.so.2 => /nix/store/o/ld-other.so.2 (0x1)")
        self.assertEqual(done["status"], "passed", done)
        [harness] = self.calls_by("harness")
        self.assertEqual(harness["rsrcs"]["ld-other.so.2"], "/nix/store/o/ld-other.so.2")

    def test_non_dbt_cell_gets_no_links_and_needs_no_client(self) -> None:
        for lib in ("libreverie_dbt_client.so", "libdetcore_dbt.so"):
            (self.bundle / "hermit" / "install" / "rsrcs" / lib).unlink()
        done, result = self.run_cell("ptrace", HERMIT_E2E_CONTAINER="pinned-root", FAKE_LDD_RC="1")
        self.assertEqual(done["status"], "passed", done)
        [wrapper] = self.calls_by("wrapper")
        self.assertEqual(wrapper["command"][0], "env", wrapper["command"])
        [harness] = self.calls_by("harness")
        self.assertEqual(harness["rsrcs"], {"libshipped.so": None})
        self.assertEqual(result["harness_rc"], 0)

    def test_ldd_failure_is_an_error(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", FAKE_LDD_RC="1")
        self.assert_error(done)
        self.assertNotEqual(result["harness_rc"], 0)

    def test_harness_deadline_leaves_the_wrapper_time_to_stop(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", CELL_DEADLINE_S="100")
        self.assertEqual(done["status"], "passed", done)
        # The first bounds the wrapper, the second, run in the container, the harness.
        wrapper_timeout, harness_timeout = [c["argv"] for c in self.calls_by("timeout")]
        self.assertEqual(wrapper_timeout[:1], ["--kill-after=10"], wrapper_timeout)
        self.assertTrue(wrapper_timeout[2].endswith("/run-in-pinned-root.sh"), wrapper_timeout)
        outer = int(wrapper_timeout[1])
        self.assertTrue(85 <= outer <= 100, f"the wrapper may run {outer} s of a 100 s deadline")
        self.assertEqual(harness_timeout[:2], ["--kill-after=10", str(outer - 15)], harness_timeout)
        self.assertTrue(harness_timeout[2].endswith("/bin/test-harness"), harness_timeout)
        self.assertEqual(result["deadline_s"], outer - 15)

    def test_pinned_root_failure_is_reported(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", FAKE_OUTCOME="FAIL")
        self.assertEqual(done["status"], "failed")
        self.assertEqual(json.loads(done["details"])["outcome"], "FAIL")
        self.assertEqual(result["outcome"], "FAIL")
        self.assertEqual(result["harness_rc"], 1, "the harness's own exit status must reach the result")

    def test_missing_image_is_an_error_not_a_host_run(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", FAKE_IMAGE_RC="2")
        self.assert_error(done, "pinned-root image unavailable", "is not present", "ci/hermetic/build-image.sh")
        self.assertEqual(self.calls_by("wrapper"), [])
        self.assertIsNone(result)

    def test_missing_wrapper_is_an_error(self) -> None:
        (self.bundle / "src" / "ci" / "hermetic" / "run-in-pinned-root.sh").unlink()
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root")
        self.assert_error(done, "run-in-pinned-root.sh")

    def test_pinned_root_on_re_is_an_error(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root", HERMIT_E2E_ROUTE="re")
        self.assert_error(done, "needs a local route", "'re'")
        self.assertEqual(self.calls_by("check-image"), [])

    def test_local_cell_uses_the_bound_workdir_like_an_re_cell(self) -> None:
        # Backend parity compares a local candidate (kvm) with a ptrace reference that may
        # have run on RE, where /test cannot be mounted: both must use the bound /tmp/test.
        done, result = self.run_cell(HERMIT_E2E_ROUTE="local")
        self.assertEqual(done["status"], "passed", done)
        [harness] = self.calls_by("harness")
        self.assertIsNone(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"], "local cells use the bound /tmp/test")
        self.assertEqual(result["empty_workdir"], "")

    def test_every_pinned_root_cell_keeps_the_test_workdir(self) -> None:
        # A cell in the pinned root keeps the container's fresh tmpfs at /test, whatever its
        # backend (a DBT cell is there only for the same reasons as any other now).
        for backend, mode in (("dbt", "verify"), ("dbt", "custom"), ("ptrace", "verify"), ("kvm", "verify")):
            with self.subTest(backend=backend, mode=mode):
                self.calls.write_text("")
                done, result = self.run_cell(backend, TEST, mode, HERMIT_E2E_CONTAINER="pinned-root",
                                             HERMIT_E2E_CONTAINER_REASON="privileged lane")
                self.assertEqual(done["status"], "passed", done)
                [wrapper] = self.calls_by("wrapper")
                self.assertEqual(wrapper["forwarded"]["HERMIT_E2E_EMPTY_WORKDIR"], "/test")
                [harness] = self.calls_by("harness")
                self.assertEqual(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"], "/test")
                self.assertEqual(result["empty_workdir"], "HERMIT_E2E_EMPTY_WORKDIR=/test")

    def test_a_callers_workdir_marker_never_reaches_a_bound_cell(self) -> None:
        # A cell outside the pinned root uses the bound /tmp/test even when its caller
        # exported the pinned root's marker.
        done, _ = self.run_cell(HERMIT_E2E_ROUTE="local", HERMIT_E2E_EMPTY_WORKDIR="/test")
        self.assertEqual(done["status"], "passed", done)
        [harness] = self.calls_by("harness")
        self.assertIsNone(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"])

    def test_every_cell_whose_guest_asserts_the_test_workdir_gets_it(self) -> None:
        # The guest of c-programs/environment-and-workdir asserts that it starts in a fresh
        # tmpfs at /test (tests/c/environment_and_workdir.c, check_workdir), the only test
        # source that requires /test rather than adapting to it. Whatever route and container
        # defs.bzl gives each of its cells, cell.sh must hand the harness the /test workdir.
        # 5c59c0c0 gave /test only to pinned-root cells and left the verify cell on the host.
        for routing in ("hybrid", "local"):
            cells = [t for t in evaluate_cells(routing).values()
                     if t["args"][0] == "c-programs/environment-and-workdir"]
            self.assertTrue(cells, routing)
            for target in cells:
                test, mode, backend = target["args"]
                with self.subTest(routing=routing, cell=target["name"]):
                    self.calls.write_text("")
                    done, _ = self.run_cell(backend, test, mode, **target["env"])
                    self.assertEqual(done["status"], "passed", done)
                    [harness] = self.calls_by("harness")
                    self.assertEqual(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"], "/test")

    def test_unknown_container_is_an_error(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="docker")
        self.assert_error(done, "unknown HERMIT_E2E_CONTAINER 'docker'")

    def test_no_container_runs_the_harness_on_the_host(self) -> None:
        done, result = self.run_cell(HERMIT_E2E_ROUTE="re")
        self.assertEqual(done["status"], "passed", done)
        self.assertEqual(self.calls_by("check-image") + self.calls_by("wrapper"), [])
        [harness] = self.calls_by("harness")
        self.assertIsNone(harness["env"]["HERMIT_E2E_EMPTY_WORKDIR"], "RE cells use the bound /tmp/test workdir")
        self.assertEqual(harness["env"]["E2E_BUILD_ROOT"], str(self.bundle / "build"))
        self.assertEqual(harness["env"]["HERMIT_BIN"], str(self.bundle / "hermit" / "hermit"))
        self.assertEqual(result["container"], "")
        self.assertEqual(result["empty_workdir"], "")

    # An RE worker's cgroup is not writable by the test, so the harness cannot give a
    # budgeted invocation a cgroup of its own there (https://github.com/rrnewton/hermit/issues/3766).
    def test_re_cell_declares_a_run_without_cgroups(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_ROUTE="re")
        self.assertEqual(done["status"], "passed", done)
        [harness] = self.calls_by("harness")
        self.assertEqual(harness["env"]["HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN"], "1")

    def test_local_cell_drops_a_callers_cpu_scan_marker(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN="1")
        self.assertEqual(done["status"], "passed", done)
        [harness] = self.calls_by("harness")
        self.assertIsNone(harness["env"]["HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN"])

    def test_pinned_root_cell_drops_a_callers_cpu_scan_marker(self) -> None:
        done, _ = self.run_cell(HERMIT_E2E_CONTAINER="pinned-root",
                                HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN="1")
        self.assertEqual(done["status"], "passed", done)
        [wrapper] = self.calls_by("wrapper")
        self.assertNotIn("HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN", wrapper["forwarded"])
        [harness] = self.calls_by("harness")
        self.assertIsNone(harness["env"]["HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN"])

    # Every KVM ioctl passes a host BPF LSM program that takes one global lock, so a KVM
    # cell's CPU grows with the KVM cells beside it (timed-progress-bar: 3.6 s of CPU with
    # 8 at once, 16.6 s with 48, budget 24 s); local KVM cells share a few host-wide slots.
    def slot_env(self, slots: int, name: str = "kvm-slots") -> dict[str, str]:
        self.slot_dir = self.tmp / name
        self.slot_dir.mkdir()
        return {"HERMIT_E2E_KVM_SLOT_DIR": str(self.slot_dir), "FAKE_KVM_SLOT_DIR": str(self.slot_dir),
                "HERMIT_E2E_KVM_SLOTS": str(slots)}

    def hold_slot(self, n: int):
        f = open(self.slot_dir / f"slot.{n}", "a")
        self.addCleanup(f.close)
        fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return f

    def assert_slot_free(self, n: int) -> None:
        with open(self.slot_dir / f"slot.{n}", "a") as f:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)  # raises if still held

    def assert_no_slot_descriptor(self, harness: dict) -> None:
        held = [t for t in harness["fd_targets"] if t.startswith(str(self.slot_dir) + "/")]
        self.assertEqual(held, [], "the harness inherited a KVM slot's descriptor")

    def test_kvm_cell_waits_for_a_free_slot_and_holds_it_for_the_whole_run(self) -> None:
        for container in ("", "pinned-root"):
            with self.subTest(container=container):
                self.calls.write_text("")
                env = self.slot_env(2, "kvm-slots-" + (container or "host"))
                self.hold_slot(0)
                busy = self.hold_slot(1)
                threading.Timer(1.0, busy.close).start()
                done, result = self.run_cell("kvm", HERMIT_E2E_CONTAINER=container, CELL_DEADLINE_S="100",
                                             **env)
                self.assertEqual(done["status"], "passed", done)
                # Slot 0 is held throughout and slot 1 only frees after 1 s, so holding slot 1
                # means the cell waited for it.
                self.assertEqual(result["kvm_slot"], "1", result)
                self.assertGreater(result["kvm_slot_wait_ms"], 0, result)
                # The wait comes out of the harness deadline (in the container, 15 s less again).
                self.assertLessEqual(result["deadline_s"], 84 if container else 99, result)
                [harness] = self.calls_by("harness")
                # slot.0 is the test's; slot.1, released by the test, is now the cell's.
                self.assertEqual(harness["slots"], {"slot.0": "locked", "slot.1": "locked"})
                self.assert_no_slot_descriptor(harness)
                self.assert_slot_free(1)

    def test_kvm_cell_with_no_free_slot_runs_after_half_its_deadline(self) -> None:
        env = self.slot_env(1)
        self.hold_slot(0)
        done, result = self.run_cell("kvm", CELL_DEADLINE_S="10", **env)
        self.assertEqual(done["status"], "passed", done)
        self.assertEqual(result["kvm_slot"], "none", result)
        self.assertGreaterEqual(result["kvm_slot_wait_ms"], 5000, result)
        self.assertLess(result["kvm_slot_wait_ms"], 9000, result)
        self.assertEqual(len(self.calls_by("harness")), 1)

    def test_only_local_kvm_cells_take_a_slot(self) -> None:
        # Slot 0 is held and slot 1 does not exist. A cell that tries for a slot first opens
        # every slot file for append, which creates slot.1, before it takes a slot or waits for
        # one: the local KVM cell takes slot 1. A slot.1 that never appears therefore shows
        # that a cell did neither, however slowly a loaded host ran it.
        for backend, route in (("kvm", "local"), ("ptrace", "local"), ("dbt", "local"), ("kvm", "re")):
            with self.subTest(backend=backend, route=route):
                self.calls.write_text("")
                env = self.slot_env(2, f"kvm-slots-{backend}-{route}")
                self.hold_slot(0)
                done, result = self.run_cell(backend, HERMIT_E2E_ROUTE=route, CELL_DEADLINE_S="100",
                                             **env)
                self.assertEqual(done["status"], "passed", done)
                [harness] = self.calls_by("harness")
                self.assert_no_slot_descriptor(harness)
                slot_files = sorted(p.name for p in self.slot_dir.iterdir())
                if (backend, route) == ("kvm", "local"):
                    self.assertEqual(result["kvm_slot"], "1", result)
                    self.assertEqual(harness["slots"], {"slot.0": "locked", "slot.1": "locked"})
                    self.assertEqual(slot_files, ["slot.0", "slot.1"])
                else:
                    self.assertEqual((result["kvm_slot"], result["kvm_slot_wait_ms"]), ("", 0), result)
                    self.assertEqual(harness["slots"], {"slot.0": "locked"})
                    self.assertEqual(slot_files, ["slot.0"],
                                     "a cell that takes no slot must not try for one")

    def test_bad_kvm_slot_count_is_an_error(self) -> None:
        done, _ = self.run_cell("kvm", **dict(self.slot_env(1), HERMIT_E2E_KVM_SLOTS="0"))
        self.assert_error(done, "HERMIT_E2E_KVM_SLOTS must be a positive integer, not '0'")


    def assert_evidence(self, complete: bool, missing: str = "detlogs", **env: str) -> None:
        done, result = self.run_cell(**env)
        details = json.loads(done["details"])
        if complete:
            self.assertEqual(done["status"], "passed", done)
            self.assertTrue(result["evidence_complete"], result)
        else:
            self.assertEqual(done["status"], "failed", done)
            self.assertEqual(details["outcome"], "ERROR")
            self.assertIn(missing, details["reason"])
            self.assertFalse(result["evidence_complete"], result)
            self.assertIn(missing, result["missing"])

    def test_matched_verify_with_only_the_golden_log_is_complete(self) -> None:
        self.assert_evidence(True)
        self.assert_evidence(True, FAKE_VERDICT="matched", FAKE_DETLOGS="1")

    def test_matched_verify_without_the_golden_log_is_an_error(self) -> None:
        self.assert_evidence(False, FAKE_VERDICT="matched", FAKE_DETLOGS="2")
        self.assert_evidence(False, FAKE_VERDICT="matched", FAKE_DETLOGS="")

    def test_matched_verify_that_kept_both_logs_is_an_error(self) -> None:
        # hermit deletes run 2's log after a match; finding it means the retention changed.
        self.assert_evidence(False, FAKE_VERDICT="matched", FAKE_DETLOGS="12")

    def test_unmatched_passing_verify_needs_both_logs(self) -> None:
        self.assert_evidence(True, FAKE_VERDICT="diverged", FAKE_DETLOGS="12")
        self.assert_evidence(False, FAKE_VERDICT="diverged", FAKE_DETLOGS="1")

    def test_passing_verify_without_a_readable_verdict_file_is_an_error(self) -> None:
        # Both logs present, so only the verdict file itself can make these incomplete.
        for verify_json in ("missing", "garbage"):
            with self.subTest(verify_json=verify_json):
                self.assert_evidence(False, "verify-1.json", FAKE_VERIFY_JSON=verify_json,
                                     FAKE_DETLOGS="12")
        self.assert_evidence(False, "verify-1.json", FAKE_VERDICT="", FAKE_DETLOGS="12")
    def test_local_host_inapplicable_cell_is_a_skip_with_the_harness_reason(self) -> None:
        done, result = self.run_cell(FAKE_OUTCOME="HOST-INAPPLICABLE", HERMIT_E2E_ROUTE="local")
        self.assertEqual(done["test"], CELL)
        self.assertEqual(done["status"], "skipped", done)
        details = json.loads(done["details"])
        self.assertEqual(details["outcome"], "HOST-INAPPLICABLE")
        self.assertEqual(details["reason"],
                         "SKIP: NOT RUN, NOT a pass, no coverage: this machine lacks exact-branch-counter")
        self.assertEqual(result["harness_rc"], 1)

    def test_re_host_inapplicable_cell_still_fails_so_tpx_retries_it(self) -> None:
        # ci/buck-e2e/run relies on the failure: Tpx retries the cell on another RE worker.
        done, _ = self.run_cell(FAKE_OUTCOME="HOST-INAPPLICABLE", HERMIT_E2E_ROUTE="re")
        self.assertEqual(done["status"], "failed", done)
        self.assertIn("produced 2 test_done records", json.loads(done["details"])["reason"])

    def test_host_inapplicable_cell_with_another_run_failure_still_fails(self) -> None:
        done, _ = self.run_cell(FAKE_OUTCOME="HOST-INAPPLICABLE", HERMIT_E2E_ROUTE="local",
                                FAKE_RUN_FAILURES=json.dumps(["cannot write the test-count file"]))
        self.assertEqual(done["status"], "failed", done)
        self.assertIn("produced 2 test_done records", json.loads(done["details"])["reason"])

    # Each of these changes one part of the withheld pair; each must stay a plain failure,
    # not a skip, so every clause of cell.sh's withheld_here predicate is pinned.
    def assert_withheld_variant_fails(self, **env: str) -> None:
        done, _ = self.run_cell(FAKE_OUTCOME="HOST-INAPPLICABLE", HERMIT_E2E_ROUTE="local", **env)
        self.assertEqual(done["test"], CELL)
        self.assertEqual(done["status"], "failed", done)
        self.assertIn("produced 2 test_done records", json.loads(done["details"])["reason"])

    def test_a_skipped_record_for_another_cell_still_fails(self) -> None:
        self.assert_withheld_variant_fails(FAKE_WITHHELD_TEST="other_cell/strict@ptrace")

    def test_a_skipped_record_whose_outcome_is_not_host_inapplicable_still_fails(self) -> None:
        self.assert_withheld_variant_fails(FAKE_WITHHELD_OUTCOME="FAIL")

    def test_a_host_inapplicable_record_that_is_not_skipped_still_fails(self) -> None:
        self.assert_withheld_variant_fails(FAKE_WITHHELD_STATUS="failed")

    def test_a_single_run_failure_other_than_the_vacuity_refusal_still_fails(self) -> None:
        self.assert_withheld_variant_fails(
            FAKE_RUN_FAILURES_ONLY=json.dumps(["cannot write the test-count file"]))

    def test_a_diverged_cells_run_logs_come_back_through_ingest(self) -> None:
        # Run 2's log is over cell.sh's 256 KiB compression threshold, as a real DETLOG is.
        done, result = self.run_cell(FAKE_OUTCOME="FAIL", FAKE_VERDICT="diverged", FAKE_DETLOG_LINES="20000")
        self.assertEqual(done["status"], "failed", done)
        artifacts = sorted(p.name for p in (self.tmp / "tpx" / "artifacts").iterdir())
        self.assertIn("cell__verify-logs__verify-1__run2_log_detlog.zst", artifacts)
        expected = {f"{result['run_id']}/verify-logs/verify-1/run{n}_log_detlog":
                    "".join(f"run {n} detlog line {i}\n" for i in range(20000)).encode() for n in (1, 2)}
        # As testx lists the execution, and as Buck leaves a local one in buck-out.
        fake = self.tmp / "testx"
        shutil.copytree(self.tmp / "tpx" / "artifacts", fake / "art" / "RUN.1.100")
        listing = [{"test_details": {"name": f"//ci/buck-e2e:cell - {CELL}", "id": 1}, "end_time": 100,
                    "artifacts": [{"name": n} for n in artifacts]}]
        (fake / "list-RUN.json").write_text(json.dumps({"results": {"test_results": listing}}))
        write_exe(self.tmp / "bin" / "testx", ingest_test.FAKE_TESTX)
        buck_out = self.tmp / "buck-out-test-execution"
        shutil.copytree(self.tmp / "tpx" / "artifacts", buck_out / "c" / "t" / "default" / "artifacts_directory")
        plan = self.tmp / "plan.json"
        plan.write_text(json.dumps({"cells": [{"test": TEST, "mode": MODE, "backend": BACKEND,
                                               "lane": "portable", "category": "cat"}]}))
        for route, local in (("remote", []), ("local", ["--local-artifacts", str(buck_out)])):
            with self.subTest(route):
                kept = self.tmp / route / "kept"
                proc = subprocess.run(
                    [sys.executable, str(CI / "buck-e2e" / "ingest.py"), "--plan", str(plan),
                     "--out", str(self.tmp / route / "import"), "--work", str(self.tmp / route / "work"),
                     *local, "--failed-verify-logs", str(kept), "RUN"],
                    capture_output=True, text=True, timeout=120,
                    env=dict(os.environ, TESTX=str(self.tmp / "bin" / "testx"), FAKE_TESTX_DIR=str(fake)))
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertEqual(json.loads(proc.stdout)["sources"], {"testx" if route == "remote" else "local": 1})
                files = {str(p.relative_to(kept)): p.read_bytes() for p in kept.rglob("*")
                         if p.is_file() and p.name != "index.jsonl"}
                self.assertEqual(files, expected)


class _Anything:
    """Stand-in for Buck builtins defs.bzl names at load time but these tests never call."""

    def __getattr__(self, _name: str) -> "_Anything":
        return self

    def __call__(self, *_args, **_kwargs) -> "_Anything":
        return self


def evaluate_cells(routing: str, pmu_on_re: str = "true") -> dict[str, dict]:
    """Runs defs.bzl's hermit_e2e_cells over the real plan; returns each target's attrs by name."""
    targets: dict[str, dict] = {}
    rule_calls = []

    def rule(impl, attrs):
        rule_calls.append(impl.__name__)
        if impl.__name__ == "_cell_test_impl":
            return lambda **kwargs: targets.__setitem__(kwargs["name"], kwargs)
        return _Anything()

    config = {("hermit_e2e", "routing"): routing, ("hermit_e2e", "pmu_on_re"): pmu_on_re}
    namespace = {
        "rule": rule,
        "attrs": _Anything(),
        "read_root_config": lambda section, key, default=None: config.get((section, key), default),
        "native": _Anything(),
    }
    exec(compile(DEFS.read_text(), str(DEFS), "exec"), namespace)
    assert "_cell_test_impl" in rule_calls, rule_calls
    namespace["hermit_e2e_cells"](json.loads(PLAN.read_text()), json.loads(RE_EXCLUSIONS.read_text()))
    return targets


class ContainerChoiceTest(unittest.TestCase):
    plan = json.loads(PLAN.read_text())["cells"]

    def slug(self, cell: dict) -> str:
        return "{}-{}-{}".format(cell["test"].replace("/", "-"), cell["mode"], cell["backend"])

    def check(self, routing: str) -> dict[str, dict]:
        targets = evaluate_cells(routing)
        self.assertEqual(len(targets), len(self.plan))
        for cell in self.plan:
            target = targets[self.slug(cell)]
            env, labels = target["env"], target["labels"]
            # cell.sh grants the CPU-scan marker from this variable, while the
            # rule's `route` picks the executor; they must name the same route.
            self.assertEqual(env["HERMIT_E2E_ROUTE"], target["route"], target["name"])
            expected = target["route"] == "local" and (
                cell["lane"] == "privileged"
                or cell["test"] == "c-programs/environment-and-workdir")
            self.assertEqual(env["HERMIT_E2E_CONTAINER"], "pinned-root" if expected else "", target["name"])
            self.assertEqual(bool(env["HERMIT_E2E_CONTAINER_REASON"]), expected, target["name"])
            self.assertEqual("hermit_e2e_container_pinned_root" in labels, expected, target["name"])
        return targets

    def containerized(self, targets: dict[str, dict]) -> set[str]:
        return {n for n, t in targets.items() if t["env"]["HERMIT_E2E_CONTAINER"] == "pinned-root"}

    def test_hybrid(self) -> None:
        targets = self.check("hybrid")
        chosen = self.containerized(targets)
        privileged = {self.slug(c) for c in self.plan if c["lane"] == "privileged"}
        self.assertTrue(privileged, "the plan has privileged-lane cells")
        self.assertLessEqual(privileged, chosen)
        self.assertIn("c-programs-environment-and-workdir-custom-ptrace", chosen)
        self.assertIn("c-programs-environment-and-workdir-verify-ptrace", chosen)
        # The DBT adapter takes its own user namespace for its mounts, so a DBT cell needs no
        # container and no particular route: a kvm test's DBT verify cell runs locally with
        # its parity reference, and the others go to RE.
        self.assertEqual(targets["c-programs-cpuid-probe-verify-dbt"]["route"], "local")
        dbt = {self.slug(c) for c in self.plan if c["backend"] == "dbt" and c["lane"] != "privileged"}
        self.assertTrue(dbt)
        self.assertFalse(dbt & chosen)
        self.assertTrue(any(targets[name]["route"] == "re" for name in dbt))
        # The test's KVM verify cell (enabled 2026-10-08) also runs locally, so it gets the
        # same pinned-root /test workdir its guest asserts, and so do its in-guest LiteInst
        # verify cells (liteinst and in-guest-trap, enabled the same day).
        self.assertEqual(chosen, privileged | {"c-programs-environment-and-workdir-custom-ptrace",
                                               "c-programs-environment-and-workdir-verify-ptrace",
                                               "c-programs-environment-and-workdir-verify-kvm",
                                               "c-programs-environment-and-workdir-verify-liteinst",
                                               "c-programs-environment-and-workdir-verify-in-guest-trap"})

    def test_local(self) -> None:
        chosen = self.containerized(self.check("local"))
        dbt = {self.slug(c) for c in self.plan if c["backend"] == "dbt" and c["lane"] != "privileged"}
        self.assertTrue(dbt)
        self.assertFalse(dbt & chosen)


class ParityRouteTest(unittest.TestCase):
    """Backend parity credits a candidate verify cell against its test's ptrace verify cell
    only when both ran on one route and container (ci/manifest-plan/src/parity.rs
    shares_route), so each pair must be routed alike."""

    # Every routing configuration defs.bzl accepts: hybrid with the PMU allowed on RE or
    # not (-c hermit_e2e.pmu_on_re), and all-local.
    CONFIGS = (("hybrid", "true"), ("hybrid", "false"), ("local", "true"))

    def routes(self, routing: tuple[str, str]) -> dict[tuple[str, str], tuple[str, str]]:
        targets = evaluate_cells(*routing)
        return {(t["args"][0], t["args"][2]): (t["route"], t["env"]["HERMIT_E2E_CONTAINER"])
                for t in targets.values() if t["args"][1] == "verify"}

    def test_every_kvm_cells_ptrace_reference_shares_its_route(self) -> None:
        for routing in self.CONFIGS:
            routes = self.routes(routing)
            kvm = sorted(test for test, backend in routes if backend == "kvm")
            self.assertGreater(len(kvm), 250, routing)
            for test in kvm:
                if (test, "ptrace") in routes:
                    self.assertEqual(routes[(test, "ptrace")], routes[(test, "kvm")], (routing, test))

    def test_every_sabre_cell_shares_its_references_route(self) -> None:
        # Moving a reference to the kvm cell's route must not leave the test's sabre cell,
        # compared with the same reference, on another route.
        for routing in self.CONFIGS:
            routes = self.routes(routing)
            sabre = sorted(test for test, backend in routes if backend == "sabre")
            self.assertGreater(len(sabre), 200, routing)
            for test in sabre:
                if (test, "ptrace") in routes:
                    self.assertEqual(routes[(test, "sabre")], routes[(test, "ptrace")], (routing, test))

    def test_every_dbt_cell_shares_its_references_route(self) -> None:
        # A DBT verify cell is compared with its test's ptrace verify cell like any other
        # candidate, so it runs on that cell's route and container, local or RE.
        for routing in self.CONFIGS:
            routes = self.routes(routing)
            dbt = sorted(test for test, backend in routes if backend == "dbt")
            self.assertGreater(len(dbt), 20, routing)
            for test in dbt:
                if (test, "ptrace") in routes:
                    self.assertEqual(routes[(test, "dbt")], routes[(test, "ptrace")], (routing, test))

    def test_every_in_guest_cells_ptrace_reference_shares_its_route(self) -> None:
        # A liteinst or in-guest-trap verify cell always runs locally (InGuestRouteTest), so
        # its ptrace reference must run there too, or parity never credits the pair.
        for routing in self.CONFIGS:
            routes = self.routes(routing)
            for backend in IN_GUEST_BACKENDS:
                tests = sorted(test for test, b in routes if b == backend)
                self.assertGreater(len(tests), 250, (routing, backend))
                for test in tests:
                    if (test, "ptrace") in routes:
                        self.assertEqual(routes[(test, "ptrace")], routes[(test, backend)],
                                         (routing, backend, test))


# The two backends that run the in-guest LiteInst runtime.
IN_GUEST_BACKENDS = ("liteinst", "in-guest-trap")


class InGuestRouteTest(unittest.TestCase):
    """The in-guest LiteInst runtime (liteinst and in-guest-trap) arms CPUID faulting before
    the guest's first instruction and refuses to start without it, and RE workers may lack
    it: a validate of d60c6e78 sent 251 cells of each backend to RE, and every one that
    landed on a worker without CPUID faulting failed (exit 125 for liteinst, refusal 122 for
    in-guest-trap). Each such plan row names cpuid-faulting, so _route keeps it local."""

    plan = json.loads(PLAN.read_text())["cells"]

    def test_every_in_guest_plan_row_needs_cpuid_faulting(self) -> None:
        rows = [c for c in self.plan if c["backend"] in IN_GUEST_BACKENDS]
        self.assertGreater(len(rows), 500)
        for row in rows:
            self.assertIn("cpuid-faulting", row.get("requires_host_capabilities", []), row)

    def test_every_in_guest_cell_runs_locally_under_every_config(self) -> None:
        for routing in ParityRouteTest.CONFIGS:
            targets = evaluate_cells(*routing)
            for backend in IN_GUEST_BACKENDS:
                cells = [t for t in targets.values() if t["args"][2] == backend]
                self.assertGreater(len(cells), 250, (routing, backend))
                remote = sorted(t["name"] for t in cells if t["route"] != "local")
                self.assertEqual(remote, [], (routing, backend))


if __name__ == "__main__":
    unittest.main()
