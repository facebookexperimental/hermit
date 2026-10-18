#!/usr/bin/env python3
"""Scenario tests for the host-prerequisite check in ci/buck-e2e/stage --from-cargo.

Each case runs a copy of stage in a scratch tree, with a PATH holding only the commands
it needs and a scratch library directory (HERMIT_STAGE_HOST_LIBDIR) in place of
/usr/lib64. Refuse: a missing patchelf, readelf, strip or cmake, or a missing libunwind
library, stops stage with the dnf package that provides it and the documentation
section, and the previous staging is left in place. Accept: with everything present,
stage gets past the check and removes the previous staging (it then fails on the
scratch tree, which is not a git checkout). --bundle needs none of them.

StageCargoOverlapTest runs stage in a committed scratch checkout with a fake cargo. The
release harness build and the validate-profile hermit build, in a target directory of
its own, overlap detcore-dbt and the install bundle and finish before the harness
binaries are stripped, each build runs main's exact command line, and the hermit binary
is copied into target/validate only after its build ends, even when it ends last; a
failed background build stops stage with its exit status and its log; a failed
detcore-dbt build stops stage and both running background builds with it, and each
background build's own child gets the SIGTERM too (cargo does not pass it on, so stage
signals the build's whole process group). The fake cargo creates a missing target
directory as cargo does, renaming a temporary directory onto it; forced into the
interleaving that failed a validation, the release build replaces the empty target/ the
hermit build has just made as its target directory's parent, and the hermit build fails
with ENOENT at its temporary directory. Stage makes both target directories before any
build starts, so the forced interleaving cannot arise, and the fake alone, without them,
reproduces the failure.

Both classes above stage in place (HERMIT_BUCK_STAGE_REPRODUCIBLE=0): the check and the
builds are the same code under reproducible staging, which only re-runs stage from a
copy. StageReproducibleTest covers that wrapper, with the same fake cargo. The builds
run in the fixed directory's copy of the checkout, with CARGO_HOME reached through the
fixed directory's symlink and SOURCE_DATE_EPOCH at HEAD's commit time, and the copy, the
symlink and the holder note are removed afterwards; a failed build keeps the previous
staging and returns its status; a stage that cannot take the lock within
HERMIT_BUCK_STAGE_LOCK_TIMEOUT fails naming the holder, runs no build and leaves the
holder's note alone; a missing host package is refused before the lock; and an
uncommitted change is refused even when the copy cannot read a submodule whose .git file
names its gitdir by a relative path (git status fails there and prints nothing), and a
status git cannot read at all is refused rather than taken as clean.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


STAGE = Path(__file__).resolve().parent / "buck-e2e" / "stage"
RETRY = Path(__file__).resolve().parent / "retry-on-compiler-crash.sh"
TOOLS = ("patchelf", "readelf", "strip", "cmake")
LIBS = ("libunwind.so.8", "libunwind-x86_64.so.8")
# Commands stage runs on its way to the check and just after it.
HOST_COMMANDS = ("dirname", "realpath", "rm", "mkdir", "git", "cp", "python3", "du", "cut", "cat")
# And the reproducible-staging wrapper's own.
WRAPPER_COMMANDS = ("flock", "ln", "date")
DOC = '(docs/BUCK2_OSS.md, "Host prerequisites for Buck validation")'


class StagePrerequisiteTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="buck-e2e-stage-test."))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.stage = self.tmp / "tree" / "ci" / "buck-e2e" / "stage"
        self.stage.parent.mkdir(parents=True)
        shutil.copy2(STAGE, self.stage)
        self.sentinel = self.stage.parent / "staged" / "SENTINEL"
        self.sentinel.parent.mkdir()
        self.sentinel.write_text("previous staging\n")
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name in HOST_COMMANDS:
            real = shutil.which(name)
            self.assertIsNotNone(real, name)
            (self.bin / name).symlink_to(real)
        for name in TOOLS:
            self.tool(name).write_text("#!/bin/sh\nexit 0\n")
            self.tool(name).chmod(0o755)
        self.libdir = self.tmp / "lib64"
        self.libdir.mkdir()
        for name in LIBS:
            (self.libdir / name).write_text(name + "\n")

    def tool(self, name: str) -> Path:
        return self.bin / name

    def run_stage(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        base = {"PATH": str(self.bin), "HOME": str(self.tmp), "HERMIT_STAGE_HOST_LIBDIR": str(self.libdir),
                "HERMIT_BUCK_STAGE_REPRODUCIBLE": "0"}
        base.update(env)
        return subprocess.run(["/bin/bash", str(self.stage), *args], env=base, capture_output=True,
                              text=True, timeout=60)

    def assert_refused(self, proc: subprocess.CompletedProcess, what: str, package: str) -> None:
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn(f"ci/buck-e2e/stage: {what} is missing; install it with  sudo dnf install -y {package}",
                      proc.stderr)
        self.assertIn(DOC, proc.stderr)
        self.assertTrue(self.sentinel.exists(), "a refusal removed the previous staging")

    def test_each_missing_tool_names_its_package(self) -> None:
        for name, package in (("patchelf", "patchelf"), ("readelf", "binutils"), ("strip", "binutils"),
                              ("cmake", "cmake")):
            with self.subTest(name):
                saved = self.tool(name).read_text()
                self.tool(name).unlink()
                try:
                    self.assert_refused(self.run_stage("--from-cargo"), name, package)
                finally:
                    self.tool(name).write_text(saved)
                    self.tool(name).chmod(0o755)

    def test_cmake_named_by_the_cmake_variable(self) -> None:
        self.tool("cmake").rename(self.tool("cmake-pinned"))
        self.assert_refused(self.run_stage("--from-cargo"), "cmake", "cmake")
        proc = self.run_stage("--from-cargo", CMAKE="cmake-pinned")
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), proc.stderr)

    def test_each_missing_libunwind_library_names_libunwind_devel(self) -> None:
        for name in LIBS:
            with self.subTest(name):
                (self.libdir / name).unlink()
                try:
                    self.assert_refused(self.run_stage("--from-cargo"), f"{self.libdir}/{name}", "libunwind-devel")
                finally:
                    (self.libdir / name).write_text(name + "\n")

    def test_all_present_passes_the_check(self) -> None:
        proc = self.run_stage("--from-cargo")
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), f"stage stopped before replacing the staging: {proc.stderr}")
        self.assertNotEqual(proc.returncode, 0, "the scratch tree is not a checkout, so stage cannot finish")

    def test_invalid_build_width_preserves_previous_staging(self) -> None:
        for width in ("0", "-1", "eight"):
            with self.subTest(width=width):
                proc = self.run_stage("--from-cargo", CARGO_BUILD_JOBS=width)
                self.assertEqual(proc.returncode, 2, proc.stderr)
                self.assertIn("CARGO_BUILD_JOBS must be a positive integer", proc.stderr)
                self.assertTrue(self.sentinel.exists(), "invalid admission removed previous staging")

    def test_bundle_mode_needs_no_host_packages(self) -> None:
        for name in TOOLS:
            self.tool(name).unlink()
        shutil.rmtree(self.libdir)
        proc = self.run_stage("--bundle", str(self.tmp / "no-bundle"))
        self.assertNotIn("is missing", proc.stderr)
        self.assertFalse(self.sentinel.exists(), proc.stderr)


FAKE_CARGO = r"""#!/usr/bin/env python3
import json, os, signal, subprocess, sys, tempfile, time
args = sys.argv[1:]
if "--release" in args:
    label = "release"
