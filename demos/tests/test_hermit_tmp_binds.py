#!/usr/bin/env python3
"""Tests for demos/lib/hermit-tmp-binds.sh and demo 9's use of it.

`hermit run` gives the program it runs a private, empty /tmp, and Hermit
refuses to start a program under the host's /tmp. A demo checked out under
/tmp therefore passes `--bind` for what its program needs: the program's
directory, and each other file by itself. The helper decides which paths to
bind, which to refuse with an explanation, and which to leave alone; a
checkout outside /tmp must get no --bind at all, so its command line is the one
it had before the helper existed.

HelperTest runs the helper's functions in bash. Demo9TmpBindTest runs a copy
of demo 9's run.sh with a stub `hermit` first on PATH that records its
arguments; no real Hermit, QEMU, kernel, or network is involved.
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
HELPER = DEMO_DIR / "lib" / "hermit-tmp-binds.sh"
DEMO9 = DEMO_DIR / "09-qemu-busybox"

# The line the guest's /init prints once every stage of its workload succeeded.
MARKER = "HERMIT-QEMU-BUSYBOX-PASS stages=5"

# Stands in for `hermit ... run ... -- boot_qemu.sh KERNEL INITRAMFS QEMU`.
# run.sh copies the run's standard output to console.log through a `tee` that
# it does not wait for, and reads console.log as soon as the run exits, so the
# stub waits until the marker has reached console.log before it exits.
DEMO9_STUB = r"""#!/usr/bin/env bash
if [ "${1:-}" = --version ]; then
  echo 'hermit 0.2.0 (2026-09-30, g0123456789ab)'
  exit 0
fi
printf '%s\n' "$*" >>"$DEMO09_TEST_ARGS_FILE"
echo 'HERMIT-QEMU-BUSYBOX-PASS stages=5'
for _ in $(seq 1 200); do
  if grep -Fq 'HERMIT-QEMU-BUSYBOX-PASS stages=5' "$DEMO09_TEST_CONSOLE" 2>/dev/null; then
    exit 0
  fi
  sleep 0.05
