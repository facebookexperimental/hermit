# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This build configuration is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

load("@bazel_skylib//lib:paths.bzl", "paths")
load("@fbcode_macros//build_defs:cpp_binary.bzl", "cpp_binary")
load("@fbcode_macros//build_defs:export_files.bzl", "export_file")
load("@fbcode_macros//build_defs:native_rules.bzl", "buck_sh_test")
load("@fbcode_macros//build_defs:python_binary.bzl", "python_binary")
load("@fbcode_macros//build_defs:rust_binary.bzl", "rust_binary")

def build_test(name, bin_target, raw, run, no_sequentialize_threads, no_deterministic_io, record_and_replay, chaos, chaosreplay, tracereplay):
    # Used only by shell tests.
    common_env = {
        "HERMIT_BIN": "$(location //hermetic_infra/hermit/hermit-cli:hermit)",
        "PYTHON_SIMPLEST_SERVER": "$(location //hermetic_infra/hermit/tests:simplest_server_py)",
    }

    if raw:
        # # Also run tests without any sort of syscall interception. "raw" mode:
        buck_sh_test(
            name = "raw_run__" + name,
            args = [
                "--no-sequentialize-threads",
                "--no-deterministic-io",
            ],
            env = common_env,
            test = bin_target,
        )

    if run:
        # Run tests in hermit run mode, default settings:
        # TODO: add determinism-assertion by adding a hermit flag for running twice.
        buck_sh_test(
            name = "hermit_run_default__" + name,
            args = [
                "run",
                "--no-sequentialize-threads",
                "--no-deterministic-io",
                "--env=HERMIT_MODE=default",
                "$(location " + bin_target + ")",
            ],
            env = common_env,
            test = "//hermetic_infra/hermit/hermit-cli:hermit",
        )

    # if strict:
    # Run tests in hermit run mode, strict settings:
    if not (no_deterministic_io or no_sequentialize_threads):
        buck_sh_test(
            name = "hermit_run_strict__" + name,
            # Warning: hacky tuning! The preemption-timeout here is
            # arbitrary, and is basically selected to be the largest
            # number that we can tolerate while still observing some
            # context switches in the mem_race test.  It needs to be
            # large because the performance of deterministic
            # preemptions is currently quite slow, and frequent
            # preemptions will slow down *all* tests.
            args = [
                "--hermit-bin=$(location //hermetic_infra/hermit/hermit-cli:hermit)",
                "run",
                "--isolate-workdir",
                "--hermit-arg=--base-env=empty",
                "--hermit-arg=--env=HERMIT_MODE=strict",
                "--hermit-arg=--preemption-timeout=80000000",
                "--",
                "$(location " + bin_target + ")",
            ],
            env = {},
            test = "//hermetic_infra/hermit/hermit-verify:hermit-verify",
        )

    if tracereplay:
        # Also run with replay of recorded preemptions.
        buck_sh_test(
            name = "hermit_run_tracereplay__" + name,
            args = [
                "--hermit-bin=$(location //hermetic_infra/hermit/hermit-cli:hermit)",
                "trace-replay",
                "--isolate-workdir",
                "--hermit-arg=--base-env=empty",
                "--hermit-arg=--env=HERMIT_MODE=tracereplay",
                "--",
                "$(location " + bin_target + ")",
            ],
            env = common_env,
            test = "//hermetic_infra/hermit/hermit-verify:hermit-verify",
        )

    if chaos:
        # Run tests in hermit run mode, strict settings, chaotic scheduling:
        buck_sh_test(
            name = "hermit_run_chaos__" + name,
            # Warning: hacky tuning! The preemption-timeout here is
            # arbitrary, and is basically selected to be the smallest
            # number that we can tolerate for chaos tests. Smaller numbers
            # create more priority change points, which are deterministic
            # preemptions and thus quite slow. Frequent change points thus
            # slow down *all* tests. The slowest/deadlocking tests have specific
            # disables for chaos mode.
            args = [
                "--hermit-bin=$(location //hermetic_infra/hermit/hermit-cli:hermit)",
                "run",
                "--isolate-workdir",
                "--hermit-arg=--base-env=empty",
                "--hermit-arg=--env=HERMIT_MODE=chaos",
                "--hermit-arg=--chaos",
                "--hermit-arg=--preemption-timeout=1000000",
                "--",
                "$(location " + bin_target + ")",
            ],
            env = {},
            test = "//hermetic_infra/hermit/hermit-verify:hermit-verify",
        )
    if chaosreplay:
        # Also run with replay of recorded preemptions.
        buck_sh_test(
            name = "hermit_run_chaosreplay__" + name,
            args = [
                "--hermit-bin=$(location //hermetic_infra/hermit/hermit-cli:hermit)",
                "chaos-replay",
                "--isolate-workdir",
                "--hermit-args=--base-env=empty",
                "--hermit-args=--env=HERMIT_MODE=chaosreplay",
                "--hermit-args=--chaos",
                "--",
                "$(location " + bin_target + ")",
            ],
            env = common_env,
            test = "//hermetic_infra/hermit/hermit-verify:hermit-verify",
        )

    if record_and_replay:
        buck_sh_test(
            name = "hermit_record_" + name,
            args = [
                "record",
                "--verify",
                "--",
                "$(location " + bin_target + ")",
            ],
            env = dict(
                common_env,
                HERMIT_MODE = "record",
            ),
            test = "//hermetic_infra/hermit/hermit-cli:hermit",
        )

