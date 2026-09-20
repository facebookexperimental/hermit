#!/usr/bin/env python3
"""One definition of the review labels written and read by ci-hub."""

from __future__ import annotations

import argparse
import json
import re
from collections.abc import Callable, Iterable, Sequence
from pathlib import Path
import subprocess
import sys


SUPPORTED_REPOS = ("rrnewton/hermit", "rrnewton/reverie")
REVIEW_FAMILIES = ("codex", "claude")
REVIEW_ROUNDS = (1, 2, 3, 4)
POST_FACTO_LABEL = "post-facto-human-review"
PASSED_REVIEW_LABELS = {
    "codex": "passed-review-codex",
    "claude": "passed-review-claude",
}
GH = Path(__file__).resolve().parent / "bin/gh"

_ROUND_ALTERNATION = "|".join(str(round_number) for round_number in REVIEW_ROUNDS)
REVIEW_ROUND_LABEL = re.compile(
    rf"^adversarial-review-(?P<reviewer>{'|'.join(REVIEW_FAMILIES)})"
    rf"(?P<round>{_ROUND_ALTERNATION})$"
)


def round_label(family: str, round_number: int) -> str:
    """Return one accepted numbered review-activity label."""
    if family not in REVIEW_FAMILIES:
        raise ValueError(f"unknown review family: {family}")
    if round_number not in REVIEW_ROUNDS:
        raise ValueError(
            "review round must be one of "
            + ", ".join(str(value) for value in REVIEW_ROUNDS)
        )
    return f"adversarial-review-{family}{round_number}"


def round_label_prefix(family: str) -> str:
    """Return the prefix shared by accepted and malformed round labels."""
    if family not in REVIEW_FAMILIES:
        raise ValueError(f"unknown review family: {family}")
    return f"adversarial-review-{family}"


def round_labels(family: str) -> tuple[str, ...]:
    """Return every accepted numbered label for one review family."""
    return tuple(round_label(family, value) for value in REVIEW_ROUNDS)


def parse_round_label(label: str) -> tuple[str, int] | None:
    """Return the review family and round for an exact accepted label."""
    match = REVIEW_ROUND_LABEL.fullmatch(label)
    if match is None:
        return None
    return match.group("reviewer"), int(match.group("round"))


def missing_round_requirement(family: str) -> str:
    """Spell the exact label alternatives accepted for one family."""
    return "one of " + ", ".join(round_labels(family))


def required_repository_labels() -> tuple[str, ...]:
    """Return every label required for the protocol in each supported repo."""
    labels = [POST_FACTO_LABEL]
    for family in REVIEW_FAMILIES:
        labels.extend(round_labels(family))
        labels.append(PASSED_REVIEW_LABELS[family])
    return tuple(labels)


def repository_label_gaps(
    labels_for_repo: Callable[[str], Iterable[str]],
) -> tuple[str, ...]:
    """Name every contract label absent from either supported repository."""
    required = set(required_repository_labels())
    missing: list[str] = []
    for repo in SUPPORTED_REPOS:
        available = set(labels_for_repo(repo))
        missing.extend(f"{repo}:{label}" for label in sorted(required - available))
    return tuple(missing)


def lint_records() -> tuple[str, ...]:
    """Return tab-separated records consumed by Hermit's shell lint."""
    records = [f"post-facto\t{POST_FACTO_LABEL}"]
    for family in REVIEW_FAMILIES:
        records.append(
            "\t".join(
                (family, PASSED_REVIEW_LABELS[family], ",".join(round_labels(family)))
            )
        )
    return tuple(records)


def _live_labels(repo: str) -> set[str]:
    result = subprocess.run(
        [str(GH), "label", "list", "--repo", repo, "--limit", "200", "--json", "name"],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()[:800]
        raise RuntimeError(f"cannot read labels for {repo}: {detail}")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"label list for {repo} is malformed JSON") from error
    if not isinstance(value, list):
        raise RuntimeError(f"label list for {repo} is not an array")
    return {
        str(item["name"])
        for item in value
        if isinstance(item, dict) and isinstance(item.get("name"), str)
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--format",
        choices=("lint-records",),
        default="lint-records",
    )
    parser.add_argument("--check-repositories", action="store_true")
    args = parser.parse_args(argv)
    if args.check_repositories:
        try:
            missing = repository_label_gaps(_live_labels)
        except RuntimeError as error:
            print(f"review-contract: UNAVAILABLE: {error}", file=sys.stderr)
            return 2
        if missing:
            print(
                "review-contract: REFUSED: required labels absent: "
                + ", ".join(missing),
                file=sys.stderr,
            )
            return 1
        print(
            "review-contract: all accepted review labels exist in "
            + ", ".join(SUPPORTED_REPOS)
        )
        return 0
    print("\n".join(lint_records()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
