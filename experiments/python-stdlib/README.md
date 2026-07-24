# Python standard-library tests under strict Hermit

## Scope

This experiment uses the system CPython 3.12.13+meta installation and runs the
following standard-library test modules:

| Module | Cases | Expected skips | Result |
| --- | ---: | ---: | --- |
| `test_math` | 79 | 2 | Pass |
| `test_string` | 38 | 0 | Pass |
| `test_re` | 164 | 2 | Pass |
| `test_json` | 180 | 1 | Pass |
| `test_hashlib` | 78 | 12 | Pass |
| **Total** | **539** | **17** | **Pass** |

The skips are CPython's normal resource gates for CPU-intensive, network
download, debug-build, and very-large-memory cases.

## Results

The native control passed all five modules:

```text
python3 -m test -v test_math test_string test_re test_json test_hashlib
All 5 tests OK.
Total tests: run=539 skipped=17
```

The equivalent direct `python3 -m test` invocation does not currently
complete under Hermit:

1. `/usr/local/bin/python3` is an `fbpython` launcher. Under Hermit it calls
   `clone` with `CLONE_VFORK`, emits the existing unsupported-vfork
   diagnostic, and blocks.
2. Resolving `sys.executable` outside Hermit yields the real interpreter
   (`/usr/local/fbcode/platform010/bin/python3.12`) and avoids that launcher
   deadlock.
3. The real interpreter runs ordinary Python and imports every requested test
   module under Hermit, but the Hermit invocation terminates with `SIGSEGV`
   (status 139) when CPython's `test.regrtest` harness starts.

The committed integration test bypasses the failing `regrtest` invocation.
It uses `unittest` to load the same five module suites from `Lib/test`, sets
CPython's optional resource list to empty to match `regrtest` defaults, and
runs the full 539-case selection twice. The driver reports each module's
discovered case count and fails immediately if any requested module discovers
zero tests. The Rust harness independently requires one positive count per
module and verifies that their sum matches the aggregate count. Both strict
runs exit zero with 17 expected skips and byte-identical stdout and stderr.

Each Hermit execution is bounded to 120 seconds, followed by a 10-second kill
grace period. Hermit uses `--no-virtualize-cpuid` and
`--max-timeslice=disabled` for this host-compatible validation. Strict
thread serialization and deterministic I/O remain enabled.

## Reproduce

```bash
cargo build --release -p hermit --bin hermit
HERMIT_PYTHON=/usr/local/fbcode/platform010/bin/python3.12 \
  cargo test -p hermit --release --test python_stdlib \
  strict_python_stdlib_is_deterministic -- \
  --ignored --exact --nocapture --test-threads=1
```

Without `HERMIT_PYTHON`, the integration test asks the system `python3`
launcher for `sys.executable` before starting Hermit. The test stays ignored
by default because many distributions do not install CPython's full
`Lib/test` package.
