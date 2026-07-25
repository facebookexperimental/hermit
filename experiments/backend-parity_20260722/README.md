# Hermit backend parity matrix

This directory tracks executable parity contracts across Hermit's ptrace,
DynamoRIO (DBI), and KVM backends. `matrix.tsv` is the ratchet: changing a pair
from `gap` to `pass` makes `run_matrix.py` enforce it on every subsequent run.
A `gap` must have a concrete implementation reason.

## Current ratchet

| Backend | Passing pairs | Parity vs ptrace |
| --- | ---: | ---: |
| ptrace | 10/10 | 100% |
| DBI | 7/10 | 70% |
| KVM | 1/10 | 10% |

The task's pre-existing DBI-native baseline is 70/89 tests (78.7%). That number
measures the backend's own Reverie suite. The 7/10 number above is deliberately
separate: it measures the cross-backend Hermit contracts in this directory.
Conflating the two would overstate Detcore parity because the current DBI client
observes most syscalls but only rewrites `write` and CPUID; it does not yet use
Detcore's scheduler, virtual clock, PID model, or random model.

KVM's single passing pair is the built-in hello/write VM-exit path. The current
KVM prototype does not load the requested Linux ELF, so treating `/bin/true`
returning zero as a pass would be a false positive. Its CPUID policy is covered
inside `reverie-kvm`, but it cannot yet execute this suite's CPUID probe ELF.

## Matrix

| Test | ptrace | DBI | KVM |
| --- | --- | --- | --- |
| `hello_stdout` | pass | pass | pass |
| `argument_forwarding` | pass | pass | gap |
| `exit_zero` | pass | pass | gap |
| `exit_status` | pass | pass | gap |
| `file_read` | pass | pass | gap |
| `pthread_lifecycle` | pass | pass | gap |
| `cpuid_policy` | pass | pass | gap |
| `virtual_clock` | pass | gap | gap |
| `random_sources` | pass | gap | gap |
| `virtual_pid` | pass | gap | gap |

The authoritative reasons live in `matrix.tsv`, next to the status they
justify. The runner executes each passing pair three times and checks exit
status, stdout, and (for determinism cases) byte-identical repeated output.

## Running

Validate the checked-in matrix without backend prerequisites:

```bash
python3 experiments/backend-parity_20260722/run_matrix.py --check
```

Build Hermit, then enforce the ptrace baseline:

```bash
cargo build -p hermit
python3 experiments/backend-parity_20260722/run_matrix.py --backend ptrace
```

Run DBI with a source-built DynamoRIO and the Reverie client:

```bash
export DYNAMORIO_HOME=$HOME/dynamorio/install
export HERMIT_DRRUN=$DYNAMORIO_HOME/bin64/drrun
export HERMIT_DBI_CLIENT=$HOME/work/dev-reverie/reverie/target/reverie-dbi-native/libreverie_dbi_client.so
python3 experiments/backend-parity_20260722/run_matrix.py --backend dbi --require-backend
```

Run KVM on a host with read-write `/dev/kvm` access:

```bash
python3 experiments/backend-parity_20260722/run_matrix.py --backend kvm --require-backend
```

Use `--probe-gaps` to execute documented gaps and report `XPASS` candidates.
Use `--output /tmp/backend-parity.tsv` to retain machine-readable observations.
`BLOCKED` means a required host capability or runtime artifact was absent; it
does not change the checked-in pass/gap claim.
