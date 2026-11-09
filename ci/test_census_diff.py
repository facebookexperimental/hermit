import json, subprocess, sys
from pathlib import Path

TOOL = Path(__file__).parent / "census-diff"


def _run(tmp_path, base, cand, dropped=None):
    import uuid
    root = tmp_path / uuid.uuid4().hex
    b, c = root / "base", root / "cand"
    b.mkdir(parents=True); c.mkdir()
    for suite, ids in base.items():
        (b / f"{suite}.listed-ids").write_text("".join(f"{i}\n" for i in ids))
    for suite, rows in cand.items():
        (c / f"{suite}.outcomes.json").write_text(json.dumps({"outcomes": rows}))
    args = [sys.executable, str(TOOL), "--base", str(b), "--candidate", str(c)]
    if dropped is not None:
        d = tmp_path / "dropped.txt"; d.write_text(dropped); args += ["--dropped", str(d)]
    return subprocess.run(args, capture_output=True, text=True)


def _row(i, outcome="passed", reason=""):
    r = {"id": i, "outcome": outcome}
    if reason:
        r["reason"] = reason
    return r


def test_unlisted_disappearance_fails_naming_the_id(tmp_path):
    r = _run(tmp_path, {"s": ["a", "gone_test"]}, {"s": [_row("a")]})
    assert r.returncode == 1 and "gone_test" in r.stderr and "MISSING" in r.stderr


def test_skip_with_reason_passes_and_is_reported(tmp_path):
    r = _run(tmp_path, {"s": ["a", "b"]},
             {"s": [_row("a"), _row("b", "skipped", "flaky on KVM, #1234")]})
    assert r.returncode == 0 and "skipped=1" in r.stdout


def test_new_test_appears_and_passes(tmp_path):
    r = _run(tmp_path, {"s": ["a"]}, {"s": [_row("a"), _row("brand_new")]})
    assert r.returncode == 0 and "+brand_new" in r.stdout and "added=1" in r.stdout


def _run_raw(tmp_path, base, cand_raw, report_only=False):
    import uuid
    root = tmp_path / uuid.uuid4().hex
    b, c = root / "base", root / "cand"
    b.mkdir(parents=True); c.mkdir()
    for suite, ids in base.items():
        (b / f"{suite}.listed-ids").write_text("".join(f"{i}\n" for i in ids))
    for suite, raw in cand_raw.items():
        (c / f"{suite}.outcomes.json").write_text(raw)
    args = [sys.executable, str(TOOL), "--base", str(b), "--candidate", str(c)]
    if report_only:
        args.append("--report-only")
    return subprocess.run(args, capture_output=True, text=True)


def _assert_named_failure(r, *needles, returncode=1):
    assert r.returncode == returncode, (r.returncode, r.stdout, r.stderr)
    assert "census FAIL:" in r.stderr and "Traceback" not in r.stderr
    for needle in needles:
        assert needle in r.stderr


def test_duplicate_candidate_id_failed_then_passed_fails(tmp_path):
    r = _run(tmp_path, {"s": ["a"]},
             {"s": [_row("a", "failed"), _row("a", "passed")]})
    _assert_named_failure(r, "duplicate", "a")


def test_duplicate_candidate_id_passed_then_failed_fails(tmp_path):
    r = _run(tmp_path, {"s": ["a"]},
             {"s": [_row("a", "passed"), _row("a", "failed")]})
    _assert_named_failure(r, "duplicate", "a")


def test_duplicate_base_id_fails(tmp_path):
    r = _run(tmp_path, {"s": ["a", "a"]}, {"s": [_row("a")]})
    _assert_named_failure(r, "duplicate", "a")


def test_malformed_candidate_json_is_named_failure(tmp_path):
    r = _run_raw(tmp_path, {"s": ["a"]}, {"s": "{not json"})
    _assert_named_failure(r, "malformed", "s")


def test_missing_outcomes_key_is_named_failure(tmp_path):
    r = _run_raw(tmp_path, {"s": ["a"]}, {"s": json.dumps({"rows": []})})
    _assert_named_failure(r, "outcomes", "s")


def test_malformed_candidate_json_report_only_reports(tmp_path):
    r = _run_raw(tmp_path, {"s": ["a"]}, {"s": "{not json"}, report_only=True)
    _assert_named_failure(r, "malformed", "s", returncode=0)
    assert "report-only" in r.stderr


def test_missing_outcomes_key_report_only_reports(tmp_path):
    r = _run_raw(tmp_path, {"s": ["a"]}, {"s": json.dumps({"rows": []})},
                 report_only=True)
    _assert_named_failure(r, "outcomes", "s", returncode=0)
    assert "report-only" in r.stderr


