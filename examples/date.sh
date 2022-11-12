#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Print exact time with nanoseconds so it is always different in consecutive calls:
exec /usr/bin/date +'%Y-%M-%d_%R:%S_%N'
