"""Shared __main__ runner for ci/test_*.py files run as `python3 ./ci/test_*.py`.

Discovers test_* callables in the caller's globals, runs each with its own
temp dir (the pytest tmp_path equivalent), and guards collection: a test_*
defined AFTER the `if __name__ == "__main__":` block is never executed by
that block, so the runner first counts top-level test_* definitions in the
file's own source (AST) and fails with a named error (exit 2) when any
defined test was not collected, or when none are defined at all.
"""
import ast
import sys
import tempfile
from pathlib import Path


def defined_test_names(path):
    tree = ast.parse(Path(path).read_text())
    return [node.name for node in tree.body
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
            and node.name.startswith("test_")]


def run_main(path, namespace, label):
    defined = defined_test_names(path)
    tests = [(name, namespace[name]) for name in sorted(namespace)
             if name.startswith("test_") and callable(namespace[name])]
    collected = [name for name, _ in tests]
    if not defined:
        print(f"{label} test runner: ERROR: zero tests collected "
              "(no test_* functions found)", file=sys.stderr)
        return 2
    missing = [name for name in defined if name not in collected]
    if missing:
        print(f"{label} test runner: ERROR: defined-but-not-collected "
              f"test(s): {', '.join(missing)} "
              f"(defined={len(defined)} collected={len(collected)}); "
              "a test_* defined after the __main__ block is never run",
              file=sys.stderr)
        return 2
    failed = 0
    for name, fn in tests:
        try:
            with tempfile.TemporaryDirectory(prefix=f"test-{label}-") as td:
                fn(Path(td))
        except Exception as exc:
            failed += 1
            print(f"FAIL {name}: {exc!r}", file=sys.stderr)
        else:
            print(f"PASS {name}")
    print(f"{len(tests) - failed}/{len(tests)} tests passed")
    return 1 if failed else 0
