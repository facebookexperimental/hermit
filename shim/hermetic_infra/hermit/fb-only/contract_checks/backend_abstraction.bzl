# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/fb-only/contract_checks:backend_abstraction.bzl,
# which the root BUCK loads as
# "@fbcode//hermetic_infra/hermit/fb-only/contract_checks:backend_abstraction.bzl"
# (the fbcode cell is this shim here). It carries the same public workspace
# member list as the internal file, with labels in this tree's layout: the
# root BUCK's backend-abstraction trees that read it are marked broken-in-oss,
# but the list itself is the Cargo workspace (root Cargo.toml [workspace]
# members) and must not be narrowed here. The open-source build runs the
# check itself as scripts/check-detcore-backend-abstraction.sh.

WORKSPACE_MEMBERS = [
    "ci/manifest-plan",
    "common/test-allocator",
    "common/test-workdir",
    "detcore",
    "detcore-liteinst",
    "detcore-model",
    "detcore/tests",
    "detcore/tests/lit-guests",
    "detcore/tests/testutils",
    "flaky-tests",
    "hermit-cdylib",
    "hermit-cli",
    "hermit-install",
    "hermit-record",
    "hermit-record-format",
    "hermit-replay",
    "hermit-resources",
    "hermit-run-kvm",
    "hermit-run-ptrace",
    "hermit-verify",
    "inguest-engine",
    "tests",
]

# The Buck package that exports a member's Cargo.toml, where that is not the
# member's own directory (the same table as the internal file).
_MANIFEST_OWNERS = {
    "detcore-liteinst": "",
    "detcore/tests": "detcore",
    "detcore/tests/lit-guests": "detcore",
    "detcore/tests/testutils": "detcore",
    "hermit-cdylib": "",
    "hermit-install": "",
    "inguest-engine": "",
}

def _member_manifest(member):
    owner = _MANIFEST_OWNERS.get(member, member)
    if not owner:
        return member + "/Cargo.toml"
    return "//{}:{}".format(owner, (member + "/Cargo.toml").removeprefix(owner + "/"))

MEMBER_MANIFESTS = {member: _member_manifest(member) for member in WORKSPACE_MEMBERS}
