#!/usr/bin/env python3
"""Load the content-pinned ci-hub check-outcome authority."""

from __future__ import annotations

import argparse
from functools import cache
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from types import ModuleType
from typing import Callable, Protocol, Sequence, cast


#: Exit status for "could not consult the authority at all". Distinct from 0
#: (a classification was produced), from 1 (an ordinary error), and from 2
#: (argparse's usage error) so a caller can tell an outage from a verdict and
#: from its own bad arguments. Reusing any of those is the defect this code
#: exists to avoid.
EXIT_AUTHORITY_UNAVAILABLE = 3


class AuthorityUnavailable(RuntimeError):
    """The authority could not be OBTAINED. Says nothing about any check.

    ⚠️ THIS IS NOT A VERDICT AND MUST NEVER BE RENDERED AS ONE. Measured
    2026-09-04 on the host recorded for this measurement in
    ``docs/TESTING_ENVIRONMENTS.md`` under "Named measurement hosts": a GitHub
    ``HTTP 504`` while fetching the pinned authority made ``make lint-checks``
    exit 2, which the validation DAG recorded as gate ``check.lint_checks``
    FAILED. Two gates went red on four consecutive hourly runs of a tree that
    had passed 267/267 earlier the same day, so an outage was reported as a code
    defect on every lane at once.

    In particular this is NOT ``NO_RESULT``. ``NO_RESULT`` is the authority's
    own statement ABOUT a check -- cancelled, skipped, neutral, stale, pending,
    absent or unknown -- and it deliberately blocks admission. Folding "we could
    not ask" into it would put a transport failure behind a value that reads as
    an answer, which is the same shape as a red that means never-selected.
    """


class AuthorityRefused(RuntimeError):
    """The authority was REACHED and the answer was no.

    ⚠️ NOT AN OUTAGE, AND CONFLATING THE TWO IS THE INVERSION OF THIS FILE'S
    WHOLE PURPOSE. Measured 2026-09-04: `gh` returning ``Not Found (HTTP 404)``
    -- the pinned commit or path gone -- came back as AuthorityUnavailable, so
    every guarded checker printed a no-result marker and exited 0. A revoked
    credential (403) did the same. Those are refusals: the request arrived and
    was answered. Reporting them as "could not reach it" is the mirror of
    reporting a 504 as a refusal, and it is worse, because it is silent.
    """


def _is_transport_failure(detail: str) -> bool:
    """Whether `gh`'s failure is POSITIVELY identifiable as transport.

    ⚠️ DEFAULT LOUD. Anything not matched here is treated as a refusal and fails
    the checker rather than skipping it. That direction is deliberate: a missed
    transport signature costs a false red that someone investigates, while a
    missed refusal costs a silent skip of the check itself.
    """
    lowered = detail.lower()
    for status in ("http 500", "http 502", "http 503", "http 504", "http 429"):
        if status in lowered:
            return True
    return any(
        signature in lowered
        for signature in (
            "could not resolve host",
            "temporary failure in name resolution",
            "connection refused",
            "connection reset",
            # Go's net/http client prefixes both connect failures and DNS
            # lookup failures with this spelling. This is the form `gh api`
            # actually emits, rather than the libc-style messages above.
            "dial tcp",
            # gh prints this before its explicit internet/status-page advice
            # when its API client cannot establish the connection.
            "error connecting to api.github.com",
            "network is unreachable",
            "no route to host",
            "operation timed out",
            "timeout",
            "tls handshake",
            "unexpected eof",
            "proxy",
        )
    )


class AuthorityIntegrityError(RuntimeError):
    """The authority was obtained and does NOT match the pin.

    Deliberately a different type from :class:`AuthorityUnavailable`: this one
    is a genuine refusal and must keep failing closed. Reaching the authority
    and finding the wrong bytes is evidence, not an outage.
    """


AUTHORITY_COMMIT = "4b78d727f35bc8612ac460a6e270dda5f5df304c"
AUTHORITY_SHA256 = "2f1c61d5ec9d98b9697317fd9e66b705161defb69b808d23e6d83384e1e2a1e8"
AUTHORITY_RELATIVE_PATH = Path("ci-hub/check_outcome.py")
AUTHORITY_API_PATH = (
    "repos/rrnewton/dev-hermit/contents/ci-hub/check_outcome.py"
    f"?ref={AUTHORITY_COMMIT}"
)


