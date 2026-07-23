#!/usr/bin/env python3
"""Report operational health for open Hermit and Reverie pull requests.

The report splits open PRs into two buckets:

* ``human-review`` -- carries the ``human-review`` label and must not be landed
  by an automated agent.
* ``free-to-land`` -- everything else; an agent may land these once CI is green.

By default both ``rrnewton/hermit`` and ``rrnewton/reverie`` are queried. Use
``-R``/``--repo`` (repeatable, gh-style) to target one or more specific repos,
for example ``pr_status.py -R rrnewton/reverie``.

All GitHub access goes through the ``with-proxy`` wrapper, which is required for
network egress on Meta devservers.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from typing import Sequence

DEFAULT_REPOS = ("rrnewton/hermit", "rrnewton/reverie")
DEFAULT_WARN_THRESHOLD = 10
HUMAN_REVIEW_LABEL = "human-review"

RED_CONCLUSIONS = frozenset(
    (
        "FAILURE",
        "TIMED_OUT",
        "CANCELLED",
        "ERROR",
        "ACTION_REQUIRED",
        "STARTUP_FAILURE",
        "STALE",
    )
)
PENDING_STATES = frozenset(
    ("PENDING", "EXPECTED", "QUEUED", "IN_PROGRESS", "WAITING", "REQUESTED")
)


@dataclass(frozen=True)
class PullRequest:
    repo: str
    number: int
    title: str
    url: str
    is_draft: bool
    labels: frozenset[str]
    ci_status: str

    @property
    def needs_human_review(self) -> bool:
        return HUMAN_REVIEW_LABEL in self.labels


def classify_ci_rollup(checks: object) -> str:
    """Classify a GitHub statusCheckRollup as green, red, pending, or none."""
    if not isinstance(checks, list) or not checks:
        return "none"

    saw_check = False
    saw_pending = False
    for check in checks:
        if not isinstance(check, dict):
            continue
        saw_check = True
        conclusion = str(check.get("conclusion") or check.get("state") or "").upper()
        status = str(check.get("status") or "").upper()

        if conclusion in RED_CONCLUSIONS:
            return "red"
        if (
            conclusion in PENDING_STATES
            or not conclusion
            or (status and status != "COMPLETED")
        ):
            saw_pending = True

    if not saw_check:
        return "none"
    return "pending" if saw_pending else "green"


def parse_pull_request(repo: str, raw: object) -> PullRequest:
    if not isinstance(raw, dict):
        raise ValueError(f"{repo}: expected PR object, got {type(raw).__name__}")

    labels_raw = raw.get("labels")
    labels = frozenset(
        str(label.get("name"))
        for label in labels_raw
        if isinstance(label, dict) and label.get("name")
    ) if isinstance(labels_raw, list) else frozenset()

    try:
        number = int(raw["number"])
        title = str(raw["title"])
        url = str(raw["url"])
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError(f"{repo}: malformed PR payload: {raw!r}") from error

    return PullRequest(
        repo=repo,
        number=number,
        title=" ".join(title.split()),
        url=url,
        is_draft=raw.get("isDraft") is True,
        labels=labels,
        ci_status=classify_ci_rollup(raw.get("statusCheckRollup")),
    )


def fetch_open_prs(repo: str) -> list[PullRequest]:
    command = [
        "with-proxy",
        "gh",
        "pr",
        "list",
        "-R",
        repo,
        "--state",
        "open",
        "--limit",
        "200",
        "--json",
        "number,title,url,isDraft,labels,statusCheckRollup",
    ]
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=False)
    except FileNotFoundError as error:
        raise RuntimeError(
            "with-proxy was not found; GitHub queries must use the proxy wrapper"
        ) from error

    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "unknown error"
        raise RuntimeError(f"{repo}: gh pr list failed: {detail}")

    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"{repo}: gh pr list returned invalid JSON") from error
    if not isinstance(payload, list):
        raise RuntimeError(f"{repo}: gh pr list returned a non-list payload")

    return [parse_pull_request(repo, raw) for raw in payload]


def _format_pr(pr: PullRequest) -> str:
    draft = "yes" if pr.is_draft else "no"
    return (
        f"  {pr.repo}#{pr.number:<4} ci={pr.ci_status:<7} draft={draft:<3} "
        f"{pr.title}\n"
        f"    {pr.url}"
    )


def render_report(
    prs: Sequence[PullRequest],
    warn_threshold: int,
    repos: Sequence[str] = DEFAULT_REPOS,
) -> str:
    human_review = sorted(
        (pr for pr in prs if pr.needs_human_review),
        key=lambda pr: (pr.repo, -pr.number),
    )
    free_to_land = sorted(
        (pr for pr in prs if not pr.needs_human_review),
        key=lambda pr: (pr.repo, -pr.number),
    )
    ci_failing = sum(pr.ci_status == "red" for pr in prs)

    lines = [
        f"Open PR health: {' + '.join(repos)}",
        "",
        f"Human review ({len(human_review)})",
    ]
    lines.extend(_format_pr(pr) for pr in human_review)
    if not human_review:
        lines.append("  (none)")

    lines.extend(("", f"Free to land: no human-review label ({len(free_to_land)})"))
    lines.extend(_format_pr(pr) for pr in free_to_land)
    if not free_to_land:
        lines.append("  (none)")

    lines.extend(
        (
            "",
            "Summary",
            f"  total open:    {len(prs)}",
            f"  human-blocked: {len(human_review)}",
            f"  free-to-land:  {len(free_to_land)}",
            f"  CI-failing:    {ci_failing}",
        )
    )

    if len(free_to_land) > warn_threshold:
        lines.extend(
            (
                "",
                "WARNING: "
                f"{len(free_to_land)} free-to-land PRs exceeds the "
                f"{warn_threshold} PR threshold; prioritize CI repair, review, and landing.",
            )
        )
    return "\n".join(lines)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Report open-PR landing health for one or more GitHub repos.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "-R",
        "--repo",
        dest="repos",
        action="append",
        metavar="OWNER/REPO",
        help=(
            "GitHub OWNER/REPO to query; repeat to query several. "
            f"Defaults to {' and '.join(DEFAULT_REPOS)}."
        ),
    )
    parser.add_argument(
        "--warn-threshold",
        type=int,
        default=DEFAULT_WARN_THRESHOLD,
        help=f"warn above this free-to-land count (default: {DEFAULT_WARN_THRESHOLD})",
    )
    args = parser.parse_args(argv)
    if args.warn_threshold < 0:
        parser.error("--warn-threshold must be non-negative")
    for repo in args.repos or ():
        if repo.count("/") != 1 or not all(repo.split("/")):
            parser.error(f"--repo expects OWNER/REPO, got {repo!r}")
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    repos = tuple(args.repos) if args.repos else DEFAULT_REPOS

    try:
        prs = [pr for repo in repos for pr in fetch_open_prs(repo)]
    except (RuntimeError, ValueError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    print(render_report(prs, args.warn_threshold, repos))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
