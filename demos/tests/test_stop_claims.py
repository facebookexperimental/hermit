#!/usr/bin/env python3
"""No demo message, or passage that quotes or describes one, claims a stop.

Background. Demos 5 and 6 start the ``hermit`` command in a process group of
its own. When Hermit's INFO log grows past its cap (``QEMU_MAX_LOG_BYTES``),
when ``QEMU_TIMEOUT`` passes, or when Hermit's output is still open too long
after it exits, the shared library signals that group (``stop_process_group``
in ``demos/lib/demo_common.py``: SIGTERM, a wait of up to 10 seconds, SIGKILL,
another wait of up to 10 seconds) and then raises an error whose message the
demo prints. ``stop_process_group`` does not report whether the group emptied,
and its signals reach no process outside the group. Under ``bin/safehermit``,
Hermit, its tracer and QEMU run in a systemd user unit outside the group: on
2026-10-04 that unit was still active a minute after demo 6 printed its
FAILURE line, a line that then said the run "was stopped".

So these messages, and the docstrings and README passages that quote or
describe them, say what the demo did (it signalled the group) and never that
something stopped. Review finding R7-4 on
https://github.com/rrnewton/hermit/pull/3703.

Nothing in the demos observes the outcome after signalling. If a check that
the processes are gone is ever added, a message may report what that check
found; change this test in the same commit.
"""

import ast
import re
import runpy
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402

# "was stopped", "were stopped", "is stopped", "can no longer be stopped", ...
PASSIVE_CLAIM = re.compile(r"\b(?:is|are|was|were|been|be) stopped\b", re.IGNORECASE)
# "stops the run", "stop a resume", "stops Hermit", "stops them", "stopped it", ...
ACTIVE_CLAIM = re.compile(
    r"\bstop(?:s|ped)? (?:a |the )?(?:run|resume|boot|command|Hermit|them|it)\b",
    re.IGNORECASE,
)

# Where demos 5 and 6 and the shared library produce these messages.
SOURCES = (
    LIB_DIR / "demo_common.py",
    DEMO_DIR / "05-qemu-boot" / "run.py",
    DEMO_DIR / "06-qemu-resume" / "run.py",
)
# Where the messages and the stops behind them are quoted or described.
READMES = (
    DEMO_DIR / "05-qemu-boot" / "README.md",
    DEMO_DIR / "06-qemu-resume" / "README.md",
)


def claims(text: str):
    """Each stop claim in ``text``, with some context, whitespace collapsed."""
    words = " ".join(text.split())
    found = []
    for pattern in (PASSIVE_CLAIM, ACTIVE_CLAIM):
        for match in pattern.finditer(words):
            start = max(0, match.start() - 60)
            found.append("{!r} in ...{}...".format(match.group(0), words[start : match.end() + 20]))
    return found


class StopClaimTest(unittest.TestCase):
    # A failure lists every claim found, not the first few.
    maxDiff = None

    def test_no_message_or_docstring_claims_a_stop(self):
        # Every string constant: the messages, which the parser has already
        # joined across implicitly concatenated literals, and the docstrings.
        found = []
        for source in SOURCES:
            tree = ast.parse(source.read_text(), filename=str(source))
            for node in ast.walk(tree):
                if isinstance(node, ast.Constant) and isinstance(node.value, str):
                    for claim in claims(node.value):
                        where = "{}:{}".format(source.relative_to(DEMO_DIR), node.lineno)
                        found.append("{}: {}".format(where, claim))
        self.assertEqual(found, [], "a message or docstring claims a stop nothing checked")

    def test_no_readme_passage_claims_a_stop(self):
        found = []
        for readme in READMES:
            for claim in claims(readme.read_text()):
                found.append("{}: {}".format(readme.relative_to(DEMO_DIR), claim))
        self.assertEqual(found, [], "a README passage claims a stop nothing checked")

    def test_each_cap_message_says_the_demo_signalled_the_group(self):
        log = Path("hermit-info.log")
        variants = (
            ("during the run", {}),
            ("after the launched process exited", {"exit_status": 3}),
            ("checked once more after it exited", {"exit_status": 3, "final_check": True}),
        )
        for name, extra in variants:
            with self.subTest(variant=name):
                message = str(dc.LogCapExceeded(log, 8192, 4096, 1.5, **extra))
                self.assertEqual(claims(message), [], message)
                self.assertIn("the demo then signalled", message)

    def test_demo_6s_failure_lines_say_what_the_demo_did(self):
        demo6 = runpy.run_path(str(DEMO_DIR / "06-qemu-resume" / "run.py"))
        stopped_run_message = demo6["stopped_run_message"]
        cap = demo6["LogCapExceeded"]
        log = Path("hermit-info.log")
        with tempfile.TemporaryDirectory() as directory:
            # No serial log at all: the message then ends "QEMU had not written
            # the serial log ...", which is not what this test is about.
            serial_log = Path(directory) / "serial.log"
            with mock.patch.dict(stopped_run_message.__globals__, {"TIMEOUT": 120}):
                for name, error, signalled in (
                    ("cap during the run", cap(log, 538072392, 536870912, 29.25), True),
                    (
                        "cap after Hermit exited",
                        cap(log, 538072392, 536870912, 31.0, exit_status=0),
                        True,
                    ),
                    (
                        "cap checked once more after Hermit exited",
                        cap(log, 536871936, 536870912, 16.25, exit_status=0, final_check=True),
                        True,
                    ),
                    ("timeout", TimeoutError("process exceeded timeout of 120s"), False),
                ):
                    with self.subTest(error=name):
                        message = stopped_run_message(error, serial_log)
                        self.assertEqual(claims(message), [], message)
                        if signalled:
                            self.assertIn("signalled Hermit's process group", message)

    def test_the_patterns_find_the_claims_they_are_for(self):
        # So that an empty result above means no claim, not a broken pattern.
        for text in (
            "so the run was stopped before QEMU_TIMEOUT",
            "so the processes still holding it were stopped",
            "anything left in its process\n  group was stopped",
            "Seconds before the resume is stopped",
            "can no longer be stopped safely",
            "the first check that finds a bound passed stops the run",
            "Two bounds outside Hermit stop a resume",
            "the demo then stops Hermit and every process",
            "Stop the run if Hermit's event log grows",
        ):
            with self.subTest(text=text):
                self.assertNotEqual(claims(text), [])
        for text in (
            "the demo then signalled Hermit's process group",
            "no FAILURE line says that anything stopped",
            "this demo stops with demo 5's failure",
            "the controller stops as soon as it sees the BEGIN line",
        ):
            with self.subTest(text=text):
                self.assertEqual(claims(text), [])


if __name__ == "__main__":
    unittest.main()
