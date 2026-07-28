#!/usr/bin/env python3
"""Offline unit tests for pr_status.py (no network required).

Run with: python3 scripts/test_pr_status.py
"""

from __future__ import annotations

import unittest

import pr_status


class ClassifyCiRollupTest(unittest.TestCase):
    def test_empty_or_missing_is_none(self) -> None:
        self.assertEqual(pr_status.classify_ci_rollup("rrnewton/hermit", []), "none")
        self.assertEqual(pr_status.classify_ci_rollup("rrnewton/hermit", None), "none")

    def test_failure_conclusion_is_red(self) -> None:
        checks = [
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "FAILURE",
                "status": "COMPLETED",
            },
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "red"
        )

    def test_incomplete_status_is_pending(self) -> None:
        checks = [
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "",
                "status": "IN_PROGRESS",
            }
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "pending"
        )

    def test_all_success_is_green(self) -> None:
        checks = [
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "SUCCESS",
                "status": "COMPLETED",
            }
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "green"
        )

    def test_merge_gate_failure_is_ignored(self) -> None:
        checks = [
            {
                "name": "merge-gate",
                "conclusion": "FAILURE",
                "status": "COMPLETED",
                "startedAt": "2026-07-26T12:00:00Z",
            },
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "SUCCESS",
                "status": "COMPLETED",
                "startedAt": "2026-07-26T12:01:00Z",
            },
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "green"
        )

    def test_latest_authoritative_rerun_wins(self) -> None:
        checks = [
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "FAILURE",
                "status": "COMPLETED",
                "startedAt": "2026-07-26T12:00:00Z",
            },
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "SUCCESS",
                "status": "COMPLETED",
                "startedAt": "2026-07-26T12:05:00Z",
            },
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "green"
        )

    def test_newer_queued_run_wins_over_older_success(self) -> None:
        checks = [
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "SUCCESS",
                "status": "COMPLETED",
                "startedAt": "2026-07-26T12:00:00Z",
                "detailsUrl": "https://github.com/o/r/actions/runs/10/job/1",
            },
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "",
                "status": "QUEUED",
                "startedAt": "0001-01-01T00:00:00Z",
                "detailsUrl": "https://github.com/o/r/actions/runs/11/job/2",
            },
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "pending"
        )

    def test_hermit_privileged_failure_is_nonblocking(self) -> None:
        checks = [
            {
                "name": pr_status.REGULAR_PORTABLE_CHECK,
                "conclusion": "SUCCESS",
                "status": "COMPLETED",
            },
            {
                "name": "Privileged capability and E2E tests",
                "conclusion": "FAILURE",
                "status": "COMPLETED",
            },
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "green"
        )

    def test_reverie_requires_portable_and_host_dependent_checks(self) -> None:
        portable = {
            "name": pr_status.REGULAR_PORTABLE_CHECK,
            "conclusion": "SUCCESS",
            "status": "COMPLETED",
        }
        host_dependent = {
            "name": "Host-dependent tests (privileged)",
            "conclusion": "SUCCESS",
            "status": "COMPLETED",
        }
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/reverie", [portable]), "pending"
        )
        self.assertEqual(
            pr_status.classify_ci_rollup(
                "rrnewton/reverie", [portable, host_dependent]
            ),
            "green",
        )

        failed = {**host_dependent, "conclusion": "FAILURE"}
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/reverie", [portable, failed]),
            "red",
        )


class ParsePullRequestTest(unittest.TestCase):
    def test_human_review_label_detected(self) -> None:
        pr = pr_status.parse_pull_request(
            "rrnewton/reverie",
            {
                "number": 8,
                "title": "Extend KVM syscall interception",
                "url": "https://github.com/rrnewton/reverie/pull/8",
                "isDraft": False,
                "labels": [{"name": "human-review"}],
                "statusCheckRollup": [],
            },
        )
        self.assertTrue(pr.needs_human_review)
        self.assertEqual(pr.repo, "rrnewton/reverie")

    def test_unlabeled_pr_is_free_to_land(self) -> None:
        pr = pr_status.parse_pull_request(
            "rrnewton/reverie",
            {
                "number": 20,
                "title": "Fix unaligned remote memory writes",
                "url": "https://github.com/rrnewton/reverie/pull/20",
                "labels": [],
                "statusCheckRollup": [],
            },
        )
        self.assertFalse(pr.needs_human_review)

    def test_malformed_payload_raises(self) -> None:
        with self.assertRaises(ValueError):
            pr_status.parse_pull_request("r/r", {"title": "no number"})


class RenderReportTest(unittest.TestCase):
    def _pr(self, number: int, *, human: bool) -> pr_status.PullRequest:
        return pr_status.PullRequest(
            repo="rrnewton/reverie",
            number=number,
            title=f"pr {number}",
            url=f"https://example/{number}",
            is_draft=False,
            labels=frozenset({"human-review"}) if human else frozenset(),
            ci_status="green",
        )

    def test_buckets_and_header(self) -> None:
        prs = [self._pr(1, human=True), self._pr(2, human=False)]
        report = pr_status.render_report(prs, 10, repos=("rrnewton/reverie",))
        self.assertIn("Open PR health: rrnewton/reverie", report)
        self.assertIn("Human review (1)", report)
        self.assertIn("Free to land: no human-review label (1)", report)
        self.assertIn("human-blocked: 1", report)
        self.assertIn("free-to-land:  1", report)


