#!/usr/bin/env python3
"""Tests for ci/buck-e2e/stages.py, the per-cell stage summary of a Buck event log.

The events are synthetic `buck2 log show` lines with the shapes stages.py reads: TestRun
spans, executor stage spans below them, and a TestRun SpanEnd that reports whether its
remote command was a cache hit.
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

STAGES = Path(__file__).resolve().parent / "buck-e2e" / "stages.py"
spec = importlib.util.spec_from_file_location("buck_e2e_stages", STAGES)
stages = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stages)

T0 = 1_700_000_000


def event(seconds, kind, data, span=0, parent=0):
    whole = int(seconds)
    return json.dumps({"Event": {
        "timestamp": [T0 + whole, round((seconds - whole) * 1e9)], "span_id": span, "parent_id": parent,
        "data": {kind: {"data": data}}}}) + "\n"


def test_run(cell):
    return {"TestRun": {"suite": {"suite_name": f"root//ci/buck-e2e:{cell}"}}}


def remote_end(cell, hit):
    run = test_run(cell)
    run["TestRun"]["command_report"] = {"details": {"command_kind": {"command": {
        "RemoteCommand": {"action_digest": "0" * 40 + ":1", "cache_hit": hit}}}}}
    return run


def re_stage(name):
    return {"ExecutorStage": {"stage": {"Re": {"stage": {name: {"action_digest": "0" * 40 + ":1"}}}}}}


def local_stage(name):
    return {"ExecutorStage": {"stage": {"Local": {"stage": {name: {}}}}}}


def two_cells():
    """slow: an RE cell that waits 8 s for its inputs; fast: a local cell."""
    return [
        event(0.0, "Instant", {"Other": {}}),
        event(1.0, "SpanStart", test_run("slow"), span=10),
        event(1.0, "SpanStart", {"ReUpload": {}}, span=11, parent=10),
        event(3.0, "SpanEnd", {"ReUpload": {}}, span=11, parent=10),
        event(3.0, "SpanStart", re_stage("WorkerDownload"), span=12, parent=10),
        event(11.0, "SpanEnd", re_stage("WorkerDownload"), span=12, parent=10),
        event(11.0, "SpanStart", re_stage("Execute"), span=13, parent=10),
        event(12.5, "SpanEnd", re_stage("Execute"), span=13, parent=10),
        event(13.0, "SpanEnd", remote_end("slow", False), span=10),
        event(2.0, "SpanStart", test_run("fast"), span=20),
        event(2.0, "SpanStart", local_stage("Execute"), span=21, parent=20),
        event(4.0, "SpanEnd", local_stage("Execute"), span=21, parent=20),
        event(4.0, "SpanEnd", test_run("fast"), span=20),
    ]


class StagesTest(unittest.TestCase):
    def summary(self, lines, top=20):
        return stages.report(*stages.summarize(lines), top)

    def test_each_stage_is_charged_to_its_cell(self):
        runs, per_cell, first, last = stages.summarize(two_cells())
        names = {span: run["name"] for span, run in runs.items()}
        self.assertEqual(names, {10: "slow", 20: "fast"})
        self.assertEqual(dict(per_cell[10]), {"ReUpload": 2.0, "Re.WorkerDownload": 8.0, "Re.Execute": 1.5})
        self.assertEqual(dict(per_cell[20]), {"Local.Execute": 2.0})
        # The fast cell's events come after the slow cell's, but the slow cell ends last.
        self.assertEqual((first, last), (T0, T0 + 13.0))

    def test_a_stage_nested_below_a_stage_is_charged_to_the_same_cell(self):
        lines = [
            event(0.0, "SpanStart", test_run("a"), span=1),
            event(0.0, "SpanStart", re_stage("Queue"), span=2, parent=1),
            event(1.0, "SpanStart", {"Materialization": {}}, span=3, parent=2),
            event(4.0, "SpanEnd", {"Materialization": {}}, span=3, parent=2),
            event(5.0, "SpanEnd", re_stage("Queue"), span=2, parent=1),
            event(5.0, "SpanEnd", test_run("a"), span=1),
        ]
        _runs, per_cell, _first, _last = stages.summarize(lines)
        self.assertEqual(dict(per_cell[1]), {"Re.Queue": 5.0, "Materialization": 3.0})

    def test_the_last_cells_to_finish_are_listed_last_with_their_longest_stages(self):
        text = self.summary(two_cells())
        self.assertIn("event span 13.0s, 2 cell executions", text)
        listed = text.split("longest stages):\n", 1)[1].splitlines()
        self.assertEqual(len(listed), 2)
        self.assertRegex(listed[0], r"^\s+4\.0\s+2\.0s -\s+fast Local\.Execute=2\.0$")
        self.assertRegex(listed[1], r"^\s+13\.0\s+12\.0s miss slow Re\.WorkerDownload=8\.0 ReUpload=2\.0 "
                                    r"Re\.Execute=1\.5$")

    def test_the_stage_table_is_ordered_by_total_time_then_name(self):
        text = self.summary(two_cells())
        table = text.split("\n\n", 1)[0].splitlines()[2:]
        self.assertEqual([row.split()[0] for row in table],
                         ["Re.WorkerDownload", "Local.Execute", "ReUpload", "Re.Execute"])
        self.assertEqual(table[0].split()[1:], ["1", "8.0", "8.00", "8.00", "8.00", "8.00"])

    def test_top_limits_the_listed_cells_and_never_exceeds_them(self):
        only = self.summary(two_cells(), top=1)
        self.assertIn("last 1 cell executions", only)
        self.assertEqual(len(only.split("longest stages):\n", 1)[1].splitlines()), 1)
        self.assertIn(" slow ", only)
        every = self.summary(two_cells(), top=5)
        self.assertIn("last 2 cell executions", every)
        self.assertEqual(len(every.split("longest stages):\n", 1)[1].splitlines()), 2)
        none = self.summary(two_cells(), top=0)
        self.assertEqual(none.split("longest stages):\n", 1)[1], "")

    def test_a_cache_hit_is_reported(self):
        lines = [
            event(0.0, "SpanStart", test_run("cached"), span=1),
            event(0.5, "SpanEnd", remote_end("cached", True), span=1),
        ]
        self.assertRegex(self.summary(lines), r"\n\s+0\.5\s+0\.5s hit\s+cached")

    def test_a_cell_still_running_when_the_log_ends_is_counted_to_the_last_event(self):
        lines = two_cells()[:8] + [event(20.0, "Instant", {"Other": {}})]
        text = self.summary(lines)
        self.assertRegex(text, r"\n\s+20\.0\s+19\.0s -\s+slow ")

    def cut_download(self, closed):
        """A cell that waits on its inputs from t=2; CLOSED: the log holds their end and the cell's, at t=12."""
        lines = [
            event(1.0, "SpanStart", test_run("slow"), span=1),
            event(2.0, "SpanStart", re_stage("WorkerDownload"), span=2, parent=1),
        ]
        if closed:
            return lines + [event(12.0, "SpanEnd", re_stage("WorkerDownload"), span=2, parent=1),
                            event(12.0, "SpanEnd", remote_end("slow", False), span=1)]
        return lines + [event(12.0, "Instant", {"Other": {}})]

    def test_a_closed_stage_is_exact(self):
        runs, per_cell, _first, _last = stages.summarize(self.cut_download(closed=True))
        self.assertEqual(dict(per_cell[1]), {"Re.WorkerDownload": 10.0})
        self.assertEqual(runs[1]["open"], set())
        text = self.summary(self.cut_download(closed=True))
        self.assertNotIn("lower bound", text)
        self.assertRegex(text, r"\n\s+11\.0\s+11\.0s miss slow Re\.WorkerDownload=10\.0\n")

    def test_a_stage_the_log_ends_in_is_counted_to_the_last_event_as_a_lower_bound(self):
        runs, per_cell, _first, _last = stages.summarize(self.cut_download(closed=False))
        self.assertEqual(dict(per_cell[1]), {"Re.WorkerDownload": 10.0})
        self.assertEqual(runs[1]["open"], {"Re.WorkerDownload"})
        text = self.summary(self.cut_download(closed=False))
        table = text.split("\n\n", 1)[0].splitlines()[2:]
        self.assertEqual(table, [f"{'Re.WorkerDownload':28} {1:5} {10.0:9.1f} {10.0:7.2f} {10.0:7.2f} {10.0:7.2f} "
                                 f"{10.0:7.2f}  still open in 1, a lower bound"])
        self.assertRegex(text, r"\n\s+11\.0\s+11\.0s -\s+slow Re\.WorkerDownload>=10\.0\n")

    def test_nested_stages_the_log_ends_in_are_each_counted_to_the_last_event(self):
        lines = [
            event(1.0, "SpanStart", test_run("a"), span=1),
            event(2.0, "SpanStart", re_stage("Queue"), span=2, parent=1),
            event(4.0, "SpanStart", {"Materialization": {}}, span=3, parent=2),
            event(12.0, "Instant", {"Other": {}}),
        ]
        runs, per_cell, _first, _last = stages.summarize(lines)
        self.assertEqual(dict(per_cell[1]), {"Re.Queue": 10.0, "Materialization": 8.0})
        self.assertEqual(runs[1]["open"], {"Re.Queue", "Materialization"})
        self.assertRegex(self.summary(lines), r"\n\s+11\.0\s+11\.0s -\s+a Re\.Queue>=10\.0 Materialization>=8\.0\n")

    def test_a_stage_still_open_when_its_cell_ended_is_counted_to_the_cells_end(self):
        lines = [
            event(1.0, "SpanStart", test_run("a"), span=1),
            event(2.0, "SpanStart", local_stage("Execute"), span=2, parent=1),
            event(5.0, "SpanEnd", test_run("a"), span=1),
            event(12.0, "Instant", {"Other": {}}),
        ]
        runs, per_cell, _first, _last = stages.summarize(lines)
        self.assertEqual(dict(per_cell[1]), {"Local.Execute": 3.0})
        self.assertEqual(runs[1]["open"], {"Local.Execute"})

    def test_an_open_stage_beside_a_closed_one_of_the_same_name_makes_the_sum_a_lower_bound(self):
        lines = [
            event(0.0, "SpanStart", test_run("a"), span=1),
            event(0.0, "SpanStart", local_stage("Execute"), span=2, parent=1),
            event(1.0, "SpanEnd", local_stage("Execute"), span=2, parent=1),
            event(2.0, "SpanStart", local_stage("Execute"), span=3, parent=1),
            event(4.0, "Instant", {"Other": {}}),
        ]
        runs, per_cell, _first, _last = stages.summarize(lines)
        self.assertEqual(dict(per_cell[1]), {"Local.Execute": 3.0})
        self.assertRegex(self.summary(lines), r" a Local\.Execute>=3\.0\n")

    def test_lines_that_are_not_events_are_skipped(self):
        lines = ['{"command_line_args": ["buck2", "test"]}\n', "not json\n", '{"Event": {}}\n',
                 json.dumps({"Event": {"timestamp": "x"}}) + "\n"] + two_cells()
        self.assertEqual(self.summary(lines), self.summary(two_cells()))

    def test_an_empty_log_has_no_events(self):
        self.assertEqual(self.summary([]), "no events\n")

    def test_the_command_line_reads_a_file_or_stdin(self):
        with tempfile.TemporaryDirectory(prefix="test-buck-e2e-stages-") as temporary:
            events = Path(temporary) / "events.jsonl"
            events.write_text("".join(two_cells()))
            from_file = subprocess.run([sys.executable, str(STAGES), "--top", "1", str(events)],
                                       capture_output=True, text=True, check=True)
        from_stdin = subprocess.run([sys.executable, str(STAGES), "--top", "1", "-"], input="".join(two_cells()),
                                    capture_output=True, text=True, check=True)
        self.assertEqual(from_file.stdout, self.summary(two_cells(), top=1))
        self.assertEqual(from_stdin.stdout, from_file.stdout)


if __name__ == "__main__":
    unittest.main()
