load("@bazel_skylib//lib:paths.bzl", "paths")
load("@fbcode_macros//build_defs:cpp_binary.bzl", "cpp_binary")
load("@fbcode_macros//build_defs:go_binary.bzl", "go_binary")
load("@fbcode_macros//build_defs:lit_unittest.bzl", "lit_unittest")
load("@fbsource//tools/build_defs:glob_defs.bzl", "subdir_glob")
load("@fbsource//tools/build_defs:rust_binary.bzl", "rust_binary")

_X86_64_ONLY = ["ovr_config//cpu:x86_64"]

# Match the frozen OSS lit harness and canonical_test_epoch.rs.
LIT_TEST_ENV = {"HERMIT_EPOCH": "2026-01-01T00:00:00Z"}

def rust_lit_unittest(name, src, edition = None, deps = None):
    bin_name = name + "_main"

    rust_binary(
        name = bin_name,
        srcs = [src],
        allocator = "malloc",
        crate_root = src,
        edition = edition,
        deps = deps,
        rustc_flags = [
            "-Aunused-crate-dependencies",
        ],
        target_compatible_with = _X86_64_ONLY,
        unittests = False,
    )

    # Primary lit test.
    lit_unittest(
        name = name,
        # these tests run guests under `hermit run`; hermit is x86-64 only.
        target_compatible_with = _X86_64_ONLY,
        env = LIT_TEST_ENV,
        lit_args = ["--show-all"],
        subs = [
            ("hermit", "//hermetic_infra/hermit/hermit-cli:hermit"),
            (
                "me",
                ":{}".format(bin_name),
            ),
        ],
        test = src,
    )

    # Find additional lit tests in the same source directory.
    tests = subdir_glob([(paths.dirname(src), "*.lit")])
    for relpath, fullpath in tests.items():
        lit_unittest(
            name = "{}-{}".format(name, paths.replace_extension(relpath, "")),
            # these tests run guests under `hermit run`; hermit is x86-64 only.
            target_compatible_with = _X86_64_ONLY,
            env = LIT_TEST_ENV,
            lit_args = ["--show-all"],
            subs = [
                (
                    "hermit",
                    "//hermetic_infra/hermit/fb-only/lit:hermit-guest-tmp"
                    if name in ["utime", "utimes"] and relpath in ["hermit-run.lit", "hermit-run-strict.lit"]
                    else "//hermetic_infra/hermit/hermit-cli:hermit",
                ),
                (
                    "me",
                    ":{}".format(bin_name),
                ),
            ],
            test = fullpath,
        )

def cpp_lit_unittest(name, src, compiler_flags = None, preprocessor_flags = None, deps = None):
    bin_name = name + "_main"

    cpp_binary(
        name = bin_name,
        srcs = [src],
        allocator = "malloc",
        compiler_flags = compiler_flags,
        preprocessor_flags = preprocessor_flags,
        target_compatible_with = _X86_64_ONLY,
        deps = deps,
    )

    # Primary lit test.
    lit_unittest(
        name = name,
        # these tests run guests under `hermit run`; hermit is x86-64 only.
        target_compatible_with = _X86_64_ONLY,
        env = LIT_TEST_ENV,
        lit_args = ["--show-all"],
        subs = [
            ("hermit", "//hermetic_infra/hermit/hermit-cli:hermit"),
            (
                "me",
                ":{}".format(bin_name),
            ),
        ],
        test = src,
    )

    # Find additional lit tests in the same source directory.
    tests = subdir_glob([(paths.dirname(src), "*.lit")])
    for relpath, fullpath in tests.items():
        lit_unittest(
            name = "{}-{}".format(name, paths.replace_extension(relpath, "")),
            # these tests run guests under `hermit run`; hermit is x86-64 only.
            target_compatible_with = _X86_64_ONLY,
            env = LIT_TEST_ENV,
            lit_args = ["--show-all"],
            subs = [
                ("hermit", "//hermetic_infra/hermit/hermit-cli:hermit"),
                (
                    "me",
                    ":{}".format(bin_name),
                ),
            ],
            test = fullpath,
        )

def go_lit_unittest(name, src, deps = None):
    bin_name = name + "_main"

    go_binary(
        name = bin_name,
        srcs = [src],
        target_compatible_with = _X86_64_ONLY,
        deps = deps,
    )

    # Primary lit test.
    lit_unittest(
        name = name,
        # these tests run guests under `hermit run`; hermit is x86-64 only.
        target_compatible_with = _X86_64_ONLY,
        env = LIT_TEST_ENV,
        lit_args = ["--show-all"],
        subs = [
            ("hermit", "//hermetic_infra/hermit/hermit-cli:hermit"),
            (
                "me",
                ":{}".format(bin_name),
            ),
        ],
        test = src,
    )

    # Find additional lit tests in the same source directory.
    tests = subdir_glob([(paths.dirname(src), "*.lit")])
    for relpath, fullpath in tests.items():
        lit_unittest(
            name = "{}-{}".format(name, paths.replace_extension(relpath, "")),
            # these tests run guests under `hermit run`; hermit is x86-64 only.
            target_compatible_with = _X86_64_ONLY,
            env = LIT_TEST_ENV,
            lit_args = ["--show-all"],
            subs = [
                ("hermit", "//hermetic_infra/hermit/hermit-cli:hermit"),
                (
                    "me",
                    ":{}".format(bin_name),
                ),
            ],
            test = fullpath,
        )
