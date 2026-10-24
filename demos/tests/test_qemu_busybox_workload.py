#!/usr/bin/env python3
"""Demo 9 passes only when every stage of its guest workload passed.

Demo 9 boots Linux under Hermit and QEMU from an initramfs whose /init runs a
fixed BusyBox workload and then prints the PASS line that run.sh requires.
These tests run that /init on the host with stand-in BusyBox applets, run
build-initramfs.sh with a stand-in BusyBox, and run run.sh with a stand-in
Hermit that prints a console transcript. They need no QEMU, no Hermit, and no
BusyBox. When the host has a BusyBox whose shell runs commands from PATH, its
shell runs /init too, as the guest's does.
"""

import gzip
import hashlib
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
DEMO9 = DEMO_DIR / "09-qemu-busybox"
INIT = DEMO9 / "init"
BUILDER = DEMO9 / "build-initramfs.sh"

# Every version of /init has printed a line that starts with this marker. The
# one from before /init checked its stages printed it whatever the workload did.
BARE_MARKER = "HERMIT-QEMU-BUSYBOX-PASS"
FAIL_PREFIX = "HERMIT-QEMU-BUSYBOX-FAIL"

# The commands /init runs, as commands_init_runs() reads them from /init. If
# /init starts running another command, add it here and to the applets that
# build-initramfs.sh requires.
EXPECTED_APPLETS = frozenset(
    {
        "bc",
        "head",
        "ls",
        "mknod",
        "mount",
        "poweroff",
        "printf",
        "sh",
        "sha256sum",
        "sort",
        "uname",
    }
)

# Where BusyBox installs each applet, for `busybox --list-full`, plus two that
# /init does not run.
APPLET_PATHS = {
    "bc": "usr/bin/bc",
    "cat": "bin/cat",
    "head": "usr/bin/head",
    "ls": "bin/ls",
    "mknod": "bin/mknod",
    "mount": "bin/mount",
    "poweroff": "sbin/poweroff",
    "printf": "usr/bin/printf",
    "sh": "bin/sh",
    "sha256sum": "usr/bin/sha256sum",
    "sort": "usr/bin/sort",
    "tr": "usr/bin/tr",
    "uname": "bin/uname",
}

# ---------------------------------------------------------------------------
# Reading the commands of a shell script

# Words at the start of a command that are not BusyBox applets: the shell
# builtins /init uses. The functions a script defines are added per script.
SHELL_BUILTINS = frozenset({"[", "echo", "exit", "set"})
# Reserved words after which a command still begins.
RESERVED_WORDS = frozenset(
    {"!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until"}
)
# Syntax the reader does not follow. A script that uses it fails the reading
# rather than having a command missed.
UNREAD_WORDS = frozenset({"case", "for", "function", "select"})
UNREAD_OPERATORS = frozenset({"<<", "<<-", ";;", ";&", ";;&"})
CONTROL_OPERATORS = frozenset({"|", "||", "&&", ";", "&", "("})
REDIRECTIONS = frozenset({"<", ">", ">>", "<>", ">|", "<&", ">&", "&>", "&>>"})
ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=")
FUNCTION_DEFINITION = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(\)", re.MULTILINE)


