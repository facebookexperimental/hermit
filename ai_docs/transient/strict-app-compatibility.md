# `--strict --verify` real-world app compatibility matrix

Last tested: 2026-07-22

This report measures how unmodified host binaries behave under
`hermit run --strict --verify`. `--strict` enables strict deterministic mode:
*"Unsupported syscalls panic instead of passing through to the host kernel."*
`--verify` runs the guest twice and compares the two executions, failing if they
differ (nondeterminism) or if the guest itself errors.

## TL;DR

**`--strict` is currently unusable on essentially every modern
dynamically-linked binary.** All 13 real-world apps tested fail immediately at
process startup, and the failure is a hard `SIGSEGV`/abort rather than a clean
diagnostic. `--verify` on its own works fine; `--strict` is the sole cause of
every failure below.

Only **1 of 13** apps (`sed`) can be made to pass, and only after disabling the
first blocker with a glibc tunable.

## Test environment

- Hermit: `344200e45423bed3050f0cabf7192b82b95a2a6c` (worktree from `frontier`;
  see note below)
- Reverie: `e3e2c965e24b2a2287bac8b520caf7cd1b020d94`
- OS: Linux 6.13.2-0_fbk13_hardened x86_64
- CPU: AMD EPYC 9D85 158-Core
- CPUID faulting: unavailable on this host (harmless `ARCH_SET_CPUID ENODEV`
  warning only; not related to any failure here)
- Build: `cargo build` (debug), binary `target/debug/hermit`

> Branch note: the task requested "build from main," but `slot-init.sh slot81`
> checks the worktree out detached at `frontier` (`344200e`). Per project memory,
> `origin/main` lags and lacks dependencies needed to build; `frontier` builds
> cleanly. The dispatch code that produces these results
> (`detcore/src/lib.rs:1178-1186`) is not frontier-specific, so the conclusions
> apply to `main` as well.

## Method

For each app: `hermit run --strict --verify -- <cmd>` against a small text file
(`small.txt`, 200 lines). Recorded: pass/fail, exit code, and the first syscall
that triggers the panic.

Because the *first* blocker (`rseq`) masks everything else, each app was also run
with `rseq` disabled at the glibc level
(`GLIBC_TUNABLES=glibc.pthread.rseq=0`). This does not change Hermit; it only
stops glibc from issuing the `rseq` startup syscall, exposing the *next*
unsupported syscall so we can measure how deep each app gets.

`python3` on this host resolves (via `$PATH`) to Meta's `fbpython` wrapper, which
crashes independently; the real interpreter `/usr/bin/python3.9` was used
instead.

## Results

### As requested: `hermit run --strict --verify -- <cmd>`

| # | App        | Command                              | Result | Exit | First blocking syscall |
|---|------------|--------------------------------------|--------|-----:|------------------------|
| 1 | gzip       | `gzip -k -f small.txt && gzip -t`    | FAIL   | 1    | `rseq` |
| 2 | bzip2      | `bzip2 -k -f && bzip2 -t`            | FAIL   | 1    | `rseq` |
| 3 | xz         | `xz -k -f && xz -t`                  | FAIL   | 1    | `rseq` |
| 4 | grep       | `grep -c foo small.txt`              | FAIL   | 1    | `rseq` |
| 5 | sed        | `sed -n 's/foo/FOO/p;10p'`           | FAIL   | 1    | `rseq` |
| 6 | awk        | `awk '{s+=NF} END{print s}'`         | FAIL   | 1    | `rseq` |
| 7 | bash       | `bash -c` pipe + subshell            | FAIL   | 1    | `rseq` |
| 8 | sqlite3    | `sqlite3 :memory:` create/insert/sum | FAIL   | 1    | `rseq` |
| 9 | python3.9  | `python3.9 -c 'print(sum(...))'`     | FAIL   | 1    | `rseq` |
| 10| make       | `make -s -f Makefile`                | FAIL   | 1    | `rseq` |
| 11| openssl    | `openssl dgst -sha256 small.txt`     | FAIL   | 1    | `rseq` |
| 12| perl       | `perl -e` sum 1..100                 | FAIL   | 1    | `rseq` |
| 13| cmake      | `cmake --version`                    | FAIL   | 1    | `rseq` |

**13/13 FAIL, all at `rseq`, all before executing any app logic.**

### With `rseq` disabled (`GLIBC_TUNABLES=glibc.pthread.rseq=0`) — reveals depth

