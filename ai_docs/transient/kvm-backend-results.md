# KVM Backend Test Results

Last tested: 2026-07-22

This report records an attempt to validate Hermit's `--backend kvm` execution
backend against real applications (`echo`, a hello binary, Redis, Python), the
same way the DBI backend is being exercised. The headline result is that the
KVM backend **is not an execution backend**: it cannot run arbitrary Linux ELF
programs, so "test it with real apps" reduces to confirming the status of its
built-in demonstration path and the CLI plumbing around it.

## Test environment

- Repo: `rrnewton/hermit`, slot78 worktree, branch `kvm-backend-tests`
- Hermit base commit: `344200e` ("Reconcile backend wiring with refactor and
  reverie e3e2c965 API"), the frontier tip at test time
- Reverie deps pinned rev: `e3e2c965e24b2a2287bac8b520caf7cd1b020d94`
  (includes `reverie-kvm`)
- OS: Linux `6.13.2-0_fbk13_hardened_0_g02230262e956` x86_64
- CPU: AMD EPYC 9D85 158-Core (AMD-V)
- `/dev/kvm`: `crw-rw-rw-` (world read/write); KVM device access is not a blocker
- Build: `with-proxy cargo build --workspace` (clean, exit 0)
- Binary: `target/debug/hermit` (`hermit 0.1`)

## What the KVM backend actually is

`reverie-kvm` is a research backend that creates a VM + vCPU, exposes bounded
guest-physical memory, and turns a guest `vmcall`/`vmmcall` into a typed Reverie
syscall event (see the crate README). It applies a deterministic CPUID policy to
the vCPU via `KVM_SET_CPUID2` (masks `RDRAND`, `RDSEED`, TSX, AVX-512). It does
**not** yet provide a guest-kernel ABI, so it cannot load or execute a Linux ELF.

Hermit's driver reflects this. `hermit-cli/src/bin/hermit/backends.rs::run_kvm`
**ignores the program argument** and always builds a tiny real-mode guest that
issues a single `write(2)` via `vmcall`; the host handler performs the write so
the message reaches real stdout. The module doc-comment states plainly that the
DBI and KVM backends "do not yet load and execute arbitrary Linux ELF programs"
and only "run a minimal 'hello world' demonstration through their real
interception path."

Consequence: there is no code path in which `echo`, `hello`, `redis-server`, or
`python3` runs as a real guest under the KVM backend. The most that can be
validated is (a) the CLI wiring/availability reporting, and (b) whether the
vmcall→syscall-interception demo works on this host.

## Result 1 — On frontier, the KVM demo is unreachable (fails closed)

On the unmodified frontier base, every `--backend kvm` invocation fails **before**
reaching the demo:

```
$ ./target/debug/hermit --backend kvm run -- /bin/echo hello
Error: backend `kvm` is unavailable: the bare KVM prototype cannot execute Linux programs without a guest-kernel ABI
exit=1

$ ./target/debug/hermit run --backend kvm -- /bin/echo hello
Error: backend `kvm` is unavailable: the bare KVM prototype cannot execute Linux programs without a guest-kernel ABI
exit=1
```

Both the global (`hermit --backend kvm run …`) and subcommand
(`hermit run --backend kvm …`) positions behave identically.

Root cause is an ordering/consistency issue between two pieces of code:

- `hermit-cli/src/lib.rs::Backend::unavailable_reason` returns `Some(…)` for
  `Kvm` **unconditionally**. Even when `/dev/kvm` opens read-write, the
  `.or_else(…)` falls through to the fixed message *"the bare KVM prototype
  cannot execute Linux programs without a guest-kernel ABI."* So
  `Backend::Kvm.is_available()` is always `false`.
- `hermit-cli/src/bin/hermit/run.rs::main` calls `backend.ensure_available()?`
  (run.rs:746) **before** the backend dispatch `match` (run.rs:754-758). Since
  KVM is never "available", `ensure_available()` errors out first and the
  `Backend::Kvm => run_kvm(...)` arm (run.rs:757) is **dead / unreachable code**
  on this branch.

The fix for this exact reachability problem exists upstream as commit `07e6f80`
("Make `--backend kvm` reach its built-in demonstration"), part of the KVM
prototype work tracked as **PR #179**. That commit is **not merged into
frontier** (`git merge-base --is-ancestor 07e6f80 HEAD` → false), so frontier
still has the inconsistent behavior above.

## Result 2 — With the gate bypassed, the demo works on this host

To confirm whether the `reverie-kvm` interception path itself works on this AMD
EPYC host (i.e. what a frontier including PR #179 would do), a **temporary,
uncommitted** one-line edit skipped the availability gate for KVM
(`} else if backend != Backend::Kvm { backend.ensure_available()?; }` in
run.rs). After `cargo build -p hermit`:

```
$ ./target/debug/hermit run --backend kvm -- /bin/echo hello
hermit: [kvm backend] "/bin/echo" is not executed as an ELF; the reverie-kvm prototype runs a built-in hello-world guest that issues write(2) via vmcall.
hello world
exit=0

$ ./target/debug/hermit run --backend kvm -- /usr/bin/python3 -c 'print(1)'
hermit: [kvm backend] "/usr/bin/python3" is not executed as an ELF; the reverie-kvm prototype runs a built-in hello-world guest that issues write(2) via vmcall.
hello world
exit=0
```

Observations:

- The demo runs successfully: KVM guest creation, `KVM_RUN`, the real-mode
  `vmcall`, and the host-side `write(2)` all succeed and `hello world` reaches
  stdout. The `reverie-kvm` VM-exit → syscall-interception path is functional on
  this hardware / kernel.
- **The program argument is completely ignored.** Whether given `/bin/echo`,
  `/usr/bin/python3 -c 'print(1)'`, or anything else, the output is always the
  fixed `hello world` — never the program's real output (`hello`, `1`, etc.).
- The only way the program argument matters is Hermit's generic existence check
  (`validate_program`), which still runs first:
  ```
  $ ./target/debug/hermit run --backend kvm -- /no/such/prog
  Error: Program /no/such/prog does not exist or is not accessible. ...
  exit=1
  ```

The temporary edit was reverted immediately after this experiment; the working
tree is clean and this branch contains **no source changes** to the backend.

## Per-application results

| App | Command | Result under `--backend kvm` |
| --- | --- | --- |
| echo | `run --backend kvm -- /bin/echo hello` | frontier: fails closed (unavailable). Gate-bypassed: prints `hello world` (demo), ELF not run. |
| hello binary | any ELF path | Same as echo — program arg ignored; demo only. |
| Redis | `run --backend kvm -- redis-server …` | **Not runnable.** KVM backend never execs an ELF; a long-lived multi-threaded server is far beyond a single-`write` real-mode demo. Not attempted as it cannot succeed by construction. |
| Python | `run --backend kvm -- /usr/bin/python3 -c 'print(1)'` | frontier: fails closed. Gate-bypassed: prints `hello world` (demo), interpreter not run. |

For reference, the ptrace backend runs these normally, e.g.:

```
$ ./target/debug/hermit run -- /bin/echo hello
... (CPUID-faulting WARN on this AMD host; rseq blocked ENOSYS) ...
hello
exit=0
```

## Performance comparison vs ptrace

Not applicable. A performance comparison requires both backends to execute the
same real workload. The KVM backend executes no real workload (only a fixed
`hello world` demo), so there is no comparable unit of work to time against
ptrace. Any wall-clock number for the KVM demo would measure VM setup for a
single `write`, not application execution, and would be misleading.

## Bugs / issues identified

1. **Frontier: `run_kvm` demo is unreachable (dead code).** `ensure_available()`
   (run.rs:746) rejects KVM before the dispatch `match` (run.rs:757) can call
   `run_kvm`, because `unavailable_reason` always returns `Some` for KVM. Users
   on frontier get "unavailable" and never see the demo. Fixed by PR #179 commit
   `07e6f80`, which is not yet merged to frontier. Recommendation: land/rebase
   PR #179 onto frontier so the demo becomes reachable, then re-run this matrix.
2. **KVM backend is not an execution backend (by design, but under-signalled at
   the CLI).** `--backend kvm` accepts and validates an arbitrary program path,
   implying it will run it, but silently ignores it and runs a hello-world demo.
   The stderr notice mitigates this, but `--help` ("Use the KVM backend") does
   not hint that arbitrary programs are not executed. Not a correctness bug in
   the demo; a usability/expectations gap.

## Conclusion

The KVM backend cannot be "validated with real apps" the way DBI can, because it
is a prototype vmcall demonstration, not an ELF execution backend. Concretely:

- **What works:** the `reverie-kvm` VM-exit → typed-syscall interception path and
  deterministic CPUID vCPU policy; the built-in `write(2)` hello-world demo runs
  correctly on this AMD EPYC / KVM host when it is reachable.
- **What does not work / is out of scope:** running `echo`, `hello`, Redis,
  Python, or any real ELF; only the fixed demo output is ever produced.
- **Frontier blocker:** the demo is currently unreachable on frontier
  (availability gate ordering); PR #179 (`07e6f80`) is required to reach it and
  is not yet merged here.

Next steps to make the KVM backend genuinely testable with real apps require the
planned Linux guest-kernel execution bridge in `reverie-kvm` (per its README);
until then, KVM validation is limited to the interception-path demo above.
