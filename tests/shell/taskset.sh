#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

# Report for the bash process, not the taskset child process:
taskset -c -p $$
# The root process in the container is always pid 3 under hermit currently.
# In this case the bash process will be pid 3.
