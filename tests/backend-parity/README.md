# Hermit backend parity runner

This directory tracks executable parity contracts across Hermit's ptrace,
DynamoRIO (DBT), and KVM backends. The case catalog and its small set of known
gaps live in `run_matrix.py`; new cases are green contracts by default. Live
results are compatibility measurement state, so when Hermit is checked out
inside dev-hermit the runner writes them to one ignored per-run observation
file under `compat-envelope/ignored/backend-parity/` instead of maintaining a
generated TSV here. It does **not** touch the tracked
`compat-envelope/scorecard.csv`; that artifact is advanced only by the parent
publisher `compat-envelope/publish-scorecard.py`, whose exact invocation the
runner prints at the end of the run.

## Current ratchet

The L1 ratchet (`--strict`, run three times, byte-identical stdout) and the
Stripped verification ratchet (`--strict --verify`, Hermit's double-run
comparison after selected numeric, address, path, and time fields are stripped)
are tracked separately. Stripped verification is not L2.

L1 (`hermit run --strict`):

| Backend | Passing pairs | Parity vs ptrace |
| --- | ---: | ---: |
| ptrace | 28/28 | 100% |
| DBT | 26/28 | 93% |
| KVM | 23/28 | 82% |

Stripped verification (`hermit run --strict --verify`):

| Backend | Verified pairs | Verification kind | Parity vs ptrace |
| --- | ---: | --- | ---: |
| ptrace | 28/28 | Stripped DETLOG | 100% |
| DBT | 26/28 | Stripped DETLOG | 93% |
| KVM | 22/28 | guest-visible only | 79% |

The two verification kinds are not interchangeable. **Stripped DETLOG**
(ptrace, DBT) means Hermit re-ran the guest and found the two normalized DETLOG
streams equal after stripping selected fields; it does not mean the full syscall
and scheduling traces were bitwise-identical and does not establish L2.
**guest-visible** verification (KVM) is weaker: reverie-kvm runs concurrently and
declares outright that its internal syscall trace order is not deterministic, so
`--verify` compares only guest stdout and exit status across the two runs. KVM's
column is therefore capped at `guest`, never `detlog`. See the verification
subsection below for the contract that holds at L1 but not under `--verify`.

