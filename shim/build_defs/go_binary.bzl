# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# The open-source build configures no Go toolchain, so a Go binary defines no
# target here. Its only users are Go lit guests, and this shim's lit_unittest
# defines no test either.

def go_binary(**_kwargs):
    pass
