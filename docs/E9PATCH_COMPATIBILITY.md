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
compatibility matrix, followed by 38 additional probes when their programs are
installed. Every available probe uses `hermit run --backend e9patch --strict
--verify`, so a pass is L2. Missing extended programs skip; an installed program
that fails or lacks a backend diagnostic fails the gate. Missing e9patch
artifacts fail before either matrix.

For identity-dependent core rows (`whoami`, `groups`, `pinky`, `logname`,
`tar`, and `chown`), the harness bind-mounts a files-only `nsswitch.conf`.
This keeps asynchronous host identity daemons out of the two-run comparison
without changing the commands under test. The fixture is a stable filesystem
input, not a determinism relaxation.

## 2026-07-24 core result

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

## 2026-07-24 extended result

All 55 extended programs were installed on the measurement host.

| Result | Programs | Meaning |
| --- | ---: | --- |
| L2 pass | 55 | Every installed extended probe produced equivalent logs. |
| Rewritten ELF | 21 | Ten previously measured rows plus the 11 rewritten tools below. |
| Zero-site ELF | 32 | No candidate instruction sites in the main executable. |
| Candidate-only ELF | 1 | `shellcheck` had six candidates that e9tool classified as data. |
| Non-ELF fallback | 1 | `ldd` ran through the explicit ptrace fallback. |
| Failure | 0 | No failure remained in the blocking extended set. |

Together, the core and extended matrices cover 206 entrypoints at L2 on this
host, including 27 rewritten rows.

The five added cache-miss rewrites recovered and patched `perf` 9/9 sites,
`rustup` 24/49 candidates, `mysql` 125/125 sites, `nginx` 2/2 sites, and
`ldconfig` 183/183 sites. Rustup's unrecovered offsets are data according to
e9tool, so its 24/24 recovered instructions are complete coverage. The large
internal mysql executable took about 95 seconds to preprocess on this host and
then passed at L2 from cache; its complete row has a 180-second bound. The
other rows retain the default 60-second bound.

The next system-tool tier recovered and patched `buildah` 54/54 sites, `bat`
15/15, `rg` 6/6, `busybox` 183/183, `qemu-img`, `qemu-io`, and `qemu-nbd` 5/5
each, `btrfs` 13/13, `llvm-exegesis` 22/22, `lto-dump` 10/10, and
`my_print_defaults` 29/29. All 12 rows, including candidate-only `shellcheck`,
passed three additional L2 repetitions (36/36) before entering the blocking
matrix.

`gh --version` initially passed with a 48/48 rewrite, but subsequent runs
exposed an intermittent thread-scheduling divergence that also occurs without
preprocessing. It remains outside the blocking e9patch matrix until the ptrace
runtime behavior is stable.

The initial full Go rewrite exposed an e9tool optimizer interaction. The
default O2 artifact patched all 49 candidates and ran directly on the host, but
terminated with SIGSEGV under Hermit. Instruction-class isolation showed that
syscall-only, CPUID-only, RDTSC/RDTSCP-only, syscall-plus-CPUID, and
CPUID-plus-RDTSC/RDTSCP artifacts ran; the combined syscall-plus-RDTSC/RDTSCP
artifact failed. Rewriting the same complete 49-site set with `e9tool -O0`
passed at L2. Hermit now uses the conservative setting and the rewrite schema
invalidates artifacts made with the old optimizer default.

Across the initial 45-program survey and two replacement probes, nine
exclusions were identified that are not presented as
e9patch rewrite coverage. `mount` and `umount` are intentionally rejected
because they are privilege-bearing. `javap` and `ssh` had zero candidate sites
but diverged on an external identity-service Unix-socket poll, so they are
ptrace/environment gaps. The tested `jar` invocation was unsupported by the
installed Java 8 CLI; npm/pip wrappers failed before useful backend coverage,
npx produced differing output, and PHP timed out.

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
