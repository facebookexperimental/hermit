# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/fb-only:e2e_cells.bzl, which ci/buck-e2e/BUCK
# loads as "@fbcode//hermetic_infra/hermit/fb-only:e2e_cells.bzl" (the fbcode
# cell is this shim here). Internally that file keeps the c-programs cells on
# the ptrace and kvm backends and builds every bundle input with Buck. This
# build defines every cell of the plan, and its bundle takes the inputs
# ci/buck-e2e/stage stages from a cargo build. Keep the exported names in step
# with the internal file.

# The root cell is named hermit; inside this cell `root` names the shim itself.
load("@hermit//ci:expected-e2e-plan.json", _PLAN = "value")

# The plan ci/buck-e2e/BUCK defines its cells from.
E2E_PLAN = _PLAN

# Every category: the Buck-built :fixtures passes no --category.
KEPT_CATEGORIES = []

# No backend is excluded.
EXCLUDED_BACKENDS = {}

def kept_cells(plan):
    """Every cell of the plan."""
    return plan["cells"]

# Which hermit the cells run (-c hermit_e2e.hermit=...):
#   release  //hermit-cli:hermit-release, the feature-complete release build
#            (default; ci/buck-e2e/run builds it with -c hermit_release.unstamped=true,
#            so it carries no commit hash and stays cached across commits that
#            change no compiled source)
#   debug    //hermit-cli:hermit (LiteInst-enabled shared app; no DBT/SaBRe/e9patch)
#   staged   the staged cargo-built hermit, e.g. to compare against a cargo build
_HERMIT = {
    "debug": "//hermit-cli:hermit",
    "release": "//hermit-cli:hermit-release",
    "staged": ":staged-hermit",
}

def e2e_bundle_inputs():
    """The inputs of ci/buck-e2e's :bundle, as export_file targets of what
    ci/buck-e2e/stage staged into staged/ (gitignored) from a cargo-built
    harness bundle: test-harness binaries, `test-harness build` fixtures, the
    source snapshot cells read, compat run-state and the backend install
    bundle.

    ./stage writes staged/SOURCE_SHA last in both modes, so its presence at
    parse time means a complete staging. Without one (a fresh checkout), the
    exports point at a committed placeholder so that `buck2 targets hermit//...`
    parses; building :bundle in that state produces no usable bundle, and
    validate-node --cells-only still refuses to run unstaged."""
    staged = bool(native.glob(["staged/SOURCE_SHA"]))
    for part in ["bin", "build", "hermit", "install", "run-state", "src", "SOURCE_SHA"]:
        native.export_file(
            name = "staged-" + part,
            src = ("staged/" + part) if staged else "unstaged-placeholder.txt",
        )
    return {
        "fixtures": ":staged-build",
        "harness_bin": ":staged-bin",
        "hermit": _HERMIT[read_root_config("hermit_e2e", "hermit", "release")],
        "install": ":staged-install",
        "run_state": ":staged-run-state",
        "source_sha": ":staged-SOURCE_SHA",
        "src": ":staged-src",
    }
