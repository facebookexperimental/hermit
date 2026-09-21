#!/usr/bin/env python3
"""Check refusal handling in the validate stop-path test.

This check is deliberately about behavior, not Python spelling.  The defect in
https://github.com/rrnewton/hermit/pull/2637 was that the consumer searched the
complete captured channel for refusal-shaped text.  That turned ordinary failures
which merely mentioned that text into `ValidateChildRefused`.

The full stop-path test can run inside `make lint-checks`: it clears the inherited
active-validation marker, and every child enters stop-test mode before admission
or the DAG.  This companion check imports the file without spawning those children
because its process work is guarded by `if __name__ == "__main__"`.  It therefore
drives the real `wait_for_text()` path directly with genuine re-entrancy refusals,
other could-not-run outcomes, status mismatches, quoted text, and ordinary failure
output, including branches the full happy-path run may not encounter.

The check refuses when the refusal-classification path is absent.  Pull request
2637 must therefore be underneath this change before it can land. The
`ValidateChildRefused` and `wait_for_text` consumer path must exist and classify
every case correctly. A source-only check cannot decide whether arbitrary
rewritten Python has the same behavior; executing the real consumer on the
required inputs can.

Exit status:
    0  wait_for_text classifies every case correctly
    1  the path is incomplete, the corpus is one-sided, or a case is wrong
    2  usage or load error
"""

from __future__ import annotations

import runpy
import sys
import tempfile
from collections.abc import Callable, Sequence
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TARGET = ROOT / "scripts" / "test_validate_stop_paths.py"
SAMPLE_COMMIT = "0123456789abcdef0123456789abcdef01234567"
FINAL_COULD_NOT_RUN = "FINAL_VALIDATE_STATUS: COULD_NOT_RUN"
FINAL_FAILED = "FINAL_VALIDATE_STATUS: FAILED"
REENTRANCY_REASON = "   refused by: the re-entrancy guard"


def refusal_output(
    *,
    reason: str,
    commit: str = SAMPLE_COMMIT,
    profile: str = "full",
    lead_in: str = "",
) -> str:
    return (
        lead_in
        + f"🚫 validate REFUSED (exit 75) — profile {profile} @ {commit}\n"
        + f"{reason}\n"
        + "   nodes: none executed (stopped before the DAG ran)\n"
        + f"{FINAL_COULD_NOT_RUN}\n"
    )

# These are complete channel contents plus the observed child status, not source
# fragments. The thirteen False cases include ordinary failures, other refusal
# reasons, incomplete status output, and a status/code disagreement. The three
# True cases are the specific re-entrancy refusal that prevents the stop-test seam
# from running, including the documented unknown-commit fallback and a channel
# containing earlier text from another writer.
CASES: tuple[tuple[str, int, bool], ...] = (
    ("thread 'main' panicked at src/x.rs:9: connection refused by: peer", 1, False),
    ("guest: server said 'refused by: firewall'\nerror: compilation failed", 1, False),
    ("error[E0433]: file fixtures/validate: REFUSED_cases/t.rs not found", 1, False),
    ("refused by: guest firewall policy", 1, False),
    ("   refused by: guest firewall policy", 1, False),
    ("validate: REFUSED_cases is a guest label", 1, False),
    ("another validate is already running in this documentation", 1, False),
    (
        "wrapper: "
        + refusal_output(reason=REENTRANCY_REASON),
        75,
        False,
    ),
    (
        "🚫 validate REFUSED is quoted documentation, not a final summary\n"
        f"{REENTRANCY_REASON}\n"
        f"{FINAL_COULD_NOT_RUN}\n",
        75,
        False,
    ),
    (
        refusal_output(reason="   refused by: argument parsing"),
        75,
        False,
    ),
    (
        refusal_output(reason=REENTRANCY_REASON),
        1,
        False,
    ),
    (
        f"{FINAL_COULD_NOT_RUN}\n",
        75,
        False,
    ),
    (
        f"{FINAL_COULD_NOT_RUN}\nquoted earlier\n{FINAL_FAILED}\n",
        1,
        False,
    ),
    (
        refusal_output(reason=REENTRANCY_REASON),
        75,
        True,
    ),
    (
        refusal_output(reason=REENTRANCY_REASON, commit="unknown", profile="strict"),
        75,
        True,
    ),
    (
        refusal_output(
            reason=REENTRANCY_REASON,
            lead_in=(
                "wrapper mentioned validate: REFUSED and "
                "FINAL_VALIDATE_STATUS: FAILED\n"
            ),
        ),
        75,
        True,
    ),
)


