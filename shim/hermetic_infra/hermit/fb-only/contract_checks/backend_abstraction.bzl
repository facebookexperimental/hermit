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
# (the fbcode cell is this shim here). Internally WORKSPACE_MEMBERS feeds the
# tree that check.backend_abstraction runs on; the open-source build runs that
# check as scripts/check-detcore-backend-abstraction.sh instead, so the tree
# has no members here.

WORKSPACE_MEMBERS = []
