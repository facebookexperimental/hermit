# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/fb-only/liteinst-runtime-profile.bzl, which
# shared BUCK files load as
# "@fbcode//hermetic_infra/hermit/fb-only:liteinst-runtime-profile.bzl" (the
# fbcode cell is this shim here). Keep the exported names and the flag list in
# step with the internal file.

# The reviewed rustc flags of Cargo's liteinst-runtime profile (the root
# Cargo.toml), for the Detcore runtime's whole transitive closure: the release
# flags with debug assertions and overflow checks on. Cargo's lto = false
# passes no -Clto flag and leaves rustc's thin local LTO in effect, so none
# appears here: -Clto=off would be lto = "off", different code generation.
# stage-detcore-liteinst and ci/verify-packaged-detcore.py pin this exact
# list in the receipt each produces or reads. shim/BUCK applies the same list
# to the shim Rust toolchain under its liteinst-runtime platform.
LITEINST_RUNTIME_RUSTC_FLAGS = [
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cdebug-assertions=yes",
    "-Coverflow-checks=yes",
    "-Ccodegen-units=16",
    "-Cembed-bitcode=no",
    "-Cpanic=unwind",
    "-Cstrip=none",
]

# The liteinst-runtime rustc flags that fbcode delivers per target: every
# rust target in the closure appends
# `select({LITEINST_RUNTIME_FLAGS_CONSTRAINT: LITEINST_RUNTIME_SELECT_RUSTC_FLAGS, "DEFAULT": []})`
# to its rustc_flags. The canonical home of these two names is the
# constraints package (internal:
# fbcode//hermetic_infra/hermit/constraints:liteinst-runtime-flags.bzl;
# here: hermetic_infra/hermit/constraints/liteinst-runtime-flags.bzl in
# this cell); this stand-in re-exports them for the alias and any loader
# that still names the profile. Under the shim liteinst-runtime platform
# the toolchain already passes these values, so the target-level select
# is a no-op duplicate here. Keep the values in step with the
# constraints stand-in.
LITEINST_RUNTIME_FLAGS_CONSTRAINT = "shim//:hermit_liteinst_runtime_enabled"
LITEINST_RUNTIME_SELECT_RUSTC_FLAGS = [
    "-Coverflow-checks=yes",
    "-Ccodegen-units=16",
    "-Cdebuginfo=0",
]

def detcore_liteinst_runtime_alias(name, actual):
    # The alias pins `actual` to the OSS-only shim liteinst-runtime platform
    # through configured_alias, a raw Buck rule that fbcode build files may
    # not call. Define the alias only in the GitHub build, whose root config
    # carries [oss] (internal_cell); fbcode's root config has no such
    # section, so this definition does not apply internally, where the
    # internal file's fbcode_configured_alias defines the same name.
    if read_root_config("oss", "internal_cell", "") == "":
        return

    native.configured_alias(
        name = name,
        actual = actual,
        platform = "shim//:hermit_liteinst_runtime_platform",
        visibility = ["PUBLIC"],
    )
