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
    from runner_guard import run_main
    sys.exit(run_main(__file__, globals(), "recount-pins"))
