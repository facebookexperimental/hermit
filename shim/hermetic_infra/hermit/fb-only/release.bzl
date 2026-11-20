# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/hermit/fb-only/release.bzl, which shared BUCK files
# load as "@fbcode//hermetic_infra/hermit/fb-only:release.bzl" (the fbcode
# cell is this shim here).
#
# The release values this tree's Buck build reports. hermit-cli/BUCK, which
# both trees share, takes them from here, so that a release or a Reverie pin
# bump in one tree edits only that tree's own file:
# - HERMIT_VERSION: hermit-cli's package version (hermit-cli/Cargo.toml), which
#   `hermit --version` reports.
# - HERMIT_REVERIE_PIN: the Reverie revision this tree builds. It equals the
#   reverie gitlink and every Reverie revision in the Cargo manifests.
#
# scripts/check-reverie-pin.rs checks both. A release edits HERMIT_VERSION
# here; a Reverie pin bump (docs/updating-reverie.md) carries
# HERMIT_REVERIE_PIN with every other derived pin site. Each value stays one
# line, `NAME = "value"`.

HERMIT_VERSION = "0.5.7"
HERMIT_REVERIE_PIN = "c0f1a9e13b58dc4415ac7831087294f351769870"
