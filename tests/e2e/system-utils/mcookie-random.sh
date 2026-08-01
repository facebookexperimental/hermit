#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# mcookie is the util-linux generator for X11 "magic cookie" authentication
# tokens: a 128-bit value printed as 32 hex digits. It draws those 128 bits from
# the kernel CSPRNG via getrandom(2) (confirmed by strace: an 8-byte GRND_NONBLOCK
# probe followed by a 128-byte draw), so its output varies run to run natively yet
# Hermit determinizes that entropy channel to a fixed value. This is a distinct
# real tool from uuidgen (a different program and purpose -- X authority cookie vs
# RFC-4122 UUID) even though both are util-linux getrandom consumers, and distinct
# from random-device (which reads the /dev/urandom char device rather than calling
# getrandom). mcookie takes no arguments and is a single fast process with no file.
set -euo pipefail

case ${1:-} in
    --prepare) command -v mcookie >/dev/null ;;
    --run) exec mcookie ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
