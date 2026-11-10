# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@shim//build_defs/lib:oss.bzl", "DEFAULT_REWRITE_CTX", "translate_macros", "translate_target")

TEST_CTX = struct(
    cells = struct(
        active = "root",
        root = "root",
        shim = "shim",
        internal = "internal",
    ),
    project_dirs = ["project"],
    stripped_root_dirs = ["root_dir"],
    prefix_mappings = [
        struct(
            match = struct(cell = "internal", root_dir = "dep"),
            replace = struct(cell = "dep", root_dir = "dep_rename"),
        ),
    ],
    implicit_rewrite_rules = {
        "internal": struct(
            exact = {
                # Inside the prefix-mapped, project and stripped-root
                # directories: an exact rule wins over each of them.
                "dep/fbonly:lib": "third_party/rust:deplib",
                "exact:exact": "foo/shimmed:exact",
                "project/fbonly:lib": "third_party/rust:projlib",
                "root_dir/fbonly/crate:crate": "third_party/rust:crate",
            },
            dirs = [
                ("third-party", "third_party"),
            ],
            dynamic = [
                ("dynamic", lambda path: path.upper()),
            ],
        ),
    },
)

# The same rules, seen from a BUCK file in the mapped `dep` cell.
DEP_CTX = struct(
    cells = struct(
        active = "dep",
        root = TEST_CTX.cells.root,
        shim = TEST_CTX.cells.shim,
        internal = TEST_CTX.cells.internal,
    ),
    project_dirs = TEST_CTX.project_dirs,
    stripped_root_dirs = TEST_CTX.stripped_root_dirs,
    prefix_mappings = TEST_CTX.prefix_mappings,
    implicit_rewrite_rules = TEST_CTX.implicit_rewrite_rules,
)

def _test_target(target: str, expected: str, ctx = TEST_CTX):
    actual = translate_target(target, ctx)

    if actual != expected:
        fail("Expected {} == {}".format(actual, expected))

def test_translate_target():
    _test_target("//:foo", "//:foo")
    _test_target("root//:foo", "root//:foo")
    _test_target("other//:foo", "other//:foo")

    _test_target("//project/foo:bar", "root//project/foo:bar")
    _test_target("internal//project/foo:bar", "root//project/foo:bar")
    _test_target("internal//project2/foo:bar", "internal//project2/foo:bar")

    _test_target("//root_dir/foo:bar", "root//foo:bar")
    _test_target("//root_dir/with/subdir/foo:bar", "root//with/subdir/foo:bar")
    _test_target("internal//root_dir/foo:bar", "root//foo:bar")

    _test_target("//dep:foo", "dep//dep_rename:foo")
    _test_target("//dep/with/subdir:foo", "dep//dep_rename/with/subdir:foo")
    _test_target("internal//dep:foo", "dep//dep_rename:foo")
    _test_target("other//dep:foo", "other//dep:foo")

    _test_target("//exact:exact", "shim//foo/shimmed:exact")
    _test_target("internal//exact:exact", "shim//foo/shimmed:exact")
    _test_target("other//exact:exact", "other//exact:exact")

    # An exact rule wins over the project, stripped-root and prefix-mapping
    # rules; the rest of each directory is unchanged.
    _test_target("//project/fbonly:lib", "shim//third_party/rust:projlib")
    _test_target("//project/fbonly:other", "root//project/fbonly:other")
    _test_target("//root_dir/fbonly/crate:crate", "shim//third_party/rust:crate")
    _test_target("internal//root_dir/fbonly/crate:crate", "shim//third_party/rust:crate")
    _test_target("//root_dir/fbonly/other:other", "root//fbonly/other:other")
    _test_target("//dep/fbonly:lib", "shim//third_party/rust:deplib")
    _test_target("//dep/fbonly:other", "dep//dep_rename/fbonly:other")
    _test_target("other//root_dir/fbonly/crate:crate", "other//root_dir/fbonly/crate:crate")

    _test_target("//third-party/lib/foo:bar", "shim//third_party/lib/foo:bar")
    _test_target("internal//third-party/lib/foo:bar", "shim//third_party/lib/foo:bar")

    _test_target("//dynamic:foo", "shim//DYNAMIC:FOO")
    _test_target("internal//dynamic:foo", "shim//DYNAMIC:FOO")
    _test_target("other//dynamic:foo", "other//dynamic:foo")

    # In the mapped cell, cell-relative labels are internal ones too; local
    # and explicitly-celled labels are unchanged.
    _test_target(":foo", ":foo", DEP_CTX)
    _test_target("//dep:foo", "dep//dep_rename:foo", DEP_CTX)
    _test_target("//dep/with/subdir:foo", "dep//dep_rename/with/subdir:foo", DEP_CTX)
    _test_target("//root_dir/foo:bar", "root//foo:bar", DEP_CTX)
    _test_target("//root_dir/fbonly/crate:crate", "shim//third_party/rust:crate", DEP_CTX)
    _test_target("//third-party/lib/foo:bar", "shim//third_party/lib/foo:bar", DEP_CTX)
    _test_target("//elsewhere:foo", "//elsewhere:foo", DEP_CTX)
    _test_target("dep//dep_rename:foo", "dep//dep_rename:foo", DEP_CTX)
    _test_target("other//dep:foo", "other//dep:foo", DEP_CTX)