class ClassifyRunConclusionTest(unittest.TestCase):
    def test_success_is_pass(self) -> None:
        self.assertEqual(pr_status.classify_run_conclusion("success", "completed"), "pass")
        self.assertEqual(pr_status.classify_run_conclusion("SUCCESS", "COMPLETED"), "pass")

    def test_failure_family_is_fail(self) -> None:
        for concl in ("failure", "timed_out", "cancelled", "startup_failure", "stale"):
            self.assertEqual(
                pr_status.classify_run_conclusion(concl, "completed"),
                "fail",
                msg=concl,
            )

    def test_skipped_and_neutral(self) -> None:
        self.assertEqual(pr_status.classify_run_conclusion("skipped", "completed"), "skipped")
        self.assertEqual(pr_status.classify_run_conclusion("neutral", "completed"), "skipped")

    def test_no_conclusion_yet_is_pending(self) -> None:
        self.assertEqual(pr_status.classify_run_conclusion("", "in_progress"), "pending")
        self.assertEqual(pr_status.classify_run_conclusion(None, "queued"), "pending")
        self.assertEqual(pr_status.classify_run_conclusion("", "completed"), "pending")

    def test_unknown_conclusion_is_other(self) -> None:
        self.assertEqual(
            pr_status.classify_run_conclusion("something_new", "completed"), "other"
        )


class FormatRunTimeTest(unittest.TestCase):
    def test_iso_is_trimmed_to_minute(self) -> None:
        self.assertEqual(
            pr_status._format_run_time("2026-07-27T18:03:45Z"), "2026-07-27 18:03"
        )

    def test_missing_or_odd_shapes_pass_through(self) -> None:
        self.assertEqual(pr_status._format_run_time(""), "?")
        self.assertEqual(pr_status._format_run_time(None), "?")
        self.assertEqual(pr_status._format_run_time("not-a-date"), "not-a-date")


class ParseWorkflowRunTest(unittest.TestCase):
    def test_parse_and_short_sha_and_state(self) -> None:
        run = pr_status.parse_workflow_run(
            "rrnewton/hermit",
            {
                "headSha": "6cd2b1d4716d165fed5c46bbeadeceebde7c9754",
                "workflowName": "CI (GitHub-managed portable)",
                "name": "commit message title",
                "conclusion": "failure",
                "status": "completed",
                "createdAt": "2026-07-27T18:03:45Z",
            },
        )
        self.assertEqual(run.head_sha, "6cd2b1d4")
        self.assertEqual(run.workflow_name, "CI (GitHub-managed portable)")
        self.assertEqual(run.state, "fail")
        self.assertEqual(run.created_at_display, "2026-07-27 18:03")

    def test_workflow_name_falls_back_to_name(self) -> None:
        run = pr_status.parse_workflow_run(
            "rrnewton/hermit",
            {"headSha": "abc123", "name": "Docs", "conclusion": "success"},
        )
        self.assertEqual(run.workflow_name, "Docs")
        self.assertEqual(run.head_sha, "abc123")

    def test_malformed_payload_raises(self) -> None:
        with self.assertRaises(ValueError):
            pr_status.parse_workflow_run("r/r", ["not", "a", "dict"])


class RenderMainCiTest(unittest.TestCase):
    def _run(self, sha: str, name: str, concl: str, when: str) -> pr_status.WorkflowRun:
        return pr_status.WorkflowRun(
            repo="rrnewton/hermit",
            head_sha=sha,
            workflow_name=name,
            conclusion=concl,
            status="completed",
            created_at=when,
        )

    def test_empty_runs(self) -> None:
        report = pr_status.render_main_ci([], "rrnewton/hermit", 10)
        self.assertIn("Recent main CI: rrnewton/hermit (last 10 runs)", report)
        self.assertIn("(no runs found)", report)

    def test_counts_ordering_and_failure_highlight(self) -> None:
        runs = [
            self._run("aaaaaaaa", "CI (GitHub-managed portable)", "success", "2026-07-27T10:00:00Z"),
            self._run("bbbbbbbb", "CI (GitHub-managed portable)", "failure", "2026-07-27T12:00:00Z"),
            self._run("cccccccc", "Docs", "", "2026-07-27T13:00:00Z"),
        ]
        report = pr_status.render_main_ci(runs, "rrnewton/hermit", 10)
        # Newest first: the pending Docs run (13:00) precedes the failure (12:00).
        self.assertLess(report.index("cccccccc"), report.index("bbbbbbbb"))
        self.assertLess(report.index("bbbbbbbb"), report.index("aaaaaaaa"))
        self.assertIn("pass:        1", report)
        self.assertIn("fail:        1", report)
        self.assertIn("pending:     1", report)
        self.assertIn("runs shown:  3 across 3 commits", report)
        self.assertIn("FAILURES (1)", report)
        self.assertIn("FAIL", report)
        # The failing commit is called out in the FAILURES block with its reason.
        self.assertIn("bbbbbbbb CI (GitHub-managed portable) (failure)", report)

    def test_no_failures_omits_failure_block(self) -> None:
        runs = [self._run("aaaaaaaa", "Docs", "success", "2026-07-27T10:00:00Z")]
        report = pr_status.render_main_ci(runs, "rrnewton/hermit", 5)
        self.assertNotIn("FAILURES", report)


class ParseArgsMainCiTest(unittest.TestCase):
    def test_defaults(self) -> None:
        args = pr_status.parse_args([])
        self.assertEqual(args.main_limit, pr_status.DEFAULT_MAIN_LIMIT)
        self.assertFalse(args.no_main_ci)

    def test_no_main_ci_flag(self) -> None:
        args = pr_status.parse_args(["--no-main-ci"])
        self.assertTrue(args.no_main_ci)

    def test_main_limit_must_be_positive(self) -> None:
        with self.assertRaises(SystemExit):
            pr_status.parse_args(["--main-limit", "0"])


if __name__ == "__main__":
    unittest.main()