def hermit_shell_test(path, raw, run, no_sequentialize_threads, no_deterministic_io, chaos, record_and_replay, chaosreplay, tracereplay = False):
    basename = paths.replace_extension(paths.basename(path), "")
    export_file(name = "shellfile_" + basename, src = path)
    build_test("sh_" + basename, ":shellfile_" + basename, raw, run, no_sequentialize_threads, no_deterministic_io, record_and_replay, chaos, chaosreplay, tracereplay)

def hermit_python_test(path, module_base, raw, run, no_sequentialize_threads, no_deterministic_io, chaos, record_and_replay, chaosreplay = False, tracereplay = False):
    basename = paths.replace_extension(paths.basename(path), "")
    bin_name = "pythonbin_" + basename
    bin_target = ":" + bin_name
    python_binary(
        name = bin_name,
        srcs = [path],
        main_module = module_base + "." + basename,
        deps = [],
        # NOTE: We use par_style = "live" here because it appears as if `/tmp`
        # is owned by UID 65534 due to our usage of a user namespace. Other par
        # styles don't like that and will bail. Since this target is only used
        # for testing purposes, we don't need a self-contained par file.
        par_style = "live",
    )
    build_test("py_" + basename, bin_target, raw, run, no_sequentialize_threads, no_deterministic_io, record_and_replay, chaos, chaosreplay, tracereplay)

def hermit_c_test(path, raw, run, no_sequentialize_threads, no_deterministic_io, chaos, record_and_replay, chaosreplay, tracereplay = False):
    basename = paths.replace_extension(paths.basename(path), "")
    bin_name = "cbin_" + basename
    bin_target = ":" + bin_name
    cpp_binary(
        name = bin_name,
        srcs = [path],
        headers = ["c/util/assert.h"],
        deps = [],
    )
    build_test("c_" + basename, bin_target, raw, run, no_sequentialize_threads, no_deterministic_io, record_and_replay, chaos, chaosreplay, tracereplay)

# Similar to C/Rust tests but with a prebuilt custom binary.
def hermit_bin_test(bin_target, raw, run, no_sequentialize_threads, no_deterministic_io, chaos, record_and_replay, chaosreplay, tracereplay = False):
    # Accept a fairly limited syntax of targets only:
    basename = bin_target.split(":")[-1]
    build_test("custombin_" + basename, bin_target, raw, run, no_sequentialize_threads, no_deterministic_io, record_and_replay, chaos, chaosreplay, tracereplay)

def hermit_rust_test(path, raw, run, no_sequentialize_threads, no_deterministic_io, chaos, record_and_replay, chaosreplay, tracereplay = False):
    basename = paths.replace_extension(paths.basename(path), "")
    bin_name = "rustbin_" + basename
    bin_target = ":" + bin_name
    rust_binary(
        name = bin_name,
        srcs = [path, paths.dirname(path) + "/test_utils/mod.rs"],
        crate_root = path,
        deps = [
            "fbsource//third-party/rust:libc",
            "fbsource//third-party/rust:nix",
            "fbsource//third-party/rust:tempfile",
        ],
    )
    build_test("rs_" + basename, bin_target, raw, run, no_sequentialize_threads, no_deterministic_io, record_and_replay, chaos, chaosreplay, tracereplay)

def hermit_chaos_stress_test(name, bin_target, preempt_interval, max_iterations):
    buck_sh_test(
        name = "hermit_chaos_fail_" + name,
        args = [
            "$(location //hermetic_infra/hermit/hermit-cli:hermit)",
            "$(location " + bin_target + ")",
            str(preempt_interval),
            str(max_iterations),
        ],
        test = "//hermetic_infra/hermit/tests:chaos_stress_wrapper",
    )