def test_real_rules():
    # This repository's own rules (IMPLICIT_REWRITE_RULES and .buckconfig [oss]).
    shim = DEFAULT_REWRITE_CTX.cells.shim
    root = DEFAULT_REWRITE_CTX.cells.root
    # Spelled in two parts so that scripts/check-buck-graph-drift.rs, which
    # requires a real BUCK reference for each first-party rename, does not
    # count this test as one.
    _test_target("fbcode//hermetic_infra/hermit/fb-only/" + "dagrun:dagrun", shim + "//third-party/rust:dagrun", DEFAULT_REWRITE_CTX)
    _test_target("fbcode//hermetic_infra/hermit/fb-only/other:other", root + "//fb-only/other:other", DEFAULT_REWRITE_CTX)
    _test_target("fbcode//hermetic_infra/reverie:reverie", "reverie//:reverie", DEFAULT_REWRITE_CTX)
    _test_target("fbcode//hermetic_infra/hermit/hermit-cli:hermit", root + "//hermit-cli:hermit", DEFAULT_REWRITE_CTX)
    # Spelled in two parts so that scripts/check-buck-graph-drift.rs, which
    # requires every third-party label in a tracked file to name a Reindeer
    # target, does not read this test's input as a dependency.
    _test_target("fbsource//third-party/rust:" + "sha2-010", shim + "//third-party/rust:sha2", DEFAULT_REWRITE_CTX)
    _test_target("fbsource//third-party/rust:serde", shim + "//third-party/rust:serde", DEFAULT_REWRITE_CTX)

def _test_macros(s: str, expected: str):
    actual = translate_macros(s, TEST_CTX)

    if actual != expected:
        fail("Expected {} == {}".format(actual, expected))

def test_translate_macros():
    _test_macros("", "")
    _test_macros("plain text", "plain text")
    _test_macros("$(location //root_dir/foo:bar)", "$(location root//foo:bar)")
    _test_macros("$(exe //root_dir/foo:bar)", "$(exe root//foo:bar)")
    _test_macros("$(exe_target //root_dir/foo:bar)", "$(exe_target root//foo:bar)")
    _test_macros("$(locations //root_dir/foo:bar[sub])", "$(locations root//foo:bar[sub])")
    _test_macros("$(location :local)", "$(location :local)")
    _test_macros("--x=$(location //dep:foo) --y", "--x=$(location dep//dep_rename:foo) --y")
    _test_macros(
        "$(location //root_dir/a:b) and $(location //third-party/lib:c)",
        "$(location root//a:b) and $(location shim//third_party/lib:c)",
    )

    # Other macros, and text that only resembles one, are left alone.
    _test_macros("$(query_targets deps(//root_dir/a:b))", "$(query_targets deps(//root_dir/a:b))")
    _test_macros("$(location)", "$(location)")
    _test_macros("$(location //root_dir/a:b", "$(location //root_dir/a:b")
    _test_macros("${OUT} $(location //root_dir/a:b)", "${OUT} $(location root//a:b)")
