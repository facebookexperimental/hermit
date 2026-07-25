# KVM M5 results: 2026-07-24

## Environment

- Hermit commit: `d4b8607c4315b8a6ca27d683e33b85dde64f17f9`
- Hermit binary: release profile, SHA-256
  `9868f4555530454836c6b4fe7732cb1d0f621df2f55b951a6336ec87db71e823`
- CPU: AMD EPYC 9D85 158-Core Processor, logical CPU 8
- CPU governor: `performance`
- Kernel: `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79`
- Samples: 15 after 3 warmups; backend order alternated per sample
- Host load average at completion: 324.03, 309.69, 318.40
- Common mode: `--strict --base-env=minimal --tmp=/tmp --log=off`

This was a heavily loaded shared host. Medians are the primary comparison;
p95 and raw samples are retained to expose scheduler noise rather than hide it.

## Headline metrics

| Metric | ptrace | KVM | KVM advantage |
| --- | ---: | ---: | ---: |
| Incremental syscall interception | 562.7 us/call | 10.1 us/call | 55.88x |
| Incremental pipe supervisor boundary | 576.2 us/boundary | 25.3 us/boundary | 22.77x |
| Host context switches per syscall | 6.0144 | 0.0001 | 60,144x fewer |
| Host context switches per pipe boundary | 6.0185 | 0.0005 | 12,037x fewer |
| End-to-end `cat` throughput | 73.6 MiB/s | 136.3 MiB/s | 1.85x |

The context-switch ratios are effectively a categorical result: the ptrace
tracer and tracee require scheduler handoffs, while KVM exits return to the
same host thread. KVM's nonzero residual is below one switch per thousands of
operations and is dominated by setup and measurement resolution.

## Application latency

| Program | ptrace median | KVM median | ptrace p95 | KVM p95 | KVM speedup |
| --- | ---: | ---: | ---: | ---: | ---: |
| `/bin/echo hello` | 67.104 ms | 117.011 ms | 170.811 ms | 167.328 ms | 0.57x |
| `/bin/ls -1` (64 entries) | 117.179 ms | 116.522 ms | 216.874 ms | 317.381 ms | 1.01x |
| `/bin/cat` (16 MiB) | 217.344 ms | 117.379 ms | 467.583 ms | 266.632 ms | 1.85x |
| `/bin/true` | 66.988 ms | 116.740 ms | 116.847 ms | 266.608 ms | 0.57x |
| `/usr/bin/pwd` | 66.811 ms | 116.913 ms | 116.877 ms | 167.260 ms | 0.57x |
| `/usr/bin/seq 10` | 66.793 ms | 116.928 ms | 117.958 ms | 417.164 ms | 0.57x |
| `/usr/bin/head` | 66.673 ms | 116.855 ms | 117.031 ms | 216.566 ms | 0.57x |
| `/usr/bin/base64` | 66.763 ms | 116.449 ms | 317.703 ms | 267.633 ms | 0.57x |
| `/usr/bin/id -u` | 116.920 ms | 116.688 ms | 217.096 ms | 266.970 ms | 1.00x |
| `/usr/bin/printf` | 66.797 ms | 116.444 ms | 116.865 ms | 269.850 ms | 0.57x |

Tiny programs show roughly 50 ms of additional KVM setup cost on the median.
The crossover appears once syscall or byte volume is high enough: `ls` reaches
parity, 16 MiB `cat` is 1.85x faster, 1,000 pipe round trips are 7.29x faster
end to end, and the 10,000-syscall loop is 26.14x faster end to end.

## Conclusions

1. **The KVM interception path is substantially cheaper once amortized.**
   Startup-subtracted syscall and pipe-boundary costs improve by 55.88x and
   22.77x respectively.
2. **KVM avoids ptrace scheduler handoffs.** Ptrace measured about six host
   context switches per boundary; KVM remained at measurement-floor levels.
3. **KVM does not win every program.** VM construction and ELF setup dominate
   short commands such as `echo`, `true`, and `seq`, making their KVM medians
   about 1.75x slower.
4. **Throughput crosses over.** The 16 MiB `cat` workload reached 136.3 MiB/s
   on KVM versus 73.6 MiB/s through ptrace.
5. **This is supported-corpus parity, not full Linux parity.** Both sides used
   strict mode and byte-identical preflight output, but KVM still lacks general
   thread/process lifecycle and deterministic preemption support.

Machine-readable metadata, all 420 raw observations, summaries, and derived
metrics are checked in under [`results/2026-07-24`](results/2026-07-24/).
