# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal
# fbcode//hermetic_infra/reverie/fb-only/rpc_compat:defs.bzl, which Reverie's
# BUCK loads as "@fbcode//hermetic_infra/reverie/fb-only/rpc_compat:defs.bzl"
# (the fbcode cell is this shim here). Internally the macro compiles
# reverie-rpc-transport with one module rewritten for fbsource's compiler
# settings (explicit unsafe blocks in guest_log/buffer/ordered.rs); here the
# library compiles its sources as they are.

load("@fbsource//tools/build_defs:rust_library.bzl", "rust_library")

def rpc_transport_library(name, srcs, **kwargs):
    rust_library(name = name, srcs = srcs, **kwargs)
