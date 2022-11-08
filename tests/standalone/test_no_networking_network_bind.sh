#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

set -eu

hermit="$1"

input_program="$2"

"$hermit" --log=error run --no-networking "$input_program"
