#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Receipt-reader controls. Synthetic receipts are not constructor qualification.

The Buck-literal cross-checks are split by the file each one reads. The
finalizer comparison needs only exported files and always runs, and so
does the root BUCK comparison: the GitHub export carries BUCK. The
profile comparison reads fb-only/liteinst-runtime-profile.bzl, which the
export, facebookexperimental/hermit, has never carried. There it is
skipped, each skipped test printing a "test_packaged_detcore.py:
SKIPPED ..." line to stderr. It skips only where the checkout does not
carry the profile, as ci/checkout-carries.sh decides from the checkout
itself (a path the export has never carried, absent, at the top level of
a git worktree whose commit does not track it), so a fresh clone needs
no environment. Under Buck the root is the test's resources tree, never
a worktree's top level, so nothing skips there: its staging is
authoritative. Any other checkout missing the profile still fails, a
profile that is present but corrupt still fails, and a checkout that has
it runs the comparison. In fbsource the Buck test
//hermetic_infra/hermit/ci:test_packaged_detcore runs the whole suite
with every file present as resources, so the comparisons execute there.
"""
import copy
import hashlib
import importlib.resources
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import subprocess
import tempfile
import unittest
from unittest import mock

# The GitHub export ShipIt publishes; named only in the SKIPPED text.
EXPORT_REPOSITORY = "facebookexperimental/hermit"
PROFILE_NAME = "fb-only/liteinst-runtime-profile.bzl"
# The flags constant's home is the Reverie sibling tree; it is
# resolved below hermit_root()'s parent (hermetic_infra/ in a
# checkout, the resources root under Buck).
REVERIE_CONSTRAINTS_NAME = "reverie/constraints/liteinst-runtime-flags.bzl"
BUCK_NAME = "BUCK"
FINALIZER_NAME = "hermit-install/src/bin/stage-detcore-liteinst.rs"


def _load_verifier():
    sibling = Path(__file__).with_name("verify-packaged-detcore.py")
    try:
        sibling.lstat()
    except FileNotFoundError:
        # Under Buck the verifier is a resource, laid out as hermit/ci/.
        text = (importlib.resources.files(__package__) / "hermit"
                / "ci" / "verify-packaged-detcore.py").read_text()
        module = importlib.util.module_from_spec(
            importlib.util.spec_from_loader("packaged_detcore", loader=None))
        exec(compile(text, "verify-packaged-detcore.py", "exec"), module.__dict__)
        return module
    spec = importlib.util.spec_from_file_location("packaged_detcore", sibling)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verifier = _load_verifier()


def hermit_root(case):
    """The Hermit tree these tests read, at its repository paths.

    Under Buck, which runs the module in its package, the files are the
    test's resources, laid out as hermit/, and are read only there: an
    absent resources tree fails rather than falling back to the source
    tree, which on a remote-execution worker holds only the test's own
    inputs. Run as a script, in a source checkout or the GitHub export, the
    tree is the parent of ci/.
    """
    if not __package__:
        return Path(__file__).resolve().parent.parent
    resources = importlib.resources.files(__package__) / "hermit"
    if not resources.is_dir():
        raise FileNotFoundError(
            f"the test's resources tree {resources} is absent: ci/BUCK "
            "stages the files these controls read there")
    return case.enterContext(importlib.resources.as_file(resources))


def checkout_lacks(root, name, tool_root=None):
    """Whether ci/checkout-carries.sh, the one shared rule, from the checkout
    at tool_root (by default this script's, the parent of ci/), answers that
    the checkout at root does not carry name (its exit 1: a path the export
    has never carried, absent here, at the top level of a git worktree whose
    commit does not track it). Every other outcome, including the tool being
    absent or failing to run, is False, so the caller reads the file and fails
    by name. A present file never reaches the tool, so a resources tree under
    Buck, which may not stage the tool, prints nothing. For an absent one the
    tool runs under bash, so a lost execute bit cannot stop it; its own line
    when git cannot answer, bash's when the tool is missing, and one line when
    bash itself cannot start go to stderr."""
    if os.path.lexists(Path(root) / name):
        return False
    if tool_root is None:
        tool_root = Path(__file__).resolve().parent.parent
    tool = Path(tool_root) / "ci" / "checkout-carries.sh"
    try:
        answer = subprocess.run(
            ["bash", str(tool), str(root), name],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
    except OSError as error:
        print(f"test_packaged_detcore.py: could not start bash for {tool}: {error}; "
              f"{name} counts as carried", file=sys.stderr)
        return False
    return answer.returncode == 1


def export_skip_reason(root, name, what):
    """The SKIPPED line for a checkout that does not carry name, as on the
    GitHub export, or None to run the comparison. It skips only when
    checkout_lacks says so. Any other checkout missing the file still
    fails, and a checkout that has it runs the comparison."""
    if not checkout_lacks(root, name):
        return None
    return (f"SKIPPED: {name} is absent and this checkout's commit does not "
            f"track it: the {EXPORT_REPOSITORY} export has never carried this "
            f"fb-only file, so {what} is skipped")


def export_file_text(root, name, what):
    """The fb-only file `name` below root. Where the checkout does not carry
    it, skips the calling comparison (export_skip_reason); a present file is
    read, so a corrupt one fails the comparison rather than skipping it."""
    reason = export_skip_reason(root, name, what)
    if reason:
        raise unittest.SkipTest(reason)
    return (root / name).read_text()


def fixture_receipt(install, pin, *, core=False):
    resources = Path(install) / "rsrcs"
    runtime = resources / "libdetcore_liteinst.so"
    digest = hashlib.sha256(runtime.read_bytes()).hexdigest()
    source = {"path": "/fixture/Cargo.toml", "sha256": "1" * 64}
    package = {"name": "detcore-liteinst", "target": "detcore_liteinst", "source": None,
               "kind": ["cdylib"], "crate_types": ["cdylib"]}
    artifact = "/fixture/target/liteinst-runtime/libdetcore_liteinst.so"
    args = ["build", "--locked", "--offline", "--manifest-path", source["path"],
            "-p", "detcore-liteinst", "--lib", "--no-default-features", "--profile", "liteinst-runtime",
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
        "qualifier_source": {"path": "/fixture/reverie/reverie-liteinst-artifact/src/lib.rs", "sha256": "2" * 64}, "compiled_qualifier_sha256": "2" * 64,
        "qualification": {"initializer": {"name": "detcore_liteinst_initialize"},
            "allocator_backing": backing, "detcore_constructor": constructor,
            "generic_bootstrap": constructor, "artifact": {"path": artifact, "sha256": digest}},
        "installed": {"sha256": digest}, "cargo_argv": args,
        "commands": [{"status": 0, "program": "cargo", "arguments": args}],
        "source_close_completed": True}
    (resources / ("libreverie_liteinst.so.revision" if core else "sabre.revision")).write_text(pin + "\n")
    (resources / "libdetcore_liteinst.package.json").write_text(json.dumps(record))
    return record


def buck_fixture_receipt(install, pin, *, core=False):
    resources = Path(install) / "rsrcs"
    runtime = resources / "libdetcore_liteinst.so"
    digest = hashlib.sha256(runtime.read_bytes()).hexdigest()
    # The Buck arm's profile input is the reviewed profile definition, and
    # its compiled_sources manifest slot carries that same file identity.
    source = {"path": "/fixture/fb-only/liteinst-runtime-profile.bzl", "sha256": "1" * 64}
    package = {"name": "detcore-liteinst", "target": "detcore_liteinst", "source": None,
               "kind": ["cdylib"], "crate_types": ["cdylib"]}
    artifact = "/fixture/buck-out/libdetcore_liteinst.so"
    # Fixture literals are the verifier's pins, never a second copy; the
    # cross-check test binds those pins to the finalizer and profile texts.
    flags = list(verifier.BUCK_RUSTC_FLAGS)
    target = verifier.BUCK_TARGET
    platform = verifier.BUCK_PLATFORM
    args = list(verifier.BUCK_ARGV)
    # Shape-only transport fixtures, not actual emitted machine-code evidence.
    backing = {name: {"fixture": True} for name in ("descriptor", "fixed_backing_record", "tls_boundary", "compiler_routes")}
    backing["function_sha256"] = {"fixture": "3" * 64}
    constructor = {name: {"fixture": True} for name in ("body", "bridge", "prepare", "native_gate")}
    constructor.update(function_sha256={"fixture": "4" * 64}, observation="synthetic receipt transport")
    record = {
        "schema": "hermit-packaged-detcore/v1", "builder": "buck2", "reverie_pin": pin,
        "profile_name": "liteinst-runtime", "profile_inputs": source,
        "profile_objects": {"liteinst-runtime": {"rustc_flags": flags}},
        "buck_target": target, "buck_platform": platform,
        "buck_constraints": list(verifier.BUCK_CONSTRAINTS),
        "buck_artifact": {"features": [], "profile": {"opt_level": "3", "debuginfo": 0,
            "debug_assertions": True, "overflow_checks": True, "test": False},
            "package": package, "reported_path": artifact},
        "compiled_sources": {"image_kind": "Detcore", "reverie_pin": pin,
            "manifest": source, "leaf": package, "reverie_root": "/fixture/reverie"},
        "qualifier_source": {"path": "/fixture/reverie/reverie-liteinst-artifact/src/lib.rs", "sha256": "2" * 64}, "compiled_qualifier_sha256": "2" * 64,
        "qualification": {"initializer": {"name": "detcore_liteinst_initialize"},
            "allocator_backing": backing, "detcore_constructor": constructor,
            "generic_bootstrap": constructor, "artifact": {"path": artifact, "sha256": digest}},
        "installed": {"sha256": digest}, "buck_argv": args,
        "buck_output_sha256": digest,
        "commands": [{"status": 0, "program": "buck2", "arguments": args[1:],
                      "stdout": {"path": "/fixture/logs/buck2-stdout", "sha256": "6" * 64}},
                     {"status": 0, "program": "readelf", "arguments": ["--version-info", "--wide", artifact]},
                     {"status": 0, "program": "objdump", "arguments": ["--disassemble", artifact]}],
        "source_close_completed": True}
    (resources / ("libreverie_liteinst.so.revision" if core else "sabre.revision")).write_text(pin + "\n")
    (resources / "libdetcore_liteinst.package.json").write_text(json.dumps(record))
    return record


def declared_inventory_sha256(inventory):
    # The producer's digest input: serde's compact encoding of inventory
    # entries in their declared (relative, size, sha256) field order.
    canonical = [{"relative": entry["relative"], "size": entry["size"],
                  "sha256": entry["sha256"]} for entry in inventory]
    encoded = json.dumps(canonical, separators=(",", ":"), ensure_ascii=False)
    return hashlib.sha256(encoded.encode()).hexdigest()


def path_fixture_receipt(install, pin, *, core=False):
    # The Cargo fixture stamped as the step-1/2 path layout: the same
    # receipt plus the declared-content authority a HermitPath compiled
    # source record serializes (inventories, their digests, and the path
    # compiler-root binding with no Cargo checkout).
    resources = Path(install) / "rsrcs"
    record = fixture_receipt(install, pin, core=core)
    record = copy.deepcopy(record)
    workspace = [{"relative": "Cargo.lock", "size": 11, "sha256": "7" * 64},
                 {"relative": "Cargo.toml", "size": 3, "sha256": "1" * 64}]
    reverie = [{"relative": "reverie-inguest/src/lib.rs", "size": 5, "sha256": "8" * 64},
               {"relative": "reverie-liteinst-artifact/src/lib.rs", "size": 7, "sha256": "2" * 64}]
    record["compiled_sources"].update({
        "layout": "HermitPath", "workspace_root": "/fixture",
        "authority": {"kind": "declared_content",
            "workspace_inventory": workspace,
            "workspace_sha256": declared_inventory_sha256(workspace),
            "reverie_inventory": reverie,
            "reverie_sha256": declared_inventory_sha256(reverie),
            "compiler_root": {"logical_root": "/fixture/reverie",
                              "canonical_root": "/fixture/reverie",
                              "cargo_home": None}}})
    (resources / "libdetcore_liteinst.package.json").write_text(json.dumps(record))
    return record


def buck_fixture_record():
    with tempfile.TemporaryDirectory() as tmp:
        install = Path(tmp)
        resources = install / "rsrcs"
        resources.mkdir()
        (resources / "libdetcore_liteinst.so").write_bytes(
            b"synthetic reader fixture, not an ELF proof")
        record = buck_fixture_receipt(install, "a" * 40)
        verifier.verify(install)
    return record


def finalizer_literals(case, root):
    finalizer = (root / FINALIZER_NAME).read_text()

    def rust_const(name):
        match = re.search(rf'const {name}: &str = "([^"]+)";', finalizer)
        case.assertIsNotNone(match, name)
        return match.group(1)

    def rust_list(name):
        match = re.search(rf"const {name}: \[&str; \d+\] = \[(.*?)\];", finalizer, re.S)
        case.assertIsNotNone(match, name)
        return re.findall(r'"([^"]*)"', match.group(1))

    target = rust_const("BUCK_TARGET")
    platform = rust_const("BUCK_PLATFORM")
    # The finalizer's buck_argv(), resolved token by token.
    argv_match = re.search(r"fn buck_argv\(\)[^{]*\{.*?vec!\[(.*?)\]\s*\}", finalizer, re.S)
    case.assertIsNotNone(argv_match)
    resolved = []
    for literal, constant in re.findall(
            r'"([^"]+)"\.into\(\)|(BUCK_TARGET|BUCK_PLATFORM)\.into\(\)', argv_match.group(1)):
        resolved.append(literal or {"BUCK_TARGET": target,
                                     "BUCK_PLATFORM": platform}[constant])
    return {"target": target, "platform": platform,
            "constraints": rust_list("BUCK_CONSTRAINTS"),
            "flags": rust_list("BUCK_RUSTC_FLAGS"), "argv": resolved}


def profile_literals(case, root):
    # The profile .bzl: the flag list, the alias platform and its
    # additional constraints.
    profile = export_file_text(root, PROFILE_NAME,
                               "the liteinst-runtime profile comparison")
    flags = re.search(r"LITEINST_RUNTIME_RUSTC_FLAGS = \[(.*?)\]", profile, re.S)
    case.assertIsNotNone(flags, PROFILE_NAME)
    platform = re.search(r'platform = "([^"]+)"', profile)
    case.assertIsNotNone(platform, PROFILE_NAME)
    # The list holds a bracketed value literal
    # (debug-assertions[on]), so it ends at the platform line that
    # follows it, not at the first closing bracket.
    additional = re.search(
        r"additional_constraints = \[(.*?)\],?\s*\n\s*platform =", profile, re.S)
    case.assertIsNotNone(additional, PROFILE_NAME)
    additional_text = additional.group(1)
    additional_values = re.findall(r'"([^"]*)"', additional_text)
    if "LITEINST_RUNTIME_FLAGS_CONSTRAINT" in additional_text:
        # The constant's canonical home is the constraints package;
        # the profile re-exports it without a literal.
        constraints_bzl = (root.parent / REVERIE_CONSTRAINTS_NAME).read_text()
        match = (re.search(r'LITEINST_RUNTIME_FLAGS_CONSTRAINT = "([^"]+)"', constraints_bzl)
                 or re.search(r'LITEINST_RUNTIME_FLAGS_CONSTRAINT = "([^"]+)"', profile))
        case.assertIsNotNone(match, "LITEINST_RUNTIME_FLAGS_CONSTRAINT literal")
        additional_values.append(match.group(1))
    return {"flags": re.findall(r'"([^"]*)"', flags.group(1)),
            "platform": platform.group(1),
            "additional": additional_values}


def buck_literals(case, root):
    # The root BUCK: the profiled alias name and the runtime target's
    # declared cpu/os compatibility.
    buck = (root / BUCK_NAME).read_text()
    alias = re.search(r'detcore_liteinst_runtime_alias\(\s*name = "([^"]+)"', buck)
    case.assertIsNotNone(alias, BUCK_NAME)
    compat = re.search(r'name = "detcore-liteinst",.*?target_compatible_with = \[(.*?)\]',
                       buck, re.S)
    case.assertIsNotNone(compat, BUCK_NAME)
    return {"compat": re.findall(r'"([^"]*)"', compat.group(1)),
            "canonical_target": f"//hermetic_infra/hermit:{alias.group(1)}[cdylib]"}


def check_finalizer_literals(case, root):
    rust = finalizer_literals(case, root)
    record = buck_fixture_record()
    case.assertEqual(record["buck_target"], rust["target"])
    case.assertEqual(record["buck_target"], verifier.BUCK_TARGET)
    case.assertEqual(record["buck_platform"], rust["platform"])
    case.assertEqual(record["buck_platform"], verifier.BUCK_PLATFORM)
    case.assertEqual(record["buck_constraints"], rust["constraints"])
    case.assertEqual(record["buck_constraints"], verifier.BUCK_CONSTRAINTS)
    case.assertEqual(record["buck_constraints"], sorted(record["buck_constraints"]))
    flags = record["profile_objects"]["liteinst-runtime"]["rustc_flags"]
    case.assertEqual(len(flags), 8)
    case.assertEqual(flags, rust["flags"])
    case.assertEqual(flags, verifier.BUCK_RUSTC_FLAGS)
    case.assertEqual(record["buck_argv"], rust["argv"])
    case.assertEqual(record["buck_argv"], verifier.BUCK_ARGV)
    buck2_entries = [command for command in record["commands"]
                     if command["program"] == "buck2"]
    case.assertEqual([entry["arguments"] for entry in buck2_entries],
                     [record["buck_argv"][1:]])


def check_profile_literals(case, root):
    bzl = profile_literals(case, root)
    record = buck_fixture_record()
    case.assertEqual(record["buck_platform"], bzl["platform"])
    flags = record["profile_objects"]["liteinst-runtime"]["rustc_flags"]
    case.assertEqual(flags, bzl["flags"])


def check_buck_literals(case, root):
    buck = buck_literals(case, root)
    record = buck_fixture_record()
    case.assertEqual(buck["canonical_target"],
                     "//hermetic_infra/hermit:detcore-liteinst-liteinst-runtime[cdylib]")
    case.assertEqual(record["buck_target"], buck["canonical_target"])


def check_profile_and_buck_literals(case, root):
    bzl = profile_literals(case, root)
    buck = buck_literals(case, root)
    record = buck_fixture_record()
    case.assertEqual(record["buck_constraints"],
                     sorted(buck["compat"] + bzl["additional"]))
    case.assertEqual(record["buck_argv"],
                     ["buck2", "build", buck["canonical_target"],
                      "--target-platforms", bzl["platform"],
                      "--show-full-output", "--client-metadata",
                      "id=stage-detcore-liteinst"])


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


    def test_core_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            runtime = resources / "libdetcore_liteinst.so"
            runtime.write_bytes(b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            good = fixture_receipt(install, "a" * 40, core=True)
            verifier.verify(install, core=True)
            marker=resources/'libreverie_liteinst.so.revision'
            marker.write_text('b'*40+'\n')
            with self.assertRaisesRegex(ValueError,'different Reverie pins'):
                verifier.verify(install,core=True)
            marker.write_text('a'*40+'\n')
            for field, value in [("profile_name", "validate"), ("source_close_completed", False)]:
                bad = copy.deepcopy(good)
                bad[field] = value
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install, core=True)
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
                    verifier.verify(install, core=True)
            receipt.write_text(json.dumps(good))
            runtime.write_bytes(b"changed installed bytes")
            with self.assertRaises(ValueError):
                verifier.verify(install, core=True)
            receipt.unlink()
            with self.assertRaises(ValueError):
                verifier.verify(install, core=True)


    def _buck_controls(self, core):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            runtime = resources / "libdetcore_liteinst.so"
            runtime.write_bytes(b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            # Round-trip so nested mutations cannot alias fixture dicts.
            good = json.loads(json.dumps(buck_fixture_receipt(install, "a" * 40, core=core)))
            verifier.verify(install, core=core)

            def refused(bad):
                receipt.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    verifier.verify(install, core=core)

            for field, value in [("schema", "hermit-packaged-detcore/v2"),
                    ("reverie_pin", "b" * 40), ("profile_name", "validate"),
                    ("buck_target", "//hermetic_infra/hermit:detcore-liteinst"),
                    ("buck_platform", "ovr_config//platform/linux:aarch64-fbcode"),
                    ("source_close_completed", False)]:
                bad = copy.deepcopy(good)
                bad[field] = value
                refused(bad)
            for section, field, value in [("buck_artifact", "features", ["allocator-fixture"]),
                    ("compiled_sources", "reverie_pin", "b" * 40),
                    ("compiled_sources", "image_kind", "Other"),
                    ("profile_inputs", "sha256", "0" * 64),
                    ("installed", "sha256", "0" * 64),
                    ("qualifier_source", "path", "/wrong/proof.rs"),
                    ("qualifier_source", "sha256", "0" * 64),
                    ("compiled_qualifier_sha256", None, "0" * 64),
                    ("buck_artifact", "reported_path", "/wrong/libdetcore_liteinst.so"),
                    ("qualification", "generic_bootstrap", {}),
                    ("qualification", "allocator_backing", {"success": False}),
                    ("qualification", "detcore_constructor", {"allocation_length": 232})]:
                bad = copy.deepcopy(good)
                if field is None:
                    bad[section] = value
                else:
                    bad[section][field] = value
                refused(bad)
            for field, value in [("opt_level", "2"), ("debuginfo", 2),
                    ("debug_assertions", False), ("overflow_checks", False),
                    ("test", True)]:
                bad = copy.deepcopy(good)
                bad["buck_artifact"]["profile"][field] = value
                refused(bad)
            for field, value in [("name", "other"), ("target", "other"),
                    ("source", "registry"), ("kind", ["lib"]),
                    ("crate_types", ["lib"])]:
                bad = copy.deepcopy(good)
                bad["buck_artifact"]["package"][field] = value
                refused(bad)
            flags = good["profile_objects"]["liteinst-runtime"]["rustc_flags"]
            for index, flag in enumerate(flags):
                bad = copy.deepcopy(good)
                bad["profile_objects"]["liteinst-runtime"]["rustc_flags"][index] = flag + "X"
                refused(bad)
            for mutated in [flags + ["-Clto=off"], flags + ["-Clto=thin"],
                    flags[:-1], flags[::-1]]:
                bad = copy.deepcopy(good)
                bad["profile_objects"]["liteinst-runtime"]["rustc_flags"] = mutated
                refused(bad)
            bad = copy.deepcopy(good)
            bad["profile_objects"]["liteinst-runtime"]["lto"] = False
            refused(bad)
            bad = copy.deepcopy(good)
            bad["profile_objects"]["validate"] = {"inherits": "release"}
            refused(bad)
            for index, argument in enumerate(good["buck_argv"]):
                bad = copy.deepcopy(good)
                bad["buck_argv"][index] = argument + "X"
                refused(bad)
            for index, constraint in enumerate(good["buck_constraints"]):
                bad = copy.deepcopy(good)
                bad["buck_constraints"][index] = constraint + "X"
                refused(bad)
            bad = copy.deepcopy(good)
            bad["buck_constraints"] = good["buck_constraints"][::-1]
            refused(bad)
            for commands in [[{"status": 1, "program": "readelf", "arguments": []},
                              {"status": 0, "program": "objdump", "arguments": []}],
                    [{"status": 0, "program": "readelf", "arguments": []}],
                    [{"status": 0, "program": "objdump", "arguments": []}],
                    []]:
                bad = copy.deepcopy(good)
                bad["commands"] = commands
                refused(bad)
            # Appended argv/constraint elements are not the pinned build.
            bad = copy.deepcopy(good)
            bad["buck_argv"] = good["buck_argv"] + ["--extra"]
            refused(bad)
            bad = copy.deepcopy(good)
            bad["buck_argv"] = good["buck_argv"][:-1]
            refused(bad)
            bad = copy.deepcopy(good)
            bad["buck_constraints"] = good["buck_constraints"] + ["ovr_config//build_mode/constraints:dbgsym"]
            refused(bad)
            bad = copy.deepcopy(good)
            bad["buck_constraints"] = good["buck_constraints"][:-1]
            refused(bad)
            # The flags constraint specifically: the other five without
            # it are not the pinned build either. Neither is the
            # pre-alias three-constraint set this pin grew from.
            bad = copy.deepcopy(good)
            bad["buck_constraints"] = [
                c for c in good["buck_constraints"] if "liteinst-runtime-flags" not in c]
            refused(bad)
            bad = copy.deepcopy(good)
            bad["buck_constraints"] = ["ovr_config//build_mode/constraints:opt",
                                       "ovr_config//cpu:x86_64", "ovr_config//os:linux"]
            refused(bad)
            # The executed producer entry binds exactly: program buck2 with
            # arguments == buck_argv[1:], and readelf/objdump entries over
            # the staged artifact path. Missing, garbage or wrong-artifact
            # entries refuse even when every retained field is pinned.
            buck2_entry, readelf_entry, objdump_entry = good["commands"]
            artifact = good["buck_artifact"]["reported_path"]
            for commands in [
                    # No buck2 producer entry at all.
                    [readelf_entry, objdump_entry],
                    # buck2 present but not the pinned invocation.
                    [{"status": 0, "program": "buck2", "arguments": ["build"]},
                     readelf_entry, objdump_entry],
                    [{"status": 0, "program": "buck2", "arguments": good["buck_argv"]},
                     readelf_entry, objdump_entry],
                    [{"status": 0, "program": "buck2", "arguments": good["buck_argv"][1:] + ["--extra"]},
                     readelf_entry, objdump_entry],
                    # Garbage qualification arguments: no artifact path.
                    [buck2_entry,
                     {"status": 0, "program": "readelf", "arguments": ["--version"]},
                     objdump_entry],
                    [buck2_entry, readelf_entry,
                     {"status": 0, "program": "objdump", "arguments": ["--version"]}],
                    # Qualification ran over a different artifact.
                    [buck2_entry,
                     {"status": 0, "program": "readelf", "arguments": ["--wide", "/other/lib.so"]},
                     {"status": 0, "program": "objdump", "arguments": ["--disassemble", "/other/lib.so"]}],
                    # Only buck2: no qualification commands.
                    [buck2_entry]]:
                bad = copy.deepcopy(good)
                bad["commands"] = copy.deepcopy(commands)
                refused(bad)
            # The producer-computed output digest binds to the installed
            # bytes, and the single buck2 entry must carry its retained
            # stdout identity: a synthesized entry without one refuses.
            bad = copy.deepcopy(good)
            bad["buck_output_sha256"] = "0" * 64
            refused(bad)
            bad = copy.deepcopy(good)
            del bad["commands"][0]["stdout"]
            refused(bad)
            bad = copy.deepcopy(good)
            bad["commands"][0]["stdout"]["sha256"] = "not-a-digest"
            refused(bad)
            bad = copy.deepcopy(good)
            bad["commands"] = copy.deepcopy(good["commands"]) + [copy.deepcopy(buck2_entry)]
            refused(bad)
            # Deleted keys are a clean refusal (ValueError), never a leaked
            # KeyError; refused() asserts ValueError, which a KeyError fails.
            for key in ("schema", "reverie_pin", "profile_name", "profile_inputs",
                    "profile_objects", "buck_target", "buck_platform",
                    "buck_constraints", "buck_argv", "buck_output_sha256", "buck_artifact",
                    "compiled_sources", "qualifier_source",
                    "compiled_qualifier_sha256", "qualification", "installed",
                    "commands", "source_close_completed"):
                bad = copy.deepcopy(good)
                del bad[key]
                refused(bad)
            for section, field in [("buck_artifact", "features"),
                    ("buck_artifact", "profile"), ("buck_artifact", "package"),
                    ("buck_artifact", "reported_path"),
                    ("compiled_sources", "manifest"), ("compiled_sources", "leaf"),
                    ("qualifier_source", "sha256"), ("qualification", "artifact"),
                    ("qualification", "initializer"), ("installed", "sha256")]:
                bad = copy.deepcopy(good)
                del bad[section][field]
                refused(bad)
            for field in ("status", "program", "arguments"):
                bad = copy.deepcopy(good)
                del bad["commands"][0][field]
                refused(bad)
            receipt.write_text(json.dumps(good))
            runtime.write_bytes(b"changed installed bytes")
            with self.assertRaises(ValueError):
                verifier.verify(install, core=core)
            receipt.unlink()
            with self.assertRaises(ValueError):
                verifier.verify(install, core=core)

    def test_buck_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        self._buck_controls(core=False)

    def test_core_buck_receipt_binds_profile_feature_artifact_source_close_and_installed_bytes(self):
        self._buck_controls(core=True)

    def test_buck_literals_match_finalizer(self):
        """Guard against copy drift: the fixture/verifier Buck literals must
        equal the constants extracted from the finalizer source, not merely
        agree with each other. The finalizer is exported, so this always runs."""
        check_finalizer_literals(self, hermit_root(self))

    def test_buck_literals_match_profile(self):
        """The fixture/verifier literals equal the profile .bzl texts.
        Skipped only where the checkout does not carry that file (the export)."""
        check_profile_literals(self, hermit_root(self))

    def test_buck_literals_match_buck_alias(self):
        """The fixture/verifier target equals the root BUCK alias.
        The export carries BUCK, so this runs everywhere."""
        check_buck_literals(self, hermit_root(self))

    def test_buck_literals_match_profile_and_buck_alias(self):
        """The constraints and buck_argv the profile and the root BUCK
        name together. Skipped only where the checkout does not carry the
        profile (the export)."""
        check_profile_and_buck_literals(self, hermit_root(self))

    def test_builder_dispatch_refuses_mixed_and_unknown_provenance(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            (resources / "libdetcore_liteinst.so").write_bytes(
                b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            cargo = fixture_receipt(install, "a" * 40)
            buck = buck_fixture_receipt(install, "a" * 40)

            def refused(record):
                receipt.write_text(json.dumps(record))
                with self.assertRaises(ValueError):
                    verifier.verify(install)

            # An explicit cargo builder takes the same arm as its absence.
            explicit = copy.deepcopy(cargo)
            explicit["builder"] = "cargo"
            receipt.write_text(json.dumps(explicit))
            verifier.verify(install)
            receipt.write_text(json.dumps(buck))
            verifier.verify(install)
            for key in ("cargo_argv", "cargo_artifact", "cargo_version"):
                bad = copy.deepcopy(buck)
                bad[key] = cargo.get(key, "synthetic")
                refused(bad)
            for key in ("buck_argv", "buck_artifact", "buck_target"):
                bad = copy.deepcopy(cargo)
                bad[key] = buck[key]
                refused(bad)
            bad = copy.deepcopy(cargo)
            bad["builder"] = "buck2"
            refused(bad)
            bad = copy.deepcopy(buck)
            del bad["builder"]
            refused(bad)
            bad = copy.deepcopy(buck)
            bad["builder"] = "ninja"
            refused(bad)

    def _path_controls(self, core):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp)
            resources = install / "rsrcs"
            resources.mkdir()
            (resources / "libdetcore_liteinst.so").write_bytes(
                b"synthetic reader fixture, not an ELF proof")
            receipt = resources / "libdetcore_liteinst.package.json"
            # Round-trip so nested mutations cannot alias fixture dicts.
            good = json.loads(json.dumps(
                path_fixture_receipt(install, "a" * 40, core=core)))
            verifier.verify(install, core=core)

            def refused(bad, pattern=None):
                receipt.write_text(json.dumps(bad))
                if pattern is None:
                    with self.assertRaises(ValueError):
                        verifier.verify(install, core=core)
                else:
                    with self.assertRaisesRegex(ValueError, pattern):
                        verifier.verify(install, core=core)

            def mutated_sources(mutate):
                bad = copy.deepcopy(good)
                mutate(bad["compiled_sources"])
                return bad

            # The path layout without its content authority is the gap this
            # step closes: absence, the Git arm's authority kind, and a
            # non-object authority all refuse by name.
            refused(mutated_sources(lambda sources: sources.pop("authority")),
                    "lacks declared-content authority")
            refused(mutated_sources(
                lambda sources: sources["authority"].__setitem__("kind", "git")),
                "lacks declared-content authority")
            refused(mutated_sources(
                lambda sources: sources.__setitem__("authority", "declared")),
                "lacks declared-content authority")
            # Both declared inventories and both digests are required;
            # junk digests refuse whether misshapen or merely unbound.
            for field in ("workspace_inventory", "reverie_inventory",
                          "workspace_sha256", "reverie_sha256", "compiler_root"):
                refused(mutated_sources(
                    lambda sources, field=field: sources["authority"].pop(field)))
            refused(mutated_sources(
                lambda sources: sources.pop("workspace_root")))
            refused(mutated_sources(
                lambda sources: sources["authority"].__setitem__(
                    "reverie_inventory", [])))
            refused(mutated_sources(
                lambda sources: sources["authority"].__setitem__(
                    "reverie_sha256", "junk")),
                "lacks the declared reverie inventory digest")
            refused(mutated_sources(
                lambda sources: sources["authority"].__setitem__(
                    "reverie_sha256", "0" * 64)),
                "does not bind its inventory")
            refused(mutated_sources(
                lambda sources: sources["authority"].__setitem__(
                    "workspace_sha256", "0" * 64)),
                "does not bind its inventory")
            for entry in ("reverie_inventory", "workspace_inventory"):
                refused(mutated_sources(
                    lambda sources, entry=entry: sources["authority"][entry][0]
                    .__setitem__("sha256", "not-a-digest")))
                # A well-formed but different entry digest no longer
                # matches the retained inventory digest.
                refused(mutated_sources(
                    lambda sources, entry=entry: sources["authority"][entry][0]
                    .__setitem__("sha256", "9" * 64)))
            refused(mutated_sources(
                lambda sources: sources["authority"]["reverie_inventory"][0]
                .__setitem__("size", -1)))
            refused(mutated_sources(
                lambda sources: sources["authority"]["reverie_inventory"][0]
                .__setitem__("extra", True)))
            refused(mutated_sources(
                lambda sources: sources["authority"]["reverie_inventory"][0]
                .pop("relative")))
            # Reversed order refuses even with a digest recomputed over the
            # reversed list: the serialized inventory is canonical.
            def reversed_inventory(sources):
                authority = sources["authority"]
                authority["reverie_inventory"] = authority["reverie_inventory"][::-1]
                authority["reverie_sha256"] = declared_inventory_sha256(
                    authority["reverie_inventory"])
            refused(mutated_sources(reversed_inventory), "canonical order")
            # The compiler root is the path binding: logical
            # <workspace>/reverie over the receipt's Reverie root, and no
            # Cargo checkout.
            refused(mutated_sources(
                lambda sources: sources["authority"]["compiler_root"]
                .__setitem__("logical_root", "/other/reverie")))
            refused(mutated_sources(
                lambda sources: sources["authority"]["compiler_root"]
                .__setitem__("canonical_root", "/other/reverie")))
            refused(mutated_sources(
                lambda sources: sources["authority"]["compiler_root"]
                .__setitem__("cargo_home", ["/cargo", "/cargo"])),
                "names a Cargo checkout")
            refused(mutated_sources(
                lambda sources: sources["authority"]["compiler_root"]
                .pop("cargo_home")), "names a Cargo checkout")
            # The same receipt as the Git layout, authority removed, takes
            # the original Cargo arm unchanged.
            git = copy.deepcopy(good)
            git["compiled_sources"]["layout"] = "HermitGit"
            del git["compiled_sources"]["authority"]
            del git["compiled_sources"]["workspace_root"]
            receipt.write_text(json.dumps(git))
            verifier.verify(install, core=core)

    def test_path_receipt_requires_declared_content_authority(self):
        self._path_controls(core=False)

    def test_core_path_receipt_requires_declared_content_authority(self):
        self._path_controls(core=True)


class BuckLiteralExportSkipControls(unittest.TestCase):
    """The profile comparison skips only where the checkout does not carry
    the profile; BUCK, which the export carries, is always compared; a
    present file is compared and can fail. The checkout's answer is
    ci/checkout-carries.sh's (tested with the scripts that share it); here
    a stand-in gives the same answer for a commit that tracks everything but
    the profile, so these controls need no git, as under Buck."""

    def _scratch_root(self, profile=True, buck=True, tamper_profile=False):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        flags = list(verifier.BUCK_RUSTC_FLAGS)
        if tamper_profile:
            flags[0] += "X"
        quoted = ", ".join(f'"{flag}"' for flag in flags)
        if profile:
            path = root / PROFILE_NAME
            path.parent.mkdir(parents=True)
            path.write_text(
                f"LITEINST_RUNTIME_RUSTC_FLAGS = [{quoted}]\n"
                'additional_constraints = ['
                '"fbcode//hermetic_infra/reverie/constraints:liteinst-runtime-flags[applied]", '
                '"fbcode_macros//config/constraints:ld-as-needed-default-enabled", '
                '"ovr_config//build_mode/constraints:opt", '
                '"ovr_config//rust:debug-assertions[on]"]\n'
                f'platform = "{verifier.BUCK_PLATFORM}"\n')
        if buck:
            (root / BUCK_NAME).write_text(
                'detcore_liteinst_runtime_alias(\n'
                '    name = "detcore-liteinst-liteinst-runtime",\n)\n'
                'rust_library(\n'
                '    name = "detcore-liteinst",\n'
                '    target_compatible_with = ["ovr_config//cpu:x86_64", '
                '"ovr_config//os:linux"],\n)\n')
        return root

    def _profile_untracked(self):
        """Answer as ci/checkout-carries.sh would for a commit that tracks
        everything but the profile, a path the export has never carried:
        lacking means that path, and nothing at it."""
        def lacks(root, name, tool_root=None):
            return name == PROFILE_NAME and not os.path.lexists(Path(root) / name)
        return mock.patch.object(sys.modules[__name__], "checkout_lacks", lacks)

    def test_only_a_checkout_that_does_not_carry_the_profile_skips(self):
        root = self._scratch_root(profile=False, buck=True)
        with self._profile_untracked():
            reason = export_skip_reason(root, PROFILE_NAME,
                                        "the liteinst-runtime profile comparison")
            self.assertIsNotNone(reason)
            self.assertTrue(reason.startswith(
                f"SKIPPED: {PROFILE_NAME} is absent and this checkout's commit "
                "does not track it"), reason)
            with self.assertRaises(unittest.SkipTest) as skipped:
                check_profile_literals(self, root)
            self.assertEqual(str(skipped.exception), reason)
            # The BUCK comparison still runs on the same tree.
            check_buck_literals(self, root)
        # A commit that tracks the profile fails when it is missing: no skip.
        with mock.patch.object(sys.modules[__name__], "checkout_lacks",
                               lambda root, name, tool_root=None: False):
            self.assertIsNone(export_skip_reason(root, PROFILE_NAME, "the comparison"))
            with self.assertRaises(FileNotFoundError):
                check_profile_literals(self, root)

    def test_a_missing_buck_always_fails(self):
        # The export carries BUCK, so its absence is a deletion: no skip,
        # whatever the shared rule would say about the profile.
        root = self._scratch_root(profile=True, buck=False)
        with self._profile_untracked():
            check_profile_literals(self, root)
            with self.assertRaises(FileNotFoundError):
                check_buck_literals(self, root)

    def test_the_real_rule_never_skips_where_git_cannot_answer(self):
        # The scratch root is no worktree (and under Buck there is no tool at
        # all), so the shared rule answers "carried": the comparison runs and
        # fails on the missing file. This is the Buck resources case.
        root = self._scratch_root(profile=False, buck=True)
        self.assertFalse(checkout_lacks(root, PROFILE_NAME))
        self.assertIsNone(export_skip_reason(root, PROFILE_NAME, "the comparison"))
        with self.assertRaises(FileNotFoundError):
            check_profile_literals(self, root)

    def test_a_checkout_with_the_profile_runs_the_comparison(self):
        root = self._scratch_root(profile=False, buck=False)
        profile = root / PROFILE_NAME
        profile.parent.mkdir(parents=True)
        profile.write_text("")
        with self._profile_untracked():
            self.assertIsNone(export_skip_reason(root, PROFILE_NAME, "the comparison"))
            # Something at the path that is not a readable file is not
            # absent either: the comparison runs and fails on it.
            profile.unlink()
            profile.symlink_to(root / "nowhere")
            self.assertIsNone(export_skip_reason(root, PROFILE_NAME, "the comparison"))
            profile.unlink()
            profile.mkdir()
            self.assertIsNone(export_skip_reason(root, PROFILE_NAME, "the comparison"))

    def test_both_present_run_and_a_tampered_profile_fails_not_skips(self):
        root = self._scratch_root(profile=True, buck=True)
        with self._profile_untracked():
            check_profile_literals(self, root)
            check_buck_literals(self, root)
            check_profile_and_buck_literals(self, root)
            tampered = self._scratch_root(profile=True, buck=True, tamper_profile=True)
            with self.assertRaises(AssertionError):
                check_profile_literals(self, tampered)


class _SkipNamingResult(unittest.TextTestResult):
    """Name each skip that reaches unittest on stderr: the Makefile
    runs without -v, which prints no skip reasons. Only real skips
    print (controls that catch SkipTest on purpose stay silent), once
    per skipped test."""

    def addSkip(self, test, reason):
        print(f"test_packaged_detcore.py: {reason}",
              file=sys.stderr, flush=True)
        super().addSkip(test, reason)


class _SkipNamingRunner(unittest.TextTestRunner):
    # _makeResult is TextTestRunner's factory hook; assigning the
    # resultclass attribute directly is rejected by Pyre, whose
    # resultclass type is the factory signature this override matches.
    def _makeResult(self) -> unittest.TextTestResult:
        return _SkipNamingResult(
            self.stream, self.descriptions, self.verbosity)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] in ("--write-fixture", "--write-core-fixture"):
        fixture_receipt(sys.argv[2], sys.argv[3], core=sys.argv[1] == "--write-core-fixture")
    elif len(sys.argv) == 4 and sys.argv[1] in ("--write-buck-fixture", "--write-core-buck-fixture"):
        buck_fixture_receipt(sys.argv[2], sys.argv[3], core=sys.argv[1] == "--write-core-buck-fixture")
    else:
        unittest.main(testRunner=_SkipNamingRunner)
