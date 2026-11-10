import os, subprocess, sys
from pathlib import Path

CI = Path(__file__).parent

HEADER = 'import sys\nfrom pathlib import Path\n\n\ndef test_ok(tmp_path):\n    assert True\n'
RUNNER = ('\n\nif __name__ == "__main__":\n'
          '    from runner_guard import run_main\n'
          '    sys.exit(run_main(__file__, globals(), "fixture"))\n')
POST_MAIN = '\n\ndef test_after_main(tmp_path):\n    assert False, "must be collected"\n'


def _run_fixture(tmp_path, body):
    fixture = tmp_path / "test_fixture.py"
    fixture.write_text(body)
    env = dict(os.environ, PYTHONPATH=str(CI))
    return subprocess.run([sys.executable, str(fixture)],
                          capture_output=True, text=True, env=env)


def test_post_main_definition_fails_named(tmp_path):
    r = _run_fixture(tmp_path, HEADER + RUNNER + POST_MAIN)
    assert r.returncode == 2
    assert "defined-but-not-collected" in r.stderr
    assert "test_after_main" in r.stderr


def test_all_defined_collected_passes(tmp_path):
    r = _run_fixture(tmp_path, HEADER + RUNNER)
    assert r.returncode == 0 and "1/1 tests passed" in r.stdout


def test_zero_tests_fails_named(tmp_path):
    r = _run_fixture(tmp_path, 'import sys\nfrom pathlib import Path\n' + RUNNER)
    assert r.returncode == 2 and "zero tests collected" in r.stderr


if __name__ == "__main__":
    from runner_guard import run_main
    sys.exit(run_main(__file__, globals(), "runner-guard"))
