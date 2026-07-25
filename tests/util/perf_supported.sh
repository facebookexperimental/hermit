#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -uo pipefail

PERF="${PERF:-perf}"

# `perf stat` reports its measurement on stderr; capture both streams. On a
# capable host this exits 0 and prints a numeric "branches:u" count. On a host
# without usable counters it either exits non-zero (event not supported, perf
# missing) or, in restricted containers/VMs, exits 0 while printing
# "<not supported>" or "<not counted>" for the event -- so exit status alone is
# not sufficient.
if ! output=$("$PERF" stat -e branches:u -- /bin/true 2>&1); then
  >&2 echo "perf_supported: '$PERF stat -e branches:u' failed; assuming no usable PMU."
  >&2 echo "$output"
  exit 1
fi

if printf '%s\n' "$output" | grep -qiE '<not supported>|<not counted>'; then
  >&2 echo "perf_supported: retired-branch counter could not be opened (perf reported it as not supported/counted)."
  >&2 echo "$output"
  exit 1
fi

# Require an actual retired-branch measurement in the report. This guards
# against unexpected perf output formats that would otherwise be treated as
# success.
if ! printf '%s\n' "$output" | grep -qiE '\bbranches:u\b|branch-instructions'; then
  >&2 echo "perf_supported: could not find a retired-branch measurement in perf output."
  >&2 echo "$output"
  exit 1
fi

exit 0
