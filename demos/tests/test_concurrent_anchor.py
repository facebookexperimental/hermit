#!/usr/bin/env python3
"""Concurrency tests for the demo 5 atomic anchor claim.

These exercise the primitive that makes ``05-qemu-boot.py`` safe to run in 2+
terminals at once: many runs build a private result directory and then race to
publish it as THE anchor with a single atomic, no-clobber rename. Exactly one
run must win; the rest must lose cleanly and compare against a fully-committed
(never partial) anchor.

The tests drive the real ``demo_common`` primitives across genuinely concurrent
processes (fork + a barrier so every worker calls ``publish_anchor`` at the same
instant); they do not need Hermit, QEMU, or a kernel and run in well under a
second. Run directly (``python3 demos/tests/test_concurrent_anchor.py``) or via
``make -C demos test``.
"""

import dataclasses
import multiprocessing
import re
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

DEMO_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = DEMO_DIR / "lib"
sys.path.insert(0, str(LIB_DIR))

import demo_common as dc  # noqa: E402


# Three Hermit INFO records in the form Hermit writes them to standard error.
# A repeat check counts a match only when both logs hold an INFO record.
BASE_LOG = (
    "2026-08-17T04:27:14.000001Z  INFO detcore: line-a\n"
    "2026-08-17T04:27:14.000002Z  INFO detcore: line-b\n"
    "2026-08-17T04:27:14.000003Z  INFO detcore: line-c\n"
)

# The line Hermit's bounded log writer ends a log with once the log reaches
# HERMIT_LOG_MAX_BYTES (TRUNCATION_MARKER in detcore/src/logdiff.rs); a test
# below checks this copy against that source.
TRUNCATION_MARKER = (
    "=== HERMIT LOG TRUNCATED: reached the configured size bound "
    "(HERMIT_LOG_MAX_BYTES). Output beyond this point was DISCARDED. The run "
    "itself continued and was NOT affected. ==="
)


def _boot_record(work, idx=0, qcow2_sha="d" * 64, info_log=None):
    info_log = work / "hermit-info.log" if info_log is None else Path(info_log)
    return {
        "schema_version": dc.RUN_METADATA_SCHEMA_VERSION,
        "kind": "qemu-boot",
        "created_at": str(idx),
        "info_log": str(info_log.resolve()),
        "info_log_sha256": "a" * 64,
        "hermit_version": "hermit-test",
        "qemu_version": "qemu-test",
        "qemu_binary_sha256": "b" * 64,
        "qemu_argv": ["qemu-system-x86_64", "-nographic"],
        "serial_log": str((work / "serial.log").resolve()),
        "serial_sha256": "c" * 64,
        "qcow2_path": str((work / "boot-snapshot.qcow2").resolve()),
        "qcow2_sha256": qcow2_sha,
        "qcow2_size": 1,
        "snapshot_name": "booted",
        "snapshot_date_nsec_canonicalized": True,
    }


def _resume_record(schema_version=dc.RUN_METADATA_SCHEMA_VERSION, info_log=None):
    record = {
        "schema_version": schema_version,
        "kind": "qemu-resume",
        "created_at": "2026-08-28T07:00:00Z",
        "info_log": "/tmp/info.log" if info_log is None else str(info_log),
        "info_log_sha256": "a" * 64,
        "hermit_version": "hermit-test",
        "qemu_version": "qemu-test",
        "qemu_binary_sha256": "b" * 64,
        "qemu_argv": ["qemu-system-x86_64", "-nographic"],
        "serial_log": "/tmp/serial.log",
        "command": "uname -a",
        "command_sha256": "c" * 64,
        "guest_output": "/tmp/output.log",
        "guest_output_sha256": "d" * 64,
        "snapshot_saved": False,
    }
    if schema_version >= 3:
        record["guest_exit_status"] = 0
    return record


def _stamped(text, second):
    """Give every line of ``text`` a wall-clock prefix in Hermit's format."""
    return "".join(
        "{}.{:06d}Z  {}".format(second, number, line)
        for number, line in enumerate(text.splitlines(keepends=True), start=1)
    )


def _build_and_publish(assets_str, lib_str, barrier, queue, idx, divergent):
    """One concurrent run: build a private result dir, then race to publish it."""
    if lib_str not in sys.path:
        sys.path.insert(0, lib_str)
    import demo_common as worker_dc

    assets = Path(assets_str)
    anchor_dir = assets / "boot-anchor"
    work = worker_dc.make_temp_result_dir(assets, "boot")

    # A divergent run differs from its peers (distinct snapshot hash + log tail),
    # so a loser comparing against a different winner must report a mismatch.
    qcow2_sha = "cafe{:060d}".format(idx) if divergent else "d" * 64
    log_text = BASE_LOG + ("extra-{}\n".format(idx) if divergent else "")
    (work / "hermit-info.log").write_text(log_text)
    metadata = worker_dc.parse_run_metadata(_boot_record(work, idx, qcow2_sha))
    worker_dc._write_json(work / "run-metadata.json", dict(metadata.raw))

    barrier.wait()  # release every worker into the rename race simultaneously
    won = worker_dc.publish_anchor(work, anchor_dir)

    outcome = {"idx": idx, "won": won, "work_survived": work.exists()}
    if not won:
        anchor = worker_dc.load_committed_anchor(anchor_dir)
        # Compare while the working dir (and its info_log) is still in place.
        passed, _report = worker_dc.compare_runs(anchor, metadata)
        outcome["passed"] = passed
        outcome["anchor_worker_idx"] = int(anchor.created_at)
        worker_dc.archive_result_dir(work, assets, "boot")
    queue.put(outcome)


