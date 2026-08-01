# Hermit backend parity matrix

This directory tracks executable parity contracts across Hermit's ptrace,
DynamoRIO (DBI), and KVM backends. `matrix.tsv` is the ratchet: changing a pair
from `gap` to `pass` (L1) or from `gap` to `detlog`/`guest` (L2) makes
`run_matrix.py` enforce it on every subsequent run. A `gap` must have a concrete
implementation reason.

## Current ratchet

The L1 ratchet (`--strict`, run three times, byte-identical stdout) and the L2
ratchet (`--strict --verify`, hermit's own double-run bitwise comparison) are
tracked separately, because a contract can hold at L1 yet not at L2.

L1 (`hermit run --strict`):

| Backend | Passing pairs | Parity vs ptrace |
| --- | ---: | ---: |
| ptrace | 23/23 | 100% |
| DBI | 22/23 | 96% |
| KVM | 22/23 | 96% |

L2 (`hermit run --strict --verify`):

| Backend | Verified pairs | L2 kind | Parity vs ptrace |
| --- | ---: | --- | ---: |
| ptrace | 23/23 | DETLOG-bitwise | 100% |
| DBI | 21/23 | DETLOG-bitwise | 91% |
| KVM | 21/23 | guest-visible only | 91% |

The two L2 assurance *kinds* are not interchangeable. **DETLOG-bitwise** L2
(ptrace, DBI) means hermit re-ran the guest and found the two normalized DETLOG
streams — the full syscall and scheduling trace — bitwise-identical.
**guest-visible** L2 (KVM) is strictly weaker: reverie-kvm runs concurrently and
declares outright that its internal syscall trace order is not deterministic, so
`--verify` compares only guest stdout and exit status across the two runs. KVM's
column is therefore capped at `guest`, never `detlog`. See the L2 subsection
below for the two contracts that hold at L1 but not L2.

The task's pre-existing DBI-native baseline is 70/89 tests (78.7%). That number
measures the backend's own Reverie suite. The 22/23 number above is deliberately
separate: it measures the cross-backend Hermit contracts in this directory.
The current DBI path satisfies the virtual clock, virtual PID, root-thread
random-source, process wait lifecycle, application executable-memory, and
file-mutation and file-metadata contracts, plus deterministic memory-advice and
memory-layout behavior. It also deterministically refuses io_uring and listmount,
verifies that epoll remains available as a fallback, and refuses process-memory
reads and writes with deterministic `EPERM`. The wait contract covers deterministic
`wait4`/`waitid` results, at least one SIGCHLD handler delivery (standard signals
may coalesce), complete reaping, and zeroed child CPU accounting. The
executable-memory contract writes machine code into an anonymous mapping,
transitions it from writable to executable, and calls it.
The memory-advice row checks accepted and rejected advice, address validation,
and file-backed `MADV_DONTNEED` restoration; KVM instead enforces its documented
deterministic `ENOSYS` refusal for `MADV_DONTNEED`. The memory-layout rows check
that `sbrk`/`brk` growth, ordered one-, two-, and three-page private anonymous
mappings, and a written two-page shared anonymous mapping produce the same
address sequences across repeated runs of each backend; they deliberately
permit different backend-local layouts. Portable pthread startup still exits
or stalls intermittently during DynamoRIO startup, so it remains an explicit
gap rather than making the strict CI gate flaky. The random-source row continues
to use root-only mode so it measures the cross-backend root stream independently
of the pthread lifecycle gap.

The file-mutation row creates, writes, attempts allocation, truncates, renames,
links, reads, and removes temporary files without exposing backend-specific metadata.
The file-metadata row checks positional I/O, ownership and access operations,
hard and symbolic links, path/fd/symlink extended attributes, a shared file
mapping, readahead, and range synchronization. It permits documented filesystem
policy failures for extended attributes but not an unimplemented syscall.
The io_uring fallback row requires all three io_uring entry points to return
deterministic `ENOSYS`, then checks that `epoll_create1` still succeeds.
The listmount row requires deterministic `ENOSYS` even when the host kernel
recognizes the syscall and returns `EINVAL` for the same request.
The process-memory refusal rows supply valid local and remote iovecs for
self-targeted `process_vm_readv` and `process_vm_writev` calls. Both require
deterministic `EPERM` without copying the source byte, while the same calls
succeed outside Hermit.

KVM loads dynamic Linux ELF programs through `KvmGuest<Detcore>` and passes
twenty-two pairs, including its bounded cooperative pthread lifecycle, executable
memory, deterministic memory-advice policy, clock, PID, synthetic CPUID, and
threaded random-source probes, plus file mutation, listmount refusal,
process-memory read/write refusal, io_uring refusal with epoll fallback,
repeatable heap growth, and private/shared anonymous mapping layouts. KVM
thread syscalls bypass per-child Detcore callbacks, but the shared personality
still provides distinct worker samples and byte-identical output across strict
verification runs. Its no-xattr filesystem model validates xattr targets and
arguments before returning deterministic Linux-compatible errors, while its
in-memory mapping model validates `msync` and translates range-advice file
descriptors. Serialized child exits support both `wait4` and `waitid`, including
canonical zero CPU accounting and complete reaping. The remaining process-wait
lifecycle gap is guest SIGCHLD handler delivery: the KVM personality records the
exit but does not yet synthesize an x86-64 signal frame to run the handler.

## Matrix

Each cell shows the L1 status and, after `/`, the L2 status: `detlog` for
DETLOG-bitwise L2, `guest` for KVM guest-visible L2, and `gap` where the level
is not reached.

| Test | ptrace | DBI | KVM |
| --- | --- | --- | --- |
| `hello_stdout` | pass / detlog | pass / detlog | pass / guest |
| `argument_forwarding` | pass / detlog | pass / detlog | pass / guest |
| `exit_zero` | pass / detlog | pass / detlog | pass / guest |
| `exit_status` | pass / detlog | pass / **gap** | pass / guest |
| `file_read` | pass / detlog | pass / detlog | pass / guest |
| `file_mutation` | pass / detlog | pass / detlog | pass / guest |
| `file_metadata` | pass / detlog | pass / detlog | pass / guest |
| `io_uring_fallback` | pass / detlog | pass / detlog | pass / guest |
| `listmount_unavailable` | pass / detlog | pass / detlog | pass / guest |
| `process_vm_readv_refusal` | pass / detlog | pass / detlog | pass / guest |
| `process_vm_writev_refusal` | pass / detlog | pass / detlog | pass / guest |
| `executable_mmap` | pass / detlog | pass / detlog | pass / guest |
| `memory_advice` | pass / detlog | pass / detlog | pass / guest |
| `heap_growth` | pass / detlog | pass / detlog | pass / guest |
| `anonymous_mmap_layout` | pass / detlog | pass / detlog | pass / guest |
| `shared_anonymous_mmap` | pass / detlog | pass / detlog | pass / guest |
| `pthread_lifecycle` | pass / detlog | gap / gap | pass / guest |
| `process_wait_accounting` | pass / detlog | pass / detlog | pass / **gap** |
| `process_wait_lifecycle` | pass / detlog | pass / detlog | gap / gap |
| `cpuid_policy` | pass / detlog | pass / detlog | pass / guest |
| `virtual_clock` | pass / detlog | pass / detlog | pass / guest |
| `random_sources` | pass / detlog | pass / detlog | pass / guest |
| `virtual_pid` | pass / detlog | pass / detlog | pass / guest |

The authoritative reasons live in `matrix.tsv`, next to the status they
justify. The runner executes each passing pair three times and checks exit
status, stdout, and (for determinism cases) byte-identical repeated output.
Passing `--strict` adds `hermit run --strict` to every probe; the hosted DBI
gate uses this mode.
The DBI random-source contract also compares the root thread's post-fault
random stream byte-for-byte with a ptrace reference run. It deliberately uses
the fixture's root-only mode to keep that comparison independent of the
pthread lifecycle row.
Without `--strict`, repeat-run results are compatibility evidence rather than
an assurance level. With `--strict`, they are L1 strict-mode evidence backed by
three byte-identical runs. The runner disables PMU timeslicing for portability.

### L2 verification (`--verify`)

Passing `--verify` lifts every probe to L2: the runner invokes
`hermit run --strict --verify --verify-allow both`, so hermit itself runs each
guest twice and asserts a bitwise-identical result. Because `--verify` diverts
the guest's own stdout into per-run temporary logs, the L2 path cannot re-check
stdout the way the L1 path does; instead it enforces that the guest exit status
matches and that hermit's double-run comparison succeeded at *at least* the
assurance kind recorded in `matrix.tsv`. The runner keys on two distinct stderr
witnesses: `Determinism verified` (DETLOG-bitwise, ptrace and DBI) and
`guest output and exit status matched` (KVM guest-visible). A DETLOG result
satisfies a `guest` contract because it is strictly stronger; the reverse fails.

Two contracts hold at L1 but not L2, and both are recorded as L2 `gap`s with
reasons in `matrix.tsv`:

- **`exit_status` on DBI.** With `--verify-allow both`, hermit runs the DBI
  guest only once when the first run exits non-zero — it never performs the
  second run — so the double-run DETLOG comparison never executes for this
  non-zero-exit contract. ptrace performs both runs and reaches `detlog` here.
- **`process_wait_accounting` on KVM.** The `--verify` concurrent double-run
  races child reaping: `waitid` on the already-reaped child returns `ECHILD`
  (`No child processes`), so the second run exits non-zero and verification
  fails. reverie-kvm synchronizes `wait4` child state but not `waitid`. This is
  reproducible across repeated runs; L1's stdout-only, three-run check does not
  surface it, which is precisely the value of the L2 lift.

Hermit's KVM root process enters the shared tool through
`run_static_elf_with_tool::<Detcore>`, but child process and thread syscalls
currently execute in the backend's deterministic `ElfExecutor` personality
without per-child Detcore tool callbacks. The CPUID row similarly validates
reverie-kvm's backend-local `KVM_SET_CPUID2` policy, not Detcore CPUID-event
parity.

## e9patch preprocessing corpus

e9patch is not a backend in this matrix. It is binary-rewriting *preprocessing*
for the ptrace backend: e9tool rewrites the guest ELF ahead of time to pre-trap
its `SYSCALL` sites, then Detcore runs the rewritten image under ptrace. e9tool
rewrites only the *main* executable, so the dynamically linked libc guests above
expose zero in-ELF `SYSCALL` sites (`candidate_sites=0`) and never exercise the
rewrite path — which is why e9patch is not a column here. Its parity is instead
ratcheted by `e9patch_corpus.py` over a freestanding, statically linked,
raw-`syscall` corpus under `e9patch_corpus/`, where `candidate_sites > 0`.

For each guest that harness enforces exit-status parity, stdout parity, golden
L2 (`hermit run --strict --verify`), e9patch L2
(`hermit --backend e9patch run --strict --verify`), full direct-AOT coverage
(`mapped_sites == candidate_sites > 0`), no signal fallback (`b0_sites == 0`),
and guest-syscall DETLOG **tail-match**: the golden guest-syscall sequence
equals the suffix of the e9patch sequence. Byte-identical DETLOG parity is
impossible by construction because the e9patch image runs a fixed deterministic
e9loader prologue (readlink/open/arch_prctl/`N`×mmap/close) before the guest's
`_start`; that prologue is a pure prefix, so the enforced parity is guest-syscall
DETLOG identity *modulo* the deterministic prologue, plus L2 and guest-visible
parity. No strict-detlog-identity claim is made.

Like the KVM `/dev/kvm` gate, this harness is `BLOCKED` in CI: it needs a hermit
built `--features e9patch` and a built e9tool/e9patch pair
(`HERMIT_E9TOOL`/`HERMIT_E9PATCH_BACKEND`). Run it locally:

```bash
cargo build -p hermit --features e9patch
HERMIT_E9TOOL=<path>/e9tool HERMIT_E9PATCH_BACKEND=<path>/e9patch \
    python3 tests/backend-parity/e9patch_corpus.py \
    --hermit target/debug/hermit --require-backend
```

Use `--check` to validate the corpus contract without prerequisites.

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
    --hermit target/release/hermit --backend dbi --strict --require-backend
```

Run KVM on a host with read-write `/dev/kvm` access:

```bash
python3 tests/backend-parity/run_matrix.py --backend kvm --require-backend
```

Enforce the L2 ratchet on any backend by adding `--verify` (it implies
`--strict`); hermit's own double-run then asserts the recorded L2 kind per
contract:

```bash
python3 tests/backend-parity/run_matrix.py --backend ptrace --verify --require-backend
python3 tests/backend-parity/run_matrix.py --hermit target/release/hermit \
    --backend dbi --verify --require-backend
python3 tests/backend-parity/run_matrix.py --backend kvm --verify --require-backend
```

Use `--probe-gaps` to execute documented gaps and report `XPASS` candidates
(in `--verify` mode the probe reports which L2 kind a gap actually reached).
Use `--output /tmp/backend-parity.tsv` to retain machine-readable observations.
`BLOCKED` means a required host capability or runtime artifact was absent; it
does not change the checked-in pass/gap claim.
