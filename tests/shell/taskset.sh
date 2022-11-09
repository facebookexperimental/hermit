#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Report for the bash process, not the taskset child process:
taskset -c -p $$
# The root process in the container is always pid 3 under hermit currently.
# In this case the bash process will be pid 3.
