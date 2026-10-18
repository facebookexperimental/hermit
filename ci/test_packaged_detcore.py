#!/usr/bin/env python3
"""Receipt-reader controls. Synthetic receipts are not constructor qualification."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("packaged_detcore", Path(__file__).with_name("verify-packaged-detcore.py"))
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


def fixture_receipt(install, pin):
    resources = Path(install) / "rsrcs"
    runtime = resources / "libdetcore_liteinst.so"
    digest = hashlib.sha256(runtime.read_bytes()).hexdigest()
    source = {"path": "/fixture/Cargo.toml", "sha256": "1" * 64}
    package = {"name": "detcore-liteinst", "target": "detcore_liteinst", "source": None,
               "kind": ["cdylib"], "crate_types": ["cdylib"]}
    artifact = "/fixture/target/liteinst-runtime/libdetcore_liteinst.so"
    args = ["build", "--locked", "--offline", "--manifest-path", source["path"],
            "-p", "detcore-liteinst", "--lib", "--profile", "liteinst-runtime",
            "--target-dir", "/fixture/target", "--message-format=json-render-diagnostics"]
    # Shape-only transport fixtures, not actual emitted machine-code evidence.
    backing = {name: {"fixture": True} for name in ("descriptor", "fixed_backing_record", "tls_boundary", "compiler_routes")}
    backing["function_sha256"] = {"fixture": "3" * 64}
    constructor = {name: {"fixture": True} for name in ("body", "bridge", "prepare", "native_gate")}
    constructor.update(function_sha256={"fixture": "4" * 64}, observation="synthetic receipt transport")
    record = {
        "schema": "hermit-packaged-detcore/v1", "reverie_pin": pin,
        "profile_name": "liteinst-runtime", "profile_inputs": source,
        "profile_objects": {"liteinst-runtime": {"inherits": "validate", "lto": False}},
        "cargo_artifact": {"features": [], "profile": {"opt_level": "3", "debuginfo": 0,
            "debug_assertions": True, "overflow_checks": True, "test": False},
            "package": package, "reported_path": artifact},
        "compiled_sources": {"image_kind": "Detcore", "reverie_pin": pin,
            "manifest": source, "leaf": package, "reverie_root": "/fixture/reverie"},
        "qualifier_source": {"path": "/fixture/reverie/scripts/m1_artifact.rs", "sha256": "2" * 64}, "compiled_qualifier_sha256": "2" * 64,
        "qualification": {"initializer": {"name": "detcore_liteinst_initialize"},
            "allocator_backing": backing, "detcore_constructor": constructor,
            "generic_bootstrap": constructor, "artifact": {"path": artifact, "sha256": digest}},
        "installed": {"sha256": digest}, "cargo_argv": args,
        "commands": [{"status": 0, "program": "cargo", "arguments": args}],
        "source_close_completed": True}
    (resources / "sabre.revision").write_text(pin + "\n")
    (resources / "libdetcore_liteinst.package.json").write_text(json.dumps(record))
    return record


class ReceiptControls(unittest.TestCase):
    def test_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            runtime = resources / "libdetcore_liteinst.so"
            runtime.write_bytes(b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            good = fixture_receipt(install, "a" * 40)
            verifier.verify(install)
            for field, value in [("profile_name", "validate"), ("source_close_completed", False)]:
                bad = copy.deepcopy(good)
                bad[field] = value
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install)
            for section, field, value in [("cargo_artifact", "features", ["allocator-fixture"]),
                    ("compiled_sources", "reverie_pin", "b" * 40),
                    ("installed", "sha256", "0" * 64),
                    ("qualifier_source", "path", "/wrong/proof.rs"),
                    ("cargo_artifact", "reported_path", "/wrong/libdetcore_liteinst.so"),
                    ("qualification", "generic_bootstrap", {}),
                    ("qualification", "allocator_backing", {"success": False}),
                    ("qualification", "detcore_constructor", {"allocation_length": 232}),
                    ("commands", 0, {"status": 1})]:
                bad = copy.deepcopy(good)
                bad[section][field] = value
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install)
            receipt.write_text(json.dumps(good))
            runtime.write_bytes(b"changed installed bytes")
            with self.assertRaises(ValueError):
                verifier.verify(install)
            receipt.unlink()
            with self.assertRaises(ValueError):
                verifier.verify(install)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--write-fixture":
        fixture_receipt(sys.argv[2], sys.argv[3])
    else:
        unittest.main()
