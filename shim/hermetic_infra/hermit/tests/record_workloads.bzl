# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/tests:record_workloads.bzl, which shared BUCK
# files load as "@fbcode//hermetic_infra/hermit/tests:record_workloads.bzl"
# (the fbcode cell is this shim here).
#
# Internally, record_c_workloads compiles record_replay's prepared C workloads
# as Buck targets for the fbsource test runner. The open-source build runs
# record_replay through Cargo, which compiles the same workloads itself
# (ci/record-replay-workloads.rs), so here it defines no targets.

def record_c_workloads(_package):
    pass

def record_c_workload_target(_alias):
    fail("record_c_workload_target: the open-source build defines no record workload targets")
