#!/usr/bin/env python3
"""Controlled Cargo fixture for the actual prepared-test producer/consumer test.

No test is executed here. The driver asserts the exact build calls and runs the
real preparation helper against cold, missing, wrong and ambiguous artifacts.
"""
import json
import os
from pathlib import Path
import sys

root = Path.cwd()
target = root / "custom-cargo-target"
args = sys.argv[1:]
with open(os.environ["CARGO_CALL_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps(args) + "\n")

if args[:2] == ["nextest", "list"] and os.environ.get("CARGO_ARTIFACT_MODE") == "declined":
    raise SystemExit(75)


def package_id(name):
    return f"path+file://{root}/{name}#{name}@1.0.0"


def selectors(arguments):
    package = "regular-fixture"
    tests = []
    for index, arg in enumerate(arguments[:-1]):
        if arg == "-p":
            package = arguments[index + 1]
        if arg == "--test":
            tests.append(arguments[index + 1])
    return package, tests or ["library-fixture"]


def write_binary(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    path.chmod(0o755)


graph = json.loads((root / "ci/dag/validate.json").read_text())
selections = {
    tuple(json.loads(step["env"]["NEXTEST_PREPARED_BUILD_SELECTION"]))
    for step in graph["steps"]
    if "NEXTEST_PREPARED_BUILD_SELECTION" in step.get("env", {})
}
guest_names = json.loads((root / "guest-names.json").read_text())
packages = {}
for selection in selections:
    package, names = selectors(selection)
    targets = packages.setdefault(package, {})
    for name in names:
        targets[name] = {"name": name, "kind": ["lib" if name == "library-fixture" else "test"]}
packages["hermetic_infra_hermit_tests"] = {
    name: {"name": name, "kind": ["bin"]} for name in guest_names
}

packages["hermit-manifest-plan"] = {
    "nextest-cpu-wrapper": {"name": "nextest-cpu-wrapper", "kind": ["bin"]}
}

if args[:1] == ["metadata"]:
    print(json.dumps({
        "workspace_root": str(root), "target_directory": str(target),
        "packages": [{"name": name, "id": package_id(name), "source": None,
                      "manifest_path": str(root / "Cargo.toml"), "targets": list(targets.values())}
                     for name, targets in packages.items()],
    }))
elif args[:2] == ["nextest", "list"] and "--binaries-metadata" not in args:
    package, names = selectors(args)
    binaries = {}
    for name in names:
        path = target / "debug" / "build" / package / "out" / name
        mode = os.environ.get("CARGO_ARTIFACT_MODE", "current") if name == "tests_misc" else "current"
        if mode != "missing":
            write_binary(path)
        binary_id = f"{package}::{name}"
        entry = {"binary-id": binary_id, "package-id": package_id(package),
                 "binary-name": name, "kind": "lib" if name == "library-fixture" else "test",
                 "build-platform": "target", "binary-path": str(path)}
        if mode == "wrong":
            entry["binary-name"] = "wrong-target"
        binaries[binary_id] = entry
        if mode == "ambiguous":
            second = path.with_name(name + "-other")
            write_binary(second)
            duplicate = {**entry, "binary-id": binary_id + "-other", "binary-path": str(second)}
            binaries[duplicate["binary-id"]] = duplicate
    print(json.dumps({"rust-build-meta": {"target-directory": str(target), "non-test-binaries": {}},
                      "rust-binaries": binaries}))
elif args[:2] in (["nextest", "list"], ["nextest", "run"]) and "--binaries-metadata" in args:
    # Metadata-only enumeration must not retain any Cargo build selector.
    assert not any(arg in args for arg in ["-p", "--features", "--test", "--lib", "--bins", "--workspace"]), args
    assert "--cargo-metadata" in args, args
    metadata = json.loads(Path(args[args.index("--binaries-metadata") + 1]).read_text())
    assert all(Path(binary["binary-path"]).is_file() for binary in metadata["rust-binaries"].values())
    print(json.dumps({"rust-suites": {}}))
elif args[:1] == ["build"] and "hermit-manifest-plan" in args:
    assert args == ["build", "--locked", "--message-format=json-render-diagnostics", "-p", "hermit-manifest-plan", "--bin", "nextest-cpu-wrapper"], args
    mode = os.environ.get("CARGO_ARTIFACT_MODE", "current")
    path = target / "debug" / "nextest-cpu-wrapper"
    if mode != "wrapper-missing":
        write_binary(path)
    event = {"reason": "compiler-artifact", "package_id": package_id("hermit-manifest-plan"),
             "target": {"name": "nextest-cpu-wrapper", "kind": ["bin"]},
             "profile": {"test": mode == "wrapper-wrong"}, "executable": str(path)}
    print(json.dumps(event))
    if mode == "wrapper-ambiguous":
        print(json.dumps(event))
elif args[:1] == ["build"] and "hermetic_infra_hermit_tests" in args:
    for name in guest_names:
        path = target / "debug" / name
        write_binary(path)
        print(json.dumps({"reason": "compiler-artifact", "package_id": package_id("hermetic_infra_hermit_tests"),
                          "target": {"name": name, "kind": ["bin"]}, "profile": {"test": False},
                          "executable": str(path)}))
else:
    raise SystemExit(f"unexpected Cargo invocation: {args!r}")
