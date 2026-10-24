#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Probe whether user-space retired-branch hardware performance counters are
# actually usable on this host. Hermit's chaos mode and --max-timeslice
# depend on these counters.
#
# We probe by *opening* the retired-branch counter with a minimal
# `perf stat -e branches:u` command, rather than by matching presentation text
# from `perf list`. The `perf list` text is not a reliable capability signal:
# on current x86_64 hosts the retired-branch counter works, yet `perf list
# hardware` labels the section "legacy hardware" and never prints the phrase
# "Hardware event", so a `grep -i "Hardware event"` probe reports a false
# negative and silently skips chaos coverage (GH #21).
#
# Exit status:
#   0  - user-space retired-branch counters are available and usable
#   1  - counters are unavailable (perf missing, event unsupported, or the
#         kernel refused to open the counter)
#
# All diagnostics go to stderr; nothing is printed to stdout. The `perf` binary
# can be overridden with the PERF environment variable (used by tests).

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