def _candidate_authorities() -> list[Path]:
    if parent := os.environ.get("DEV_HERMIT_PARENT"):
        return [Path(parent) / AUTHORITY_RELATIVE_PATH]

    # GitHub's repository token cannot read the private dev-hermit repository.
    # Keep the exact content-pinned authority in this public checkout so hosted
    # validation can evaluate the contract without broader credentials.  The
    # digest check below remains the authority boundary; this copy is not
    # trusted merely because it is local.
    candidates = [
        Path(__file__).resolve().parents[1]
        / "ci/vendor/dev-hermit/ci-hub/check_outcome.py"
    ]
    for start in (Path(__file__).resolve(), Path.cwd().resolve()):
        candidates.extend(parent / AUTHORITY_RELATIVE_PATH for parent in start.parents)
    return candidates


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
            "cannot fetch the pinned check-status authority: gh is unavailable; "
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
        # `gh` exits nonzero for a 504, a 404 and a revoked credential alike, so
        # the exit code alone cannot tell an outage from an answer. Classify on
        # the message, and default to the loud reading.
        if _is_transport_failure(detail):
            raise AuthorityUnavailable(
                "cannot fetch the pinned check-status authority with gh api: "
                f"{detail}"
            )
        raise AuthorityRefused(
            "the pinned check-status authority was reached and refused: "
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
        # Reached it and got the wrong bytes. A refusal, and it stays one.
        raise AuthorityIntegrityError(
            "canonical check-status authority digest mismatch: "
            f"expected {AUTHORITY_SHA256}, got {digest}"
        )
    return source


def _load_authority() -> ModuleType:
    name = "ci_hub_check_outcome_authority"
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
    """Load the authority only when a caller first needs a classification."""
    return _load_authority()


class _SelectLatestChecks(Protocol):
    # The authority is loaded dynamically, so its return shape is not statically
    # guaranteed: elements are object, forcing every consumer to narrow defensively.
    def __call__(self, value: object, *, head_sha: str = ...) -> list[object]: ...


def classify_check(status: object, conclusion: object) -> str:
    """Return the canonical authority's PASSED/FAILED/NO_RESULT value."""
    classify = cast(
        Callable[[object, object], object],
        _authority().classify_check,
    )
    result = classify(status, conclusion)
    value = getattr(result, "value", None)
    if value not in ("PASSED", "FAILED", "NO_RESULT"):
        raise RuntimeError(f"canonical check-status authority returned {value!r}")
    return cast(str, value)


def select_latest_checks(value: object, *, head_sha: str = "") -> list[object]:
    """Delegate exact-head/latest-context selection to the pinned authority.

    Elements are typed object (not dict): the authority is loaded dynamically, so no
    element shape is statically guaranteed and callers must narrow defensively.
    """
    select_latest = cast(_SelectLatestChecks, _authority().select_latest_checks)
    return select_latest(value, head_sha=head_sha)


def annotate_rollups(value: object) -> object:
    """Attach the canonical result to every check-like object in JSON."""
    if isinstance(value, list):
        return [annotate_rollups(item) for item in value]
    if not isinstance(value, dict):
        return value
    result: dict[object, object] = {}
    for key, item in value.items():
        if key == "statusCheckRollup":
            item = select_latest_checks(item, head_sha=str(value.get("headRefOid") or ""))
        result[key] = annotate_rollups(item)
    if "status" in value or "conclusion" in value or "state" in value:
        result["_checkOutcome"] = classify_check(
            value.get("status"), value.get("conclusion", value.get("state"))
        )
    return result


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
    parser.add_argument("--status", default="")
    parser.add_argument("--conclusion", default="")
    parser.add_argument("--annotate-rollups", action="store_true")
    parser.add_argument("--select-latest-rollup", action="store_true")
    parser.add_argument("--head-sha", default="")
    args = parser.parse_args(argv)
    if args.annotate_rollups and args.select_latest_rollup:
        parser.error("select exactly one rollup mode")
    # ⚠️ NOTHING IS WRITTEN TO STDOUT ON THIS PATH, DELIBERATELY. Callers read
    # stdout as the classification and compare it against PASSED/FAILED/
    # NO_RESULT. Emitting any token here -- including a new one -- would put a
    # transport failure where a verdict is read, and a caller that does not know
    # the new token would compare it and get a wrong answer silently. The
    # distinction is carried by the exit status and the diagnostic goes to
    # stderr, so a caller that has not been taught about it fails loudly rather
    # than quietly mis-reading an outage as an answer.
    try:
        if args.materialize_authority:
            # ⚠️ THIS IS WHAT CLOSES THE RACE. Obtaining the authority ONCE and
            # writing it where later invocations read it locally means a
            # checker performs exactly one fetch, instead of one per adapter
            # process. Reported by the independent codex lane at head
            # 327f6713: the preflight probe fetched, succeeded, and then every
            # later fetch was an independent chance to hit the 504 -- so a
            # transport failure arriving AFTER the probe still made the checker
            # exit nonzero, which classify_run correctly ranks above any marker.
            #
            # The digest is verified by _verified_source before anything is
            # written, so materialising cannot launder a wrong authority: a
            # mismatch still raises AuthorityIntegrityError and never lands here.
            target = Path(args.materialize_authority) / AUTHORITY_RELATIVE_PATH
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(_verified_source())
            return 0
        if args.annotate_rollups:
            json.dump(
                annotate_rollups(json.load(sys.stdin)), sys.stdout, separators=(",", ":")
            )
            print()
        elif args.select_latest_rollup:
            json.dump(
                select_latest_checks(json.load(sys.stdin), head_sha=args.head_sha),
                sys.stdout,
                separators=(",", ":"),
            )
            print()
        else:
            print(classify_check(args.status, args.conclusion))
    except AuthorityUnavailable as unavailable:
        print(
            f"COULD-NOT-DETERMINE: {unavailable}. state: NO SIGNAL -- the "
            "check-status authority was not consulted, so this says nothing "
            "about any check and is not a failing verdict. remedy: restore "
            "access to the authority, or set DEV_HERMIT_PARENT to a dev-hermit "
            "checkout holding the pinned file",
            file=sys.stderr,
        )
        return EXIT_AUTHORITY_UNAVAILABLE
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
