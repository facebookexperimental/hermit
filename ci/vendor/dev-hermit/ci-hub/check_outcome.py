#!/usr/bin/env python3
"""Canonical three-state interpretation of CI check status and conclusion.

A missing answer is not a bad answer and is not a passing answer.  Every caller
must preserve that distinction:

* PASSED: a completed check with conclusion ``success``.
* FAILED: a completed check with a genuine failing conclusion.
* NO_RESULT: cancelled, skipped, neutral, stale, pending, absent, or unknown.

Unknown values deliberately fail into NO_RESULT.  That blocks admission while
remaining recoverable by re-dispatch, and prevents a future GitHub conclusion
from silently becoming either green or red.
"""

from __future__ import annotations

import argparse
from enum import Enum
import json
import re
import sys
from typing import Mapping, Sequence


class CheckOutcome(str, Enum):
    PASSED = "PASSED"
    FAILED = "FAILED"
    NO_RESULT = "NO_RESULT"


PASS_CONCLUSIONS = frozenset(("success",))
FAIL_CONCLUSIONS = frozenset(("failure", "timed_out", "error", "startup_failure"))

_RUN_URL = re.compile(r"/actions/runs/(\d+)(?:/|$)")


def _text(value: object) -> str:
    return str(value or "").strip()


def _check_context(check: Mapping[str, object]) -> str:
    return _text(check.get("name") or check.get("context"))


def _check_head(check: Mapping[str, object]) -> str:
    return _text(
        check.get("headSha")
        or check.get("head_sha")
        or check.get("headRefOid")
    )


def _run_id(check: Mapping[str, object]) -> int:
    for key in ("runId", "run_id"):
        if not check.get(key):
            continue
        try:
            return int(check[key])
        except (TypeError, ValueError):
            pass
    url = _text(
        check.get("detailsUrl")
        or check.get("details_url")
        or check.get("url")
        or check.get("html_url")
    )
    match = _RUN_URL.search(url)
    if match:
        return int(match.group(1))
    return 0


def _timestamp(check: Mapping[str, object]) -> str:
    # A queued CheckRun can expose startedAt=0001-01-01. Treat that sentinel as
    # absent so it cannot make a newer queued run look older than a completed
    # predecessor. Run IDs remain the primary ordering key when available.
    for key in ("createdAt", "created_at", "startedAt", "started_at", "completedAt"):
        value = _text(check.get(key))
        if value and not value.startswith("0001-01-01"):
            return value
    return ""


def _ambiguous_check(context: str) -> dict[str, object]:
    return {
        "name": context,
        "status": "AMBIGUOUS",
        "conclusion": "",
        "_selectionError": (
            "duplicate check context has equal ordering identity and contrary verdicts"
        ),
    }


def select_latest_checks(value: object, *, head_sha: str = "") -> list[dict[str, object]]:
    """Return one deterministically newest check per context.

    ``statusCheckRollup`` can retain multiple attempts for the same required
    context. A consumer that classifies the first or every entry can obtain
    opposite verdicts from the same exact head. Entries carrying a head SHA are
    first restricted to ``head_sha``. Newness is then determined by Actions run
    ID (monotonic and present even while queued), timestamp, and only finally by
    input position for byte-identical duplicates. If two contrary duplicates
    have the same ordering identity, the result is an explicit NO_RESULT-shaped
    ambiguity instead of a list-order coin flip.

    GitHub's PR ``statusCheckRollup`` is scoped to the queried current PR head,
    so entries without their own head field remain eligible. Callers must still
    supply the PR's exact ``headRefOid`` so any head-bearing entry is checked.
    """
    if isinstance(value, Mapping):
        value = value.get("statusCheckRollup", value.get("check_runs", []))
    if not isinstance(value, list):
        return []

    latest: dict[str, tuple[tuple[int, str], int, dict[str, object]]] = {}
    order: list[str] = []
    for index, raw in enumerate(value):
        if not isinstance(raw, Mapping):
            continue
        check = {str(key): item for key, item in raw.items()}
        observed_head = _check_head(check)
        if head_sha and observed_head and observed_head != head_sha:
            continue
        context = _check_context(check)
        # Nameless entries cannot collide meaningfully; preserve them so their
        # state still blocks rather than disappearing from the rollup.
        key = context or f"\0unnamed-{index}"
        if key not in latest:
            order.append(key)
            latest[key] = ((_run_id(check), _timestamp(check)), index, check)
            continue

        previous_key, previous_index, previous = latest[key]
        candidate_key = (_run_id(check), _timestamp(check))
        if candidate_key > previous_key:
            latest[key] = (candidate_key, index, check)
        elif candidate_key == previous_key:
            same_verdict = (
                _text(previous.get("status")) == _text(check.get("status"))
                and _text(previous.get("conclusion") or previous.get("state"))
                == _text(check.get("conclusion") or check.get("state"))
            )
            if not same_verdict:
                latest[key] = (
                    candidate_key,
                    max(previous_index, index),
                    _ambiguous_check(context),
                )
            elif same_verdict and index > previous_index:
                latest[key] = (candidate_key, index, check)
    return [latest[key][2] for key in order]


