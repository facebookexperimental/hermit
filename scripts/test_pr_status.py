#!/usr/bin/env python3
"""Offline unit tests for pr_status.py (no network required).

Run with: python3 scripts/test_pr_status.py
"""

from __future__ import annotations

import unittest

import pr_status


class ClassifyCiRollupTest(unittest.TestCase):
    def test_empty_or_missing_is_none(self) -> None:
        self.assertEqual(pr_status.classify_ci_rollup([]), "none")
        self.assertEqual(pr_status.classify_ci_rollup(None), "none")

    def test_failure_conclusion_is_red(self) -> None:
        checks = [
            {"conclusion": "SUCCESS", "status": "COMPLETED"},
            {"conclusion": "FAILURE", "status": "COMPLETED"},
        ]
        self.assertEqual(pr_status.classify_ci_rollup(checks), "red")

    def test_incomplete_status_is_pending(self) -> None:
        checks = [{"conclusion": "", "status": "IN_PROGRESS"}]
        self.assertEqual(pr_status.classify_ci_rollup(checks), "pending")

    def test_all_success_is_green(self) -> None:
        checks = [{"conclusion": "SUCCESS", "status": "COMPLETED"}]
        self.assertEqual(pr_status.classify_ci_rollup(checks), "green")


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