def commands_init_runs(text):
    """The commands a shell script runs, other than builtins and its functions.

    `busybox APPLET` counts as APPLET. The interpreter on the #! line counts
    too, because the kernel runs /init through it. The reader follows simple
    commands, pipelines, lists, subshells, braces, assignments, redirections,
    and comments, one line at a time; it raises ValueError on syntax it does
    not follow rather than miss a command.
    """
    lines = text.splitlines()
    commands = set()
    if lines and lines[0].startswith("#!"):
        commands.add(os.path.basename(lines[0][2:].split()[0]))
    functions = set(FUNCTION_DEFINITION.findall(text))
    for number, line in enumerate(lines, 1):
        lexer = shlex.shlex(line, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
        at_command = True
        index = 0
        while index < len(tokens):
            token = tokens[index]
            following = tokens[index + 1] if index + 1 < len(tokens) else None
            if (
                token in UNREAD_OPERATORS
                or "`" in token
                or "$(" in token
                or (token.endswith("$") and following == "(")
                or (at_command and token in UNREAD_WORDS)
            ):
                raise ValueError("line {}: unread shell syntax: {}".format(number, line))
            if token in CONTROL_OPERATORS:
                at_command = True
            elif token in (")", "()"):
                at_command = False
            elif token in REDIRECTIONS:
                index += 1  # the redirection's target
            elif token.isdigit() and following in REDIRECTIONS:
                pass  # the file descriptor of the redirection that follows
            elif not at_command or token in RESERVED_WORDS or ASSIGNMENT.match(token):
                pass
            else:
                at_command = False
                if token == "busybox":
                    if following is None:
                        raise ValueError("line {}: busybox without an applet".format(number))
                    commands.add(following)
                    index += 1
                elif token not in SHELL_BUILTINS and token not in functions:
                    commands.add(token)
            index += 1
    return commands


def init_pass_line(text):
    """The PASS line /init prints, read from its source."""
    lines = re.findall(r"^\s*echo '({}[^']*)'\s*$".format(BARE_MARKER), text, re.MULTILINE)
    if len(lines) != 1:
        raise ValueError("expected one PASS line in /init, found {!r}".format(lines))
    return lines[0]


def _write_executable(path, text, mode=0o755):
    path.write_text(text)
    path.chmod(mode)


def _newc_entries(archive):
    """Map each entry name of a newc cpio archive to its stored contents."""
    entries = {}
    offset = 0
    while True:
        header = archive[offset : offset + 110]
        if header[:6] != b"070701":
            raise ValueError("no newc header at offset {}".format(offset))
        fields = [int(header[6 + 8 * i : 14 + 8 * i], 16) for i in range(13)]
        file_size, name_size = fields[6], fields[11]
        name_start = offset + 110
        name = archive[name_start : name_start + name_size - 1].decode()
        offset = (name_start + name_size + 3) & ~3
        if name == "TRAILER!!!":
            return entries
        if name.startswith("./"):
            name = name[2:]
        entries[name] = archive[offset : offset + file_size]
        offset = (offset + file_size + 3) & ~3


class CommandReaderTest(unittest.TestCase):
    """The reader that the applet tests below rely on."""

    def test_the_reader_finds_every_command_of_init(self):
        self.assertEqual(EXPECTED_APPLETS, commands_init_runs(INIT.read_text()))

    def test_the_reader_skips_builtins_functions_redirections_and_assignments(self):
        script = (
            "#!/bin/sh\n"
            "# comment: ignored `here`\n"
            "helper() {\n"
            "  first 2>/dev/null || second\n"
            "}\n"
            "(set -o pipefail) 2>&1 | busybox tr a b\n"
            "NAME=value third <in >out && helper\n"
            "echo 'a;b|c' | busybox fourth -n 3 |\n"
            "  fifth\n"
        )
        self.assertEqual(
            {"sh", "first", "second", "tr", "third", "fourth", "fifth"},
            commands_init_runs(script),
        )

    def test_the_reader_refuses_syntax_it_does_not_follow(self):
        for script in (
            "for name in a b; do run $name; done\n",
            "case $1 in a) run ;; esac\n",
            "cat <<EOF\nrun\nEOF\n",
            "value=$(run)\n",
        ):
            with self.subTest(script=script):
                with self.assertRaises(ValueError):
                    commands_init_runs(script)


# ---------------------------------------------------------------------------
# build-initramfs.sh refuses a BusyBox that /init cannot use

# The script refuses a BusyBox that file(1) does not call statically linked.
FAKE_FILE = """#!/bin/sh
printf '%s: ELF 64-bit LSB executable, x86-64, statically linked\\n' "$1"
"""

# A stand-in BusyBox that answers what build-initramfs.sh asks it. @SH@ is its
# shell: it receives `-c SCRIPT`.
BUILD_BUSYBOX = """#!/bin/sh
case "${1:-}" in
  --list) printf '%s\\n' @NAMES@ ;;
  --list-full) printf '%s\\n' @PATHS@ ;;
  sh) shift; @SH@ ;;
  *) exit 1 ;;
esac
"""
# A shell that supports pipefail; bash is one.
SHELL_WITH_PIPEFAIL = 'exec bash "$@"'
# A shell that rejects `set -o pipefail`.
SHELL_WITHOUT_PIPEFAIL = "exit 2"
# A shell that accepts `set -o pipefail` and still reports a failed upstream
# stage as success.
SHELL_IGNORING_PIPEFAIL = "exit 0"


class BuilderRefusesUnusableBusyBoxTest(unittest.TestCase):
    """A BusyBox that /init cannot use is refused before anything is built."""

    def _build(self, applets, shell=SHELL_WITH_PIPEFAIL):
        """Run a copy of build-initramfs.sh; return its result and archive path."""
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        scratch = Path(holder.name)
        demo = scratch / "repo" / "demos" / "09-qemu-busybox"
        demo.mkdir(parents=True)
        shutil.copy2(BUILDER, demo / "build-initramfs.sh")
        shutil.copy2(INIT, demo / "init")
        fake_bin = scratch / "bin"
        fake_bin.mkdir()
        _write_executable(fake_bin / "file", FAKE_FILE)
        busybox = scratch / "busybox"
        _write_executable(
            busybox,
            BUILD_BUSYBOX.replace("@NAMES@", " ".join(sorted(applets)))
            .replace("@PATHS@", " ".join(APPLET_PATHS[name] for name in sorted(applets)))
            .replace("@SH@", shell),
        )
        archive = scratch / "out" / "initramfs-busybox.cpio.gz"
        environment = dict(os.environ)
        environment.update(
            PATH="{}:{}".format(fake_bin, environment.get("PATH", "/usr/bin:/bin")),
            BUSYBOX=str(busybox),
            LC_ALL="C",
        )
        result = subprocess.run(
            ["bash", str(demo / "build-initramfs.sh"), str(archive)],
            env=environment,
            cwd=scratch,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=120,
        )
        return result, archive

    def _assert_refused(self, result, archive, *words):
        self.assertNotEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertFalse(archive.exists(), "an archive was built despite the refusal")
        for word in words:
            self.assertRegex(result.stderr, r"\b{}\b".format(re.escape(word)))

    def test_every_applet_init_runs_is_required(self):
        for applet in sorted(commands_init_runs(INIT.read_text())):
            with self.subTest(missing=applet):
                result, archive = self._build(set(APPLET_PATHS) - {applet})
                self._assert_refused(result, archive, applet)

    def test_the_applets_of_the_pipeline_are_required(self):
        result, archive = self._build(set(APPLET_PATHS) - {"head", "printf", "sort"})
        self._assert_refused(result, archive, "head", "printf", "sort")

    def test_a_shell_without_pipefail_is_refused(self):
        result, archive = self._build(set(APPLET_PATHS), SHELL_WITHOUT_PIPEFAIL)
        self._assert_refused(result, archive, "pipefail")

    def test_a_shell_that_ignores_pipefail_is_refused(self):
        result, archive = self._build(set(APPLET_PATHS), SHELL_IGNORING_PIPEFAIL)
        self._assert_refused(result, archive, "pipefail")

    def test_a_complete_busybox_builds_an_archive_holding_this_init(self):
        """Positive control: the checks pass a BusyBox that has everything."""
        result, archive = self._build(set(APPLET_PATHS))
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        entries = _newc_entries(gzip.decompress(archive.read_bytes()))
        self.assertEqual(INIT.read_bytes(), entries["init"])


# ---------------------------------------------------------------------------
# /init checks every stage before it prints the PASS line

# Stands in for BusyBox when /init runs on the host. It records each call in
# $STUB_RECORD, emulates the applets /init runs, and exits 3 for the call that
# $STUB_FAIL names, or one that begins with it and a space.
STAND_IN_BUSYBOX = r'''#!@PYTHON@
import hashlib
import os
import sys

args = sys.argv[1:]
call = " ".join(args)
with open(os.environ["STUB_RECORD"], "a") as record:
    record.write("busybox " + call + "\n")
fail = os.environ.get("STUB_FAIL", "")
if fail and (call == fail or call.startswith(fail + " ")):
    sys.exit(3)


def output(text):
    try:
        sys.stdout.write(text)
        sys.stdout.flush()
    except BrokenPipeError:
        os._exit(141)  # as an applet killed by SIGPIPE


if args == ["uname", "-a"]:
    output("Linux (none) 6.12.0 #1 SMP x86_64 GNU/Linux\n")
elif args == ["ls", "-1", "/"]:
    output("bin\ndev\netc\ninit\nproc\nsys\n")
elif len(args) == 2 and args[0] == "printf":
    output(args[1].encode().decode("unicode_escape"))
elif args == ["sort"]:
    output("".join(sorted(sys.stdin.readlines())))
elif len(args) == 3 and args[:2] == ["head", "-n"]:
    output("".join(sys.stdin.readlines()[: int(args[2])]))
elif args == ["sha256sum"]:
    output(hashlib.sha256(sys.stdin.buffer.read()).hexdigest() + "  -\n")
elif len(args) == 2 and args[0] == "sha256sum":
    output("0" * 64 + "  " + args[1] + "\n")
elif args == ["bc", "-l"]:
    sys.stdin.read()
    output("3.1415926532\n")
else:
    sys.stderr.write("stand-in busybox: unexpected call: " + call + "\n")
    sys.exit(127)
'''

# Stands in for mount, mknod, and poweroff: it records the call and succeeds.
RECORDING_COMMAND = """#!/bin/sh
printf '%s %s\\n' "${0##*/}" "$*" >>"$STUB_RECORD"
"""

# The BusyBox calls of a workload in which every stage passes.
WORKLOAD_CALLS = (
    "uname -a",
    "ls -1 /",
    "printf delta\\nalpha\\ncharlie\\nbravo\\n",
    "sort",
    "head -n 3",
    "sha256sum",
    "bc -l",
    "sha256sum /bin/busybox",
)

# A call that fails, the stage /init must name, and calls of later stages,
# which must not run.
STAGE_FAILURES = (
    ("uname -a", "uname", ("ls -1 /", "sort", "bc -l", "sha256sum /bin/busybox")),
    ("ls -1 /", "ls", ("sort", "bc -l", "sha256sum /bin/busybox")),
    ("printf", "pipeline", ("bc -l", "sha256sum /bin/busybox")),
    ("sort", "pipeline", ("bc -l", "sha256sum /bin/busybox")),
    ("head -n 3", "pipeline", ("bc -l", "sha256sum /bin/busybox")),
    ("sha256sum", "pipeline", ("bc -l", "sha256sum /bin/busybox")),
    ("bc -l", "bc", ("sha256sum /bin/busybox",)),
    ("sha256sum /bin/busybox", "sha256sum", ()),
)

# bash running /init as a shell would that lacks `set -o pipefail`.
SHELL_WITHOUT_PIPEFAIL_RUNNING_INIT = (
    'set() { if [ "$1" = -o ] && [ "$2" = pipefail ]; then '
    'echo "set: pipefail: invalid option name" >&2; return 2; fi; '
    'builtin set "$@"; }; . "$0"'
)


class InitStagesTest(unittest.TestCase):
    """/init prints the PASS line only after every stage of its workload passed."""

    def _stand_ins(self):
        """A directory of stand-in applets, and the file they record calls in."""
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        directory = Path(holder.name)
        stand_ins = directory / "bin"
        stand_ins.mkdir()
        _write_executable(
            stand_ins / "busybox", STAND_IN_BUSYBOX.replace("@PYTHON@", sys.executable)
        )
        for name in ("mknod", "mount", "poweroff"):
            _write_executable(stand_ins / name, RECORDING_COMMAND)
        return stand_ins, directory / "record"

    def _run_init(self, shell, fail=""):
        """Run /init with `shell` and the stand-ins; return the result and calls."""
        stand_ins, record = self._stand_ins()
        result = subprocess.run(
            shell + [str(INIT)],
            env={
                "PATH": str(stand_ins),
                "STUB_RECORD": str(record),
                "STUB_FAIL": fail,
                "LC_ALL": "C",
            },
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        calls = record.read_text().splitlines() if record.exists() else []
        return result, calls

    def _busybox_shell_problem(self, busybox):
        """Why BusyBox's shell cannot run /init with the stand-ins, or None."""
        stand_ins, _ = self._stand_ins()
        names = ("busybox", "mknod", "mount", "poweroff")
        probe = subprocess.run(
            [
                busybox,
                "sh",
                "-c",
                "for name in {}; do command -v $name; done; "
                "(set -o pipefail) 2>/dev/null && echo pipefail".format(" ".join(names)),
            ],
            env={"PATH": str(stand_ins)},
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        expected = [str(stand_ins / name) for name in names] + ["pipefail"]
        if probe.stdout.splitlines() != expected:
            return (
                "{}'s shell does not run the stand-ins from PATH, or lacks "
                "pipefail: {!r}".format(busybox, probe.stdout)
            )
        return None

    def _shells(self):
        """(label, command, reason to skip or None) for each shell to run /init."""
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required")
        shells = [("bash", [bash], None)]
        busybox = os.environ.get("BUSYBOX") or shutil.which("busybox")
        if busybox is None:
            shells.append(("busybox sh", None, "this host has no BusyBox"))
        else:
            problem = self._busybox_shell_problem(busybox)
            shells.append(("busybox sh", [busybox, "sh"], problem))
        return shells

    def test_init_and_run_sh_expect_the_same_pass_line(self):
        """Positive control: run.sh requires the line /init prints."""
        markers = [
            quoted or bare
            for quoted, bare in re.findall(
                r"^marker=(?:'([^']*)'|(\S+))$",
                (DEMO9 / "run.sh").read_text(),
                re.MULTILINE,
            )
        ]
        self.assertEqual([init_pass_line(INIT.read_text())], markers)

    def test_every_stage_passing_ends_with_the_pass_line(self):
        """Positive control: the stand-ins let the whole workload pass."""
        pass_line = init_pass_line(INIT.read_text())
        pipeline_digest = hashlib.sha256(b"alpha\nbravo\ncharlie\n").hexdigest()
        for label, shell, problem in self._shells():
            with self.subTest(shell=label):
                if problem:
                    self.skipTest(problem)
                result, calls = self._run_init(shell)
                report = result.stdout + result.stderr
                self.assertEqual(0, result.returncode, report)
                lines = result.stdout.splitlines()
                self.assertEqual(pass_line, lines[-1], report)
                self.assertIn(pipeline_digest + "  -", lines, report)
                self.assertNotIn(FAIL_PREFIX, result.stdout)
                for call in WORKLOAD_CALLS:
                    self.assertIn("busybox " + call, calls)
                self.assertEqual("poweroff -f", calls[-1])

    def test_a_failing_stage_stops_the_workload_without_the_pass_line(self):
        for label, shell, problem in self._shells():
            for fail, stage, later in STAGE_FAILURES:
                with self.subTest(shell=label, failing=fail):
                    if problem:
                        self.skipTest(problem)
                    result, calls = self._run_init(shell, fail)
                    report = result.stdout + result.stderr
                    self.assertNotIn(BARE_MARKER, result.stdout)
                    self.assertEqual(
                        "{} stage={} status=3".format(FAIL_PREFIX, stage),
                        result.stdout.splitlines()[-1],
                        report,
                    )
                    self.assertEqual(1, result.returncode, report)
                    self.assertEqual("poweroff -f", calls[-1])
                    for call in later:
                        self.assertNotIn("busybox " + call, calls)

    def test_a_shell_without_pipefail_stops_before_the_workload(self):
        bash = shutil.which("bash")
        self.assertIsNotNone(bash, "bash is required")
        result, calls = self._run_init([bash, "-c", SHELL_WITHOUT_PIPEFAIL_RUNNING_INIT])
        report = result.stdout + result.stderr
        self.assertEqual(
            ["{} stage=pipefail status=1".format(FAIL_PREFIX)],
            result.stdout.splitlines(),
            report,
        )
        self.assertEqual(1, result.returncode, report)
        self.assertEqual("poweroff -f", calls[-1])
        self.assertEqual([], [call for call in calls if call.startswith("busybox ")])


# ---------------------------------------------------------------------------
# run.sh reads the console

# Stands in for `hermit ... run ... -- boot_qemu.sh ...`: it prints the console
# transcript. run.sh copies standard output to console.log through a `tee` it
# does not wait for, so the stand-in waits until console.log holds it all.
TRANSCRIPT_HERMIT = r"""#!/usr/bin/env bash
if [ "${1:-}" = --version ]; then
  echo 'hermit 0.4.0 (stand-in)'
  exit 0
fi
cat "$DEMO09_TEST_TRANSCRIPT"
for _ in $(seq 1 200); do
  if cmp -s "$DEMO09_TEST_TRANSCRIPT" "$DEMO09_TEST_CONSOLE"; then
    exit 0
  fi
  sleep 0.05
done
echo "stand-in: the transcript never reached $DEMO09_TEST_CONSOLE" >&2
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

# Console lines up to the pipeline stage. The serial console ends lines with
# CR LF.
CONSOLE_START = (
    "[    1.905816] Run /init as init process",
    "HERMIT-QEMU-BUSYBOX-START",
    "Linux (none) 6.12.0 #1 SMP x86_64 GNU/Linux",
    "--- root filesystem ---",
    "bin",
    "--- four-stage pipeline ---",
)
CONSOLE_WORKLOAD_REST = (
    "3eca7ea48b0da0ad30bee679c92c7b68d487547068b6914d10a64e8cedb03f51  -",
    "--- pi (bc -l, scale=10) ---",
    "3.1415926532",
    "--- busybox sha256 ---",
    "0000000000000000000000000000000000000000000000000000000000000000  /bin/busybox",
)
CONSOLE_POWER_DOWN = ("[    2.001234] reboot: Power down",)


class RunShReadsTheConsoleTest(unittest.TestCase):
    """run.sh passes on the PASS line alone, and names a failed stage."""

    def setUp(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.state = Path(holder.name)
        stand_ins = self.state / "bin"
        stand_ins.mkdir()
        _write_executable(stand_ins / "hermit", TRANSCRIPT_HERMIT)
        # Neither may run: the tests give demo 9 its kernel and initramfs.
        _write_executable(stand_ins / "curl", RECORDING_FAILURE)
        _write_executable(stand_ins / "with-proxy", RECORDING_FAILURE)
        self.stand_ins = stand_ins
        true = shutil.which("true")
        self.assertIsNotNone(true)
        self.qemu = os.path.realpath(true)
        self.root = self.state / "checkout"
        shutil.copytree(DEMO9, self.root / "demos" / "09-qemu-busybox")
        shutil.copytree(DEMO_DIR / "lib", self.root / "demos" / "lib")
        _write_executable(
            self.root / "demos" / "09-qemu-busybox" / "build-initramfs.sh",
            RECORDING_FAILURE,
        )
        self.output = self.root / "target" / "qemu-busybox"
        self.output.mkdir(parents=True)
        self.kernel = self.output / "bzImage"
        self.kernel.write_bytes(b"stand-in kernel\n")
        self.initramfs = self.output / "initramfs-busybox.cpio.gz"
        self.initramfs.write_bytes(b"stand-in initramfs\n")
        self.console = self.output / "console.log"

    def _run(self, *console_lines):
        transcript = self.state / "transcript"
        transcript.write_bytes(
            "".join(line + "\r\n" for line in console_lines).encode()
        )
        environment = {
            key: value
            for key, value in os.environ.items()
            if key not in DEMO9_SETTINGS and not key.startswith("DEMO09_")
        }
        environment.update(
            {
                "PATH": "{}:{}".format(self.stand_ins, environment.get("PATH", "")),
                "LC_ALL": "C",
                "QEMU_BIN": self.qemu,
                "KERNEL_IMAGE": str(self.kernel),
                "INITRAMFS_IMAGE": str(self.initramfs),
                "DEMO_TIMEOUT_SECONDS": "60",
                "DEMO09_TEST_TRANSCRIPT": str(transcript),
                "DEMO09_TEST_CONSOLE": str(self.console),
                "DEMO09_TEST_RAN_FILE": str(self.state / "ran"),
            }
        )
        result = subprocess.run(
            [str(self.root / "demos" / "09-qemu-busybox" / "run.sh")],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=120,
        )
        self.assertFalse((self.state / "ran").exists(), result.stdout)
        return result

    def test_a_failed_stage_fails_the_run_and_is_named(self):
        result = self._run(
            *CONSOLE_START,
            FAIL_PREFIX + " stage=pipeline status=1",
            *CONSOLE_POWER_DOWN,
        )
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn(
            "error: the guest workload failed ({} stage=pipeline status=1); "
            "inspect {}\n".format(FAIL_PREFIX, self.console),
            result.stdout,
        )
        self.assertNotIn("SUCCESS ===", result.stdout)

    def test_the_bare_marker_of_an_init_that_does_not_check_is_refused(self):
        result = self._run(
            *CONSOLE_START, *CONSOLE_WORKLOAD_REST, BARE_MARKER, *CONSOLE_POWER_DOWN
        )
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn("does not check its workload stages", result.stdout)
        self.assertIn("Rebuild {} with".format(self.initramfs), result.stdout)
        self.assertNotIn("SUCCESS ===", result.stdout)

    def test_the_pass_line_passes(self):
        """Positive control: the line /init prints after every stage passed."""
        result = self._run(
            *CONSOLE_START,
            *CONSOLE_WORKLOAD_REST,
            init_pass_line(INIT.read_text()),
            *CONSOLE_POWER_DOWN,
        )
        self.assertEqual(0, result.returncode, result.stdout)
        self.assertIn("=== Demo 9: QEMU BusyBox Boot: SUCCESS ===", result.stdout)


if __name__ == "__main__":
    unittest.main()
