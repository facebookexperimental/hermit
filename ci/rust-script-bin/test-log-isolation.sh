#!/usr/bin/env bash
# Exercise the real launchers without compiling Rust or running Hermit guests.
set -euo pipefail
SOURCE_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
python3 - "$SOURCE_DIR" <<'PY'
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tempfile

source_dir = Path(sys.argv[1])
sentinel = b"outer runner evidence\x00must not change\xff\n"
with tempfile.TemporaryDirectory(prefix="hermit-script-test-logs-", dir="/tmp") as scratch:
    scratch = Path(scratch)
    root = scratch / "fixture checkout"
    tools = root / "ci/rust-script-bin"
    tools.mkdir(parents=True)
    for name in ("rust-script", "run-test-harness"):
        shutil.copy2(source_dir / name, tools / name)
    scripts = root / "scripts"
    scripts.mkdir()
    shutil.copy2(source_dir.parents[1] / "scripts/run-script-tests.sh", scripts)
    subprocess.run(["git", "init", "-q", str(root)], check=True)
    sources = ["scripts/alpha.rs", "scripts/beta.rs"]
    for source in sources:
        (root / source).write_text("#!/usr/bin/env -S rust-script --force\n#[cfg(test)]\n")
    subprocess.run(["git", "-C", str(root), "add", "--", *sources], check=True)
    published = root / "target/ci/rust-scripts"
    published.mkdir(parents=True)
    (published / "manifest.tsv").write_text("".join(
        f"{source}\trun\ttest\n" for source in sources
    ))
    fixture = '''#!/usr/bin/env bash
set -euo pipefail
[[ $(umask) == 0022 ]]
[[ $DAGRUN_LOG_MAX_BYTES == 4096 && $CARGO_BUILD_JOBS == 3 ]]
[[ $RUN_NODE_PERF_DIR == preserved-profile-root && $DAGRUN_NO_STEP_LOGS == 0 ]]
printf '%s\\0' "$@" > "$DAGRUN_LOG_DIR/received.argv"
printf '%s\\n' '{"event":"step_start","step":"test.cli"}' > "$DAGRUN_LOG_DIR/journal.jsonl"
printf 'fixture failure retained\\n' > "$DAGRUN_LOG_DIR/test.cli.log"
printf '%s\\n' '{"event":"step_end","step":"test.cli","ok":false,"returncode":"23"}' >> "$DAGRUN_LOG_DIR/journal.jsonl"
printf '%s\\n' '{"event":"step_skip","step":"test.dependent","reason":"dependency_failed"}' >> "$DAGRUN_LOG_DIR/journal.jsonl"
printf 'fixture stdout\\n'
if [[ ${FIXTURE_SIGNAL:-0} == 1 ]]; then kill -TERM "$$"; fi
if [[ ${1:-} == --test && ${2:-} == scripts/alpha.rs ]]; then exit 23; fi
exit "${FIXTURE_STATUS:-0}"
'''
    (published / "test").write_text(fixture)
    (published / "run").write_text('''#!/usr/bin/env bash
set -euo pipefail
printf '%s' "$DAGRUN_LOG_DIR"
exit 17
''')
    external = scratch / "external"
    external.mkdir()
    (external / "rust-script").write_text(fixture)
    for path in (published / "test", published / "run", external / "rust-script"):
        path.chmod(0o755)
    outer = scratch / "outer logs"
    outer.mkdir(mode=0o700)
    for name in ("journal.jsonl", "test.cli.log"):
        (outer / name).write_bytes(sentinel)
    env = dict(os.environ, DAGRUN_LOG_DIR=str(outer), DAGRUN_LOG_MAX_BYTES="4096",
               DAGRUN_NO_STEP_LOGS="0", CARGO_BUILD_JOBS="3",
               RUN_NODE_PERF_DIR="preserved-profile-root",
               HERMIT_RUST_SCRIPT_ARTIFACT_ROOT=str(published),
               HERMIT_REAL_RUST_SCRIPT=str(external / "rust-script"))
    env.pop("FIXTURE_SIGNAL", None)
    env.pop("FIXTURE_STATUS", None)
    prefix = "rust-script test runner logs: "

    def run(argv, environment=env):
        return subprocess.run(argv, cwd=root, env=environment, capture_output=True,
                              umask=0o022, timeout=20)

    def roots(output):
        return [Path(line[len(prefix):]) for line in output.stderr.decode().splitlines()
                if line.startswith(prefix)]

    def unchanged():
        for name in ("journal.jsonl", "test.cli.log"):
            assert (outer / name).read_bytes() == sentinel, name

    def retained(path):
        assert path != outer and path.is_dir()
        assert stat.S_IMODE(path.stat().st_mode) == 0o700
        assert (path / "test.cli.log").read_bytes() == b"fixture failure retained\n"
        rows = [json.loads(line) for line in (path / "journal.jsonl").read_text().splitlines()]
        ends = [row for row in rows if row.get("event") == "step_end"]
        skips = [row for row in rows if row.get("event") == "step_skip"]

        def step_end_ok(row):
            if row.get("event") != "step_end" or type(row.get("ok")) is not bool:
                raise ValueError("journal step_end ok must be a boolean")
            return row["ok"]

        assert len(ends) == 1 and step_end_ok(ends[0]) is False
        try:
            step_end_ok({"event": "step_end", "ok": "false"})
        except ValueError:
            pass
        else:
            raise AssertionError("string false journal verdict was accepted")
        assert skips == [{"event": "step_skip", "step": "test.dependent",
                          "reason": "dependency_failed"}]
        for name in ("source.path", "command.argv"):
            assert stat.S_IMODE((path / name).stat().st_mode) == 0o600

    # Actual prepared dispatcher: exact argv, status and retained failure bytes.
    argv = [str(tools / "rust-script"), "--test", sources[0], "--exact", "two words", "", "$literal"]
    output = run(argv, dict(env, FIXTURE_STATUS="23"))
    assert output.returncode == 23, output
    assert output.stdout == b"fixture stdout\n"
    unchanged()
    [first] = roots(output)
    assert first.parent == outer
    retained(first)
    assert (first / "source.path").read_bytes() == sources[0].encode() + b"\0"
    expected = b"--exact\0two words\0\0$literal\0"
    assert (first / "received.argv").read_bytes() == expected
    assert (first / "command.argv").read_bytes() == os.fsencode(published / "test") + b"\0" + expected
    unchanged()

    # The shim also delegates external/independent sources in test mode. Their
    # original rust-script argv stays intact inside an equally private scope.
    outside_source = external / "external.rs"
    outside_source.write_text("fn main() {}\n")
    output = run([str(tools / "rust-script"), "--test", str(outside_source), "--exact", "outside"])
    assert output.returncode == 0, output
    [delegated] = roots(output)
    retained(delegated)
    assert (delegated / "received.argv").read_bytes() == (
        b"--test\0" + os.fsencode(outside_source) + b"\0--exact\0outside\0"
    )
    unchanged()

    # Concurrent harnesses allocate distinct roots; neither reuses the first.
    children = [subprocess.Popen(argv, cwd=root, env=env, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, umask=0o022) for _ in range(2)]
    concurrent = []
    for child in children:
        stdout, stderr = child.communicate(timeout=20)
        assert child.returncode == 0 and stdout == b"fixture stdout\n"
        [path] = roots(subprocess.CompletedProcess(argv, child.returncode, stdout, stderr))
        retained(path)
        concurrent.append(path)
    assert len({first, *concurrent}) == 3
    unchanged()

    output = run(argv, dict(env, FIXTURE_SIGNAL="1"))
    assert output.returncode == -signal.SIGTERM, output
    [path] = roots(output)
    retained(path)
    unchanged()

    # Run mode never enters the test launcher or changes its evidence root.
    before = set(outer.iterdir())
    output = run([str(tools / "rust-script"), "--force", sources[0]])
    assert output.returncode == 17 and output.stdout == os.fsencode(outer)
    assert output.stderr == b"" and set(outer.iterdir()) == before
    unchanged()

    # Without an enclosing run, retain logs under the ignored fixture target.
    absent = dict(env)
    absent.pop("DAGRUN_LOG_DIR")
    output = run(argv, absent)
    assert output.returncode == 0
    [path] = roots(output)
    assert path.parent == root / "target/ci/script-test-logs"
    retained(path)

    # Failure to allocate evidence must not execute the harness in the outer root.
    obstacle = scratch / "not-a-directory"
    obstacle.write_bytes(sentinel)
    output = run(argv, dict(env, DAGRUN_LOG_DIR=str(obstacle)))
    assert output.returncode != 0 and output.stdout == b""
    assert obstacle.read_bytes() == sentinel
    unchanged()

    # Exercise actual discovery with ordinary rust-script: failure is reported,
    # both suites run, and each gets its own root. This is not a skipped test.
    output = run([str(scripts / "run-script-tests.sh")],
                 dict(env, PATH=str(external) + os.pathsep + env["PATH"]))
    assert output.returncode == 1, output
    assert b"1 of 2 script test suites failed" in output.stderr
    ordinary = roots(output)
    assert len(ordinary) == 2 and len(set(ordinary)) == 2
    assert output.stdout.count(b"fixture stdout\n") == 2
    for path in ordinary:
        retained(path)
    unchanged()

    # Prepared composition is deliberately nested, never one shared harness
    # root. The inner scopes contain fixture logs; the outer scopes retain argv.
    output = run([str(scripts / "run-script-tests.sh")],
                 dict(env, PATH=str(tools) + os.pathsep + env["PATH"]))
    assert output.returncode == 0, output
    assert b"OK -- 2 script test suites passed" in output.stdout
    nested = roots(output)
    assert len(nested) == 4 and len(set(nested)) == 4
    for parent, child in zip(nested[::2], nested[1::2]):
        assert parent.parent == outer and child.parent == parent
        assert (parent / "command.argv").is_file()
        retained(child)
    unchanged()

print("rust-script log isolation: prepared/ordinary/nested/concurrent harnesses retain private logs; outer bytes, argv, failure and signals preserved")
PY