def test_duplicate_candidate_report_only_reports(tmp_path):
    import uuid
    root = tmp_path / uuid.uuid4().hex
    b, c = root / "base", root / "cand"
    b.mkdir(parents=True); c.mkdir()
    (b / "s.listed-ids").write_text("a\n")
    (c / "s.outcomes.json").write_text(json.dumps(
        {"outcomes": [_row("a", "failed"), _row("a", "passed")]}))
    r = subprocess.run([sys.executable, str(TOOL), "--base", str(b),
                        "--candidate", str(c), "--report-only"],
                       capture_output=True, text=True)
    _assert_named_failure(r, "duplicate", "a", returncode=0)
    assert "report-only" in r.stderr


def test_whitespace_only_skip_reason_fails(tmp_path):
    import uuid
    root = tmp_path / uuid.uuid4().hex
    b, c = root / "base", root / "cand"
    b.mkdir(parents=True); c.mkdir()
    (b / "s.listed-ids").write_text("a\n")
    (c / "s.outcomes.json").write_text(json.dumps(
        {"outcomes": [{"id": "a", "outcome": "skipped", "reason": "   "}]}))
    r = subprocess.run([sys.executable, str(TOOL), "--base", str(b),
                        "--candidate", str(c)], capture_output=True, text=True)
    _assert_named_failure(r, "without a reason", "a")


CHECK = Path(__file__).parent / "census-check.sh"


def _check(tmp_path, base_ids, cand_ids=None, cand_outcomes=None,
           dropped=None, report_only=False):
    import uuid
    root = tmp_path / uuid.uuid4().hex
    root.mkdir(parents=True)
    (root / "base.listed-ids").write_text("".join(f"{i}\n" for i in base_ids))
    args = ["bash", str(CHECK), "--base-listed", str(root / "base.listed-ids")]
    if cand_outcomes is not None:
        (root / "cand.outcomes.json").write_text(
            json.dumps({"outcomes": cand_outcomes}))
        args += ["--candidate-outcomes", str(root / "cand.outcomes.json")]
    else:
        (root / "cand.listed-ids").write_text(
            "".join(f"{i}\n" for i in (cand_ids or [])))
        args += ["--candidate-listed", str(root / "cand.listed-ids")]
    if dropped is not None:
        (root / "dropped.txt").write_text(dropped)
        args += ["--dropped", str(root / "dropped.txt")]
    if report_only:
        args.append("--report-only")
    return subprocess.run(args, capture_output=True, text=True)


def test_wired_listed_removal_fails_naming_it(tmp_path):
    # (a)+(d): a test removed from the suite's listed population on the
    # wired census-check path fails naming it -- the failure mode the
    # deleted count pins used to catch, now with the ID.
    r = _check(tmp_path, ["pkg/bin/kept", "pkg/bin/gone_test"],
               cand_ids=["pkg/bin/kept"])
    assert r.returncode == 1 and "gone_test" in r.stderr and "MISSING" in r.stderr


def test_wired_skip_with_reason_passes_and_is_reported(tmp_path):
    # (b): skipped with a reason passes and is reported in the totals.
    r = _check(tmp_path, ["pkg/bin/a", "pkg/bin/b"],
               cand_outcomes=[_row("pkg/bin/a"),
                              _row("pkg/bin/b", "skipped", "flaky on KVM, #1234")])
    assert r.returncode == 0 and "skipped=1" in r.stdout


def test_wired_new_test_passes(tmp_path):
    # (c): a newly added test passes and is reported as an addition.
    r = _check(tmp_path, ["pkg/bin/a"],
               cand_ids=["pkg/bin/a", "pkg/bin/brand_new"])
    assert r.returncode == 0 and "+pkg/bin/brand_new" in r.stdout and "added=1" in r.stdout


def test_wired_removal_with_report_only_passes(tmp_path):
    # Enforcement is opt-out, not opt-in: the same disappearance that fails
    # by default is only a report with --report-only.
    r = _check(tmp_path, ["pkg/bin/kept", "pkg/bin/gone_test"],
               cand_ids=["pkg/bin/kept"], report_only=True)
    assert r.returncode == 0 and "gone_test" in r.stderr and "report-only" in r.stderr


def test_wired_missing_base_file_fails_closed(tmp_path):
    # A supplied-but-missing base must never degrade to "everything added".
    import uuid
    root = tmp_path / uuid.uuid4().hex
    root.mkdir(parents=True)
    (root / "cand.listed-ids").write_text("pkg/bin/a\n")
    r = subprocess.run(["bash", str(CHECK), "--base-listed", str(root / "absent.listed-ids"),
                        "--candidate-listed", str(root / "cand.listed-ids")],
                       capture_output=True, text=True)
    assert r.returncode == 2 and "absent.listed-ids" in r.stderr and "refusing" in r.stderr


