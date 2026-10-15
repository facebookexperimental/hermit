#!/usr/bin/env python3
"""Controlled Cargo fixture for the actual prepared-test producer/consumer test.

No test is executed here. The driver asserts the exact build calls and runs the
real preparation helper against cold, missing, wrong and ambiguous artifacts.
"""

import json
import os
import sys
from pathlib import Path

root = Path.cwd()
target = root / "custom-cargo-target"
args = sys.argv[1:]
with open(os.environ["CARGO_CALL_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps(args) + "\n")

if (
    args[:2] == ["nextest", "list"]
    and os.environ.get("CARGO_ARTIFACT_MODE") == "declined"
):
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
        targets[name] = {
            "name": name,
            "kind": ["lib" if name == "library-fixture" else "test"],
            "test": True,
        }
packages["hermetic_infra_hermit_tests"] = {
    name: {"name": name, "kind": ["bin"], "test": False} for name in guest_names
}
# A profile that selects record_replay also prepares the record/replay
# workloads, whose Rust targets come from the same package build. The producer
# checks each one's Cargo target name and source, so they carry src_path as real
# Cargo metadata does. The driver writes this map from RUST_SOURCES in
# ci/record-replay-workloads.rs: Cargo target name -> repository-relative source.
record_sources = json.loads((root / "record-workloads.json").read_text())
for name, source in record_sources.items():
    packages["hermetic_infra_hermit_tests"][name] = {
        "name": name,
        "kind": ["bin"],
        "test": False,
        "src_path": str(root / source),
    }

packages["hermit-manifest-plan"] = {
    "nextest-cpu-wrapper": {
        "name": "nextest-cpu-wrapper",
        "kind": ["bin"],
        "test": False,
    }
}

if args[:1] == ["metadata"]:
    print(
        json.dumps(
            {
                "workspace_root": str(root),
                "target_directory": str(target),
                "workspace_members": [package_id(name) for name in packages],
                "packages": [
                    {
                        "name": name,
                        "id": package_id(name),
                        "source": None,
                        # Cargo names each package's own manifest. The guest
                        # package is tests/Cargo.toml, which the record
                        # workload producer requires of it.
                        "manifest_path": str(
                            root
                            / (
                                "tests/Cargo.toml"
                                if name == "hermetic_infra_hermit_tests"
                                else "Cargo.toml"
                            )
                        ),
                        "targets": list(targets.values()),
                    }
                    for name, targets in packages.items()
                ],
            }
        )
    )
elif args[:2] == ["nextest", "list"] and "--binaries-metadata" not in args:
    # A profile with several selections lists them all in one unified
    # `--workspace --all-targets` call; one selection is listed by itself.
    # Guest and wrapper binaries are built by their own calls below.
    if "--workspace" in args and "--all-targets" in args:
        listed = [
            (package, target["name"])
            for package, targets in packages.items()
            for target in targets.values()
            if target["kind"] != ["bin"]
        ]
    else:
        package, names = selectors(args)
        listed = [(package, name) for name in names]
    binaries = {}
    for package, name in listed:
        path = target / "debug" / "build" / package / "out" / name
        mode = (
            os.environ.get("CARGO_ARTIFACT_MODE", "current")
            if name == "tests_misc"
            else "current"
        )
        if mode != "missing":
            write_binary(path)
        binary_id = f"{package}::{name}"
        entry = {
            "binary-id": binary_id,
            "package-id": package_id(package),
            "binary-name": name,
            "kind": "lib" if name == "library-fixture" else "test",
            "build-platform": "target",
            "binary-path": str(path),
        }
        if mode == "wrong":
            entry["binary-name"] = "wrong-target"
        binaries[binary_id] = entry
        if mode == "ambiguous":
            second = path.with_name(name + "-other")
            write_binary(second)
            duplicate = {
                **entry,
                "binary-id": binary_id + "-other",
                "binary-path": str(second),
            }
            binaries[duplicate["binary-id"]] = duplicate
    print(
        json.dumps(
            {
                "rust-build-meta": {
                    "target-directory": str(target),
                    "non-test-binaries": {},
                },
                "rust-binaries": binaries,
            }
        )
    )
elif (
    args[:2] in (["nextest", "list"], ["nextest", "run"])
    and "--binaries-metadata" in args
):
    # Metadata-only enumeration must not retain any Cargo build selector.
    assert not any(
        arg in args
        for arg in ["-p", "--features", "--test", "--lib", "--bins", "--workspace"]
    ), args
    assert "--cargo-metadata" in args, args
    metadata = json.loads(Path(args[args.index("--binaries-metadata") + 1]).read_text())
    assert all(
        Path(binary["binary-path"]).is_file()
        for binary in metadata["rust-binaries"].values()
    )
    print(json.dumps({"rust-suites": {}}))
elif args[:1] == ["build"] and (
    "hermit-manifest-plan" in args or ("--workspace" in args and "--all-targets" in args)
):
    # The CPU wrapper: built by itself, or (validate-profile graph profiles) by
    # the producer's unified workspace build, which also reports the bin's own
    # unit-test build under the same target name.
    unified = "--workspace" in args
    if unified:
        assert args[:4] == ["build", "--locked", "--profile", "validate"], args
        assert args[-1] == "--message-format=json-render-diagnostics", args
    else:
        assert args == [
            "build",
            "--locked",
            "--message-format=json-render-diagnostics",
            "-p",
            "hermit-manifest-plan",
            "--bin",
            "nextest-cpu-wrapper",
        ], args
    mode = os.environ.get("CARGO_ARTIFACT_MODE", "current")
    if unified and mode == "current":
        unit_test = target / "validate" / "deps" / "nextest_cpu_wrapper-0123"
        write_binary(unit_test)
        print(json.dumps({
            "reason": "compiler-artifact",
            "package_id": package_id("hermit-manifest-plan"),
            "target": {"name": "nextest-cpu-wrapper", "kind": ["bin"]},
            "profile": {"test": True},
            "executable": str(unit_test),
        }))
    path = target / ("validate" if unified else "debug") / "nextest-cpu-wrapper"
    if mode != "wrapper-missing":
        write_binary(path)
    event = {
        "reason": "compiler-artifact",
        "package_id": package_id("hermit-manifest-plan"),
        "target": {"name": "nextest-cpu-wrapper", "kind": ["bin"]},
        "profile": {"test": mode == "wrapper-wrong"},
        "executable": str(path),
    }
    print(json.dumps(event))
    if mode == "wrapper-ambiguous":
        print(json.dumps(event))
elif args[:1] == ["build"] and "hermetic_infra_hermit_tests" in args:
    # `--bins` builds every binary of the package: the hermit_modes guests and
    # the record workloads' Rust targets.
    for spec in packages["hermetic_infra_hermit_tests"].values():
        name = spec["name"]
        path = target / "debug" / name
        write_binary(path)
        artifact_target = {"name": name, "kind": ["bin"]}
        if "src_path" in spec:
            artifact_target["src_path"] = spec["src_path"]
        print(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "package_id": package_id("hermetic_infra_hermit_tests"),
                    "target": artifact_target,
                    "profile": {"test": False},
                    "executable": str(path),
                }
            )
        )
    # Cargo's JSON stream ends with this; the record workload producer refuses
    # a build that does not report success.
    print(json.dumps({"reason": "build-finished", "success": True}))
else:
    raise SystemExit(f"unexpected Cargo invocation: {args!r}")
