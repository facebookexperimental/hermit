#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# THIS FILE IS A SHIM, NOT AN IMPLEMENTATION. The validation driver is
# scripts/validate.rs; there is no second version, so the two cannot drift.
#
# The shim exists only so that `validate.sh` stays a valid historical entrypoint
# across the refactor boundary. Production callers (Make, CI workflows, the DAG,
# and ci-hub) invoke scripts/validate.rs directly.
# The Rust CLI accepts validate.sh's entire former flag surface (verified flag by
# flag), so forwarding "$@" untouched is a pure pass-through.
#
# `exec` is load-bearing: the driver must BE this process, so its pid is the one a
# caller signals, waits on, and finds in the re-entrancy marker's ancestry.
exec "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/scripts/validate.rs" "$@"