The task's pre-existing DBT-native baseline is 70/89 tests (78.7%). That number
measures the backend's own Reverie suite. The 23/24 number above is deliberately
separate: it measures the cross-backend Hermit contracts in this directory.
The current DBT path satisfies the virtual clock, virtual PID, root-thread
random-source, process wait lifecycle, application executable-memory, and
file-mutation contracts, plus deterministic memory-advice and
memory-layout behavior. It is an explicit gap on the file-metadata row (see
below). It also deterministically refuses io_uring and listmount,
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
policy failures for extended attributes but not an unimplemented syscall. DBT is
an explicit gap on this row: it forwards `fchown` to the real kernel, so once
credential queries are determinized to virtual-root identity `0` (PR #1549) the
guest's `fchown(fd, 0, 0)` becomes an unprivileged chown-to-root and returns
`EPERM`, while ptrace remaps it through the user namespace. `fchown` is not
correctly implemented under DBT, and asserting against a half-implemented syscall
could pass by accident and prove nothing, so the DBT cell is a declared gap until
DBT determinizes `fchown`.
The io_uring fallback row requires all three io_uring entry points to return
deterministic `ENOSYS`, then checks that `epoll_create1` still succeeds.
The listmount row requires deterministic `ENOSYS` even when the host kernel
recognizes the syscall and returns `EINVAL` for the same request.
The process-memory refusal rows supply valid local and remote iovecs for
self-targeted `process_vm_readv` and `process_vm_writev` calls. Both require
deterministic `EPERM` without copying the source byte, while the same calls
succeed outside Hermit.

KVM loads dynamic Linux ELF programs through `KvmGuest<Detcore>` and passes
twenty-three pairs, including its bounded cooperative pthread lifecycle, executable
memory, deterministic memory-advice policy, clock, PID, inert scheduler-policy
queries, synthetic CPUID, and
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

## Cases

Each cell shows the L1 status and, after `/`, the `--verify` status: `detlog`
for Stripped DETLOG equality, `guest` for KVM guest-visible equality, and `gap`
where verification does not succeed. Neither successful status is an L2 claim.

| Test | ptrace | DBT | KVM |
| --- | --- | --- | --- |
| `hello_stdout` | pass / detlog | pass / detlog | pass / guest |
| `argument_forwarding` | pass / detlog | pass / detlog | pass / guest |
| `exit_zero` | pass / detlog | pass / detlog | pass / guest |
| `exit_status` | pass / detlog | pass / detlog | pass / guest |
| `file_read` | pass / detlog | pass / detlog | pass / guest |
| `file_mutation` | pass / detlog | pass / detlog | pass / guest |
| `file_metadata` | pass / detlog | gap / gap | pass / guest |
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
| `scheduler_policy_queries` | pass / detlog | pass / detlog | pass / guest |
| `signal_disposition` | pass / detlog | pass / detlog | **gap** / gap |
| `sigaction_state` | pass / detlog | pass / detlog | **gap** / gap |
| `sigprocmask_state` | pass / detlog | pass / detlog | **gap** / gap |
| `sigaltstack_state` | pass / detlog | pass / detlog | **gap** / gap |

The `scheduler_policy_queries` contract pins Detcore's inert-scheduler-policy
model: the guest arms and re-reads an `ITIMER_REAL` one-shot against virtual
time, queries `ioprio_get` (fixed virtual default 0), and issues a
`sched_setattr` requesting `SCHED_DEADLINE`. That last call returns `EPERM`
outside Hermit (real-time scheduling needs privilege), but Detcore accepts it
as a deterministic no-op because it replaces the Linux scheduler with its own,
so the guest observes an identical, host-independent result across ptrace, DBT,
and KVM and across the `--verify` double run.

The authoritative exceptions and their reasons live in `L1_GAPS` and
`L2_GAPS` in the runner; `L2_GAPS` is the existing source identifier, not an L2
claim. The runner executes each passing pair three times and
checks exit status, stdout, and (for determinism cases) byte-identical repeated output.
Cross-backend stdout SHA-256 equality is an exact-byte contract only for rows
that already define one: fixed `expected_stdout` rows, the dynamic
`virtual_pid` row, and DBT's pre-existing ptrace root-stream comparison for
`random_sources`. Those rows capture both raw operands without normalization,
and a missing side or unequal digest fails regardless of whether an observation
artifact is written. Dynamic memory-layout and clock rows remain explicitly
within-backend repeatability contracts; their observation rows do not invent a
cross-backend digest verdict. In particular, the absolute-address cases below
remain outside the raw stdout-parity contract.
Passing `--strict` adds `hermit run --strict` to every probe; the hosted DBT
gate uses this mode.
The DBT random-source contract also compares the root thread's post-fault
random stream byte-for-byte with a ptrace reference run. It deliberately uses
the fixture's root-only mode to keep that comparison independent of the
pthread lifecycle row.
Without `--strict`, repeat-run results are compatibility evidence rather than
an assurance level. With `--strict`, they are L1 strict-mode evidence backed by
three byte-identical runs. The runner disables PMU timeslicing for portability.

### Stripped verification (`--verify`)

Passing `--verify` adds a two-run comparison: the runner invokes
`hermit run --strict --verify --verify-allow both`. For ptrace and DBT, Hermit
compares DETLOG streams after Stripped normalization; this is not bitwise parity
and not L2. Because `--verify` diverts the guest's own stdout into per-run
temporary logs, this path cannot re-check stdout the way the L1 path does;
instead it enforces that the guest exit status matches and that Hermit's
double-run comparison succeeded at *at least* the verification kind expected
for the backend. The runner keys on two stderr witnesses: `Determinism verified`
(Stripped DETLOG, ptrace and DBT) and `guest output and exit status matched`
(KVM guest-visible). A DETLOG result satisfies a `guest` contract because it
compares more observations; the reverse fails.

One contract holds at L1 but not under `--verify` and is recorded as a `gap`
with its reason in the runner:

- **`process_wait_accounting` on KVM.** The `--verify` concurrent double-run
  races child reaping: `waitid` on the already-reaped child returns `ECHILD`
  (`No child processes`), so the second run exits non-zero and verification
  fails. reverie-kvm synchronizes `wait4` child state but not `waitid`. This is
  reproducible across repeated runs; L1's stdout-only, three-run check does not
  surface it, which is precisely the additional value of the two-run check.

Hermit's KVM root process enters the shared tool through
`run_static_elf_with_tool::<Detcore>`, but child process and thread syscalls
currently execute in the backend's deterministic `ElfExecutor` personality
without per-child Detcore tool callbacks. The CPUID row similarly validates
reverie-kvm's backend-local `KVM_SET_CPUID2` policy, not Detcore CPUID-event
parity.

### Memory-layout ADDRESSES are not a parity contract under DBT

`anonymous_mmap_layout` checks that a backend places anonymous mappings
**repeatably across its own runs**. Its name invites a stronger reading, so
state the limit explicitly: it does **not** compare layout between backends, and
it cannot — the guest prints absolute addresses (`multiple %p %p %p`), which a
translator necessarily shifts.

Do not add a cross-backend fixture that asserts anonymous-mmap addresses, either
absolute **or relative**. It is unreachable for DBI, and the reason is
structural rather than a bug to fix:

* DynamoRIO's runtime makes the guest-visible address space **185 VMAs instead
  of 25**, which changes where the kernel's top-down allocator puts the
  **guest's own** mappings.
* This is *not* translator allocations interleaving with the guest's. Measured:
  under DBI four successive anonymous mmaps of 1+2+3+4 pages occupy a span of
  **exactly the 10 pages requested, coalesced into one VMA, with zero DynamoRIO
  allocations inside it**. The ptrace arm needs 14 pages for the same 10,
  because of glibc loader slack. DBI packs *tighter*, not looser.
* It is therefore also not a separability problem. DR's allocations are
  separable by provenance (`dr_memory_is_dr_internal`, `dr_query_memory_ex`);
  perfect separability would not move the guest's own mappings by one byte.
  Attribution and placement are different properties.
* There is no stable ptrace layout to match in any case. Varying the guest's
  allocation prefix by 0–8 pages produces **nine distinct ptrace layouts and one
  DBI layout**, and at a 7-page prefix the two backends are byte-identical.
  Native itself shows a 1–2% tail on the same measurement.

What **is** a parity contract, and does hold on every backend measured: pointer
**ordering** between mappings, and mapping **contents**. Assert those.

Evidence and reproduction: `dev-hermit` PR #60,
`experiments/dbi-anon-mmap-layout-divergence_20260806/` (`VERDICT.md`,
`README-prefix-sweep.md`, `interleave.txt`).

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
Stripped verification (`hermit run --strict --verify`), e9patch Stripped
verification (`hermit --backend e9patch run --strict --verify`), full direct-AOT
coverage
(`mapped_sites == candidate_sites > 0`), no signal fallback (`b0_sites == 0`),
and guest-syscall DETLOG **tail-match**: the golden guest-syscall sequence
equals the suffix of the e9patch sequence. Byte-identical DETLOG parity is
impossible by construction because the e9patch image runs a fixed deterministic
e9loader prologue (readlink/open/arch_prctl/`N`×mmap/close) before the guest's
`_start`; that prologue is a pure prefix, so the enforced parity is guest-syscall
DETLOG identity *modulo* the deterministic prologue, plus Stripped and
guest-visible equality. No strict-detlog-identity or L2 claim is made.

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

## Splitting asymmetric backlog PRs

PRs that predate the shared-manifest symmetry guard may combine useful code
with additions to this backend-private corpus. Do not hand-edit those patches.
Plan a lossless split first:

```bash
tests/backend-parity/split_asymmetric_pr.py --pr <number>
```

The dry run assigns every changed path and hunk to code or deferred tests,
replays both partitions, and requires their union to reproduce the source PR's
Git tree exactly. It fails instead of guessing on mixed inventory edits,
private-test deletions, unknown asymmetry shapes, or code replay conflicts.

Publishing is a separate explicit operation:

```bash
tests/backend-parity/split_asymmetric_pr.py --pr <number> --publish \
  --role-tag '[impl agent, MODEL]'
```

A mixed PR becomes a code-only draft against fresh `main` and a test-only draft
against the source PR's original base. The latter is labeled
`matrix-asymmetric-tests-deferred` and carries a required next-action checklist:
promote through the shared ptrace front door, minimize then promote, or reject
with evidence. The welded source closes only after both replacements exist. A
test-only source stays open as the labeled deferred PR; the tool does not create
an empty code PR.

The open PR count can rise after a mixed split. That is intentional: one
unlandable PR becomes landable code plus explicit, queryable test debt.

## Running

Validate the case catalog and known-gap invariants without backend prerequisites:

```bash
python3 tests/backend-parity/run_matrix.py --check
```

Build Hermit, then enforce the ptrace baseline:

```bash
cargo build -p hermit
python3 tests/backend-parity/run_matrix.py --backend ptrace
```

Run DBT with the pinned DynamoRIO runtime and client built by Cargo:

```bash
cargo build --release -p hermit
python3 tests/backend-parity/run_matrix.py \
    --hermit target/release/hermit --backend dbt --strict --require-backend
```

Run KVM on a host with read-write `/dev/kvm` access:

```bash
python3 tests/backend-parity/run_matrix.py --backend kvm --require-backend
```

Enforce the Stripped verification ratchet on any backend by adding `--verify`
(it implies `--strict`); Hermit's double-run then asserts the recorded
verification kind per contract:

```bash
python3 tests/backend-parity/run_matrix.py --backend ptrace --verify --require-backend
python3 tests/backend-parity/run_matrix.py --hermit target/release/hermit \
    --backend dbt --verify --require-backend
python3 tests/backend-parity/run_matrix.py --backend kvm --verify --require-backend
```

Use `--probe-gaps` to execute documented gaps and report `XPASS` candidates
(in `--verify` mode the probe reports which verification kind a gap reached).
Every non-check run auto-discovers an outer dev-hermit checkout and writes its
observation rows to one ignored per-run file,
`compat-envelope/ignored/backend-parity/<run-id>.csv`; the tracked
`compat-envelope/scorecard.csv` is left unchanged. The run then prints the exact
`publish-scorecard.py --observation ... --current ... --history ...` command that
folds that observation into the tracked scorecard and its history, so publishing
is a reviewed step rather than a side effect of measuring. Use
`--parent-scorecard PATH` to write the observation to that exact artifact
instead of the default per-run file (the tracked
`compat-envelope/scorecard.csv` is always refused), `--no-parent-scorecard` to
skip only observation output without weakening any comparison, or `--output
/tmp/backend-parity.tsv` for the legacy standalone observation TSV. `BLOCKED`
means a required host capability or runtime artifact was absent; it does not
change the known-gap contract.