else:
    label = "+".join(args[i + 1] for i, a in enumerate(args) if a == "-p")
target_dir = args[args.index("--target-dir") + 1]
log = os.environ["FAKE_CARGO_LOG"]
def wait_for(event, seconds=5.0):
    deadline = time.monotonic() + seconds
    while not os.path.exists(log) or event not in open(log).read():
        if time.monotonic() > deadline:
            return False
        time.sleep(0.02)
    return True
def mark(event):
    with open(log, "a") as f:
        f.write(f"{event} {label} {time.monotonic():.3f} {target_dir}\n")
# Like cargo (cargo_util::paths::create_dir_all_excluded_from_backups_atomic), a missing
# target directory is created by making its parent, then renaming a temporary directory
# made in that parent onto it; a failed rename is ignored if the directory now exists.
# FAKE_CARGO_RACE_TARGET forces the interleaving a concurrent stage can hit: the hermit
# build makes $target_dir (its target directory's parent) after the release build has
# found $target_dir missing, and the release build's rename then replaces it. Each
# pause happens only on the branch where the build found its target directory missing,
# except that the other builds wait for the release build's to exist, so they cannot
# create $target_dir first and hide the interleaving.
race = os.environ.get("FAKE_CARGO_RACE_TARGET") == "1"
if race and label not in ("release", "hermit"):
    wait_for("created release ")
