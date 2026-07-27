#!/usr/bin/env python3
"""Report operational health for open Hermit and Reverie pull requests.

The report splits open PRs into two buckets:

* ``human-review`` -- carries the ``human-review`` label and must not be landed
  by an automated agent.
* ``free-to-land`` -- everything else; an agent may land these once CI is green.

After the open-PR buckets the report also prints a ``Recent main CI`` section
per repo: the CI conclusions of the most recent workflow runs on the ``main``
branch (like the checkmark column at ``github.com/<repo>/commits/main``), so
main-branch health is visible without opening the browser. Failing runs are
called out explicitly. Use ``--main-limit`` to change how many runs are shown
and ``--no-main-ci`` to omit the section entirely.

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
from urllib.parse import urlparse

DEFAULT_REPOS = ("rrnewton/hermit", "rrnewton/reverie")
DEFAULT_WARN_THRESHOLD = 10
DEFAULT_MAIN_LIMIT = 10
HUMAN_REVIEW_LABEL = "human-review"
REGULAR_HOSTED_CHECK = "Regular tests (GitHub-hosted)"
REQUIRED_CHECKS = {
    "rrnewton/hermit": (REGULAR_HOSTED_CHECK,),
    "rrnewton/reverie": (
        REGULAR_HOSTED_CHECK,
        "Host-dependent tests (self-hosted)",
    ),
}

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


def _check_sort_key(check: dict[object, object], index: int) -> tuple[int, str, int]:
    details_url = str(check.get("detailsUrl") or "")
    path_parts = [part for part in urlparse(details_url).path.split("/") if part]
    try:
        runs_index = path_parts.index("runs")
        run_id = int(path_parts[runs_index + 1])
    except (ValueError, IndexError):
        run_id = -1
    timestamp = str(
        check.get("startedAt")
        or check.get("createdAt")
        or check.get("completedAt")
        or ""
    )
    return run_id, timestamp, index


def classify_ci_rollup(repo: str, checks: object) -> str:
    """Classify the latest authoritative checks as green, red, pending, or none.

    GitHub retains older reruns and auxiliary checks in ``statusCheckRollup``.
    In particular, Hermit's merge gate intentionally starts red and refires
    after hosted CI completes. Those historical placeholders must not turn a
    hosted-green pull request red in this operational report.
    """
    if not isinstance(checks, list) or not checks:
        return "none"

    required = REQUIRED_CHECKS.get(repo, (REGULAR_HOSTED_CHECK,))
    latest: dict[str, tuple[tuple[int, str, int], dict[object, object]]] = {}
    for index, check in enumerate(checks):
        if not isinstance(check, dict):
            continue
        name = str(check.get("name") or check.get("context") or "")
        if name not in required:
            continue
        sort_key = _check_sort_key(check, index)
        previous = latest.get(name)
        if previous is None or sort_key > previous[0]:
            latest[name] = (sort_key, check)

    if not latest:
        return "none"

    saw_pending = len(latest) != len(required)
    for _, check in latest.values():
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
        ci_status=classify_ci_rollup(repo, raw.get("statusCheckRollup")),
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


@dataclass(frozen=True)
class WorkflowRun:
    """One workflow run reported by ``gh run list`` for a branch."""

    repo: str
    head_sha: str
    workflow_name: str
    conclusion: str
    status: str
    created_at: str

    @property
    def state(self) -> str:
        return classify_run_conclusion(self.conclusion, self.status)

    @property
    def created_at_display(self) -> str:
        return _format_run_time(self.created_at)


def _format_run_time(created_at: object) -> str:
    """Render an ISO-8601 timestamp as ``YYYY-MM-DD HH:MM`` without parsing.

    Keeping this string-based avoids a timezone dependency and stays robust to
    whatever precision GitHub returns; unexpected shapes pass through verbatim.
    """
    text = str(created_at or "")
    if len(text) >= 16 and text[10] == "T":
        return text[:16].replace("T", " ")
    return text or "?"


def classify_run_conclusion(conclusion: object, status: object) -> str:
    """Map a run's ``conclusion``/``status`` to pass/fail/pending/skipped/other.

    A run with no conclusion yet (empty conclusion, or a non-completed status)
    is ``pending``. ``RED_CONCLUSIONS`` (failure, timed out, cancelled, ...)
    are ``fail`` so they can be highlighted; ``success`` is ``pass``.
    """
    concl = str(conclusion or "").upper()
    stat = str(status or "").upper()
    if concl == "SUCCESS":
        return "pass"
    if concl in RED_CONCLUSIONS:
        return "fail"
    if concl in ("SKIPPED", "NEUTRAL"):
        return "skipped"
    if not concl or (stat and stat != "COMPLETED"):
        return "pending"
    return "other"


def parse_workflow_run(repo: str, raw: object) -> WorkflowRun:
    if not isinstance(raw, dict):
        raise ValueError(f"{repo}: expected run object, got {type(raw).__name__}")
    head_sha = str(raw.get("headSha") or "")[:8] or "????????"
    workflow_name = str(raw.get("workflowName") or raw.get("name") or "?")
    return WorkflowRun(
        repo=repo,
        head_sha=head_sha,
        workflow_name=" ".join(workflow_name.split()),
        conclusion=str(raw.get("conclusion") or ""),
        status=str(raw.get("status") or ""),
        created_at=str(raw.get("createdAt") or ""),
    )


def fetch_main_runs(repo: str, limit: int) -> list[WorkflowRun]:
    command = [
        "with-proxy",
        "gh",
        "run",
        "list",
        "-R",
        repo,
        "--branch",
        "main",
        "--limit",
        str(limit),
        "--json",
        "conclusion,name,workflowName,headSha,createdAt,status",
    ]
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=False)
    except FileNotFoundError as error:
        raise RuntimeError(
            "with-proxy was not found; GitHub queries must use the proxy wrapper"
        ) from error

    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "unknown error"
        raise RuntimeError(f"{repo}: gh run list failed: {detail}")

    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"{repo}: gh run list returned invalid JSON") from error
    if not isinstance(payload, list):
        raise RuntimeError(f"{repo}: gh run list returned a non-list payload")

    return [parse_workflow_run(repo, raw) for raw in payload]


_STATE_MARKER = {
    "pass": "ok",
    "fail": "FAIL",
    "pending": "...",
    "skipped": "skip",
    "other": "?",
}


def _format_run(run: WorkflowRun) -> str:
    marker = _STATE_MARKER.get(run.state, run.state)
    return (
        f"  {marker:<4} {run.head_sha:<8} {run.created_at_display:<16} "
        f"{run.workflow_name}"
    )


def render_main_ci(runs: Sequence[WorkflowRun], repo: str, limit: int) -> str:
    """Render recent main-branch CI runs, newest first, highlighting failures."""
    lines = [f"Recent main CI: {repo} (last {limit} runs)"]
    if not runs:
        lines.append("  (no runs found)")
        return "\n".join(lines)

    ordered = sorted(runs, key=lambda run: run.created_at, reverse=True)
    lines.extend(_format_run(run) for run in ordered)

    passing = sum(run.state == "pass" for run in runs)
    failing = sum(run.state == "fail" for run in runs)
    pending = sum(run.state == "pending" for run in runs)
    skipped = sum(run.state == "skipped" for run in runs)
    commits = len({run.head_sha for run in runs})
    lines.extend(
        (
            "",
            "Summary",
            f"  runs shown:  {len(runs)} across {commits} commits",
            f"  pass:        {passing}",
            f"  fail:        {failing}",
            f"  pending:     {pending}",
            f"  skipped:     {skipped}",
        )
    )

    failures = [run for run in ordered if run.state == "fail"]
    if failures:
        lines.append(f"\nFAILURES ({len(failures)})")
        for run in failures:
            detail = run.conclusion or "no conclusion"
            lines.append(
                f"  {run.head_sha} {run.workflow_name} "
                f"({detail}) {run.created_at_display}"
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
    parser.add_argument(
        "--main-limit",
        type=int,
        default=DEFAULT_MAIN_LIMIT,
        help=(
            "number of recent main-branch CI runs to show per repo "
            f"(default: {DEFAULT_MAIN_LIMIT})"
        ),
    )
    parser.add_argument(
        "--no-main-ci",
        action="store_true",
        help="skip the recent main-branch CI section",
    )
    args = parser.parse_args(argv)
    if args.warn_threshold < 0:
        parser.error("--warn-threshold must be non-negative")
    if args.main_limit < 1:
        parser.error("--main-limit must be positive")
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

    if not args.no_main_ci:
        for repo in repos:
            try:
                runs = fetch_main_runs(repo, args.main_limit)
            except (RuntimeError, ValueError) as error:
                print(f"ERROR: {error}", file=sys.stderr)
                return 2
            print()
            print(render_main_ci(runs, repo, args.main_limit))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
