#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Performs substantial work in parallel.  Test of slowdown do to loss
# of parallelism under instrumentation/recording.

# Takes roughly 0.47s natively for 1 work unit, 0.6s under rr.

# For 10 tasks / 200K loop tripcount:
# Shows perfect parallelism, 0.48s realtime 4.6s user, neglible system.
# - Under rr, perfect non-parallelism: 5.6s real, 5.1 user, 0.6 sys.
#   - rr chaos 7.564s real, 6.7 user, 0.7 sys.
# - hermit run: 0.86 real, 5.7 user, 2.2 sys.
#  - hermit strict: 5.5s real, 5.2 user, 0.46 sys.
# - strace -cf: 0.53s real, 4.7s user, 0.12 sys.
#               Only ~6052 syscalls.
# - taskset 0x1: 4.9s real, 4.8s user, 42ms sys.

if [[ "$HERMIT_MODE" = "chaosreplay" ]] ||
   [[ "$HERMIT_MODE" = "tracereplay" ]];
then
    # TODO(T100400409): Reenable after performance improvements
    echo "Skipping par_work in unsupported mode. Re-enable when it is fixed." >&2
    exit 0
fi

PARALLELISM=10

function work() {
    name=$1
    echo "Start $name"
    python3 <<EOF
a=1
b=1
for x in range(0, 10000):
    tmp=a
    a+=b
    b=tmp
print("Finished $name", hash(a))
EOF
}

for((t=0; t<PARALLELISM; t++)); do
   work "task_$t" &
done
wait