def _run_race(assets, count, divergent):
    """Fork ``count`` workers that publish simultaneously; return their outcomes."""
    ctx = multiprocessing.get_context("fork")
    barrier = ctx.Barrier(count)
    queue = ctx.Queue()
    procs = [
        ctx.Process(
            target=_build_and_publish,
            args=(str(assets), str(LIB_DIR), barrier, queue, idx, divergent),
        )
        for idx in range(count)
    ]
    for proc in procs:
        proc.start()
    outcomes = [queue.get() for _ in range(count)]
    for proc in procs:
        proc.join(timeout=30)
        assert proc.exitcode == 0, "worker exited with {}".format(proc.exitcode)
    return outcomes


class ConcurrentAnchorTest(unittest.TestCase):
    def _assert_single_complete_anchor(self, assets, outcomes):
        winners = [o for o in outcomes if o["won"]]
        losers = [o for o in outcomes if not o["won"]]
        self.assertEqual(len(winners), 1, "exactly one run must win the anchor")
        self.assertEqual(len(losers), len(outcomes) - 1)

        # The winner's working dir was renamed away; losers were archived away.
        self.assertFalse(winners[0]["work_survived"])

        anchor_dir = assets / "boot-anchor"
        self.assertTrue((anchor_dir / "run-metadata.json").is_file())
        self.assertTrue((anchor_dir / "hermit-info.log").is_file())

        # The committed anchor is complete and belongs to the sole winner.
        committed = dc.load_committed_anchor(anchor_dir)
        self.assertEqual(int(committed.created_at), winners[0]["idx"])

        # Every loser saw the winner's anchor (not a partial, not its own).
        for loser in losers:
            self.assertEqual(loser["anchor_worker_idx"], winners[0]["idx"])

        # Exactly one boot-anchor exists; losers landed in run-history.
        self.assertEqual(len(list(assets.glob("boot-anchor"))), 1)
        history = list((assets / "run-history").glob("boot-*")) if (
            assets / "run-history"
        ).is_dir() else []
        self.assertEqual(len(history), len(losers))
        return winners, losers

    def test_identical_runs_all_losers_pass(self):
        for count in (2, 3):
            with tempfile.TemporaryDirectory() as tmp:
                assets = Path(tmp) / "qemu-linux"
                outcomes = _run_race(assets, count, divergent=False)
                _winners, losers = self._assert_single_complete_anchor(
                    assets, outcomes
                )
                for loser in losers:
                    self.assertTrue(
                        loser["passed"],
                        "identical run should PASS against anchor: {}".format(loser),
                    )

    def test_divergent_runs_report_mismatch(self):
        for count in (2, 3):
            with tempfile.TemporaryDirectory() as tmp:
                assets = Path(tmp) / "qemu-linux"
                outcomes = _run_race(assets, count, divergent=True)
                _winners, losers = self._assert_single_complete_anchor(
                    assets, outcomes
                )
                for loser in losers:
                    self.assertFalse(
                        loser["passed"],
                        "divergent run must NOT falsely PASS: {}".format(loser),
                    )

    def test_second_publish_loses_without_clobber(self):
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp) / "qemu-linux"
            anchor_dir = assets / "boot-anchor"

            first = dc.make_temp_result_dir(assets, "boot")
            (first / "hermit-info.log").write_text(BASE_LOG)
            dc._write_json(first / "run-metadata.json", _boot_record(first, 1))
            self.assertTrue(dc.publish_anchor(first, anchor_dir))

            second = dc.make_temp_result_dir(assets, "boot")
            (second / "hermit-info.log").write_text(BASE_LOG)
            dc._write_json(second / "run-metadata.json", _boot_record(second, 2))
            self.assertFalse(dc.publish_anchor(second, anchor_dir))

            # The first winner's content was not clobbered by the second claim.
            committed = dc.load_committed_anchor(anchor_dir)
            self.assertEqual(int(committed.created_at), 1)
            # The loser's dir is untouched (caller decides how to archive it).
            self.assertTrue((second / "run-metadata.json").is_file())


