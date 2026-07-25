This directory contains helpers and standalone diagnostics used when developing
Hermit.

## PMU RCB skid benchmark

`pmu_skid.c` measures retired-conditional-branch (RCB) overflow delivery skid
using the same raw Intel and AMD events as Hermit. It uses a sampling counter
to trigger each signal and a separate counting counter to measure the offset,
because sampling counters can drop increments around an overflow.

Build and run it on x86_64 Linux:

```sh
cc -O2 -Wall -Wextra -Werror -std=gnu11 \
  tests/util/pmu_skid.c -o /tmp/pmu-skid-test
/tmp/pmu-skid-test --iterations 1000
```

The process pins itself and its tracee to one CPU. Use `--cpu` to select a
different online CPU and `--period` to change the default 1,000,000-RCB sample
period. Raw PMU access must be permitted by the host kernel.

The reported margin is twice the largest observed skid, with a minimum of 100
RCBs. It is an empirical starting point rather than a hardware guarantee; run
the tool repeatedly under representative host load before changing a margin.
