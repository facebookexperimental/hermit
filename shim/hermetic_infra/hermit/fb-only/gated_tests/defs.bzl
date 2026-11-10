# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/fb-only/gated_tests:defs.bzl, which shared
# BUCK files load as "@fbcode//hermetic_infra/hermit/fb-only/gated_tests:defs.bzl"
# (the fbcode cell is this shim here).
#
# Internally these macros run tests through a host-capability probe (CPUID
# faulting, PMU, native ABI) so that the fbsource test runner reports a case
# the host cannot run as not relevant instead of failing it. The open-source
# build has no such runner, so:
# - hermit_host_test is the plain buck_sh_test it wraps;
# - hermit_host_lit_test is lit_unittest, which this shim does not run;
# - gated_test, which re-runs an existing test target under the probe, defines
#   nothing, and run_gated_only, which hides the original target from that
#   runner, adds no label: the original test target is the test here.

load("@fbcode_macros//build_defs:lit_unittest.bzl", "lit_unittest")
load("@fbcode_macros//build_defs:native_rules.bzl", "buck_sh_test")
load("@fbsource//tools/build_defs:testpilot_defs.bzl", "tpx_labels")

CPUID_FAULTING_REASON = "host-inapplicable: Hermit's ptrace backend virtualizes CPUID through CPUID faulting, which this host lacks"

def run_gated_only():
    return []

def gated_test(name, test, env = {}, labels = [], abi_probe = None, pmu_probe = None, allow_cpuid_re = True):
    _unused = (name, test, env, labels, abi_probe, pmu_probe, allow_cpuid_re)  # @unused

def hermit_host_test(name, test, args = [], env = {}, labels = [], allow_cpuid_re = True):
    _unused = (allow_cpuid_re,)  # @unused
    buck_sh_test(
        name = name,
        test = test.removeprefix("./"),
        args = args,
        env = env,
        labels = [tpx_labels.enable_artifact_reporting] + labels,
        # hermit/detcore virtualizes the x86 cpuid instruction; x86-64 only.
        target_compatible_with = ["ovr_config//cpu:x86_64"],
    )

def hermit_host_lit_test(name, test, subs, env = {}, lit_args = ["--show-all"], allow_cpuid_re = True):
    _unused = (allow_cpuid_re,)  # @unused
    lit_unittest(
        name = name,
        test = test,
        subs = subs,
        env = env,
        lit_args = lit_args,
    )