if not os.path.isdir(target_dir):
    if race and label == "hermit":
        wait_for("checked release ")
    parent = os.path.dirname(target_dir)
    os.makedirs(parent, exist_ok=True)
    # The kernel resolves the parent before it creates the temporary directory in it.
    # If a rename replaces the parent in between, the creation lands in a removed
    # directory and fails with ENOENT. The forced interleaving holds the resolved
    # parent open across the other build's rename to make that window certain.
    parent_fd = os.open(parent, os.O_RDONLY | os.O_DIRECTORY)
    if race and label == "hermit":
        mark("parent-made")
        wait_for("created release ")
    elif race and label == "release":
        mark("checked")
    try:
        name = os.path.basename(tempfile.mktemp(prefix=os.path.basename(target_dir), dir=parent))
        os.mkdir(name, dir_fd=parent_fd)
        tmp = os.path.join(parent, name)
        with open(os.path.join(tmp, "CACHEDIR.TAG"), "w") as f:
            f.write("Signature: 8a477f597d28d172789f06886806bc55\n")
    except FileNotFoundError:
        print(f"error: No such file or directory (os error 2) at path \"{parent}/{name}\"", file=sys.stderr)
        sys.exit(101)
    if race and label == "release":
        wait_for("parent-made hermit ")
    try:
        os.rename(tmp, target_dir)
    except OSError:
        if not os.path.exists(target_dir):
            raise
        os.remove(os.path.join(tmp, "CACHEDIR.TAG"))
        os.rmdir(tmp)
if race and label == "release":
    mark("created")
# Like cargo, every build creates its profile directory.
os.makedirs(os.path.join(target_dir, "release" if "--release" in args else args[args.index("--profile") + 1]),
            exist_ok=True)
def note(event):
    with open(log, "a") as f:
        f.write(f"{event} {label} {time.monotonic():.3f} {target_dir}\n")
# A child that records the SIGTERM it gets, so a test can tell a group-wide SIGTERM from
# a later SIGKILL. It reports "ready" only once its handler is installed: a SIGTERM that
# arrives before then kills it silently, which reads as no SIGTERM at all.
CHILD = '''import signal, sys, time
def term(*_):
    with open(sys.argv[1], "a") as f:
        f.write(f"child-term {sys.argv[2]} {time.monotonic():.3f} -\\n")
    sys.exit(143)
signal.signal(signal.SIGTERM, term)
print("ready", flush=True)
time.sleep(30)
'''
def killed(*_):
    note("killed")
    sys.exit(143)