class CheckFailed(RuntimeError):
    """The refusal classification or its required corpus is wrong."""


class ExitedProcess:
    """The part of subprocess.Popen that wait_for_text observes after exit."""

    def __init__(self, returncode: int) -> None:
        self.returncode = returncode

    def poll(self) -> int:
        return self.returncode


def check_wait_for_text(
    wait_for_text: Callable[[Path, str, object], object],
    refusal_type: type[BaseException],
    cases: Sequence[tuple[str, int, bool]],
    *,
    subject: str,
) -> None:
    expected = {want for _, _, want in cases}
    if expected != {False, True}:
        raise CheckFailed(
            f"{subject}: refusal predicate corpus must contain refusing and "
            "non-refusing cases"
        )

    wrong: list[str] = []
    for sample, returncode, want in cases:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "validate.log"
            log.write_text(sample, encoding="utf-8")
            try:
                wait_for_text(
                    log,
                    "REFUSAL-CHECK-READY-MARKER",
                    ExitedProcess(returncode),
                )
            except BaseException as exc:  # target exit 0 must not bypass the gate
                if isinstance(exc, refusal_type):
                    got = True
                elif isinstance(exc, AssertionError):
                    got = False
                else:
                    raise CheckFailed(
                        f"{subject}: wait_for_text raised {type(exc).__name__}: {exc}"
                    ) from exc
            else:
                raise CheckFailed(
                    f"{subject}: wait_for_text returned after the process had exited"
                )
        if got is not want:
            direction = "must" if want else "must NOT"
            wrong.append(f"{direction} classify as refused: {sample!r}")

    if wrong:
        raise CheckFailed(
            f"{subject}: refusal classification misclassified {len(wrong)} of "
            f"{len(cases)} channel examples:\n  " + "\n  ".join(wrong)
        )


def check_target(path: Path) -> None:
    """Check the real wait_for_text refusal-classification path."""
    try:
        namespace = runpy.run_path(str(path), run_name="validate_stop_paths_check")
    except (OSError, SyntaxError) as exc:
        raise RuntimeError(f"could not load {path}: {exc}") from exc
    except BaseException as exc:  # target exit 0 must not bypass the gate
        raise RuntimeError(
            f"{path}: loading the stop-path test raised {type(exc).__name__}: {exc}"
        ) from exc

    has_exception = "ValidateChildRefused" in namespace
    if not has_exception:
        raise CheckFailed(
            f"{path}: refusal-classification path is absent; this gate cannot "
            "pass without observing it"
        )
    refusal_type = namespace.get("ValidateChildRefused")
    if not isinstance(refusal_type, type) or not issubclass(
        refusal_type, BaseException
    ):
        raise CheckFailed(
            f"{path}: refusal classification exists without a usable "
            "ValidateChildRefused exception"
        )
    wait_for_text = namespace.get("wait_for_text")
    if not callable(wait_for_text):
        raise CheckFailed(
            f"{path}: ValidateChildRefused exists but wait_for_text is not callable"
        )

    check_wait_for_text(
        wait_for_text,
        refusal_type,
        CASES,
        subject=f"{path}:wait_for_text refusal classification",
    )


