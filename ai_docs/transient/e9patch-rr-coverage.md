# e9patch record/replay coverage

Measured: 2026-07-25

## Result

PR #696 adds the missing e9patch record CLI path and fixes replay-output
truncation under pipe backpressure.

The 34-program post-fix `validate.sh`-derived matrix passes completely:

- Ptrace: **34/34 PASS R/R**.
- E9patch: **34/34 PASS R/R**.
- E9patch exercised five rewritten executables: `gcc` (28 sites), `g++`
  (28), `cpp` (28), `gcov` (10), and `lscpu` (1).
- The other 29 e9patch rows exercised the zero-site path.
- Every passing row matched record/replay exit status, stdout, and stderr.

Record mode is strict by definition. These are record/replay compatibility
results, not L1-L4 `run --verify` assurance claims.

## Snapshot

- Merged `origin/main`: `2327925ff9db7a05488996e3ac79f5bf2dbebe6e`.
- Measured merge commit: `0c452c83b69b22ec719a344e555d1f983bee42ed`.
- Branch: `impl-e9patch-rr-cli-integration-slot115`.
- Pull request: #696, `Add e9patch record and replay integration`.
- Host: x86_64 Linux `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79`.
- CPU: AMD EPYC 9D85 158-Core Processor.
- Optimized post-fix Hermit SHA-256:
  `086bf3255af462a3b149ba4b1ff45d79b1e1505599b7608ced3effdec60b8085`.
- e9tool SHA-256:
  `8569c9c62f2b9ad79f22903ae01b58d99abad438023f7a4d49538785419625d0`.
- e9patch SHA-256:
  `083e7deee709d66b82ca9e3692c7cd31326e64fdcec515704c769d336320d5fe`.
- Matrix build: current-main post-fix debug build; final rewritten-GCC and
  `cat` smokes also passed with the optimized build above.
- Log level: default.
- Relaxations: none.

## Integration

`hermit --backend e9patch record` now:

1. Resolves the guest executable and runs the cached e9patch preprocessor.
2. Bind-mounts the prepared ELF read-only over the canonical original path in
   the recording container.
3. Records the original program, argv, and executable path while the existing
   recorder copies the prepared bytes into `recording/exe`.
4. Replays from that saved executable through the existing ptrace replayer.

Replay intentionally remains backend-independent. It does not need e9tool,
e9patch, or the instruction-map cache after recording.

The replay output path now retries after `EAGAIN` instead of silently dropping
the unwritten suffix. Per-stream locks preserve ordering, readiness polling
runs in bounded blocking-pool tasks, and shared nonblocking flags are restored
before every async suspension. Regressions cover blocking-pipe success and
cancellation plus 256 KiB stdout and stderr record/verify flows.

## Method

Each current-main matrix row used a fresh recording home and a bounded inline
record/replay comparison:

```text
ptrace:
  timeout 120s hermit --backend ptrace record start --verify \
    --data-dir CASE/recording-home --record-timeout 90 -- PROGRAM ARGS...

e9patch:
  HERMIT_E9TOOL=... HERMIT_E9PATCH_BACKEND=... \
  timeout 120s hermit --backend e9patch record start --verify \
    --data-dir CASE/recording-home --record-timeout 90 -- PROGRAM ARGS...
```

Success required exit 0 and Hermit's comparison of record/replay status,
stdout, stderr, and normalized logs. A separate normal e9patch record plus
cache-independent replay checked saved-artifact behavior.

## Comparison table