# Like cargo, SIGTERM stops only this process: a child it started (rustc, a build
# script) keeps running unless its whole process group is signalled.
signal.signal(signal.SIGTERM, killed)
if os.environ.get("FAKE_CARGO_CHILDREN"):
    # The child is started, and its SIGTERM handler installed, before the build notes
    # "start", so a build that has started has a child able to record a SIGTERM.
    child = subprocess.Popen([sys.executable, "-c", CHILD, log, label], stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if child.stdout.readline() != b"ready\n":
        note("child-not-ready")
        sys.exit(98)
    with open(os.environ["FAKE_CARGO_CHILDREN"], "a") as f:
        f.write(f"{label} {child.pid}\n")
note("start")
with open(log, "a") as f:
    f.write(f"argv {label} {json.dumps(args)}\n")
    cargo_home = os.environ.get("CARGO_HOME")
    f.write(f"env {label} " + json.dumps({
        "CARGO_HOME": cargo_home,
        "CARGO_HOME_TARGET": os.path.realpath(cargo_home) if cargo_home else None,
        "SOURCE_DATE_EPOCH": os.environ.get("SOURCE_DATE_EPOCH"),
    }) + "\n")
print(f"fake cargo {label} output", file=sys.stderr)
if os.environ.get("FAKE_CARGO_FAIL") == label:
    # Fail only once every named build (comma-separated) has started, so the test sees
    # each one running.
    waited_for = [name for name in os.environ.get("FAKE_CARGO_FAIL_AFTER_START", "").split(",") if name]
    deadline = time.monotonic() + 20
    while not all(f"start {name} " in open(log).read() for name in waited_for):
        if time.monotonic() > deadline:
            note("never-started")
            sys.exit(99)
        time.sleep(0.05)
    note("fail")
    sys.exit(int(os.environ.get("FAKE_CARGO_FAIL_STATUS", "3")))
time.sleep(float(os.environ.get(f"FAKE_CARGO_SECONDS_{label.replace('-', '_').replace('+', '_')}", "0.5")))
if label == "hermit":
    with open(os.path.join(target_dir, "validate", "hermit"), "w") as f:
        f.write("fake hermit built in " + target_dir + "\n")
if label == "hermit-install+detcore-sabre+detcore-liteinst":
    # An explicitly synthetic finalizer: exercise stage's ordering and failure
    # propagation, without claiming to build or qualify any runtime bytes.
    finalizer = os.path.join(target_dir, "validate", "stage-detcore-liteinst")
    with open(finalizer, "w") as f:
        f.write('''#!/usr/bin/env python3
import json, os, sys, time
args = sys.argv[1:]
target = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
def note(event):
    with open(os.environ["FAKE_CARGO_LOG"], "a") as log:
        log.write(f"{event} finalizer {time.monotonic():.3f} {target}\\n")
with open(os.environ["FAKE_CARGO_LOG"], "a") as log:
    log.write("finalizer-argv " + json.dumps(args) + "\\n")
note("start")
status = int(os.environ.get("FAKE_FINALIZER_FAIL_STATUS", "0"))
if status:
    note("fail")
    print(f"synthetic finalizer failure (exit {status})", file=sys.stderr)
    sys.exit(status)
note("end")
''')
    os.chmod(finalizer, 0o755)
note("end")
"""


class StageCargoOverlapTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="buck-e2e-stage-overlap."))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        tree = self.tmp / "tree"
        self.stage = tree / "ci" / "buck-e2e" / "stage"
        self.stage.parent.mkdir(parents=True)
        shutil.copy2(STAGE, self.stage)
        # stage runs each cargo build through this wrapper; a clean or plainly
        # failing fake cargo passes through it unchanged.
        shutil.copy2(RETRY, tree / "ci" / "retry-on-compiler-crash.sh")
        (tree / ".gitignore").write_text("ci/buck-e2e/staged/\n")
        git = ["git", "-C", str(tree), "-c", "user.email=t@example.invalid", "-c", "user.name=t",
               "-c", "commit.gpgsign=false"]
        subprocess.run(git[:3] + ["init", "-q"], check=True)
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "fixture"], check=True)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name in HOST_COMMANDS + ("mktemp", "setsid", "sleep", "tee", "grep"):
            real = shutil.which(name)
            self.assertIsNotNone(real, name)
            (self.bin / name).symlink_to(real)
        for name in TOOLS:
            (self.bin / name).write_text("#!/bin/sh\nexit 0\n")
            (self.bin / name).chmod(0o755)
        (self.bin / "strip").write_text('#!/bin/sh\necho "strip $*" >>"$FAKE_CARGO_LOG"\n')
        (self.bin / "install").write_text(f'#!/bin/sh\necho "install $*" >>"$FAKE_CARGO_LOG"\nexec {shutil.which("install")} "$@"\n')
        (self.bin / "cargo").write_text(FAKE_CARGO)
        for name in ("strip", "install", "cargo"):
            (self.bin / name).chmod(0o755)
        self.libdir = self.tmp / "lib64"
        self.libdir.mkdir()
        for name in LIBS:
            (self.libdir / name).write_text(name + "\n")
        self.log = self.tmp / "cargo.log"

    def run_stage(self, **env: str) -> subprocess.CompletedProcess:
        base = {"PATH": str(self.bin), "HOME": str(self.tmp), "HERMIT_STAGE_HOST_LIBDIR": str(self.libdir),
                "FAKE_CARGO_LOG": str(self.log), "HERMIT_BUCK_STAGE_REPRODUCIBLE": "0"}
        base.update(env)
        return subprocess.run(["/bin/bash", str(self.stage), "--from-cargo"], env=base, capture_output=True,
                              text=True, timeout=60)

    def events(self) -> list[tuple[str, str, float]]:
        rows = []
        self.target_dirs = {}
        self.argv = {}
        self.env = {}
        self.finalizer_argv = []
        for line in self.log.read_text().splitlines():
            parts = line.split()
            if parts[0] in ("strip", "install"):
                rows.append((parts[0], parts[-1], 0.0))
            elif parts[0] == "argv":
                self.argv[parts[1]] = json.loads(line.split(" ", 2)[2])
            elif parts[0] == "env":
                self.env[parts[1]] = json.loads(line.split(" ", 2)[2])
            elif parts[0] == "finalizer-argv":
                self.finalizer_argv.append(json.loads(line.split(" ", 1)[1]))
            else:
                rows.append((parts[0], parts[1], float(parts[2])))
                self.target_dirs[parts[1]] = parts[3]
        return rows

    def assert_finalized_before_publication(self, events: list[tuple[str, str, float]]) -> None:
        names = [(event, label) for event, label, _ in events]
        target = str(self.stage.parent.parent.parent / "target")
        self.assertEqual(self.finalizer_argv, [["--source-copy", target]])
        self.assertEqual(names.count(("start", "finalizer")), 1, names)
        self.assertEqual(names.count(("end", "finalizer")), 1, names)
        for build in ("release", "hermit", "detcore-liteinst", "detcore-dbt+detcore-dbt-cdylib",
                      "hermit-install+detcore-sabre+detcore-liteinst"):
            self.assertLess(names.index(("end", build)), names.index(("start", "finalizer")), names)
        publication = [i for i, (event, _) in enumerate(names) if event in ("install", "strip")]
        self.assertTrue(publication, names)
        self.assertLess(names.index(("end", "finalizer")), publication[0], names)

    def test_the_background_builds_overlap_the_validate_builds_and_finish_before_strip(self) -> None:
        # Each background build outlasts detcore-dbt and the install bundle (0.5 s each),
        # so strip waits for all three.
        proc = self.run_stage(FAKE_CARGO_SECONDS_release="2.5", FAKE_CARGO_SECONDS_hermit="2.0",
                              FAKE_CARGO_SECONDS_detcore_liteinst="2.0")
        events = self.events()
        self.assert_finalized_before_publication(events)
        names = [(event, label) for event, label, _ in events]
        times = {(event, label): t for event, label, t in events if event != "strip"}
        first_validate = "detcore-dbt+detcore-dbt-cdylib"
        strips = [i for i, (event, _) in enumerate(names) if event == "strip"]
        self.assertTrue(strips, f"stage never reached strip: {proc.stderr}")
        for background in ("release", "hermit", "detcore-liteinst"):
            self.assertLess(times[("start", background)], times[("end", first_validate)], events)
            self.assertGreater(times[("end", background)], times[("start", first_validate)], events)
            self.assertLess(names.index(("end", background)), strips[0], events)
            self.assertIn(f"fake cargo {background} output", proc.stderr)
        target = str(self.stage.parent.parent.parent / "target")
        self.assertEqual(self.target_dirs["release"], target)
        self.assertEqual(self.target_dirs["detcore-dbt+detcore-dbt-cdylib"], target)
        self.assertEqual(self.target_dirs["hermit-install+detcore-sabre+detcore-liteinst"], target)
        self.assertEqual(self.target_dirs["detcore-liteinst"], target)
        self.assertEqual(self.target_dirs["hermit"], target + "/stage-hermit")
        self.assertEqual((Path(target) / "validate" / "hermit").read_text(),
                         f"fake hermit built in {target}/stage-hermit\n")
        self.assertLess(names.index(("end", "hermit")), names.index(("install", f"{target}/validate/hermit")), events)
        # Each build is main's command; only the hermit build's target directory moved.
        # The liteinst-runtime build is the packaging finalizer's own command, without
        # --offline (the stage's other builds still fetch the git dependencies).
        manifest = str(self.stage.parent.parent.parent / "Cargo.toml")
        common = ["build", "--manifest-path", manifest, "--locked"]
        self.assertEqual(self.argv, {
            "release": common + ["--release", "-p", "hermit-manifest-plan", "--bins", "--target-dir", target],
            "hermit": common + ["--profile", "validate", "-p", "hermit", "--bin", "hermit",
                                "--features", "hermit/third-party-backends", "--target-dir", target + "/stage-hermit"],
            "detcore-dbt+detcore-dbt-cdylib": common + ["--profile", "validate", "-p", "detcore-dbt", "-p", "detcore-dbt-cdylib",
                                                "--target-dir", target],
            "hermit-install+detcore-sabre+detcore-liteinst": common + [
                "--profile", "validate", "-p", "hermit-install", "-p", "detcore-sabre", "-p", "detcore-liteinst",
                "--target-dir", target],
            "detcore-liteinst": common + ["-p", "detcore-liteinst", "--lib", "--profile", "liteinst-runtime",
                                          "--target-dir", target],
        })

    def test_the_hermit_binary_is_copied_only_after_its_build_finishes_last(self) -> None:
        # In practice the hermit build (about 120 s) finishes long after the release build
        # (about 55 s); the copy must wait for it, not for the release build.
        proc = self.run_stage(FAKE_CARGO_SECONDS_release="0.5", FAKE_CARGO_SECONDS_hermit="3.0")
        events = self.events()
        self.assert_finalized_before_publication(events)
        names = [(event, label) for event, label, _ in events]
        target = str(self.stage.parent.parent.parent / "target")
        copy = ("install", f"{target}/validate/hermit")
        self.assertIn(copy, names, proc.stderr)
        self.assertLess(names.index(("end", "release")), names.index(("end", "hermit")), names)
        self.assertLess(names.index(("end", "hermit")), names.index(copy), names)
        self.assertEqual((Path(target) / "validate" / "hermit").read_text(),
                         f"fake hermit built in {target}/stage-hermit\n")

    def test_a_missing_target_directory_cannot_be_replaced_under_the_hermit_build(self) -> None:
        # Reproducible staging starts with no target directory. Forced into the bad
        # interleaving, the release build would rename its temporary directory onto the
        # empty target/ the hermit build had just made as its target directory's parent.
        target = self.stage.parent.parent.parent / "target"
        self.assertFalse(target.exists())
        proc = self.run_stage(FAKE_CARGO_RACE_TARGET="1")
        # (Stage itself stops later, at the install bundle the fake cargo does not make.)
        self.assertNotIn("the validate-profile build of hermit failed", proc.stderr)
        self.assertNotIn("No such file or directory (os error 2)", proc.stderr)
        events = self.events()
        self.assert_finalized_before_publication(events)
        names = [(event, label) for event, label, _ in events]
        self.assertNotIn(("checked", "release"), names, "the release build found target/ missing")
        self.assertEqual((target / "validate" / "hermit").read_text(),
                         f"fake hermit built in {target}/stage-hermit\n")

    def test_a_failed_finalizer_stops_stage_with_its_status_before_publication(self) -> None:
        proc = self.run_stage(FAKE_FINALIZER_FAIL_STATUS="23")
        self.assertEqual(proc.returncode, 23, proc.stderr)
        self.assertIn("synthetic finalizer failure (exit 23)", proc.stderr)
        names = [(event, label) for event, label, _ in self.events()]
        target = self.stage.parent.parent.parent / "target"
        self.assertEqual(self.finalizer_argv, [["--source-copy", str(target)]])
        self.assertEqual(names.count(("start", "finalizer")), 1, names)
        self.assertEqual(names.count(("fail", "finalizer")), 1, names)
        self.assertNotIn(("end", "finalizer"), names)
        for build in ("release", "hermit", "detcore-liteinst", "detcore-dbt+detcore-dbt-cdylib",
                      "hermit-install+detcore-sabre+detcore-liteinst"):
            self.assertLess(names.index(("end", build)), names.index(("start", "finalizer")), names)
        self.assertFalse(any(event in ("strip", "install") for event, _ in names), names)
        self.assertFalse((target / "validate" / "hermit").exists())
        for part in ("bin", "hermit", "install"):
            self.assertFalse((self.stage.parent / "staged" / part).exists(), part)

    def test_the_fake_cargo_reproduces_the_replaced_target_directory(self) -> None:
        # Without the directories stage makes first, the same interleaving fails the
        # hermit build exactly as validation saw it, so the test above is not vacuous.
        target = self.tmp / "bare-target"
        env = {"PATH": str(self.bin), "FAKE_CARGO_LOG": str(self.log), "FAKE_CARGO_RACE_TARGET": "1"}
        cargo = [str(self.bin / "cargo"), "build"]
        release = subprocess.Popen(cargo + ["--release", "--target-dir", str(target)], env=env,
                                   stderr=subprocess.PIPE, text=True)
        hermit = subprocess.run(cargo + ["--profile", "validate", "-p", "hermit", "--target-dir",
                                         f"{target}/stage-hermit"], env=env, capture_output=True, text=True,
                                timeout=60)
        release.communicate(timeout=60)
        self.assertEqual(hermit.returncode, 101, hermit.stderr)
        self.assertIn("No such file or directory (os error 2) at path", hermit.stderr)
        self.assertEqual(release.returncode, 0)

    def test_a_failed_release_build_stops_stage_with_its_status_and_log(self) -> None:
        proc = self.run_stage(FAKE_CARGO_FAIL="release", FAKE_CARGO_FAIL_STATUS="7")
        self.assertEqual(proc.returncode, 7, proc.stderr)
        self.assertIn("the release build of hermit-manifest-plan failed (exit 7)", proc.stderr)
        self.assertIn("fake cargo release output", proc.stderr)
        self.assertNotIn("strip", [event for event, _, _ in self.events()])

    def test_a_failed_hermit_build_stops_stage_with_its_status_and_log(self) -> None:
        proc = self.run_stage(FAKE_CARGO_FAIL="hermit", FAKE_CARGO_FAIL_STATUS="5")
        self.assertEqual(proc.returncode, 5, proc.stderr)
        self.assertIn("the validate-profile build of hermit failed (exit 5)", proc.stderr)
        self.assertIn("fake cargo hermit output", proc.stderr)
        self.assertNotIn("strip", [event for event, _, _ in self.events()])

    def test_a_failed_validate_build_stops_every_running_background_build(self) -> None:
        children = self.tmp / "children"
        proc = self.run_stage(FAKE_CARGO_FAIL="detcore-dbt+detcore-dbt-cdylib",
                              FAKE_CARGO_FAIL_AFTER_START="hermit,release,detcore-liteinst",
                              FAKE_CARGO_SECONDS_release="30", FAKE_CARGO_SECONDS_hermit="30",
                              FAKE_CARGO_SECONDS_detcore_liteinst="30",
                              FAKE_CARGO_CHILDREN=str(children))
        self.assertEqual(proc.returncode, 3, proc.stderr)
        names = [(event, label) for event, label, _ in self.events()]
        for background in ("release", "hermit", "detcore-liteinst"):
            self.assertIn(("killed", background), names)
            self.assertNotIn(("end", background), names)
        # The background builds' own children (rustc, build scripts) are stopped too,
        # before stage returns. (The failed foreground build's child is the fake's own
        # leftover; real cargo waits for its children before it exits.)
        pids = dict(line.split() for line in children.read_text().splitlines())
        for pid in pids.values():
            self.addCleanup(self.kill_quietly, int(pid))
        self.assertEqual(sorted(pids), ["detcore-dbt+detcore-dbt-cdylib", "detcore-liteinst", "hermit", "release"])
        for background in ("release", "hermit", "detcore-liteinst"):
            self.assertFalse(self.running(int(pids[background])),
                             f"the {background} build's child outlived stage")
            self.assertIn(("child-term", background), names, "the child was not sent SIGTERM with its build")

    @staticmethod
    def running(pid: int) -> bool:
        try:
            return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
        except FileNotFoundError:
            return False

    @staticmethod
    def kill_quietly(pid: int) -> None:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass


class StageReproducibleTest(unittest.TestCase):
    """The reproducible-staging wrapper, in StageCargoOverlapTest's committed scratch checkout."""

    events = StageCargoOverlapTest.events

    def setUp(self) -> None:
        StageCargoOverlapTest.setUp(self)
        for name in WRAPPER_COMMANDS:
            real = shutil.which(name)
            self.assertIsNotNone(real, name)
            (self.bin / name).symlink_to(real)
        self.tree = self.stage.parent.parent.parent
        self.fixed = self.tmp / ".cache" / "hermit-buck-stage"
        self.cargo_home = self.tmp / "cargo-home"
        self.cargo_home.mkdir()
        self.sentinel = self.stage.parent / "staged" / "SENTINEL"
        self.sentinel.parent.mkdir()
        self.sentinel.write_text("previous staging\n")

    def run_stage(self, **env: str) -> subprocess.CompletedProcess:
        base = {"PATH": str(self.bin), "HOME": str(self.tmp), "HERMIT_STAGE_HOST_LIBDIR": str(self.libdir),
                "FAKE_CARGO_LOG": str(self.log), "CARGO_HOME": str(self.cargo_home)}
        base.update(env)
        return subprocess.run(["/bin/bash", str(self.stage), "--from-cargo"], env=base, capture_output=True,
                              text=True, timeout=60)

    def assert_cleaned_up_and_previous_staging_kept(self, proc: subprocess.CompletedProcess) -> None:
        for leftover in (self.fixed / "src", self.fixed / "cargo", Path(f"{self.fixed}.holder")):
            self.assertFalse(leftover.exists() or leftover.is_symlink(), f"{leftover} left behind: {proc.stderr}")
        self.assertEqual(self.sentinel.read_text(), "previous staging\n", proc.stderr)

    def test_builds_run_in_the_fixed_copy_with_a_fixed_cargo_home_and_epoch(self) -> None:
        proc = self.run_stage(FAKE_CARGO_FAIL="detcore-dbt+detcore-dbt-cdylib",
                              FAKE_CARGO_FAIL_AFTER_START="hermit,release,detcore-liteinst")
        self.assertEqual(proc.returncode, 3, proc.stderr)
        self.events()
        target = str(self.fixed / "src" / "target")
        self.assertEqual(self.target_dirs["release"], target)
        self.assertEqual(self.target_dirs["detcore-liteinst"], target)
        self.assertEqual(self.target_dirs["detcore-dbt+detcore-dbt-cdylib"], target)
        self.assertEqual(self.target_dirs["hermit"], target + "/stage-hermit")
        epoch = subprocess.run(["git", "-C", str(self.tree), "log", "-1", "--format=%ct", "HEAD"],
                               capture_output=True, text=True, check=True).stdout.strip()
        self.assertEqual(sorted(self.env), ["detcore-dbt+detcore-dbt-cdylib", "detcore-liteinst", "hermit", "release"])
        for label, env in self.env.items():
            self.assertEqual(env, {"CARGO_HOME": str(self.fixed / "cargo"),
                                   "CARGO_HOME_TARGET": str(self.cargo_home.resolve()),
                                   "SOURCE_DATE_EPOCH": epoch}, label)
        self.assert_cleaned_up_and_previous_staging_kept(proc)

    def test_a_failed_build_returns_its_status_and_keeps_the_previous_staging(self) -> None:
        proc = self.run_stage(FAKE_CARGO_FAIL="release", FAKE_CARGO_FAIL_STATUS="7")
        self.assertEqual(proc.returncode, 7, proc.stderr)
        self.assertIn("the release build of hermit-manifest-plan failed (exit 7)", proc.stderr)
        self.assert_cleaned_up_and_previous_staging_kept(proc)

    def test_a_stage_that_cannot_take_the_lock_fails_naming_the_holder(self) -> None:
        import fcntl
        self.fixed.mkdir(parents=True)
        holder = Path(f"{self.fixed}.holder")
        holder.write_text("pid 4242 since 2026-10-08T00:00:00Z for /elsewhere\n")
        with open(f"{self.fixed}.lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            proc = self.run_stage(HERMIT_BUCK_STAGE_LOCK_TIMEOUT="1")
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn(f"timed out after 1s waiting for {self.fixed}.lock", proc.stderr)
        self.assertIn("pid 4242 since 2026-10-08T00:00:00Z for /elsewhere", proc.stderr)
        self.assertFalse(self.log.exists(), "a build ran without the lock")
        self.assertEqual(holder.read_text(), "pid 4242 since 2026-10-08T00:00:00Z for /elsewhere\n")
        self.assertEqual(self.sentinel.read_text(), "previous staging\n")

    def test_the_lock_timeout_default_leaves_the_node_time_to_name_the_holder(self) -> None:
        # A stage that waits out the lock must still be alive to print the holder,
        # so the default wait plus one stage stays inside e2e.buck_stage's wall
        # timeout, and the waiter fails naming the holder rather than being killed.
        import re
        defaults = set(re.findall(r"HERMIT_BUCK_STAGE_LOCK_TIMEOUT:-(\d+)", STAGE.read_text()))
        self.assertEqual(len(defaults), 1, defaults)
        default = int(defaults.pop())
        dag = json.loads((STAGE.parent.parent / "dag" / "validate.json").read_text())
        (node,) = [s for s in dag["steps"] if (s["group"], s["job"]) == ("e2e", "buck_stage")]
        self.assertLess(default + node["hint"]["est_duration_s"], node["timeout"], (default, node))

    def test_a_missing_host_package_is_refused_before_the_lock(self) -> None:
        (self.bin / "patchelf").unlink()
        proc = self.run_stage()
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("ci/buck-e2e/stage: patchelf is missing", proc.stderr)
        self.assertFalse(Path(f"{self.fixed}.lock").exists(), proc.stderr)

    def test_an_uncommitted_change_is_refused_even_when_the_copy_cannot_read_a_submodule(self) -> None:
        # A submodule whose .git file names its gitdir relatively, as agent-utils does:
        # valid in the checkout, dangling from the fixed directory's copy.
        git = ["git", "-c", "user.email=t@example.invalid", "-c", "user.name=t", "-c", "commit.gpgsign=false"]
        sub = self.tree / "sub"
        sub.mkdir()
        (sub / "file").write_text("sub\n")
        subprocess.run(git + ["init", "-q", f"--separate-git-dir={self.tmp / 'sub-gitdir'}", str(sub)], check=True)
        (sub / ".git").write_text("gitdir: ../../sub-gitdir\n")
        subprocess.run(git + ["-C", str(sub), "add", "file"], check=True)
        subprocess.run(git + ["-C", str(sub), "commit", "-qm", "sub"], check=True)
        subprocess.run(git + ["-C", str(self.tree), "add", "sub"], check=True)
        subprocess.run(git + ["-C", str(self.tree), "commit", "-qm", "add sub"], check=True)
        (self.tree / ".gitignore").write_text("ci/buck-e2e/staged/\nuncommitted\n")
        proc = self.run_stage()
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("ci/buck-e2e/stage: commit first", proc.stderr)
        self.assertFalse(self.log.exists(), "a build ran from an uncommitted tree")
        self.assert_cleaned_up_and_previous_staging_kept(proc)

    def test_a_status_that_git_cannot_read_is_refused_not_taken_as_clean(self) -> None:
        # The same submodule, its gitdir then removed: git status fails in the checkout too.
        git = ["git", "-c", "user.email=t@example.invalid", "-c", "user.name=t", "-c", "commit.gpgsign=false"]
        sub = self.tree / "sub"
        sub.mkdir()
        (sub / "file").write_text("sub\n")
        subprocess.run(git + ["init", "-q", f"--separate-git-dir={self.tmp / 'sub-gitdir'}", str(sub)], check=True)
        subprocess.run(git + ["-C", str(sub), "add", "file"], check=True)
        subprocess.run(git + ["-C", str(sub), "commit", "-qm", "sub"], check=True)
        subprocess.run(git + ["-C", str(self.tree), "add", "sub"], check=True)
        subprocess.run(git + ["-C", str(self.tree), "commit", "-qm", "add sub"], check=True)
        shutil.rmtree(self.tmp / "sub-gitdir")
        proc = self.run_stage()
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn(f"ci/buck-e2e/stage: cannot read the status of {self.tree}", proc.stderr)
        self.assertFalse(self.log.exists(), "a build ran without a readable status")
        self.assert_cleaned_up_and_previous_staging_kept(proc)


if __name__ == "__main__":
    unittest.main()
