#!/usr/bin/env python3
"""Check the finalizer's ordinary-runtime receipt against installed bytes.

This reads retained evidence; it does not replace or repeat the physical proof.
Both mutable install publication and copied/relocated bundles use this reader.
"""
import hashlib
import json
from pathlib import Path
import re
import sys


def require(ok, message):
    if not ok:
        raise ValueError(message)


def verify(install):
    resources = Path(install) / "rsrcs"
    runtime = resources / "libdetcore_liteinst.so"
    receipt = resources / "libdetcore_liteinst.package.json"
    require(runtime.is_file() and not runtime.is_symlink(), "packaged Detcore must be a finalized regular file")
    require(receipt.is_file() and not receipt.is_symlink(), "missing ordinary Detcore finalization receipt; run stage-detcore-liteinst")
    record = json.loads(receipt.read_text())
    require(record["schema"] == "hermit-packaged-detcore/v1", "unsupported Detcore packaging receipt")
    pin = record["reverie_pin"]
    require(re.fullmatch("[0-9a-f]{40}", pin), "invalid packaging Reverie pin")
    require((resources / "sabre.revision").read_text().strip() == pin, "packaged runtimes name different Reverie pins")
    require(record["profile_name"] == "liteinst-runtime", "unqualified packaging profile")
    profiles = record["profile_objects"]
    require(profiles["liteinst-runtime"]["inherits"] == "validate" and profiles["liteinst-runtime"]["lto"] is False,
            "ordinary packaging did not retain the reviewed LTO profile")
    artifact = record["cargo_artifact"]
    require(artifact["features"] == [], "packaged runtime is not the ordinary feature mode")
    require(artifact["profile"] == {"opt_level": "3", "debuginfo": 0, "debug_assertions": True,
                                      "overflow_checks": True, "test": False}, "wrong compiler profile")
    sources = record["compiled_sources"]
    require(sources["image_kind"] == "Detcore" and sources["reverie_pin"] == pin, "source authority differs from packaging inputs")
    package = artifact["package"]
    require(package == sources["leaf"] and package["name"] == "detcore-liteinst"
            and package["target"] == "detcore_liteinst" and package["source"] is None
            and package["kind"] == ["cdylib"] and package["crate_types"] == ["cdylib"], "wrong current runtime package")
    require(record["profile_inputs"] == sources["manifest"], "profile inputs differ from source authority")
    require(record["qualifier_source"]["path"] == str(Path(sources["reverie_root"]) / "reverie-liteinst-artifact/src/lib.rs"),
            "proof source is outside the resolved compiler tree")
    require(record["qualifier_source"]["sha256"] == record["compiled_qualifier_sha256"], "proof source identity differs")
    qualification = record["qualification"]
    require(qualification["initializer"]["name"] == "detcore_liteinst_initialize", "missing ordinary constructor proof")
    # Validate retained-result shape, not machine-code recipes. Only the shared
    # qualifier in the producer establishes physical admission of these bytes.
    for name in ("allocator_backing", "detcore_constructor", "generic_bootstrap"):
        evidence = qualification[name]
        require(isinstance(evidence, dict), "malformed physical result")
        hashes = evidence.get("function_sha256")
        require(isinstance(hashes, dict) and hashes and all(isinstance(key, str) and
                isinstance(value, str) and re.fullmatch("[0-9a-f]{64}", value)
                for key, value in hashes.items()), "malformed physical function identities")
        fields = ("descriptor", "fixed_backing_record", "tls_boundary", "compiler_routes") if name == "allocator_backing" else ("body", "bridge", "prepare", "native_gate")
        require(all(isinstance(evidence.get(field), dict) and evidence[field] for field in fields),
                "incomplete physical result")
        if name != "allocator_backing":
            require(isinstance(evidence.get("observation"), str) and evidence["observation"],
                    "missing constructor observation")
    require(qualification["artifact"]["path"] == artifact["reported_path"], "proof is for another compiler artifact")
    digest = hashlib.sha256(runtime.read_bytes()).hexdigest()
    require(digest == record["installed"]["sha256"] == qualification["artifact"]["sha256"], "installed runtime differs from qualified bytes")
    args = record["cargo_argv"]
    require(args == ["build", "--locked", "--offline", "--manifest-path", sources["manifest"]["path"],
                     "-p", "detcore-liteinst", "--lib", "--profile", "liteinst-runtime", "--target-dir",
                     str(Path(artifact["reported_path"]).parent.parent), "--message-format=json-render-diagnostics"],
            "receipt does not identify the fixed ordinary runtime build")
    commands = record["commands"]
    require(commands and all(command["status"] == 0 for command in commands), "unsuccessful producer command")
    require(any(command["program"] == "cargo" and command["arguments"] == args for command in commands), "actual producer command absent")
    require(record["source_close_completed"] is True, "source transaction did not close")
    return record


if __name__ == "__main__":
    try:
        require(len(sys.argv) == 2, "usage: verify-packaged-detcore.py INSTALL")
        verify(sys.argv[1])
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"verify-packaged-detcore: {error}", file=sys.stderr)
        sys.exit(2)
