# Hermit backend parity matrix

This directory tracks executable parity contracts across Hermit's ptrace,
DynamoRIO (DBI), and KVM backends. `matrix.tsv` is the ratchet: changing a pair
from `gap` to `pass` makes `run_matrix.py` enforce it on every subsequent run.
A `gap` must have a concrete implementation reason.

## Current ratchet

| Backend | Passing pairs | Parity vs ptrace |
| --- | ---: | ---: |
| ptrace | 13/13 | 100% |
| DBI | 12/13 | 92% |
| KVM | 11/13 | 85% |

The task's pre-existing DBI-native baseline is 70/89 tests (78.7%). That number
measures the backend's own Reverie suite. The 12/13 number above is deliberately
separate: it measures the cross-backend Hermit contracts in this directory.
The current DBI path satisfies the virtual clock, virtual PID, root-thread
random-source, process wait lifecycle, and application executable-memory
contracts, plus deterministic memory-advice behavior. The wait contract covers
deterministic `wait4`/`waitid` results, at least one SIGCHLD handler delivery
(standard signals may coalesce), complete reaping, and zeroed child CPU
accounting. The executable-memory contract writes machine code into an
anonymous mapping, transitions it from writable to executable, and calls it.
The memory-advice row checks accepted and rejected advice, address validation,
and file-backed `MADV_DONTNEED` restoration; KVM instead enforces its documented
deterministic `ENOSYS` refusal for `MADV_DONTNEED`. Hosted pthread startup can
still stall during native startup and remains the sole DBI gap; child-thread
random sources remain covered by that lifecycle gap rather than the
random-source pair.

KVM loads dynamic Linux ELF programs through `KvmGuest<Detcore>` and passes
eleven pairs, including its bounded cooperative pthread lifecycle, executable
memory, deterministic memory-advice policy, clock, PID, and synthetic CPUID
probes. Its remaining gaps are the threaded random-source fixture, where
child-thread syscalls bypass per-child Detcore callbacks and the KVM personality
repeats fixed random streams across workers, and process wait accounting,
because KVM child processes do not run through per-child Detcore callbacks.

## Matrix

| Test | ptrace | DBI | KVM |
| --- | --- | --- | --- |
| `hello_stdout` | pass | pass | pass |
| `argument_forwarding` | pass | pass | pass |
| `exit_zero` | pass | pass | pass |
| `exit_status` | pass | pass | pass |
| `file_read` | pass | pass | pass |
| `executable_mmap` | pass | pass | pass |
| `memory_advice` | pass | pass | pass |
| `pthread_lifecycle` | pass | gap | pass |
| `process_wait_lifecycle` | pass | pass | gap |
| `cpuid_policy` | pass | pass | pass |
| `virtual_clock` | pass | pass | pass |
| `random_sources` | pass | pass | gap |
| `virtual_pid` | pass | pass | pass |

The authoritative reasons live in `matrix.tsv`, next to the status they
justify. The runner executes each passing pair three times and checks exit
status, stdout, and (for determinism cases) byte-identical repeated output.
The DBI random-source contract also compares the root thread's post-fault
random stream byte-for-byte with a ptrace reference run. It deliberately uses
the fixture's root-only mode because hosted DBI pthread startup remains a
separate declared gap.
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
cargo build --release -p hermit
python3 tests/backend-parity/run_matrix.py \
    --hermit target/release/hermit --backend dbi --require-backend
```

Run KVM on a host with read-write `/dev/kvm` access:

```bash
python3 tests/backend-parity/run_matrix.py --backend kvm --require-backend
```

Use `--probe-gaps` to execute documented gaps and report `XPASS` candidates.
Use `--output /tmp/backend-parity.tsv` to retain machine-readable observations.
`BLOCKED` means a required host capability or runtime artifact was absent; it
does not change the checked-in pass/gap claim.