| # | App        | Result | Next blocking syscall |
|---|------------|--------|-----------------------|
| 1 | gzip       | FAIL   | `ioctl` |
| 2 | bzip2      | FAIL   | `ioctl` |
| 3 | xz         | FAIL   | `ioctl` |
| 4 | grep       | FAIL   | `lseek` |
| 5 | sed        | **PASS** (deterministic verified) | — |
| 6 | awk        | FAIL   | `lseek` |
| 7 | bash       | FAIL   | `ioctl` |
| 8 | sqlite3    | FAIL   | `ioctl` |
| 9 | python3.9  | FAIL   | `lseek` |
| 10| make       | FAIL   | `getcwd` |
| 11| openssl    | FAIL   | `lseek` |
| 12| perl       | FAIL   | `getuid` |
| 13| cmake      | FAIL   | `getcwd` |

**1/13 PASS** (`sed`). The rest hit a second tier of extremely common,
unmodeled syscalls.

## Root cause

`--strict` sets `config.panic_on_unsupported_syscalls = true`. Any syscall not
matched by detcore's ~113-arm dispatch falls to the catch-all, which panics:

```rust
// detcore/src/lib.rs:1178-1186
_ => {
    if config.panic_on_unsupported_syscalls {
        error!(...);
        panic!("unsupported syscall: {:?}", call);   // <-- --strict lands here
    }
    // non-strict: recordreplay passthrough / --allow-passthrough / ENOSYS
    ...
}
```

Two compounding problems:

1. **The set of "unsupported" syscalls includes ones every process uses.**
   `rseq`, `lseek`, `ioctl`, `getcwd`, `getuid`, and `getpid` all have **zero**
   dispatch arms in `detcore/src/lib.rs` and are classified `MISSING` in
   `ai_docs/syscall-coverage-map.md`. `rseq` in particular is issued by glibc
   ≥ 2.35 during startup of *every* process, so `--strict` cannot get any modern
   binary past `_start`.

2. **The panic is fatal and un-diagnosable to the user.** The syscall is handled
   inside a callback that runs on the cloned guest stack
   (`reverie-process/src/clone.rs:27`, `clone_with_stack::callback`), which is
   declared to not unwind. The `panic!` therefore becomes a
   `panic_cannot_unwind` → `SIGSEGV`, and Hermit reports only:

   ```
   thread caused non-unwinding panic. aborting.
   Error: Sandbox container exited unexpectedly
        > Process exited with code: Signaled(SIGSEGV, true)
   ```

   The useful line (`panicked at detcore/src/lib.rs:1185: unsupported syscall:
   Other(rseq, ...)`) is only visible with logging turned up; the default
   user-facing output is an opaque SIGSEGV.

`--verify` is not implicated: `hermit run --verify -- <cmd>` (no `--strict`)
verified determinism successfully for the same apps.

## Failure modes found (candidates to file as issues)

1. **`--strict` panics on `rseq`, blocking all modern glibc binaries.**
   Highest impact — it is a total blocker at startup. Options: model/no-op `rseq`
   (return `ENOSYS` deterministically as the kernel would for an unsupported
   feature, letting glibc fall back), or special-case it before the strict panic.

2. **`--strict` panics on ubiquitous syscalls `lseek`, `ioctl`, `getcwd`,
   `getuid`, `getpid`.** Even past `rseq`, these are the immediate next walls.
   They are deterministic or trivially determinizable and should not be treated
   as "unsupported" under strict.

3. **Unsupported-syscall panic surfaces as an opaque `SIGSEGV`, not a clean
   error.** Because it fires in the non-unwinding clone callback, users get no
   actionable message by default. Even if strict coverage stays narrow, the
   failure should be reported as a clear "unsupported syscall X under --strict"
   error with a nonzero-but-diagnostic exit, not a crash.

## Reproduction

```bash
H=target/debug/hermit
# Universal rseq blocker (any app):
$H run --strict --verify -- /bin/echo hello
#   -> SIGSEGV; log shows "unsupported syscall: Other(rseq, ...)"

# Second-tier blockers, exposed by disabling rseq in glibc:
GLIBC_TUNABLES=glibc.pthread.rseq=0 \
  $H run --strict --verify -- openssl dgst -sha256 small.txt
#   -> "unsupported syscall: Lseek(...)"

# The one that works today:
GLIBC_TUNABLES=glibc.pthread.rseq=0 \
  $H run --strict --verify -- sed -n 's/foo/FOO/p;10p' small.txt
#   -> ":: Success: deterministic. Determinism verified."
```

## Related docs

- `ai_docs/syscall-coverage-map.md` — full MISSING/DETERMINIZED classification
  (corroborates `rseq`, `lseek`, `ioctl`, `getcwd`, `getuid`, `getpid` = MISSING).
- `ai_docs/arbitrary-binary-matrix.md` — non-strict launch/functional matrix.
