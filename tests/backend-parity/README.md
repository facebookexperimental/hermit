# Hermit backend parity matrix

This directory tracks executable parity contracts across Hermit's ptrace,
DynamoRIO (DBI), and KVM backends. `matrix.tsv` is the ratchet: changing a pair
from `gap` to `pass` makes `run_matrix.py` enforce it on every subsequent run.
A `gap` must have a concrete implementation reason.

## Current ratchet

| Backend | Passing pairs | Parity vs ptrace |
| --- | ---: | ---: |
| ptrace | 10/10 | 100% |
| DBI | 6/10 | 60% |
| KVM | 9/10 | 90% |

The task's pre-existing DBI-native baseline is 70/89 tests (78.7%). That number
measures the backend's own Reverie suite. The 6/10 number above is deliberately
separate: it measures the cross-backend Hermit contracts in this directory.
Conflating the two would overstate Detcore parity because the current DBI client
observes most syscalls but only rewrites `write` and CPUID; it does not yet use
Detcore's scheduler, virtual clock, PID model, or random model.

KVM loads dynamic Linux ELF programs through `KvmGuest<Detcore>` and passes
nine pairs, including its bounded cooperative pthread lifecycle and
deterministic clock, PID, and synthetic CPUID probes. The remaining gap is the
threaded random-source fixture: child-thread syscalls bypass per-child Detcore
callbacks, and the KVM personality's fixed random streams repeat across worker
threads.

## Matrix

| Test | ptrace | DBI | KVM |
| --- | --- | --- | --- |
| `hello_stdout` | pass | pass | pass |
| `argument_forwarding` | pass | pass | pass |
| `exit_zero` | pass | pass | pass |
| `exit_status` | pass | pass | pass |
| `file_read` | pass | pass | pass |
| `pthread_lifecycle` | pass | gap | pass |
| `cpuid_policy` | pass | pass | pass |
| `virtual_clock` | pass | gap | pass |
| `random_sources` | pass | gap | gap |
| `virtual_pid` | pass | gap | pass |

The authoritative reasons live in `matrix.tsv`, next to the status they
justify. The runner executes each passing pair three times and checks exit
status, stdout, and (for determinism cases) byte-identical repeated output.
These repeat-run results are compatibility evidence, not an L1/L2 assurance
level: the runner disables timeslicing and does not pass `--strict --verify`.

Hermit's KVM root process enters the shared tool through
`run_static_elf_with_tool::<Detcore>`, but child process and thread syscalls
currently execute in the backend's deterministic `ElfExecutor` personality
without per-child Detcore tool callbacks. The CPUID row similarly validates
reverie-kvm's backend-local `KVM_SET_CPUID2` policy, not Detcore CPUID-event
parity.

## Running

Validate the checked-in matrix without backend prerequisites:

```bash
python3 tests/backend-parity/run_matrix.py --check
```

Build Hermit, then enforce the ptrace baseline:

```bash
cargo build -p hermit
python3 tests/backend-parity/run_matrix.py --backend ptrace
```

Run DBI with the pinned DynamoRIO runtime and client built by Cargo:

```bash
cargo build -p hermit
python3 tests/backend-parity/run_matrix.py --backend dbi --require-backend
```

Run KVM on a host with read-write `/dev/kvm` access:

```bash
python3 tests/backend-parity/run_matrix.py --backend kvm --require-backend
```

Use `--probe-gaps` to execute documented gaps and report `XPASS` candidates.
Use `--output /tmp/backend-parity.tsv` to retain machine-readable observations.
`BLOCKED` means a required host capability or runtime artifact was absent; it
does not change the checked-in pass/gap claim.