def select_latest_workflow_run(
    value: object,
    *,
    head_sha: str,
    events: Sequence[str] = (),
) -> dict[str, object]:
    """Select the latest Actions run at one exact head, with a stable ID tie-break."""
    candidates = select_latest_workflow_attempts(
        value, head_sha=head_sha, events=events
    )
    if not candidates:
        return {}
    return max(candidates, key=_workflow_order_key)


def _workflow_head(run: Mapping[str, object]) -> str:
    return _text(run.get("head_sha") or run.get("headSha"))


def _workflow_name(run: Mapping[str, object]) -> str:
    return _text(run.get("workflowName") or run.get("workflow_name") or run.get("name"))


def _workflow_order_key(run: Mapping[str, object]) -> tuple[str, int]:
    try:
        run_id = int(run.get("id") or run.get("databaseId") or run.get("run_id") or 0)
    except (TypeError, ValueError):
        run_id = 0
    return _text(run.get("created_at") or run.get("createdAt")), run_id


def select_latest_workflow_attempts(
    value: object,
    *,
    head_sha: str = "",
    events: Sequence[str] = (),
    workflows: Sequence[str] = (),
) -> list[dict[str, object]]:
    """Return one latest attempt per exact ``(head, workflow)`` authority."""
    if isinstance(value, Mapping):
        value = value.get("workflow_runs", [])
    if not isinstance(value, list):
        return []
    allowed_events = set(events)
    allowed_workflows = set(workflows)
    latest: dict[tuple[str, str], dict[str, object]] = {}
    order: list[tuple[str, str]] = []
    for raw in value:
        if not isinstance(raw, Mapping):
            continue
        run = {str(key): item for key, item in raw.items()}
        run_head = _workflow_head(run)
        workflow = _workflow_name(run)
        if head_sha and run_head != head_sha:
            continue
        if allowed_events and _text(run.get("event")) not in allowed_events:
            continue
        if allowed_workflows and workflow not in allowed_workflows:
            continue
        key = (run_head, workflow)
        previous = latest.get(key)
        if previous is None:
            order.append(key)
            latest[key] = run
            continue
        candidate_key = _workflow_order_key(run)
        previous_key = _workflow_order_key(previous)
        if candidate_key > previous_key:
            latest[key] = run
        elif candidate_key == previous_key:
            same_verdict = (
                _text(previous.get("status")) == _text(run.get("status"))
                and _text(previous.get("conclusion")) == _text(run.get("conclusion"))
            )
            if not same_verdict:
                ambiguous = dict(previous)
                ambiguous.update(
                    {
                        "status": "AMBIGUOUS",
                        "conclusion": "",
                        "_selectionError": (
                            "duplicate workflow attempt has equal identity and contrary verdicts"
                        ),
                    }
                )
                latest[key] = ambiguous
    return [latest[key] for key in order]


def classify_check(
    status: object,
    conclusion: object,
    *,
    self_timeout: bool = False,
) -> CheckOutcome:
    """Classify one check without forcing absence into a Boolean result."""
    normalized_status = str(status or "").strip().lower()
    normalized_conclusion = str(conclusion or "").strip().lower()

    if self_timeout:
        return CheckOutcome.FAILED
    if normalized_status and normalized_status != "completed":
        return CheckOutcome.NO_RESULT
    if normalized_conclusion in PASS_CONCLUSIONS:
        return CheckOutcome.PASSED
    if normalized_conclusion in FAIL_CONCLUSIONS:
        return CheckOutcome.FAILED
    return CheckOutcome.NO_RESULT


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--status", default="")
    parser.add_argument("--conclusion", default="")
    parser.add_argument("--self-timeout", action="store_true")
    parser.add_argument("--select-latest-rollup", action="store_true")
    parser.add_argument("--select-latest-run", action="store_true")
    parser.add_argument("--head-sha", default="")
    parser.add_argument("--event", action="append", default=[])
    args = parser.parse_args(argv)
    if args.select_latest_rollup and args.select_latest_run:
        parser.error("select exactly one latest-result mode")
    if args.select_latest_rollup:
        json.dump(
            select_latest_checks(json.load(sys.stdin), head_sha=args.head_sha),
            sys.stdout,
        )
        print()
        return 0
    if args.select_latest_run:
        if not args.head_sha:
            parser.error("--select-latest-run requires --head-sha")
        json.dump(
            select_latest_workflow_run(
                json.load(sys.stdin), head_sha=args.head_sha, events=args.event
            ),
            sys.stdout,
        )
        print()
        return 0
    print(
        classify_check(
            args.status,
            args.conclusion,
            self_timeout=args.self_timeout,
        ).value
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
