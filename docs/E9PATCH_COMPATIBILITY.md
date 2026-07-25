# e9patch Compatibility

The e9patch selection is a cached main-ELF preprocessor followed by Hermit's
ptrace Detcore runtime. This report describes the measured application envelope;
it is not a claim that e9patch is a standalone instrumentation runtime.

## Reproduce the matrix

Build e9patch and point Hermit at both executables:

```bash
HERMIT_E9TOOL=/path/to/e9tool \
HERMIT_E9PATCH_BACKEND=/path/to/e9patch \
./validate.sh --e9patch-compat-only
```

The mode runs the same 151 installed-program probes used by the existing
compatibility matrix. Every probe uses `hermit run --backend e9patch --strict
--verify`, so a pass is L2. Missing e9patch artifacts fail before the matrix.

## 2026-07-24 result

Environment: x86_64 CentOS Stream 9; e9patch backend; default log level;
relaxations: none.

| Result | Programs | Meaning |
| --- | ---: | --- |
| L2 pass | 151 | Both executions produced equivalent deterministic logs. |
| Rewritten ELF | 6 | `cargo`, `rustc`, `gcc`, `g++`, `cpp`, and `gcov`. |
| Zero-site ELF | 144 | The main executable contained no candidate instruction sites. |
| Candidate-only ELF | 0 | The linear scan found candidates but e9tool recovered none as instructions. |
| Non-ELF fallback | 1 | `file` resolved to a wrapper; preprocessing was not applicable and ptrace ran it. |
| Failure | 0 | No failure remained in this corpus. |

`cargo` and `rustc` both resolve to the same rustup executable. Its linear map
contains 49 candidate offsets, but e9tool recovers 24 of them as instructions
and patches all 24 without B0. The remaining offsets are in regions e9tool
classifies as data. The first matrix run rejected this 24/24 rewrite because it
compared the e9tool count to all 49 linear-scan candidates. Coverage accounting
now keeps the two counts separate and still rejects any partial recovered-site
rewrite. A cache-miss `cargo --version` run and cache-hit `rustc --version` run
both passed at L2.

## Current limits and intentional failures

- The matrix uses bounded command, version, and small functional probes. It does
  not establish L2 for every workload those programs can execute.
- Only the main executable is preprocessed. Shared objects, the vDSO, JIT code,
  and child executables remain on the ptrace correctness path.
- Empty trampolines preserve the original instructions. Raw `RDRAND`, `RDSEED`,
  and TSX are not made deterministic by preprocessing.
- Hermit rejects partial e9tool coverage, any B0 signal fallback, privileged
  executables, unsafe overlay paths, and missing preprocessing artifacts.
- Non-ELF entrypoints intentionally skip preprocessing rather than invoking
  e9tool on a script or wrapper.
