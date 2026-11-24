#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Base-against-head identity check for the mini-crypto seam (off and on).

Evaluates both trees' ci/buck-e2e/defs.bzl and (for the fbsource view)
fb-only/e2e_cells.bzl with Python stand-ins, as ci/test_buck_e2e_cell.py
does for defs.bzl, over the head tree's expected plan. OFF must be
identical everywhere: fbsource targets in three configs, kept_cells()
and the e2e_cells extras, the OSS view, and an explicit false. The
bundle entry keys may differ only by the added "mini-crypto" entry.
ON must at least load in the fbsource view and keep system-utils
ptrace (never kvm) cells. Exit 0 with the counts, 1 on any difference.
"""

from __future__ import annotations

import argparse
import ast
import json
import pathlib
import sys


class StarlarkFail(Exception):
    pass


def _fail(message: str) -> None:
    raise StarlarkFail(message)


class _Anything:
    def __getattr__(self, _name):
        return _Anything()

    def __call__(self, *args, **kwargs):
        return _Anything()


def _exec_defs(path, config, targets):
    namespace = {}

    def rule(impl, attrs):
        if impl.__name__ == "_cell_test_impl":
            return lambda **kw: targets.__setitem__(kw["name"], kw)
        return _Anything()

    def load(_label, *names, **aliases):
        module = {"buck_sh_test": lambda **kw: targets.__setitem__(kw["name"], kw)}
        for name in names:
            namespace[name] = module[name]
        for alias, name in aliases.items():
            namespace[alias] = module[name]

    namespace.update({
        "rule": rule, "attrs": _Anything(), "load": load,
        "native": _Anything(), "fail": _fail,
        "read_root_config": lambda s, k, d=None: config.get((s, k), d),
    })
    exec(compile(pathlib.Path(path).read_text(), str(path), "exec"), namespace)
    return namespace


def _exec_e2e(path, config, defs, plan):
    namespace = {}

    def load(label, *names, **aliases):
        module = ({"value": plan} if label.endswith("expected-e2e-plan.json")
                  else {"PINNED_ROOT_ONLY": defs["PINNED_ROOT_ONLY"]})
        for name in names:
            namespace[name] = module[name]
        for alias, name in aliases.items():
            namespace[alias] = module[name]

    namespace.update({
        "load": load, "fail": _fail, "native": _Anything(),
        "read_root_config": lambda s, k, d=None: config.get((s, k), d),
    })
    exec(compile(pathlib.Path(path).read_text(), str(path), "exec"), namespace)
    return namespace


def _canon(value) -> str:
    return json.dumps(value, sort_keys=True, default=str)


def _fbsource(defs_path, e2e_path, config, plan, rex):
    targets = {}
    defs = _exec_defs(defs_path, config, targets)
    e2e = _exec_e2e(e2e_path, config, defs, plan)
    kept = e2e["kept_cells"](plan)
    defs["hermit_e2e_cells"]({"cells": kept, "schema": plan["schema"]}, rex,
                             require_host_caps_for=e2e["required_host_caps"]())
    extras = {key: e2e[key] for key in ("KEPT_CATEGORIES", "EXCLUDED_BACKENDS")}
    extras["EXCLUDED_CATEGORIES"] = list(e2e["EXCLUDED_CATEGORIES"].items())
    extras["bundle_inputs"] = e2e["e2e_bundle_inputs"]()
    kept_ids = sorted(
        "{}/{}/{}".format(c["test"], c["mode"], c["backend"]) for c in kept)
    return targets, extras, kept_ids


def _oss(defs_path, config, plan, rex):
    targets = {}
    defs = _exec_defs(defs_path, config, targets)
    defs["hermit_e2e_cells"](plan, rex)
    return targets


def _bundle_keys(defs_path) -> list[str]:
    tree = ast.parse(pathlib.Path(defs_path).read_text())
    for node in ast.walk(tree):
        if isinstance(node, ast.FunctionDef) and node.name == "_bundle_impl":
            for inner in ast.walk(node):
                if (isinstance(inner, ast.Assign)
                        and getattr(inner.targets[0], "id", "") == "entries"
                        and isinstance(inner.value, ast.Dict)):
                    return [k.value for k in inner.value.keys
                            if isinstance(k, ast.Constant)]
    raise SystemExit(f"check_seam_identity: no entries dict in {defs_path}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-defs", required=True)
    parser.add_argument("--base-e2e", required=True)
    parser.add_argument("--head-defs", required=True)
    parser.add_argument("--head-e2e", required=True)
    parser.add_argument("--plan", required=True)
    parser.add_argument("--re-exclusions", required=True)
    args = parser.parse_args()

    plan = json.loads(pathlib.Path(args.plan).read_text())
    rex = json.loads(pathlib.Path(args.re_exclusions).read_text())
    on = ("hermit_e2e", "mini_crypto_system_utils")
    configs = [
        ("default", {}),
        ("explicit-false", {on: "false"}),
        ("routing-local", {("hermit_e2e", "routing"): "local"}),
        ("hybrid-flags", {("hermit_e2e", "pmu_on_re"): "true",
                          ("hermit_e2e", "cpuid_on_re"): "true"}),
    ]
    failures = []
    for name, config in configs:
        base = _fbsource(args.base_defs, args.base_e2e, config, plan, rex)
        head = _fbsource(args.head_defs, args.head_e2e, config, plan, rex)
        same = _canon(base[0]) == _canon(head[0]) and base[1] == head[1] \
            and base[2] == head[2]
        print(f"fbsource {name}: base {len(base[0])} targets, "
              f"head {len(head[0])}; identical={same}")
        if not same:
            failures.append(f"fbsource {name}")
        base_oss = _oss(args.base_defs, config, plan, rex)
        head_oss = _oss(args.head_defs, config, plan, rex)
        same = _canon(base_oss) == _canon(head_oss)
        print(f"oss {name}: base {len(base_oss)}, head {len(head_oss)}; "
              f"identical={same}")
        if not same:
            failures.append(f"oss {name}")

    base_keys, head_keys = _bundle_keys(args.base_defs), _bundle_keys(args.head_defs)
    added = [k for k in head_keys if k not in base_keys]
    removed = [k for k in base_keys if k not in head_keys]
    print(f"bundle entries: base {base_keys}; added={added} removed={removed}")
    if added != ["mini-crypto"] or removed:
        failures.append("bundle entries")

    try:
        targets, _extras, kept_ids = _fbsource(
            args.head_defs, args.head_e2e, {on: "true"}, plan, rex)
    except StarlarkFail as error:
        failures.append(f"opt-in load: {error}")
    else:
        system_utils = [i for i in kept_ids if i.startswith("system-utils/")]
        bad = [i for i in system_utils if i.rsplit("/", 1)[-1] != "ptrace"]
        print(f"opt-in ON: loads, {len(targets)} targets, "
              f"{len(system_utils)} system-utils cells kept, non-ptrace {bad}")
        if not system_utils or bad:
            failures.append("opt-in system-utils selection")

    if failures:
        print("FAIL: " + "; ".join(failures), file=sys.stderr)
        return 1
    print("OK: off-mode identical in every config; opt-in loads")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
