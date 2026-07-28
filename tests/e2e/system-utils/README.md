# System utility strict-verification tests

These standalone end-to-end tests execute common system inspection utilities
through Hermit with strict mode, verification, and INFO logging. Each test also
runs a strict oracle pass so it can assert the canonical guest-visible value;
`--verify` then establishes repeatability.

Build a release binary and run the ptrace suite:

```shell
cargo build --release -p hermit
tests/e2e/system-utils/run.sh target/release/hermit ptrace
```

Run the currently supported KVM subset on a host with `/dev/kvm` access:

```shell
tests/e2e/system-utils/run.sh target/release/hermit kvm
```

Individual scripts accept the same optional `HERMIT_BIN BACKEND` arguments.
`SYSTEM_UTIL_TIMEOUT_SECONDS` overrides the 120-second per-command bound.
Missing optional host utilities are reported as skips.

## Backend allowlist

The allowlist records strict `--verify` results on the backend; it is not a
claim that different backends expose byte-identical hardware descriptions.

| Test | ptrace | KVM | KVM gap |
| --- | --- | --- | --- |
| `whoami.sh` | pass | pass | |
| `hostname.sh` | pass | pass | |
| `lscpu.sh` | pass | gap | `/proc/cpuinfo` read is denied |
| `lshw.sh` | pass | pass | |
| `numactl.sh` | pass | gap | host free-memory counter varies |
| `uname.sh` | pass | pass | |
| `id.sh` | pass | pass | |
| `groups.sh` | pass | pass | |
| `proc.sh` | pass | gap | shell procfs probe stalls |
| `du.sh` | pass | pass | |
| `df.sh` | pass | pass | |

Native observations are diagnostic. `/proc/uptime` is required to change
between two uncontained probes, directly demonstrating the host nondeterminism
that Hermit replaces with the canonical `120.00 0.00` value. Other utilities
report a native-output digest because identity, hardware topology, and
filesystem geometry are host-specific even when stable during one short run.
