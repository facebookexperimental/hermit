#!/usr/bin/env python3
"""Load the content-pinned ci-hub review-label contract."""

from __future__ import annotations

import argparse
from functools import cache
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
from types import ModuleType
from typing import Callable, Sequence, cast


AUTHORITY_COMMIT = "9f9517bb94354c307de7324d507ff24af7974560"
AUTHORITY_SHA256 = "1b0d798e55f8a5976a4334255a5e7de1792a79e81c6d27034d31d11d4294c18d"
AUTHORITY_RELATIVE_PATH = Path("ci-hub/review_contract.py")
AUTHORITY_API_PATH = (
    "repos/rrnewton/dev-hermit/contents/ci-hub/review_contract.py"
    f"?ref={AUTHORITY_COMMIT}"
)


def _candidate_authorities() -> list[Path]:
    if parent := os.environ.get("DEV_HERMIT_PARENT"):
        return [Path(parent) / AUTHORITY_RELATIVE_PATH]

    # Hosted runners cannot read the private parent repository. Keep the exact
    # pinned bytes locally and continue to authenticate them by digest below.
    candidates = [
        Path(__file__).resolve().parents[1]
        / "ci/vendor/dev-hermit/ci-hub/review_contract.py"
    ]
    for start in (Path(__file__).resolve(), Path.cwd().resolve()):
        candidates.extend(parent / AUTHORITY_RELATIVE_PATH for parent in start.parents)
    return candidates


#: Exit status for "could not consult the contract at all". Mirrors
#: check_outcome_adapter.EXIT_AUTHORITY_UNAVAILABLE deliberately: one spelling
#: across both pinned-authority adapters, so a caller learns the rule once.
EXIT_AUTHORITY_UNAVAILABLE = 3


class AuthorityUnavailable(RuntimeError):
    """The contract could not be OBTAINED. Says nothing about any pull request.

    ⚠️ NOT A VERDICT. Measured 2026-09-04 on the host recorded for this
    measurement in ``docs/TESTING_ENVIRONMENTS.md`` under "Named measurement
    hosts": with `gh` unreachable this raised an ordinary RuntimeError, `make
    lint-checks` exited 1, and the validation DAG recorded check.lint_checks as
    FAILED -- an outage reported as a code defect.
    """


class AuthorityRefused(RuntimeError):
    """Reached and answered no. Not an outage; see check_outcome_adapter."""


# ⚠️ IMPORTED, NOT COPIED. Two lists of transport signatures is exactly where one
# gets a new entry and the other does not, and the consequence of the stale copy
# is a silent skip.
from check_outcome_adapter import _is_transport_failure  # noqa: E402


class AuthorityIntegrityError(RuntimeError):
    """Obtained and does NOT match the pin. A refusal; still fails closed."""


def _fetch_pinned_source() -> bytes:
    """Fetch the private parent file through authenticated GitHub tooling."""
    proxy = shutil.which("with-proxy")
    gh = shutil.which("gh")
    if proxy:
        command = [proxy, "gh"]
    elif gh:
        command = [gh]
    else:
        raise AuthorityUnavailable(
            "cannot fetch the pinned review-label contract: gh is unavailable; "
            "set DEV_HERMIT_PARENT to a dev-hermit checkout"
        )
    command.extend(
        [
            "api",
            AUTHORITY_API_PATH,
            "-H",
            "Accept: application/vnd.github.raw+json",
        ]
    )
    result = subprocess.run(command, capture_output=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip() or "no error output"
        if _is_transport_failure(detail):
            raise AuthorityUnavailable(
                "cannot fetch the pinned review-label contract: " f"{detail}"
            )
        raise AuthorityRefused(
            "the pinned review-label contract was reached and refused: "
            f"{detail}. This is not an outage and is not being skipped"
        )
    return result.stdout


def _verified_source() -> bytes:
    for candidate in _candidate_authorities():
        if candidate.is_file():
            source = candidate.read_bytes()
            if hashlib.sha256(source).hexdigest() == AUTHORITY_SHA256:
                return source

    source = _fetch_pinned_source()
    digest = hashlib.sha256(source).hexdigest()
    if digest != AUTHORITY_SHA256:
        raise AuthorityIntegrityError(
            "canonical review-label contract digest mismatch: "
            f"expected {AUTHORITY_SHA256}, got {digest}"
        )
    return source


def _load_authority() -> ModuleType:
    name = "ci_hub_review_contract_authority"
    source_name = f"{AUTHORITY_API_PATH}@{AUTHORITY_SHA256}"
    module = ModuleType(name)
    module.__file__ = source_name
    sys.modules[name] = module
    try:
        exec(compile(_verified_source(), source_name, "exec"), module.__dict__)
    except BaseException:
        sys.modules.pop(name, None)
        raise
    return module


@cache
def _authority() -> ModuleType:
    """Load the contract only when a caller first requests its records."""
    return _load_authority()


def lint_records() -> tuple[str, ...]:
    """Return validated tab-separated records for the Hermit shell lint."""
    producer = cast(Callable[[], object], _authority().lint_records)
    records = producer()
    if not isinstance(records, tuple) or not records:
        raise RuntimeError("canonical review-label contract returned no lint records")
    if not all(isinstance(record, str) and record for record in records):
        raise RuntimeError("canonical review-label contract returned malformed lint records")
    return cast(tuple[str, ...], records)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--materialize-authority",
        metavar="DIR",
        default="",
        help=(
            "write the verified authority under DIR at its pinned relative "
            "path and exit, so later invocations can read it locally via "
            "DEV_HERMIT_PARENT instead of fetching it again"
        ),
    )
    parser.add_argument("--format", choices=("lint-records",), default="lint-records")
    args = parser.parse_args(argv)
    # Nothing on stdout when the contract cannot be consulted: callers read
    # stdout as the records. The distinction rides the exit status.
    try:
        if args.materialize_authority:
            # Same purpose as in check_outcome_adapter: one fetch per checker
            # instead of one per process, which is what removes the window the
            # codex lane reproduced. Digest-verified before writing.
            target = Path(args.materialize_authority) / AUTHORITY_RELATIVE_PATH
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(_verified_source())
            return 0
        records = lint_records()
    except AuthorityUnavailable as unavailable:
        print(
            f"COULD-NOT-DETERMINE: {unavailable}. state: NO SIGNAL -- the "
            "review-label contract was not consulted, so this says nothing "
            "about any pull request and is not a failing verdict.",
            file=sys.stderr,
        )
        return EXIT_AUTHORITY_UNAVAILABLE
    print("\n".join(records))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
