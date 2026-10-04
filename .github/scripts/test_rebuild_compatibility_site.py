#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Contract tests for the hosted adapter; no GitHub write or website build."""

import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("rebuild-compatibility-site.py")
SPEC = importlib.util.spec_from_file_location("nightly_adapter", SCRIPT)
ADAPTER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ADAPTER)


class HostedAdapterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.config = ADAPTER.configuration()
        self.baseline = json.loads(ADAPTER.CHECKED_IN.read_bytes())
        self.pin = copy.deepcopy(self.baseline["releases"][0])

    def test_config_accepts_exact_source_and_refuses_floating_unknown_or_foreign(self):
        path = self.root / "config.json"
        path.write_text(json.dumps(self.config))
        self.assertEqual(ADAPTER.configuration(path), self.config)
        for key, value in (
            ("parent_commit", "main"),
            ("hermit_commit", "abc"),
            ("parent_repository", "foreign/source"),
            ("extra", True),
            ("schema", 2),
        ):
            wrong = dict(self.config)
            wrong[key] = value
            path.write_text(json.dumps(wrong))
            with self.subTest(key=key), self.assertRaises(ValueError):
                ADAPTER.configuration(path)

    def source_git(self, root, *args):
        if args == ("rev-parse", "HEAD^{commit}"):
            return self.config[
                "hermit_commit" if root.name == "hermit" else "parent_commit"
            ]
        if args == ("rev-parse", "HEAD:hermit"):
            return self.config["hermit_commit"]
        if args[0] == "status":
            return ""
        if args == ("config", "--get", "remote.origin.url"):
            return ADAPTER.LEDGER
        self.fail(f"unexpected Git query {args}")

    def test_sources_refuse_wrong_head_gitlink_dirty_tree_and_foreign_ledger(self):
        with patch.object(ADAPTER, "git", side_effect=self.source_git):
            self.assertEqual(
                ADAPTER.require_sources(self.root, self.config),
                self.root / "hermit_test_ledger",
            )
        for fault in ("parent", "hermit", "gitlink", "dirty", "ledger"):

            def wrong(root, *args, fault=fault):
                result = self.source_git(root, *args)
                if (
                    fault == "parent"
                    and root == self.root
                    and args == ("rev-parse", "HEAD^{commit}")
                    or fault == "hermit"
                    and root.name == "hermit"
                    and args == ("rev-parse", "HEAD^{commit}")
                    or fault == "gitlink"
                    and args == ("rev-parse", "HEAD:hermit")
                ):
                    return "0" * 40
                if fault == "dirty" and args[0] == "status":
                    return " M ci-hub/example.py"
                if fault == "ledger" and args[0] == "config":
                    return "https://example.invalid/history.git"
                return result

            with (
                self.subTest(fault=fault),
                patch.object(ADAPTER, "git", side_effect=wrong),
                self.assertRaises(ValueError),
            ):
                ADAPTER.require_sources(self.root, self.config)

    def test_append_preserves_every_pin_and_refuses_identity_rewrite(self):
        pin = copy.deepcopy(self.pin)
        pin["identity"] = "f" * 64
        result = ADAPTER.updated_registry(self.baseline, pin)
        for old in self.baseline["releases"]:
            self.assertIn(old, result["releases"])
        self.assertEqual(result["latest_identity"], pin["identity"])
        self.assertEqual(ADAPTER.updated_registry(result, pin), result)
        changed = copy.deepcopy(self.pin)
        changed["archive_bytes"] += 1
        with self.assertRaisesRegex(ValueError, "immutable"):
            ADAPTER.updated_registry(self.baseline, changed)

    def test_release_requires_stable_identity_and_exact_single_asset(self):
        release = {
            "tag_name": self.pin["tag"],
            "name": self.pin["release_title"],
            "draft": False,
            "prerelease": False,
            "assets": [
                {
                    "name": "site.tar.gz",
                    "state": "uploaded",
                    "id": 23,
                    "digest": "sha256:" + "a" * 64,
                    "size": 12,
                }
            ],
        }
        self.assertEqual(
            ADAPTER.asset_id(release, self.pin, "site.tar.gz", "a" * 64, 12), 23
        )
        for field, value in (
            ("draft", True),
            ("prerelease", True),
            ("tag_name", "other"),
            ("name", "other"),
            ("assets", []),
            ("assets", release["assets"] * 2),
        ):
            wrong = copy.deepcopy(release)
            wrong[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                ADAPTER.asset_id(wrong, self.pin, "site.tar.gz", "a" * 64, 12)
        for field, value in (
            ("size", 13),
            ("digest", "sha256:" + "b" * 64),
            ("id", "23"),
            ("state", "new"),
        ):
            wrong = copy.deepcopy(release)
            wrong["assets"][0][field] = value
            with self.subTest(asset_field=field), self.assertRaises(ValueError):
                ADAPTER.asset_id(wrong, self.pin, "site.tar.gz", "a" * 64, 12)

    def test_release_lookup_does_not_treat_permission_or_transport_failure_as_absence(
        self,
    ):
        responses = [(404, 1, None), (403, 1, ValueError), (500, 1, ValueError)]
        for status, code, error in responses:
            result = subprocess.CompletedProcess(
                [], code, f"HTTP/2.0 {status} x\r\n\r\n{{}}".encode(), b"failed"
            )
            with (
                self.subTest(status=status),
                patch.object(ADAPTER.subprocess, "run", return_value=result),
            ):
                if error:
                    with self.assertRaises(error):
                        ADAPTER.release_by_tag("tag")
                else:
                    self.assertIsNone(ADAPTER.release_by_tag("tag"))

    def test_release_request_is_stable_non_latest_and_never_overwrites_an_asset(self):
        archive, registry = self.root / self.pin["asset"], self.root / "registry.json"
        archive.write_bytes(b"archive")
        registry.write_bytes(b"registry")
        pin = {
            **self.pin,
            "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
            "archive_bytes": archive.stat().st_size,
        }
        assets = [
            {
                "name": p.name,
                "size": p.stat().st_size,
                "state": "uploaded",
                "id": i,
                "digest": "sha256:" + hashlib.sha256(p.read_bytes()).hexdigest(),
            }
            for i, p in enumerate((archive, registry), 20)
        ]
        release = {
            "tag_name": pin["tag"],
            "name": pin["release_title"],
            "draft": False,
            "prerelease": False,
            "assets": assets,
        }
        with (
            patch.object(ADAPTER, "release_by_tag", side_effect=[None, release]),
            patch.object(ADAPTER, "gh") as request,
        ):
            self.assertEqual(
                ADAPTER.publish_release(
                    pin, archive, registry, "a" * 40, self.config["hermit_commit"]
                ),
                21,
            )
        args = request.call_args.args
        self.assertEqual(args[:3], ("release", "create", pin["tag"]))
        self.assertIn("--latest=false", args)
        self.assertEqual(args[args.index("--target") + 1], self.config["hermit_commit"])
        for forbidden in ("--draft", "--prerelease", "--clobber"):
            self.assertNotIn(forbidden, args)
        with (
            patch.object(ADAPTER, "release_by_tag", return_value=release),
            patch.object(ADAPTER, "gh") as request,
        ):
            self.assertEqual(
                ADAPTER.publish_release(
                    pin, archive, registry, "a" * 40, self.config["hermit_commit"]
                ),
                21,
            )
            request.assert_not_called()
        release["assets"][0]["digest"] = "sha256:" + "0" * 64
        with (
            patch.object(ADAPTER, "release_by_tag", return_value=release),
            patch.object(ADAPTER, "gh") as request,
            self.assertRaises(ValueError),
        ):
            ADAPTER.publish_release(
                pin, archive, registry, "a" * 40, self.config["hermit_commit"]
            )
        request.assert_not_called()

    def test_runtime_fetch_is_captured_once_and_builder_failure_cannot_reuse_old_archive(
        self,
    ):
        calls = []
        captured = "a" * 40
        history = "b" * 40
        builder = SimpleNamespace(build=lambda *a, **kw: None)

        def refused(output, **options):
            self.assertEqual(
                options,
                {
                    "series_ref": captured,
                    "published_only": True,
                    "state_root": self.root,
                    "validation_history_commit": history,
                },
            )
            raise RuntimeError("fresh ledger build refused")

        builder.build = refused

        def module(name, path):
            return builder if path.name == "build_and_serve.py" else SimpleNamespace()

        with (
            patch.object(
                ADAPTER,
                "require_sources",
                return_value=self.root / "hermit_test_ledger",
            ),
            patch.object(
                ADAPTER, "run", side_effect=lambda args, **kw: calls.append(args)
            ),
            patch.object(
                ADAPTER,
                "git",
                side_effect=lambda root, *args: history
                if root == self.root
                else captured,
            ),
            patch.object(ADAPTER, "load_module", side_effect=module),
            patch.object(
                ADAPTER,
                "published_registry",
                return_value=(self.baseline, self.root / "previous"),
            ),
            patch.object(ADAPTER, "publish_release") as publish,
            self.assertRaisesRegex(RuntimeError, "fresh ledger build refused"),
        ):
            ADAPTER.rebuild(
                self.root,
                self.root / "scratch",
                self.root / "receipt.json",
                ledger_commit=captured,
                validation_history_commit=history,
            )
        self.assertEqual(
            calls,
            [
                [
                    "git",
                    "-C",
                    self.root / "hermit_test_ledger",
                    "fetch",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "origin",
                    captured,
                ]
            ],
        )
        publish.assert_not_called()
        self.assertFalse((self.root / "receipt.json").exists())

    def test_ledger_capture_is_exact_and_invocation_failures_refuse(self):
        captured = "a" * 40
        with patch.object(
            ADAPTER, "run", return_value=(captured + "\trefs/heads/main\n").encode()
        ) as native:
            self.assertEqual(
                ADAPTER.capture_ledger(),
                {"repository": ADAPTER.LEDGER, "commit": captured},
            )
        native.assert_called_once_with(
            ["git", "ls-remote", "--exit-code", ADAPTER.LEDGER, "refs/heads/main"]
        )
        for output in (
            b"",
            b"main\trefs/heads/main\n",
            (captured + "\trefs/heads/other\n").encode(),
            ((captured + "\trefs/heads/main\n") * 2).encode(),
        ):
            with (
                self.subTest(output=output),
                patch.object(ADAPTER, "run", return_value=output),
                self.assertRaises(ValueError),
            ):
                ADAPTER.capture_ledger()
        for code in (126, 127):
            error = io.StringIO()
            with (
                self.subTest(code=code),
                patch.object(
                    ADAPTER,
                    "run",
                    side_effect=subprocess.CalledProcessError(code, "git"),
                ),
                contextlib.redirect_stderr(error),
            ):
                self.assertEqual(ADAPTER.main(["capture-ledger"]), 1)
            self.assertIn("install Git or repair GitHub access", error.getvalue())

    def test_bad_or_mixed_capture_refuses_before_builder_or_publication(self):
        for ledger, history in (("main", "b" * 40), ("a" * 40, "origin/main")):
            with (
                self.subTest(ledger=ledger, history=history),
                patch.object(ADAPTER, "require_sources") as source,
                self.assertRaisesRegex(ValueError, "full 40-hex"),
            ):
                ADAPTER.rebuild(
                    self.root,
                    self.root / "scratch",
                    self.root / "receipt.json",
                    ledger_commit=ledger,
                    validation_history_commit=history,
                )
            source.assert_not_called()
        with (
            patch.object(
                ADAPTER,
                "require_sources",
                return_value=self.root / "hermit_test_ledger",
            ),
            patch.object(ADAPTER, "git", return_value="c" * 40),
            patch.object(ADAPTER, "run") as native,
            patch.object(ADAPTER, "load_module") as builder,
            self.assertRaisesRegex(ValueError, "captured checkout main"),
        ):
            ADAPTER.rebuild(
                self.root,
                self.root / "scratch",
                self.root / "receipt.json",
                ledger_commit="a" * 40,
                validation_history_commit="b" * 40,
            )
        native.assert_not_called()
        builder.assert_not_called()

    def test_failed_build_retains_resource_facts_without_deployment_claim(self):
        receipt = self.root / "receipt.json"
        argv = [
            "rebuild",
            "--builder-parent",
            str(self.root),
            "--scratch",
            str(self.root / "scratch"),
            "--receipt",
            str(receipt),
            "--ledger-commit",
            "a" * 40,
            "--validation-history-commit",
            "b" * 40,
        ]
        with (
            patch.object(ADAPTER, "rebuild", side_effect=RuntimeError("build refused")),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(ADAPTER.main(argv), 1)
        self.assertFalse(receipt.exists())
        resources = receipt.with_suffix(".resources.json")
        before = resources.read_bytes()
        facts = json.loads(before)
        self.assertGreater(facts["before"]["cpu_affinity_count"], 0)
        self.assertGreater(facts["after"]["MemTotal_bytes"], 0)
        self.assertIn("not aggregate", facts["after"]["peak_scope"])
        self.assertIn("cgroup lifetime", facts["after"]["cgroup_peak_scope"])
        with (
            patch.object(ADAPTER, "rebuild") as build,
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(ADAPTER.main(argv), 1)
        build.assert_not_called()
        self.assertEqual(resources.read_bytes(), before)

    def test_public_readback_requires_exact_bytes_without_cache_buster(self):
        tree = self.root / "tree"
        for name in ADAPTER.PUBLIC_FILES:
            (tree / name).parent.mkdir(parents=True, exist_ok=True)
            (tree / name).write_bytes(name.encode())
        receipt = self.root / "receipt.json"
        receipt.write_text(json.dumps({"website": str(tree), "deployed": False}))
        import io

        calls = []

        def read(url, timeout):
            calls.append(url)
            name = url.removeprefix(ADAPTER.PUBLIC)
            return io.BytesIO((tree / name).read_bytes())

        with patch.object(ADAPTER.urllib.request, "urlopen", side_effect=read):
            self.assertTrue(ADAPTER.verify_public(receipt)["deployed"])
        self.assertEqual(len(calls), 9)
        self.assertTrue(all("?" not in url for url in calls))
        receipt.write_text(json.dumps({"website": str(tree), "deployed": False}))
        with (
            patch.object(
                ADAPTER.urllib.request, "urlopen", return_value=io.BytesIO(b"stale")
            ),
            patch.object(ADAPTER.time, "monotonic", side_effect=[0, 0, 0, 2, 2]),
            patch.object(ADAPTER.time, "sleep"),
            self.assertRaisesRegex(ValueError, "did not serve"),
        ):
            ADAPTER.verify_public(receipt, timeout=1)
        self.assertFalse(json.loads(receipt.read_bytes())["deployed"])

    def test_public_help_is_readable_and_has_no_build_side_effect(self):
        for command in (
            [],
            ["source-config"],
            ["capture-ledger"],
            ["rebuild"],
            ["verify-public"],
        ):
            for flag in ("-h", "--help"):
                with self.subTest(command=command, flag=flag):
                    result = subprocess.run(
                        [sys.executable, "-B", SCRIPT, *command, flag],
                        text=True,
                        capture_output=True,
                        timeout=5,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("usage:", result.stdout)
                    if not command:
                        self.assertIn("Does not run Hermit tests", result.stdout)
                    self.assertNotIn("Traceback", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