def self_test() -> None:
    import re

    summary = re.compile(
        r"🚫 validate REFUSED \(exit 75\) — profile .+ @ (?:[0-9a-f]{40}|unknown)"
    )

    def correct(output: str, returncode: int) -> bool:
        if returncode != 75:
            return False
        lines = output.splitlines()
        statuses = [
            (index, line.removeprefix("FINAL_VALIDATE_STATUS: "))
            for index, line in enumerate(lines)
            if line.startswith("FINAL_VALIDATE_STATUS: ")
        ]
        if not statuses or statuses[-1][1] != "COULD_NOT_RUN":
            return False
        final_index = statuses[-1][0]
        summaries = [
            index
            for index, line in enumerate(lines[:final_index])
            if summary.fullmatch(line)
        ]
        return bool(
            summaries
            and lines[summaries[-1] + 1 : summaries[-1] + 2]
            == [REENTRANCY_REASON]
        )

    shapes = ("refused by:", "validate: REFUSED", "another validate is already running")

    def original_defect(output: str, _returncode: int) -> bool:
        return any(shape in output for shape in shapes)

    def final_status_only(output: str, returncode: int) -> bool:
        statuses = [
            line.removeprefix("FINAL_VALIDATE_STATUS: ")
            for line in output.splitlines()
            if line.startswith("FINAL_VALIDATE_STATUS: ")
        ]
        return bool(statuses and statuses[-1] == "COULD_NOT_RUN" and returncode == 75)

    class Refused(RuntimeError):
        pass

    def wait_with(
        predicate: Callable[[str, int], bool],
    ) -> Callable[[Path, str, object], None]:
        def wait(log: Path, _text: str, process: object) -> None:
            if predicate(
                log.read_text(encoding="utf-8"), int(getattr(process, "returncode"))
            ):
                raise Refused("refused")
            raise AssertionError("ordinary failure")

        return wait

    correct_wait = wait_with(correct)
    check_wait_for_text(
        correct_wait,
        Refused,
        CASES,
        subject="self-test correct wait_for_text",
    )

    expected_failures = (
        (
            "original whole-channel predicate",
            wait_with(original_defect),
            CASES,
            "misclassified",
        ),
        (
            "predicate that misses every refusal",
            wait_with(lambda _output, _returncode: False),
            CASES,
            "misclassified",
        ),
        (
            "final-status-only predicate",
            wait_with(final_status_only),
            CASES,
            "misclassified",
        ),
        (
            "empty corpus",
            correct_wait,
            (),
            "must contain refusing and non-refusing cases",
        ),
        (
            "non-refusing-only corpus",
            correct_wait,
            tuple(case for case in CASES if not case[2]),
            "must contain refusing and non-refusing cases",
        ),
        (
            "refusing-only corpus",
            correct_wait,
            tuple(case for case in CASES if case[2]),
            "must contain refusing and non-refusing cases",
        ),
        (
            "consumer exits zero",
            lambda _log, _text, _process: sys.exit(0),
            CASES,
            "raised SystemExit: 0",
        ),
    )
    for name, wait_for_text, cases, required_message in expected_failures:
        try:
            check_wait_for_text(
                wait_for_text,
                Refused,
                cases,
                subject=f"self-test {name}",
            )
        except CheckFailed as exc:
            if required_message not in str(exc):
                raise AssertionError(
                    f"self-test refused {name} for the wrong reason: {exc}"
                ) from exc
        else:
            raise AssertionError(f"self-test did not refuse {name}")

    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory) / "test_validate_stop_paths.py"
        fixture.write_text("value = 1\n", encoding="utf-8")
        try:
            check_target(fixture)
        except CheckFailed:
            pass
        else:
            raise AssertionError("self-test accepted an absent refusal path")

        fixture.write_text(
            "class ValidateChildRefused(RuntimeError):\n    pass\n",
            encoding="utf-8",
        )
        try:
            check_target(fixture)
        except CheckFailed:
            pass
        else:
            raise AssertionError("self-test accepted a class without its predicate")

        fixture.write_text("raise SystemExit(0)\n", encoding="utf-8")
        try:
            check_target(fixture)
        except RuntimeError:
            pass
        else:
            raise AssertionError("self-test accepted a target that exits zero while loading")


def usage() -> int:
    print(
        f"usage: {Path(sys.argv[0]).name} [--self-test | PATH]",
        file=sys.stderr,
    )
    return 2


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--self-test":
        try:
            self_test()
        except (AssertionError, CheckFailed, RuntimeError) as exc:
            print(f"FAIL: {exc}", file=sys.stderr)
            return 1
        print(f"PASS: {Path(argv[0]).name} self-test")
        return 0

    if len(argv) > 2 or (len(argv) == 2 and argv[1].startswith("-")):
        return usage()
    target = Path(argv[1]) if len(argv) == 2 else DEFAULT_TARGET
    if not target.is_file():
        print(f"error: target is not a file: {target}", file=sys.stderr)
        return 2

    try:
        check_target(target)
    except CheckFailed as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    print(
        f"PASS: {target}: wait_for_text refusal classification passed "
        f"{len(CASES)} channel examples"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