| Program | Workload | Ptrace R/R | E9patch R/R | Mapped sites |
| --- | --- | --- | --- | ---: |
| `echo` | `echo hermit-compat` | PASS | PASS | 0 |
| `true` | no arguments | PASS | PASS | 0 |
| `pwd` | no arguments | PASS | PASS | 0 |
| `seq` | `seq 10` | PASS | PASS | 0 |
| `cat` | `cat README.md` | PASS | PASS | 0 |
| `wc` | `wc -c README.md` | PASS | PASS | 0 |
| `head` | `head -n 3 README.md` | PASS | PASS | 0 |
| `base64` | encode `README.md` | PASS | PASS | 0 |
| `base32` | encode `README.md` | PASS | PASS | 0 |
| `id` | `id -u` | PASS | PASS | 0 |
| `lua` | `print(42)` | PASS | PASS | 0 |
| `perl` | print `42` and newline | PASS | PASS | 0 |
| `awk` | `BEGIN { print 42 }` | PASS | PASS | 0 |
| `sqlite3` | in-memory insert/count/sum | PASS | PASS | 0 |
| `bash` | deterministic three-line loop | PASS | PASS | 0 |
| `gcc` | `--version` | PASS | PASS | 28 |
| `g++` | `--version` | PASS | PASS | 28 |
| `make` | `--version` | PASS | PASS | 0 |
| `cpp` | `--version` | PASS | PASS | 28 |
| `gcov` | `--version` | PASS | PASS | 10 |
| `jq` | range/filter JSON workload | PASS | PASS | 0 |
| `xmllint` | `--version` | PASS | PASS | 0 |
| `clang` | `--version` | PASS | PASS | 0 |
| `javac` | `-version` | PASS | PASS | 0 |
| `wget` | `--version` | PASS | PASS | 0 |
| `netcat` | `-h` | PASS | PASS | 0 |
| `find` | `/etc -maxdepth 1` | PASS | PASS | 0 |
| `env` | clean environment round trip | PASS | PASS | 0 |
| `factor` | factor 42 | PASS | PASS | 0 |
| `ip` | `-V` | PASS | PASS | 0 |
| `ss` | `-V` | PASS | PASS | 0 |
| `lsof` | `-v` | PASS | PASS | 0 |
| `lscpu` | `--version` | PASS | PASS | 1 |
| `time` | `--version` | PASS | PASS | 0 |

## Focused evidence

The rewritten GCC recording proves identity preservation and cache-independent
replay:

- E9patch diagnostic: 28 candidate sites, 28 mapped sites, 0 B0 sites.
- Recording metadata `exe`, `program`, and `arg0`: `/usr/bin/gcc`.
- Prepared artifact and saved `recording/exe` SHA-256:
  `73849134719da2234953fe22a2b5f97ac6fa9aa985cabb7e0a18135b953b8dae`.
- Record/replay stdout: byte-identical.
- Replay passed without `HERMIT_E9TOOL` or `HERMIT_E9PATCH_BACKEND`.
- `hermit --backend e9patch record start --verify -- /usr/bin/gcc --version`
  also reported `Success: replay matched recording` in debug and optimized
  builds.

The former `cat` regression is fixed at default logging:

- Source, record stdout, and replay stdout: 13,331 bytes each.
- Record/replay stdout SHA-256:
  `8433d783d9d1c305f3b7c0d0b88dec6ab763b4a3e80fe24f386db39b6f6fcbc0`.

## Remaining failure

An executable Python shebang script correctly takes the e9patch non-ELF
fallback (`mapped_sites=0; preprocessing=not-applicable`) and records
successfully, but replay fails while constructing its chroot:

```text
Error: Failed to create chroot environment
     > File exists (os error 17)
```

The identical ptrace record/replay case fails the same way, so this is an
existing shebang replay limitation rather than an e9patch regression. The
34-program matrix contains ELF entrypoints and is unaffected.

## Validation

- `cargo test -p hermit --lib --bin hermit`: 52 library and 63 CLI tests
  pass.
- `cargo test -p hermit --test record_replay -- --test-threads=1`: all 34
  tests pass. A parallel full-package run exceeded one timeout test's
  15-second wall-clock bound under concurrent load; its isolated rerun passed
  in 1.03 seconds.
- `cargo fmt --all -- --check`: pass.
- `cargo clippy -p hermit --all-targets -- -D warnings`: pass.
- Ptrace/e9patch current-main matrix: 34/34 and 34/34 PASS R/R.
- Optimized rewritten-GCC normal record/cache-independent replay: pass.
- Optimized 13,331-byte `cat` normal record/replay: pass.

Raw local evidence:

- `/tmp/e9rr-results-pr696-main.csv`
- `/tmp/e9rr-matrix-pr696-main.2ZeeDe/`
- `/tmp/e9rr-pr696-final-gcc.FyT8Bf/`
- `/tmp/e9rr-pr696-final-cat.KjQ3oe/`