done
echo "stub: the marker never reached $DEMO09_TEST_CONSOLE" >&2
exit 0
"""

# Stands in for a command that must not run: it records that it ran and fails.
RECORDING_FAILURE = """#!/usr/bin/env bash
printf '%s %s\\n' "$(basename "$0")" "$*" >>"$DEMO09_TEST_RAN_FILE"
exit 1
"""

# The settings demo 9's run.sh reads; the tests set the ones they need.
DEMO9_SETTINGS = (
    "DEMO_TIMEOUT_SECONDS",
    "INITRAMFS_IMAGE",
    "KERNEL_IMAGE",
    "OUTPUT_DIR",
    "QEMU_BIN",
    "QEMU_FETCH_CONNECT_TIMEOUT",
    "QEMU_FETCH_PROBE_TIMEOUT",
    "QEMU_KERNEL_SHA256",
    "QEMU_KERNEL_URL",
    "SKID_MARGIN",
    "VERIFY",
)

REFUSAL = (
    "error: {} is under /tmp. Hermit gives the program it runs a private /tmp, "
    "and the demo cannot make this path visible there: {}. "
)

# The advice demo 9 prints after REFUSAL.
DEMO9_TMP_HINT = (
    "Run demo 9 from a checkout outside /tmp or through a path that begins with "
    "/tmp/, or set OUTPUT_DIR, KERNEL_IMAGE, and INITRAMFS_IMAGE to paths outside "
    "/tmp or to absolute paths that begin with /tmp/."
)


def _write_executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


def _is_under_tmp(path):
    return path == "/tmp" or path.startswith("/tmp/")


def _temporary_root(test, under_tmp):
    """A new directory under /tmp, or outside it, removed after `test`.

    The parent is the system temporary directory when it is on the requested
    side of /tmp, so a run with TMPDIR set stays inside it; otherwise /tmp, or
    /var/tmp for a directory outside /tmp. Parents are resolved first, so a
    symbolic link in TMPDIR cannot put the directory on the wrong side. The
    test is skipped if neither parent is usable.
    """
    for parent in (tempfile.gettempdir(), "/tmp" if under_tmp else "/var/tmp"):
        resolved = os.path.realpath(parent)
        if _is_under_tmp(resolved) != under_tmp:
            continue
        if not os.access(resolved, os.W_OK | os.X_OK):
            continue
        holder = tempfile.TemporaryDirectory(dir=resolved, prefix="tmp-binds-test-")
        test.addCleanup(holder.cleanup)
        return Path(holder.name)
    test.skipTest(
        "no writable directory {} /tmp".format("under" if under_tmp else "outside")
    )


def _bash(script, *args, cwd=None):
    """Run `script` in bash with the helper sourced and `args` as $1, $2, ..."""
    return subprocess.run(
        ["bash", "-c", 'set -u; source "$0"; ' + script, str(HELPER), *map(str, args)],
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=60,
    )


class HelperTest(unittest.TestCase):
    """What hermit_tmp_bind_args binds and what hermit_tmp_check_paths refuses."""

    def setUp(self):
        self.inside = _temporary_root(self, under_tmp=True)
        self.outside = _temporary_root(self, under_tmp=False)

    def _binds(self, *paths, cwd=None):
        result = _bash(
            'binds=(); hermit_tmp_bind_args binds "$@"; echo "status=$?"; '
            'for a in "${binds[@]}"; do printf "%s\\n" "$a"; done',
            *paths,
            cwd=cwd,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        lines = result.stdout.splitlines()
        self.assertEqual(lines[0], "status=0")
        return lines[1:]

    def _check(self, *paths, cwd=None):
        return _bash('hermit_tmp_check_paths "the hint." "$@"', *paths, cwd=cwd)

    def _assert_accepted(self, *paths, cwd=None):
        result = self._check(*paths, cwd=cwd)
        self.assertEqual((result.returncode, result.stderr), (0, ""))

    def _assert_refused(self, paths, refused, problem, binds=(), cwd=None):
        """Only `refused` is reported, and `binds` are all that is bound."""
        result = self._check(*paths, cwd=cwd)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(
            result.stderr, REFUSAL.format(refused, problem) + "the hint.\n"
        )
        self.assertEqual(self._binds(*paths, cwd=cwd), list(binds))

    def test_paths_under_tmp_bind_the_program_directory_and_each_file(self):
        program = self.inside / "bin" / "prog"
        data, image = self.inside / "data", self.inside / "images" / "a.img"
        self._assert_accepted(program, data, image)
        self.assertEqual(
            self._binds(program, data, image),
            ["--bind", str(program.parent), "--bind", str(data), "--bind", str(image)],
        )

    def test_paths_outside_tmp_bind_nothing(self):
        program = self.outside / "bin" / "prog"
        self._assert_accepted(program, self.outside / "data", "relative/file")
        self.assertEqual(self._binds(program, self.outside / "data", "relative/file"), [])
        self.assertEqual(self._binds(), [])

    def test_a_file_inside_the_program_directory_is_not_bound_again(self):
        directory = self.inside / "d"
        files = (directory / "data", directory / "sub" / "x", self.inside / "dd" / "x")
        self.assertEqual(
            self._binds(directory / "prog", *files),
            # d/dd shares the prefix "d" but is not inside d.
            ["--bind", str(directory), "--bind", str(files[2])],
        )

    def test_a_program_directly_in_tmp_is_refused(self):
        for program in ("/tmp/prog", "/tmp/./prog", "/tmp//prog"):
            with self.subTest(program):
                self._assert_refused(
                    [program],
                    program,
                    "it is directly in /tmp, and binding its directory would show "
                    "the program all of /tmp",
                )

    def test_a_file_that_names_tmp_itself_is_refused(self):
        program = self.inside / "bin" / "prog"
        for path in ("/tmp/", "/tmp/.", "/tmp//"):
            with self.subTest(path):
                self._assert_refused(
                    [program, path],
                    path,
                    "it names /tmp itself, not a file below it",
                    binds=["--bind", str(program.parent)],
                )

    def test_a_colon_is_refused(self):
        program = self.inside / "bin" / "prog"
        path = self.inside / "a:b"
        self._assert_refused(
            [program, path],
            path,
            "it contains ':', which --bind reads as SOURCE:TARGET",
            binds=["--bind", str(program.parent)],
        )

    def test_a_dot_dot_component_is_refused(self):
        program = "{}/x/../bin/prog".format(self.inside)
        self._assert_refused([program], program, "it contains a '..' component")
        # A name that only contains two dots is not a '..' component.
        named = self.inside / "x..y" / "prog"
        self._assert_accepted(named)
        self.assertEqual(self._binds(named), ["--bind", str(named.parent)])

    def test_a_program_directory_that_is_a_symbolic_link_is_refused(self):
        (self.inside / "real").mkdir()
        (self.inside / "link").symlink_to(self.inside / "real")
        program = self.inside / "link" / "prog"
        self._assert_refused(
            [program],
            program,
            "its directory, {}, is a symbolic link, which Hermit would mount as a "
            "file".format(self.inside / "link"),
        )

    def test_an_absolute_path_into_tmp_through_a_symbolic_link_is_refused(self):
        (self.outside / "link").symlink_to(self.inside)
        program = self.outside / "link" / "bin" / "prog"
        self._assert_refused(
            [program],
            program,
            "it resolves to {}, under /tmp, but it does not begin with /tmp/".format(
                self.inside / "bin" / "prog"
            ),
        )

    def test_a_relative_program_under_tmp_is_refused(self):
        self._assert_refused(
            ["bin/prog"],
            "bin/prog",
            "Hermit starts a program by the absolute path it resolves on the host, "
            "{}, which is under /tmp".format(self.inside / "bin" / "prog"),
            cwd=self.inside,
        )

    def test_a_relative_file_under_tmp_is_left_to_the_working_directory(self):
        program = self.outside / "bin" / "prog"
        self._assert_accepted(program, "images/a.img", cwd=self.inside)
        self.assertEqual(self._binds(program, "images/a.img", cwd=self.inside), [])

    def test_a_relative_file_that_reaches_tmp_through_dot_dot_is_refused(self):
        program = self.outside / "bin" / "prog"
        relative = os.path.relpath(self.inside / "a.img", self.outside)
        self.assertIn("..", relative.split("/"))
        # The lookup climbs to / and steps into tmp, where the program sees
        # its private /tmp.
        self._assert_refused(
            [program, relative],
            relative,
            "it resolves to {}, under /tmp, through /tmp itself, where the program "
            "sees its private /tmp".format(self.inside / "a.img"),
            cwd=self.outside,
        )

    def test_a_relative_file_whose_dot_dot_stays_below_tmp_is_left_alone(self):
        # The program looks these up from the working directory it inherits,
        # and none of them leaves the host's files below /tmp.
        program = self.outside / "bin" / "prog"
        (self.inside / "sub" / "deeper").mkdir(parents=True)
        (self.inside / "w").mkdir()
        (self.inside / "a.img").write_bytes(b"image\n")
        for cwd, relative in (
            (self.inside, "sub/../a.img"),
            (self.inside, "sub/deeper/../../a.img"),
            (self.inside, "./sub/./../a.img"),
            (self.inside / "w", "../a.img"),
            (self.inside / "w", "../w/../a.img"),
        ):
            with self.subTest(cwd=cwd, relative=relative):
                self._assert_accepted(program, relative, cwd=cwd)
                self.assertEqual(self._binds(program, relative, cwd=cwd), [])

    def test_a_relative_file_whose_lookup_passes_through_tmp_itself_is_refused(self):
        program = self.outside / "bin" / "prog"
        below_tmp = os.path.relpath(self.inside, "/tmp")
        problem = (
            "it resolves to {}, under /tmp, through /tmp itself, where the program "
            "sees its private /tmp".format(self.inside / "a.img")
        )
        # Climbing to /tmp from below it, and stepping into tmp from /.
        climb = "../" * len(Path(below_tmp).parts) + below_tmp + "/a.img"
        for cwd, relative in (
            (self.inside, climb),
            ("/", "tmp/{}/a.img".format(below_tmp)),
        ):
            with self.subTest(cwd=cwd, relative=relative):
                self._assert_refused(
                    [program, relative], relative, problem, cwd=cwd
                )

    def test_a_relative_file_through_a_symbolic_link_and_dot_dot_is_refused(self):
        # The program resolves the absolute link in its own view, where it
        # points into the private /tmp, before the `..` that follows it.
        program = self.outside / "bin" / "prog"
        (self.inside / "real").mkdir()
        (self.inside / "link").symlink_to(self.inside / "real")
        self._assert_refused(
            [program, "link/../a.img"],
            "link/../a.img",
            "it resolves to {}, under /tmp, through a symbolic link".format(
                self.inside / "a.img"
            ),
            cwd=self.inside,
        )

    def test_a_file_inside_the_program_directory_through_a_link_is_refused(self):
        directory = self.inside / "d"
        directory.mkdir()
        (self.inside / "kernels").mkdir()
        kernel = self.inside / "kernels" / "bzImage"
        kernel.write_bytes(b"kernel\n")
        (directory / "kernel").symlink_to(kernel)
        (directory / "relative-kernel").symlink_to("../kernels/bzImage")
        (directory / "sub").symlink_to(self.inside / "kernels")
        for path, link in (
            (directory / "kernel", directory / "kernel"),
            (directory / "relative-kernel", directory / "relative-kernel"),
            (directory / "sub" / "bzImage", directory / "sub"),
        ):
            with self.subTest(path=path):
                self._assert_refused(
                    [directory / "prog", path],
                    path,
                    "it is inside the program's directory, {}, which is bound as a "
                    "whole, and {} is a symbolic link, which the program would "
                    "follow in its own view, where the rest of the host's /tmp is "
                    "hidden; give the path it resolves to, {}, instead".format(
                        directory, link, kernel
                    ),
                    binds=["--bind", str(directory)],
                )

    def test_a_link_inside_the_program_directory_is_refused_even_if_it_works(self):
        # A link out of /tmp, or to another file in the directory, would
        # resolve in the program's view too; the helper refuses every link
        # below the directory rather than follow each one.
        directory = self.inside / "d"
        directory.mkdir()
        (directory / "data").write_bytes(b"data\n")
        (self.outside / "bzImage").write_bytes(b"kernel\n")
        (directory / "out").symlink_to(self.outside / "bzImage")
        (directory / "alias").symlink_to("data")
        for link, target in (
            (directory / "out", self.outside / "bzImage"),
            (directory / "alias", directory / "data"),
        ):
            with self.subTest(link=link):
                self._assert_refused(
                    [directory / "prog", link],
                    link,
                    "it is inside the program's directory, {}, which is bound as a "
                    "whole, and {} is a symbolic link, which the program would "
                    "follow in its own view, where the rest of the host's /tmp is "
                    "hidden; give the path it resolves to, {}, instead".format(
                        directory, link, target
                    ),
                    binds=["--bind", str(directory)],
                )
                # The path it resolves to is accepted.
                self._assert_accepted(directory / "prog", target)

    def test_a_program_that_is_a_symbolic_link_is_refused(self):
        directory = self.inside / "d"
        directory.mkdir()
        (self.inside / "elsewhere").mkdir()
        target = self.inside / "elsewhere" / "prog"
        _write_executable(target, "#!/bin/sh\n")
        (directory / "prog").symlink_to(target)
        self._assert_refused(
            [directory / "prog"],
            directory / "prog",
            "it is a symbolic link, which Hermit would follow in the program's "
            "view, where the rest of the host's /tmp is hidden; give the path it "
            "resolves to, {}, instead".format(target),
        )

    def test_real_files_inside_the_program_directory_are_accepted(self):
        """Positive control: regular files and directories, bound once."""
        directory = self.inside / "d"
        (directory / "sub").mkdir(parents=True)
        _write_executable(directory / "prog", "#!/bin/sh\n")
        (directory / "data").write_bytes(b"data\n")
        (directory / "sub" / "x").write_bytes(b"x\n")
        paths = (directory / "prog", directory / "data", directory / "sub" / "x")
        self._assert_accepted(*paths)
        self.assertEqual(self._binds(*paths), ["--bind", str(directory)])

    def test_a_link_above_the_program_directory_is_accepted(self):
        """Positive control: Hermit resolves it on the host when it binds."""
        (self.inside / "real" / "d").mkdir(parents=True)
        (self.inside / "alias").symlink_to(self.inside / "real")
        directory = self.inside / "alias" / "d"
        _write_executable(directory / "prog", "#!/bin/sh\n")
        (directory / "data").write_bytes(b"data\n")
        self._assert_accepted(directory / "prog", directory / "data")
        self.assertEqual(
            self._binds(directory / "prog", directory / "data"),
            ["--bind", str(directory)],
        )

    def test_a_relative_file_that_reaches_tmp_through_a_symbolic_link_is_refused(self):
        program = self.outside / "bin" / "prog"
        (self.outside / "link").symlink_to(self.inside)
        self._assert_refused(
            [program, "link/a.img"],
            "link/a.img",
            "it resolves to {}, under /tmp, through a symbolic link".format(
                self.inside / "a.img"
            ),
            cwd=self.outside,
        )

    def test_only_the_first_refused_path_is_reported(self):
        result = self._check(self.inside / "a:b" / "prog", self.inside / "c:d")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(len(result.stderr.splitlines()), 1, result.stderr)
        self.assertIn("a:b/prog is under /tmp", result.stderr)

    def test_is_under(self):
        (self.outside / "link").symlink_to(self.inside)
        for path, cwd, expected in (
            ("/tmp", None, 0),
            ("/tmp/", None, 0),
            ("/tmp/x/y", None, 0),
            (str(self.outside / "link" / "x"), None, 0),
            ("x", self.inside, 0),
            ("/tmpx", None, 1),
            ("/var/tmp/x", None, 1),
            (str(self.outside / "x"), None, 1),
            ("x", self.outside, 1),
        ):
            with self.subTest(path=path, cwd=cwd):
                result = _bash('hermit_tmp_is_under "$1"', path, cwd=cwd)
                self.assertEqual(result.returncode, expected, result.stderr)


class Demo9TmpBindTest(unittest.TestCase):
    """Demo 9 binds its launcher, kernel, and initramfs when they are in /tmp."""

    def setUp(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        # The stubs and their records. Their location is not on the command line.
        self.state = Path(holder.name)
        self.stub_dir = self.state / "bin"
        self.stub_dir.mkdir()
        _write_executable(self.stub_dir / "hermit", DEMO9_STUB)
        # Neither may run: the tests give demo 9 its kernel and initramfs.
        _write_executable(self.stub_dir / "curl", RECORDING_FAILURE)
        _write_executable(self.stub_dir / "with-proxy", RECORDING_FAILURE)
        true = shutil.which("true")
        self.assertIsNotNone(true)
        self.qemu = os.path.realpath(true)
        self.assertFalse(_is_under_tmp(self.qemu), self.qemu)

    def _checkout(self, under_tmp):
        """A copy of demo 9 and demos/lib at ROOT/demos, with its kernel."""
        root = _temporary_root(self, under_tmp)
        shutil.copytree(DEMO9, root / "demos" / "09-qemu-busybox")
        shutil.copytree(DEMO_DIR / "lib", root / "demos" / "lib")
        # Building the initramfs needs BusyBox; the tests provide one instead.
        _write_executable(
            root / "demos" / "09-qemu-busybox" / "build-initramfs.sh",
            RECORDING_FAILURE,
        )
        output = root / "target" / "qemu-busybox"
        output.mkdir(parents=True)
        (output / "bzImage").write_bytes(b"stand-in kernel\n")
        (output / "initramfs-busybox.cpio.gz").write_bytes(b"stand-in initramfs\n")
        return root

    def _run(self, root, cwd=None, **settings):
        environment = {
            key: value
            for key, value in os.environ.items()
            if key not in DEMO9_SETTINGS and not key.startswith("DEMO09_")
        }
        output = root / "target" / "qemu-busybox"
        environment.update(
            {
                "PATH": "{}:{}".format(self.stub_dir, environment.get("PATH", "")),
                "LC_ALL": "C",
                "QEMU_BIN": self.qemu,
                "KERNEL_IMAGE": str(output / "bzImage"),
                "INITRAMFS_IMAGE": str(output / "initramfs-busybox.cpio.gz"),
                "DEMO_TIMEOUT_SECONDS": "60",
                "DEMO09_TEST_ARGS_FILE": str(self.state / "hermit-args"),
                "DEMO09_TEST_CONSOLE": str(output / "console.log"),
                "DEMO09_TEST_RAN_FILE": str(self.state / "ran"),
            }
        )
        for key, value in settings.items():
            if value is None:
                environment.pop(key, None)
            else:
                environment[key] = value
        return subprocess.run(
            [str(root / "demos" / "09-qemu-busybox" / "run.sh")],
            cwd=cwd,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=120,
        )

    def _expected(self, root, binds, kernel=None):
        output = root / "target" / "qemu-busybox"
        launcher = root / "demos" / "09-qemu-busybox"
        initramfs = output / "initramfs-busybox.cpio.gz"
        if kernel is None:
            kernel = output / "bzImage"
            bound = (launcher, kernel, initramfs)
        else:
            # A relative kernel is reached through the working directory.
            bound = (launcher, initramfs)
        bind_args = (
            "".join("--bind {} ".format(path) for path in bound) if binds else ""
        )
        return [
            "--log info --log-file {}/hermit-info.log run --strict "
            "--epoch=2026-01-01T00:00:00Z --base-env=minimal {}"
            "-- {}/boot_qemu.sh {} {} {}".format(
                output, bind_args, launcher, kernel, initramfs, self.qemu
            )
        ]

    def _assert_passed_with(self, result, expected):
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("=== Demo 9: QEMU BusyBox Boot: SUCCESS ===", result.stdout)
        self.assertEqual(
            (self.state / "hermit-args").read_text().splitlines(), expected
        )
        self.assertFalse((self.state / "ran").exists())

    def _assert_stopped(self, result, message):
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(message, result.stdout)
        self.assertNotIn("SUCCESS ===", result.stdout)
        self.assertFalse((self.state / "hermit-args").exists(), result.stdout)
        # Stopped before the kernel download and the initramfs build.
        self.assertFalse((self.state / "ran").exists(), result.stdout)

    def test_a_checkout_under_tmp_binds_the_launcher_kernel_and_initramfs(self):
        root = self._checkout(under_tmp=True)
        self._assert_passed_with(self._run(root), self._expected(root, binds=True))

    def test_a_checkout_outside_tmp_keeps_its_command_line(self):
        """Positive control: no --bind at all, as before the binds existed."""
        root = self._checkout(under_tmp=False)
        self._assert_passed_with(self._run(root), self._expected(root, binds=False))

    def test_a_qemu_under_tmp_is_refused(self):
        root = self._checkout(under_tmp=True)
        qemu = root / "qemu" / "qemu-system-x86_64"
        qemu.parent.mkdir()
        _write_executable(qemu, "#!/bin/sh\nexit 0\n")
        self._assert_stopped(
            self._run(root, QEMU_BIN=str(qemu)),
            "error: QEMU {} is under /tmp, which Hermit hides from the program it "
            "runs, and QEMU also reads firmware from where it was built or "
            "installed; set QEMU_BIN to a QEMU installed outside /tmp".format(qemu),
        )

    def test_a_path_it_cannot_bind_stops_it_before_the_download_and_the_build(self):
        root = self._checkout(under_tmp=True)
        output = root / "out:put"
        self._assert_stopped(
            self._run(
                root, OUTPUT_DIR=str(output), KERNEL_IMAGE=None, INITRAMFS_IMAGE=None
            ),
            REFUSAL.format(
                "{}/bzImage".format(output),
                "it contains ':', which --bind reads as SOURCE:TARGET",
            )
            + "Run demo 9 from a checkout outside /tmp or through a path that begins "
            "with /tmp/, or set OUTPUT_DIR, KERNEL_IMAGE, and INITRAMFS_IMAGE to "
            "paths outside /tmp or to absolute paths that begin with /tmp/.",
        )
        # Nor is the output directory created.
        self.assertFalse(output.exists())

    def test_a_kernel_linked_from_the_launcher_directory_is_refused(self):
        # The launcher's directory is bound as a whole, so the program would
        # follow this link in its own view, where the kernel is hidden.
        root = self._checkout(under_tmp=True)
        launcher = root / "demos" / "09-qemu-busybox"
        kernel = root / "target" / "qemu-busybox" / "bzImage"
        link = launcher / "kernel"
        link.symlink_to(kernel)
        self._assert_stopped(
            self._run(root, KERNEL_IMAGE=str(link)),
            REFUSAL.format(
                link,
                "it is inside the program's directory, {}, which is bound as a "
                "whole, and {} is a symbolic link, which the program would follow "
                "in its own view, where the rest of the host's /tmp is hidden; give "
                "the path it resolves to, {}, instead".format(launcher, link, kernel),
            )
            + DEMO9_TMP_HINT,
        )

    def test_a_relative_kernel_with_dot_dot_under_tmp_is_used_in_place(self):
        root = self._checkout(under_tmp=True)
        (root / "target" / "qemu-busybox" / "sub").mkdir()
        relative = "target/qemu-busybox/sub/../bzImage"
        self._assert_passed_with(
            self._run(root, cwd=root, KERNEL_IMAGE=relative),
            self._expected(root, binds=True, kernel=relative),
        )


if __name__ == "__main__":
    unittest.main()
