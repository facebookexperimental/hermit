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
                "name": pr_status.REGULAR_HOSTED_CHECK,
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
                "name": pr_status.REGULAR_HOSTED_CHECK,
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
                "name": pr_status.REGULAR_HOSTED_CHECK,
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
                "name": pr_status.REGULAR_HOSTED_CHECK,
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
                "name": pr_status.REGULAR_HOSTED_CHECK,
                "conclusion": "FAILURE",
                "status": "COMPLETED",
                "startedAt": "2026-07-26T12:00:00Z",
            },
            {
                "name": pr_status.REGULAR_HOSTED_CHECK,
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
                "name": pr_status.REGULAR_HOSTED_CHECK,
                "conclusion": "SUCCESS",
                "status": "COMPLETED",
                "startedAt": "2026-07-26T12:00:00Z",
                "detailsUrl": "https://github.com/o/r/actions/runs/10/job/1",
            },
            {
                "name": pr_status.REGULAR_HOSTED_CHECK,
                "conclusion": "",
                "status": "QUEUED",
                "startedAt": "0001-01-01T00:00:00Z",
                "detailsUrl": "https://github.com/o/r/actions/runs/11/job/2",
            },
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "pending"
        )

    def test_hermit_self_hosted_failure_is_nonblocking(self) -> None:
        checks = [
            {
                "name": pr_status.REGULAR_HOSTED_CHECK,
                "conclusion": "SUCCESS",
                "status": "COMPLETED",
            },
            {
                "name": "PMU and CPUID tests (self-hosted)",
                "conclusion": "FAILURE",
                "status": "COMPLETED",
            },
        ]
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/hermit", checks), "green"
        )

    def test_reverie_requires_hosted_and_host_dependent_checks(self) -> None:
        hosted = {
            "name": pr_status.REGULAR_HOSTED_CHECK,
            "conclusion": "SUCCESS",
            "status": "COMPLETED",
        }
        host_dependent = {
            "name": "Host-dependent tests (self-hosted)",
            "conclusion": "SUCCESS",
            "status": "COMPLETED",
        }
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/reverie", [hosted]), "pending"
        )
        self.assertEqual(
            pr_status.classify_ci_rollup(
                "rrnewton/reverie", [hosted, host_dependent]
            ),
            "green",
        )

        failed = {**host_dependent, "conclusion": "FAILURE"}
        self.assertEqual(
            pr_status.classify_ci_rollup("rrnewton/reverie", [hosted, failed]),
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


if __name__ == "__main__":
    unittest.main()
