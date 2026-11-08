import json, subprocess, sys
from pathlib import Path

TOOL = Path(__file__).parent / "recount-pins"

def _run(tmp_path, counts, pin):
    (tmp_path / "ci").mkdir(exist_ok=True)
    import shutil; shutil.copy(TOOL, tmp_path / "ci/recount-pins")
    (tmp_path / "ci/recount-pins.json").write_text(json.dumps({"sites": [{"name": "unit", "file": "ci/pins.txt", "pattern": "unit=(?P<count>[0-9]+)"}]}))
    (tmp_path / "ci/pins.txt").write_text(f"unit={pin}\n")
    receipt = tmp_path / "receipt.json"; receipt.write_text(json.dumps({"counts": counts}))
    return subprocess.run([sys.executable, str(tmp_path / "ci/recount-pins"), str(receipt)], capture_output=True, text=True)

def test_drop_refused_and_growth_applies(tmp_path):
    assert _run(tmp_path, {"unit": 5}, 10).returncode == 1
    r = _run(tmp_path, {"unit": 12}, 10)
    assert r.returncode == 0 and "10 -> 12" in r.stdout


def test_inventory_loss_refused(tmp_path):
    import shutil
    (tmp_path / "ci").mkdir(exist_ok=True)
    shutil.copy(TOOL, tmp_path / "ci/recount-pins")
    (tmp_path / "ci/recount-pins.json").write_text(json.dumps({"sites": [{"kind": "inventory", "name": "s", "file": "ci/inv.txt"}]}))
    (tmp_path / "ci/inv.txt").write_text("a\nb\n")
    receipt = tmp_path / "r.json"; receipt.write_text(json.dumps({"inventories": {"s": ["a"]}}))
    r = subprocess.run([sys.executable, str(tmp_path / "ci/recount-pins"), str(receipt)], capture_output=True, text=True)
    assert r.returncode == 1 and "-b" in r.stdout + r.stderr


if __name__ == "__main__":
    import tempfile
    _tests = [(n, f) for n, f in sorted(globals().items())
              if n.startswith("test_") and callable(f)]
    if not _tests:
        print("recount-pins test runner: ERROR: zero tests collected "
              "(no test_* functions found)", file=sys.stderr)
        sys.exit(2)
    _failed = 0
    for _name, _fn in _tests:
        try:
            with tempfile.TemporaryDirectory(prefix="test-recount-pins-") as _td:
                _fn(Path(_td))
        except Exception as _exc:
            _failed += 1
            print(f"FAIL {_name}: {_exc!r}", file=sys.stderr)
        else:
            print(f"PASS {_name}")
    print(f"{len(_tests) - _failed}/{len(_tests)} tests passed")
    sys.exit(1 if _failed else 0)