class InfoLogAdmissionTest(unittest.TestCase):
    def _metadata(self, log_path):
        return dc.parse_run_metadata(
            _boot_record(Path(log_path).parent, 0, info_log=log_path)
        )

    def test_missing_qemu_argv_fails_by_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            record = _boot_record(Path(tmp))
            del record["qemu_argv"]
            with self.assertRaisesRegex(ValueError, "qemu-run-metadata-qemu_argv"):
                dc.parse_run_metadata(record)

    def test_logs_that_differ_only_in_the_wallclock_prefix_match(self):
        body = (
            "INFO detcore: launcher read FileContents(DetInode(4)) "
            "at 0x7fffffffa210\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_logs(
                tmp,
                "2026-08-17T04:27:14.000000Z " + body,
                "2026-08-17T04:29:10.000000Z " + body,
                stamp=False,
            )

            self.assertTrue(passed, report)
            self.assertTrue(
                any(
                    entry.startswith("PASS: Hermit INFO log matches first run exactly")
                    for entry in report
                ),
                report,
            )

    def test_a_changed_file_resource_id_is_refused(self):
        # Hermit derives the N in FileContents(DetInode(N)) deterministically,
        # so a different N means a different file or a divergent execution. The
        # older raw form FileContents(<inode>) is not folded either.
        cases = (
            (
                "INFO detcore: launcher read FileContents(DetInode(4))\n",
                "INFO detcore: launcher read FileContents(DetInode(5))\n",
            ),
            (
                "INFO detcore: launcher read FileContents(123)\n",
                "INFO detcore: launcher read FileContents(987)\n",
            ),
        )
        for anchor_text, current_text in cases:
            with self.subTest(anchor=anchor_text), tempfile.TemporaryDirectory() as tmp:
                passed, report = self._compare_logs(tmp, anchor_text, current_text)

                self.assertFalse(passed, "a changed resource id must fail the repeat")
                self.assertTrue(
                    any("canonical repeat verification failed" in entry for entry in report),
                    report,
                )

    def test_relocated_qmp_socket_is_stable_for_repeat_comparison(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first_run = root / "first-run"
            second_run = root / "second-run"
            first_run.mkdir()
            second_run.mkdir()
            first_log = first_run / "hermit-info.log"
            second_log = second_run / "hermit-info.log"
            first_log.write_text(BASE_LOG)
            second_log.write_text(BASE_LOG)

            first_socket = Path("/var/tmp/hermit-qmp-test/boot-first.sock")
            second_socket = Path("/var/tmp/hermit-qmp-test/boot-second.sock")
            first_record = _boot_record(first_run, info_log=first_log)
            second_record = _boot_record(second_run, info_log=second_log)
            first_record["qemu_argv"] = [
                dc.canonicalize_qemu_runtime_path(
                    "-qmp=unix:{}".format(first_socket), first_run, first_socket
                )
            ]
            second_record["qemu_argv"] = [
                dc.canonicalize_qemu_runtime_path(
                    "-qmp=unix:{}".format(second_socket), second_run, second_socket
                )
            ]

            passed, report = dc.compare_runs(
                dc.parse_run_metadata(first_record),
                dc.parse_run_metadata(second_record),
            )

            self.assertTrue(passed, report)
            self.assertEqual(first_record["qemu_argv"], ["-qmp=unix:<qmp-socket>"])
            self.assertEqual(first_record["qemu_argv"], second_record["qemu_argv"])

    def _compare_logs(self, tmp, anchor_text, current_text, stamp=True):
        """Compare two logs given as text, one line per log line.

        With ``stamp`` (the default), each line is given a wall-clock prefix in
        Hermit's format, a different one in each log, so every line is a Hermit
        record and the fixtures exercise the comparison of record bodies. Such
        fixtures always hold INFO records, so no verdict here comes from an
        empty log.
        """
        if stamp:
            anchor_text = _stamped(anchor_text, "2026-08-17T04:27:14")
            current_text = _stamped(current_text, "2026-08-17T04:29:10")
        passed, report = self._compare_log_bytes(
            tmp, anchor_text.encode(), current_text.encode()
        )
        if stamp:
            self.assertFalse(
                any("holds no Hermit INFO record" in entry for entry in report), report
            )
        return passed, report

    def _compare_log_bytes(self, tmp, anchor_bytes, current_bytes):
        root = Path(tmp)
        anchor_log = root / "anchor.log"
        current_log = root / "current.log"
        anchor_log.write_bytes(anchor_bytes)
        current_log.write_bytes(current_bytes)
        return dc.compare_runs(self._metadata(anchor_log), self._metadata(current_log))

    # Guest addresses are compared byte for byte. Both logs come from separate
    # `hermit run` invocations of identical input with guest address-space
    # randomization off, so the same execution prints the same addresses. Any
    # address change fails: a shift of the whole layout, one address moved to an
    # unrelated value, two moved, or two swapped everywhere. Each case uses two
    # addresses, because a relationship needs two.

    def test_identical_guest_addresses_match(self):
        same = (
            "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa310\n"
            "INFO detcore: c 0x7fffffffa210\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_logs(tmp, same, same)

            self.assertTrue(passed, report)

    def test_a_uniform_guest_address_shift_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_logs(
                tmp,
                "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa310\n",
                "INFO detcore: a 0x7fffffff9210 b 0x7fffffff9310\n",
            )

            self.assertFalse(
                passed, "the same input must print the same guest addresses"
            )
            self.assertTrue(
                any("canonical repeat verification failed" in entry for entry in report),
                report,
            )

    def test_one_guest_address_moved_to_a_fresh_value_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa310\n"
                "INFO detcore: c 0x7fffffffa210\n",
                "INFO detcore: a 0x7fffffffa210 b 0x7ffff7dd4000\n"
                "INFO detcore: c 0x7fffffffa210\n",
            )

            self.assertFalse(
                passed, "one guest address moving to an unrelated value is a real change"
            )

    def test_two_guest_addresses_moved_to_fresh_values_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa310\n",
                "INFO detcore: a 0x7ffff7dd4000 b 0x7ffff7ff1000\n",
            )

            self.assertFalse(
                passed, "two guest addresses moving to unrelated values is a real change"
            )

    def test_two_guest_addresses_swapped_everywhere_are_refused(self):
        # Swapping two addresses at every site keeps every sharing relationship.
        # The earlier first-appearance comparator could not see it; byte-for-byte
        # comparison does.
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa310\n"
                "INFO detcore: c 0x7fffffffa210\n",
                "INFO detcore: a 0x7fffffffa310 b 0x7fffffffa210\n"
                "INFO detcore: c 0x7fffffffa310\n",
            )

            self.assertFalse(passed, "two guest addresses swapped is a real change")

    def test_two_addresses_that_become_aliased_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa310\n",
                "INFO detcore: a 0x7fffffff9210 b 0x7fffffff9210\n",
            )
            self.assertFalse(
                passed, "two distinct addresses collapsing into one is a real change"
            )

    def test_one_address_splitting_into_two_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa210\n",
                "INFO detcore: a 0x7fffffff9210 b 0x7fffffff9310\n",
            )
            self.assertFalse(
                passed, "one address becoming two distinct ones is a real change"
            )

    def test_a_site_that_stops_reusing_an_earlier_address_is_refused(self):
        # Anchor's third site reuses the first address; current uses a fresh one.
        # That is an identity change at a site, not a shift of the whole layout.
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: a 0x7fffffffa210 b 0x7fffffffa310\n"
                "INFO detcore: c 0x7fffffffa210\n",
                "INFO detcore: a 0x7fffffff9210 b 0x7fffffff9310\n"
                "INFO detcore: c 0x7fffffff9410\n",
            )
            self.assertFalse(
                passed, "a site that stops reusing an earlier address changed identity"
            )

    # Hermit's marker for a host-side address, `<hostaddr 0x...>`, is the one
    # address the canonical policy renumbers (canonicalize_addresses_in_line in
    # detcore/src/logdiff.rs); these mirror that file's canonical_* controls.

    def test_marked_host_addresses_with_the_same_structure_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_logs(
                tmp,
                "INFO detcore: [t] p=<hostaddr 0x1111> q=<hostaddr 0x2222>\n"
                "INFO detcore: [t] use <hostaddr 0x1111> then <hostaddr 0x2222>\n",
                "INFO detcore: [t] p=<hostaddr 0xaaaa> q=<hostaddr 0xbbbb>\n"
                "INFO detcore: [t] use <hostaddr 0xaaaa> then <hostaddr 0xbbbb>\n",
            )

            self.assertTrue(passed, report)

    def test_marked_host_addresses_introduced_in_another_order_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: [t] alloc <hostaddr 0x1111>\n"
                "INFO detcore: [t] alloc <hostaddr 0x2222>\n"
                "INFO detcore: [t] pair <hostaddr 0x1111> <hostaddr 0x2222>\n",
                "INFO detcore: [t] alloc <hostaddr 0xbbbb>\n"
                "INFO detcore: [t] alloc <hostaddr 0xaaaa>\n"
                "INFO detcore: [t] pair <hostaddr 0xaaaa> <hostaddr 0xbbbb>\n",
            )

            self.assertFalse(passed, "a different introduction order is a real change")

    def test_marked_host_addresses_that_stop_aliasing_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: [t] two <hostaddr 0x1111> <hostaddr 0x1111>\n",
                "INFO detcore: [t] two <hostaddr 0xaaaa> <hostaddr 0xbbbb>\n",
            )

            self.assertFalse(passed, "one marked address becoming two is a real change")

    def test_a_bare_hex_value_beside_a_marked_address_is_compared_exactly(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, _ = self._compare_logs(
                tmp,
                "INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0x1111>\n",
                "INFO detcore: flock(fd=3, operation=0x6) at <hostaddr 0xaaaa>\n",
            )
            self.assertFalse(passed, "only the marked value is renumbered")

        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_logs(
                tmp,
                "INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0x1111>\n",
                "INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0xaaaa>\n",
            )
            self.assertTrue(passed, report)

    def test_info_divergence_fails_even_when_vm_artifacts_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            anchor_log = root / "anchor.log"
            current_log = root / "current.log"
            anchor_log.write_text(
                "2026-08-17T04:27:14.000001Z  INFO detcore::scheduler: COMMIT turn "
                "48 on previously committed 1_767_225_600.042_170_525s\n"
            )
            current_log.write_text(
                "2026-08-17T04:29:10.000001Z  INFO detcore::scheduler: COMMIT turn "
                "48 on previously committed 1_767_225_600.042_170_465s\n"
            )

            passed, report = dc.compare_runs(
                self._metadata(anchor_log), self._metadata(current_log)
            )

            self.assertFalse(
                passed, "a canonical INFO divergence must make the demo red"
            )
            self.assertTrue(
                any("canonical repeat verification failed" in line for line in report)
            )

    def test_missing_info_log_fails_repeat_verification(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            missing = root / "missing.log"
            current_log = root / "current.log"
            current_log.write_text("INFO detcore: identical work\n")

            passed, report = dc.compare_runs(
                self._metadata(missing), self._metadata(current_log)
            )

            self.assertFalse(passed, "missing INFO evidence must not produce SUCCESS")
            self.assertTrue(
                any(
                    "canonical repeat verification requires both logs" in line
                    for line in report
                )
            )

    def test_missing_current_info_log_fails_repeat_verification(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            anchor_log = root / "anchor.log"
            missing = root / "missing.log"
            anchor_log.write_text("INFO detcore: identical work\n")

            passed, report = dc.compare_runs(
                self._metadata(anchor_log), self._metadata(missing)
            )

            self.assertFalse(
                passed, "missing current INFO evidence must not produce SUCCESS"
            )
            self.assertTrue(
                any(
                    "canonical repeat verification requires both logs" in line
                    for line in report
                )
            )

    # A match is evidence that the run repeated only when both logs hold a
    # Hermit INFO record: a line that starts with a wall-clock timestamp
    # followed by INFO. A run whose tracing went to a file (HERMIT_LOG_FILE), or
    # whose QEMU_LOG_FILTER kept only warnings, leaves a log with none, and two
    # such logs used to match.

    def test_two_empty_logs_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(tmp, b"", b"")

            self.assertFalse(passed, "two empty logs are no evidence of a repeat")
            self.assertIn(
                "WARN: the first-run Hermit INFO log holds no Hermit INFO record, so "
                "it is no evidence that the run repeated; QEMU_LOG_FILTER must keep "
                "Hermit's INFO records (the default does); remove the saved first "
                "run with demos/clean.sh and run again",
                report,
            )
            self.assertIn(
                "WARN: the current Hermit INFO log holds no Hermit INFO record, so it "
                "is no evidence that the run repeated; QEMU_LOG_FILTER must keep "
                "Hermit's INFO records (the default does)",
                report,
            )
            self.assertFalse(
                any(entry.startswith("PASS: Hermit INFO log") for entry in report), report
            )

    def test_identical_logs_without_an_info_record_are_refused(self):
        # A warning, a run report, and a line that says INFO without the
        # timestamp that starts a Hermit record, as a guest could print.
        log = (
            b"2026-08-17T04:27:14.000001Z  WARN reverie_ptrace::task: a warning\n"
            b"hermit run report:\n"
            b"  exit status: 0\n"
            b"INFO detcore: printed by the guest\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(tmp, log, log)

            self.assertFalse(passed, "logs without an INFO record are no evidence")
            for name in ("first-run", "current"):
                self.assertTrue(
                    any(
                        entry.startswith(
                            "WARN: the {} Hermit INFO log holds no Hermit INFO "
                            "record".format(name)
                        )
                        for entry in report
                    ),
                    report,
                )
            self.assertFalse(
                any(entry.startswith("PASS: Hermit INFO log") for entry in report), report
            )

    def test_an_empty_current_log_is_refused_beside_a_full_first_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(tmp, BASE_LOG.encode(), b"")

            self.assertFalse(passed)
            self.assertTrue(
                any("canonical repeat verification failed" in entry for entry in report),
                report,
            )
            self.assertTrue(
                any(
                    entry.startswith(
                        "WARN: the current Hermit INFO log holds no Hermit INFO record"
                    )
                    for entry in report
                ),
                report,
            )
            self.assertFalse(
                any("the first-run Hermit INFO log holds no" in entry for entry in report),
                report,
            )

    def test_a_match_reports_how_much_was_compared(self):
        log = (
            "2026-08-17T04:27:14.000001Z  INFO detcore: a\n"
            "2026-08-17T04:27:14.000002Z  WARN reverie: b\n"
            "    continuation of b\n"
            "2026-08-17T04:27:14.000003Z  INFO detcore: c\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(
                tmp, log.encode(), log.replace("04:27:14", "04:29:10").encode()
            )

            self.assertTrue(passed, report)
            self.assertIn(
                "PASS: Hermit INFO log matches first run exactly apart from the "
                "wall-clock prefix (Hermit-marked host addresses compared by first "
                "appearance); compared 4 lines, 2 of which start a Hermit INFO record",
                report,
            )

    # Hermit's bounded log writer ends a log that reached HERMIT_LOG_MAX_BYTES
    # with a marker line and discards the rest. Two logs cut at the same size
    # can match while what was discarded differed, so neither may count.

    def test_the_marker_is_the_one_hermit_writes(self):
        source = (DEMO_DIR.parent / "detcore" / "src" / "logdiff.rs").read_text()
        match = re.search(
            r'pub const TRUNCATION_MARKER: &str = "((?:[^"\\]|\\.)*)";', source, re.S
        )
        self.assertIsNotNone(match, "TRUNCATION_MARKER not found in logdiff.rs")
        # A backslash at the end of a line continues a Rust string literal and
        # drops the next line's leading whitespace.
        self.assertEqual(TRUNCATION_MARKER, re.sub(r"\\\n\s*", "", match.group(1)))
        self.assertEqual(TRUNCATION_MARKER, dc.HERMIT_LOG_TRUNCATION_MARKER)

    def test_logs_that_end_with_the_truncation_marker_are_refused(self):
        for ending in ("\n", "", "\r\n\n"):
            with self.subTest(ending=ending), tempfile.TemporaryDirectory() as tmp:
                log = (BASE_LOG + TRUNCATION_MARKER + ending).encode()
                passed, report = self._compare_log_bytes(tmp, log, log)

                self.assertFalse(passed, "a truncated log is incomplete evidence")
                for name in ("first-run", "current"):
                    self.assertIn(
                        "WARN: Hermit INFO logs not compared because the {} log ends "
                        "with Hermit's truncation marker (HERMIT_LOG_MAX_BYTES), so "
                        "part of it was discarded; canonical repeat verification "
                        "requires complete logs".format(name),
                        report,
                    )

    def test_a_truncated_current_log_is_refused_by_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(
                tmp,
                BASE_LOG.encode(),
                (BASE_LOG + TRUNCATION_MARKER + "\n").encode(),
            )

            self.assertFalse(passed)
            self.assertTrue(
                any(
                    "the current log ends with Hermit's truncation marker" in entry
                    for entry in report
                ),
                report,
            )
            self.assertFalse(
                any("the first-run log ends with" in entry for entry in report), report
            )

    def test_a_marker_followed_by_more_records_is_compared(self):
        # Positive control: the log was not cut where the marker text appears.
        log = (TRUNCATION_MARKER + "\n" + BASE_LOG).encode()
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(tmp, log, log)

            self.assertTrue(passed, report)

    def test_a_marker_inside_a_longer_last_line_is_compared(self):
        # Positive control: a record that ends with the marker text, such as a
        # guest path, is not the marker line (Hermit's own test of
        # log_was_truncated uses the same shape).
        log = (
            BASE_LOG
            + "2026-08-17T04:27:14.000004Z  INFO detcore: statx path="
            + TRUNCATION_MARKER
            + "\n"
        ).encode()
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(tmp, log, log)

            self.assertTrue(passed, report)

    # The logs are compared as bytes, a line ending only at a newline byte.

    def test_bytes_that_are_not_utf8_are_compared(self):
        anchor = BASE_LOG.encode() + b"2026-08-17T04:27:14.000004Z  INFO detcore: read \xff\n"
        current = BASE_LOG.encode() + b"2026-08-17T04:29:10.000004Z  INFO detcore: read \xfe\n"
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(tmp, anchor, current)

            self.assertFalse(passed, "\\xff and \\xfe are different bytes")
            self.assertTrue(
                any("canonical repeat verification failed" in entry for entry in report),
                report,
            )

    def test_line_endings_are_compared(self):
        cases = {
            "CRLF against LF": (
                b"2026-08-17T04:27:14.000001Z  INFO detcore: a\r\n",
                b"2026-08-17T04:29:10.000001Z  INFO detcore: a\n",
            ),
            "lone CR against LF": (
                b"2026-08-17T04:27:14.000001Z  INFO detcore: a\r"
                b"2026-08-17T04:27:14.000002Z  INFO detcore: b\n",
                b"2026-08-17T04:29:10.000001Z  INFO detcore: a\n"
                b"2026-08-17T04:29:10.000002Z  INFO detcore: b\n",
            ),
        }
        for name, (anchor, current) in cases.items():
            with self.subTest(name), tempfile.TemporaryDirectory() as tmp:
                passed, report = self._compare_log_bytes(tmp, anchor, current)

                self.assertFalse(passed, report)
                self.assertTrue(
                    any(
                        "canonical repeat verification failed" in entry
                        for entry in report
                    ),
                    report,
                )

    def test_a_timestamp_on_one_side_only_is_refused(self):
        # Whether a line starts with a wall-clock timestamp is compared; only
        # the timestamp's value is not.
        anchor = (
            b"2026-08-17T04:27:14.000001Z  INFO detcore: x\n"
            b"2026-08-17T04:27:14.000002Z  WARN reverie: y\n"
        )
        current = b"2026-08-17T04:29:10.000001Z  INFO detcore: x\nWARN reverie: y\n"
        with tempfile.TemporaryDirectory() as tmp:
            passed, report = self._compare_log_bytes(tmp, anchor, current)

            self.assertFalse(passed, report)
            divergence = next(
                entry
                for entry in report
                if "canonical repeat verification failed" in entry
            )
            self.assertIn("first divergence at line 2", divergence)
            self.assertIn("- '<wall-clock> WARN reverie: y\\n'", divergence)
            self.assertIn("+ 'WARN reverie: y\\n'", divergence)


class RunMetadataContractTest(unittest.TestCase):
    def test_every_kind_has_an_explicit_field_contract(self):
        self.assertEqual(set(dc.QemuRunKind), set(dc.METADATA_FIELDS_BY_KIND))

    def test_producer_writes_the_complete_current_type(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            info_log = run / "hermit-info.log"
            qcow2 = run / "boot-snapshot.qcow2"
            serial_log = run / "serial.log"
            info_log.write_text("INFO deterministic run\n")
            qcow2.write_bytes(b"qcow2")
            serial_log.write_text("serial\n")
            with mock.patch.object(
                dc, "_tool_version", return_value="test-version"
            ), mock.patch.object(dc, "_tool_sha256", return_value="b" * 64):
                metadata = dc.save_metadata(
                    run,
                    qcow2,
                    info_log,
                    {
                        "kind": "qemu-boot",
                        "snapshot_name": "booted",
                        "snapshot_date_nsec_canonicalized": True,
                        "qemu_argv": ["qemu-system-x86_64", "-nographic"],
                        "serial_log": str(serial_log),
                        "serial_sha256": dc.hash_file(serial_log),
                    },
                )

            self.assertEqual(dc.RUN_METADATA_SCHEMA_VERSION, metadata.schema_version)
            self.assertEqual(dc.QemuRunKind.BOOT, metadata.kind)
            self.assertEqual("b" * 64, metadata.qemu_binary_sha256)
            self.assertEqual(metadata, dc.load_anchor(run))

    def test_producer_refuses_missing_qemu_binary(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            info_log = run / "hermit-info.log"
            qcow2 = run / "boot-snapshot.qcow2"
            serial_log = run / "serial.log"
            info_log.write_text("INFO deterministic run\n")
            qcow2.write_bytes(b"qcow2")
            serial_log.write_text("serial\n")
            missing_qemu = run / "missing-qemu"
            with mock.patch.object(
                dc, "_tool_version", return_value="test-version"
            ), mock.patch.dict(dc.os.environ, {"QEMU_BIN": str(missing_qemu)}):
                with self.assertRaisesRegex(
                    ValueError, "qemu-run-metadata-qemu_binary_sha256"
                ):
                    dc.save_metadata(
                        run,
                        qcow2,
                        info_log,
                        {
                            "kind": "qemu-boot",
                            "snapshot_name": "booted",
                            "snapshot_date_nsec_canonicalized": True,
                            "qemu_argv": [str(missing_qemu), "-nographic"],
                            "serial_log": str(serial_log),
                            "serial_sha256": dc.hash_file(serial_log),
                        },
                    )

            self.assertFalse((run / "run-metadata.json").exists())

    def test_schema_two_requires_qemu_binary_identity(self):
        # Schema 2 made the QEMU binary digest required; later schemas keep it.
        for schema_version in (2, dc.RUN_METADATA_SCHEMA_VERSION):
            boot = _boot_record(Path("/tmp"))
            boot["schema_version"] = schema_version
            for record in (boot, _resume_record(schema_version)):
                with self.subTest(schema_version=schema_version, kind=record["kind"]):
                    del record["qemu_binary_sha256"]
                    with self.assertRaisesRegex(
                        ValueError, "qemu-run-metadata-qemu_binary_sha256"
                    ):
                        dc.parse_run_metadata(record)

    def test_schema_one_retains_the_older_optional_qemu_binary(self):
        record = _resume_record(schema_version=1)
        del record["qemu_binary_sha256"]
        metadata = dc.parse_run_metadata(record)
        self.assertIsNone(metadata.qemu_binary_sha256)

    def test_qemu_binary_identity_rejects_non_digest_values(self):
        for schema_version in dc.SUPPORTED_RUN_METADATA_SCHEMA_VERSIONS:
            records = (_boot_record(Path("/tmp")), _resume_record(schema_version))
            records[0]["schema_version"] = schema_version
            for record in records:
                for invalid in (
                    "unavailable: qemu-system-x86_64 is not a file",
                    "B" * 64,
                    "b" * 63,
                    "g" * 64,
                ):
                    with self.subTest(
                        schema_version=schema_version,
                        kind=record["kind"],
                        invalid=invalid,
                    ):
                        record["qemu_binary_sha256"] = invalid
                        with self.assertRaisesRegex(
                            ValueError,
                            "qemu-run-metadata-qemu_binary_sha256: must be a "
                            "lowercase 64-hex SHA-256",
                        ):
                            dc.parse_run_metadata(record)

    def test_schema_two_accepts_lowercase_qemu_binary_digest(self):
        for schema_version in (2, dc.RUN_METADATA_SCHEMA_VERSION):
            with self.subTest(schema_version=schema_version):
                metadata = dc.parse_run_metadata(_resume_record(schema_version))
                self.assertEqual("b" * 64, metadata.qemu_binary_sha256)

    def test_supported_schemas_are_one_to_current(self):
        self.assertEqual(3, dc.RUN_METADATA_SCHEMA_VERSION)
        self.assertEqual((1, 2, 3), dc.SUPPORTED_RUN_METADATA_SCHEMA_VERSIONS)

    def test_schema_three_resume_requires_guest_exit_status(self):
        record = _resume_record(3)
        del record["guest_exit_status"]
        with self.assertRaisesRegex(
            ValueError,
            "qemu-run-metadata-guest_exit_status: is required for qemu-resume",
        ):
            dc.parse_run_metadata(record)

    def test_guest_exit_status_is_parsed(self):
        for status in (0, 1, 127, 255):
            with self.subTest(status=status):
                record = _resume_record(3)
                record["guest_exit_status"] = status
                self.assertEqual(
                    status, dc.parse_run_metadata(record).guest_exit_status
                )

    def test_older_resume_schemas_have_no_guest_exit_status(self):
        for schema_version in (1, 2):
            with self.subTest(schema_version=schema_version):
                record = _resume_record(schema_version)
                self.assertNotIn("guest_exit_status", record)
                self.assertIsNone(dc.parse_run_metadata(record).guest_exit_status)
                record["guest_exit_status"] = 0
                with self.assertRaisesRegex(
                    ValueError,
                    "qemu-run-metadata-guest_exit_status: is not part of schema {}".format(
                        schema_version
                    ),
                ):
                    dc.parse_run_metadata(record)

    def test_guest_exit_status_rejects_non_status_values(self):
        for invalid in (True, False, -1, 256, "0", 0.0, None):
            with self.subTest(invalid=invalid):
                record = _resume_record(3)
                record["guest_exit_status"] = invalid
                with self.assertRaisesRegex(
                    ValueError,
                    "qemu-run-metadata-guest_exit_status: must be an integer "
                    "from 0 to 255",
                ):
                    dc.parse_run_metadata(record)

    def test_boot_rows_have_no_guest_exit_status(self):
        record = _boot_record(Path("/tmp"))
        record["guest_exit_status"] = 0
        with self.assertRaisesRegex(
            ValueError, "qemu-run-metadata-field: unknown field.*guest_exit_status"
        ):
            dc.parse_run_metadata(record)

    def test_compare_runs_checks_guest_exit_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            info_log = Path(tmp) / "hermit-info.log"
            info_log.write_text(BASE_LOG)
            anchor = dc.parse_run_metadata(_resume_record(3, info_log))
            same = dc.parse_run_metadata(_resume_record(3, info_log))
            passed, report = dc.compare_runs(anchor, same)
            self.assertTrue(passed, report)
            self.assertIn("PASS: guest command exit status matches (0)", report)

            different_record = _resume_record(3, info_log)
            different_record["guest_exit_status"] = 3
            different = dc.parse_run_metadata(different_record)
            passed, report = dc.compare_runs(anchor, different)
            self.assertFalse(passed)
            self.assertIn(
                "WARN: guest command exit status differs from first run: "
                "first=0 current=3",
                report,
            )

            # A reference run from before schema 3 recorded no status, so it
            # cannot vouch for the current run's status.
            older = dc.parse_run_metadata(_resume_record(2, info_log))
            passed, report = dc.compare_runs(older, same)
            self.assertFalse(passed)
            self.assertIn(
                "WARN: guest command exit status differs from first run: "
                "first=None current=0",
                report,
            )

    def test_saved_resume_requires_its_snapshot_fields(self):
        record = _resume_record()
        record["snapshot_saved"] = True
        with self.assertRaisesRegex(ValueError, "qemu-run-metadata-qcow2_path"):
            dc.parse_run_metadata(record)

    def test_new_kind_fails_by_name(self):
        record = _boot_record(Path("/tmp"))
        record["kind"] = "qemu-future"
        with self.assertRaisesRegex(ValueError, "qemu-run-metadata-kind"):
            dc.parse_run_metadata(record)

    def test_new_field_fails_by_name(self):
        record = _boot_record(Path("/tmp"))
        record["future_field"] = True
        with self.assertRaisesRegex(ValueError, "qemu-run-metadata-field"):
            dc.parse_run_metadata(record)

    # A compared field that neither run recorded is never passed over in
    # silence: the report says why it was not compared, or the repeat fails
    # when a row of that shape must record it.

    def _info_log(self, tmp):
        info_log = Path(tmp) / "hermit-info.log"
        info_log.write_text(BASE_LOG)
        return info_log

    def test_boot_rows_report_the_guest_command_fields_as_not_compared(self):
        with tempfile.TemporaryDirectory() as tmp:
            info_log = self._info_log(tmp)
            anchor = dc.parse_run_metadata(_boot_record(Path(tmp), 0, info_log=info_log))
            current = dc.parse_run_metadata(_boot_record(Path(tmp), 1, info_log=info_log))
            passed, report = dc.compare_runs(anchor, current)

            self.assertTrue(passed, report)
            self.assertIn(
                "NOT COMPARED: guest output SHA-256: qemu-boot runs start no guest "
                "command",
                report,
            )
            self.assertIn(
                "NOT COMPARED: guest command exit status: qemu-boot runs start no "
                "guest command",
                report,
            )

    def test_resume_rows_without_a_snapshot_report_the_snapshot_fields_as_not_compared(self):
        with tempfile.TemporaryDirectory() as tmp:
            row = dc.parse_run_metadata(_resume_record(3, self._info_log(tmp)))
            passed, report = dc.compare_runs(row, row)

            self.assertTrue(passed, report)
            self.assertIn(
                "NOT COMPARED: qcow2 SHA-256: neither run saved a snapshot", report
            )
            self.assertIn(
                "NOT COMPARED: serial output SHA-256: qemu-resume rows do not record "
                "it; the guest command's output, taken from the serial log, is "
                "compared instead",
                report,
            )

    def test_a_required_field_that_neither_run_recorded_fails(self):
        # The parser refuses a row without these fields, so only a row built
        # some other way can lack them; compare_runs does not rely on that.
        boot = "qemu-boot rows of schema 3"
        resume = "qemu-resume rows of schema 3 with snapshot_saved false"
        cases = (
            (boot, "qemu_version", "QEMU version"),
            (boot, "qemu_binary_sha256", "QEMU binary SHA-256"),
            (boot, "qcow2_sha256", "qcow2 SHA-256"),
            (boot, "serial_sha256", "serial output SHA-256"),
            (resume, "qemu_version", "QEMU version"),
            (resume, "qemu_binary_sha256", "QEMU binary SHA-256"),
            (resume, "guest_output_sha256", "guest output SHA-256"),
            (resume, "guest_exit_status", "guest command exit status"),
        )
        for description, field, label in cases:
            with self.subTest(row=description, field=field), tempfile.TemporaryDirectory() as tmp:
                info_log = self._info_log(tmp)
                record = (
                    _boot_record(Path(tmp), info_log=info_log)
                    if description == boot
                    else _resume_record(3, info_log)
                )
                row = dataclasses.replace(dc.parse_run_metadata(record), **{field: None})
                passed, report = dc.compare_runs(row, row)

                self.assertFalse(passed, report)
                self.assertIn(
                    "WARN: {} was not compared: neither run recorded it, although {} "
                    "must record it".format(label, description),
                    report,
                )

    def test_a_field_older_rows_lack_fails_where_current_rows_record_it(self):
        # Two schema-2 rows: neither recorded the guest command's exit status,
        # which the current code records, so the repeat cannot vouch for it.
        with tempfile.TemporaryDirectory() as tmp:
            older = dc.parse_run_metadata(_resume_record(2, self._info_log(tmp)))
            passed, report = dc.compare_runs(older, older)

            self.assertFalse(passed, report)
            self.assertIn(
                "WARN: guest command exit status was not compared: neither run "
                "recorded it (first run schema 2, current run schema 2; qemu-resume "
                "rows of schema 3 with snapshot_saved false record it), so this "
                "repeat cannot vouch for it",
                report,
            )

    def test_schema_one_rows_without_a_qemu_binary_digest_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            record = _resume_record(1, self._info_log(tmp))
            del record["qemu_binary_sha256"]
            row = dc.parse_run_metadata(record)
            passed, report = dc.compare_runs(row, row)

            self.assertFalse(passed, report)
            self.assertIn(
                "WARN: QEMU binary SHA-256 was not compared: neither run recorded it "
                "(first run schema 1, current run schema 1; qemu-resume rows of "
                "schema 3 with snapshot_saved false record it), so this repeat "
                "cannot vouch for it",
                report,
            )

    def test_the_required_fields_match_the_parser(self):
        # metadata_field_required restates the parser's rules for the compared
        # fields; a field is required exactly when the parser refuses a row
        # without it.
        for kind in dc.QemuRunKind:
            for schema_version in dc.SUPPORTED_RUN_METADATA_SCHEMA_VERSIONS:
                for snapshot_saved in (
                    (False, True) if kind is dc.QemuRunKind.RESUME else (None,)
                ):
                    for field, _ in dc.COMPARED_METADATA_FIELDS:
                        with self.subTest(
                            kind=kind.value,
                            schema=schema_version,
                            snapshot_saved=snapshot_saved,
                            field=field,
                        ):
                            if kind is dc.QemuRunKind.BOOT:
                                record = _boot_record(Path("/tmp"))
                                record["schema_version"] = schema_version
                            else:
                                record = _resume_record(schema_version)
                                if snapshot_saved:
                                    record.update(
                                        snapshot_saved=True,
                                        qcow2_path="/tmp/resume.qcow2",
                                        qcow2_sha256="e" * 64,
                                        qcow2_size=1,
                                        snapshot_date_nsec_canonicalized=True,
                                    )
                            # The complete row parses.
                            dc.parse_run_metadata(record)
                            record.pop(field, None)
                            if dc.metadata_field_required(
                                kind, schema_version, snapshot_saved, field
                            ):
                                with self.assertRaisesRegex(
                                    ValueError, "qemu-run-metadata-{}:".format(field)
                                ):
                                    dc.parse_run_metadata(record)
                            else:
                                self.assertIsNone(
                                    getattr(dc.parse_run_metadata(record), field)
                                )


class RuntimePathRewriteTest(unittest.TestCase):
    """Demo 5 replaces its run directory in the saved log as bytes."""

    def test_the_file_rewrite_matches_the_text_rewrite_and_keeps_other_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp) / "boot-run"
            run_dir.mkdir()
            for qmp_socket in (
                run_dir / "qmp.sock",
                Path("/var/tmp/hermit-qmp-test/boot.sock"),
            ):
                with self.subTest(qmp_socket=str(qmp_socket)):
                    text = (
                        "-drive file={run}/disk.qcow2 -qmp unix:{socket}\r\n"
                        "progress\rnext {run}\n"
                    ).format(run=run_dir, socket=qmp_socket)
                    log = run_dir / "hermit-info.log"
                    log.write_bytes(text.encode() + b"\xff\n")

                    dc.canonicalize_qemu_runtime_paths_in_file(log, run_dir, qmp_socket)

                    self.assertEqual(
                        dc.canonicalize_qemu_runtime_path(text, run_dir, qmp_socket).encode()
                        + b"\xff\n",
                        log.read_bytes(),
                    )
                    self.assertEqual(["hermit-info.log"], sorted(p.name for p in run_dir.iterdir()))


if __name__ == "__main__":
    unittest.main(verbosity=2)
