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


def test_declared_drop_passes_and_unreasoned_skip_fails(tmp_path):
    r = _run(tmp_path, {"s": ["a", "old"]}, {"s": [_row("a")]},
             dropped="Test-Dropped: old -- removed with its backend, #999\n")
    assert r.returncode == 0 and "omitted=1" in r.stdout
    r2 = _run(tmp_path, {"s": ["a"]}, {"s": [_row("a", "skipped")]})
    assert r2.returncode == 1 and "without a reason" in r2.stderr
