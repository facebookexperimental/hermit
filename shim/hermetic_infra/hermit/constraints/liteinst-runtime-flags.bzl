# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/constraints:liteinst-runtime-flags.bzl,
# which shared BUCK files load as
# "@fbcode//hermetic_infra/hermit/constraints:liteinst-runtime-flags.bzl"
# (the fbcode cell is this shim here). It is the canonical home of the
# two liteinst-runtime select names; the fb-only profile stand-in
# re-exports them. Keep the flag list in step with the internal file.
#
# The internal constraint is
# fbcode//hermetic_infra/hermit/constraints:liteinst-runtime-flags[applied];
# here the select keys on the shim's liteinst-runtime configuration
# (shim//:hermit_liteinst_runtime_enabled, the config_setting for the
# hermit_liteinst_runtime constraint value the shim liteinst-runtime
# platform carries). The shim Rust toolchain already passes the full
# liteinst-runtime profile under that platform, so this target-level
# select duplicates the three values below there; outside the platform
# it defaults to no added flags, as builds behave today. It exists so
# the shared BUCK files resolve in both cells.
LITEINST_RUNTIME_FLAGS_CONSTRAINT = "shim//:hermit_liteinst_runtime_enabled"
LITEINST_RUNTIME_SELECT_RUSTC_FLAGS = [
    "-Coverflow-checks=yes",
    "-Ccodegen-units=16",
    "-Cdebuginfo=0",
]