def test_wired_require_base_without_base_fails(tmp_path):
    # Counted nodes require their base since the pins were deleted; only an
    # explicit local opt-out may run baseless.
    r = subprocess.run(["bash", str(CHECK), "--candidate-listed", "/dev/null",
                        "--require-base"], capture_output=True, text=True)
    assert r.returncode == 2 and "require-base" in r.stderr
    r2 = subprocess.run(["bash", str(CHECK), "--candidate-listed", "/dev/null",
                         "--require-base", "--allow-no-base"],
                        capture_output=True, text=True)
    assert r2.returncode == 0 and "report-only" in r2.stderr


SHA_A = "a" * 40
SHA_B = "b" * 40


def _check_stamped(tmp_path, base_ids, cand_ids, commit=SHA_A, declared=None,
                   expected=SHA_A, require=True):
    import uuid
    root = tmp_path / uuid.uuid4().hex
    root.mkdir(parents=True)
    n = len(base_ids) if declared is None else declared
    (root / "base.listed-ids").write_text(
        f"# census-base: commit={commit} count={n}\n"
        + "".join(f"{i}\n" for i in base_ids))
    (root / "cand.listed-ids").write_text("".join(f"{i}\n" for i in cand_ids))
    args = ["bash", str(CHECK), "--base-listed", str(root / "base.listed-ids"),
            "--candidate-listed", str(root / "cand.listed-ids")]
    if require:
        args.append("--require-base")
    if expected is not None:
        args += ["--expected-base-commit", expected]
    return subprocess.run(args, capture_output=True, text=True)


def test_wired_stamped_fresh_base_passes(tmp_path):
    r = _check_stamped(tmp_path, ["pkg/bin/a", "pkg/bin/b"],
                       ["pkg/bin/a", "pkg/bin/b"])
    assert r.returncode == 0, (r.returncode, r.stdout, r.stderr)


def test_wired_stale_base_commit_refused_by_name(tmp_path):
    # A base listed at the wrong commit must not pass as if current.
    r = _check_stamped(tmp_path, ["pkg/bin/a"], ["pkg/bin/a"], commit=SHA_B)
    assert r.returncode == 2 and "census-base-stale" in r.stderr
    assert SHA_B in r.stderr and SHA_A in r.stderr


def test_wired_truncated_base_refused_by_name(tmp_path):
    # Header declares 3 IDs, file holds 2: a short/truncated base.
    r = _check_stamped(tmp_path, ["pkg/bin/a", "pkg/bin/b"],
                       ["pkg/bin/a", "pkg/bin/b"], declared=3)
    assert r.returncode == 2 and "census-base-truncated" in r.stderr
    assert "count=3" in r.stderr and "holds 2" in r.stderr


def test_wired_require_base_without_provenance_refused(tmp_path):
    # The plain (unstamped) wired path still works manually, but a counted
    # node requiring its base refuses a base with no provenance header.
    r = _check(tmp_path, ["pkg/bin/a"], cand_ids=["pkg/bin/a"])
    assert r.returncode == 0
    import uuid
    root = tmp_path / uuid.uuid4().hex
    root.mkdir(parents=True)
    (root / "base.listed-ids").write_text("pkg/bin/a\n")
    (root / "cand.listed-ids").write_text("pkg/bin/a\n")
    r2 = subprocess.run(
        ["bash", str(CHECK), "--base-listed", str(root / "base.listed-ids"),
         "--candidate-listed", str(root / "cand.listed-ids"), "--require-base"],
        capture_output=True, text=True)
    assert r2.returncode == 2 and "census-base-provenance" in r2.stderr


def test_provenance_header_is_not_a_base_id(tmp_path):
    # census-diff fed a stamped base directly must not diff the header.
    r = _check_stamped(tmp_path, ["a"], ["a"], require=False, expected=None)
    assert r.returncode == 0 and "added=0" in r.stdout


def test_wired_no_base_is_report_only_and_passes(tmp_path):
    r = subprocess.run(["bash", str(CHECK), "--candidate-listed", "/dev/null"],
                       capture_output=True, text=True)
    assert r.returncode == 0 and "report-only" in r.stderr


def test_declared_drop_passes_and_unreasoned_skip_fails(tmp_path):
    r = _run(tmp_path, {"s": ["a", "old"]}, {"s": [_row("a")]},
             dropped="Test-Dropped: old -- removed with its backend, #999\n")
    assert r.returncode == 0 and "omitted=1" in r.stdout
    r2 = _run(tmp_path, {"s": ["a"]}, {"s": [_row("a", "skipped")]})
    assert r2.returncode == 1 and "without a reason" in r2.stderr


if __name__ == "__main__":
    from runner_guard import run_main
    sys.exit(run_main(__file__, globals(), "census-diff"))
