#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

# Print exact time with nanoseconds so it is always different in consecutive calls:
exec /usr/bin/date +'%Y-%M-%d_%R:%S_%N'
